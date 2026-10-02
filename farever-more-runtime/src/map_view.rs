//! Read-only full-map capture. Copy synchronized geometry on the game thread;
//! combine it with the game's own world/map scale on the runtime worker.

use crate::hashlink::{
    object_has_exact_type, validate_object, HashLink, HashLinkFieldSpec, HashLinkKind,
    HashLinkMethodSpec, HashLinkObjectSpec, HashLinkRuntime, HashLinkTypeSpec,
};
use crate::map_clicks::Hook;
use crossbeam_queue::ArrayQueue;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const MAP: &str = "ui.win.MapWindow";
const CAPACITY: usize = 64;
const MAX_AGE: Duration = Duration::from_millis(500);
// The runtime lookup retains h2d.Object's virtual declaration signature;
// resolution selects the implementation from MapWindow's concrete method table.
const SYNC: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: MAP,
    name: c"sync",
    arguments: &[
        HashLinkTypeSpec::Object("h2d.Object"),
        HashLinkTypeSpec::Object("h2d.RenderContext"),
    ],
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const SCALE: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: MAP,
    name: c"get_tileScale",
    arguments: &[HashLinkTypeSpec::Object(MAP)],
    result: HashLinkTypeSpec::Kind(HashLinkKind::F64),
};
const REMOVE: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: MAP,
    name: c"onRemove",
    arguments: &[HashLinkTypeSpec::Object("h2d.Object")],
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const TRANSFORM: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::scalar("matA", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("matB", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("matC", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("matD", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("absX", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("absY", HashLinkKind::F64),
];
const MAP_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: MAP,
    kind: HashLinkKind::Object,
    fields: &[
        HashLinkFieldSpec::scalar("visible", HashLinkKind::Bool),
        HashLinkFieldSpec::object("scroll", "h2d.Layers"),
        HashLinkFieldSpec::object("scrollContainer", "ui.BaseElement"),
    ],
};
const CONTAINER_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "ui.BaseElement",
    kind: HashLinkKind::Object,
    fields: &[
        TRANSFORM[0],
        TRANSFORM[1],
        TRANSFORM[2],
        TRANSFORM[3],
        TRANSFORM[4],
        TRANSFORM[5],
        HashLinkFieldSpec::scalar("calculatedWidth", HashLinkKind::F64),
        HashLinkFieldSpec::scalar("calculatedHeight", HashLinkKind::F64),
    ],
};
const CONTEXT_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "h2d.RenderContext",
    kind: HashLinkKind::Object,
    fields: &[HashLinkFieldSpec::object("scene", "h2d.Scene")],
};
const SCENE_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "h2d.Scene",
    kind: HashLinkKind::Object,
    fields: &[
        HashLinkFieldSpec::scalar("viewportScaleX", HashLinkKind::F64),
        HashLinkFieldSpec::scalar("viewportScaleY", HashLinkKind::F64),
        HashLinkFieldSpec::scalar("offsetX", HashLinkKind::F64),
        HashLinkFieldSpec::scalar("offsetY", HashLinkKind::F64),
    ],
};

static MAP_TYPE: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(true);
static SYNC_HOOK: Hook = Hook::new();
static SCALE_HOOK: Hook = Hook::new();
static REMOVE_HOOK: Hook = Hook::new();
static LAYOUT: OnceLock<Layout> = OnceLock::new();
static VIEWS: OnceLock<ArrayQueue<(RawView, Instant)>> = OnceLock::new();
static SCALES: OnceLock<ArrayQueue<(usize, f64)>> = OnceLock::new();
static INVALID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct GeometryLayout {
    kind: usize,
    transform: [usize; 6],
    width: usize,
    height: usize,
}

#[derive(Clone, Copy)]
struct Layout {
    map_type: usize,
    visible: usize,
    scroll: usize,
    container: usize,
    scroll_type: usize,
    scroll_transform: [usize; 6],
    container_layout: GeometryLayout,
    context_type: usize,
    scene: usize,
    scene_type: usize,
    viewport: [usize; 4],
}

