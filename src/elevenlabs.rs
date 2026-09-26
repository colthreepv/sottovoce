//! Speech-to-text client for the ElevenLabs Scribe API.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::blocking::multipart::{Form, Part};
use serde_json::Value;

use crate::types::{Abort, CANCELLED, Word, WordKind};

const ENDPOINT: &str = "https://api.elevenlabs.io/v1/speech-to-text";
const MAX_ATTEMPTS: u32 = 5;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

#[derive(Clone, Debug)]
pub struct SttOptions {
    pub api_key: String,
    pub model_id: String,
    /// ISO 639 code, None for automatic detection.
    pub language: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Transcription {
    pub language_code: Option<String>,
    pub words: Vec<Word>,
}

/// Uploads the audio file and returns its words with timestamps. When a cache
/// path is given and exists, the stored raw response is parsed instead of
/// calling the API again; after a successful call the raw response is saved there.
pub fn transcribe_file(
    audio: &Path,
    options: &SttOptions,
    cache: Option<&Path>,
    abort: &Abort,
) -> Result<Transcription, String> {
    check_abort(abort)?;
    if let Some(cache) = cache.filter(|path| path.is_file()) {
        let raw = std::fs::read_to_string(cache)
            .map_err(|e| format!("could not read STT cache {}: {e}", cache.display()))?;
        return parse_response(&raw)
            .map_err(|e| format!("invalid STT cache {}: {e}", cache.display()));
    }

    if options.api_key.trim().is_empty() {
        return Err(
            "no ElevenLabs API key: set it in Settings or in the ELEVENLABS_API_KEY variable"
                .into(),
        );
    }
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("could not create ElevenLabs HTTP client: {e}"))?;

    for attempt in 1..=MAX_ATTEMPTS {
        check_abort(abort)?;
        let form = make_form(audio, options)?;
        let response = client
            .post(ENDPOINT)
            .header("xi-api-key", options.api_key.trim())
            .multipart(form)
            .send();

        match response {
            Err(error) => {
                if attempt == MAX_ATTEMPTS {
                    return Err(format!(
                        "ElevenLabs request failed after {MAX_ATTEMPTS} attempts: {error}"
                    ));
                }
                wait_before_retry(backoff(attempt), abort)?;
            }
            Ok(response) => {
                let status = response.status();
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(|seconds| Duration::from_secs(seconds.min(30)));
                let raw = match response.text() {
                    Ok(raw) => raw,
                    Err(error) => {
                        if attempt == MAX_ATTEMPTS {
                            return Err(format!(
                                "could not read ElevenLabs response after {MAX_ATTEMPTS} attempts: {error}"
                            ));
                        }
                        wait_before_retry(backoff(attempt), abort)?;
                        continue;
                    }
                };

                if status.is_success() {
                    let transcription = parse_response(&raw)
                        .map_err(|e| format!("could not parse ElevenLabs response: {e}"))?;
                    if let Some(cache) = cache {
                        std::fs::write(cache, raw).map_err(|e| {
                            format!("could not save STT response cache {}: {e}", cache.display())
                        })?;
                    }
                    return Ok(transcription);
                }

                if is_retryable(status) && attempt < MAX_ATTEMPTS {
                    wait_before_retry(retry_after.unwrap_or_else(|| backoff(attempt)), abort)?;
                    continue;
                }
                return Err(response_error(status, &raw, attempt));
            }
        }
    }

    Err("ElevenLabs request ended unexpectedly".into())
}

fn make_form(audio: &Path, options: &SttOptions) -> Result<Form, String> {
    let file = Part::file(audio)
        .map_err(|e| format!("could not open audio file {}: {e}", audio.display()))?;
    let mut form = Form::new()
        .part("file", file)
        .text("model_id", options.model_id.clone())
        .text("timestamps_granularity", "word")
        .text("tag_audio_events", "true")
        // Speakers are determined locally by Nemotron for each recorded track.
        .text("diarize", "false");
    if let Some(language) = options
        .language
        .as_ref()
        .filter(|language| !language.trim().is_empty())
    {
        form = form.text("language_code", language.trim().to_owned());
    }
    Ok(form)
}

fn parse_response(raw: &str) -> Result<Transcription, String> {
    let response: Value =
        serde_json::from_str(raw).map_err(|e| format!("response is not valid JSON: {e}"))?;
    let language_code = response
        .get("language_code")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let entries = response
        .get("words")
        .and_then(Value::as_array)
        .ok_or_else(|| "response has no words array".to_owned())?;
    let mut words = Vec::with_capacity(entries.len());

    for (index, entry) in entries.iter().enumerate() {
        let field = |name: &str| {
            entry
                .get(name)
                .ok_or_else(|| format!("word {index} has no {name} field"))
        };
        let text = field("text")?
            .as_str()
            .ok_or_else(|| format!("word {index} has a non-string text field"))?;
        let kind = match field("type")?.as_str() {
            Some("word") => WordKind::Word,
            Some("spacing") => WordKind::Spacing,
            Some("audio_event") => WordKind::AudioEvent,
            Some(other) => return Err(format!("word {index} has unsupported type {other:?}")),
            None => return Err(format!("word {index} has a non-string type field")),
        };
        let start_ms = seconds_to_ms(field("start")?, index, "start")?;
        let end_ms = seconds_to_ms(field("end")?, index, "end")?;
        words.push(Word {
            text: text.to_owned(),
            start_ms,
            end_ms,
            kind,
        });
    }

    Ok(Transcription {
        language_code,
        words,
    })
}

