# Sottovoce

Sottovoce is a Windows call recorder with a Tauri 2 desktop app. It records
your microphone and computer audio into separate Ogg Opus tracks. On demand,
it can identify speakers locally with NVIDIA Nemotron diarization and send the
recording to ElevenLabs for batch, speaker-attributed transcription.

Download the portable ZIP from the
[GitHub Releases](https://github.com/colthreepv/sottovoce/releases), extract it,
and run `sottovoce.exe`. WebView2 is required (normally already installed on
Windows 10 and 11). FFmpeg must currently be available on `PATH`, including
the `libopus` encoder. Transcription requires an ElevenLabs API key.

## Audio device selection

The recording screen has separate microphone and output loopback selectors.
Both default to **Follow Windows default**. A pinned device falls back to the
Windows default if unavailable and is retried during recording. Transcription
is started manually unless automatic transcription is enabled in Settings.

## Data and configuration

Configuration lives at `%APPDATA%\Sottovoce\config.toml`. Existing settings
are copied from `%APPDATA%\MeetingRecorder\config.toml` on first startup when
the new file is absent; the old file is kept. Set `SOTTOVOCE_CONFIG_DIR` to
override the config folder (`MEETING_RECORDER_CONFIG_DIR` remains a fallback
alias). Meetings default to `Documents\Meetings`. Models, caches and logs live
under `%LOCALAPPDATA%\Sottovoce`; the Nemotron model may be downloaded again
after this directory rename.

For development builds and deployment details, see
[docs/development.md](docs/development.md).

## Recordings

Each meeting folder contains `mic.ogg`, `computer.ogg`, `session.json`, and, after transcription, `meeting.json` and `transcript.md`. Session metadata includes the device names used for each track, duration, silence padding, dropped packets, and xruns.
