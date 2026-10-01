[CmdletBinding()]
param(
    [string]$BuildRoot,
    [int]$KeepBuilds = 3,
    [switch]$SkipBuild,
    [string]$TargetDirectory = (Join-Path $env:TEMP 'sottovoce-deploy-target'),
    [ValidateSet("debug", "release")][string]$Profile = "release"
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$manifestPath = Join-Path $repoRoot 'Cargo.toml'
$targetDirectory = [IO.Path]::GetFullPath($TargetDirectory)
$artifactDefinitions = @(
    @{ Name = 'sottovoce.exe'; Source = { param($releaseDir) Join-Path $releaseDir 'sottovoce.exe' }; Required = $true },
    @{ Name = 'meeting-recorder.exe'; Source = { param($releaseDir) Join-Path $releaseDir 'meeting-recorder.exe' }; Required = $true },
    @{ Name = 'ffmpeg.exe'; Source = { param($releaseDir) Join-Path $repoRoot 'packaging\dist\ffmpeg.exe' }; Required = $false }
)

if (-not $PSBoundParameters.ContainsKey('BuildRoot')) {
    $BuildRoot = Join-Path $env:LOCALAPPDATA 'Sottovoce\builds'
}
if ($KeepBuilds -lt 1) { throw 'KeepBuilds must be at least 1.' }
if ([string]::IsNullOrWhiteSpace($BuildRoot)) { throw 'BuildRoot cannot be empty.' }
$BuildRoot = [IO.Path]::GetFullPath([Environment]::ExpandEnvironmentVariables($BuildRoot))
[void][IO.Directory]::CreateDirectory($BuildRoot)

$metadataOutput = & (Join-Path $PSScriptRoot 'cargo.ps1') -Role adept -CargoArgs @('metadata', '--no-deps', '--format-version', '1', '--manifest-path', $manifestPath)
$metadataOutput = @($metadataOutput | Where-Object { -not ([string]$_).StartsWith('[cargo.ps1]') })
if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed with exit code $LASTEXITCODE." }
$metadata = ($metadataOutput -join [Environment]::NewLine) | ConvertFrom-Json
$package = $metadata.packages | Where-Object { $_.name -eq 'meeting-recorder-windows' } | Select-Object -First 1
if (-not $package) { throw 'Cargo metadata did not contain meeting-recorder-windows.' }

$sourceCommit = (& git -C $repoRoot rev-parse HEAD 2>$null | Select-Object -First 1)
if ($LASTEXITCODE -ne 0 -or -not $sourceCommit) { throw 'Could not identify the source commit.' }
$sourceCommit = ([string]$sourceCommit).Trim()
$sourceStatus = ((& git -C $repoRoot status --porcelain --untracked-files=all 2>$null) -join [Environment]::NewLine).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Could not inspect the source working tree.' }
$dirty = -not [string]::IsNullOrWhiteSpace($sourceStatus)
$shortCommit = (& git -C $repoRoot rev-parse --short=8 HEAD 2>$null | Select-Object -First 1)
if ($LASTEXITCODE -ne 0 -or -not $shortCommit) { throw 'Could not identify the short source commit.' }
$shortCommit = ([string]$shortCommit).Trim()
$baseBuildId = "$($package.version)-$shortCommit$(if ($dirty) { '-dirty' })"

if (-not $SkipBuild) {
    Write-Host "Building Sottovoce $($package.version) into '$targetDirectory'..."
    & bun run --cwd (Join-Path $repoRoot 'app/ui') build
    if ($LASTEXITCODE -ne 0) { throw 'Frontend build failed.' }
    $buildArgs = @('build', '--workspace', '--locked', '--manifest-path', $manifestPath, '--target-dir', $targetDirectory)
    if ($Profile -eq 'release') { $buildArgs += '--release' }
    & (Join-Path $PSScriptRoot 'cargo.ps1') -Role adept -CargoArgs $buildArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE." }
    $builtCommit = (& git -C $repoRoot rev-parse HEAD 2>$null | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0 -or ([string]$builtCommit).Trim() -ne $sourceCommit) {
        throw 'The source commit changed while Cargo was building; refusing to deploy this artifact.'
    }
    $postBuildStatus = ((& git -C $repoRoot status --porcelain --untracked-files=all 2>$null) -join [Environment]::NewLine).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'Could not recheck the source working tree after building.' }
    if ((-not [string]::IsNullOrWhiteSpace($postBuildStatus)) -ne $dirty) {
        throw 'The source working tree changed while Cargo was building; refusing to deploy this artifact.'
    }
}

