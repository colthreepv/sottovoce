# Sottovoce roadmap

Sottovoce records calls on Windows as two separate tracks (your microphone, the computer's audio) and turns them, on demand, into a speaker-attributed transcript. Quality over volume: few recordings, done well. Most recordings are short-lived; some are archived for years, often without a transcript.

## Principles

- The engine (capture, Nemotron diarization, ElevenLabs batch STT, transcript builder) is Rust and UI-agnostic. The UI is Tauri 2 (WebView2) with a TypeScript frontend.
- Transcription is batch and turn based. Realtime transcription and summaries are out of scope.
- Nothing starts recording or spends API credit without a deliberate user action (auto-transcribe is an opt-in setting).
- Settings live in one TOML file with a commented template, saved atomically and hot-reloaded when edited by hand (same model as Scribetray).
- Do not overengineer: features land when they serve a concrete use case below.

## Use cases

1. Record a call after checking that both levels move.
2. Back-to-back meetings: Stop, Record the next one immediately, while the previous one transcribes in the background.
3. Keep an eye on an ongoing recording from any screen.
4. Review a meeting: play, seek, jump from a transcript line to the audio, rename the meeting and speakers.
5. Clean up: delete the audio of throwaway calls but keep their transcript.
6. Archive: one click packs a call (both tracks, transcript if any) into a zip in a chosen archive folder.
7. Recover a recording after a crash or forced close.

## Screens

- **Sidebar (always visible):** app name/icon goes Home; a red Rec button that becomes Stop, with timer and two mini level meters while recording; meeting list with queued/transcribing badges; full-width Settings.
- **Home:** chosen devices, their live levels, Start recording. While recording: large levels, timer, Stop, in a fixed layout.
- **Meeting:** editable title; player with a long seek bar, compact right-aligned time, ±5 s and ±15 s buttons, shortcuts (Space play/pause, Left/Right 5 s, Shift+Left/Right 15 s), one consistent loading state; Transcribe (async) and transcript lines that seek the audio; Delete audio and Archive actions.
- **Settings:** API key, your name, meetings folder, transcripts folder, archive folder, auto-transcribe, multi-voice detection (tooltip: runs after recording, when you press Transcribe; Nemotron finds how many voices are on each side, so the other side becomes Remote 1, Remote 2...).

## Phases

1. **Core engine** (in progress). Recording state machine (Ready, Starting, Recording, Finalizing), transcription FIFO queue independent of recording, commands and serde events for the frontend, hot-reloaded config, graceful shutdown, capture behind a trait for tests. The egui UI becomes a thin temporary client.
2. **Tauri UI** on top of the core, following the screens above.
3. **Meeting view and player** polish, shortcuts, silent UI tests with screenshots.
4. **Library management and packaging:**
   - *Delete audio*: removes the two tracks; if a transcript exists it is exported to the transcripts folder as Markdown named after the meeting title, then the meeting leaves the list. Manual garbage collection, always user initiated, with confirmation.
   - *Archive*: writes a zip named after the meeting title into the archive folder (setting), containing mic.ogg, computer.ogg, transcript.md when present and the meeting metadata; verifies the zip, then removes the local folder.
   - File names: `YYYY-MM-DD HHmm <title>` (sanitized, collision suffix), so renamed meetings get precise names and everything sorts by date.
   - *Default meeting title from the calling app* (WhatsApp, Telegram, Teams, Slack...): sample per-app audio sessions during recording and use the loudest non-system app. Research: docs/research/audio-app-detection.md.
   - *Minimal FFmpeg* from a reliable, up-to-date source. Research: docs/research/ffmpeg-source.md.
   - Deploy script for day-to-day builds (`scripts/deploy-latest.ps1`).

## Later, once everything runs smoothly

- Tray icon and global Rec/Stop shortcut.
- Installer (secondary; a portable folder is enough for now).
- GPU (DirectML) for Nemotron, only if a real one-hour call takes more than a couple of minutes on CPU.

## Out of scope

- Realtime transcription, meeting summaries.
