use std::time::Duration;
use core_types::ResolverError;
use reqwest::{Client, RequestBuilder, Response};
use serde_json::Value;

pub(crate) fn client() -> Result<Client, ResolverError> {
    Client::builder()
        .user_agent(core_types::MEDIA_USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build().map_err(|e| transport("HTTP", e))
}

pub(crate) fn transport(service: &'static str, error: reqwest::Error) -> ResolverError {
    // Signed media URLs and anonymous tokens must not leak through error messages.
    ResolverError::Transport { service, message: error.without_url().to_string() }
}

pub(crate) async fn response(request: RequestBuilder, service: &'static str) -> Result<Response, ResolverError> {
    let response = request.send().await.map_err(|e| transport(service, e))?;
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // HTTP-date values are not parsed here; use a conservative fallback.
        let retry_after_secs = response.headers().get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60).max(1);
        return Err(ResolverError::RateLimited { service, retry_after_secs });
    }
    if !response.status().is_success() {
        return Err(ResolverError::Http { service, status: response.status().as_u16() });
    }
    Ok(response)
}

pub(crate) async fn text(request: RequestBuilder, service: &'static str) -> Result<String, ResolverError> {
    response(request, service).await?.text().await.map_err(|e| transport(service, e))
}

/// Match the native reader's first window, rather than accepting a one-byte
/// probe (or a successful HTML response) as proof of byte-range support.
pub(crate) async fn probe_media(client: &Client, url: &str, service: &'static str, native: bool) -> Result<(), ResolverError> {
    let end = if native { 262143 } else { 0 };
    let result = response(client.get(url).header("Accept-Encoding", "identity")
        .header("Range", format!("bytes=0-{end}")), service).await?;
    if native && (result.status() != reqwest::StatusCode::PARTIAL_CONTENT
        || !result.headers().contains_key(reqwest::header::CONTENT_RANGE)) {
        return Err(ResolverError::Unavailable { service, message: "native playback requires HTTP 206 with Content-Range".into() });
    }
    Ok(())
}

/// HLS manifests are small text files and are not byte-range addressable;
/// check that the signed URL answers with an M3U playlist.
pub(crate) async fn probe_playlist(client: &Client, url: &str, service: &'static str) -> Result<(), ResolverError> {
    let body = text(client.get(url), service).await?;
    if !body.trim_start().starts_with("#EXTM3U") {
        return Err(ResolverError::Unavailable { service, message: "HLS URL did not return a playlist".into() });
    }
    Ok(())
}

pub(crate) async fn json(request: RequestBuilder, service: &'static str) -> Result<Value, ResolverError> {
    let body = text(request, service).await?;
    serde_json::from_str(&body).map_err(|e| crate::parse_error(service, e.to_string()))
}

pub(crate) fn https_url(raw: &str, service: &'static str) -> Result<url::Url, ResolverError> {
    let url = url::Url::parse(raw).map_err(|_| crate::parse_error(service, "invalid URL"))?;
    if url.scheme() != "https" || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(crate::parse_error(service, "expected a public HTTPS URL"));
    }
    Ok(url)
}
