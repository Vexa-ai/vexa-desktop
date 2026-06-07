//! Recording orchestrator: starts each audio source separately, chunks each one,
//! transcribes every chunk against the Vexa transcription service, persists
//! segments (tagged with their source), and streams them to the UI.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::audio::{self, CaptureConfig};
use crate::chunker;
use crate::settings::Settings;
use crate::storage;
use crate::transcribe::{self, TranscriptionConfig};

/// Event payloads emitted to the frontend.
#[derive(Clone, Serialize)]
pub struct SegmentEvent {
    pub session_id: String,
    pub id: i64,
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub language: Option<String>,
    /// "mic" or "system".
    pub source: String,
}

#[derive(Clone, Serialize)]
pub struct StartedEvent {
    pub session_id: String,
    pub active_sources: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Serialize)]
pub struct StoppedEvent {
    pub session_id: String,
    pub duration_secs: f64,
}

#[derive(Clone, Serialize)]
pub struct ErrorEvent {
    pub session_id: String,
    pub message: String,
}

pub struct RecorderHandle {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    pub session_id: String,
    pub audio_base: PathBuf,
    duration: chunker::DurationHandle,
}

impl RecorderHandle {
    /// Signal all threads to stop, wait for them, finalize the session row, and
    /// emit the stopped event. Returns the recording duration in seconds.
    pub fn stop(mut self, app: &AppHandle, db_path: &std::path::Path) -> f64 {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
        let duration = *self.duration.lock();
        if let Ok(conn) = storage::open(db_path) {
            let ended_at = chrono::Utc::now().to_rfc3339();
            let _ = storage::end_session(&conn, &self.session_id, &ended_at, duration);
        }
        let _ = app.emit(
            "recording-stopped",
            StoppedEvent {
                session_id: self.session_id.clone(),
                duration_secs: duration,
            },
        );
        duration
    }
}

/// Start a new recording session.
pub fn start(
    app: AppHandle,
    db_path: PathBuf,
    audio_dir: PathBuf,
    settings: Settings,
    title: String,
) -> Result<RecorderHandle> {
    std::fs::create_dir_all(&audio_dir).ok();

    let session_id = uuid::Uuid::new_v4().to_string();
    let started_at = chrono::Utc::now().to_rfc3339();
    // Per-source WAVs are written as `{base}-mic.wav` / `{base}-system.wav`.
    let audio_base = audio_dir.join(&session_id);

    {
        let conn = storage::open(&db_path).context("open db")?;
        storage::create_session(
            &conn,
            &session_id,
            &title,
            &started_at,
            &audio_base.to_string_lossy(),
        )?;
    }

    let capture_cfg = CaptureConfig {
        microphone: settings.capture_mic,
        system_audio: settings.capture_system,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let duration: chunker::DurationHandle = Arc::new(Mutex::new(0.0));

    // Start each source independently (no mixing — we keep source attribution).
    let sources = audio::start_sources(capture_cfg, stop.clone())?;
    let mut handles = sources.handles;
    let active_sources: Vec<String> =
        sources.active.iter().map(|(s, _)| s.as_str().to_string()).collect();

    // One shared chunk channel; one transcriber drains all sources.
    let (chunk_tx, chunk_rx) = crossbeam_channel::unbounded::<chunker::Chunk>();

    for (source, buf) in sources.active {
        let (paced_tx, paced_rx) = crossbeam_channel::unbounded::<Vec<f32>>();
        let pacer = audio::spawn_pacer(buf, paced_tx, stop.clone(), source.as_str())?;
        handles.push(pacer);

        let wav_path = audio_dir.join(format!("{session_id}-{}.wav", source.as_str()));
        let chunker_handle = chunker::spawn(
            paced_rx,
            chunk_tx.clone(),
            wav_path,
            duration.clone(),
            source,
        )?;
        handles.push(chunker_handle);
    }
    drop(chunk_tx); // close the channel once all chunkers finish

    let tcfg = TranscriptionConfig {
        base_url: settings.endpoint_url.clone(),
        api_token: settings.api_token.clone(),
        language: if settings.language.is_empty() {
            None
        } else {
            Some(settings.language.clone())
        },
    };
    let worker = spawn_transcriber(app.clone(), db_path.clone(), session_id.clone(), tcfg, chunk_rx)?;
    handles.push(worker);

    let _ = app.emit(
        "recording-started",
        StartedEvent {
            session_id: session_id.clone(),
            active_sources,
            warnings: sources.warnings,
        },
    );

    Ok(RecorderHandle {
        stop,
        handles,
        session_id,
        audio_base,
        duration,
    })
}

fn spawn_transcriber(
    app: AppHandle,
    db_path: PathBuf,
    session_id: String,
    tcfg: TranscriptionConfig,
    chunk_rx: crossbeam_channel::Receiver<chunker::Chunk>,
) -> Result<JoinHandle<()>> {
    let handle = std::thread::Builder::new()
        .name("granola-transcriber".into())
        .spawn(move || {
            let conn = storage::open(&db_path).ok();
            let client = match transcribe::build_client() {
                Ok(c) => c,
                Err(e) => {
                    log::error!("[transcriber] http client: {e:#}");
                    return;
                }
            };

            for chunk in chunk_rx.iter() {
                let source = chunk.source.as_str();
                match transcribe::transcribe_chunk(&client, &tcfg, &chunk.samples) {
                    Ok(segments) => {
                        for seg in segments {
                            let start = chunk.start_secs + seg.start;
                            let end = chunk.start_secs + seg.end;
                            let created_at = chrono::Utc::now().to_rfc3339();
                            let mut id = 0i64;
                            if let Some(conn) = &conn {
                                id = storage::insert_segment(
                                    conn,
                                    &session_id,
                                    start,
                                    end,
                                    &seg.text,
                                    seg.language.as_deref(),
                                    source,
                                    &created_at,
                                )
                                .unwrap_or(0);
                            }
                            let _ = app.emit(
                                "transcript-segment",
                                SegmentEvent {
                                    session_id: session_id.clone(),
                                    id,
                                    start,
                                    end,
                                    text: seg.text,
                                    language: seg.language,
                                    source: source.to_string(),
                                },
                            );
                        }
                    }
                    Err(e) => {
                        log::warn!("[transcriber] {source} chunk failed: {e:#}");
                        let _ = app.emit(
                            "transcript-error",
                            ErrorEvent {
                                session_id: session_id.clone(),
                                message: format!("{e}"),
                            },
                        );
                    }
                }
            }
            log::info!("[transcriber] done");
        })
        .context("spawn transcriber thread")?;
    Ok(handle)
}
