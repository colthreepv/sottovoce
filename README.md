# Meeting Recorder for Windows

The Windows app records the microphone and Windows output loopback as separate Opus tracks. It includes a GUI, a command line interface, speaker diarization, and optional ElevenLabs transcription. FFmpeg with the `libopus` encoder must be available on `PATH`.

## Run

Open the GUI:

```powershell
cargo run --release --manifest-path .\windows-recorder\Cargo.toml
```

Useful command line commands include `devices`, `record [folder]`, `process <folder>`, `diarize <audio>`, `stt <audio>`, and `selftest`.

## Audio device selection

The recording screen has separate Microphone and Output / loopback selectors. Both default to **Follow Windows default**. Select a device to pin it across sessions; the choice is saved immediately. Device selectors are disabled while recording. The live meters show level and dBFS before recording and continue using the recorder's levels during capture. The idle monitor stops while recording and when the recording screen is not active.

If a pinned device is unavailable or cannot be opened, capture falls back to the Windows default, records the fallback in `session.json`, and retries the pinned device while recording. It returns to the pinned device if it becomes available again. The command `meeting-recorder devices` lists friendly names, stable cpal IDs, defaults, and formats. IDs can be stored in the configuration below.

## Transcription

Stopping a recording saves it and opens its meeting view. Use **Transcribe** in that view to start transcription manually. In Settings, **Transcribe automatically after stopping** enables automatic transcription; it is off by default. ElevenLabs detects the language automatically.

## Configuration and logs

The configuration file is `%APPDATA%\MeetingRecorder\config.toml`. Set `MEETING_RECORDER_CONFIG_DIR` to override the configuration directory, which is useful for isolated runs. Supported audio and transcription keys include:

```toml
# Omit or set to false to follow the current Windows default.
mic_device = "wasapi:{0.0.1.00000000}.{device-guid}"
output_device = "wasapi:{0.0.0.00000000}.{device-guid}"

# Defaults to false.
auto_transcribe = false
```

Application logs are written to `%LOCALAPPDATA%\MeetingRecorder\logs\YYYY-MM-DD.log`.

## Recordings

Each meeting folder contains `mic.ogg`, `computer.ogg`, `session.json`, and, after transcription, `meeting.json` and `transcript.md`. Session metadata includes the device names used for each track, duration, silence padding, dropped packets, and xruns.
