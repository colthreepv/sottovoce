#requires -Version 5.1
<#
.SYNOPSIS
    End-to-end QA harness for the Sottovoce development CLI.

.DESCRIPTION
    Builds the release binary in its own CARGO_TARGET_DIR, exercises the CLI
    (selftest, offline pipeline with cached STT responses, and help/devices hygiene).

    Every check is recorded as PASS / FAIL / WARN / INFO and printed as a
    table at the end. The script exits 1 when any check FAILs.

    No ElevenLabs spend: the pipeline runs against the cached responses in
    testdata/stt/.

.PARAMETER SkipBuild
    Reuse the exe already in the target dir instead of running cargo build.
.PARAMETER SkipSelftest
    Skip the audio selftest (useful on a machine with no capture devices).
.PARAMETER SkipGui
    Skip GUI checks; this harness tests the development CLI only.
.PARAMETER KeepWork
    Keep the temporary work folder (meeting fixtures, temp APPDATA).
.PARAMETER WorkRoot
    Where temporary state is written. Default %TEMP%\sottovoce-qa.
.EXAMPLE
    pwsh -File scripts/qa/run-e2e.ps1
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$SkipSelftest,
    [switch]$SkipGui,
    [switch]$KeepWork,
    [string]$WorkRoot = (Join-Path $env:TEMP 'sottovoce-qa')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
# Keep native non-zero exit codes from throwing on newer PowerShell hosts.
if (Test-Path variable:PSNativeCommandUseErrorActionPreference) {
    $PSNativeCommandUseErrorActionPreference = $false
}

# --------------------------------------------------------------------------- #
# Result bookkeeping
# --------------------------------------------------------------------------- #

$script:Results = New-Object System.Collections.Generic.List[object]

function Add-Result {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][ValidateSet('PASS', 'FAIL', 'WARN', 'INFO')][string]$Status,
        [string]$Detail = ''
    )
    $script:Results.Add([pscustomobject]@{ Check = $Name; Status = $Status; Detail = $Detail })
    $color = switch ($Status) {
        'PASS' { 'Green' }
        'FAIL' { 'Red' }
        'WARN' { 'Yellow' }
        default { 'Gray' }
    }
    Write-Host ('  [{0,-4}] {1}{2}' -f $Status, $Name, $(if ($Detail) { " - $Detail" } else { '' })) -ForegroundColor $color
}

function Write-Step { param([string]$Text) Write-Host ''; Write-Host "== $Text" -ForegroundColor Cyan }

# --------------------------------------------------------------------------- #
# Process helpers
# --------------------------------------------------------------------------- #

