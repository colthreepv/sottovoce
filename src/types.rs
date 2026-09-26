//! Types shared by capture, diarization, speech-to-text, transcript building
//! and the UI. Times are milliseconds; unless noted otherwise they are relative
//! to the start of the track they came from.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;

use serde::{Deserialize, Serialize};

/// Which recorded track something came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// The local microphone: "You".
    Mic,
    /// Windows render loopback: everyone on the call.
    Computer,
}

impl Side {
    pub fn file_name(self) -> &'static str {
        match self {
            Side::Mic => "mic.ogg",
            Side::Computer => "computer.ogg",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Side::Mic => "You",
            Side::Computer => "Remote",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WordKind {
    Word,
    Spacing,
    /// Non-speech markers such as "(laughter)".
    AudioEvent,
}

/// One word (or spacing / audio event) from speech-to-text, relative to the
/// start of its track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub kind: WordKind,
}

/// A stretch of one diarized speaker within one track. `speaker` counts from 0
/// in the order voices are first heard.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub start_ms: i64,
    pub end_ms: i64,
    pub speaker: usize,
}

/// Everything known about one side, the input of transcript building.
#[derive(Clone, Debug)]
pub struct SideInput {
    pub side: Side,
    pub words: Vec<Word>,
    /// Diarization of this track; empty means a single speaker.
    pub turns: Vec<Turn>,
    /// Where this track starts on the meeting timeline.
    pub offset_ms: i64,
}

/// A person in the meeting. `id` is stable (you-1, remote-2, ...);
/// `name` is what the user sees and can rename.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Speaker {
    pub id: String,
    pub name: String,
    pub side: Side,
}

/// One paragraph of the conversation, on the meeting timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Utterance {
    pub speaker: String,
    pub side: Side,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

/// meeting.json in a meeting folder.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Meeting {
    pub title: String,
    pub started_at_unix_ms: i64,
    pub duration_ms: i64,
    /// ISO 639 code reported or requested; None for automatic.
    pub language: Option<String>,
    pub stt_model: String,
    pub speakers: Vec<Speaker>,
    pub utterances: Vec<Utterance>,
}

impl Meeting {
    pub fn speaker_name(&self, id: &str) -> String {
        self.speakers
            .iter()
            .find(|s| s.id == id)
            .map_or_else(|| id.to_owned(), |s| s.name.clone())
    }
}

/// Progress of a long job, sent to whoever shows it.
#[derive(Clone, Debug)]
pub enum Event {
    /// A new step starts, e.g. "Finding speakers (mic)".
    Stage(String),
    /// Progress of the current stage, 0.0..=1.0.
    Progress(f64),
    /// A line worth showing in a log.
    Log(String),
}

pub type Events = Sender<Event>;
pub type Abort = Arc<AtomicBool>;
pub const CANCELLED: &str = "cancelled";
