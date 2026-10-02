# Rust add-on SDK

Rust add-on authors use `farever-more-sdk` to work with the
`farever:addon@1.1.0` interface. The SDK handles generated bindings, component
exports, and conversion between WIT data and Rust types, so add-on code does
not import WIT modules directly.

## Add-on shape

`context.game().map()` returns a callback-frozen `Snapshot<VisibleMap>` for
the native full map. Its `world` matches the zone's internal area ID. The
`bounds` rectangle and `world_to_client.project([x, y])` result use physical
game-client pixels; divide by `pixels_per_point` for overlay coordinates.
Closed, obscured, stale, loading, or unavailable maps carry no value.

Use `ui.passive_canvas(...)` for annotations that must preserve native mouse
input. A surface containing only that canvas paints at the exact anchor and
margin with no window gutter, decoration, or automatic repositioning. The
canvas clips primitives to its rectangle and the client viewport. The SDK
stages options through the separate `canvas-options` interface when returning
the frame. They apply atomically to that callback's replacement frame, never
to another add-on or a retained frame. Passive canvases are excluded from
configuration menus. Existing interactive canvases keep their click behavior.

An add-on is an ordinary stateful Rust value with use-case-level callbacks:

```rust
use farever_more_sdk::prelude::*;

const ENABLED: Setting<bool> = Setting::boolean("enabled", true)
    .label("Enabled")
    .description("Show the example overlay.");

struct ExampleAddon {
    enabled: bool,
    total_damage: f64,
}

impl Addon for ExampleAddon {
    fn activate(context: &mut ActivateContext) -> SdkResult<Self> {
        let enabled = context.config().register(&ENABLED)?;
        context.log().info("Example add-on activated");
        context.render();
        Ok(Self {
            enabled,
            total_damage: 0.0,
        })
    }

    fn on_damage(&mut self, context: &mut Context, damage: Damage) -> SdkResult<()> {
        self.total_damage += damage.amount;
        context.render();
        Ok(())
    }

    fn on_events_lost(
        &mut self,
        context: &mut Context,
        _loss: EventLoss,
    ) -> SdkResult<()> {
        self.total_damage = 0.0;
        context.render();
        Ok(())
    }

    fn on_ui_event(&mut self, context: &mut Context, event: UiEvent) -> SdkResult<()> {
        if let UiEvent::CheckboxChanged { id, checked } = event {
            if id == ENABLED.key() {
                context.config().set(&ENABLED, &checked)?;
                self.enabled = checked;
                context.render();
            }
        }
        Ok(())
    }

    fn view(&mut self, _context: &ViewContext) -> SdkResult<Frame> {
        let mut frame = FrameBuilder::new();
        frame.config_menu("settings", "Example", |ui| {
            ui.section("general", Section::new("General"), |ui| {
                ui.checkbox(ENABLED.key(), "Enabled", self.enabled, true);
            });
        });
        if self.enabled {
            frame.surface(
                "example",
                "Example",
                SurfaceOptions::new(Anchor::TopLeft)
                    .margin(24.0, 24.0)
                    .width(240.0),
                |ui| ui.label("damage", format!("Damage: {}", self.total_damage)),
            );
        }
        Ok(frame.finish())
    }
}

farever_more_sdk::export!(ExampleAddon);
```

The SDK calls typed `Addon` methods in the order events arrived. If the host
reports lost events, `on_events_lost` runs before the remaining events.
Add-ons do not need to handle raw WIT batches or pass callback context objects.

Calling `context.render()` coalesces any number of requests into one call to
`view` after the callback succeeds. With no UI request, the host retains the
previous successful frame. `replace_ui` supplies an already-built frame and
`clear_ui` removes all retained UI.

## Host services

Host capabilities are available from the callback context in the lifecycle
phases where they are valid:

- `context.config()` reads and writes typed `Setting<T>` values. Activation
  additionally supports `register`.
