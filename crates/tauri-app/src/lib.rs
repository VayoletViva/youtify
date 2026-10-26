mod player;

use std::path::PathBuf;
use core_types::TrackMetadata;
use player::{AppState, DownloadList, Request, Snapshot};
use tauri::{AppHandle, Manager, State};

#[tauri::command]
async fn search_tracks(state: State<'_, AppState>, query: String, source: String) -> Result<Vec<TrackMetadata>, String> {
    state.search(query, source).await
}

#[tauri::command]
async fn play_track(state: State<'_, AppState>, track: TrackMetadata) -> Result<(), String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.send(Request::Play(track, tx)).await?;
    rx.await.map_err(|_| state.reply_error())?
}

#[tauri::command]
async fn control_playback(state: State<'_, AppState>, action: String, value: Option<f64>) -> Result<(), String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.send(Request::Control(action, value, tx)).await?;
    rx.await.map_err(|_| state.reply_error())?
}

#[tauri::command]
async fn start_my_wave(state: State<'_, AppState>) -> Result<Vec<TrackMetadata>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.send(Request::Wave(tx)).await?;
    rx.await.map_err(|_| state.reply_error())?
}

#[tauri::command]
fn get_player_state(state: State<'_, AppState>) -> Result<Snapshot, String> {
    state.snapshot.read().map(|state| state.clone()).map_err(|e| e.to_string())
}

#[tauri::command]
async fn download_track(app: AppHandle, state: State<'_, AppState>, track: TrackMetadata) -> Result<(), String> {
    state.download(app, track).await
}

#[tauri::command]
async fn list_downloads(state: State<'_, AppState>) -> Result<DownloadList, String> {
    state.list_downloads().await
}

#[tauri::command]
async fn delete_download(app: AppHandle, state: State<'_, AppState>, track_id: String) -> Result<(), String> {
    state.delete_download(&app, track_id).await
}

/// First directory that accepts a file. Android API 30+ lets apps create files
/// under shared Music/ without permissions; older versions fall back to
/// app-specific storage, so trying beats checking the API level.
fn download_dir(app: &tauri::App) -> tauri::Result<PathBuf> {
    let paths = app.path();
    #[cfg(target_os = "android")]
    let candidates = [paths.home_dir().map(|home| home.join("Music").join("Youtify")),
        paths.audio_dir().map(|dir| dir.join("Youtify")), paths.app_data_dir().map(|dir| dir.join("downloads"))];
    #[cfg(not(target_os = "android"))]
    let candidates = [paths.audio_dir().map(|dir| dir.join("Youtify")),
        paths.app_local_data_dir().map(|dir| dir.join("downloads"))];
    // Some Android path calls fail outright (e.g. unmounted storage); skip those.
    for dir in candidates.into_iter().flatten() {
        // An audio extension: Android's Music/ accepts only audio MIME types.
        let probe = dir.join(".youtify-probe.m4a");
        if std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&probe, b"")).is_ok() {
            let _ = std::fs::remove_file(&probe);
            return Ok(dir);
        }
    }
    let dir = paths.app_data_dir()?.join("downloads");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let directory = app.path().app_data_dir()?;
            std::fs::create_dir_all(&directory)?;
            let downloads = download_dir(app)?;
            app.manage(AppState::new(app.handle().clone(), directory.join("wave.sqlite3"), downloads)?);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![search_tracks, play_track, control_playback, start_my_wave,
            get_player_state, download_track, list_downloads, delete_download])
        .run(tauri::generate_context!())
        .expect("failed to run Youtify");
}
