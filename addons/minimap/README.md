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
- All eight demon summoning sites are permanent POIs in the nearby map view,
  enabled by default under "Demon summoning sites". Their inventory portraits
  sit in compact bronze diamond frames shared with the full-map add-on.
  An inventory soulstone click keeps normal game behavior, sets GPS, and adds
  a larger portrait, bright gold outline, and contrasting circular halo to its
  matching POI. Clicking a demon POI selects the same named destination as its
  inventory soulstone. Arrival, clearing, world/session mismatch,
  or an unavailable GPS service removes the highlight and leaves the POI.
  The category and master POI toggles also apply to demon sites. Locations
  stay at their true coordinates; distant demon sites are outside the view.
- Regular GPS destinations use a purple ring, regardless of POI toggles, and
  distant destinations sit inside the map edge in their direction of travel.
  `/gps hide` hides the arrow while retaining the current marker or highlight.
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
minimap to open the service handle. The separate [Map Waypoints](../map-waypoints/README.md)
add-on draws all eight permanent demon POIs on the game's full map.
The eight soulstone portraits are bundled game assets. The POI database uses
the required soulstone item ID as each demon site's stable ID; GPS supplies
the selected item ID, so highlights never depend on translated labels.

## Limits and live QA

The map currently covers W1. It has no panning, zoom controls, or party
markers. Clicking a marker requires the GPS add-on to show a waypoint arrow.
The soulstone destination marker was confirmed in-game by the user on Steam
build `25632706`. All eight portraits, arrival clearing, and other supported
builds still need individual live checks. The permanent POIs and shared diamond
styling pass source and compiled host-boundary tests, including all eight
portraits and arrival preserving the POI. On 2026-10-02, the user confirmed
the stronger selected-site highlight and correct demon waypoint name on
`25658350`; individual live checks of all eight sites remain outstanding.

## Support

<a href="https://www.buymeacoffee.com/colorise" target="_blank" rel="noopener noreferrer">
  <img src="https://cdn.buymeacoffee.com/buttons/v2/default-blue.png" alt="Buy Me a Coffee" style="height: 60px !important; width: 217px !important;">
</a>
