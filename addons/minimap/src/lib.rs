//! A map-backed minimap proof of concept.
//!
//! The map is an ordinary add-on-owned image. The host contributes the
//! existing player and zone snapshots; the add-on applies the W1 calibration
//! and returns a generic image/canvas frame.

use farever_more_sdk::bus::Target;
use farever_more_sdk::prelude::*;
use farever_poi_protocol::{
    Bounds as PoiBounds, Client as PoiClient, Poi, PoiFamily, PoiFamilyRef, PoiKind, PoiKindRef,
    QueryRegion,
};

// Match the host's continuous-state polling cadence so player and camera
// direction changes are delivered at approximately 60 Hz.
const RENDER_INTERVAL: Duration = Duration::from_millis(16);
const SURFACE_ID: &str = "minimap-poc";
const CONFIG_MENU_ID: &str = "settings";
const SQUARE_LAYOUT_KEY: &str = "minimap-square-layout";
const POI_MARKERS_KEY: &str = "minimap-poi-markers";
const ACTIVITIES_ATLAS_KEY: &str = "minimap-atlas-activities";
const MAPINFO_ATLAS_KEY: &str = "minimap-atlas-mapinfo";
const CUSTOM_ATLAS_KEY: &str = "minimap-atlas-custom";

/// Per-kind marker visibility, indexed by [`PoiKind::ordinal`], so the shared
/// enum and the minimap tables stay aligned by construction.
type PoiKindFlags = [bool; PoiKind::ALL.len()];

/// One cell of a marker atlas: (atlas, column, row, grid columns, grid rows),
/// counting from the top-left exactly like `crop_uv` maps pixels (v =
/// row / rows, v = 0 at the top).
type PoiIconCell = (usize, u32, u32, u32, u32);

/// Everything the minimap adds to a provider kind: the settings key that gates
/// it, its menu label, and the atlas cell its marker samples. The name itself
/// comes from [`PoiKind`], so the consumer cannot drift from the provider.
struct PoiKindDef {
    kind: PoiKind,
    setting_key: &'static str,
    label: &'static str,
    /// Whether the checkbox starts enabled. Dense gather kinds start hidden:
    /// ores, plants and orbs are 859 of the 1224 W1 records and would
    /// carpet a 24 px marker map until the player opts in.
    default_visible: bool,
    /// Cell for every record of this kind. The obelisk, respawn, chest and
    /// activity cells are the ones the game's own `icon` sheet binds to those
    /// markers (`ObeliskMarker`, `RespawnPointMarker`, `ChestCompletion`,
    /// `NPC_Goal_*`); merchant and dungeon are unverified picks from the
    /// map-information atlas, and ore, plant and orb are generated art in
    /// the add-on-owned atlas.
    icon: PoiIconCell,
}

/// One entry per kind, in [`PoiKind::ALL`] order, so `ordinal()` indexes this
/// table directly. Kinds the shared enum does not name yet render with the
/// fallback marker and stay visible: new data must never be hidden silently
/// just because no checkbox exists for it yet.
const POI_KIND_DEFS: [PoiKindDef; PoiKind::ALL.len()] = [
    PoiKindDef {
        kind: PoiKind::Obelisk,
        setting_key: "minimap-poi-cat-obelisk",
        label: "Obelisks",
        default_visible: true,
        icon: (0, 4, 0, 8, 3),
    },
    PoiKindDef {
        kind: PoiKind::Merchant,
        setting_key: "minimap-poi-cat-merchant",
        label: "Merchants",
        default_visible: true,
        icon: (1, 1, 0, 5, 1),
    },
    PoiKindDef {
        kind: PoiKind::Dungeon,
        setting_key: "minimap-poi-cat-dungeon",
        label: "Dungeons",
        default_visible: true,
        icon: (1, 2, 0, 5, 1),
    },
    PoiKindDef {
        kind: PoiKind::Respawn,
        setting_key: "minimap-poi-cat-respawn",
        label: "Respawn points",
        default_visible: true,
        icon: (0, 5, 0, 8, 3),
    },
    PoiKindDef {
        kind: PoiKind::Chest,
        setting_key: "minimap-poi-cat-chest",
        label: "Chests",
        default_visible: true,
        icon: (0, 0, 2, 8, 3),
    },
    PoiKindDef {
        kind: PoiKind::RedOrb,
        setting_key: "minimap-poi-cat-red-orb",
        label: "Orbs",
        default_visible: false,
        icon: (2, 2, 0, 3, 1),
    },
    PoiKindDef {
        kind: PoiKind::Plant,
        setting_key: "minimap-poi-cat-plant",
        label: "Plants",
        default_visible: false,
        icon: (2, 1, 0, 3, 1),
    },
    PoiKindDef {
        kind: PoiKind::Ore,
        setting_key: "minimap-poi-cat-ore",
        label: "Ores",
        default_visible: false,
        icon: (2, 0, 0, 3, 1),
    },
    PoiKindDef {
        kind: PoiKind::Activity,
        setting_key: "minimap-poi-cat-activity",
        label: "Activities",
        default_visible: true,
        icon: (0, 3, 1, 8, 3),
    },
];
const POI_SECTION_ID: &str = "minimap-poi-section";

/// The table entry for a known kind.
fn poi_kind_def(kind: PoiKind) -> &'static PoiKindDef {
    &POI_KIND_DEFS[kind.ordinal()]
}

/// Marker source atlases, aligned with the `poi_atlases` array: the two
/// atlases shipped by the farever-minimap release plus one add-on-owned
/// atlas for generated art. Markers sample atlas cells instead of shipping one
/// PNG per type, so atlas updates never touch the draw code.
const POI_ATLAS_KEYS: [&str; 3] = [ACTIVITIES_ATLAS_KEY, MAPINFO_ATLAS_KEY, CUSTOM_ATLAS_KEY];

/// Marker atlas cell for a record's kind. Unknown kinds get `None`, so they
/// keep rendering through the neutral fallback plate instead of disappearing.
fn poi_icon_def(kind: &PoiKindRef) -> Option<PoiIconCell> {
    kind.known().map(|kind| poi_kind_def(kind).icon)
}

/// Marker atlas cell per activity family, for the families whose art the game
/// draws differently inside the activity kind. A family that is not listed
/// keeps its kind's cell instead of borrowing another family's art, so a new
/// family from a newer provider cannot silently take the wrong icon.
const ACTIVITY_ICON_CELLS: [(PoiFamily, PoiIconCell); 5] = [
    (PoiFamily::ChestOrb, (0, 1, 0, 8, 3)),
    (PoiFamily::FightStone, (0, 2, 1, 8, 3)),
    (PoiFamily::WorldCamp, (0, 3, 0, 8, 3)),
    (PoiFamily::Ascension, (0, 1, 1, 8, 3)),
    (PoiFamily::WorldElite, (0, 0, 0, 8, 3)),
];

/// Marker atlas cell for a record: its kind's cell, except where the record's
/// activity family has its own art.
fn poi_icon_cell(kind: &PoiKindRef, family: Option<&PoiFamilyRef>) -> Option<PoiIconCell> {
    if kind.known() == Some(PoiKind::Activity) {
        let family = family.and_then(PoiFamilyRef::known);
        if let Some((_, cell)) = ACTIVITY_ICON_CELLS
            .iter()
            .find(|(listed, _)| Some(*listed) == family)
        {
            return Some(*cell);
        }
    }
    poi_icon_def(kind)
}

/// UV rect for one atlas cell.
fn atlas_cell_uv(col: u32, row: u32, cols: u32, rows: u32) -> ([f32; 2], [f32; 2]) {
    let (col, row) = (col as f32, row as f32);
    let (cols, rows) = (cols as f32, rows as f32);
    (
        [col / cols, row / rows],
        [(col + 1.0) / cols, (row + 1.0) / rows],
    )
}

fn poi_kind_setting(def: &PoiKindDef) -> Setting<bool> {
    Setting::boolean(def.setting_key, def.default_visible)
        .label(def.label)
        .description("Show this point-of-interest type on the map")
}

/// Kind toggles as they start, before the player changes anything. Stored
/// overrides win at registration, so this only seeds kinds the player has
/// never touched.
fn default_kind_flags() -> PoiKindFlags {
    let mut flags = [false; PoiKind::ALL.len()];
    for def in &POI_KIND_DEFS {
        flags[def.kind.ordinal()] = def.default_visible;
    }
    flags
}

/// Whether the toggles show this kind. Kinds with no checkbox stay visible:
/// new data must never be hidden silently.
fn poi_kind_visible(types: &PoiKindFlags, kind: &PoiKindRef) -> bool {
    kind.known().is_none_or(|kind| types[kind.ordinal()])
}
const MAP_IMAGE_KEY: &str = "w1-siagarta-map";
const PLAYER_IMAGE_KEY: &str = "player-map-arrow";
const W1_AREA_ID: &str = "World/W1_Siagarta";
const W1_AREA_PREFIX: &str = "World/W1_";

const CANVAS_SIZE: f32 = 236.0;
const MAP_RADIUS: f32 = 114.0;
const SQUARE_CANVAS_SIZE: [f32; 2] = [236.0, 236.0];
const SQUARE_MAP_INSET: f32 = 1.0;
const SQUARE_CORNER_RADIUS: f32 = 6.0;
const NORTH_LABEL_GUTTER: f32 = 12.0;
const NORTH_LABEL_GAP: f32 = 1.0;
const NORTH_LABEL_SIZE: f32 = 11.0;
const CAMERA_DIRECTION_LENGTH: f32 = 82.0;
const CAMERA_DIRECTION_HALF_ANGLE: f32 = 0.36;
// The reference preview is 4096x4096 and is bundled at its original size.
const REFERENCE_MAP_SIZE_PIXELS: f32 = 4096.0;
const MAP_SIZE_PIXELS: f32 = REFERENCE_MAP_SIZE_PIXELS;
const REFERENCE_IMAGE_SCALE: f32 = 1.0;