# Windows CreateProcess argument quoting (works on Windows PowerShell 5.1 and pwsh).
function ConvertTo-Arg {
    param([string]$Value)
    if ($null -eq $Value) { return '""' }
    if ($Value.Length -gt 0 -and $Value -notmatch '[\s"]') { return $Value }
    $sb = New-Object System.Text.StringBuilder
    [void]$sb.Append('"')
    $backslashes = 0
    foreach ($ch in $Value.ToCharArray()) {
        if ($ch -eq '\') {
            $backslashes++
            continue
        }
        if ($ch -eq '"') {
            [void]$sb.Append([char]'\', (2 * $backslashes) + 1)
            $backslashes = 0
            [void]$sb.Append('"')
            continue
        }
        if ($backslashes -gt 0) {
            [void]$sb.Append([char]'\', $backslashes)
            $backslashes = 0
        }
        [void]$sb.Append($ch)
    }
    if ($backslashes -gt 0) { [void]$sb.Append([char]'\', 2 * $backslashes) }
    [void]$sb.Append('"')
    return $sb.ToString()
}

function Invoke-Captured {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$File,
        [string[]]$Arguments = @(),
        [hashtable]$Environment = @{},
        [string]$WorkingDirectory = (Get-Location).Path,
        [int]$TimeoutMs = 600000,
        [string]$StandardInput = $null
    )
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $File
    $psi.Arguments = (($Arguments | ForEach-Object { ConvertTo-Arg $_ }) -join ' ')
    $psi.WorkingDirectory = $WorkingDirectory
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.RedirectStandardInput = $true
    foreach ($key in $Environment.Keys) { $psi.EnvironmentVariables[[string]$key] = [string]$Environment[$key] }

    $proc = New-Object System.Diagnostics.Process
    $proc.StartInfo = $psi
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    [void]$proc.Start()
    if ($null -ne $StandardInput) { $proc.StandardInput.Write($StandardInput) }
    $proc.StandardInput.Close()
    $outTask = $proc.StandardOutput.ReadToEndAsync()
    $errTask = $proc.StandardError.ReadToEndAsync()
    $exited = $proc.WaitForExit($TimeoutMs)
    if (-not $exited) {
        try { $proc.Kill($true) } catch { try { $proc.Kill() } catch { } }
        [void]$proc.WaitForExit(5000)
    }
    $sw.Stop()
    [pscustomobject]@{
        ExitCode = if ($exited) { $proc.ExitCode } else { $null }
        Stdout   = $outTask.Result
        Stderr   = $errTask.Result
        TimedOut = (-not $exited)
        Seconds  = [math]::Round($sw.Elapsed.TotalSeconds, 2)
    }
}

function Get-Tail {
    param([string]$Text, [int]$Lines = 12)
    if ([string]::IsNullOrWhiteSpace($Text)) { return '' }
    $split = $Text -split "`r?`n"
    ($split | Where-Object { $_ -ne '' } | Select-Object -Last $Lines) -join ' | '
}

# UTF-8 without BOM: serde_json and the toml crate choke on a leading BOM.
function Set-TextNoBom {
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Value)
    $encoding = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllText($Path, $Value, $encoding)
}

# --------------------------------------------------------------------------- #
# Paths
# --------------------------------------------------------------------------- #

$crate = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$repo = $crate
$fixtures = Join-Path $repo 'testdata\fixtures'
$sttCache = Join-Path $crate 'testdata\stt'
$targetDir = Join-Path $env:TEMP 'sottovoce-qa-target'
$exe = Join-Path $targetDir 'release\sottovoce-dev-cli.exe'

$runStamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$work = Join-Path $WorkRoot "run-$runStamp"
$meetingsRoot = Join-Path $work 'meetings'
$tempAppData = Join-Path $work 'appdata'
New-Item -ItemType Directory -Force -Path $work, $meetingsRoot, $tempAppData | Out-Null

Write-Host "Sottovoce QA harness" -ForegroundColor White
Write-Host "crate:      $crate"
Write-Host "work:       $work"
Write-Host "target:     $targetDir"

# --------------------------------------------------------------------------- #
# 1. Build
# --------------------------------------------------------------------------- #

Write-Step '1. Build release binary'
if ($SkipBuild) {
    if (-not (Test-Path -LiteralPath $exe)) {
        Add-Result 'build' 'FAIL' "-SkipBuild but no exe at $exe"
        throw "missing $exe"
    }
    Add-Result 'build' 'INFO' "skipped, using $exe"
} else {
    $build = Invoke-Captured -File 'pwsh' `
        -Arguments @('-NoProfile', '-File', (Join-Path $repo 'scripts\cargo.ps1'), '-Role', 'builder', 'build', '--release', '--bin', 'sottovoce-dev-cli', '--manifest-path', (Join-Path $crate 'Cargo.toml'), '--target-dir', $targetDir) `
        -WorkingDirectory $crate -TimeoutMs 1800000
    if ($build.TimedOut) {
        Add-Result 'build' 'FAIL' 'cargo build timed out after 30 min'
    } elseif ($build.ExitCode -ne 0) {
        Add-Result 'build' 'FAIL' "cargo build exit $($build.ExitCode): $(Get-Tail $build.Stderr)"
    } elseif (-not (Test-Path -LiteralPath $exe)) {
        Add-Result 'build' 'FAIL' "build succeeded but $exe is missing"
    } else {
        Add-Result 'build' 'PASS' "$([math]::Round((Get-Item -LiteralPath $exe).Length / 1MB, 1)) MB in $($build.Seconds)s"
    }
}

if (-not (Test-Path -LiteralPath $exe)) {
    Write-Host 'No exe: cannot continue.' -ForegroundColor Red
    $script:Results | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
    exit 1
}

# --------------------------------------------------------------------------- #
# 2. Selftest: Remote track peak above -40 dBFS, both durations ~4 s
# --------------------------------------------------------------------------- #

Write-Step '2. Selftest (tone through loopback)'
if ($SkipSelftest) {
    Add-Result 'selftest' 'INFO' 'skipped'
} else {
    $env:CARGO_TARGET_DIR = $targetDir
    $self = Invoke-Captured -File $exe -Arguments @('selftest') -WorkingDirectory $work -TimeoutMs 120000
    $combined = "$($self.Stdout)`n$($self.Stderr)"
    $pattern = '(?m)^(You|Remote)\s+([0-9.]+)s\s+peak\s+(-?[0-9.]+)\s+dBFS'
    $matchesFound = [regex]::Matches($combined, $pattern)
    if ($self.TimedOut) {
        Add-Result 'selftest' 'FAIL' 'timed out after 120s'
    } elseif ($matchesFound.Count -lt 2) {
        Add-Result 'selftest' 'FAIL' "no level lines (exit $($self.ExitCode)): $(Get-Tail $combined)"
    } else {
        $levels = @{}
        foreach ($m in $matchesFound) { $levels[$m.Groups[1].Value] = @{ Duration = [double]$m.Groups[2].Value; Peak = [double]$m.Groups[3].Value } }
        $problems = @()
        foreach ($side in @('You', 'Remote')) {
            if (-not $levels.ContainsKey($side)) { $problems += "$side missing"; continue }
            $d = $levels[$side].Duration
            if ($d -lt 3.0 -or $d -gt 5.5) { $problems += "$side duration ${d}s not ~4s" }
        }
        if ($levels.ContainsKey('You') -and $levels.ContainsKey('Remote') -and
            [math]::Abs($levels['You'].Duration - $levels['Remote'].Duration) -gt 1.0) {
            $problems += 'track durations differ by more than 1s'
        }
        if ($levels.ContainsKey('Remote') -and $levels['Remote'].Peak -le -40.0) {
            $problems += "Remote peak $($levels['Remote'].Peak) dBFS is not above -40"
        }
        $summary = (($levels.Keys | Sort-Object) | ForEach-Object { "$_ $($levels[$_].Duration)s @ $($levels[$_].Peak) dBFS" }) -join ', '
        if ($problems.Count -gt 0) {
            Add-Result 'selftest' 'FAIL' (($problems -join '; ') + " [$summary]")
        } else {
            Add-Result 'selftest' 'PASS' $summary
        }
    }
}

# --------------------------------------------------------------------------- #
# 3. Offline pipeline against cached STT responses
# --------------------------------------------------------------------------- #

Write-Step '3. Pipeline (cached STT, no API spend)'

# Use an explicit temp config path so this harness never touches real AppData.
$configToml = @(
    "meetings_dir = '$($meetingsRoot -replace '\\', '/')'",
    'diarize = true',
    'language = "auto"'
) -join "`n"
$qaConfigDir = Join-Path $work 'config'
New-Item -ItemType Directory -Force -Path $qaConfigDir | Out-Null
Set-TextNoBom -Path (Join-Path $qaConfigDir 'config.toml') -Value $configToml

$childEnv = @{
    'APPDATA'              = $tempAppData
    'SOTTOVOCE_CONFIG_DIR' = $qaConfigDir
    'ELEVENLABS_API_KEY' = 'qa-dummy-key-no-spend'
}

$startedMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() - 300000

function New-MeetingFixture {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][bool]$IncludeComputer
    )
    $src = Join-Path $fixtures $Name
    $dir = Join-Path $meetingsRoot $Name
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Copy-Item -LiteralPath (Join-Path $src 'mic.ogg') -Destination (Join-Path $dir 'mic.ogg') -Force

    if ($IncludeComputer) {
        Copy-Item -LiteralPath (Join-Path $src 'computer.ogg') -Destination (Join-Path $dir 'computer.ogg') -Force
    } else {
        # Silent computer track: the pipeline must detect it and skip it.
        $duration = 60.0
        $probe = Invoke-Captured -File 'ffprobe' -Arguments @(
            '-v', 'error', '-show_entries', 'format=duration', '-of', 'csv=p=0',
            (Join-Path $dir 'mic.ogg')
        ) -WorkingDirectory $work -TimeoutMs 30000
        if ($probe.ExitCode -eq 0) {
            $parsed = 0.0
            if ([double]::TryParse(($probe.Stdout.Trim()), [ref]$parsed) -and $parsed -gt 0) { $duration = $parsed }
        }
        $gen = Invoke-Captured -File 'ffmpeg' -Arguments @(
            '-y', '-f', 'lavfi', '-i', 'anullsrc=r=48000:cl=mono',
            '-t', ('{0:0.###}' -f $duration), '-c:a', 'libopus', '-b:a', '48000',
            (Join-Path $dir 'computer.ogg')
        ) -WorkingDirectory $work -TimeoutMs 60000
        if ($gen.ExitCode -ne 0) { throw "could not generate silent computer track: $(Get-Tail $gen.Stderr)" }

        # No computer cache: a silent track never reaches STT.
    }

    # Cached STT responses copied where pipeline.rs looks for them
    # (.stt-mic.json / .stt-computer.json).
    Copy-Item -LiteralPath (Join-Path $sttCache "$Name-mic.json") -Destination (Join-Path $dir '.stt-mic.json') -Force
    if ($IncludeComputer -and (Test-Path -LiteralPath (Join-Path $sttCache "$Name-computer.json"))) {
        Copy-Item -LiteralPath (Join-Path $sttCache "$Name-computer.json") -Destination (Join-Path $dir '.stt-computer.json') -Force
    }

    $session = [ordered]@{
        status              = 'completed'
        started_at_unix_ms  = $startedMs
        stopped_at_unix_ms  = $startedMs + 60000
        mic                 = $null
        computer            = $null
        errors              = @()
    } | ConvertTo-Json -Depth 4
    Set-TextNoBom -Path (Join-Path $dir 'session.json') -Value $session
    return $dir
}

