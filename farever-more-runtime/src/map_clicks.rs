//! Full-map clicks copied from build-verified native callbacks. Game-thread
//! hooks forward once and enqueue coordinates; bus delivery stays on the host.

use crate::game_build::GameBuildProfile;
use crate::hashlink::{
    object_has_exact_type, validate_object, HashLink, HashLinkFieldSpec, HashLinkKind,
    HashLinkMethodSpec, HashLinkObjectSpec, HashLinkRuntime, HashLinkTypeSpec,
    ValidatedHashLinkMethod,
};
use crossbeam_queue::ArrayQueue;
use minhook::MinHook;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

pub(crate) const MAP_CLICK_TOPIC: &str = "farever.map-click@1";
const MAP_WINDOW: &str = "ui.win.MapWindow";
const QUEUE_CAPACITY: usize = 64;
const WORLD_CLICK: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: MAP_WINDOW,
    name: c"onClickWorld",
    arguments: &[
        HashLinkTypeSpec::Object(MAP_WINDOW),
        HashLinkTypeSpec::Kind(HashLinkKind::F64),
        HashLinkTypeSpec::Kind(HashLinkKind::F64),
    ],
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const ACTIVITY_CLICK: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: MAP_WINDOW,
    name: c"popupActivityMenu",
    arguments: &[
        HashLinkTypeSpec::Object(MAP_WINDOW),
        HashLinkTypeSpec::Object("ui.win.map.ActivityMarker"),
    ],
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const ACTIVITY_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "ui.win.map.ActivityMarker",
    kind: HashLinkKind::Object,
    fields: &[HashLinkFieldSpec::object("worldPos", "h3d.VectorImpl")],
};
const POSITION_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "h3d.VectorImpl",
    kind: HashLinkKind::Object,
    fields: &[
        HashLinkFieldSpec::scalar("x", HashLinkKind::F64),
        HashLinkFieldSpec::scalar("y", HashLinkKind::F64),
    ],
};

static MAP_TYPE: AtomicUsize = AtomicUsize::new(0);
static CAPTURING: AtomicBool = AtomicBool::new(true);
static WORLD_HOOK: Hook = Hook::new();
static ACTIVITY_HOOK: Hook = Hook::new();
static ACTIVITY_LAYOUT: OnceLock<ActivityLayout> = OnceLock::new();
static CLICKS: OnceLock<ArrayQueue<MapClick>> = OnceLock::new();
static QUEUE_DROPS: AtomicU64 = AtomicU64::new(0);
static INVALID_COPIES: AtomicU64 = AtomicU64::new(0);

struct Hook {
    status: AtomicUsize,
    target: AtomicUsize,
    original: AtomicUsize,
    error: OnceLock<String>,
}

impl Hook {
    const fn new() -> Self {
        Self {
            status: AtomicUsize::new(0),
            target: AtomicUsize::new(0),
            original: AtomicUsize::new(0),
            error: OnceLock::new(),
        }
    }

