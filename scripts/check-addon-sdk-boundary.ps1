[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Get-WitApiVersion {
    # `wit/farever-addon.wit` owns the add-on API number; everything else is a
    # mirror of it.
    param([Parameter(Mandatory)][string]$RepoRoot)

    $witPath = Join-Path $RepoRoot "wit\farever-addon.wit"
    $match = [regex]::Match(
        [System.IO.File]::ReadAllText($witPath),
        '(?m)^package\s+farever:addon@(?<version>\d+\.\d+\.\d+);'
    )
    if (-not $match.Success) {
        throw "Could not read the add-on API version from $witPath"
    }
    return $match.Groups["version"].Value
}

$repoRoot = [System.IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$addonRoot = Join-Path $repoRoot "addons"

# The add-on API is one number. It is the WIT package, the versioned interface
# names a component imports, the SDK that mirrors them, and the constant the
# runtime and the manager enforce against. Every build checks all four are
# still the same value.
$apiVersion = (Get-WitApiVersion -RepoRoot $repoRoot)
$sdkSource = Join-Path $repoRoot "farever-more-sdk\src\lib.rs"
$sdkPackage = "farever:addon@$apiVersion"
if (-not [System.IO.File]::ReadAllText($sdkSource).Contains("`"$sdkPackage`"")) {
    throw "farever-more-sdk WIT_PACKAGE must equal $sdkPackage"
}
$sdkManifest = Join-Path $repoRoot "farever-more-sdk\Cargo.toml"
$sdkVersionMatch = [regex]::Match(
    [System.IO.File]::ReadAllText($sdkManifest),
    '(?ms)^\[package\].*?^version\s*=\s*"(?<version>[^"]+)"'
)
if (-not $sdkVersionMatch.Success -or $sdkVersionMatch.Groups['version'].Value -ne $apiVersion) {
    throw "farever-more-sdk package version must equal the add-on API version $apiVersion"
}
$manifestSource = Join-Path $repoRoot "farever-more-manifest\src\api.rs"
$manifestMatch = [regex]::Match(
    [System.IO.File]::ReadAllText($manifestSource),
    'ADDON_API_VERSION:\s*&str\s*=\s*"(?<version>[^"]+)"'
)
if (-not $manifestMatch.Success -or $manifestMatch.Groups['version'].Value -ne $apiVersion) {
    throw "farever-more-manifest ADDON_API_VERSION must equal the add-on API version $apiVersion"
}

$sourceFiles = Get-ChildItem -LiteralPath $addonRoot -Recurse -File |
    Where-Object { $_.Extension -eq ".rs" -or $_.Name -eq "Cargo.toml" }
$forbidden = @(
    'farever_more_sdk::bindings',
    'farever_more_sdk::__wit',
    'farever_more_sdk::raw',
    'bindings::farever::addon',
    'raw-wit'
)
$matches = $sourceFiles | Select-String -Pattern $forbidden
if ($matches) {
    $details = $matches | ForEach-Object {
        "$($_.Path):$($_.LineNumber): $($_.Line.Trim())"
    }
    throw "Maintained add-ons must use the supported SDK facade:`n$($details -join "`n")"
}

Write-Output "Maintained add-ons use only the supported SDK facade"
Write-Output "Add-on API $apiVersion matches the WIT package, the SDK and the manifest constant"
