//! Hook-driven local combat membership and combat-state provider.
//!
//! The exact replicated-property setters are build/signature/layout gated.
//! Callbacks compare the validated field before and after the original call,
//! filter to the hook-maintained local Hero, and enqueue fixed-size records.

use crate::hashlink::{
    validate_object, HashLink, HashLinkFieldSpec, HashLinkKind, HashLinkMethodSpec,
    HashLinkObjectSpec, HashLinkRuntime, HashLinkTypeSpec, ValidatedHashLinkMethod,
};
use crossbeam_queue::ArrayQueue;
use minhook::MinHook;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

const RAW_QUEUE_CAPACITY: usize = 256;
const COMBAT_EDGE_QUEUE_CAPACITY: usize = 128;
const MAX_DECODE_PER_TICK: usize = 64;

const HERO_FIELDS: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::scalar("target", HashLinkKind::I64),
    HashLinkFieldSpec::scalar("lockedTarget", HashLinkKind::I64),
    HashLinkFieldSpec::scalar("autoTarget", HashLinkKind::I64),
    HashLinkFieldSpec::scalar("isInCombat", HashLinkKind::Bool),
];
const HERO_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "ent.Hero",
    kind: HashLinkKind::Object,
    fields: HERO_FIELDS,
};
const UNIT_I64_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ent.Unit"),
    HashLinkTypeSpec::Kind(HashLinkKind::I64),
];
const HERO_I64_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ent.Hero"),
    HashLinkTypeSpec::Kind(HashLinkKind::I64),
];
const UNIT_BOOL_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ent.Unit"),
    HashLinkTypeSpec::Kind(HashLinkKind::Bool),
];

const SET_TARGET: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.Unit",
    name: c"set_target",
    arguments: UNIT_I64_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::I64),
};
const SET_LOCKED_TARGET: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.Hero",
    name: c"set_lockedTarget",
    arguments: HERO_I64_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::I64),
};
const SET_AUTO_TARGET: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.Hero",
    name: c"set_autoTarget",
    arguments: HERO_I64_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::I64),
};
const SET_IN_COMBAT: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.Unit",
    name: c"set_isInCombat",
    arguments: UNIT_BOOL_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Bool),
};

type HlSetI64 = unsafe extern "C" fn(*mut c_void, i64) -> i64;
type HlSetBool = unsafe extern "C" fn(*mut c_void, bool) -> bool;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LocalCombatState {
    pub(crate) hero: usize,
    pub(crate) references: [Option<usize>; 3],
    pub(crate) in_combat: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawKind {
    Target,
    LockedTarget,
    AutoTarget,
    Combat,
}

#[derive(Clone, Copy, Debug)]
struct RawState {
    sequence: u64,
    kind: RawKind,
    hero: usize,
    value: i64,
}

#[derive(Clone, Copy, Debug)]
struct CombatLayout {
    target: usize,
    locked_target: usize,
    auto_target: usize,
    in_combat: usize,
}

#[derive(Clone, Copy, Debug)]
struct Reconciliation {
    through_sequence: u64,
    state: LocalCombatState,
}

struct ResolvedHooks {
    target: ValidatedHashLinkMethod,
    locked_target: ValidatedHashLinkMethod,
    auto_target: ValidatedHashLinkMethod,
    combat: ValidatedHashLinkMethod,
}

// 0 = waiting for Hero type, 1 = active, 3 = failed, 4 = installing.
static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);

static TARGET_TARGET: AtomicUsize = AtomicUsize::new(0);
static LOCKED_TARGET: AtomicUsize = AtomicUsize::new(0);
static AUTO_TARGET: AtomicUsize = AtomicUsize::new(0);
static COMBAT_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_LOCKED: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_AUTO: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_COMBAT: AtomicUsize = AtomicUsize::new(0);
static LAYOUT: OnceLock<CombatLayout> = OnceLock::new();

static RAW: OnceLock<ArrayQueue<RawState>> = OnceLock::new();
static COMBAT_EDGES: OnceLock<ArrayQueue<bool>> = OnceLock::new();
static RECONCILIATION: OnceLock<Mutex<Option<Reconciliation>>> = OnceLock::new();
static RAW_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static DISCARD_THROUGH: AtomicU64 = AtomicU64::new(0);

