//! Prototype POI database provider backed by a coordinate grid index, so
//! scans walk only overlapped cells and materialize only the requested page.

use farever_more_sdk::prelude::*;
use farever_poi_protocol::{
    decode_nearby, decode_query, encode_page, query_nearby_indexed, query_region_indexed, Poi,
    PoiFamilyRef, PoiIndex, PoiKindRef, QUERY_NEARBY_OPERATION, QUERY_REGION_OPERATION, SERVICE_ID,
};

struct PoiDatabase {
    revision: u64,
    pois: Vec<Poi>,
    index: PoiIndex,
}

impl Addon for PoiDatabase {
    fn activate(context: &mut ActivateContext) -> SdkResult<Self> {
        let pois = load_w1_pois();
        let index = PoiIndex::build(&pois);
        context
            .log()
            .info(&format!("POI database activated records={}", pois.len()));
        Ok(Self {
            revision: 1,
            pois,
            index,
        })
    }

    fn call_service(
        &mut self,
        service: &str,
        operation: u32,
        request: &[u8],
    ) -> SdkResult<Vec<u8>> {
        if service != SERVICE_ID {
            return Err(format!("unknown service {service:?}"));
        }
        let page = match operation {
            QUERY_REGION_OPERATION => {
                let query = decode_query(request)?;
                query_region_indexed(&self.pois, &self.index, self.revision, &query)?
            }
            QUERY_NEARBY_OPERATION => {
                let query = decode_nearby(request)?;
                query_nearby_indexed(&self.pois, &self.index, self.revision, &query)?
            }
            _ => return Err(format!("unknown poi operation {operation}")),
        };
        encode_page(&page)
    }
}

/// Bundled W1 dataset projected from farever-db's placement table (the
/// farever-minimap W1_Siagarta census, 1224 records) and eight reviewed demon sites into
/// `assets/pois_w1_generated.rs` by `scripts/generate-game-data.ps1`. The table
/// is static on purpose: parsing it inside the guest would blow the host's
/// 25M-fuel activation budget, while data-segment initialization is already
/// covered by it. The released census is per-world, so every record belongs to
/// `World/W1_Siagarta` - the same world farever-db records for that table.
const W1_WORLD: &str = "World/W1_Siagarta";

include!("../assets/pois_w1_generated.rs");

