//! Where things live on disk.

use std::path::PathBuf;

pub const APP_DIR: &str = "MeetingRecorder";

/// %LOCALAPPDATA%\MeetingRecorder: models and caches.
pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR)
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

/// %APPDATA%\MeetingRecorder: config.toml.
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR)
}

/// Documents\Meetings unless the config says otherwise.
pub fn default_meetings_dir() -> PathBuf {
    dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("Meetings")
}
