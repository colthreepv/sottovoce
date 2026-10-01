param(
    [string]$FfmpegVersion = '8.1.3',
    [string]$OpusVersion = '1.5.2'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$dockerCommand = Get-Command docker.exe -ErrorAction Stop
$outputPath = Join-Path $PSScriptRoot 'dist'
$outputFullPath = [System.IO.Path]::GetFullPath($outputPath)
$archivePins = @{
    'ffmpeg-8.1.3.tar.xz' = @{
        Url = 'https://ffmpeg.org/releases/ffmpeg-8.1.3.tar.xz'
        Sha256 = '7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3'
    }
    'opus-1.5.2.tar.gz' = @{
        Url = 'https://downloads.xiph.org/releases/opus/opus-1.5.2.tar.gz'
        Sha256 = '65c1d2f78b9f2fb20082c38cbe47c951ad5839345876e46941612ee87f9a7ce1'
    }
}
$ffmpegArchiveName = "ffmpeg-$FfmpegVersion.tar.xz"
$opusArchiveName = "opus-$OpusVersion.tar.gz"
if (-not $archivePins.ContainsKey($ffmpegArchiveName) -or -not $archivePins.ContainsKey($opusArchiveName)) {
    throw 'Update the pinned source URLs and SHA-256 values before changing the FFmpeg or libopus versions.'
}
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
    --tag "meeting-recorder-ffmpeg:$FfmpegVersion-opus-$OpusVersion" `
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
