# Farever game data

The generated tables in this directory are derived from Farever in-game data. Most are extracted from the installed game by the `farever-db` tools; `build_source.rs` records the source build fingerprint. Regenerate the extracted tables from a supported Farever installation with [generate-game-data.ps1](../../scripts/generate-game-data.ps1). Do not edit the generated files by hand.

`placements.rs` is imported from a W1_Siagarta placement census; its source details are recorded in the file header. The placement records are also derived from Farever world data.

Farever is developed and published by [Shiro Games](https://store.steampowered.com/app/3672400/Farever/). The data is included solely for use within the Farever client and is not covered by Farever More's MIT source license. See [third-party notices](../../THIRD_PARTY_NOTICES.md).
