//! Internal shadow capture for Farever's client-routed healing RPC.
//!
//! Healing is not part of the public add-on contract yet. This provider keeps
//! the callback bounded and records only aggregate routing/deduplication
//! telemetry so live QA can establish semantics before any WIT expansion.

use crate::game_build::GameBuildProfile;
use crate::hashlink::{
    object_has_exact_type, validate_object, HashLink, HashLinkFieldSpec, HashLinkKind,
    HashLinkMethodSpec, HashLinkObjectSpec, HashLinkRuntime, HashLinkTypeSpec,
    ValidatedHashLinkMethod,
};
use crossbeam_queue::ArrayQueue;
use minhook::MinHook;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const RAW_QUEUE_CAPACITY: usize = 512;
const MAX_DECODE_PER_TICK: usize = 64;
const MAX_SKILL_ID_CODE_UNITS: usize = 128;
const DEDUPE_WINDOW: Duration = Duration::from_secs(3);

const HEAL_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ent.Unit"),
    HashLinkTypeSpec::Object("st.skill.DamageResult"),
];
const DISPLAY_HEAL: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.Unit",
    name: c"rpcDisplayHeal__impl",
    arguments: HEAL_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};

const RESULT_FIELDS_STABLE: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::object("baseSkill", "st.skill.BaseSkill"),
    HashLinkFieldSpec::object("serverSource", "ent.GameObject"),
    HashLinkFieldSpec::scalar("weakSource", HashLinkKind::I64),
    HashLinkFieldSpec::object("target", "ent.GameObject"),
    HashLinkFieldSpec::scalar("_amount", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("_hitCount", HashLinkKind::I32),
    HashLinkFieldSpec::scalar("_critical", HashLinkKind::Bool),
];
const RESULT_FIELDS_BETA: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::object("skill", "st.skill.BaseSkill"),
    HashLinkFieldSpec::object("serverSource", "ent.GameObject"),
    HashLinkFieldSpec::scalar("weakSource", HashLinkKind::I64),
    HashLinkFieldSpec::object("target", "ent.GameObject"),
    HashLinkFieldSpec::scalar("_amount", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("_hitCount", HashLinkKind::I32),
    HashLinkFieldSpec::scalar("_critical", HashLinkKind::Bool),
];
const RESULT_SCHEMA_STABLE: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.skill.DamageResult",
    kind: HashLinkKind::Object,
    fields: RESULT_FIELDS_STABLE,
};
const RESULT_SCHEMA_BETA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.skill.DamageResult",
    kind: HashLinkKind::Object,
    fields: RESULT_FIELDS_BETA,
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

type HlDisplayHeal = unsafe extern "C" fn(*mut c_void, *mut c_void);

#[derive(Clone, Copy, Debug)]
struct HealLayout {
    result_type: usize,
    hero_type: usize,
    hero_owner_player: usize,
    base_skill: usize,
    server_source: usize,
    weak_source: usize,
    target: usize,
    amount: usize,
    hit_ordinal: usize,
    critical: usize,
    base_skill_kind: usize,
    string_type: usize,
    string_bytes: usize,
    string_length: usize,
}

