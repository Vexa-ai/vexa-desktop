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
    window: Option<u32>,
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
                    if capture(&path, window) {
                        count += 1;
                    }
                    next = elapsed + interval;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            let what = window.map(|w| format!("window {w}")).unwrap_or_else(|| "all displays".into());
            log::info!("[frames] captured {count} frame(s) of {what} → {}", frames_dir.display());
        })
        .context("spawn frames thread")?;
    Ok(handle)
}

/// Capture a frame. With a `window` id, grab just that window
/// (`screencapture -l<id>`) — occlusion-independent, so the call need not be
/// visible. Otherwise capture every display (`<ms>.jpg` = display 1, others →
/// `<ms>-d2.jpg`…). Returns true if the frame was saved.
#[cfg(target_os = "macos")]
fn capture(path: &Path, window: Option<u32>) -> bool {
    // `screencapture` is Apple's native tool; runs under our Screen-Recording grant.
    let mut cmd = std::process::Command::new("screencapture");
    cmd.args(["-x", "-t", "jpg"]);
    if let Some(id) = window {
        // -l<id>: capture exactly this window (content composited even if
        // occluded/behind others); -o: omit the window's drop shadow.
        cmd.arg("-o").arg(format!("-l{id}")).arg(path);
    } else {
        cmd.arg(path);
        if let (Some(stem), Some(dir)) =
            (path.file_stem().and_then(|s| s.to_str()), path.parent())
        {
            for d in 2..=4 {
                cmd.arg(dir.join(format!("{stem}-d{d}.jpg")));
            }
        }
    }
    match cmd.status() {
        Ok(s) if s.success() => path.exists(),
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
fn capture(_path: &Path, _window: Option<u32>) -> bool {
    // TODO: Windows (PrintWindow/DXGI) and Linux (grim/scrot) capture.
    false
}
