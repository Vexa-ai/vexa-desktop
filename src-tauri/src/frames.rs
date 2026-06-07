//! Periodic screen-frame capture during a recording.
//!
//! Frames are saved as `<elapsed_ms>.jpg` in a per-session folder, where
//! `elapsed_ms` is milliseconds since the recording started — so frame
//! timestamps align with transcript segment times (seconds from start). These
//! frames feed the later speaker-naming step (diarization clusters × frame at
//! that time → active speaker's name via vision).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Spawn the frame-capture thread. Captures one frame immediately, then every
/// `interval`, until `stop` is set.
pub fn spawn(
    frames_dir: PathBuf,
    interval: Duration,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    std::fs::create_dir_all(&frames_dir)
        .with_context(|| format!("create frames dir {}", frames_dir.display()))?;

    let handle = std::thread::Builder::new()
        .name("granola-frames".into())
        .spawn(move || {
            let start = Instant::now();
            let mut next = Duration::ZERO;
            let mut count = 0u64;
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let elapsed = start.elapsed();
                if elapsed >= next {
                    let ms = elapsed.as_millis();
                    let path = frames_dir.join(format!("{ms}.jpg"));
                    if capture(&path) {
                        count += 1;
                    }
                    next = elapsed + interval;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            log::info!("[frames] captured {count} frame(s) → {}", frames_dir.display());
        })
        .context("spawn frames thread")?;
    Ok(handle)
}

/// Capture the main display to `path`. Returns true on success.
#[cfg(target_os = "macos")]
fn capture(path: &Path) -> bool {
    // `screencapture` is Apple's native tool; runs under our Screen-Recording grant.
    match std::process::Command::new("screencapture")
        .args(["-x", "-t", "jpg"])
        .arg(path)
        .status()
    {
        Ok(s) if s.success() => true,
        Ok(s) => {
            log::warn!("[frames] screencapture exited {s}");
            false
        }
        Err(e) => {
            log::warn!("[frames] screencapture failed: {e}");
            false
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn capture(_path: &Path) -> bool {
    // TODO: Windows (PrintWindow/DXGI) and Linux (grim/scrot) capture.
    false
}
