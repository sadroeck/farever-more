//! Host-side model for normalized Farever state, events, and UI frames.
//!
//! The portable add-on contract lives in `wit/farever-addon.wit`. These Rust
//! types are deliberately independent of the generated Wasmtime bindings and
//! never contain `HashLink` pointers.

use std::sync::Arc;

/// Whether a provider can currently supply a trustworthy value.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Availability {
    /// No value has been observed or the provider is unsupported.
    #[default]
    Unavailable,
    /// A previously observed value exists but is no longer current.
    Stale,
    /// The value was observed for the current process session.
    Live,
}

/// How a host event or state value was obtained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceQuality {
    /// Captured directly from a game event or runtime object.
    Observed,
    /// Detected by comparing periodic state snapshots.
    Sampled,
    /// Computed from other observed or sampled values.
    Derived,
}

/// Process-level state shared by every domain snapshot.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionState {
    /// Opaque ID that changes when the host attaches to a new game process.
    pub process_session: u64,
    pub adapter: Availability,
    pub in_world: bool,
    /// Raw game loading-state value; the enum mapping is not yet stable.
    pub loading_state: Option<i32>,
}

/// Why a state provider cannot currently return a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    NotInWorld,
    Loading,
    NotYetObserved,
    Unsupported,
    PermissionDenied,
    ProviderFailed,
}

/// Availability metadata and optional value for one independently sampled domain.
#[derive(Clone, Debug, PartialEq)]
pub struct StateSnapshot<T> {
    pub availability: Availability,
    pub observed_at_ms: Option<u64>,
    pub revision: u64,
    pub unavailable_reason: Option<UnavailableReason>,
    pub value: Option<T>,
}

impl<T> StateSnapshot<T> {
    #[must_use]
    pub fn live(observed_at_ms: u64, revision: u64, value: T) -> Self {
        Self {
            availability: Availability::Live,
            observed_at_ms: Some(observed_at_ms),
            revision,
            unavailable_reason: None,
            value: Some(value),
        }
    }

    #[must_use]
    pub fn unavailable(revision: u64, reason: UnavailableReason) -> Self {
        Self {
            availability: Availability::Unavailable,
            observed_at_ms: None,
            revision,
            unavailable_reason: Some(reason),
            value: None,
        }
    }
}

impl<T> Default for StateSnapshot<T> {
    fn default() -> Self {
        Self::unavailable(0, UnavailableReason::NotYetObserved)
    }
}

/// One position in Farever world coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Current local-player values normalized by the host.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerState {
    /// Opaque host actor ID shared with the local [`PartyMember`].
    pub runtime_id: Option<String>,
    pub name: Option<String>,
    pub class_id: Option<String>,
    pub position: Vec3,
    pub heading_radians: Option<f32>,
    pub in_combat: Option<bool>,
}

/// Current camera yaw in Farever world coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraState {
    pub heading_radians: f32,
}

/// The exact Farever Hero reference slot from which an observation came.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CombatReferenceSlot {
    Target,
    LockedTarget,
    AutoTarget,
}

/// One occupied combat-reference slot. The host deliberately does not choose
/// which slot an add-on should treat as its target.
#[derive(Clone, Debug, PartialEq)]
pub struct CombatReference {
    pub slot: CombatReferenceSlot,
    /// Absent when the slot is occupied but its entity position could not be
    /// validated at this observation boundary.
    pub position: Option<Vec3>,
}

/// All occupied Farever Hero reference slots at one observation boundary.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CombatReferencesState {
    pub references: Vec<CombatReference>,
}

/// Relationship between one combat actor and the local player.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ActorRelation {
    LocalPlayer,
    GroupMember,
    Other,
    #[default]
    Unknown,
}

/// Opaque, process-scoped identity and descriptive metadata for a combat actor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CombatActorRef {
    /// Opaque host actor ID. Group-member IDs correspond to
    /// [`PartyMember::actor_id`].
    pub actor_id: Option<String>,
    pub relation: ActorRelation,
    /// Runtime kind or class name. This does not identify one actor instance.
    pub kind: Option<String>,
}

