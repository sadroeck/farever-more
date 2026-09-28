[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Version,

    [ValidateSet("Debug", "Release")]
    [string]$Configuration = "Release",

    [string]$OutputRoot = "dist\release"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = [System.IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$version = $Version.Trim()
if ($version.StartsWith("v", [System.StringComparison]::OrdinalIgnoreCase)) {
    $version = $version.Substring(1)
}
if ($version -notmatch '^\d+\.\d+\.\d+$') {
    throw "Release version must be a semantic version, found: $Version"
}

$outputRoot = if ([System.IO.Path]::IsPathRooted($OutputRoot)) {
    [System.IO.Path]::GetFullPath($OutputRoot)
}
else {
    [System.IO.Path]::GetFullPath((Join-Path $repoRoot $OutputRoot))
}
if (Test-Path -LiteralPath $outputRoot) {
    throw "Release output already exists; choose a fresh -OutputRoot: $outputRoot"
}

$profile = $Configuration.ToLowerInvariant()
$nativeRoot = Join-Path $repoRoot "target\$profile"
$addonOutput = Join-Path $repoRoot "target\release-addon-dev"
$frameworkRoot = Join-Path $outputRoot "framework"
$addonsRoot = Join-Path $outputRoot "addons"

function Assert-File {
    param([Parameter(Mandatory)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Expected release file was not produced: $Path"
    }
}

function Get-CargoPackageVersion {
    param(
        [Parameter(Mandatory)][string]$ManifestPath,
        [Parameter(Mandatory)][string]$PackageName
    )

    $text = [System.IO.File]::ReadAllText($ManifestPath)
    $match = [regex]::Match(
        $text,
        '(?ms)^\[package\].*?^name\s*=\s*"' + [regex]::Escape($PackageName) + '".*?^version\s*=\s*"(?<version>[^"\r\n]+)"'
    )
    if (-not $match.Success) {
        throw "Could not read package version for $PackageName from $ManifestPath"
    }
    return $match.Groups["version"].Value
}

function Write-JsonFile {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)]$Value
    )

    [System.IO.File]::WriteAllText(
        $Path,
        ($Value | ConvertTo-Json -Depth 16),
        [System.Text.UTF8Encoding]::new($false)
    )
}

function New-Zip {
    param(
        [Parameter(Mandatory)][string]$SourceDirectory,
        [Parameter(Mandatory)][string]$Destination
    )

    Compress-Archive -Path (Join-Path $SourceDirectory "*") -DestinationPath $Destination -CompressionLevel Optimal
    Assert-File $Destination
}

function Get-ReleaseNotes {
    param(
        [Parameter(Mandatory)][string]$RepoRoot,
        [Parameter(Mandatory)][string]$Version
    )

    $changelog = [System.IO.File]::ReadAllText((Join-Path $RepoRoot "CHANGELOG.md"))
    $releaseSection = [regex]::Match(
        $changelog,
        '(?ms)^## ' + [regex]::Escape($Version) + '(?:[ \t][^\r\n]*)?\r?\n.*?(?=^## |\z)'
    )
    $readme = [System.IO.File]::ReadAllText((Join-Path $RepoRoot "README.md"))
    $installSection = [regex]::Match(
        $readme,
        '(?ms)^## How to install\r?\n.*?(?=^## |\z)'
    )
    if (-not $releaseSection.Success -or -not $installSection.Success) {
        throw "Release notes require CHANGELOG.md version $Version and README.md installation instructions"
    }
    return $releaseSection.Value.Trim() + "`n`n" + $installSection.Value.Trim() + "`n"
}

