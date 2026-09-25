//! Internal shadow capture for kills credited to the local player.
//!
//! Farever routes the credited unit kind through a Player-targeted RPC. The
//! callback copies only that bounded identifier; public kill-event semantics
//! remain deferred until live QA establishes coverage for assists, summons,
//! parties, and non-foe units.

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

const RAW_QUEUE_CAPACITY: usize = 256;
const MAX_DECODE_PER_TICK: usize = 64;
const MAX_UNIT_KIND_CODE_UNITS: usize = 128;

const KILL_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.Player"),
    HashLinkTypeSpec::Object("String"),
];
const NOTIFY_UNIT_KILLED: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.Player",
    name: c"notifyUnitKilled__impl",
    arguments: KILL_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
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

type HlNotifyUnitKilled = unsafe extern "C" fn(*mut c_void, *mut c_void);

#[derive(Clone, Copy, Debug)]
struct KillLayout {
    string_type: usize,
    string_bytes: usize,
    string_length: usize,
}

#[derive(Clone, Copy, Debug)]
struct RawKill {
    player_pointer: usize,
    unit_kind_length: u16,
    unit_kind: [u16; MAX_UNIT_KIND_CODE_UNITS],
}

static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_NOTIFY_UNIT_KILLED: AtomicUsize = AtomicUsize::new(0);
static LAYOUT: OnceLock<KillLayout> = OnceLock::new();
static RAW: OnceLock<ArrayQueue<RawKill>> = OnceLock::new();
static LAST_UNIT_KIND: OnceLock<Mutex<Option<String>>> = OnceLock::new();

static OBSERVED: AtomicU64 = AtomicU64::new(0);
static FILTERED: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static DROPS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn prepare_queue() {
    let _ = RAW.get_or_init(|| ArrayQueue::new(RAW_QUEUE_CAPACITY));
    let _ = LAST_UNIT_KIND.get_or_init(|| Mutex::new(None));
}

pub(crate) fn try_install_hook(hl: &HashLink<'_>) {
    if HOOK_STATUS.load(Ordering::Acquire) != 0 {
        return;
    }
    let Some(player_type) = crate::player_hooks::player_type() else {
        return;
    };
    if HOOK_STATUS
        .compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let result = resolve_hook(hl, player_type).and_then(install_hook);
    match result {
        Ok(()) => HOOK_STATUS.store(1, Ordering::Release),
        Err(error) => {
            let _ = HOOK_ERROR.set(error);
            HOOK_STATUS.store(3, Ordering::Release);
        }
    }
}

fn resolve_hook(hl: &HashLink<'_>, player_type: usize) -> Result<ValidatedHashLinkMethod, String> {
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    let method = runtime.resolve_method(hl, player_type, &NOTIFY_UNIT_KILLED)?;
    let string_type = method
        .argument_type(1)
        .ok_or_else(|| "validated kill signature omitted String".to_owned())?;
    let string = validate_object(hl, string_type, &STRING_SCHEMA)?;
    LAYOUT
        .set(KillLayout {
            string_type: string.type_address,
            string_bytes: string
                .offset("bytes")
                .ok_or_else(|| "validated String layout omitted bytes".to_owned())?,
            string_length: string
                .offset("length")
                .ok_or_else(|| "validated String layout omitted length".to_owned())?,
        })
        .map_err(|_| "kill hook layout was already initialized".to_owned())?;
    Ok(method)
}

