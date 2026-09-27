# Add-on lifecycle and ticks

## Status

This document describes the public `farever:addon@1.0.0` lifecycle. Earlier
0.x components are incompatible; see [API versioning](api-versioning.md) for
the compatibility rules.

The flow looks like this:

```text
Farever and trusted host
    -> game state and events
    -> one add-on callback at a time
         on-ui-event / on-event / on-message / on-tick
    -> add-on's current UI
    -> host renderer
```

Farever and the host supply game state and events. The host calls add-ons when
work is ready. The renderer only draws the latest valid frame; it never calls
add-ons or reads game memory.

## Component contract

The world imports game state through separate `game`, `player`, `party`,
`camera`, `combat`, `instance-session`, `zone`, and `windows` interfaces. The
Before each callback, the host captures one game-state snapshot. Every getter
in that callback reads from the same snapshot without querying the game again.
Logging and scheduling are provided through `runtime`; fonts and image
references are provided through `assets`.

The guest `game.session().in-world` becomes true only after the game's final
loading state is observed for the current world. A world object and player
position may be available earlier while the loading screen is still visible.
The host continues capturing those early observations internally; add-ons use
`in-world` to decide when to show world UI. On the two exact supported builds,
the final loading state is `10`.

Each component exports six functions:

| Export | Purpose |
| --- | --- |
| `activate(context)` | Initialize an instance, register resources, optionally schedule a timer, and optionally show an initial UI frame. |
| `on-ui-event(event)` | Handle a UI event checked by the host, such as a button press, and return an optional UI change. |
| `on-event(batch)` | Handle an ordered batch of game events and return an optional UI change. |
| `on-message(dropped-before, messages)` | Handle messages from other add-ons; `dropped-before` counts lost messages. |
| `on-tick(tick)` | Handle a scheduled tick, decide whether to keep the timer, and return an optional UI change. |
| `deactivate(reason)` | Clean up when the host shuts down, reloads, disables, or removes an add-on, or after repeated failures. |

`activation-context` contains a stable add-on ID, a host-assigned instance ID,
and the host monotonic time. An add-on creates or replaces its single recurring
timer by calling `runtime.schedule-tick(interval-ms)` during activation or a
callback. The import returns the interval accepted by the host.

The host invokes `on-event` only when a batch contains events or loss
information, `on-message` only when messages or a message-loss notification are
pending, and `on-tick` only when the component's recurring tick is due. When
more than one is ready, the order is UI events, event, message, then tick. Each successful
output is applied before the next callback, and callbacks never run concurrently.

The Rust SDK adapter invokes `on_events_lost` followed by typed `Addon` event
methods in wire order. Ticks arrive directly through `Addon::on_tick`.
The event batch includes `player-disconnected(player-disconnected-event)` for
the local player's validated `st.Player.disconnect` call. Add-ons receive the
SDK callback `Addon::on_player_disconnected` with a `DisconnectReason` value:
`ManualExit`, `Kick`, `Timeout`, `SwitchingServer`, or `Unknown`. The beta hook
reads the reason enum. The stable hook reads the older boolean: `true` is
normalized as `ManualExit`, while `false` is `Unknown` because that build does
not expose which non-manual cause occurred. The event does not include the
beta-only switching-server payload.

UI, event, and message callbacks return only `ui-update`: retain the previous frame,
replace it, or clear it. `on-tick` returns the same UI decision plus
`continue-ticking: bool`. `false` stops the timer that caused the callback;
`true` retains it. If the callback calls `runtime.schedule-tick`, that new timer
replaces the old one and wins over `continue-ticking`. Multiple scheduling
calls in one activation or callback use the last requested interval.

Event and tick callbacks may change add-on state without rebuilding the UI.
Calls to `Context::render` during `on-event` are combined into at most one
`Addon::view` call after the callback finishes. `on-tick` separately chooses
whether to rebuild the UI.

## Tick semantics

The current host accepts one recurring tick per component. Requested intervals
are clamped to 16 through 60,000 milliseconds. The accepted interval is
returned by `runtime.schedule-tick` and reported in every delivered tick. A
schedule request is committed only after the activation or callback and its UI
output succeed, so a rejected result cannot silently alter future cadence.

Tick time uses the host's monotonic epoch rather than wall-clock time. Each tick
contains:

- its scheduled and actual delivery times;
- elapsed time since the previous delivery;
- the accepted interval;
  - how many intervals were skipped because the host was late.

The host never replays missed ticks in a burst. A component receives one late
tick and can integrate using `elapsed-ms` or decide to skip work.

