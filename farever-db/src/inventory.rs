//! The game inventory embedded in this crate.
//!
//! Every table this crate itemizes lives here, generated into checked-in Rust
//! source under `assets/` and read back through [`Inventory`]. Two halves meet:
//!
//! * the **generation** half ([`validate`], [`render`]) reads the `CastleDB`
//!   document that [`GameInstall::load_cdb`] returns and refuses anything the
//!   whitelist below does not describe, then emits one artifact per table;
//! * the **embedded** half ([`Inventory`]) is those artifacts, compiled in. A
//!   consumer reads typed records and never needs the game installation.
//!
//! # Provenance
//!
//! Generated tables record where they came from: [`BuildSource`] for everything
//! extracted from the installed game build, so a stale artifact is detectable.
//!
//! # Building without the tables
//!
//! The embedded half is behind the default `embedded` feature. Turning it off
//! leaves out every `include!` above, so `extract-db` still builds when a table
//! has gone stale or is missing entirely - which is the state it exists to
//! repair, and one it cannot repair from a package the tables prevent from
//! compiling.
//!
//! # Adding a table
//!
//! A table is added deliberately, in one place: a [`SheetSpec`] per sheet path
//! (including its `sheet@group` sub-paths), a record type with the fields
//! consumers need, an artifact name in [`files`], a line in [`render`], and an
//! accessor on [`Inventory`]. The whitelist is the contract with the game build:
//! a column that is not listed fails extraction instead of being ignored, and a
//! renderer reads its sheet through `whitelisted_rows`, so a table cannot be
//! generated from a sheet no spec describes.
//!
//! [`GameInstall::load_cdb`]: crate::cdb::GameInstall::load_cdb

use serde_json::{Map, Value};
use std::collections::HashMap;
use thiserror::Error;

use crate::cdb::BuildFingerprint;

/// File names the extractor writes into the crate's `assets/` folder.
pub mod files {
    /// The game build the `data.cdb` tables below were extracted from.
    pub const BUILD_SOURCE: &str = "build_source.rs";
    /// The `gatherable` sheet.
    pub const GATHERABLES: &str = "gatherables.rs";
    /// The `zone` sheet.
    pub const ZONES: &str = "zones.rs";
    /// The world placements imported from a released map census.
    pub const PLACEMENTS: &str = "placements.rs";
    /// The `skill` sheet.
    pub const SKILLS: &str = "skills.rs";
    /// The `icon` sheet.
    pub const ICONS: &str = "icons.rs";
    /// The `item` sheet.
    pub const ITEMS: &str = "items.rs";
    /// The `itemType` sheet.
    pub const ITEM_TYPES: &str = "item_types.rs";
    /// The `aptitude` sheet.
    pub const APTITUDES: &str = "aptitudes.rs";
    /// The `rarity` sheet.
    pub const RARITIES: &str = "rarities.rs";
    /// The `lootTable` sheet.
    pub const LOOT_TABLES: &str = "loot_tables.rs";
    /// The `unit` sheet.
    pub const UNITS: &str = "units.rs";
    /// The `unitType` sheet.
    pub const UNIT_TYPES: &str = "unit_types.rs";
    /// The `craft` sheet.
    pub const CRAFTS: &str = "crafts.rs";
    /// The `ach` sheet.
    pub const ACHIEVEMENTS: &str = "achievements.rs";
}

/// The game build a set of extracted tables came from.
#[derive(Clone, Debug, PartialEq)]
pub struct BuildSource {
    pub steam_build_id: Option<&'static str>,
    pub cdb_checksum: u32,
    pub cdb_size: u64,
    pub sheets: usize,
}

impl BuildSource {
    /// The source recorded in an artifact that [`render`] is about to produce.
    ///
    /// The build id is written into the artifact as a literal, so it must
    /// already be static: the extractor leaks the one string it reads.
    #[must_use]
    pub fn new(
        steam_build_id: Option<&'static str>,
        fingerprint: &BuildFingerprint,
        sheets: usize,
    ) -> Self {
        Self {
            steam_build_id,
            cdb_checksum: fingerprint.cdb_checksum,
            cdb_size: fingerprint.cdb_size,
            sheets,
        }
    }
}

/// How a gatherable relates to the ore/plant families the game defines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatherableKind {
    Ore,
    Plant,
    /// A gatherable whose inheritance chain names neither family.
    Other,
}

/// One `gatherable` row: what a gather node is, and how long it takes to return.
#[derive(Clone, Debug, PartialEq)]
pub struct Gatherable {
    pub id: &'static str,
    pub kind: GatherableKind,
    pub inherit: Option<&'static str>,
    pub name: Option<&'static str>,
    pub model: Option<&'static str>,
    pub loot: Option<&'static str>,
    /// The table a successful gathering hit draws from.
    pub hit_loot: Option<&'static str>,
    pub required_tool: Option<&'static str>,
    pub size: Option<i64>,
    pub hit_points: Option<i64>,
    pub respawn_time: Option<i64>,
}

/// One `zone` row: the authored area tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Zone {
    pub id: &'static str,
    pub parent: Option<&'static str>,
    pub zone_type: i64,
    pub level: i64,
    pub color: i64,
    pub name: Option<&'static str>,
    pub desc: Option<&'static str>,
    pub map_illustration: Option<&'static str>,
    pub release_allowed: Option<bool>,
}

/// One world placement: a thing the shipped map puts at a position.
///
/// `data.cdb` holds no placements, so these records are imported from a released
/// census of the map archive instead of extracted from the install.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    /// Id the release assigned, when it assigned one.
    pub id: Option<&'static str>,
    /// Placement kind, e.g. `plant`, `chest`, `activity`.
    pub kind: &'static str,
    /// Kind refinement, e.g. `WorldElite`; only activities carry one.
    pub family: Option<&'static str>,
    /// Prefab basename, e.g. `Madrigold_Small`, or an authored name.
    pub name: &'static str,
    /// Prefab the placement instantiates, as the census records it.
    pub prefab: &'static str,
    /// Map tile the placement was found in, e.g. `L0_+0_+11`.
    pub tile: &'static str,
    pub x: f32,
    pub y: f32,
    pub z: Option<f32>,
}

/// Where a table that did not come from the installed build came from.
#[derive(Clone, Debug, PartialEq)]
pub struct ReleaseSource {
    /// Label of the release the records were imported from.
    pub name: &'static str,
    pub records: usize,
}

/// One `skill` row: the public name and the atlas crop of its icon.
#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub id: &'static str,
    /// Authored display name, with `CastleDB` skill references already resolved
    /// to the value they name.
    pub name: Option<&'static str>,
    /// Where the icon lives, at pixel precision.
    pub icon: Option<IconCrop>,
}

/// One `icon` row: an interface glyph the game binds to ids across its data.
#[derive(Clone, Debug, PartialEq)]
pub struct Icon {
    pub id: &'static str,
    /// Authored display name, which other sheets reference by id.
    pub name: Option<&'static str>,
    /// The glyph's `gfx` crop, at pixel precision.
    pub gfx: Option<IconCrop>,
}

/// Pixel-space crop of one icon inside a packaged atlas (PNG or compiled DDS).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct IconCrop {
    /// Atlas path inside the game's `res.pak`.
    pub atlas_path: &'static str,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One `item` row: a catalogue entry consumers search and resolve.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: &'static str,
    /// Authored display name, falling back to the id when the game has none.
    pub name: &'static str,
    pub item_type: Option<&'static str>,
    pub rarity: Option<&'static str>,
    pub faction: Option<&'static str>,
    /// Aptitudes the item declares, as its sheet references them. What they
    /// mean to a rule is the rule's business, not the table's.
    pub aptitudes: &'static [&'static str],
}

/// One `itemType` row: the item family tree.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemType {
    pub id: &'static str,
    pub inherit: Option<&'static str>,
}

/// One `aptitude` row: a requirement an item can declare.
#[derive(Clone, Debug, PartialEq)]
pub struct Aptitude {
    pub id: &'static str,
    /// The aptitude's authored flag bits.
    pub flags: i64,
}

/// One `rarity` row: the tier and how it is drawn as loot level rises.
#[derive(Clone, Debug, PartialEq)]
pub struct Rarity {
    pub id: &'static str,
    /// Level brackets, in the order the game declares them.
    pub brackets: &'static [RarityBracket],
}

/// One rarity level bracket: the chance of drawing this tier at a level.
#[derive(Clone, Debug, PartialEq)]
pub struct RarityBracket {
    pub min_level: i64,
    pub max_level: i64,
    pub chance: f64,
}

/// One `lootTable` row: what a source can drop.
#[derive(Clone, Debug, PartialEq)]
pub struct LootTable {
    pub id: &'static str,
    /// The table's authored flag bits; what each bit means is the reader's rule.
    pub flags: i64,
    pub entries: &'static [LootEntry],
}

/// One entry of a loot table: an item, or another table to fall through to.
#[derive(Clone, Debug, PartialEq)]
pub struct LootEntry {
    /// Chance of this entry, or its weight when the table is weighted.
    pub probability: f64,
    pub item: Option<&'static str>,
    pub loot_table: Option<&'static str>,
    pub item_min: Option<i64>,
    pub item_max: Option<i64>,
    pub min_level: Option<i64>,
    pub max_level: Option<i64>,
    /// Condition bits a source must satisfy for this entry to be drawn.
    pub conditions: Option<i64>,
    pub flags: i64,
}

/// One `unit` row: an enemy or boss the world places.
#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    pub id: &'static str,
    pub name: Option<&'static str>,
    pub faction: Option<&'static str>,
    pub loot_table: Option<&'static str>,
    /// The signature table the game reserves for defeating this unit as a boss.
    pub boss_loot_table: Option<&'static str>,
}

/// One `unitType` row: an enemy family that can drop.
#[derive(Clone, Debug, PartialEq)]
pub struct UnitType {
    pub id: &'static str,
    pub name: Option<&'static str>,
    pub loot_table: Option<&'static str>,
}

/// One `craft` row: a recipe that yields an item.
///
/// Recipes carry no id; the sheet is an ordered list, and a recipe is found by
/// the item it yields.
#[derive(Clone, Debug, PartialEq)]
pub struct Craft {
    pub item: Option<&'static str>,
    pub job: Option<&'static str>,
    pub level: i64,
    pub ingredients: &'static [Ingredient],
}

/// One ingredient of a recipe.
#[derive(Clone, Debug, PartialEq)]
pub struct Ingredient {
    pub item: Option<&'static str>,
    pub count: i64,
}

/// One `ach` row: an achievement that rewards items.
#[derive(Clone, Debug, PartialEq)]
pub struct Achievement {
    pub id: &'static str,
    pub name: Option<&'static str>,
    pub desc: Option<&'static str>,
    /// Items the achievement rewards, as its `reward.items` names them.
    pub rewards: &'static [&'static str],
}

#[cfg(feature = "embedded")]
mod build_source {
    include!("../assets/build_source.rs");
}

#[cfg(feature = "embedded")]
mod gatherables {
    include!("../assets/gatherables.rs");
}

#[cfg(feature = "embedded")]
mod zones {
    include!("../assets/zones.rs");
}

#[cfg(feature = "embedded")]
mod placements {
    include!("../assets/placements.rs");
}

#[cfg(feature = "embedded")]
mod skills {
    include!("../assets/skills.rs");
}

#[cfg(feature = "embedded")]
mod icons {
    include!("../assets/icons.rs");
}

#[cfg(feature = "embedded")]
mod items {
    include!("../assets/items.rs");
}

#[cfg(feature = "embedded")]
mod item_types {
    include!("../assets/item_types.rs");
}

