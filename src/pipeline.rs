//! After a recording: speakers per side (Nemotron), words per side
//! (ElevenLabs), one conversation (transcript), saved into the meeting folder.

use std::path::Path;
use std::sync::atomic::Ordering;

use crate::config::Config;
use crate::elevenlabs::SttOptions;
use crate::types::{Abort, CANCELLED, Event, Events, Meeting, Side, SideInput};

#[derive(Clone, Debug)]
pub struct Options {
    pub stt: SttOptions,
    pub diarize: bool,
    pub your_name: Option<String>,
}

impl Options {
    pub fn from_config(config: &Config) -> Result<Options, String> {
        let api_key = config.api_key().ok_or(
            "no ElevenLabs API key: set it in Settings or in the ELEVENLABS_API_KEY variable",
        )?;
        Ok(Options {
            stt: SttOptions {
                api_key,
                model_id: config.stt_model(),
                language: config.language(),
            },
            diarize: config.diarize(),
            your_name: config.your_name.clone().filter(|n| !n.trim().is_empty()),
        })
    }
}

/// Peak under this counts as a silent track: nothing to transcribe.
const SILENT_PEAK: f32 = 0.004;

fn check(abort: &Abort) -> Result<(), String> {
    if abort.load(Ordering::Relaxed) {
        Err(CANCELLED.into())
    } else {
        Ok(())
    }
}