$releaseDirectory = Join-Path $targetDirectory $Profile
$resolvedArtifacts = @()
foreach ($definition in $artifactDefinitions) {
    $source = & $definition.Source $releaseDirectory
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        if ($definition.Required) { throw "Required build artifact is missing: '$source'." }
        continue
    }
    $resolvedArtifacts += [pscustomobject]@{ Name = $definition.Name; Source = $source }
}

$rustcVersion = (& rustc --version --verbose | Out-String).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Could not record the Rust compiler version.' }
$cargoVersion = (& cargo --version | Select-Object -First 1)
if ($LASTEXITCODE -ne 0) { throw 'Could not record the Cargo version.' }

$buildId = $baseBuildId
$counter = 1
while (Test-Path -LiteralPath (Join-Path $BuildRoot $buildId)) {
    $buildId = "$baseBuildId-$counter"
    $counter++
}
$buildPath = Join-Path $BuildRoot $buildId
$stagingPath = Join-Path $BuildRoot ".staging-$([Guid]::NewGuid().ToString('N'))"
$latestPath = Join-Path $BuildRoot 'latest'
$nextLinkPath = Join-Path $BuildRoot ".latest-next-$([Guid]::NewGuid().ToString('N'))"
$oldLinkPath = Join-Path $BuildRoot ".latest-old-$([Guid]::NewGuid().ToString('N'))"
$previousBuildId = $null