The runtime wakes at most every 16 milliseconds to service the overlay and due
ticks. That wake-up does not poll game memory. The existing state adapter keeps
its separate 100 millisecond polling cadence, so a 50 millisecond Wayfinder tick
can redraw from the latest available snapshot without doubling polling cost.

## Reference behaviors

The damage meter is dormant until a combat-start or damage event makes the
encounter active, then calls `runtime.schedule-tick(250)`. It aggregates every
damage event immediately but normally returns `ui-update.unchanged`; while
combat is active, each tick requests one render from the accumulated state and
returns `continue-ticking: true`. Combat start/end and event-gap callbacks
request an immediate frame so important boundaries do not wait for the next
tick. After combat ends, the next already-scheduled tick returns `false`; the
completed meter remains cached without continuing to rebuild it.

Wayfinder calls `runtime.schedule-tick(50)` during activation and returns
`continue-ticking: true` from each tick. Event-only calls leave its UI
unchanged. On each tick it reads the latest snapshot and replaces its frame
only when the host reports a new observation; duplicate ticks return
`unchanged`. Player and camera data can be up to 100 milliseconds old. A faster
add-on timer does not make the game-state poll run faster.

An event-only add-on simply never calls `runtime.schedule-tick`. It remains dormant
when there are no events and can start a recurring timer from a later callback.

## Isolation and failure behavior

- Each component has its own Wasmtime store and 128 MiB linear-memory limit.
- No WASI interfaces are linked, so the component receives no ambient native
  filesystem, network, process, environment, or clock access.
- Activation, UI-event, event, message, tick, and deactivation callbacks receive fresh
  fuel budgets.
- UI replacement is validated before it becomes visible. `unchanged` retains
  the previous validated frame.
- A failed event, message, or tick callback retains the previous frame while the
  component is recoverable. Its staged timer and bus publications are discarded.
  Three consecutive failures disable the component, cancel its tick, clear its
  frame, and trigger best-effort deactivation.
- Wasm never executes in a HashLink hook or on the DirectComposition renderer
  thread. All callbacks for one component are serialized on the runtime worker.
- Initial discovery and replacement compilation run on a dedicated compiler
  thread. Hot reload deactivates and drops the old store before synchronously
  activating the replacement at a runtime safe point. Activation failure has
  no rollback and leaves a disabled manager record with the error.
- A local `ManualExit` disconnect delivers its final event batch, then stops
  the add-on manager at the same runtime safe point. Each active component
  receives best-effort `deactivate(host-shutdown)` and its store, timer,
  messages, service registrations, and UI resources are discarded. The host
  waits for the world to exit and a new character world to become live before
  discovering and activating fresh instances. A server-switch disconnect does
  not stop the manager. Persisted add-on configuration survives this cycle.
- A component built for an add-on API this host cannot satisfy is refused during
  load, before any guest code runs: the declared `api-version` and the
  component's own versioned imports are both checked against the host's, and the
  record keeps the reason. See [api-versioning.md](api-versioning.md).

The full watcher, publication, teardown, and failure semantics are documented
in [Add-on hot reload](hot-reload.md).

Add-ons use the WebAssembly interface generated from WIT; they do not depend on
Windows calling conventions or Rust struct layouts. The injected host currently
supports Windows x86-64. The add-on interface and scheduler could also be used
by a future Linux host.

## Polling and direct-event transition

This lifecycle does not choose how a normalized event was captured. Existing
sampled providers continue producing batches. The damage domain now uses a
validated HashLink hook when available. An empty `use_polling.flag` selects the
earlier `DamageDisplay` polling producer without suppressing hook installation;
without that flag, an event type lacking an active hook is polled automatically.
Provider selection remains exclusive per event domain so polling and hooking
cannot emit duplicate logical events.

A game hook copies only the needed values into a fixed-size queue. It does not
invoke Wasm, query an add-on, build UI, or render. The runtime still uses its
existing polling and event-collection path; queued hook events do not yet wake
the runtime worker directly.

## Live QA

Runtime behavior and timing still need more in-game QA.

## Not implemented yet

- Add-ons cannot yet subscribe to individual game events, and the host does not
  start service providers on demand. Add-on messaging works separately.
- Game hooks do not yet wake the runtime directly, and game-window state is
  still read by polling.
- Users cannot yet disable or suspend an add-on from a component manager.
- Add-ons have no wall-clock timeout in addition to their execution budget.
- Components cannot schedule more than one recurring tick.
- Tick intervals do not yet adapt to measured frame cost.
