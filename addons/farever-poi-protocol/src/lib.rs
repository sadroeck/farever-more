//! Typed client and bounded binary wire contract for the prototype POI service.

use farever_more_sdk::dependencies::{CallError, Dependencies, OpenError, Service};
use std::fmt;

pub const SERVICE_ID: &str = "poi";
pub const QUERY_REGION_OPERATION: u32 = 1;
pub const QUERY_NEARBY_OPERATION: u32 = 2;
const WIRE_VERSION: u16 = 3;
const MAX_TEXT_BYTES: usize = 4_096;
pub const MAX_POIS: usize = 1_024;
/// Uniform grid cell size in world units for provider-side coordinate
/// indexes: roughly a viewport across, so scans over-include at most a
/// one-cell fringe while the cell table stays small.
pub const INDEX_CELL: f32 = 250.0;
/// Upper bound on cells walked per axis in one indexed scan, so absurd
/// bounds fail instead of hanging the provider.
const INDEX_MAX_CELLS_PER_AXIS: i64 = 4_096;

/// Canonical POI kinds. [`PoiKind::id`] is the wire form, so this enum is the
/// one place the names are written down: providers and consumers go through it
/// instead of repeating string literals and drifting apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PoiKind {
    Obelisk,
    Merchant,
    Dungeon,
    Respawn,
    Chest,
    RedOrb,
    Plant,
    Ore,
    Activity,
    Soulstone,
}

impl PoiKind {
    /// Every kind, in the order consumers present them.
    pub const ALL: [PoiKind; 10] = [
        PoiKind::Obelisk,
        PoiKind::Merchant,
        PoiKind::Dungeon,
        PoiKind::Respawn,
        PoiKind::Chest,
        PoiKind::RedOrb,
        PoiKind::Plant,
        PoiKind::Ore,
        PoiKind::Activity,
        PoiKind::Soulstone,
    ];

    /// Stable wire id. Renaming one is a data change, not a cosmetic one.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            PoiKind::Obelisk => "obelisk",
            PoiKind::Merchant => "merchant",
            PoiKind::Dungeon => "dungeon",
            PoiKind::Respawn => "respawn",
            PoiKind::Chest => "chest",
            PoiKind::RedOrb => "red_orb",
            PoiKind::Plant => "plant",
            PoiKind::Ore => "ore",
            PoiKind::Activity => "activity",
            PoiKind::Soulstone => "soulstone",
        }
    }

    /// Position in [`PoiKind::ALL`], for tables indexed by kind.
    #[must_use]
    pub const fn ordinal(self) -> usize {
        match self {
            PoiKind::Obelisk => 0,
            PoiKind::Merchant => 1,
            PoiKind::Dungeon => 2,
            PoiKind::Respawn => 3,
            PoiKind::Chest => 4,
            PoiKind::RedOrb => 5,
            PoiKind::Plant => 6,
            PoiKind::Ore => 7,
            PoiKind::Activity => 8,
            PoiKind::Soulstone => 9,
        }
    }

    /// Classifies a wire id, or `None` for a kind this build does not know.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.id() == id)
    }
}

impl fmt::Display for PoiKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.id())
    }
}

/// A POI kind as data carries it: a kind this build knows, or the raw id of a
/// newer provider's kind. Unknown ids are preserved rather than dropped, so
/// consumers can keep showing records they cannot classify.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PoiKindRef {
    Known(PoiKind),
    Other(String),
}

impl PoiKindRef {
    /// Classifies a wire id without ever losing it.
    #[must_use]
    pub fn parse(id: &str) -> Self {
        match PoiKind::parse(id) {
            Some(kind) => Self::Known(kind),
            None => Self::Other(id.to_owned()),
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Known(kind) => kind.id(),
            Self::Other(id) => id,
        }
    }

    /// The known kind, or `None` when this build cannot classify the record.
    #[must_use]
    pub fn known(&self) -> Option<PoiKind> {
        match self {
            Self::Known(kind) => Some(*kind),
            Self::Other(_) => None,
        }
    }

    /// Whether a kind filter selects this kind. An empty filter takes every
    /// kind; otherwise the wire ids must match, so a filter naming a kind this
    /// provider does not know is honoured instead of silently widening the page.
    #[must_use]
    pub fn selected_by(&self, filter: &[Self]) -> bool {
        filter.is_empty() || filter.iter().any(|candidate| candidate.id() == self.id())
    }
}

impl From<PoiKind> for PoiKindRef {
    fn from(kind: PoiKind) -> Self {
        Self::Known(kind)
    }
}

impl fmt::Display for PoiKindRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.id())
    }
}

