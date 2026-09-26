//! %APPDATA%\MeetingRecorder\config.toml

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const DEFAULT_STT_MODEL: &str = "scribe_v2";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// ElevenLabs API key; ELEVENLABS_API_KEY is used when this is empty.
    pub elevenlabs_api_key: Option<String>,
    /// ElevenLabs STT model id.
    pub stt_model: Option<String>,
    /// ISO 639 language code; None or "auto" detects it.
    pub language: Option<String>,
    /// Folder that holds one folder per meeting.
    pub meetings_dir: Option<PathBuf>,
    /// Your own name, used for the mic speaker when there is one voice.
    pub your_name: Option<String>,
    /// Find several voices per side with Nemotron (default true).
    pub diarize: Option<bool>,
}

impl Config {
    pub fn path() -> PathBuf {
        crate::paths::config_dir().join("config.toml")
    }

    pub fn load() -> Config {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn api_key(&self) -> Option<String> {
        self.elevenlabs_api_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .or_else(|| std::env::var("ELEVENLABS_API_KEY").ok())
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty())
    }

    pub fn stt_model(&self) -> String {
        self.stt_model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_STT_MODEL.to_owned())
    }

    pub fn language(&self) -> Option<String> {
        self.language
            .clone()
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty() && l != "auto")
    }

    pub fn meetings_dir(&self) -> PathBuf {
        self.meetings_dir
            .clone()
            .unwrap_or_else(crate::paths::default_meetings_dir)
    }

    pub fn diarize(&self) -> bool {
        self.diarize.unwrap_or(true)
    }
}