#[derive(Clone, Copy, Debug)]
struct RawView {
    // Identity only. No native pointer is dereferenced on the worker.
    map: usize,
    geometry: Option<Geometry>,
}

#[derive(Clone, Copy, Debug)]
struct Geometry {
    scroll: [f64; 6],
    container: [f64; 6],
    size: [f64; 2],
    viewport: [f64; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MapView {
    /// [a, b, c, d, tx, ty], mapping world X/Y to client pixels.
    pub(crate) world_to_client: [f32; 6],
    /// [left, top, width, height] in client pixels.
    pub(crate) bounds: [f32; 4],
}

impl Geometry {
    fn project(self, scale: f64) -> Option<MapView> {
        let [sx, sy, ox, oy] = self.viewport;
        if !scale.is_finite() || scale <= 0.0 || sx <= 0.0 || sy <= 0.0 {
            return None;
        }
        let [a, b, c, d, x, y] = self.scroll;
        let transform = [
            a * scale * sx,
            b * scale * sy,
            c * scale * sx,
            d * scale * sy,
            x * sx + ox,
            y * sy + oy,
        ]
        .map(|v| v as f32);
        let [a, b, c, d, x, y] = self.container;
        // Farever's map container is axis-aligned. Reject unsupported rotation
        // instead of allowing the overlay to escape the native clipping area.
        if b.abs() > 1e-6 || c.abs() > 1e-6 || a <= 0.0 || d <= 0.0 {
            return None;
        }
        let bounds = [
            x * sx + ox,
            y * sy + oy,
            self.size[0] * a * sx,
            self.size[1] * d * sy,
        ]
        .map(|v| v as f32);
        let view = MapView {
            world_to_client: transform,
            bounds,
        };
        (transform.iter().chain(bounds.iter()).all(|v| v.is_finite())
            && (1.0..=32_768.0).contains(&bounds[2])
            && (1.0..=32_768.0).contains(&bounds[3])
            && (transform[0] * transform[3] - transform[1] * transform[2]).abs() > 1e-8)
            .then_some(view)
    }
}

pub(crate) struct MapViewCapture {
    scale: Option<(usize, f64)>,
    latest: Option<(RawView, Instant)>,
    statuses: [usize; 3],
    invalid: u64,
}

impl MapViewCapture {
    pub(crate) fn new() -> Self {
        prepare_queues();
        Self {
            scale: None,
            latest: None,
            statuses: [usize::MAX; 3],
            invalid: 0,
        }
    }

    pub(crate) fn refresh(&mut self, in_world: bool) -> Option<MapView> {
        if let Some(queue) = SCALES.get() {
            for _ in 0..CAPACITY {
                let Some(scale) = queue.pop() else { break };
                self.scale = Some(scale);
            }
        }
        if let Some(queue) = VIEWS.get() {
            for _ in 0..CAPACITY {
                let Some((view, captured)) = queue.pop() else {
                    break;
                };
                if view.geometry.is_some()
                    || self.latest.is_some_and(|(old, _)| old.map == view.map)
                {
                    self.latest = Some((view, captured));
                }
            }
        }
        if !in_world || !ACTIVE.load(Ordering::Acquire) {
            self.scale = None;
            self.latest = None;
            return None;
        }
        if [&SYNC_HOOK, &SCALE_HOOK, &REMOVE_HOOK]
            .iter()
            .any(|hook| hook.status.load(Ordering::Acquire) != 1)
        {
            return None;
        }
        self.current_view(Instant::now())
    }

    pub(crate) fn unavailable_reason(&self) -> farever_more_api::UnavailableReason {
        if [&SYNC_HOOK, &SCALE_HOOK, &REMOVE_HOOK]
            .iter()
            .any(|hook| hook.status.load(Ordering::Acquire) == 3)
        {
            farever_more_api::UnavailableReason::ProviderFailed
        } else {
            farever_more_api::UnavailableReason::NotYetObserved
        }
    }

    fn current_view(&self, now: Instant) -> Option<MapView> {
        let (raw, captured) = self.latest?;
        let (map, scale) = self.scale?;
        if raw.map != map || now.saturating_duration_since(captured) > MAX_AGE {
            return None;
        }
        raw.geometry?.project(scale)
    }

    pub(crate) fn take_diagnostics(&mut self) -> Vec<String> {
        let mut messages = Vec::new();
        for (index, (name, hook)) in [
            ("sync", &SYNC_HOOK),
            ("scale", &SCALE_HOOK),
            ("remove", &REMOVE_HOOK),
        ]
        .into_iter()
        .enumerate()
        {
            let status = hook.status.load(Ordering::Acquire);
            if self.statuses[index] != status {
                self.statuses[index] = status;
                messages.push(format!(
                    "map viewport capture provider={name} state={}{}",
                    match status {
                        0 => "waiting-for-map",
                        1 => "active",
                        3 => "failed",
                        _ => "installing",
                    },
                    hook.error
                        .get()
                        .map(|s| format!(" reason={s}"))
                        .unwrap_or_default()
                ));
            }
        }
        let invalid = INVALID.load(Ordering::Relaxed);
        if invalid != self.invalid {
            messages.push(format!(
                "map viewport copies-rejected={} total={invalid}",
                invalid.saturating_sub(self.invalid)
            ));
            self.invalid = invalid;
        }
        messages
    }
}

pub(crate) fn prepare_queues() {
    VIEWS.get_or_init(|| ArrayQueue::new(CAPACITY));
    SCALES.get_or_init(|| ArrayQueue::new(CAPACITY));
}

pub(crate) fn observe_map_type(kind: usize) {
    if kind >= 0x1_0000 {
        MAP_TYPE.store(kind, Ordering::Release);
    }
}

pub(crate) fn try_install_hooks(hl: &HashLink<'_>) {
    let kind = MAP_TYPE.load(Ordering::Acquire);
    if kind == 0 {
        return;
    }
    SYNC_HOOK.install(
        || {
            let runtime = HashLinkRuntime::loaded().ok_or("libhl.dll is not loaded")?;
            let method = runtime.resolve_method(hl, kind, &SYNC)?;
            let layout = resolve_layout(
                hl,
                kind,
                method.argument_type(1).ok_or("missing render context")?,
            )?;
            let _ = LAYOUT.set(layout);
            Ok(method)
        },
        hook_sync as *mut c_void,
    );
    SCALE_HOOK.install(
        || {
            HashLinkRuntime::loaded()
                .ok_or("libhl.dll is not loaded")?
                .resolve_method(hl, kind, &SCALE)
        },
        hook_scale as *mut c_void,
    );
    REMOVE_HOOK.install(
        || {
            HashLinkRuntime::loaded()
                .ok_or("libhl.dll is not loaded")?
                .resolve_method(hl, kind, &REMOVE)
        },
        hook_remove as *mut c_void,
    );
}

fn resolve_layout(
    hl: &HashLink<'_>,
    map_type: usize,
    context_type: usize,
) -> Result<Layout, String> {
    let map = validate_object(hl, map_type, &MAP_SCHEMA)?;
    let field_type = |name| {
        map.field_type_address(name)
            .ok_or_else(|| format!("missing MapWindow.{name} type"))
    };
    let scroll = validate_object(
        hl,
        field_type("scroll")?,
        &HashLinkObjectSpec {
            name: "h2d.Layers",
            kind: HashLinkKind::Object,
            fields: TRANSFORM,
        },
    )?;
    let container = validate_object(hl, field_type("scrollContainer")?, &CONTAINER_SCHEMA)?;
    let context = validate_object(hl, context_type, &CONTEXT_SCHEMA)?;
    let scene = validate_object(
        hl,
        context
            .field_type_address("scene")
            .ok_or("missing scene type")?,
        &SCENE_SCHEMA,
    )?;
    let offsets = |obj: &crate::hashlink::ValidatedHashLinkObject| -> Result<[usize; 6], String> {
        let mut result = [0; 6];
        for (i, field) in ["matA", "matB", "matC", "matD", "absX", "absY"]
            .iter()
            .enumerate()
        {
            result[i] = obj
                .offset(field)
                .ok_or_else(|| format!("missing transform {field}"))?;
        }
        Ok(result)
    };
    Ok(Layout {
        map_type,
        visible: map.offset("visible").ok_or("missing visible")?,
        scroll: map.offset("scroll").ok_or("missing scroll")?,
        container: map
            .offset("scrollContainer")
            .ok_or("missing scrollContainer")?,
        scroll_type: scroll.type_address,
        scroll_transform: offsets(&scroll)?,
        container_layout: GeometryLayout {
            kind: container.type_address,
            transform: offsets(&container)?,
            width: container.offset("calculatedWidth").ok_or("missing width")?,
            height: container
                .offset("calculatedHeight")
                .ok_or("missing height")?,
        },
        context_type,
        scene: context.offset("scene").ok_or("missing scene")?,
        scene_type: scene.type_address,
        viewport: [
            scene.offset("viewportScaleX").ok_or("missing scale X")?,
            scene.offset("viewportScaleY").ok_or("missing scale Y")?,
            scene.offset("offsetX").ok_or("missing offset X")?,
            scene.offset("offsetY").ok_or("missing offset Y")?,
        ],
    })
}

unsafe fn pointer(object: *mut c_void, offset: usize) -> *mut c_void {
    unsafe { std::ptr::read_unaligned(object.cast::<u8>().add(offset).cast()) }
}
unsafe fn number(object: *mut c_void, offset: usize) -> f64 {
    unsafe { std::ptr::read_unaligned(object.cast::<u8>().add(offset).cast()) }
}
unsafe fn copy_geometry(
    map: *mut c_void,
    context: *mut c_void,
    layout: Layout,
) -> Option<Geometry> {
    // SAFETY: the callback arguments root this object graph. Every referenced
    // object is type-checked before reading offsets validated on the worker.
    unsafe {
        if !object_has_exact_type(map, layout.map_type)
            || !object_has_exact_type(context, layout.context_type)
            || std::ptr::read_unaligned(map.cast::<u8>().add(layout.visible)) == 0u8
        {
            return None;
        }
        let scroll = pointer(map, layout.scroll);
        let container = pointer(map, layout.container);
        let scene = pointer(context, layout.scene);
        if !object_has_exact_type(scroll, layout.scroll_type)
            || !object_has_exact_type(container, layout.container_layout.kind)
            || !object_has_exact_type(scene, layout.scene_type)
        {
            return None;
        }
        Some(Geometry {
            scroll: layout.scroll_transform.map(|offset| number(scroll, offset)),
            container: layout
                .container_layout
                .transform
                .map(|offset| number(container, offset)),
            size: [
                number(container, layout.container_layout.width),
                number(container, layout.container_layout.height),
            ],
            viewport: layout.viewport.map(|offset| number(scene, offset)),
        })
    }
}

unsafe extern "C" fn hook_sync(map: *mut c_void, context: *mut c_void) {
    let original = SYNC_HOOK.original.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: validated (Object, RenderContext) -> Void, forwarded once.
        let call: unsafe extern "C" fn(*mut c_void, *mut c_void) =
            unsafe { std::mem::transmute(original) };
        unsafe { call(map, context) };
    }
    // The inherited callback is shared by other UI elements. Only MapWindow
    // supplies a snapshot, after sync has updated the descendants' transforms.
    if ACTIVE.load(Ordering::Acquire)
        && unsafe { object_has_exact_type(map, MAP_TYPE.load(Ordering::Acquire)) }
    {
        let geometry = LAYOUT
            .get()
            .and_then(|layout| unsafe { copy_geometry(map, context, *layout) });
        if geometry.is_none() {
            INVALID.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(queue) = VIEWS.get() {
            queue.force_push((
                RawView {
                    map: map as usize,
                    geometry,
                },
                Instant::now(),
            ));
        }
    }
}

unsafe extern "C" fn hook_scale(map: *mut c_void) -> f64 {
    let original = SCALE_HOOK.original.load(Ordering::Acquire);
    let result = if original != 0 {
        // SAFETY: validated (MapWindow) -> F64, forwarded once.
        let call: unsafe extern "C" fn(*mut c_void) -> f64 =
            unsafe { std::mem::transmute(original) };
        unsafe { call(map) }
    } else {
        f64::NAN
    };
    if ACTIVE.load(Ordering::Acquire)
        && result.is_finite()
        && result > 0.0
        && unsafe { object_has_exact_type(map, MAP_TYPE.load(Ordering::Acquire)) }
    {
        // The getter runs once per marker. Keep the newest bounded scalar
        // samples separately so they cannot crowd out synchronized geometry.
        if let Some(queue) = SCALES.get() {
            queue.force_push((map as usize, result));
        }
    }
    result
}

unsafe extern "C" fn hook_remove(map: *mut c_void) {
    let original = REMOVE_HOOK.original.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: validated (Object) -> Void, forwarded once.
        let call: unsafe extern "C" fn(*mut c_void) = unsafe { std::mem::transmute(original) };
        unsafe { call(map) };
    }
    if ACTIVE.load(Ordering::Acquire)
        && unsafe { object_has_exact_type(map, MAP_TYPE.load(Ordering::Acquire)) }
    {
        if let Some(queue) = VIEWS.get() {
            queue.force_push((
                RawView {
                    map: map as usize,
                    geometry: None,
                },
                Instant::now(),
            ));
        }
    }
}

pub(crate) fn shutdown_hooks() {
    ACTIVE.store(false, Ordering::Release);
    for hook in [&SYNC_HOOK, &SCALE_HOOK, &REMOVE_HOOK] {
        let target = hook.target.load(Ordering::Acquire);
        if target != 0 {
            // SAFETY: only successfully installed hook targets are published.
            let _ = unsafe { minhook::MinHook::disable_hook(target as *mut c_void) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> Geometry {
        Geometry {
            scroll: [2.0, 0.0, 0.0, 2.0, -50.0, 80.0],
            container: [1.0, 0.0, 0.0, 1.0, 100.0, 200.0],
            size: [800.0, 600.0],
            viewport: [1.5, 1.5, 10.0, 20.0],
        }
    }

    #[test]
    fn projection_combines_game_scale_pan_zoom_and_scene_viewport() {
        let view = geometry().project(0.5).unwrap();
        assert_eq!(view.world_to_client, [1.5, 0.0, 0.0, 1.5, -65.0, 140.0]);
        assert_eq!(view.bounds, [160.0, 320.0, 1200.0, 900.0]);
    }

    #[test]
    fn projection_rejects_nonfinite_singular_or_unsupported_clipping() {
        for invalid in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            assert!(geometry().project(invalid).is_none());
        }
        let mut g = geometry();
        g.container[1] = 0.1;
        assert!(g.project(1.0).is_none());
        let mut g = geometry();
        g.scroll[0] = 0.0;
        assert!(g.project(1.0).is_none());
        let mut g = geometry();
        g.size[0] = f64::NAN;
        assert!(g.project(1.0).is_none());
    }

    #[test]
    fn snapshot_requires_matching_map_identity_and_expires() {
        let now = Instant::now();
        let mut capture = MapViewCapture::new();
        capture.latest = Some((
            RawView {
                map: 1,
                geometry: Some(geometry()),
            },
            now,
        ));
        capture.scale = Some((2, 0.5));
        assert!(capture.current_view(now).is_none());
        capture.scale = Some((1, 0.5));
        assert!(capture.current_view(now).is_some());
        assert!(capture
            .current_view(now + MAX_AGE + Duration::from_millis(1))
            .is_none());
        capture.latest = Some((
            RawView {
                map: 1,
                geometry: None,
            },
            now,
        ));
        assert!(capture.current_view(now).is_none());
        assert!(capture.refresh(false).is_none());
        assert!(capture.latest.is_none());
        assert!(capture.scale.is_none());
    }

    #[test]
    fn geometry_copy_requires_every_rooted_object_to_match_its_layout() {
        #[repr(C)]
        struct Object {
            kind: usize,
            transform: [f64; 6],
            width: f64,
            height: f64,
        }
        #[repr(C)]
        struct Scene {
            kind: usize,
            viewport: [f64; 4],
        }
        #[repr(C)]
        struct Context {
            kind: usize,
            scene: *mut c_void,
        }
        #[repr(C)]
        struct Map {
            kind: usize,
            scroll: *mut c_void,
            container: *mut c_void,
            visible: u8,
        }
        let g = geometry();
        let mut scroll = Object {
            kind: 0x110000,
            transform: g.scroll,
            width: 0.0,
            height: 0.0,
        };
        let mut container = Object {
            kind: 0x120000,
            transform: g.container,
            width: g.size[0],
            height: g.size[1],
        };
        let mut scene = Scene {
            kind: 0x130000,
            viewport: g.viewport,
        };
        let mut context = Context {
            kind: 0x140000,
            scene: (&mut scene as *mut Scene).cast(),
        };
        let mut map = Map {
            kind: 0x150000,
            scroll: (&mut scroll as *mut Object).cast(),
            container: (&mut container as *mut Object).cast(),
            visible: 1,
        };
        let transform = std::array::from_fn(|i| std::mem::offset_of!(Object, transform) + i * 8);
        let layout = Layout {
            map_type: map.kind,
            visible: std::mem::offset_of!(Map, visible),
            scroll: std::mem::offset_of!(Map, scroll),
            container: std::mem::offset_of!(Map, container),
            scroll_type: scroll.kind,
            scroll_transform: transform,
            container_layout: GeometryLayout {
                kind: container.kind,
                transform,
                width: std::mem::offset_of!(Object, width),
                height: std::mem::offset_of!(Object, height),
            },
            context_type: context.kind,
            scene: std::mem::offset_of!(Context, scene),
            scene_type: scene.kind,
            viewport: std::array::from_fn(|i| std::mem::offset_of!(Scene, viewport) + i * 8),
        };
        // SAFETY: all referenced fake objects are rooted on this test's stack.
        let copy = |map: &mut Map, context: &mut Context| unsafe {
            copy_geometry(
                (map as *mut Map).cast(),
                (context as *mut Context).cast(),
                layout,
            )
        };
        assert_eq!(
            copy(&mut map, &mut context).unwrap().project(0.5),
            g.project(0.5)
        );
        map.visible = 0;
        assert!(copy(&mut map, &mut context).is_none());
        map.visible = 1;
        scroll.kind = 0x160000;
        map.scroll = (&mut scroll as *mut Object).cast();
        assert!(copy(&mut map, &mut context).is_none());
        scroll.kind = layout.scroll_type;
        map.scroll = (&mut scroll as *mut Object).cast();
        scene.kind = 0x170000;
        context.scene = (&mut scene as *mut Scene).cast();
        assert!(copy(&mut map, &mut context).is_none());
        map.container = std::ptr::null_mut();
        assert!(copy(&mut map, &mut context).is_none());
    }
}
