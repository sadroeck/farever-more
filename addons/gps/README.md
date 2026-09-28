# GPS waypoint arrow

## Waypoint behavior

The reference add-on owns target selection and direction rendering. The host
continues to expose independent Farever `target`, `lockedTarget`, and
`autoTarget` observations without assigning priority.

The add-on starts without a target or visible surface. A `/gps` command can
create an ordered, comma-separated sequence of add-on-owned waypoints. Names
may be double-quoted to include commas, with doubled quotes representing a
literal quote. All entries capture the player's current Z coordinate because
the public syntax is intentionally limited to horizontal same-world
coordinates. `/gps hide` and `/gps show` toggle only the surface and preserve
the queue; setting a sequence while hidden does not make it visible. The add-on
clears the sequence after leaving the world or starting a new process session.
Replacing this command-owned source with an add-on-owned combat-reference
policy remains a later step.

## Map clicks

Clicking open space on Farever's full map sets a GPS waypoint at the game's
converted world coordinates. On beta `25531577`, clicking an activity marker
also sets a waypoint when Farever opens its built-in HUD selector. The host
preserves the native selector and map dragging behavior; drags do not set GPS
waypoints. Full-map clicks arrive on `farever.map-click@1` from `farever.host`,
with one `x y` waypoint and the default name "Waypoint".

The host forwards the player's own `/gps` commands on `gps`, which always apply.
Other add-ons may publish a waypoint request on `gps` with the same syntax as a
waypoint sequence (`x y "name"`); the minimap publishes one when a POI marker is
pressed. Full-map clicks and peer requests are gated by this add-on's `map-clicks` setting
(config page "GPS", default on), so the choice belongs to the add-on whose UI is
affected rather than to the sender, and a peer cannot pass itself off as the
player's command. With the setting off, `/gps` commands keep working and map
requests are answered with a chat line naming the setting instead of being
dropped in silence. A click is also reported when the arrow is hidden: the
waypoint is set, and the player is told to run `/gps show`. Requests take the
same path as commands: an unavailable player position or a session outside the
world is reported as an ignored request instead of failing the callback.

It subtracts `camera.heading-radians` from the horizontal world-space bearing
between player and selected reference, wraps the result to `(-pi, pi]`, and
rotates a CPU-projected mesh. Matching the camera heading points straight up.
The vertical angle is `atan2(dz, max(horizontal-distance, 1))`; it bends the
arrow mesh upward or downward by a small amount without changing horizontal
target selection. The current waypoint is removed once horizontal ground
distance is strictly below 3 meters, even if the surface is hidden or camera
heading is temporarily unavailable, and rendering advances immediately to the
next queued waypoint. `/gps next` performs the same transition explicitly.
Cross-world routing, interaction, dragging, and persistence are intentionally
absent.

The bearing and screen-space handedness follow the navigation arrow of
farever-minimap's example `nav_arrow.lua` plug-in:
its `atan2(right, forward)` local-frame calculation is equivalent to subtracting
heading from the world bearing, and a positive relative angle points toward
screen-right. This add-on intentionally substitutes camera heading for player
heading. Its altitude tilt is retained, but scaled to fit the compact 104-point
surface; animation and waypoint controls remain omitted.

The arrow uses a 104-point-high canvas inside a compact 176-point-wide,
fully-transparent surface. Its shape is assembled from a shadow, an extruded
underside, beveled edges, and raised center ridge represented by an embedded
25-vertex, 43-triangle mesh. Each update applies altitude pitch, a fixed oblique
view, perspective projection, back-face culling, directional lighting, and
depth sorting on the add-on CPU. The result is submitted through the existing
canvas as ordinary filled triangles, so this experiment does not add a host
mesh or GPU API. No independent projected edge lines are drawn; face lighting
and occlusion provide the depth cues. There is no panel fill or box outline.

The surface uses the viewport's top-center anchor with a 106 logical-point top
margin. This lowers it by 54 logical points from the previous default, roughly
5% of a 1,080-point-high viewport, placing the arrow farther below the centered
area-name banner without a resolution-dependent horizontal offset. The target
name is centered
immediately below the arrow, with the horizontal ground distance centered on
the next line. Distances below 10
meters retain one decimal place and longer distances use whole meters; the
GPS-style copy is `<distance>m away`, with no travel-time estimate. Both
lines use the same embedded Noto Sans Regular/Bold faces as the reference
damage meter, matching Farever's normal UI family, with a one-point near-black
outline matching Farever nameplate text.

The projected silhouette is
re-centered inside its canvas after camera projection, then bottom-aligned
above a compact two-point vertical stack. This keeps the arrow, name, and
distance grouped like GPS's Crazy Arrow layout even while the asymmetric
mesh rotates. Farever More's overlay remains
globally click-through. The surface is omitted before a sequence is set, after
the sequence is exhausted, while hidden, or whenever current player position
or camera heading is unavailable.

Build the development components with `scripts/build-wasm-addons.ps1`. The
managed `scripts/farever-addons.ps1 Install` and `Update` flows include both
reference components unless `-SkipReferenceAddon` is passed.

## Live QA

Waypoint direction still needs an in-game check because the camera and player
heading may use different zero directions.
Full-map target selection and the lower default arrow placement were confirmed
in user beta testing on Steam profile `25531577`.

## Support

<a href="https://www.buymeacoffee.com/colorise" target="_blank" rel="noopener noreferrer">
  <img src="https://cdn.buymeacoffee.com/buttons/v2/default-blue.png" alt="Buy Me a Coffee" style="height: 60px !important; width: 217px !important;">
</a>
