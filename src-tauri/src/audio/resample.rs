//! Lightweight audio conversion helpers.
//!
//! Everything downstream of capture works in **16 kHz mono f32**, which is what
//! Whisper-style transcription expects. These helpers convert arbitrary capture
//! formats (interleaved, multi-channel, any sample rate) into that canonical form.

pub const TARGET_RATE: u32 = 16_000;

/// Down-mix interleaved multi-channel samples to mono by averaging channels.
pub fn to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    if ch == 1 {
        return interleaved.to_vec();
    }
    let frames = interleaved.len() / ch;
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut acc = 0.0f32;
        for c in 0..ch {
            acc += interleaved[f * ch + c];
        }
        out.push(acc / ch as f32);
    }
    out
}

/// Resample a mono signal from `src_rate` to 16 kHz using linear interpolation.
///
/// This is stateless per-block. For speech recognition the tiny discontinuities
/// at block boundaries are inaudible/irrelevant, and it keeps the pipeline simple
/// and dependency-free.
pub fn resample_mono(input: &[f32], src_rate: u32) -> Vec<f32> {
    if input.is_empty() {
        return Vec::new();
    }
    if src_rate == TARGET_RATE {
        return input.to_vec();
    }
    let ratio = TARGET_RATE as f64 / src_rate as f64;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    if out_len == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;
        let a = input[idx.min(input.len() - 1)];
        let b = input[(idx + 1).min(input.len() - 1)];
        out.push(a + (b - a) * frac);
    }
    out
}

/// Convert any interleaved f32 capture block to canonical 16 kHz mono.
pub fn to_canonical(interleaved: &[f32], channels: u16, src_rate: u32) -> Vec<f32> {
    let mono = to_mono(interleaved, channels);
    resample_mono(&mono, src_rate)
}