/// One member of the local player's validated party.
#[derive(Clone, Debug, PartialEq)]
pub struct PartyMember {
    /// Opaque host actor ID, unique within this party and stable only for the
    /// attached game process.
    pub actor_id: String,
    pub is_local: bool,
    pub name: Option<String>,
    pub class_id: Option<String>,
    pub class_icon: Option<ImageRef>,
    pub in_combat: Option<bool>,
}

/// Current party membership. A solo player may appear as one local member.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PartyState {
    pub party_id: Option<String>,
    pub members: Vec<PartyMember>,
}

/// Best validated classification of a logical world instance.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InstanceKind {
    OpenWorld,
    Dungeon,
    Other,
    #[default]
    Unknown,
}

/// Host-owned instance session. Its ID is unique within `process_session`.
#[derive(Clone, Debug, PartialEq)]
pub struct InstanceState {
    pub session_id: u64,
    pub kind: InstanceKind,
    pub area_id: Option<String>,
}

/// Map data captured as one coherent host revision.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MapState {
    /// Stable internal area path, such as `World/W1_Siagarta`.
    pub area_id: Option<String>,
    /// Best available user-facing area name.
    pub display_name: Option<String>,
    /// Increments when any map field changes.
    pub data_revision: u64,
}

/// Snapshot of the host's known game-window registry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiState {
    pub open_windows: Vec<String>,
    /// Frontmost game-owned window from `BaseUI.windows`, not text/control
    /// input focus.
    pub focused_window: Option<String>,
    /// Increments when the open/focused window state changes.
    pub revision: u64,
}

/// Immutable state captured for a single add-on update.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GameSnapshot {
    /// Monotonic host poll sequence.
    pub sequence: u64,
    /// Milliseconds since this host process session began.
    pub captured_at_ms: u64,
    pub session: SessionState,
    pub player: StateSnapshot<PlayerState>,
    pub party: StateSnapshot<PartyState>,
    pub camera: StateSnapshot<CameraState>,
    pub combat_references: StateSnapshot<CombatReferencesState>,
    pub instance_session: StateSnapshot<InstanceState>,
    pub map: MapState,
    pub ui: UiState,
}

/// Ordering and provenance fields common to every event.
#[derive(Clone, Debug, PartialEq)]
pub struct EventHeader {
    /// Monotonic event sequence within `process_session`.
    pub sequence: u64,
    /// Milliseconds since this host process session began.
    pub monotonic_ms: u64,
    pub quality: SourceQuality,
}

/// Opaque host-provided reference to a validated immutable image resource.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ImageRef {
    pub id: String,
}

/// One host-selected actor-attributed damage observation.
#[derive(Clone, Debug, PartialEq)]
pub struct DamageEvent {
    pub header: EventHeader,
    pub source: CombatActorRef,
    pub target: CombatActorRef,
    /// Stable internal skill kind, not a localized display name.
    pub skill_id: String,
    /// Game-authored current-build display name, when present in static data.
    pub skill_display_name: Option<String>,
    /// Host-provided current-build skill icon, when present in static data.
    pub skill_icon: Option<ImageRef>,
    pub amount: f64,
    pub hit_count: u32,
    pub critical: bool,
    pub killed: bool,
    /// Portion reported as blocked; `None` means unavailable, not zero.
    pub blocked: Option<f64>,
}

/// One host-owned combat-session boundary.
///
/// `fight_id` is monotonic and unique within the containing `process_session`.
/// Start and end events for the same encounter carry the same ID.
#[derive(Clone, Debug, PartialEq)]
pub struct CombatEvent {
    pub header: EventHeader,
    pub fight_id: u64,
}

/// Party roster change. Consumers rebuild membership from the matching
/// [`GameSnapshot::party`] revision.
#[derive(Clone, Debug, PartialEq)]
pub struct PartyEvent {
    pub header: EventHeader,
    pub revision: u64,
}

/// Transition between host-owned logical world-instance sessions.
#[derive(Clone, Debug, PartialEq)]
pub struct InstanceEvent {
    pub header: EventHeader,
    pub previous: Option<InstanceState>,
    pub current: Option<InstanceState>,
}

