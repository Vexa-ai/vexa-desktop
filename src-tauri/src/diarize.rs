//! Real-time speaker diarization bridge.
//!
//! Spawns the Node diarizer sidecar (Vexa's pyannote-seg + wespeaker + online
//! clustering), streams the live system-audio f32 frames into its stdin, and
//! reads `{speakerId,startMs,endMs}` label lines from its stdout — emitted to
//! the UI as `diar-label` events so transcript segments can be tagged
//! "Speaker 1/2/3…" by time overlap.
//!
//! Dev: requires Node + the `tools/diarize` deps (auto-detected). Shipping will
//! later bundle the runtime or move to onnxruntime-web in the webview.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

#[derive(Clone, Serialize)]
struct DiarLabel {
    session: String,
    speaker_id: String,
    start: f64,
    end: f64,
}

#[derive(Clone)]
pub struct DiarHandle {
    stdin: Arc<Mutex<Option<std::process::ChildStdin>>>,
    child: Arc<Mutex<Child>>,
}

impl DiarHandle {
    /// Stream one 16 kHz mono f32 block to the diarizer (as little-endian bytes).
    pub fn write_f32(&self, block: &[f32]) {
        let mut g = self.stdin.lock();
        if let Some(w) = g.as_mut() {
            let mut bytes = Vec::with_capacity(block.len() * 4);
            for &s in block {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
            let _ = w.write_all(&bytes);
        }
    }

    pub fn stop(&self) {
        *self.stdin.lock() = None; // EOF → sidecar drains + exits
        let _ = self.child.lock().kill();
    }
}

/// Locate the diarizer tool dir (containing `sidecar.ts`).
pub fn find_dir(explicit: Option<&str>) -> Option<PathBuf> {
    let has = |p: &PathBuf| p.join("sidecar.ts").exists();
    if let Some(e) = explicit {
        let p = PathBuf::from(e);
        if !e.is_empty() && has(&p) {
            return Some(p);
        }
    }
    if let Ok(e) = std::env::var("VEXA_DIARIZER_DIR") {
        let p = PathBuf::from(e);
        if has(&p) {
            return Some(p);
        }
    }
    // Walk up from the executable looking for tools/diarize.
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        for _ in 0..8 {
            if let Some(d) = &dir {
                let cand = d.join("tools/diarize");
                if has(&cand) {
                    return Some(cand);
                }
                dir = d.parent().map(|p| p.to_path_buf());
            }
        }
    }
    None
}

/// Spawn the sidecar for a session. Emits `diar-label` events as labels commit.
pub fn spawn(app: AppHandle, session_id: String, dir: PathBuf) -> Result<DiarHandle> {
    let tsx = dir.join("node_modules/.bin/tsx");
    let sidecar = dir.join("sidecar.ts");
    if !tsx.exists() {
        return Err(anyhow!("diarizer deps missing (run `npm install` in {})", dir.display()));
    }
    let mut child = Command::new(&tsx)
        .arg(&sidecar)
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn diarizer ({})", tsx.display()))?;

    let stdin = child.stdin.take();
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no diarizer stdout"))?;

    // Parse label lines → events.
    let app2 = app.clone();
    let sid = session_id.clone();
    std::thread::Builder::new()
        .name("vexa-diar-read".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                let v: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let speaker = v.get("speakerId").and_then(|x| x.as_str()).unwrap_or("");
                if speaker.is_empty() {
                    continue;
                }
                let start = v.get("startMs").and_then(|x| x.as_f64()).unwrap_or(0.0) / 1000.0;
                let end = v.get("endMs").and_then(|x| x.as_f64()).unwrap_or(0.0) / 1000.0;
                let _ = app2.emit(
                    "diar-label",
                    DiarLabel {
                        session: sid.clone(),
                        speaker_id: speaker.to_string(),
                        start,
                        end,
                    },
                );
            }
        })
        .ok();

    // Drain stderr to logs (READY, model load, errors).
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            for l in BufReader::new(err).lines().map_while(Result::ok) {
                log::info!("[diar] {l}");
            }
        });
    }

    Ok(DiarHandle {
        stdin: Arc::new(Mutex::new(stdin)),
        child: Arc::new(Mutex::new(child)),
    })
}
