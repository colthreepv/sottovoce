//! Turns ElevenLabs words and Nemotron speaker turns into one conversation.

use std::collections::{HashMap, HashSet};

use crate::types::{Meeting, Side, SideInput, Speaker, Turn, Utterance, Word, WordKind};

const PARAGRAPH_PAUSE_MS: i64 = 3000;
const PARAGRAPH_MAX_MS: i64 = 90_000;

#[derive(Clone, Debug)]
struct Sentence {
    side: Side,
    start_ms: i64,
    end_ms: i64,
    track_end_ms: i64,
    speaker: usize,
    text: String,
}

#[derive(Debug)]
struct BuiltSide {
    side: Side,
    offset_ms: i64,
    sentences: Vec<Sentence>,
    /// Speaker indices in order of their first recognized word or event.
    speaker_order: Vec<(usize, i64)>,
}

#[derive(Debug)]
struct OpenSentence {
    start_ms: i64,
    end_ms: i64,
    track_start_ms: i64,
    text: String,
    votes: HashMap<usize, i64>,
    last_speaker: Option<usize>,
    needs_space: bool,
    has_speech: bool,
}

/// Builds speakers and paragraphs on the meeting timeline from both sides.
pub fn build(sides: &[SideInput], your_name: Option<&str>) -> (Vec<Speaker>, Vec<Utterance>) {
    let mut built: Vec<BuiltSide> = sides.iter().map(build_side).collect();
    let mut ids = HashMap::<(Side, usize), String>::new();
    let mut speakers_at = Vec::<(i64, usize, usize, Speaker)>::new();

    for (side_order, source) in built.iter().enumerate() {
        for (ordinal, (speaker, first_ms)) in source.speaker_order.iter().enumerate() {
            let base = match source.side {
                Side::Mic => "you",
                Side::Computer => "remote",
            };
            let id = format!("{base}-{}", ordinal + 1);
            ids.insert((source.side, *speaker), id.clone());
            let name = if source.speaker_order.len() == 1 {
                if source.side == Side::Mic {
                    your_name
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .unwrap_or("You")
                        .to_owned()
                } else {
                    "Remote".to_owned()
                }
            } else {
                format!("{} {}", source.side.label(), ordinal + 1)
            };
            let first_on_timeline = first_ms.saturating_add(source.offset_ms);
            speakers_at.push((
                first_on_timeline,
                side_order,
                ordinal,
                Speaker {
                    id,
                    name,
                    side: source.side,
                },
            ));
        }
    }

    // Remove microphone text that is a near-simultaneous copy of loopback,
    // then merge nearby sentences from the same voice into paragraphs.
    let mut sentences: Vec<Sentence> = built
        .drain(..)
        .flat_map(|source| source.sentences)
        .collect();
    sentences.sort_by_key(|sentence| sentence.start_ms);
    let keep: Vec<bool> = sentences
        .iter()
        .map(|sentence| sentence.side != Side::Mic || !is_echo(sentence, &sentences))
        .collect();

    let mut utterances = Vec::<Utterance>::new();
    for (sentence, keep) in sentences.into_iter().zip(keep) {
        if !keep {
            continue;
        }
        let Some(speaker_id) = ids.get(&(sentence.side, sentence.speaker)) else {
            continue;
        };
        match utterances.last_mut() {
            Some(last)
                if last.speaker == *speaker_id
                    && (sentence.start_ms - last.end_ms < PARAGRAPH_PAUSE_MS
                        || !ends_sentence(&last.text))
                    && sentence.end_ms - last.start_ms < PARAGRAPH_MAX_MS =>
            {
                if !last.text.chars().last().is_some_and(char::is_whitespace) {
                    last.text.push(' ');
                }
                last.text.push_str(&sentence.text);
                last.end_ms = last.end_ms.max(sentence.end_ms);
            }
            _ => utterances.push(Utterance {
                speaker: speaker_id.clone(),
                side: sentence.side,
                start_ms: sentence.start_ms,
                end_ms: sentence.end_ms,
                text: sentence.text,
            }),
        }
    }

    speakers_at.sort_by_key(|(time, side_order, ordinal, _)| (*time, *side_order, *ordinal));
    let speakers = speakers_at
        .into_iter()
        .map(|(_, _, _, speaker)| speaker)
        .collect();
    (speakers, utterances)
}