- `context.assets()` and subscription-only `context.bus()` are exposed
  during activation. `assets.register_font(...)` installs an embedded OpenType
  face for semantic text styles; `assets.register_image(id, png)` validates an
  embedded PNG once and returns an opaque, add-on-namespaced `Image` reference.
- regular callbacks expose game snapshots, logging, timers, chat output, and
  message publication through `context.game()`, `log()`, `timer()`,
  `chat()`, and `bus()`.
- `context.game().snapshot()` groups commonly used observation, session,
  player, camera, and window state. Hook-backed combat, party, and instance
  state is available through `combat()`, `party()`, and `instance()`. Each
  getter reads from the snapshot captured before the callback and copies only
  the data it returns.
- party actor IDs correspond to the actor IDs on damage events. `Party::member`
  and `CombatActor::party_member` handle those lookups without exposing native
  hook identities or pointers.
- timers use `std::time::Duration`; config variants and millisecond integers
  do not leak into add-on code.

UI is constructed with semantic SDK values such as `Color`, `Column`,
`SurfaceOptions`, and `Text`. Their constructors encode valid combinations
before the SDK converts the finished `Frame` to the WIT document.

Every author-facing UI construction type is available from
`farever_more_sdk::ui`, including the image and text-style types used by UI
widgets. Add-ons can therefore import individual primitives without enabling
raw WIT access:

```rust
use farever_more_sdk::ui::{Color, Stroke, SurfaceStyle};

let panel = SurfaceStyle::new(Color::rgba8(28, 30, 36, 240))
    .stroke(Stroke::new(1.0, Color::WHITE));
```

## Raw WIT escape hatch

The generated module is reserved for the export adapter and hidden from API
documentation. An add-on experimenting with a WIT function that has no façade
yet may explicitly enable the unsupported `raw-wit` Cargo feature and import
`farever_more_sdk::raw`. Raw access is not covered by the SDK compatibility
promise. Maintained reference add-ons are checked and may not use it.

## Build

Install the ordinary Rust Wasm target once:

```powershell
rustup target add wasm32-unknown-unknown
```

Build and componentize every reference add-on with:

```powershell
.\scripts\build-wasm-addons.ps1
```

The script first confirms the reference add-ons use the supported SDK and that
its version matches WIT. It then builds the Wasm modules and packages them with
the repository's `farever-more-build` tool. Add-on authors do not need
`cargo-component` or a local copy of the WIT files.

## Manifest and pack step

Every unit carries `addon.json` beside its component. The component is always
named `addon.wasm`; the manifest never names it, and the pack step is what
makes a unit installable:

```json
{
  "manifest-version": 1,
  "id": "minimap",
  "version": "0.1.0",
  "name": "Minimap",
  "description": "World minimap with POI markers.",
  "tags": ["map", "overlay"],
  "sha256": "<stamped by the pack step>",
  "api-version": "<stamped by the pack step>",
  "provides": ["poi"],
  "dependencies": [
    { "addon": "poi-database", "services": [{ "id": "poi", "version": "^1" }] }
  ]
}
```

Source trees omit `sha256` and `api-version`, because neither the hash of a
future build nor the SDK it will be built with is known upfront:
`build-wasm-addons.ps1` and the installer read both out of the built component
and stamp them in. `api-version` is the add-on API the component was built
against, and it is the number the runtime and the manager enforce before an
add-on is allowed to load — see
[api-versioning.md](api-versioning.md) for the compatibility rules.
A service is served at the add-on's own version, and a
dependency is named by the provider's add-on id — the same string
`context.dependencies().open(addon, service)` takes.

## WIT synchronization

The repository's `wit/` directory is authoritative. Maintainers regenerate
the SDK's single internal binding file with pinned `cargo-component 0.21.1`:

```powershell
.\scripts\sync-addon-sdk.ps1
```

Use `-Check` to see whether regeneration would change the binding file. The
script also checks that the SDK package version and `WIT_PACKAGE` constant
match the WIT package version. After changing WIT, regenerate the bindings and
rebuild the reference add-ons.