fn load_w1_pois() -> Vec<Poi> {
    RECORDS
        .iter()
        .map(|(id, kind, family, name, x, y, z)| {
            // The family refines an activity for consumers whose art differs
            // per family. Visibility, filtering and queries stay on the kind.
            let poi = Poi::new(PoiKindRef::parse(kind), *id, *name, W1_WORLD, *x, *y, *z);
            match family {
                Some(family) => poi.with_family(PoiFamilyRef::parse(family)),
                None => poi,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use farever_poi_protocol::{
        decode_nearby, distance_squared, encode_nearby, query_nearby_indexed, query_region_indexed,
        Bounds, NearbyQuery, PoiFamily, PoiFamilyRef, PoiKind, PoiPage, QueryRegion,
    };

    fn database() -> PoiDatabase {
        let pois = load_w1_pois();
        let index = PoiIndex::build(&pois);
        PoiDatabase {
            revision: 1,
            pois,
            index,
        }
    }

    fn region(
        min_x: f32,
        min_y: f32,
        max_x: f32,
        max_y: f32,
        kinds: &[PoiKind],
        limit: u32,
        offset: u32,
    ) -> QueryRegion {
        QueryRegion::new(
            W1_WORLD,
            Bounds {
                min_x,
                min_y,
                max_x,
                max_y,
            },
            limit,
        )
        .with_kinds(kinds.iter().copied())
        .with_offset(offset)
    }

    fn ids(page: &PoiPage) -> Vec<&str> {
        page.pois.iter().map(|poi| poi.id.as_str()).collect()
    }

    #[test]
    fn generated_table_is_well_formed() {
        // The table's provenance is checked where it is projected, in
        // farever-db; what a guest can still check is that the data it
        // compiles in is usable: identified, named, and at real coordinates.
        assert_eq!(RECORDS.len(), RECORD_COUNT);
        let mut ids = Vec::with_capacity(RECORDS.len());
        for (index, (id, kind, _family, name, x, y, z)) in RECORDS.iter().enumerate() {
            assert!(!id.is_empty(), "record {index} has no id");
            assert!(!kind.is_empty(), "record {index} has no kind");
            assert!(!name.is_empty(), "record {index} has no name");
            assert!(x.is_finite() && y.is_finite(), "record {index} is off-map");
            assert!(z.is_none_or(f32::is_finite), "record {index} is off-map");
            assert!(!ids.contains(id), "record {index} repeats id {id}");
            ids.push(id);
        }
    }

    #[test]
    fn bundled_dataset_contains_the_census_and_reviewed_demons() {
        // Independent count of the farever-minimap W1_Siagarta census: the
        // loader must carry every record with a usable identity. Counts are
        // keyed by the shared kind enum, so a kind the crate does not name
        // fails here instead of reaching consumers as a stray string.
        let pois = load_w1_pois();
        assert_eq!(pois.len(), 1232);
        let mut total = 0;
        for (kind, expected) in [
            (PoiKind::Plant, 313),
            (PoiKind::RedOrb, 283),
            (PoiKind::Ore, 263),
            (PoiKind::Chest, 177),
            (PoiKind::Activity, 132),
            (PoiKind::Respawn, 29),
            (PoiKind::Dungeon, 12),
            (PoiKind::Obelisk, 11),
            (PoiKind::Merchant, 4),
            (PoiKind::Soulstone, 8),
        ] {
            let found = pois
                .iter()
                .filter(|poi| poi.kind.known() == Some(kind))
                .count();
            assert_eq!(found, expected, "kind {kind}");
            total += found;
        }
        assert_eq!(total, pois.len(), "the census must cover every record");
        // Every activity carries exactly one shared family, with the counts the
        // release data has, and no other kind carries one.
        for (family, expected) in [
            (PoiFamily::WorldElite, 42),
            (PoiFamily::FightStone, 27),
            (PoiFamily::ChestOrb, 23),
            (PoiFamily::TimerCollectRun, 18),
            (PoiFamily::Ascension, 8),
            (PoiFamily::WorldCamp, 6),
            (PoiFamily::WorldPlant, 6),
            (PoiFamily::MountRush, 2),
        ] {
            let found = pois
                .iter()
                .filter(|poi| poi.family.as_ref().and_then(PoiFamilyRef::known) == Some(family))
                .count();
            assert_eq!(found, expected, "family {family}");
        }
        let activities = pois
            .iter()
            .filter(|poi| poi.kind.known() == Some(PoiKind::Activity))
            .count();
        let classified = pois.iter().filter(|poi| poi.family.is_some()).count();
        assert_eq!(classified, activities, "every activity names a family");
        assert!(
            pois.iter()
                .all(|poi| poi.kind.known() == Some(PoiKind::Activity) || poi.family.is_none()),
            "only activities carry a family"
        );
        assert!(
            pois.iter().all(|poi| poi.kind.known().is_some()),
            "the bundled table carries a kind this build cannot name"
        );
        assert!(pois
            .iter()
            .all(|poi| poi.world == W1_WORLD && poi.x.is_finite() && poi.y.is_finite()));
        // The released dataset gives gatherables no stable id; the
        // synthesized positional ids must still be unique.
        let mut id_set: Vec<&str> = pois.iter().map(|poi| poi.id.as_str()).collect();
        id_set.sort_unstable();
        id_set.dedup();
        assert_eq!(id_set.len(), pois.len());
        // A known record keeps its released identity and coordinates.
        let obelisk = pois
            .iter()
            .find(|poi| poi.id == "Z1_World_Greenlands_Obelisk_3")
            .expect("obelisk record");
        assert_eq!(obelisk.kind.known(), Some(PoiKind::Obelisk));
        assert_eq!((obelisk.x, obelisk.y), (-395.0, 1460.0));
    }

    #[test]
    fn region_takes_whole_cells_filtered_by_world_and_type() {
        let database = database();
        // Tight window around a Greenlands obelisk at (-395, 1460): the
        // overlapped cells may contribute neighbours too, but the obelisk
        // itself must be present. Over-inclusion is bounded by one cell.
        let page = query_region_indexed(
            &database.pois,
            &database.index,
            1,
            &region(-400.0, 1455.0, -390.0, 1465.0, &[], 100, 0),
        )
        .unwrap();
        assert!(ids(&page).contains(&"Z1_World_Greenlands_Obelisk_3"));
        // The type filter still applies within taken cells.
        let obelisks = query_region_indexed(
            &database.pois,
            &database.index,
            1,
            &region(-400.0, 1455.0, -390.0, 1465.0, &[PoiKind::Obelisk], 100, 0),
        )
        .unwrap();
        assert!(!obelisks.pois.is_empty());
        assert!(obelisks
            .pois
            .iter()
            .all(|poi| poi.kind.known() == Some(PoiKind::Obelisk)));
        // A full-map scan totals the whole table.
        let full = query_region_indexed(
            &database.pois,
            &database.index,
            1,
            &region(-100_000.0, -100_000.0, 100_000.0, 100_000.0, &[], 1_024, 0),
        )
        .unwrap();
        assert_eq!(full.total, 1232);
    }

    #[test]
    fn origin_buffer_pages_through_without_hitting_the_safety_cap() {
        // The minimap seeds its buffer in a +/-1500 window around the
        // origin. 1024 census records sit strictly inside; whole-cell takes
        // add boundary cells plus seven demon sites for 1112 total. The
        // 64-record pages walk it in 18 pages, below the fetch safety cap.
        let database = database();
        let window = |offset| {
            query_region_indexed(
                &database.pois,
                &database.index,
                1,
                &region(-1500.0, -1500.0, 1500.0, 1500.0, &[], 64, offset),
            )
            .unwrap()
        };
        let first = window(0);
        assert_eq!(first.total, 1112);
        assert_eq!(first.pois.len(), 64);
        assert!(first.truncated);
        let last = window(1088);
        assert_eq!(last.pois.len(), 24);
        assert!(!last.truncated);
    }

    #[test]
    fn nearby_returns_nearest_first_through_pages() {
        let database = database();
        let nearby = |offset: u32, limit: u32| {
            query_nearby_indexed(
                &database.pois,
                &database.index,
                1,
                &NearbyQuery::new(W1_WORLD, -395.0, 1460.0, 500.0, limit).with_offset(offset),
            )
            .unwrap()
        };
        // The Greenlands obelisk sits exactly at the center: always first.
        let first = nearby(0, 2);
        assert_eq!(first.pois[0].id, "Z1_World_Greenlands_Obelisk_3");
        let rest = nearby(2, 1_024);
        assert_eq!(first.total as usize, first.pois.len() + rest.pois.len());
        let mut previous = 0.0f32;
        for offset in [0u32, 2, 4] {
            for poi in &nearby(offset, 2).pois {
                let distance = distance_squared(-395.0, 1460.0, None, poi);
                assert!(distance >= previous);
                previous = distance;
            }
        }
        // Radius validation and unknown operations still fail loudly.
        let query = NearbyQuery::new(W1_WORLD, -395.0, 1460.0, 0.0, 2);
        let invalid = encode_nearby(&query).unwrap();
        assert!(decode_nearby(&invalid).is_err());
        let mut addon = database;
        assert!(addon.call_service(SERVICE_ID, 99, &[]).is_err());
        assert!(addon
            .call_service("other", QUERY_REGION_OPERATION, &[])
            .is_err());
    }
}

farever_more_sdk::export!(PoiDatabase);
