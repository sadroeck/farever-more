//! Build-gated direct HashLink hooks for Farever UI-window lifecycle edges.
//!
//! Hook callbacks may run on arbitrary game threads. They only forward the
//! original call, copy fixed-size pointer identity, and enqueue a bounded raw
//! record. Runtime type lookup, duplicate suppression, diagnostics, and later
//! geometry work stay on the existing HashLink observer worker.

use crate::hashlink::{
    validate_object, HashLink, HashLinkFieldSpec, HashLinkKind, HashLinkMethodSpec,
    HashLinkObjectSpec, HashLinkRuntime, HashLinkTypeSpec, ValidatedHashLinkMethod,
};
use crossbeam_queue::ArrayQueue;
use minhook::MinHook;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

const RAW_QUEUE_CAPACITY: usize = 512;
const EDGE_QUEUE_CAPACITY: usize = 256;
const MAX_DECODE_PER_TICK: usize = 64;
const ESCAPE_MENU_TYPE: &str = "ui.win.EscapeMenu";

const ESCAPE_MENU_GEOMETRY_FIELDS: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::object("parent", "h2d.Object"),
    HashLinkFieldSpec::scalar("matA", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("matB", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("matC", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("matD", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("absX", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("absY", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("calculatedWidth", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("calculatedHeight", HashLinkKind::F64),
];
const ESCAPE_MENU_GEOMETRY_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: ESCAPE_MENU_TYPE,
    kind: HashLinkKind::Object,
    fields: ESCAPE_MENU_GEOMETRY_FIELDS,
};
const H2D_SCENE_TRANSFORM_FIELDS: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::scalar("viewportScaleX", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("viewportScaleY", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("offsetX", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("offsetY", HashLinkKind::F64),
];
const H2D_SCENE_TRANSFORM_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "h2d.Scene",
    kind: HashLinkKind::Object,
    fields: H2D_SCENE_TRANSFORM_FIELDS,
};
const MAX_SCENE_PARENT_DEPTH: usize = 32;

const DISPLAY_WINDOW_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ui.BaseUI"),
    HashLinkTypeSpec::Object("ui.win.BaseWindow"),
    HashLinkTypeSpec::Object("ui.UIElement"),
];
const REMOVE_WINDOW_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ui.BaseUI"),
    HashLinkTypeSpec::Object("ui.win.BaseWindow"),
];

const GAME_UI_DISPLAY_WINDOW: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ui.GameUI",
    name: c"displayWindow",
    arguments: DISPLAY_WINDOW_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(crate::hashlink::HashLinkKind::Void),
};
const GAME_UI_REMOVE_WINDOW: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ui.GameUI",
    name: c"removeWindow",
    arguments: REMOVE_WINDOW_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(crate::hashlink::HashLinkKind::Void),
};
const BASE_UI_DISPLAY_WINDOW: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ui.BaseUI",
    name: c"displayWindow",
    arguments: DISPLAY_WINDOW_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(crate::hashlink::HashLinkKind::Void),
};
const BASE_UI_REMOVE_WINDOW: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ui.BaseUI",
    name: c"removeWindow",
    arguments: REMOVE_WINDOW_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(crate::hashlink::HashLinkKind::Void),
};

