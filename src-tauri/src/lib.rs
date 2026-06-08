//! Granola — local meeting recorder + transcription client for Vexa.
//!
//! Captures microphone + system audio, mixes to 16 kHz mono, chunks on pauses,
//! transcribes each chunk via the Vexa transcription service, and stores
//! sessions + transcripts locally (SQLite + WAV). The frontend drives recording
//! and renders live transcripts streamed over Tauri events.

mod audio;
mod chunker;
mod claude;
mod diarize;
mod frames;
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

/// The agent's editable brain, read fresh each run (self-heals if missing).
fn read_agent_md(vault: &Path) -> Option<String> {
    vault::ensure_agent_md(vault)
}

#[derive(Serialize)]
struct StoryRef {
    /// Absolute path to the living report file (for the UI to read/open).
    report_path: String,
    /// Max segment end-time incorporated so far (the next window's `since`).
    processed_until: f64,
    /// Whether new content existed and an update was actually launched.
    ran: bool,
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let t = out.trim_matches('-').to_string();
    if t.is_empty() { "meeting".into() } else { t }
}

/// Incrementally extend the living meeting report from the transcript window
/// after `since_secs`. The agent (fast model) reads the report + the new window
/// and edits the report in place. Routed to the Story tab via the "story" kind.
#[tauri::command]
fn story_update(
    app: AppHandle,
    state: State<AppState>,
    session_id: String,
    since_secs: f64,
) -> Result<StoryRef, String> {
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

    let processed_until = segments.iter().map(|g| g.end).fold(since_secs, f64::max);
    let window: Vec<storage::SegmentRow> = segments
        .into_iter()
        .filter(|g| g.end > since_secs + 0.01)
        .collect();

    let slug = slugify(&session.title);
    let report_rel = format!("reports/{slug}.md");
    let report_abs = vault_path.join(&report_rel);
    let report_path = report_abs.to_string_lossy().to_string();

    if window.is_empty() {
        return Ok(StoryRef {
            report_path,
            processed_until,
            ran: false,
        });
    }

    // Pass the current report + only the new window INLINE; the model returns the
    // full updated report as text (far more reliable than haiku editing a file).
    let existing = std::fs::read_to_string(&report_abs).unwrap_or_default();
    let window_text = vault::attributed(&window);
    let existing_block = if existing.trim().is_empty() {
        "(no report yet — start a new one)".to_string()
    } else {
        existing
    };
    let prompt = format!(
        "# Current report\n\n{existing_block}\n\n\
         # New transcript window (attributed: Me = the user's mic, Others = system audio)\n\n\
         {window_text}\n\n\
         # Task\nReturn the FULL updated meeting report, extending it with the NEW window per \
         your instructions (concise, attributed, [[wikilinks]]). Output ONLY the report \
         markdown — no preamble, no code fences."
    );
    let _ = report_rel; // path is for the UI to persist/open
    let story_md = vault::ensure_story_md(&vault_path);

    claude::spawn_run(
        app,
        claude::RunOpts {
            claude_path: info.path,
            vault: vault_path,
            db_path: state.db_path.clone(),
            app_session: session_id,
            prompt,
            model: "haiku".into(),
            resume_sid: None,
            max_budget: s.max_budget_usd,
            append_system: story_md,
            kind: "story".into(),
            persist: false,
        },
    );
    Ok(StoryRef {
        report_path,
        processed_until,
        ran: true,
    })
}

