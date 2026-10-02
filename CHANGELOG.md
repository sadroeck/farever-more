# Changelog

## Unreleased

- Add API 1.1.0's read-only map snapshot and separate passive-canvas options
  interface, retaining compatibility with API 1.0.0 components.
- Left-clicking any of the eight soulstones in the backpack preserves its
  normal behavior and replaces the GPS destination with its fixed W1 summoning
  site. Creating a destination also shows a previously hidden GPS arrow.
- Show the active soulstone's inventory portrait on the minimap and clear it
  through GPS's existing arrival behavior.
- Share the active destination through GPS's read-only waypoint service, with
  no change to the WIT add-on API. Native full-map markers remain pending.

Soulstone click navigation, the GPS arrow, and the minimap marker were confirmed
in-game by the user on Steam build `25632706`.

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