/// Canonical activity families: the game's own activity categories, which the
/// world map draws with different art. A family is a category of the records
/// inside [`PoiKind::Activity`] - every activity has exactly one, and no other
/// kind carries one. As with [`PoiKind`], [`PoiFamily::id`] is the wire form
/// and the single place the names are written down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PoiFamily {
    Ascension,
    ChestOrb,
    FightStone,
    MountRush,
    TimerCollectRun,
    WorldCamp,
    WorldElite,
    WorldPlant,
}

impl PoiFamily {
    /// Every family, in the order consumers present them.
    pub const ALL: [PoiFamily; 8] = [
        PoiFamily::Ascension,
        PoiFamily::ChestOrb,
        PoiFamily::FightStone,
        PoiFamily::MountRush,
        PoiFamily::TimerCollectRun,
        PoiFamily::WorldCamp,
        PoiFamily::WorldElite,
        PoiFamily::WorldPlant,
    ];

    /// Stable wire id, matching the family name the source data carries.
    /// Renaming one is a data change, not a cosmetic one.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            PoiFamily::Ascension => "Ascension",
            PoiFamily::ChestOrb => "ChestOrb",
            PoiFamily::FightStone => "FightStone",
            PoiFamily::MountRush => "MountRush",
            PoiFamily::TimerCollectRun => "TimerCollectRun",
            PoiFamily::WorldCamp => "WorldCamp",
            PoiFamily::WorldElite => "WorldElite",
            PoiFamily::WorldPlant => "WorldPlant",
        }
    }

    /// Position in [`PoiFamily::ALL`], for tables indexed by family.
    #[must_use]
    pub const fn ordinal(self) -> usize {
        match self {
            PoiFamily::Ascension => 0,
            PoiFamily::ChestOrb => 1,
            PoiFamily::FightStone => 2,
            PoiFamily::MountRush => 3,
            PoiFamily::TimerCollectRun => 4,
            PoiFamily::WorldCamp => 5,
            PoiFamily::WorldElite => 6,
            PoiFamily::WorldPlant => 7,
        }
    }

    /// Classifies a wire id, or `None` for a family this build does not know.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|family| family.id() == id)
    }
}

impl fmt::Display for PoiFamily {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.id())
    }
}

/// A family as data carries it: one this build knows, or the raw id of a newer
/// provider's family. Unknown ids are preserved rather than dropped, so a
/// consumer can still group the record instead of losing the detail.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PoiFamilyRef {
    Known(PoiFamily),
    Other(String),
}

impl PoiFamilyRef {
    /// Classifies a wire id without ever losing it.
    #[must_use]
    pub fn parse(id: &str) -> Self {
        match PoiFamily::parse(id) {
            Some(family) => Self::Known(family),
            None => Self::Other(id.to_owned()),
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Known(family) => family.id(),
            Self::Other(id) => id,
        }
    }

    /// The known family, or `None` when this build cannot classify the record.
    #[must_use]
    pub fn known(&self) -> Option<PoiFamily> {
        match self {
            Self::Known(family) => Some(*family),
            Self::Other(_) => None,
        }
    }
}

impl From<PoiFamily> for PoiFamilyRef {
    fn from(family: PoiFamily) -> Self {
        Self::Known(family)
    }
}

impl fmt::Display for PoiFamilyRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.id())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Bounds {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}
impl Bounds {
    #[must_use]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueryRegion {
    pub world: String,
    pub bounds: Bounds,
    /// Kind filter: an empty list takes every kind.
    pub kinds: Vec<PoiKindRef>,
    pub limit: u32,
    /// Records to skip before the page. Lets consumers walk result sets
    /// larger than one response without raising transfer caps.
    pub offset: u32,
}

impl QueryRegion {
    #[must_use]
    pub fn new(world: impl Into<String>, bounds: Bounds, limit: u32) -> Self {
        Self {
            world: world.into(),
            bounds,
            kinds: Vec::new(),
            limit,
            offset: 0,
        }
    }