/// Best available reason for a local-player disconnect across supported builds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayerDisconnectReason {
    ManualExit,
    Kick,
    Timeout,
    SwitchingServer,
    Unknown,
}

/// One local-player disconnect event.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerDisconnectedEvent {
    pub header: EventHeader,
    pub reason: PlayerDisconnectReason,
}

/// Typed events delivered to add-ons in sequence order.
#[derive(Clone, Debug, PartialEq)]
pub enum HostEvent {
    Damage(DamageEvent),
    CombatStarted(CombatEvent),
    CombatEnded(CombatEvent),
    PartyChanged(PartyEvent),
    InstanceChanged(InstanceEvent),
    ZoneChanged {
        header: EventHeader,
        previous_area_id: Option<String>,
        area_id: Option<String>,
    },
    UiWindowOpened {
        header: EventHeader,
        window_id: String,
    },
    UiWindowClosed {
        header: EventHeader,
        window_id: String,
    },
    PlayerDisconnected(PlayerDisconnectedEvent),
}

/// Bounded event delivery unit for one add-on update.
///
/// If `dropped_before` is non-zero or `snapshot_required` is true, consumers
/// must reconcile from the accompanying snapshot before treating later deltas
/// as continuous.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventBatch {
    pub process_session: u64,
    pub first_sequence: Option<u64>,
    /// Sequence assigned to the next event after this batch.
    pub next_sequence: u64,
    /// Number of events omitted before or while constructing this batch.
    pub dropped_before: u64,
    pub snapshot_required: bool,
    pub events: Vec<HostEvent>,
}

/// Linear RGBA color with components in the inclusive range `0.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub red: f32,
    pub green: f32,
    pub blue: f32,
    pub alpha: f32,
}

impl Rgba {
    pub const WHITE: Self = Self {
        red: 0.9,
        green: 0.93,
        blue: 0.97,
        alpha: 1.0,
    };
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

/// Host-relative anchor used to place an add-on surface.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SurfaceAnchor {
    #[default]
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Center,
    TopCenter,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LayoutDirection {
    #[default]
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ContainerStyle {
    #[default]
    Plain,
    Group,
    Scroll,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum TextStyle {
    #[default]
    Body,
    Small,
    Strong,
    Heading,
    Monospace,
}

/// Horizontal placement of content within a table column.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HorizontalAlignment {
    #[default]
    Left,
    Center,
    Right,
}

/// Host-managed width policy for one table column.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TableColumnSizing {
    /// Size from the column's visible cell contents.
    Auto,
    /// Reserve an exact number of logical points.
    Exact(f32),
    /// Equally share the width left after auto and exact columns are allocated.
    Remainder,
}

/// Declarative layout and responsive visibility for one table column.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TableColumn {
    pub sizing: TableColumnSizing,
    pub alignment: HorizontalAlignment,
    /// Show this column only when the table has at least this many logical
    /// points available. `None` makes the column always visible.
    pub visible_from_width: Option<f32>,
    /// Symmetric horizontal inset for cell content in logical points.
    pub content_padding: f32,
}

/// An aligned table whose direct children are [`TableRowWidget`] nodes.
#[derive(Clone, Debug, PartialEq)]
pub struct TableWidget {
    pub columns: Vec<TableColumn>,
    pub striped: bool,
    /// Maximum scrolling body height in logical points. The fixed header is
    /// outside this height.
    pub max_body_height: Option<f32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableRowKind {
    Header,
    Body,
}

/// One fixed-height table row. A header row remains above a scrolling body.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TableRowWidget {
    pub kind: TableRowKind,
    pub height: f32,
    /// Optional full-row background behind every visible cell.
    pub background: Option<Rgba>,
    /// Optional proportional background spanning the complete visible row.
    pub progress: Option<TableRowProgress>,
}

/// A proportional background track painted behind a table row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TableRowProgress {
    pub fraction: f32,
    pub color: Rgba,
    /// Zero-based always-visible column where the track starts, or the table
    /// leading edge when omitted.
    pub start_column: Option<usize>,
}

/// A table cell assigned to a zero-based column in its owning table.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TableCellWidget {
    pub column: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContainerWidget {
    pub direction: LayoutDirection,
    pub style: ContainerStyle,
    pub spacing: Option<f32>,
    pub max_height: Option<f32>,
}