/// The polish persona: turn a raw attributed transcript window into a clean,
/// readable, attributed story with clickable entities + highlighted numbers.
const POLISH_PROMPT: &str = "\
You maintain a CONDENSED, FIRST-PERSON live meeting transcript as ONE Markdown \
document. You are given the CURRENT DOCUMENT (already condensed) and NEW RAW \
LINES just transcribed. Return the COMPLETE updated document — a holistic file \
edit, not an append.\n\
Rules:\n\
- LANGUAGE: write the ENTIRE document in the SAME LANGUAGE the speakers use in \
  the raw lines. Never translate to English (or any other language) — if they \
  speak Spanish/French/German/etc., the document is in that language.\n\
- Write each turn in the SPEAKER'S OWN FIRST-PERSON VOICE (I / we), as a tight \
  paraphrase of what they actually said — as if they wrote a crisp note. NEVER \
  use third-person reporting verbs (no `discussed`, `praised`, `noted`, `asked`, \
  `acknowledges`, `notes that`). e.g. not \"Praised the proposal\" but \"This \
  proposal is well-prepared — I like the multifunctional approach.\"\n\
- Keep ONLY meaningful content: decisions, facts, figures, asks, positions. DROP \
  procedural/meta chatter that carries no information (\"someone raised their \
  hand\", \"had been muted\", \"thanks\", \"right\", \"yes thanks\") and garbled \
  fragments. If a turn has no substance, omit it entirely.\n\
- Integrate the new lines, then RECONSIDER the whole document: merge adjacent \
  same-speaker turns, unify a speaker's name spelling across the doc when newer \
  context clarifies it, dedupe, and tighten. Compress hard: ~1 line per turn.\n\
- STRUCTURE the document with a live HEADER, reconsidered fully each pass, then \
  the turns. Use these exact heading levels:\n\
    `## <short meeting topic>`\n\
    `### Participants` then one line: `[[Name]], [[Name]], …` (every speaker as a link)\n\
    `### Decisions` then a bullet list of key decisions/outcomes so far \
    (refine/extend each pass; omit this whole section if none yet)\n\
    `### Quick actions` then 2–4 bullets, each a useful next step written as a \
    Markdown link `[<imperative action>](#act)` (e.g. `[Draft follow-up to Tony \
    on benchmarking](#act)`) — plain text in the label, no [[brackets]] inside it\n\
    `---` (divider)\n\
  Then the chronological turns. Use `**bold**` ONLY for numbers/dates/money — \
  never for the section labels.\n\
- Each kept turn: `#### <Speaker> · <mm:ss>` on its own line, the first-person \
  line(s) below. Paraphrase freely for brevity; NEVER invent content or names.\n\
- HIGHLIGHT GENEROUSLY: wrap in [[double brackets]] every substantive term worth \
  clicking to research — named entities (people, companies, products, projects, \
  places, orgs) AND key topics, concepts, domain terms, initiatives, documents, \
  policies, and technical terms (e.g. [[fees and charges]], [[benchmarking]], \
  [[inflation rate]], [[social services]], [[Appendix 3]]). Aim for a few per \
  turn where they exist; skip only common/filler words. Wrap key NUMBERS / dates \
  / money / percentages in **bold**.\n\
- Output ONLY the full Markdown document. Do NOT acknowledge these instructions, \
  add a preamble, heading, or any commentary.\n";

/// Local path for a session's living polished transcript document.
fn story_path(audio_dir: &Path, session_id: &str) -> PathBuf {
    audio_dir.join(format!("{session_id}-story.md"))
}

/// Strip a wrapping ```/```markdown code fence and any preamble before the first
/// heading — LLMs sometimes wrap the doc in a fence, which would render literally.
fn clean_doc(s: &str) -> String {
    let mut t = s.trim();
    if t.starts_with("```") {
        if let Some(nl) = t.find('\n') {
            t = t[nl + 1..].trim_start();
        }
        if let Some(idx) = t.rfind("```") {
            t = t[..idx].trim_end();
        }
    }
    t.trim().to_string()
}

/// Read a session's saved polished story document (empty string if none yet).
#[tauri::command]
fn read_story(state: State<AppState>, session_id: String) -> Result<String, String> {
    let p = story_path(&state.audio_dir, &session_id);
    Ok(std::fs::read_to_string(p).unwrap_or_default())
}