    fn install(
        &self,
        resolve: impl FnOnce() -> Result<ValidatedHashLinkMethod, String>,
        detour: *mut c_void,
    ) {
        if self
            .status
            .compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let result = resolve().and_then(|method| {
            let target = method.target() as *mut c_void;
            // SAFETY: resolution checks the exact supported build's metadata,
            // full signature, and executable target before reaching MinHook.
            let original =
                std::panic::catch_unwind(|| unsafe { MinHook::create_hook(target, detour) })
                    .map_err(|_| "MinHook initialization panicked".to_owned())?
                    .map_err(|status| format!("create map click hook returned {status:?}"))?;
            self.original.store(original as usize, Ordering::Release);
            // SAFETY: the hook and trampoline were created immediately above.
            if let Err(status) = unsafe { MinHook::enable_hook(target) } {
                // SAFETY: the hook exists but was not successfully enabled.
                let _ = unsafe { MinHook::remove_hook(target) };
                self.original.store(0, Ordering::Release);
                return Err(format!("enable map click hook returned {status:?}"));
            }
            self.target.store(target as usize, Ordering::Release);
            Ok(())
        });
        match result {
            Ok(()) => self.status.store(1, Ordering::Release),
            Err(error) => {
                let _ = self.error.set(error);
                self.status.store(3, Ordering::Release);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MapClick {
    x: f32,
    y: f32,
}

impl MapClick {
    fn new(x: f64, y: f64) -> Option<Self> {
        let click = Self {
            x: x as f32,
            y: y as f32,
        };
        (click.x.is_finite() && click.y.is_finite()).then_some(click)
    }

    pub(crate) fn payload(self) -> Vec<u8> {
        format!("{} {}", self.x, self.y).into_bytes()
    }
}

#[derive(Clone, Copy)]
struct ActivityLayout {
    marker_type: usize,
    position_type: usize,
    world_position: usize,
    x: usize,
    y: usize,
}

pub(crate) struct MapClickCapture {
    statuses: [usize; 2],
    drops: u64,
    invalid: u64,
    diagnostics: Vec<String>,
}

impl MapClickCapture {
    pub(crate) fn new() -> Self {
        prepare_queue();
        Self {
            statuses: [usize::MAX; 2],
            drops: 0,
            invalid: 0,
            diagnostics: Vec::new(),
        }
    }

    pub(crate) fn drain(&mut self) -> Vec<MapClick> {
        let mut clicks = Vec::new();
        if let Some(queue) = CLICKS.get() {
            for _ in 0..QUEUE_CAPACITY {
                let Some(click) = queue.pop() else { break };
                if CAPTURING.load(Ordering::Acquire) {
                    clicks.push(click);
                }
            }
        }
        clicks
    }

    pub(crate) fn take_diagnostics(&mut self) -> Vec<String> {
        for (index, (name, hook)) in [("world", &WORLD_HOOK), ("activity", &ACTIVITY_HOOK)]
            .into_iter()
            .enumerate()
        {
            let status = hook.status.load(Ordering::Acquire);
            if self.statuses[index] != status {
                self.statuses[index] = status;
                self.diagnostics.push(format!(
                    "map click capture provider={name} state={}{}",
                    match status {
                        0 => "waiting-for-map",
                        1 => "active",
                        2 => "unavailable-on-stable",
                        3 => "failed",
                        _ => "installing",
                    },
                    hook.error
                        .get()
                        .map_or_else(String::new, |error| format!(" error={error}"))
                ));
            }
        }
        for (name, total, previous) in [
            (
                "queue-dropped",
                QUEUE_DROPS.load(Ordering::Acquire),
                &mut self.drops,
            ),
            (
                "copies-rejected",
                INVALID_COPIES.load(Ordering::Acquire),
                &mut self.invalid,
            ),
        ] {
            if total != *previous {
                self.diagnostics.push(format!(
                    "map click {name}={} total={total}",
                    total.saturating_sub(*previous)
                ));
                *previous = total;
            }
        }
        std::mem::take(&mut self.diagnostics)
    }
}

pub(crate) fn prepare_queue() {
    let _ = CLICKS.get_or_init(|| ArrayQueue::new(QUEUE_CAPACITY));
}

pub(crate) fn observe_map_type(type_pointer: usize) {
    if type_pointer >= 0x1_0000 {
        MAP_TYPE.store(type_pointer, Ordering::Release);
    }
}

pub(crate) fn try_install_hooks(hl: &HashLink<'_>, build: GameBuildProfile) {
    let map_type = MAP_TYPE.load(Ordering::Acquire);
    if map_type == 0 {
        return;
    }
    WORLD_HOOK.install(
        || {
            HashLinkRuntime::loaded()
                .ok_or_else(|| "libhl.dll is not loaded".to_owned())?
                .resolve_bound_method(hl, map_type, &WORLD_CLICK)
        },
        hook_world_click as *mut c_void,
    );
    if !build.uses_beta_abi() {
        ACTIVITY_HOOK.status.store(2, Ordering::Release);
        return;
    }
    ACTIVITY_HOOK.install(
        || {
            let method = HashLinkRuntime::loaded()
                .ok_or_else(|| "libhl.dll is not loaded".to_owned())?
                .resolve_method(hl, map_type, &ACTIVITY_CLICK)?;
            let marker_type = method
                .argument_type(1)
                .ok_or_else(|| "missing activity marker argument".to_owned())?;
            let marker = validate_object(hl, marker_type, &ACTIVITY_SCHEMA)?;
            let position = validate_object(
                hl,
                marker
                    .field_type_address("worldPos")
                    .ok_or_else(|| "missing worldPos type".to_owned())?,
                &POSITION_SCHEMA,
            )?;
            let _ = ACTIVITY_LAYOUT.set(ActivityLayout {
                marker_type,
                position_type: position.type_address,
                world_position: marker
                    .offset("worldPos")
                    .ok_or_else(|| "missing worldPos offset".to_owned())?,
                x: position
                    .offset("x")
                    .ok_or_else(|| "missing position x".to_owned())?,
                y: position
                    .offset("y")
                    .ok_or_else(|| "missing position y".to_owned())?,
            });
            Ok(method)
        },
        hook_activity_click as *mut c_void,
    );
}

pub(crate) fn shutdown_hooks() {
    CAPTURING.store(false, Ordering::Release);
    for hook in [&WORLD_HOOK, &ACTIVITY_HOOK] {
        let target = hook.target.load(Ordering::Acquire);
        if target != 0 {
            // SAFETY: only enabled, signature-checked targets are published.
            let _ = unsafe { MinHook::disable_hook(target as *mut c_void) };
        }
    }
}

fn enqueue(click: Option<MapClick>) {
    if !CAPTURING.load(Ordering::Acquire) {
        return;
    }
    let Some(click) = click else {
        INVALID_COPIES.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if let Some(queue) = CLICKS.get() {
        if queue.push(click).is_err() {
            QUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

unsafe extern "C" fn hook_world_click(map: *mut c_void, x: f64, y: f64) {
    // SAFETY: `map` is a live callback argument; only its header is inspected.
    let click = unsafe { object_has_exact_type(map, MAP_TYPE.load(Ordering::Acquire)) }
        .then(|| MapClick::new(x, y))
        .flatten();
    let original = WORLD_HOOK.original.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: the binding's complete (MapWindow, F64, F64) -> Void ABI was
        // validated. This trampoline is forwarded exactly once.
        let original: unsafe extern "C" fn(*mut c_void, f64, f64) =
            unsafe { std::mem::transmute(original) };
        unsafe { original(map, x, y) };
    }
    enqueue(click);
}

unsafe extern "C" fn hook_activity_click(map: *mut c_void, marker: *mut c_void) {
    // Copy before forwarding: opening the native picker can allocate and GC.
    // SAFETY: both pointers are live arguments of the validated native method.
    let click = if unsafe { object_has_exact_type(map, MAP_TYPE.load(Ordering::Acquire)) } {
        ACTIVITY_LAYOUT
            .get()
            .and_then(|layout| unsafe { copy_activity_click(marker, *layout) })
    } else {
        None
    };
    let original = ACTIVITY_HOOK.original.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: the complete (MapWindow, ActivityMarker) -> Void ABI matches
        // the trampoline. Farever's picker behavior is preserved.
        let original: unsafe extern "C" fn(*mut c_void, *mut c_void) =
            unsafe { std::mem::transmute(original) };
        unsafe { original(map, marker) };
    }
    enqueue(click);
}

unsafe fn copy_activity_click(marker: *mut c_void, layout: ActivityLayout) -> Option<MapClick> {
    // SAFETY: the caller owns the live marker argument; offsets and field types
    // were validated on the worker before installing the hook.
    if !unsafe { object_has_exact_type(marker, layout.marker_type) } {
        return None;
    }
    let position = unsafe {
        std::ptr::read_unaligned(
            marker
                .cast::<u8>()
                .add(layout.world_position)
                .cast::<*mut c_void>(),
        )
    };
    if !unsafe { object_has_exact_type(position, layout.position_type) } {
        return None;
    }
    let base = position.cast::<u8>();
    let x = unsafe { std::ptr::read_unaligned(base.add(layout.x).cast::<f64>()) };
    let y = unsafe { std::ptr::read_unaligned(base.add(layout.y).cast::<f64>()) };
    MapClick::new(x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_coordinates_are_finite_and_use_the_gps_payload() {
        assert_eq!(MapClick::new(120.0, -45.0).unwrap().payload(), b"120 -45");
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX] {
            assert!(MapClick::new(invalid, 1.0).is_none());
            assert!(MapClick::new(1.0, invalid).is_none());
        }
    }

    #[test]
    fn activity_copy_checks_both_object_types_and_copies_world_coordinates() {
        #[repr(C)]
        struct Position {
            kind: usize,
            x: f64,
            y: f64,
        }
        #[repr(C)]
        struct Marker {
            kind: usize,
            position: *mut c_void,
        }
        let mut position = Position {
            kind: 0x1234_0000,
            x: 210.0,
            y: -34.0,
        };
        let mut marker = Marker {
            kind: 0x2345_0000,
            position: (&mut position as *mut Position).cast(),
        };
        let layout = ActivityLayout {
            marker_type: marker.kind,
            position_type: position.kind,
            world_position: std::mem::offset_of!(Marker, position),
            x: std::mem::offset_of!(Position, x),
            y: std::mem::offset_of!(Position, y),
        };
        // SAFETY: these test objects remain live and use the declared layouts.
        let copy = |marker: &mut Marker| unsafe {
            copy_activity_click((marker as *mut Marker).cast(), layout)
        };
        assert_eq!(copy(&mut marker), MapClick::new(210.0, -34.0));
        marker.kind = 0x3456_0000;
        assert!(copy(&mut marker).is_none());
        marker.kind = layout.marker_type;
        position.kind = 0x4567_0000;
        marker.position = (&mut position as *mut Position).cast();
        assert!(copy(&mut marker).is_none());
        marker.position = std::ptr::null_mut();
        assert!(copy(&mut marker).is_none());
    }
}