/// A titled section: a host-styled header over the child nodes grouped under it.
/// The host draws the header itself so add-on pages read like the host's own
/// settings pages.
#[derive(Clone, Debug, PartialEq)]
pub struct SectionWidget {
    pub title: String,
    /// Optional muted line drawn under the title.
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextWidget {
    pub text: String,
    pub style: TextStyle,
    /// Host-resolved, add-on-namespaced font family for this semantic style.
    pub font_family: Option<String>,
    pub color: Option<Rgba>,
    /// Optional outline painted behind the laid-out glyphs.
    pub outline: Option<Stroke>,
    pub wrap: bool,
}

/// One image resource rendered at an explicit logical size.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageWidget {
    pub source: ImageRef,
    pub size: Size,
    pub tint: Option<Rgba>,
}

/// One host-rendered control. The containing [`UiNode::id`] is the stable
/// callback route delivered to the owning add-on when the button is pressed.
#[derive(Clone, Debug, PartialEq)]
pub struct ButtonWidget {
    pub label: String,
    pub enabled: bool,
}

/// One controlled boolean input rendered only inside an add-on config menu.
/// The add-on remains the source of truth and returns the next checked state in
/// its replacement frame after handling the semantic change event.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckboxWidget {
    pub label: String,
    pub checked: bool,
    pub enabled: bool,
}

/// One stable, user-visible choice in a controlled config dropdown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DropdownOption {
    pub id: String,
    pub label: String,
}

/// One controlled single-choice input rendered only inside an add-on config menu.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DropdownWidget {
    pub label: String,
    pub selected_id: String,
    pub options: Vec<DropdownOption>,
    pub enabled: bool,
}

/// One controlled numeric input over a contiguous inclusive range.
#[derive(Clone, Debug, PartialEq)]
pub struct SliderWidget {
    pub label: String,
    pub value: f64,
    pub minimum: f64,
    pub maximum: f64,
    /// Positive increment anchored at [`Self::minimum`], or `None` for continuous input.
    pub step: Option<f64>,
    pub enabled: bool,
}

/// One validated font face registered from bytes embedded in an add-on.
#[derive(Clone, Debug, PartialEq)]
pub struct FontAsset {
    /// Host-assigned family name, namespaced to the registering add-on.
    pub family: String,
    pub bytes: Arc<[u8]>,
}

