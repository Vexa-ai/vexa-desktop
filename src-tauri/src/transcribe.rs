//! HTTP client for the Vexa transcription service.
//!
//! Mirrors the contract the Vexa bot uses (`services/vexa-bot/.../transcription-client.ts`):
//! POST a 16-bit PCM WAV as multipart/form-data to
//! `{base_url}/v1/audio/transcriptions` (OpenAI-compatible) and parse the
//! `verbose_json` response.

use std::io::Cursor;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;

use crate::audio::resample::TARGET_RATE;

#[derive(Clone, Debug)]
pub struct TranscriptionConfig {
    /// Base URL of the transcription service, e.g. `http://localhost:8083`.
    pub base_url: String,
    /// API token sent as `X-API-Key` and `Authorization: Bearer`.
    pub api_token: String,
    /// Optional language hint (ISO code). Empty = auto-detect.
    pub language: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub language: Option<String>,
}

/// Encode mono 16 kHz f32 samples as an in-memory 16-bit PCM WAV.
fn encode_wav(samples: &[f32]) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        let mut w = hound::WavWriter::new(&mut cursor, spec).context("wav writer")?;
        for &s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?;
        }
        w.finalize().context("finalize wav")?;
    }
    Ok(cursor.into_inner())
}

pub fn build_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()
        .context("build http client")
}

fn endpoint(base: &str) -> String {
    format!(
        "{}/v1/audio/transcriptions",
        base.trim_end_matches('/')
    )
}

/// Transcribe one chunk. Retries a few times on transient/busy responses.
pub fn transcribe_chunk(
    client: &reqwest::blocking::Client,
    cfg: &TranscriptionConfig,
    samples: &[f32],
) -> Result<Vec<Segment>> {
    let wav = encode_wav(samples)?;
    let url = endpoint(&cfg.base_url);

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..3u32 {
        let part = reqwest::blocking::multipart::Part::bytes(wav.clone())
            .file_name("audio.wav")
            .mime_str("audio/wav")?;
        let mut form = reqwest::blocking::multipart::Form::new()
            .part("file", part)
            .text("model", "whisper-1")
            .text("response_format", "verbose_json")
            .text("temperature", "0");
        if let Some(lang) = &cfg.language {
            if !lang.is_empty() {
                form = form.text("language", lang.clone());
            }
        }

        let mut req = client.post(&url).multipart(form);
        if !cfg.api_token.is_empty() {
            req = req
                .header("X-API-Key", &cfg.api_token)
                .bearer_auth(&cfg.api_token);
        }

        match req.send() {
            Ok(resp) => {
                let status = resp.status();
                if status.as_u16() == 503 || status.as_u16() == 429 {
                    // Service busy — back off and retry.
                    last_err = Some(anyhow!("transcription service busy ({status})"));
                    std::thread::sleep(Duration::from_millis(500 * (attempt as u64 + 1)));
                    continue;
                }
                if !status.is_success() {
                    let body = resp.text().unwrap_or_default();
                    return Err(anyhow!("transcription failed: {status}: {body}"));
                }
                let json: serde_json::Value = resp.json().context("parse json response")?;
                return Ok(parse_segments(&json));
            }
            Err(e) => {
                last_err = Some(anyhow!("request error: {e}"));
                std::thread::sleep(Duration::from_millis(400 * (attempt as u64 + 1)));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("transcription failed after retries")))
}

fn parse_segments(json: &serde_json::Value) -> Vec<Segment> {
    let top_lang = json
        .get("language")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut out = Vec::new();
    if let Some(arr) = json.get("segments").and_then(|v| v.as_array()) {
        for seg in arr {
            let text = seg
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if text.is_empty() {
                continue;
            }
            out.push(Segment {
                start: seg.get("start").and_then(|v| v.as_f64()).unwrap_or(0.0),
                end: seg.get("end").and_then(|v| v.as_f64()).unwrap_or(0.0),
                text,
                language: seg
                    .get("language")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| top_lang.clone()),
            });
        }
    }

    // Fallback: some responses only carry top-level `text`.
    if out.is_empty() {
        if let Some(text) = json.get("text").and_then(|v| v.as_str()) {
            let text = text.trim().to_string();
            if !text.is_empty() {
                let dur = json.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
                out.push(Segment {
                    start: 0.0,
                    end: dur,
                    text,
                    language: top_lang,
                });
            }
        }
    }
    out
}
