# Minimap reference add-on

The minimap is a Rust-authored WebAssembly component. It draws a bundled map
for the W1 area and gets point-of-interest records from the declared
poi-database service.

## Behavior

- The map stays north-up. A custom bronze-and-parchment player arrow follows
  the player's heading, while a pale cone shows camera direction.
- A north marker sits above the map edge. The frame can be circular or
  rounded-square.
- POIs are filtered by category. Clicking a visible marker publishes a
  waypoint request; the GPS add-on turns that request into an arrow when its
  map-clicks setting is enabled.
- With GPS installed, a soulstone destination uses that stone's inventory
  portrait with a small gold border. Regular destinations use a purple ring.
  Both stay visible regardless of the POI category toggles. Distant destinations sit inside the map edge in
  their direction of travel. The pin disappears on GPS arrival or clearing,
  world/session mismatch, or an unavailable GPS service. `/gps hide` hides the
  arrow while retaining the minimap pin.
- The add-on hides outside W1 or when the player is not in the world.
  EscapeMenu, GameMenu, and LoadingScreen may remain open; another registered
  game window hides the minimap. LoadingScreen is allowed only after the host
  reports the world playable, the W1 area, and a valid player position. This
  permits the map to appear promptly even if the loading window stays
  registered after the player can move.

## Data and rendering

The add-on owns its world-to-image conversion and map image. The player arrow
has a scalable SVG source and a bundled PNG that the host rotates with the
player's heading. Rendering uses the generic image and canvas features in the
add-on API, without a minimap-specific host interface. See the [asset
note](assets/README.md) for provenance and attribution.

The point-of-interest provider currently serves the bundled W1 dataset. See
[services.md](../../docs/services.md) for the service and [the third-party
notices](../../THIRD_PARTY_NOTICES.md) for data and artwork boundaries.

GPS is an optional dependency. The minimap opens its `waypoint` service during
activation and queries the current destination on each tick, using the shared
`farever-waypoint-protocol` codec. Installing GPS later requires reloading the
minimap to open the service handle. No native full-map marker is added yet.
The eight soulstone portraits are bundled game assets; GPS supplies the item ID
so the minimap selects the correct image without matching waypoint labels.

## Limits and live QA

The map currently covers W1. It has no panning, zoom controls, or party
markers. Clicking a marker requires the GPS add-on to show a waypoint arrow.
The soulstone destination marker was confirmed in-game by the user on Steam
build `25632706`. All eight portraits, arrival clearing, and other supported
builds still need individual live checks.

## Support

<a href="https://www.buymeacoffee.com/colorise" target="_blank" rel="noopener noreferrer">
  <img src="https://cdn.buymeacoffee.com/buttons/v2/default-blue.png" alt="Buy Me a Coffee" style="height: 60px !important; width: 217px !important;">
</a>
