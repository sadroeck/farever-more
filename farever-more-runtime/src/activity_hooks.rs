//! Build-gated `st.GameLayer.set_mainActivity` observation.
//!
//! The callback can run on arbitrary game threads. It forwards the original
//! call, copies only fixed-size activity identity, and writes to a bounded
//! lock-free queue. Runtime metadata traversal, instance classification, local
//! layer filtering, diagnostics, and Wasm delivery happen later.

use crate::hashlink::{
    object_has_exact_type, validate_object, HashLinkFieldSpec, HashLinkKind, HashLinkMethodSpec,
    HashLinkObjectSpec, HashLinkRuntime, HashLinkTypeSpec, ValidatedHashLinkMethod,
};
use crate::state::{classify_main_activity_type, HashLink, InstanceObservation};
use crossbeam_queue::ArrayQueue;
use minhook::MinHook;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const RAW_QUEUE_CAPACITY: usize = 128;
const MAX_PENDING: usize = 128;
const MAX_DECODE_PER_TICK: usize = 32;
const MAX_ACTIVITY_KIND_CODE_UNITS: usize = 128;
const UNMATCHED_LIFETIME: Duration = Duration::from_secs(1);

const SET_MAIN_ACTIVITY_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.GameLayer"),
    HashLinkTypeSpec::Object("st.Activity"),
];
const SET_MAIN_ACTIVITY: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.GameLayer",
    name: c"set_mainActivity",
    arguments: SET_MAIN_ACTIVITY_ARGUMENTS,
    result: HashLinkTypeSpec::Object("st.Activity"),
};
const ACTIVITY_FIELDS: &[HashLinkFieldSpec] = &[HashLinkFieldSpec::object("kind", "String")];
const ACTIVITY_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.Activity",
    kind: HashLinkKind::Object,
    fields: ACTIVITY_FIELDS,
};
const STRING_FIELDS: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::scalar("bytes", HashLinkKind::Bytes),
    HashLinkFieldSpec::scalar("length", HashLinkKind::I32),
];
const STRING_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "String",
    kind: HashLinkKind::Object,
    fields: STRING_FIELDS,
};

type HlSetMainActivity = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;