#[derive(Clone, Copy, Debug)]
struct RawHealObserved {
    result_pointer: usize,
    receiver_pointer: usize,
    server_source_pointer: usize,
    source_owner_pointer: usize,
    weak_source: i64,
    target_pointer: usize,
    amount: f64,
    hit_ordinal: i32,
    critical: u8,
    skill_id_length: u16,
    skill_id: [u16; MAX_SKILL_ID_CODE_UNITS],
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ExactKey {
    result_pointer: usize,
    receiver_pointer: usize,
    target_pointer: usize,
    amount_bits: u64,
    hit_ordinal: i32,
    critical: bool,
    skill_id: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ContentKey {
    server_source_pointer: usize,
    source_owner_pointer: usize,
    weak_source: i64,
    target_pointer: usize,
    amount_bits: u64,
    hit_ordinal: i32,
    critical: bool,
    skill_id: String,
}

#[derive(Clone, Debug, PartialEq)]
struct HealSample {
    skill_id: String,
    amount: f64,
    hit_ordinal: i32,
    critical: bool,
    route: &'static str,
}

#[derive(Default)]
pub(crate) struct HealingHookDecoder {
    exact: HashMap<ExactKey, Instant>,
    content: HashMap<ContentKey, Instant>,
}

// 0 = waiting for Hero/Unit metadata, 1 = active shadow capture,
// 3 = failed, 4 = installing.
static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_DISPLAY_HEAL: AtomicUsize = AtomicUsize::new(0);
static LAYOUT: OnceLock<HealLayout> = OnceLock::new();
static RAW: OnceLock<ArrayQueue<RawHealObserved>> = OnceLock::new();
static LAST_SAMPLE: OnceLock<Mutex<Option<HealSample>>> = OnceLock::new();

static OBSERVED: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static DROPS: AtomicU64 = AtomicU64::new(0);
static EXACT_DUPLICATES: AtomicU64 = AtomicU64::new(0);
static CONTENT_REPEATS: AtomicU64 = AtomicU64::new(0);
static RECEIVER_TARGET_MISMATCHES: AtomicU64 = AtomicU64::new(0);
static SOURCE_MISSING: AtomicU64 = AtomicU64::new(0);
static LOCAL_SOURCE: AtomicU64 = AtomicU64::new(0);
static LOCAL_TARGET: AtomicU64 = AtomicU64::new(0);
static SELF_HEAL: AtomicU64 = AtomicU64::new(0);
static POSITIVE_AMOUNT: AtomicU64 = AtomicU64::new(0);
static ZERO_AMOUNT: AtomicU64 = AtomicU64::new(0);
static NEGATIVE_AMOUNT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn prepare_queue() {
    let _ = RAW.get_or_init(|| ArrayQueue::new(RAW_QUEUE_CAPACITY));
    let _ = LAST_SAMPLE.get_or_init(|| Mutex::new(None));
}

pub(crate) fn try_install_hook(hl: &HashLink<'_>, game_build: GameBuildProfile) {
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

    let result = resolve_hook(hl, hero_type, game_build).and_then(install_hook);
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
    hero_type: usize,
    game_build: GameBuildProfile,
) -> Result<ValidatedHashLinkMethod, String> {
    let unit_type = hl
        .type_address_named(hero_type, "ent.Unit")
        .ok_or_else(|| "ent.Hero does not inherit the expected ent.Unit type".to_owned())?;
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    let method = runtime.resolve_method(hl, unit_type, &DISPLAY_HEAL)?;
    let result_type = method
        .argument_type(1)
        .ok_or_else(|| "validated healing signature omitted DamageResult".to_owned())?;
    let layout = resolve_layout(hl, hero_type, result_type, game_build)?;
    LAYOUT
        .set(layout)
        .map_err(|_| "healing hook layout was already initialized".to_owned())?;
    Ok(method)
}

fn resolve_layout(
    hl: &HashLink<'_>,
    hero_type: usize,
    result_type: usize,
    game_build: GameBuildProfile,
) -> Result<HealLayout, String> {
    let hero = validate_object(hl, hero_type, &HERO_SCHEMA)?;
    let result_schema = if game_build.uses_beta_abi() {
        &RESULT_SCHEMA_BETA
    } else {
        &RESULT_SCHEMA_STABLE
    };
    let result = validate_object(hl, result_type, result_schema)?;
    let skill_field = game_build.base_skill_access_field();
    let result_offset = |name| {
        result
            .offset(name)
            .ok_or_else(|| format!("validated DamageResult layout omitted {name}"))
    };
    let base_skill_type = result
        .field_type_address(skill_field)
        .ok_or_else(|| format!("validated DamageResult layout omitted {skill_field} type"))?;
    let base_skill = validate_object(hl, base_skill_type, &BASE_SKILL_SCHEMA)?;
    let string_type = base_skill
        .field_type_address("kind")
        .ok_or_else(|| "validated BaseSkill layout omitted kind type".to_owned())?;
    let string = validate_object(hl, string_type, &STRING_SCHEMA)?;

    Ok(HealLayout {
        result_type: result.type_address,
        hero_type: hero.type_address,
        hero_owner_player: hero
            .offset("ownerPlayer")
            .ok_or_else(|| "validated Hero layout omitted ownerPlayer".to_owned())?,
        base_skill: result_offset(skill_field)?,
        server_source: result_offset("serverSource")?,
        weak_source: result_offset("weakSource")?,
        target: result_offset("target")?,
        amount: result_offset("_amount")?,
        hit_ordinal: result_offset("_hitCount")?,
        critical: result_offset("_critical")?,
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
    // SAFETY: resolution validates the full two-object-argument/Void ABI and
    // every copied object field before this call.
    let original = std::panic::catch_unwind(|| unsafe {
        MinHook::create_hook(target, hook_display_heal as *mut c_void)
    })
    .map_err(|_| "MinHook initialization panicked for rpcDisplayHeal__impl".to_owned())?
    .map_err(|status| format!("create rpcDisplayHeal__impl hook returned {status:?}"))?;
    ORIGINAL_DISPLAY_HEAL.store(original as usize, Ordering::Release);
    // SAFETY: the hook and trampoline were created immediately above.
    if let Err(status) = unsafe { MinHook::enable_hook(target) } {
        // SAFETY: the target is the hook created immediately above.
        let _ = unsafe { MinHook::remove_hook(target) };
        ORIGINAL_DISPLAY_HEAL.store(0, Ordering::Release);
        return Err(format!(
            "enable rpcDisplayHeal__impl hook returned {status:?}"
        ));
    }
    HOOK_TARGET.store(target as usize, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

unsafe extern "C" fn hook_display_heal(receiver: *mut c_void, result: *mut c_void) {
    let observation = if ACTIVE.load(Ordering::Relaxed) {
        // SAFETY: installation validated the callback ABI and all fixed field
        // offsets. The copy is bounded to one stack record.
        unsafe { copy_observation(receiver, result) }
    } else {
        None
    };

    let original = ORIGINAL_DISPLAY_HEAL.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: MinHook returned this trampoline for the validated method.
        let original: HlDisplayHeal = unsafe { std::mem::transmute(original) };
        unsafe { original(receiver, result) };
    }

    if let Some(observation) = observation {
        if RAW
            .get()
            .is_none_or(|queue| queue.push(observation).is_err())
        {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

unsafe fn copy_observation(receiver: *mut c_void, result: *mut c_void) -> Option<RawHealObserved> {
    if receiver.is_null() || result.is_null() {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let Some(layout) = LAYOUT.get().copied() else {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if !unsafe { object_has_exact_type(result, layout.result_type) } {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let result_base = result.cast::<u8>();
    let base_skill =
        unsafe { std::ptr::read_unaligned(result_base.add(layout.base_skill).cast::<usize>()) };
    let server_source =
        unsafe { std::ptr::read_unaligned(result_base.add(layout.server_source).cast::<usize>()) };
    let source_owner = if server_source >= 0x1_0000
        && unsafe { object_has_exact_type(server_source as *const c_void, layout.hero_type) }
    {
        unsafe {
            std::ptr::read_unaligned(
                (server_source as *const u8)
                    .add(layout.hero_owner_player)
                    .cast::<usize>(),
            )
        }
    } else {
        0
    };
    let weak_source =
        unsafe { std::ptr::read_unaligned(result_base.add(layout.weak_source).cast::<i64>()) };
    let target =
        unsafe { std::ptr::read_unaligned(result_base.add(layout.target).cast::<usize>()) };
    if base_skill < 0x1_0000 {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let skill_string = unsafe {
        std::ptr::read_unaligned(
            (base_skill as *const u8)
                .add(layout.base_skill_kind)
                .cast::<usize>(),
        )
    };
    if skill_string < 0x1_0000
        || !unsafe { object_has_exact_type(skill_string as *const c_void, layout.string_type) }
    {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let string_base = skill_string as *const u8;
    let skill_length =
        unsafe { std::ptr::read_unaligned(string_base.add(layout.string_length).cast::<i32>()) };
    if !(1..=MAX_SKILL_ID_CODE_UNITS as i32).contains(&skill_length) {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let skill_bytes =
        unsafe { std::ptr::read_unaligned(string_base.add(layout.string_bytes).cast::<usize>()) };
    if skill_bytes < 0x1_0000 {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut skill_id = [0_u16; MAX_SKILL_ID_CODE_UNITS];
    // SAFETY: the validated String length is positive and bounded by the
    // fixed destination array.
    unsafe {
        std::ptr::copy_nonoverlapping(
            skill_bytes as *const u16,
            skill_id.as_mut_ptr(),
            skill_length as usize,
        );
    }

    Some(RawHealObserved {
        result_pointer: result as usize,
        receiver_pointer: receiver as usize,
        server_source_pointer: server_source,
        source_owner_pointer: source_owner,
        weak_source,
        target_pointer: target,
        amount: unsafe { std::ptr::read_unaligned(result_base.add(layout.amount).cast()) },
        hit_ordinal: unsafe {
            std::ptr::read_unaligned(result_base.add(layout.hit_ordinal).cast())
        },
        critical: unsafe { std::ptr::read_unaligned(result_base.add(layout.critical).cast()) },
        skill_id_length: skill_length as u16,
        skill_id,
    })
}

impl HealingHookDecoder {
    pub(crate) fn decode_pending(&mut self) {
        let now = Instant::now();
        self.exact
            .retain(|_, seen| now.duration_since(*seen) < DEDUPE_WINDOW);
        self.content
            .retain(|_, seen| now.duration_since(*seen) < DEDUPE_WINDOW);
        let Some(queue) = RAW.get() else {
            return;
        };
        for _ in 0..MAX_DECODE_PER_TICK {
            let Some(raw) = queue.pop() else {
                break;
            };
            self.decode(raw, now);
        }
    }

    fn decode(&mut self, raw: RawHealObserved, now: Instant) {
        let Some((skill_id, critical)) = decode_payload(&raw) else {
            INVALID.fetch_add(1, Ordering::Relaxed);
            return;
        };
        OBSERVED.fetch_add(1, Ordering::Relaxed);
        let exact = ExactKey {
            result_pointer: raw.result_pointer,
            receiver_pointer: raw.receiver_pointer,
            target_pointer: raw.target_pointer,
            amount_bits: raw.amount.to_bits(),
            hit_ordinal: raw.hit_ordinal,
            critical,
            skill_id: skill_id.clone(),
        };
        let exact_duplicate = self.exact.insert(exact, now).is_some();
        if exact_duplicate {
            EXACT_DUPLICATES.fetch_add(1, Ordering::Relaxed);
        }
        let content = ContentKey {
            server_source_pointer: raw.server_source_pointer,
            source_owner_pointer: raw.source_owner_pointer,
            weak_source: raw.weak_source,
            target_pointer: raw.target_pointer,
            amount_bits: raw.amount.to_bits(),
            hit_ordinal: raw.hit_ordinal,
            critical,
            skill_id: skill_id.clone(),
        };
        if self.content.insert(content, now).is_some() && !exact_duplicate {
            CONTENT_REPEATS.fetch_add(1, Ordering::Relaxed);
        }

        if raw.receiver_pointer != raw.target_pointer {
            RECEIVER_TARGET_MISMATCHES.fetch_add(1, Ordering::Relaxed);
        }
        if raw.server_source_pointer < 0x1_0000 && raw.weak_source == 0 {
            SOURCE_MISSING.fetch_add(1, Ordering::Relaxed);
        }
        let local_hero = crate::player_hooks::local_hero_pointer();
        let local_player = crate::player_hooks::local_player_pointer();
        let local_source = raw.server_source_pointer == local_hero
            || (local_player >= 0x1_0000 && raw.source_owner_pointer == local_player);
        let local_target = raw.receiver_pointer == local_hero || raw.target_pointer == local_hero;
        if local_source {
            LOCAL_SOURCE.fetch_add(1, Ordering::Relaxed);
        }
        if local_target {
            LOCAL_TARGET.fetch_add(1, Ordering::Relaxed);
        }
        if local_source && local_target {
            SELF_HEAL.fetch_add(1, Ordering::Relaxed);
        }
        if raw.amount > 0.0 {
            POSITIVE_AMOUNT.fetch_add(1, Ordering::Relaxed);
        } else if raw.amount == 0.0 {
            ZERO_AMOUNT.fetch_add(1, Ordering::Relaxed);
        } else {
            NEGATIVE_AMOUNT.fetch_add(1, Ordering::Relaxed);
        }
        let route = match (local_source, local_target) {
            (true, true) => "self",
            (true, false) => "local-source",
            (false, true) => "local-target",
            (false, false) => "remote",
        };
        if let Some(last) = LAST_SAMPLE.get() {
            if let Ok(mut last) = last.lock() {
                *last = Some(HealSample {
                    skill_id,
                    amount: raw.amount,
                    hit_ordinal: raw.hit_ordinal,
                    critical,
                    route,
                });
            }
        }
    }
}

fn decode_payload(raw: &RawHealObserved) -> Option<(String, bool)> {
    let length = usize::from(raw.skill_id_length);
    if length == 0
        || length > raw.skill_id.len()
        || !raw.amount.is_finite()
        || raw.amount.abs() >= 100_000_000.0
        || raw.critical > 1
    {
        return None;
    }
    let skill_id = String::from_utf16(&raw.skill_id[..length]).ok()?;
    (!skill_id.is_empty()).then_some((skill_id, raw.critical == 1))
}

pub(crate) fn status() -> usize {
    HOOK_STATUS.load(Ordering::Acquire)
}

pub(crate) fn status_name(status: usize) -> &'static str {
    match status {
        0 => "waiting-for-heal-metadata",
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
            || "heal_last=none".to_owned(),
            |sample| {
                format!(
                    "heal_last={}:{}:{}:{}:{}",
                    metric_token(&sample.skill_id),
                    sample.amount,
                    sample.hit_ordinal,
                    u8::from(sample.critical),
                    sample.route,
                )
            },
        );
    format!(
        "healing_hook={} healing_mode=shadow-only heal_observed={} heal_invalid={} heal_queue_drops={} heal_exact_duplicates={} heal_content_repeats={} heal_receiver_target_mismatches={} heal_source_missing={} heal_local_source={} heal_local_target={} heal_self={} heal_amount_positive={} heal_amount_zero={} heal_amount_negative={} {}",
        status_name(status()),
        OBSERVED.load(Ordering::Relaxed),
        INVALID.load(Ordering::Relaxed),
        DROPS.load(Ordering::Relaxed),
        EXACT_DUPLICATES.load(Ordering::Relaxed),
        CONTENT_REPEATS.load(Ordering::Relaxed),
        RECEIVER_TARGET_MISMATCHES.load(Ordering::Relaxed),
        SOURCE_MISSING.load(Ordering::Relaxed),
        LOCAL_SOURCE.load(Ordering::Relaxed),
        LOCAL_TARGET.load(Ordering::Relaxed),
        SELF_HEAL.load(Ordering::Relaxed),
        POSITIVE_AMOUNT.load(Ordering::Relaxed),
        ZERO_AMOUNT.load(Ordering::Relaxed),
        NEGATIVE_AMOUNT.load(Ordering::Relaxed),
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

    fn raw(amount: f64, critical: u8, skill: &[u16]) -> RawHealObserved {
        let mut skill_id = [0_u16; MAX_SKILL_ID_CODE_UNITS];
        skill_id[..skill.len()].copy_from_slice(skill);
        RawHealObserved {
            result_pointer: 0x10_000,
            receiver_pointer: 0x20_000,
            server_source_pointer: 0x30_000,
            source_owner_pointer: 0x40_000,
            weak_source: 0,
            target_pointer: 0x20_000,
            amount,
            hit_ordinal: 1,
            critical,
            skill_id_length: skill.len() as u16,
            skill_id,
        }
    }

    #[test]
    fn method_spec_matches_verified_healing_boundary() {
        assert_eq!(DISPLAY_HEAL.name, c"rpcDisplayHeal__impl");
        assert_eq!(DISPLAY_HEAL.arguments, HEAL_ARGUMENTS);
        assert_eq!(
            DISPLAY_HEAL.result,
            HashLinkTypeSpec::Kind(HashLinkKind::Void)
        );
    }

    #[test]
    fn shadow_decoder_preserves_unsettled_amount_signs() {
        let skill = "Priest_Heal".encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            decode_payload(&raw(50.0, 0, &skill)),
            Some(("Priest_Heal".to_owned(), false))
        );
        assert_eq!(
            decode_payload(&raw(0.0, 1, &skill)),
            Some(("Priest_Heal".to_owned(), true))
        );
        assert_eq!(
            decode_payload(&raw(-10.0, 0, &skill)),
            Some(("Priest_Heal".to_owned(), false))
        );
        assert!(decode_payload(&raw(f64::NAN, 0, &skill)).is_none());
        assert!(decode_payload(&raw(1.0, 2, &skill)).is_none());
    }

    #[test]
    fn metric_tokens_do_not_break_the_space_delimited_metrics_line() {
        assert_eq!(metric_token("Priest Heal:Ⅱ"), "Priest_Heal__");
    }
}
