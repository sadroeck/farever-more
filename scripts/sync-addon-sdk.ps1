[CmdletBinding()]
param(
    [switch]$Check
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = [System.IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$witPath = Join-Path $repoRoot "wit\farever-addon.wit"
$sdkRoot = Join-Path $repoRoot "farever-more-sdk"
$sdkManifest = Join-Path $sdkRoot "Cargo.toml"
$sdkSource = Join-Path $sdkRoot "src\lib.rs"
$bindingsPath = Join-Path $sdkRoot "src\bindings.rs"

$witMatch = [regex]::Match(
    [System.IO.File]::ReadAllText($witPath),
    '(?m)^package\s+(?<package>[^;]+);'
)
if (-not $witMatch.Success) {
    throw "Could not read the WIT package from $witPath"
}
$witPackage = $witMatch.Groups["package"].Value
$versionMatch = [regex]::Match($witPackage, '@(?<version>\d+\.\d+\.\d+)$')
if (-not $versionMatch.Success) {
    throw "The WIT package does not end in a semantic version: $witPackage"
}
$witVersion = $versionMatch.Groups["version"].Value

$manifestMatch = [regex]::Match(
    [System.IO.File]::ReadAllText($sdkManifest),
    '(?ms)^\[package\].*?^version\s*=\s*"(?<version>[^"]+)"'
)
if (-not $manifestMatch.Success -or $manifestMatch.Groups["version"].Value -ne $witVersion) {
    throw "SDK package version must match WIT $witVersion"
}
if (-not [System.IO.File]::ReadAllText($sdkSource).Contains("`"$witPackage`"")) {
    throw "SDK WIT_PACKAGE must equal $witPackage"
}

$cargo = Get-Command cargo -ErrorAction Stop
$componentVersion = & $cargo.Source component --version
if ($LASTEXITCODE -ne 0 -or $componentVersion -notmatch '0\.21\.1') {
    throw "SDK regeneration requires cargo-component 0.21.1; found: $componentVersion"
}

$beforeBytes = if (Test-Path -LiteralPath $bindingsPath) {
    [System.IO.File]::ReadAllBytes($bindingsPath)
}
else {
    $null
}
$before = if ($null -ne $beforeBytes) {
    [regex]::Replace([System.IO.File]::ReadAllText($bindingsPath), '\r\n?', [string][char]10)
}
else {
    $null
}

Push-Location $sdkRoot
try {
    & $cargo.Source component bindings
    if ($LASTEXITCODE -ne 0) {
        throw "cargo component bindings failed with exit code $LASTEXITCODE"
    }
    & $cargo.Source fmt --manifest-path $sdkManifest --package farever-more-sdk
    if ($LASTEXITCODE -ne 0) {
        throw "cargo fmt failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
}

$after = [regex]::Replace([System.IO.File]::ReadAllText($bindingsPath), '\r\n?', [string][char]10)
if ($Check) {
    if (-not [string]::Equals($before, $after, [System.StringComparison]::Ordinal)) {
        throw "farever-more-sdk/src/bindings.rs was stale and has been regenerated"
    }
    if ($null -ne $beforeBytes) {
        [System.IO.File]::WriteAllBytes($bindingsPath, $beforeBytes)
    }
}

Write-Output "SDK bindings match $witPackage using cargo-component 0.21.1"
