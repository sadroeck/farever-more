# Minimap reference add-on

The minimap is a Rust-authored WebAssembly component. It draws a bundled map
for the W1 area and gets point-of-interest records from the declared
poi-database service.

## Behavior

- The map stays north-up. The player arrow follows the player's heading, while
  a pale cone shows camera direction.
- A north marker sits above the map edge. The frame can be circular or
  rounded-square.
- POIs are filtered by category. Clicking a visible marker publishes a
  waypoint request; the GPS add-on turns that request into an arrow when its
  map-clicks setting is enabled.
- The add-on hides outside W1 or when the player is not in the world.
  EscapeMenu, GameMenu, and LoadingScreen may remain open; another registered
  game window hides the minimap. LoadingScreen is allowed only after the W1
  area and a valid player position are available, so it cannot expose a map
  over the character menu.

## Data and rendering

The add-on owns its world-to-image conversion and map image. It uses the generic
image and canvas features in the add-on API, without a minimap-specific host
interface. See the [asset note](assets/README.md) for provenance and
attribution.

The point-of-interest provider currently serves the bundled W1 dataset. See
[services.md](../../docs/services.md) for the service and [the third-party
notices](../../THIRD_PARTY_NOTICES.md) for data and artwork boundaries.

## Limits and live QA

The map currently covers W1. It has no panning, zoom controls, or party
markers. Clicking a marker requires the GPS add-on to show a waypoint arrow.
The map's appearance and hooks still need in-game QA on each supported build.
