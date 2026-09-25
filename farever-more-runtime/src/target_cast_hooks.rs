//! Internal shadow capture for target skill/cast lifecycles.
//!
//! A successful `GameObject.doUseSkill` call starts observation only when its
//! receiver occupies one of the hook-maintained local target slots. The
//! client `BaseSkill.rpcStop__impl` boundary closes the same skill pointer and
//! preserves the stop reason, including interruption.

use crate::hashlink::{
    object_has_exact_type, validate_object, HashLink, HashLinkFieldSpec, HashLinkKind,
    HashLinkMethodSpec, HashLinkObjectSpec, HashLinkRuntime, HashLinkTypeSpec,
    ValidatedHashLinkMethod,
};
use crossbeam_queue::ArrayQueue;
use minhook::MinHook;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

const RAW_QUEUE_CAPACITY: usize = 512;
const MAX_DECODE_PER_TICK: usize = 64;
const MAX_ACTIVE_CASTS: usize = 16;
const MAX_SKILL_KIND_CODE_UNITS: usize = 128;
const STOP_REASON_INTERRUPT: u8 = 3;

const DO_USE_SKILL_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ent.GameObject"),
    HashLinkTypeSpec::Object("st.skill.Skill"),
    HashLinkTypeSpec::Enum("st.skill.SkillTarget"),
    HashLinkTypeSpec::Object("st.Item"),
];
const DO_USE_SKILL: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.GameObject",
    name: c"doUseSkill",
    arguments: DO_USE_SKILL_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const STOP_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.skill.BaseSkill"),
    HashLinkTypeSpec::Enum("st.skill.SkillStopReason"),
];
const RPC_STOP_IMPL: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.skill.BaseSkill",
    name: c"rpcStop__impl",
    arguments: STOP_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const SKILL_FIELDS: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::object("kind", "String"),
    HashLinkFieldSpec::object("owner", "ent.GameObject"),
    HashLinkFieldSpec::object("runningCtx", "st.skill.SkillContext"),
];
const SKILL_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.skill.Skill",
    kind: HashLinkKind::Object,
    fields: SKILL_FIELDS,
};
const BASE_SKILL_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.skill.BaseSkill",
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

type HlDoUseSkill = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void);
type HlRpcStop = unsafe extern "C" fn(*mut c_void, *mut c_void);

#[derive(Clone, Copy, Debug)]
struct CastLayout {
    kind: usize,
    owner: usize,
    running_context: usize,
    string_type: usize,
    string_bytes: usize,
    string_length: usize,
    stop_reason_type: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawKind {
    Started,
    Stopped,
}

#[derive(Clone, Copy, Debug)]
struct RawTargetCast {
    kind: RawKind,
    actor_pointer: usize,
    skill_pointer: usize,
    skill_type: usize,
    target_slot_mask: u8,
    stop_reason: u8,
    skill_kind_length: u16,
    skill_kind: [u16; MAX_SKILL_KIND_CODE_UNITS],
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CastSample {
    kind: RawKind,
    skill_kind: String,
    target_slot_mask: u8,
    stop_reason: Option<u8>,
}

#[derive(Clone, Copy, Debug)]
struct ActiveCast {
    actor_pointer: usize,
    skill_pointer: usize,
    skill_type: usize,
    target_slot_mask: u8,
}

#[derive(Default)]
pub(crate) struct TargetCastHookDecoder {
    active: Vec<ActiveCast>,
}

struct ResolvedHooks {
    start: ValidatedHashLinkMethod,
    stop: ValidatedHashLinkMethod,
}

static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static START_TARGET: AtomicUsize = AtomicUsize::new(0);
static STOP_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_START: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_STOP: AtomicUsize = AtomicUsize::new(0);
static LAYOUT: OnceLock<CastLayout> = OnceLock::new();
static RAW: OnceLock<ArrayQueue<RawTargetCast>> = OnceLock::new();
static LAST_SAMPLE: OnceLock<Mutex<Option<CastSample>>> = OnceLock::new();

static STARTS: AtomicU64 = AtomicU64::new(0);
static ENDS: AtomicU64 = AtomicU64::new(0);
static INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static UNMATCHED_STOPS: AtomicU64 = AtomicU64::new(0);
static FILTERED: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static DROPS: AtomicU64 = AtomicU64::new(0);
static ACTIVE_OVERFLOW: AtomicU64 = AtomicU64::new(0);

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
    let start = runtime.resolve_method(hl, game_object_type, &DO_USE_SKILL)?;
    let skill_type = start
        .argument_type(1)
        .ok_or_else(|| "validated doUseSkill signature omitted Skill".to_owned())?;
    let base_skill_type = hl
        .type_address_named(skill_type, "st.skill.BaseSkill")
        .ok_or_else(|| "st.skill.Skill does not inherit BaseSkill".to_owned())?;
    let stop = runtime.resolve_method(hl, base_skill_type, &RPC_STOP_IMPL)?;
    let stop_reason_type = stop
        .argument_type(1)
        .ok_or_else(|| "validated rpcStop__impl signature omitted stop reason".to_owned())?;

