//! Internal shadow capture for local skill cooldown and charge changes.
//!
//! `Skill.onTriggerCD` is the discrete cooldown-start boundary after the game
//! computes the effective duration. `Skill.set_charges` is the central setter
//! used by consumption, cooldown reduction, refills, resets, and replicated
//! reconciliation. Both callbacks admit only skills owned by the local Hero.

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
const MAX_SKILL_KIND_CODE_UNITS: usize = 128;

const DO_USE_SKILL_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ent.GameObject"),
    HashLinkTypeSpec::Object("st.skill.Skill"),
    HashLinkTypeSpec::Enum("st.skill.SkillTarget"),
    HashLinkTypeSpec::Object("st.Item"),
];
const DO_USE_SKILL_ANCHOR: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.GameObject",
    name: c"doUseSkill",
    arguments: DO_USE_SKILL_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const ON_TRIGGER_CD_ARGUMENTS: &[HashLinkTypeSpec] = &[HashLinkTypeSpec::Object("st.skill.Skill")];
const ON_TRIGGER_CD: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.skill.Skill",
    name: c"onTriggerCD",
    arguments: ON_TRIGGER_CD_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const SET_CHARGES_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.skill.Skill"),
    HashLinkTypeSpec::Kind(HashLinkKind::F64),
];
const SET_CHARGES: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.skill.Skill",
    name: c"set_charges",
    arguments: SET_CHARGES_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::F64),
};
const SKILL_FIELDS: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::object("kind", "String"),
    HashLinkFieldSpec::object("owner", "ent.GameObject"),
    HashLinkFieldSpec::scalar("charges", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("cooldownDuration", HashLinkKind::F64),
];
const SKILL_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.skill.Skill",
    kind: HashLinkKind::Object,
    fields: SKILL_FIELDS,
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

type HlOnTriggerCd = unsafe extern "C" fn(*mut c_void);
type HlSetCharges = unsafe extern "C" fn(*mut c_void, f64) -> f64;

