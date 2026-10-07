[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$ReleaseDirectory,
    [string]$FfmpegPath,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot 'dist')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$releaseDirectory = (Resolve-Path -LiteralPath $ReleaseDirectory).Path
$resolvedFfmpegPath = $null
if (-not [string]::IsNullOrWhiteSpace($FfmpegPath)) {
    $resolvedFfmpegPath = (Resolve-Path -LiteralPath $FfmpegPath).Path
}
$outputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$engineManifest = Get-Content -LiteralPath (Join-Path $repoRoot 'Cargo.toml') -Raw
$engineVersionMatch = [regex]::Match($engineManifest, '(?s)\[package\].*?name\s*=\s*"sottovoce-engine".*?version\s*=\s*"([^"]+)"')
if (-not $engineVersionMatch.Success) { throw 'Could not read the sottovoce-engine version from Cargo.toml.' }
$version = $engineVersionMatch.Groups[1].Value
$tauriManifest = Get-Content -LiteralPath (Join-Path $repoRoot 'app/src-tauri/Cargo.toml') -Raw
$tauriVersionMatch = [regex]::Match($tauriManifest, '(?s)\[package\].*?name\s*=\s*"sottovoce".*?version\s*=\s*"([^"]+)"')
$tauriConfig = Get-Content -LiteralPath (Join-Path $repoRoot 'app/src-tauri/tauri.conf.json') -Raw | ConvertFrom-Json
if (-not $tauriVersionMatch.Success -or $tauriVersionMatch.Groups[1].Value -ne $version -or $tauriConfig.version -ne $version) {
    throw 'Cargo engine, Tauri package, and Tauri config versions must match.'
}

$appPath = Join-Path $releaseDirectory 'sottovoce.exe'
$directMlPath = Join-Path $releaseDirectory 'DirectML.dll'
foreach ($requiredPath in @($appPath, $directMlPath)) {
    if (-not (Test-Path -LiteralPath $requiredPath -PathType Leaf)) {
        throw "Required release file is missing: $requiredPath"
    }
}
if ($resolvedFfmpegPath -and -not (Test-Path -LiteralPath $resolvedFfmpegPath -PathType Leaf)) {
    throw "Required release file is missing: $resolvedFfmpegPath"
}
if ($resolvedFfmpegPath -and (Get-Item -LiteralPath $resolvedFfmpegPath).Length -ge 10MB) {
    throw 'Bundled FFmpeg must be smaller than 10 MB.'
}

$outputDirectory = [IO.Path]::GetFullPath($outputDirectory)
[void][IO.Directory]::CreateDirectory($outputDirectory)
$zipName = "sottovoce-$version-windows-x64.zip"
$zipPath = Join-Path $outputDirectory $zipName
$stagingPath = Join-Path ([IO.Path]::GetTempPath()) "sottovoce-package-$([Guid]::NewGuid().ToString('N'))"
$archivePath = Join-Path ([IO.Path]::GetTempPath()) "sottovoce-package-$([Guid]::NewGuid().ToString('N')).zip"