/// One decoded, immutable RGBA image owned by the trusted host.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageAsset {
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProgressWidget {
    pub fraction: f32,
    pub label: Option<String>,
    pub color: Option<Rgba>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Widget {
    Container(ContainerWidget),
    Section(SectionWidget),
    Text(TextWidget),
    Image(ImageWidget),
    Button(ButtonWidget),
    Checkbox(CheckboxWidget),
    Dropdown(DropdownWidget),
    Slider(SliderWidget),
    Progress(ProgressWidget),
    Separator,
    Spacer(f32),
    Canvas(Size),
    Table(TableWidget),
    TableRow(TableRowWidget),
    TableCell(TableCellWidget),
}

/// A validated, parent-before-child widget node.
#[derive(Clone, Debug, PartialEq)]
pub struct UiNode {
    pub id: String,
    /// Index of a preceding parent-capable node. `None` places the node at the
    /// surface root. Validation restricts which structural widgets may contain
    /// each child kind.
    pub parent: Option<usize>,
    pub widget: Widget,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stroke {
    pub width: f32,
    pub color: Rgba,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CanvasPrimitive {
    Line {
        from: Point,
        to: Point,
        stroke: Stroke,
    },
    Rect {
        min: Point,
        max: Point,
        corner_radius: f32,
        fill: Option<Rgba>,
        stroke: Option<Stroke>,
    },
    Circle {
        center: Point,
        radius: f32,
        fill: Option<Rgba>,
        stroke: Option<Stroke>,
    },
    Path {
        points: Vec<Point>,
        closed: bool,
        fill: Option<Rgba>,
        stroke: Option<Stroke>,
    },
    Text {
        position: Point,
        text: String,
        color: Rgba,
        size: f32,
    },
    Image {
        source: ImageRef,
        destination_min: Point,
        destination_max: Point,
        uv_min: Point,
        uv_max: Point,
        rotation_radians: f32,
        tint: Option<Rgba>,
        corner_radius: f32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CanvasCommand {
    /// Index of the canvas node that receives this primitive.
    pub canvas: usize,
    pub primitive: CanvasPrimitive,
}

/// Opt-in surface chrome. `None` leaves appearance under host theme control.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceStyle {
    /// Whether the host-rendered title bar remains visible.
    pub title_bar: bool,
    pub fill: Rgba,
    pub stroke: Option<Stroke>,
    pub corner_radius: f32,
    pub padding: f32,
}

/// A renderer-independent top-level add-on window.
#[derive(Clone, Debug, PartialEq)]
pub struct UiSurface {
    /// Host-assigned add-on namespace. This is never controlled by the guest.
    pub owner: String,
    pub id: String,
    pub title: String,
    pub anchor: SurfaceAnchor,
    /// Signed horizontal offset for center and top-center anchors; non-negative inset for corners.
    pub margin_x: f32,
    /// Signed vertical offset for center anchors; top-center and corners use a non-negative inset.
    pub margin_y: f32,
    /// Preferred logical width. The host may reduce it to keep the surface in
    /// the current viewport, allowing responsive children to reflow.
    pub width: Option<f32>,
    pub style: Option<SurfaceStyle>,
    pub nodes: Vec<UiNode>,
    pub canvas: Vec<CanvasCommand>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigValueKind {
    Boolean,
    Integer,
    Number,
    Text,
    Bytes,
}

/// A typed effective configuration value exposed in the host inventory.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigValue {
    Boolean(bool),
    Integer(i64),
    Number(f64),
    Text(String),
    Bytes(Vec<u8>),
}

/// Host presentation policy for one add-on-declared configuration property.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigPropertyAccess {
    Editable,
    /// Shown by host-generated configuration UI, but not user-editable.
    Readonly,
    /// Retained in storage and inventory but omitted from configuration UI.
    Hidden,
}

/// One validated property in the host's central add-on configuration inventory.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigPropertyDescriptor {
    /// Host-assigned add-on namespace. This is never controlled by the guest.
    pub owner: String,
    pub key: String,
    pub label: String,
    pub description: Option<String>,
    pub value_kind: ConfigValueKind,
    pub default_value: ConfigValue,
    pub access: ConfigPropertyAccess,
}

/// Add-on-owned content embedded into the host's central configuration shell.
/// The host owns navigation, placement, chrome, and visibility.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigMenu {
    /// Host-assigned add-on namespace. This is never controlled by the guest.
    pub owner: String,
    pub id: String,
    pub title: String,
    pub nodes: Vec<UiNode>,
    pub canvas: Vec<CanvasCommand>,
}

/// Retained document that owned an interactive control.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum UiView {
    Surface(String),
    ConfigMenu(String),
}

/// Semantic UI event routed to one add-on. Raw pointer input never crosses the
/// component boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum UiEvent {
    ConfigMenuShown(String),
    ConfigMenuHidden(String),
    ButtonPressed {
        view: UiView,
        node_id: String,
    },
    /// Pointer press inside a canvas node, in canvas-local logical points.
    /// Add-ons receive this instead of raw pointer input, so they can hit-test
    /// the drawing they produced for that canvas.
    CanvasPressed {
        view: UiView,
        node_id: String,
        x: f64,
        y: f64,
    },
    /// Requested value for a checkbox on the currently selected config page.
    CheckboxChanged {
        node_id: String,
        checked: bool,
    },
    /// Requested option ID for a dropdown on the currently selected config page.
    DropdownChanged {
        node_id: String,
        selected_id: String,
    },
    /// Requested committed value for a slider on the currently selected config page.
    SliderChanged {
        node_id: String,
        value: f64,
    },
}

/// A semantic UI event plus the host-validated owner that must receive it.
#[derive(Clone, Debug, PartialEq)]
pub struct RoutedUiEvent {
    pub owner: String,
    pub event: UiEvent,
}