// Values copied from farever-minimap's data/minimap_calibration.json for
// W1_Siagarta. That renderer multiplies world coordinates by
// these scale values, adds the offsets, normalizes by its 11264-unit map
// coordinate extent, and then samples the 4096x4096 preview. zoom is a
// normalized image magnification: the visible crop span is 1 / zoom.
const REFERENCE_SOURCE_SCALE_X: f32 = 1.7756499661218983;
const REFERENCE_SOURCE_SCALE_Y: f32 = 1.7707905370196702;
const REFERENCE_SOURCE_OFFSET_X: f32 = 4096.76539455183;
const REFERENCE_SOURCE_OFFSET_Y: f32 = 6150.390302226916;
const REFERENCE_FLIP_Y: bool = false;
const REFERENCE_MAP_COORDINATE_EXTENT: f32 = 11264.0;
const REFERENCE_VIEW_ZOOM: f32 = 19.0;

const SHADOW: Color = Color::rgba8(0, 0, 0, 70);
const MAP_BACKGROUND: Color = Color::rgba8(50, 72, 72, 255);
const MAP_EDGE: Color = Color::rgba8(215, 190, 62, 235);
const CAMERA_DIRECTION_FILL: Color = Color::rgba8(235, 245, 255, 48);
const CAMERA_DIRECTION_EDGE: Color = Color::rgba8(235, 245, 255, 155);
const NORTH_LABEL_COLOR: Color = Color::rgba8(255, 245, 208, 235);
const POI_EDGE: Color = Color::rgba8(24, 24, 28, 230);
const POI_FALLBACK_FILL: Color = Color::rgba8(240, 240, 240, 245);
/// Half-size of a pre-baked POI icon on the canvas: the 128 px source
/// shrinks to a 24 px marker, matching the regular map's marker scale.
const POI_ICON_HALF_SIZE: f32 = 12.0;
/// A press counts as a marker hit inside this radius, slightly larger than the
/// icon so a 24 px marker stays easy to click.
const POI_HIT_RADIUS: f32 = POI_ICON_HALF_SIZE * 1.5;
/// The map canvas node id: presses on it are hit-tested against the markers of
/// the last rendered frame.
const MAP_CANVAS_ID: &str = "minimap-canvas";
/// The GPS add-on's command topic. A published waypoint request is applied
/// there only when the player has that add-on's map clicks enabled.
const GPS_TOPIC: &str = "gps";

fn square_layout_setting() -> Setting<bool> {
    Setting::boolean(SQUARE_LAYOUT_KEY, false)
        .label("Square map layout")
        .description("Use a square map frame instead of the circular minimap")
}

fn poi_markers_setting() -> Setting<bool> {
    Setting::boolean(POI_MARKERS_KEY, true)
        .label("Show on map")
        .description("Show points of interest from the POI database on the map")
}

struct MinimapPoc {
    map_image: Image,
    player_marker: Image,
    /// Marker source atlases, aligned with POI_ATLAS_KEYS order.
    poi_atlases: [Image; POI_ATLAS_KEYS.len()],
    square_layout: bool,
    show_pois: bool,
    poi_types: PoiKindFlags,
    // Opened once at activation and reused for every re-poll, so ticks never
    // grow the host's per-add-on service-handle table.
    poi_client: PoiClient,
    // Buffered window around the player: frames render from this cache, and
    // the service is only re-polled once the player leaves it.
    poi_buffer: PoiBuffer,
    /// Markers of the last rendered frame, in canvas-local points, so a press
    /// can be matched to what the player actually sees.
    marker_hits: Vec<MarkerHit>,
}

/// One marker as drawn on the last frame.
#[derive(Clone, Debug, PartialEq)]
struct MarkerHit {
    /// Canvas-local center of the icon.
    canvas: [f32; 2],
    /// World position the press should open a waypoint for.
    world: [f32; 2],
    name: String,
}

/// Marker nearest `point` within [`POI_HIT_RADIUS`], if any.
fn nearest_marker(hits: &[MarkerHit], x: f32, y: f32) -> Option<&MarkerHit> {
    let radius_squared = POI_HIT_RADIUS * POI_HIT_RADIUS;
    hits.iter()
        .filter_map(|hit| {
            let dx = hit.canvas[0] - x;
            let dy = hit.canvas[1] - y;
            let distance = dx * dx + dy * dy;
            (distance <= radius_squared).then_some((distance, hit))
        })
        .min_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, hit)| hit)
}

/// One page of fetch uses a small transfer; the buffer walks pages until the
/// provider's total is reached. Sized so the dense 1024-record origin
/// window walks in 16 pages, far below the safety cap.
const POI_FETCH_PAGE_SIZE: u32 = 64;
/// Buffered window half-extent in world units: many viewports wide, so
/// ordinary movement never re-polls.
const POI_BUFFER_HALF_EXTENT: f32 = 1500.0;
/// Safety budget for one window fetch, so a misbehaving provider cannot turn
/// a single re-poll into an unbounded walk.
const POI_FETCH_MAX_PAGES: u32 = 64;

struct PoiBuffer {
    center_x: f32,
    center_y: f32,
    half_extent: f32,
    revision: u64,
    pois: Vec<Poi>,
}

impl PoiBuffer {
    /// Refresh once the player leaves the inner half of the buffer, so
    /// crossings near the edge don't re-poll every tick.
    fn needs_refresh(&self, x: f32, y: f32) -> bool {
        let margin = self.half_extent / 2.0;
        x < self.center_x - margin
            || x > self.center_x + margin
            || y < self.center_y - margin
            || y > self.center_y + margin
    }
}

/// Fetches one buffered window around (`center_x`, `center_y`), walking
/// pages until the provider's total is reached.
fn fetch_poi_buffer(
    client: &PoiClient,
    center_x: f32,
    center_y: f32,
    half_extent: f32,
) -> SdkResult<PoiBuffer> {
    let mut pois = Vec::new();
    let mut offset = 0u32;
    let mut revision = 0u64;
    for _ in 0..POI_FETCH_MAX_PAGES {
        let page = client
            .query_region(
                &QueryRegion::new(
                    W1_AREA_ID,
                    PoiBounds {
                        min_x: center_x - half_extent,
                        min_y: center_y - half_extent,
                        max_x: center_x + half_extent,
                        max_y: center_y + half_extent,
                    },
                    POI_FETCH_PAGE_SIZE,
                )
                .with_offset(offset),
            )
            .map_err(|error| format!("query typed POI service: {error}"))?;
        revision = page.revision;
        offset += page.pois.len() as u32;
        let done = !page.truncated || pois.len() + page.pois.len() >= page.total as usize;
        pois.extend(page.pois);
        if done {
            break;
        }
    }
    Ok(PoiBuffer {
        center_x,
        center_y,
        half_extent,
        revision,
        pois,
    })
}

impl Addon for MinimapPoc {
    fn activate(context: &mut ActivateContext) -> SdkResult<Self> {
        let square_layout = context.config().register(&square_layout_setting())?;
        let show_pois = context.config().register(&poi_markers_setting())?;
        let mut poi_types = default_kind_flags();
        for def in &POI_KIND_DEFS {
            poi_types[def.kind.ordinal()] = context.config().register(&poi_kind_setting(def))?;
        }
        let map_image = context
            .assets()
            .register_image(MAP_IMAGE_KEY, MAP_IMAGE_PNG)?;
        let player_marker = context
            .assets()
            .register_image(PLAYER_IMAGE_KEY, PLAYER_MARKER_PNG)?;
        // The array literal keeps the atlases aligned with
        // POI_ATLAS_KEYS order: farever-minimap activities, farever-minimap
        // map-information, add-on-owned custom art.
        let poi_atlases = [
            context
                .assets()
                .register_image(ACTIVITIES_ATLAS_KEY, ACTIVITIES_ATLAS_PNG)?,
            context
                .assets()
                .register_image(MAPINFO_ATLAS_KEY, MAPINFO_ATLAS_PNG)?,
            context
                .assets()
                .register_image(CUSTOM_ATLAS_KEY, CUSTOM_ATLAS_PNG)?,
        ];
        // The POI database is a required dependency: without it the minimap
        // cannot render its markers, so activation fails and the host reports
        // the failure to the player. Optional dependencies must degrade
        // gracefully instead; see the manifest contract.
        let poi_client = PoiClient::open(&context.dependencies(), "poi-database")
            .map_err(|error| format!("open typed POI service: {error}"))?;
        // Seed a buffered window around the origin; the first tick recenters
        // it on the player when needed. Paging keeps every transfer small.
        let poi_buffer = fetch_poi_buffer(&poi_client, 0.0, 0.0, POI_BUFFER_HALF_EXTENT)?;
        context.timer().schedule(RENDER_INTERVAL);
        context.render();
        context.log().info(&format!(
            "Minimap PoC activated with POI service version={} revision={} records={}",
            poi_client.version(),
            poi_buffer.revision,
            poi_buffer.pois.len()
        ));
        Ok(Self {
            map_image,
            player_marker,
            poi_atlases,
            square_layout,
            show_pois,
            poi_types,
            poi_client,
            poi_buffer,
            marker_hits: Vec::new(),
        })
    }

