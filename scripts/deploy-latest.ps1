[CmdletBinding()]
param(
    [string]$FromRelease,
    [string]$BuildRoot,
    [int]$KeepBuilds = 3,
    [switch]$SkipBuild,
    [string]$TargetDirectory = (Join-Path $env:TEMP 'sottovoce-deploy-target'),
    [ValidateSet('debug', 'release')][string]$Profile = 'release'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$manifestPath = Join-Path $repoRoot 'Cargo.toml'
$targetDirectory = [IO.Path]::GetFullPath($TargetDirectory)
$usingDefaultBuildRoot = -not $PSBoundParameters.ContainsKey('BuildRoot')
if ($usingDefaultBuildRoot) {
    $BuildRoot = Join-Path $env:LOCALAPPDATA 'Sottovoce\builds'
}
if ($KeepBuilds -lt 1) { throw 'KeepBuilds must be at least 1.' }
if ([string]::IsNullOrWhiteSpace($BuildRoot)) { throw 'BuildRoot cannot be empty.' }
$BuildRoot = [IO.Path]::GetFullPath([Environment]::ExpandEnvironmentVariables($BuildRoot))
[void][IO.Directory]::CreateDirectory($BuildRoot)

function Get-ReleaseCommit {
    param([Parameter(Mandatory)][string]$Tag)

    $escapedTag = [Uri]::EscapeDataString($Tag)
    $apiUri = "https://api.github.com/repos/colthreepv/sottovoce/commits/$escapedTag"
    $gh = Get-Command gh -ErrorAction SilentlyContinue
    if ($gh) {
        $sha = & $gh.Source api "repos/colthreepv/sottovoce/commits/$escapedTag" --jq .sha 2>$null
        if ($LASTEXITCODE -eq 0 -and ([string]$sha).Trim() -match '^[0-9a-fA-F]{40}$') {
            return ([string]$sha).Trim().ToLowerInvariant()
        }
    }
    $response = Invoke-RestMethod -Uri $apiUri -Headers @{ 'User-Agent' = 'Sottovoce-deploy' }
    $sha = [string]$response.sha
    if ($sha -notmatch '^[0-9a-fA-F]{40}$') { throw "GitHub did not return a commit SHA for tag '$Tag'." }
    return $sha.ToLowerInvariant()
}

function Get-ReleasePackage {
    param(
        [Parameter(Mandatory)][string]$Tag,
        [Parameter(Mandatory)][string]$Version,
        [Parameter(Mandatory)][string]$Destination
    )

    $zipName = "sottovoce-$Version-windows-x64.zip"
    $zipPath = Join-Path $Destination $zipName
    $checksumsPath = Join-Path $Destination 'SHA256SUMS'
    $downloaded = $false
    $gh = Get-Command gh -ErrorAction SilentlyContinue
    if ($gh) {
        & $gh.Source release download $Tag --repo colthreepv/sottovoce `
            --pattern $zipName --pattern SHA256SUMS --dir $Destination 2>$null
        $downloaded = $LASTEXITCODE -eq 0 -and
            (Test-Path -LiteralPath $zipPath -PathType Leaf) -and
            (Test-Path -LiteralPath $checksumsPath -PathType Leaf)
    }
    if (-not $downloaded) {
        $escapedTag = [Uri]::EscapeDataString($Tag)
        foreach ($assetName in @($zipName, 'SHA256SUMS')) {
            $escapedAsset = [Uri]::EscapeDataString($assetName)
            $uri = "https://github.com/colthreepv/sottovoce/releases/download/$escapedTag/$escapedAsset"
            Invoke-WebRequest -Uri $uri -OutFile (Join-Path $Destination $assetName) -UseBasicParsing
        }
    }

    if (-not (Test-Path -LiteralPath $zipPath -PathType Leaf) -or
        -not (Test-Path -LiteralPath $checksumsPath -PathType Leaf)) {
        throw "Release '$Tag' did not provide $zipName and SHA256SUMS."
    }
    $escapedZipName = [regex]::Escape($zipName)
    $sumMatch = [regex]::Match(
        (Get-Content -LiteralPath $checksumsPath -Raw),
        "(?m)^\s*([0-9a-fA-F]{64})\s+\*?$escapedZipName\s*$"
    )
    if (-not $sumMatch.Success) { throw "SHA256SUMS does not contain an entry for $zipName." }
    $expectedHash = $sumMatch.Groups[1].Value.ToLowerInvariant()
    $actualHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $expectedHash) {
        throw "SHA-256 mismatch for $zipName (expected $expectedHash, got $actualHash)."
    }
    return $zipPath
}

$isRelease = -not [string]::IsNullOrWhiteSpace($FromRelease)
$releaseDownloadDirectory = $null
$packageOutputDirectory = $null
$source = 'local'
$tag = $null
$dirty = $false
$rustcVersion = $null
$cargoVersion = $null

if ($isRelease) {
    $tagMatch = [regex]::Match($FromRelease, '^v(?<version>\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)$')
    if (-not $tagMatch.Success) {
        throw "Release tag must be a version tag such as v0.2.0; got '$FromRelease'."
    }
    $tag = $FromRelease
    $version = $tagMatch.Groups['version'].Value
    $source = 'github-release'
    $sourceCommit = Get-ReleaseCommit -Tag $tag
    $shortCommit = $sourceCommit.Substring(0, 8)
    $releaseDownloadDirectory = Join-Path ([IO.Path]::GetTempPath()) "sottovoce-release-$([Guid]::NewGuid().ToString('N'))"
    [void][IO.Directory]::CreateDirectory($releaseDownloadDirectory)
    try {
        $releaseZip = Get-ReleasePackage -Tag $tag -Version $version -Destination $releaseDownloadDirectory
    } catch {
        Remove-Item -LiteralPath $releaseDownloadDirectory -Recurse -Force -ErrorAction SilentlyContinue
        throw
    }
} else {
    $metadataOutput = & (Join-Path $PSScriptRoot 'cargo.ps1') -Role builder -CargoArgs @(
        'metadata', '--no-deps', '--format-version', '1', '--manifest-path', $manifestPath
    )
    $metadataOutput = @($metadataOutput | Where-Object { -not ([string]$_).StartsWith('[cargo.ps1]') })
    if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed with exit code $LASTEXITCODE." }
    $metadata = ($metadataOutput -join [Environment]::NewLine) | ConvertFrom-Json
    $package = $metadata.packages | Where-Object { $_.name -eq 'sottovoce-engine' } | Select-Object -First 1
    if (-not $package) { throw 'Cargo metadata did not contain sottovoce-engine.' }
    $version = [string]$package.version

    $sourceCommit = (& git -C $repoRoot rev-parse HEAD 2>$null | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0 -or -not $sourceCommit) { throw 'Could not identify the source commit.' }
    $sourceCommit = ([string]$sourceCommit).Trim()
    $sourceStatus = ((& git -C $repoRoot status --porcelain --untracked-files=all 2>$null) -join [Environment]::NewLine).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'Could not inspect the source working tree.' }
    $dirty = -not [string]::IsNullOrWhiteSpace($sourceStatus)
    $shortCommit = (& git -C $repoRoot rev-parse --short=8 HEAD 2>$null | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0 -or -not $shortCommit) { throw 'Could not identify the short source commit.' }
    $shortCommit = ([string]$shortCommit).Trim()

    if (-not $SkipBuild) {
        Write-Host "Building Sottovoce $version into '$targetDirectory'..."
        & bun run --cwd (Join-Path $repoRoot 'app/ui') build
        if ($LASTEXITCODE -ne 0) { throw 'Frontend build failed.' }
        $buildArgs = @('build', '--workspace', '--locked', '--manifest-path', $manifestPath, '--target-dir', $targetDirectory)
        if ($Profile -eq 'release') { $buildArgs += '--release' }
        & (Join-Path $PSScriptRoot 'cargo.ps1') -Role builder -CargoArgs $buildArgs
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
    $localFfmpeg = Join-Path $repoRoot 'packaging\dist\ffmpeg.exe'
    $packageOutputDirectory = Join-Path ([IO.Path]::GetTempPath()) "sottovoce-package-$([Guid]::NewGuid().ToString('N'))"
    [void][IO.Directory]::CreateDirectory($packageOutputDirectory)
    $packageArguments = @{
        ReleaseDirectory = $releaseDirectory
        OutputDirectory = $packageOutputDirectory
    }
    if (Test-Path -LiteralPath $localFfmpeg -PathType Leaf) {
        $packageArguments.FfmpegPath = $localFfmpeg
    } else {
        Write-Warning "Local FFmpeg is missing at '$localFfmpeg'; deploying without ffmpeg.exe."
    }
    try {
        & (Join-Path $repoRoot 'packaging\package.ps1') @packageArguments
        $releaseZip = Join-Path $packageOutputDirectory "sottovoce-$version-windows-x64.zip"
        if (-not (Test-Path -LiteralPath $releaseZip -PathType Leaf)) {
            throw "Packaging did not produce the expected archive '$releaseZip'."
        }
    } catch {
        Remove-Item -LiteralPath $packageOutputDirectory -Recurse -Force -ErrorAction SilentlyContinue
        throw
    }
}

$baseBuildId = "$version-$shortCommit$(if ($dirty) { '-dirty' })"
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
    Expand-Archive -LiteralPath $releaseZip -DestinationPath $stagingPath -Force
    if (-not (Test-Path -LiteralPath (Join-Path $stagingPath 'sottovoce.exe') -PathType Leaf)) {
        throw 'The portable package is missing sottovoce.exe.'
    }
    if ($isRelease) {
        $buildInfo = [ordered]@{
            application = 'Sottovoce'
            version = $version
            commit = $sourceCommit
            buildTimeUtc = [DateTime]::UtcNow.ToString('o')
            source = $source
            tag = $tag
        }
    } else {
        $rustcVersion = (& rustc --version --verbose | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) { throw 'Could not record the Rust compiler version.' }
        $cargoVersionOutput = & (Join-Path $PSScriptRoot 'cargo.ps1') -Role builder -CargoArgs @('--version')
        $cargoVersionOutput = @($cargoVersionOutput | Where-Object { -not ([string]$_).StartsWith('[cargo.ps1]') })
        if ($LASTEXITCODE -ne 0) { throw 'Could not record the Cargo version.' }
        $cargoVersion = ($cargoVersionOutput | Select-Object -Last 1)
        $buildInfo = [ordered]@{
            application = 'Sottovoce'
            version = $version
            commit = $sourceCommit
            dirty = $dirty
            buildTimeUtc = [DateTime]::UtcNow.ToString('o')
            rustcVersion = $rustcVersion
            cargoVersion = ([string]$cargoVersion).Trim()
            source = $source
            tag = $null
        }
        $buildInfo.profile = $Profile
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
        $previousTargetText = [string](@($latestItem.Target)[0])
        $previousTarget = [IO.Path]::GetFullPath($previousTargetText)
        $buildRootPrefix = $BuildRoot.TrimEnd('\') + '\'
        if ($previousTarget.Equals($BuildRoot, [StringComparison]::OrdinalIgnoreCase) -or
            -not $previousTarget.StartsWith($buildRootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to replace '$latestPath' because its junction target is outside '$BuildRoot'."
        }
        if (-not (Test-Path -LiteralPath $previousTarget -PathType Container) -or
            -not (Test-Path -LiteralPath (Join-Path $previousTarget 'sottovoce.exe') -PathType Leaf)) {
            throw "Refusing to replace '$latestPath' because its target is not a Sottovoce build."
        }
        $previousTargetItem = Get-Item -LiteralPath $previousTarget -Force
        if (($previousTargetItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Refusing to replace '$latestPath' because its target is itself a reparse point."
        }
        $previousInfoPath = Join-Path $previousTarget 'build.json'
        if (Test-Path -LiteralPath $previousInfoPath -PathType Leaf) {
            try { $previousInfo = Get-Content -LiteralPath $previousInfoPath -Raw | ConvertFrom-Json }
            catch { throw "Refusing to replace '$latestPath' because its build metadata is invalid." }
            if ($previousInfo.application -ne 'Sottovoce' -or -not $previousInfo.version -or -not $previousInfo.commit) {
                throw "Refusing to replace '$latestPath' because its build metadata is invalid."
            }
            Write-Host "Replacing managed build '$($previousInfo.version)' at '$previousTarget'."
        } else {
            Write-Warning "Replacing latest junction to unmanaged build '$previousTarget'; leaving that folder untouched."
        }
        $previousBuildId = Split-Path -Leaf $previousTarget
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
    if ($releaseDownloadDirectory -and (Test-Path -LiteralPath $releaseDownloadDirectory)) {
        Remove-Item -LiteralPath $releaseDownloadDirectory -Recurse -Force
    }
    if ($packageOutputDirectory -and (Test-Path -LiteralPath $packageOutputDirectory)) {
        Remove-Item -LiteralPath $packageOutputDirectory -Recurse -Force
    }
}

$runningPaths = @()
try {
    $runningPaths = @(Get-CimInstance Win32_Process -Filter "Name = 'sottovoce.exe'" -ErrorAction Stop |
        ForEach-Object { $_.ExecutablePath } | Where-Object { $_ })
} catch {
    $runningPaths = @(Get-Process -Name sottovoce -ErrorAction SilentlyContinue |
        ForEach-Object { try { $_.Path } catch { $null } } | Where-Object { $_ })
}
$runningPaths = @($runningPaths | ForEach-Object { [IO.Path]::GetFullPath($_) })
if ($previousBuildId -and $runningPaths -contains [IO.Path]::GetFullPath((Join-Path $latestPath 'sottovoce.exe'))) {
    $runningPaths += [IO.Path]::GetFullPath((Join-Path (Join-Path $BuildRoot $previousBuildId) 'sottovoce.exe'))
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
    $exePath = [IO.Path]::GetFullPath((Join-Path $build.Path 'sottovoce.exe'))
    if ($runningPaths -contains $exePath) {
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

# Real deployments only: keep a Start Menu entry pointing through the junction.
if ($usingDefaultBuildRoot) {
    $shortcutPath = Join-Path ([Environment]::GetFolderPath('Programs')) 'Sottovoce.lnk'
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($shortcutPath)
    if ($shortcut.TargetPath -ne (Join-Path $latestPath 'sottovoce.exe')) {
        $shortcut.TargetPath = Join-Path $latestPath 'sottovoce.exe'
        $shortcut.WorkingDirectory = $latestPath
        $shortcut.IconLocation = "$(Join-Path $latestPath 'sottovoce.exe'),0"
        $shortcut.Description = 'Sottovoce call recorder (latest build)'
        $shortcut.Save()
        Write-Host "Start Menu shortcut: '$shortcutPath'."
    }
}

Write-Host "Sottovoce $version is available at '$latestPath\sottovoce.exe'."
Write-Host "Build metadata: '$latestPath\build.json'."
