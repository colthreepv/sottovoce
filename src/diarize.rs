//! STUB — owned by the Nemotron builder. Speaker turns within one track.

use crate::types::{Abort, Events, Turn};

/// Finds the speakers in `samples` (16 kHz mono). `speakers` fixes how many
/// there are; None lets the model decide.
pub fn turns(
    _samples: &[f32],
    _speakers: Option<usize>,
    _events: &Events,
    _abort: &Abort,
) -> Result<Vec<Turn>, String> {
    Err("diarization not implemented yet".to_owned())
}

/// The whole track as one speaker.
pub fn single(samples: &[f32]) -> Vec<Turn> {
    vec![Turn {
        start_ms: 0,
        end_ms: (samples.len() * 1000 / 16_000) as i64,
        speaker: 0,
    }]
}