#[cfg(feature = "embedded")]
mod aptitudes {
    include!("../assets/aptitudes.rs");
}

#[cfg(feature = "embedded")]
mod rarities {
    include!("../assets/rarities.rs");
}

#[cfg(feature = "embedded")]
mod loot_tables {
    include!("../assets/loot_tables.rs");
}

#[cfg(feature = "embedded")]
mod units {
    include!("../assets/units.rs");
}

#[cfg(feature = "embedded")]
mod unit_types {
    include!("../assets/unit_types.rs");
}

#[cfg(feature = "embedded")]
mod crafts {
    include!("../assets/crafts.rs");
}

#[cfg(feature = "embedded")]
mod achievements {
    include!("../assets/achievements.rs");
}

/// Everything this crate itemizes about the game, compiled in.
///
/// Reads nothing from disk: every record below was generated from a verified
/// source and is part of this crate's binary.
pub struct Inventory;

#[cfg(feature = "embedded")]
impl Inventory {
    /// Which game build the extracted tables came from.
    #[must_use]
    pub fn build() -> &'static BuildSource {
        &build_source::BUILD
    }

    /// Every gather node the game defines, in `data.cdb` order.
    #[must_use]
    pub fn gatherables() -> &'static [Gatherable] {
        gatherables::RECORDS
    }

    /// Every zone the game defines, in `data.cdb` order.
    #[must_use]
    pub fn zones() -> &'static [Zone] {
        zones::RECORDS
    }

    /// Every placement the imported census records, in census order.
    #[must_use]
    pub fn placements() -> &'static [Placement] {
        placements::RECORDS
    }

    /// The world every record in [`Self::placements`] belongs to, e.g.
    /// `World/W1_Siagarta`.
    #[must_use]
    pub fn placement_world() -> &'static str {
        placements::WORLD
    }

    /// The release [`Self::placements`] was imported from.
    #[must_use]
    pub fn placement_release() -> &'static ReleaseSource {
        &placements::RELEASE
    }

    /// Every skill the game defines, sorted by id.
    #[must_use]
    pub fn skills() -> &'static [Skill] {
        skills::RECORDS
    }

    /// One skill by exact internal id, in `O(log n)`.
    #[must_use]
    pub fn skill(id: &str) -> Option<&'static Skill> {
        let skills = Self::skills();
        skills
            .binary_search_by_key(&id, |skill| skill.id)
            .ok()
            .map(|index| &skills[index])
    }

    /// Every interface glyph the game defines, sorted by id.
    #[must_use]
    pub fn icons() -> &'static [Icon] {
        icons::RECORDS
    }

    /// One icon by exact internal id, in `O(log n)`.
    #[must_use]
    pub fn icon(id: &str) -> Option<&'static Icon> {
        let icons = Self::icons();
        icons
            .binary_search_by_key(&id, |icon| icon.id)
            .ok()
            .map(|index| &icons[index])
    }

    /// Every item the game defines, sorted by id.
    #[must_use]
    pub fn items() -> &'static [Item] {
        items::RECORDS
    }

    /// One item by exact internal id, in `O(log n)`.
    #[must_use]
    pub fn item(id: &str) -> Option<&'static Item> {
        lookup(Self::items(), id, |item| item.id)
    }

    /// Every item family the game defines, sorted by id.
    #[must_use]
    pub fn item_types() -> &'static [ItemType] {
        item_types::RECORDS
    }

    /// One item family by exact internal id, in `O(log n)`.
    #[must_use]
    pub fn item_type(id: &str) -> Option<&'static ItemType> {
        lookup(Self::item_types(), id, |item_type| item_type.id)
    }

    /// Every aptitude the game defines, sorted by id.
    #[must_use]
    pub fn aptitudes() -> &'static [Aptitude] {
        aptitudes::RECORDS
    }

    /// One aptitude by exact internal id, in `O(log n)`.
    #[must_use]
    pub fn aptitude(id: &str) -> Option<&'static Aptitude> {
        lookup(Self::aptitudes(), id, |aptitude| aptitude.id)
    }

    /// Every rarity the game defines, sorted by id.
    #[must_use]
    pub fn rarities() -> &'static [Rarity] {
        rarities::RECORDS
    }

    /// One rarity by exact internal id, in `O(log n)`.
    #[must_use]
    pub fn rarity(id: &str) -> Option<&'static Rarity> {
        lookup(Self::rarities(), id, |rarity| rarity.id)
    }

    /// Every loot table the game defines, sorted by id.
    #[must_use]
    pub fn loot_tables() -> &'static [LootTable] {
        loot_tables::RECORDS
    }

    /// One loot table by exact internal id, in `O(log n)`.
    #[must_use]
    pub fn loot_table(id: &str) -> Option<&'static LootTable> {
        lookup(Self::loot_tables(), id, |table| table.id)
    }

    /// Every unit the game defines, sorted by id.
    #[must_use]
    pub fn units() -> &'static [Unit] {
        units::RECORDS
    }

    /// One unit by exact internal id, in `O(log n)`.
    #[must_use]
    pub fn unit(id: &str) -> Option<&'static Unit> {
        lookup(Self::units(), id, |unit| unit.id)
    }

    /// Every enemy family the game defines, sorted by id.
    #[must_use]
    pub fn unit_types() -> &'static [UnitType] {
        unit_types::RECORDS
    }

    /// One enemy family by exact internal id, in `O(log n)`.
    #[must_use]
    pub fn unit_type(id: &str) -> Option<&'static UnitType> {
        lookup(Self::unit_types(), id, |unit_type| unit_type.id)
    }

    /// Every recipe the game defines, in sheet order.
    #[must_use]
    pub fn crafts() -> &'static [Craft] {
        crafts::RECORDS
    }

    /// Every achievement the game defines, sorted by id.
    #[must_use]
    pub fn achievements() -> &'static [Achievement] {
        achievements::RECORDS
    }

    /// One achievement by exact internal id, in `O(log n)`.
    #[must_use]
    pub fn achievement(id: &str) -> Option<&'static Achievement> {
        lookup(Self::achievements(), id, |achievement| achievement.id)
    }
}

/// Finds one record by exact id in a table the extractor sorted by id.
#[cfg(feature = "embedded")]
fn lookup<T>(records: &'static [T], id: &str, key: impl Fn(&T) -> &str) -> Option<&'static T> {
    records
        .binary_search_by_key(&id, |record| key(record))
        .ok()
        .map(|index| &records[index])
}

/// One generated artifact: the file name under `assets/`, and its contents.
#[derive(Clone, Debug, PartialEq)]
pub struct Artifact {
    pub file_name: &'static str,
    pub contents: String,
}

/// One sheet as the game declares it.
///
/// This is the view [`WHITELIST`] is written against, so maintaining the
/// whitelist can be done by reading the game instead of guessing at it.
#[derive(Clone, Debug, PartialEq)]
pub struct SheetDeclaration {
    /// Sheet path, e.g. `gatherable` or `gatherable@props`.
    pub path: String,
    /// Column names in declaration order. Used for both the whitelist contract and
    /// the listing above.
    pub columns: Vec<String>,
    /// Number of rows the sheet carries.
    pub rows: usize,
    /// Keys rows carry that the declaration does not list, sorted. These are
    /// what a [`SheetSpec`] has to name in `tolerated` to accept the data.
    pub undeclared_keys: Vec<String>,
}

/// Lists every sheet the document declares, in document order.
///
/// # Errors
///
/// Fails when the document has no sheets array.
pub fn declarations(document: &Value) -> Result<Vec<SheetDeclaration>, InventoryError> {
    Ok(sheets(document)?
        .iter()
        .map(|sheet| {
            let columns = declared_columns(sheet);
            let mut undeclared: Vec<String> = sheet
                .get("lines")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_object)
                .flat_map(|object| object.keys())
                .filter(|key| !columns.contains(key))
                .cloned()
                .collect();
            undeclared.sort_unstable();
            undeclared.dedup();
            SheetDeclaration {
                path: sheet
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                columns,
                rows: sheet
                    .get("lines")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len),
                undeclared_keys: undeclared,
            }
        })
        .collect())
}

/// One sheet path and the columns the extractor understands for it.
///
/// The path is `CastleDB`'s own naming: a group column `props` on sheet
/// `gatherable` is the sheet path `gatherable@props`.
pub struct SheetSpec {
    pub path: &'static str,
    /// Column names exactly as the game declares them. A column present in the
    /// document but absent here fails extraction.
    pub columns: &'static [&'static str],
    /// Keys rows carry at this path although the declaration puts them on a
    /// nested group, or on the group's parent. `CastleDB`'s exporter is not
    /// consistent about where a group's own columns land, so the outliers are
    /// listed here: tolerated, never read.
    pub tolerated: &'static [&'static str],
}

/// Keys `CastleDB` writes into rows on every sheet for its own bookkeeping.
///
/// `__ignoreLoc__` marks a cell that its localisation pass must leave alone; it
/// is not part of any schema. Tolerated on every sheet, never read.
pub const BOOKKEEPING_KEYS: &[&str] = &["__ignoreLoc__"];

/// Sheets whose rows are an ordered list rather than definitions keyed by id.
///
/// A recipe has no id: it is found by the item it yields, and its position is
/// data. Every other sheet is addressed by id, and a row without one defines
/// nothing.
pub const UNKEYED_SHEETS: &[&str] = &["craft"];