fn seconds_to_ms(value: &Value, index: usize, field: &str) -> Result<i64, String> {
    let seconds = value
        .as_f64()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .ok_or_else(|| format!("word {index} has an invalid {field} timestamp"))?;
    Ok((seconds * 1000.0).round() as i64)
}

fn check_abort(abort: &Abort) -> Result<(), String> {
    if abort.load(Ordering::Relaxed) {
        Err(CANCELLED.into())
    } else {
        Ok(())
    }
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.saturating_sub(1).min(4))
}

fn wait_before_retry(delay: Duration, abort: &Abort) -> Result<(), String> {
    let deadline = std::time::Instant::now() + delay;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        check_abort(abort)?;
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
    check_abort(abort)
}

fn response_error(status: StatusCode, body: &str, attempts: u32) -> String {
    let detail = body.trim();
    let detail = if detail.len() > 1200 {
        format!("{}…", &detail[..detail.floor_char_boundary(1200)])
    } else {
        detail.to_owned()
    };
    let attempts = if is_retryable(status) {
        format!(" after {attempts} attempts")
    } else {
        String::new()
    };
    match status {
        StatusCode::UNAUTHORIZED => format!(
            "ElevenLabs rejected the API key (401); check ELEVENLABS_API_KEY or the key in Settings{attempts}. {detail}"
        ),
        StatusCode::FORBIDDEN => format!(
            "ElevenLabs denied this request (403); check API key permissions and account access{attempts}. {detail}"
        ),
        StatusCode::PAYMENT_REQUIRED => {
            format!("ElevenLabs account has no available STT credits (402){attempts}. {detail}")
        }
        StatusCode::TOO_MANY_REQUESTS => {
            format!("ElevenLabs rate limit reached (429){attempts}. {detail}")
        }
        status if status.is_server_error() => {
            format!("ElevenLabs service returned HTTP {status}{attempts}. {detail}")
        }
        _ => format!("ElevenLabs returned HTTP {status}. {detail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_response;
    use crate::types::WordKind;

    const SAMPLE: &str = r#"{
        "language_code": "en",
        "text": "Hi there. (laughter)",
        "words": [
            {"text":"Hi","start":0.125,"end":0.42,"type":"word"},
            {"text":" ","start":0.42,"end":0.42,"type":"spacing"},
            {"text":"there.","start":0.42,"end":0.91,"type":"word"},
            {"text":" (laughter)","start":1.0,"end":1.75,"type":"audio_event"}
        ]
    }"#;

    #[test]
    fn parses_word_spacing_and_audio_event_timestamps() {
        let transcription = parse_response(SAMPLE).expect("sample response parses");
        assert_eq!(transcription.language_code.as_deref(), Some("en"));
        assert_eq!(transcription.words.len(), 4);
        assert_eq!(transcription.words[0].start_ms, 125);
        assert_eq!(transcription.words[0].end_ms, 420);
        assert_eq!(transcription.words[1].kind, WordKind::Spacing);
        assert_eq!(transcription.words[2].text, "there.");
        assert_eq!(transcription.words[3].kind, WordKind::AudioEvent);
        assert_eq!(transcription.words[3].end_ms, 1750);
    }

    #[test]
    fn rejects_invalid_timestamp_and_unknown_word_kind() {
        let bad_time = r#"{"words":[{"text":"x","start":-1,"end":1,"type":"word"}]}"#;
        assert!(
            parse_response(bad_time)
                .unwrap_err()
                .contains("invalid start timestamp")
        );

        let bad_kind = r#"{"words":[{"text":"x","start":0,"end":1,"type":"phoneme"}]}"#;
        assert!(
            parse_response(bad_kind)
                .unwrap_err()
                .contains("unsupported type")
        );
    }

    #[test]
    fn parses_saved_live_fixture_responses() {
        let responses = [
            include_str!("../testdata/stt/call-mic.json"),
            include_str!("../testdata/stt/call-computer.json"),
            include_str!("../testdata/stt/call-speakers-mic.json"),
            include_str!("../testdata/stt/call-speakers-computer.json"),
            include_str!("../testdata/stt/room-mic.json"),
        ];
        for response in responses {
            let transcription = parse_response(response).expect("live fixture response parses");
            assert!(!transcription.words.is_empty());
        }
    }
}