/// Complete visual output produced by one host or add-on update.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiFrame {
    pub surfaces: Vec<UiSurface>,
    pub config_menus: Vec<ConfigMenu>,
}

/// Maximum events delivered to an add-on in one update.
pub const MAX_EVENT_BATCH: usize = 256;
/// Maximum top-level surfaces returned by one add-on.
pub const MAX_UI_SURFACES_PER_ADDON: usize = 8;
/// Maximum configuration pages registered by one add-on.
pub const MAX_CONFIG_MENUS_PER_ADDON: usize = 8;
/// Maximum widget nodes returned by one add-on.
pub const MAX_UI_NODES_PER_ADDON: usize = 1_024;
/// Maximum declared columns in one table.
pub const MAX_UI_TABLE_COLUMNS: usize = 32;
/// Maximum choices accepted in one config dropdown.
pub const MAX_DROPDOWN_OPTIONS: usize = 64;
/// Maximum parent/child nesting accepted in an add-on frame.
pub const MAX_UI_DEPTH: usize = 16;
/// Maximum canvas commands returned by one add-on.
pub const MAX_CANVAS_COMMANDS_PER_ADDON: usize = 4_096;
/// Maximum points accepted in one canvas path.
pub const MAX_CANVAS_POINTS_PER_PATH: usize = 1_024;
/// Maximum aggregate UTF-8 payload accepted in one add-on frame.
pub const MAX_UI_TEXT_BYTES_PER_ADDON: usize = 64 * 1024;
/// Maximum font faces an add-on may register during initialization.
pub const MAX_FONTS_PER_ADDON: usize = 4;
/// Maximum encoded size of one registered OpenType face.
pub const MAX_FONT_BYTES_PER_FACE: usize = 2 * 1024 * 1024;
/// Maximum aggregate encoded font data registered by one add-on.
pub const MAX_FONT_BYTES_PER_ADDON: usize = 4 * 1024 * 1024;
/// Maximum immutable images an add-on may register during initialization.
pub const MAX_IMAGES_PER_ADDON: usize = 64;
/// Maximum PNG size accepted for one registered image.
pub const MAX_IMAGE_ENCODED_BYTES: usize = 16 * 1024 * 1024;
/// Maximum width or height accepted for one registered image.
pub const MAX_IMAGE_EDGE: u32 = 4_096;
/// Maximum decoded RGBA data allowed for one registered image.
pub const MAX_IMAGE_DECODED_BYTES_PER_IMAGE: usize = 64 * 1024 * 1024;

/// Version of the fixed-size native poller snapshot layout.
pub const FAS_POC_ABI: u32 = 1;
pub const AREA_CAPACITY: usize = 256;
pub const WINDOW_CAPACITY: usize = 16;
pub const WINDOW_NAME_CAPACITY: usize = 96;

pub const COMBAT_REFERENCE_TARGET: u8 = 1 << 0;
pub const COMBAT_REFERENCE_LOCKED_TARGET: u8 = 1 << 1;
pub const COMBAT_REFERENCE_AUTO_TARGET: u8 = 1 << 2;

pub const ADAPTER_SEARCHING: u32 = 0;
pub const ADAPTER_LIVE: u32 = 1;
pub const ADAPTER_WAITING_FOR_GAME: u32 = 2;
pub const ADAPTER_UNAVAILABLE: u32 = 3;
/// Farever exists, but the host is cheaply waiting for boot/loading activity
/// to settle before it reads bulk `HashLink` memory.
pub const ADAPTER_WAITING_TO_SCAN: u32 = 4;