$pipelineCases = @(
    [pscustomobject]@{ Name = 'call'; Truth = 'call'; IncludeComputer = $true },
    [pscustomobject]@{ Name = 'call-speakers'; Truth = 'call-speakers'; IncludeComputer = $true },
    [pscustomobject]@{ Name = 'room'; Truth = 'room'; IncludeComputer = $false }
)

foreach ($case in $pipelineCases) {
    $folder = New-MeetingFixture -Name $case.Name -IncludeComputer $case.IncludeComputer
    $run = Invoke-Captured -File $exe -Arguments @('process', $folder) `
        -Environment $childEnv -WorkingDirectory $work -TimeoutMs 1800000

    $meetingJson = Join-Path $folder 'meeting.json'
    $transcript = Join-Path $folder 'transcript.md'
    if ($run.TimedOut) {
        Add-Result "pipeline/$($case.Name)" 'FAIL' 'timed out after 30 min'
        continue
    }
    if ($run.ExitCode -ne 0) {
        Add-Result "pipeline/$($case.Name)" 'FAIL' "exit $($run.ExitCode): $(Get-Tail $run.Stderr)"
        continue
    }
    if (-not (Test-Path -LiteralPath $meetingJson) -or -not (Test-Path -LiteralPath $transcript)) {
        Add-Result "pipeline/$($case.Name)" 'FAIL' 'meeting.json or transcript.md missing'
        continue
    }

    $meeting = Get-Content -LiteralPath $meetingJson -Raw | ConvertFrom-Json
    $actualSpeakers = @($meeting.speakers).Count
    $actualUtterances = @($meeting.utterances).Count

    $truth = Get-Content -LiteralPath (Join-Path $fixtures "$($case.Truth)\truth.json") -Raw | ConvertFrom-Json
    $sides = @('mic')
    if ($case.IncludeComputer) { $sides += 'computer' }
    $expectedSpeakers = @(
        $truth.truth | Where-Object { $sides -contains $_.side } | Select-Object -ExpandProperty speaker -Unique
    ).Count

    $detail = "speakers $actualSpeakers (truth $expectedSpeakers), utterances $actualUtterances, $($run.Seconds)s"
    if ($actualUtterances -eq 0) {
        Add-Result "pipeline/$($case.Name)" 'FAIL' "no utterances; $detail"
    } elseif ([math]::Abs($actualSpeakers - $expectedSpeakers) -gt 1) {
        Add-Result "pipeline/$($case.Name)" 'FAIL' $detail
    } else {
        Add-Result "pipeline/$($case.Name)" 'PASS' $detail
    }
}

# --------------------------------------------------------------------------- #
# 4. CLI hygiene
# --------------------------------------------------------------------------- #

Write-Step '4. CLI hygiene'

function Test-ClIcreatesFolders {
    param([Parameter(Mandatory)][string]$Label, [Parameter(Mandatory)][string[]]$CliArgs)
    $cwd = Join-Path $work ("cli-" + ($Label -replace '[^A-Za-z0-9]', '-'))
    New-Item -ItemType Directory -Force -Path $cwd | Out-Null
    $before = @(Get-ChildItem -LiteralPath $cwd -Force | Select-Object -ExpandProperty Name)
    $run = Invoke-Captured -File $exe -Arguments $CliArgs -Environment $childEnv `
        -WorkingDirectory $cwd -TimeoutMs 60000 -StandardInput ''
    $after = @(Get-ChildItem -LiteralPath $cwd -Force | Select-Object -ExpandProperty Name)
    $created = @($after | Where-Object { $before -notcontains $_ })
    $run | Add-Member -NotePropertyName CwdCreated -NotePropertyValue $created -Force
    return $run
}

