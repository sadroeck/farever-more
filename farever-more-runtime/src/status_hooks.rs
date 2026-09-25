//! Internal shadow capture for local status-registry and stack changes.
//!
//! Status membership is an `ArrayProxyData` property on `GameObject`. Full
//! replacement goes through `set_statuses`; incremental add/remove marks bit
//! 11 through `networkSetBitCond`. Stack changes use the exact replicated
//! `Status.set_stacks` setter and do not necessarily dirty membership.

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
use std::sync::{Mutex, OnceLock};

const RAW_QUEUE_CAPACITY: usize = 512;
const MAX_DECODE_PER_TICK: usize = 128;
const MAX_STATUS_KIND_CODE_UNITS: usize = 128;
const STATUSES_PROPERTY_BIT: i32 = 11;

const SET_STATUSES_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ent.GameObject"),
    HashLinkTypeSpec::Object("hxbit.ArrayProxyData"),
];
const SET_STATUSES: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.GameObject",
    name: c"set_statuses",
    arguments: SET_STATUSES_ARGUMENTS,
    result: HashLinkTypeSpec::Object("hxbit.ArrayProxyData"),
};
const STATUS_DIRTY_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ent.GameObject"),
    HashLinkTypeSpec::Kind(HashLinkKind::I32),
];
const STATUS_DIRTY_BETA_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.BaseState"),
    HashLinkTypeSpec::Kind(HashLinkKind::I32),
];
const STATUS_DIRTY: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.GameObject",
    name: c"networkSetBitCond",
    arguments: STATUS_DIRTY_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const STATUS_DIRTY_BETA: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.GameObject",
    name: c"networkSetBitCond",
    arguments: STATUS_DIRTY_BETA_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const ADD_STACKS_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ent.GameObject"),
    HashLinkTypeSpec::Object("st.skill.Status"),
    HashLinkTypeSpec::Kind(HashLinkKind::I32),
    HashLinkTypeSpec::Object("st.skill.BaseSkill"),
    HashLinkTypeSpec::Nullable(HashLinkKind::F64),
];
const ADD_STACKS_ANCHOR: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.GameObject",
    name: c"addStacks",
    arguments: ADD_STACKS_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::I32),
};
const SET_STACKS_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.skill.Status"),
    HashLinkTypeSpec::Kind(HashLinkKind::I32),
];
const SET_STACKS: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.skill.Status",
    name: c"set_stacks",
    arguments: SET_STACKS_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::I32),
};
const HERO_FIELDS: &[HashLinkFieldSpec] = &[HashLinkFieldSpec::object(
    "statuses",
    "hxbit.ArrayProxyData",
)];
const HERO_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "ent.Hero",
    kind: HashLinkKind::Object,
    fields: HERO_FIELDS,
};
const STATUS_FIELDS: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::object("kind", "String"),
    HashLinkFieldSpec::object("owner", "ent.GameObject"),
    HashLinkFieldSpec::scalar("stacks", HashLinkKind::I32),
];
const STATUS_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.skill.Status",
    kind: HashLinkKind::Object,
    fields: STATUS_FIELDS,
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

type HlSetStatuses = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
type HlSetBit = unsafe extern "C" fn(*mut c_void, i32);
type HlSetStacks = unsafe extern "C" fn(*mut c_void, i32) -> i32;