// One observer worker writes this seqlock-backed tuple.
static STATE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static STATE_HERO: AtomicUsize = AtomicUsize::new(0);
static STATE_TARGET: AtomicUsize = AtomicUsize::new(0);
static STATE_LOCKED: AtomicUsize = AtomicUsize::new(0);
static STATE_AUTO: AtomicUsize = AtomicUsize::new(0);
// 0 = unavailable, 1 = false, 2 = true.
static STATE_COMBAT: AtomicUsize = AtomicUsize::new(0);

static RAW_DROPS: AtomicU64 = AtomicU64::new(0);
static EDGE_DROPS: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static FILTERED: AtomicU64 = AtomicU64::new(0);
static DUPLICATES: AtomicU64 = AtomicU64::new(0);
static TARGET_EDGES: AtomicU64 = AtomicU64::new(0);
static COMBAT_EDGES_PUBLISHED: AtomicU64 = AtomicU64::new(0);
static RECONCILES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn prepare_queues() {
    let _ = RAW.get_or_init(|| ArrayQueue::new(RAW_QUEUE_CAPACITY));
    let _ = COMBAT_EDGES.get_or_init(|| ArrayQueue::new(COMBAT_EDGE_QUEUE_CAPACITY));
    let _ = RECONCILIATION.get_or_init(|| Mutex::new(None));
}

pub(crate) fn try_install_hooks(hl: &HashLink<'_>) {
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
    let result = resolve_hooks(hl, hero_type).and_then(|hooks| install_hooks(&hooks));
    match result {
        Ok(()) => HOOK_STATUS.store(1, Ordering::Release),
        Err(error) => {
            let _ = HOOK_ERROR.set(error);
            HOOK_STATUS.store(3, Ordering::Release);
        }
    }
}

fn resolve_hooks(hl: &HashLink<'_>, hero_type: usize) -> Result<ResolvedHooks, String> {
    let unit_type = hl
        .type_address_named(hero_type, "ent.Unit")
        .ok_or_else(|| "ent.Hero does not inherit the expected ent.Unit type".to_owned())?;
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    let target = runtime.resolve_method(hl, unit_type, &SET_TARGET)?;
    let locked_target = runtime.resolve_method(hl, hero_type, &SET_LOCKED_TARGET)?;
    let auto_target = runtime.resolve_method(hl, hero_type, &SET_AUTO_TARGET)?;
    let combat = runtime.resolve_method(hl, unit_type, &SET_IN_COMBAT)?;
    let hero = validate_object(hl, hero_type, &HERO_SCHEMA)?;
    let offset = |name| {
        hero.offset(name)
            .ok_or_else(|| format!("validated Hero layout omitted {name}"))
    };
    LAYOUT
        .set(CombatLayout {
            target: offset("target")?,
            locked_target: offset("lockedTarget")?,
            auto_target: offset("autoTarget")?,
            in_combat: offset("isInCombat")?,
        })
        .map_err(|_| "combat hook layout was already initialized".to_owned())?;
    Ok(ResolvedHooks {
        target,
        locked_target,
        auto_target,
        combat,
    })
}