/// Update the living condensed transcript document, persisted to a local file.
/// Reads the current doc from disk, folds in the new raw lines (holistic rewrite,
/// first-person), writes it back, and returns the new doc. The FILE is the source
/// of truth — the UI renders what's on disk.
#[tauri::command]
async fn polish_doc(
    state: State<'_, AppState>,
    session_id: String,
    raw: String,
) -> Result<String, String> {
    let raw = raw.trim().to_string();
    let path = story_path(&state.audio_dir, &session_id);
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if raw.is_empty() {
        return Ok(current);
    }
    let s = state.settings.lock().clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let info = claude::detect(opt(&s.claude_path)).map_err(|e| e.to_string())?;
        let cur = if current.trim().is_empty() {
            "(empty — start fresh)".to_string()
        } else {
            current.trim().to_string()
        };
        let prompt =
            format!("{POLISH_PROMPT}\nCURRENT DOCUMENT:\n{cur}\n\nNEW RAW LINES:\n{raw}\n");
        let raw_out = claude::run_prompt(&info.path, "haiku", &prompt)
            .map_err(|e| format!("{e:#}"))?;
        let out = clean_doc(&raw_out);
        log::info!("[polish] in {} raw line(s) → {} char doc", raw.lines().count(), out.len());
        // Only persist a real document (guard against conversational replies).
        if out.contains("####") || current.trim().is_empty() {
            std::fs::write(&path, &out).map_err(|e| format!("write story: {e}"))?;
            Ok(out)
        } else {
            log::warn!("[polish] non-doc reply, kept prior: {:.120}", out);
            Ok(current) // keep the prior doc
        }
    })
    .await
    .map_err(|e| format!("polish task failed: {e}"))?
}

/// Live, concise in-meeting copilot pass (the "Update" button). Reads the
/// transcript so far and runs the agent (agent.md persona) for an actionable note.
#[tauri::command]
fn update_meeting(app: AppHandle, state: State<AppState>, session_id: String) -> Result<(), String> {
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
        return Err("No transcript yet to update on.".into());
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
    let prompt = format!(
        "LIVE MEETING UPDATE. The meeting-so-far transcript is at {rel}. Follow your \
         agent instructions: give a concise, proactive in-meeting note with hot actions. \
         Read-only unless an action requires writing."
    );
    let resume = storage::get_claude_session(&conn, &session_id).ok().flatten();
    let agent_md = read_agent_md(&vault_path);

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
            append_system: agent_md,
            kind: "chat".into(),
            persist: true,
        },
    );
    Ok(())
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

    let agent_md = read_agent_md(&vault_path);
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
            append_system: agent_md,
            kind: "chat".into(),
            persist: true,
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

    // Make the current transcript available to the agent — it's referenced at
    // `.vexa/transcripts/<id>.md` but is otherwise only written on Process, so a
    // live meeting wouldn't have it. Best-effort refresh each turn.
    if let Ok(sessions) = storage::list_sessions(&conn) {
        if let Some(sess) = sessions.into_iter().find(|x| x.id == session_id) {
            if let Ok(segs) = storage::get_segments(&conn, &session_id) {
                if !segs.is_empty() {
                    let _ = vault::write_transcript(
                        &vault_path,
                        &session_id,
                        &sess.title,
                        &sess.started_at,
                        &segs,
                    );
                }
            }
        }
    }

    let prompt = if resume.is_some() {
        message
    } else {
        let rel = format!(".vexa/transcripts/{session_id}.md");
        format!(
            "You are working in this Obsidian knowledge vault (see CLAUDE.md). The current meeting transcript is at {rel} (read it if relevant).\n\n{message}"
        )
    };

    let agent_md = read_agent_md(&vault_path);
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
            append_system: agent_md,
            kind: "chat".into(),
            persist: true,
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
fn reveal_in_finder(app: AppHandle, file: String) -> Result<(), String> {
    app.opener()
        .reveal_item_in_dir(PathBuf::from(file))
        .map_err(|e| e.to_string())
}

fn vault_root(state: &State<AppState>) -> Result<PathBuf, String> {
    let p = state.settings.lock().vault_path.clone();
    if p.is_empty() {
        return Err("No knowledge folder set up.".into());
    }
    Ok(PathBuf::from(p))
}

#[tauri::command]
fn vault_tree(state: State<AppState>) -> Result<Vec<vault::Node>, String> {
    let root = vault_root(&state)?;
    Ok(vault::tree(&root))
}

