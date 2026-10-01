# Minimap assets

The W1 map image and bundled POI atlas artwork are derived from Farever in-game assets by [Shiro Games](https://store.steampowered.com/app/3672400/Farever/). They are included solely for use within the Farever client and are not covered by Farever More's MIT source license.

`player_marker.svg` is the custom scalable source for the player arrow;
`player_marker.png` is its pre-rendered runtime asset. The host image API accepts
PNG, then rotates the registered image to follow the player's heading.

The custom `poi_custom_atlas.png` was created for Farever More. See [third-party notices](../../../THIRD_PARTY_NOTICES.md).

`soulstones/*.png` are the eight unmodified 256x256 inventory portraits from
Farever Steam build `25632706`, decoded from the top mip of the DDS BC7 textures
stored under `.png` resource names in `res.pak`. Source paths live in the curated
`farever-db/assets/soulstones.rs` table. These are Shiro Games assets under the
same Farever-only use and licensing boundary as the map. They retain the source
colors and transparency; the minimap scales them when drawing a destination.

Import or verify the portraits and generated item-to-image mapping with:

```powershell
cargo run -p farever-db --features portrait-import --bin soulstones -- --icons addons/minimap/assets/soulstones
cargo run -p farever-db --features portrait-import --bin soulstones -- --icons addons/minimap/assets/soulstones --check
```

The importer verifies every reviewed `item.gfx` definition against the installed
CastleDB, accepts only the reviewed 2D BC7 portrait format, and reads no other
sheet. It is also part of `generate-game-data.ps1`; the standalone command works
even when unrelated extracted tables need updating. PNG/BC7 decoding dependencies
are enabled only for this offline importer, not the host or add-on components.