/// Transcribes the meeting in `dir` and saves meeting.json and transcript.md.
/// Speaker names already given are kept for speakers that still exist.
pub fn process(
    dir: &Path,
    options: &Options,
    events: &Events,
    abort: &Abort,
) -> Result<Meeting, String> {
    let session = crate::capture::Session::load(dir).unwrap_or_default();
    let previous = crate::meetings::load(dir);
    let mut sides = Vec::new();
    let mut duration_ms = 0i64;
    let mut language = options.stt.language.clone();

    for side in [Side::Mic, Side::Computer] {
        let path = dir.join(side.file_name());
        if !path.is_file() {
            continue;
        }
        check(abort)?;
        let who = side.label();
        let _ = events.send(Event::Stage(format!("Reading audio ({who})")));
        let samples = crate::ffmpeg::decode_mono_16k(&path)?;
        duration_ms = duration_ms.max(samples.len() as i64 * 1000 / 16_000);
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        if peak < SILENT_PEAK {
            let _ = events.send(Event::Log(format!("{who}: silent, skipped")));
            continue;
        }

        let turns = if options.diarize {
            let _ = events.send(Event::Stage(format!("Finding speakers ({who})")));
            match crate::diarize::turns(&samples, None, events, abort) {
                Ok(turns) => turns,
                Err(e) if e == CANCELLED => return Err(e),
                Err(e) => {
                    let _ = events.send(Event::Log(format!("{who}: speakers not found: {e}")));
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        drop(samples);

        check(abort)?;
        let _ = events.send(Event::Stage(format!(
            "Transcribing with ElevenLabs ({who})"
        )));
        let _ = events.send(Event::Progress(0.0));
        let cache = dir.join(format!(
            ".stt-{}.json",
            side.file_name().trim_end_matches(".ogg")
        ));
        let stt = crate::elevenlabs::transcribe_file(&path, &options.stt, Some(&cache), abort)?;
        if language.is_none() {
            language = stt.language_code.clone();
        }
        let _ = events.send(Event::Progress(1.0));
        sides.push(SideInput {
            side,
            words: stt.words,
            turns,
            offset_ms: 0,
        });
    }

    let _ = events.send(Event::Stage("Building the conversation".into()));
    let (mut speakers, utterances) = crate::transcript::build(&sides, options.your_name.as_deref());
    if let Some(previous) = &previous {
        preserve_speaker_names(&mut speakers, &utterances, previous);
    }
    let started = previous
        .as_ref()
        .map(|m| m.started_at_unix_ms)
        .filter(|t| *t > 0)
        .unwrap_or(session.started_at_unix_ms);
    let meeting = Meeting {
        title: previous
            .as_ref()
            .map(|m| m.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| crate::meetings::default_title(started)),
        started_at_unix_ms: started,
        duration_ms,
        language,
        source_app: previous.as_ref().and_then(|m| m.source_app.clone()),
        device_changes: previous
            .as_ref()
            .filter(|m| !m.device_changes.is_empty())
            .map(|m| m.device_changes.clone())
            .unwrap_or_else(|| session.device_changes.clone()),
        stt_model: options.stt.model_id.clone(),
        speakers,
        utterances,
    };
    check(abort)?;
    crate::meetings::save(dir, &meeting)?;
    let _ = events.send(Event::Stage("Done".into()));
    Ok(meeting)
}

/// Echo removal can renumber IDs. Match names by surviving speech timing,
/// and never carry an automatically numbered label back onto a single voice.
fn preserve_speaker_names(
    speakers: &mut [crate::types::Speaker],
    utterances: &[crate::types::Utterance],
    previous: &Meeting,
) {
    for speaker in speakers {
        let mut overlap = std::collections::HashMap::<&str, i64>::new();
        for current in utterances.iter().filter(|u| u.speaker == speaker.id) {
            for old in previous
                .utterances
                .iter()
                .filter(|u| u.side == speaker.side)
            {
                let shared = current.end_ms.min(old.end_ms) - current.start_ms.max(old.start_ms);
                if shared > 0 {
                    *overlap.entry(&old.speaker).or_default() += shared;
                }
            }
        }
        let old_id = overlap
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(id, _)| id)
            .unwrap_or(&speaker.id);
        if let Some(old) = previous
            .speakers
            .iter()
            .find(|s| s.id == old_id && s.side == speaker.side)
        {
            let label = old.side.label();
            let generated = old.name == label
                || old
                    .name
                    .strip_prefix(&format!("{label} "))
                    .is_some_and(|suffix| suffix.parse::<usize>().is_ok());
            if !generated {
                speaker.name = old.name.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Side, Speaker, Utterance};
    #[test]
    fn device_history_roundtrips_and_survives_retranscription() {
        use crate::types::DeviceChange;
        use std::sync::{Arc, atomic::AtomicBool, mpsc};
        let root = std::env::temp_dir().join(format!(
            "sottovoce-device-history-{}-{}",
            std::process::id(),
            crate::capture::unix_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let change = DeviceChange {
            at_ms: 1234,
            side: Side::Computer,
            device: "Headset".into(),
        };
        let legacy: Meeting = serde_json::from_str(r#"{"title":"Legacy meeting"}"#).unwrap();
        assert!(legacy.device_changes.is_empty());
        let original = Meeting {
            device_changes: vec![change.clone()],
            ..legacy
        };
        crate::meetings::save(&root, &original).unwrap();
        assert_eq!(
            crate::meetings::load(&root).unwrap().device_changes,
            vec![change.clone()]
        );
        let options = Options {
            stt: SttOptions {
                api_key: "unused-offline-key".into(),
                model_id: "scribe_v2".into(),
                language: None,
            },
            diarize: false,
            your_name: None,
        };
        let (events, _) = mpsc::channel();
        let abort = Arc::new(AtomicBool::new(false));
        // No audio files: exercise metadata rebuilding without hardware or API calls.
        for _ in 0..2 {
            let rebuilt = process(&root, &options, &events, &abort).unwrap();
            assert_eq!(rebuilt.device_changes, original.device_changes);
            assert_eq!(
                crate::meetings::load(&root).unwrap().device_changes,
                original.device_changes
            );
        }
        crate::capture::Session {
            device_changes: vec![change.clone()],
            ..Default::default()
        }
        .save(&root)
        .unwrap();
        crate::meetings::save(&root, &Meeting::default()).unwrap();
        assert_eq!(
            process(&root, &options, &events, &abort)
                .unwrap()
                .device_changes,
            vec![change]
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn preserves_custom_names_after_renumbering_but_refreshes_default_labels() {
        let utterance = |speaker: &str| Utterance {
            speaker: speaker.into(),
            side: Side::Mic,
            start_ms: 1000,
            end_ms: 2000,
            text: "Local speech".into(),
        };
        let mut speakers = vec![Speaker {
            id: "you-1".into(),
            side: Side::Mic,
            name: "You".into(),
        }];
        let mut previous = Meeting {
            speakers: vec![Speaker {
                id: "you-4".into(),
                side: Side::Mic,
                name: "You 4".into(),
            }],
            utterances: vec![utterance("you-4")],
            ..Default::default()
        };
        preserve_speaker_names(&mut speakers, &[utterance("you-1")], &previous);
        assert_eq!(speakers[0].name, "You");
        previous.speakers[0].name = "Valerio".into();
        preserve_speaker_names(&mut speakers, &[utterance("you-1")], &previous);
        assert_eq!(speakers[0].name, "Valerio");
    }
}
