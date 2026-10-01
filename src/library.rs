//! File operations for deleting and archiving completed meetings.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

#[cfg(test)]
use std::io::Read;

use crate::types::Meeting;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    DeleteAudio,
    Archive,
}

pub fn output_name(started_at_unix_ms: i64, title: &str) -> String {
    let date = Local
        .timestamp_millis_opt(started_at_unix_ms)
        .single()
        .map(|time| time.format("%Y-%m-%d %H%M").to_string())
        .unwrap_or_else(|| "Unknown".into());
    let title = sanitize_title(title);
    format!("{date} {title}")
}

fn sanitize_title(title: &str) -> String {
    let mut safe: String = title
        .chars()
        .map(|character| {
            if r#"<>:"/\\|?*"#.contains(character) || character.is_control() {
                '_'
            } else {
                character
            }
        })
        .collect();
    safe = safe.trim().trim_end_matches(['.', ' ']).to_owned();
    if safe.is_empty() {
        safe = "Meeting".into();
    }
    let stem = safe.split('.').next().unwrap_or_default();
    if matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        safe.insert(0, '_');
    }
    safe
}

fn meeting_name(meeting: &Meeting) -> String {
    let title = if meeting.title.trim().is_empty() {
        crate::meetings::default_title(meeting.started_at_unix_ms)
    } else {
        meeting.title.clone()
    };
    output_name(meeting.started_at_unix_ms, &title)
}

fn available_path(directory: &Path, name: &str, extension: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let mut suffix = 1u32;
    loop {
        let leaf = if suffix == 1 {
            format!("{name}.{extension}")
        } else {
            format!("{name} ({suffix}).{extension}")
        };
        let path = directory.join(leaf);
        if !path.exists() {
            return Ok(path);
        }
        suffix = suffix
            .checked_add(1)
            .ok_or_else(|| "Too many files with the same meeting name".to_string())?;
    }
}

pub fn delete_audio(meeting_dir: &Path, transcripts_dir: &Path) -> Result<Option<PathBuf>, String> {
    delete_audio_with(meeting_dir, transcripts_dir, |path| {
        trash::delete(path).map_err(|error| error.to_string())
    })
}

fn delete_audio_with(
    meeting_dir: &Path,
    transcripts_dir: &Path,
    trash_meeting: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<Option<PathBuf>, String> {
    let meeting = crate::meetings::load(meeting_dir);
    let transcript_exists = meeting
        .as_ref()
        .is_some_and(|meeting| !meeting.utterances.is_empty())
        && meeting_dir.join("transcript.md").is_file();
    let transcript_output = if transcript_exists {
        let meeting = meeting
            .as_ref()
            .ok_or("Transcript exists but meeting metadata is invalid")?;
        let output = available_path(transcripts_dir, &meeting_name(meeting), "md")?;
        let markdown = crate::transcript::to_markdown(meeting);
        write_new_file(&output, markdown.as_bytes())?;
        Some(output)
    } else {
        None
    };
    trash_meeting(meeting_dir).map_err(|error| {
        if let Some(output) = &transcript_output {
            let _ = fs::remove_file(output);
        }
        format!("Could not move meeting to the Recycle Bin: {error}")
    })?;
    Ok(transcript_output)
}

pub fn archive(meeting_dir: &Path, archive_dir: &Path) -> Result<PathBuf, String> {
    let meeting =
        crate::meetings::load(meeting_dir).ok_or("Meeting metadata is missing or invalid")?;
    let name = meeting_name(&meeting);
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let temporary = archive_dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let output = available_path(archive_dir, &name, "zip")?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for filename in ["mic.ogg", "computer.ogg"] {
            add_file(&mut zip, meeting_dir, filename, options)?;
        }
        if !meeting.utterances.is_empty() && meeting_dir.join("transcript.md").is_file() {
            add_file(&mut zip, meeting_dir, "transcript.md", options)?;
        }
        add_file(&mut zip, meeting_dir, "meeting.json", options)?;
        let file = zip.finish().map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        verify_archive(&temporary)?;
        fs::hard_link(&temporary, &output).map_err(|error| error.to_string())?;
        fs::remove_file(&temporary).map_err(|error| error.to_string())?;
        fs::remove_dir_all(meeting_dir).map_err(|error| {
            format!(
                "Archive is verified at {}; could not remove original meeting: {error}",
                output.display()
            )
        })?;
        Ok(output)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_new_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(contents)
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())
}

fn add_file(
    zip: &mut ZipWriter<File>,
    meeting_dir: &Path,
    filename: &str,
    options: SimpleFileOptions,
) -> Result<(), String> {
    let path = meeting_dir.join(filename);
    let mut source =
        File::open(&path).map_err(|error| format!("Could not read {}: {error}", path.display()))?;
    zip.start_file(filename, options)
        .map_err(|error| error.to_string())?;
    io::copy(&mut source, zip).map_err(|error| error.to_string())?;
    Ok(())
}

