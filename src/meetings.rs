//! The meetings folder: one folder per meeting, newest first.
//!
//! A meeting folder holds mic.ogg, computer.ogg, session.json (capture),
//! meeting.json (transcript and speakers) and transcript.md.

use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone};

use crate::capture::Session;
use crate::types::Meeting;

#[derive(Clone, Debug)]
pub struct Entry {
    pub dir: PathBuf,
    pub title: String,
    pub started_at_unix_ms: i64,
    pub duration_ms: i64,
    pub transcribed: bool,
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
    format!("Meeting {}", local_time(started_at_unix_ms, "%Y-%m-%d %H:%M"))
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
    save(dir, &meeting)?;
    let stamp: String = dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.chars().take_while(|c| c.is_ascii_digit()).collect())
        .unwrap_or_default();
    let safe: String = title
        .chars()
        .map(|c| if r#"<>:"/\|?*"#.contains(c) || c.is_control() { '_' } else { c })
        .collect();
    let name = if stamp.is_empty() { safe } else { format!("{stamp} {safe}") };
    let target = dir.with_file_name(name.trim_end_matches(['.', ' ']));
    if target == dir {
        return Ok(target);
    }
    if target.exists() {
        return Err(format!("{} already exists", target.display()));
    }
    std::fs::rename(dir, &target).map_err(|e| format!("could not rename the folder: {e}"))?;
    Ok(target)
}
