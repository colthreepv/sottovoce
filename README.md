# Windows capture MVP

The Windows recorder is a command-line capture MVP. It records the default Windows microphone and the default Windows render endpoint's loopback into separate tracks. The loopback track contains the full output mix, including other system audio alongside meeting audio.

The recordings are intended for later use by an external speech-to-text (STT) tool. This recorder does not transcribe audio or connect to the Omarchy desktop app. It requires FFmpeg on `PATH`, built with the `libopus` encoder.

## Run

From the repository root in PowerShell:

```powershell
cargo run --release --manifest-path .\windows-recorder\Cargo.toml -- [output-directory]
```

The output directory is optional. By default, the recorder creates `recordings\recording-<unix-milliseconds>` relative to the current directory. To stop, press Enter or Ctrl+C; the recorder then finalizes the track files.

## Output

Each recording writes:

```text
recording-<unix-milliseconds>/
├── mic.ogg
├── system.ogg
└── session.json
```

Each Ogg file is a separate Opus track: `mic.ogg` is the microphone and `system.ogg` is Windows render loopback. Audio is encoded as it is captured, so no large uncompressed intermediate files are written. The recorder preserves each endpoint's channel count, encodes speech-optimized Opus at 32 kbps per channel (64 kbps for stereo), and lets FFmpeg resample to Opus's 48 kHz output rate. `session.json` records the session state, timestamps, device names, input format, encoding settings and duration, dropped callback packets, and xruns.

ElevenLabs lists OGG and Opus among its supported audio formats in the [Speech to Text documentation](https://elevenlabs.io/docs/overview/capabilities/speech-to-text). Upload `mic.ogg` and `system.ogg` as two separate files; leave `file_format` at its default unless the API reports that it needs an explicit format.

## Current limitations

- Capture uses only the Windows default input and output devices. There is no device picker.
- The microphone and render loopback streams start independently. There is no resampling, start-skew alignment, or clock-drift correction; use the session timestamps and durations when comparing the tracks.
- The recorder needs FFmpeg with `libopus` on `PATH`; it does not bundle the encoder.
- There is no GUI or STT integration. Upload the saved files to an external STT tool.
