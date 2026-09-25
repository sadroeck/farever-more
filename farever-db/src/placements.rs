//! Placement data: importing the released map census into the inventory, and
//! projecting the inventory into the table the reference POI add-on compiles in.
//!
//! `data.cdb` holds no placements - its `element`, `spawner` and `activity`
//! sheets carry full schemas but zero lines in the shipped build, and the world
//! instances live in `res.map.pak` as per-tile `gameplayData/*.prefab`
//! documents. Until a reader for that tile format exists, the placements come
//! from a census of the map archive published with the farever-minimap release:
//!
//! * [`render_placements`] imports that census into
//!   `farever-db/assets/placements.rs`, the inventory's one placement table. It
//!   refuses anything it cannot describe, including a record whose kind
//!   contradicts what the game database says the prefab is;
//! * [`render_table`] projects the inventory into
//!   `addons/poi-database/assets/pois_w1_generated.rs`, because the add-on is a
//!   sandboxed Wasm component in its own workspace and cannot link this crate.
//!
//! Both are driven by `scripts/generate-game-data.ps1`; only the import needs
//! the released census file, and it is run when that release changes the data.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde::Deserialize;
use thiserror::Error;

use crate::inventory::{files, Artifact, GatherableKind, Inventory, Placement};

/// The world the released census covers.
///
/// The released file is per-world and does not name the world it was scanned
/// from, so this is the crate's declaration of which one it imported.
pub const CENSUS_WORLD: &str = "World/W1_Siagarta";

/// Label recorded in the imported artifact as its provenance.
pub const CENSUS_RELEASE: &str = "farever-minimap W1_Siagarta placement census";

/// The table the POI add-on compiles in.
pub const TABLE_FILE_NAME: &str = "pois_w1_generated.rs";

/// Prefix for the positional ids that replace census records without one.
const POSITIONAL_ID_PREFIX: &str = "w1";

/// One record of the placement census, exactly as the upstream release writes
/// it. Unknown fields are ignored so a newer release still loads.
#[derive(Clone, Debug, Deserialize)]
pub struct CensusRecord {
    pub kind: String,
    #[serde(default)]
    pub subkind: Option<String>,
    pub name: String,
    #[serde(default)]
    pub id: Option<String>,
    pub x: f32,
    pub y: f32,
    #[serde(default)]
    pub z: Option<f32>,
    /// Prefab the placement instantiates, e.g.
    /// `Gameplay/Prefabs/Gatherables/Plants/Z1/Madrigold_Small.prefab`.
    #[serde(default)]
    pub source: String,
    /// Tile the placement was found in, e.g. `L0_+0_+11`.
    #[serde(default)]
    pub source_tile: String,
}

/// Parses the census JSON.
///
/// # Errors
///
/// Fails when the bytes are not the census JSON this importer expects.
pub fn parse_census(bytes: &[u8]) -> Result<Vec<CensusRecord>, String> {
    serde_json::from_slice(bytes).map_err(|error| format!("parse the census: {error}"))
}

/// Why an import was refused.
///
/// Every problem found is reported, so one run tells the whole story instead of
/// revealing one mismatch per attempt.
#[derive(Debug, Error, PartialEq)]
#[error("{}", .0.join("; "))]
pub struct ImportError(Vec<String>);

impl ImportError {
    /// The problems, in the order they were found.
    #[must_use]
    pub fn problems(&self) -> &[String] {
        &self.0
    }
}

/// What importing a census found, and what the game database accounts for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Coverage {
    pub records: usize,
    /// Records whose name matched a `gatherable` the database defines.
    pub resolved: usize,
    /// Records the database does not define (placements only, such as
    /// activities named by the map itself).
    pub unresolved: usize,
    /// Resolved records whose kind the database either agrees with or does not
    /// constrain.
    pub kind_matches: usize,
    /// `(record, database kind, census kind)` for records the database defines
    /// as something else - the importer refuses to store these.
    pub kind_conflicts: Vec<(String, String, String)>,
}

