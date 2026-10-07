//! Where things live on disk.

use std::path::PathBuf;

pub const APP_DIR: &str = "Sottovoce";
const LEGACY_APP_DIR: &str = "MeetingRecorder";

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
        .or_else(|| std::env::var_os("MEETING_RECORDER_CONFIG_DIR").filter(|path| !path.is_empty()))
        .map(PathBuf::from);
    let new_base = dirs::config_dir().unwrap_or_else(std::env::temp_dir);
    let old_base = new_base.join(LEGACY_APP_DIR);
    match override_path {
        Some(path) => path,
        None => {
            let new_dir = new_base.join(APP_DIR);
            let _ = migrate_config(&new_dir.join("config.toml"), &old_base.join("config.toml"));
            new_dir
        }
    }
}

/// Copy the legacy settings once while preserving the user's original file.
/// Model caches are intentionally left in place and can be downloaded again.
fn migrate_config(new_config: &std::path::Path, old_config: &std::path::Path) -> std::io::Result<bool> {
    use std::io::Write;
    if new_config.exists() || !old_config.is_file() {
        return Ok(false);
    }
    let parent = new_config.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "config has no parent directory")
    })?;
    std::fs::create_dir_all(parent)?;
    let contents = std::fs::read(old_config)?;
    let mut destination = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(new_config)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(error),
    };
    if let Err(error) = destination.write_all(&contents).and_then(|()| destination.sync_all()) {
        drop(destination);
        let _ = std::fs::remove_file(new_config);
        return Err(error);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::migrate_config;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "sottovoce-paths-test-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ))
    }

    #[test]
    fn copies_legacy_config_only_when_new_config_is_absent() {
        let root = temp_dir();
        let old = root.join("old/MeetingRecorder/config.toml");
        let new = root.join("new/Sottovoce/config.toml");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, b"language = 'en'\n").unwrap();

        assert!(migrate_config(&new, &old).unwrap());
        assert_eq!(std::fs::read(&new).unwrap(), std::fs::read(&old).unwrap());
        assert!(old.exists(), "migration must preserve the legacy config");
        assert!(!migrate_config(&new, &old).unwrap());
        std::fs::write(&new, b"language = 'it'\n").unwrap();
        assert!(!migrate_config(&new, &old).unwrap());
        assert_eq!(std::fs::read(&new).unwrap(), b"language = 'it'\n");

        std::fs::remove_dir_all(root).unwrap();
    }
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
