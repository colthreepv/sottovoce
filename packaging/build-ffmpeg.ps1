param(
    [string]$FfmpegVersion,
    [string]$OpusVersion
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$dockerCommand = Get-Command docker.exe -ErrorAction Stop
$outputPath = Join-Path $PSScriptRoot 'dist'
$outputFullPath = [System.IO.Path]::GetFullPath($outputPath)
$pins = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'ffmpeg-pins.json') -Raw | ConvertFrom-Json
if ([string]::IsNullOrWhiteSpace($FfmpegVersion)) { $FfmpegVersion = $pins.ffmpeg.version }
if ([string]::IsNullOrWhiteSpace($OpusVersion)) { $OpusVersion = $pins.opus.version }
if ($FfmpegVersion -ne $pins.ffmpeg.version -or $OpusVersion -ne $pins.opus.version) {
    throw 'Requested versions must match packaging/ffmpeg-pins.json; update the pins and Dockerfile together.'
}
$archivePins = @{
    "ffmpeg-$FfmpegVersion.tar.xz" = @{ Url = $pins.ffmpeg.url; Sha256 = $pins.ffmpeg.sha256 }
    "opus-$OpusVersion.tar.gz" = @{ Url = $pins.opus.url; Sha256 = $pins.opus.sha256 }
}
$ffmpegArchiveName = "ffmpeg-$FfmpegVersion.tar.xz"
$opusArchiveName = "opus-$OpusVersion.tar.gz"
$cachePath = Join-Path $PSScriptRoot 'build-cache'
New-Item -ItemType Directory -Path $cachePath -Force | Out-Null
foreach ($archiveName in @($ffmpegArchiveName, $opusArchiveName)) {
    $archivePath = Join-Path $cachePath $archiveName
    $expectedHash = $archivePins[$archiveName].Sha256
    $downloadNeeded = -not (Test-Path -LiteralPath $archivePath -PathType Leaf)
    if (-not $downloadNeeded) {
        $downloadNeeded = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expectedHash
    }
    if ($downloadNeeded) {
        $partialPath = "$archivePath.part"
        Remove-Item -LiteralPath $partialPath -Force -ErrorAction SilentlyContinue
        Invoke-WebRequest -Uri $archivePins[$archiveName].Url -OutFile $partialPath -TimeoutSec 180
        $actualHash = (Get-FileHash -LiteralPath $partialPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -ne $expectedHash) {
            Remove-Item -LiteralPath $partialPath -Force
            throw "SHA-256 mismatch for $archiveName (got $actualHash)"
        }
        Move-Item -LiteralPath $partialPath -Destination $archivePath -Force
    }
}
New-Item -ItemType Directory -Path $outputFullPath -Force | Out-Null

& $dockerCommand.Source buildx build `
    --platform linux/amd64 `
    --build-arg "FFMPEG_VERSION=$FfmpegVersion" `
    --build-arg "OPUS_VERSION=$OpusVersion" `
    --target artifact `
    --output "type=local,dest=$outputFullPath" `
    --tag "sottovoce-ffmpeg:$FfmpegVersion-opus-$OpusVersion" `
    --file (Join-Path $PSScriptRoot 'Dockerfile') `
    $PSScriptRoot
if ($LASTEXITCODE -ne 0) {
    throw "Docker FFmpeg cross-build failed with exit code $LASTEXITCODE"
}

$ffmpegPath = Join-Path $outputFullPath 'ffmpeg.exe'
if (-not (Test-Path -LiteralPath $ffmpegPath -PathType Leaf)) {
    throw "Docker build completed without producing $ffmpegPath"
}
$size = (Get-Item -LiteralPath $ffmpegPath).Length
if ($size -ge 10MB) {
    throw "FFmpeg is $size bytes; expected a binary smaller than 10 MB"
}
Write-Host ("Built {0:N2} MB: {1}" -f ($size / 1MB), $ffmpegPath)
Write-Host 'Run .\verify-ffmpeg.ps1 to check the exact application command shapes.'
