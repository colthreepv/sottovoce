<#
.SYNOPSIS
  Run cargo for Sottovoce with a shared CPU budget.

.DESCRIPTION
  Every agent and test run must call cargo through this wrapper:
    pwsh -File scripts/cargo.ps1 -Role adept test
    pwsh -File scripts/cargo.ps1 -Role builder check --workspace

  - At most -Slots (default 2) cargo runs execute at once on this machine; the
    others wait on a named semaphore shared by every process in the session.
  - cargo and every rustc/linker it spawns run at BelowNormal priority
    (child processes inherit the priority class).
  - Jobs are capped by ~/.cargo/config.toml (jobs = 3) unless -j is passed.
  - CARGO_TARGET_DIR defaults to %TEMP%\sottovoce-<role>-target, so roles never
    share or clobber a target, and nothing is written to the user's builds.
#>
param(
    [string]$Role = $(if ($env:SOTTOVOCE_ROLE) { $env:SOTTOVOCE_ROLE } else { 'dev' }),
    [int]$Slots = 2,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CargoArgs
)
$ErrorActionPreference = 'Stop'

if ($Role -notmatch '^[A-Za-z0-9_-]+$') { throw "Invalid -Role '$Role'" }
if (-not $env:CARGO_TARGET_DIR) {
    $env:CARGO_TARGET_DIR = Join-Path $env:TEMP "sottovoce-$($Role.ToLower())-target"
}
$buildsRoot = Join-Path $env:LOCALAPPDATA 'Sottovoce\builds'
if ([IO.Path]::GetFullPath($env:CARGO_TARGET_DIR).StartsWith([IO.Path]::GetFullPath($buildsRoot), 'OrdinalIgnoreCase')) {
    throw "CARGO_TARGET_DIR must not be under $buildsRoot"
}

[Diagnostics.Process]::GetCurrentProcess().PriorityClass = 'BelowNormal'

$semaphore = [Threading.Semaphore]::new($Slots, $Slots, 'Local\SottovoceCargoSlots')
$acquired = $false
try {
    if (-not $semaphore.WaitOne(0)) {
        Write-Host "[cargo.ps1] waiting for a build slot ($Slots in use)..."
        [void]$semaphore.WaitOne()
    }
    $acquired = $true
    Write-Host "[cargo.ps1] role=$Role target=$env:CARGO_TARGET_DIR"
    & cargo @CargoArgs
    $code = $LASTEXITCODE
}
finally {
    if ($acquired) { [void]$semaphore.Release() }
    $semaphore.Dispose()
}
exit $code

