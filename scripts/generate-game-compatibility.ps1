# Project a reviewed --all inspector snapshot into the host's required contract.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Snapshot,
    [string]$Output = 'farever-more-runtime/assets/game-compatibility-25632706.json',
    [switch]$Check
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$baseline = Get-Content -LiteralPath $Snapshot -Raw | ConvertFrom-Json -AsHashtable
if ($baseline.schema_version -ne 1 -or $baseline.scope.selection.mode -ne 'all') {
    throw 'Supply a reviewed schema-1 inspector snapshot extracted with --all.'
}
if (-not @($baseline.types | Where-Object { $_.kind -eq 'object' -and $_.name -ceq 'String' }).Count) {
    throw 'Re-extract the snapshot with the current inspector so native String signatures are canonical.'
}
$source = (Get-ChildItem -LiteralPath (Join-Path $root 'farever-more-runtime/src') -Filter '*.rs' |
    Sort-Object Name | Get-Content -Raw) -join "`n"
# Include field names, class names, and callback names used by the host. The
# closure below adds declaring ancestors and named field/argument/payload types.
$names = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($match in [regex]::Matches($source, '"([A-Za-z_][A-Za-z0-9_.$]*)"')) {
    [void]$names.Add($match.Groups[1].Value)
}
$methods = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($match in [regex]::Matches($source, 'c"([A-Za-z_][A-Za-z0-9_]*)"')) {
    [void]$methods.Add($match.Groups[1].Value)
}
$byName = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
foreach ($type in $baseline.types) {
    if ($type.kind -ne 'virtual' -and $type.name -match '^[A-Za-z_][A-Za-z0-9_.$]*$') {
        if ($byName.ContainsKey($type.name)) { throw "Ambiguous baseline type $($type.name)" }
        $byName[$type.name] = $type
    }
}
$pending = [Collections.Generic.Queue[string]]::new()
foreach ($name in $names) { if ($byName.ContainsKey($name)) { $pending.Enqueue($name) } }
$selected = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::Ordinal)
while ($pending.Count -gt 0) {
    $name = $pending.Dequeue()
    if ($selected.ContainsKey($name)) { continue }
    $type = $byName[$name]
    $fields = [ordered]@{}
    foreach ($field in @($type.fields | Sort-Object name)) {
        if ($names.Contains($field.name)) { $fields[$field.name] = $field.type }
    }
    $requiredMethods = [ordered]@{}
    foreach ($method in @($type.methods | Sort-Object name)) {
        if ($methods.Contains($method.name)) {
            $requiredMethods[$method.name] = [ordered]@{ arguments = @($method.arguments | Where-Object { $null -ne $_ }); return_type = $method.return_type }
        }
    }
    $bindings = [ordered]@{}
    foreach ($binding in @($type.bindings | Sort-Object field_name)) {
        if ($names.Contains($binding.field_name)) {
            $bindings[$binding.field_name] = [ordered]@{ arguments = @($binding.arguments | Where-Object { $null -ne $_ }); return_type = $binding.return_type }
        }
    }
    $selected[$name] = [ordered]@{
        name = $name; kind = $type.kind; super_type = $type.super_type
        fields = $fields; methods = $requiredMethods; bindings = $bindings; variants = @($type.variants | Where-Object { $null -ne $_ })
    }
    if ($type.super_type) { $pending.Enqueue($type.super_type) }
    $expressions = @($fields.Values) + @($requiredMethods.Values | ForEach-Object { $_.arguments; $_.return_type }) +
        @($bindings.Values | ForEach-Object { $_.arguments; $_.return_type }) +
        @($type.variants | ForEach-Object { $_.parameters })
    foreach ($expression in $expressions) {
        # Index-dependent recursion markers cannot be trusted as ABI signatures.
        if ($expression -match 'type#|<invalid-type:') { throw "Unresolved signature on $name`: $expression" }
        foreach ($match in [regex]::Matches($expression, '[A-Za-z_][A-Za-z0-9_.$]*')) {
            $reference = $match.Value
            if ($byName.ContainsKey($reference)) { $pending.Enqueue($reference) }
        }
    }
}
$contract = [ordered]@{
    schema_version = 1; baseline_sha256 = $baseline.source.hlboot.sha256
    types = @($selected.Keys | Sort-Object -CaseSensitive | ForEach-Object { $selected[$_] })
}
$json = ($contract | ConvertTo-Json -Depth 60) + "`n"
$destination = Join-Path $root $Output
if ($Check) {
    if ((Get-Content -LiteralPath $destination -Raw).Replace("`r`n", "`n") -cne $json.Replace("`r`n", "`n")) {
        throw 'Committed game compatibility contract is stale.'
    }
} else {
    [void](New-Item -ItemType Directory -Force -Path (Split-Path $destination -Parent))
    [IO.File]::WriteAllText($destination, $json.Replace("`r`n", "`n"), [Text.UTF8Encoding]::new($false))
}
Write-Output "Game compatibility contract: $($contract.types.Count) types, baseline $($contract.baseline_sha256)"
