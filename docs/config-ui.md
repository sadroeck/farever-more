# Host-embedded add-on configuration UI

## Status

The current configuration UI supports buttons, checkboxes, dropdowns, and
sliders, with nested containers and titled sections. Text boxes are not yet
available.

## Ownership model

An add-on owns its settings and can declare one or more `config-menu` pages in
its `ui-frame`. Each page has an add-on-local ID, a title, the same widgets as
ordinary surfaces, and optional canvas commands.

The host provides the shared Add-ons window, navigation, theme, and input
handling. It lists loaded add-ons, gives each page a unique `(add-on ID,
menu ID)`, and sends each UI event only to the page's owner. Invalid, stale, or
disabled controls are ignored.

Add-ons provide page and node IDs, page content, button behavior, and
activation-time property descriptors. They update their own state and settings
when the host sends a UI event.

The trusted host draws every page and handles input, while each add-on keeps
control of its own settings.

## Entry point and shell

The host shows a rectangular cog button beside the Game Menu's social buttons
only when all of the following are true:

- the host has observed Farever's `ui.win.EscapeMenu` (the visible Game Menu)
  as open;
- the Farever client is the usable foreground target for the overlay.

The Farever More cog is always available, even when no add-on provides a page.

Pressing the cog opens a host-rendered Add-ons window. Its left rail pins
`Farever More` under a Framework heading, then lists registered add-ons
alphabetically in an independently scrollable Add-ons section. Multiple pages
from the same owner share one add-on row and appear as local tabs in the content
pane. The host derives two-letter navigation marks from the display title or
owner namespace, so the layout does not require an add-on icon API. The wider
right pane renders the selected host settings or add-on document and states that
changes apply immediately. The host page currently contains an immediately
persisted `Enable slash commands` toggle.
The title-bar X is the only explicit close control; there are no Done, Save, or
Reset-All buttons. Closing Farever's Game Menu also closes the window and emits
the matching hidden event for a selected add-on page.

The shell uses a Farever-inspired theme with parchment panels, cocoa text and
borders, rose controls, and gold highlights. Add-ons provide the page content;
the host applies the same theme to every page.

## Control and event contract

A button is declared as a widget on a stable node ID:

```wit
record button-widget {
    label: string,
    enabled: bool,
}
```

A checkbox is a controlled widget valid only in a `config-menu` document:

```wit
record checkbox-widget {
    label: string,
    checked: bool,
    enabled: bool,
}
```

The add-on supplies the current `checked` value in every retained frame. The
host does not mutate add-on state locally.

A dropdown declares a stable selected option ID and an ordered list of up to 64
stable ID and display-label pairs. The selected ID must name one of those
options. A slider declares a finite current value, an inclusive contiguous
`minimum` and `maximum`, and an optional positive `step`. `step: none` is
continuous; a supplied step is anchored at the minimum.

Dropdowns open and close inside the host shell and emit one change when a new
option is chosen. Slider drags are captured by the host, previewed locally, and
quantized when a step is present. One committed value is emitted on mouse
release rather than one callback per pointer move.

The host does not send pointer coordinates or native input handles to add-ons.
Instead, it calls `on-ui-event` with one of:

- `config-menu-shown(menu-id)`;
- `config-menu-hidden(menu-id)`;
- `button-pressed({ view, node-id })`;
- `canvas-pressed({ view, node-id, position })`;
- `checkbox-changed((node-id, checked))`;
- `dropdown-changed((node-id, option-id))`;
- `slider-changed((node-id, value))`.

`view` identifies either `surface(surface-id)` or
`config-menu(config-menu-id)`, so the button primitive is reusable outside the
configuration shell. The runtime routes the event to the validated owner and
serializes it before that add-on's event, message, and tick callbacks for the
same dispatch pass.

`canvas-pressed` is the one exception to the coordinate rule: a canvas that
draws anything becomes pressable, and the add-on receives the pointer position
in canvas-local logical points. That is what lets the minimap turn a marker
press into a GPS waypoint request without either add-on seeing raw input. A
press on a canvas the latest frame no longer contains is dropped, exactly like a
stale button.