fn verify_archive(path: &Path) -> Result<(), String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let mut archive = ZipArchive::new(file).map_err(|error| error.to_string())?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        if entry.compression() != CompressionMethod::Stored {
            return Err(format!("Unexpected compression for {}", entry.name()));
        }
        io::copy(&mut entry, &mut io::sink()).map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Meeting, Side, Speaker, Utterance};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sottovoce-library-{}-{}",
                std::process::id(),
                TEST_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(root: &Path, title: &str, transcript: bool) -> PathBuf {
        let dir = root.join("meeting");
        fs::create_dir_all(&dir).unwrap();
        let meeting = Meeting {
            title: title.into(),
            started_at_unix_ms: 1_700_000_000_000,
            duration_ms: 4200,
            language: Some("en".into()),
            speakers: vec![Speaker {
                id: "you-1".into(),
                name: "Me".into(),
                side: Side::Mic,
            }],
            utterances: if transcript {
                vec![Utterance {
                    speaker: "you-1".into(),
                    side: Side::Mic,
                    start_ms: 0,
                    end_ms: 1000,
                    text: "hello".into(),
                }]
            } else {
                vec![]
            },
            ..Default::default()
        };
        crate::meetings::save(&dir, &meeting).unwrap();
        fs::write(dir.join("mic.ogg"), b"mic bytes").unwrap();
        fs::write(dir.join("computer.ogg"), b"system bytes").unwrap();
        if !transcript {
            fs::remove_file(dir.join("transcript.md")).unwrap();
        }
        dir
    }

    #[test]
    fn output_names_sanitize_and_add_collision_suffixes() {
        let root = TempDir::new();
        let name = output_name(1_700_000_000_000, "Call: <planning>? ");
        assert!(name.ends_with("Call_ _planning__"));
        let first = available_path(&root.0, &name, "zip").unwrap();
        fs::write(&first, b"existing").unwrap();
        let second = available_path(&root.0, &name, "zip").unwrap();
        assert_eq!(
            second.file_name().unwrap().to_string_lossy(),
            format!("{name} (2).zip")
        );
    }

    #[test]
    fn delete_exports_transcript_then_trashes_meeting() {
        let root = TempDir::new();
        let dir = fixture(&root.0, "Renamed title", true);
        let transcripts = root.0.join("transcripts");
        let output = delete_audio_with(&dir, &transcripts, |path| {
            fs::remove_dir_all(path).map_err(|error| error.to_string())
        })
        .unwrap()
        .unwrap();
        assert!(output.is_file());
        let markdown = fs::read_to_string(output).unwrap();
        assert!(markdown.contains("# Renamed title"));
        assert!(markdown.contains("hello"));
        assert!(!dir.exists());
    }

    #[test]
    fn delete_without_transcript_does_not_export_markdown() {
        let root = TempDir::new();
        let dir = fixture(&root.0, "No transcript", false);
        assert_eq!(
            delete_audio_with(&dir, &root.0.join("transcripts"), |path| {
                fs::remove_dir_all(path).map_err(|error| error.to_string())
            })
            .unwrap(),
            None
        );
        assert!(!dir.exists());
    }

    #[test]
    fn delete_failure_keeps_meeting_and_removes_partial_export() {
        let root = TempDir::new();
        let dir = fixture(&root.0, "Keep on failure", true);
        let transcripts = root.0.join("transcripts");
        assert!(
            delete_audio_with(
                &dir,
                &transcripts,
                |_| Err("simulated trash failure".into())
            )
            .is_err()
        );
        assert!(dir.exists());
        assert_eq!(fs::read_dir(transcripts).unwrap().count(), 0);
    }

    #[test]
    fn archive_contains_required_files_and_verified_stored_entries() {
        let root = TempDir::new();
        let dir = fixture(&root.0, "Archive me", true);
        let output = archive(&dir, &root.0.join("archive")).unwrap();
        assert!(!dir.exists());
        let mut zip = ZipArchive::new(File::open(output).unwrap()).unwrap();
        let mut names: Vec<_> = (0..zip.len())
            .map(|index| zip.by_index(index).unwrap().name().to_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["computer.ogg", "meeting.json", "mic.ogg", "transcript.md"]
        );
        let mut mic = String::new();
        zip.by_name("mic.ogg")
            .unwrap()
            .read_to_string(&mut mic)
            .unwrap();
        assert_eq!(mic, "mic bytes");
    }

    #[test]
    fn archive_failure_keeps_original_meeting() {
        let root = TempDir::new();
        let dir = fixture(&root.0, "Missing track", false);
        fs::remove_file(dir.join("computer.ogg")).unwrap();
        assert!(archive(&dir, &root.0.join("archive")).is_err());
        assert!(dir.exists());
        assert!(dir.join("meeting.json").exists());
        assert_eq!(fs::read_dir(root.0.join("archive")).unwrap().count(), 0);
    }

    #[test]
    fn archive_without_transcript_omits_placeholder_markdown() {
        let root = TempDir::new();
        let dir = fixture(&root.0, "No transcript", false);
        let output = archive(&dir, &root.0.join("archive")).unwrap();
        let zip = ZipArchive::new(File::open(output).unwrap()).unwrap();
        let names: Vec<_> = (0..zip.len())
            .map(|index| zip.file_names().nth(index).unwrap().to_owned())
            .collect();
        assert_eq!(names.len(), 3);
        assert!(!names.iter().any(|name| name == "transcript.md"));
    }
}
