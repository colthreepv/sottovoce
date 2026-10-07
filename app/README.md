# Sottovoce desktop UI

`src-tauri` owns the desktop bridge; `ui` is Vite + Svelte 5 + TypeScript.
The workspace uses the root Cargo.lock. The Tauri `sottovoce` binary and the
dev-only `sottovoce-dev-cli` (QA harness, never deployed) share `sottovoce_engine`.

From the repository root:

```powershell
bun install --cwd app/ui
bun run --cwd app/ui build
bun run --cwd app/ui check
pwsh -NoProfile -Command "& ./scripts/cargo.ps1 -Role adept -CargoArgs @('check','--workspace')"
pwsh -NoProfile -Command "& ./scripts/cargo.ps1 -Role adept -CargoArgs @('test','--workspace')"
```

The Tauri CLI is a local devDependency (`cd app/ui; bun run tauri --help`).
Do not let CLI development builds bypass the machine-wide Cargo wrapper.
For a portable debug UI with embedded web assets, build the frontend first,
then run the wrapper with `@('build','--workspace')`. The app is in
`$env:TEMP/sottovoce-adept-target/debug/sottovoce.exe`.
Set `SOTTOVOCE_CONFIG_DIR` to an isolated config directory before tests;
its config must also point meetings_dir to a temporary folder.

## Bridge

Managed `Engine` owns Core. `core-event` carries the serde-tagged Core event
union. Subscribe before requesting `get_snapshot`; the snapshot includes config,
devices, recording state/timer and meetings/job statuses. Commands map to the
Core methods. Additional read commands are `get_snapshot` and `get_meeting`.
`get_meeting` checks library membership, reads metadata, and grants asset-protocol
access only to the selected mic.ogg/computer.ogg files.

CloseRequested prevents close once and requests Core shutdown. `closing` shows
the finalization overlay. ShutdownComplete exits; a blocking 30-second watchdog
caps finalization with a forced process exit. No polling close loop.
The single-instance plugin is registered before Core setup.

Web assets and Tauri capabilities are embedded in the executable. The deploy
script copies sottovoce.exe, FFmpeg when available, runtime DLLs and any
resources directory. WebView2 must already be installed. Test deployment only
with a temporary BuildRoot, for example using the debug artifacts:

```powershell
pwsh -File scripts/deploy-latest.ps1 -SkipBuild -Profile debug `
  -TargetDirectory "$env:TEMP/sottovoce-adept-target" `
  -BuildRoot "$env:TEMP/sottovoce-deploy-smoke"
```

Recording and playback require deliberate clicks. Two native audio elements
start together and correct drift on timeupdate; keyboard shortcuts and speaker
renaming remain Phase 3.

Monitoring (live levels outside a recording) is opt-in: the UI asks for it only
while Home is visible and the window is not minimized, and the user can pause
it. While paused no capture stream is open, so Windows shows no microphone
indicator. Recording opens its own streams and reports levels from any screen.
