# Regenerates the extracted inventory and its POI/soulstone projections.
# Also verifies reviewed soulstone roots against the installed map archive.
#
#   .\scripts\generate-game-data.ps1          # write projections and check sites
#   .\scripts\generate-game-data.ps1 -Check   # fail on stale artifacts/sites
#
# The extractor reads the installed game; -Check still does, because comparing
# the artifacts against the installation is the point of the check. It runs with
# --no-default-features so that it builds even when a table no longer matches
# the types it is generated against - rewriting them is its whole job, and it
# could not do that from a package they prevent from compiling. The projection
# runs afterwards, with the tables, once they are current.
#
# The placement table is not regenerated here: it is imported from a released
# map census, which this repository does not vendor:
#
#   cargo run -p farever-db --bin placements -- import <census.json> farever-db\assets
#
# Run that import when a release changes the placements; everything downstream
# is projected from the table it writes.

[CmdletBinding()]
param(
    [switch]$Check
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = [System.IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$assetsDirectory = Join-Path $repoRoot "farever-db\assets"
$tablePath = Join-Path $repoRoot "addons\poi-database\assets\pois_w1_generated.rs"

# Typed as an array on purpose: PowerShell unwraps a one-element array on
# assignment, and a bare string splats as something other than one argument.
[string[]]$mode = @()
if ($Check) { $mode = @("--check") }
$cargo = (Get-Command cargo -ErrorAction Stop).Source

Push-Location $repoRoot
try {
    & $cargo run -q -p farever-db --no-default-features --bin extract-db -- $assetsDirectory @mode
    if ($LASTEXITCODE -ne 0) {
        throw "extract-db failed with exit code $LASTEXITCODE"
    }

    & $cargo run -q -p farever-db --bin placements -- project $tablePath @mode
    if ($LASTEXITCODE -ne 0) {
        throw "placements project failed with exit code $LASTEXITCODE"
    }
    & $cargo run -q -p farever-db --bin soulstones -- (Join-Path $repoRoot 'addons\gps\assets\soulstones_generated.rs') @mode
    if ($LASTEXITCODE -ne 0) { throw "soulstones projection failed with exit code $LASTEXITCODE" }
    & $cargo run -q -p farever-db --bin soulstones -- --verify-map
    if ($LASTEXITCODE -ne 0) { throw "soulstone map verification failed with exit code $LASTEXITCODE" }
    & $cargo run -q -p farever-db --features portrait-import --bin soulstones -- --icons (Join-Path $repoRoot 'addons\minimap\assets\soulstones') @mode
    if ($LASTEXITCODE -ne 0) { throw "soulstone portrait import failed with exit code $LASTEXITCODE" }
}
finally {
    Pop-Location
}
