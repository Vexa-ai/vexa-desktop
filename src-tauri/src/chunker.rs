//! Splits the mixed 16 kHz mono stream into utterance-sized chunks for
//! transcription, and simultaneously writes the full recording to a WAV file.
//!
//! Chunking strategy (simple + robust):
//!   * emit when the buffer reaches `MAX_CHUNK` seconds, OR
//!   * emit when the buffer has at least `MIN_CHUNK` seconds and the tail has
//!     gone quiet (so we cut on natural pauses, not mid-word).
//! Near-silent chunks are written to the recording but skipped for transcription.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;

use crate::audio::resample::TARGET_RATE;
use crate::audio::Source;

const SR: usize = TARGET_RATE as usize;
const MIN_CHUNK: usize = SR * 3 / 2; // 1.5 s — lower latency on short utterances
const MAX_CHUNK: usize = SR * 8; // 8 s — bounds worst-case latency on monologues
const SILENCE_TAIL: usize = SR * 2 / 5; // 0.4 s of trailing quiet triggers a cut
const SILENCE_RMS: f32 = 0.012; // below this is "quiet"
const DROP_RMS: f32 = 0.006; // chunks quieter than this aren't transcribed

/// An utterance-sized chunk handed to the transcription worker.
pub struct Chunk {
    pub samples: Vec<f32>,
    /// Offset of this chunk from the start of the recording, in seconds.
    pub start_secs: f64,
    /// Which audio source produced it.
    pub source: Source,
}

fn rms(s: &[f32]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    let sum: f32 = s.iter().map(|x| x * x).sum();
    (sum / s.len() as f32).sqrt()
}

/// Shared running duration (seconds) of the recording, updated as samples arrive.
pub type DurationHandle = Arc<Mutex<f64>>;

/// Spawn the chunker thread. Consumes `mixed_rx` until it closes, writes all audio
/// to `wav_path`, and forwards voiced chunks to `chunk_tx`.
pub fn spawn(
    mixed_rx: Receiver<Vec<f32>>,
    chunk_tx: Sender<Chunk>,
    wav_path: PathBuf,
    duration: DurationHandle,
    source: Source,
) -> Result<JoinHandle<()>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&wav_path, spec)
        .with_context(|| format!("create wav file {}", wav_path.display()))?;

    let handle = std::thread::Builder::new()
        .name(format!("granola-chunker-{}", source.as_str()))
        .spawn(move || {
            let mut buffer: Vec<f32> = Vec::with_capacity(MAX_CHUNK);
            let mut cursor: u64 = 0; // global index of buffer[0]

            // Auto-gain the MIC only: many mics record well below system-audio
            // level, so the fixed silence/drop thresholds fragment or drop the
            // user's words. We track a slow running speech level and boost toward
            // a target so the thresholds (and Whisper) see a healthy signal.
            // System audio is already at level — left untouched.
            let agc = matches!(source, Source::Mic);
            let mut agc_level: f32 = 0.0; // EMA of voiced RMS
            const AGC_TARGET: f32 = 0.06;
            const AGC_MAX_GAIN: f32 = 12.0;

            let emit = |buffer: &mut Vec<f32>, end: usize, cursor: &mut u64| {
                let chunk: Vec<f32> = buffer.drain(0..end).collect();
                let start_secs = *cursor as f64 / SR as f64;
                *cursor += chunk.len() as u64;
                if rms(&chunk) >= DROP_RMS {
                    let _ = chunk_tx.send(Chunk {
                        samples: chunk,
                        start_secs,
                        source,
                    });
                }
            };

            for mut block in mixed_rx.iter() {
                // Mic auto-gain: adapt the level estimate on voiced frames only
                // (so silence doesn't blow up the gain), then apply a clamped
                // boost in-place before anything downstream sees the samples.
                if agc {
                    let r = rms(&block);
                    if r > 0.003 {
                        agc_level = if agc_level <= 0.0 { r } else { 0.9 * agc_level + 0.1 * r };
                    }
                    if agc_level > 1e-4 {
                        let gain = (AGC_TARGET / agc_level).clamp(1.0, AGC_MAX_GAIN);
                        if gain > 1.01 {
                            for s in block.iter_mut() {
                                *s = (*s * gain).clamp(-1.0, 1.0);
                            }
                        }
                    }
                }
                // Persist every sample to the recording.
                for &s in &block {
                    let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    let _ = writer.write_sample(v);
                }
                buffer.extend_from_slice(&block);
                let elapsed = (cursor + buffer.len() as u64) as f64 / SR as f64;
                {
                    let mut d = duration.lock();
                    *d = d.max(elapsed);
                }

                // Hard cap: emit fixed-size chunks while oversized.
                while buffer.len() >= MAX_CHUNK {
                    emit(&mut buffer, MAX_CHUNK, &mut cursor);
                }
                // Soft cut on a trailing pause.
                if buffer.len() >= MIN_CHUNK {
                    let tail = &buffer[buffer.len() - SILENCE_TAIL..];
                    if rms(tail) < SILENCE_RMS {
                        let end = buffer.len();
                        emit(&mut buffer, end, &mut cursor);
                    }
                }
            }

            // Channel closed: flush the remainder as a final chunk.
            if buffer.len() >= SR / 3 {
                let end = buffer.len();
                emit(&mut buffer, end, &mut cursor);
            }
            if let Err(e) = writer.finalize() {
                log::error!("[chunker] failed to finalize wav: {e}");
            }
            log::info!("[chunker] done");
        })
        .context("spawn chunker thread")?;

    Ok(handle)
}