impl Coverage {
    /// A one-line summary for the importer's output.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "records={} resolved={} unresolved={} kind_matches={} kind_conflicts={}",
            self.records,
            self.resolved,
            self.unresolved,
            self.kind_matches,
            self.kind_conflicts.len()
        )
    }
}

/// Imports a census into the inventory's placement artifact.
///
/// # Errors
///
/// Fails when a record has no kind or name, repeats an id the release assigned,
/// carries a non-finite position, or contradicts the kind the game database
/// gives the same prefab.
pub fn render_placements(records: &[CensusRecord]) -> Result<(Artifact, Coverage), ImportError> {
    let gatherables: HashMap<&str, GatherableKind> = Inventory::gatherables()
        .iter()
        .map(|gatherable| (gatherable.id, gatherable.kind))
        .collect();
    let mut coverage = Coverage {
        records: records.len(),
        ..Coverage::default()
    };
    let mut problems = Vec::new();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        if record.kind.is_empty() {
            problems.push(format!("record {index} has no kind"));
        }
        if record.name.is_empty() {
            problems.push(format!("record {index} has no name"));
        }
        if !record.x.is_finite() || !record.y.is_finite() {
            problems.push(format!(
                "record {index} ({:?}) is not at a finite position",
                record.name
            ));
        }
        if record.z.is_some_and(|z| !z.is_finite()) {
            problems.push(format!(
                "record {index} ({:?}) is not at a finite height",
                record.name
            ));
        }
        if let Some(id) = record.id.as_deref().filter(|id| !id.is_empty()) {
            if let Some(first) = seen.insert(id, index) {
                problems.push(format!(
                    "record {index} repeats id {id:?}, first used by record {first}"
                ));
            }
        }

        let Some(kind) = gatherables.get(record.name.as_str()).copied() else {
            coverage.unresolved += 1;
            continue;
        };
        coverage.resolved += 1;
        let defined = match kind {
            GatherableKind::Ore => Some("ore"),
            GatherableKind::Plant => Some("plant"),
            GatherableKind::Other => None,
        };
        match defined {
            None => coverage.kind_matches += 1,
            Some(defined) if defined == record.kind => coverage.kind_matches += 1,
            Some(defined) => {
                coverage.kind_conflicts.push((
                    record.name.clone(),
                    defined.to_owned(),
                    record.kind.clone(),
                ));
                problems.push(format!(
                    "record {index} ({:?}) is defined as {defined} by the game database but \
                     placed as {:?}",
                    record.name, record.kind
                ));
            }
        }
    }
    if !problems.is_empty() {
        return Err(ImportError(problems));
    }

    let mut contents = String::with_capacity(records.len() * 160);
    contents.push_str("// Generated by `cargo run -p farever-db --bin placements -- import`.\n");
    contents.push_str("// Do not edit by hand: re-import the released census instead.\n");
    contents.push_str("// Read through `farever_db::Inventory`, never directly.\n");
    contents.push_str(&format!("// Source: {CENSUS_RELEASE}\n"));
    contents.push_str("use super::{Placement, ReleaseSource};\n\n");
    contents.push_str("/// World every record below was placed in.\n");
    contents.push_str(&format!("pub const WORLD: &str = {CENSUS_WORLD:?};\n\n"));
    contents.push_str("/// The release the records were imported from.\n");
    contents.push_str("pub const RELEASE: ReleaseSource = ReleaseSource {\n");
    contents.push_str(&format!("    name: {CENSUS_RELEASE:?},\n"));
    contents.push_str(&format!("    records: {},\n", records.len()));
    contents.push_str("};\n\n");
    contents.push_str("/// Every placement the release records, in census order.\n");
    contents.push_str("pub static RECORDS: &[Placement] = &[\n");
    for record in records {
        contents.push_str(&format!(
            "    Placement {{ id: {}, kind: {:?}, family: {}, name: {:?}, prefab: {:?}, \
             tile: {:?}, x: {:?}, y: {:?}, z: {} }},\n",
            option_text(record.id.as_deref().filter(|id| !id.is_empty())),
            record.kind,
            option_text(
                record
                    .subkind
                    .as_deref()
                    .filter(|family| !family.is_empty())
            ),
            record.name,
            record.source,
            record.source_tile,
            record.x,
            record.y,
            option_f32(record.z),
        ));
    }
    contents.push_str("];\n");

    Ok((
        Artifact {
            file_name: files::PLACEMENTS,
            contents,
        },
        coverage,
    ))
}