/// Sheets that carry rows, and the sub-sheets whose columns define them.
///
/// Deliberately explicit: this list is the only thing the extractor reads, and
/// growing it is a reviewed change.
pub const WHITELIST: &[SheetSpec] = &[
    SheetSpec {
        path: "gatherable",
        columns: &[
            "id",
            "inherit",
            "requiredTool",
            "props",
            "texts",
            "model",
            "loot",
            "hitLoot",
            "flags",
        ],
        tolerated: &[],
    },
    SheetSpec {
        path: "gatherable@props",
        columns: &[
            "hitPoints",
            "respawnTime",
            "hitFX",
            "affixes",
            "size",
            "affixChance",
            "gatherSkill",
        ],
        tolerated: &[],
    },
    SheetSpec {
        path: "gatherable@props@affixes",
        columns: &["status", "fxSet", "chance"],
        tolerated: &[],
    },
    SheetSpec {
        path: "gatherable@texts",
        columns: &["name", "desc", "type"],
        tolerated: &[],
    },
    SheetSpec {
        path: "zone",
        columns: &["id", "parent", "type", "level", "color", "texts", "props"],
        tolerated: &[],
    },
    SheetSpec {
        path: "zone@texts",
        columns: &["name", "desc"],
        tolerated: &[],
    },
    SheetSpec {
        // `zone.props.ambient` (a scalar) sits beside the `ambients` group on
        // some rows, and `zone.props.ambients` rows carry a nested `ambients`
        // beside their own `ambient`. Both spellings are authored data the
        // extractor ignores.
        path: "zone@props",
        columns: &["ambients", "releaseAllowed", "zoneFlags", "mapIllustration"],
        tolerated: &["ambient"],
    },
    SheetSpec {
        path: "zone@props@ambients",
        columns: &["ambient"],
        tolerated: &["ambients"],
    },
    SheetSpec {
        path: "skill",
        columns: &[
            "status", "gfx", "id", "type", "nature", "texts", "anim", "cooldown", "duration",
            "steps", "affixes", "aiProps", "flags", "mastery", "props", "notes", "vars", "script",
        ],
        // `costs` and `dur` are authored data the sheet declaration does not
        // list: a cost list beside the skill's props, and a legacy duration
        // beside `duration`. Never read.
        tolerated: &["costs", "dur"],
    },
    SheetSpec {
        path: "skill@texts",
        columns: &["name", "desc", "rankDescs", "refs"],
        tolerated: &[],
    },
    SheetSpec {
        // Only the id, the name other sheets reference, and the `gfx` crop are
        // read; `props` describes the glyph's behaviour, which nothing here
        // needs, and is tolerated as a column without being followed.
        path: "icon",
        columns: &["gfx", "id", "name", "desc", "props"],
        tolerated: &[],
    },
    SheetSpec {
        path: "item",
        columns: &[
            "gfx",
            "id",
            "texts",
            "visuals",
            "type",
            "affinity",
            "aptitudes",
            "level",
            "iLevel",
            "faction",
            "sellPrice",
            "rarity",
            "affixes",
            "skills",
            "props",
            "flags",
        ],
        // `index` is authored data the sheet declaration does not list.
        tolerated: &["index"],
    },
    SheetSpec {
        path: "item@texts",
        columns: &[
            "desc",
            "flavorDesc",
            "author",
            "name",
            "descPrefix",
            "inherit",
        ],
        tolerated: &[],
    },
    SheetSpec {
        path: "item@aptitudes",
        columns: &["ref"],
        tolerated: &[],
    },
    SheetSpec {
        path: "itemType",
        columns: &[
            "gfx",
            "id",
            "inherit",
            "texts",
            "props",
            "atbRatio",
            "setup",
            "skills",
            "flags",
            "moveSet",
            "slot",
            "defaultIcons",
        ],
        tolerated: &[],
    },
    SheetSpec {
        path: "aptitude",
        columns: &["gfx", "id", "name", "combines", "atbScaling", "props"],
        tolerated: &[],
    },
    SheetSpec {
        path: "aptitude@props",
        columns: &["armorReduction", "flags"],
        tolerated: &[],
    },
    SheetSpec {
        path: "rarity",
        columns: &["color", "id", "name", "craftVals", "props", "flags"],
        tolerated: &[],
    },
    SheetSpec {
        path: "rarity@props",
        columns: &[
            "iLevelBonus",
            "generationChance",
            "gearUpgrades",
            "sellPriceFactor",
        ],
        tolerated: &[],
    },
    SheetSpec {
        path: "rarity@props@generationChance",
        columns: &["minLevel", "maxLevel", "chance"],
        tolerated: &[],
    },
    SheetSpec {
        path: "lootTable",
        columns: &["id", "loot", "flags"],
        tolerated: &[],
    },
    SheetSpec {
        path: "lootTable@loot",
        columns: &[
            "proba",
            "item",
            "lootTable",
            "itemMin",
            "itemMax",
            "minLvl",
            "maxLvl",
            "conds",
            "flags",
        ],
        tolerated: &[],
    },
    SheetSpec {
        path: "unit",
        columns: &[
            "gfx",
            "id",
            "type",
            "faction",
            "inherit",
            "texts",
            "lvl",
            "maxLvl",
            "models",
            "parts",
            "skills",
            "talentTrees",
            "moveSetBase",
            "stats",
            "props",
            "sequences",
            "flags",
            "vars",
            "script",
        ],
        // `family` is authored data the sheet declaration does not list.
        tolerated: &["family"],
    },
    SheetSpec {
        path: "unit@texts",
        columns: &["desc", "lines", "namePerPhase", "name"],
        tolerated: &[],
    },
    SheetSpec {
        path: "unit@props",
        columns: &[
            "lootTable",
            "hitShakeRatio",
            "flightHeight",
            "aptitudes",
            "foe",
            "phases",
            "vehicle",
            "activityData",
            "noAggroHeight",
            "xpFactor",
            "bossLootTable",
            "consts",
            "bannerIcon",
        ],
        // `list` is authored data the unit props declaration does not list.
        tolerated: &["list"],
    },
    SheetSpec {
        path: "unitType",
        columns: &["gfx", "id", "parent", "name", "desc", "lootTable", "props"],
        tolerated: &[],
    },
    SheetSpec {
        path: "craft",
        columns: &[
            "item",
            "count",
            "level",
            "job",
            "input",
            "slots",
            "cost",
            "loot",
            "unlockSource",
        ],
        tolerated: &[],
    },
    SheetSpec {
        path: "craft@input",
        columns: &["count", "item"],
        tolerated: &[],
    },
    SheetSpec {
        path: "ach",
        columns: &[
            "qa",
            "id",
            "gfx",
            "category",
            "type",
            "parent",
            "guid",
            "name",
            "desc",
            "points",
            "objectives",
            "reward",
            "props",
        ],
        tolerated: &[],
    },
];

/// Why an extraction was refused.
///
/// Every variant means the game data no longer matches what this crate
/// declares it reads; none of them are warnings.
#[derive(Debug, Error, PartialEq)]
pub enum InventoryError {
    #[error("data.cdb has no sheets array")]
    MissingSheets,
    #[error("data.cdb no longer defines sheet {path:?}")]
    MissingSheet { path: &'static str },
    #[error("sheet {path:?} declares unwhitelisted column {column:?}; whitelist allows {allowed}")]
    UnknownColumn {
        path: String,
        column: String,
        allowed: String,
    },
    #[error("sheet {path:?} no longer declares column {column:?}")]
    MissingColumn { path: String, column: String },
    #[error("group sheet {path:?} has {lines} rows; groups are definitions only")]
    GroupHasRows { path: String, lines: usize },
    #[error(
        "row {id:?} of sheet {path:?} has unwhitelisted key {key:?}; whitelist allows {allowed}"
    )]
    UnknownKey {
        path: String,
        id: String,
        key: String,
        allowed: String,
    },
    #[error("row {id:?} of sheet {path:?} is not an object")]
    RowNotObject { path: String, id: String },
    #[error("row {id:?} of sheet {path:?} has a duplicate id")]
    DuplicateId { path: String, id: String },
    #[error("sheet {path:?} row {id:?} field {field:?} is {found}, expected {expected}")]
    WrongType {
        path: String,
        id: String,
        field: &'static str,
        expected: &'static str,
        found: &'static str,
    },
    #[error("row {id:?} of sheet {sheet:?} names {name:?}, which cannot be resolved: {reason}")]
    UnresolvedName {
        sheet: &'static str,
        id: String,
        name: String,
        reason: &'static str,
    },
    #[error("sheet {path:?} is rendered but no whitelist entry describes it")]
    UnreadSheet { path: &'static str },
}

/// Validates a `CastleDB` document against [`WHITELIST`].
///
/// # Errors
///
/// Fails on a missing sheet, a column the whitelist does not describe, a
/// whitelisted column that disappeared, a group that grew rows, a row key that
/// belongs to no spec, a row without an id, and a duplicate id.
pub fn validate(document: &Value) -> Result<(), InventoryError> {
    let sheets = sheets(document)?;
    for spec in WHITELIST {
        let sheet = find_sheet(sheets, spec.path)
            .ok_or(InventoryError::MissingSheet { path: spec.path })?;
        let declared = declared_columns(sheet);
        for column in &declared {
            if !spec.columns.contains(&column.as_str()) {
                return Err(InventoryError::UnknownColumn {
                    path: spec.path.to_owned(),
                    column: column.clone(),
                    allowed: spec.columns.join(", "),
                });
            }
        }
        for column in spec.columns {
            if !declared.iter().any(|name| name == column) {
                return Err(InventoryError::MissingColumn {
                    path: spec.path.to_owned(),
                    column: (*column).to_owned(),
                });
            }
        }
        let lines = sheet
            .get("lines")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if spec.path.contains('@') {
            if !lines.is_empty() {
                return Err(InventoryError::GroupHasRows {
                    path: spec.path.to_owned(),
                    lines: lines.len(),
                });
            }
            continue;
        }
        let mut seen = Vec::with_capacity(lines.len());
        for line in lines {
            let object = line
                .as_object()
                .ok_or_else(|| InventoryError::RowNotObject {
                    path: spec.path.to_owned(),
                    id: row_id(line),
                })?;
            let id = row_id(line);
            // A row without an id cannot be addressed by anything, so it is not
            // part of the table. The count is reported instead of guessed at.
            // A list sheet has no ids at all: its rows are all of it.
            if !UNKEYED_SHEETS.contains(&spec.path) {
                if id.is_empty() {
                    continue;
                }
                if seen.contains(&id) {
                    return Err(InventoryError::DuplicateId {
                        path: spec.path.to_owned(),
                        id,
                    });
                }
                seen.push(id.clone());
            }
            for (key, value) in object {
                if !spec.columns.contains(&key.as_str())
                    && !spec.tolerated.contains(&key.as_str())
                    && !BOOKKEEPING_KEYS.contains(&key.as_str())
                {
                    return Err(InventoryError::UnknownKey {
                        path: spec.path.to_owned(),
                        id: id.clone(),
                        key: key.clone(),
                        allowed: spec.columns.join(", "),
                    });
                }
                validate_group(&format!("{}@{key}", spec.path), value, &id)?;
            }
        }
    }
    Ok(())
}

