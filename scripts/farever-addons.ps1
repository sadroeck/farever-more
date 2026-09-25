<#
.SYNOPSIS
Installs, updates, inspects, or uninstalls the Farever add-on framework.

.DESCRIPTION
Builds the optimized native framework and Wasm components, locates the Steam
Farever installation, installs the dinput8 proxy and host, and optionally
installs one or more add-on components.
Every installed file is recorded in farever-addons\install-manifest.json so an
uninstall removes only files owned by this utility.

.EXAMPLE
.\scripts\farever-addons.ps1 Install

.EXAMPLE
.\scripts\farever-addons.ps1 Update -AddonWasm C:\build\my-addon.wasm

.EXAMPLE
.\scripts\farever-addons.ps1 Status

.EXAMPLE
.\scripts\farever-addons.ps1 Logs -Follow

.EXAMPLE
.\scripts\farever-addons.ps1 Uninstall
#>
[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = "Medium")]
param(
    [Parameter(Position = 0)]
    [ValidateSet("Install", "Update", "Uninstall", "Status", "Logs")]
    [string]$Action = "Status",

    [string]$GameDirectory,

    [ValidateSet("Debug", "Release")]
    [string]$Configuration = "Release",

    [string[]]$AddonWasm = @(),

    [switch]$SkipReferenceAddon,

    [switch]$NoBuild,

    [switch]$Force,

    [ValidateRange(1, 10000)]
    [int]$Tail = 200,

    [switch]$Follow
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$script:AppId = "3672400"
$script:RepoRoot = [System.IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$script:RuntimeRelativePath = "farever-addons"
$script:ManifestRelativePath = "farever-addons\install-manifest.json"

function Get-FareverDirectoryFromSteam {
    $steamRoots = [System.Collections.Generic.List[string]]::new()

    try {
        $steamSettings = Get-ItemProperty -LiteralPath "HKCU:\Software\Valve\Steam" -ErrorAction Stop
        if ($steamSettings.SteamPath) {
            $steamRoots.Add([string]$steamSettings.SteamPath)
        }
    }
    catch {
        # Steam may be installed without this per-user registry value.
    }

    if (${env:ProgramFiles(x86)}) {
        $steamRoots.Add((Join-Path ${env:ProgramFiles(x86)} "Steam"))
    }
    if ($env:ProgramFiles) {
        $steamRoots.Add((Join-Path $env:ProgramFiles "Steam"))
    }

    $libraryRoots = [System.Collections.Generic.List[string]]::new()
    foreach ($steamRoot in @($steamRoots | Select-Object -Unique)) {
        if (-not (Test-Path -LiteralPath $steamRoot -PathType Container)) {
            continue
        }

        $libraryRoots.Add($steamRoot)
        $libraryFile = Join-Path $steamRoot "steamapps\libraryfolders.vdf"
        if (-not (Test-Path -LiteralPath $libraryFile -PathType Leaf)) {
            continue
        }

        $libraryText = Get-Content -LiteralPath $libraryFile -Raw
        foreach ($match in [regex]::Matches($libraryText, '"path"\s+"([^"]+)"')) {
            $libraryRoot = $match.Groups[1].Value.Replace("\\", "\")
            if ($libraryRoot) {
                $libraryRoots.Add($libraryRoot)
            }
        }
    }

    foreach ($libraryRoot in @($libraryRoots | Select-Object -Unique)) {
        $appManifest = Join-Path $libraryRoot "steamapps\appmanifest_$($script:AppId).acf"
        if (-not (Test-Path -LiteralPath $appManifest -PathType Leaf)) {
            continue
        }

        $manifestText = Get-Content -LiteralPath $appManifest -Raw
        $installMatch = [regex]::Match($manifestText, '"installdir"\s+"([^"]+)"')
        if (-not $installMatch.Success) {
            continue
        }

        $candidate = Join-Path $libraryRoot ("steamapps\common\" + $installMatch.Groups[1].Value)
        if (Test-Path -LiteralPath (Join-Path $candidate "Farever.exe") -PathType Leaf) {
            return [System.IO.Path]::GetFullPath($candidate)
        }
    }

    return $null
}

function Resolve-FareverDirectory {
    param([string]$ConfiguredDirectory)

    if ($ConfiguredDirectory) {
        if (-not (Test-Path -LiteralPath $ConfiguredDirectory -PathType Container)) {
            throw "Farever game directory does not exist: $ConfiguredDirectory"
        }
        $resolvedDirectory = [System.IO.Path]::GetFullPath(
            (Resolve-Path -LiteralPath $ConfiguredDirectory).Path
        )
    }
    else {
        $resolvedDirectory = Get-FareverDirectoryFromSteam
        if (-not $resolvedDirectory) {
            throw "Could not locate Steam app $($script:AppId). Pass -GameDirectory explicitly."
        }
    }

    $gameExecutable = Join-Path $resolvedDirectory "Farever.exe"
    if (-not (Test-Path -LiteralPath $gameExecutable -PathType Leaf)) {
        throw "Farever.exe was not found in: $resolvedDirectory"
    }
    return $resolvedDirectory.TrimEnd([char[]]@('\', '/'))
}

function Resolve-PathInsideGame {
    param(
        [string]$GameRoot,
        [string]$RelativePath
    )

    if ([System.IO.Path]::IsPathRooted($RelativePath)) {
        throw "Install manifest contains an absolute path: $RelativePath"
    }

    $root = [System.IO.Path]::GetFullPath($GameRoot).TrimEnd([char[]]@('\', '/'))
    $candidate = [System.IO.Path]::GetFullPath((Join-Path $root $RelativePath))
    $rootPrefix = $root + [System.IO.Path]::DirectorySeparatorChar
    if (-not $candidate.StartsWith($rootPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Install path escapes the Farever directory: $RelativePath"
    }
    return $candidate
}

function Read-InstallManifest {
    param([string]$GameRoot)

    $manifestPath = Resolve-PathInsideGame $GameRoot $script:ManifestRelativePath
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
        return $null
    }

    try {
        $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    }
    catch {
        throw "The Farever add-on install manifest is invalid: $manifestPath"
    }

    if ($manifest.formatVersion -ne 1 -or $manifest.frameworkId -ne "farever-addon-poc") {
        throw "Unsupported Farever add-on install manifest: $manifestPath"
    }
    return $manifest
}

function Get-OwnedFileMap {
    param($Manifest)

    $ownedFiles = @{}
    if ($null -eq $Manifest) {
        return $ownedFiles
    }
    foreach ($file in @($Manifest.files)) {
        $ownedFiles[[string]$file.path] = $file
    }
    return $ownedFiles
}

function Get-FileSha256 {
    param([string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
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
    # pack step stamps both in, so every installed unit pins the exact
    # component that ships beside it and the API it needs.
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
}

function Get-RuntimeVersion {
    # The runtime (host + proxy) versions independently from the workspace and
    # the manager. Installers stamp this version into
    # farever-addons\runtime.json so the manager can display it.
    $cargoManifest = Join-Path $script:RepoRoot "farever-more-host\Cargo.toml"
    $match = Select-String -LiteralPath $cargoManifest -Pattern '^version = "([^"]+)"' |
        Select-Object -First 1
    if ($null -eq $match) {
        throw "Could not determine the runtime version from: $cargoManifest"
    }
    return $match.Matches[0].Groups[1].Value
}

function Assert-FareverStopped {
    if (Get-Process -Name "Farever" -ErrorAction SilentlyContinue) {
        throw "Farever is running. Close the game before installing, updating, or uninstalling framework files."
    }
}

function Invoke-FrameworkBuild {
    param(
        [string]$BuildConfiguration
    )

    $cargo = Get-Command cargo -ErrorAction Stop
    $arguments = [System.Collections.Generic.List[string]]::new()
    $arguments.Add("build")
    if ($BuildConfiguration -eq "Release") {
        $arguments.Add("--release")
    }
    foreach ($package in @("farever-more-proxy", "farever-more-host")) {
        $arguments.Add("-p")
        $arguments.Add($package)
    }

    Push-Location $script:RepoRoot
    try {
        & $cargo.Source @arguments
        if ($LASTEXITCODE -ne 0) {
            throw "The Rust build failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}

function Invoke-ReferenceAddonBuild {
    param([string]$BuildConfiguration)

    $cargo = Get-Command cargo -ErrorAction Stop
    $addonManifest = Join-Path $script:RepoRoot "addons\Cargo.toml"
    $arguments = [System.Collections.Generic.List[string]]::new()
    $arguments.Add("component")
    $arguments.Add("build")
    if ($BuildConfiguration -eq "Release") {
        $arguments.Add("--release")
    }
    $arguments.Add("--target")
    $arguments.Add("wasm32-unknown-unknown")
    $arguments.Add("--manifest-path")
    $arguments.Add($addonManifest)
    $arguments.Add("--workspace")
    $arguments.Add("--target-dir")
    $arguments.Add((Join-Path $script:RepoRoot "target\component-build"))

    & $cargo.Source @arguments
    if ($LASTEXITCODE -ne 0) {
        throw "The reference Wasm component build failed with exit code $LASTEXITCODE"
    }
    & $cargo.Source fmt --manifest-path $addonManifest --all
    if ($LASTEXITCODE -ne 0) {
        throw "Formatting generated reference add-on bindings failed with exit code $LASTEXITCODE"
    }
}

function New-InstallArtifacts {
    param(
        [string]$BuildConfiguration,
        [bool]$IncludeReferenceAddon,
        [string[]]$AdditionalAddons
    )

    $profile = $BuildConfiguration.ToLowerInvariant()
    $buildDirectory = Join-Path $script:RepoRoot "target\$profile"
    $artifacts = [System.Collections.Generic.List[object]]::new()
    $artifacts.Add([pscustomobject]@{
        id = "framework-proxy"
        source = Join-Path $buildDirectory "farever_more_proxy.dll"
        path = "dinput8.dll"
    })
    $artifacts.Add([pscustomobject]@{
        id = "framework-host"
        source = Join-Path $buildDirectory "farever_more_host.dll"
        path = "farever-addons\host.dll"
    })

    if ($IncludeReferenceAddon) {
        $componentRoot = Join-Path $script:RepoRoot "target\component-build\wasm32-unknown-unknown\$profile"
        # Each shipped add-on installs as `<id>/addon.wasm` plus the manifest
        # that names it, stamped with the component's fingerprint.
        foreach ($unit in @("dyno", "gps", "minimap", "poi-database")) {
            $component = Join-Path $componentRoot "$($unit.Replace('-', '_')).wasm"
            $artifacts.Add([pscustomobject]@{
                id = $unit
                source = $component
                path = "farever-addons\addons\$unit\addon.wasm"
            })
            $artifacts.Add([pscustomobject]@{
                id = "$unit-manifest"
                source = Join-Path $script:RepoRoot "addons\$unit\addon.json"
                path = "farever-addons\addons\$unit\addon.json"
                packComponent = $component
            })
        }
        $artifacts.Add([pscustomobject]@{
            id = "dyno-font-license"
            source = Join-Path $script:RepoRoot "addons\dyno\assets\fonts\OFL-1.1.txt"
            path = "farever-addons\addons\dyno\LICENSES\Noto-Sans-OFL-1.1.txt"
        })
    }

    foreach ($addonPath in $AdditionalAddons) {
        if (-not (Test-Path -LiteralPath $addonPath -PathType Leaf)) {
            throw "Add-on component does not exist: $addonPath"
        }
        $resolvedAddon = [System.IO.Path]::GetFullPath((Resolve-Path -LiteralPath $addonPath).Path)
        if ([System.IO.Path]::GetExtension($resolvedAddon) -ne ".wasm") {
            throw "Add-on must be a Wasm component: $resolvedAddon"
        }
        $fileName = [System.IO.Path]::GetFileName($resolvedAddon)
        $addonName = [System.IO.Path]::GetFileNameWithoutExtension($resolvedAddon)
        $artifacts.Add([pscustomobject]@{
            id = $addonName
            source = $resolvedAddon
            path = "farever-addons\addons\$addonName\addon.wasm"
        })
        Write-Verbose "Add-on $fileName installs as $addonName\addon.wasm"
    }

    $duplicate = $artifacts | Group-Object path | Where-Object Count -gt 1 | Select-Object -First 1
    if ($duplicate) {
        throw "More than one artifact targets $($duplicate.Name)"
    }
    foreach ($artifact in $artifacts) {
        if (-not (Test-Path -LiteralPath $artifact.source -PathType Leaf)) {
            throw "Build artifact does not exist: $($artifact.source)"
        }
    }
    return @($artifacts)
}

function Show-InstallStatus {
    param(
        [string]$GameRoot,
        $Manifest
    )

    Write-Output "Farever directory: $GameRoot"
    $running = $null -ne (Get-Process -Name "Farever" -ErrorAction SilentlyContinue)
    Write-Output "Game running:      $running"

    if ($null -eq $Manifest) {
        Write-Output "Managed install:   not installed"
        foreach ($relativePath in @("dinput8.dll", "farever-addons\host.dll")) {
            $path = Resolve-PathInsideGame $GameRoot $relativePath
            if (Test-Path -LiteralPath $path -PathType Leaf) {
                Write-Warning "Unmanaged file present: $relativePath"
            }
        }
        return
    }

    Write-Output "Managed install:   installed"
    Write-Output "Configuration:     $($Manifest.configuration)"
    foreach ($file in @($Manifest.files)) {
        $path = Resolve-PathInsideGame $GameRoot ([string]$file.path)
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            $state = "MISSING"
        }
        elseif ((Get-FileSha256 $path) -ne [string]$file.sha256) {
            $state = "MODIFIED"
        }
        else {
            $state = "OK"
        }
        Write-Output ("{0,-8} {1}" -f $state, [string]$file.path)
    }
}

function Show-DiagnosticLog {
    param(
        [string]$GameRoot,
        [int]$LineCount,
        [bool]$WaitForChanges
    )

    $logPath = Resolve-PathInsideGame $GameRoot "farever-addons\logs\host.log"
    Write-Output "Diagnostics log: $logPath"
    if (-not (Test-Path -LiteralPath $logPath -PathType Leaf)) {
        Write-Output "The host has not created a diagnostic log yet."
        return
    }
    Get-Content -LiteralPath $logPath -Tail $LineCount -Wait:$WaitForChanges
}

function Install-Framework {
    param(
        [string]$GameRoot,
        $ExistingManifest,
        [object[]]$Artifacts,
        [string]$BuildConfiguration,
        [string]$Operation
    )

    if ($Operation -eq "Install" -and $null -ne $ExistingManifest) {
        throw "A managed install already exists. Use the Update action."
    }
    if ($Operation -eq "Update" -and $null -eq $ExistingManifest) {
        throw "No managed install exists. Use the Install action."
    }

    $ownedFiles = Get-OwnedFileMap $ExistingManifest
    $retiredNativeAddons = @()
    if ($null -ne $ExistingManifest) {
        $retiredNativeAddons = @($ExistingManifest.files | Where-Object {
            ([string]$_.path).StartsWith(
                "farever-addons\addons\",
                [System.StringComparison]::OrdinalIgnoreCase
            ) -and ([System.IO.Path]::GetExtension([string]$_.path) -eq ".dll")
        })
        foreach ($file in $retiredNativeAddons) {
            $path = Resolve-PathInsideGame $GameRoot ([string]$file.path)
            if ((Test-Path -LiteralPath $path -PathType Leaf) -and
                (Get-FileSha256 $path) -ne [string]$file.sha256 -and
                -not $Force) {
                throw "Legacy managed native add-on was modified: $path. Review it, then rerun with -Force to retire it."
            }
        }
    }
    foreach ($artifact in $Artifacts) {
        $destination = Resolve-PathInsideGame $GameRoot ([string]$artifact.path)
        if (-not (Test-Path -LiteralPath $destination -PathType Leaf)) {
            continue
        }
        if (-not $ownedFiles.ContainsKey([string]$artifact.path)) {
            throw "Refusing to overwrite unmanaged file: $destination"
        }

        $expectedHash = [string]$ownedFiles[[string]$artifact.path].sha256
        $currentHash = Get-FileSha256 $destination
        if ($currentHash -ne $expectedHash -and -not $Force) {
            throw "Managed file was modified: $destination. Review it, then rerun with -Force to replace it."
        }
    }

    Assert-FareverStopped
    if (-not $PSCmdlet.ShouldProcess($GameRoot, "$Operation Farever add-on framework and add-ons")) {
        return
    }

    $fileRecords = @{}
    if ($null -ne $ExistingManifest) {
        foreach ($file in @($ExistingManifest.files)) {
            $fileRecords[[string]$file.path] = $file
        }
    }

    foreach ($file in $retiredNativeAddons) {
        $relativePath = [string]$file.path
        $path = Resolve-PathInsideGame $GameRoot $relativePath
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            Remove-Item -LiteralPath $path -Force
            Write-Output "Retired legacy native add-on: $relativePath"
        }
        $fileRecords.Remove($relativePath)
    }

    foreach ($artifact in $Artifacts) {
        $destination = Resolve-PathInsideGame $GameRoot ([string]$artifact.path)
        $destinationDirectory = Split-Path -Parent $destination
        New-Item -ItemType Directory -Force -Path $destinationDirectory | Out-Null
        $packComponent = $artifact.PSObject.Properties["packComponent"]
        if ($null -ne $packComponent) {
            Write-PackedManifest `
                -SourceManifest $artifact.source `
                -DestinationManifest $destination `
                -Component $packComponent.Value
        }
        else {
            Copy-Item -LiteralPath $artifact.source -Destination $destination -Force
        }
        $installedFile = Get-Item -LiteralPath $destination
        $fileRecords[[string]$artifact.path] = [ordered]@{
            id = [string]$artifact.id
            path = [string]$artifact.path
            sha256 = Get-FileSha256 $destination
            size = $installedFile.Length
        }
        Write-Output "$Operation`: $($artifact.path)"
    }

    $runtimeVersion = Get-RuntimeVersion
    $sidecarPath = Resolve-PathInsideGame $GameRoot "farever-addons\runtime.json"
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $sidecarPath) | Out-Null
    $sidecar = [ordered]@{
        formatVersion = 1
        runtimeVersion = $runtimeVersion
    }
    [System.IO.File]::WriteAllText(
        $sidecarPath,
        ($sidecar | ConvertTo-Json -Compress),
        [System.Text.UTF8Encoding]::new($false)
    )
    Write-Output "Stamped runtime version: $runtimeVersion"

    $now = [DateTime]::UtcNow.ToString("o")
    $installedAt = $now
    if ($null -ne $ExistingManifest -and $ExistingManifest.installedAt) {
        $installedAt = [string]$ExistingManifest.installedAt
    }
    $newManifest = [ordered]@{
        formatVersion = 1
        frameworkId = "farever-addon-poc"
        configuration = $BuildConfiguration
        installedAt = $installedAt
        updatedAt = $now
        files = @($fileRecords.Values | Sort-Object path)
    }

    $manifestPath = Resolve-PathInsideGame $GameRoot $script:ManifestRelativePath
    $manifestDirectory = Split-Path -Parent $manifestPath
    New-Item -ItemType Directory -Force -Path $manifestDirectory | Out-Null
    $temporaryManifest = "$manifestPath.tmp-$PID"
    try {
        $json = $newManifest | ConvertTo-Json -Depth 6
        [System.IO.File]::WriteAllText(
            $temporaryManifest,
            $json,
            [System.Text.UTF8Encoding]::new($false)
        )
        Move-Item -LiteralPath $temporaryManifest -Destination $manifestPath -Force
    }
    finally {
        if (Test-Path -LiteralPath $temporaryManifest) {
            Remove-Item -LiteralPath $temporaryManifest -Force
        }
    }

    Write-Output "Managed install manifest: $manifestPath"
}

function Uninstall-Framework {
    param(
        [string]$GameRoot,
        $Manifest
    )

    if ($null -eq $Manifest) {
        throw "No managed Farever add-on install was found. Unmanaged files will not be removed."
    }

    foreach ($file in @($Manifest.files)) {
        $path = Resolve-PathInsideGame $GameRoot ([string]$file.path)
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            continue
        }
        if ((Get-FileSha256 $path) -ne [string]$file.sha256 -and -not $Force) {
            throw "Managed file was modified: $path. Review it, then rerun with -Force to remove it."
        }
    }

    Assert-FareverStopped
    if (-not $PSCmdlet.ShouldProcess($GameRoot, "Uninstall managed Farever add-on framework and add-ons")) {
        return
    }

    foreach ($file in @($Manifest.files)) {
        $path = Resolve-PathInsideGame $GameRoot ([string]$file.path)
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            Remove-Item -LiteralPath $path -Force
            Write-Output "Removed: $($file.path)"
        }
    }

    $manifestPath = Resolve-PathInsideGame $GameRoot $script:ManifestRelativePath
    if (Test-Path -LiteralPath $manifestPath -PathType Leaf) {
        Remove-Item -LiteralPath $manifestPath -Force
    }

    $runtimeDirectory = Resolve-PathInsideGame $GameRoot $script:RuntimeRelativePath
    if (Test-Path -LiteralPath $runtimeDirectory -PathType Container) {
        $directories = Get-ChildItem -LiteralPath $runtimeDirectory -Directory -Recurse |
            Sort-Object { $_.FullName.Length } -Descending
        foreach ($directory in $directories) {
            if (-not (Get-ChildItem -LiteralPath $directory.FullName -Force | Select-Object -First 1)) {
                Remove-Item -LiteralPath $directory.FullName -Force
            }
        }
        if (-not (Get-ChildItem -LiteralPath $runtimeDirectory -Force | Select-Object -First 1)) {
            Remove-Item -LiteralPath $runtimeDirectory -Force
        }
    }
    Write-Output "Managed Farever add-on installation removed."
}

$resolvedGameDirectory = Resolve-FareverDirectory $GameDirectory
$installManifest = Read-InstallManifest $resolvedGameDirectory

switch ($Action) {
    "Status" {
        Show-InstallStatus $resolvedGameDirectory $installManifest
    }
    "Logs" {
        Show-DiagnosticLog $resolvedGameDirectory $Tail $Follow
    }
    "Uninstall" {
        Uninstall-Framework $resolvedGameDirectory $installManifest
    }
    { $_ -in @("Install", "Update") } {
        $includeReference = -not $SkipReferenceAddon
        if (-not $NoBuild) {
            # The host enforces the add-on API version the components were built
            # against, so the four copies of that number must agree before
            # anything is built or stamped.
            & (Join-Path $PSScriptRoot "check-addon-sdk-boundary.ps1")
            Invoke-FrameworkBuild $Configuration
            if ($includeReference) {
                Invoke-ReferenceAddonBuild $Configuration
            }
        }
        $installArtifacts = New-InstallArtifacts $Configuration $includeReference $AddonWasm
        Install-Framework $resolvedGameDirectory $installManifest $installArtifacts $Configuration $Action
    }
}
