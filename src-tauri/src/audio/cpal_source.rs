//! Audio capture via `cpal`.
//!
//! * **Microphone** — works on every platform using the default input device.
//! * **System audio (loopback)** —
//!   * Windows: WASAPI loopback on the default output device.
//!   * Linux: a PulseAudio/PipeWire `*.monitor` source.
//!   * macOS: not supported here (cpal has no loopback on macOS) — see
//!     `macos_system.rs` (ScreenCaptureKit).
//!
//! Each source resamples its input to canonical 16 kHz mono f32 and appends it to
//! a shared ring buffer that the mixer drains.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;

use super::resample;

/// Shared 16 kHz mono ring buffer, written by a source and drained by the mixer.
pub type SharedBuf = Arc<Mutex<VecDeque<f32>>>;

/// Hard cap so a stalled consumer can't grow a buffer without bound (~30 s).
const MAX_BUFFERED: usize = resample::TARGET_RATE as usize * 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Microphone,
    SystemAudio,
}

/// Append already-canonical (16 kHz mono) samples to a source buffer, trimming
/// the oldest data if the consumer has fallen behind. Shared with other capture
/// backends (e.g. ScreenCaptureKit on macOS).
pub fn push(buf: &SharedBuf, samples: Vec<f32>) {
    if samples.is_empty() {
        return;
    }
    let mut b = buf.lock();
    b.extend(samples);
    if b.len() > MAX_BUFFERED {
        let overflow = b.len() - MAX_BUFFERED;
        b.drain(0..overflow);
    }
}

fn push_canonical(buf: &SharedBuf, samples: Vec<f32>) {
    push(buf, samples);
}

/// Resolve the capture device for a given source kind.
fn resolve_device(kind: SourceKind) -> Result<cpal::Device> {
    let host = cpal::default_host();
    match kind {
        SourceKind::Microphone => host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default microphone found")),
        SourceKind::SystemAudio => resolve_loopback_device(&host),
    }
}

#[cfg(target_os = "windows")]
fn resolve_loopback_device(host: &cpal::Host) -> Result<cpal::Device> {
    // On WASAPI, building an *input* stream on the default *output* device
    // captures the system mix (loopback).
    host.default_output_device()
        .ok_or_else(|| anyhow!("no default output device for loopback"))
}

#[cfg(target_os = "linux")]
fn resolve_loopback_device(host: &cpal::Host) -> Result<cpal::Device> {
    // PulseAudio / PipeWire expose each sink's loopback as a `*.monitor` input.
    for device in host.input_devices()? {
        if let Ok(name) = device.name() {
            if name.contains("monitor") || name.contains("Monitor") {
                return Ok(device);
            }
        }
    }
    Err(anyhow!(
        "no monitor (loopback) source found; ensure PulseAudio/PipeWire exposes a *.monitor input"
    ))
}

#[cfg(target_os = "macos")]
fn resolve_loopback_device(_host: &cpal::Host) -> Result<cpal::Device> {
    Err(anyhow!(
        "system-audio loopback is captured via ScreenCaptureKit on macOS, not cpal"
    ))
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn resolve_loopback_device(_host: &cpal::Host) -> Result<cpal::Device> {
    Err(anyhow!("system-audio loopback is not supported on this platform"))
}

/// On Windows, the loopback config comes from the output side.
#[cfg(target_os = "windows")]
fn default_config(device: &cpal::Device, kind: SourceKind) -> Result<cpal::SupportedStreamConfig> {
    match kind {
        SourceKind::SystemAudio => device
            .default_output_config()
            .context("default output (loopback) config"),
        SourceKind::Microphone => device.default_input_config().context("default input config"),
    }
}

#[cfg(not(target_os = "windows"))]
fn default_config(device: &cpal::Device, _kind: SourceKind) -> Result<cpal::SupportedStreamConfig> {
    device.default_input_config().context("default input config")
}

/// Start capturing `kind` into `buf`. Returns a join handle for the worker thread
/// that owns the cpal stream; the thread exits when `stop` is set.
pub fn spawn(kind: SourceKind, buf: SharedBuf, stop: Arc<AtomicBool>) -> Result<JoinHandle<()>> {
    // Resolve device + config up front so failures surface to the caller
    // (Device / SupportedStreamConfig are Send; the Stream itself is not).
    let device = resolve_device(kind)?;
    let supported = default_config(&device, kind)?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.config();
    let src_rate = config.sample_rate.0;
    let channels = config.channels;

    let label = format!("{:?}", kind);
    let handle = std::thread::Builder::new()
        .name(format!("granola-capture-{label}"))
        .spawn(move || {
            let buf_cb = buf.clone();
            let err_label = label.clone();
            let err_fn = move |e| log::warn!("[{err_label}] stream error: {e}");

            let stream_res = match sample_format {
                cpal::SampleFormat::F32 => device.build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        push_canonical(&buf_cb, resample::to_canonical(data, channels, src_rate));
                    },
                    err_fn,
                    None,
                ),
                cpal::SampleFormat::I16 => device.build_input_stream(
                    &config,
                    move |data: &[i16], _| {
                        let f: Vec<f32> = data.iter().map(|s| *s as f32 / 32768.0).collect();
                        push_canonical(&buf_cb, resample::to_canonical(&f, channels, src_rate));
                    },
                    err_fn,
                    None,
                ),
                cpal::SampleFormat::U16 => device.build_input_stream(
                    &config,
                    move |data: &[u16], _| {
                        let f: Vec<f32> =
                            data.iter().map(|s| (*s as f32 - 32768.0) / 32768.0).collect();
                        push_canonical(&buf_cb, resample::to_canonical(&f, channels, src_rate));
                    },
                    err_fn,
                    None,
                ),
                other => {
                    log::error!("[{label}] unsupported sample format: {other:?}");
                    return;
                }
            };

            let stream = match stream_res {
                Ok(s) => s,
                Err(e) => {
                    log::error!("[{label}] failed to build input stream: {e}");
                    return;
                }
            };
            if let Err(e) = stream.play() {
                log::error!("[{label}] failed to start stream: {e}");
                return;
            }
            log::info!("[{label}] capturing @ {src_rate} Hz, {channels} ch");

            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            // Stream is dropped here, stopping capture.
            log::info!("[{label}] stopped");
        })
        .context("spawn capture thread")?;

    Ok(handle)
}
