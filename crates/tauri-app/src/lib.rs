mod player;

use core_types::TrackMetadata;
use player::{AppState, Request, Snapshot};
use tauri::{Manager, State};

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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let directory = app.path().app_data_dir()?;
            std::fs::create_dir_all(&directory)?;
            app.manage(AppState::new(app.handle().clone(), directory.join("wave.sqlite3"))?);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![search_tracks, play_track, control_playback, start_my_wave, get_player_state])
        .run(tauri::generate_context!())
        .expect("failed to run Youtify");
}