type HlDisplayWindow = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);
type HlRemoveWindow = unsafe extern "C" fn(*mut c_void, *mut c_void);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawWindowEdgeKind {
    Opened,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RawWindowEdge {
    kind: RawWindowEdgeKind,
    window: usize,
    type_pointer: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct WindowGeometry {
    /// Absolute bounds reported by the live Heaps UI object. The overlay
    /// validates the coordinate space against the current client before use.
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) runtime_x: f32,
    pub(crate) runtime_y: f32,
    pub(crate) runtime_width: f32,
    pub(crate) runtime_height: f32,
    pub(crate) viewport_scale_x: f32,
    pub(crate) viewport_scale_y: f32,
    pub(crate) viewport_offset_x: f32,
    pub(crate) viewport_offset_y: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WindowHookEdge {
    pub(crate) opened: bool,
    pub(crate) window_pointer: usize,
    pub(crate) runtime_type: String,
    pub(crate) geometry: Option<WindowGeometry>,
    pub(crate) geometry_refresh: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct FocusedWindowState {
    known: bool,
    runtime_type: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WindowSnapshotEntry {
    window_pointer: usize,
    runtime_type: String,
}

#[derive(Clone, Copy, Debug)]
struct EscapeMenuGeometryLayout {
    type_pointer: usize,
    parent: usize,
    matrix_a: usize,
    matrix_b: usize,
    matrix_c: usize,
    matrix_d: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

#[derive(Clone, Copy, Debug)]
struct RuntimeWindowGeometry {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    matrix_a: f64,
    matrix_b: f64,
    matrix_c: f64,
    matrix_d: f64,
}

#[derive(Clone, Copy, Debug)]
struct SceneTransform {
    scale_x: f64,
    scale_y: f64,
    offset_x: f64,
    offset_y: f64,
}

#[derive(Default)]
pub(crate) struct WindowHookDecoder {
    active: HashMap<usize, String>,
    /// Mirrors `BaseUI.windows` order. `displayWindow` inserts at index zero,
    /// so the first entry is the frontmost game-owned window.
    focus_order: Vec<usize>,
    observed_raw_drops: u64,
    escape_menu_geometry: Option<EscapeMenuGeometryLayout>,
    escape_menu_geometry_failed: bool,
    escape_menu_geometry_pointer: Option<usize>,
}

impl WindowHookDecoder {
    pub(crate) fn decode_pending(&mut self, hl: &HashLink<'_>) {
        let raw_drops = RAW_DROPS.load(Ordering::Acquire);
        if raw_drops != self.observed_raw_drops {
            // A missing lifecycle record makes pointer membership ambiguous.
            // Clear shadow state instead of manufacturing an edge. A later
            // one-time BaseUI.windows seed can provide full reconciliation.
            self.active.clear();
            self.focus_order.clear();
            publish_focus(false, None);
            self.observed_raw_drops = raw_drops;
            RESYNCS.fetch_add(1, Ordering::Relaxed);
        }

        if let Some(snapshot) = take_reconciliation_snapshot() {
            self.reconcile(snapshot);
            RESYNCS.fetch_add(1, Ordering::Relaxed);
        }

        if let Some(queue) = RAW_EDGES.get() {
            for _ in 0..MAX_DECODE_PER_TICK {
                let Some(raw) = queue.pop() else {
                    break;
                };
                let runtime_type = hl.type_name(raw.type_pointer);
                if let Some(edge) = self.apply(raw, runtime_type) {
                    publish_edge(edge);
                }
            }
        }
        self.refresh_escape_menu_geometry(hl);
    }

    fn reconcile(&mut self, snapshot: Vec<WindowSnapshotEntry>) {
        self.focus_order = snapshot.iter().map(|entry| entry.window_pointer).collect();
        self.active = snapshot
            .into_iter()
            .map(|entry| (entry.window_pointer, entry.runtime_type))
            .collect();
        self.publish_focus();
    }

    fn capture_escape_menu_geometry(
        &mut self,
        hl: &HashLink<'_>,
        raw: RawWindowEdge,
    ) -> Option<WindowGeometry> {
        if hl.memory.u64(raw.window) != Some(raw.type_pointer) {
            GEOMETRY_INVALID.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        if self.escape_menu_geometry.is_none() && !self.escape_menu_geometry_failed {
            match resolve_escape_menu_geometry(hl, raw.type_pointer) {
                Ok(layout) => self.escape_menu_geometry = Some(layout),
                Err(error) => {
                    let _ = GEOMETRY_ERROR.set(error);
                    self.escape_menu_geometry_failed = true;
                }
            }
        }
        let layout = self.escape_menu_geometry?;
        if layout.type_pointer != raw.type_pointer {
            GEOMETRY_INVALID.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let runtime = RuntimeWindowGeometry {
            x: hl.memory.f64(raw.window.checked_add(layout.x)?)?,
            y: hl.memory.f64(raw.window.checked_add(layout.y)?)?,
            width: hl.memory.f64(raw.window.checked_add(layout.width)?)?,
            height: hl.memory.f64(raw.window.checked_add(layout.height)?)?,
            matrix_a: hl.memory.f64(raw.window.checked_add(layout.matrix_a)?)?,
            matrix_b: hl.memory.f64(raw.window.checked_add(layout.matrix_b)?)?,
            matrix_c: hl.memory.f64(raw.window.checked_add(layout.matrix_c)?)?,
            matrix_d: hl.memory.f64(raw.window.checked_add(layout.matrix_d)?)?,
        };
        let scene = match resolve_scene_transform(hl, raw.window, layout.parent) {
            Ok(scene) => scene,
            Err(error) => {
                let _ = GEOMETRY_ERROR.set(error);
                GEOMETRY_INVALID.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        let geometry = project_window_geometry(runtime, scene);
        if !window_geometry_is_plausible(geometry) {
            GEOMETRY_INVALID.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        GEOMETRY_CAPTURED.fetch_add(1, Ordering::Relaxed);
        Some(geometry)
    }

    fn refresh_escape_menu_geometry(&mut self, hl: &HashLink<'_>) {
        let window_pointer = self.active_escape_menu_pointer();
        let Some(window_pointer) = window_pointer else {
            self.escape_menu_geometry_pointer = None;
            return;
        };
        if self.escape_menu_geometry_pointer == Some(window_pointer)
            || self.escape_menu_geometry_failed
        {
            return;
        }
        let Some(type_pointer) = hl.memory.u64(window_pointer) else {
            GEOMETRY_RECONCILIATION_REQUIRED.store(true, Ordering::Release);
            return;
        };
        if hl.type_name(type_pointer).as_deref() != Some(ESCAPE_MENU_TYPE) {
            GEOMETRY_RECONCILIATION_REQUIRED.store(true, Ordering::Release);
            return;
        }
        let raw = RawWindowEdge {
            kind: RawWindowEdgeKind::Opened,
            window: window_pointer,
            type_pointer,
        };
        let Some(geometry) = self.capture_escape_menu_geometry(hl, raw) else {
            return;
        };
        if publish_edge(WindowHookEdge {
            opened: true,
            window_pointer,
            runtime_type: ESCAPE_MENU_TYPE.to_owned(),
            geometry: Some(geometry),
            geometry_refresh: true,
        }) {
            self.escape_menu_geometry_pointer = Some(window_pointer);
            GEOMETRY_RECONCILIATION_REQUIRED.store(false, Ordering::Release);
        }
    }

    fn active_escape_menu_pointer(&self) -> Option<usize> {
        self.focus_order.iter().copied().find(|pointer| {
            self.active
                .get(pointer)
                .is_some_and(|runtime_type| runtime_type == ESCAPE_MENU_TYPE)
        })
    }

    fn apply(
        &mut self,
        raw: RawWindowEdge,
        runtime_type: Option<String>,
    ) -> Option<WindowHookEdge> {
        match raw.kind {
            RawWindowEdgeKind::Opened => {
                if self.active.contains_key(&raw.window) {
                    DUPLICATES.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                let Some(runtime_type) = runtime_type else {
                    INVALID.fetch_add(1, Ordering::Relaxed);
                    return None;
                };
                self.active.insert(raw.window, runtime_type.clone());
                self.focus_order.insert(0, raw.window);
                self.publish_focus();
                OPENED.fetch_add(1, Ordering::Relaxed);
                Some(WindowHookEdge {
                    opened: true,
                    window_pointer: raw.window,
                    runtime_type,
                    geometry: None,
                    geometry_refresh: false,
                })
            }
            RawWindowEdgeKind::Closed => {
                let (removed_pointer, runtime_type) =
                    if let Some(runtime_type) = self.active.remove(&raw.window) {
                        (raw.window, runtime_type)
                    } else {
                        // HashLink objects may move during collection. If this is
                        // the sole active window of its concrete runtime type,
                        // reconcile the close by type without pretending an
                        // ambiguous same-class instance is the same object.
                        let Some(runtime_type) = runtime_type else {
                            UNMATCHED_CLOSES.fetch_add(1, Ordering::Relaxed);
                            return None;
                        };
                        let mut matches = self
                            .active
                            .iter()
                            .filter(|(_, active_type)| *active_type == &runtime_type)
                            .map(|(pointer, _)| *pointer);
                        let Some(previous_pointer) = matches.next() else {
                            UNMATCHED_CLOSES.fetch_add(1, Ordering::Relaxed);
                            return None;
                        };
                        if matches.next().is_some() {
                            UNMATCHED_CLOSES.fetch_add(1, Ordering::Relaxed);
                            return None;
                        }
                        self.active.remove(&previous_pointer);
                        RELOCATIONS.fetch_add(1, Ordering::Relaxed);
                        (previous_pointer, runtime_type)
                    };
                self.focus_order
                    .retain(|pointer| *pointer != removed_pointer);
                self.publish_focus();
                CLOSED.fetch_add(1, Ordering::Relaxed);
                Some(WindowHookEdge {
                    opened: false,
                    window_pointer: raw.window,
                    runtime_type,
                    geometry: None,
                    geometry_refresh: false,
                })
            }
        }
    }

    fn publish_focus(&self) {
        let focused = self
            .focus_order
            .first()
            .and_then(|pointer| self.active.get(pointer))
            .cloned();
        publish_focus(true, focused);
    }
}

fn resolve_escape_menu_geometry(
    hl: &HashLink<'_>,
    type_pointer: usize,
) -> Result<EscapeMenuGeometryLayout, String> {
    let object = validate_object(hl, type_pointer, &ESCAPE_MENU_GEOMETRY_SCHEMA)?;
    let offset = |name| {
        object
            .offset(name)
            .ok_or_else(|| format!("validated EscapeMenu layout omitted {name}"))
    };
    Ok(EscapeMenuGeometryLayout {
        type_pointer: object.type_address,
        parent: offset("parent")?,
        matrix_a: offset("matA")?,
        matrix_b: offset("matB")?,
        matrix_c: offset("matC")?,
        matrix_d: offset("matD")?,
        x: offset("absX")?,
        y: offset("absY")?,
        width: offset("calculatedWidth")?,
        height: offset("calculatedHeight")?,
    })
}

fn resolve_scene_transform(
    hl: &HashLink<'_>,
    window: usize,
    parent_offset: usize,
) -> Result<SceneTransform, String> {
    let mut object = window;
    for _ in 0..MAX_SCENE_PARENT_DEPTH {
        let type_pointer = hl
            .memory
            .u64(object)
            .ok_or_else(|| "could not read UI parent type".to_owned())?;
        if hl.type_name(type_pointer).as_deref() == Some(H2D_SCENE_TRANSFORM_SCHEMA.name) {
            let scene = validate_object(hl, type_pointer, &H2D_SCENE_TRANSFORM_SCHEMA)?;
            let read =
                |name| {
                    let offset = scene
                        .offset(name)
                        .ok_or_else(|| format!("validated h2d.Scene layout omitted {name}"))?;
                    hl.memory
                        .f64(object.checked_add(offset).ok_or_else(|| {
                            format!("h2d.Scene field address overflowed for {name}")
                        })?)
                        .ok_or_else(|| format!("could not read h2d.Scene.{name}"))
                };
            return Ok(SceneTransform {
                scale_x: read("viewportScaleX")?,
                scale_y: read("viewportScaleY")?,
                offset_x: read("offsetX")?,
                offset_y: read("offsetY")?,
            });
        }

        let parent = hl
            .memory
            .u64(
                object
                    .checked_add(parent_offset)
                    .ok_or_else(|| "UI parent address overflowed".to_owned())?,
            )
            .ok_or_else(|| "could not read UI parent".to_owned())?;
        if parent < 0x1_0000 {
            return Err("EscapeMenu parent chain did not reach h2d.Scene".to_owned());
        }
        object = parent;
    }
    Err(format!(
        "EscapeMenu parent chain exceeded {MAX_SCENE_PARENT_DEPTH} objects"
    ))
}

fn project_window_geometry(
    runtime: RuntimeWindowGeometry,
    scene: SceneTransform,
) -> WindowGeometry {
    let mut minimum_x = f64::INFINITY;
    let mut minimum_y = f64::INFINITY;
    let mut maximum_x = f64::NEG_INFINITY;
    let mut maximum_y = f64::NEG_INFINITY;
    for (local_x, local_y) in [
        (0.0, 0.0),
        (runtime.width, 0.0),
        (0.0, runtime.height),
        (runtime.width, runtime.height),
    ] {
        let scene_x = runtime.x + local_x * runtime.matrix_a + local_y * runtime.matrix_c;
        let scene_y = runtime.y + local_x * runtime.matrix_b + local_y * runtime.matrix_d;
        let physical_x = scene.offset_x + scene_x * scene.scale_x;
        let physical_y = scene.offset_y + scene_y * scene.scale_y;
        minimum_x = minimum_x.min(physical_x);
        minimum_y = minimum_y.min(physical_y);
        maximum_x = maximum_x.max(physical_x);
        maximum_y = maximum_y.max(physical_y);
    }
    WindowGeometry {
        x: minimum_x as f32,
        y: minimum_y as f32,
        width: (maximum_x - minimum_x) as f32,
        height: (maximum_y - minimum_y) as f32,
        runtime_x: runtime.x as f32,
        runtime_y: runtime.y as f32,
        runtime_width: runtime.width as f32,
        runtime_height: runtime.height as f32,
        viewport_scale_x: scene.scale_x as f32,
        viewport_scale_y: scene.scale_y as f32,
        viewport_offset_x: scene.offset_x as f32,
        viewport_offset_y: scene.offset_y as f32,
    }
}

fn window_geometry_is_plausible(geometry: WindowGeometry) -> bool {
    [
        geometry.x,
        geometry.y,
        geometry.width,
        geometry.height,
        geometry.runtime_x,
        geometry.runtime_y,
        geometry.runtime_width,
        geometry.runtime_height,
        geometry.viewport_scale_x,
        geometry.viewport_scale_y,
        geometry.viewport_offset_x,
        geometry.viewport_offset_y,
    ]
    .iter()
    .all(|value| value.is_finite())
        && (-16_384.0..=16_384.0).contains(&geometry.x)
        && (-16_384.0..=16_384.0).contains(&geometry.y)
        && (64.0..=4_096.0).contains(&geometry.width)
        && (64.0..=4_096.0).contains(&geometry.height)
        && (64.0..=4_096.0).contains(&geometry.runtime_width)
        && (64.0..=4_096.0).contains(&geometry.runtime_height)
        && (0.1..=10.0).contains(&geometry.viewport_scale_x.abs())
        && (0.1..=10.0).contains(&geometry.viewport_scale_y.abs())
}

static UI_TYPE: AtomicUsize = AtomicUsize::new(0);
// 0 = waiting for type, 1 = active, 3 = failed, 4 = installing.
static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static GEOMETRY_ERROR: OnceLock<String> = OnceLock::new();
static GEOMETRY_RECONCILIATION_REQUIRED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static DISPLAY_TARGET: AtomicUsize = AtomicUsize::new(0);
static REMOVE_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_DISPLAY: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_REMOVE: AtomicUsize = AtomicUsize::new(0);
static RAW_EDGES: OnceLock<ArrayQueue<RawWindowEdge>> = OnceLock::new();
static EDGES: OnceLock<ArrayQueue<WindowHookEdge>> = OnceLock::new();
static RECONCILIATION_SNAPSHOT: OnceLock<Mutex<Option<Vec<WindowSnapshotEntry>>>> = OnceLock::new();
static FOCUSED_WINDOW: OnceLock<Mutex<FocusedWindowState>> = OnceLock::new();
static RAW_DROPS: AtomicU64 = AtomicU64::new(0);
static EDGE_DROPS: AtomicU64 = AtomicU64::new(0);
static OPENED: AtomicU64 = AtomicU64::new(0);
static CLOSED: AtomicU64 = AtomicU64::new(0);
static DUPLICATES: AtomicU64 = AtomicU64::new(0);
static UNMATCHED_CLOSES: AtomicU64 = AtomicU64::new(0);
static RELOCATIONS: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static RESYNCS: AtomicU64 = AtomicU64::new(0);
static GEOMETRY_CAPTURED: AtomicU64 = AtomicU64::new(0);
static GEOMETRY_INVALID: AtomicU64 = AtomicU64::new(0);
static FOCUS_CHANGES: AtomicU64 = AtomicU64::new(0);
static FOCUS_INVALIDATIONS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn prepare_queues() {
    let _ = RAW_EDGES.get_or_init(|| ArrayQueue::new(RAW_QUEUE_CAPACITY));
    let _ = EDGES.get_or_init(|| ArrayQueue::new(EDGE_QUEUE_CAPACITY));
    let _ = RECONCILIATION_SNAPSHOT.get_or_init(|| Mutex::new(None));
    let _ = FOCUSED_WINDOW.get_or_init(|| Mutex::new(FocusedWindowState::default()));
}

fn publish_focus(known: bool, runtime_type: Option<String>) {
    let state = FOCUSED_WINDOW.get_or_init(|| Mutex::new(FocusedWindowState::default()));
    if let Ok(mut state) = state.lock() {
        if state.known == known && state.runtime_type == runtime_type {
            return;
        }
        if known {
            FOCUS_CHANGES.fetch_add(1, Ordering::Relaxed);
        } else {
            FOCUS_INVALIDATIONS.fetch_add(1, Ordering::Relaxed);
        }
        *state = FocusedWindowState {
            known,
            runtime_type,
        };
    }
}

/// Returns `None` until a direct edge or one bounded reconciliation has
/// established frontmost-window state. `Some(None)` means the registry is
/// authoritatively empty.
pub(crate) fn current_focus() -> Option<Option<String>> {
    let state = FOCUSED_WINDOW.get()?.lock().ok()?.clone();
    state.known.then_some(state.runtime_type)
}

/// Replaces the decoder's pointer/type membership from one bounded native
/// snapshot. This is called only for hook handover, root replacement, or loss
/// recovery; hook callbacks never acquire this lock.
pub(crate) fn reconcile_active_windows(entries: Vec<(usize, String)>) {
    let snapshot = entries
        .into_iter()
        .map(|(window_pointer, runtime_type)| WindowSnapshotEntry {
            window_pointer,
            runtime_type,
        })
        .collect();
    let slot = RECONCILIATION_SNAPSHOT.get_or_init(|| Mutex::new(None));
    if let Ok(mut pending) = slot.lock() {
        *pending = Some(snapshot);
        GEOMETRY_RECONCILIATION_REQUIRED.store(false, Ordering::Release);
    }
}

pub(crate) fn geometry_reconciliation_required() -> bool {
    GEOMETRY_RECONCILIATION_REQUIRED.load(Ordering::Acquire)
}

fn take_reconciliation_snapshot() -> Option<Vec<WindowSnapshotEntry>> {
    RECONCILIATION_SNAPSHOT
        .get()
        .and_then(|slot| slot.lock().ok()?.take())
}

pub(crate) fn observe_ui_type(type_pointer: usize) {
    if type_pointer >= 0x1_0000 {
        let _ = UI_TYPE.compare_exchange(0, type_pointer, Ordering::AcqRel, Ordering::Acquire);
    }
}

pub(crate) fn try_install_hooks(hl: &HashLink<'_>) {
    if HOOK_STATUS.load(Ordering::Acquire) != 0 {
        return;
    }
    let ui_type = UI_TYPE.load(Ordering::Acquire);
    if ui_type == 0 {
        return;
    }
    if HOOK_STATUS
        .compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let result = resolve_ui_methods(hl, ui_type)
        .and_then(|(display, remove)| install_hooks(display, remove));
    match result {
        Ok(()) => HOOK_STATUS.store(1, Ordering::Release),
        Err(error) => {
            let _ = HOOK_ERROR.set(error);
            HOOK_STATUS.store(3, Ordering::Release);
        }
    }
}

fn resolve_ui_methods(
    hl: &HashLink<'_>,
    ui_type: usize,
) -> Result<(ValidatedHashLinkMethod, ValidatedHashLinkMethod), String> {
    let runtime_type = hl
        .type_name(ui_type)
        .ok_or_else(|| "UI hook lookup type is unreadable".to_owned())?;
    let (display_spec, remove_spec) = match runtime_type.as_str() {
        "ui.GameUI" => (&GAME_UI_DISPLAY_WINDOW, &GAME_UI_REMOVE_WINDOW),
        "ui.BaseUI" => (&BASE_UI_DISPLAY_WINDOW, &BASE_UI_REMOVE_WINDOW),
        _ => return Err(format!("unsupported UI hook lookup type {runtime_type}")),
    };
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    let display = runtime.resolve_method(hl, ui_type, display_spec)?;
    let remove = runtime.resolve_method(hl, ui_type, remove_spec)?;
    Ok((display, remove))
}

fn install_hooks(
    display: ValidatedHashLinkMethod,
    remove: ValidatedHashLinkMethod,
) -> Result<(), String> {
    let display_target = display.target() as *mut c_void;
    let remove_target = remove.target() as *mut c_void;
    if display_target == remove_target {
        return Err("displayWindow and removeWindow resolved to the same target".to_owned());
    }

    // SAFETY: both targets were resolved by declaring name and validated exact
    // HashLink signatures on the already build-gated observer worker.
    let display_original = std::panic::catch_unwind(|| unsafe {
        MinHook::create_hook(display_target, hook_display_window as *mut c_void)
    })
    .map_err(|_| "MinHook initialization panicked for displayWindow".to_owned())?
    .map_err(|status| format!("create displayWindow hook returned {status:?}"))?;
    let remove_original = match std::panic::catch_unwind(|| unsafe {
        MinHook::create_hook(remove_target, hook_remove_window as *mut c_void)
    }) {
        Ok(Ok(original)) => original,
        Ok(Err(status)) => {
            // SAFETY: the display hook was created immediately above.
            let _ = unsafe { MinHook::remove_hook(display_target) };
            return Err(format!("create removeWindow hook returned {status:?}"));
        }
        Err(_) => {
            // SAFETY: the display hook was created immediately above.
            let _ = unsafe { MinHook::remove_hook(display_target) };
            return Err("MinHook initialization panicked for removeWindow".to_owned());
        }
    };
    ORIGINAL_DISPLAY.store(display_original as usize, Ordering::Release);
    ORIGINAL_REMOVE.store(remove_original as usize, Ordering::Release);

    // SAFETY: both hooks and their original trampolines were created above.
    if let Err(status) = unsafe { MinHook::enable_hook(display_target) } {
        let _ = unsafe { MinHook::remove_hook(remove_target) };
        let _ = unsafe { MinHook::remove_hook(display_target) };
        ORIGINAL_DISPLAY.store(0, Ordering::Release);
        ORIGINAL_REMOVE.store(0, Ordering::Release);
        return Err(format!("enable displayWindow hook returned {status:?}"));
    }
    // SAFETY: the remove hook and its original trampoline were created above.
    if let Err(status) = unsafe { MinHook::enable_hook(remove_target) } {
        let _ = unsafe { MinHook::disable_hook(display_target) };
        let _ = unsafe { MinHook::remove_hook(remove_target) };
        let _ = unsafe { MinHook::remove_hook(display_target) };
        ORIGINAL_DISPLAY.store(0, Ordering::Release);
        ORIGINAL_REMOVE.store(0, Ordering::Release);
        return Err(format!("enable removeWindow hook returned {status:?}"));
    }
    DISPLAY_TARGET.store(display_target as usize, Ordering::Release);
    REMOVE_TARGET.store(remove_target as usize, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

unsafe extern "C" fn hook_display_window(
    base_ui: *mut c_void,
    window: *mut c_void,
    anchor: *mut c_void,
) {
    // Capture the header while `window` is still a live callback argument. The
    // original method may allocate and trigger a moving HashLink collection.
    let type_pointer = copy_type_pointer(window);
    let original = ORIGINAL_DISPLAY.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: MinHook returned this trampoline for the signature-validated
    // `(ui.BaseUI, ui.win.BaseWindow, ui.UIElement) -> Void` target.
    let original: HlDisplayWindow = unsafe { std::mem::transmute(original) };
    unsafe { original(base_ui, window, anchor) };

    if ACTIVE.load(Ordering::Relaxed) {
        queue_raw_edge_with_type(RawWindowEdgeKind::Opened, window, type_pointer);
    }
}

unsafe extern "C" fn hook_remove_window(base_ui: *mut c_void, window: *mut c_void) {
    let type_pointer = copy_type_pointer(window);
    let original = ORIGINAL_REMOVE.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: MinHook returned this trampoline for the signature-validated
    // `(ui.BaseUI, ui.win.BaseWindow) -> Void` target.
    let original: HlRemoveWindow = unsafe { std::mem::transmute(original) };
    unsafe { original(base_ui, window) };

    if ACTIVE.load(Ordering::Relaxed) {
        queue_raw_edge_with_type(RawWindowEdgeKind::Closed, window, type_pointer);
    }
}

fn queue_raw_edge_with_type(kind: RawWindowEdgeKind, window: *mut c_void, type_pointer: usize) {
    let window = window as usize;
    if window < 0x1_0000 || type_pointer < 0x1_0000 {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let raw = RawWindowEdge {
        kind,
        window,
        type_pointer,
    };
    if RAW_EDGES.get().is_none_or(|queue| queue.push(raw).is_err()) {
        RAW_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

fn copy_type_pointer(window: *mut c_void) -> usize {
    if window.is_null() {
        return 0;
    }
    // SAFETY: the signature-validated HashLink method supplies a live
    // BaseWindow object argument. Copying its first pointer-sized type word is
    // fixed-size work and does not traverse metadata inside the hook.
    unsafe { std::ptr::read_unaligned(window.cast::<usize>()) }
}

fn publish_edge(edge: WindowHookEdge) -> bool {
    if EDGES.get().is_none_or(|queue| queue.push(edge).is_err()) {
        EDGE_DROPS.fetch_add(1, Ordering::Relaxed);
        false
    } else {
        true
    }
}

pub(crate) fn drain_edges() -> Vec<WindowHookEdge> {
    let mut edges = Vec::new();
    if let Some(queue) = EDGES.get() {
        while edges.len() < farever_more_api::MAX_EVENT_BATCH {
            let Some(edge) = queue.pop() else {
                break;
            };
            edges.push(edge);
        }
    }
    edges
}

pub(crate) fn status() -> usize {
    HOOK_STATUS.load(Ordering::Acquire)
}

pub(crate) fn error() -> Option<&'static str> {
    HOOK_ERROR.get().map(String::as_str)
}

pub(crate) fn geometry_error() -> Option<&'static str> {
    GEOMETRY_ERROR.get().map(String::as_str)
}

pub(crate) fn total_drops() -> u64 {
    RAW_DROPS
        .load(Ordering::Acquire)
        .saturating_add(EDGE_DROPS.load(Ordering::Acquire))
}

pub(crate) fn metrics() -> String {
    format!(
        "ui_window_hooks={} ui_window_opened={} ui_window_closed={} ui_window_duplicates={} ui_window_unmatched_closes={} ui_window_relocations={} ui_window_invalid={} ui_window_resyncs={} ui_window_focus_changes={} ui_window_focus_invalidations={} ui_window_geometry_captured={} ui_window_geometry_invalid={} ui_window_raw_drops={} ui_window_edge_drops={}",
        status_name(status()),
        OPENED.load(Ordering::Relaxed),
        CLOSED.load(Ordering::Relaxed),
        DUPLICATES.load(Ordering::Relaxed),
        UNMATCHED_CLOSES.load(Ordering::Relaxed),
        RELOCATIONS.load(Ordering::Relaxed),
        INVALID.load(Ordering::Relaxed),
        RESYNCS.load(Ordering::Relaxed),
        FOCUS_CHANGES.load(Ordering::Relaxed),
        FOCUS_INVALIDATIONS.load(Ordering::Relaxed),
        GEOMETRY_CAPTURED.load(Ordering::Relaxed),
        GEOMETRY_INVALID.load(Ordering::Relaxed),
        RAW_DROPS.load(Ordering::Relaxed),
        EDGE_DROPS.load(Ordering::Relaxed),
    )
}

pub(crate) fn status_name(status: usize) -> &'static str {
    match status {
        0 => "waiting",
        1 => "active",
        3 => "failed",
        4 => "installing",
        _ => "unknown",
    }
}

pub(crate) fn shutdown_hooks() {
    ACTIVE.store(false, Ordering::Release);
    for target in [
        DISPLAY_TARGET.load(Ordering::Acquire),
        REMOVE_TARGET.load(Ordering::Acquire),
    ] {
        if target != 0 {
            // SAFETY: targets are published only after build-gated,
            // signature-validated hooks were created and enabled.
            let _ = unsafe { MinHook::disable_hook(target as *mut c_void) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_specs_match_the_verified_window_boundaries() {
        assert_eq!(GAME_UI_DISPLAY_WINDOW.lookup_type, "ui.GameUI");
        assert_eq!(GAME_UI_DISPLAY_WINDOW.name, c"displayWindow");
        assert_eq!(GAME_UI_DISPLAY_WINDOW.arguments, DISPLAY_WINDOW_ARGUMENTS);
        assert_eq!(GAME_UI_REMOVE_WINDOW.name, c"removeWindow");
        assert_eq!(GAME_UI_REMOVE_WINDOW.arguments, REMOVE_WINDOW_ARGUMENTS);
    }

    #[test]
    fn decoder_deduplicates_identity_edges() {
        let mut decoder = WindowHookDecoder::default();
        let opened = RawWindowEdge {
            kind: RawWindowEdgeKind::Opened,
            window: 0x1000_0000,
            type_pointer: 0x2000_0000,
        };
        assert_eq!(
            decoder.apply(opened, Some("ui.win.GameMenu".to_owned())),
            Some(WindowHookEdge {
                opened: true,
                window_pointer: 0x1000_0000,
                runtime_type: "ui.win.GameMenu".to_owned(),
                geometry: None,
                geometry_refresh: false,
            })
        );
        assert_eq!(decoder.focus_order, [0x1000_0000]);
        assert_eq!(
            decoder.apply(opened, Some("ui.win.GameMenu".to_owned())),
            None
        );

        let closed = RawWindowEdge {
            kind: RawWindowEdgeKind::Closed,
            ..opened
        };
        assert_eq!(
            decoder.apply(closed, None),
            Some(WindowHookEdge {
                opened: false,
                window_pointer: 0x1000_0000,
                runtime_type: "ui.win.GameMenu".to_owned(),
                geometry: None,
                geometry_refresh: false,
            })
        );
        assert_eq!(decoder.apply(closed, None), None);
        assert!(decoder.focus_order.is_empty());
    }

    #[test]
    fn decoder_reconciles_a_unique_moved_window_by_runtime_type() {
        let mut decoder = WindowHookDecoder::default();
        let opened = RawWindowEdge {
            kind: RawWindowEdgeKind::Opened,
            window: 0x1000_0000,
            type_pointer: 0x2000_0000,
        };
        assert!(decoder
            .apply(opened, Some("ui.win.GameMenu".to_owned()))
            .is_some());

        let moved_close = RawWindowEdge {
            kind: RawWindowEdgeKind::Closed,
            window: 0x1000_1000,
            type_pointer: 0x2000_0000,
        };
        assert_eq!(
            decoder.apply(moved_close, Some("ui.win.GameMenu".to_owned())),
            Some(WindowHookEdge {
                opened: false,
                window_pointer: 0x1000_1000,
                runtime_type: "ui.win.GameMenu".to_owned(),
                geometry: None,
                geometry_refresh: false,
            })
        );
        assert!(decoder.active.is_empty());
    }

    #[test]
    fn decoder_seed_makes_a_preexisting_window_close_observable() {
        let mut decoder = WindowHookDecoder::default();
        decoder.reconcile(vec![WindowSnapshotEntry {
            window_pointer: 0x1000_0000,
            runtime_type: "ui.win.GameMenu".to_owned(),
        }]);

        let closed = RawWindowEdge {
            kind: RawWindowEdgeKind::Closed,
            window: 0x1000_0000,
            type_pointer: 0x2000_0000,
        };
        assert_eq!(
            decoder.apply(closed, None),
            Some(WindowHookEdge {
                opened: false,
                window_pointer: 0x1000_0000,
                runtime_type: "ui.win.GameMenu".to_owned(),
                geometry: None,
                geometry_refresh: false,
            })
        );
        assert!(decoder.focus_order.is_empty());
    }

    #[test]
    fn decoder_tracks_frontmost_registry_order() {
        let mut decoder = WindowHookDecoder::default();
        decoder.reconcile(vec![
            WindowSnapshotEntry {
                window_pointer: 0x1000_2000,
                runtime_type: "ui.win.Inventory".to_owned(),
            },
            WindowSnapshotEntry {
                window_pointer: 0x1000_1000,
                runtime_type: "ui.win.GameMenu".to_owned(),
            },
        ]);
        assert_eq!(decoder.focus_order, [0x1000_2000, 0x1000_1000]);

        let opened = RawWindowEdge {
            kind: RawWindowEdgeKind::Opened,
            window: 0x1000_3000,
            type_pointer: 0x2000_0000,
        };
        assert!(decoder
            .apply(opened, Some("ui.win.CharacterUI".to_owned()))
            .is_some());
        assert_eq!(decoder.focus_order[0], 0x1000_3000);
    }

    #[test]
    fn decoder_finds_a_reconciled_escape_menu_for_geometry_refresh() {
        let mut decoder = WindowHookDecoder::default();
        decoder.reconcile(vec![
            WindowSnapshotEntry {
                window_pointer: 0x1000_2000,
                runtime_type: "ui.win.InventoryUI".to_owned(),
            },
            WindowSnapshotEntry {
                window_pointer: 0x1000_1000,
                runtime_type: ESCAPE_MENU_TYPE.to_owned(),
            },
        ]);

        assert_eq!(decoder.active_escape_menu_pointer(), Some(0x1000_1000));

        decoder.reconcile(Vec::new());
        assert_eq!(decoder.active_escape_menu_pointer(), None);
    }

    #[test]
    fn geometry_plausibility_rejects_unusable_menu_bounds() {
        let runtime = RuntimeWindowGeometry {
            x: 150.0,
            y: 219.0,
            width: 396.0,
            height: 573.0,
            matrix_a: 1.0,
            matrix_b: 0.0,
            matrix_c: 0.0,
            matrix_d: 1.0,
        };
        let scene = SceneTransform {
            scale_x: 1.25,
            scale_y: 1.25,
            offset_x: 0.0,
            offset_y: 0.0,
        };
        let geometry = project_window_geometry(runtime, scene);
        assert!(window_geometry_is_plausible(geometry));
        assert_eq!(geometry.x, 187.5);
        assert_eq!(geometry.y, 273.75);
        assert_eq!(geometry.width, 495.0);
        assert_eq!(geometry.height, 716.25);

        let mut invalid = geometry;
        invalid.x = f32::NAN;
        assert!(!window_geometry_is_plausible(invalid));
        invalid = geometry;
        invalid.runtime_width = 0.0;
        assert!(!window_geometry_is_plausible(invalid));
        invalid = geometry;
        invalid.viewport_scale_x = 0.0;
        assert!(!window_geometry_is_plausible(invalid));
    }
}
