# Development notes

This document covers local development and deployment details. User setup and
recording instructions are in the [README](../README.md).

## Compiling without freezing the PC

Run cargo through the wrapper, never bare:

```powershell
pwsh -File scripts/cargo.ps1 -Role dev test
pwsh -File scripts/cargo.ps1 -Role adept check --workspace
```

It runs cargo and every compiler process at BelowNormal priority, uses a
per-role target directory under `%TEMP%`, and lets at most two cargo runs
execute at once machine-wide through a named semaphore; extra runs wait for a
slot. This matters when several agents work in parallel.

## Build and launch locally

Use the deployment helper to build a locked release and stage it under a
versioned directory. The stable junction always launches the selected build:

```powershell
.\scripts\deploy-latest.ps1
& "$env:LOCALAPPDATA\Sottovoce\builds\latest\meeting-recorder.exe"
```

By default, the newest three builds are kept in
`%LOCALAPPDATA%\Sottovoce\builds`. Each versioned directory is named from the
Cargo version and short Git commit, with `-dirty` when the working tree has
changes. A numeric suffix is added if that name already exists. Each directory
contains `build.json` with the version, full commit, dirty flag, UTC build
time, and Rust/Cargo versions. The helper stages `meeting-recorder.exe` and
copies `packaging\dist\ffmpeg.exe` when that file exists. The artifact list is
defined near the top of `scripts\deploy-latest.ps1` and can be updated when
the executable layout changes, including the planned Tauri frontend.

The `latest` path is a junction. Deployment refuses to replace it if it is not
a junction to a managed Sottovoce build. Old builds are pruned after the
junction switches; builds whose executable is currently running are retained,
and the helper never stops a process.

A real deployment (no `-BuildRoot`) also keeps a `Sottovoce` Start Menu
shortcut pointing to `latest\sottovoce.exe`, so the Start Menu always opens the
newest build. Deploying while the app is open is safe: the running executable
stays in its versioned folder and only the junction moves.

Agents and tests must never launch, write to, or clean up anything under the
real `%LOCALAPPDATA%\Sottovoce\builds` directory. For a local deployment test,
pass a temporary `-BuildRoot`, and use a temporary
`MEETING_RECORDER_CONFIG_DIR` when launching the app. Do not run tests that
record or play audio.

The helper accepts `-BuildRoot`, `-KeepBuilds`, and `-SkipBuild`. Use
`-SkipBuild` only when the release executable already exists in the helper's
dedicated `%TEMP%\sottovoce-deploy-target\release` directory:

```powershell
.\scripts\deploy-latest.ps1 -BuildRoot "$env:TEMP\sottovoce-builds" -KeepBuilds 5
.\scripts\deploy-latest.ps1 -BuildRoot "$env:TEMP\sottovoce-builds" -SkipBuild
```

The Cargo release build always uses `--locked` and writes to that dedicated
target directory rather than the repository's default `target` directory.
