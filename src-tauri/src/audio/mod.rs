//! Audio capture.
//!
//! Each source (microphone, system audio) is captured **separately** into its own
//! 16 kHz mono ring buffer and transcribed independently, so every transcript
//! segment keeps its source (mic vs system). A per-source "pacer" drains the
//! buffer at wall-clock speed and forwards fixed blocks to that source's chunker.

pub mod cpal_source;
pub mod macos_system;
pub mod resample;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{anyhow, Result};
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use serde::Serialize;

use cpal_source::{SharedBuf, SourceKind};

/// Logical origin of an audio stream / transcript segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Mic,
    System,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Mic => "mic",
            Source::System => "system",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CaptureConfig {
    pub microphone: bool,
    pub system_audio: bool,
}

pub struct Sources {
    pub handles: Vec<JoinHandle<()>>,
    /// Non-fatal warnings (e.g. a requested source that couldn't start).
    pub warnings: Vec<String>,
    /// The sources that actually started, each with its ring buffer.
    pub active: Vec<(Source, SharedBuf)>,
}

fn new_buf() -> SharedBuf {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Pop up to `n` samples from `buf`, padding with silence if fewer are available.
fn drain_padded(buf: &SharedBuf, n: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(n);
    let mut b = buf.lock();
    let take = n.min(b.len());
    for _ in 0..take {
        out.push(b.pop_front().unwrap());
    }
    out.resize(n, 0.0);
    out
}

/// Start capture for the requested sources. Does **not** mix — returns each
/// active source's buffer so the caller can transcribe them independently.
pub fn start_sources(config: CaptureConfig, stop: Arc<AtomicBool>) -> Result<Sources> {
    let mut handles = Vec::new();
    let mut warnings = Vec::new();
    let mut active = Vec::new();

    if config.microphone {
        let buf = new_buf();
        match cpal_source::spawn(SourceKind::Microphone, buf.clone(), stop.clone()) {
            Ok(h) => {
                handles.push(h);
                active.push((Source::Mic, buf));
            }
            Err(e) => warnings.push(format!("microphone unavailable: {e}")),
        }
    }

    if config.system_audio {
        let buf = new_buf();
        let res = if cfg!(target_os = "macos") {
            macos_system::spawn(buf.clone(), stop.clone())
        } else {
            cpal_source::spawn(SourceKind::SystemAudio, buf.clone(), stop.clone())
        };
        match res {
            Ok(h) => {
                handles.push(h);
                active.push((Source::System, buf));
            }
            Err(e) => warnings.push(format!("system audio unavailable: {e}")),
        }
    }

    if active.is_empty() {
        return Err(anyhow!(
            "no audio sources could be started: {}",
            warnings.join("; ")
        ));
    }

    Ok(Sources {
        handles,
        warnings,
        active,
    })
}

/// Drain a single source buffer at wall-clock speed, forwarding fixed 16 kHz
/// mono blocks to `out_tx`. Exits (after a final flush) when `stop` is set.
pub fn spawn_pacer(
    buf: SharedBuf,
    out_tx: Sender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    label: &str,
) -> Result<JoinHandle<()>> {
    let name = format!("granola-pacer-{label}");
    let handle = std::thread::Builder::new().name(name).spawn(move || {
        let rate = resample::TARGET_RATE as f64;
        let start = Instant::now();
        let mut emitted: u64 = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let stopping = stop.load(Ordering::Relaxed);

            let target = (start.elapsed().as_secs_f64() * rate) as u64;
            let mut n = target.saturating_sub(emitted) as usize;
            if stopping {
                n = n.max(buf.lock().len());
            }
            if n == 0 {
                if stopping {
                    break;
                }
                continue;
            }

            let block = drain_padded(&buf, n);
            emitted += n as u64;
            if out_tx.send(block).is_err() {
                break;
            }
            if stopping {
                break;
            }
        }
    })?;
    Ok(handle)
}
