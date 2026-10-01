//! The meetings folder: one folder per meeting, newest first.
//!
//! A meeting folder holds mic.ogg, computer.ogg, session.json (capture),
//! meeting.json (transcript and speakers) and transcript.md.

use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone};

use crate::capture::Session;
use crate::types::Meeting;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub dir: PathBuf,
    pub title: String,
    pub started_at_unix_ms: i64,
    pub duration_ms: i64,
    pub transcribed: bool,
    pub status: String,
}

pub fn local_time(unix_ms: i64, format: &str) -> String {
    Local
        .timestamp_millis_opt(unix_ms)
        .single()
        .map(|t| t.format(format).to_string())
        .unwrap_or_default()
}

/// A new, not yet existing folder for a meeting starting now.
pub fn new_dir(root: &Path, started_at_unix_ms: i64) -> PathBuf {
    let stamp = local_time(started_at_unix_ms, "%Y%m%d%H%M");
    let mut dir = root.join(format!("{stamp} Meeting"));
    let mut n = 2;
    while dir.exists() {
        dir = root.join(format!("{stamp} Meeting {n}"));
        n += 1;
    }
    dir
}

pub fn default_title(started_at_unix_ms: i64) -> String {
    format!(
        "Meeting {}",
        local_time(started_at_unix_ms, "%Y-%m-%d %H:%M")
    )
}

pub fn load(dir: &Path) -> Option<Meeting> {
    let text = std::fs::read_to_string(dir.join("meeting.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes meeting.json and transcript.md.
pub fn save(dir: &Path, meeting: &Meeting) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(meeting).map_err(|e| e.to_string())?;
    let path = dir.join("meeting.json");
    std::fs::write(&path, json).map_err(|e| format!("{}: {e}", path.display()))?;
    let path = dir.join("transcript.md");
    std::fs::write(&path, crate::transcript::to_markdown(meeting))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// All meetings under `root`, newest first.
pub fn list(root: &Path) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = read
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| entry(&e.path()))
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.started_at_unix_ms));
    entries
}

pub fn entry(dir: &Path) -> Option<Entry> {
    let session = Session::load(dir);
    let meeting = load(dir);
    if session.is_none() && meeting.is_none() {
        return None;
    }
    let started = meeting
        .as_ref()
        .map(|m| m.started_at_unix_ms)
        .filter(|t| *t > 0)
        .or(session.as_ref().map(|s| s.started_at_unix_ms))
        .unwrap_or_default();
    let duration = meeting
        .as_ref()
        .map(|m| m.duration_ms)
        .filter(|d| *d > 0)
        .or_else(|| {
            session
                .as_ref()
                .and_then(|s| Some(s.stopped_at_unix_ms? - s.started_at_unix_ms))
        })
        .unwrap_or_default();
    let title = meeting
        .as_ref()
        .map(|m| m.title.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| default_title(started));
    Some(Entry {
        dir: dir.to_path_buf(),
        title,
        started_at_unix_ms: started,
        duration_ms: duration,
        status: session
            .as_ref()
            .map(|s| s.status.clone())
            .unwrap_or_default(),
        transcribed: meeting.is_some_and(|m| !m.utterances.is_empty()),
    })
}

/// Renames the meeting (title and folder); returns the new folder.
pub fn rename(dir: &Path, title: &str) -> Result<PathBuf, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("the name is empty".into());
    }
    let mut meeting = load(dir).unwrap_or_default();
    meeting.title = title.to_owned();
    let stamp: String = dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.chars().take_while(|c| c.is_ascii_digit()).collect())
        .unwrap_or_default();
    let safe: String = title
        .chars()
        .map(|c| {
            if r#"<>:"/\|?*"#.contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = if stamp.is_empty() {
        safe
    } else {
        format!("{stamp} {safe}")
    };
    let target = dir.with_file_name(name.trim_end_matches(['.', ' ']));
    if target == dir {
        save(dir, &meeting)?;
        return Ok(target);
    }
    if target.exists() {
        return Err(format!("{} already exists", target.display()));
    }
    if target.file_name().is_none() || target.file_name().is_some_and(|n| n == "." || n == "..") {
        return Err("the name is not a valid folder name".into());
    }
    std::fs::rename(dir, &target).map_err(|e| format!("could not rename the folder: {e}"))?;
    if let Err(error) = save(&target, &meeting) {
        let _ = std::fs::rename(&target, dir);
        return Err(error);
    }
    Ok(target)
}

