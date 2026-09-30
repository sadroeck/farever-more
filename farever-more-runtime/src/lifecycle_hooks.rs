//! Build-gated GameApp loading, world, and zone lifecycle hooks.
//!
//! Callbacks only forward the original call, validate/copy bounded data, and
//! enqueue fixed-size records. String decoding, state publication, and event
//! construction remain on the observer worker.

use crate::game_build::GameBuildProfile;
use crate::hashlink::{
    object_has_exact_type, HashLink, HashLinkKind, HashLinkMethodSpec, HashLinkRuntime,
    HashLinkTypeSpec, ValidatedHashLinkMethod,
};
use crossbeam_queue::ArrayQueue;
use minhook::MinHook;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

const RAW_QUEUE_CAPACITY: usize = 128;
const ZONE_QUEUE_CAPACITY: usize = 32;
const MAX_DECODE_PER_TICK: usize = 32;
const AREA_UTF16_CAPACITY: usize = 96;

const GAME_APP_ARGUMENTS: &[HashLinkTypeSpec] = &[HashLinkTypeSpec::Object("GameApp")];
const SET_LOADING_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("GameApp"),
    HashLinkTypeSpec::Kind(HashLinkKind::I32),
];
const LOAD_LEVEL_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("GameApp"),
    // Farever's String is an unnamed concrete HOBJ in this build. The exact
    // resolved type pointer is retained and checked by the callback.
    HashLinkTypeSpec::Kind(HashLinkKind::Object),
    HashLinkTypeSpec::Reference(HashLinkKind::Bool),
];

const SET_LOADING: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "GameApp",
    name: c"set_loadingState",
    arguments: SET_LOADING_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::I32),
};
const LOAD_LEVEL: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "GameApp",
    name: c"loadLevel",
    arguments: LOAD_LEVEL_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const FINISHED_LOADING: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "GameApp",
    name: c"finishedLoading",
    arguments: GAME_APP_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const DISPOSE: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "GameApp",
    name: c"dispose",
    arguments: GAME_APP_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
// In Beta the shared virtual slot is typed with hxd.App as its receiver.
const DISPOSE_BETA_ARGUMENTS: &[HashLinkTypeSpec] = &[HashLinkTypeSpec::Object("hxd.App")];
const DISPOSE_BETA: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "GameApp",
    name: c"dispose",
    arguments: DISPOSE_BETA_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};