try {
    [void][IO.Directory]::CreateDirectory($stagingPath)
    Copy-Item -LiteralPath $appPath -Destination (Join-Path $stagingPath 'sottovoce.exe')
    if ($resolvedFfmpegPath) {
        Copy-Item -LiteralPath $resolvedFfmpegPath -Destination (Join-Path $stagingPath 'ffmpeg.exe')
    }
    Get-ChildItem -LiteralPath $releaseDirectory -Filter '*.dll' -File | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $stagingPath $_.Name) -Force
    }
    $resources = Join-Path $releaseDirectory 'resources'
    if (Test-Path -LiteralPath $resources -PathType Container) {
        Copy-Item -LiteralPath $resources -Destination (Join-Path $stagingPath 'resources') -Recurse
    }
    $noticesPath = Join-Path $PSScriptRoot 'THIRD-PARTY-NOTICES.txt'
    if ($resolvedFfmpegPath) {
        Copy-Item -LiteralPath $noticesPath -Destination $stagingPath
    } else {
        $notices = Get-Content -LiteralPath $noticesPath -Raw
        $notices = [regex]::Replace($notices, '(?ms)^FFmpeg [^\r\n]+\r?\n.*?(?=^ONNX Runtime)', '')
        [IO.File]::WriteAllText(
            (Join-Path $stagingPath 'THIRD-PARTY-NOTICES.txt'),
            $notices,
            [Text.UTF8Encoding]::new($false)
        )
    }

    $licenseSources = @(
        @{ Source = (Join-Path $PSScriptRoot 'licenses/ONNX-Runtime-MIT.txt'); Name = 'ONNX-Runtime-MIT.txt' },
        @{ Source = (Join-Path $PSScriptRoot 'licenses/Nemotron-OpenMDW-1.1.txt'); Name = 'Nemotron-OpenMDW-1.1.txt' }
    )
    if ($resolvedFfmpegPath) {
        $licenseSources = @(
            @{ Source = (Join-Path (Split-Path -Parent $resolvedFfmpegPath) 'licenses/FFmpeg-LGPL-2.1.txt'); Name = 'FFmpeg-LGPL-2.1.txt' },
            @{ Source = (Join-Path (Split-Path -Parent $resolvedFfmpegPath) 'licenses/libopus-BSD.txt'); Name = 'libopus-BSD.txt' }
        ) + $licenseSources
    }
    $licensesDirectory = Join-Path $stagingPath 'licenses'
    [void][IO.Directory]::CreateDirectory($licensesDirectory)
    foreach ($license in $licenseSources) {
        if (-not (Test-Path -LiteralPath $license.Source -PathType Leaf)) {
            throw "Required license file is missing: $($license.Source)"
        }
        Copy-Item -LiteralPath $license.Source -Destination (Join-Path $licensesDirectory $license.Name)
    }

    $ffmpegReadme = if ($resolvedFfmpegPath) {
        'The portable folder includes FFmpeg and the runtime DLLs.'
    } else {
        'This local package does not include FFmpeg; install it on PATH to use audio features.'
    }
    $readme = @"
Sottovoce for Windows x64

Run sottovoce.exe. $ffmpegReadme
Set your ElevenLabs API key in Settings or in %APPDATA%\Sottovoce\config.toml.
Meetings default to %USERPROFILE%\Documents\Meetings. Nemotron diarization
downloads its model the first time it is used.

This build is unsigned. Windows SmartScreen may show a warning when you run it.
See THIRD-PARTY-NOTICES.txt and the licenses folder for component notices.
"@
    Set-Content -LiteralPath (Join-Path $stagingPath 'README.txt') -Value $readme -Encoding UTF8

    Compress-Archive -Path (Join-Path $stagingPath '*') -DestinationPath $archivePath -CompressionLevel Optimal
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archivePath)
    try {
        $entries = @($zip.Entries | ForEach-Object { $_.FullName })
    } finally {
        $zip.Dispose()
    }
    $requiredEntries = @(
        'sottovoce.exe', 'DirectML.dll', 'README.txt',
        'THIRD-PARTY-NOTICES.txt', 'licenses/ONNX-Runtime-MIT.txt',
        'licenses/Nemotron-OpenMDW-1.1.txt'
    )
    if ($resolvedFfmpegPath) {
        $requiredEntries += @('ffmpeg.exe', 'licenses/FFmpeg-LGPL-2.1.txt', 'licenses/libopus-BSD.txt')
    }
    foreach ($requiredEntry in $requiredEntries) {
        if ($entries -notcontains $requiredEntry) { throw "Package archive is missing $requiredEntry" }
    }
    Move-Item -LiteralPath $archivePath -Destination $zipPath -Force
    Write-Host "Portable package: $zipPath"
    Write-Host ("Archive size: {0:N2} MB" -f ((Get-Item -LiteralPath $zipPath).Length / 1MB))
} finally {
    if (Test-Path -LiteralPath $archivePath) { Remove-Item -LiteralPath $archivePath -Force }
    if (Test-Path -LiteralPath $stagingPath) { Remove-Item -LiteralPath $stagingPath -Recurse -Force }
}