$help = Test-ClIcreatesFolders -Label '--help' -CliArgs @('--help')
if ($help.CwdCreated.Count -gt 0) {
    Add-Result 'cli/--help' 'FAIL' "created: $($help.CwdCreated -join ', ')"
} else {
    Add-Result 'cli/--help' 'PASS' "no folders created (exit $($help.ExitCode), usage on stderr)"
}

$recordHelp = Test-ClIcreatesFolders -Label 'record-help' -CliArgs @('record', '--help')
if ($recordHelp.CwdCreated.Count -gt 0) {
    Add-Result 'cli/record --help' 'FAIL' "created folder(s): $($recordHelp.CwdCreated -join ', ') (treated as a recording dir)"
} else {
    Add-Result 'cli/record --help' 'PASS' "no folders created (exit $($recordHelp.ExitCode))"
}

$devices = Invoke-Captured -File $exe -Arguments @('devices') -Environment $childEnv -WorkingDirectory $work -TimeoutMs 60000
$deviceText = "$($devices.Stdout)`n$($devices.Stderr)"
$deviceCount = ([regex]::Matches($deviceText, '(?m)^(input|output):')).Count
if ($devices.ExitCode -ne 0) {
    Add-Result 'cli/devices' 'FAIL' "exit $($devices.ExitCode): $(Get-Tail $deviceText)"
} elseif ($deviceText -notmatch 'default input:' -or $deviceCount -eq 0) {
    Add-Result 'cli/devices' 'FAIL' "no devices listed: $(Get-Tail $deviceText)"
} else {
    Add-Result 'cli/devices' 'PASS' "$deviceCount device line(s)"
}

# --------------------------------------------------------------------------- #
# Summary
# --------------------------------------------------------------------------- #

Write-Step 'Summary'
$script:Results | Format-Table -AutoSize | Out-String -Width 220 | Write-Host
$failed = @($script:Results | Where-Object { $_.Status -eq 'FAIL' }).Count
$warned = @($script:Results | Where-Object { $_.Status -eq 'WARN' }).Count
$passed = @($script:Results | Where-Object { $_.Status -eq 'PASS' }).Count
Write-Host "$passed passed, $warned warned, $failed failed" -ForegroundColor $(if ($failed -gt 0) { 'Red' } else { 'Green' })


$script:ExitCode = if ($failed -gt 0) { 1 } else { 0 }

if (-not $KeepWork) {
    try { Remove-Item -LiteralPath $work -Recurse -Force } catch { }
} else {
    Write-Host "kept work: $work"
}

exit $script:ExitCode
