//! Hook-driven party-roster dirty observations.
//!
//! Farever binds both normal `st.Group.players` and Rift
//! `st.GameLayer.players` as `hxbit.ArrayProxyData`. Every proxy mutation marks
//! the owning network property through `networkSetBitCond`; the exact owner
//! hooks below filter to the active group/layer and enqueue only a fixed-size
//! dirty record. Roster decoding and metadata refresh stay on the poller.

use crate::game_build::GameBuildProfile;
use crate::hashlink::{
    validate_object, HashLink, HashLinkFieldSpec, HashLinkKind, HashLinkMethodSpec,
    HashLinkObjectSpec, HashLinkRuntime, HashLinkTypeSpec,
};
use crossbeam_queue::ArrayQueue;
use minhook::MinHook;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

const RAW_QUEUE_CAPACITY: usize = 128;
const GROUP_PLAYERS_BIT: i32 = 4;
const LAYER_PLAYERS_BIT: i32 = 9;

const PLAYER_FIELDS: &[HashLinkFieldSpec] = &[HashLinkFieldSpec::object("group", "st.Group")];
const PLAYER_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.Player",
    kind: HashLinkKind::Object,
    fields: PLAYER_FIELDS,
};
const SET_GROUP_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.Player"),
    HashLinkTypeSpec::Object("st.Group"),
];
const GROUP_DIRTY_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.Group"),
    HashLinkTypeSpec::Kind(HashLinkKind::I32),
];
const GROUP_DIRTY_BETA_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.BaseState"),
    HashLinkTypeSpec::Kind(HashLinkKind::I32),
];
const LAYER_DIRTY_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.GameLayer"),
    HashLinkTypeSpec::Kind(HashLinkKind::I32),
];
const LAYER_DIRTY_BETA_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.BaseState"),
    HashLinkTypeSpec::Kind(HashLinkKind::I32),
];
const SET_GROUP: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.Player",
    name: c"set_group",
    arguments: SET_GROUP_ARGUMENTS,
    result: HashLinkTypeSpec::Object("st.Group"),
};
const GROUP_DIRTY: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.Group",
    name: c"networkSetBitCond",
    arguments: GROUP_DIRTY_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
// Beta's runtime vtable exposes the shared BaseState receiver even though the
// bytecode declares class-specific virtual overrides on Group and GameLayer.
const GROUP_DIRTY_BETA: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.Group",
    name: c"networkSetBitCond",
    arguments: GROUP_DIRTY_BETA_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const LAYER_DIRTY: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.GameLayer",
    name: c"networkSetBitCond",
    arguments: LAYER_DIRTY_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const LAYER_DIRTY_BETA: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.GameLayer",
    name: c"networkSetBitCond",
    arguments: LAYER_DIRTY_BETA_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};