fn build_side(input: &SideInput) -> BuiltSide {
    let mut words: Vec<&Word> = input.words.iter().collect();
    words.sort_by_key(|word| word.start_ms);

    let mut sentences: Vec<Sentence> = Vec::new();
    let mut current: Option<OpenSentence> = None;
    let mut first_seen = HashMap::<usize, i64>::new();

    for word in words {
        if word.kind == WordKind::Spacing {
            if let Some(open) = current.as_mut() {
                open.text.push_str(&word.text);
                open.needs_space = false;
            }
            continue;
        }

        let start = word.start_ms;
        let end = word.end_ms.max(start);
        if word.kind == WordKind::Word && is_noise_marker(&word.text) {
            continue;
        }

        let speaker = speaker_at(&input.turns, start, end);
        first_seen
            .entry(speaker)
            .and_modify(|first| *first = (*first).min(start))
            .or_insert(start);

        if word.kind == WordKind::AudioEvent && current.is_none() {
            if let Some(previous) = sentences.last_mut() {
                if start.saturating_sub(previous.track_end_ms) < PARAGRAPH_PAUSE_MS {
                    if !previous
                        .text
                        .chars()
                        .last()
                        .is_some_and(char::is_whitespace)
                    {
                        previous.text.push(' ');
                    }
                    previous.text.push_str(&word.text);
                    previous.end_ms = previous.end_ms.max(end.saturating_add(input.offset_ms));
                    previous.track_end_ms = previous.track_end_ms.max(end);
                    continue;
                }
            }
        }

        if current
            .as_ref()
            .is_some_and(|open| start.saturating_sub(open.end_ms) >= PARAGRAPH_PAUSE_MS)
        {
            flush_sentence(&mut current, input, &mut sentences);
        }

        let open = current.get_or_insert_with(|| OpenSentence {
            start_ms: start,
            end_ms: end,
            track_start_ms: start,
            text: String::new(),
            votes: HashMap::new(),
            last_speaker: None,
            needs_space: false,
            has_speech: false,
        });

        if open.needs_space
            && !open.text.chars().last().is_some_and(char::is_whitespace)
            && !word.text.chars().next().is_some_and(char::is_whitespace)
            && (word.kind == WordKind::AudioEvent || !starts_with_punctuation(&word.text))
        {
            open.text.push(' ');
        }
        open.text.push_str(&word.text);
        open.start_ms = open.start_ms.min(start);
        open.end_ms = open.end_ms.max(end);
        open.last_speaker = Some(speaker);
        open.has_speech |= word.kind != WordKind::AudioEvent;
        for (voice, overlap) in overlap_by_speaker(&input.turns, start, end) {
            *open.votes.entry(voice).or_default() += overlap;
        }
        open.needs_space = true;

        // Long silences and sentence-ending punctuation make phrase boundaries.
        if ends_sentence(&word.text) {
            flush_sentence(&mut current, input, &mut sentences);
        }
    }
    flush_sentence(&mut current, input, &mut sentences);

    let mut speaker_order: Vec<(usize, i64)> = first_seen.into_iter().collect();
    speaker_order.sort_by_key(|(speaker, first)| (*first, *speaker));

    BuiltSide {
        side: input.side,
        offset_ms: input.offset_ms,
        sentences,
        speaker_order,
    }
}

fn flush_sentence(
    current: &mut Option<OpenSentence>,
    input: &SideInput,
    sentences: &mut Vec<Sentence>,
) {
    let Some(open) = current.take() else {
        return;
    };
    let text = open.text.trim().to_owned();
    if text.is_empty()
        || !open.has_speech
        || only_audio_events_or_noise(&text)
        || is_stock_noise_phrase(&text)
    {
        return;
    }

    let speaker = open
        .votes
        .iter()
        .max_by_key(|(speaker, span)| (**span, usize::MAX - **speaker))
        .map_or_else(|| open.last_speaker.unwrap_or(0), |(speaker, _)| *speaker);
    let mut start_ms = open.start_ms.saturating_add(input.offset_ms);
    let end_ms = open.end_ms.saturating_add(input.offset_ms);
    let previous = sentences.last();
    if previous.is_none_or(|previous| previous.speaker != speaker) {
        if let Some(takeover) = turn_start_near(&input.turns, speaker, open.track_start_ms) {
            start_ms = takeover
                .saturating_add(input.offset_ms)
                .max(previous.map_or(0, |previous| previous.start_ms.saturating_add(1)));
        }
    }

    sentences.push(Sentence {
        side: input.side,
        start_ms,
        end_ms: end_ms.max(start_ms.saturating_add(1)),
        track_end_ms: open.end_ms,
        speaker,
        text,
    });
}

