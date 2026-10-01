param(
    [string]$FfmpegPath = (Join-Path $PSScriptRoot 'dist\ffmpeg.exe'),
    [string]$ReferenceFfmpeg = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function ConvertTo-QuotedArgument([string]$Argument) {
    if ($Argument.Length -gt 0 -and $Argument -notmatch '[\s"]') {
        return $Argument
    }

    $builder = New-Object System.Text.StringBuilder
    [void]$builder.Append('"')
    $slashes = 0
    foreach ($character in $Argument.ToCharArray()) {
        if ($character -eq [char]'\') {
            $slashes++
        } elseif ($character -eq [char]'"') {
            if ($slashes -gt 0) {
                [void]$builder.Append([char]'\', 2 * $slashes)
            }
            [void]$builder.Append([char]'\')
            [void]$builder.Append([char]'"')
            $slashes = 0
        } else {
            if ($slashes -gt 0) {
                [void]$builder.Append([char]'\', $slashes)
                $slashes = 0
            }
            [void]$builder.Append($character)
        }
    }
    if ($slashes -gt 0) {
        [void]$builder.Append([char]'\', 2 * $slashes)
    }
    [void]$builder.Append('"')
    return $builder.ToString()
}

function Invoke-ProcessCapture {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [string]$StdinPath,
        [string]$StdoutPath
    )

    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $Executable
    $startInfo.Arguments = (($Arguments | ForEach-Object { ConvertTo-QuotedArgument $_ }) -join ' ')
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardInput = -not [string]::IsNullOrEmpty($StdinPath)

    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $startInfo
    if (-not $process.Start()) {
        throw "Could not start $Executable"
    }

    $stderrTask = $process.StandardError.ReadToEndAsync()
    $stdinTransferError = $null
    if (-not [string]::IsNullOrEmpty($StdinPath)) {
        $inputStream = [System.IO.File]::OpenRead($StdinPath)
        try {
            try {
                $inputStream.CopyTo($process.StandardInput.BaseStream)
            } catch [System.IO.IOException] {
                $stdinTransferError = $_.Exception.Message
            }
        } finally {
            $inputStream.Dispose()
            try {
                $process.StandardInput.Close()
            } catch [System.IO.IOException] {
                if ([string]::IsNullOrEmpty($stdinTransferError)) {
                    $stdinTransferError = $_.Exception.Message
                }
            }
        }
    }

    if (-not [string]::IsNullOrEmpty($StdoutPath)) {
        $outputStream = [System.IO.File]::Create($StdoutPath)
        try {
            $process.StandardOutput.BaseStream.CopyTo($outputStream)
        } finally {
            $outputStream.Dispose()
        }
        $stdout = ''
    } else {
        $stdout = $process.StandardOutput.ReadToEnd()
    }

    $process.WaitForExit()
    $stderr = $stderrTask.GetAwaiter().GetResult()
    $exitCode = $process.ExitCode
    $process.Dispose()
    if ($exitCode -eq 0 -and -not [string]::IsNullOrEmpty($stdinTransferError)) {
        throw "Could not deliver all stdin to ${Executable}: $stdinTransferError"
    }
    return [pscustomobject]@{
        ExitCode = $exitCode
        Stdout = $stdout
        Stderr = $stderr
    }
}

function Assert-ProcessSucceeded($Result, [string]$Description) {
    if ($Result.ExitCode -ne 0) {
        throw "$Description failed (exit $($Result.ExitCode)): $($Result.Stderr.Trim())"
    }
}

if (-not (Test-Path -LiteralPath $FfmpegPath -PathType Leaf)) {
    throw "Bundled FFmpeg not found: $FfmpegPath"
}
$FfmpegPath = (Resolve-Path -LiteralPath $FfmpegPath).Path
if ([string]::IsNullOrWhiteSpace($ReferenceFfmpeg)) {
    $referenceCommand = Get-Command ffmpeg.exe -ErrorAction Stop
    $ReferenceFfmpeg = $referenceCommand.Source
}
$ReferenceFfmpeg = (Resolve-Path -LiteralPath $ReferenceFfmpeg).Path

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$fixtureRoot = Join-Path $repoRoot 'testdata\fixtures'
$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("meeting-recorder-ffmpeg-check-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tempRoot | Out-Null

try {
    $encoderListing = Invoke-ProcessCapture -Executable $FfmpegPath -Arguments @('-hide_banner', '-nostdin', '-loglevel', 'error', '-encoders')
    Assert-ProcessSucceeded $encoderListing 'Bundled FFmpeg -encoders'
    if ($encoderListing.Stdout -notmatch '(?m)\s+libopus\b') {
        throw 'The bundled FFmpeg encoder list does not contain libopus.'
    }
    Write-Host 'PASS: -encoders lists libopus'

    $rawPcm = Join-Path $tempRoot 'capture-input.s16le'
    $generated = Invoke-ProcessCapture -Executable $ReferenceFfmpeg -Arguments @(
        '-hide_banner', '-nostdin', '-loglevel', 'error', '-f', 'lavfi',
        '-i', 'sine=frequency=440:sample_rate=48000:duration=1.5',
        '-f', 's16le', '-acodec', 'pcm_s16le', '-ar', '48000', '-ac', '1', 'pipe:1'
    ) -StdoutPath $rawPcm
    Assert-ProcessSucceeded $generated 'System FFmpeg raw PCM generation'

    $encodedOgg = Join-Path $tempRoot 'capture-output.ogg'
    $encoded = Invoke-ProcessCapture -Executable $FfmpegPath -Arguments @(
        '-hide_banner', '-nostdin', '-loglevel', 'error', '-y',
        '-f', 's16le', '-ar', '48000', '-ac', '1', '-i', 'pipe:0',
        '-map_metadata', '-1', '-c:a', 'libopus', '-b:a', '48000', '-vbr', 'on',
        '-application', 'voip', '-f', 'ogg', $encodedOgg
    ) -StdinPath $rawPcm
    Assert-ProcessSucceeded $encoded 'Bundled FFmpeg s16le pipe to Ogg/Opus'
    if (-not (Test-Path -LiteralPath $encodedOgg) -or (Get-Item -LiteralPath $encodedOgg).Length -eq 0) {
        throw 'The bundled FFmpeg produced an empty Ogg recording.'
    }
    Write-Host ("PASS: s16le stdin to Ogg/Opus ({0:N0} bytes)" -f (Get-Item -LiteralPath $encodedOgg).Length)

    $fixtures = @(Get-ChildItem -LiteralPath $fixtureRoot -Filter '*.ogg' -File -Recurse)
    if ($fixtures.Count -eq 0) {
        throw "No Ogg fixtures found under $fixtureRoot"
    }
    foreach ($fixture in $fixtures) {
        $safeName = ($fixture.FullName.Substring($fixtureRoot.Length) -replace '[^A-Za-z0-9.-]', '_')
        $referenceRaw = Join-Path $tempRoot "$safeName.reference.f32le"
        $bundledRaw = Join-Path $tempRoot "$safeName.bundled.f32le"
        $decodeArguments = @(
            '-hide_banner', '-nostdin', '-loglevel', 'error', '-i', $fixture.FullName,
            '-vn', '-f', 'f32le', '-acodec', 'pcm_f32le', '-ar', '16000', '-ac', '1', 'pipe:1'
        )
        $referenceDecode = Invoke-ProcessCapture -Executable $ReferenceFfmpeg -Arguments $decodeArguments -StdoutPath $referenceRaw
        Assert-ProcessSucceeded $referenceDecode "System FFmpeg decode of $($fixture.FullName)"
        $bundledDecode = Invoke-ProcessCapture -Executable $FfmpegPath -Arguments $decodeArguments -StdoutPath $bundledRaw
        Assert-ProcessSucceeded $bundledDecode "Bundled FFmpeg decode of $($fixture.FullName)"

        $referenceBytes = (Get-Item -LiteralPath $referenceRaw).Length
        $bundledBytes = (Get-Item -LiteralPath $bundledRaw).Length
        if (($referenceBytes % 4) -ne 0 -or ($bundledBytes % 4) -ne 0) {
            throw "Non-f32-aligned output while decoding $($fixture.FullName)"
        }
        $referenceSamples = $referenceBytes / 4
        $bundledSamples = $bundledBytes / 4
        if ($referenceSamples -ne $bundledSamples) {
            throw "Sample count differs for $($fixture.FullName): bundled $bundledSamples, system $referenceSamples"
        }
        Write-Host ("PASS: {0} -> f32le 16 kHz mono ({1:N0} samples)" -f $fixture.FullName.Substring($fixtureRoot.Length + 1), $bundledSamples)
    }

    $importInputs = @(
        @{ Name = 'mp3'; Codec = 'libmp3lame'; Extension = 'mp3'; Extra = @('-q:a', '4', '-f', 'mp3') },
        @{ Name = 'm4a'; Codec = 'aac'; Extension = 'm4a'; Extra = @('-b:a', '96k', '-f', 'ipod') },
        @{ Name = 'wav'; Codec = 'pcm_s16le'; Extension = 'wav'; Extra = @('-f', 'wav') }
    )
    foreach ($format in $importInputs) {
        $inputPath = Join-Path $tempRoot "import-test.$($format.Extension)"
        $created = Invoke-ProcessCapture -Executable $ReferenceFfmpeg -Arguments (@(
            '-hide_banner', '-nostdin', '-loglevel', 'error', '-y', '-f', 'lavfi',
            '-i', 'sine=frequency=660:sample_rate=48000:duration=0.8', '-vn', '-c:a', $format.Codec
        ) + $format.Extra + @($inputPath))
        Assert-ProcessSucceeded $created "System FFmpeg $($format.Name) creation"

        $decodedPath = Join-Path $tempRoot "import-test.$($format.Name).f32le"
        $decoded = Invoke-ProcessCapture -Executable $FfmpegPath -Arguments @(
            '-hide_banner', '-nostdin', '-loglevel', 'error', '-i', $inputPath,
            '-vn', '-f', 'f32le', '-acodec', 'pcm_f32le', '-ar', '16000', '-ac', '1', 'pipe:1'
        ) -StdoutPath $decodedPath
        Assert-ProcessSucceeded $decoded "Bundled FFmpeg $($format.Name) decode"
        if ((Get-Item -LiteralPath $decodedPath).Length -eq 0) {
            throw "Bundled FFmpeg produced no samples decoding $($format.Name)."
        }
        Write-Host "PASS: system-generated $($format.Name) decodes to f32le 16 kHz mono"
    }
} finally {
    Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}