Config-control events deliberately carry only their node ID and requested
value. Their owner and currently selected config menu are already trusted host
state, so repeating the page identity in the component callback would be
redundant.

Before delivery, a queued press is checked against the latest retained frame.
The node must still exist, still be a button, and still be enabled. A config
button must also belong to the currently selected page while the Game Menu is
open. A config control must remain enabled on that selected page, its requested
value must differ from the latest retained value, dropdown choices must still
exist, and slider values must remain within the unchanged declared range. This
prevents a click based on an old frame from triggering a removed, hidden, or
already-updated control. The input and event queues have fixed capacities.

### Layout and sections

Pages use the same nested widget layout as ordinary surfaces. Two widgets are
structural: `container` lays children out along one direction with
an optional host-themed group box, and `section` draws a titled header with the
children underneath it:

```wit
record section-widget {
    title: string,
    description: option<string>,
}
```

The host draws that header itself, in the same visual language as its own page
headers: a strong accent-colored title, an optional muted description line, and
a separator above the children. Add-ons therefore get section chrome that
matches the host instead of hand-rolling a group box plus a heading text, which
is what earlier pages did.

In the SDK a section is one call, and both structural widgets accept ordinary
widget children:

```rust
ui.section(POI_SECTION, Section::new("Points of interest").description("Cached"), |ui| {
    ui.checkbox("markers", "Show on map", true, true);
});
```

Leaf widgets cannot own children. The host rejects a frame that nests children
under a text, image, or control node instead of rendering it.

## Property descriptors

During `plugin.activate`, an add-on may register up to 1024 ordered property
descriptors. Each descriptor contains a stable key, presentation label,
optional description, value kind, type-matching default value, and access
policy:

- `editable` is available to future host-generated input controls;
- `readonly` remains visible but disabled in host-generated UI, while the
  owning add-on may still update its value;
- `hidden` remains persisted and inventoried but is omitted from
  host-generated UI.

Descriptors are validated and retained by the runtime rather than persisted.
They must therefore be registered on every activation. Registration and
`config.get` return the effective value: a previously persisted override when
present, otherwise the descriptor default. Defaults do not create snapshots or
consume storage quota. `config.remove` deletes the override, after which the
property immediately resolves to its default. Defaults have their own 64 MiB
aggregate encoded registration limit to bound host memory without imposing a
per-value layout policy. A stored value from an older version is returned even
if its kind no longer matches the new descriptor; subsequent writes must use
the newly declared kind.

## Immediate updates and persistence

The add-on handles an interaction by mutating its own in-memory state, calling
`config.set` or `config.remove`, and returning a normal `ui-update`. Each
changed value is committed before the persistence call returns. A replacement
frame is validated and becomes the new retained surface and configuration
inventory immediately. `unchanged` retains the current frame. There is no
separate commit or Save event and no arbitrary per-callback write-count limit.

Values are typed as boolean, signed integer, finite number, UTF-8 text, or
bytes. The only logical data allowance is 64 MiB of aggregate encoded entries
per add-on; there is no per-value quota. No ambient filesystem access is given
to the component. The runtime writes checksummed, revisioned files below
`farever-addons/config/` and retains current, pending, and previous revisions
for recovery.

The host reads the add-on's standard WebAssembly component version metadata,
passes it in `activation-context.addon-version`, and stamps every changed
snapshot with it. `config.status` exposes the current revision, saved-by
version, usage, and quota so add-ons do not embed a second version constant.

Farever More's own settings reuse the same revisioned persistence machinery in
the host namespace and are stamped with the runtime package version. This is
separate from the add-on-facing config imports.

The reference Damage Meter always registers a `settings` page. Its editable
`meter-visible` property is restored at activation and updated immediately by a
controlled checkbox; encounter reset remains a button. Wayfinder registers no
page, demonstrating that configuration is optional.

## Live QA

The configuration window's placement and appearance beside Farever's social
buttons still need in-game QA.
