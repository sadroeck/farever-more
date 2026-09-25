//! Internal shadow capture for replicated shield-amount changes.
//!
//! Shielding is not part of the public add-on contract yet. The hook observes
//! the exact replicated `Status.shieldAmount` setter and leaves refresh,
//! replacement, source-attribution, and publication semantics to live QA.

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
const MAX_DECODE_PER_TICK: usize = 64;
const MAX_SKILL_ID_CODE_UNITS: usize = 128;
const MAX_ABSOLUTE_AMOUNT: f64 = 100_000_000.0;

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

const SET_SHIELD_AMOUNT_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.skill.Status"),
    HashLinkTypeSpec::Kind(HashLinkKind::F64),
];
const SET_SHIELD_AMOUNT: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.skill.Status",
    name: c"set_shieldAmount",
    arguments: SET_SHIELD_AMOUNT_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::F64),
};

const STATUS_FIELDS: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::object("kind", "String"),
    HashLinkFieldSpec::object("owner", "ent.GameObject"),
    HashLinkFieldSpec::object("instigatorSkill", "st.skill.BaseSkill"),
    HashLinkFieldSpec::object("instigator", "ent.GameObject"),
    HashLinkFieldSpec::scalar("shieldAmount", HashLinkKind::F64),
];
const STATUS_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.skill.Status",
    kind: HashLinkKind::Object,
    fields: STATUS_FIELDS,
};
const HERO_FIELDS: &[HashLinkFieldSpec] = &[HashLinkFieldSpec::object("ownerPlayer", "st.Player")];
const HERO_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "ent.Hero",
    kind: HashLinkKind::Object,
    fields: HERO_FIELDS,
};
const BASE_SKILL_FIELDS: &[HashLinkFieldSpec] = &[HashLinkFieldSpec::object("kind", "String")];
const BASE_SKILL_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.skill.BaseSkill",
    kind: HashLinkKind::Object,
    fields: BASE_SKILL_FIELDS,
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

type HlSetShieldAmount = unsafe extern "C" fn(*mut c_void, f64) -> f64;

#[derive(Clone, Copy, Debug)]
struct ShieldLayout {
    hero_type: usize,
    hero_owner_player: usize,
    status_kind: usize,
    status_owner: usize,
    instigator_skill: usize,
    instigator: usize,
    shield_amount: usize,
    base_skill_kind: usize,
    string_type: usize,
    string_bytes: usize,
    string_length: usize,
}

#[derive(Clone, Copy, Debug)]
struct RawShieldChange {
    target_pointer: usize,
    source_pointer: usize,
    source_owner_pointer: usize,
    old_amount: f64,
    new_amount: f64,
    status_id_length: u16,
    status_id: [u16; MAX_SKILL_ID_CODE_UNITS],
    source_skill_id_length: u16,
    source_skill_id: [u16; MAX_SKILL_ID_CODE_UNITS],
}

#[derive(Clone, Debug, PartialEq)]
struct ShieldSample {
    status_id: Option<String>,
    source_skill_id: Option<String>,
    old_amount: f64,
    new_amount: f64,
    delta: f64,
    route: &'static str,
}

#[derive(Default)]
pub(crate) struct ShieldHookDecoder;

// 0 = waiting for Hero/runtime metadata, 1 = active shadow capture,
// 3 = failed, 4 = installing.
static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_SET_SHIELD_AMOUNT: AtomicUsize = AtomicUsize::new(0);
static LAYOUT: OnceLock<ShieldLayout> = OnceLock::new();
static RAW: OnceLock<ArrayQueue<RawShieldChange>> = OnceLock::new();
static LAST_SAMPLE: OnceLock<Mutex<Option<ShieldSample>>> = OnceLock::new();

