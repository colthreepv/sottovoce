# Windows QA harness

`run-e2e.ps1` builds the development-only `sottovoce-dev-cli.exe` through
`scripts/cargo.ps1`, then checks the offline transcription pipeline and CLI
hygiene. It does not launch the Tauri app.

The cached `call`, `call-speakers`, and `room` fixtures run with a dummy
`ELEVENLABS_API_KEY`; there is no API spend. `room` gets a silent computer
track generated with FFmpeg. The harness verifies output files, utterances,
and speaker counts. The `selftest` option plays a tone and records it, so use
`-SkipSelftest` unless audio testing is explicitly intended. It also supports
`-SkipBuild`, `-SkipGui` (compatibility flag), `-KeepWork`, and `-WorkRoot`.

The harness sets `SOTTOVOCE_CONFIG_DIR` to an isolated temp directory and does
not access the user's real AppData config. The Nemotron model may be downloaded
to `%LOCALAPPDATA%\Sottovoce\models` for diarized runs and is reused later.

```powershell
pwsh -File scripts/qa/run-e2e.ps1 -SkipSelftest -SkipGui
```
