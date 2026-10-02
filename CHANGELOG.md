# Changelog

## Unreleased

## 0.3.0 - Map API and soulstone navigation

- Make selected demon POIs more prominent with a larger portrait and a bright
  gold outline/halo. Clicking their map icons reuses the inventory-click name,
  exact destination and selection identity instead of naming them "Waypoint".
- Left-clicking any of the eight soulstones in the backpack preserves its
  normal behavior and replaces the GPS destination with its fixed W1 summoning
  site. Creating a destination also shows a previously hidden GPS arrow.
- Add all eight demon summoning sites as permanent POIs on the minimap and full
  map, using inventory portraits in matching bronze diamond frames. Soulstone
  clicks highlight the matching site in gold; arrival clears the highlight.
- Share the active destination through GPS's read-only waypoint service, with
  no change to the WIT add-on API for GPS/minimap navigation.
- Add Map Waypoints, which reuses the eight extracted inventory portraits on
  the native full map. It follows pan and zoom, clips to the map rectangle,
  passes mouse input through, and keeps permanent POIs after GPS clears.
- Add API 1.1.0's read-only map snapshot and separate passive-canvas options
  interface, retaining compatibility with API 1.0.0 components.

Soulstone click navigation, the GPS arrow, and the minimap marker were confirmed
in-game by the user on Steam build `25632706`.
The user confirmed the full-map overlay works on `25658350` and, on 2026-10-02,
confirmed the stronger target highlight and correct demon waypoint name.
Permanent demon POIs and the shared icon styling also pass source and compiled
host-boundary tests. Individual live checks of all eight sites remain pending.

The user validated the map implementation for release on 2026-10-02.
The full bundle ships host/API `1.1.0` and all five rebuilt add-ons together;
API `1.0.0` components remain compatible with the newer runtime.
Bundled versions: Dyno `0.1.2`, GPS `0.2.0`, Minimap `0.1.2`,
Map Waypoints `0.1.0`, and POI Database `1.3.1`.

## 0.2.5 - Restore DPS meter icons

- Restore skill icons when Farever packages `.png` atlas paths as DDS/BC7
  textures. Select the decoder by payload signature and retain PNG support.
- Validate single 2D BC7 textures, top-mip bounds, dimensions, and decoded
  memory limits before allocating and decoding.

All 365 unique skill crops across 53 installed atlases passed offline decoding
checks. The user confirmed the restored DPS meter icons in-game on 2026-10-02.
The WIT add-on API remains `1.0.0`; bundled add-on versions are unchanged.

## 0.2.4 — Startup metadata and Farever compatibility

- Recognize Farever Steam build `25658350` by its reviewed executable,
  bytecode, and HashLink runtime fingerprints.
- Read metadata names up to their terminator within page boundaries, preventing
  valid names near an unreadable page from being rejected during startup.
- Report the failing stage when a complete game-object shape cannot be read.

The protected-page regression and installed-bytecode checks pass. A live launch
recognized the exact profile and loaded all four add-ons with none disabled.
Repeated cold starts remain necessary to verify the intermittent startup stall.
The WIT add-on API remains `1.0.0`; bundled add-on versions are unchanged.

## 0.2.3

- Support version 25632706 of farever
- Check bytecode signatures before loading add-ons on unknown game versions
  with the reviewed native executable and HashLink runtime; known fingerprints
  keep the fast path.
- Show the specific compatibility mismatch when loading is refused.

## 0.2.2

- Support version 25628371 of farever

## 0.2.1 — Bundled installation

- Ship a `-full.zip` with the framework and all four add-ons already arranged
  for installation beside `Farever.exe`.
- Exclude the unfinished standalone manager from builds and release packages.
- Include concise installation instructions in the README and release notes,
  with the custom add-on folder layout and Minimap's POI Database dependency.

Bundled add-ons: Dyno, GPS, and Minimap `0.1.1`; POI Database `1.3.0`.
The WIT add-on API is `1.0.0`.

## 0.2.0 — Full-map GPS fixes

- Expose `in-world` only after loading reaches the playable state and clear
  overlays on logout.
- Attribute native chat output to the sending add-on.
- Capture full-map world coordinates and beta activity-marker selections for
  Farever More GPS, preserving the game's native HUD selector and map dragging.
- Synchronize the generated SDK bindings with the playable-world documentation.
- Ship GPS `0.1.1`: full-map clicks honor "Arrows from the map", explicit `/gps`
  commands keep working, and the default arrow sits 54 logical points lower.
- Ship Dyno `0.1.1` with `/dyno` clear, reset, and visibility commands while
  preserving the `/dps` aliases.
- Ship Minimap `0.1.1` with its custom player marker artwork.
- Package the native runtime without the unfinished standalone manager and
  include manual installation instructions with the add-on folder layout.
- Offer a `-full.zip` with the runtime and all four add-ons ready to extract
  into the game folder.

Full-map targeting and the lower arrow placement were validated in-game on
Farever beta Steam profile `25531577`. The WIT add-on API remains `1.0.0`.

## 0.1.0 — Initial public source release

Farever More 0.1.0 is an experimental Windows x86-64 add-on host for the Steam
version of Farever. This first source release includes a desktop manager and
installer, a Rust SDK, four reference add-ons, and tools for inspecting game
data.

The host recognizes exact Steam file profiles for stable build 25257040 and
beta build 25531577. Beta hook behavior still needs live gameplay QA.

Linux and Proton are not supported in this release. Linux support is planned for
a future release.