static OBSERVED: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static DROPS: AtomicU64 = AtomicU64::new(0);
static INCREASES: AtomicU64 = AtomicU64::new(0);
static DECREASES: AtomicU64 = AtomicU64::new(0);
static UNCHANGED: AtomicU64 = AtomicU64::new(0);
static INITIAL_APPLICATIONS: AtomicU64 = AtomicU64::new(0);
static REFRESH_INCREASES: AtomicU64 = AtomicU64::new(0);
static CLEARED: AtomicU64 = AtomicU64::new(0);
static SOURCE_MISSING: AtomicU64 = AtomicU64::new(0);
static TARGET_MISSING: AtomicU64 = AtomicU64::new(0);
static LOCAL_SOURCE: AtomicU64 = AtomicU64::new(0);
static LOCAL_TARGET: AtomicU64 = AtomicU64::new(0);
static SELF_SHIELD: AtomicU64 = AtomicU64::new(0);
static STATUS_ID_MISSING: AtomicU64 = AtomicU64::new(0);
static SOURCE_SKILL_MISSING: AtomicU64 = AtomicU64::new(0);

pub(crate) fn prepare_queue() {
    let _ = RAW.get_or_init(|| ArrayQueue::new(RAW_QUEUE_CAPACITY));
    let _ = LAST_SAMPLE.get_or_init(|| Mutex::new(None));
}

pub(crate) fn try_install_hook(hl: &HashLink<'_>) {
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

    let result = resolve_hook(hl, hero_type).and_then(install_hook);
    match result {
        Ok(()) => HOOK_STATUS.store(1, Ordering::Release),
        Err(error) => {
            let _ = HOOK_ERROR.set(error);
            HOOK_STATUS.store(3, Ordering::Release);
        }
    }
}

fn resolve_hook(hl: &HashLink<'_>, hero_type: usize) -> Result<ValidatedHashLinkMethod, String> {
    let game_object_type = hl
        .type_address_named(hero_type, "ent.GameObject")
        .ok_or_else(|| "ent.Hero does not inherit the expected ent.GameObject type".to_owned())?;
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    // `addStacks` is an exact-signature metadata anchor reachable from the
    // already validated Hero hierarchy. Its Status argument gives us the live
    // runtime type without waiting for or scanning a Status allocation.
    let anchor = runtime.resolve_method(hl, game_object_type, &ADD_STACKS_ANCHOR)?;
    let status_type = anchor
        .argument_type(1)
        .ok_or_else(|| "validated addStacks signature omitted Status".to_owned())?;
    let method = runtime.resolve_method(hl, status_type, &SET_SHIELD_AMOUNT)?;
    let layout = resolve_layout(hl, hero_type, status_type)?;
    LAYOUT
        .set(layout)
        .map_err(|_| "shield hook layout was already initialized".to_owned())?;
    Ok(method)
}

fn resolve_layout(
    hl: &HashLink<'_>,
    hero_type: usize,
    status_type: usize,
) -> Result<ShieldLayout, String> {
    let hero = validate_object(hl, hero_type, &HERO_SCHEMA)?;
    let status = validate_object(hl, status_type, &STATUS_SCHEMA)?;
    let base_skill_type = status
        .field_type_address("instigatorSkill")
        .ok_or_else(|| "validated Status layout omitted instigatorSkill type".to_owned())?;
    let base_skill = validate_object(hl, base_skill_type, &BASE_SKILL_SCHEMA)?;
    let status_string_type = status
        .field_type_address("kind")
        .ok_or_else(|| "validated Status layout omitted kind type".to_owned())?;
    let source_string_type = base_skill
        .field_type_address("kind")
        .ok_or_else(|| "validated BaseSkill layout omitted kind type".to_owned())?;
    if status_string_type != source_string_type {
        return Err("Status and BaseSkill kind fields use different String types".to_owned());
    }
    let string = validate_object(hl, status_string_type, &STRING_SCHEMA)?;
    let status_offset = |name| {
        status
            .offset(name)
            .ok_or_else(|| format!("validated Status layout omitted {name}"))
    };

    Ok(ShieldLayout {
        hero_type: hero.type_address,
        hero_owner_player: hero
            .offset("ownerPlayer")
            .ok_or_else(|| "validated Hero layout omitted ownerPlayer".to_owned())?,
        status_kind: status_offset("kind")?,
        status_owner: status_offset("owner")?,
        instigator_skill: status_offset("instigatorSkill")?,
        instigator: status_offset("instigator")?,
        shield_amount: status_offset("shieldAmount")?,
        base_skill_kind: base_skill
            .offset("kind")
            .ok_or_else(|| "validated BaseSkill layout omitted kind".to_owned())?,
        string_type: string.type_address,
        string_bytes: string
            .offset("bytes")
            .ok_or_else(|| "validated String layout omitted bytes".to_owned())?,
        string_length: string
            .offset("length")
            .ok_or_else(|| "validated String layout omitted length".to_owned())?,
    })
}

