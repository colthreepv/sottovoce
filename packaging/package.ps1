$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$crateRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$manifestPath = Join-Path $crateRoot 'Cargo.toml'
$manifest = Get-Content -LiteralPath $manifestPath -Raw
$versionMatch = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"')
if (-not $versionMatch.Success) {
    throw "Could not read the crate version from $manifestPath"
}
$version = $versionMatch.Groups[1].Value

$distPath = Join-Path $PSScriptRoot 'dist'
$ffmpegPath = Join-Path $distPath 'ffmpeg.exe'
if (-not (Test-Path -LiteralPath $ffmpegPath -PathType Leaf)) {
    & (Join-Path $PSScriptRoot 'build-ffmpeg.ps1')
    if ($LASTEXITCODE -and $LASTEXITCODE -ne 0) {
        throw "The FFmpeg build failed with exit code $LASTEXITCODE"
    }
}
& (Join-Path $PSScriptRoot 'verify-ffmpeg.ps1') -FfmpegPath $ffmpegPath

$cargoCommand = Get-Command cargo.exe -ErrorAction Stop
& $cargoCommand.Source build --manifest-path $manifestPath --release
if ($LASTEXITCODE -ne 0) {
    throw "The meeting-recorder release build failed with exit code $LASTEXITCODE"
}

$appPath = Join-Path $crateRoot 'target\release\meeting-recorder.exe'
if (-not (Test-Path -LiteralPath $appPath -PathType Leaf)) {
    throw "Cargo succeeded but the binary is missing: $appPath"
}

$packageName = "MeetingRecorder-$version-win64"
$packagePath = Join-Path $distPath $packageName
$zipPath = Join-Path $distPath "$packageName.zip"
$distFullPath = [System.IO.Path]::GetFullPath($distPath).TrimEnd('\') + '\'
foreach ($generatedPath in @($packagePath, $zipPath)) {
    $generatedFullPath = [System.IO.Path]::GetFullPath($generatedPath)
    if (-not $generatedFullPath.StartsWith($distFullPath, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to replace a package output outside $distPath"
    }
}
if (Test-Path -LiteralPath $packagePath) {
    Remove-Item -LiteralPath $packagePath -Recurse -Force
}
if (Test-Path -LiteralPath $zipPath) {
    Remove-Item -LiteralPath $zipPath -Force
}
New-Item -ItemType Directory -Path (Join-Path $packagePath 'licenses') -Force | Out-Null

Copy-Item -LiteralPath $appPath -Destination (Join-Path $packagePath 'meeting-recorder.exe')
Copy-Item -LiteralPath $ffmpegPath -Destination (Join-Path $packagePath 'ffmpeg.exe')
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'THIRD-PARTY-NOTICES.txt') -Destination $packagePath

$licenseFiles = @(
    @{ Source = (Join-Path $distPath 'licenses\FFmpeg-LGPL-2.1.txt'); Name = 'FFmpeg-LGPL-2.1.txt' },
    @{ Source = (Join-Path $distPath 'licenses\libopus-BSD.txt'); Name = 'libopus-BSD.txt' },
    @{ Source = (Join-Path $PSScriptRoot 'licenses\ONNX-Runtime-MIT.txt'); Name = 'ONNX-Runtime-MIT.txt' },
    @{ Source = (Join-Path $PSScriptRoot 'licenses\Nemotron-OpenMDW-1.1.txt'); Name = 'Nemotron-OpenMDW-1.1.txt' }
)
foreach ($license in $licenseFiles) {
    if (-not (Test-Path -LiteralPath $license.Source -PathType Leaf)) {
        throw "Required third-party license file is missing: $($license.Source)"
    }
    Copy-Item -LiteralPath $license.Source -Destination (Join-Path $packagePath "licenses\$($license.Name)")
}

$readme = @'
Meeting Recorder for Windows

Run meeting-recorder.exe to open the app. Recording creates separate mic.ogg
and computer.ogg tracks; processing builds the merged conversation transcript.
Set your ElevenLabs API key in Settings or in config.toml before transcription.

Files and first run
  Settings: %APPDATA%\MeetingRecorder\config.toml
  Meetings: %USERPROFILE%\Documents\Meetings
  Models:   %LOCALAPPDATA%\MeetingRecorder\models

Speaker diarization downloads NVIDIA Nemotron 3 locally the first time it is
used (about 120 MB). FFmpeg is included beside the app and is used for audio
recording, import, and playback.

The command line is also available from Command Prompt or PowerShell:
  meeting-recorder.exe record [folder]
  meeting-recorder.exe process <folder>
  meeting-recorder.exe diarize <audio> [--speakers N]
  meeting-recorder.exe stt <audio> [--language xx]

See THIRD-PARTY-NOTICES.txt and the licenses folder for component notices.
'@
Set-Content -LiteralPath (Join-Path $packagePath 'README.txt') -Value $readme -Encoding UTF8

Compress-Archive -Path $packagePath -DestinationPath $zipPath -CompressionLevel Optimal
$zipSize = (Get-Item -LiteralPath $zipPath).Length
Write-Host "Package: $packagePath"
Write-Host ("Archive: {0} ({1:N2} MB)" -f $zipPath, ($zipSize / 1MB))