type HlSetLoading = unsafe extern "C" fn(*mut c_void, i32) -> i32;
type HlLoadLevel = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);
type HlGameAppVoid = unsafe extern "C" fn(*mut c_void);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LifecycleSnapshot {
    pub(crate) app: usize,
    pub(crate) loading_state: i32,
    pub(crate) in_world: bool,
    pub(crate) area: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ZoneHookEdge {
    pub(crate) app: usize,
    pub(crate) area: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawLifecycleKind {
    Loading,
    LoadLevel,
    Ready,
    Lost,
}

#[derive(Clone, Copy, Debug)]
struct RawLifecycle {
    sequence: u64,
    kind: RawLifecycleKind,
    app: usize,
    loading_state: i32,
    area_len: u16,
    area: [u16; AREA_UTF16_CAPACITY],
}

impl RawLifecycle {
    fn edge(kind: RawLifecycleKind, app: usize) -> Self {
        Self {
            sequence: 0,
            kind,
            app,
            loading_state: -1,
            area_len: 0,
            area: [0; AREA_UTF16_CAPACITY],
        }
    }
}

#[derive(Clone, Debug)]
struct LifecycleReconciliation {
    through_sequence: u64,
    snapshot: LifecycleSnapshot,
}

#[derive(Clone, Copy, Debug)]
struct LifecycleLayout {
    game_app_type: usize,
    string_type: usize,
}

struct ResolvedHooks {
    set_loading: ValidatedHashLinkMethod,
    load_level: ValidatedHashLinkMethod,
    finished_loading: ValidatedHashLinkMethod,
    dispose: ValidatedHashLinkMethod,
}

#[derive(Default)]
pub(crate) struct LifecycleHookDecoder {
    staged_area: Option<(usize, String)>,
    observed_raw_drops: u64,
}

// 0 = waiting for GameApp type, 1 = active, 3 = failed, 4 = installing.
static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static GAME_APP_TYPE: AtomicUsize = AtomicUsize::new(0);
static LAYOUT: OnceLock<LifecycleLayout> = OnceLock::new();

static SET_LOADING_TARGET: AtomicUsize = AtomicUsize::new(0);
static LOAD_LEVEL_TARGET: AtomicUsize = AtomicUsize::new(0);
static FINISHED_LOADING_TARGET: AtomicUsize = AtomicUsize::new(0);
static DISPOSE_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_SET_LOADING: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_LOAD_LEVEL: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_FINISHED_LOADING: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_DISPOSE: AtomicUsize = AtomicUsize::new(0);

static RAW_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static DISCARD_THROUGH: AtomicU64 = AtomicU64::new(0);
static RAW_LIFECYCLE: OnceLock<ArrayQueue<RawLifecycle>> = OnceLock::new();
static ZONE_EDGES: OnceLock<ArrayQueue<ZoneHookEdge>> = OnceLock::new();
static CURRENT: OnceLock<Mutex<Option<LifecycleSnapshot>>> = OnceLock::new();
static RECONCILIATION: OnceLock<Mutex<Option<LifecycleReconciliation>>> = OnceLock::new();

static RAW_DROPS: AtomicU64 = AtomicU64::new(0);
static EDGE_DROPS: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static FILTERED: AtomicU64 = AtomicU64::new(0);
static LOADING_EDGES: AtomicU64 = AtomicU64::new(0);
static LOADS: AtomicU64 = AtomicU64::new(0);
static READY_EDGES: AtomicU64 = AtomicU64::new(0);
static LOSSES: AtomicU64 = AtomicU64::new(0);
static ZONE_EDGES_PUBLISHED: AtomicU64 = AtomicU64::new(0);
static RECONCILES: AtomicU64 = AtomicU64::new(0);
static RESYNCS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn prepare_queues() {
    let _ = RAW_LIFECYCLE.get_or_init(|| ArrayQueue::new(RAW_QUEUE_CAPACITY));
    let _ = ZONE_EDGES.get_or_init(|| ArrayQueue::new(ZONE_QUEUE_CAPACITY));
    let _ = CURRENT.get_or_init(|| Mutex::new(None));
    let _ = RECONCILIATION.get_or_init(|| Mutex::new(None));
}

pub(crate) fn observe_game_app_type(type_pointer: usize) {
    if type_pointer >= 0x1_0000 {
        let _ =
            GAME_APP_TYPE.compare_exchange(0, type_pointer, Ordering::AcqRel, Ordering::Acquire);
    }
}

pub(crate) fn capture_sequence() -> u64 {
    RAW_SEQUENCE.load(Ordering::Acquire)
}

pub(crate) fn reconcile(snapshot: LifecycleSnapshot, through_sequence: u64) {
    let slot = RECONCILIATION.get_or_init(|| Mutex::new(None));
    if let Ok(mut pending) = slot.lock() {
        *pending = Some(LifecycleReconciliation {
            through_sequence,
            snapshot,
        });
    }
}

pub(crate) fn current() -> Option<LifecycleSnapshot> {
    CURRENT.get()?.lock().ok()?.clone()
}

pub(crate) fn try_install_hooks(hl: &HashLink<'_>, profile: GameBuildProfile) {
    if HOOK_STATUS.load(Ordering::Acquire) != 0 {
        return;
    }
    let game_app_type = GAME_APP_TYPE.load(Ordering::Acquire);
    if game_app_type == 0 {
        return;
    }
    if HOOK_STATUS
        .compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let result = resolve_hooks(hl, game_app_type, profile).and_then(|hooks| install_hooks(&hooks));
    match result {
        Ok(()) => HOOK_STATUS.store(1, Ordering::Release),
        Err(error) => {
            let _ = HOOK_ERROR.set(error);
            HOOK_STATUS.store(3, Ordering::Release);
        }
    }
}

fn resolve_hooks(
    hl: &HashLink<'_>,
    game_app_type: usize,
    profile: GameBuildProfile,
) -> Result<ResolvedHooks, String> {
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    let set_loading = runtime.resolve_method(hl, game_app_type, &SET_LOADING)?;
    let load_level = runtime.resolve_method(hl, game_app_type, &LOAD_LEVEL)?;
    let finished_loading = runtime.resolve_method(hl, game_app_type, &FINISHED_LOADING)?;
    let dispose_spec = if profile.uses_beta_abi() {
        &DISPOSE_BETA
    } else {
        &DISPOSE
    };
    let dispose = runtime.resolve_method(hl, game_app_type, dispose_spec)?;
    let string_type = load_level
        .argument_type(1)
        .ok_or_else(|| "validated loadLevel signature omitted String".to_owned())?;
    LAYOUT
        .set(LifecycleLayout {
            game_app_type,
            string_type,
        })
        .map_err(|_| "lifecycle layout was already initialized".to_owned())?;
    Ok(ResolvedHooks {
        set_loading,
        load_level,
        finished_loading,
        dispose,
    })
}

fn install_hooks(hooks: &ResolvedHooks) -> Result<(), String> {
    let targets = [
        hooks.set_loading.target() as *mut c_void,
        hooks.load_level.target() as *mut c_void,
        hooks.finished_loading.target() as *mut c_void,
        hooks.dispose.target() as *mut c_void,
    ];
    for index in 0..targets.len() {
        if targets[..index].contains(&targets[index]) {
            return Err("lifecycle methods resolved to duplicate targets".to_owned());
        }
    }
    let detours = [
        hook_set_loading as *mut c_void,
        hook_load_level as *mut c_void,
        hook_finished_loading as *mut c_void,
        hook_dispose as *mut c_void,
    ];
    let names = [
        "set_loadingState",
        "loadLevel",
        "finishedLoading",
        "dispose",
    ];
    let mut originals = [0_usize; 4];
    for index in 0..targets.len() {
        match create_hook(targets[index], detours[index], names[index]) {
            Ok(original) => originals[index] = original,
            Err(error) => {
                for target in targets[..index].iter().copied() {
                    remove_hook(target);
                }
                return Err(error);
            }
        }
    }
    ORIGINAL_SET_LOADING.store(originals[0], Ordering::Release);
    ORIGINAL_LOAD_LEVEL.store(originals[1], Ordering::Release);
    ORIGINAL_FINISHED_LOADING.store(originals[2], Ordering::Release);
    ORIGINAL_DISPOSE.store(originals[3], Ordering::Release);
    for index in 0..targets.len() {
        // SAFETY: every method target and callback ABI was validated above.
        if let Err(status) = unsafe { MinHook::enable_hook(targets[index]) } {
            for target in targets[..index].iter().copied() {
                // SAFETY: only successfully enabled targets are disabled.
                let _ = unsafe { MinHook::disable_hook(target) };
            }
            for target in targets {
                remove_hook(target);
            }
            ORIGINAL_SET_LOADING.store(0, Ordering::Release);
            ORIGINAL_LOAD_LEVEL.store(0, Ordering::Release);
            ORIGINAL_FINISHED_LOADING.store(0, Ordering::Release);
            ORIGINAL_DISPOSE.store(0, Ordering::Release);
            return Err(format!("enable {} hook returned {status:?}", names[index]));
        }
    }

    SET_LOADING_TARGET.store(targets[0] as usize, Ordering::Release);
    LOAD_LEVEL_TARGET.store(targets[1] as usize, Ordering::Release);
    FINISHED_LOADING_TARGET.store(targets[2] as usize, Ordering::Release);
    DISPOSE_TARGET.store(targets[3] as usize, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

fn create_hook(target: *mut c_void, detour: *mut c_void, name: &str) -> Result<usize, String> {
    // SAFETY: the caller resolved and signature-validated this exact method.
    std::panic::catch_unwind(|| unsafe { MinHook::create_hook(target, detour) })
        .map_err(|_| format!("MinHook initialization panicked for {name}"))?
        .map(|original| original as usize)
        .map_err(|status| format!("create {name} hook returned {status:?}"))
}

fn remove_hook(target: *mut c_void) {
    // SAFETY: callers pass only successfully created targets.
    let _ = unsafe { MinHook::remove_hook(target) };
}

unsafe extern "C" fn hook_set_loading(app: *mut c_void, requested: i32) -> i32 {
    let valid_app = valid_app(app);
    let original = ORIGINAL_SET_LOADING.load(Ordering::Acquire);
    if original == 0 {
        return requested;
    }
    // SAFETY: this trampoline has the validated `(GameApp, i32) -> i32` ABI.
    let original: HlSetLoading = unsafe { std::mem::transmute(original) };
    let result = unsafe { original(app, requested) };
    if ACTIVE.load(Ordering::Relaxed) && valid_app {
        let mut raw = RawLifecycle::edge(RawLifecycleKind::Loading, app as usize);
        raw.loading_state = result;
        queue_raw(raw);
    } else if ACTIVE.load(Ordering::Relaxed) {
        INVALID.fetch_add(1, Ordering::Relaxed);
    }
    result
}

unsafe extern "C" fn hook_load_level(app: *mut c_void, level: *mut c_void, force: *mut c_void) {
    let valid_app = valid_app(app);
    let original = ORIGINAL_LOAD_LEVEL.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: this trampoline has the validated
    // `(GameApp, String, ref<bool>) -> Void` ABI.
    let original: HlLoadLevel = unsafe { std::mem::transmute(original) };
    unsafe { original(app, level, force) };
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let Some((area, area_len)) = (valid_app.then(|| unsafe { copy_area(level) })).flatten() else {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let mut raw = RawLifecycle::edge(RawLifecycleKind::LoadLevel, app as usize);
    raw.area = area;
    raw.area_len = area_len;
    queue_raw(raw);
}

unsafe extern "C" fn hook_finished_loading(app: *mut c_void) {
    let valid_app = valid_app(app);
    let original = ORIGINAL_FINISHED_LOADING.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: this trampoline has the validated `(GameApp) -> Void` ABI.
    let original: HlGameAppVoid = unsafe { std::mem::transmute(original) };
    unsafe { original(app) };
    if ACTIVE.load(Ordering::Relaxed) && valid_app {
        queue_raw(RawLifecycle::edge(RawLifecycleKind::Ready, app as usize));
    } else if ACTIVE.load(Ordering::Relaxed) {
        INVALID.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe extern "C" fn hook_dispose(app: *mut c_void) {
    let valid_app = valid_app(app);
    let original = ORIGINAL_DISPOSE.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: this trampoline has the validated `(GameApp) -> Void` ABI.
    let original: HlGameAppVoid = unsafe { std::mem::transmute(original) };
    unsafe { original(app) };
    if ACTIVE.load(Ordering::Relaxed) && valid_app {
        queue_raw(RawLifecycle::edge(RawLifecycleKind::Lost, app as usize));
        crate::player_hooks::observe_app_loss(app as usize);
    } else if ACTIVE.load(Ordering::Relaxed) {
        INVALID.fetch_add(1, Ordering::Relaxed);
    }
}

fn valid_app(app: *mut c_void) -> bool {
    LAYOUT
        .get()
        .is_some_and(|layout| unsafe { object_has_exact_type(app, layout.game_app_type) })
}

unsafe fn copy_area(level: *mut c_void) -> Option<([u16; AREA_UTF16_CAPACITY], u16)> {
    let layout = LAYOUT.get().copied()?;
    if !unsafe { object_has_exact_type(level, layout.string_type) } {
        return None;
    }
    let base = level.cast::<u8>();
    // SAFETY: Farever's exact String argument type was validated before the
    // hook was enabled; these are its fixed data pointer and length fields.
    let chars = unsafe { std::ptr::read_unaligned(base.add(8).cast::<*const u16>()) };
    let length = unsafe { std::ptr::read_unaligned(base.add(0x10).cast::<i32>()) };
    let length = usize::try_from(length).ok()?;
    if length == 0 || length > AREA_UTF16_CAPACITY || chars.is_null() {
        return None;
    }
    let mut area = [0_u16; AREA_UTF16_CAPACITY];
    // SAFETY: the live immutable String argument contains `length` UTF-16
    // units, and the validated bound fits the fixed destination.
    unsafe { std::ptr::copy_nonoverlapping(chars, area.as_mut_ptr(), length) };
    Some((area, length as u16))
}

/// Receives the already validated disconnect boundary owned by player_hooks.
/// This is callback-safe and never performs metadata traversal or allocation.
pub(crate) fn observe_disconnect(app: usize) {
    if ACTIVE.load(Ordering::Relaxed) && app >= 0x1_0000 {
        queue_raw(RawLifecycle::edge(RawLifecycleKind::Lost, app));
    }
}

fn queue_raw(mut raw: RawLifecycle) {
    raw.sequence = RAW_SEQUENCE.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    if RAW_LIFECYCLE
        .get()
        .is_none_or(|queue| queue.push(raw).is_err())
    {
        RAW_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

impl LifecycleHookDecoder {
    pub(crate) fn decode_pending(&mut self, hl: &HashLink<'_>) {
        let raw_drops = RAW_DROPS.load(Ordering::Acquire);
        if raw_drops != self.observed_raw_drops {
            self.staged_area = None;
            self.observed_raw_drops = raw_drops;
            RESYNCS.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(reconciliation) = take_reconciliation() {
            DISCARD_THROUGH.fetch_max(reconciliation.through_sequence, Ordering::AcqRel);
            self.staged_area = None;
            publish_state(reconciliation.snapshot, false);
            RECONCILES.fetch_add(1, Ordering::Relaxed);
        }
        let Some(queue) = RAW_LIFECYCLE.get() else {
            return;
        };
        let discard_through = DISCARD_THROUGH.load(Ordering::Acquire);
        for _ in 0..MAX_DECODE_PER_TICK {
            let Some(raw) = queue.pop() else {
                break;
            };
            if raw.sequence <= discard_through {
                continue;
            }
            self.apply(hl, raw);
        }
    }

    fn apply(&mut self, hl: &HashLink<'_>, raw: RawLifecycle) {
        if raw.app != crate::player_hooks::current_app().unwrap_or_default() {
            FILTERED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        match raw.kind {
            RawLifecycleKind::Loading => {
                let mut snapshot = state_for(raw.app);
                snapshot.loading_state = raw.loading_state;
                publish_state(snapshot, false);
                LOADING_EDGES.fetch_add(1, Ordering::Relaxed);
            }
            RawLifecycleKind::LoadLevel => {
                let length = usize::from(raw.area_len).min(raw.area.len());
                match String::from_utf16(&raw.area[..length]) {
                    Ok(area) if !area.is_empty() => {
                        self.staged_area = Some((raw.app, area));
                        LOADS.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        INVALID.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            RawLifecycleKind::Ready => {
                let sampled_area = read_area(hl, raw.app);
                let staged_area = self
                    .staged_area
                    .take()
                    .filter(|(app, _)| *app == raw.app)
                    .map(|(_, area)| area);
                let mut snapshot = state_for(raw.app);
                snapshot.in_world = true;
                snapshot.area = sampled_area.or(staged_area);
                publish_state(snapshot, true);
                READY_EDGES.fetch_add(1, Ordering::Relaxed);
            }
            RawLifecycleKind::Lost => {
                self.staged_area = None;
                let mut snapshot = state_for(raw.app);
                snapshot.in_world = false;
                snapshot.area = None;
                publish_state(snapshot, true);
                LOSSES.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn read_area(hl: &HashLink<'_>, app: usize) -> Option<String> {
    let world = hl.pointer_field(app, "world")?;
    let level = hl.pointer_field(world, "level")?;
    hl.string(level).filter(|area| !area.is_empty())
}

fn state_for(app: usize) -> LifecycleSnapshot {
    current()
        .filter(|snapshot| snapshot.app == app)
        .unwrap_or(LifecycleSnapshot {
            app,
            loading_state: -1,
            in_world: false,
            area: None,
        })
}

fn publish_state(snapshot: LifecycleSnapshot, emit_zone_edge: bool) {
    let previous_area = current()
        .filter(|previous| previous.app == snapshot.app)
        .and_then(|previous| previous.area);
    let changed = previous_area != snapshot.area;
    if let Some(state) = CURRENT.get() {
        if let Ok(mut current) = state.lock() {
            *current = Some(snapshot.clone());
        }
    }
    if emit_zone_edge && changed {
        let edge = ZoneHookEdge {
            app: snapshot.app,
            area: snapshot.area,
        };
        if ZONE_EDGES
            .get()
            .is_none_or(|queue| queue.push(edge).is_err())
        {
            EDGE_DROPS.fetch_add(1, Ordering::Relaxed);
        } else {
            ZONE_EDGES_PUBLISHED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn take_reconciliation() -> Option<LifecycleReconciliation> {
    RECONCILIATION
        .get()
        .and_then(|slot| slot.lock().ok()?.take())
}

pub(crate) fn drain_zone_edges() -> Vec<Option<String>> {
    let current_app = crate::player_hooks::current_app().unwrap_or_default();
    let Some(queue) = ZONE_EDGES.get() else {
        return Vec::new();
    };
    let mut result = Vec::new();
    while let Some(edge) = queue.pop() {
        if edge.app == current_app {
            result.push(edge.area);
        } else {
            FILTERED.fetch_add(1, Ordering::Relaxed);
        }
    }
    result
}

pub(crate) fn status() -> usize {
    HOOK_STATUS.load(Ordering::Acquire)
}

pub(crate) fn provider_available() -> bool {
    status() == 1 && crate::player_hooks::status() == 1
}

pub(crate) fn provider_status() -> usize {
    if provider_available() {
        1
    } else if status() == 3 || crate::player_hooks::status() == 3 {
        3
    } else {
        0
    }
}

pub(crate) fn status_name(status: usize) -> &'static str {
    match status {
        0 => "waiting-for-game-app-type",
        1 => "active",
        3 => "failed",
        4 => "installing",
        _ => "unknown",
    }
}

pub(crate) fn error() -> Option<&'static str> {
    HOOK_ERROR.get().map(String::as_str)
}

pub(crate) fn total_drops() -> u64 {
    RAW_DROPS
        .load(Ordering::Acquire)
        .saturating_add(EDGE_DROPS.load(Ordering::Acquire))
}

pub(crate) fn metrics() -> String {
    format!(
        "lifecycle_hooks={} lifecycle_provider={} lifecycle_loading={} lifecycle_loads={} lifecycle_ready={} lifecycle_losses={} lifecycle_zone_edges={} lifecycle_reconciles={} lifecycle_resyncs={} lifecycle_filtered={} lifecycle_invalid={} lifecycle_raw_drops={} lifecycle_edge_drops={}",
        status_name(status()),
        if provider_available() { "direct" } else { "unavailable" },
        LOADING_EDGES.load(Ordering::Relaxed),
        LOADS.load(Ordering::Relaxed),
        READY_EDGES.load(Ordering::Relaxed),
        LOSSES.load(Ordering::Relaxed),
        ZONE_EDGES_PUBLISHED.load(Ordering::Relaxed),
        RECONCILES.load(Ordering::Relaxed),
        RESYNCS.load(Ordering::Relaxed),
        FILTERED.load(Ordering::Relaxed),
        INVALID.load(Ordering::Relaxed),
        RAW_DROPS.load(Ordering::Relaxed),
        EDGE_DROPS.load(Ordering::Relaxed),
    )
}

pub(crate) fn shutdown_hooks() {
    ACTIVE.store(false, Ordering::Release);
    for target in [
        SET_LOADING_TARGET.load(Ordering::Acquire),
        LOAD_LEVEL_TARGET.load(Ordering::Acquire),
        FINISHED_LOADING_TARGET.load(Ordering::Acquire),
        DISPOSE_TARGET.load(Ordering::Acquire),
    ] {
        if target != 0 {
            // SAFETY: targets are stored only after validated hooks are enabled.
            let _ = unsafe { MinHook::disable_hook(target as *mut c_void) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_specs_match_verified_lifecycle_boundaries() {
        assert_eq!(SET_LOADING.name, c"set_loadingState");
        assert_eq!(SET_LOADING.arguments, SET_LOADING_ARGUMENTS);
        assert_eq!(LOAD_LEVEL.name, c"loadLevel");
        assert_eq!(LOAD_LEVEL.arguments, LOAD_LEVEL_ARGUMENTS);
        assert_eq!(FINISHED_LOADING.name, c"finishedLoading");
        assert_eq!(DISPOSE.name, c"dispose");
    }

    #[test]
    fn raw_edge_constructor_is_bounded_and_empty() {
        let raw = RawLifecycle::edge(RawLifecycleKind::Ready, 0x10_000);
        assert_eq!(raw.app, 0x10_000);
        assert_eq!(raw.area_len, 0);
        assert!(raw.area.iter().all(|unit| *unit == 0));
    }
}