#[derive(Clone, Copy, Debug)]
struct ActivityLayout {
    activity_kind: usize,
    string_type: usize,
    string_bytes: usize,
    string_length: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RawMainActivity {
    layer_pointer: usize,
    matched_local_layer: bool,
    activity_type: usize,
    kind_length: u16,
    kind: [u16; MAX_ACTIVITY_KIND_CODE_UNITS],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActivityHookEdge {
    pub(crate) observation: Option<InstanceObservation>,
}

struct PendingRaw {
    raw: RawMainActivity,
    first_seen: Instant,
}

#[derive(Default)]
pub(crate) struct ActivityHookDecoder {
    pending: VecDeque<PendingRaw>,
}

impl ActivityHookDecoder {
    pub(crate) fn decode_pending(
        &mut self,
        hl: &HashLink<'_>,
        local_layers: &[usize],
    ) -> Vec<ActivityHookEdge> {
        if let Some(queue) = RAW_EDGES.get() {
            while self.pending.len() < MAX_PENDING {
                let Some(raw) = queue.pop() else {
                    break;
                };
                self.pending.push_back(PendingRaw {
                    raw,
                    first_seen: Instant::now(),
                });
            }
        }

        let mut edges = Vec::new();
        let count = self.pending.len().min(MAX_DECODE_PER_TICK);
        for _ in 0..count {
            let Some(pending) = self.pending.pop_front() else {
                break;
            };
            if !pending.raw.matched_local_layer
                && !local_layers.contains(&pending.raw.layer_pointer)
            {
                if pending.first_seen.elapsed() < UNMATCHED_LIFETIME {
                    self.pending.push_back(pending);
                } else {
                    FILTERED.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
            match decode_raw(hl, &pending.raw) {
                Some(edge) => {
                    DECODED.fetch_add(1, Ordering::Relaxed);
                    edges.push(edge);
                }
                None => {
                    INVALID.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        edges
    }
}

// 0 = waiting for type, 1 = active, 3 = failed, 4 = installing.
static GAME_LAYER_TYPE: AtomicUsize = AtomicUsize::new(0);
static LOCAL_LAYER: AtomicUsize = AtomicUsize::new(0);
static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_SET_MAIN_ACTIVITY: AtomicUsize = AtomicUsize::new(0);
static ACTIVITY_LAYOUT: OnceLock<ActivityLayout> = OnceLock::new();
static RAW_EDGES: OnceLock<ArrayQueue<RawMainActivity>> = OnceLock::new();
static RAW_DROPS: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static DECODED: AtomicU64 = AtomicU64::new(0);
static FILTERED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn prepare_queue() {
    let _ = RAW_EDGES.get_or_init(|| ArrayQueue::new(RAW_QUEUE_CAPACITY));
}

pub(crate) fn observe_game_layer_type(type_pointer: usize) {
    if type_pointer >= 0x1_0000 {
        let _ =
            GAME_LAYER_TYPE.compare_exchange(0, type_pointer, Ordering::AcqRel, Ordering::Acquire);
    }
}

pub(crate) fn game_layer_type() -> Option<usize> {
    let pointer = GAME_LAYER_TYPE.load(Ordering::Acquire);
    (pointer >= 0x1_0000).then_some(pointer)
}

pub(crate) fn observe_local_layer(layer_pointer: Option<usize>) {
    if let Some(layer_pointer) = layer_pointer.filter(|pointer| *pointer >= 0x1_0000) {
        LOCAL_LAYER.store(layer_pointer, Ordering::Release);
    }
}

pub(crate) fn clear_local_layer() {
    LOCAL_LAYER.store(0, Ordering::Release);
}

pub(crate) fn try_install_hook(hl: &HashLink<'_>) {
    if HOOK_STATUS.load(Ordering::Acquire) != 0 {
        return;
    }
    let game_layer_type = GAME_LAYER_TYPE.load(Ordering::Acquire);
    if game_layer_type == 0 {
        return;
    }
    if HOOK_STATUS
        .compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let result = resolve_hook(hl, game_layer_type).and_then(install_hook);
    match result {
        Ok(()) => HOOK_STATUS.store(1, Ordering::Release),
        Err(error) => {
            let _ = HOOK_ERROR.set(error);
            HOOK_STATUS.store(3, Ordering::Release);
        }
    }
}

fn resolve_hook(
    hl: &HashLink<'_>,
    game_layer_type: usize,
) -> Result<ValidatedHashLinkMethod, String> {
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    let method = runtime.resolve_method(hl, game_layer_type, &SET_MAIN_ACTIVITY)?;
    let activity_type = method.argument_type(1).ok_or_else(|| {
        "validated set_mainActivity signature omitted its Activity argument".to_owned()
    })?;
    let activity = validate_object(hl, activity_type, &ACTIVITY_SCHEMA)?;
    let string_type = activity
        .field_type_address("kind")
        .ok_or_else(|| "validated Activity layout omitted kind type".to_owned())?;
    let string = validate_object(hl, string_type, &STRING_SCHEMA)?;
    let layout = ActivityLayout {
        activity_kind: activity
            .offset("kind")
            .ok_or_else(|| "validated Activity layout omitted kind".to_owned())?,
        string_type: string.type_address,
        string_bytes: string
            .offset("bytes")
            .ok_or_else(|| "validated String layout omitted bytes".to_owned())?,
        string_length: string
            .offset("length")
            .ok_or_else(|| "validated String layout omitted length".to_owned())?,
    };
    ACTIVITY_LAYOUT
        .set(layout)
        .map_err(|_| "set_mainActivity layout was already initialized".to_owned())?;
    Ok(method)
}

fn install_hook(method: ValidatedHashLinkMethod) -> Result<(), String> {
    let target = method.target() as *mut c_void;
    // SAFETY: method resolution is build-gated, name-addressed, and validates
    // the exact `(GameLayer, Activity) -> Activity` HashLink signature.
    let original = std::panic::catch_unwind(|| unsafe {
        MinHook::create_hook(target, hook_set_main_activity as *mut c_void)
    })
    .map_err(|_| "MinHook initialization panicked for set_mainActivity".to_owned())?
    .map_err(|status| format!("create set_mainActivity hook returned {status:?}"))?;
    ORIGINAL_SET_MAIN_ACTIVITY.store(original as usize, Ordering::Release);
    // SAFETY: the hook and original trampoline were created above.
    if let Err(status) = unsafe { MinHook::enable_hook(target) } {
        // SAFETY: the hook exists but was not successfully enabled.
        let _ = unsafe { MinHook::remove_hook(target) };
        ORIGINAL_SET_MAIN_ACTIVITY.store(0, Ordering::Release);
        return Err(format!("enable set_mainActivity hook returned {status:?}"));
    }
    HOOK_TARGET.store(target as usize, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

unsafe extern "C" fn hook_set_main_activity(
    layer: *mut c_void,
    activity: *mut c_void,
) -> *mut c_void {
    // Capture before forwarding because the original setter can allocate and
    // trigger a moving collection. This performs only fixed-size reads.
    let raw = unsafe { capture_raw(layer, activity) };
    let original = ORIGINAL_SET_MAIN_ACTIVITY.load(Ordering::Acquire);
    if original == 0 {
        return activity;
    }
    // SAFETY: MinHook returned this trampoline for the signature-validated
    // `(st.GameLayer, st.Activity) -> st.Activity` target.
    let original: HlSetMainActivity = unsafe { std::mem::transmute(original) };
    let result = unsafe { original(layer, activity) };
    if ACTIVE.load(Ordering::Relaxed) {
        if let Some(raw) = raw {
            if RAW_EDGES.get().is_none_or(|queue| queue.push(raw).is_err()) {
                RAW_DROPS.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            INVALID.fetch_add(1, Ordering::Relaxed);
        }
    }
    result
}

unsafe fn capture_raw(layer: *mut c_void, activity: *mut c_void) -> Option<RawMainActivity> {
    let layer_pointer = layer as usize;
    if layer_pointer < 0x1_0000 {
        return None;
    }
    if activity.is_null() {
        return Some(RawMainActivity {
            layer_pointer,
            matched_local_layer: LOCAL_LAYER.load(Ordering::Acquire) == layer_pointer,
            activity_type: 0,
            kind_length: 0,
            kind: [0; MAX_ACTIVITY_KIND_CODE_UNITS],
        });
    }
    let layout = ACTIVITY_LAYOUT.get()?;
    // SAFETY: the signature-validated callback supplies a live Activity
    // object, whose first word is its concrete HashLink type pointer.
    let activity_type = unsafe { std::ptr::read_unaligned(activity.cast::<usize>()) };
    if activity_type < 0x1_0000 {
        return None;
    }
    let activity_base = activity.cast::<u8>();
    // SAFETY: `activity_kind` was validated from the base Activity layout and
    // remains the inherited field offset for every concrete Activity subtype.
    let kind_pointer = unsafe {
        std::ptr::read_unaligned(activity_base.add(layout.activity_kind).cast::<usize>())
    };
    if kind_pointer < 0x1_0000
        || !unsafe { object_has_exact_type(kind_pointer as *const c_void, layout.string_type) }
    {
        return None;
    }
    let string_base = kind_pointer as *const u8;
    // SAFETY: both String offsets were validated before the hook was enabled.
    let kind_length =
        unsafe { std::ptr::read_unaligned(string_base.add(layout.string_length).cast::<i32>()) };
    if !(0..=MAX_ACTIVITY_KIND_CODE_UNITS as i32).contains(&kind_length) {
        return None;
    }
    let mut kind = [0_u16; MAX_ACTIVITY_KIND_CODE_UNITS];
    if kind_length > 0 {
        // SAFETY: the String layout and bounded positive length are validated.
        let bytes = unsafe {
            std::ptr::read_unaligned(string_base.add(layout.string_bytes).cast::<usize>())
        };
        if bytes < 0x1_0000 {
            return None;
        }
        // SAFETY: the copy is capped to the fixed-size destination.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes as *const u16,
                kind.as_mut_ptr(),
                kind_length as usize,
            );
        }
    }
    Some(RawMainActivity {
        layer_pointer,
        matched_local_layer: LOCAL_LAYER.load(Ordering::Acquire) == layer_pointer,
        activity_type,
        kind_length: kind_length as u16,
        kind,
    })
}

fn decode_raw(hl: &HashLink<'_>, raw: &RawMainActivity) -> Option<ActivityHookEdge> {
    if raw.activity_type == 0 {
        return Some(ActivityHookEdge { observation: None });
    }
    let length = usize::from(raw.kind_length);
    if length > raw.kind.len() {
        return None;
    }
    let kind = String::from_utf16(&raw.kind[..length]).ok()?;
    let observation = classify_main_activity_type(hl, raw.activity_type, &kind).ok()?;
    Some(ActivityHookEdge {
        observation: Some(observation),
    })
}

pub(crate) fn status() -> usize {
    HOOK_STATUS.load(Ordering::Acquire)
}

pub(crate) fn error() -> Option<&'static str> {
    HOOK_ERROR.get().map(String::as_str)
}

pub(crate) fn total_losses() -> u64 {
    RAW_DROPS
        .load(Ordering::Acquire)
        .saturating_add(INVALID.load(Ordering::Acquire))
}

pub(crate) fn metrics() -> String {
    format!(
        "activity_hook={} activity_edges_decoded={} activity_edges_filtered={} activity_edges_invalid={} activity_edge_drops={}",
        status_name(status()),
        DECODED.load(Ordering::Relaxed),
        FILTERED.load(Ordering::Relaxed),
        INVALID.load(Ordering::Relaxed),
        RAW_DROPS.load(Ordering::Relaxed),
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

pub(crate) fn shutdown_hook() {
    ACTIVE.store(false, Ordering::Release);
    clear_local_layer();
    let target = HOOK_TARGET.load(Ordering::Acquire);
    if target != 0 {
        // SAFETY: the target is published only after the build-gated,
        // signature-validated hook was created and enabled.
        let _ = unsafe { MinHook::disable_hook(target as *mut c_void) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_spec_matches_verified_setter_boundary() {
        assert_eq!(SET_MAIN_ACTIVITY.lookup_type, "st.GameLayer");
        assert_eq!(SET_MAIN_ACTIVITY.name, c"set_mainActivity");
        assert_eq!(SET_MAIN_ACTIVITY.arguments, SET_MAIN_ACTIVITY_ARGUMENTS);
        assert_eq!(
            SET_MAIN_ACTIVITY.result,
            HashLinkTypeSpec::Object("st.Activity")
        );
    }

    #[test]
    fn null_activity_is_a_valid_non_transition_observation() {
        let raw = RawMainActivity {
            layer_pointer: 0x1000_0000,
            matched_local_layer: true,
            activity_type: 0,
            kind_length: 0,
            kind: [0; MAX_ACTIVITY_KIND_CODE_UNITS],
        };
        assert_eq!(
            decode_raw(
                &HashLink::new(&crate::memory::ProcessMemory::current()),
                &raw
            ),
            Some(ActivityHookEdge { observation: None })
        );
    }
}
