//! %APPDATA%\Sottovoce\config.toml

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const DEFAULT_STT_MODEL: &str = "scribe_v2";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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
    /// Folder for Markdown exports of deleted meetings.
    pub transcripts_dir: Option<PathBuf>,
    /// Folder for archived meeting zip files.
    pub archive_dir: Option<PathBuf>,
    /// Your own name, used for the mic speaker when there is one voice.
    pub your_name: Option<String>,
    /// Find several voices per side with Nemotron (default true).
    pub diarize: Option<bool>,
    /// Transcribe automatically after a recording stops (default false).
    pub auto_transcribe: Option<bool>,
    /// Stable cpal device id, or None to follow the Windows default.
    pub mic_device: Option<String>,
    /// Stable cpal render device id, or None to follow the Windows default.
    pub output_device: Option<String>,
}

impl Config {
    pub fn path() -> PathBuf {
        crate::paths::config_dir().join("config.toml")
    }

    pub fn load() -> Config {
        Self::load_at(&Self::path()).unwrap_or_default()
    }

    pub fn load_at(path: &std::path::Path) -> Result<Self, String> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            use std::io::Write;
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut file) => {
                    file.write_all(DEFAULT_TEMPLATE.as_bytes())
                        .map_err(|e| e.to_string())?;
                    file.sync_all().map_err(|e| e.to_string())?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e.to_string()),
            }
        }
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        toml::from_str(&text).map_err(|e| format!("Invalid config: {e}"))
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_at(&Self::path())
    }

    pub fn save_at(&self, path: &std::path::Path) -> Result<(), String> {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let parent = path.parent().ok_or("config has no parent directory")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let temporary = parent.join(format!(
            ".config.{}.{}.tmp",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.map_err(|e| e.to_string())
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

    pub fn transcripts_dir(&self) -> PathBuf {
        self.transcripts_dir
            .clone()
            .unwrap_or_else(crate::paths::default_transcripts_dir)
    }

    pub fn archive_dir(&self) -> PathBuf {
        self.archive_dir
            .clone()
            .unwrap_or_else(crate::paths::default_archive_dir)
    }

    pub fn auto_transcribe(&self) -> bool {
        self.auto_transcribe.unwrap_or(false)
    }

    pub fn mic_device(&self) -> Option<String> {
        self.mic_device.clone().filter(|id| !id.trim().is_empty())
    }

    pub fn output_device(&self) -> Option<String> {
        self.output_device
            .clone()
            .filter(|id| !id.trim().is_empty())
    }
}

const DEFAULT_TEMPLATE: &str = r#"# Sottovoce settings. Manual edits apply within one second.
# Stored in %APPDATA%\Sottovoce\config.toml.
# Empty key uses ELEVENLABS_API_KEY.
elevenlabs_api_key = ""
stt_model = "scribe_v2"
language = "auto"
# Find multiple speakers with local Nemotron diarization.
diarize = true
# Paid transcription is opt-in.
auto_transcribe = false
# Empty device IDs follow the Windows default.
mic_device = ""
output_device = ""
# Optional settings (uncomment to override):
# your_name = "Valerio"
# meetings_dir = 'C:\Users\Valerio\Documents\Meetings'
# transcripts_dir = 'C:\Users\Valerio\Documents\Meeting Transcriptions'
# archive_dir = 'C:\Users\Valerio\Documents\Meeting Archive'
"#;
