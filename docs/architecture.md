# Farever More architecture

Farever More runs Rust add-ons as WebAssembly components. A trusted Windows
host reads selected game state, calls add-ons through a versioned WIT interface,
and renders their overlays. Each add-on folder under `farever-addons/addons`
contains an `addon.json` manifest and an `addon.wasm` component.

## Architecture

```text
Farever
  `-- dinput8.dll proxy -> trusted native host
                              |-- game-state reader
                              |-- damage capture
                              |-- Wasmtime components
                              `-- overlay renderer

farever-addons/addons/
  `-- dyno/addon.wasm (reference add-on)
```

Each component runs in its own Wasmtime store. It cannot access game-memory
pointers, native entry points, files, or the network. It has 128 MiB of linear
memory, a fixed object limit, and an execution budget that resets on every
call.
Components can read game snapshots, write to the host log, schedule recurring
callbacks, register fonts during activation, and exchange local messages. The
host checks each UI frame before drawing it.

For build and contributor checks, see the root [README](../README.md) and
[contributor guide](../CONTRIBUTING.md).

## Install into Farever

Close Farever before installing or updating:

```powershell
.\scripts\farever-addons.ps1 Install
```

The utility detects Steam app `3672400`, builds Release artifacts by default,
and installs:

```text
<Farever>/
  dinput8.dll
  farever-addons/
    host.dll
    addons/
      dyno/
        addon.json
        addon.wasm
      gps/
        addon.json
        addon.wasm
```

It records owned paths and hashes in
`farever-addons/install-manifest.json`, refuses to overwrite an unmanaged
`dinput8.dll`, and only updates/removes files it owns. Use `Update`, `Status`,
`Logs -Follow`, and `Uninstall` for the normal development cycle. Pass
`-GameDirectory` if Steam discovery does not find the desired installation.

Additional components can be supplied during update:

```powershell
.\scripts\farever-addons.ps1 Update `
  -AddonWasm C:\build\my-addon.wasm
```

The reference meter remains included unless `-SkipReferenceAddon` is used.

## Supported game builds

Hooks are enabled only for these exact combinations of game-file hashes:

| Steam profile | `Farever.exe` SHA-256 | `hlboot.dat` SHA-256 | `libhl.dll` SHA-256 |
| --- | --- | --- | --- |
| Stable `25257040` | `96F5DFEEF6F1D3E1AA0810BE333CF23965E42C0C0328CFDBD02F2693913C28EB` | `7D0C189415AD6832B11DA0FAFDA29EAAA2A41F93330C4489942820B495E1DF89` | `0D67CA75C73F93306158D67BD2B61763C539F3155375403CD52D8F16DB7F73ED` |
| Beta `25531577` | `186440648F9906C2E64955F9A7B2508D9C6842F90ABEE749187191A822CAEF36` | `42A7EE2E85ED9166510BDC7DF2BFDDB8ECEBCD10917A3FDEA94A8BC4F381E5ED` | `0F6FB5D60D42039BE36D1426098A73127388A892AB1E59D2C81DD33AC9C279FB` |

A partial match is not enough. To support another release, all three file
hashes must match and the host's game interfaces must be reviewed. Stable and
beta builds have different chat, combat, and inventory interfaces, so the host
selects the matching build before enabling hooks. The beta build compiles and
matches the inspected metadata, but beta behavior and live capture still need
more in-game QA.

## How the host reads game state

Before installing hooks, the host checks the exact hashes of `Farever.exe`,
`hlboot.dat`, and `libhl.dll`; unknown builds are rejected. It then waits until
the local Hero is fully constructed and its player reference is valid before
resolving the game state needed by add-ons.

The live-capture path hooks HashLink's allocator and forwards observations to a
fixed-size queue. It does not change game objects, but installing the hook
patches game code, so this path is not strictly read-only. The ordinary state
poller remains read-only.

A direct `ent.Unit.onInflictDamage` hook currently supplies outgoing damage.
After checking its signature and fields, the callback copies the source, owner,
target, and damage values to a queue. A worker normalizes those records and
sends events to add-ons outside the game callback.

The older `ui.comp.DamageDisplay` reader remains available as an alternative;
the two providers are never active at the same time. By default, the host uses
the direct hook and polls any event type without an active hook. Create an
empty `farever-addons/use_polling.flag` before launch to select polling instead.
The direct hook reports client-routed damage and does not yet cover every
server combat-log event.

Combat tracking observes changes to the local Hero's `set_isInCombat` field
and keeps polling as a fallback. It can infer a combat start from the first
outgoing hit.

## Hooking rules

Hooks are enabled only after the host checks the expected Windows x64 calling
convention, object layout, and function signatures. A game-thread callback
copies the needed values into a fixed-size queue; logging, Wasm calls, and other
work happen later on a worker. If the callback cannot safely copy a value, it
must keep the object rooted until the worker is done with it. MinHook owns the
machine-code patches and their cleanup.

The host does not modify `hlboot.dat` or replace HashLink's standard library.
It inspects bytecode metadata offline, then checks the live layout before
enabling a reader or hook.

Player, camera, target positions, and party details are still polled. Hook-backed
state keeps a polling fallback until live QA confirms the hook.

## UI and diagnostics

The host draws overlays in a separate Direct3D window that follows Farever's
client area, lets mouse clicks pass through, and hides when Farever is not the
active window. The damage meter takes design cues from Skada but uses no Skada
code or assets. Its layout is built with egui and displayed through
DirectComposition.

Add-ons describe their UI through the WIT frame format; they do not use egui or
receive native input handles. The host keeps showing the last valid frame if
an update is rejected. Add-on code runs when work arrives or a registered
timer fires.

The damage meter bundles Noto Sans Regular and Bold. Fonts and images are
handled by the host renderer; add-ons refer to images by opaque IDs and never
read the game archive directly.

The host log at `farever-addons/logs/host.log` records startup, component
failures, capture status, and overlay placement.

Log lines use the format `[unix_ms=<milliseconds>] <category> <message>`.
Failures, degradations, and state transitions are logged once per change.
Steady-state counters and per-frame or per-fight telemetry are available at
debug level. Add-on `log()` messages use the same file and level policy.
`FAREVER_LOG_LEVEL` accepts `error`, `warn`, `info`, or `debug` and defaults to
`info`. The log rotates at 8 MB through `host.log.1` to `host.log.3`.

During startup, a small host panel shows the runtime version and loaded
add-ons. Manual logout clears the overlay frame and hides both overlay windows
at the main menu. They reappear when the next character world is entered.
The damage meter appears while Farever is usable and active, and scales with
the game window.

## Current limits

- Hooks are enabled only for the two exact profiles above. Beta behavior and
  live capture still need more in-game QA. Sampled state remains available as a
  fallback.
- Outgoing damage is the only combat event currently exposed to add-ons. Other
  observed combat and game activity remains internal until live testing
  establishes its coverage and meaning.
- `windows.focused-window` identifies the frontmost registered game window; it
  does not report keyboard, text-input, or child-control focus.
- Map parity, broader game-state access, packaged-texture queries, input,
  storage, and manifest capabilities remain planned.
- Components have fuel and memory limits. Wall-clock limits and protections
  against malicious components remain open work.

See the [lifecycle and tick overview](lifecycle.md),
[add-on message bus](message-bus.md), and executable
[`farever-addon.wit`](../wit/farever-addon.wit) contract.

The reference meter's behavior and remaining live QA are documented in
[`addons/dyno/README.md`](../addons/dyno/README.md).