Push-Location $repoRoot
try {
    & (Join-Path $PSScriptRoot "check-addon-sdk-boundary.ps1")

    $runtimeVersion = Get-CargoPackageVersion `
        -ManifestPath (Join-Path $repoRoot "farever-more-host\Cargo.toml") `
        -PackageName "farever-more-host"
    if ($runtimeVersion -ne $version) {
        throw "Release tag $version does not match the framework runtime version $runtimeVersion"
    }

    $releaseNotes = Get-ReleaseNotes -RepoRoot $repoRoot -Version $version

    $cargo = Get-Command cargo -ErrorAction Stop
    & $cargo.Source build --release -p farever-more-proxy -p farever-more-host
    if ($LASTEXITCODE -ne 0) {
        throw "Framework release build failed with exit code $LASTEXITCODE"
    }

    & (Join-Path $PSScriptRoot "build-wasm-addons.ps1") `
        -Configuration $Configuration `
        -OutputRoot "target\release-addon-dev"
    if ($LASTEXITCODE -ne 0) {
        throw "Reference add-on release build failed with exit code $LASTEXITCODE"
    }

    New-Item -ItemType Directory -Force -Path $frameworkRoot, $addonsRoot | Out-Null
    $frameworkAddonRoot = Join-Path $frameworkRoot "farever-addons"
    New-Item -ItemType Directory -Force -Path $frameworkAddonRoot | Out-Null

    $nativeFiles = @(
        @{ Source = Join-Path $nativeRoot "farever_more_proxy.dll"; Destination = Join-Path $frameworkRoot "farever_more_proxy.dll" },
        @{ Source = Join-Path $nativeRoot "farever_more_host.dll"; Destination = Join-Path $frameworkRoot "farever_more_host.dll" },
        @{ Source = Join-Path $nativeRoot "farever_more_proxy.dll"; Destination = Join-Path $frameworkRoot "dinput8.dll" },
        @{ Source = Join-Path $nativeRoot "farever_more_host.dll"; Destination = Join-Path $frameworkAddonRoot "host.dll" }
    )
    foreach ($file in $nativeFiles) {
        Assert-File $file.Source
        Copy-Item -LiteralPath $file.Source -Destination $file.Destination
    }

    Write-JsonFile -Path (Join-Path $frameworkAddonRoot "runtime.json") -Value ([ordered]@{
        formatVersion = 1
        runtimeVersion = $runtimeVersion
    })
    Copy-Item -LiteralPath (Join-Path $repoRoot "README.md") -Destination (Join-Path $frameworkRoot "README.md")
    Copy-Item -LiteralPath (Join-Path $repoRoot "CHANGELOG.md") -Destination (Join-Path $frameworkRoot "CHANGELOG.md")
    Copy-Item -LiteralPath (Join-Path $repoRoot "LICENSE-MIT") -Destination (Join-Path $frameworkRoot "LICENSE-MIT")
    Copy-Item -LiteralPath (Join-Path $repoRoot "THIRD_PARTY_NOTICES.md") -Destination (Join-Path $frameworkRoot "THIRD_PARTY_NOTICES.md")
    [System.IO.File]::WriteAllText(
        (Join-Path $outputRoot "release-notes.md"),
        $releaseNotes,
        [System.Text.UTF8Encoding]::new($false)
    )

    $frameworkArchive = Join-Path $outputRoot "farever-more-framework-v$version-windows-x86_64.zip"
    New-Zip -SourceDirectory $frameworkRoot -Destination $frameworkArchive

    $addonRecords = [ordered]@{}
    foreach ($id in @("dyno", "gps", "minimap", "poi-database")) {
        $source = Join-Path $addonOutput "addons\$id"
        $manifestPath = Join-Path $source "addon.json"
        $wasmPath = Join-Path $source "addon.wasm"
        Assert-File $manifestPath
        Assert-File $wasmPath

        $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
        if ($manifest.id -ne $id) {
            throw "Packed manifest $manifestPath declares id '$($manifest.id)', expected '$id'"
        }
        if (-not $manifest.version) {
            throw "Packed manifest $manifestPath has no add-on version"
        }
        if (-not $manifest.sha256 -or -not $manifest.'api-version') {
            throw "Packed manifest $manifestPath is missing stamped hash or API version"
        }

        $destination = Join-Path $addonsRoot $id
        New-Item -ItemType Directory -Force -Path $destination | Out-Null
        Get-ChildItem -LiteralPath $source -Force | Copy-Item -Destination $destination -Recurse -Force
        $archive = Join-Path $outputRoot "farever-more-addon-$id-v$($manifest.version).zip"
        New-Zip -SourceDirectory $destination -Destination $archive
        $addonRecords[$id] = [ordered]@{
            version = [string]$manifest.version
            apiVersion = [string]$manifest.'api-version'
            archive = [System.IO.Path]::GetFileName($archive)
            sha256 = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }

    $releaseManifest = [ordered]@{
        framework = [ordered]@{
            version = $version
            archive = [System.IO.Path]::GetFileName($frameworkArchive)
            sha256 = (Get-FileHash -LiteralPath $frameworkArchive -Algorithm SHA256).Hash.ToLowerInvariant()
        }
        addons = $addonRecords
    }
    Write-JsonFile -Path (Join-Path $outputRoot "release-manifest.json") -Value $releaseManifest
    Write-Output "Release packages written to $outputRoot"
}
finally {
    Pop-Location
}