/// Returns the turn with the greatest overlap, or the nearest turn for a word
/// that falls in a diarization gap. Empty turns mean one speaker.
fn speaker_at(turns: &[Turn], start_ms: i64, end_ms: i64) -> usize {
    if turns.is_empty() {
        return 0;
    }
    let end_ms = end_ms.max(start_ms.saturating_add(1));
    let mut overlap = HashMap::<usize, i64>::new();
    for turn in turns {
        let shared = turn.end_ms.min(end_ms) - turn.start_ms.max(start_ms);
        if shared > 0 {
            *overlap.entry(turn.speaker).or_default() += shared;
        }
    }
    if let Some((speaker, _)) = overlap
        .into_iter()
        .max_by_key(|(speaker, duration)| (*duration, usize::MAX - *speaker))
    {
        return speaker;
    }
    let middle = start_ms.saturating_add(end_ms.saturating_sub(start_ms) / 2);
    turns
        .iter()
        .min_by_key(|turn| {
            if middle < turn.start_ms {
                turn.start_ms - middle
            } else {
                (middle - turn.end_ms).max(0)
            }
        })
        .map_or(0, |turn| turn.speaker)
}

/// Counts each word's overlap by turn so sentence ownership reflects how much
/// of the spoken sentence each voice actually covered.
fn overlap_by_speaker(turns: &[Turn], start_ms: i64, end_ms: i64) -> HashMap<usize, i64> {
    let end_ms = end_ms.max(start_ms.saturating_add(1));
    let mut overlap = HashMap::<usize, i64>::new();
    for turn in turns {
        let shared = turn.end_ms.min(end_ms) - turn.start_ms.max(start_ms);
        if shared > 0 {
            *overlap.entry(turn.speaker).or_default() += shared;
        }
    }
    if overlap.is_empty() {
        overlap.insert(speaker_at(turns, start_ms, end_ms), end_ms - start_ms);
    }
    overlap
}

/// Finds a speaker turn beginning within 1.5 seconds of the guessed takeover.
fn turn_start_near(turns: &[Turn], speaker: usize, around_ms: i64) -> Option<i64> {
    turns
        .iter()
        .filter(|turn| turn.speaker == speaker && turn.start_ms.abs_diff(around_ms) <= 1500)
        .min_by_key(|turn| turn.start_ms.abs_diff(around_ms))
        .map(|turn| turn.start_ms)
}

fn is_echo(mine: &Sentence, sentences: &[Sentence]) -> bool {
    let own_words = normalized_words(&mine.text);
    if own_words.is_empty() {
        return false;
    }
    let nearby: Vec<Vec<String>> = sentences
        .iter()
        .filter(|other| {
            other.side == Side::Computer
                && other.start_ms < mine.end_ms.saturating_add(2000)
                && mine.start_ms < other.end_ms.saturating_add(2000)
        })
        .map(|other| normalized_words(&other.text))
        .collect();

    if own_words.len() < 3 {
        return nearby
            .iter()
            .any(|words| contains_sequence(words, &own_words));
    }
    let own_trigrams: Vec<String> = own_words.windows(3).map(|words| words.join(" ")).collect();
    let their_trigrams: HashSet<String> = nearby
        .iter()
        .flat_map(|words| words.windows(3).map(|trigram| trigram.join(" ")))
        .collect();
    own_trigrams
        .iter()
        .filter(|trigram| their_trigrams.contains(*trigram))
        .count()
        * 2
        >= own_trigrams.len()
}