/// Validates the keys of one row value against the group sheet it belongs to.
fn validate_group(path: &str, value: &Value, id: &str) -> Result<(), InventoryError> {
    let Some(spec) = WHITELIST.iter().find(|spec| spec.path == path) else {
        // A scalar column: nothing nested to check.
        return Ok(());
    };
    match value {
        Value::Object(object) => validate_group_object(spec, object, id),
        Value::Array(items) => {
            for item in items {
                if let Value::Object(object) = item {
                    validate_group_object(spec, object, id)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_group_object(
    spec: &SheetSpec,
    object: &Map<String, Value>,
    id: &str,
) -> Result<(), InventoryError> {
    for (key, value) in object {
        if !spec.columns.contains(&key.as_str())
            && !spec.tolerated.contains(&key.as_str())
            && !BOOKKEEPING_KEYS.contains(&key.as_str())
        {
            let mut present: Vec<&str> = object.keys().map(String::as_str).collect();
            present.sort_unstable();
            return Err(InventoryError::UnknownKey {
                path: spec.path.to_owned(),
                id: id.to_owned(),
                key: key.clone(),
                allowed: format!(
                    "{} (row has: {})",
                    spec.columns.join(", "),
                    present.join(", ")
                ),
            });
        }
        validate_group(&format!("{}@{key}", spec.path), value, id)?;
    }
    Ok(())
}

/// Renders every artifact the installed game build produces.
///
/// Validates first, so an artifact is never written from data the whitelist
/// does not describe.
///
/// # Errors
///
/// Fails for the same reasons [`validate`] does, or on a row whose whitelisted
/// field holds an unexpected value.
pub fn render(document: &Value, build: &BuildSource) -> Result<Vec<Artifact>, InventoryError> {
    validate(document)?;
    let sheets = sheets(document)?;
    Ok(vec![
        render_build_source(build),
        render_gatherables(sheets)?,
        render_zones(sheets)?,
        render_skills(sheets)?,
        render_icons(sheets)?,
        render_items(sheets)?,
        render_aptitudes(sheets)?,
        render_item_types(sheets)?,
        render_rarities(sheets)?,
        render_loot_tables(sheets)?,
        render_units(sheets)?,
        render_unit_types(sheets)?,
        render_crafts(sheets)?,
        render_achievements(sheets)?,
    ])
}

const HEADER: &[&str] = &[
    "// Generated by `cargo run -p farever-db --bin extract-db`.",
    "// Do not edit by hand: regenerate from the installed game instead.",
    "// Read through `farever_db::Inventory`, never directly.",
];

fn render_build_source(build: &BuildSource) -> Artifact {
    let mut out = base();
    match &build.steam_build_id {
        Some(id) => out.push_str(&format!("// Game build: {id}\n")),
        None => out.push_str("// Game build: unknown\n"),
    }
    out.push_str(&format!(
        "// data.cdb checksum {:#010x}, {} bytes, {} sheets\n",
        build.cdb_checksum, build.cdb_size, build.sheets
    ));
    out.push_str("use super::BuildSource;\n\n");
    out.push_str("/// Which game build the extracted tables came from.\n");
    out.push_str("pub const BUILD: BuildSource = BuildSource {\n");
    out.push_str(&format!(
        "    steam_build_id: {},\n",
        option_text(build.steam_build_id)
    ));
    out.push_str(&format!("    cdb_checksum: {},\n", build.cdb_checksum));
    out.push_str(&format!("    cdb_size: {},\n", build.cdb_size));
    out.push_str(&format!("    sheets: {},\n", build.sheets));
    out.push_str("};\n");
    Artifact {
        file_name: files::BUILD_SOURCE,
        contents: out,
    }
}

fn render_gatherables(sheets: &[Value]) -> Result<Artifact, InventoryError> {
    let gatherable_rows = whitelisted_rows(sheets, "gatherable")?;
    let mut out = base();
    skipped_note(&mut out, without_id(gatherable_rows));
    out.push_str("use super::{Gatherable, GatherableKind};\n\n");
    out.push_str("/// Every gather node the game defines, in `data.cdb` order.\n");
    out.push_str("pub static RECORDS: &[Gatherable] = &[\n");
    let inherits = inherit_chains(gatherable_rows);
    for (id, row) in identified(gatherable_rows) {
        let mut out_row = String::new();
        out_row.push_str(&format!(
            "    Gatherable {{ id: {id:?}, kind: GatherableKind::{:?}, inherit: {}, name: {}, \
             model: {}, loot: {}, hit_loot: {}, required_tool: {}, size: {}, hit_points: {}, \
             respawn_time: {} }},\n",
            gatherable_kind(&id, &inherits),
            option_text(path_str(row, "inherit")?),
            option_text(path_str(row, "texts.name")?),
            option_text(path_str(row, "model")?),
            option_text(path_str(row, "loot")?),
            option_text(path_str(row, "hitLoot")?),
            option_text(path_str(row, "requiredTool")?),
            option_i64(path_i64(row, "props.size")?),
            option_i64(path_i64(row, "props.hitPoints")?),
            option_i64(path_i64(row, "props.respawnTime")?),
        ));
        out.push_str(&out_row);
    }
    out.push_str("];\n");
    Ok(Artifact {
        file_name: files::GATHERABLES,
        contents: out,
    })
}

fn render_zones(sheets: &[Value]) -> Result<Artifact, InventoryError> {
    let zone_rows = whitelisted_rows(sheets, "zone")?;
    let mut out = base();
    skipped_note(&mut out, without_id(zone_rows));
    out.push_str("use super::Zone;\n\n");
    out.push_str("/// Every zone the game defines, in `data.cdb` order.\n");
    out.push_str("pub static RECORDS: &[Zone] = &[\n");
    for (id, row) in identified(zone_rows) {
        out.push_str(&format!(
            "    Zone {{ id: {id:?}, parent: {}, zone_type: {}, level: {}, color: {}, name: {}, \
             desc: {}, map_illustration: {}, release_allowed: {} }},\n",
            option_text(path_str(row, "parent")?),
            required_i64(row, &id, "type", "zone")?,
            required_i64(row, &id, "level", "zone")?,
            required_i64(row, &id, "color", "zone")?,
            option_text(path_str(row, "texts.name")?),
            option_text(path_str(row, "texts.desc")?),
            option_text(path_str(row, "props.mapIllustration")?),
            option_bool(path_bool(row, "props.releaseAllowed")?),
        ));
    }
    out.push_str("];\n");
    Ok(Artifact {
        file_name: files::ZONES,
        contents: out,
    })
}

/// One icon cell: 96 pixels, one cell. The game omits the nullable tile
/// dimensions for the normal case and its own renderer treats a non-positive
/// value as this default.
const ICON_CELL: u32 = 96;

fn render_skills(sheets: &[Value]) -> Result<Artifact, InventoryError> {
    let skill_rows = whitelisted_rows(sheets, "skill")?;
    let icon_rows = whitelisted_rows(sheets, "icon")?;
    let authored: Map<String, Value> = identified(skill_rows)
        .filter_map(|(id, row)| {
            let name = row.get("texts")?.get("name")?.as_str()?;
            Some((id, Value::String(name.to_owned())))
        })
        .collect();
    let document = document_ids(sheets);
    // Names other sheets reference live on the icons, so a skill name that
    // points at one becomes text instead of staying unresolved.
    let icon_names: HashMap<&str, &str> = identified(icon_rows)
        .filter_map(|(id, row)| {
            let name = row.get("name").and_then(Value::as_str)?.trim();
            (!name.is_empty()).then_some((leak_text(&id), name))
        })
        .collect();

    let mut skills = Vec::new();
    let mut foreign: Map<String, Value> = Map::new();
    for (id, row) in identified(skill_rows) {
        let name = match path_str(row, "texts.name")? {
            Some(name) if !name.trim().is_empty() => {
                match resolve_skill_name(name.trim(), &authored, &icon_names, &document, &id, 0)? {
                    Name::Text(name) => Some(name),
                    // The name belongs to a table this crate does not itemize, so
                    // the skill keeps no name; the counts below say how many.
                    Name::Foreign(sheet) => {
                        let count = foreign.get(sheet).and_then(Value::as_u64).unwrap_or(0);
                        foreign.insert(sheet.to_owned(), Value::from(count + 1));
                        None
                    }
                }
            }
            _ => None,
        };
        let icon = gfx_crop(row, "skill", &id)?;
        skills.push((id, name, icon));
    }
    skills.sort_by(|left, right| left.0.cmp(&right.0));

    let mut out = base();
    skipped_note(&mut out, without_id(skill_rows));
    if !foreign.is_empty() {
        let mut counts: Vec<String> = foreign
            .iter()
            .map(|(sheet, count)| format!("{sheet}: {count}"))
            .collect();
        counts.sort();
        out.push_str(&format!(
            "// {} names reference ids from tables this crate does not itemize ({}).\n",
            foreign.values().filter_map(Value::as_u64).sum::<u64>(),
            counts.join(", ")
        ));
    }
    out.push_str("use super::{Skill, IconCrop};\n\n");
    out.push_str("/// Every skill the game defines, sorted by id.\n");
    out.push_str("pub static RECORDS: &[Skill] = &[\n");
    for (id, name, icon) in &skills {
        out.push_str(&format!(
            "    Skill {{ id: {id:?}, name: {}, icon: {} }},\n",
            option_text(*name),
            option_icon(icon),
        ));
    }
    out.push_str("];\n");
    Ok(Artifact {
        file_name: files::SKILLS,
        contents: out,
    })
}

/// Renders the interface glyphs, sorted by id.
///
/// Icons carry the names other sheets reference, so this table is also what
/// lets a skill name like `[Item_Consume]` become text.
fn render_icons(sheets: &[Value]) -> Result<Artifact, InventoryError> {
    let icon_rows = whitelisted_rows(sheets, "icon")?;
    let mut icons = Vec::new();
    for (id, row) in identified(icon_rows) {
        icons.push((
            id.clone(),
            path_str(row, "name")?.filter(|name| !name.trim().is_empty()),
            gfx_crop(row, "icon", &id)?,
        ));
    }
    icons.sort_by(|left, right| left.0.cmp(&right.0));
    let mut out = base();
    skipped_note(&mut out, without_id(icon_rows));
    out.push_str("use super::{Icon, IconCrop};\n\n");
    out.push_str("/// Every interface glyph the game defines, sorted by id.\n");
    out.push_str("pub static RECORDS: &[Icon] = &[\n");
    for (id, name, gfx) in &icons {
        out.push_str(&format!(
            "    Icon {{ id: {id:?}, name: {}, gfx: {} }},\n",
            option_text(*name),
            option_icon(gfx),
        ));
    }
    out.push_str("];\n");
    Ok(Artifact {
        file_name: files::ICONS,
        contents: out,
    })
}

/// Records how many rows one table left out, so a skipped row is a fact in the
/// artifact rather than something a reader has to know about.
fn skipped_note(out: &mut String, skipped: usize) {
    if skipped > 0 {
        out.push_str(&format!(
            "// {skipped} rows carried no id and are not part of this table.\n"
        ));
    }
}

/// What one skill name turned out to be.
enum Name<'a> {
    /// The name itself, or the name a chain of skill references ends at.
    Text(&'a str),
    /// A reference to an id another sheet defines.
    Foreign(&'a str),
}

/// Resolves `CastleDB`'s `[Id]` name references.
///
/// A reference into another sheet is real data this crate cannot name yet, so
/// it is reported by the sheet that defines the id. A reference that is
/// malformed, points at an id no sheet defines, or chains without ending, is
/// broken data and fails extraction.
fn resolve_skill_name<'a>(
    name: &'a str,
    authored: &'a Map<String, Value>,
    icon_names: &'a HashMap<&'a str, &'a str>,
    document: &'a HashMap<&'a str, &'a str>,
    id: &str,
    depth: usize,
) -> Result<Name<'a>, InventoryError> {
    if depth >= 8 {
        return Err(InventoryError::UnresolvedName {
            sheet: "skill",
            id: id.to_owned(),
            name: name.to_owned(),
            reason: "the reference chain does not end",
        });
    }
    let Some(reference) = name
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return Ok(Name::Text(name));
    };
    if reference.is_empty() || reference.contains(['[', ']']) {
        return Err(InventoryError::UnresolvedName {
            sheet: "skill",
            id: id.to_owned(),
            name: name.to_owned(),
            reason: "the reference is malformed",
        });
    }
    if let Some(target) = authored.get(reference).and_then(Value::as_str) {
        return resolve_skill_name(target.trim(), authored, icon_names, document, id, depth + 1);
    }
    if let Some(name) = icon_names.get(reference) {
        return Ok(Name::Text(name));
    }
    document
        .get(reference)
        .copied()
        .map(Name::Foreign)
        .ok_or_else(|| InventoryError::UnresolvedName {
            sheet: "skill",
            id: id.to_owned(),
            name: name.to_owned(),
            reason: "no sheet defines that id",
        })
}

/// Maps every id the document defines to the sheet that defines it.
fn document_ids(sheets: &[Value]) -> HashMap<&str, &str> {
    let mut ids = HashMap::new();
    for sheet in sheets {
        let path = sheet
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        for row in rows(sheets, path) {
            if let Some(id) = row
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                ids.entry(id).or_insert(path);
            }
        }
    }
    ids
}

/// Reads a `CastleDB` reference: either the id itself, or a row it points at.
fn ref_text(value: &Value) -> Option<&str> {
    match value {
        Value::String(text) => Some(text.as_str()),
        Value::Object(object) => object
            .get("id")
            .or_else(|| object.get("ref"))
            .and_then(Value::as_str),
        _ => None,
    }
}

/// Reads a reference out of a dotted path: either the id itself, or the row it
/// points at.
fn path_ref<'a>(row: &'a Value, path: &str) -> Option<&'a str> {
    path_value(row, path).and_then(ref_text)
}

/// Renders a slice of ids as the Rust literal a record field holds.
fn ref_list(values: &[String]) -> String {
    let rendered: Vec<String> = values.iter().map(|value| format!("{value:?}")).collect();
    format!("&[{}]", rendered.join(", "))
}