/// One emitted table record: `(id, kind, family, name, x, y, z)`.
#[derive(Clone, Debug, PartialEq)]
pub struct TableRecord {
    pub id: String,
    pub kind: String,
    pub family: Option<String>,
    pub name: String,
    pub x: f32,
    pub y: f32,
    pub z: Option<f32>,
}

/// Projects the inventory's placements into table records.
///
/// A record keeps the id the release assigned it; where the release has none -
/// gatherables and most markers - a deterministic positional id stands in, so
/// the add-on's wire ids and the minimap's marker identity stay stable across
/// regenerations.
#[must_use]
pub fn build_table(records: &[Placement]) -> Vec<TableRecord> {
    records
        .iter()
        .enumerate()
        .map(|(index, record)| TableRecord {
            id: match record.id {
                Some(id) if !id.is_empty() => id.to_owned(),
                _ => format!("{POSITIONAL_ID_PREFIX}-{}-{index:04}", record.kind),
            },
            kind: record.kind.to_owned(),
            family: record.family.map(str::to_owned),
            name: record.name.to_owned(),
            x: record.x,
            y: record.y,
            z: record.z,
        })
        .collect()
}

/// Renders the generated Rust source for a table.
#[must_use]
pub fn render_table(records: &[TableRecord]) -> String {
    let mut source = String::with_capacity(records.len() * 96);
    source.push_str("// Generated by scripts/generate-game-data.ps1 from the placement table\n");
    source.push_str(&format!(
        "// farever-db embeds ({CENSUS_RELEASE}), imported with\n"
    ));
    source.push_str("// `cargo run -p farever-db --bin placements -- import`.\n");
    source.push_str("// Do not edit by hand: run the script instead.\n");
    source.push_str(
        "// Each record is (id, kind, family, name, x, y, z); ids the census leaves empty are\n",
    );
    source.push_str("// already replaced with deterministic positional ids.\n");
    let _ = writeln!(source, "pub const RECORD_COUNT: usize = {};", records.len());
    source.push_str(
        "pub const RECORDS: [(&str, &str, Option<&str>, &str, f32, f32, Option<f32>); RECORD_COUNT] = [\n",
    );
    for record in records {
        let family = match &record.family {
            Some(family) => format!("Some({family:?})"),
            None => "None".to_owned(),
        };
        let z = match record.z {
            Some(z) => format!("Some({z:?})"),
            None => "None".to_owned(),
        };
        let _ = writeln!(
            source,
            "    ({:?}, {:?}, {family}, {:?}, {:?}, {:?}, {z}),",
            record.id, record.kind, record.name, record.x, record.y
        );
    }
    source.push_str("];\n");
    source
}

/// Whether a committed table already matches generated source.
///
/// Line endings are the checkout's business, not the generator's, so the
/// comparison normalizes them.
#[must_use]
pub fn table_is_current(committed: &str, generated: &str) -> bool {
    committed.replace("\r\n", "\n") == generated
}

fn option_text(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("Some({value:?})"),
        None => "None".to_owned(),
    }
}

