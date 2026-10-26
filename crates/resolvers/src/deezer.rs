//! Public Deezer API (no key): metadata search, playlists and discovery.
//! Deezer previews are 30 s clips, so audio is still resolved via YouTube/SoundCloud.
use core_types::{MetadataProvider, ResolverError, TrackMetadata};
use regex::Regex;
use reqwest::Client;
use serde_json::Value;
use std::{cmp::Reverse, collections::{HashMap, HashSet, VecDeque}, sync::LazyLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
use tokio::sync::Mutex;

use crate::{http, parse_error, required_string, string};

const SERVICE: &str = "Deezer";
const API: &str = "https://api.deezer.com";
const API_HOST: &str = "api.deezer.com";
const SEARCH_MAX: usize = 100;
const MATCH_CANDIDATES: usize = 25;
const PLAYLIST_CAP: usize = 2000;
const PER_ARTIST: usize = 2;
const MAX_REDIRECTS: usize = 5;
// Documented quota is 50 requests / 5 s per IP, answered with code 4 inside an
// HTTP 200; leave headroom for other clients behind the same IP.
const QUOTA_WINDOW: Duration = Duration::from_secs(5);
const QUOTA_REQUESTS: usize = 40;
const QUOTA_RETRY_SECS: u64 = 5;
// Penalized in title matching unless the wanted title contains them too.
const VARIANTS: &[&str] = &["remix", "live", "cover", "acoustic", "instrumental", "sped up", "slowed", "nightcore",
    "karaoke", "tribute", "backing track", "as made famous", "in the style of", "originally performed"];
// Sound-alike re-recordings that search and radio results mix in; never wanted on a station.
const JUNK: &[&str] = &["karaoke", "tribute", "backing track", "as made famous", "in the style of", "originally performed"];

// Locale segment is optional (`/en/track/1`, `/en-us/track/1`); query strings are not part of the path.
static ENTITY: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"^/(?:[a-z]{2}(?:-[a-z]{2})?/)?(track|album|playlist|artist)/(\d+)/?$").expect("valid regex"));
static SHORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/s/[A-Za-z0-9]+/?$").expect("valid regex"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind { Track, Album, Playlist, Artist }

impl Kind {
    fn parse(name: &str) -> Option<Self> {
        Some(match name { "track" => Self::Track, "album" => Self::Album, "playlist" => Self::Playlist,
            "artist" => Self::Artist, _ => return None })
    }

    fn name(self) -> &'static str {
        match self { Self::Track => "track", Self::Album => "album", Self::Playlist => "playlist", Self::Artist => "artist" }
    }
}

#[derive(Debug, PartialEq)]
enum Input { Id(u64), Entity(Kind, u64), Short(url::Url), Text }

pub struct Deezer {
    client: Client,
    /// Short links are followed hop by hop: the final `www.deezer.com/track/<id>`
    /// redirects to `/soon` in blocked regions, which would lose the ID.
    links: Client,
    /// Send times inside the current quota window.
    sent: Mutex<VecDeque<Instant>>,
}

impl Deezer {
    pub fn new() -> Result<Self, ResolverError> {
        let links = Client::builder()
            .user_agent(core_types::MEDIA_USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build().map_err(|e| http::transport(SERVICE, e))?;
        Ok(Self { client: http::client()?, links, sent: Mutex::new(VecDeque::new()) })
    }

    /// Plain-text track search, best first. `limit` is capped at 100.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<TrackMetadata>, ResolverError> {
        Ok(self.search_raw(query, limit).await?.iter().filter_map(|item| parse_track(item).ok()).collect())
    }

    /// Best Deezer match for a bare title (e.g. from Spotify oEmbed). The
    /// title is all that is known, so this is a heuristic, not an identity.
    pub async fn match_title(&self, title: &str) -> Result<TrackMetadata, ResolverError> {
        let title = title.trim();
        let base = base_title(title);
        // Deezer finds nothing for Spotify-style "X - Remastered 2011"; retry with the bare title.
        let mut queries = vec![title];
        if !base.is_empty() && base != title { queries.push(&base); }
        for query in queries {
            if let Some(track) = best_match(title, &self.search_raw(query, MATCH_CANDIDATES).await?) {
                return Ok(track);
            }
        }
        Err(ResolverError::NotFound { service: SERVICE, query: title.into() })
    }