/// Only called for a newly allocated folder, never for pre-existing meetings.
pub fn cleanup_failed_start(dir: &Path) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let files: Vec<_> = read.collect();
    if files.iter().any(|entry| {
        entry.as_ref().map_or(true, |entry| {
            !entry.path().is_file()
                || !matches!(
                    entry.file_name().to_str(),
                    Some("mic.ogg" | "computer.ogg" | "session.json" | ".recorder-pid")
                )
        })
    }) {
        return;
    }
    for entry in files.into_iter().flatten() {
        let _ = std::fs::remove_file(entry.path());
    }
    let _ = std::fs::remove_dir(dir);
}

fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    unsafe {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
            fn GetExitCodeProcess(handle: *mut std::ffi::c_void, code: *mut u32) -> i32;
        }
        let handle = OpenProcess(0x1000, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == 259
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}

/// Runs on a worker because decoding large interrupted tracks may take time.
pub fn recover(root: &Path) {
    let Ok(read) = std::fs::read_dir(root) else {
        return;
    };
    for entry in read.flatten() {
        let dir = entry.path();
        let Some(mut session) = Session::load(&dir) else {
            continue;
        };
        if session.status != "recording" {
            continue;
        }
        let owner = std::fs::read_to_string(dir.join(".recorder-pid"))
            .ok()
            .and_then(|p| p.parse::<u32>().ok());
        if owner.is_some_and(process_alive) {
            continue;
        }
        // Legacy sessions have no owner marker. Protect files held by a running encoder.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            if ["mic.ogg", "computer.ogg"].iter().any(|name| {
                let path = dir.join(name);
                path.exists()
                    && std::fs::OpenOptions::new()
                        .read(true)
                        .share_mode(0)
                        .open(path)
                        .is_err()
            }) {
                continue;
            }
        }
        let mut duration = 0;
        let mut bytes = 0;
        for name in ["mic.ogg", "computer.ogg"] {
            let path = dir.join(name);
            let length = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            bytes += length;
            if length > 0 {
                match decoded_duration(&path) {
                    Ok(ms) => duration = duration.max(ms),
                    Err(error) => session.errors.push(error),
                }
            }
        }
        session.status = if bytes == 0 { "empty" } else { "interrupted" }.into();
        session.stopped_at_unix_ms = Some(session.started_at_unix_ms + duration);
        session
            .errors
            .push("Recovered after the recording process exited without finalizing".into());
        if let Err(error) = session.save(&dir) {
            crate::log_event(&error);
            continue;
        }
        let _ = std::fs::remove_file(dir.join(".recorder-pid"));
        let mut meeting = load(&dir).unwrap_or_else(|| Meeting {
            title: default_title(session.started_at_unix_ms),
            started_at_unix_ms: session.started_at_unix_ms,
            ..Default::default()
        });
        meeting.duration_ms = duration;
        let _ = save(&dir, &meeting);
        crate::log_event(&format!(
            "Recovered {}: {} ({} ms)",
            dir.display(),
            session.status,
            duration
        ));
    }
}

fn decoded_duration(path: &Path) -> Result<i64, String> {
    // Decode to a null sink: bounded memory even for an overnight recording.
    let output = crate::ffmpeg::command()
        .arg("-i")
        .arg(path)
        .args(["-progress", "pipe:1", "-nostats", "-f", "null", "-"])
        .output()
        .map_err(|e| e.to_string())?;
    let duration = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            line.strip_prefix("out_time_us=")
                .and_then(|v| v.parse::<i64>().ok())
        })
        .max()
        .unwrap_or(0)
        / 1000;
    if duration > 0 || output.status.success() {
        Ok(duration)
    } else {
        Err(format!(
            "Cannot recover {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn failed_start_cleanup_preserves_foreign_files() {
        let dir = std::env::temp_dir().join(format!("mr-cleanup-{}", crate::capture::unix_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mic.ogg"), []).unwrap();
        std::fs::write(dir.join("keep.txt"), "user data").unwrap();
        cleanup_failed_start(&dir);
        assert!(dir.join("mic.ogg").exists());
        std::fs::remove_file(dir.join("keep.txt")).unwrap();
        cleanup_failed_start(&dir);
        assert!(!dir.exists());
    }
    #[test]
    fn recovery_marks_zero_byte_recording_empty() {
        let root = std::env::temp_dir().join(format!("mr-recovery-{}", crate::capture::unix_ms()));
        let dir = root.join("meeting");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mic.ogg"), []).unwrap();
        Session {
            status: "recording".into(),
            started_at_unix_ms: 1000,
            ..Default::default()
        }
        .save(&dir)
        .unwrap();
        recover(&root);
        assert_eq!(Session::load(&dir).unwrap().status, "empty");
        assert_eq!(list(&root).len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