fn option_f32(value: Option<f32>) -> String {
    match value {
        Some(value) => format!("Some({value:?})"),
        None => "None".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn census_json() -> Vec<u8> {
        br#"[
            {"kind": "plant", "subkind": null, "name": "Madrigold_Small", "id": null,
             "x": -9.6961, "y": 1011.6821, "z": 83.2592,
             "source": "Gameplay/Prefabs/Gatherables/Plants/Z1/Madrigold_Small.prefab",
             "source_tile": "L0_+0_+11"},
            {"kind": "chest", "subkind": null, "name": "WorldChest", "id": "W1_Siagarta_WorldChest_24",
             "x": -0.6901, "y": 1093.1557, "z": 112.5, "source": "x.prefab", "source_tile": "L0_+0_+11"},
            {"kind": "activity", "subkind": "WorldElite", "name": "Schist Happens", "id": null,
             "x": -7.4176, "y": 1200.1156, "z": null, "source": "", "source_tile": "L0_+0_+15"}
        ]"#
        .to_vec()
    }

    fn census() -> Vec<CensusRecord> {
        parse_census(&census_json()).expect("parse")
    }

    fn record(name: &str, kind: &str) -> CensusRecord {
        CensusRecord {
            kind: kind.to_owned(),
            subkind: None,
            name: name.to_owned(),
            id: None,
            x: 1.0,
            y: 2.0,
            z: Some(3.0),
            source: "x.prefab".to_owned(),
            source_tile: "L0_+0_+11".to_owned(),
        }
    }

    #[test]
    fn an_import_renders_the_placement_table_in_census_order() {
        let (artifact, coverage) = render_placements(&census()).expect("import");

        assert_eq!(artifact.file_name, files::PLACEMENTS);
        assert!(artifact
            .contents
            .contains("pub const WORLD: &str = \"World/W1_Siagarta\";"));
        assert!(artifact.contents.contains("records: 3,"));
        assert!(artifact.contents.contains(
            "    Placement { id: None, kind: \"plant\", family: None, name: \"Madrigold_Small\", \
             prefab: \"Gameplay/Prefabs/Gatherables/Plants/Z1/Madrigold_Small.prefab\", \
             tile: \"L0_+0_+11\", x: -9.6961, y: 1011.6821, z: Some(83.2592) },"
        ));
        assert!(artifact.contents.contains("z: None },"));

        assert_eq!(coverage.records, 3);
        assert_eq!(coverage.resolved, 1, "only the gatherable is defined");
        assert_eq!(coverage.unresolved, 2, "the chest and the activity are not");
        assert!(coverage.summary().contains("records=3"));
    }

    #[test]
    fn an_import_refuses_a_record_without_a_kind_or_name() {
        let mut records = census();
        records[1].kind = String::new();
        records[2].name = String::new();

        let error = render_placements(&records).expect_err("refused");
        assert_eq!(
            error.problems(),
            [
                "record 1 has no kind".to_owned(),
                "record 2 has no name".to_owned()
            ]
        );
    }

    #[test]
    fn an_import_refuses_a_repeated_id() {
        let mut records = census();
        records[2].id = records[1].id.clone();

        let error = render_placements(&records).expect_err("refused");
        assert_eq!(
            error.problems(),
            [
                "record 2 repeats id \"W1_Siagarta_WorldChest_24\", first used by record 1"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn an_import_refuses_a_position_the_map_cannot_have() {
        let mut records = census();
        records[1].y = f32::NAN;

        let error = render_placements(&records).expect_err("refused");
        assert_eq!(
            error.problems(),
            ["record 1 (\"WorldChest\") is not at a finite position".to_owned()]
        );
    }

    #[test]
    fn an_import_refuses_a_kind_the_database_contradicts() {
        let records = [record("Madrigold_Small", "ore")];

        let error = render_placements(&records).expect_err("refused");
        assert_eq!(
            error.problems(),
            [
                "record 0 (\"Madrigold_Small\") is defined as plant by the game database but \
              placed as \"ore\""
                    .to_owned()
            ]
        );
    }

    #[test]
    fn a_gatherable_the_database_does_not_classify_is_accepted() {
        let (_, coverage) = render_placements(&[record("Ore", "ore")]).expect("import");

        assert_eq!(coverage.resolved, 1);
        assert_eq!(coverage.kind_matches, 1);
        assert!(coverage.kind_conflicts.is_empty(), "{coverage:?}");
    }

    #[test]
    fn the_embedded_placements_agree_with_the_game_database() {
        let placements = Inventory::placements();
        assert_eq!(placements.len(), Inventory::placement_release().records);
        assert!(placements.len() > 1000, "{}", placements.len());
        assert_eq!(Inventory::placement_world(), CENSUS_WORLD);

        let conflicting: Vec<&str> = placements
            .iter()
            .filter(|placement| {
                Inventory::gatherables()
                    .iter()
                    .find(|gatherable| gatherable.id == placement.name)
                    .is_some_and(|gatherable| {
                        matches!(
                            (gatherable.kind, placement.kind),
                            (GatherableKind::Ore, "plant") | (GatherableKind::Plant, "ore")
                        )
                    })
            })
            .map(|placement| placement.name)
            .collect();
        assert!(conflicting.is_empty(), "{conflicting:?}");
    }

    #[test]
    fn positional_ids_replace_only_the_records_without_one() {
        let records = build_table(&[
            placement(
                None,
                "plant",
                None,
                "Madrigold_Small",
                -9.6961,
                1011.6821,
                Some(83.2592),
            ),
            placement(
                Some("W1_Siagarta_WorldChest_24"),
                "chest",
                None,
                "WorldChest",
                -0.6901,
                1093.1557,
                Some(112.5),
            ),
            placement(
                None,
                "activity",
                Some("WorldElite"),
                "Schist Happens",
                -7.4176,
                1200.1156,
                None,
            ),
        ]);

        assert_eq!(records[0].id, "w1-plant-0000");
        assert_eq!(records[1].id, "W1_Siagarta_WorldChest_24");
        assert_eq!(records[2].id, "w1-activity-0002");
        assert_eq!(records[2].family.as_deref(), Some("WorldElite"));
        assert_eq!(records[0].family, None);
        assert_eq!(records[2].z, None);
    }

    #[test]
    fn rendered_source_matches_the_committed_shape() {
        let table = build_table(&[
            placement(
                None,
                "plant",
                None,
                "Madrigold_Small",
                -9.6961,
                1011.6821,
                Some(83.2592),
            ),
            placement(
                None,
                "activity",
                Some("WorldElite"),
                "Schist Happens",
                -7.4176,
                1200.1156,
                None,
            ),
        ]);
        let source = render_table(&table);

        assert!(source.starts_with("// Generated by scripts/generate-game-data.ps1"));
        assert!(source.contains("pub const RECORD_COUNT: usize = 2;"));
        assert!(source.contains(
            "pub const RECORDS: [(&str, &str, Option<&str>, &str, f32, f32, Option<f32>); RECORD_COUNT] = ["
        ));
        assert!(source.contains(
            "    (\"w1-plant-0000\", \"plant\", None, \"Madrigold_Small\", -9.6961, 1011.6821, Some(83.2592)),"
        ));
        assert!(source.contains(
            "    (\"w1-activity-0001\", \"activity\", Some(\"WorldElite\"), \"Schist Happens\", -7.4176, 1200.1156, None),"
        ));
        assert!(source.ends_with("];\n"));
    }

    #[test]
    fn a_committed_table_is_compared_without_line_ending_noise() {
        let table = build_table(Inventory::placements());
        let source = render_table(&table);

        assert!(table_is_current(&source.replace('\n', "\r\n"), &source));
        assert!(!table_is_current(
            &source.replace("Madrigold_Small", "Zealotus"),
            &source
        ));
    }

    fn placement(
        id: Option<&'static str>,
        kind: &'static str,
        family: Option<&'static str>,
        name: &'static str,
        x: f32,
        y: f32,
        z: Option<f32>,
    ) -> Placement {
        Placement {
            id,
            kind,
            family,
            name,
            prefab: "Gameplay/Prefabs/x.prefab",
            tile: "L0_+0_+11",
            x,
            y,
            z,
        }
    }
}