try {
    [void][IO.Directory]::CreateDirectory($stagingPath)
    foreach ($artifact in $resolvedArtifacts) {
        Copy-Item -LiteralPath $artifact.Source -Destination (Join-Path $stagingPath $artifact.Name)
    }
    $buildInfo = [ordered]@{
        application = 'Sottovoce'
        version = [string]$package.version
        commit = $sourceCommit
        dirty = $dirty
        buildTimeUtc = [DateTime]::UtcNow.ToString('o')
        rustcVersion = $rustcVersion
        cargoVersion = ([string]$cargoVersion).Trim()
    }
    $buildInfo.profile = $Profile
    # Web assets and capabilities are compiled into sottovoce.exe. Tauri build
    # output may also contain runtime DLLs and a resources directory.
    Get-ChildItem -LiteralPath $releaseDirectory -Filter '*.dll' -File | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $stagingPath $_.Name)
    }
    $resources = Join-Path $releaseDirectory 'resources'
    if (Test-Path -LiteralPath $resources -PathType Container) {
        Copy-Item -LiteralPath $resources -Destination (Join-Path $stagingPath 'resources') -Recurse
    }
    $json = $buildInfo | ConvertTo-Json
    [IO.File]::WriteAllText((Join-Path $stagingPath 'build.json'), $json, [Text.UTF8Encoding]::new($false))
    [IO.Directory]::Move($stagingPath, $buildPath)

    [void](New-Item -ItemType Junction -Path $nextLinkPath -Target $buildPath)
    $hadPrevious = Test-Path -LiteralPath $latestPath
    if ($hadPrevious) {
        $latestItem = Get-Item -LiteralPath $latestPath -Force
        if (($latestItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0 -or $latestItem.LinkType -ne 'Junction') {
            throw "Refusing to replace '$latestPath' because it is not a junction."
        }
        $previousTarget = [IO.Path]::GetFullPath([string](@($latestItem.Target)[0]))
        $buildRootPrefix = $BuildRoot.TrimEnd('\') + '\'
        if (-not $previousTarget.StartsWith($buildRootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to replace '$latestPath' because its junction target is outside '$BuildRoot'."
        }
        $previousInfoPath = Join-Path $latestPath 'build.json'
        if (-not (Test-Path -LiteralPath $previousInfoPath -PathType Leaf)) {
            throw "Refusing to replace '$latestPath' because it does not point to a managed Sottovoce build."
        }
        $previousInfo = Get-Content -LiteralPath $previousInfoPath -Raw | ConvertFrom-Json
        if ($previousInfo.application -ne 'Sottovoce' -or -not $previousInfo.version -or -not $previousInfo.commit) {
            throw "Refusing to replace '$latestPath' because its build metadata is invalid."
        }
        $previousBuildId = Split-Path -Leaf $previousTarget
        if (-not $previousBuildId -or -not (Test-Path -LiteralPath (Join-Path $BuildRoot $previousBuildId))) {
            throw "Refusing to replace '$latestPath' because it does not target a build inside '$BuildRoot'."
        }
        [IO.Directory]::Move($latestPath, $oldLinkPath)
    }

    try {
        [IO.Directory]::Move($nextLinkPath, $latestPath)
    } catch {
        if ($hadPrevious -and -not (Test-Path -LiteralPath $latestPath) -and (Test-Path -LiteralPath $oldLinkPath)) {
            [IO.Directory]::Move($oldLinkPath, $latestPath)
        }
        throw
    }
    if (Test-Path -LiteralPath $oldLinkPath) { [IO.Directory]::Delete($oldLinkPath) }
} finally {
    if (Test-Path -LiteralPath $stagingPath) { Remove-Item -LiteralPath $stagingPath -Recurse -Force }
    if (Test-Path -LiteralPath $nextLinkPath) { [IO.Directory]::Delete($nextLinkPath) }
}

$runningPaths = @()
try {
    $runningPaths = @(Get-CimInstance Win32_Process -Filter "Name = 'meeting-recorder.exe' OR Name = 'sottovoce.exe'" -ErrorAction Stop |
        ForEach-Object { $_.ExecutablePath } | Where-Object { $_ })
} catch {
    $runningPaths = @(Get-Process -Name meeting-recorder,sottovoce -ErrorAction SilentlyContinue |
        ForEach-Object { try { $_.Path } catch { $null } } | Where-Object { $_ })
}
$runningPaths = @($runningPaths | ForEach-Object { [IO.Path]::GetFullPath($_) })
foreach ($executable in @('meeting-recorder.exe', 'sottovoce.exe')) {
    if ($previousBuildId -and $runningPaths -contains [IO.Path]::GetFullPath((Join-Path $latestPath $executable))) {
        $runningPaths += [IO.Path]::GetFullPath((Join-Path (Join-Path $BuildRoot $previousBuildId) $executable))
    }
}

$managedBuilds = @()
foreach ($directory in Get-ChildItem -LiteralPath $BuildRoot -Directory -Force) {
    if (($directory.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { continue }
    $infoPath = Join-Path $directory.FullName 'build.json'
    if (-not (Test-Path -LiteralPath $infoPath -PathType Leaf)) { continue }
    try { $info = Get-Content -LiteralPath $infoPath -Raw | ConvertFrom-Json } catch { continue }
    if ($info.application -eq 'Sottovoce' -and $info.version -and $info.commit -and $info.buildTimeUtc) {
        $managedBuilds += [pscustomobject]@{ Path = $directory.FullName; Info = $info }
    }
}
$orderedBuilds = @($managedBuilds | Sort-Object { [string]$_.Info.buildTimeUtc } -Descending)
$keepIds = @($orderedBuilds | Select-Object -First $KeepBuilds | ForEach-Object { Split-Path -Leaf $_.Path })
$keepIds += $buildId
foreach ($build in $orderedBuilds) {
    $id = Split-Path -Leaf $build.Path
    if ($keepIds -contains $id) { continue }
    $exePaths = @('meeting-recorder.exe', 'sottovoce.exe') | ForEach-Object { [IO.Path]::GetFullPath((Join-Path $build.Path $_)) }
    if (@($exePaths | Where-Object { $runningPaths -contains $_ }).Count -gt 0) {
        Write-Host "Keeping running build $id."
        continue
    }
    try {
        Remove-Item -LiteralPath $build.Path -Recurse -Force
        Write-Host "Removed old build $id."
    } catch {
        Write-Warning "Could not remove old build '$($build.Path)': $($_.Exception.Message)"
    }
}

Write-Host "Sottovoce $($package.version) is available at '$latestPath\sottovoce.exe'."
Write-Host "Build metadata: '$latestPath\build.json'."
