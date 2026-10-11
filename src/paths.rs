//! Where things live on disk.

use std::path::PathBuf;

pub const APP_DIR: &str = "Sottovoce";

/// %LOCALAPPDATA%\Sottovoce: models and caches (separate from deploy-only builds).
pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR)
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

/// %APPDATA%\Sottovoce: config.toml.
pub fn config_dir() -> PathBuf {
    let override_path = std::env::var_os("SOTTOVOCE_CONFIG_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    override_path.unwrap_or_else(|| {
        dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(APP_DIR)
    })
}

/// Documents\Meetings unless the config says otherwise.
pub fn default_meetings_dir() -> PathBuf {
    dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("Meetings")
}

pub fn default_transcripts_dir() -> PathBuf {
    dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("Meeting Transcriptions")
}

pub fn default_archive_dir() -> PathBuf {
    dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("Meeting Archive")
}
