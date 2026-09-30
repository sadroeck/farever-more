# Farever More architecture

Farever More runs add-ons as WebAssembly components. Authors can use any
language that produces a component compatible with the versioned WIT interface;
the repository provides a Rust SDK for that interface. A trusted Windows host
reads selected game state, calls add-ons, and renders their overlays. Each
add-on folder under `farever-addons/addons`
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

These exact combinations of game-file hashes use the startup fast path:

| Steam profile | `Farever.exe` SHA-256 | `hlboot.dat` SHA-256 | `libhl.dll` SHA-256 |
| --- | --- | --- | --- |
| Stable `25257040` | `96F5DFEEF6F1D3E1AA0810BE333CF23965E42C0C0328CFDBD02F2693913C28EB` | `7D0C189415AD6832B11DA0FAFDA29EAAA2A41F93330C4489942820B495E1DF89` | `0D67CA75C73F93306158D67BD2B61763C539F3155375403CD52D8F16DB7F73ED` |
| Beta `25531577` | `186440648F9906C2E64955F9A7B2508D9C6842F90ABEE749187191A822CAEF36` | `42A7EE2E85ED9166510BDC7DF2BFDDB8ECEBCD10917A3FDEA94A8BC4F381E5ED` | `0F6FB5D60D42039BE36D1426098A73127388A892AB1E59D2C81DD33AC9C279FB` |
| Stable `25628371` | `186440648F9906C2E64955F9A7B2508D9C6842F90ABEE749187191A822CAEF36` | `8BB2CE5180018EBFFE0C3F357C77B86BF5EBCECD06AA65B97A69367F2F806E49` | `0F6FB5D60D42039BE36D1426098A73127388A892AB1E59D2C81DD33AC9C279FB` |
| Stable `25632706` | `186440648F9906C2E64955F9A7B2508D9C6842F90ABEE749187191A822CAEF36` | `D48F5F511A8257832EF470FCBA82A0A7F5DB402CE8EABD11CAF42BBBA21E4280` | `0F6FB5D60D42039BE36D1426098A73127388A892AB1E59D2C81DD33AC9C279FB` |

All three hashes must match to skip bytecode inspection. Stable
`25257040` and beta `25531577` have different chat, combat, and inventory interfaces, so the host
selects the matching build before enabling hooks. The beta build compiles and
matches the inspected metadata, but beta behavior and live capture still need
more in-game QA.

Stable `25628371` retains the ABI introduced by beta `25531577`. Its full
offline metadata inventory was compared against that beta: the host's hook
entry-point signatures are unchanged. Added Player, Skill, and ActivityMarker
fields are resolved through live metadata, not fixed gameplay offsets. The
executable and HashLink runtime hashes are unchanged. This review establishes
offline compatibility; live behavior on this release still needs in-game QA.

Stable `25632706` retains the same native pair and host ABI. Its full metadata
diff against `25628371` changes NPC icon records and unhooked methods including
`GameApp.playCutscene`, `ent.Hero.getStandingObelisk`,
`st.Player.doStartObeliskSequence`, and two MapWindow marker helpers. Required
hook and callback signatures, field schemas, enum payloads, and the playable
loading-state pattern are unchanged. A fresh local launch verified the exact
profile, four active add-ons with none disabled, playable loading state `10`,
and both map-click hooks active. Broader gameplay and unknown-release live QA
remain outstanding; the unknown-build contract path was tested offline against
the same installed bytecode with its exact profile omitted.

An unknown `hlboot.dat` is also accepted when `Farever.exe` and `libhl.dll`
still match the reviewed native pair from stable `25632706`, and runtime
inspection verifies the embedded bytecode contract from that release. Changed
native files require a separately reviewed profile: bytecode signatures cannot
establish the native HashLink memory layout or executable ABI.

Before installing even the allocator hook or loading any add-on, the worker
uses the inspector's parser to check required types and inheritance, host-read
field types, method receiver/argument/return signatures, bound callbacks, and
ordered enum constructors and payloads. Function bodies, numeric metadata
indexes, and unrelated members may change. One explicit behavior check also
requires `GameApp.finishedLoading` to call `set_loadingState(10)` on its receiver
with the inspected constant/call pattern. A signature check does not prove all
gameplay semantics; live hook targets, calling conventions, and memory layouts
are still checked as each provider is installed.

The contract is embedded in
`farever-more-runtime/assets/game-compatibility-25632706.json`. Generate it from
a reviewed full inspector snapshot with
`scripts/generate-game-compatibility.ps1 -Snapshot <snapshot.json>`; use `-Check`
to detect stale coverage after changing the host. The generator conservatively
selects host-used field and callback names on referenced classes, then includes
declaring ancestors and referenced field, argument, and enum-payload types.
Review this projection together with new readers and hooks; dynamically built
metadata names need explicit coverage. The startup log distinguishes
`verification=fingerprint` from `verification=bytecode-signatures` and names the
reviewed baseline. Failed inspection refuses startup with the mismatch reason,
closes the loading overlay, and uses the native loading-error dialog.

## How the host reads game state

Before installing hooks, the host checks the game-file fingerprints and, for
unknown bytecode, the compatibility contract described above. It then waits until
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

Full-map navigation captures `ui.win.MapWindow.onClickWorld` and, on beta
`25531577`, stable `25628371` / `25632706`, and signature-compatible bytecode,
`popupActivityMenu`. The former is a receiver-bound function field:
the host resolves its default implementation through the validated HashLink
binding table and checks both the stored field and implementation signatures.
Activity clicks copy the marker's validated `worldPos` before opening the native
selector. Both hooks forward the original callback once and enqueue finite
horizontal coordinates for the runtime to publish on `farever.map-click@1`.
The GPS add-on owns the waypoint and its "Arrows from the map" preference.
Map dragging and the native HUD selector keep their game behavior. See the
[message protocol](message-bus.md) and [GPS add-on](../addons/gps/README.md).
Native full-map target selection has been confirmed in user beta testing.

## Hooking rules

Hooks are enabled only after the host checks the expected Windows x64 calling
convention, object layout, and function signatures. A game-thread callback
copies the needed values into a fixed-size queue; logging, Wasm calls, and other
work happen later on a worker. If the callback cannot safely copy a value, it
must keep the object rooted until the worker is done with it. MinHook owns the
machine-code patches and their cleanup.

The host does not modify `hlboot.dat` or replace HashLink's standard library.
It inspects bytecode metadata offline or during unknown-build startup, then checks the live layout before
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

During startup, a small host panel shows the runtime version and discovered
add-ons. It disappears when initial compilation and activation finish,
including when components fail or no components are installed. Failed components
produce a native error dialog with the retained reason and host-log path;
unchanged failures are reported once. A new failure after recovery is reported
again. Successful add-ons keep running when another component fails.
An unsupported build or terminal game-observer failure ends the runtime,
closes its overlay windows, and shows a native error dialog instead of waiting
forever for a readiness signal that cannot arrive. Dialogs run on a separate
thread and do not block the game or add-on callbacks.
Manual logout clears the overlay frame and hides both overlay windows
at the main menu. They reappear when the next character world is entered.
The damage meter appears while Farever is usable and active, and scales with
the game window.

## Current limits

- Hooks require an exact profile or a compatible bytecode contract with the
  reviewed native executable/runtime pair. New-release behavior and
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
