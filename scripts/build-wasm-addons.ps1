[CmdletBinding()]
param(
    [ValidateSet("Debug", "Release")]
    [string]$Configuration = "Release",

    [string]$OutputRoot = "target\addon-dev"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = [System.IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$outputRoot = if ([System.IO.Path]::IsPathRooted($OutputRoot)) {
    [System.IO.Path]::GetFullPath($OutputRoot)
}
else {
    [System.IO.Path]::GetFullPath((Join-Path $repoRoot $OutputRoot))
}
$targetRoot = Join-Path $repoRoot "target\component-build"
$addonWorkspace = Join-Path $repoRoot "addons\Cargo.toml"
$hostWorkspace = Join-Path $repoRoot "Cargo.toml"
$profile = $Configuration.ToLowerInvariant()
$componentRoot = Join-Path $targetRoot "wasm32-unknown-unknown\$profile"
$licenseSource = Join-Path $repoRoot "addons\dyno\assets\fonts\OFL-1.1.txt"
$licenseDestination = Join-Path $outputRoot "addons\dyno\LICENSES\Noto-Sans-OFL-1.1.txt"

# Every unit is one component (`addon.wasm`) plus its manifest. The built
# artifact is named after the crate, so a dashed add-on id maps to an
# underscored file name.
$units = foreach ($id in @("dyno", "gps", "minimap", "map-waypoints", "poi-database")) {
    [pscustomobject]@{
        Id = $id
        Source = Join-Path $componentRoot "$($id.Replace('-', '_')).wasm"
        Component = Join-Path $outputRoot "addons\$id\addon.wasm"
        ManifestSource = Join-Path $repoRoot "addons\$id\addon.json"
        ManifestDestination = Join-Path $outputRoot "addons\$id\addon.json"
    }
}

function Get-ComponentApiVersion {
    # The add-on API a component was built against is written into its own
    # imports (`farever:addon/dependencies@1.0.0`), so the stamped manifest can
    # never disagree with the component it ships beside. A component that does
    # not name the API at all is not an add-on component.
    param([Parameter(Mandatory)][string]$Component)

    $bytes = [System.IO.File]::ReadAllBytes($Component)
    $text = [System.Text.Encoding]::GetEncoding('iso-8859-1').GetString($bytes)
    $versions = @(
        [regex]::Matches(
            $text,
            'farever:addon/[a-z_]+@(\d+\.\d+\.\d+)'
        ) | ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique
    )
    if ($versions.Count -ne 1) {
        throw "Could not determine one add-on API version from: $Component ($($versions -join ', '))"
    }
    return $versions[0]
}

function Write-PackedManifest {
    # Source manifests carry no fingerprint or API version: neither the hash of
    # a future build nor the SDK it will be built with is known upfront. The
    # pack step stamps both in, so every packed unit pins the exact component
    # that ships beside it and the API it needs.
    param(
        [Parameter(Mandatory)][string]$SourceManifest,
        [Parameter(Mandatory)][string]$DestinationManifest,
        [Parameter(Mandatory)][string]$Component
    )

    $manifest = Get-Content -LiteralPath $SourceManifest -Raw | ConvertFrom-Json
    $sha256 = (Get-FileHash -LiteralPath $Component -Algorithm SHA256).Hash.ToLowerInvariant()
    $apiVersion = Get-ComponentApiVersion -Component $Component
    $manifest | Add-Member -NotePropertyName sha256 -NotePropertyValue $sha256 -Force
    $manifest | Add-Member -NotePropertyName 'api-version' -NotePropertyValue $apiVersion -Force
    [System.IO.File]::WriteAllText(
        $DestinationManifest,
        ($manifest | ConvertTo-Json -Depth 16),
        [System.Text.UTF8Encoding]::new($false)
    )
    Write-Output "Packed manifest: $DestinationManifest sha256=$sha256 api-version=$apiVersion"
}

& (Join-Path $PSScriptRoot "check-addon-sdk-boundary.ps1")

$cargo = Get-Command cargo -ErrorAction Stop
$arguments = @(
    "build",
    "--manifest-path", $addonWorkspace,
    "--workspace",
    "--target", "wasm32-unknown-unknown",
    "--target-dir", $targetRoot
)
if ($Configuration -eq "Release") {
    $arguments = @("build", "--release") + $arguments[1..($arguments.Length - 1)]
}

& $cargo.Source @arguments
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}

$componentArguments = @(
    "run",
    "--manifest-path", $hostWorkspace,
    "--package", "farever-more-build",
    "--"
)
foreach ($unit in $units) {
    $componentArguments += @($unit.Source, $unit.Component)
}
& $cargo.Source @componentArguments
if ($LASTEXITCODE -ne 0) {
    throw "Farever component encoding failed with exit code $LASTEXITCODE"
}

New-Item -ItemType Directory -Force -Path (Split-Path -Parent $licenseDestination) | Out-Null
Copy-Item -LiteralPath $licenseSource -Destination $licenseDestination -Force
Write-Output "Font license: $licenseDestination"
foreach ($unit in $units) {
    Write-PackedManifest `
        -SourceManifest $unit.ManifestSource `
        -DestinationManifest $unit.ManifestDestination `
        -Component $unit.Component
}