type HlSetGroup = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
type HlSetBit = unsafe extern "C" fn(*mut c_void, i32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirtySource {
    GroupSwitch,
    GroupPlayers,
    RiftPlayers,
}

#[derive(Clone, Copy, Debug)]
struct RawDirty {
    source: DirtySource,
    owner: usize,
}

struct ResolvedHooks {
    set_group: usize,
    group_dirty: usize,
    layer_dirty: usize,
}

// 0 = waiting for types, 1 = active, 3 = failed, 4 = installing.
static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static OBSERVED_GROUP: AtomicUsize = AtomicUsize::new(0);
static OBSERVED_RIFT_LAYER: AtomicUsize = AtomicUsize::new(0);

static SET_GROUP_TARGET: AtomicUsize = AtomicUsize::new(0);
static GROUP_DIRTY_TARGET: AtomicUsize = AtomicUsize::new(0);
static LAYER_DIRTY_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_SET_GROUP: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_GROUP_DIRTY: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_LAYER_DIRTY: AtomicUsize = AtomicUsize::new(0);

static RAW_DIRTY: OnceLock<ArrayQueue<RawDirty>> = OnceLock::new();
static RAW_DROPS: AtomicU64 = AtomicU64::new(0);
static GROUP_SWITCHES: AtomicU64 = AtomicU64::new(0);
static GROUP_DIRTY_EDGES: AtomicU64 = AtomicU64::new(0);
static RIFT_DIRTY_EDGES: AtomicU64 = AtomicU64::new(0);
static FILTERED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn prepare_queue() {
    let _ = RAW_DIRTY.get_or_init(|| ArrayQueue::new(RAW_QUEUE_CAPACITY));
}

pub(crate) fn observe_sources(group: Option<usize>, rift_layer: Option<usize>) {
    OBSERVED_GROUP.store(group.unwrap_or_default(), Ordering::Release);
    OBSERVED_RIFT_LAYER.store(rift_layer.unwrap_or_default(), Ordering::Release);
}

pub(crate) fn try_install_hooks(hl: &HashLink<'_>, profile: GameBuildProfile) {
    if HOOK_STATUS.load(Ordering::Acquire) != 0 {
        return;
    }
    let Some(player_type) = crate::player_hooks::player_type() else {
        return;
    };
    let Some(layer_type) = crate::activity_hooks::game_layer_type() else {
        return;
    };
    if HOOK_STATUS
        .compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let result = resolve_hooks(hl, player_type, layer_type, profile).and_then(install_hooks);
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
    player_type: usize,
    layer_type: usize,
    profile: GameBuildProfile,
) -> Result<ResolvedHooks, String> {
    let player = validate_object(hl, player_type, &PLAYER_SCHEMA)?;
    let group_type = player
        .field_type_address("group")
        .ok_or_else(|| "validated Player layout omitted group".to_owned())?;
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    let (group_dirty_spec, layer_dirty_spec) = if profile.uses_beta_abi() {
        (&GROUP_DIRTY_BETA, &LAYER_DIRTY_BETA)
    } else {
        (&GROUP_DIRTY, &LAYER_DIRTY)
    };
    Ok(ResolvedHooks {
        set_group: runtime
            .resolve_method(hl, player_type, &SET_GROUP)?
            .target(),
        group_dirty: runtime
            .resolve_method(hl, group_type, group_dirty_spec)?
            .target(),
        layer_dirty: runtime
            .resolve_method(hl, layer_type, layer_dirty_spec)?
            .target(),
    })
}

fn install_hooks(hooks: ResolvedHooks) -> Result<(), String> {
    let targets = [
        hooks.set_group as *mut c_void,
        hooks.group_dirty as *mut c_void,
        hooks.layer_dirty as *mut c_void,
    ];
    if targets[0] == targets[1] || targets[0] == targets[2] || targets[1] == targets[2] {
        return Err("party dirty methods resolved to duplicate targets".to_owned());
    }
    let detours = [
        hook_set_group as *mut c_void,
        hook_group_dirty as *mut c_void,
        hook_layer_dirty as *mut c_void,
    ];
    let names = [
        "Player.set_group",
        "Group.networkSetBitCond",
        "GameLayer.networkSetBitCond",
    ];
    let mut originals = [0_usize; 3];
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
    ORIGINAL_SET_GROUP.store(originals[0], Ordering::Release);
    ORIGINAL_GROUP_DIRTY.store(originals[1], Ordering::Release);
    ORIGINAL_LAYER_DIRTY.store(originals[2], Ordering::Release);
    for index in 0..targets.len() {
        // SAFETY: all exact method signatures were validated above.
        if let Err(status) = unsafe { MinHook::enable_hook(targets[index]) } {
            for target in targets[..index].iter().copied() {
                // SAFETY: only the successfully enabled prefix is disabled.
                let _ = unsafe { MinHook::disable_hook(target) };
            }
            for target in targets {
                remove_hook(target);
            }
            ORIGINAL_SET_GROUP.store(0, Ordering::Release);
            ORIGINAL_GROUP_DIRTY.store(0, Ordering::Release);
            ORIGINAL_LAYER_DIRTY.store(0, Ordering::Release);
            return Err(format!("enable {} hook returned {status:?}", names[index]));
        }
    }
    SET_GROUP_TARGET.store(targets[0] as usize, Ordering::Release);
    GROUP_DIRTY_TARGET.store(targets[1] as usize, Ordering::Release);
    LAYER_DIRTY_TARGET.store(targets[2] as usize, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

fn create_hook(target: *mut c_void, detour: *mut c_void, name: &str) -> Result<usize, String> {
    // SAFETY: the caller signature-validated this exact method.
    std::panic::catch_unwind(|| unsafe { MinHook::create_hook(target, detour) })
        .map_err(|_| format!("MinHook initialization panicked for {name}"))?
        .map(|original| original as usize)
        .map_err(|status| format!("create {name} hook returned {status:?}"))
}

fn remove_hook(target: *mut c_void) {
    // SAFETY: callers pass only successfully created targets.
    let _ = unsafe { MinHook::remove_hook(target) };
}

unsafe extern "C" fn hook_set_group(player: *mut c_void, group: *mut c_void) -> *mut c_void {
    let local = ACTIVE.load(Ordering::Relaxed)
        && player as usize == crate::player_hooks::local_player_pointer();
    let original = ORIGINAL_SET_GROUP.load(Ordering::Acquire);
    if original == 0 {
        return group;
    }
    // SAFETY: this trampoline has `(st.Player, st.Group) -> st.Group`.
    let original: HlSetGroup = unsafe { std::mem::transmute(original) };
    let result = unsafe { original(player, group) };
    if local {
        queue_dirty(DirtySource::GroupSwitch, group as usize);
    } else if ACTIVE.load(Ordering::Relaxed) {
        FILTERED.fetch_add(1, Ordering::Relaxed);
    }
    result
}

unsafe extern "C" fn hook_group_dirty(group: *mut c_void, bit: i32) {
    let original = ORIGINAL_GROUP_DIRTY.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: this trampoline has `(st.Group, i32) -> Void`.
    let original: HlSetBit = unsafe { std::mem::transmute(original) };
    unsafe { original(group, bit) };
    if ACTIVE.load(Ordering::Relaxed)
        && bit == GROUP_PLAYERS_BIT
        && group as usize == OBSERVED_GROUP.load(Ordering::Relaxed)
    {
        queue_dirty(DirtySource::GroupPlayers, group as usize);
    }
}

unsafe extern "C" fn hook_layer_dirty(layer: *mut c_void, bit: i32) {
    let original = ORIGINAL_LAYER_DIRTY.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: this trampoline has `(st.GameLayer, i32) -> Void`.
    let original: HlSetBit = unsafe { std::mem::transmute(original) };
    unsafe { original(layer, bit) };
    if ACTIVE.load(Ordering::Relaxed)
        && bit == LAYER_PLAYERS_BIT
        && layer as usize == OBSERVED_RIFT_LAYER.load(Ordering::Relaxed)
    {
        queue_dirty(DirtySource::RiftPlayers, layer as usize);
    }
}

fn queue_dirty(source: DirtySource, owner: usize) {
    if RAW_DIRTY
        .get()
        .is_none_or(|queue| queue.push(RawDirty { source, owner }).is_err())
    {
        RAW_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn drain_dirty() -> bool {
    let Some(queue) = RAW_DIRTY.get() else {
        return false;
    };
    let mut dirty = false;
    while let Some(raw) = queue.pop() {
        match raw.source {
            DirtySource::GroupSwitch => GROUP_SWITCHES.fetch_add(1, Ordering::Relaxed),
            DirtySource::GroupPlayers => GROUP_DIRTY_EDGES.fetch_add(1, Ordering::Relaxed),
            DirtySource::RiftPlayers => RIFT_DIRTY_EDGES.fetch_add(1, Ordering::Relaxed),
        };
        let source_matches = match raw.source {
            DirtySource::GroupSwitch => true,
            DirtySource::GroupPlayers => raw.owner == OBSERVED_GROUP.load(Ordering::Acquire),
            DirtySource::RiftPlayers => raw.owner == OBSERVED_RIFT_LAYER.load(Ordering::Acquire),
        };
        if source_matches {
            dirty = true;
        } else {
            FILTERED.fetch_add(1, Ordering::Relaxed);
        }
    }
    dirty
}

pub(crate) fn status() -> usize {
    HOOK_STATUS.load(Ordering::Acquire)
}

pub(crate) fn provider_available() -> bool {
    status() == 1 && crate::player_hooks::status() == 1
}

pub(crate) fn status_name(status: usize) -> &'static str {
    match status {
        0 => "waiting-for-roster-types",
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
    RAW_DROPS.load(Ordering::Acquire)
}

pub(crate) fn metrics() -> String {
    format!(
        "party_hooks={} party_provider={} party_group_switches={} party_group_dirty={} party_rift_dirty={} party_filtered={} party_raw_drops={}",
        status_name(status()),
        if provider_available() { "direct" } else { "sampled" },
        GROUP_SWITCHES.load(Ordering::Relaxed),
        GROUP_DIRTY_EDGES.load(Ordering::Relaxed),
        RIFT_DIRTY_EDGES.load(Ordering::Relaxed),
        FILTERED.load(Ordering::Relaxed),
        RAW_DROPS.load(Ordering::Relaxed),
    )
}

pub(crate) fn shutdown_hooks() {
    ACTIVE.store(false, Ordering::Release);
    for target in [
        SET_GROUP_TARGET.load(Ordering::Acquire),
        GROUP_DIRTY_TARGET.load(Ordering::Acquire),
        LAYER_DIRTY_TARGET.load(Ordering::Acquire),
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
    fn method_specs_match_verified_roster_boundaries() {
        assert_eq!(SET_GROUP.name, c"set_group");
        assert_eq!(GROUP_DIRTY.name, c"networkSetBitCond");
        assert_eq!(LAYER_DIRTY.name, c"networkSetBitCond");
        assert_eq!(GROUP_PLAYERS_BIT, 4);
        assert_eq!(LAYER_PLAYERS_BIT, 9);
    }
}