#[tauri::command]
fn read_note(state: State<AppState>, path: String) -> Result<String, String> {
    let root = vault_root(&state)?;
    vault::read_note(&root, Path::new(&path)).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn write_note(state: State<AppState>, path: String, content: String) -> Result<(), String> {
    let root = vault_root(&state)?;
    vault::write_note(&root, Path::new(&path), &content).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn resolve_wikilink(state: State<AppState>, name: String) -> Result<Option<String>, String> {
    let root = vault_root(&state)?;
    Ok(vault::resolve_link(&root, &name).map(|p| p.to_string_lossy().to_string()))
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

// ---- Speaker naming from frames (diarization × vision fusion) ----

#[derive(serde::Deserialize)]
struct NameItem {
    label: String,
    frame: String,
}
#[derive(Serialize)]
struct FrameRef {
    ms: u64,
    path: String,
}
#[derive(Serialize)]
struct NameResult {
    label: String,
    name: String,
}

#[tauri::command]
fn list_frames(state: State<AppState>, session_id: String) -> Result<Vec<FrameRef>, String> {
    let dir = state.audio_dir.join(format!("{session_id}-frames"));
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(ms) = name.strip_suffix(".jpg").and_then(|s| s.parse::<u64>().ok()) {
                v.push(FrameRef {
                    ms,
                    path: e.path().to_string_lossy().to_string(),
                });
            }
        }
    }
    v.sort_by_key(|f| f.ms);
    Ok(v)
}

/// All display captures for one moment: the main `<ms>.jpg` plus any
/// per-display siblings `<ms>-d2.jpg`, `<ms>-d3.jpg`, … (multi-monitor setups).
fn sibling_frames(main: &Path) -> Vec<PathBuf> {
    let mut out = vec![main.to_path_buf()];
    if let (Some(stem), Some(dir)) =
        (main.file_stem().and_then(|s| s.to_str()), main.parent())
    {
        let prefix = format!("{stem}-d");
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut sibs: Vec<PathBuf> = rd
                .flatten()
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    n.starts_with(&prefix) && n.ends_with(".jpg")
                })
                .map(|e| e.path())
                .collect();
            sibs.sort();
            out.extend(sibs);
        }
    }
    out
}

/// Split a frame into overlapping 2×2 quadrant tiles so on-tile name labels
/// survive the vision Read tool's downscale. Returns the saved tile paths, or
/// the original frame if it's small or anything fails. ~20% overlap means a
/// label cut by one seam still appears whole in a neighbouring tile.
fn tile_frame(src: &Path, out_dir: &Path, stem: &str) -> Vec<PathBuf> {
    let img = match image::open(src) {
        Ok(i) => i,
        Err(e) => {
            log::warn!("[name] tile open {} failed: {e}", src.display());
            return vec![src.to_path_buf()];
        }
    };
    let (w, h) = (img.width(), img.height());
    if w < 1600 && h < 1600 {
        return vec![src.to_path_buf()]; // already legible whole
    }
    std::fs::create_dir_all(out_dir).ok();
    // Adaptive grid: aim for ~1500px tiles so each survives the vision Read
    // tool's ~1568px downscale near-1:1 (keeps small gallery name labels sharp).
    // Big 4K screens get a finer grid; tiles overlap ~30% so a label split by a
    // seam still appears whole somewhere.
    let cols = ((w as f32 / 1500.0).round() as u32).clamp(1, 4);
    let rows = ((h as f32 / 1500.0).round() as u32).clamp(1, 4);
    let tw = ((w as f32 / cols as f32) * 1.30).min(w as f32) as u32;
    let th = ((h as f32 / rows as f32) * 1.30).min(h as f32) as u32;
    let pos = |i: u32, n: u32, span: u32, tile: u32| -> u32 {
        if n <= 1 { 0 } else { ((span - tile) as f32 * (i as f32 / (n - 1) as f32)) as u32 }
    };
    let mut out = Vec::new();
    let mut idx = 0;
    for r in 0..rows {
        for c in 0..cols {
            let x = pos(c, cols, w, tw);
            let y = pos(r, rows, h, th);
            let tile = img.crop_imm(x, y, tw, th);
            let p = out_dir.join(format!("{stem}-{idx}.jpg"));
            if tile.save(&p).is_ok() {
                out.push(p);
            }
            idx += 1;
        }
    }
    if out.is_empty() {
        vec![src.to_path_buf()]
    } else {
        out
    }
}

