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
                language: None,
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
        for speaker in &mut speakers {
            if let Some(old) = previous.speakers.iter().find(|s| s.id == speaker.id) {
                speaker.name = old.name.clone();
            }
        }
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
        stt_model: options.stt.model_id.clone(),
        speakers,
        utterances,
    };
    check(abort)?;
    crate::meetings::save(dir, &meeting)?;
    let _ = events.send(Event::Stage("Done".into()));
    Ok(meeting)
}
