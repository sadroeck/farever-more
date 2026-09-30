# Farever internal API inspector

`farever-api-inspector` reads the HashLink metadata in Farever's `hlboot.dat`
without starting the game. It provides a command-line version of the read-only
API inventory shown in the `api_inspector.lua` example from Farever Minimap.
By default, it reports the types most useful to add-on authors. Each JSON
snapshot includes:

- objects, structs, abstracts, enums, and anonymous virtual records;
- declared fields with logical slots and types, plus each type's inheritance
  chain and inherited-field count;
- object prototypes and field-bound functions with exact signatures;
- relevant globals and native-call metadata (with methods and bindings kept on
  their declaring types); and
- hashes of `hlboot.dat`, `Farever.exe`, and `libhl.dll` plus the Steam build
  ID when extraction starts from a game directory.

The export helps find candidates for host readers; it does not make those
entries safe to use. It contains no live addresses or offsets and cannot show
whether a type is active or what a method does. Before using a method in-game,
the runtime must find it, check its signature and object layout, and reject
unknown shapes.

## Build

```powershell
cargo build -p farever-api-inspector --release
```

The parser uses an MIT-licensed `hlbc` fork at commit
`c1a56ee322561b7bc256c2592ab683d5e07696fd`. It reads HashLink bytecode versions
4 through 6 and rejects unknown newer versions. Reading a bytecode version
does not mean the installed HashLink runtime can execute it; the host still
requires the reviewed native runtime and game compatibility checks listed in
the [architecture guide](architecture.md#supported-game-builds).
The upstream parser changes are documented in [Gui-Yom/hlbc#14](https://github.com/Gui-Yom/hlbc/pull/14).

## Extract a release

```powershell
.\target\release\farever-api-inspector.exe extract `
  --game-dir "D:\SteamLibrary\steamapps\common\Farever" `
  --output ".\target\api-inspector\farever-current.json"
```

For an unpacked or archived bytecode file, pass `--hlboot` instead. Optional
`--farever-exe`, `--libhl`, and `--steam-manifest` inputs add release metadata.

Snapshot arrays and IDs are deterministic. The extraction timestamp and raw
type, global, function, field, and prototype indexes remain in the JSON for
diagnostics. Fields are emitted once on their declaring type rather than copied
onto every descendant; follow `super_type` to reconstruct a flattened view.
The anonymous built-in string object is reported as `String` when its exact
`bytes: bytes` and `length: i32` schema is present. Older snapshots reported it
as `<none>`; re-extract both inputs before comparing across this naming change.

By default, the export includes types used by the current host and reference
add-ons. It keeps their inheritance chains without expanding every referenced
type. Use `--root-type`, `--namespace`, `--expand-fields`, or `--all` for a
broader view.

Extend the focused selection when investigating a new domain. `--root-type`
adds one precise structural root; `--namespace` intentionally includes a whole
domain. Add `--expand-fields` when definitions directly referenced by the
selected fields and enum payloads are useful for that investigation:

```powershell
.\target\release\farever-api-inspector.exe extract `
  --game-dir "D:\SteamLibrary\steamapps\common\Farever" `
  --namespace "script." `
  --root-type "gamepad.Pad" `
  --expand-fields `
  --output ".\target\api-inspector\farever-script-and-input.json"
```

Use `--all` for the complete bytecode inventory. `--all` cannot be combined
with `--namespace`, `--root-type`, or `--expand-fields`. Diffs reject snapshots
made with different selections so a filter change cannot masquerade as a game
API change.

## Compare two snapshots

```powershell
.\target\release\farever-api-inspector.exe diff `
  --old ".\snapshots\farever-old.json" `
  --new ".\target\api-inspector\farever-current.json"
```

Text is the default diff format. Use `--format json --output report.json` for a
machine-readable report. `--check` returns exit code 2 when semantic changes
exist, which is useful in automation.

The semantic diff ignores type/global/function/field/prototype index churn
while reporting changes to names, inheritance, fields and their types, method
and binding signatures, enum variants, global type counts, and module/native
callables. Raw indexes remain available in the snapshots for manual
investigation.

## Runtime compatibility contract

The runtime uses this crate's `verify_bytecode_contract` library entry point for
unknown game bytecode, before installing hooks or loading components. It uses
the same parser and signature rendering as the CLI. Required fields, methods,
bindings, inheritance, and enum payloads are compared with an embedded reviewed
projection; unrelated additions and raw index changes are accepted. This check
also verifies the host's playable-state constant/call pattern. Native file
fingerprints and live ABI/layout validation remain required. See the
[architecture guide](architecture.md#supported-game-builds) for regeneration
and failure behavior.
