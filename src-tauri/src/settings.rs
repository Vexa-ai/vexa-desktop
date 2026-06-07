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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            endpoint_url: "http://localhost:8083".to_string(),
            api_token: String::new(),
            language: String::new(),
            capture_mic: true,
            capture_system: true,
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