fn install_hook(method: ValidatedHashLinkMethod) -> Result<(), String> {
    let target = method.target() as *mut c_void;
    // SAFETY: resolution validated `(st.Player, String) -> Void` and the
    // complete String layout copied by the callback.
    let original = std::panic::catch_unwind(|| unsafe {
        MinHook::create_hook(target, hook_notify_unit_killed as *mut c_void)
    })
    .map_err(|_| "MinHook initialization panicked for notifyUnitKilled__impl".to_owned())?
    .map_err(|status| format!("create notifyUnitKilled__impl hook returned {status:?}"))?;
    ORIGINAL_NOTIFY_UNIT_KILLED.store(original as usize, Ordering::Release);
    // SAFETY: the hook and trampoline were created immediately above.
    if let Err(status) = unsafe { MinHook::enable_hook(target) } {
        // SAFETY: the target is the hook created immediately above.
        let _ = unsafe { MinHook::remove_hook(target) };
        ORIGINAL_NOTIFY_UNIT_KILLED.store(0, Ordering::Release);
        return Err(format!(
            "enable notifyUnitKilled__impl hook returned {status:?}"
        ));
    }
    HOOK_TARGET.store(target as usize, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

unsafe extern "C" fn hook_notify_unit_killed(player: *mut c_void, kind: *mut c_void) {
    let observation = if ACTIVE.load(Ordering::Relaxed) {
        unsafe { copy_observation(player, kind) }
    } else {
        None
    };

    let original = ORIGINAL_NOTIFY_UNIT_KILLED.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: MinHook returned this trampoline for the validated method.
        let original: HlNotifyUnitKilled = unsafe { std::mem::transmute(original) };
        unsafe { original(player, kind) };
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

unsafe fn copy_observation(player: *mut c_void, kind: *mut c_void) -> Option<RawKill> {
    if player.is_null() || player as usize != crate::player_hooks::local_player_pointer() {
        FILTERED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let Some(layout) = LAYOUT.get().copied() else {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if !unsafe { object_has_exact_type(kind, layout.string_type) } {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let string_base = kind.cast::<u8>();
    let length =
        unsafe { std::ptr::read_unaligned(string_base.add(layout.string_length).cast::<i32>()) };
    if !(1..=MAX_UNIT_KIND_CODE_UNITS as i32).contains(&length) {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let bytes =
        unsafe { std::ptr::read_unaligned(string_base.add(layout.string_bytes).cast::<usize>()) };
    if bytes < 0x1_0000 {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut unit_kind = [0_u16; MAX_UNIT_KIND_CODE_UNITS];
    // SAFETY: the String layout is validated and length is bounded by the
    // fixed destination array.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes as *const u16, unit_kind.as_mut_ptr(), length as usize);
    }
    Some(RawKill {
        player_pointer: player as usize,
        unit_kind_length: length as u16,
        unit_kind,
    })
}

pub(crate) fn decode_pending() {
    let Some(queue) = RAW.get() else {
        return;
    };
    for _ in 0..MAX_DECODE_PER_TICK {
        let Some(raw) = queue.pop() else {
            break;
        };
        if raw.player_pointer != crate::player_hooks::local_player_pointer() {
            FILTERED.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let Some(unit_kind) = decode_unit_kind(&raw) else {
            INVALID.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        OBSERVED.fetch_add(1, Ordering::Relaxed);
        if let Some(last) = LAST_UNIT_KIND.get() {
            if let Ok(mut last) = last.lock() {
                *last = Some(unit_kind);
            }
        }
    }
}

fn decode_unit_kind(raw: &RawKill) -> Option<String> {
    let length = usize::from(raw.unit_kind_length);
    if length == 0 || length > raw.unit_kind.len() {
        return None;
    }
    let value = String::from_utf16(&raw.unit_kind[..length]).ok()?;
    (!value.is_empty()).then_some(value)
}

pub(crate) fn status() -> usize {
    HOOK_STATUS.load(Ordering::Acquire)
}

pub(crate) fn status_name(status: usize) -> &'static str {
    match status {
        0 => "waiting-for-player-metadata",
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
    let last = LAST_UNIT_KIND
        .get()
        .and_then(|last| last.lock().ok()?.clone())
        .map_or_else(
            || "kill_last=none".to_owned(),
            |kind| format!("kill_last={}", metric_token(&kind)),
        );
    format!(
        "kill_hook={} kill_mode=shadow-only kill_observed={} kill_filtered={} kill_invalid={} kill_queue_drops={} {}",
        status_name(status()),
        OBSERVED.load(Ordering::Relaxed),
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

    fn raw(kind: &[u16]) -> RawKill {
        let mut unit_kind = [0_u16; MAX_UNIT_KIND_CODE_UNITS];
        unit_kind[..kind.len()].copy_from_slice(kind);
        RawKill {
            player_pointer: 0x10_000,
            unit_kind_length: kind.len() as u16,
            unit_kind,
        }
    }

    #[test]
    fn method_spec_matches_verified_player_rpc_boundary() {
        assert_eq!(NOTIFY_UNIT_KILLED.name, c"notifyUnitKilled__impl");
        assert_eq!(NOTIFY_UNIT_KILLED.arguments, KILL_ARGUMENTS);
        assert_eq!(
            NOTIFY_UNIT_KILLED.result,
            HashLinkTypeSpec::Kind(HashLinkKind::Void)
        );
    }

    #[test]
    fn decoder_preserves_one_credited_unit_kind_per_callback() {
        let kind = "Foe_Wolf".encode_utf16().collect::<Vec<_>>();
        assert_eq!(decode_unit_kind(&raw(&kind)), Some("Foe_Wolf".to_owned()));
        assert!(decode_unit_kind(&raw(&[])).is_none());
    }

    #[test]
    fn metric_tokens_do_not_break_the_space_delimited_metrics_line() {
        assert_eq!(metric_token("Foe Wolf:Ⅱ"), "Foe_Wolf__");
    }
}