    /// New tracks related to `seeds` (artist radio, related artists' top tracks);
    /// Deezer chart when there are no seeds. Deduplicated, at most `limit`.
    pub async fn discover(&self, seeds: &[TrackMetadata], limit: usize) -> Result<Vec<TrackMetadata>, ResolverError> {
        if limit == 0 { return Ok(Vec::new()); }
        let mut seen = Seen::default();
        for seed in seeds { seen.insert(&seed.id, seed.isrc.as_deref(), seed.artists.first().map(String::as_str), &seed.title); }
        let salt = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() as usize);
        // At most three seeds: each costs about five requests.
        let (first, second, third) = tokio::join!(self.seed_pool(seeds.first(), salt),
            self.seed_pool(seeds.get(1), salt / 3), self.seed_pool(seeds.get(2), salt / 7));
        let (mut lists, mut failures) = (Vec::new(), Vec::new());
        for (results, seed_track) in [first, second, third] {
            if let Some(id) = seed_track { seen.ids.insert(format!("deezer:track:{id}")); }
            for result in results {
                match result { Ok(list) => lists.push(list), Err(error) => failures.push(error) }
            }
        }
        let mut succeeded = !lists.is_empty();
        let mut tracks = mix(lists, &mut seen, limit);
        if tracks.is_empty() {
            // Cold start, or seeds Deezer cannot place: the chart keeps the station going.
            match self.list("/chart/0/tracks", &[("limit", "100")]).await {
                Ok(chart) => { succeeded = true; tracks = mix(vec![chart], &mut seen, limit); }
                Err(error) => failures.push(error),
            }
        }
        if tracks.is_empty() && !succeeded {
            // Prefer a quota hit over whichever error came first: callers back off on it.
            let index = failures.iter().position(|e| matches!(e, ResolverError::RateLimited { .. })).unwrap_or(0);
            if index < failures.len() { return Err(failures.swap_remove(index)); }
        }
        Ok(tracks)
    }

    /// Sources for one seed: its artist radio plus top tracks of two related
    /// artists, and the seed's Deezer track ID when the lookup found one.
    async fn seed_pool(&self, seed: Option<&TrackMetadata>, salt: usize)
        -> (Vec<Result<Vec<Value>, ResolverError>>, Option<u64>) {
        let Some(seed) = seed else { return (Vec::new(), None) };
        let (artist, seed_track) = match self.seed_artist(seed).await {
            Ok(found) => found,
            Err(error) => return (vec![Err(error)], None),
        };
        // An unplaceable seed is an answered request, not a failure.
        let Some(artist) = artist else { return (vec![Ok(Vec::new())], seed_track) };
        // Radio is randomized server-side on every call, so it is never cached.
        let (radio_path, related_path) = (format!("/artist/{artist}/radio"), format!("/artist/{artist}/related"));
        let (radio, related) = tokio::join!(self.list(&radio_path, &[("limit", "40")]),
            self.list(&related_path, &[("limit", "10")]));
        let mut results = vec![radio];
        match related {
            Ok(related) => {
                let ids: Vec<u64> = related.iter().filter_map(|a| a["id"].as_u64()).filter(|id| *id != artist).collect();
                let picks = pick_related(ids.len(), salt);
                let (first, second) = tokio::join!(self.top(picks.first().map(|&i| ids[i])),
                    self.top(picks.get(1).map(|&i| ids[i])));
                results.extend(first);
                results.extend(second);
            }
            Err(error) => results.push(Err(error)),
        }
        (results, seed_track)
    }

    async fn top(&self, artist: Option<u64>) -> Option<Result<Vec<Value>, ResolverError>> {
        Some(self.list(&format!("/artist/{}/top", artist?), &[("limit", "5")]).await)
    }

    /// Deezer artist for a seed: by Deezer track ID, then ISRC, then exact artist name.
    async fn seed_artist(&self, seed: &TrackMetadata) -> Result<(Option<u64>, Option<u64>), ResolverError> {
        let by_id = seed.id.strip_prefix("deezer:track:").and_then(|id| id.parse::<u64>().ok()).map(|id| id.to_string());
        let by_isrc = seed.isrc.as_deref().and_then(valid_isrc).map(|isrc| format!("isrc:{isrc}"));
        for key in [by_id, by_isrc].into_iter().flatten() {
            match self.get(api_url(&format!("/track/{key}"), &[])?).await {
                Ok(track) => if let Some(artist) = track.pointer("/artist/id").and_then(Value::as_u64) {
                    return Ok((Some(artist), track["id"].as_u64()));
                },
                Err(ResolverError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        let Some(name) = seed.artists.first() else { return Ok((None, None)) };
        // YouTube auto-generated channels are named "<Artist> - Topic".
        let name = name.strip_suffix(" - Topic").unwrap_or(name).trim();
        if name.is_empty() { return Ok((None, None)); }
        let artists = self.list("/search/artist", &[("q", name), ("limit", "10")]).await?;
        Ok((best_artist(&artists, name), None))
    }

    async fn search_raw(&self, query: &str, limit: usize) -> Result<Vec<Value>, ResolverError> {
        let query = query.trim();
        if query.is_empty() { return Err(ResolverError::InvalidInput("search query is empty".into())); }
        if limit == 0 { return Ok(Vec::new()); }
        // Advanced `artist:"..."` syntax returns nothing; plain text works.
        let limit = limit.min(SEARCH_MAX).to_string();
        self.list("/search", &[("q", query), ("limit", &limit)]).await
    }

    async fn list(&self, path: &str, query: &[(&str, &str)]) -> Result<Vec<Value>, ResolverError> {
        let mut value = self.get(api_url(path, query)?).await?;
        match value.get_mut("data").map(Value::take) {
            Some(Value::Array(items)) => Ok(items),
            _ => Err(parse_error(SERVICE, "data missing")),
        }
    }

    async fn get(&self, url: url::Url) -> Result<Value, ResolverError> {
        // `next` links come from responses; never let them point elsewhere.
        if url.scheme() != "https" || url.host_str() != Some(API_HOST) || url.port().is_some() {
            return Err(parse_error(SERVICE, "unexpected API URL"));
        }
        self.throttle().await;
        let path = url.path().to_owned();
        // Names (genres, radios) otherwise follow the IP's locale.
        let value = http::json(self.client.get(url).header(reqwest::header::ACCEPT_LANGUAGE, "en"), SERVICE).await?;
        match api_error(&value, &path) { Some(error) => Err(error), None => Ok(value) }
    }

    async fn throttle(&self) {
        loop {
            let wait = reserve(&mut *self.sent.lock().await, Instant::now());
            match wait { None => return, Some(wait) => tokio::time::sleep(wait).await }
        }
    }

    /// Resolves `link.deezer.com/s/<code>` without leaving deezer.com.
    async fn follow_short(&self, mut url: url::Url) -> Result<(Kind, u64), ResolverError> {
        for _ in 0..MAX_REDIRECTS {
            let response = self.links.get(url.clone()).send().await.map_err(|e| http::transport(SERVICE, e))?;
            if !response.status().is_redirection() { break; }
            let location = response.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok())
                .ok_or_else(|| parse_error(SERVICE, "redirect without Location"))?;
            let next = url.join(location).map_err(|_| parse_error(SERVICE, "invalid redirect"))?;
            if next.scheme() != "https" || !is_deezer_host(&next) {
                return Err(ResolverError::Unavailable { service: SERVICE, message: "short link left deezer.com".into() });
            }
            if let Some(entity) = entity(&next) { return Ok(entity); }
            if next.path().contains("deezer-links-404") {
                return Err(ResolverError::NotFound { service: SERVICE, query: "short link".into() });
            }
            url = next;
        }
        Err(ResolverError::Unavailable { service: SERVICE, message: "short link did not resolve to a Deezer page".into() })
    }

    async fn entity_id(&self, input: &str, want: Kind) -> Result<Option<u64>, ResolverError> {
        let wrong = || ResolverError::InvalidInput(format!("expected a Deezer {} URL or ID", want.name()));
        match parse_input(input)? {
            Input::Id(id) => Ok(Some(id)),
            Input::Entity(kind, id) if kind == want => Ok(Some(id)),
            Input::Short(url) => match self.follow_short(url).await? {
                (kind, id) if kind == want => Ok(Some(id)),
                _ => Err(wrong()),
            },
            Input::Entity(..) => Err(wrong()),
            Input::Text => Ok(None),
        }
    }
}

impl MetadataProvider for Deezer {
    async fn track(&self, input: &str) -> Result<TrackMetadata, ResolverError> {
        let input = input.trim();
        if input.is_empty() { return Err(ResolverError::InvalidInput("empty Deezer track input".into())); }
        match self.entity_id(input, Kind::Track).await? {
            Some(id) => parse_track(&self.get(api_url(&format!("/track/{id}"), &[])?).await?),
            None => self.search_raw(input, 10).await?.iter().find_map(|item| parse_track(item).ok())
                .ok_or_else(|| ResolverError::NotFound { service: SERVICE, query: input.into() }),
        }
    }

    async fn playlist(&self, input: &str) -> Result<Vec<TrackMetadata>, ResolverError> {
        let Some(id) = self.entity_id(input.trim(), Kind::Playlist).await? else {
            return Err(ResolverError::InvalidInput("expected a Deezer playlist URL or ID".into()));
        };
        // The embedded list in `/playlist/<id>` stops at 100; `/tracks` pages via `next`.
        let mut url = api_url(&format!("/playlist/{id}/tracks"), &[("limit", "100")])?;
        let mut pages = PlaylistPages::default();
        while let Some(next) = pages.add(&self.get(url).await?)? {
            url = http::https_url(&next, SERVICE)?;
        }
        pages.finish()
    }
}

fn api_url(path: &str, query: &[(&str, &str)]) -> Result<url::Url, ResolverError> {
    let raw = format!("{API}{path}");
    let url = if query.is_empty() { url::Url::parse(&raw) } else { url::Url::parse_with_params(&raw, query) };
    url.map_err(|_| parse_error(SERVICE, "invalid API URL"))
}

/// Deezer reports errors inside HTTP 200 bodies.
fn api_error(value: &Value, query: &str) -> Option<ResolverError> {
    let error = value.get("error").filter(|e| e.is_object())?;
    let code = error["code"].as_u64();
    let message = format!("{} (code {})", error["message"].as_str().unwrap_or("API error"),
        code.map_or_else(|| "?".to_owned(), |c| c.to_string()));
    Some(match code {
        Some(4) => ResolverError::RateLimited { service: SERVICE, retry_after_secs: QUOTA_RETRY_SECS },
        Some(800) => ResolverError::NotFound { service: SERVICE, query: query.into() },
        // Bad parameters or path: this client sent something Deezer does not understand.
        Some(500 | 501 | 600) => ResolverError::Parse { service: SERVICE, message },
        _ => ResolverError::Unavailable { service: SERVICE, message },
    })
}

/// Records a send at `now` and returns `None`, or returns how long to wait.
fn reserve(sent: &mut VecDeque<Instant>, now: Instant) -> Option<Duration> {
    while sent.front().is_some_and(|t| now.duration_since(*t) >= QUOTA_WINDOW) { sent.pop_front(); }
    if sent.len() < QUOTA_REQUESTS {
        sent.push_back(now);
        return None;
    }
    Some(QUOTA_WINDOW.saturating_sub(now.duration_since(sent[0])).max(Duration::from_millis(1)))
}

fn parse_input(input: &str) -> Result<Input, ResolverError> {
    let input = input.trim();
    if let Some(rest) = input.strip_prefix("deezer:") {
        let (kind, id) = rest.split_once(':').ok_or_else(|| ResolverError::InvalidInput("malformed Deezer ID".into()))?;
        return match (Kind::parse(kind), id.parse::<u64>()) {
            (Some(kind), Ok(id)) if id > 0 => Ok(Input::Entity(kind, id)),
            _ => Err(ResolverError::InvalidInput("malformed Deezer ID".into())),
        };
    }
    if !input.is_empty() && input.len() <= 20 && input.bytes().all(|b| b.is_ascii_digit()) {
        return input.parse::<u64>().ok().filter(|id| *id > 0).map(Input::Id)
            .ok_or_else(|| ResolverError::InvalidInput("invalid Deezer ID".into()));
    }
    let lower = input.to_ascii_lowercase();
    let raw = if ["deezer.com/", "www.deezer.com/", "link.deezer.com/"].iter().any(|p| lower.starts_with(p)) {
        format!("https://{input}")
    } else if lower.contains("://") {
        input.to_owned()
    } else {
        return Ok(Input::Text);
    };
    let not_deezer = || ResolverError::InvalidInput("not a Deezer URL".into());
    let mut url = url::Url::parse(&raw).map_err(|_| not_deezer())?;
    if !matches!(url.scheme(), "https" | "http") || !is_deezer_host(&url) || !url.username().is_empty() || url.password().is_some() {
        return Err(not_deezer());
    }
    if url.host_str() == Some("link.deezer.com") && SHORT.is_match(url.path()) {
        url.set_scheme("https").map_err(|_| not_deezer())?;
        url.set_port(None).map_err(|_| not_deezer())?;
        return Ok(Input::Short(url));
    }
    entity(&url).map(|(kind, id)| Input::Entity(kind, id))
        .ok_or_else(|| ResolverError::InvalidInput("unsupported Deezer URL".into()))
}

fn is_deezer_host(url: &url::Url) -> bool {
    url.host_str().is_some_and(|host| host == "deezer.com" || host.ends_with(".deezer.com"))
}

fn entity(url: &url::Url) -> Option<(Kind, u64)> {
    if !is_deezer_host(url) || url.host_str() == Some("link.deezer.com") { return None; }
    let captures = ENTITY.captures(url.path())?;
    let id = captures[2].parse::<u64>().ok().filter(|id| *id > 0)?;
    Some((Kind::parse(&captures[1])?, id))
}

fn parse_track(item: &Value) -> Result<TrackMetadata, ResolverError> {
    let id = item["id"].as_u64().filter(|id| *id > 0).ok_or_else(|| parse_error(SERVICE, "track id missing"))?;
    // `title` keeps the version suffix ("(Radio Edit)") that `title_short` drops.
    let title = required_string(item, "/title", SERVICE)?;
    // Search and radio rows omit `contributors`; the main artist is always there.
    let mut artists: Vec<String> = Vec::new();
    for name in item["contributors"].as_array().into_iter().flatten().filter_map(|c| string(c, "/name")) {
        if !artists.contains(&name) { artists.push(name); }
    }
    if artists.is_empty() { artists.extend(string(item, "/artist/name")); }
    if artists.is_empty() { return Err(parse_error(SERVICE, "track artist missing")); }
    let duration_ms = item["duration"].as_u64().filter(|d| *d > 0).and_then(|d| d.checked_mul(1000))
        .ok_or_else(|| parse_error(SERVICE, "track duration missing"))?;
    let cover_url = string(item, "/album/cover_xl").filter(|url| url.starts_with("https://"))
        .or_else(|| string(item, "/album/md5_image").or_else(|| string(item, "/md5_image")).as_deref().and_then(cover));
    Ok(TrackMetadata { id: format!("deezer:track:{id}"), title, artists, album: string(item, "/album/title"),
        duration_ms, cover_url, isrc: string(item, "/isrc").as_deref().and_then(valid_isrc) })
}

/// 1000x1000 cover from an image hash, as `album.cover_xl` would be.
fn cover(md5: &str) -> Option<String> {
    (md5.len() == 32 && md5.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("https://cdn-images.dzcdn.net/images/cover/{md5}/1000x1000-000000-80-0-0.jpg"))
}

fn valid_isrc(raw: &str) -> Option<String> {
    let isrc = raw.trim().chars().filter(|c| *c != '-').collect::<String>().to_ascii_uppercase();
    (isrc.len() == 12 && isrc.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(isrc)
}

/// Lowercased words joined by single spaces; punctuation is a separator.
fn normalize(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Title without version decorations: "X - Remastered 2011", "X (Live)", "X [feat. Y]".
fn base_title(title: &str) -> String {
    let mut base = title.split(" - ").next().unwrap_or(title).trim();
    while base.ends_with([')', ']']) {
        match base.rfind(['(', '[']) {
            Some(open) if open > 0 => base = base[..open].trim_end(),
            _ => break,
        }
    }
    base.to_owned()
}

fn has_term(haystack: &str, term: &str) -> bool {
    format!(" {haystack} ").contains(&format!(" {term} "))
}

/// Version/junk text of a raw row: title, version, album and artist.
fn row_text(item: &Value) -> String {
    let parts = ["/title", "/title_version", "/album/title", "/artist/name"];
    normalize(&parts.iter().filter_map(|p| item.pointer(p).and_then(Value::as_str)).collect::<Vec<_>>().join(" "))
}

/// Exact title first, then fewest variant mismatches (unwanted ones present,
/// wanted ones missing), then full-title equality, then Deezer popularity.
fn best_match(wanted: &str, items: &[Value]) -> Option<TrackMetadata> {
    let wanted_text = normalize(wanted);
    let wanted_base = normalize(&base_title(wanted));
    items.iter().enumerate().filter_map(|(index, item)| {
        let track = parse_track(item).ok()?;
        let full = normalize(&track.title) == wanted_text;
        let short = normalize(item["title_short"].as_str().unwrap_or(&track.title));
        let exact = full || short == wanted_base;
        let text = row_text(item);
        let mismatches = VARIANTS.iter().filter(|v| has_term(&text, v) != has_term(&wanted_text, v)).count();
        let rank = item["rank"].as_u64().unwrap_or(0);
        Some(((!exact, mismatches, !full, Reverse(rank), index), track))
    }).min_by(|a, b| a.0.cmp(&b.0)).map(|(_, track)| track)
}

/// Exact (case-insensitive) name match with the most fans; no fuzzy fallback,
/// since a wrong artist's radio is worse than the chart.
fn best_artist(artists: &[Value], name: &str) -> Option<u64> {
    let name = normalize(name);
    if name.is_empty() { return None; }
    artists.iter().filter(|a| a["name"].as_str().is_some_and(|n| normalize(n) == name))
        .max_by_key(|a| a["nb_fan"].as_u64().unwrap_or(0)).and_then(|a| a["id"].as_u64())
}

/// Up to two distinct indices below `len`, varied by `salt`.
fn pick_related(len: usize, salt: usize) -> Vec<usize> {
    match len {
        0 => Vec::new(),
        1 => vec![0],
        _ => {
            let first = salt % len;
            vec![first, (first + 1 + (salt / len) % (len - 1)) % len]
        }
    }
}

/// Recordings already taken: the same song appears under album, single and
/// compilation IDs, sometimes with different ISRCs.
#[derive(Default)]
struct Seen { ids: HashSet<String>, isrcs: HashSet<String>, songs: HashSet<String> }

impl Seen {
    /// True when nothing about this recording was seen before; records it either way.
    fn insert(&mut self, id: &str, isrc: Option<&str>, artist: Option<&str>, title: &str) -> bool {
        let isrc = isrc.and_then(valid_isrc);
        let song = artist.map(|a| format!("{}|{}", normalize(a), normalize(&base_title(title))));
        let new = !self.ids.contains(id) && isrc.as_ref().is_none_or(|i| !self.isrcs.contains(i))
            && song.as_ref().is_none_or(|s| !self.songs.contains(s));
        self.ids.insert(id.to_owned());
        self.isrcs.extend(isrc);
        self.songs.extend(song);
        new
    }
}

/// Round-robins `lists` so no seed or source dominates, at most `PER_ARTIST`
/// tracks per artist, skipping junk versions and anything `seen`.
fn mix(lists: Vec<Vec<Value>>, seen: &mut Seen, limit: usize) -> Vec<TrackMetadata> {
    let mut queues: Vec<VecDeque<Value>> = lists.into_iter().map(VecDeque::from).collect();
    let mut per_artist: HashMap<String, usize> = HashMap::new();
    let mut tracks = Vec::new();
    while tracks.len() < limit && queues.iter().any(|q| !q.is_empty()) {
        for queue in &mut queues {
            while let Some(item) = queue.pop_front() {
                let Ok(track) = parse_track(&item) else { continue };
                if JUNK.iter().any(|j| has_term(&row_text(&item), j)) { continue; }
                let artist = normalize(&track.artists[0]);
                if per_artist.get(&artist).is_some_and(|n| *n >= PER_ARTIST) { continue; }
                if !seen.insert(&track.id, track.isrc.as_deref(), Some(&track.artists[0]), &track.title) { continue; }
                *per_artist.entry(artist).or_default() += 1;
                tracks.push(track);
                break;
            }
            if tracks.len() == limit { break; }
        }
    }
    tracks
}

#[derive(Default)]
struct PlaylistPages { tracks: Vec<TrackMetadata>, rows: usize, total: Option<usize>, pages: usize }

impl PlaylistPages {
    /// Adds one `/playlist/<id>/tracks` page; returns the `next` URL, if any.
    fn add(&mut self, page: &Value) -> Result<Option<String>, ResolverError> {
        let items = page["data"].as_array().ok_or_else(|| parse_error(SERVICE, "playlist data missing"))?;
        if let Some(total) = page["total"].as_u64() {
            let total = usize::try_from(total).unwrap_or(usize::MAX);
            if total > PLAYLIST_CAP {
                return Err(ResolverError::Unavailable { service: SERVICE,
                    message: format!("playlist has more than {PLAYLIST_CAP} tracks") });
            }
            self.total = Some(total);
        }
        self.pages += 1;
        self.rows += items.len();
        if self.rows > PLAYLIST_CAP || self.pages > PLAYLIST_CAP / 100 + 2 {
            return Err(ResolverError::Unavailable { service: SERVICE, message: "playlist pagination did not end".into() });
        }
        // Rows that are not tracks (removed or malformed) are skipped, not fatal.
        self.tracks.extend(items.iter().filter_map(|item| parse_track(item).ok()));
        // An empty page with `next` would loop forever; stop and let `finish` judge.
        Ok(string(page, "/next").filter(|_| !items.is_empty()))
    }

    /// Fewer rows than `total` means a page went missing.
    fn finish(self) -> Result<Vec<TrackMetadata>, ResolverError> {
        if self.total.is_some_and(|total| self.rows < total) {
            return Err(ResolverError::Unavailable { service: SERVICE, message: "incomplete playlist".into() });
        }
        Ok(self.tracks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Trimmed from live `/track/3135553`.
    fn full_track() -> Value {
        json!({"id": 3135553, "readable": true, "title": "One More Time", "title_short": "One More Time",
            "title_version": "", "isrc": "GBDUW0000053", "duration": 320, "rank": 896666,
            "contributors": [{"id": 27, "name": "Daft Punk", "role": "Main"}], "artist": {"id": 27, "name": "Daft Punk"},
            "album": {"id": 302127, "title": "Discovery",
                "cover_xl": "https://cdn-images.dzcdn.net/images/cover/5718f7c81c27e0b2417e2a4c45224f8a/1000x1000-000000-80-0-0.jpg",
                "md5_image": "5718f7c81c27e0b2417e2a4c45224f8a"}, "type": "track"})
    }

    fn row(id: u64, title: &str, short: &str, version: &str, artist: &str, rank: u64) -> Value {
        json!({"id": id, "title": title, "title_short": short, "title_version": version, "duration": 200,
            "rank": rank, "artist": {"name": artist}, "album": {"title": "Album"}})
    }

    #[test]
    fn parses_full_track() {
        let track = parse_track(&full_track()).unwrap();
        assert_eq!(track.id, "deezer:track:3135553");
        assert_eq!(track.title, "One More Time");
        assert_eq!(track.artists, ["Daft Punk"]);
        assert_eq!(track.album.as_deref(), Some("Discovery"));
        assert_eq!(track.duration_ms, 320_000);
        assert_eq!(track.isrc.as_deref(), Some("GBDUW0000053"));
        assert!(track.cover_url.unwrap().contains("/5718f7c81c27e0b2417e2a4c45224f8a/1000x1000-"));
    }

    #[test]
    fn radio_rows_fall_back_to_artist_and_md5_cover() {
        // Trimmed from live `/artist/27/radio`: no contributors, isrc or cover_xl.
        let item = json!({"id": 3135557, "title": "Crescendolls", "title_short": "Crescendolls", "duration": 211,
            "md5_image": "5718f7c81c27e0b2417e2a4c45224f8a", "artist": {"id": 27, "name": "Daft Punk"},
            "album": {"id": 302127, "title": "Discovery", "md5_image": "5718f7c81c27e0b2417e2a4c45224f8a"}});
        let track = parse_track(&item).unwrap();
        assert_eq!(track.artists, ["Daft Punk"]);
        assert_eq!(track.isrc, None);
        assert_eq!(track.cover_url.as_deref(),
            Some("https://cdn-images.dzcdn.net/images/cover/5718f7c81c27e0b2417e2a4c45224f8a/1000x1000-000000-80-0-0.jpg"));
    }

    #[test]
    fn contributors_keep_order_without_duplicates() {
        let mut item = full_track();
        item["contributors"] = json!([{"name": "Daft Punk"}, {"name": "Julian Casablancas"}, {"name": "Daft Punk"}]);
        assert_eq!(parse_track(&item).unwrap().artists, ["Daft Punk", "Julian Casablancas"]);
    }

    #[test]
    fn version_suffix_is_kept_and_unreadable_is_accepted() {
        let mut item = row(7868755, "Without Me (Radio Edit)", "Without Me", "(Radio Edit)", "Eminem", 1);
        item["readable"] = json!(false);
        assert_eq!(parse_track(&item).unwrap().title, "Without Me (Radio Edit)");
    }

    #[test]
    fn rejects_incomplete_tracks() {
        for (field, value) in [("id", json!(0)), ("duration", json!(0)), ("title", json!("  ")), ("artist", json!({}))] {
            let mut item = full_track();
            item[field] = value;
            if field == "artist" { item.as_object_mut().unwrap().remove("contributors"); }
            assert!(matches!(parse_track(&item), Err(ResolverError::Parse { .. })), "{field}");
        }
        assert!(parse_track(&json!({"id": 1})).is_err());
    }

    #[test]
    fn maps_in_body_errors() {
        let error = |code: u64| json!({"error": {"type": "X", "message": "m", "code": code}});
        assert!(matches!(api_error(&error(4), "/search"),
            Some(ResolverError::RateLimited { retry_after_secs: 5, .. })));
        assert!(matches!(api_error(&error(800), "/track/1"), Some(ResolverError::NotFound { .. })));
        assert!(matches!(api_error(&error(600), "/foo"), Some(ResolverError::Parse { .. })));
        assert!(matches!(api_error(&error(200), "/user/me"), Some(ResolverError::Unavailable { .. })));
        assert!(api_error(&full_track(), "/track/3135553").is_none());
        assert!(api_error(&json!({"data": [], "error": null}), "/search").is_none());
    }

    #[test]
    fn parses_inputs() {
        for (input, kind, id) in [
            ("https://www.deezer.com/track/3135553", Kind::Track, 3135553),
            ("https://www.deezer.com/en/track/3135553?utm_source=x", Kind::Track, 3135553),
            ("https://www.deezer.com/pt-br/playlist/1313621735", Kind::Playlist, 1313621735),
            ("deezer.com/us/album/302127", Kind::Album, 302127),
            ("http://www.deezer.com/artist/27/", Kind::Artist, 27),
            ("deezer:track:3135553", Kind::Track, 3135553),
        ] {
            assert_eq!(parse_input(input).unwrap(), Input::Entity(kind, id), "{input}");
        }
        assert_eq!(parse_input(" 3135553 ").unwrap(), Input::Id(3135553));
        assert_eq!(parse_input("daft punk one more time").unwrap(), Input::Text);
        assert!(matches!(parse_input("https://link.deezer.com/s/30abcDEF").unwrap(), Input::Short(url)
            if url.as_str() == "https://link.deezer.com/s/30abcDEF"));
        for bad in ["https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT", "https://deezer.com.evil.example/track/1",
            "https://www.deezer.com/track/abc", "https://www.deezer.com/en/track/0", "https://user@www.deezer.com/track/1",
            "https://www.deezer.com/show/1", "deezer:track:", "deezer:song:1", "0", "ftp://www.deezer.com/track/1"] {
            assert!(matches!(parse_input(bad), Err(ResolverError::InvalidInput(_))), "{bad}");
        }
    }

    #[test]
    fn short_link_targets_parse_but_link_host_does_not() {
        let target = url::Url::parse("https://www.deezer.com/en/track/3135553?host=0&utm=x").unwrap();
        assert_eq!(entity(&target), Some((Kind::Track, 3135553)));
        assert_eq!(entity(&url::Url::parse("https://link.deezer.com/track/1").unwrap()), None);
        assert_eq!(entity(&url::Url::parse("https://www.deezer.com/soon").unwrap()), None);
    }

    #[test]
    fn normalizes_titles() {
        assert_eq!(normalize("  Never Gonna   GIVE You Up!! "), "never gonna give you up");
        assert_eq!(base_title("Bohemian Rhapsody - Remastered 2011"), "Bohemian Rhapsody");
        assert_eq!(base_title("Get Lucky (feat. Pharrell) [Radio Edit]"), "Get Lucky");
        assert_eq!(base_title("(I Can't Get No) Satisfaction"), "(I Can't Get No) Satisfaction");
        assert!(has_term("blinding lights acoustic version", "acoustic"));
        assert!(!has_term("alive", "live"));
        assert_eq!(valid_isrc("us-ir1-02-11038").as_deref(), Some("USIR10211038"));
        assert_eq!(valid_isrc("short"), None);
    }

    #[test]
    fn title_match_prefers_exact_original_by_rank() {
        // Shape of live "Blinding Lights" results (2026-10).
        let items = [
            row(851937072, "Blinding Lights (Acoustic Version)", "Blinding Lights", "(Acoustic Version)", "Victoria Voss", 990000),
            row(1161752782, "Blinding Lights (Remix)", "Blinding Lights", "(Remix)", "The Weeknd", 980000),
            row(3638278102, "Blinding Lights", "Blinding Lights", "", "Emily Dawn", 592298),
            row(908604612, "Blinding Lights", "Blinding Lights", "", "The Weeknd", 984833),
            row(1, "Blinding", "Blinding", "", "Someone", 999999),
        ];
        assert_eq!(best_match("Blinding Lights", &items).unwrap().id, "deezer:track:908604612");
        // A wanted variant is not penalized.
        assert_eq!(best_match("Blinding Lights (Remix)", &items).unwrap().id, "deezer:track:1161752782");
        // Spotify-style suffixes still match the plain Deezer title.
        assert_eq!(best_match("Blinding Lights - 2020 Remaster", &items).unwrap().id, "deezer:track:908604612");
        assert!(best_match("Blinding Lights", &[]).is_none());
    }

    #[test]
    fn title_match_penalizes_junk_in_album_or_artist() {
        let mut karaoke = row(2, "Never Gonna Give You Up", "Never Gonna Give You Up", "", "Karaoke Hits", 999999);
        karaoke["album"]["title"] = json!("In the Style of Rick Astley");
        let items = [karaoke, row(14408104, "Never Gonna Give You Up", "Never Gonna Give You Up", "", "Rick Astley", 769702)];
        assert_eq!(best_match("Never Gonna Give You Up", &items).unwrap().id, "deezer:track:14408104");
    }

    #[test]
    fn artist_match_is_exact_with_most_fans() {
        // Live `/search/artist?q=Rick Astley`.
        let artists = json!([{"id": 6160, "name": "Rick Astley", "nb_fan": 228008}, {"id": 413113291, "name": "Rick Astley", "nb_fan": 0},
            {"id": 462676, "name": "Karaoke - Rick Astley", "nb_fan": 103}, {"id": 1241878, "name": "Rick Astley Tribute Band", "nb_fan": 135}]);
        let artists = artists.as_array().unwrap();
        assert_eq!(best_artist(artists, "rick astley"), Some(6160));
        assert_eq!(best_artist(artists, "Rick"), None);
        assert_eq!(best_artist(artists, " "), None);
    }

    #[test]
    fn related_picks_are_distinct_and_in_range() {
        assert!(pick_related(0, 7).is_empty());
        assert_eq!(pick_related(1, 7), [0]);
        for len in 2..12 {
            for salt in 0..100 {
                let picks = pick_related(len, salt);
                assert!(picks.len() == 2 && picks[0] != picks[1] && picks.iter().all(|&i| i < len), "{len} {salt}");
            }
        }
    }

    #[test]
    fn dedups_by_id_isrc_and_song() {
        let mut seen = Seen::default();
        assert!(seen.insert("deezer:track:916424", Some("USIR10211038"), Some("Eminem"), "Without Me"));
        assert!(!seen.insert("deezer:track:916424", None, None, "x"));
        assert!(!seen.insert("deezer:track:2", Some("usir10211038"), Some("Other"), "Other"));
        // Radio edit: different ID and ISRC, same song.
        assert!(!seen.insert("deezer:track:7868755", Some("USIR10211036"), Some("EMINEM"), "Without Me (Radio Edit)"));
        assert!(seen.insert("deezer:track:3", None, Some("Eminem"), "Lose Yourself"));
    }

    #[test]
    fn mix_interleaves_caps_artists_and_drops_junk_and_seeds() {
        let radio: Vec<Value> = (1..=6).map(|i| row(i, &format!("Song {i}"), &format!("Song {i}"), "", "Daft Punk", 1)).collect();
        let mut tribute = row(20, "One More Time", "One More Time", "", "Daft Punk Tribute Band", 1);
        tribute["album"]["title"] = json!("Tribute to Daft Punk");
        let related = vec![row(10, "D.A.N.C.E.", "D.A.N.C.E.", "", "Justice", 1), tribute,
            row(11, "Genesis", "Genesis", "", "Justice", 1), row(12, "Phantom", "Phantom", "", "Justice", 1)];
        let mut seen = Seen::default();
        seen.insert("deezer:track:1", None, Some("Daft Punk"), "Song 1");
        let ids: Vec<String> = mix(vec![radio, related], &mut seen, 10).into_iter().map(|t| t.id).collect();
        assert_eq!(ids, ["deezer:track:2", "deezer:track:10", "deezer:track:3", "deezer:track:11"]);
        // Later batches reset the per-artist cap but keep dedup.
        let more = mix(vec![vec![row(2, "Song 2", "Song 2", "", "Daft Punk", 1), row(5, "Song 5", "Song 5", "", "Daft Punk", 1),
            row(6, "Song 6", "Song 6", "", "Daft Punk", 1)]], &mut seen, 1);
        assert_eq!(more.len(), 1);
        assert_eq!(more[0].id, "deezer:track:5");
    }

    #[test]
    fn playlist_pages_must_be_complete() {
        let page = |ids: &[u64], total: u64, next: Option<&str>| json!({"data": ids.iter()
            .map(|&id| row(id, "T", "T", "", "A", 1)).collect::<Vec<_>>(), "total": total, "next": next});
        let mut pages = PlaylistPages::default();
        let next = "https://api.deezer.com/playlist/1/tracks?limit=2&index=2";
        assert_eq!(pages.add(&page(&[1, 2], 4, Some(next))).unwrap().as_deref(), Some(next));
        // An unparseable row still counts toward `total`.
        let mut last = page(&[3], 4, None);
        last["data"].as_array_mut().unwrap().push(json!({"id": 4}));
        assert_eq!(pages.add(&last).unwrap(), None);
        assert_eq!(pages.finish().unwrap().len(), 3);

        let mut short = PlaylistPages::default();
        assert_eq!(short.add(&page(&[1, 2], 5, Some(next))).unwrap().as_deref(), Some(next));
        assert_eq!(short.add(&page(&[], 5, Some(next))).unwrap(), None);
        assert!(matches!(short.finish(), Err(ResolverError::Unavailable { .. })));

        assert!(PlaylistPages::default().add(&page(&[1], 2001, None)).is_err());
        assert!(PlaylistPages::default().add(&json!({"total": 1})).is_err());
    }

    #[test]
    fn limiter_allows_quota_then_waits_for_oldest() {
        let mut sent = VecDeque::new();
        let start = Instant::now();
        for _ in 0..QUOTA_REQUESTS { assert_eq!(reserve(&mut sent, start), None); }
        let later = start + Duration::from_secs(2);
        assert_eq!(reserve(&mut sent, later), Some(Duration::from_secs(3)));
        assert_eq!(sent.len(), QUOTA_REQUESTS);
        assert_eq!(reserve(&mut sent, start + QUOTA_WINDOW), None);
        assert_eq!(sent.len(), 1);
    }

    #[test]
    fn api_urls_encode_queries() {
        let url = api_url("/search", &[("q", "AC/DC & co"), ("limit", "5")]).unwrap();
        assert_eq!(url.host_str(), Some(API_HOST));
        assert_eq!(url.query(), Some("q=AC%2FDC+%26+co&limit=5"));
    }
}
