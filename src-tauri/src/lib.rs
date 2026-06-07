//! Granola — local meeting recorder + transcription client for Vexa.
//!
//! Captures microphone + system audio, mixes to 16 kHz mono, chunks on pauses,
//! transcribes each chunk via the Vexa transcription service, and stores
//! sessions + transcripts locally (SQLite + WAV). The frontend drives recording
//! and renders live transcripts streamed over Tauri events.

mod audio;
mod chunker;
mod claude;
mod recorder;
mod settings;
mod storage;
mod transcribe;
mod vault;

use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_opener::OpenerExt;

use recorder::RecorderHandle;
use settings::Settings;
use storage::{ChatRow, SegmentRow, SessionRow};
use vault::VaultStatus;

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

// ---- Knowledge vault + Claude Code ----

#[derive(Serialize)]
struct VaultClaude {
    vault: Option<VaultStatus>,
    claude_version: Option<String>,
    claude_path: Option<String>,
    claude_error: Option<String>,
}

fn opt(s: &str) -> Option<&str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[tauri::command]
fn get_vault_status(state: State<AppState>) -> VaultClaude {
    let s = state.settings.lock().clone();
    let vault = if s.vault_path.is_empty() {
        None
    } else {
        Some(vault::status(Path::new(&s.vault_path)))
    };
    let (claude_version, claude_path, claude_error) = match claude::detect(opt(&s.claude_path)) {
        Ok(i) => (Some(i.version), Some(i.path), None),
        Err(e) => (None, None, Some(e.to_string())),
    };
    VaultClaude {
        vault,
        claude_version,
        claude_path,
        claude_error,
    }
}

#[tauri::command]
fn setup_vault(state: State<AppState>, path: String) -> Result<VaultStatus, String> {
    let status = vault::setup(Path::new(&path)).map_err(|e| format!("{e:#}"))?;
    let mut s = { state.settings.lock().clone() };
    s.vault_path = path;
    settings::save(&state.settings_path, &s).map_err(|e| e.to_string())?;
    *state.settings.lock() = s;
    Ok(status)
}

#[tauri::command]
fn process_meeting(app: AppHandle, state: State<AppState>, session_id: String) -> Result<(), String> {
    let s = state.settings.lock().clone();
    if s.vault_path.is_empty() {
        return Err("Set up a knowledge folder in Settings first.".into());
    }
    let vault_path = PathBuf::from(&s.vault_path);
    let info = claude::detect(opt(&s.claude_path)).map_err(|e| e.to_string())?;

    let conn = state.db()?;
    let session = storage::list_sessions(&conn)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|x| x.id == session_id)
        .ok_or_else(|| "session not found".to_string())?;
    let segments = storage::get_segments(&conn, &session_id).map_err(|e| e.to_string())?;
    if segments.is_empty() {
        return Err("This session has no transcript to process.".into());
    }
    let tpath = vault::write_transcript(
        &vault_path,
        &session_id,
        &session.title,
        &session.started_at,
        &segments,
    )
    .map_err(|e| e.to_string())?;
    let rel = tpath
        .strip_prefix(&vault_path)
        .unwrap_or(&tpath)
        .to_string_lossy()
        .to_string();
    let template = std::fs::read_to_string(vault_path.join(".vexa/prompts/process-meeting.md"))
        .unwrap_or_else(|_| {
            "Process the transcript at {{transcript}} following CLAUDE.md. Upsert entities, write a report in reports/, link them, and summarize files created vs updated.".into()
        });
    let prompt = template.replace("{{transcript}}", &rel);

    let now = chrono::Utc::now().to_rfc3339();
    let _ = storage::insert_chat(
        &conn,
        &session_id,
        "user",
        "▶ Process meeting into the knowledge graph",
        &now,
    );
    let resume = storage::get_claude_session(&conn, &session_id).ok().flatten();

    claude::spawn_run(
        app,
        claude::RunOpts {
            claude_path: info.path,
            vault: vault_path,
            db_path: state.db_path.clone(),
            app_session: session_id,
            prompt,
            model: s.claude_model.clone(),
            resume_sid: resume,
            max_budget: s.max_budget_usd,
        },
    );
    Ok(())
}

#[tauri::command]
fn chat_send(
    app: AppHandle,
    state: State<AppState>,
    session_id: String,
    message: String,
) -> Result<(), String> {
    let s = state.settings.lock().clone();
    if s.vault_path.is_empty() {
        return Err("Set up a knowledge folder in Settings first.".into());
    }
    let vault_path = PathBuf::from(&s.vault_path);
    let info = claude::detect(opt(&s.claude_path)).map_err(|e| e.to_string())?;

    let conn = state.db()?;
    let now = chrono::Utc::now().to_rfc3339();
    storage::insert_chat(&conn, &session_id, "user", &message, &now).map_err(|e| e.to_string())?;
    let resume = storage::get_claude_session(&conn, &session_id).ok().flatten();

    let prompt = if resume.is_some() {
        message
    } else {
        let rel = format!(".vexa/transcripts/{session_id}.md");
        format!(
            "You are working in this Obsidian knowledge vault (see CLAUDE.md). The current meeting transcript is at {rel} (read it if relevant).\n\n{message}"
        )
    };

    claude::spawn_run(
        app,
        claude::RunOpts {
            claude_path: info.path,
            vault: vault_path,
            db_path: state.db_path.clone(),
            app_session: session_id,
            prompt,
            model: s.claude_model.clone(),
            resume_sid: resume,
            max_budget: s.max_budget_usd,
        },
    );
    Ok(())
}

#[tauri::command]
fn get_chat(state: State<AppState>, id: String) -> Result<Vec<ChatRow>, String> {
    let conn = state.db()?;
    storage::get_chat(&conn, &id).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_in_obsidian(app: AppHandle, file: String) -> Result<(), String> {
    let uri = vault::obsidian_uri(Path::new(&file));
    app.opener()
        .open_url(uri, None::<&str>)
        .map_err(|e| e.to_string())
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
        .plugin(tauri_plugin_dialog::init())
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
            get_vault_status,
            setup_vault,
            process_meeting,
            chat_send,
            get_chat,
            open_in_obsidian,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