fn render_items(sheets: &[Value]) -> Result<Artifact, InventoryError> {
    let item_rows = whitelisted_rows(sheets, "item")?;
    let mut items = Vec::new();
    for (id, row) in identified(item_rows) {
        // The catalogue's own name is the id when the game authored none.
        let name = match path_str(row, "texts.name")? {
            Some(name) if !name.trim().is_empty() => name.to_owned(),
            _ => id.clone(),
        };
        let aptitudes: Vec<String> = row
            .get("aptitudes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("ref").and_then(ref_text))
            .map(str::to_owned)
            .collect();
        items.push(format!(
            "    Item {{ id: {id:?}, name: {name:?}, item_type: {}, rarity: {}, faction: {}, \
             aptitudes: {} }},",
            option_text(path_str(row, "type")?),
            option_text(path_str(row, "rarity")?),
            option_text(path_str(row, "faction")?),
            ref_list(&aptitudes),
        ));
    }
    items.sort_unstable();
    Ok(record_artifact(
        files::ITEMS,
        "Item",
        "Every item the game defines, sorted by id.",
        items,
    ))
}

fn render_aptitudes(sheets: &[Value]) -> Result<Artifact, InventoryError> {
    let aptitude_rows = whitelisted_rows(sheets, "aptitude")?;
    let mut aptitudes = Vec::new();
    for (id, row) in identified(aptitude_rows) {
        aptitudes.push(format!(
            "    Aptitude {{ id: {id:?}, flags: {} }},",
            path_i64(row, "props.flags")?.unwrap_or(0),
        ));
    }
    aptitudes.sort_unstable();
    Ok(record_artifact(
        files::APTITUDES,
        "Aptitude",
        "Every aptitude the game defines, sorted by id.",
        aptitudes,
    ))
}

fn render_item_types(sheets: &[Value]) -> Result<Artifact, InventoryError> {
    let itemtype_rows = whitelisted_rows(sheets, "itemType")?;
    let mut item_types: Vec<String> = identified(itemtype_rows)
        .map(|(id, row)| {
            format!(
                "    ItemType {{ id: {id:?}, inherit: {} }},",
                option_text(row.get("inherit").and_then(ref_text)),
            )
        })
        .collect();
    item_types.sort_unstable();
    Ok(record_artifact(
        files::ITEM_TYPES,
        "ItemType",
        "Every item family the game defines, sorted by id.",
        item_types,
    ))
}

fn render_rarities(sheets: &[Value]) -> Result<Artifact, InventoryError> {
    let rarity_rows = whitelisted_rows(sheets, "rarity")?;
    let mut rarities = Vec::new();
    for (id, row) in identified(rarity_rows) {
        let brackets = row
            .pointer("/props/generationChance")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut rendered = Vec::with_capacity(brackets.len());
        for bracket in brackets {
            let number = |field: &str| -> Result<f64, InventoryError> {
                bracket.get(field).and_then(Value::as_f64).ok_or_else(|| {
                    InventoryError::WrongType {
                        path: "rarity".to_owned(),
                        id: id.clone(),
                        field: leak(field),
                        expected: "a number",
                        found: kind_name(bracket.get(field).unwrap_or(&Value::Null)),
                    }
                })
            };
            rendered.push(format!(
                "RarityBracket {{ min_level: {}, max_level: {}, chance: {:?} }}",
                number("minLevel")? as i64,
                number("maxLevel")? as i64,
                number("chance")?,
            ));
        }
        rarities.push(format!(
            "    Rarity {{ id: {id:?}, brackets: &[{}] }},",
            rendered.join(", ")
        ));
    }
    rarities.sort_unstable();
    Ok(record_artifact(
        files::RARITIES,
        "Rarity",
        "Every rarity the game defines, sorted by id.",
        rarities,
    ))
}

/// Renders the loot tables, sorted by id.
///
/// Entry flags and condition bits are stored as the game authors them: which
/// bit means "weighted" is the reader's rule, not the table's.
fn render_loot_tables(sheets: &[Value]) -> Result<Artifact, InventoryError> {
    let loottable_rows = whitelisted_rows(sheets, "lootTable")?;
    let mut tables = Vec::new();
    for (id, row) in identified(loottable_rows) {
        let mut entries = Vec::new();
        for entry in row
            .get("loot")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            entries.push(format!(
                "LootEntry {{ probability: {:?}, item: {}, loot_table: {}, item_min: {}, \
                 item_max: {}, min_level: {}, max_level: {}, conditions: {}, flags: {} }}",
                entry.get("proba").and_then(Value::as_f64).unwrap_or(0.0),
                option_text(entry.get("item").and_then(ref_text)),
                option_text(entry.get("lootTable").and_then(ref_text)),
                option_i64(entry.get("itemMin").and_then(Value::as_i64)),
                option_i64(entry.get("itemMax").and_then(Value::as_i64)),
                option_i64(entry.get("minLvl").and_then(Value::as_i64)),
                option_i64(entry.get("maxLvl").and_then(Value::as_i64)),
                option_i64(entry.get("conds").and_then(Value::as_i64)),
                entry.get("flags").and_then(Value::as_i64).unwrap_or(0),
            ));
        }
        tables.push(format!(
            "    LootTable {{ id: {id:?}, flags: {}, entries: &[{}] }},",
            row.get("flags").and_then(Value::as_i64).unwrap_or(0),
            entries.join(", ")
        ));
    }
    tables.sort_unstable();
    Ok(record_artifact(
        files::LOOT_TABLES,
        "LootTable",
        "Every loot table the game defines, sorted by id.",
        tables,
    ))
}

/// Renders the units, sorted by id.
fn render_units(sheets: &[Value]) -> Result<Artifact, InventoryError> {
    let unit_rows = whitelisted_rows(sheets, "unit")?;
    let mut units = Vec::new();
    for (id, row) in identified(unit_rows) {
        units.push(format!(
            "    Unit {{ id: {id:?}, name: {}, faction: {}, loot_table: {}, boss_loot_table: {} }},",
            option_text(path_str(row, "texts.name")?),
            option_text(path_ref(row, "faction")),
            option_text(path_ref(row, "props.lootTable")),
            option_text(path_ref(row, "props.bossLootTable")),
        ));
    }
    units.sort_unstable();
    Ok(record_artifact(
        files::UNITS,
        "Unit",
        "Every unit the game defines, sorted by id.",
        units,
    ))
}

/// Renders the enemy families, sorted by id.
fn render_unit_types(sheets: &[Value]) -> Result<Artifact, InventoryError> {
    let unittype_rows = whitelisted_rows(sheets, "unitType")?;
    let mut unit_types = Vec::new();
    for (id, row) in identified(unittype_rows) {
        unit_types.push(format!(
            "    UnitType {{ id: {id:?}, name: {}, loot_table: {} }},",
            option_text(path_str(row, "name")?),
            option_text(path_ref(row, "lootTable")),
        ));
    }
    unit_types.sort_unstable();
    Ok(record_artifact(
        files::UNIT_TYPES,
        "UnitType",
        "Every enemy family the game defines, sorted by id.",
        unit_types,
    ))
}

/// Renders the recipes, in sheet order: a recipe has no id of its own.
fn render_crafts(sheets: &[Value]) -> Result<Artifact, InventoryError> {
    let craft_rows = whitelisted_rows(sheets, "craft")?;
    let mut crafts = Vec::new();
    for row in craft_rows {
        let mut ingredients = Vec::new();
        for ingredient in row
            .get("input")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            ingredients.push(format!(
                "Ingredient {{ item: {}, count: {} }}",
                option_text(ingredient.get("item").and_then(ref_text)),
                ingredient.get("count").and_then(Value::as_i64).unwrap_or(1),
            ));
        }
        crafts.push(format!(
            "    Craft {{ item: {}, job: {}, level: {}, ingredients: &[{}] }},",
            option_text(row.get("item").and_then(ref_text)),
            option_text(row.get("job").and_then(ref_text)),
            row.get("level").and_then(Value::as_i64).unwrap_or(0),
            ingredients.join(", ")
        ));
    }
    Ok(record_artifact(
        files::CRAFTS,
        "Craft",
        "Every recipe the game defines, in sheet order.",
        crafts,
    ))
}

