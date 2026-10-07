# Sottovoce roadmap

Sottovoce records calls on Windows as separate microphone and computer-audio
tracks. On demand it identifies speakers locally and creates a speaker-attributed
transcript through batch transcription. The desktop app uses Tauri 2 and
TypeScript.

## Principles

- The Rust engine handles capture, diarization, batch transcription, and meeting data.
- Transcription is batch and turn based. Realtime transcription and summaries are out of scope.
- Recording and paid transcription require deliberate user action; auto-transcribe is opt-in.
- Settings use one TOML file with a commented template and atomic saves.
- Ship only features that serve a concrete use case.

## Use cases

1. Record a call after checking that both audio levels move.
2. Start another meeting while the previous one transcribes in the background.
3. Review a meeting, play and seek audio, and follow transcript lines.
4. Rename meetings and speakers.
5. Delete audio while keeping an exported transcript when requested.
6. Archive a meeting and its metadata to a zip.
7. Recover a recording after a crash or forced close.

## Phases

1. **Core engine — done.** Recording state machine, background transcription,
   frontend commands and events, hot-reloaded config, graceful shutdown, and
   capture interfaces for tests.
2. **Tauri desktop app — done.** Windows app with device selection, recording
   controls, settings, and a meeting library.
3. **Meeting view and player polish — open.** Add keyboard shortcuts, speaker
   renaming, and silent UI tests.
4. **Library management and releases — done.** Delete/archive flows, titles
   suggested from the active audio application, portable packaging, a minimal
   bundled FFmpeg, and CI-built GitHub releases.

## Later

- Tray controls and a global Record/Stop shortcut.
- In-process Opus encoding and decoding to remove the FFmpeg sidecar.
- An installer and code signing.
- GPU acceleration for Nemotron is skipped: a one-hour, two-speaker call was
  fine on CPU.

## Out of scope

- Realtime transcription.
- Meeting summaries.
