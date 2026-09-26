//! STUB — owned by the ElevenLabs builder. Speech-to-text for one file.

use std::path::Path;

use crate::types::{Abort, Word};

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
    _audio: &Path,
    _options: &SttOptions,
    _cache: Option<&Path>,
    _abort: &Abort,
) -> Result<Transcription, String> {
    Err("ElevenLabs speech-to-text not implemented yet".to_owned())
}