fn install_hooks(hooks: &ResolvedHooks) -> Result<(), String> {
    let targets = [
        hooks.target.target() as *mut c_void,
        hooks.locked_target.target() as *mut c_void,
        hooks.auto_target.target() as *mut c_void,
        hooks.combat.target() as *mut c_void,
    ];
    for index in 0..targets.len() {
        if targets[..index].contains(&targets[index]) {
            return Err("combat setters resolved to duplicate targets".to_owned());
        }
    }
    let detours = [
        hook_target as *mut c_void,
        hook_locked_target as *mut c_void,
        hook_auto_target as *mut c_void,
        hook_combat as *mut c_void,
    ];
    let names = [
        "set_target",
        "set_lockedTarget",
        "set_autoTarget",
        "set_isInCombat",
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
    ORIGINAL_TARGET.store(originals[0], Ordering::Release);
    ORIGINAL_LOCKED.store(originals[1], Ordering::Release);
    ORIGINAL_AUTO.store(originals[2], Ordering::Release);
    ORIGINAL_COMBAT.store(originals[3], Ordering::Release);
    for index in 0..targets.len() {
        // SAFETY: every exact setter ABI was validated before hook creation.
        if let Err(status) = unsafe { MinHook::enable_hook(targets[index]) } {
            for target in targets[..index].iter().copied() {
                // SAFETY: only the successfully enabled prefix is disabled.
                let _ = unsafe { MinHook::disable_hook(target) };
            }
            for target in targets {
                remove_hook(target);
            }
            ORIGINAL_TARGET.store(0, Ordering::Release);
            ORIGINAL_LOCKED.store(0, Ordering::Release);
            ORIGINAL_AUTO.store(0, Ordering::Release);
            ORIGINAL_COMBAT.store(0, Ordering::Release);
            return Err(format!("enable {} hook returned {status:?}", names[index]));
        }
    }
    TARGET_TARGET.store(targets[0] as usize, Ordering::Release);
    LOCKED_TARGET.store(targets[1] as usize, Ordering::Release);
    AUTO_TARGET.store(targets[2] as usize, Ordering::Release);
    COMBAT_TARGET.store(targets[3] as usize, Ordering::Release);
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

unsafe extern "C" fn hook_target(hero: *mut c_void, requested: i64) -> i64 {
    unsafe {
        hook_i64(
            hero,
            requested,
            RawKind::Target,
            ORIGINAL_TARGET.load(Ordering::Acquire),
        )
    }
}

unsafe extern "C" fn hook_locked_target(hero: *mut c_void, requested: i64) -> i64 {
    unsafe {
        hook_i64(
            hero,
            requested,
            RawKind::LockedTarget,
            ORIGINAL_LOCKED.load(Ordering::Acquire),
        )
    }
}

unsafe extern "C" fn hook_auto_target(hero: *mut c_void, requested: i64) -> i64 {
    unsafe {
        hook_i64(
            hero,
            requested,
            RawKind::AutoTarget,
            ORIGINAL_AUTO.load(Ordering::Acquire),
        )
    }
}

unsafe fn hook_i64(hero: *mut c_void, requested: i64, kind: RawKind, original: usize) -> i64 {
    if original == 0 {
        return requested;
    }
    let local = ACTIVE.load(Ordering::Relaxed)
        && hero as usize == crate::player_hooks::local_hero_pointer();
    let offset = LAYOUT.get().map(|layout| match kind {
        RawKind::Target => layout.target,
        RawKind::LockedTarget => layout.locked_target,
        RawKind::AutoTarget => layout.auto_target,
        RawKind::Combat => unreachable!(),
    });
    let before = if local {
        offset.map(|offset| unsafe {
            std::ptr::read_unaligned(hero.cast::<u8>().add(offset).cast::<i64>())
        })
    } else {
        None
    };
    // SAFETY: MinHook returned this trampoline for the validated setter ABI.
    let original: HlSetI64 = unsafe { std::mem::transmute(original) };
    let result = unsafe { original(hero, requested) };
    if let (Some(before), Some(offset)) = (before, offset) {
        // SAFETY: the local Hero is still the live callback receiver and the
        // offset belongs to its validated concrete layout.
        let after =
            unsafe { std::ptr::read_unaligned(hero.cast::<u8>().add(offset).cast::<i64>()) };
        if before != after {
            queue_raw(RawState {
                sequence: 0,
                kind,
                hero: hero as usize,
                value: after,
            });
        } else {
            DUPLICATES.fetch_add(1, Ordering::Relaxed);
        }
    } else if ACTIVE.load(Ordering::Relaxed) {
        FILTERED.fetch_add(1, Ordering::Relaxed);
    }
    result
}

unsafe extern "C" fn hook_combat(hero: *mut c_void, requested: bool) -> bool {
    let original = ORIGINAL_COMBAT.load(Ordering::Acquire);
    if original == 0 {
        return requested;
    }
    let local = ACTIVE.load(Ordering::Relaxed)
        && hero as usize == crate::player_hooks::local_hero_pointer();
    let offset = LAYOUT.get().map(|layout| layout.in_combat);
    let before = if local {
        offset.map(|offset| unsafe { std::ptr::read_unaligned(hero.cast::<u8>().add(offset)) })
    } else {
        None
    };
    // SAFETY: MinHook returned this trampoline for `(ent.Unit, bool) -> bool`.
    let original: HlSetBool = unsafe { std::mem::transmute(original) };
    let result = unsafe { original(hero, requested) };
    if let (Some(before), Some(offset)) = (before, offset) {
        // SAFETY: the offset belongs to the validated local Hero layout.
        let after = unsafe { std::ptr::read_unaligned(hero.cast::<u8>().add(offset)) };
        if before <= 1 && after <= 1 && before != after {
            queue_raw(RawState {
                sequence: 0,
                kind: RawKind::Combat,
                hero: hero as usize,
                value: i64::from(after),
            });
        } else if before == after {
            DUPLICATES.fetch_add(1, Ordering::Relaxed);
        } else {
            INVALID.fetch_add(1, Ordering::Relaxed);
        }
    } else if ACTIVE.load(Ordering::Relaxed) {
        FILTERED.fetch_add(1, Ordering::Relaxed);
    }
    result
}

fn queue_raw(mut raw: RawState) {
    raw.sequence = RAW_SEQUENCE.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    if RAW.get().is_none_or(|queue| queue.push(raw).is_err()) {
        RAW_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn decode_pending() {
    if let Some(reconciliation) = take_reconciliation() {
        DISCARD_THROUGH.fetch_max(reconciliation.through_sequence, Ordering::AcqRel);
        publish_state(reconciliation.state);
        RECONCILES.fetch_add(1, Ordering::Relaxed);
    }
    let Some(queue) = RAW.get() else {
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
        let Some(mut state) = current().filter(|state| state.hero == raw.hero) else {
            FILTERED.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        match raw.kind {
            RawKind::Target | RawKind::LockedTarget | RawKind::AutoTarget => {
                let Some(value) = reference_value(raw.value) else {
                    INVALID.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                let index = match raw.kind {
                    RawKind::Target => 0,
                    RawKind::LockedTarget => 1,
                    RawKind::AutoTarget => 2,
                    RawKind::Combat => unreachable!(),
                };
                state.references[index] = value;
                publish_state(state);
                TARGET_EDGES.fetch_add(1, Ordering::Relaxed);
            }
            RawKind::Combat if (0..=1).contains(&raw.value) => {
                let active = raw.value == 1;
                state.in_combat = Some(active);
                publish_state(state);
                if COMBAT_EDGES
                    .get()
                    .is_none_or(|queue| queue.push(active).is_err())
                {
                    EDGE_DROPS.fetch_add(1, Ordering::Relaxed);
                } else {
                    COMBAT_EDGES_PUBLISHED.fetch_add(1, Ordering::Relaxed);
                }
            }
            RawKind::Combat => {
                INVALID.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn reference_value(value: i64) -> Option<Option<usize>> {
    let value = usize::try_from(value).ok()?;
    if value == 0 {
        Some(None)
    } else if value >= 0x1_0000 {
        Some(Some(value))
    } else {
        None
    }
}

fn publish_state(state: LocalCombatState) {
    STATE_SEQUENCE.fetch_add(1, Ordering::AcqRel);
    STATE_HERO.store(state.hero, Ordering::Relaxed);
    STATE_TARGET.store(state.references[0].unwrap_or_default(), Ordering::Relaxed);
    STATE_LOCKED.store(state.references[1].unwrap_or_default(), Ordering::Relaxed);
    STATE_AUTO.store(state.references[2].unwrap_or_default(), Ordering::Relaxed);
    STATE_COMBAT.store(
        state
            .in_combat
            .map_or(0, |active| if active { 2 } else { 1 }),
        Ordering::Relaxed,
    );
    STATE_SEQUENCE.fetch_add(1, Ordering::Release);
}

pub(crate) fn current() -> Option<LocalCombatState> {
    for _ in 0..8 {
        let before = STATE_SEQUENCE.load(Ordering::Acquire);
        if before & 1 != 0 {
            std::hint::spin_loop();
            continue;
        }
        let hero = STATE_HERO.load(Ordering::Relaxed);
        let references = [
            STATE_TARGET.load(Ordering::Relaxed),
            STATE_LOCKED.load(Ordering::Relaxed),
            STATE_AUTO.load(Ordering::Relaxed),
        ]
        .map(|value| (value >= 0x1_0000).then_some(value));
        let in_combat = match STATE_COMBAT.load(Ordering::Relaxed) {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        };
        let after = STATE_SEQUENCE.load(Ordering::Acquire);
        if before == after {
            return (hero >= 0x1_0000).then_some(LocalCombatState {
                hero,
                references,
                in_combat,
            });
        }
    }
    None
}

pub(crate) fn capture_sequence() -> u64 {
    RAW_SEQUENCE.load(Ordering::Acquire)
}

pub(crate) fn reconcile(state: LocalCombatState, through_sequence: u64) {
    let slot = RECONCILIATION.get_or_init(|| Mutex::new(None));
    if let Ok(mut pending) = slot.lock() {
        *pending = Some(Reconciliation {
            through_sequence,
            state,
        });
    }
}

fn take_reconciliation() -> Option<Reconciliation> {
    RECONCILIATION
        .get()
        .and_then(|slot| slot.lock().ok()?.take())
}

pub(crate) fn drain_combat_edges() -> Vec<bool> {
    let Some(queue) = COMBAT_EDGES.get() else {
        return Vec::new();
    };
    let mut edges = Vec::new();
    while let Some(edge) = queue.pop() {
        edges.push(edge);
    }
    edges
}

pub(crate) fn status() -> usize {
    HOOK_STATUS.load(Ordering::Acquire)
}

pub(crate) fn hooks_available() -> bool {
    status() == 1 && crate::player_hooks::status() == 1
}

pub(crate) fn provider_available() -> bool {
    let local_hero = crate::player_hooks::local_hero_pointer();
    hooks_available() && provider_state_matches_local_hero(current(), local_hero)
}

fn provider_state_matches_local_hero(state: Option<LocalCombatState>, local_hero: usize) -> bool {
    local_hero >= 0x1_0000 && state.is_some_and(|state| state.hero == local_hero)
}

pub(crate) fn provider_status() -> usize {
    if hooks_available() {
        1
    } else if status() == 3 || crate::player_hooks::status() == 3 {
        3
    } else {
        0
    }
}

pub(crate) fn status_name(status: usize) -> &'static str {
    match status {
        0 => "waiting-for-local-hero-type",
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
        "combat_setter_hooks={} combat_setter_provider={} target_edges={} combat_edges={} combat_reconciles={} combat_duplicates={} combat_filtered={} combat_invalid={} combat_raw_drops={} combat_edge_drops={}",
        status_name(status()),
        if provider_available() { "direct" } else { "unavailable" },
        TARGET_EDGES.load(Ordering::Relaxed),
        COMBAT_EDGES_PUBLISHED.load(Ordering::Relaxed),
        RECONCILES.load(Ordering::Relaxed),
        DUPLICATES.load(Ordering::Relaxed),
        FILTERED.load(Ordering::Relaxed),
        INVALID.load(Ordering::Relaxed),
        RAW_DROPS.load(Ordering::Relaxed),
        EDGE_DROPS.load(Ordering::Relaxed),
    )
}

pub(crate) fn shutdown_hooks() {
    ACTIVE.store(false, Ordering::Release);
    for target in [
        TARGET_TARGET.load(Ordering::Acquire),
        LOCKED_TARGET.load(Ordering::Acquire),
        AUTO_TARGET.load(Ordering::Acquire),
        COMBAT_TARGET.load(Ordering::Acquire),
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
    fn method_specs_match_verified_setters() {
        assert_eq!(SET_TARGET.name, c"set_target");
        assert_eq!(SET_LOCKED_TARGET.name, c"set_lockedTarget");
        assert_eq!(SET_AUTO_TARGET.name, c"set_autoTarget");
        assert_eq!(SET_IN_COMBAT.name, c"set_isInCombat");
    }

    #[test]
    fn reference_values_accept_only_null_or_pointer_sized_identities() {
        assert_eq!(reference_value(0), Some(None));
        assert_eq!(reference_value(0x10_000), Some(Some(0x10_000)));
        assert_eq!(reference_value(42), None);
        assert_eq!(reference_value(-1), None);
    }

    #[test]
    fn provider_state_must_match_the_current_local_hero() {
        let state = LocalCombatState {
            hero: 0x20_000,
            references: [None; 3],
            in_combat: Some(false),
        };

        assert!(provider_state_matches_local_hero(Some(state), 0x20_000));
        assert!(!provider_state_matches_local_hero(Some(state), 0x30_000));
        assert!(!provider_state_matches_local_hero(Some(state), 0));
        assert!(!provider_state_matches_local_hero(None, 0x20_000));
    }
}
