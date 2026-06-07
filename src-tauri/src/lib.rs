//! Granola — local meeting recorder + transcription client for Vexa.
//!
//! Captures microphone + system audio, mixes to 16 kHz mono, chunks on pauses,
//! transcribes each chunk via the Vexa transcription service, and stores
//! sessions + transcripts locally (SQLite + WAV). The frontend drives recording
//! and renders live transcripts streamed over Tauri events.

mod audio;
mod chunker;
mod recorder;
mod settings;
mod storage;
mod transcribe;

use std::path::PathBuf;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use recorder::RecorderHandle;
use settings::Settings;
use storage::{SegmentRow, SessionRow};

struct AppState {
    db_path: PathBuf,
    audio_dir: PathBuf,
    settings_path: PathBuf,
    settings: Mutex<Settings>,
    recorder: Mutex<Option<RecorderHandle>>,
}

impl AppState {
    fn db(&self) -> Result<rusqlite::Connection, String> {
        storage::open(&self.db_path).map_err(|e| e.to_string())
    }
}

#[derive(Serialize)]
struct StartResult {
    session_id: String,
}

#[tauri::command]
fn get_settings(state: State<AppState>) -> Settings {
    state.settings.lock().clone()
}

#[tauri::command]
fn save_settings(state: State<AppState>, new_settings: Settings) -> Result<(), String> {
    settings::save(&state.settings_path, &new_settings).map_err(|e| e.to_string())?;
    *state.settings.lock() = new_settings;
    Ok(())
}

#[tauri::command]
fn is_recording(state: State<AppState>) -> bool {
    state.recorder.lock().is_some()
}

#[tauri::command]
fn start_recording(
    app: AppHandle,
    state: State<AppState>,
    title: Option<String>,
) -> Result<StartResult, String> {
    let mut guard = state.recorder.lock();
    if guard.is_some() {
        return Err("already recording".into());
    }
    let settings = state.settings.lock().clone();
    let title = title.unwrap_or_else(|| {
        format!("Recording {}", chrono::Local::now().format("%Y-%m-%d %H:%M"))
    });
    let handle = recorder::start(
        app,
        state.db_path.clone(),
        state.audio_dir.clone(),
        settings,
        title,
    )
    .map_err(|e| format!("{e:#}"))?;
    let session_id = handle.session_id.clone();
    *guard = Some(handle);
    Ok(StartResult { session_id })
}

#[tauri::command]
fn stop_recording(app: AppHandle, state: State<AppState>) -> Result<f64, String> {
    let handle = state.recorder.lock().take();
    match handle {
        Some(h) => Ok(h.stop(&app, &state.db_path)),
        None => Err("not recording".into()),
    }
}

#[tauri::command]
fn list_sessions(state: State<AppState>) -> Result<Vec<SessionRow>, String> {
    let conn = state.db()?;
    storage::list_sessions(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_session_segments(state: State<AppState>, id: String) -> Result<Vec<SegmentRow>, String> {
    let conn = state.db()?;
    storage::get_segments(&conn, &id).map_err(|e| e.to_string())
}

#[tauri::command]
fn rename_session(state: State<AppState>, id: String, title: String) -> Result<(), String> {
    let conn = state.db()?;
    storage::rename_session(&conn, &id, &title).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_session(state: State<AppState>, id: String) -> Result<(), String> {
    let conn = state.db()?;
    let audio_base = storage::delete_session(&conn, &id).map_err(|e| e.to_string())?;
    if let Some(base) = audio_base {
        // Per-source WAVs are `{base}-mic.wav` / `{base}-system.wav`.
        for src in ["mic", "system"] {
            std::fs::remove_file(format!("{base}-{src}.wav")).ok();
        }
        // Also remove a legacy single-file recording, if present.
        std::fs::remove_file(&base).ok();
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Logs go to stderr; control verbosity with RUST_LOG (e.g. RUST_LOG=info).
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .try_init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let config_dir = app.path().app_config_dir()?;
            std::fs::create_dir_all(&data_dir).ok();
            std::fs::create_dir_all(&config_dir).ok();

            let db_path = data_dir.join("granola.db");
            let audio_dir = data_dir.join("recordings");
            let settings_path = config_dir.join("settings.json");
            std::fs::create_dir_all(&audio_dir).ok();

            // Ensure schema exists early.
            let _ = storage::open(&db_path);
            let loaded = settings::load(&settings_path);

            app.manage(AppState {
                db_path,
                audio_dir,
                settings_path,
                settings: Mutex::new(loaded),
                recorder: Mutex::new(None),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            is_recording,
            start_recording,
            stop_recording,
            list_sessions,
            get_session_segments,
            rename_session,
            delete_session,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