/// Renders the achievements, sorted by id.
fn render_achievements(sheets: &[Value]) -> Result<Artifact, InventoryError> {
    let ach_rows = whitelisted_rows(sheets, "ach")?;
    let mut achievements = Vec::new();
    for (id, row) in identified(ach_rows) {
        let rewards: Vec<String> = row
            .pointer("/reward/items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|reward| reward.get("item").and_then(ref_text))
            .map(|item| format!("{item:?}"))
            .collect();
        achievements.push(format!(
            "    Achievement {{ id: {id:?}, name: {}, desc: {}, rewards: &[{}] }},",
            option_text(path_str(row, "name")?),
            option_text(path_str(row, "desc")?),
            rewards.join(", ")
        ));
    }
    achievements.sort_unstable();
    Ok(record_artifact(
        files::ACHIEVEMENTS,
        "Achievement",
        "Every achievement the game defines, sorted by id.",
        achievements,
    ))
}

/// Wraps rendered records in the artifact header, import and static they share.
///
/// The import is a glob on purpose: it is one of these files, not the module,
/// that names the record and bracket types, and a record that grows a field of
/// another type must not leave a stale artifact that cannot be regenerated.
fn record_artifact(
    file_name: &'static str,
    module: &str,
    doc: &str,
    records: Vec<String>,
) -> Artifact {
    let mut out = base();
    out.push_str("use super::*;\n\n");
    out.push_str(&format!("/// {doc}\n"));
    out.push_str(&format!("pub static RECORDS: &[{module}] = &[\n"));
    for line in &records {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("];\n");
    Artifact {
        file_name,
        contents: out,
    }
}

/// Reads one row's `gfx` crop, in pixels.
///
/// The game stores tile coordinates and a cell size, and leaves the cell size
/// and extent out for the common case; a non-positive value means the default,
/// as it does for the game's own renderer. A negative pixel coordinate is not
/// that case and fails extraction.
fn gfx_crop(
    row: &Value,
    sheet: &'static str,
    id: &str,
) -> Result<Option<IconCrop>, InventoryError> {
    let Some(gfx) = path_value(row, "gfx") else {
        return Ok(None);
    };
    let Some(atlas_path) = gfx.get("file").and_then(Value::as_str) else {
        return Ok(None);
    };
    if atlas_path.trim().is_empty() {
        return Ok(None);
    }
    let cell = match gfx.get("size").and_then(Value::as_i64) {
        Some(size) if size > 0 => {
            u32::try_from(size).map_err(|_| icon_error(row, sheet, id, "size"))?
        }
        _ => ICON_CELL,
    };
    let origin = |field: &str| -> Result<u32, InventoryError> {
        match gfx.get(field).and_then(Value::as_i64) {
            Some(value) => u32::try_from(value).map_err(|_| icon_error(row, sheet, id, field)),
            None => Ok(0),
        }
    };
    let extent = |field: &str| -> Result<u32, InventoryError> {
        match gfx.get(field).and_then(Value::as_i64) {
            Some(value) if value > 0 => {
                u32::try_from(value).map_err(|_| icon_error(row, sheet, id, field))
            }
            _ => Ok(1),
        }
    };
    Ok(Some(IconCrop {
        atlas_path: leak_text(atlas_path),
        x: origin("x")? * cell,
        y: origin("y")? * cell,
        width: extent("width")? * cell,
        height: extent("height")? * cell,
    }))
}

fn icon_error(row: &Value, sheet: &'static str, id: &str, field: &str) -> InventoryError {
    InventoryError::WrongType {
        path: sheet.to_owned(),
        id: id.to_owned(),
        field: leak(field),
        expected: "a non-negative tile coordinate",
        found: kind_name(
            row.get("gfx")
                .and_then(|gfx| gfx.get(field))
                .unwrap_or(&Value::Null),
        ),
    }
}

/// Keeps atlas paths in the artifact's static lifetime.
///
/// Extraction runs once per process and the strings live as long as it does,
/// which is exactly as long as the artifact being rendered needs them.
fn leak_text(value: &str) -> &'static str {
    Box::leak(value.to_owned().into_boxed_str())
}

fn option_icon(icon: &Option<IconCrop>) -> String {
    match icon {
        Some(icon) => format!(
            "Some(IconCrop {{ atlas_path: {:?}, x: {}, y: {}, width: {}, height: {} }})",
            icon.atlas_path, icon.x, icon.y, icon.width, icon.height
        ),
        None => "None".to_owned(),
    }
}

fn base() -> String {
    let mut out = String::with_capacity(64 * 1024);
    for line in HEADER {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The kind a gatherable inherits, from the roots of its inheritance chain.
fn gatherable_kind(id: &str, inherits: &Map<String, Value>) -> GatherableKind {
    let mut current = id.to_owned();
    for _ in 0..16 {
        let root = inherits
            .get(&current)
            .and_then(Value::as_str)
            .unwrap_or(current.as_str())
            .to_owned();
        match root.as_str() {
            "Ore" => return GatherableKind::Ore,
            "Plant" => return GatherableKind::Plant,
            _ if root == current => return GatherableKind::Other,
            _ => current = root,
        }
    }
    GatherableKind::Other
}

fn inherit_chains(rows: &[Value]) -> Map<String, Value> {
    let mut chains = Map::new();
    for row in rows {
        if let (Some(id), Some(inherit)) = (
            row.get("id").and_then(Value::as_str),
            row.get("inherit").and_then(Value::as_str),
        ) {
            chains.insert(id.to_owned(), Value::String(inherit.to_owned()));
        }
    }
    chains
}

fn sheets(document: &Value) -> Result<&[Value], InventoryError> {
    document
        .get("sheets")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or(InventoryError::MissingSheets)
}

fn find_sheet<'a>(sheets: &'a [Value], path: &str) -> Option<&'a Value> {
    sheets
        .iter()
        .find(|sheet| sheet.get("name").and_then(Value::as_str) == Some(path))
}

fn rows<'a>(sheets: &'a [Value], path: &str) -> &'a [Value] {
    find_sheet(sheets, path)
        .and_then(|sheet| sheet.get("lines"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Number of rows a document holds for one sheet path, for reporting.
#[must_use]
pub fn row_count(document: &Value, path: &str) -> usize {
    sheets(document).map_or(0, |sheets| rows(sheets, path).len())
}

/// The rows of one sheet a table renders from.
///
/// A renderer may only read sheets [`WHITELIST`] describes: reading an unlisted
/// sheet is how a table ends up generated from data nothing validated, which
/// only shows up against the installed game because a fixture document only
/// carries the sheets the fixture declares.
fn whitelisted_rows<'a>(
    sheets: &'a [Value],
    path: &'static str,
) -> Result<&'a [Value], InventoryError> {
    if !WHITELIST.iter().any(|spec| spec.path == path) {
        return Err(InventoryError::UnreadSheet { path });
    }
    Ok(rows(sheets, path))
}

/// The rows of one sheet that carry an id, in document order.
///
/// A row without an id defines nothing anything can address, so every table
/// renders from this and [`without_id`] reports what was left out.
fn identified(rows: &[Value]) -> impl Iterator<Item = (String, &Value)> {
    rows.iter().filter_map(|row| {
        let id = row_id(row);
        (!id.is_empty()).then_some((id, row))
    })
}

/// Number of rows one sheet holds that carry no id.
fn without_id(rows: &[Value]) -> usize {
    rows.iter().filter(|row| row_id(row).is_empty()).count()
}

fn declared_columns(sheet: &Value) -> Vec<String> {
    sheet
        .get("columns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|column| column.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn row_id(row: &Value) -> String {
    row.get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Reads a dotted path out of a row, e.g. `props.size` or `texts.name`.
fn path_value<'a>(row: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = row;
    for step in path.split('.') {
        current = current.get(step)?;
    }
    if current.is_null() {
        return None;
    }
    Some(current)
}

fn path_str<'a>(row: &'a Value, path: &str) -> Result<Option<&'a str>, InventoryError> {
    match path_value(row, path) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| type_error(row, path, "a string", value)),
    }
}

fn path_i64(row: &Value, path: &str) -> Result<Option<i64>, InventoryError> {
    match path_value(row, path) {
        None => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| type_error(row, path, "a signed integer", value)),
    }
}

fn path_bool(row: &Value, path: &str) -> Result<Option<bool>, InventoryError> {
    match path_value(row, path) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| type_error(row, path, "a boolean", value)),
    }
}

fn required_i64(
    row: &Value,
    id: &str,
    path: &str,
    sheet: &'static str,
) -> Result<i64, InventoryError> {
    path_i64(row, path)?.ok_or_else(|| InventoryError::WrongType {
        path: sheet.to_owned(),
        id: id.to_owned(),
        field: "missing",
        expected: "a signed integer",
        found: "absent",
    })
}

fn type_error(row: &Value, field: &str, expected: &'static str, found: &Value) -> InventoryError {
    InventoryError::WrongType {
        path: "data.cdb".to_owned(),
        id: row_id(row),
        field: leak(field),
        expected,
        found: kind_name(found),
    }
}

/// Field names in errors are derived from the whitelist, which is static; this
/// keeps [`InventoryError`] free of ownership for them.
fn leak(value: &str) -> &'static str {
    WHITELIST
        .iter()
        .flat_map(|spec| spec.columns.iter().copied())
        .chain(["id", "type", "level", "color"])
        .find(|candidate| *candidate == value || value.ends_with(&format!(".{candidate}")))
        .unwrap_or("field")
}

fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

fn option_text(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("Some({value:?})"),
        None => "None".to_owned(),
    }
}

fn option_i64(value: Option<i64>) -> String {
    match value {
        Some(value) => format!("Some({value})"),
        None => "None".to_owned(),
    }
}

