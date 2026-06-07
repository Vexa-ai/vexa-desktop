//! macOS system-audio capture via ScreenCaptureKit.
//!
//! cpal cannot capture system output on macOS, so we use ScreenCaptureKit's audio
//! tap (the same mechanism Granola uses). It needs the "Screen & System Audio
//! Recording" permission, which macOS prompts for on first capture.
//!
//! The real implementation is compiled only with `--features sck`. Without it,
//! `spawn` returns a descriptive error and the app still works with the
//! microphone (and, on Windows/Linux, loopback).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::Result;

use super::cpal_source::SharedBuf;

#[cfg(all(target_os = "macos", feature = "sck"))]
pub fn spawn(buf: SharedBuf, stop: Arc<AtomicBool>) -> Result<JoinHandle<()>> {
    imp::spawn(buf, stop)
}

#[cfg(not(all(target_os = "macos", feature = "sck")))]
pub fn spawn(_buf: SharedBuf, _stop: Arc<AtomicBool>) -> Result<JoinHandle<()>> {
    anyhow::bail!(
        "macOS system-audio capture requires building with `--features sck` (ScreenCaptureKit)"
    )
}

#[cfg(all(target_os = "macos", feature = "sck"))]
mod imp {
    use super::*;
    use std::sync::atomic::Ordering;

    use anyhow::anyhow;
    use core_media_rs::cm_sample_buffer::CMSampleBuffer;
    use screencapturekit::{
        shareable_content::SCShareableContent,
        stream::{
            configuration::SCStreamConfiguration, content_filter::SCContentFilter,
            output_trait::SCStreamOutputTrait, output_type::SCStreamOutputType, SCStream,
        },
    };

    use crate::audio::resample::TARGET_RATE;

    /// Map any Debug error (e.g. CFError) into anyhow.
    fn cferr<E: std::fmt::Debug>(e: E) -> anyhow::Error {
        anyhow!("ScreenCaptureKit error: {e:?}")
    }

    struct Output {
        buf: SharedBuf,
    }

    impl SCStreamOutputTrait for Output {
        fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
            if of_type != SCStreamOutputType::Audio {
                return;
            }
            let list = match sample.get_audio_buffer_list() {
                Ok(l) => l,
                Err(_) => return,
            };
            let bufs = list.buffers();
            if bufs.is_empty() {
                return;
            }
            // Down-mix to mono. SCK delivers planar f32 (one AudioBuffer per
            // channel); a single interleaved buffer is also handled.
            let mut mono: Vec<f32> = Vec::new();
            let nb = bufs.len();
            for ab in bufs {
                let m = buffer_to_mono(ab.number_channels as usize, ab.data());
                if mono.is_empty() {
                    mono = m;
                } else {
                    for i in 0..mono.len().min(m.len()) {
                        mono[i] += m[i];
                    }
                }
            }
            if nb > 1 {
                for v in mono.iter_mut() {
                    *v /= nb as f32;
                }
            }
            // Config requests 16 kHz mono, so no resampling is needed.
            crate::audio::cpal_source::push(&self.buf, mono);
        }
    }

    fn buffer_to_mono(channels: usize, bytes: &[u8]) -> Vec<f32> {
        let ch = channels.max(1);
        let total = bytes.len() / 4;
        let frames = total / ch;
        let mut out = Vec::with_capacity(frames);
        for f in 0..frames {
            let mut acc = 0.0f32;
            for c in 0..ch {
                let idx = (f * ch + c) * 4;
                acc += f32::from_le_bytes([
                    bytes[idx],
                    bytes[idx + 1],
                    bytes[idx + 2],
                    bytes[idx + 3],
                ]);
            }
            out.push(acc / ch as f32);
        }
        out
    }

    pub fn spawn(buf: SharedBuf, stop: Arc<AtomicBool>) -> Result<JoinHandle<()>> {
        // Build the stream on the capture thread so it lives there for its
        // whole lifetime. Surface setup errors via a oneshot channel.
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<()>>(1);
        let handle = std::thread::Builder::new()
            .name("granola-capture-sck".into())
            .spawn(move || match build_stream(buf) {
                Ok(stream) => {
                    let _ = ready_tx.send(Ok(()));
                    log::info!("[SystemAudio/SCK] capturing");
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    let _ = stream.stop_capture();
                    log::info!("[SystemAudio/SCK] stopped");
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })?;

        // Wait briefly for setup so a permission/availability failure surfaces now.
        match ready_rx.recv_timeout(std::time::Duration::from_secs(8)) {
            Ok(Ok(())) => Ok(handle),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(handle), // slow start; assume it's coming up
        }
    }

    fn build_stream(buf: SharedBuf) -> Result<SCStream> {
        let content = SCShareableContent::get().map_err(cferr)?;
        let mut displays = content.displays();
        if displays.is_empty() {
            return Err(anyhow!("no display available for ScreenCaptureKit"));
        }
        let display = displays.remove(0);
        let filter = SCContentFilter::new().with_display_excluding_windows(&display, &[]);

        let mut config = SCStreamConfiguration::new();
        config = config.set_captures_audio(true).map_err(cferr)?;
        config = config.set_sample_rate(TARGET_RATE).map_err(cferr)?;
        config = config.set_channel_count(1).map_err(cferr)?;
        // SCStream requires a valid video size even for audio-only capture.
        config = config.set_width(2).map_err(cferr)?;
        config = config.set_height(2).map_err(cferr)?;

        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(Output { buf }, SCStreamOutputType::Audio);
        stream.start_capture().map_err(cferr)?;
        Ok(stream)
    }
}