    /// Restricts the query to these kinds. Accepts [`PoiKind`] values, so a
    /// typo cannot reach the provider.
    #[must_use]
    pub fn with_kinds<K: Into<PoiKindRef>>(mut self, kinds: impl IntoIterator<Item = K>) -> Self {
        self.kinds = kinds.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn with_offset(mut self, offset: u32) -> Self {
        self.offset = offset;
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Poi {
    pub id: String,
    /// Kind as the provider declares it. Unknown kinds keep their raw id.
    pub kind: PoiKindRef,
    /// Activity family, present on every activity record and on nothing else.
    /// Visibility, filtering and queries stay on the kind; the family only
    /// refines how a consumer draws or groups the record.
    pub family: Option<PoiFamilyRef>,
    pub name: String,
    pub world: String,
    pub x: f32,
    pub y: f32,
    pub z: Option<f32>,
}

impl Poi {
    /// Record for a provider. Pass a [`PoiKind`] for a known kind, or a
    /// [`PoiKindRef::Other`] id for one this build cannot name yet.
    #[must_use]
    pub fn new(
        kind: impl Into<PoiKindRef>,
        id: impl Into<String>,
        name: impl Into<String>,
        world: impl Into<String>,
        x: f32,
        y: f32,
        z: Option<f32>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            family: None,
            name: name.into(),
            world: world.into(),
            x,
            y,
            z,
        }
    }

    /// Names the record's activity family. The kind still owns visibility,
    /// filtering and queries.
    #[must_use]
    pub fn with_family(mut self, family: impl Into<PoiFamilyRef>) -> Self {
        self.family = Some(family.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PoiPage {
    pub revision: u64,
    pub pois: Vec<Poi>,
    pub truncated: bool,
    /// Total matches before paging, so consumers know when to stop.
    pub total: u32,
}

/// Distance scan around a point. Matches are nearest-first, so pages walk
/// outward from the center.
#[derive(Clone, Debug, PartialEq)]
pub struct NearbyQuery {
    pub world: String,
    pub x: f32,
    pub y: f32,
    /// When both the query and the record carry depth, distance is 3D;
    /// otherwise it falls back to planar distance.
    pub z: Option<f32>,
    pub radius: f32,
    /// Kind filter: an empty list takes every kind.
    pub kinds: Vec<PoiKindRef>,
    pub limit: u32,
    pub offset: u32,
}

impl NearbyQuery {
    #[must_use]
    pub fn new(world: impl Into<String>, x: f32, y: f32, radius: f32, limit: u32) -> Self {
        Self {
            world: world.into(),
            x,
            y,
            z: None,
            radius,
            kinds: Vec::new(),
            limit,
            offset: 0,
        }
    }

    /// Adds the query depth; distances become 3D against records that carry
    /// their own depth.
    #[must_use]
    pub fn with_depth(mut self, z: f32) -> Self {
        self.z = Some(z);
        self
    }

    /// Restricts the query to these kinds. Accepts [`PoiKind`] values, so a
    /// typo cannot reach the provider.
    #[must_use]
    pub fn with_kinds<K: Into<PoiKindRef>>(mut self, kinds: impl IntoIterator<Item = K>) -> Self {
        self.kinds = kinds.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn with_offset(mut self, offset: u32) -> Self {
        self.offset = offset;
        self
    }
}

#[derive(Clone, Debug)]
pub struct Client {
    service: Service,
}

impl Client {
    /// Opens the POI service from the provider named in the consumer's
    /// manifest (`poi-database` for the shipped provider). The service version
    /// consumers see is that add-on's own version.
    pub fn open(dependencies: &Dependencies, addon: &str) -> Result<Self, Error> {
        let service = dependencies.open(addon, SERVICE_ID)?;
        Ok(Self { service })
    }

    #[must_use]
    pub fn version(&self) -> &str {
        self.service.version()
    }

    pub fn query_region(&self, query: &QueryRegion) -> Result<PoiPage, Error> {
        let request = encode_query(query).map_err(Error::Codec)?;
        let response = self.service.call(QUERY_REGION_OPERATION, &request)?;
        decode_page(&response).map_err(Error::Codec)
    }
}

/// Shared query execution for providers: cell-granular inclusion (every
/// record in an overlapped cell, still filtered by world and type) with only
/// the requested page materialized. Page order is deterministic per table
/// revision: row-major cells, insertion order within a cell.
pub fn query_region_indexed(
    pois: &[Poi],
    index: &PoiIndex,
    revision: u64,
    query: &QueryRegion,
) -> Result<PoiPage, String> {
    let limit = validate_limit(query.limit)?;
    let ordinals = index
        .scan_bounds(
            query.bounds.min_x,
            query.bounds.min_y,
            query.bounds.max_x,
            query.bounds.max_y,
        )?
        .into_iter()
        .filter(|&ordinal| {
            let poi = &pois[ordinal];
            poi.world == query.world && poi.kind.selected_by(&query.kinds)
        })
        .collect::<Vec<_>>();
    page_from_ordinals(pois, ordinals, revision, query.offset, limit)
}

/// Shared nearest-first execution for providers. Radius selects whole
/// overlapped cells (no exact distance cutoff); matches sort nearest-first
/// so pages walk outward from the center.
pub fn query_nearby_indexed(
    pois: &[Poi],
    index: &PoiIndex,
    revision: u64,
    query: &NearbyQuery,
) -> Result<PoiPage, String> {
    let limit = validate_limit(query.limit)?;
    let mut scored = index
        .scan_bounds(
            query.x - query.radius,
            query.y - query.radius,
            query.x + query.radius,
            query.y + query.radius,
        )?
        .into_iter()
        .filter(|&ordinal| {
            let poi = &pois[ordinal];
            poi.world == query.world && poi.kind.selected_by(&query.kinds)
        })
        .map(|ordinal| {
            (
                distance_squared(query.x, query.y, query.z, &pois[ordinal]),
                ordinal,
            )
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| left.0.total_cmp(&right.0));
    let ordinals = scored.into_iter().map(|(_, ordinal)| ordinal).collect();
    page_from_ordinals(pois, ordinals, revision, query.offset, limit)
}

fn validate_limit(limit: u32) -> Result<usize, String> {
    if limit == 0 || limit as usize > MAX_POIS {
        return Err(format!("query limit must be between 1 and {MAX_POIS}"));
    }
    Ok(limit as usize)
}

fn page_from_ordinals(
    pois: &[Poi],
    ordinals: Vec<usize>,
    revision: u64,
    offset: u32,
    limit: usize,
) -> Result<PoiPage, String> {
    let total = u32::try_from(ordinals.len()).map_err(|_| "too many POI matches".to_owned())?;
    let start = (offset as usize).min(ordinals.len());
    let end = start.saturating_add(limit).min(ordinals.len());
    let page = ordinals[start..end]
        .iter()
        .map(|&ordinal| pois[ordinal].clone())
        .collect::<Vec<_>>();
    Ok(PoiPage {
        revision,
        truncated: end < ordinals.len(),
        total,
        pois: page,
    })
}

/// Encodes a region query for provider-side indexed scans.
pub fn encode_query(query: &QueryRegion) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    push_u16(&mut bytes, WIRE_VERSION);
    push_text(&mut bytes, &query.world)?;
    for value in [
        query.bounds.min_x,
        query.bounds.min_y,
        query.bounds.max_x,
        query.bounds.max_y,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    push_kinds(&mut bytes, &query.kinds)?;
    bytes.extend_from_slice(&query.limit.to_le_bytes());
    bytes.extend_from_slice(&query.offset.to_le_bytes());
    Ok(bytes)
}

/// Decodes a region query for provider-side indexed scans.
pub fn decode_query(bytes: &[u8]) -> Result<QueryRegion, String> {
    let mut reader = Reader::new(bytes);
    expect_version(&mut reader)?;
    let world = reader.text()?;
    let bounds = Bounds {
        min_x: reader.f32()?,
        min_y: reader.f32()?,
        max_x: reader.f32()?,
        max_y: reader.f32()?,
    };
    let kinds = read_kinds(&mut reader)?;
    let limit = reader.u32()?;
    let offset = reader.u32()?;
    reader.finish()?;
    Ok(QueryRegion {
        world,
        bounds,
        kinds,
        limit,
        offset,
    })
}

/// Coordinate grid index over a POI table. Cells hold table ordinals in
/// insertion order; bbox walks visit cells row-major, so scans are
/// deterministic without copying or sorting the table.
#[derive(Clone, Debug, Default)]
pub struct PoiIndex {
    cells: std::collections::HashMap<(i32, i32), Vec<usize>>,
}

impl PoiIndex {
    /// Builds the index over `pois`. Rebuild when the table revision changes.
    pub fn build(pois: &[Poi]) -> Self {
        let mut cells = std::collections::HashMap::new();
        for (ordinal, poi) in pois.iter().enumerate() {
            cells
                .entry(cell_of(poi.x, poi.y))
                .or_insert_with(Vec::new)
                .push(ordinal);
        }
        Self { cells }
    }

    /// Table ordinals in cells overlapped by the bbox, row-major. Whole
    /// cells are yielded: callers filter by world and type but never clip
    /// to the bounds, so edge cells over-include by at most one cell.
    pub fn scan_bounds(
        &self,
        min_x: f32,
        min_y: f32,
        max_x: f32,
        max_y: f32,
    ) -> Result<Vec<usize>, String> {
        let (x0, y0) = cell_of(min_x, min_y);
        let (x1, y1) = cell_of(max_x, max_y);
        if x1 as i64 - x0 as i64 > INDEX_MAX_CELLS_PER_AXIS
            || y1 as i64 - y0 as i64 > INDEX_MAX_CELLS_PER_AXIS
        {
            return Err("POI bounds cover too many index cells".to_owned());
        }
        let mut ordinals = Vec::new();
        for cell_y in y0..=y1 {
            for cell_x in x0..=x1 {
                if let Some(cell) = self.cells.get(&(cell_x, cell_y)) {
                    ordinals.extend(cell.iter().copied());
                }
            }
        }
        Ok(ordinals)
    }
}

fn cell_of(x: f32, y: f32) -> (i32, i32) {
    (
        (x / INDEX_CELL).floor() as i32,
        (y / INDEX_CELL).floor() as i32,
    )
}

/// Squared distance from a point to a record. 3D when both sides carry
/// depth, planar otherwise.
pub fn distance_squared(x: f32, y: f32, z: Option<f32>, poi: &Poi) -> f32 {
    let dx = poi.x - x;
    let dy = poi.y - y;
    match (z, poi.z) {
        (Some(query_z), Some(poi_z)) => {
            let dz = poi_z - query_z;
            dx * dx + dy * dy + dz * dz
        }
        _ => dx * dx + dy * dy,
    }
}

/// Encodes a result page for provider-side indexed scans.
pub fn encode_page(page: &PoiPage) -> Result<Vec<u8>, String> {
    if page.pois.len() > MAX_POIS {
        return Err("too many POIs in response".to_owned());
    }
    let mut bytes = Vec::new();
    push_u16(&mut bytes, WIRE_VERSION);
    bytes.extend_from_slice(&page.revision.to_le_bytes());
    bytes.push(u8::from(page.truncated));
    bytes.extend_from_slice(&page.total.to_le_bytes());
    bytes.extend_from_slice(&(page.pois.len() as u32).to_le_bytes());
    for poi in &page.pois {
        push_text(&mut bytes, &poi.id)?;
        push_text(&mut bytes, poi.kind.id())?;
        match &poi.family {
            Some(family) => {
                bytes.push(1);
                push_text(&mut bytes, family.id())?;
            }
            None => bytes.push(0),
        }
        push_text(&mut bytes, &poi.name)?;
        push_text(&mut bytes, &poi.world)?;
        bytes.extend_from_slice(&poi.x.to_le_bytes());
        bytes.extend_from_slice(&poi.y.to_le_bytes());
        match poi.z {
            Some(z) => {
                bytes.push(1);
                bytes.extend_from_slice(&z.to_le_bytes());
            }
            None => bytes.push(0),
        }
    }
    Ok(bytes)
}

/// Decodes a result page for provider-side indexed scans.
pub fn decode_page(bytes: &[u8]) -> Result<PoiPage, String> {
    let mut reader = Reader::new(bytes);
    expect_version(&mut reader)?;
    let revision = reader.u64()?;
    let truncated = match reader.u8()? {
        0 => false,
        1 => true,
        _ => return Err("invalid truncated flag".to_owned()),
    };
    let total = reader.u32()?;
    let count = reader.u32()? as usize;
    if count > MAX_POIS {
        return Err("too many POIs in response".to_owned());
    }
    let mut pois = Vec::with_capacity(count);
    for _ in 0..count {
        let id = reader.text()?;
        let kind = PoiKindRef::parse(&reader.text()?);
        let family = match reader.u8()? {
            0 => None,
            1 => Some(PoiFamilyRef::parse(&reader.text()?)),
            _ => return Err("invalid POI family flag".to_owned()),
        };
        let name = reader.text()?;
        let world = reader.text()?;
        let x = reader.f32()?;
        let y = reader.f32()?;
        let z = match reader.u8()? {
            0 => None,
            1 => Some(reader.f32()?),
            _ => return Err("invalid POI z flag".to_owned()),
        };
        pois.push(Poi {
            id,
            kind,
            family,
            name,
            world,
            x,
            y,
            z,
        });
    }
    reader.finish()?;
    Ok(PoiPage {
        revision,
        pois,
        truncated,
        total,
    })
}

/// Decodes a nearby query for provider-side indexed scans.
pub fn decode_nearby(bytes: &[u8]) -> Result<NearbyQuery, String> {
    let mut reader = Reader::new(bytes);
    expect_version(&mut reader)?;
    let world = reader.text()?;
    let x = reader.f32()?;
    let y = reader.f32()?;
    let z = match reader.u8()? {
        0 => None,
        1 => Some(reader.f32()?),
        _ => return Err("invalid nearby z flag".to_owned()),
    };
    let radius = reader.f32()?;
    if !(radius > 0.0) || !radius.is_finite() {
        return Err("nearby radius must be positive and finite".to_owned());
    }
    let kinds = read_kinds(&mut reader)?;
    let limit = reader.u32()?;
    let offset = reader.u32()?;
    reader.finish()?;
    Ok(NearbyQuery {
        world,
        x,
        y,
        z,
        radius,
        kinds,
        limit,
        offset,
    })
}

/// Encodes a nearby query for provider-side indexed scans.
pub fn encode_nearby(query: &NearbyQuery) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    push_u16(&mut bytes, WIRE_VERSION);
    push_text(&mut bytes, &query.world)?;
    bytes.extend_from_slice(&query.x.to_le_bytes());
    bytes.extend_from_slice(&query.y.to_le_bytes());
    match query.z {
        Some(z) => {
            bytes.push(1);
            bytes.extend_from_slice(&z.to_le_bytes());
        }
        None => bytes.push(0),
    }
    bytes.extend_from_slice(&query.radius.to_le_bytes());
    push_kinds(&mut bytes, &query.kinds)?;
    bytes.extend_from_slice(&query.limit.to_le_bytes());
    bytes.extend_from_slice(&query.offset.to_le_bytes());
    Ok(bytes)
}

fn expect_version(reader: &mut Reader<'_>) -> Result<(), String> {
    let version = reader.u16()?;
    if version == WIRE_VERSION {
        Ok(())
    } else {
        Err(format!("unsupported POI wire version {version}"))
    }
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_kinds(bytes: &mut Vec<u8>, kinds: &[PoiKindRef]) -> Result<(), String> {
    push_u16(
        bytes,
        u16::try_from(kinds.len()).map_err(|_| "too many kind filters".to_owned())?,
    );
    for kind in kinds {
        push_text(bytes, kind.id())?;
    }
    Ok(())
}

fn read_kinds(reader: &mut Reader<'_>) -> Result<Vec<PoiKindRef>, String> {
    let kind_count = reader.u16()? as usize;
    if kind_count > 256 {
        return Err("too many kind filters".to_owned());
    }
    let mut kinds = Vec::with_capacity(kind_count);
    for _ in 0..kind_count {
        kinds.push(PoiKindRef::parse(&reader.text()?));
    }
    Ok(kinds)
}

fn push_text(bytes: &mut Vec<u8>, value: &str) -> Result<(), String> {
    if value.len() > MAX_TEXT_BYTES {
        return Err("POI text exceeds protocol bound".to_owned());
    }
    push_u16(
        bytes,
        u16::try_from(value.len()).map_err(|_| "POI text is too long".to_owned())?,
    );
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let end = self
            .cursor
            .checked_add(count)
            .ok_or_else(|| "POI payload offset overflow".to_owned())?;
        let value = self
            .bytes
            .get(self.cursor..end)
            .ok_or_else(|| "truncated POI payload".to_owned())?;
        self.cursor = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn text(&mut self) -> Result<String, String> {
        let length = self.u16()? as usize;
        if length > MAX_TEXT_BYTES {
            return Err("POI text exceeds protocol bound".to_owned());
        }
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| "POI text is not UTF-8".to_owned())
    }

    fn finish(self) -> Result<(), String> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err("trailing bytes in POI payload".to_owned())
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Open(OpenError),
    Call(CallError),
    Codec(String),
}

impl From<OpenError> for Error {
    fn from(value: OpenError) -> Self {
        Self::Open(value)
    }
}

impl From<CallError> for Error {
    fn from(value: CallError) -> Self {
        Self::Call(value)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_filters_world_bounds_and_kinds() {
        let pois = vec![
            Poi::new(PoiKind::Obelisk, "a", "A", "w1", 10.0, 20.0, None),
            Poi::new(PoiKind::Merchant, "b", "B", "w1", 12.0, 20.0, None),
        ];
        let index = PoiIndex::build(&pois);
        let page = query_region_indexed(
            &pois,
            &index,
            7,
            &QueryRegion::new(
                "w1",
                Bounds {
                    min_x: 0.0,
                    min_y: 0.0,
                    max_x: 20.0,
                    max_y: 30.0,
                },
                10,
            )
            .with_kinds([PoiKind::Obelisk]),
        )
        .unwrap();
        assert_eq!(page.revision, 7);
        assert_eq!(page.pois.len(), 1);
        assert_eq!(page.pois[0].id, "a");
        assert_eq!(page.total, 1);
        assert!(!page.truncated);
    }

    #[test]
    fn kinds_are_unique_and_round_trip_through_their_ids() {
        let mut ids = Vec::new();
        for kind in PoiKind::ALL {
            assert_eq!(PoiKind::parse(kind.id()), Some(kind), "kind {kind}");
            assert_eq!(PoiKind::ALL[kind.ordinal()], kind, "kind {kind} ordinal");
            assert_eq!(kind.to_string(), kind.id(), "kind {kind} display");
            ids.push(kind.id());
        }
        ids.sort_unstable();
        let unique = {
            let mut copy = ids.clone();
            copy.dedup();
            copy
        };
        assert_eq!(ids, unique, "duplicate kind ids");
        assert_eq!(PoiKind::parse("shrine"), None);
        assert_eq!(PoiKind::parse("Ore"), None, "ids are exact");
        assert_eq!(PoiKindRef::from(PoiKind::RedOrb).id(), "red_orb");
    }

    #[test]
    fn families_are_unique_and_round_trip_through_their_ids() {
        let mut ids = Vec::new();
        for family in PoiFamily::ALL {
            assert_eq!(
                PoiFamily::parse(family.id()),
                Some(family),
                "family {family}"
            );
            assert_eq!(
                PoiFamily::ALL[family.ordinal()],
                family,
                "family {family} ordinal"
            );
            assert_eq!(family.to_string(), family.id(), "family {family} display");
            ids.push(family.id());
        }
        ids.sort_unstable();
        let unique = {
            let mut copy = ids.clone();
            copy.dedup();
            copy
        };
        assert_eq!(ids, unique, "duplicate family ids");
        assert_eq!(PoiFamily::parse("Fightstone"), None, "ids are exact");
        // A family a newer provider adds survives as a raw id.
        let unseen = PoiFamilyRef::parse("RiftSprint");
        assert_eq!(unseen.known(), None);
        assert_eq!(unseen.id(), "RiftSprint");
        assert_eq!(
            PoiFamilyRef::from(PoiFamily::FightStone).known(),
            Some(PoiFamily::FightStone)
        );
    }

    #[test]
    fn page_round_trip_carries_the_optional_family() {
        let page = PoiPage {
            revision: 4,
            pois: vec![
                Poi::new(PoiKind::Activity, "a", "Fight Stone", "w1", 1.0, 2.0, None)
                    .with_family(PoiFamily::FightStone),
                Poi::new(
                    PoiKind::Activity,
                    "b",
                    "Rift Sprint",
                    "w1",
                    3.0,
                    4.0,
                    Some(5.0),
                )
                .with_family(PoiFamilyRef::parse("RiftSprint")),
                Poi::new(PoiKind::Chest, "c", "Chest", "w1", 6.0, 7.0, None),
            ],
            truncated: false,
            total: 3,
        };
        let bytes = encode_page(&page).unwrap();
        let decoded = decode_page(&bytes).unwrap();
        assert_eq!(decoded, page);
        assert_eq!(
            decoded.pois[0]
                .family
                .as_ref()
                .and_then(PoiFamilyRef::known),
            Some(PoiFamily::FightStone)
        );
        assert_eq!(
            decoded.pois[1].family.as_ref().map(PoiFamilyRef::id),
            Some("RiftSprint")
        );
        assert_eq!(decoded.pois[2].family, None);
    }

    #[test]
    fn unknown_kind_ids_survive_the_wire_and_filter_by_id() {
        let page = PoiPage {
            revision: 1,
            pois: vec![
                Poi::new(
                    PoiKindRef::Other("shrine".to_owned()),
                    "a",
                    "A",
                    "w1",
                    1.0,
                    2.0,
                    None,
                ),
                Poi::new(PoiKind::Ore, "b", "B", "w1", 3.0, 4.0, None),
            ],
            truncated: false,
            total: 2,
        };
        let decoded = decode_page(&encode_page(&page).unwrap()).unwrap();
        assert_eq!(decoded, page);
        assert_eq!(decoded.pois[0].kind.known(), None);
        assert_eq!(decoded.pois[0].kind.id(), "shrine");
        assert_eq!(decoded.pois[1].kind.known(), Some(PoiKind::Ore));

        // Filters compare wire ids, so a provider extension keeps working.
        let shrine = [PoiKindRef::Other("shrine".to_owned())];
        assert!(decoded.pois[0].kind.selected_by(&shrine));
        assert!(!decoded.pois[1].kind.selected_by(&shrine));
        assert!(decoded.pois[1].kind.selected_by(&[PoiKind::Ore.into()]));
        assert!(!decoded.pois[0].kind.selected_by(&[PoiKind::Ore.into()]));
        assert!(
            decoded.pois[0].kind.selected_by(&[]),
            "empty filter takes all"
        );
    }

    #[test]
    fn pages_walk_past_the_first_limit_with_stable_totals() {
        let pois = (0..5)
            .map(|index| {
                Poi::new(
                    PoiKind::Obelisk,
                    format!("poi-{index}"),
                    format!("POI {index}"),
                    "w1",
                    10.0 + index as f32,
                    20.0,
                    None,
                )
            })
            .collect::<Vec<_>>();
        let index = PoiIndex::build(&pois);
        let query = |offset: u32, limit: u32| {
            query_region_indexed(
                &pois,
                &index,
                3,
                &QueryRegion::new(
                    "w1",
                    Bounds {
                        min_x: 0.0,
                        min_y: 0.0,
                        max_x: 20.0,
                        max_y: 30.0,
                    },
                    limit,
                )
                .with_offset(offset),
            )
            .unwrap()
        };
        let first = query(0, 2);
        assert_eq!(first.total, 5);
        assert!(first.truncated);
        assert_eq!(first.pois.len(), 2);
        assert_eq!(first.pois[0].id, "poi-0");
        let second = query(2, 2);
        assert_eq!(second.total, 5);
        assert!(second.truncated);
        assert_eq!(second.pois[1].id, "poi-3");
        let last = query(4, 2);
        assert_eq!(last.total, 5);
        assert!(!last.truncated);
        assert_eq!(last.pois.len(), 1);
        let past_end = query(9, 2);
        assert_eq!(past_end.total, 5);
        assert!(!past_end.truncated);
        assert!(past_end.pois.is_empty());
    }

    #[test]
    fn query_round_trip_preserves_offset_and_total() {
        let query = QueryRegion::new(
            "w1",
            Bounds {
                min_x: 1.0,
                min_y: 2.0,
                max_x: 3.0,
                max_y: 4.0,
            },
            16,
        )
        .with_kinds([PoiKind::Dungeon])
        .with_offset(32);
        let bytes = encode_query(&query).unwrap();
        assert_eq!(decode_query(&bytes).unwrap(), query);
        // A v1 payload (version field downgraded in place) is rejected
        // instead of misread.
        let mut stale = bytes.clone();
        stale[0] = WIRE_VERSION as u8 - 1;
        stale[1] = 0;
        assert!(decode_query(&stale).is_err());
        let page = PoiPage {
            revision: 9,
            pois: Vec::new(),
            truncated: false,
            total: 48,
        };
        let bytes = encode_page(&page).unwrap();
        assert_eq!(decode_page(&bytes).unwrap(), page);
    }

    #[test]
    fn index_walks_only_overlapped_cells_row_major() {
        let pois = vec![
            Poi::new(PoiKind::Obelisk, "far", "Far", "w1", 5_000.0, 5_000.0, None),
            Poi::new(PoiKind::Obelisk, "near-a", "A", "w1", 10.0, 20.0, None),
            Poi::new(PoiKind::Obelisk, "near-b", "B", "w1", 600.0, 20.0, None),
        ];
        let index = PoiIndex::build(&pois);
        // Window covers the first two cells only; the far record's cell is
        // never visited.
        let ordinals = index.scan_bounds(0.0, 0.0, 700.0, 100.0).unwrap();
        assert_eq!(ordinals, vec![1, 2]);
        // The index is conservative: a window inside an occupied cell still
        // yields that cell's records for the caller to filter exactly.
        assert_eq!(index.scan_bounds(0.0, 0.0, 5.0, 5.0).unwrap(), vec![1]);
        // Absurd spans fail instead of walking billions of cells.
        assert!(index.scan_bounds(-1e30, -1e30, 1e30, 1e30).is_err());
    }

    #[test]
    fn distance_prefers_3d_only_when_both_sides_carry_depth() {
        let poi = Poi::new(PoiKind::Obelisk, "p", "P", "w1", 3.0, 4.0, Some(12.0));
        assert_eq!(distance_squared(0.0, 0.0, None, &poi), 25.0);
        assert_eq!(distance_squared(0.0, 0.0, Some(0.0), &poi), 169.0);
        let flat = Poi {
            z: None,
            ..poi.clone()
        };
        assert_eq!(distance_squared(0.0, 0.0, Some(0.0), &flat), 25.0);
    }

    #[test]
    fn nearby_round_trip_and_radius_validation() {
        let query = NearbyQuery::new("w1", 10.0, 20.0, 100.0, 8)
            .with_depth(5.0)
            .with_kinds([PoiKind::Obelisk])
            .with_offset(2);
        let bytes = encode_nearby(&query).unwrap();
        assert_eq!(decode_nearby(&bytes).unwrap(), query);
        for radius in [0.0, -3.0, f32::INFINITY, f32::NAN] {
            let bytes = encode_nearby(&NearbyQuery {
                radius,
                ..query.clone()
            })
            .unwrap();
            assert!(decode_nearby(&bytes).is_err());
        }
    }
}