#[derive(Clone, Copy, Debug)]
struct StatusLayout {
    hero_type: usize,
    hero_statuses: usize,
    status_type: usize,
    status_kind: usize,
    status_owner: usize,
    status_stacks: usize,
    string_type: usize,
    string_bytes: usize,
    string_length: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawKind {
    RegistryReplaced,
    RegistryDirty,
    StacksChanged,
}

#[derive(Clone, Copy, Debug)]
struct RawStatusChange {
    kind: RawKind,
    owner_pointer: usize,
    status_pointer: usize,
    old_stacks: i32,
    new_stacks: i32,
    status_kind_length: u16,
    status_kind: [u16; MAX_STATUS_KIND_CODE_UNITS],
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StatusSample {
    kind: RawKind,
    status_kind: Option<String>,
    old_stacks: Option<i32>,
    new_stacks: Option<i32>,
}

struct ResolvedHooks {
    registry: ValidatedHashLinkMethod,
    dirty: ValidatedHashLinkMethod,
    stacks: ValidatedHashLinkMethod,
}

static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static REGISTRY_TARGET: AtomicUsize = AtomicUsize::new(0);
static DIRTY_TARGET: AtomicUsize = AtomicUsize::new(0);
static STACKS_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_REGISTRY: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_DIRTY: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_STACKS: AtomicUsize = AtomicUsize::new(0);
static LAYOUT: OnceLock<StatusLayout> = OnceLock::new();
static RAW: OnceLock<ArrayQueue<RawStatusChange>> = OnceLock::new();
static LAST_SAMPLE: OnceLock<Mutex<Option<StatusSample>>> = OnceLock::new();

static REGISTRY_REPLACED: AtomicU64 = AtomicU64::new(0);
static REGISTRY_DIRTY: AtomicU64 = AtomicU64::new(0);
static STACKS_CHANGED: AtomicU64 = AtomicU64::new(0);
static UNCHANGED: AtomicU64 = AtomicU64::new(0);
static FILTERED: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static DROPS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn prepare_queue() {
    let _ = RAW.get_or_init(|| ArrayQueue::new(RAW_QUEUE_CAPACITY));
    let _ = LAST_SAMPLE.get_or_init(|| Mutex::new(None));
}

pub(crate) fn try_install_hooks(hl: &HashLink<'_>, profile: GameBuildProfile) {
    if HOOK_STATUS.load(Ordering::Acquire) != 0 {
        return;
    }
    let Some(hero_type) = crate::player_hooks::hero_type() else {
        return;
    };
    if HOOK_STATUS
        .compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let result = resolve_hooks(hl, hero_type, profile).and_then(|hooks| install_hooks(&hooks));
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
    hero_type: usize,
    profile: GameBuildProfile,
) -> Result<ResolvedHooks, String> {
    let game_object_type = hl
        .type_address_named(hero_type, "ent.GameObject")
        .ok_or_else(|| "ent.Hero does not inherit ent.GameObject".to_owned())?;
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    let registry = runtime.resolve_method(hl, game_object_type, &SET_STATUSES)?;
    let dirty_spec = if profile.is_beta() {
        &STATUS_DIRTY_BETA
    } else {
        &STATUS_DIRTY
    };
    let dirty = runtime.resolve_method(hl, game_object_type, dirty_spec)?;
    let anchor = runtime.resolve_method(hl, game_object_type, &ADD_STACKS_ANCHOR)?;
    let status_type = anchor
        .argument_type(1)
        .ok_or_else(|| "validated addStacks signature omitted Status".to_owned())?;
    let stacks = runtime.resolve_method(hl, status_type, &SET_STACKS)?;
    let hero = validate_object(hl, hero_type, &HERO_SCHEMA)?;
    let status = validate_object(hl, status_type, &STATUS_SCHEMA)?;
    let string_type = status
        .field_type_address("kind")
        .ok_or_else(|| "validated Status layout omitted kind type".to_owned())?;
    let string = validate_object(hl, string_type, &STRING_SCHEMA)?;
    LAYOUT
        .set(StatusLayout {
            hero_type: hero.type_address,
            hero_statuses: hero
                .offset("statuses")
                .ok_or_else(|| "validated Hero layout omitted statuses".to_owned())?,
            status_type: status.type_address,
            status_kind: status
                .offset("kind")
                .ok_or_else(|| "validated Status layout omitted kind".to_owned())?,
            status_owner: status
                .offset("owner")
                .ok_or_else(|| "validated Status layout omitted owner".to_owned())?,
            status_stacks: status
                .offset("stacks")
                .ok_or_else(|| "validated Status layout omitted stacks".to_owned())?,
            string_type: string.type_address,
            string_bytes: string
                .offset("bytes")
                .ok_or_else(|| "validated String layout omitted bytes".to_owned())?,
            string_length: string
                .offset("length")
                .ok_or_else(|| "validated String layout omitted length".to_owned())?,
        })
        .map_err(|_| "status hook layout was already initialized".to_owned())?;
    Ok(ResolvedHooks {
        registry,
        dirty,
        stacks,
    })
}

fn install_hooks(hooks: &ResolvedHooks) -> Result<(), String> {
    let targets = [
        hooks.registry.target() as *mut c_void,
        hooks.dirty.target() as *mut c_void,
        hooks.stacks.target() as *mut c_void,
    ];
    if targets[0] == targets[1] || targets[0] == targets[2] || targets[1] == targets[2] {
        return Err("status methods resolved to duplicate targets".to_owned());
    }
    let detours = [
        hook_set_statuses as *mut c_void,
        hook_status_dirty as *mut c_void,
        hook_set_stacks as *mut c_void,
    ];
    let originals = [&ORIGINAL_REGISTRY, &ORIGINAL_DIRTY, &ORIGINAL_STACKS];
    for index in 0..targets.len() {
        if let Err(error) = install_one(targets[index], detours[index], originals[index]) {
            for previous in 0..index {
                remove_one(targets[previous], originals[previous]);
            }
            return Err(error);
        }
    }
    REGISTRY_TARGET.store(targets[0] as usize, Ordering::Release);
    DIRTY_TARGET.store(targets[1] as usize, Ordering::Release);
    STACKS_TARGET.store(targets[2] as usize, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

fn install_one(
    target: *mut c_void,
    detour: *mut c_void,
    original: &AtomicUsize,
) -> Result<(), String> {
    // SAFETY: the caller exact-signature validated target and detour.
    let trampoline = std::panic::catch_unwind(|| unsafe { MinHook::create_hook(target, detour) })
        .map_err(|_| "MinHook initialization panicked for status hooks".to_owned())?
        .map_err(|status| format!("create status hook returned {status:?}"))?;
    original.store(trampoline as usize, Ordering::Release);
    // SAFETY: the hook was created immediately above.
    if let Err(status) = unsafe { MinHook::enable_hook(target) } {
        // SAFETY: target names the hook created immediately above.
        let _ = unsafe { MinHook::remove_hook(target) };
        original.store(0, Ordering::Release);
        return Err(format!("enable status hook returned {status:?}"));
    }
    Ok(())
}

fn remove_one(target: *mut c_void, original: &AtomicUsize) {
    // SAFETY: called only for a successfully installed target.
    let _ = unsafe { MinHook::disable_hook(target) };
    // SAFETY: called only for a successfully installed target.
    let _ = unsafe { MinHook::remove_hook(target) };
    original.store(0, Ordering::Release);
}

unsafe extern "C" fn hook_set_statuses(owner: *mut c_void, statuses: *mut c_void) -> *mut c_void {
    let before = if ACTIVE.load(Ordering::Relaxed) {
        unsafe { read_local_registry(owner) }
    } else {
        None
    };
    let original = ORIGINAL_REGISTRY.load(Ordering::Acquire);
    let result = if original == 0 {
        statuses
    } else {
        // SAFETY: trampoline matches the validated method.
        let original: HlSetStatuses = unsafe { std::mem::transmute(original) };
        unsafe { original(owner, statuses) }
    };
    if let Some(previous) = before {
        if previous != result as usize {
            queue(RawStatusChange {
                kind: RawKind::RegistryReplaced,
                owner_pointer: owner as usize,
                status_pointer: 0,
                old_stacks: 0,
                new_stacks: 0,
                status_kind_length: 0,
                status_kind: [0; MAX_STATUS_KIND_CODE_UNITS],
            });
        } else {
            UNCHANGED.fetch_add(1, Ordering::Relaxed);
        }
    }
    result
}

unsafe extern "C" fn hook_status_dirty(owner: *mut c_void, bit: i32) {
    let original = ORIGINAL_DIRTY.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: trampoline matches the validated method.
        let original: HlSetBit = unsafe { std::mem::transmute(original) };
        unsafe { original(owner, bit) };
    }
    if ACTIVE.load(Ordering::Relaxed)
        && bit == STATUSES_PROPERTY_BIT
        && owner as usize == crate::player_hooks::local_hero_pointer()
    {
        queue(RawStatusChange {
            kind: RawKind::RegistryDirty,
            owner_pointer: owner as usize,
            status_pointer: 0,
            old_stacks: 0,
            new_stacks: 0,
            status_kind_length: 0,
            status_kind: [0; MAX_STATUS_KIND_CODE_UNITS],
        });
    }
}

unsafe extern "C" fn hook_set_stacks(status: *mut c_void, stacks: i32) -> i32 {
    let observation = if ACTIVE.load(Ordering::Relaxed) {
        unsafe { copy_stack_change(status, stacks) }
    } else {
        None
    };
    let original = ORIGINAL_STACKS.load(Ordering::Acquire);
    let result = if original == 0 {
        stacks
    } else {
        // SAFETY: trampoline matches the validated method.
        let original: HlSetStacks = unsafe { std::mem::transmute(original) };
        unsafe { original(status, stacks) }
    };
    if let Some(mut observation) = observation {
        observation.new_stacks = result;
        if observation.old_stacks != observation.new_stacks {
            queue(observation);
        } else {
            UNCHANGED.fetch_add(1, Ordering::Relaxed);
        }
    }
    result
}

unsafe fn read_local_registry(owner: *mut c_void) -> Option<usize> {
    if owner.is_null() || owner as usize != crate::player_hooks::local_hero_pointer() {
        FILTERED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let layout = LAYOUT.get().copied()?;
    if !unsafe { object_has_exact_type(owner, layout.hero_type) } {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    Some(unsafe {
        std::ptr::read_unaligned(owner.cast::<u8>().add(layout.hero_statuses).cast::<usize>())
    })
}

unsafe fn copy_stack_change(status: *mut c_void, requested: i32) -> Option<RawStatusChange> {
    let Some(layout) = LAYOUT.get().copied() else {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if !unsafe { object_has_exact_type(status, layout.status_type) } {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let base = status.cast::<u8>();
    let owner = unsafe { std::ptr::read_unaligned(base.add(layout.status_owner).cast::<usize>()) };
    if owner != crate::player_hooks::local_hero_pointer() {
        FILTERED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let old_stacks =
        unsafe { std::ptr::read_unaligned(base.add(layout.status_stacks).cast::<i32>()) };
    let kind = unsafe { std::ptr::read_unaligned(base.add(layout.status_kind).cast::<usize>()) };
    if !unsafe { object_has_exact_type(kind as *const c_void, layout.string_type) } {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let string = kind as *const u8;
    let length =
        unsafe { std::ptr::read_unaligned(string.add(layout.string_length).cast::<i32>()) };
    if !(1..=MAX_STATUS_KIND_CODE_UNITS as i32).contains(&length) {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let bytes =
        unsafe { std::ptr::read_unaligned(string.add(layout.string_bytes).cast::<usize>()) };
    if bytes < 0x1_0000 {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut raw = RawStatusChange {
        kind: RawKind::StacksChanged,
        owner_pointer: owner,
        status_pointer: status as usize,
        old_stacks,
        new_stacks: requested,
        status_kind_length: length as u16,
        status_kind: [0; MAX_STATUS_KIND_CODE_UNITS],
    };
    // SAFETY: String layout and positive length are validated and bounded.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes as *const u16,
            raw.status_kind.as_mut_ptr(),
            length as usize,
        );
    }
    Some(raw)
}

fn queue(raw: RawStatusChange) {
    if RAW.get().is_none_or(|queue| queue.push(raw).is_err()) {
        DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn decode_pending() {
    let Some(queue) = RAW.get() else {
        return;
    };
    for _ in 0..MAX_DECODE_PER_TICK {
        let Some(raw) = queue.pop() else {
            break;
        };
        if raw.owner_pointer != crate::player_hooks::local_hero_pointer() {
            FILTERED.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let Some(sample) = decode_sample(&raw) else {
            INVALID.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        match sample.kind {
            RawKind::RegistryReplaced => REGISTRY_REPLACED.fetch_add(1, Ordering::Relaxed),
            RawKind::RegistryDirty => REGISTRY_DIRTY.fetch_add(1, Ordering::Relaxed),
            RawKind::StacksChanged => STACKS_CHANGED.fetch_add(1, Ordering::Relaxed),
        };
        if let Some(last) = LAST_SAMPLE.get() {
            if let Ok(mut last) = last.lock() {
                *last = Some(sample);
            }
        }
    }
}

fn decode_sample(raw: &RawStatusChange) -> Option<StatusSample> {
    if raw.owner_pointer < 0x1_0000 {
        return None;
    }
    match raw.kind {
        RawKind::RegistryReplaced | RawKind::RegistryDirty => Some(StatusSample {
            kind: raw.kind,
            status_kind: None,
            old_stacks: None,
            new_stacks: None,
        }),
        RawKind::StacksChanged => {
            if raw.status_pointer < 0x1_0000 {
                return None;
            }
            let length = usize::from(raw.status_kind_length);
            if length == 0 || length > raw.status_kind.len() {
                return None;
            }
            Some(StatusSample {
                kind: raw.kind,
                status_kind: Some(String::from_utf16(&raw.status_kind[..length]).ok()?),
                old_stacks: Some(raw.old_stacks),
                new_stacks: Some(raw.new_stacks),
            })
        }
    }
}

pub(crate) fn status() -> usize {
    HOOK_STATUS.load(Ordering::Acquire)
}

pub(crate) fn status_name(status: usize) -> &'static str {
    match status {
        0 => "waiting-for-hero-metadata",
        1 => "active-shadow",
        3 => "failed",
        4 => "installing",
        _ => "unknown",
    }
}

pub(crate) fn error() -> Option<&'static str> {
    HOOK_ERROR.get().map(String::as_str)
}

pub(crate) fn metrics() -> String {
    let last = LAST_SAMPLE
        .get()
        .and_then(|last| last.lock().ok()?.clone())
        .map_or_else(
            || "status_last=unknown".to_owned(),
            |sample| match sample.kind {
                RawKind::RegistryReplaced => "status_last=registry-replaced".to_owned(),
                RawKind::RegistryDirty => "status_last=registry-dirty".to_owned(),
                RawKind::StacksChanged => format!(
                    "status_last=stacks:{}:{}->{}",
                    sample
                        .status_kind
                        .as_deref()
                        .map_or_else(|| "unknown".to_owned(), metric_token),
                    sample.old_stacks.unwrap_or_default(),
                    sample.new_stacks.unwrap_or_default(),
                ),
            },
        );
    format!(
        "status_hooks={} status_mode=shadow-only status_registry_replaced={} status_registry_dirty={} status_stacks_changed={} status_unchanged={} status_filtered={} status_invalid={} status_queue_drops={} {}",
        status_name(status()),
        REGISTRY_REPLACED.load(Ordering::Relaxed),
        REGISTRY_DIRTY.load(Ordering::Relaxed),
        STACKS_CHANGED.load(Ordering::Relaxed),
        UNCHANGED.load(Ordering::Relaxed),
        FILTERED.load(Ordering::Relaxed),
        INVALID.load(Ordering::Relaxed),
        DROPS.load(Ordering::Relaxed),
        last,
    )
}

fn metric_token(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn shutdown_hooks() {
    ACTIVE.store(false, Ordering::Release);
    for target in [&REGISTRY_TARGET, &DIRTY_TARGET, &STACKS_TARGET] {
        let target = target.load(Ordering::Acquire);
        if target != 0 {
            // SAFETY: each target is published only after successful enablement.
            let _ = unsafe { MinHook::disable_hook(target as *mut c_void) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stacks(kind: &str, old: i32, new: i32) -> RawStatusChange {
        let kind = kind.encode_utf16().collect::<Vec<_>>();
        let mut raw = RawStatusChange {
            kind: RawKind::StacksChanged,
            owner_pointer: 0x10_000,
            status_pointer: 0x20_000,
            old_stacks: old,
            new_stacks: new,
            status_kind_length: kind.len() as u16,
            status_kind: [0; MAX_STATUS_KIND_CODE_UNITS],
        };
        raw.status_kind[..kind.len()].copy_from_slice(&kind);
        raw
    }

    #[test]
    fn method_specs_match_verified_status_boundaries() {
        assert_eq!(SET_STATUSES.name, c"set_statuses");
        assert_eq!(STATUS_DIRTY.name, c"networkSetBitCond");
        assert_eq!(SET_STACKS.name, c"set_stacks");
        assert_eq!(STATUSES_PROPERTY_BIT, 11);
    }

    #[test]
    fn decoder_preserves_status_kind_and_stack_delta() {
        assert_eq!(
            decode_sample(&stacks("Status_Burning", 1, 2)),
            Some(StatusSample {
                kind: RawKind::StacksChanged,
                status_kind: Some("Status_Burning".to_owned()),
                old_stacks: Some(1),
                new_stacks: Some(2),
            })
        );
    }
}
