# Changelog

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