#[derive(Clone, Copy, Debug)]
struct CooldownLayout {
    kind: usize,
    owner: usize,
    charges: usize,
    cooldown_duration: usize,
    string_type: usize,
    string_bytes: usize,
    string_length: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawKind {
    Triggered,
    ChargesChanged,
}

#[derive(Clone, Copy, Debug)]
struct RawCooldownChange {
    kind: RawKind,
    owner_pointer: usize,
    skill_pointer: usize,
    skill_type: usize,
    old_charges: f64,
    new_charges: f64,
    cooldown_duration: f64,
    skill_kind_length: u16,
    skill_kind: [u16; MAX_SKILL_KIND_CODE_UNITS],
}

#[derive(Clone, Debug, PartialEq)]
struct CooldownSample {
    kind: RawKind,
    skill_kind: String,
    old_charges: f64,
    new_charges: f64,
    cooldown_duration: f64,
}

#[derive(Default)]
pub(crate) struct CooldownHookDecoder;

struct ResolvedHooks {
    trigger: ValidatedHashLinkMethod,
    charges: ValidatedHashLinkMethod,
}

static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static TRIGGER_TARGET: AtomicUsize = AtomicUsize::new(0);
static CHARGES_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_TRIGGER: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_CHARGES: AtomicUsize = AtomicUsize::new(0);
static LAYOUT: OnceLock<CooldownLayout> = OnceLock::new();
static RAW: OnceLock<ArrayQueue<RawCooldownChange>> = OnceLock::new();
static LAST_SAMPLE: OnceLock<Mutex<Option<CooldownSample>>> = OnceLock::new();

static TRIGGERS: AtomicU64 = AtomicU64::new(0);
static CHARGE_CHANGES: AtomicU64 = AtomicU64::new(0);
static CHARGE_DECREASES: AtomicU64 = AtomicU64::new(0);
static CHARGE_INCREASES: AtomicU64 = AtomicU64::new(0);
static UNCHANGED: AtomicU64 = AtomicU64::new(0);
static FILTERED: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static DROPS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn prepare_queue() {
    let _ = RAW.get_or_init(|| ArrayQueue::new(RAW_QUEUE_CAPACITY));
    let _ = LAST_SAMPLE.get_or_init(|| Mutex::new(None));
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
    let game_object_type = hl
        .type_address_named(hero_type, "ent.GameObject")
        .ok_or_else(|| "ent.Hero does not inherit ent.GameObject".to_owned())?;
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    let anchor = runtime.resolve_method(hl, game_object_type, &DO_USE_SKILL_ANCHOR)?;
    let skill_type = anchor
        .argument_type(1)
        .ok_or_else(|| "validated doUseSkill signature omitted Skill".to_owned())?;
    let trigger = runtime.resolve_method(hl, skill_type, &ON_TRIGGER_CD)?;
    let charges = runtime.resolve_method(hl, skill_type, &SET_CHARGES)?;
    let skill = validate_object(hl, skill_type, &SKILL_SCHEMA)?;
    let string_type = skill
        .field_type_address("kind")
        .ok_or_else(|| "validated Skill layout omitted kind type".to_owned())?;
    let string = validate_object(hl, string_type, &STRING_SCHEMA)?;
    LAYOUT
        .set(CooldownLayout {
            kind: skill
                .offset("kind")
                .ok_or_else(|| "validated Skill layout omitted kind".to_owned())?,
            owner: skill
                .offset("owner")
                .ok_or_else(|| "validated Skill layout omitted owner".to_owned())?,
            charges: skill
                .offset("charges")
                .ok_or_else(|| "validated Skill layout omitted charges".to_owned())?,
            cooldown_duration: skill
                .offset("cooldownDuration")
                .ok_or_else(|| "validated Skill layout omitted cooldownDuration".to_owned())?,
            string_type: string.type_address,
            string_bytes: string
                .offset("bytes")
                .ok_or_else(|| "validated String layout omitted bytes".to_owned())?,
            string_length: string
                .offset("length")
                .ok_or_else(|| "validated String layout omitted length".to_owned())?,
        })
        .map_err(|_| "cooldown hook layout was already initialized".to_owned())?;
    Ok(ResolvedHooks { trigger, charges })
}

fn install_hooks(hooks: &ResolvedHooks) -> Result<(), String> {
    let targets = [
        hooks.trigger.target() as *mut c_void,
        hooks.charges.target() as *mut c_void,
    ];
    if targets[0] == targets[1] {
        return Err("cooldown methods resolved to one target".to_owned());
    }
    let detours = [
        hook_on_trigger_cd as *mut c_void,
        hook_set_charges as *mut c_void,
    ];
    let originals = [&ORIGINAL_TRIGGER, &ORIGINAL_CHARGES];
    for index in 0..targets.len() {
        if let Err(error) = install_one(targets[index], detours[index], originals[index]) {
            for previous in 0..index {
                remove_one(targets[previous], originals[previous]);
            }
            return Err(error);
        }
    }
    TRIGGER_TARGET.store(targets[0] as usize, Ordering::Release);
    CHARGES_TARGET.store(targets[1] as usize, Ordering::Release);
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
        .map_err(|_| "MinHook initialization panicked for cooldown hooks".to_owned())?
        .map_err(|status| format!("create cooldown hook returned {status:?}"))?;
    original.store(trampoline as usize, Ordering::Release);
    // SAFETY: the hook was created immediately above.
    if let Err(status) = unsafe { MinHook::enable_hook(target) } {
        // SAFETY: target names the hook created immediately above.
        let _ = unsafe { MinHook::remove_hook(target) };
        original.store(0, Ordering::Release);
        return Err(format!("enable cooldown hook returned {status:?}"));
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

unsafe extern "C" fn hook_on_trigger_cd(skill: *mut c_void) {
    let observation = if ACTIVE.load(Ordering::Relaxed) {
        unsafe { copy_skill(skill, RawKind::Triggered) }
    } else {
        None
    };
    let original = ORIGINAL_TRIGGER.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: trampoline matches the validated method.
        let original: HlOnTriggerCd = unsafe { std::mem::transmute(original) };
        unsafe { original(skill) };
    }
    if let Some(mut observation) = observation {
        let Some(layout) = LAYOUT.get().copied() else {
            INVALID.fetch_add(1, Ordering::Relaxed);
            return;
        };
        observation.cooldown_duration = unsafe {
            std::ptr::read_unaligned(
                skill
                    .cast::<u8>()
                    .add(layout.cooldown_duration)
                    .cast::<f64>(),
            )
        };
        if observation.cooldown_duration.is_finite() {
            queue(observation);
        } else {
            INVALID.fetch_add(1, Ordering::Relaxed);
        }
    }
}

unsafe extern "C" fn hook_set_charges(skill: *mut c_void, requested: f64) -> f64 {
    let observation = if ACTIVE.load(Ordering::Relaxed) {
        unsafe { copy_skill(skill, RawKind::ChargesChanged) }
    } else {
        None
    };
    let original = ORIGINAL_CHARGES.load(Ordering::Acquire);
    let result = if original == 0 {
        requested
    } else {
        // SAFETY: trampoline matches the validated method.
        let original: HlSetCharges = unsafe { std::mem::transmute(original) };
        unsafe { original(skill, requested) }
    };
    if let Some(mut observation) = observation {
        observation.new_charges = result;
        if !result.is_finite() {
            INVALID.fetch_add(1, Ordering::Relaxed);
        } else if observation.old_charges.to_bits() == result.to_bits() {
            UNCHANGED.fetch_add(1, Ordering::Relaxed);
        } else {
            queue(observation);
        }
    }
    result
}

unsafe fn copy_skill(skill: *mut c_void, kind: RawKind) -> Option<RawCooldownChange> {
    let Some(layout) = LAYOUT.get().copied() else {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if skill.is_null() {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let base = skill.cast::<u8>();
    let owner = unsafe { std::ptr::read_unaligned(base.add(layout.owner).cast::<usize>()) };
    if owner != crate::player_hooks::local_hero_pointer() {
        FILTERED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let skill_type = unsafe { std::ptr::read_unaligned(base.cast::<usize>()) };
    if skill_type < 0x1_0000 {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let charges = unsafe { std::ptr::read_unaligned(base.add(layout.charges).cast::<f64>()) };
    let cooldown_duration =
        unsafe { std::ptr::read_unaligned(base.add(layout.cooldown_duration).cast::<f64>()) };
    if !charges.is_finite() || !cooldown_duration.is_finite() {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let string = unsafe { std::ptr::read_unaligned(base.add(layout.kind).cast::<usize>()) };
    if string < 0x1_0000
        || !unsafe { object_has_exact_type(string as *const c_void, layout.string_type) }
    {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let string_base = string as *const u8;
    let length =
        unsafe { std::ptr::read_unaligned(string_base.add(layout.string_length).cast::<i32>()) };
    if !(1..=MAX_SKILL_KIND_CODE_UNITS as i32).contains(&length) {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let bytes =
        unsafe { std::ptr::read_unaligned(string_base.add(layout.string_bytes).cast::<usize>()) };
    if bytes < 0x1_0000 {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut raw = RawCooldownChange {
        kind,
        owner_pointer: owner,
        skill_pointer: skill as usize,
        skill_type,
        old_charges: charges,
        new_charges: charges,
        cooldown_duration,
        skill_kind_length: length as u16,
        skill_kind: [0; MAX_SKILL_KIND_CODE_UNITS],
    };
    // SAFETY: String layout and positive length are validated and bounded.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes as *const u16,
            raw.skill_kind.as_mut_ptr(),
            length as usize,
        );
    }
    Some(raw)
}

fn queue(raw: RawCooldownChange) {
    if RAW.get().is_none_or(|queue| queue.push(raw).is_err()) {
        DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

impl CooldownHookDecoder {
    pub(crate) fn decode_pending(&mut self, hl: &HashLink<'_>) {
        let Some(queue) = RAW.get() else {
            return;
        };
        for _ in 0..MAX_DECODE_PER_TICK {
            let Some(raw) = queue.pop() else {
                break;
            };
            if raw.owner_pointer != crate::player_hooks::local_hero_pointer()
                || !hl.type_is_a(raw.skill_type, "st.skill.Skill")
            {
                FILTERED.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let Some(sample) = decode_sample(&raw) else {
                INVALID.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            match sample.kind {
                RawKind::Triggered => {
                    TRIGGERS.fetch_add(1, Ordering::Relaxed);
                }
                RawKind::ChargesChanged => {
                    CHARGE_CHANGES.fetch_add(1, Ordering::Relaxed);
                    if sample.new_charges < sample.old_charges {
                        CHARGE_DECREASES.fetch_add(1, Ordering::Relaxed);
                    } else {
                        CHARGE_INCREASES.fetch_add(1, Ordering::Relaxed);
                    }
                }
            };
            if let Some(last) = LAST_SAMPLE.get() {
                if let Ok(mut last) = last.lock() {
                    *last = Some(sample);
                }
            }
        }
    }
}

fn decode_sample(raw: &RawCooldownChange) -> Option<CooldownSample> {
    if raw.owner_pointer < 0x1_0000
        || raw.skill_pointer < 0x1_0000
        || raw.skill_type < 0x1_0000
        || !raw.old_charges.is_finite()
        || !raw.new_charges.is_finite()
        || !raw.cooldown_duration.is_finite()
    {
        return None;
    }
    let length = usize::from(raw.skill_kind_length);
    if length == 0 || length > raw.skill_kind.len() {
        return None;
    }
    let skill_kind = String::from_utf16(&raw.skill_kind[..length]).ok()?;
    if skill_kind.is_empty() {
        return None;
    }
    Some(CooldownSample {
        kind: raw.kind,
        skill_kind,
        old_charges: raw.old_charges,
        new_charges: raw.new_charges,
        cooldown_duration: raw.cooldown_duration,
    })
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
            || "cooldown_last=unknown".to_owned(),
            |sample| {
                format!(
                    "cooldown_last={}:{}:{:.3}->{:.3}:duration={:.3}",
                    match sample.kind {
                        RawKind::Triggered => "trigger",
                        RawKind::ChargesChanged => "charges",
                    },
                    metric_token(&sample.skill_kind),
                    sample.old_charges,
                    sample.new_charges,
                    sample.cooldown_duration,
                )
            },
        );
    format!(
        "cooldown_hooks={} cooldown_mode=shadow-only cooldown_triggers={} cooldown_charge_changes={} cooldown_charge_decreases={} cooldown_charge_increases={} cooldown_unchanged={} cooldown_filtered={} cooldown_invalid={} cooldown_queue_drops={} {}",
        status_name(status()),
        TRIGGERS.load(Ordering::Relaxed),
        CHARGE_CHANGES.load(Ordering::Relaxed),
        CHARGE_DECREASES.load(Ordering::Relaxed),
        CHARGE_INCREASES.load(Ordering::Relaxed),
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
    for target in [&TRIGGER_TARGET, &CHARGES_TARGET] {
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

    fn raw(kind: RawKind, old_charges: f64, new_charges: f64) -> RawCooldownChange {
        let skill_kind = "Skill_Flurry".encode_utf16().collect::<Vec<_>>();
        let mut raw = RawCooldownChange {
            kind,
            owner_pointer: 0x10_000,
            skill_pointer: 0x20_000,
            skill_type: 0x30_000,
            old_charges,
            new_charges,
            cooldown_duration: 4.5,
            skill_kind_length: skill_kind.len() as u16,
            skill_kind: [0; MAX_SKILL_KIND_CODE_UNITS],
        };
        raw.skill_kind[..skill_kind.len()].copy_from_slice(&skill_kind);
        raw
    }

    #[test]
    fn method_specs_match_verified_cooldown_boundaries() {
        assert_eq!(ON_TRIGGER_CD.name, c"onTriggerCD");
        assert_eq!(SET_CHARGES.name, c"set_charges");
        assert_eq!(
            SET_CHARGES.result,
            HashLinkTypeSpec::Kind(HashLinkKind::F64)
        );
    }

    #[test]
    fn decoder_preserves_trigger_and_charge_values() {
        assert_eq!(
            decode_sample(&raw(RawKind::ChargesChanged, 1.0, 0.0)),
            Some(CooldownSample {
                kind: RawKind::ChargesChanged,
                skill_kind: "Skill_Flurry".to_owned(),
                old_charges: 1.0,
                new_charges: 0.0,
                cooldown_duration: 4.5,
            })
        );
        assert_eq!(
            decode_sample(&raw(RawKind::Triggered, 1.0, 1.0)).map(|sample| sample.kind),
            Some(RawKind::Triggered)
        );
    }

    #[test]
    fn decoder_rejects_non_finite_values() {
        assert_eq!(
            decode_sample(&raw(RawKind::ChargesChanged, f64::NAN, 0.0)),
            None
        );
    }
}