fn contains_sequence(haystack: &[String], needle: &[String]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn normalized_words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|word| {
            word.trim_matches(|character: char| !character.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect()
}

fn only_audio_events_or_noise(text: &str) -> bool {
    is_noise_marker(text)
}

/// Removes subtitle credits and other obvious noise stock text. Common
/// conversational phrases such as "thank you" and "okay" remain intact.
fn is_stock_noise_phrase(text: &str) -> bool {
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|character| character.is_alphanumeric() || character.is_whitespace())
        .collect();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.starts_with("subtitles by")
        || normalized.starts_with("subtitle by")
        || normalized == "please subscribe"
        || normalized == "blank audio"
}

fn is_noise_marker(text: &str) -> bool {
    let text = text.trim();
    let wrapped = |open: char, close: char| text.starts_with(open) && text.ends_with(close);
    wrapped('[', ']')
        || wrapped('*', '*')
        || wrapped('(', ')')
        || text.chars().all(|character| !character.is_alphanumeric())
}

fn starts_with_punctuation(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|character| !character.is_alphanumeric() && !character.is_whitespace())
}

/// Whether text ends a sentence, optionally followed by a closing quote/bracket.
fn ends_sentence(text: &str) -> bool {
    text.trim_end()
        .trim_end_matches(['"', '\'', ')', '\u{201d}', '\u{2019}', ']'])
        .ends_with(['.', '?', '!', '\u{2026}'])
}

