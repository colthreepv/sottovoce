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
& "$env:LOCALAPPDATA\Sottovoce\builds\latest\sottovoce.exe"
```

By default, the newest three builds are kept in
`%LOCALAPPDATA%\Sottovoce\builds`. Each versioned directory is named from the
Cargo version and short Git commit, with `-dirty` when the working tree has
changes. A numeric suffix is added if that name already exists. Each directory
contains `build.json` with the version, full commit, dirty flag, UTC build
time, and Rust/Cargo versions. Local deployment assembles the same portable
layout as CI through `packaging\package.ps1`; if
`packaging\dist\ffmpeg.exe` is missing, it warns and packages without FFmpeg.
The development-only `sottovoce-dev-cli.exe` is not deployed.

The `latest` path is a junction. Deployment refuses to replace it if it is not
a junction to a Sottovoce executable inside the build root. An unmanaged prior
folder without `build.json` can be replaced and is left untouched; only managed
folders are eligible for pruning. Builds whose executable is currently running
are retained, and the helper never stops a process.

A real deployment (no `-BuildRoot`) also keeps a `Sottovoce` Start Menu
shortcut pointing to `latest\sottovoce.exe`, so the Start Menu always opens the
newest build. Deploying while the app is open is safe: the running executable
stays in its versioned folder and only the junction moves.

Agents and tests must never launch, write to, or clean up anything under the
real `%LOCALAPPDATA%\Sottovoce\builds` directory. For a local deployment test,
pass a temporary `-BuildRoot`, and use a temporary
`SOTTOVOCE_CONFIG_DIR` when launching the app. Do not run tests that
record or play audio.

The helper accepts `-FromRelease`, `-BuildRoot`, `-KeepBuilds`, and `-SkipBuild`.
Deploy an already-published portable release without building locally:

```powershell
.\scripts\deploy-latest.ps1 -FromRelease v0.2.0
```

The release mode downloads the versioned ZIP and `SHA256SUMS`, verifies the
archive, and records its tag commit in `build.json`. It does not run Cargo or
Bun. Use `-SkipBuild` only when the release executable and DirectML runtime DLL
already exist in the helper's dedicated
`%TEMP%\sottovoce-deploy-target\release` directory:

```powershell
.\scripts\deploy-latest.ps1 -BuildRoot "$env:TEMP\sottovoce-builds" -KeepBuilds 5
.\scripts\deploy-latest.ps1 -BuildRoot "$env:TEMP\sottovoce-builds" -SkipBuild
```

The local package is created with `packaging\package.ps1`; it includes runtime
DLLs, license texts, and notices, plus FFmpeg when available. Temporary
deployment tests must always supply a temporary `-BuildRoot`.

The Cargo release build always uses `--locked` and writes to that dedicated
target directory rather than the repository's default `target` directory.

## Releases

Push a `v*` tag matching the versions in the root Cargo package and Tauri
configuration to build and publish a portable Windows x64 ZIP. Pull requests
also build the package without publishing it. GitHub Actions
cross-builds the pinned, LGPL FFmpeg with libopus, caches that build by its
Dockerfile and source pins, then builds and verifies the app and bundled
FFmpeg on Windows. The release includes `sottovoce.exe`, its runtime DLLs,
`ffmpeg.exe`, third-party notices, license texts, and `SHA256SUMS`. Builds are
unsigned, so Windows SmartScreen may show a warning.

Use **Actions → Portable release → Run workflow** to build the ZIP without
publishing a release; the ZIP and checksum are available as workflow artifacts.
For a local package, build the frontend and Tauri app, build FFmpeg with
`packaging/build-ffmpeg.ps1`, then assemble the archive:

```powershell
bun install --cwd app/ui
bun run --cwd app/ui build
$target = Join-Path $env:TEMP 'sottovoce-builder-target'
pwsh -File scripts/cargo.ps1 -Role builder build --release --locked -p sottovoce --target-dir $target
pwsh -File packaging/build-ffmpeg.ps1
pwsh -File packaging/package.ps1 -ReleaseDirectory (Join-Path $target 'release') `
  -FfmpegPath packaging/dist/ffmpeg.exe
```