    let skill = validate_object(hl, skill_type, &SKILL_SCHEMA)?;
    let base_skill = validate_object(hl, base_skill_type, &BASE_SKILL_SCHEMA)?;
    let offset = |shape: &crate::hashlink::ValidatedHashLinkObject, name| {
        shape
            .offset(name)
            .ok_or_else(|| format!("validated skill layout omitted {name}"))
    };
    let skill_offsets = [
        offset(&skill, "kind")?,
        offset(&skill, "owner")?,
        offset(&skill, "runningCtx")?,
    ];
    let base_offsets = [
        offset(&base_skill, "kind")?,
        offset(&base_skill, "owner")?,
        offset(&base_skill, "runningCtx")?,
    ];
    if skill_offsets != base_offsets {
        return Err("Skill and BaseSkill inherited field offsets differ".to_owned());
    }
    let string_type = skill
        .field_type_address("kind")
        .ok_or_else(|| "validated Skill layout omitted kind type".to_owned())?;
    let string = validate_object(hl, string_type, &STRING_SCHEMA)?;
    LAYOUT
        .set(CastLayout {
            kind: skill_offsets[0],
            owner: skill_offsets[1],
            running_context: skill_offsets[2],
            string_type: string.type_address,
            string_bytes: string
                .offset("bytes")
                .ok_or_else(|| "validated String layout omitted bytes".to_owned())?,
            string_length: string
                .offset("length")
                .ok_or_else(|| "validated String layout omitted length".to_owned())?,
            stop_reason_type,
        })
        .map_err(|_| "target-cast layout was already initialized".to_owned())?;
    Ok(ResolvedHooks { start, stop })
}

fn install_hooks(hooks: &ResolvedHooks) -> Result<(), String> {
    let targets = [
        hooks.start.target() as *mut c_void,
        hooks.stop.target() as *mut c_void,
    ];
    if targets[0] == targets[1] {
        return Err("target-cast methods resolved to one target".to_owned());
    }
    let detours = [
        hook_do_use_skill as *mut c_void,
        hook_rpc_stop as *mut c_void,
    ];
    let names = ["doUseSkill", "rpcStop__impl"];
    let mut originals = [0_usize; 2];
    for index in 0..targets.len() {
        let original = std::panic::catch_unwind(|| unsafe {
            MinHook::create_hook(targets[index], detours[index])
        })
        .map_err(|_| format!("MinHook initialization panicked for {}", names[index]))?
        .map_err(|status| format!("create {} hook returned {status:?}", names[index]));
        match original {
            Ok(original) => originals[index] = original as usize,
            Err(error) => {
                for target in targets[..index].iter().copied() {
                    // SAFETY: only successfully created hooks are removed.
                    let _ = unsafe { MinHook::remove_hook(target) };
                }
                return Err(error);
            }
        }
    }
    ORIGINAL_START.store(originals[0], Ordering::Release);
    ORIGINAL_STOP.store(originals[1], Ordering::Release);
    for index in 0..targets.len() {
        // SAFETY: both exact callback ABIs were validated before creation.
        if let Err(status) = unsafe { MinHook::enable_hook(targets[index]) } {
            for target in targets[..index].iter().copied() {
                // SAFETY: only the successfully enabled prefix is disabled.
                let _ = unsafe { MinHook::disable_hook(target) };
            }
            for target in targets {
                // SAFETY: both hooks were successfully created.
                let _ = unsafe { MinHook::remove_hook(target) };
            }
            ORIGINAL_START.store(0, Ordering::Release);
            ORIGINAL_STOP.store(0, Ordering::Release);
            return Err(format!("enable {} hook returned {status:?}", names[index]));
        }
    }
    START_TARGET.store(targets[0] as usize, Ordering::Release);
    STOP_TARGET.store(targets[1] as usize, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

unsafe extern "C" fn hook_do_use_skill(
    actor: *mut c_void,
    skill: *mut c_void,
    target: *mut c_void,
    item: *mut c_void,
) {
    let observation = if ACTIVE.load(Ordering::Relaxed) {
        unsafe { copy_start(actor, skill) }
    } else {
        None
    };
    let original = ORIGINAL_START.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: MinHook returned this trampoline for the validated ABI.
        let original: HlDoUseSkill = unsafe { std::mem::transmute(original) };
        unsafe { original(actor, skill, target, item) };
    }
    if let Some(observation) = observation {
        queue(observation);
    }
}

unsafe extern "C" fn hook_rpc_stop(skill: *mut c_void, reason: *mut c_void) {
    let observation = if ACTIVE.load(Ordering::Relaxed) {
        unsafe { copy_stop(skill, reason) }
    } else {
        None
    };
    let original = ORIGINAL_STOP.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: MinHook returned this trampoline for the validated ABI.
        let original: HlRpcStop = unsafe { std::mem::transmute(original) };
        unsafe { original(skill, reason) };
    }
    if let Some(observation) = observation {
        let transitioned = LAYOUT.get().is_some_and(|layout| {
            let after = unsafe {
                std::ptr::read_unaligned(
                    skill
                        .cast::<u8>()
                        .add(layout.running_context)
                        .cast::<usize>(),
                )
            };
            after == 0
        });
        if transitioned {
            queue(observation);
        }
    }
}

unsafe fn copy_start(actor: *mut c_void, skill: *mut c_void) -> Option<RawTargetCast> {
    let target_slot_mask = target_slot_mask(actor as usize);
    if target_slot_mask == 0 {
        FILTERED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut raw = unsafe { copy_skill(actor as usize, skill as usize) }?;
    raw.kind = RawKind::Started;
    raw.target_slot_mask = target_slot_mask;
    Some(raw)
}

unsafe fn copy_stop(skill: *mut c_void, reason: *mut c_void) -> Option<RawTargetCast> {
    let Some(layout) = LAYOUT.get().copied() else {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if skill.is_null() {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let running_context = unsafe {
        std::ptr::read_unaligned(
            skill
                .cast::<u8>()
                .add(layout.running_context)
                .cast::<usize>(),
        )
    };
    if running_context < 0x1_0000 {
        return None;
    }
    let owner =
        unsafe { std::ptr::read_unaligned(skill.cast::<u8>().add(layout.owner).cast::<usize>()) };
    let mut raw = unsafe { copy_skill(owner, skill as usize) }?;
    raw.kind = RawKind::Stopped;
    raw.target_slot_mask = target_slot_mask(owner);
    raw.stop_reason = unsafe { stop_reason_index(reason, layout) }?;
    Some(raw)
}

unsafe fn copy_skill(actor: usize, skill: usize) -> Option<RawTargetCast> {
    let Some(layout) = LAYOUT.get().copied() else {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if actor < 0x1_0000 || skill < 0x1_0000 {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let skill_base = skill as *const u8;
    let owner = unsafe { std::ptr::read_unaligned(skill_base.add(layout.owner).cast::<usize>()) };
    if owner != actor {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let skill_type = unsafe { std::ptr::read_unaligned(skill_base.cast::<usize>()) };
    if skill_type < 0x1_0000 {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut skill_kind = [0_u16; MAX_SKILL_KIND_CODE_UNITS];
    let skill_kind_length = unsafe { copy_kind(skill, layout, &mut skill_kind) };
    if skill_kind_length == 0 {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    Some(RawTargetCast {
        kind: RawKind::Started,
        actor_pointer: actor,
        skill_pointer: skill,
        skill_type,
        target_slot_mask: 0,
        stop_reason: u8::MAX,
        skill_kind_length,
        skill_kind,
    })
}

unsafe fn copy_kind(
    skill: usize,
    layout: CastLayout,
    destination: &mut [u16; MAX_SKILL_KIND_CODE_UNITS],
) -> u16 {
    let string =
        unsafe { std::ptr::read_unaligned((skill as *const u8).add(layout.kind).cast::<usize>()) };
    if string < 0x1_0000
        || !unsafe { object_has_exact_type(string as *const c_void, layout.string_type) }
    {
        return 0;
    }
    let string_base = string as *const u8;
    let length =
        unsafe { std::ptr::read_unaligned(string_base.add(layout.string_length).cast::<i32>()) };
    if !(1..=MAX_SKILL_KIND_CODE_UNITS as i32).contains(&length) {
        return 0;
    }
    let bytes =
        unsafe { std::ptr::read_unaligned(string_base.add(layout.string_bytes).cast::<usize>()) };
    if bytes < 0x1_0000 {
        return 0;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes as *const u16,
            destination.as_mut_ptr(),
            length as usize,
        );
    }
    length as u16
}

unsafe fn stop_reason_index(reason: *mut c_void, layout: CastLayout) -> Option<u8> {
    if !unsafe { object_has_exact_type(reason, layout.stop_reason_type) } {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let index = unsafe {
        std::ptr::read_unaligned(reason.cast::<u8>().add(size_of::<usize>()).cast::<i32>())
    };
    u8::try_from(index).ok().filter(|index| *index <= 6)
}

fn target_slot_mask(actor: usize) -> u8 {
    crate::combat_hooks::current().map_or(0, |state| {
        state
            .references
            .iter()
            .enumerate()
            .fold(0_u8, |mask, (index, reference)| {
                if *reference == Some(actor) {
                    mask | (1 << index)
                } else {
                    mask
                }
            })
    })
}

fn queue(raw: RawTargetCast) {
    if RAW.get().is_none_or(|queue| queue.push(raw).is_err()) {
        DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

impl TargetCastHookDecoder {
    pub(crate) fn decode_pending(&mut self, hl: &HashLink<'_>) {
        let Some(queue) = RAW.get() else {
            return;
        };
        for _ in 0..MAX_DECODE_PER_TICK {
            let Some(raw) = queue.pop() else {
                break;
            };
            self.decode(raw, |skill_type| hl.type_is_a(skill_type, "st.skill.Skill"));
        }
    }

    fn decode(&mut self, raw: RawTargetCast, is_skill_type: impl FnOnce(usize) -> bool) {
        if !is_skill_type(raw.skill_type) {
            FILTERED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(skill_kind) = decode_kind(&raw) else {
            INVALID.fetch_add(1, Ordering::Relaxed);
            return;
        };
        match raw.kind {
            RawKind::Started if raw.target_slot_mask != 0 => {
                if let Some(active) = self
                    .active
                    .iter_mut()
                    .find(|active| active.skill_pointer == raw.skill_pointer)
                {
                    *active = ActiveCast {
                        actor_pointer: raw.actor_pointer,
                        skill_pointer: raw.skill_pointer,
                        skill_type: raw.skill_type,
                        target_slot_mask: raw.target_slot_mask,
                    };
                } else if self.active.len() < MAX_ACTIVE_CASTS {
                    self.active.push(ActiveCast {
                        actor_pointer: raw.actor_pointer,
                        skill_pointer: raw.skill_pointer,
                        skill_type: raw.skill_type,
                        target_slot_mask: raw.target_slot_mask,
                    });
                } else {
                    ACTIVE_OVERFLOW.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                STARTS.fetch_add(1, Ordering::Relaxed);
                record_sample(CastSample {
                    kind: raw.kind,
                    skill_kind,
                    target_slot_mask: raw.target_slot_mask,
                    stop_reason: None,
                });
            }
            RawKind::Started => {
                FILTERED.fetch_add(1, Ordering::Relaxed);
            }
            RawKind::Stopped => {
                let Some(index) = self.active.iter().position(|active| {
                    active.skill_pointer == raw.skill_pointer
                        && active.skill_type == raw.skill_type
                        && active.actor_pointer == raw.actor_pointer
                }) else {
                    UNMATCHED_STOPS.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                let active = self.active.swap_remove(index);
                ENDS.fetch_add(1, Ordering::Relaxed);
                if raw.stop_reason == STOP_REASON_INTERRUPT {
                    INTERRUPTS.fetch_add(1, Ordering::Relaxed);
                }
                record_sample(CastSample {
                    kind: raw.kind,
                    skill_kind,
                    target_slot_mask: active.target_slot_mask,
                    stop_reason: Some(raw.stop_reason),
                });
            }
        }
    }
}

fn decode_kind(raw: &RawTargetCast) -> Option<String> {
    let length = usize::from(raw.skill_kind_length);
    if length == 0 || length > raw.skill_kind.len() {
        return None;
    }
    let value = String::from_utf16(&raw.skill_kind[..length]).ok()?;
    (!value.is_empty()).then_some(value)
}

fn record_sample(sample: CastSample) {
    if let Some(last) = LAST_SAMPLE.get() {
        if let Ok(mut last) = last.lock() {
            *last = Some(sample);
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
            || "target_cast_last=none".to_owned(),
            |sample| {
                format!(
                    "target_cast_last={}:{}:{}:{}",
                    match sample.kind {
                        RawKind::Started => "start",
                        RawKind::Stopped => "stop",
                    },
                    metric_token(&sample.skill_kind),
                    sample.target_slot_mask,
                    sample.stop_reason.map_or(-1, i16::from),
                )
            },
        );
    format!(
        "target_cast_hooks={} target_cast_mode=shadow-only target_cast_starts={} target_cast_ends={} target_cast_interrupts={} target_cast_unmatched_stops={} target_cast_filtered={} target_cast_invalid={} target_cast_queue_drops={} target_cast_active_overflow={} {}",
        status_name(status()),
        STARTS.load(Ordering::Relaxed),
        ENDS.load(Ordering::Relaxed),
        INTERRUPTS.load(Ordering::Relaxed),
        UNMATCHED_STOPS.load(Ordering::Relaxed),
        FILTERED.load(Ordering::Relaxed),
        INVALID.load(Ordering::Relaxed),
        DROPS.load(Ordering::Relaxed),
        ACTIVE_OVERFLOW.load(Ordering::Relaxed),
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
    for target in [
        START_TARGET.load(Ordering::Acquire),
        STOP_TARGET.load(Ordering::Acquire),
    ] {
        if target != 0 {
            // SAFETY: targets are published only after successful enablement.
            let _ = unsafe { MinHook::disable_hook(target as *mut c_void) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(kind: RawKind, skill: usize, actor: usize, reason: u8) -> RawTargetCast {
        let encoded = "Foe_Cast".encode_utf16().collect::<Vec<_>>();
        let mut skill_kind = [0_u16; MAX_SKILL_KIND_CODE_UNITS];
        skill_kind[..encoded.len()].copy_from_slice(&encoded);
        RawTargetCast {
            kind,
            actor_pointer: actor,
            skill_pointer: skill,
            skill_type: 0x30_000,
            target_slot_mask: if kind == RawKind::Started { 2 } else { 0 },
            stop_reason: reason,
            skill_kind_length: encoded.len() as u16,
            skill_kind,
        }
    }

    #[test]
    fn method_specs_match_verified_cast_boundaries() {
        assert_eq!(DO_USE_SKILL.name, c"doUseSkill");
        assert_eq!(DO_USE_SKILL.arguments, DO_USE_SKILL_ARGUMENTS);
        assert_eq!(RPC_STOP_IMPL.name, c"rpcStop__impl");
        assert_eq!(RPC_STOP_IMPL.arguments, STOP_ARGUMENTS);
    }

    #[test]
    fn tracker_correlates_stop_after_target_membership_changes() {
        let mut decoder = TargetCastHookDecoder::default();
        decoder.decode(raw(RawKind::Started, 0x40_000, 0x50_000, u8::MAX), |_| true);
        assert_eq!(decoder.active.len(), 1);
        decoder.decode(
            raw(RawKind::Stopped, 0x40_000, 0x50_000, STOP_REASON_INTERRUPT),
            |_| true,
        );
        assert!(decoder.active.is_empty());
    }

    #[test]
    fn non_skill_base_objects_are_filtered() {
        let mut decoder = TargetCastHookDecoder::default();
        decoder.decode(raw(RawKind::Started, 0x40_000, 0x50_000, u8::MAX), |_| {
            false
        });
        assert!(decoder.active.is_empty());
    }
}
