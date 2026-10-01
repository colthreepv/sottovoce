//! Who speaks when, in a single audio file.
//!
//! A recording made by the app has two tracks, so the speaker of every line
//! follows from which track is louder. An imported file has one, so the voices
//! themselves have to be told apart. That is NVIDIA's Nemotron 3 Diarization
//! (see `nemotron.rs`), run locally: it follows up to eight speakers, also when
//! they talk at the same time.

use std::sync::atomic::Ordering;

use crate::types::{Abort, CANCELLED, Event, Events, Turn};

/// Finds the speakers in `samples` (16 kHz mono). `speakers` fixes how many
/// there are; `None` lets the model decide.
pub fn turns(
    samples: &[f32],
    speakers: Option<usize>,
    events: &Events,
    abort: &Abort,
) -> Result<Vec<Turn>, String> {
    let path = crate::nemotron::ensure(events, abort)?;
    if abort.load(Ordering::Relaxed) {
        return Err(CANCELLED.into());
    }
    let _ = events.send(Event::Stage("Finding speakers".into()));
    let _ = events.send(Event::Progress(0.0));
    let mut model = crate::nemotron::Model::load(&path)?;
    let probs = model.probabilities(samples, events, abort)?;
    let raw = segments(&probs, 8);
    let raw = match speakers {
        Some(n) => keep_largest(raw, n),
        // Voices heard for only a few seconds are almost always one of the
        // others on a bad moment (a cough, a laugh, crosstalk).
        None => absorb_small_clusters(raw),
    };
    if abort.load(Ordering::Relaxed) {
        return Err(CANCELLED.into());
    }
    Ok(renumber(raw))
}

/// Stretches where a speaker's probability is over one half, in ms. A frame
/// is 10 ms; pauses under half a second within one speaker are bridged and
/// blips under 0.3 seconds dropped, as the old diarization did.
fn segments(probs: &[f32], speakers: usize) -> Vec<(i64, i64, i32)> {
    let frames = probs.len() / speakers;
    let mut raw = Vec::new();
    for s in 0..speakers {
        let mut runs: Vec<(i64, i64)> = Vec::new();
        let mut start = None;
        for f in 0..=frames {
            let on = f < frames && probs[f * speakers + s] > 0.5;
            match (on, start) {
                (true, None) => start = Some(f),
                (false, Some(from)) => {
                    let (from, to) = (from as i64 * 10, f as i64 * 10);
                    match runs.last_mut() {
                        Some(last) if from - last.1 < 500 => last.1 = to,
                        _ => runs.push((from, to)),
                    }
                    start = None;
                }
                _ => {}
            }
        }
        raw.extend(
            runs.into_iter()
                .filter(|(from, to)| to - from >= 300)
                .map(|(from, to)| (from, to, s as i32)),
        );
    }
    raw
}

/// Keeps the `n` speakers with the most speech; the turns of the others go to
/// the nearest kept speaker.
fn keep_largest(raw: Vec<(i64, i64, i32)>, n: usize) -> Vec<(i64, i64, i32)> {
    let mut spoken = std::collections::HashMap::<i32, i64>::new();
    for (start, end, id) in &raw {
        *spoken.entry(*id).or_default() += end - start;
    }
    let mut ranked: Vec<(i32, i64)> = spoken.into_iter().collect();
    ranked.sort_by_key(|(id, ms)| (-ms, *id));
    let kept: Vec<i32> = ranked.iter().take(n.max(1)).map(|(id, _)| *id).collect();
    reassign(raw, |id| kept.contains(id))
}

/// Gives every cluster with little speech (under 4 seconds, or under 4% of
/// all speech) to the speaker of the nearest turn from a cluster that stays.
fn absorb_small_clusters(raw: Vec<(i64, i64, i32)>) -> Vec<(i64, i64, i32)> {
    let mut spoken = std::collections::HashMap::<i32, i64>::new();
    for (start, end, id) in &raw {
        *spoken.entry(*id).or_default() += end - start;
    }
    let total: i64 = spoken.values().sum();
    let floor = (total * 4 / 100).max(4000);
    reassign(raw, |id| spoken.get(id).is_some_and(|ms| *ms >= floor))
}

/// Moves the turns of every speaker that `keeps` rejects to the speaker of
/// the nearest kept turn.
fn reassign(raw: Vec<(i64, i64, i32)>, keeps: impl Fn(&i32) -> bool) -> Vec<(i64, i64, i32)> {
    let anchors: Vec<(i64, i64, i32)> =
        raw.iter().copied().filter(|(_, _, id)| keeps(id)).collect();
    if anchors.is_empty() {
        return raw;
    }
    raw.iter()
        .map(|&(start, end, id)| {
            if keeps(&id) {
                return (start, end, id);
            }
            let middle = (start + end) / 2;
            let nearest = anchors
                .iter()
                .min_by_key(|(s, e, _)| {
                    if middle < *s {
                        s - middle
                    } else {
                        (middle - e).max(0)
                    }
                })
                .map_or(id, |(_, _, other)| *other);
            (start, end, nearest)
        })
        .collect()
}

/// Sorts the turns and numbers the speakers in the order they are first heard.
fn renumber(mut raw: Vec<(i64, i64, i32)>) -> Vec<Turn> {
    raw.sort_by_key(|(start, _, _)| *start);
    let mut order: Vec<i32> = Vec::new();
    raw.into_iter()
        .map(|(start_ms, end_ms, id)| {
            let speaker = order.iter().position(|o| *o == id).unwrap_or_else(|| {
                order.push(id);
                order.len() - 1
            });
            Turn {
                start_ms,
                end_ms,
                speaker,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speakers_are_numbered_by_first_appearance() {
        let turns = renumber(vec![
            (5000, 6000, 3),
            (0, 1000, 7),
            (2000, 3000, 3),
            (7000, 8000, 7),
        ]);
        let order: Vec<usize> = turns.iter().map(|t| t.speaker).collect();
        assert_eq!(order, vec![0, 1, 1, 0]);
    }
}