/// Async wrapper: grab settings on the calling thread, then run the blocking
/// Claude vision call on a worker so the webview/main thread never freezes.
#[tauri::command]
async fn name_speakers(
    state: State<'_, AppState>,
    items: Vec<NameItem>,
) -> Result<Vec<NameResult>, String> {
    if items.is_empty() {
        return Ok(vec![]);
    }
    let s = state.settings.lock().clone();
    tauri::async_runtime::spawn_blocking(move || name_speakers_blocking(s, items))
        .await
        .map_err(|e| format!("name task failed: {e}"))?
}

fn name_speakers_blocking(s: Settings, items: Vec<NameItem>) -> Result<Vec<NameResult>, String> {
    let info = claude::detect(opt(&s.claude_path)).map_err(|e| e.to_string())?;
    let frames_dir = Path::new(&items[0].frame)
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "bad frame path".to_string())?;

    // Tile each frame into overlapping quadrants so small Zoom/Meet name labels
    // survive the vision Read tool's downscale (full-screen frames are ~3456px,
    // downscaled to ~1568px → tiny labels become illegible; tiles ~stay sharp).
    let tiles_dir = frames_dir.join(".tiles");
    std::fs::remove_dir_all(&tiles_dir).ok();
    let mut prompt = String::from(
        "These are screenshots from a video call (Zoom / Google Meet / Teams). Each speaker's \
         screenshot is split into overlapping quadrant tiles so small text stays legible — Read \
         ALL of a label's tiles to reconstruct the full screen. For each label, identify the NAME \
         of the ACTIVE speaker: the person whose video tile is highlighted with a speaking border \
         at that moment (read the name label on that tile). If you cannot tell, answer UNKNOWN.\n\
         Reply with EXACTLY one line per label, formatted `Label: Name` — nothing else.\n\n",
    );
    for (i, it) in items.iter().enumerate() {
        // One capture may span several displays (`<ms>.jpg` + `<ms>-d2.jpg`…);
        // the call could be on any of them, so tile and pass them all.
        let screens = sibling_frames(Path::new(&it.frame));
        let mut tiles = Vec::new();
        for (j, sc) in screens.iter().enumerate() {
            tiles.extend(tile_frame(sc, &tiles_dir, &format!("s{i}_{j}")));
        }
        prompt.push_str(&format!(
            "{} — tiles from {} screen(s) at one moment, each split into quadrants \
             (the active speaker is somewhere across these):\n",
            it.label,
            screens.len()
        ));
        for t in &tiles {
            prompt.push_str(&format!("  {}\n", t.display()));
        }
    }
    // Pin vision naming to sonnet — reading small Zoom/Meet name labels is an
    // OCR-heavy task that haiku (the fast text model) fails. Independent of the
    // user's claude_model setting (which drives the fast text passes).
    let model = "sonnet";
    log::info!(
        "[name] {} speaker(s), frames_dir={}, model={}",
        items.len(),
        frames_dir.display(),
        model
    );
    let out = claude::run_sync(&info.path, &frames_dir, &frames_dir, model, &prompt)
        .map_err(|e| {
            log::warn!("[name] claude vision failed: {e:#}");
            format!("{e:#}")
        })?;
    log::info!("[name] claude output:\n{}", out.trim());

    // Tolerant parse: for each label, find a line that mentions it and take the
    // text after the label (handles `**Speaker 1:** Name`, `- Speaker 1 — Name`, etc.).
    let mut res = Vec::new();
    for it in &items {
        for line in out.lines() {
            let clean = line.replace(['*', '#', '`'], "");
            let clean = clean.trim().trim_start_matches("- ").trim();
            if let Some(rest) = clean.strip_prefix(it.label.as_str()) {
                let name = rest
                    .trim_start_matches([':', ' ', '-', '—', '\t'])
                    .trim()
                    .to_string();
                if !name.is_empty() {
                    res.push(NameResult {
                        label: it.label.clone(),
                        name,
                    });
                }
                break;
            }
        }
    }
    log::info!("[name] parsed {} name(s)", res.len());
    Ok(res)
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
            update_meeting,
            story_update,
            polish_doc,
            read_story,
            chat_send,
            get_chat,
            open_in_obsidian,
            reveal_in_finder,
            vault_tree,
            read_note,
            write_note,
            resolve_wikilink,
            list_frames,
            name_speakers,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
