# Add-on hot reload

## Status

The host implements event-driven hot reload for `farever:addon@1.0.0`
components. The lifecycle manager watches the add-on directory with the native
filesystem notification backend (`ReadDirectoryChangesW` on Windows and the
platform equivalent elsewhere). It does not poll component timestamps or wait
for a file-size stability window.

The create, rename, and delete notifications discussed here are private host
inputs. They are not game events and are not delivered through the WIT event
API.

## Publication contract

A component must be fully written to a temporary file in the destination
directory and then atomically renamed over its final `.wasm` path. In-place
writes to an existing `.wasm` file are deliberately ignored because a watcher
could otherwise observe and compile a partial artifact.

`farever-more-build` implements this contract. Point the standard build at a
watched development installation to publish all maintained add-ons safely:

```powershell
.\scripts\build-wasm-addons.ps1 -Configuration Debug `
  -OutputRoot C:\path\to\farever-addons
```

The component encoder writes, flushes, and closes a same-directory staging
file, then replaces the destination with one rename. Direct third-party build
tools must provide the same behavior.

## Reload sequence

```text
atomic rename/create
        |
        v
native watcher -> generation-tagged compile request
                         |
                         v
               background Wasmtime compile
                         |
                         v
               serialized runtime safe point
                 1. deactivate old instance (reloaded)
                 2. unregister and discard transient host state
                 3. drop the old store
                 4. activate the compiled replacement
                 5. publish its UI/assets/timer state
```

Initial discovery uses the same compiler thread, so loading the framework does
not synchronously wait for Wasmtime compilation. Wasm activation,
deactivation, event handling, and ticks remain serialized on the runtime
worker. The watcher and compiler threads never call guest code.

If another file change arrives while compilation is in progress, the older
result is discarded. The host compares file contents and skips the reload when
nothing changed.

Reload waits until the current callback batch finishes. It never starts inside
a game hook or on the renderer thread. Clicks from an old frame are discarded
after a reload, even if the new instance uses the same add-on ID.

## State and failure policy

Reload starts a new add-on instance. The host asks the old instance to
deactivate, then discards its temporary state, including timers, UI, fonts,
images, subscriptions, and queued messages. Saved configuration remains
available to the new instance.

Compilation happens before teardown:

- If replacement compilation fails, the current active instance remains active
  and the exact compile error is logged.
- If compilation succeeds, the old instance is deactivated and destroyed
  before replacement activation starts.
- If replacement activation fails, there is no rollback. The path becomes
  disabled for the session, its error is retained by the manager, and
  the error is written to `host.log`.
- Removing a component deactivates it with `removed`, destroys its transient
  state, and removes its manager record.
- Adding a component compiles it in the background and activates it at the next
  safe point.

The manager currently exposes active, compiling, and disabled state internally
and through diagnostics. A user-facing enable/disable and manager UI remains a
separate product slice.

## Tradeoffs

- Atomic publication shifts one requirement to build/install tooling, but
  removes debounce latency and ambiguous partial-file retries.
- A single compiler thread bounds CPU and memory spikes. Several rapid edits
  may still consume compilation work, but only the newest generation can
  activate.
- There is never more than one live instance for a path. This makes ownership
  and cleanup deterministic, at the cost of no rollback after activation has
  begun.
- Deactivation and activation are synchronous at the safe point. Fuel bounds
  guest work, but a separate wall-clock deadline is still planned.
- If the native watcher misses changes, the host scans the directory again.
  This is only a recovery step, not a periodic scan.

## Live QA

Reload timing, visuals, and interaction still need more in-game QA.