    fn on_ui_event(&mut self, context: &mut Context, event: UiEvent) -> SdkResult<()> {
        match event {
            UiEvent::CheckboxChanged { id, checked } if id == SQUARE_LAYOUT_KEY => {
                context.config().set(&square_layout_setting(), &checked)?;
                self.square_layout = checked;
                context.render();
            }
            UiEvent::CheckboxChanged { id, checked } if id == POI_MARKERS_KEY => {
                context.config().set(&poi_markers_setting(), &checked)?;
                self.show_pois = checked;
                context.render();
            }
            UiEvent::CheckboxChanged { id, checked }
                if POI_KIND_DEFS.iter().any(|def| def.setting_key == id) =>
            {
                if let Some(def) = POI_KIND_DEFS.iter().find(|def| def.setting_key == id) {
                    context.config().set(&poi_kind_setting(def), &checked)?;
                    self.poi_types[def.kind.ordinal()] = checked;
                    context.render();
                }
            }
            // A map press asks the GPS add-on for an arrow. The minimap has no
            // opinion about whether that add-on wants map clicks, so it only
            // publishes the request.
            UiEvent::CanvasPressed { id, x, y, .. } if id == MAP_CANVAS_ID => {
                if let Some(hit) = nearest_marker(&self.marker_hits, x as f32, y as f32) {
                    let payload =
                        format!("{:.2} {:.2} \"{}\"", hit.world[0], hit.world[1], hit.name);
                    let listeners = context
                        .bus()
                        .publish(GPS_TOPIC, Target::Subscribers, None, payload.as_bytes())
                        .map_err(|error| format!("publish waypoint request: {error:?}"))?;
                    // The count is how many add-ons were listening at this
                    // moment, so a press that changes nothing is diagnosable
                    // from the log instead of from guesswork.
                    context.log().info(&format!(
                        "Waypoint request for {} matched {listeners} listener(s)",
                        hit.name
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_tick(&mut self, context: &mut Context, _tick: Tick) -> SdkResult<TickControl> {
        if let Ok(value) = context.config().get(&square_layout_setting()) {
            self.square_layout = value;
        }
        if let Ok(value) = context.config().get(&poi_markers_setting()) {
            self.show_pois = value;
        }
        for def in &POI_KIND_DEFS {
            if let Ok(value) = context.config().get(&poi_kind_setting(def)) {
                self.poi_types[def.kind.ordinal()] = value;
            }
        }
        let game = context.game();
        let snapshot = game.snapshot();
        let area_id = game.zone().value.and_then(|zone| zone.area_id);
        let mut marker_hits = Vec::new();
        // Re-poll only once the player leaves the buffered window; frames in
        // between render purely from the cache. A failed re-poll keeps the
        // stale buffer so the map never goes blank.
        if let Some(position) = snapshot
            .player
            .value
            .as_ref()
            .and_then(|player| player.position)
        {
            if self.poi_buffer.needs_refresh(position.x, position.y) {
                let client = self.poi_client.clone();
                match fetch_poi_buffer(&client, position.x, position.y, POI_BUFFER_HALF_EXTENT) {
                    Ok(buffer) => {
                        context.log().info(&format!(
                            "Minimap PoC refreshed POI buffer revision={} records={}",
                            buffer.revision,
                            buffer.pois.len()
                        ));
                        self.poi_buffer = buffer;
                    }
                    Err(error) => {
                        context
                            .log()
                            .warning(&format!("Minimap PoC keeping stale POI buffer: {error}"));
                    }
                }
            }
        }
        context.replace_ui(render_snapshot_with_pois(
            &snapshot,
            area_id.as_deref(),
            self.square_layout,
            self.show_pois,
            &self.poi_types,
            &self.map_image,
            &self.player_marker,
            &self.poi_atlases,
            &self.poi_buffer.pois,
            &mut marker_hits,
        ));
        self.marker_hits = marker_hits;
        Ok(TickControl::Continue)
    }

    fn deactivate(&mut self, _reason: ShutdownReason) {}
}

/// Test atlases mirroring activation's POI_ATLAS_KEYS-aligned registration.
#[cfg(test)]
fn poi_icons() -> [Image; POI_ATLAS_KEYS.len()] {
    [
        Image {
            id: ACTIVITIES_ATLAS_KEY.to_owned(),
        },
        Image {
            id: MAPINFO_ATLAS_KEY.to_owned(),
        },
        Image {
            id: CUSTOM_ATLAS_KEY.to_owned(),
        },
    ]
}

/// Kind toggles with everything enabled, matching the registered defaults.
#[cfg(test)]
fn visible_kinds() -> PoiKindFlags {
    [true; PoiKind::ALL.len()]
}

#[cfg(test)]
fn render_snapshot(
    snapshot: &GameSnapshot,
    area_id: Option<&str>,
    square_layout: bool,
    map_image: &Image,
    player_marker: &Image,
) -> Frame {
    render_snapshot_with_pois(
        snapshot,
        area_id,
        square_layout,
        true,
        &[true; PoiKind::ALL.len()],
        map_image,
        player_marker,
        &poi_icons(),
        &[],
        &mut Vec::new(),
    )
}

fn render_snapshot_with_pois(
    snapshot: &GameSnapshot,
    area_id: Option<&str>,
    square_layout: bool,
    show_pois: bool,
    poi_types: &PoiKindFlags,
    map_image: &Image,
    player_marker: &Image,
    poi_atlases: &[Image; POI_ATLAS_KEYS.len()],
    pois: &[Poi],
    marker_hits: &mut Vec<MarkerHit>,
) -> Frame {
    let mut frame = FrameBuilder::new();
    frame.config_menu(CONFIG_MENU_ID, "Minimap", |ui| {
        ui.checkbox(SQUARE_LAYOUT_KEY, "Square map layout", square_layout, true);
        ui.section(
            POI_SECTION_ID,
            Section::new("Points of interest").description("Cached from the POI database"),
            |ui| {
                ui.checkbox(POI_MARKERS_KEY, "Show on map", show_pois, true);
                for def in &POI_KIND_DEFS {
                    ui.checkbox(
                        def.setting_key,
                        def.label,
                        poi_types[def.kind.ordinal()],
                        true,
                    );
                }
            },
        );
        ui.small("map-source", "W1 Siagarta map is pre-baked in the add-on");
    });

    let Some(player) = snapshot.player.value.as_ref() else {
        return frame.finish();
    };
    let Some(position) = player.position else {
        return frame.finish();
    };
    if !snapshot.session.in_world
        || !position.x.is_finite()
        || !position.y.is_finite()
        || !position.z.is_finite()
    {
        return frame.finish();
    }
    if !has_prebaked_map(area_id) {
        return frame.finish();
    }
    let open_windows = snapshot
        .windows
        .value
        .as_ref()
        .map(|windows| windows.open.as_slice())
        .unwrap_or_default();
    if should_hide_for_game_ui(open_windows) {
        return frame.finish();
    }

    let layout = map_layout(square_layout);
    let player_heading = player.heading_radians.filter(|heading| heading.is_finite());
    let camera_heading = snapshot
        .camera
        .value
        .map(|camera| camera.heading_radians)
        .filter(|heading| heading.is_finite());

    frame.surface(
        SURFACE_ID,
        "Minimap",
        SurfaceOptions::new(Anchor::TopRight)
            .margin(28.0, 28.0)
            .width(layout.canvas_size[0])
            .style(SurfaceStyle::new(Color::TRANSPARENT)),
        |ui| {
            ui.canvas(MAP_CANVAS_ID, layout.canvas_size, |canvas| {
                draw_minimap(
                    canvas,
                    map_image,
                    position,
                    player_heading,
                    camera_heading,
                    player_marker,
                    poi_atlases,
                    layout,
                    // The cached POI set is rendered as-is; the toggles only
                    // select between the cache and an empty slice, so
                    // disabling markers never touches the data store.
                    if show_pois { pois } else { &[] },
                    poi_types,
                    marker_hits,
                );
            });
        },
    );
    frame.finish()
}

fn has_prebaked_map(area_id: Option<&str>) -> bool {
    // The host smoke fixture uses World/W1_Test; all W1 areas share the
    // reference W1 world image until separate map definitions are added.
    area_id.is_some_and(|area_id| area_id == W1_AREA_ID || area_id.starts_with(W1_AREA_PREFIX))
}

fn should_hide_for_game_ui(open_windows: &[String]) -> bool {
    // The add-on overlay is above the game HWND, so content-bearing game
    // windows remain blocking. The Escape menu is only the host for Farever's
    // configuration entry point and intentionally leaves the minimap visible.
    open_windows
        .iter()
        .any(|window| !is_allowed_over_minimap(window))
}

fn is_allowed_over_minimap(window: &str) -> bool {
    window.rsplit(['.', ':']).next().is_some_and(|name| {
        name.eq_ignore_ascii_case("EscapeMenu")
            || name.eq_ignore_ascii_case("GameMenu")
            // The world, player position, and supported area were checked
            // before this point. Farever can keep this window registered while
            // the player is already in the world and moving.
            || name.eq_ignore_ascii_case("LoadingScreen")
    })
}

fn draw_minimap(
    canvas: &mut CanvasBuilder<'_>,
    map_image: &Image,
    position: Vec3,
    player_heading: Option<f32>,
    camera_heading: Option<f32>,
    player_marker: &Image,
    poi_atlases: &[Image; POI_ATLAS_KEYS.len()],
    layout: MapLayout,
    pois: &[Poi],
    poi_types: &PoiKindFlags,
    marker_hits: &mut Vec<MarkerHit>,
) {
    if layout.square {
        canvas.rect(
            [layout.map_min[0] - 2.0, layout.map_min[1] - 2.0],
            [layout.map_max[0] + 2.0, layout.map_max[1] + 2.0],
            SQUARE_CORNER_RADIUS + 2.0,
            Some(SHADOW),
            None,
        );
        canvas.rect(
            layout.map_min,
            layout.map_max,
            SQUARE_CORNER_RADIUS,
            Some(MAP_BACKGROUND),
            None,
        );
    } else {
        canvas.circle(layout.center, MAP_RADIUS + 2.0, Some(SHADOW), None);
        canvas.circle(layout.center, MAP_RADIUS, Some(MAP_BACKGROUND), None);
    }

    let center_pixel = reference_world_to_map_pixel(position);
    let (uv_min, uv_max) = crop_uv(center_pixel, layout.map_size());
    canvas.image(
        map_image.clone(),
        layout.map_min,
        layout.map_max,
        uv_min,
        uv_max,
        0.0,
        None,
        layout.clip_radius,
    );
    marker_hits.extend(draw_poi_markers(
        canvas,
        pois,
        poi_types,
        poi_atlases,
        uv_min,
        uv_max,
        layout,
    ));
    draw_direction_cone(
        canvas,
        layout.center,
        camera_heading,
        layout.direction_length,
        CAMERA_DIRECTION_HALF_ANGLE,
        CAMERA_DIRECTION_FILL,
        CAMERA_DIRECTION_EDGE,
    );
    if layout.square {
        canvas.rect(
            layout.map_min,
            layout.map_max,
            SQUARE_CORNER_RADIUS,
            None,
            Some(Stroke::new(1.5, MAP_EDGE)),
        );
    } else {
        canvas.circle(
            layout.center,
            MAP_RADIUS,
            None,
            Some(Stroke::new(1.5, MAP_EDGE)),
        );
    }
    canvas.text(
        north_label_position(layout),
        "N",
        NORTH_LABEL_COLOR,
        NORTH_LABEL_SIZE,
    );

    canvas.image(
        player_marker.clone(),
        [layout.center[0] - 10.5, layout.center[1] - 10.5],
        [layout.center[0] + 10.5, layout.center[1] + 10.5],
        [0.0, 0.0],
        [1.0, 1.0],
        player_marker_rotation(player_heading),
        None,
        0.0,
    );
}

/// Marker icons that overhang the map plate are cut into at most this many
/// horizontal slices. The canvas has no clip region, so a slice stops at the
/// plate boundary instead of letting the icon paint over the minimap frame.
const MAX_MARKER_PIECES: usize = 12;

/// Slices whose columns match within this tolerance are drawn as one piece, so
/// straight map edges cost a single primitive.
const PIECE_MERGE_EPSILON: f32 = 1e-4;

/// One visible slice of a marker icon, with the atlas UVs that match it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct MarkerPiece {
    min: [f32; 2],
    max: [f32; 2],
    uv_min: [f32; 2],
    uv_max: [f32; 2],
}

impl MarkerPiece {
    const EMPTY: Self = Self {
        min: [0.0; 2],
        max: [0.0; 2],
        uv_min: [0.0; 2],
        uv_max: [0.0; 2],
    };
}

/// Horizontal extent of the map plate at a canvas height. The plate is the
/// rounded rectangle of the square layout or the circle of the round layout,
/// matching the mask the host applies to the map image.
fn map_plate_span(layout: MapLayout, y: f32) -> Option<[f32; 2]> {
    if layout.square {
        let (min, max) = (layout.map_min, layout.map_max);
        if y < min[1] || y > max[1] {
            return None;
        }
        let radius = SQUARE_CORNER_RADIUS
            .min((max[0] - min[0]) * 0.5)
            .min((max[1] - min[1]) * 0.5);
        let edge = (y - min[1]).min(max[1] - y).max(0.0);
        let inset = if edge < radius {
            radius
                - (radius * radius - (radius - edge) * (radius - edge))
                    .max(0.0)
                    .sqrt()
        } else {
            0.0
        };
        Some([min[0] + inset, max[0] - inset])
    } else {
        let dy = y - layout.center[1];
        let radius = layout.clip_radius;
        let half = radius * radius - dy * dy;
        if half < 0.0 {
            return None;
        }
        let half = half.sqrt();
        Some([layout.center[0] - half, layout.center[0] + half])
    }
}

/// True when the whole box stays inside the map plate. Both plate shapes are
/// convex, so the horizontal spans at the box's top and bottom edges are the
/// narrowest ones the box can meet: if they cover the box, so does everything
/// in between.
fn plate_contains_box(layout: MapLayout, min: [f32; 2], max: [f32; 2]) -> bool {
    [min[1], max[1]].iter().all(|y| {
        map_plate_span(layout, *y).is_some_and(|span| span[0] <= min[0] && span[1] >= max[0])
    })
}

/// True when a point lies inside the map plate at all.
fn point_on_map_plate(layout: MapLayout, point: [f32; 2]) -> bool {
    plate_contains_box(layout, point, point)
}

/// Cuts a marker icon box into the slices that stay inside the map plate and
/// writes their destination rectangles and atlas UVs to `pieces`, returning how
/// many pieces are visible. A box that is fully inside stays a single piece.
fn clip_marker_to_map(
    layout: MapLayout,
    min: [f32; 2],
    max: [f32; 2],
    uv_min: [f32; 2],
    uv_max: [f32; 2],
    pieces: &mut [MarkerPiece; MAX_MARKER_PIECES],
) -> usize {
    let size = [max[0] - min[0], max[1] - min[1]];
    if size[0] <= 0.0 || size[1] <= 0.0 {
        return 0;
    }
    let uv_size = [uv_max[0] - uv_min[0], uv_max[1] - uv_min[1]];
    let piece = |x0: f32, y0: f32, x1: f32, y1: f32| MarkerPiece {
        min: [x0, y0],
        max: [x1, y1],
        uv_min: [
            uv_min[0] + uv_size[0] * (x0 - min[0]) / size[0],
            uv_min[1] + uv_size[1] * (y0 - min[1]) / size[1],
        ],
        uv_max: [
            uv_min[0] + uv_size[0] * (x1 - min[0]) / size[0],
            uv_min[1] + uv_size[1] * (y1 - min[1]) / size[1],
        ],
    };

    if plate_contains_box(layout, min, max) {
        pieces[0] = piece(min[0], min[1], max[0], max[1]);
        return 1;
    }

    let mut count = 0;
    for index in 0..MAX_MARKER_PIECES {
        let y0 = min[1] + size[1] * index as f32 / MAX_MARKER_PIECES as f32;
        let y1 = min[1] + size[1] * (index + 1) as f32 / MAX_MARKER_PIECES as f32;
        // Both edges of a slice use the plate span at that height; because the
        // plate is convex the real span in between is never narrower, so a
        // slice can only under-reach the boundary, never cross it.
        let (span_top, span_bottom) = match (map_plate_span(layout, y0), map_plate_span(layout, y1))
        {
            (Some(top), Some(bottom)) => (top, bottom),
            _ => continue,
        };
        let x0 = min[0].max(span_top[0].max(span_bottom[0]));
        let x1 = max[0].min(span_top[1].min(span_bottom[1]));
        if x1 <= x0 {
            continue;
        }
        if count > 0 {
            let last = pieces[count - 1];
            let same_columns = (last.min[0] - x0).abs() <= PIECE_MERGE_EPSILON
                && (last.max[0] - x1).abs() <= PIECE_MERGE_EPSILON;
            if same_columns && (last.max[1] - y0).abs() <= PIECE_MERGE_EPSILON {
                pieces[count - 1] = piece(last.min[0], last.min[1], last.max[0], y1);
                continue;
            }
        }
        pieces[count] = piece(x0, y0, x1, y1);
        count += 1;
    }
    count
}

fn draw_poi_markers(
    canvas: &mut CanvasBuilder<'_>,
    pois: &[Poi],
    poi_types: &PoiKindFlags,
    poi_atlases: &[Image; POI_ATLAS_KEYS.len()],
    uv_min: [f32; 2],
    uv_max: [f32; 2],
    layout: MapLayout,
) -> Vec<MarkerHit> {
    let mut marker_hits = Vec::new();
    let uv_span = [uv_max[0] - uv_min[0], uv_max[1] - uv_min[1]];
    if uv_span[0] <= 0.0 || uv_span[1] <= 0.0 {
        return marker_hits;
    }
    for poi in pois {
        let pixel = reference_world_to_map_pixel(Vec3 {
            x: poi.x,
            y: poi.y,
            z: poi.z.unwrap_or_default(),
        });
        let uv = [pixel[0] / MAP_SIZE_PIXELS, pixel[1] / MAP_SIZE_PIXELS];
        if uv[0] < uv_min[0] || uv[0] > uv_max[0] || uv[1] < uv_min[1] || uv[1] > uv_max[1] {
            continue;
        }
        let point = [
            layout.map_min[0]
                + ((uv[0] - uv_min[0]) / uv_span[0]) * (layout.map_max[0] - layout.map_min[0]),
            layout.map_min[1]
                + ((uv[1] - uv_min[1]) / uv_span[1]) * (layout.map_max[1] - layout.map_min[1]),
        ];
        // Markers whose centre the plate cannot show at all are dropped; the
        // rest may still be cut off at the boundary by clip_marker_to_map.
        if !point_on_map_plate(layout, point) {
            continue;
        }
        if !poi_kind_visible(poi_types, &poi.kind) {
            continue;
        }
        // Only markers the player can see are clickable.
        marker_hits.push(MarkerHit {
            canvas: point,
            world: [poi.x, poi.y],
            name: poi.name.clone(),
        });
        match poi_icon_cell(&poi.kind, poi.family.as_ref()) {
            Some((atlas, col, row, cols, rows)) => match poi_atlases.get(atlas) {
                Some(image) => {
                    let (cell_min, cell_max) = atlas_cell_uv(col, row, cols, rows);
                    let (min, max) = poi_icon_box(point);
                    let mut pieces = [MarkerPiece::EMPTY; MAX_MARKER_PIECES];
                    let count =
                        clip_marker_to_map(layout, min, max, cell_min, cell_max, &mut pieces);
                    for piece in pieces.iter().take(count) {
                        canvas.image(
                            image.clone(),
                            piece.min,
                            piece.max,
                            piece.uv_min,
                            piece.uv_max,
                            0.0,
                            None,
                            0.0,
                        );
                    }
                }
                None => draw_poi_fallback(canvas, layout, point),
            },
            None => draw_poi_fallback(canvas, layout, point),
        }
    }
    marker_hits
}

/// Marker box for a POI icon centered on a map point.
fn poi_icon_box(point: [f32; 2]) -> ([f32; 2], [f32; 2]) {
    (
        [point[0] - POI_ICON_HALF_SIZE, point[1] - POI_ICON_HALF_SIZE],
        [point[0] + POI_ICON_HALF_SIZE, point[1] + POI_ICON_HALF_SIZE],
    )
}

/// Provider kinds with no icon yet stay visible as a neutral plate marker:
/// new data must never be hidden silently. The canvas cannot clip circles, so
/// a plate that would overhang the map boundary is skipped rather than drawn
/// over the minimap frame.
fn draw_poi_fallback(canvas: &mut CanvasBuilder<'_>, layout: MapLayout, point: [f32; 2]) {
    const RADIUS: f32 = 4.5;
    const EDGE: f32 = 1.5;
    let reach = RADIUS + EDGE;
    if !plate_contains_box(
        layout,
        [point[0] - reach, point[1] - reach],
        [point[0] + reach, point[1] + reach],
    ) {
        return;
    }
    canvas.circle(point, RADIUS + EDGE, Some(POI_EDGE), None);
    canvas.circle(point, RADIUS - 1.0, Some(POI_FALLBACK_FILL), None);
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct MapLayout {
    square: bool,
    canvas_size: [f32; 2],
    center: [f32; 2],
    map_min: [f32; 2],
    map_max: [f32; 2],
    clip_radius: f32,
    direction_length: f32,
}

impl MapLayout {
    fn map_size(self) -> [f32; 2] {
        [
            self.map_max[0] - self.map_min[0],
            self.map_max[1] - self.map_min[1],
        ]
    }
}

fn north_label_position(layout: MapLayout) -> [f32; 2] {
    [
        layout.center[0] - 4.0,
        layout.map_min[1] - NORTH_LABEL_SIZE - NORTH_LABEL_GAP,
    ]
}

fn map_layout(square: bool) -> MapLayout {
    if square {
        let canvas_size = [
            SQUARE_CANVAS_SIZE[0],
            SQUARE_CANVAS_SIZE[1] + NORTH_LABEL_GUTTER,
        ];
        MapLayout {
            square,
            canvas_size,
            center: [
                canvas_size[0] * 0.5,
                SQUARE_CANVAS_SIZE[1] * 0.5 + NORTH_LABEL_GUTTER,
            ],
            map_min: [SQUARE_MAP_INSET, SQUARE_MAP_INSET + NORTH_LABEL_GUTTER],
            map_max: [
                canvas_size[0] - SQUARE_MAP_INSET,
                SQUARE_CANVAS_SIZE[1] + NORTH_LABEL_GUTTER - SQUARE_MAP_INSET,
            ],
            clip_radius: SQUARE_CORNER_RADIUS,
            direction_length: canvas_size[1] * 0.5 - 14.0,
        }
    } else {
        let center = [CANVAS_SIZE * 0.5, CANVAS_SIZE * 0.5];
        MapLayout {
            square,
            canvas_size: [CANVAS_SIZE, CANVAS_SIZE + NORTH_LABEL_GUTTER],
            center: [center[0], center[1] + NORTH_LABEL_GUTTER],
            map_min: [
                center[0] - MAP_RADIUS,
                center[1] - MAP_RADIUS + NORTH_LABEL_GUTTER,
            ],
            map_max: [
                center[0] + MAP_RADIUS,
                center[1] + MAP_RADIUS + NORTH_LABEL_GUTTER,
            ],
            clip_radius: MAP_RADIUS,
            direction_length: CAMERA_DIRECTION_LENGTH,
        }
    }
}

/// Converts x/y world coordinates to source-image pixels with this map's own
/// projection, matching the calibration constants below. z is ignored because
/// the map is 2D.
fn reference_world_to_map_pixel(position: Vec3) -> [f32; 2] {
    let x = (position.x * REFERENCE_SOURCE_SCALE_X + REFERENCE_SOURCE_OFFSET_X)
        / REFERENCE_MAP_COORDINATE_EXTENT
        * MAP_SIZE_PIXELS
        * REFERENCE_IMAGE_SCALE;
    let mut y = (position.y * REFERENCE_SOURCE_SCALE_Y + REFERENCE_SOURCE_OFFSET_Y)
        / REFERENCE_MAP_COORDINATE_EXTENT
        * MAP_SIZE_PIXELS
        * REFERENCE_IMAGE_SCALE;
    if REFERENCE_FLIP_Y {
        y = MAP_SIZE_PIXELS - y;
    }
    [x, y]
}

fn crop_uv(center_pixel: [f32; 2], destination_size: [f32; 2]) -> ([f32; 2], [f32; 2]) {
    let span_y = (1.0 / REFERENCE_VIEW_ZOOM).min(1.0);
    let destination_aspect = destination_size[0] / destination_size[1].max(f32::EPSILON);
    let span_x = (span_y * destination_aspect).min(1.0);
    let start_x = (center_pixel[0] / MAP_SIZE_PIXELS - span_x * 0.5).clamp(0.0, 1.0 - span_x);
    let start_y = (center_pixel[1] / MAP_SIZE_PIXELS - span_y * 0.5).clamp(0.0, 1.0 - span_y);
    ([start_x, start_y], [start_x + span_x, start_y + span_y])
}

fn draw_direction_cone(
    canvas: &mut CanvasBuilder<'_>,
    center: [f32; 2],
    heading: Option<f32>,
    length: f32,
    half_angle: f32,
    fill: Color,
    edge: Color,
) {
    let Some(heading) = heading else {
        return;
    };
    canvas.path(
        direction_cone_points(center, heading, length, half_angle),
        true,
        Some(fill),
        Some(Stroke::new(1.0, edge)),
    );
}

fn direction_cone_points(
    center: [f32; 2],
    heading: f32,
    length: f32,
    half_angle: f32,
) -> Vec<[f32; 2]> {
    const ARC_SAMPLES: usize = 4;
    let mut points = Vec::with_capacity(ARC_SAMPLES + 2);
    points.push(center);
    for sample in 0..=ARC_SAMPLES {
        let fraction = sample as f32 / ARC_SAMPLES as f32;
        let angle = heading - half_angle + fraction * 2.0 * half_angle;
        let direction = heading_vector(angle);
        points.push([
            center[0] + direction[0] * length,
            center[1] + direction[1] * length,
        ]);
    }
    points
}

/// The reference heading convention is already aligned with the north-up map
/// axes: zero points along the map's +X/right axis and positive angles turn
/// toward +Y/down. The bundled player marker faces right, so no extra base
/// offset is applied to the marker or the direction cones.
fn heading_vector(heading: f32) -> [f32; 2] {
    [heading.cos(), heading.sin()]
}

fn player_marker_rotation(heading: Option<f32>) -> f32 {
    // The SVG source points right, matching heading zero.
    heading.unwrap_or(0.0)
}

farever_more_sdk::export!(MinimapPoc);

const MAP_IMAGE_PNG: &[u8] = include_bytes!("../assets/W1_Siagarta.preview.png");
const PLAYER_MARKER_PNG: &[u8] = include_bytes!("../assets/player_marker.png");
// Marker source atlases: the farever-minimap release atlases
// plus one add-on-owned atlas holding generated art (ore crystal, plant
// leaf, orb). Markers sample cells; see POI_KIND_DEFS for the mapping.
const ACTIVITIES_ATLAS_PNG: &[u8] = include_bytes!("../assets/activities.png");
const MAPINFO_ATLAS_PNG: &[u8] = include_bytes!("../assets/icon_mapInformation_atlas_128PX.png");
const CUSTOM_ATLAS_PNG: &[u8] = include_bytes!("../assets/poi_custom_atlas.png");

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn image() -> Image {
        Image {
            id: MAP_IMAGE_KEY.to_owned(),
        }
    }

    fn player_marker() -> Image {
        Image {
            id: PLAYER_IMAGE_KEY.to_owned(),
        }
    }

    fn snapshot(in_world: bool, position: Option<Vec3>) -> GameSnapshot {
        let status = StateStatus {
            observed_at_ms: Some(1),
            revision: 1,
            reason: None,
        };
        GameSnapshot {
            observation: Observation {
                sequence: 1,
                captured_at_ms: 1,
                process_session: 1,
            },
            session: Session {
                process_session: 1,
                in_world,
            },
            player: Snapshot {
                status,
                value: Some(Player {
                    runtime_id: Some("hero".to_owned()),
                    name: Some("Hero".to_owned()),
                    class_id: None,
                    level: None,
                    position,
                    heading_radians: Some(0.0),
                    health: None,
                    max_health: None,
                }),
            },
            camera: Snapshot {
                status,
                value: Some(Camera {
                    heading_radians: 0.0,
                }),
            },
            windows: Snapshot {
                status,
                value: Some(Windows {
                    open: Vec::new(),
                    focused: None,
                }),
            },
        }
    }

    #[test]
    fn valid_world_snapshot_creates_reference_style_minimap() {
        let frame = render_snapshot(
            &snapshot(
                true,
                Some(Vec3 {
                    x: -1800.0,
                    y: -2000.0,
                    z: 0.0,
                }),
            ),
            Some(W1_AREA_ID),
            false,
            &image(),
            &player_marker(),
        );
        let surface = frame.surface(SURFACE_ID).expect("minimap surface");

        assert_eq!(frame.surface_count(), 1);
        assert_eq!(frame.config_menu_count(), 1);
        assert_eq!(surface.anchor(), Anchor::TopRight);
        assert_eq!(surface.margin(), [28.0, 28.0]);
        assert_eq!(surface.width(), Some(CANVAS_SIZE));
        assert_eq!(
            surface
                .canvas()
                .filter(|command| matches!(command.primitive(), PrimitiveRef::Image))
                .count(),
            2
        );
        assert_eq!(
            surface
                .canvas()
                .filter(|command| matches!(command.primitive(), PrimitiveRef::Path(_)))
                .count(),
            1
        );
    }

    #[test]
    fn square_layout_is_available_from_config_and_preserves_map_scale() {
        let frame = render_snapshot(
            &snapshot(true, Some(Vec3::default())),
            Some(W1_AREA_ID),
            true,
            &image(),
            &player_marker(),
        );
        let surface = frame.surface(SURFACE_ID).expect("minimap surface");
        let menu = frame.config_menu(CONFIG_MENU_ID).expect("minimap settings");
        let layout = map_layout(true);
        let (min, max) = crop_uv([2000.0, 2000.0], layout.map_size());

        assert_eq!(surface.width(), Some(SQUARE_CANVAS_SIZE[0]));
        assert_eq!(
            menu.node(SQUARE_LAYOUT_KEY)
                .and_then(|node| node.checkbox_checked()),
            Some(true)
        );
        assert!((max[1] - min[1] - 1.0 / REFERENCE_VIEW_ZOOM).abs() < 0.0001);
        assert!(
            (max[0]
                - min[0]
                - (1.0 / REFERENCE_VIEW_ZOOM) * (layout.map_size()[0] / layout.map_size()[1]))
                .abs()
                < 0.0001
        );
    }

    #[test]
    fn poi_marker_toggle_controls_rendered_markers() {
        // A POI exactly at the player position always lands inside the
        // visible crop, so the toggle is the only thing deciding whether a
        // marker primitive is emitted.
        let pois = vec![Poi::new(
            PoiKind::Obelisk,
            "poi-toggle-probe",
            "Toggle Probe",
            W1_AREA_ID,
            -1800.0,
            -2000.0,
            None,
        )];
        let position = Some(Vec3 {
            x: -1800.0,
            y: -2000.0,
            z: 0.0,
        });
        let visible = visible_kinds();
        let render = |show_pois: bool| {
            render_snapshot_with_pois(
                &snapshot(true, position),
                Some(W1_AREA_ID),
                false,
                show_pois,
                &visible,
                &image(),
                &player_marker(),
                &poi_icons(),
                &pois,
                &mut Vec::new(),
            )
        };
        let counts = |show_pois: bool| {
            let frame = render(show_pois);
            let surface = frame.surface(SURFACE_ID).expect("minimap surface");
            (
                surface
                    .canvas()
                    .filter(|command| matches!(command.primitive(), PrimitiveRef::Image))
                    .count(),
                surface
                    .canvas()
                    .filter(|command| matches!(command.primitive(), PrimitiveRef::Circle))
                    .count(),
                surface
                    .canvas()
                    .filter(|command| matches!(command.primitive(), PrimitiveRef::Path(_)))
                    .count(),
            )
        };
        let baseline_frame = render_snapshot(
            &snapshot(true, position),
            Some(W1_AREA_ID),
            false,
            &image(),
            &player_marker(),
        );
        let baseline_surface = baseline_frame.surface(SURFACE_ID).expect("minimap surface");
        let baseline = (
            baseline_surface
                .canvas()
                .filter(|command| matches!(command.primitive(), PrimitiveRef::Image))
                .count(),
            baseline_surface
                .canvas()
                .filter(|command| matches!(command.primitive(), PrimitiveRef::Circle))
                .count(),
            baseline_surface
                .canvas()
                .filter(|command| matches!(command.primitive(), PrimitiveRef::Path(_)))
                .count(),
        );
        // Toggling markers off must be identical to rendering with no POIs
        // at all, while toggling on adds exactly one marker image and no
        // extra vector primitives for the single marker.
        assert_eq!(counts(false), baseline);
        assert_eq!(counts(true), (baseline.0 + 1, baseline.1, baseline.2));
        for (show_pois, expected_checked) in [(true, Some(true)), (false, Some(false))] {
            let frame = render(show_pois);
            let menu = frame.config_menu(CONFIG_MENU_ID).expect("minimap settings");
            assert_eq!(
                menu.node(POI_MARKERS_KEY)
                    .and_then(|node| node.checkbox_checked()),
                expected_checked
            );
        }
    }

    #[test]
    fn poi_type_toggles_gate_each_kind_independently() {
        // Both markers stack exactly at the player so visibility, not
        // projection, decides what is drawn.
        let pois = [PoiKind::Obelisk, PoiKind::Merchant]
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                Poi::new(
                    kind,
                    format!("poi-type-probe-{index}"),
                    format!("Type Probe {index}"),
                    W1_AREA_ID,
                    -1800.0,
                    -2000.0,
                    None,
                )
            })
            .collect::<Vec<_>>();
        let position = Some(Vec3 {
            x: -1800.0,
            y: -2000.0,
            z: 0.0,
        });
        let render = |types: &PoiKindFlags| {
            render_snapshot_with_pois(
                &snapshot(true, position),
                Some(W1_AREA_ID),
                false,
                true,
                types,
                &image(),
                &player_marker(),
                &poi_icons(),
                &pois,
                &mut Vec::new(),
            )
        };
        let image_counts = |types: &PoiKindFlags| {
            let frame = render(types);
            let surface = frame.surface(SURFACE_ID).expect("minimap surface");
            surface
                .canvas()
                .filter(|command| matches!(command.primitive(), PrimitiveRef::Image))
                .count()
        };
        let all = visible_kinds();
        let images = image_counts(&all);
        // Disabling obelisks removes exactly one marker image while the
        // merchant marker survives, and the menu reflects both states.
        let mut no_obelisk = all;
        no_obelisk[PoiKind::Obelisk.ordinal()] = false;
        assert_eq!(image_counts(&no_obelisk), images - 1);
        let frame = render(&no_obelisk);
        let menu = frame.config_menu(CONFIG_MENU_ID).expect("minimap settings");
        assert_eq!(
            menu.node(poi_kind_def(PoiKind::Obelisk).setting_key)
                .and_then(|node| node.checkbox_checked()),
            Some(false)
        );
        assert_eq!(
            menu.node(poi_kind_def(PoiKind::Merchant).setting_key)
                .and_then(|node| node.checkbox_checked()),
            Some(true)
        );
    }

    #[test]
    fn poi_kind_table_covers_every_shared_kind_exactly_once() {
        // The table is indexed by PoiKind::ordinal, so this also pins the
        // order: a new kind cannot silently land on a neighbour's icon,
        // checkbox or label.
        let mut keys = Vec::new();
        let mut labels = Vec::new();
        for (index, def) in POI_KIND_DEFS.iter().enumerate() {
            assert_eq!(def.kind.ordinal(), index, "table order at {index}");
            assert_eq!(def.kind, PoiKind::ALL[index], "table kind at {index}");
            keys.push(def.setting_key);
            labels.push(def.label);
        }
        fn unique(mut values: Vec<&str>) -> Vec<&str> {
            values.sort_unstable();
            values.dedup();
            values
        }
        assert_eq!(keys.len(), unique(keys.clone()).len(), "duplicate keys");
        assert_eq!(
            labels.len(),
            unique(labels.clone()).len(),
            "duplicate labels"
        );
        assert_eq!(
            poi_kind_def(PoiKind::Merchant).setting_key,
            "minimap-poi-cat-merchant"
        );
        assert_eq!(poi_kind_def(PoiKind::RedOrb).label, "Orbs");
    }

    #[test]
    fn dense_gather_kinds_start_hidden() {
        // Ores, plants and orbs are 859 of the 1224 W1 records: enabled by
        // default they carpet a 24 px marker map, so they start hidden and the
        // player opts in. Every other kind still starts visible.
        let defaults = default_kind_flags();
        for kind in [PoiKind::Ore, PoiKind::Plant, PoiKind::RedOrb] {
            assert!(!poi_kind_def(kind).default_visible, "kind {kind}");
            assert!(!defaults[kind.ordinal()], "kind {kind}");
        }
        for kind in [
            PoiKind::Obelisk,
            PoiKind::Merchant,
            PoiKind::Dungeon,
            PoiKind::Respawn,
            PoiKind::Chest,
            PoiKind::Activity,
        ] {
            assert!(poi_kind_def(kind).default_visible, "kind {kind}");
            assert!(defaults[kind.ordinal()], "kind {kind}");
        }
    }

    #[test]
    fn poi_icons_sample_the_mapped_atlas_cells() {
        // (atlas, column, row, grid columns, grid rows) per kind.
        // Farever-minimap cells for obelisk, respawn and activity match the
        // game's data.cdb marker entries; the rest follow the in-game check or
        // the add-on-owned atlas.
        for (kind, cell) in [
            (PoiKind::Obelisk, (0, 4, 0, 8, 3)),
            (PoiKind::Merchant, (1, 1, 0, 5, 1)),
            (PoiKind::Dungeon, (1, 2, 0, 5, 1)),
            (PoiKind::Respawn, (0, 5, 0, 8, 3)),
            (PoiKind::Chest, (0, 0, 2, 8, 3)),
            (PoiKind::RedOrb, (2, 2, 0, 3, 1)),
            (PoiKind::Plant, (2, 1, 0, 3, 1)),
            (PoiKind::Ore, (2, 0, 0, 3, 1)),
            (PoiKind::Activity, (0, 3, 1, 8, 3)),
        ] {
            assert_eq!(
                poi_icon_def(&PoiKindRef::from(kind)),
                Some(cell),
                "kind {kind}"
            );
        }
        // A provider kind the shared enum does not name falls back to the
        // neutral plate instead of borrowing another kind's art.
        assert_eq!(poi_icon_def(&PoiKindRef::Other("shrine".to_owned())), None);
        // The obelisk samples the full fourth cell of the top row.
        let (uv_min, uv_max) = atlas_cell_uv(4, 0, 8, 3);
        assert_eq!(uv_min, [0.5, 0.0]);
        assert_eq!(uv_max[0], 0.625);
        assert!((uv_max[1] - 1.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn activity_families_override_the_activity_cell() {
        let activity = PoiKindRef::from(PoiKind::Activity);
        for (family, cell) in [
            (PoiFamily::ChestOrb, (0, 1, 0, 8, 3)),
            (PoiFamily::FightStone, (0, 2, 1, 8, 3)),
            (PoiFamily::WorldCamp, (0, 3, 0, 8, 3)),
            (PoiFamily::Ascension, (0, 1, 1, 8, 3)),
            // The orange skull diamond marks a world elite.
            (PoiFamily::WorldElite, (0, 0, 0, 8, 3)),
        ] {
            assert_eq!(
                poi_icon_cell(&activity, Some(&PoiFamilyRef::from(family))),
                Some(cell),
                "{family}"
            );
        }
        // The activity kind still draws its own cell for a family with no
        // recorded art, a family this build cannot name, or no family at all.
        for family in [
            Some(PoiFamilyRef::from(PoiFamily::WorldPlant)),
            Some(PoiFamilyRef::parse("RiftSprint")),
            None,
        ] {
            assert_eq!(
                poi_icon_cell(&activity, family.as_ref()),
                poi_icon_def(&activity)
            );
        }
        // A family only refines activities; other kinds ignore it.
        for kind in [PoiKind::Ore, PoiKind::Chest] {
            let kind = PoiKindRef::from(kind);
            assert_eq!(
                poi_icon_cell(&kind, Some(&PoiFamilyRef::from(PoiFamily::FightStone))),
                poi_icon_def(&kind)
            );
        }
        assert_eq!(
            poi_icon_cell(
                &PoiKindRef::Other("shrine".to_owned()),
                Some(&PoiFamilyRef::from(PoiFamily::WorldCamp))
            ),
            None
        );
    }

    #[test]
    fn poi_markers_draw_at_regular_map_scale() {
        // Markers are 24 px boxes centered on the map point.
        let (min, max) = poi_icon_box([100.0, 50.0]);
        assert_eq!(min, [88.0, 38.0]);
        assert_eq!(max, [112.0, 62.0]);
        assert_eq!(max[0] - min[0], 24.0);
        assert_eq!(max[1] - min[1], 24.0);
    }

    #[test]
    fn poi_icon_inside_the_map_is_drawn_as_one_piece() {
        // Clear of the boundary the icon keeps its box and its atlas cell, so
        // the clip costs nothing.
        for square in [false, true] {
            let layout = map_layout(square);
            let point = [layout.center[0] + 30.0, layout.center[1] + 20.0];
            let (min, max) = poi_icon_box(point);
            let (cell_min, cell_max) = atlas_cell_uv(2, 0, 3, 1);
            let mut pieces = [MarkerPiece::EMPTY; MAX_MARKER_PIECES];
            let count = clip_marker_to_map(layout, min, max, cell_min, cell_max, &mut pieces);
            assert_eq!(count, 1, "square={square}");
            assert_eq!(pieces[0].min, min, "square={square}");
            assert_eq!(pieces[0].max, max, "square={square}");
            assert_eq!(pieces[0].uv_min, cell_min, "square={square}");
            assert_eq!(pieces[0].uv_max, cell_max, "square={square}");
        }
    }

    #[test]
    fn poi_icon_over_the_square_map_edge_is_cut_off_at_the_boundary() {
        // Centred 4 px inside the right edge, so 8 px of the icon hang over.
        let layout = map_layout(true);
        let point = [layout.map_max[0] - 4.0, layout.center[1]];
        let (min, max) = poi_icon_box(point);
        let mut pieces = [MarkerPiece::EMPTY; MAX_MARKER_PIECES];
        let count = clip_marker_to_map(layout, min, max, [0.0, 0.0], [1.0, 1.0], &mut pieces);

        // A straight edge keeps the columns, so the slices merge back into one.
        assert_eq!(count, 1);
        assert_eq!(pieces[0].min[0], min[0]);
        assert_eq!(pieces[0].max[0], layout.map_max[0]);
        assert!((pieces[0].min[1] - min[1]).abs() < 1e-3);
        assert!((pieces[0].max[1] - max[1]).abs() < 1e-3);
        // The dropped columns are dropped from the atlas cell too, so the icon
        // stays squashed against the edge instead of being rescaled.
        let visible = (layout.map_max[0] - min[0]) / (max[0] - min[0]);
        assert_eq!(pieces[0].uv_min[0], 0.0);
        assert!((pieces[0].uv_max[0] - visible).abs() < 1e-4);
    }

    #[test]
    fn poi_icon_over_the_round_map_rim_is_cut_off_at_the_circle() {
        let layout = map_layout(false);
        let radius = layout.clip_radius;
        // Centred 6 px inside the rim, so most of the icon hangs over the arc.
        let point = [layout.center[0] + radius - 6.0, layout.center[1]];
        let (min, max) = poi_icon_box(point);
        let mut pieces = [MarkerPiece::EMPTY; MAX_MARKER_PIECES];
        let count = clip_marker_to_map(layout, min, max, [0.0, 0.0], [1.0, 1.0], &mut pieces);
        assert!(count > 0 && count <= MAX_MARKER_PIECES);

        let mut visible_area = 0.0;
        for piece in pieces.iter().take(count) {
            assert!(piece.min[0] >= min[0] && piece.max[0] <= max[0]);
            assert!(piece.min[1] >= min[1] && piece.max[1] <= max[1]);
            for x in [piece.min[0], piece.max[0]] {
                for y in [piece.min[1], piece.max[1]] {
                    let dx = x - layout.center[0];
                    let dy = y - layout.center[1];
                    // Corners sit exactly on the rim, so allow sub-pixel float
                    // slack instead of demanding a strict inequality.
                    assert!(
                        (dx * dx + dy * dy).sqrt() <= radius + 0.5,
                        "piece corner {x},{y} leaves the rim"
                    );
                }
            }
            visible_area += (piece.max[0] - piece.min[0]) * (piece.max[1] - piece.min[1]);
        }
        let box_area = (max[0] - min[0]) * (max[1] - min[1]);
        assert!(visible_area > 0.0);
        assert!(
            visible_area < box_area * 0.9,
            "icon was not cut: {visible_area} of {box_area}"
        );
    }

    #[test]
    fn poi_icon_outside_the_map_plate_is_not_drawn() {
        for square in [false, true] {
            let layout = map_layout(square);
            let outside = [
                layout.center[0],
                layout.map_min[1] - POI_ICON_HALF_SIZE - 1.0,
            ];
            let (min, max) = poi_icon_box(outside);
            let mut pieces = [MarkerPiece::EMPTY; MAX_MARKER_PIECES];
            let count = clip_marker_to_map(layout, min, max, [0.0; 2], [1.0; 2], &mut pieces);
            assert_eq!(count, 0, "square={square}");
            assert!(!point_on_map_plate(layout, outside), "square={square}");
        }
    }

    #[test]
    fn markers_just_inside_the_round_rim_are_still_placed() {
        // The boundary is the plate itself; the old inner fudge used to drop
        // markers the clip can now cut correctly.
        let layout = map_layout(false);
        let radius = layout.clip_radius;
        assert!(point_on_map_plate(
            layout,
            [layout.center[0] + radius - 2.0, layout.center[1]]
        ));
        assert!(!point_on_map_plate(
            layout,
            [layout.center[0] + radius + 2.0, layout.center[1]]
        ));
        // The rounded corners of the square plate are part of the shape too.
        let square = map_layout(true);
        assert!(!point_on_map_plate(square, square.map_min));
        assert!(point_on_map_plate(square, square.center));
    }

    #[test]
    fn fallback_plate_is_skipped_when_it_would_overhang_the_edge() {
        // The neutral plate is drawn with circles, which the canvas cannot
        // clip, so it is only drawn where it fits whole.
        let layout = map_layout(true);
        let reach = 6.0;
        let fits = [layout.map_max[0] - reach, layout.center[1]];
        let overhangs = [fits[0] + 1.0, fits[1]];
        let plate_box = |point: [f32; 2]| {
            (
                [point[0] - reach, point[1] - reach],
                [point[0] + reach, point[1] + reach],
            )
        };
        let (fit_min, fit_max) = plate_box(fits);
        let (over_min, over_max) = plate_box(overhangs);
        assert!(plate_contains_box(layout, fit_min, fit_max));
        assert!(!plate_contains_box(layout, over_min, over_max));
    }

    #[test]
    fn poi_marker_pass_draws_clipped_pieces_for_overhanging_icons() {
        // Drives the marker pass itself: the map is windowed so that the probe
        // POI lands on a chosen spot of the plate, then the emitted image
        // primitives are counted. A marker clear of the boundary (or cut by a
        // straight edge, whose slices merge) is one draw; a marker hanging over
        // the rounded corner is cut into several.
        let layout = map_layout(true);
        let size = layout.map_size();
        let pixel = reference_world_to_map_pixel(Vec3 {
            x: -1800.0,
            y: -2000.0,
            z: 0.0,
        });
        let poi = Poi::new(
            PoiKind::Ore,
            "poi-clip-probe",
            "Clip Probe",
            W1_AREA_ID,
            -1800.0,
            -2000.0,
            None,
        );
        let marker_draws = |target: [f32; 2]| {
            // Centre a window of the plate on the target spot so the probe POI
            // projects onto it.
            const SPAN: f32 = 0.2;
            let fraction = [
                (target[0] - layout.map_min[0]) / size[0],
                (target[1] - layout.map_min[1]) / size[1],
            ];
            let uv = [pixel[0] / MAP_SIZE_PIXELS, pixel[1] / MAP_SIZE_PIXELS];
            let uv_min = [uv[0] - fraction[0] * SPAN, uv[1] - fraction[1] * SPAN];
            let uv_max = [uv_min[0] + SPAN, uv_min[1] + SPAN];

            let mut builder = FrameBuilder::new();
            builder.surface(
                SURFACE_ID,
                "Minimap",
                SurfaceOptions::new(Anchor::TopRight).width(layout.canvas_size[0]),
                |ui| {
                    ui.canvas("probe-canvas", layout.canvas_size, |canvas| {
                        draw_poi_markers(
                            canvas,
                            std::slice::from_ref(&poi),
                            &visible_kinds(),
                            &poi_icons(),
                            uv_min,
                            uv_max,
                            layout,
                        );
                    });
                },
            );
            let frame = builder.finish();
            let surface = frame.surface(SURFACE_ID).expect("probe surface");
            surface
                .canvas()
                .filter(|command| matches!(command.primitive(), PrimitiveRef::Image))
                .count()
        };

        assert_eq!(marker_draws(layout.center), 1, "clear of the boundary");
        assert_eq!(
            marker_draws([layout.map_max[0] - 4.0, layout.center[1]]),
            1,
            "straight edge slices merge into one draw"
        );
        assert!(
            marker_draws([layout.map_max[0] - 7.0, layout.map_min[1] + 7.0]) > 1,
            "the rounded corner cuts the icon into pieces"
        );
    }

    #[test]
    fn poi_options_live_in_one_section_without_repeating_poi() {
        let frame = render_snapshot(
            &snapshot(
                true,
                Some(Vec3 {
                    x: -1800.0,
                    y: -2000.0,
                    z: 0.0,
                }),
            ),
            Some(W1_AREA_ID),
            false,
            &image(),
            &player_marker(),
        );
        let menu = frame.config_menu(CONFIG_MENU_ID).expect("minimap settings");
        assert!(menu.node("minimap-diagnostics").is_none());
        assert!(menu.node(POI_SECTION_ID).is_some());
        assert_eq!(
            menu.node(POI_SECTION_ID)
                .and_then(|node| node.section().map(|(title, _)| title)),
            Some("Points of interest")
        );
        assert_eq!(
            menu.node(POI_MARKERS_KEY)
                .and_then(|node| node.checkbox().map(|(label, _, _)| label)),
            Some("Show on map")
        );
        let expected = [
            "Obelisks",
            "Merchants",
            "Dungeons",
            "Respawn points",
            "Chests",
            "Orbs",
            "Plants",
            "Ores",
            "Activities",
        ];
        assert_eq!(POI_KIND_DEFS.len(), expected.len());
        for (def, label) in POI_KIND_DEFS.iter().zip(expected) {
            assert_eq!(def.label, label, "label for {}", def.kind);
            assert_eq!(
                menu.node(def.setting_key)
                    .and_then(|node| node.checkbox().map(|(text, _, _)| text)),
                Some(label)
            );
            assert!(!label.contains("POI"), "label repeats POI: {label}");
        }
    }

    #[test]
    fn unknown_poi_kind_renders_fallback_marker_and_ignores_toggles() {
        // A provider kind the shared enum does not name yet must stay visible
        // as a neutral plate instead of vanishing: two circles, no image, no
        // path, and no checkbox can switch it off.
        let pois = vec![Poi::new(
            PoiKindRef::Other("shrine".to_owned()),
            "poi-unknown-probe",
            "Unknown Probe",
            W1_AREA_ID,
            -1800.0,
            -2000.0,
            None,
        )];
        let position = Some(Vec3 {
            x: -1800.0,
            y: -2000.0,
            z: 0.0,
        });
        assert!(poi_kind_visible(
            &[false; PoiKind::ALL.len()],
            &pois[0].kind
        ));
        let render = |show_pois: bool| {
            render_snapshot_with_pois(
                &snapshot(true, position),
                Some(W1_AREA_ID),
                false,
                show_pois,
                &visible_kinds(),
                &image(),
                &player_marker(),
                &poi_icons(),
                &pois,
                &mut Vec::new(),
            )
        };
        let counts = |show_pois: bool| {
            let frame = render(show_pois);
            let surface = frame.surface(SURFACE_ID).expect("minimap surface");
            (
                surface
                    .canvas()
                    .filter(|command| matches!(command.primitive(), PrimitiveRef::Image))
                    .count(),
                surface
                    .canvas()
                    .filter(|command| matches!(command.primitive(), PrimitiveRef::Circle))
                    .count(),
                surface
                    .canvas()
                    .filter(|command| matches!(command.primitive(), PrimitiveRef::Path(_)))
                    .count(),
            )
        };
        let baseline = counts(false);
        let shown = counts(true);
        assert_eq!(shown.0, baseline.0);
        assert_eq!(shown.1, baseline.1 + 2);
        assert_eq!(shown.2, baseline.2);
    }

    #[test]
    fn nearest_marker_prefers_the_closest_hit_and_ignores_empty_map_presses() {
        let hits = vec![
            MarkerHit {
                canvas: [10.0, 10.0],
                world: [1.0, 2.0],
                name: "Obelisk".to_owned(),
            },
            MarkerHit {
                canvas: [20.0, 10.0],
                world: [3.0, 4.0],
                name: "Chest".to_owned(),
            },
        ];

        assert_eq!(
            nearest_marker(&hits, 18.0, 10.0).map(|hit| hit.name.as_str()),
            Some("Chest")
        );
        // Both are in range; the closer one wins.
        assert_eq!(
            nearest_marker(&hits, 14.0, 10.0).map(|hit| hit.name.as_str()),
            Some("Obelisk")
        );
        // Empty map space is not a marker press.
        assert!(nearest_marker(&hits, 200.0, 200.0).is_none());
    }

    #[test]
    fn poi_buffer_refreshes_only_outside_the_inner_window() {
        let buffer = PoiBuffer {
            center_x: 0.0,
            center_y: 0.0,
            half_extent: 1500.0,
            revision: 1,
            pois: Vec::new(),
        };
        assert!(!buffer.needs_refresh(0.0, 0.0));
        assert!(!buffer.needs_refresh(749.0, -749.0));
        assert!(buffer.needs_refresh(751.0, 0.0));
        assert!(buffer.needs_refresh(0.0, -751.0));
        assert!(buffer.needs_refresh(5000.0, 5000.0));
    }

    #[test]
    fn north_label_is_anchored_above_the_map_outline() {
        for square in [false, true] {
            let layout = map_layout(square);
            let label = north_label_position(layout);

            assert!((label[0] + 4.0 - layout.center[0]).abs() < 0.0001);
            assert!(
                (label[1] + NORTH_LABEL_SIZE + NORTH_LABEL_GAP - layout.map_min[1]).abs() < 0.0001
            );
            assert!(label[1] >= 0.0);
        }
    }

    #[test]
    fn unavailable_player_world_or_map_hides_only_the_surface() {
        assert_eq!(
            render_snapshot(
                &snapshot(false, Some(Vec3::default())),
                Some(W1_AREA_ID),
                false,
                &image(),
                &player_marker(),
            )
            .surface_count(),
            0
        );
        assert_eq!(
            render_snapshot(
                &snapshot(true, None),
                Some(W1_AREA_ID),
                false,
                &image(),
                &player_marker(),
            )
            .surface_count(),
            0
        );
        assert_eq!(
            render_snapshot(
                &snapshot(true, Some(Vec3::default())),
                Some("World/W2_Dungeon"),
                false,
                &image(),
                &player_marker(),
            )
            .surface_count(),
            0
        );
    }

    #[test]
    fn blocking_game_windows_hide_the_minimap_surface() {
        let mut snapshot = snapshot(
            true,
            Some(Vec3 {
                x: -1800.0,
                y: -2000.0,
                z: 0.0,
            }),
        );
        snapshot
            .windows
            .value
            .as_mut()
            .expect("window snapshot")
            .open
            .push("ui.win.InventoryUI".to_owned());

        assert!(should_hide_for_game_ui(
            snapshot
                .windows
                .value
                .as_ref()
                .expect("window snapshot")
                .open
                .as_slice()
        ));
        assert_eq!(
            render_snapshot(
                &snapshot,
                Some(W1_AREA_ID),
                false,
                &image(),
                &player_marker(),
            )
            .surface_count(),
            0
        );
    }

    #[test]
    fn empty_game_window_registry_keeps_the_minimap_visible() {
        assert!(!should_hide_for_game_ui(&[]));
    }

    #[test]
    fn escape_menu_keeps_the_minimap_visible() {
        let mut snapshot = snapshot(
            true,
            Some(Vec3 {
                x: -1800.0,
                y: -2000.0,
                z: 0.0,
            }),
        );
        snapshot
            .windows
            .value
            .as_mut()
            .expect("window snapshot")
            .open
            .push("ui.win.EscapeMenu".to_owned());

        assert!(!should_hide_for_game_ui(
            &snapshot
                .windows
                .value
                .as_ref()
                .expect("window snapshot")
                .open
        ));
        assert_eq!(
            render_snapshot(
                &snapshot,
                Some(W1_AREA_ID),
                false,
                &image(),
                &player_marker(),
            )
            .surface_count(),
            1
        );

        snapshot
            .windows
            .value
            .as_mut()
            .expect("window snapshot")
            .open
            .push("ui.win.InventoryUI".to_owned());
        assert!(should_hide_for_game_ui(
            &snapshot
                .windows
                .value
                .as_ref()
                .expect("window snapshot")
                .open
        ));
    }

    #[test]
    fn reference_calibration_uses_x_y_map_axes_without_y_flip() {
        assert_eq!(
            reference_world_to_map_pixel(Vec3::default()),
            [
                REFERENCE_SOURCE_OFFSET_X / REFERENCE_MAP_COORDINATE_EXTENT
                    * MAP_SIZE_PIXELS
                    * REFERENCE_IMAGE_SCALE,
                REFERENCE_SOURCE_OFFSET_Y / REFERENCE_MAP_COORDINATE_EXTENT
                    * MAP_SIZE_PIXELS
                    * REFERENCE_IMAGE_SCALE,
            ]
        );
        assert_eq!(
            reference_world_to_map_pixel(Vec3 {
                x: 1.0,
                y: 1.0,
                z: 900.0,
            }),
            [
                (REFERENCE_SOURCE_OFFSET_X + REFERENCE_SOURCE_SCALE_X)
                    / REFERENCE_MAP_COORDINATE_EXTENT
                    * MAP_SIZE_PIXELS
                    * REFERENCE_IMAGE_SCALE,
                (REFERENCE_SOURCE_OFFSET_Y + REFERENCE_SOURCE_SCALE_Y)
                    / REFERENCE_MAP_COORDINATE_EXTENT
                    * MAP_SIZE_PIXELS
                    * REFERENCE_IMAGE_SCALE,
            ]
        );
    }

    #[test]
    fn reference_heading_zero_points_right_and_negative_quarter_turn_points_up() {
        let right = heading_vector(0.0);
        assert!((right[0] - 1.0).abs() < 0.0001);
        assert!(right[1].abs() < 0.0001);

        let up = heading_vector(-PI * 0.5);
        assert!(up[0].abs() < 0.0001);
        assert!((up[1] + 1.0).abs() < 0.0001);
    }

    #[test]
    fn player_marker_rotation_has_no_extra_base_offset() {
        let player_heading = -PI * 0.5;
        assert!((player_marker_rotation(Some(player_heading)) - player_heading).abs() < 0.0001);
    }

    #[test]
    fn reference_crop_is_a_small_local_view() {
        let (min, max) = crop_uv([2000.0, 2000.0], [228.0, 228.0]);
        assert!((max[0] - min[0] - 1.0 / REFERENCE_VIEW_ZOOM).abs() < 0.0001);
        assert!((max[1] - min[1] - 1.0 / REFERENCE_VIEW_ZOOM).abs() < 0.0001);
    }
}
