//! User settings, persisted as JSON in the app config directory.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Base URL of the Vexa transcription service (no trailing path).
    pub endpoint_url: String,
    /// API token for the transcription service.
    pub api_token: String,
    /// Language hint (ISO code). Empty string = auto-detect.
    pub language: String,
    /// Capture the microphone.
    pub capture_mic: bool,
    /// Capture system audio (loopback / ScreenCaptureKit).
    pub capture_system: bool,

    // ---- Knowledge vault + Claude Code ----
    /// Path to the knowledge vault (Obsidian folder). Empty = not set up.
    pub vault_path: String,
    /// Claude model alias for processing/chat (e.g. "sonnet", "opus").
    pub claude_model: String,
    /// Explicit path to the `claude` binary. Empty = auto-detect.
    pub claude_path: String,
    /// Optional per-run spend cap passed to `claude --max-budget-usd`.
    pub max_budget_usd: Option<f64>,

    // ---- Screen frames (for speaker naming / diarization fusion) ----
    /// Capture periodic screen frames while recording.
    pub capture_frames: bool,
    /// Seconds between captured frames.
    pub frame_interval_secs: u64,
    /// Real-time speaker diarization of system audio (Speaker 1/2/3…).
    pub diarize: bool,
    /// Override path to the diarizer tool dir (else auto-detected).
    pub diarizer_dir: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            endpoint_url: "http://localhost:8083".to_string(),
            api_token: String::new(),
            language: String::new(),
            capture_mic: true,
            capture_system: true,
            vault_path: String::new(),
            claude_model: "sonnet".to_string(),
            claude_path: String::new(),
            max_budget_usd: None,
            capture_frames: true,
            frame_interval_secs: 5,
            diarize: true,
            diarizer_dir: String::new(),
        }
    }
}

pub fn load(path: &Path) -> Settings {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

pub fn save(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_string_pretty(settings).context("serialize settings")?;
    std::fs::write(path, json).with_context(|| format!("write settings {}", path.display()))?;
    Ok(())
}
