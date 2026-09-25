#![warn(clippy::doc_markdown, clippy::missing_errors_doc)]

//! Read-only Farever game-data access and offline loot modeling.
//!
//! The game's data is compiled in as generated tables (`assets/`, written by
//! `extract-db`). Building with `--no-default-features` leaves those out, which
//! is what lets the extractor run when a table no longer matches the types it
//! is generated against; consumers always take the default features.

pub mod cdb;
pub mod inventory;
#[cfg(feature = "embedded")]
pub mod loot;
pub mod pak;
#[cfg(feature = "embedded")]
pub mod placements;

pub use cdb::{discover_game, BuildFingerprint, CdbSummary, GameInstall};
pub use inventory::{
    Achievement, Aptitude, Artifact, BuildSource, Craft, Gatherable, GatherableKind, Icon,
    IconCrop, Ingredient, Inventory, Item, ItemType, LootEntry, LootTable, Placement, Rarity,
    RarityBracket, ReleaseSource, SheetSpec, Skill, Unit, UnitType, Zone,
};
#[cfg(feature = "embedded")]
pub use loot::{AcquisitionSource, RarityChance, CLASSES};