fn install_hook(method: ValidatedHashLinkMethod) -> Result<(), String> {
    let target = method.target() as *mut c_void;
    // SAFETY: resolution validates the complete `(Status, f64) -> f64` ABI and
    // every field copied by the detour before this call.
    let original = std::panic::catch_unwind(|| unsafe {
        MinHook::create_hook(target, hook_set_shield_amount as *mut c_void)
    })
    .map_err(|_| "MinHook initialization panicked for set_shieldAmount".to_owned())?
    .map_err(|status| format!("create set_shieldAmount hook returned {status:?}"))?;
    ORIGINAL_SET_SHIELD_AMOUNT.store(original as usize, Ordering::Release);
    // SAFETY: the hook and trampoline were created immediately above.
    if let Err(status) = unsafe { MinHook::enable_hook(target) } {
        // SAFETY: the target is the hook created immediately above.
        let _ = unsafe { MinHook::remove_hook(target) };
        ORIGINAL_SET_SHIELD_AMOUNT.store(0, Ordering::Release);
        return Err(format!("enable set_shieldAmount hook returned {status:?}"));
    }
    HOOK_TARGET.store(target as usize, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

unsafe extern "C" fn hook_set_shield_amount(status: *mut c_void, requested: f64) -> f64 {
    let observation = if ACTIVE.load(Ordering::Relaxed) {
        // SAFETY: installation validated the callback ABI and all fixed field
        // offsets. The copy is bounded to one stack record.
        unsafe { copy_observation(status, requested) }
    } else {
        None
    };

    let original = ORIGINAL_SET_SHIELD_AMOUNT.load(Ordering::Acquire);
    if original == 0 {
        return requested;
    }
    // SAFETY: MinHook returned this trampoline for the validated setter.
    let original: HlSetShieldAmount = unsafe { std::mem::transmute(original) };
    let result = unsafe { original(status, requested) };

    if let Some(observation) = observation {
        if RAW
            .get()
            .is_none_or(|queue| queue.push(observation).is_err())
        {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
    result
}

unsafe fn copy_observation(status: *mut c_void, new_amount: f64) -> Option<RawShieldChange> {
    if status.is_null() {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let Some(layout) = LAYOUT.get().copied() else {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    let status_base = status.cast::<u8>();
    // The signature guarantees Status or a subclass. All copied fields are
    // inherited Status/BaseSkill fields whose offsets were shape-validated.
    let target =
        unsafe { std::ptr::read_unaligned(status_base.add(layout.status_owner).cast::<usize>()) };
    let source =
        unsafe { std::ptr::read_unaligned(status_base.add(layout.instigator).cast::<usize>()) };
    let source_owner = if source >= 0x1_0000
        && unsafe { object_has_exact_type(source as *const c_void, layout.hero_type) }
    {
        unsafe {
            std::ptr::read_unaligned(
                (source as *const u8)
                    .add(layout.hero_owner_player)
                    .cast::<usize>(),
            )
        }
    } else {
        0
    };
    let instigator_skill = unsafe {
        std::ptr::read_unaligned(status_base.add(layout.instigator_skill).cast::<usize>())
    };
    let old_amount =
        unsafe { std::ptr::read_unaligned(status_base.add(layout.shield_amount).cast::<f64>()) };
    let mut status_id = [0_u16; MAX_SKILL_ID_CODE_UNITS];
    let status_id_length =
        unsafe { copy_kind(status as usize, layout.status_kind, layout, &mut status_id) };
    let mut source_skill_id = [0_u16; MAX_SKILL_ID_CODE_UNITS];
    let source_skill_id_length = unsafe {
        copy_kind(
            instigator_skill,
            layout.base_skill_kind,
            layout,
            &mut source_skill_id,
        )
    };

    Some(RawShieldChange {
        target_pointer: target,
        source_pointer: source,
        source_owner_pointer: source_owner,
        old_amount,
        new_amount,
        status_id_length,
        status_id,
        source_skill_id_length,
        source_skill_id,
    })
}

unsafe fn copy_kind(
    object: usize,
    kind_offset: usize,
    layout: ShieldLayout,
    destination: &mut [u16; MAX_SKILL_ID_CODE_UNITS],
) -> u16 {
    if object < 0x1_0000 {
        return 0;
    }
    let string =
        unsafe { std::ptr::read_unaligned((object as *const u8).add(kind_offset).cast::<usize>()) };
    if string < 0x1_0000
        || !unsafe { object_has_exact_type(string as *const c_void, layout.string_type) }
    {
        return 0;
    }
    let string_base = string as *const u8;
    let length =
        unsafe { std::ptr::read_unaligned(string_base.add(layout.string_length).cast::<i32>()) };
    if !(1..=MAX_SKILL_ID_CODE_UNITS as i32).contains(&length) {
        return 0;
    }
    let bytes =
        unsafe { std::ptr::read_unaligned(string_base.add(layout.string_bytes).cast::<usize>()) };
    if bytes < 0x1_0000 {
        return 0;
    }
    // SAFETY: length is bounded by the fixed destination array and the live
    // HashLink String owns at least that many UTF-16 code units.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes as *const u16,
            destination.as_mut_ptr(),
            length as usize,
        );
    }
    length as u16
}

impl ShieldHookDecoder {
    pub(crate) fn decode_pending(&mut self) {
        let Some(queue) = RAW.get() else {
            return;
        };
        for _ in 0..MAX_DECODE_PER_TICK {
            let Some(raw) = queue.pop() else {
                break;
            };
            self.decode(raw);
        }
    }

    fn decode(&mut self, raw: RawShieldChange) {
        let Some((status_id, source_skill_id, delta)) = decode_payload(&raw) else {
            INVALID.fetch_add(1, Ordering::Relaxed);
            return;
        };
        OBSERVED.fetch_add(1, Ordering::Relaxed);
        if delta > 0.0 {
            INCREASES.fetch_add(1, Ordering::Relaxed);
            if raw.old_amount <= 0.0 && raw.new_amount > 0.0 {
                INITIAL_APPLICATIONS.fetch_add(1, Ordering::Relaxed);
            } else if raw.old_amount > 0.0 {
                REFRESH_INCREASES.fetch_add(1, Ordering::Relaxed);
            }
        } else if delta < 0.0 {
            DECREASES.fetch_add(1, Ordering::Relaxed);
            if raw.old_amount > 0.0 && raw.new_amount <= 0.0 {
                CLEARED.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            UNCHANGED.fetch_add(1, Ordering::Relaxed);
        }
        if raw.source_pointer < 0x1_0000 {
            SOURCE_MISSING.fetch_add(1, Ordering::Relaxed);
        }
        if raw.target_pointer < 0x1_0000 {
            TARGET_MISSING.fetch_add(1, Ordering::Relaxed);
        }
        if status_id.is_none() {
            STATUS_ID_MISSING.fetch_add(1, Ordering::Relaxed);
        }
        if source_skill_id.is_none() {
            SOURCE_SKILL_MISSING.fetch_add(1, Ordering::Relaxed);
        }

        let local_hero = crate::player_hooks::local_hero_pointer();
        let local_player = crate::player_hooks::local_player_pointer();
        let local_source = raw.source_pointer == local_hero
            || (local_player >= 0x1_0000 && raw.source_owner_pointer == local_player);
        let local_target = raw.target_pointer == local_hero;
        if local_source {
            LOCAL_SOURCE.fetch_add(1, Ordering::Relaxed);
        }
        if local_target {
            LOCAL_TARGET.fetch_add(1, Ordering::Relaxed);
        }
        if local_source && local_target {
            SELF_SHIELD.fetch_add(1, Ordering::Relaxed);
        }
        let route = match (local_source, local_target) {
            (true, true) => "self",
            (true, false) => "local-source",
            (false, true) => "local-target",
            (false, false) => "remote",
        };
        if let Some(last) = LAST_SAMPLE.get() {
            if let Ok(mut last) = last.lock() {
                *last = Some(ShieldSample {
                    status_id,
                    source_skill_id,
                    old_amount: raw.old_amount,
                    new_amount: raw.new_amount,
                    delta,
                    route,
                });
            }
        }
    }
}

fn decode_payload(raw: &RawShieldChange) -> Option<(Option<String>, Option<String>, f64)> {
    if !raw.old_amount.is_finite()
        || !raw.new_amount.is_finite()
        || raw.old_amount.abs() >= MAX_ABSOLUTE_AMOUNT
        || raw.new_amount.abs() >= MAX_ABSOLUTE_AMOUNT
    {
        return None;
    }
    let delta = raw.new_amount - raw.old_amount;
    if !delta.is_finite() || delta.abs() >= MAX_ABSOLUTE_AMOUNT {
        return None;
    }
    let status_id = decode_optional_string(raw.status_id_length, &raw.status_id)?;
    let source_skill_id = decode_optional_string(raw.source_skill_id_length, &raw.source_skill_id)?;
    Some((status_id, source_skill_id, delta))
}

fn decode_optional_string(length: u16, value: &[u16]) -> Option<Option<String>> {
    let length = usize::from(length);
    if length == 0 {
        return Some(None);
    }
    if length > value.len() {
        return None;
    }
    let decoded = String::from_utf16(&value[..length]).ok()?;
    (!decoded.is_empty()).then_some(Some(decoded))
}

pub(crate) fn status() -> usize {
    HOOK_STATUS.load(Ordering::Acquire)
}

pub(crate) fn status_name(status: usize) -> &'static str {
    match status {
        0 => "waiting-for-shield-metadata",
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
            || "shield_last=none".to_owned(),
            |sample| {
                format!(
                    "shield_last={}:{}:{}:{}:{}:{}",
                    sample
                        .status_id
                        .as_deref()
                        .map(metric_token)
                        .unwrap_or_else(|| "none".to_owned()),
                    sample
                        .source_skill_id
                        .as_deref()
                        .map(metric_token)
                        .unwrap_or_else(|| "none".to_owned()),
                    sample.old_amount,
                    sample.new_amount,
                    sample.delta,
                    sample.route,
                )
            },
        );
    format!(
        "shield_hook={} shield_mode=shadow-only shield_observed={} shield_invalid={} shield_queue_drops={} shield_increases={} shield_decreases={} shield_unchanged={} shield_initial={} shield_refresh_increase={} shield_cleared={} shield_source_missing={} shield_target_missing={} shield_local_source={} shield_local_target={} shield_self={} shield_status_id_missing={} shield_source_skill_missing={} {}",
        status_name(status()),
        OBSERVED.load(Ordering::Relaxed),
        INVALID.load(Ordering::Relaxed),
        DROPS.load(Ordering::Relaxed),
        INCREASES.load(Ordering::Relaxed),
        DECREASES.load(Ordering::Relaxed),
        UNCHANGED.load(Ordering::Relaxed),
        INITIAL_APPLICATIONS.load(Ordering::Relaxed),
        REFRESH_INCREASES.load(Ordering::Relaxed),
        CLEARED.load(Ordering::Relaxed),
        SOURCE_MISSING.load(Ordering::Relaxed),
        TARGET_MISSING.load(Ordering::Relaxed),
        LOCAL_SOURCE.load(Ordering::Relaxed),
        LOCAL_TARGET.load(Ordering::Relaxed),
        SELF_SHIELD.load(Ordering::Relaxed),
        STATUS_ID_MISSING.load(Ordering::Relaxed),
        SOURCE_SKILL_MISSING.load(Ordering::Relaxed),
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

pub(crate) fn shutdown_hook() {
    ACTIVE.store(false, Ordering::Release);
    let target = HOOK_TARGET.load(Ordering::Acquire);
    if target != 0 {
        // SAFETY: the target is published only after successful enablement.
        let _ = unsafe { MinHook::disable_hook(target as *mut c_void) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(old_amount: f64, new_amount: f64, status: &str, source_skill: &str) -> RawShieldChange {
        let mut status_id = [0_u16; MAX_SKILL_ID_CODE_UNITS];
        let status = status.encode_utf16().collect::<Vec<_>>();
        status_id[..status.len()].copy_from_slice(&status);
        let mut source_skill_id = [0_u16; MAX_SKILL_ID_CODE_UNITS];
        let source_skill = source_skill.encode_utf16().collect::<Vec<_>>();
        source_skill_id[..source_skill.len()].copy_from_slice(&source_skill);
        RawShieldChange {
            target_pointer: 0x20_000,
            source_pointer: 0x30_000,
            source_owner_pointer: 0x40_000,
            old_amount,
            new_amount,
            status_id_length: status.len() as u16,
            status_id,
            source_skill_id_length: source_skill.len() as u16,
            source_skill_id,
        }
    }

    #[test]
    fn method_specs_match_verified_shield_boundary() {
        assert_eq!(ADD_STACKS_ANCHOR.name, c"addStacks");
        assert_eq!(ADD_STACKS_ANCHOR.arguments, ADD_STACKS_ARGUMENTS);
        assert_eq!(SET_SHIELD_AMOUNT.name, c"set_shieldAmount");
        assert_eq!(SET_SHIELD_AMOUNT.arguments, SET_SHIELD_AMOUNT_ARGUMENTS);
        assert_eq!(
            SET_SHIELD_AMOUNT.result,
            HashLinkTypeSpec::Kind(HashLinkKind::F64)
        );
    }

    #[test]
    fn shadow_decoder_preserves_increase_and_consumption_deltas() {
        let increase = raw(25.0, 40.0, "Shield_Status", "Shield_Skill");
        assert_eq!(
            decode_payload(&increase),
            Some((
                Some("Shield_Status".to_owned()),
                Some("Shield_Skill".to_owned()),
                15.0
            ))
        );
        let consumed = raw(40.0, 0.0, "Shield_Status", "");
        assert_eq!(
            decode_payload(&consumed),
            Some((Some("Shield_Status".to_owned()), None, -40.0))
        );
    }

    #[test]
    fn shadow_decoder_rejects_non_finite_or_malformed_payloads() {
        assert!(decode_payload(&raw(f64::NAN, 1.0, "Shield", "Skill")).is_none());
        let mut malformed = raw(0.0, 1.0, "Shield", "Skill");
        malformed.status_id_length = (MAX_SKILL_ID_CODE_UNITS + 1) as u16;
        assert!(decode_payload(&malformed).is_none());
    }

    #[test]
    fn metric_tokens_do_not_break_the_space_delimited_metrics_line() {
        assert_eq!(metric_token("Shield Status:Ⅱ"), "Shield_Status__");
    }
}