/// Fixed-size output of the trusted state poller.
///
/// This internal native layout is converted into [`GameSnapshot`] before any
/// data crosses the WebAssembly component boundary.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FasSnapshotV0 {
    pub struct_size: u32,
    pub abi: u32,
    pub sequence: u64,
    pub process_id: u32,
    pub adapter_status: u32,
    pub process_found: u8,
    pub app_found: u8,
    pub in_world: u8,
    /// Heuristic completion percentage for the cold `HashLink` anchor scan.
    /// This occupied the final reserved status byte in ABI 0; ABI 1 appends
    /// the spatial-provider fields below.
    pub scan_progress: u8,
    pub loading_state: i32,
    pub area_len: u32,
    pub area: [u8; AREA_CAPACITY],
    pub window_count: u32,
    pub windows: [[u8; WINDOW_NAME_CAPACITY]; WINDOW_CAPACITY],
    pub player_position_valid: u8,
    pub player_heading_valid: u8,
    pub camera_heading_valid: u8,
    pub combat_references_available: u8,
    /// Occupied slots, using the `COMBAT_REFERENCE_*` bit constants.
    pub combat_reference_active_mask: u8,
    /// Occupied slots whose positions passed validation.
    pub combat_reference_position_mask: u8,
    pub pose_reserved: [u8; 2],
    pub player_position: [f64; 3],
    pub player_heading_radians: f64,
    pub camera_heading_radians: f64,
    /// Positions ordered as target, locked-target, auto-target.
    pub combat_reference_positions: [[f64; 3]; 3],
}

impl Default for FasSnapshotV0 {
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(core::mem::size_of::<Self>())
                .expect("snapshot structure size fits in the native ABI field"),
            abi: FAS_POC_ABI,
            sequence: 0,
            process_id: 0,
            adapter_status: ADAPTER_SEARCHING,
            process_found: 0,
            app_found: 0,
            in_world: 0,
            scan_progress: 0,
            loading_state: -1,
            area_len: 0,
            area: [0; AREA_CAPACITY],
            window_count: 0,
            windows: [[0; WINDOW_NAME_CAPACITY]; WINDOW_CAPACITY],
            player_position_valid: 0,
            player_heading_valid: 0,
            camera_heading_valid: 0,
            combat_references_available: 0,
            combat_reference_active_mask: 0,
            combat_reference_position_mask: 0,
            pose_reserved: [0; 2],
            player_position: [0.0; 3],
            player_heading_radians: 0.0,
            camera_heading_radians: 0.0,
            combat_reference_positions: [[0.0; 3]; 3],
        }
    }
}

impl FasSnapshotV0 {
    /// Replaces the fixed-size area buffer, truncating only at a UTF-8 boundary.
    ///
    /// # Panics
    ///
    /// Panics only if [`AREA_CAPACITY`] cannot fit in the ABI's `u32` length
    /// field, which is impossible for the declared fixed buffer.
    pub fn set_area(&mut self, value: &str) {
        self.area_len = u32::try_from(copy_utf8(value, &mut self.area))
            .expect("area capacity fits in the native ABI field");
    }

    /// Returns the area buffer as UTF-8, bounded by the recorded length.
    #[must_use]
    pub fn area(&self) -> &str {
        let len = (self.area_len as usize).min(self.area.len());
        core::str::from_utf8(&self.area[..len]).unwrap_or("<invalid UTF-8>")
    }

    /// Appends a window type name if fixed snapshot capacity remains.
    pub fn push_window(&mut self, value: &str) {
        let index = self.window_count as usize;
        if index >= WINDOW_CAPACITY {
            return;
        }
        copy_utf8(value, &mut self.windows[index]);
        self.window_count += 1;
    }

    /// Returns one recorded window type name.
    #[must_use]
    pub fn window(&self, index: usize) -> Option<&str> {
        if index >= (self.window_count as usize).min(WINDOW_CAPACITY) {
            return None;
        }
        let bytes = &self.windows[index];
        let len = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        core::str::from_utf8(&bytes[..len]).ok()
    }
}

fn copy_utf8(value: &str, destination: &mut [u8]) -> usize {
    let mut len = value.len().min(destination.len());
    while len > 0 && !value.is_char_boundary(len) {
        len -= 1;
    }
    destination[..len].copy_from_slice(&value.as_bytes()[..len]);
    if len < destination.len() {
        destination[len] = 0;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_buffers_truncate_on_a_utf8_boundary() {
        let mut snapshot = FasSnapshotV0::default();
        snapshot.set_area(&"é".repeat(200));
        assert!(snapshot.area().is_char_boundary(snapshot.area().len()));
        assert!(snapshot.area().len() <= AREA_CAPACITY);
    }
}
