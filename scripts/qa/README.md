# Windows QA harness

`run-e2e.ps1` is an end-to-end check of the Windows `meeting-recorder` crate.
It builds the release binary in its own `CARGO_TARGET_DIR`
(`%TEMP%\mr-target-seeker`), so it never touches a teammate's build or locks
`windows-recorder/target`.

## What it checks

1. **Build** - `cargo build --release`.
2. **Selftest** - `meeting-recorder selftest` plays a 660 Hz tone for ~2 s and
   records it. The Remote (loopback) peak must be above -40 dBFS and both
   tracks must be ~4 s long.
3. **Pipeline, offline** - for the `call`, `call-speakers` and `room` fixtures
   it builds a temporary meeting folder (`mic.ogg`, `computer.ogg`,
   `session.json` with `status = "completed"`) and copies the cached
   ElevenLabs responses from `windows-recorder/testdata/stt/` to
   `.stt-mic.json` / `.stt-computer.json`, which is where `pipeline.rs` looks.
   `meeting-recorder process` then runs with a dummy `ELEVENLABS_API_KEY`, so
   there is **no API spend**. `room` gets a computer track generated
   with ffmpeg `anullsrc`, which the pipeline must detect as silent and skip.
   The harness asserts `meeting.json` and `transcript.md` exist, that there is
   at least one utterance, and that the speaker count is within +/-1 of the
   distinct speakers in `truth.json` for the tracks that are not silent.
4. **CLI hygiene** - `--help` and `record --help` must not create folders;
   `devices` must list at least one device.
5. **GUI smoke** - starts the GUI with the QA config pointing `meetings_dir`
   at the processed meetings, brings the window to the
   foreground, screenshots it (System.Drawing `CopyFromScreen`) into
   `%TEMP%\mr-qa\screens`, then sends WM_CLOSE and reports whether the process
   exits within 10 s. A window that ignores WM_CLOSE is reported as WARN, not a
   hard failure.

The Nemotron speaker model (~120 MB) is downloaded into
`%LOCALAPPDATA%\MeetingRecorder\models` on the first diarized run. That folder
is deliberately *not* isolated, so later runs reuse the model.

## Config isolation (important)

`dirs` resolves the config with `SHGetKnownFolderPath`, which ignores the
`APPDATA` and `USERPROFILE` environment variables, so a temporary `APPDATA`
does **not** redirect `%APPDATA%\MeetingRecorder\config.toml`. The harness
therefore backs up that file, writes a QA config over it, and restores the
original (or deletes the file when there was none) in a `finally` block. Do not
run it while another process is editing that config.

## Usage

```powershell
pwsh -File windows-recorder/scripts/qa/run-e2e.ps1
```

Parameters: `-SkipBuild`, `-SkipSelftest`, `-SkipGui` (also implied on a
machine without audio capture), `-KeepWork`, `-WorkRoot <path>`.

The script prints a PASS/FAIL/WARN/INFO table and exits non-zero when any check
fails. Screenshots and logs are kept under `%TEMP%\mr-qa`.

## Interpreting the speaker count

`truth.json` describes the *source* conversation. A track the pipeline skips
as silent has no speakers in the output, so for `room` (silent computer track)
the expected count is the microphone speakers only (2, not 4).