fn option_bool(value: Option<bool>) -> String {
    match value {
        Some(value) => format!("Some({value})"),
        None => "None".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> Value {
        serde_json::json!({
            "sheets": [
                {
                    "name": "gatherable",
                    "columns": [
                        {"name": "id"}, {"name": "inherit"}, {"name": "requiredTool"},
                        {"name": "props"}, {"name": "texts"}, {"name": "model"},
                        {"name": "loot"}, {"name": "hitLoot"}, {"name": "flags"}
                    ],
                    "lines": [
                        {"id": "Ore", "flags": 1},
                        {"id": "Ore_Small", "inherit": "Ore"},
                        {"id": "Ore_Copper_Small", "inherit": "Ore_Small", "loot": "Ore_Copper",
                         "props": {"size": 0, "hitPoints": 60, "respawnTime": 120,
                                   "affixes": [{"status": "OreAffix_Fire", "chance": 0.24}]},
                         "texts": {"name": "Copper Cluster"}},
                        {"id": "Madrigold_Small", "inherit": "Plant_Small", "model": "Madrigold",
                         "texts": {"name": "Madrigold Sprout"}}
                    ]
                },
                {"name": "gatherable@props", "columns": [
                    {"name": "hitPoints"}, {"name": "respawnTime"}, {"name": "hitFX"},
                    {"name": "affixes"}, {"name": "size"}, {"name": "affixChance"},
                    {"name": "gatherSkill"}], "lines": []},
                {"name": "gatherable@props@affixes", "columns": [
                    {"name": "status"}, {"name": "fxSet"}, {"name": "chance"}], "lines": []},
                {"name": "gatherable@texts", "columns": [
                    {"name": "name"}, {"name": "desc"}, {"name": "type"}], "lines": []},
                {
                    "name": "zone",
                    "columns": [{"name": "id"}, {"name": "parent"}, {"name": "type"},
                                {"name": "level"}, {"name": "color"}, {"name": "texts"},
                                {"name": "props"}],
                    "lines": [
                        {"id": "Z1_Region", "color": 16711680, "level": 1, "type": 0,
                         "texts": {"name": "Skover Island"}},
                        {"id": "Z1_Meridion", "parent": "Z1_Region", "color": 14679808,
                         "level": 5, "type": 1,
                         "props": {"mapIllustration": "UI/Window/Map/Meridion.png",
                                   "releaseAllowed": true,
                                   "ambients": [{"ambient": "Music_Region_01"}]},
                         "texts": {"name": "Meridion"}}
                    ]
                },
                {"name": "zone@texts", "columns": [{"name": "name"}, {"name": "desc"}], "lines": []},
                {"name": "zone@props", "columns": [{"name": "ambients"},
                    {"name": "releaseAllowed"}, {"name": "zoneFlags"},
                    {"name": "mapIllustration"}], "lines": []},
                {"name": "zone@props@ambients", "columns": [{"name": "ambient"}], "lines": []},
                {
                    "name": "skill",
                    "columns": [
                        {"name": "status"}, {"name": "gfx"}, {"name": "id"},
                        {"name": "type"}, {"name": "nature"}, {"name": "texts"},
                        {"name": "anim"}, {"name": "cooldown"}, {"name": "duration"},
                        {"name": "steps"}, {"name": "affixes"}, {"name": "aiProps"},
                        {"name": "flags"}, {"name": "mastery"}, {"name": "props"},
                        {"name": "notes"}, {"name": "vars"}, {"name": "script"}
                    ],
                    "lines": [
                        // A literal name, an icon in the atlas, and a data-only key.
                        {"id": "Axe_Base_Attack", "dur": 0.5,
                         "texts": {"name": "Cleave"},
                         "gfx": {"file": "UI/icons/atlas_axe.png", "x": 1, "y": 2,
                                 "width": 2, "height": 1, "size": 48}},
                        // A reference to another skill, resolved to its name; one
                        // to an icon, resolved through that table's name; and one
                        // into a table this crate does not itemize.
                        {"id": "GS_Nova_Combo", "texts": {"name": "[Axe_Base_Attack]"}},
                        {"id": "ConsumeItem", "texts": {"name": "[Item_Consume]"}},
                        {"id": "CrossSheet", "texts": {"name": "[Thing]"}},
                        // No icon of its own and no name at all.
                        {"id": "Mystery"},
                        // Not part of the table: it has no id.
                        {"texts": {"name": "orphan"}}
                    ]
                },
                {"name": "skill@texts", "columns": [
                    {"name": "name"}, {"name": "desc"}, {"name": "rankDescs"},
                    {"name": "refs"}], "lines": []},
                // The glyph a skill name may reference, and a table that is not
                // whitelisted but does define an id a name points at.
                {"name": "icon", "columns": [{"name": "gfx"}, {"name": "id"},
                    {"name": "name"}, {"name": "desc"}, {"name": "props"}], "lines": [
                        {"id": "Item_Consume", "name": "Consume",
                         "gfx": {"file": "UI/icons/atlas_book.png", "x": 0, "y": 1}}
                    ]},
                {"name": "item", "columns": declared(&[
                    "gfx", "id", "texts", "visuals", "type", "affinity", "aptitudes",
                    "level", "iLevel", "faction", "sellPrice", "rarity", "affixes",
                    "skills", "props", "flags"]), "lines": [
                    {"id": "Agate", "type": "CraftingComponent", "rarity": "Uncommon",
                     "texts": {"name": "Agate"},
                     "aptitudes": [{"ref": "Cleric"}], "index": 4},
                    {"id": "Unnamed_Thing"},
                    // What the skill name `[Thing]` and `[Item_Consume]` point at.
                    {"id": "Thing"}, {"id": "Item_Consume"}
                ]},
                {"name": "item@texts", "columns": declared(&[
                    "desc", "flavorDesc", "author", "name", "descPrefix", "inherit"]),
                 "lines": []},
                {"name": "item@aptitudes", "columns": declared(&["ref"]), "lines": []},
                {"name": "itemType", "columns": declared(&[
                    "gfx", "id", "inherit", "texts", "props", "atbRatio", "setup",
                    "skills", "flags", "moveSet", "slot", "defaultIcons"]), "lines": [
                    {"id": "Weapon"}, {"id": "GreatSword", "inherit": "Weapon"}
                ]},
                {"name": "aptitude", "columns": declared(&[
                    "gfx", "id", "name", "combines", "atbScaling", "props"]), "lines": [
                    {"id": "Cleric", "props": {"flags": 1}}
                ]},
                {"name": "aptitude@props", "columns": declared(&["armorReduction", "flags"]),
                 "lines": []},
                {"name": "rarity", "columns": declared(&[
                    "color", "id", "name", "craftVals", "props", "flags"]), "lines": [
                    {"id": "Uncommon", "props": {"generationChance": [
                        {"minLevel": 1, "maxLevel": 10, "chance": 0.1}]}}
                ]},
                {"name": "rarity@props", "columns": declared(&[
                    "iLevelBonus", "generationChance", "gearUpgrades", "sellPriceFactor"]),
                 "lines": []},
                {"name": "rarity@props@generationChance", "columns": declared(&[
                    "minLevel", "maxLevel", "chance"]), "lines": []},
                {"name": "lootTable", "columns": declared(&["id", "loot", "flags"]), "lines": [
                    // Weighted: the entries are weights, not chances.
                    {"id": "Reward", "flags": 1, "loot": [
                        {"proba": 0.5, "item": "Agate", "minLvl": 1, "maxLvl": 25},
                        {"proba": 0.5, "lootTable": "Nested", "conds": 2, "flags": 0}
                    ]},
                    {"id": "Nested", "loot": [
                        {"proba": 0.25, "item": "Agate", "itemMin": 1, "itemMax": 2}
                    ]}
                ]},
                {"name": "lootTable@loot", "columns": declared(&[
                    "proba", "item", "lootTable", "itemMin", "itemMax", "minLvl", "maxLvl",
                    "conds", "flags"]), "lines": []},
                {"name": "unit", "columns": declared(&[
                    "gfx", "id", "type", "faction", "inherit", "texts", "lvl", "maxLvl",
                    "models", "parts", "skills", "talentTrees", "moveSetBase", "stats", "props",
                    "sequences", "flags", "vars", "script"]), "lines": [
                    {"id": "Reblochonk", "faction": "Faction_Z1_Beast", "family": "Boss",
                     "texts": {"name": "Reblochonk"},
                     "props": {"bossLootTable": "Reward", "lootTable": "Nested", "list": true}},
                    {"id": "Rabbit"}
                ]},
                {"name": "unit@texts", "columns": declared(&[
                    "desc", "lines", "namePerPhase", "name"]), "lines": []},
                {"name": "unit@props", "columns": declared(&[
                    "lootTable", "hitShakeRatio", "flightHeight", "aptitudes", "foe", "phases",
                    "vehicle", "activityData", "noAggroHeight", "xpFactor", "bossLootTable",
                    "consts", "bannerIcon"]), "lines": []},
                {"name": "unitType", "columns": declared(&[
                    "gfx", "id", "parent", "name", "desc", "lootTable", "props"]), "lines": [
                    {"id": "Beast", "name": "Beasts", "lootTable": "Nested"}
                ]},
                {"name": "craft", "columns": declared(&[
                    "item", "count", "level", "job", "input", "slots", "cost", "loot",
                    "unlockSource"]), "lines": [
                    // Recipes carry no id: they are a list.
                    {"item": "Agate", "job": "Blacksmith", "level": 3,
                     "input": [{"item": "CopperIngot", "count": 8}]},
                    {"item": "Uncraftable_Thing", "input": []}
                ]},
                {"name": "craft@input", "columns": declared(&["count", "item"]), "lines": []},
                {"name": "ach", "columns": declared(&[
                    "qa", "id", "gfx", "category", "type", "parent", "guid", "name", "desc",
                    "points", "objectives", "reward", "props"]), "lines": [
                    {"id": "AllActivities_Z1", "name": "Emissary", "desc": "Complete everything.",
                     "reward": {"items": [{"item": "Agate"}]}}
                ]}
            ]
        })
    }

    /// The column list a fixture sheet declares.
    fn declared(names: &[&str]) -> Value {
        Value::Array(
            names
                .iter()
                .map(|name| serde_json::json!({"name": name}))
                .collect(),
        )
    }

    fn build() -> BuildSource {
        BuildSource {
            steam_build_id: Some("1234"),
            cdb_checksum: 0xc383_de7f,
            cdb_size: 4_902_490,
            sheets: 426,
        }
    }

    fn artifacts(document: &Value) -> Vec<Artifact> {
        render(document, &build()).expect("render")
    }

    fn artifact<'a>(artifacts: &'a [Artifact], file_name: &str) -> &'a str {
        &artifacts
            .iter()
            .find(|artifact| artifact.file_name == file_name)
            .unwrap_or_else(|| panic!("no artifact named {file_name}"))
            .contents
    }

    #[test]
    fn a_whitelisted_document_renders_one_artifact_per_table() {
        let artifacts = artifacts(&document());
        let names: Vec<&str> = artifacts.iter().map(|a| a.file_name).collect();
        assert_eq!(
            names,
            vec![
                files::BUILD_SOURCE,
                files::GATHERABLES,
                files::ZONES,
                files::SKILLS,
                files::ICONS,
                files::ITEMS,
                files::APTITUDES,
                files::ITEM_TYPES,
                files::RARITIES,
                files::LOOT_TABLES,
                files::UNITS,
                files::UNIT_TYPES,
                files::CRAFTS,
                files::ACHIEVEMENTS
            ]
        );

        let gatherables = artifact(&artifacts, files::GATHERABLES);
        assert!(gatherables.contains("use super::{Gatherable, GatherableKind};"));
        assert!(gatherables.contains(
            "Gatherable { id: \"Ore_Copper_Small\", kind: GatherableKind::Ore, \
             inherit: Some(\"Ore_Small\"), name: Some(\"Copper Cluster\"), model: None, \
             loot: Some(\"Ore_Copper\"), hit_loot: None, required_tool: None, size: Some(0), \
             hit_points: Some(60), respawn_time: Some(120) },"
        ));

        let zones = artifact(&artifacts, files::ZONES);
        assert!(zones.contains("use super::Zone;"));
        assert!(zones.contains(
            "Zone { id: \"Z1_Meridion\", parent: Some(\"Z1_Region\"), zone_type: 1, level: 5, \
             color: 14679808, name: Some(\"Meridion\"), desc: None, \
             map_illustration: Some(\"UI/Window/Map/Meridion.png\"), release_allowed: Some(true) },"
        ));

        let build = artifact(&artifacts, files::BUILD_SOURCE);
        assert!(build.contains("steam_build_id: Some(\"1234\")"));
        assert!(build.contains("cdb_checksum: 3280199295"));
        assert!(build.contains("sheets: 426"));
    }

    #[test]
    fn skills_resolve_names_and_pixel_geometry() {
        let artifacts = artifacts(&document());
        let skills = artifact(&artifacts, files::SKILLS);

        assert!(skills.contains("// 1 rows carried no id and are not part of this table.\n"));
        assert!(skills.contains(
            "// 1 names reference ids from tables this crate does not itemize (item: 1).\n"
        ));
        // Sorted by id, which is what makes `Inventory::skill` a binary search.
        assert!(skills.contains(
            "    Skill { id: \"Axe_Base_Attack\", name: Some(\"Cleave\"), icon: \
             Some(IconCrop { atlas_path: \"UI/icons/atlas_axe.png\", x: 48, y: 96, \
             width: 96, height: 48 }) },"
        ));
        assert!(skills
            .contains("    Skill { id: \"GS_Nova_Combo\", name: Some(\"Cleave\"), icon: None },"));
        // A name that points at an icon arrives as the icon's name.
        assert!(skills
            .contains("    Skill { id: \"ConsumeItem\", name: Some(\"Consume\"), icon: None },"));

        let icons = artifact(&artifacts, files::ICONS);
        assert!(icons.contains(
            "    Icon { id: \"Item_Consume\", name: Some(\"Consume\"), gfx: \
             Some(IconCrop { atlas_path: \"UI/icons/atlas_book.png\", x: 0, y: 96, \
             width: 96, height: 96 }) },"
        ));
    }

    #[test]
    fn the_catalogue_renders_items_families_aptitudes_and_rarities() {
        let artifacts = artifacts(&document());

        let items = artifact(&artifacts, files::ITEMS);
        assert!(items.contains(
            "    Item { id: \"Agate\", name: \"Agate\", item_type: Some(\"CraftingComponent\"), \
             rarity: Some(\"Uncommon\"), faction: None, aptitudes: &[\"Cleric\"] },"
        ));
        // An item the game never named falls back to its id.
        assert!(items.contains(
            "    Item { id: \"Unnamed_Thing\", name: \"Unnamed_Thing\", item_type: None, \
             rarity: None, faction: None, aptitudes: &[] },"
        ));

        assert!(artifact(&artifacts, files::APTITUDES)
            .contains("    Aptitude { id: \"Cleric\", flags: 1 },"));
        assert!(artifact(&artifacts, files::ITEM_TYPES)
            .contains("    ItemType { id: \"GreatSword\", inherit: Some(\"Weapon\") },"));
        assert!(artifact(&artifacts, files::RARITIES).contains(
            "    Rarity { id: \"Uncommon\", brackets: &[RarityBracket { min_level: 1, \
             max_level: 10, chance: 0.1 }] },"
        ));
    }

    #[test]
    #[cfg(feature = "embedded")]
    fn the_embedded_catalogue_resolves_by_id() {
        let items = Inventory::items();
        assert!(items.len() > 1000, "{}", items.len());
        assert!(
            items.windows(2).all(|pair| pair[0].id < pair[1].id),
            "item ids must be sorted for the binary search"
        );

        let agate = Inventory::item("Agate").expect("Agate");
        assert_eq!(agate.name, "Agate");
        assert_eq!(agate.item_type, Some("CraftingComponent"));
        assert_eq!(agate.rarity, Some("Uncommon"));
        assert!(Inventory::item("NoSuchItem").is_none());

        assert!(Inventory::item_types().len() > 100);
        assert_eq!(
            Inventory::item_type("GreatSword").and_then(|family| family.inherit),
            Some("THWeapon")
        );
        assert_eq!(Inventory::aptitudes().len(), 15);
        assert!(Inventory::aptitude("Cleric").is_some());

        let rarities = Inventory::rarities();
        assert_eq!(rarities.len(), 5, "{rarities:?}");
        assert!(!Inventory::rarity("Uncommon")
            .expect("Uncommon")
            .brackets
            .is_empty());
        assert!(Inventory::rarity("NoSuchRarity").is_none());
    }

    #[test]
    fn the_loot_sources_render_entries_units_recipes_and_rewards() {
        let artifacts = artifacts(&document());

        let tables = artifact(&artifacts, files::LOOT_TABLES);
        assert!(tables.contains(
            r#"    LootTable { id: "Reward", flags: 1, entries: &[LootEntry { probability: 0.5, item: Some("Agate"), loot_table: None, item_min: None, item_max: None, min_level: Some(1), max_level: Some(25), conditions: None, flags: 0 }, LootEntry { probability: 0.5, item: None, loot_table: Some("Nested"), item_min: None, item_max: None, min_level: None, max_level: None, conditions: Some(2), flags: 0 }] },"#
        ));

        let units = artifact(&artifacts, files::UNITS);
        assert!(units.contains(
            r#"    Unit { id: "Reblochonk", name: Some("Reblochonk"), faction: Some("Faction_Z1_Beast"), loot_table: Some("Nested"), boss_loot_table: Some("Reward") },"#
        ));
        assert!(units.contains(
            r#"    Unit { id: "Rabbit", name: None, faction: None, loot_table: None, boss_loot_table: None },"#
        ));

        assert!(artifact(&artifacts, files::UNIT_TYPES).contains(
            r#"    UnitType { id: "Beast", name: Some("Beasts"), loot_table: Some("Nested") },"#
        ));
        assert!(artifact(&artifacts, files::CRAFTS).contains(
            r#"    Craft { item: Some("Agate"), job: Some("Blacksmith"), level: 3, ingredients: &[Ingredient { item: Some("CopperIngot"), count: 8 }] },"#
        ));
        assert!(artifact(&artifacts, files::ACHIEVEMENTS).contains(
            r#"    Achievement { id: "AllActivities_Z1", name: Some("Emissary"), desc: Some("Complete everything."), rewards: &["Agate"] },"#
        ));
    }

    #[test]
    #[cfg(feature = "embedded")]
    fn the_embedded_loot_sources_are_populated() {
        let tables = Inventory::loot_tables();
        assert!(tables.len() > 100, "{}", tables.len());
        assert!(
            tables.windows(2).all(|pair| pair[0].id < pair[1].id),
            "loot table ids must be sorted for the binary search"
        );
        let air = Inventory::loot_table("AirWeights").expect("AirWeights");
        assert_eq!(air.flags & 1, 1, "the weighted bit is set in the data");
        assert_eq!(air.entries[0].item, Some("MoteOfAir"));
        assert!(Inventory::loot_table("NoSuchTable").is_none());

        let units = Inventory::units();
        assert!(units.len() > 500, "{}", units.len());
        assert!(Inventory::unit("Reblochonk").is_some());
        assert_eq!(Inventory::unit_types().len(), 25);
        let boar = Inventory::unit_type("Boar").expect("Boar");
        assert_eq!(boar.name, Some("Boars"));
        assert_eq!(boar.loot_table, Some("Boar"));
        assert!(Inventory::unit_type("NoSuchFamily").is_none());

        let crafts = Inventory::crafts();
        assert_eq!(crafts.len(), 190, "{}", crafts.len());
        assert!(
            crafts.iter().all(|craft| craft.item.is_some()),
            "every recipe names the item it yields"
        );
        let recipe = crafts
            .iter()
            .find(|craft| craft.job == Some("Blacksmith") && !craft.ingredients.is_empty())
            .expect("at least one Blacksmith recipe with ingredients");
        assert!(recipe
            .ingredients
            .iter()
            .all(|ingredient| ingredient.count >= 1));

        let achievements = Inventory::achievements();
        assert!(achievements.len() > 180, "{}", achievements.len());
        assert!(Inventory::achievement("AllActivities_Z1").is_some());
        assert!(Inventory::achievement("NoSuchAchievement").is_none());
    }

    #[test]
    fn a_malformed_name_reference_fails() {
        let mut document = document();
        document["sheets"][8]["lines"][1]["texts"]["name"] = serde_json::json!("[]");

        assert_eq!(
            validate(&document),
            Ok(()),
            "the reference is well-formed data; only the renderer follows it"
        );
        assert_eq!(
            render(&document, &build()),
            Err(InventoryError::UnresolvedName {
                sheet: "skill",
                id: "GS_Nova_Combo".to_owned(),
                name: "[]".to_owned(),
                reason: "the reference is malformed",
            })
        );
    }

    #[test]
    fn a_reference_no_sheet_defines_fails() {
        let mut document = document();
        document["sheets"][8]["lines"][1]["texts"]["name"] = serde_json::json!("[NoSuchThing]");

        assert_eq!(
            render(&document, &build()),
            Err(InventoryError::UnresolvedName {
                sheet: "skill",
                id: "GS_Nova_Combo".to_owned(),
                name: "[NoSuchThing]".to_owned(),
                reason: "no sheet defines that id",
            })
        );
    }

    #[test]
    fn a_table_may_only_read_sheets_the_whitelist_describes() {
        let document = document();
        let sheets = sheets(&document).expect("sheets");

        assert!(
            whitelisted_rows(sheets, "gatherable").is_ok(),
            "a whitelisted sheet is readable"
        );
        assert_eq!(
            whitelisted_rows(sheets, "noSuchSheet"),
            Err(InventoryError::UnreadSheet {
                path: "noSuchSheet"
            }),
            "reading an unlisted sheet fails instead of generating from it unvalidated"
        );
    }

    #[test]
    fn a_new_column_fails_instead_of_being_ignored() {
        let mut document = document();
        document["sheets"][0]["columns"]
            .as_array_mut()
            .expect("columns")
            .push(serde_json::json!({"name": "harvestSpeed"}));

        assert_eq!(
            validate(&document),
            Err(InventoryError::UnknownColumn {
                path: "gatherable".to_owned(),
                column: "harvestSpeed".to_owned(),
                allowed: "id, inherit, requiredTool, props, texts, model, loot, hitLoot, flags"
                    .to_owned()
            })
        );
        assert!(render(&document, &build()).is_err());
    }

    #[test]
    fn a_vanished_column_fails() {
        let mut document = document();
        document["sheets"][0]["columns"] = serde_json::json!([{"name": "id"}]);

        assert_eq!(
            validate(&document),
            Err(InventoryError::MissingColumn {
                path: "gatherable".to_owned(),
                column: "inherit".to_owned()
            })
        );
    }

    #[test]
    fn a_vanished_sheet_fails() {
        let mut document = document();
        document["sheets"]
            .as_array_mut()
            .expect("sheets")
            .retain(|sheet| sheet.get("name").and_then(Value::as_str) != Some("zone@props"));

        assert_eq!(
            validate(&document),
            Err(InventoryError::MissingSheet { path: "zone@props" })
        );
    }

    #[test]
    fn an_unknown_row_key_fails_with_its_row() {
        let mut document = document();
        document["sheets"][0]["lines"][2]["props"]["newField"] = serde_json::json!(7);

        assert_eq!(
            validate(&document),
            Err(InventoryError::UnknownKey {
                path: "gatherable@props".to_owned(),
                id: "Ore_Copper_Small".to_owned(),
                key: "newField".to_owned(),
                allowed: "hitPoints, respawnTime, hitFX, affixes, size, affixChance, gatherSkill \
                          (row has: affixes, hitPoints, newField, respawnTime, size)"
                    .to_owned()
            })
        );
    }

    #[test]
    fn a_group_that_grew_rows_fails() {
        let mut document = document();
        document["sheets"][1]["lines"] = serde_json::json!([{"id": "stray"}]);

        assert!(matches!(
            validate(&document),
            Err(InventoryError::GroupHasRows { .. })
        ));
    }

    #[test]
    fn a_duplicate_id_fails() {
        let mut document = document();
        document["sheets"][0]["lines"]
            .as_array_mut()
            .expect("lines")
            .push(serde_json::json!({"id": "Ore"}));

        assert_eq!(
            validate(&document),
            Err(InventoryError::DuplicateId {
                path: "gatherable".to_owned(),
                id: "Ore".to_owned()
            })
        );
    }

    #[test]
    fn a_wrong_field_type_fails() {
        let mut document = document();
        document["sheets"][0]["lines"][2]["props"]["size"] = serde_json::json!("small");

        assert_eq!(
            render(&document, &build()),
            Err(InventoryError::WrongType {
                path: "data.cdb".to_owned(),
                id: "Ore_Copper_Small".to_owned(),
                field: "size",
                expected: "a signed integer",
                found: "a string"
            })
        );
    }

    #[test]
    #[cfg(feature = "embedded")]
    fn the_embedded_inventory_is_populated_and_self_consistent() {
        let source = Inventory::build();
        assert_eq!(source.cdb_checksum, 0xc383_de7f);
        assert!(source.sheets > 400, "{source:?}");

        let gatherables = Inventory::gatherables();
        assert!(gatherables.len() > 20, "{}", gatherables.len());
        assert!(gatherables.iter().all(|row| !row.id.is_empty()));
        assert!(gatherables
            .iter()
            .any(|row| row.id == "Madrigold_Small" && row.kind == GatherableKind::Plant));
        assert!(gatherables.iter().any(|row| row.id == "Ore"
            && row.kind == GatherableKind::Ore
            && row.inherit.is_none()));

        let zones = Inventory::zones();
        assert!(zones.len() > 100, "{}", zones.len());
        assert!(zones.iter().any(|zone| zone.id == "Z1_Region"));
    }

    #[test]
    #[cfg(feature = "embedded")]
    fn the_embedded_skills_carry_names_and_icon_crops() {
        let skills = Inventory::skills();
        assert!(skills.len() > 900, "{}", skills.len());
        assert!(
            skills.windows(2).all(|pair| pair[0].id < pair[1].id),
            "skill ids must be sorted for the binary search"
        );

        // A name written literally, and one that names another skill: both must
        // arrive as the name they end at.
        let combo = Inventory::skill("GS_Nova_Combo").expect("GS_Nova_Combo");
        assert_eq!(combo.name, Some("Mania"));
        let attack = Inventory::skill("Axe_Base_Attack").expect("Axe_Base_Attack");
        let icon = attack.icon.as_ref().expect("Axe_Base_Attack icon");
        assert_eq!((icon.width, icon.height), (96, 96), "{icon:?}");
        assert!(icon.atlas_path.ends_with(".png"), "{icon:?}");

        assert!(Inventory::skill("NoSuchSkill").is_none());

        // A skill whose name points at an icon: the icon table is what turns it
        // into text, so this is also the check that the two tables agree.
        let consume = Inventory::skill("ConsumeItem").expect("ConsumeItem");
        assert_eq!(consume.name, Some("Consume"));
    }

    #[test]
    #[cfg(feature = "embedded")]
    fn the_embedded_icons_carry_names_and_glyph_crops() {
        let icons = Inventory::icons();
        assert!(icons.len() > 200, "{}", icons.len());
        assert!(
            icons.windows(2).all(|pair| pair[0].id < pair[1].id),
            "icon ids must be sorted for the binary search"
        );

        let consume = Inventory::icon("Item_Consume").expect("Item_Consume");
        assert_eq!(consume.name, Some("Consume"));

        // Not every icon has a glyph of its own - some exist to carry a name -
        // so the crop is checked on whichever icon has one.
        let glyph = icons
            .iter()
            .find_map(|icon| icon.gfx.as_ref())
            .expect("at least one icon has a glyph");
        assert!(glyph.atlas_path.ends_with(".png"), "{glyph:?}");
        assert!(glyph.width > 0 && glyph.height > 0, "{glyph:?}");

        assert!(Inventory::icon("NoSuchIcon").is_none());
    }
}
