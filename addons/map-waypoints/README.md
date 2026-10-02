# Map Waypoints

Shows all eight demon summoning sites as permanent POIs on Farever's full map,
even without a GPS destination. Each site uses its soulstone inventory portrait
inside a compact bronze diamond frame, with the same presentation as the minimap.
Clicking a soulstone in the inventory keeps normal game behavior, sets GPS,
and gives its matching POI an enlarged portrait, a bright gold outline, and a
contrasting circular halo. Clicking the icon through the passive overlay uses
the native map click; GPS resolves it to the same inventory-click name and site
using the current zoom/display scale.

Requires POI Database's `poi` service (`^1`) and add-on API 1.1.0. The add-on
fetches the reviewed `soulstone` records during activation. GPS's `waypoint`
service (`^0.2`) is optional and supplies only the selected-site highlight.
The host supplies a read-only map snapshot with the native clipping rectangle,
world-to-client transform, and overlay display scale. The portrait follows pan,
zoom, and client resizing. Its center is never clamped to a map edge; the
native-map rectangle clips partially visible portraits.

The canvas passes mouse input through to the game. Normal map clicks, drag
panning, and zoom controls continue to work. The marker clears when the map
closes, another window covers it, or the capture expires. A missing or failed
GPS query, arrival, clearing, or stale world/process-session removes only the
highlight; the permanent sites remain visible. POIs must match the map's world.
Ordinary GPS destinations without a soulstone item ID select no demon POI.

Projection, clipping, and callback isolation have existing offline coverage.
The permanent-POI changes pass source tests and compiled host-boundary tests,
including all eight portraits, selection, and arrival clearing only the
highlight. Installed map roots and artwork match Steam build `25658350`.
The user confirmed the map overlay works on that build and, on 2026-10-02,
confirmed the stronger target highlight and correct demon waypoint name.
Scrolling was normal with the full add-on set and without Farever-More.
Individual live checks of all eight sites and arrival remain outstanding.

Artwork is subject to [the third-party notices](../../THIRD_PARTY_NOTICES.md).