/// transcript.md for a meeting.
pub fn to_markdown(meeting: &Meeting) -> String {
    let date = (meeting.started_at_unix_ms > 0)
        .then(|| chrono::DateTime::from_timestamp_millis(meeting.started_at_unix_ms))
        .flatten()
        .map(|date| {
            date.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "Unknown".to_owned());
    let language = meeting
        .language
        .as_deref()
        .map(language_name)
        .unwrap_or_else(|| "Unknown".to_owned());
    let mut output = format!("# {}\n\n", meeting.title);
    output.push_str(&format!("- **Date:** {date}\n"));
    output.push_str(&format!("- **Duration:** {}\n", clock(meeting.duration_ms)));
    output.push_str(&format!("- **Language:** {language}\n\n## Transcript\n\n"));
    if meeting.utterances.is_empty() {
        output.push_str("_No speech was recognized._\n");
    }
    for utterance in &meeting.utterances {
        output.push_str(&format!(
            "**[{}] {}:** {}\n",
            clock(utterance.start_ms),
            meeting.speaker_name(&utterance.speaker),
            utterance.text
        ));
    }
    output
}

fn clock(milliseconds: i64) -> String {
    let seconds = milliseconds.max(0) / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn language_name(code: &str) -> String {
    match code.to_ascii_lowercase().as_str() {
        "en" => "English".to_owned(),
        "nl" => "Dutch".to_owned(),
        "de" => "German".to_owned(),
        "fr" => "French".to_owned(),
        "es" => "Spanish".to_owned(),
        "it" => "Italian".to_owned(),
        "pt" => "Portuguese".to_owned(),
        "auto" => "Auto-detect".to_owned(),
        _ => code.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str, start_ms: i64) -> Vec<Word> {
        let mut output = Vec::new();
        let mut offset = 0i64;
        for piece in text.split_inclusive(' ') {
            let clean = piece.trim_end_matches(' ');
            let end = start_ms + offset + 200;
            output.push(Word {
                text: clean.to_owned(),
                start_ms: start_ms + offset,
                end_ms: end,
                kind: WordKind::Word,
            });
            offset += 250;
            if piece.ends_with(' ') {
                output.push(Word {
                    text: " ".to_owned(),
                    start_ms: start_ms + offset,
                    end_ms: start_ms + offset,
                    kind: WordKind::Spacing,
                });
            }
        }
        output
    }

    fn side(side: Side, words: Vec<Word>, turns: Vec<Turn>, offset_ms: i64) -> SideInput {
        SideInput {
            side,
            words,
            turns,
            offset_ms,
        }
    }

    #[test]
    fn interleaves_both_tracks_and_applies_track_offsets() {
        let (speakers, utterances) = build(
            &[
                side(Side::Mic, words("Sure, go ahead.", 2000), Vec::new(), 500),
                side(Side::Computer, words("I can start now.", 0), Vec::new(), 0),
            ],
            Some("Valerio"),
        );
        assert_eq!(speakers.len(), 2);
        assert_eq!(speakers[0].name, "Remote");
        assert_eq!(speakers[1].name, "Valerio");
        assert_eq!(utterances[0].text, "I can start now.");
        assert_eq!(utterances[1].text, "Sure, go ahead.");
        assert_eq!(utterances[1].start_ms, 2500);
    }

    #[test]
    fn numbers_multiple_speakers_in_order_of_first_appearance() {
        let input = side(
            Side::Computer,
            [words("First voice.", 0), words("Second voice.", 2000)].concat(),
            vec![
                Turn {
                    start_ms: 0,
                    end_ms: 1500,
                    speaker: 4,
                },
                Turn {
                    start_ms: 1500,
                    end_ms: 3500,
                    speaker: 9,
                },
            ],
            0,
        );
        let (speakers, utterances) = build(&[input], None);
        assert_eq!(
            speakers
                .iter()
                .map(|speaker| speaker.name.as_str())
                .collect::<Vec<_>>(),
            ["Remote 1", "Remote 2"]
        );
        assert_eq!(speakers[0].id, "remote-1");
        assert_eq!(speakers[1].id, "remote-2");
        assert_eq!(utterances[0].speaker, "remote-1");
        assert_eq!(utterances[1].speaker, "remote-2");
    }

    #[test]
    fn drops_mic_echo_near_the_computer_words_but_keeps_later_repetition() {
        let (speakers, utterances) = build(
            &[
                side(
                    Side::Mic,
                    [
                        words("The review is still pending after four days.", 300),
                        words("The review is still pending, I see.", 10_000),
                    ]
                    .concat(),
                    Vec::new(),
                    0,
                ),
                side(
                    Side::Computer,
                    words("The review is still pending after four days.", 0),
                    Vec::new(),
                    0,
                ),
            ],
            None,
        );
        assert_eq!(speakers.len(), 2);
        assert_eq!(utterances.len(), 2);
        assert_eq!(utterances[0].side, Side::Computer);
        assert_eq!(utterances[1].text, "The review is still pending, I see.");
    }

    #[test]
    fn merges_sentences_until_a_long_pause_and_splits_long_paragraphs() {
        let short = [
            words("First sentence.", 0),
            words("Second sentence.", 1500),
            words("After silence.", 6000),
        ]
        .concat();
        let (_, utterances) = build(&[side(Side::Mic, short, Vec::new(), 0)], None);
        assert_eq!(utterances.len(), 2);
        assert_eq!(utterances[0].text, "First sentence. Second sentence.");
        assert_eq!(utterances[1].text, "After silence.");

        let mut long = words("Start.", 0);
        long.extend(words("Continues.", 90_000));
        let (_, utterances) = build(&[side(Side::Mic, long, Vec::new(), 0)], None);
        assert_eq!(utterances.len(), 2);
    }

    #[test]
    fn keeps_a_sentence_whole_across_a_speaker_change_and_uses_turn_takeover() {
        let input = side(
            Side::Computer,
            words("I think", 0)
                .into_iter()
                .chain(words("we agree.", 1000))
                .collect(),
            vec![
                Turn {
                    start_ms: 0,
                    end_ms: 400,
                    speaker: 0,
                },
                Turn {
                    start_ms: 400,
                    end_ms: 3000,
                    speaker: 1,
                },
            ],
            0,
        );
        let (_, utterances) = build(&[input], None);
        assert_eq!(utterances.len(), 1);
        assert_eq!(utterances[0].text, "I think we agree.");
        assert_eq!(utterances[0].speaker, "remote-2");
        assert_eq!(utterances[0].start_ms, 400);
    }

    #[test]
    fn keeps_audio_events_inline_but_drops_event_only_phrases() {
        let mut input = words("Hello.", 0);
        input.push(Word {
            text: "(laughter)".to_owned(),
            start_ms: 1000,
            end_ms: 1300,
            kind: WordKind::AudioEvent,
        });
        input.push(Word {
            text: "(cough)".to_owned(),
            start_ms: 5000,
            end_ms: 5500,
            kind: WordKind::AudioEvent,
        });
        input.extend(words("(music)", 6000));
        let (_, utterances) = build(&[side(Side::Mic, input, Vec::new(), 0)], None);
        assert_eq!(utterances.len(), 1);
        assert_eq!(utterances[0].text, "Hello. (laughter)");
    }

    #[test]
    fn markdown_has_metadata_and_zero_padded_timestamps() {
        let meeting = Meeting {
            title: "Planning".to_owned(),
            started_at_unix_ms: 0,
            duration_ms: 3_661_000,
            language: Some("en".to_owned()),
            speakers: vec![Speaker {
                id: "you-1".to_owned(),
                name: "You".to_owned(),
                side: Side::Mic,
            }],
            utterances: vec![Utterance {
                speaker: "you-1".to_owned(),
                side: Side::Mic,
                start_ms: 3_661_000,
                end_ms: 3_662_000,
                text: "Ready?".to_owned(),
            }],
            ..Meeting::default()
        };
        let markdown = to_markdown(&meeting);
        assert!(markdown.contains("- **Duration:** 01:01:01"));
        assert!(markdown.contains("- **Language:** English"));
        assert!(markdown.contains("**[01:01:01] You:** Ready?"));
    }

    #[test]
    fn real_stt_fixtures_follow_the_benchmark_truth_when_present() {
        use std::path::{Path, PathBuf};

        fn matching_stt(root: &Path, fixture: &str, side: &str) -> Option<PathBuf> {
            std::fs::read_dir(root)
                .ok()?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                        return false;
                    };
                    let parts: Vec<String> = stem
                        .to_ascii_lowercase()
                        .replace('_', "-")
                        .replace('.', "-")
                        .split('-')
                        .map(str::to_owned)
                        .collect();
                    let fixture_parts: Vec<&str> = fixture.split('-').collect();
                    parts.windows(fixture_parts.len()).any(|window| {
                        window
                            .iter()
                            .map(String::as_str)
                            .eq(fixture_parts.iter().copied())
                    }) && parts.iter().any(|part| part == side)
                })
        }

        fn parse_stt(path: &Path) -> Vec<Word> {
            let json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).expect("read raw ElevenLabs fixture"))
                    .expect("parse raw ElevenLabs fixture");
            json.get("words")
                .and_then(serde_json::Value::as_array)
                .expect("ElevenLabs response has words")
                .iter()
                .map(|word| {
                    let seconds = |name: &str| {
                        (word
                            .get(name)
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(0.0)
                            * 1000.0)
                            .round() as i64
                    };
                    let kind = match word
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("word")
                    {
                        "spacing" => WordKind::Spacing,
                        "audio_event" => WordKind::AudioEvent,
                        _ => WordKind::Word,
                    };
                    Word {
                        text: word
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        start_ms: seconds("start"),
                        end_ms: seconds("end"),
                        kind,
                    }
                })
                .collect()
        }

        fn normalize(text: &str) -> Vec<String> {
            text.split_whitespace()
                .map(|word| {
                    word.trim_matches(|character: char| !character.is_alphanumeric())
                        .to_lowercase()
                })
                .filter(|word| !word.is_empty())
                .collect()
        }

        let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let stt_root = crate_root.join("testdata").join("stt");
        let fixtures_root = crate_root.join("..").join("bench").join("fixtures");
        let Ok(fixture_dirs) = std::fs::read_dir(&fixtures_root) else {
            return;
        };
        for fixture_dir in fixture_dirs.filter_map(Result::ok) {
            if !fixture_dir.path().is_dir() {
                continue;
            }
            let Some(fixture) = fixture_dir.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let mic_path = matching_stt(&stt_root, &fixture, "mic");
            let computer_path = matching_stt(&stt_root, &fixture, "computer");
            let (Some(mic_path), Some(computer_path)) = (mic_path, computer_path) else {
                continue;
            };

            let truth_path = fixture_dir.path().join("truth.json");
            let truth_json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&truth_path).expect("read benchmark truth"))
                    .expect("parse benchmark truth");
            let truth = truth_json
                .get("truth")
                .and_then(serde_json::Value::as_array)
                .expect("benchmark fixture has truth array");

            let mut entries = Vec::<(Side, String, i64, i64, String)>::new();
            for item in truth {
                let side = match item.get("side").and_then(serde_json::Value::as_str) {
                    Some("mic") => Side::Mic,
                    Some("computer") => Side::Computer,
                    _ => continue,
                };
                let seconds = |name: &str| {
                    (item
                        .get(name)
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0)
                        * 1000.0) as i64
                };
                entries.push((
                    side,
                    item.get("speaker")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    seconds("start"),
                    seconds("end"),
                    item.get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ));
            }

            let mut sides = Vec::new();
            for side in [Side::Mic, Side::Computer] {
                let mut names: Vec<(String, i64)> = entries
                    .iter()
                    .filter(|(entry_side, _, _, _, _)| *entry_side == side)
                    .map(|(_, name, start, _, _)| (name.clone(), *start))
                    .collect();
                names.sort_by_key(|(_, start)| *start);
                let mut speaker_order = Vec::<String>::new();
                for (name, _) in names {
                    if !speaker_order.contains(&name) {
                        speaker_order.push(name);
                    }
                }
                let turns = entries
                    .iter()
                    .filter(|(entry_side, _, _, _, _)| *entry_side == side)
                    .map(|(_, name, start, end, _)| Turn {
                        start_ms: *start,
                        end_ms: *end,
                        speaker: speaker_order
                            .iter()
                            .position(|speaker| speaker == name)
                            .expect("truth speaker is indexed")
                            .into(),
                    })
                    .collect();
                let path = if side == Side::Mic {
                    &mic_path
                } else {
                    &computer_path
                };
                sides.push(SideInput {
                    side,
                    words: parse_stt(path),
                    turns,
                    offset_ms: 0,
                });
            }

            let (speakers, utterances) = build(&sides, None);
            let actual: Vec<String> = utterances
                .iter()
                .flat_map(|utterance| normalize(&utterance.text))
                .collect();
            let mut available = HashMap::<String, usize>::new();
            for word in &actual {
                *available.entry(word.clone()).or_default() += 1;
            }
            let expected: Vec<String> = entries
                .iter()
                .flat_map(|(_, _, _, _, text)| normalize(text))
                .collect();
            if !expected.is_empty() {
                let matched = expected
                    .iter()
                    .filter(|word| {
                        let Some(count) = available.get_mut(*word) else {
                            return false;
                        };
                        if *count == 0 {
                            return false;
                        }
                        *count -= 1;
                        true
                    })
                    .count();
                let coverage = matched as f64 / expected.len() as f64;
                assert!(
                    coverage >= 0.60,
                    "{fixture} STT matched only {coverage:.0}% of benchmark words"
                );

                let mut previous = vec![0usize; actual.len() + 1];
                for expected_word in &expected {
                    let mut current = vec![0usize; actual.len() + 1];
                    for (index, actual_word) in actual.iter().enumerate() {
                        current[index + 1] = if expected_word == actual_word {
                            previous[index] + 1
                        } else {
                            current[index].max(previous[index + 1])
                        };
                    }
                    previous = current;
                }
                let ordered_coverage = previous[actual.len()] as f64 / expected.len() as f64;
                assert!(
                    ordered_coverage >= 0.60,
                    "{fixture} STT retained only {ordered_coverage:.0}% of benchmark word order"
                );
            }

            if fixture == "call" {
                assert!(
                    utterances
                        .windows(2)
                        .all(|pair| pair[0].start_ms <= pair[1].start_ms)
                );
                assert_eq!(
                    speakers
                        .iter()
                        .filter(|speaker| speaker.side == Side::Computer)
                        .count(),
                    3,
                    "the call fixture has three remote voices"
                );
                for key in [
                    ["release", "call"],
                    ["theme", "picker"],
                    ["plugin", "review"],
                ] {
                    assert!(
                        actual
                            .windows(2)
                            .any(|pair| { pair[0] == key[0] && pair[1] == key[1] }),
                        "call transcript missed key phrase {} {}",
                        key[0],
                        key[1]
                    );
                }
                let duration_ms = entries
                    .iter()
                    .map(|(_, _, _, end, _)| *end)
                    .max()
                    .unwrap_or(0);
                let meeting = Meeting {
                    title: "Call fixture".to_owned(),
                    started_at_unix_ms: 0,
                    duration_ms,
                    speakers,
                    utterances,
                    ..Meeting::default()
                };
                println!("Call fixture transcript:\n{}", to_markdown(&meeting));
            }
        }
    }
}
