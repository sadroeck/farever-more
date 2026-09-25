//! Internal shadow capture for the local Hero's active weapon.
//!
//! `Hero.updateWeaponInHand` resolves temporary skill overrides and the
//! equipped primary weapon before changing `weaponInHand`. The postfix
//! callback copies only an actual old/new pointer transition plus the bounded
//! item kind. Public weapon-state semantics remain deferred until live QA.

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
const MAX_WEAPON_KIND_CODE_UNITS: usize = 128;

const UPDATE_WEAPON_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ent.Hero"),
    HashLinkTypeSpec::Object("st.item.Weapon"),
];
const UPDATE_WEAPON_IN_HAND: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.Hero",
    name: c"updateWeaponInHand",
    arguments: UPDATE_WEAPON_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const HERO_FIELDS: &[HashLinkFieldSpec] =
    &[HashLinkFieldSpec::object("weaponInHand", "st.item.Weapon")];
const HERO_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "ent.Hero",
    kind: HashLinkKind::Object,
    fields: HERO_FIELDS,
};
const WEAPON_FIELDS: &[HashLinkFieldSpec] = &[HashLinkFieldSpec::object("kind", "String")];
const WEAPON_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.item.Weapon",
    kind: HashLinkKind::Object,
    fields: WEAPON_FIELDS,
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

type HlUpdateWeaponInHand = unsafe extern "C" fn(*mut c_void, *mut c_void);

#[derive(Clone, Copy, Debug)]
struct WeaponLayout {
    hero_type: usize,
    weapon_type: usize,
    weapon_in_hand: usize,
    weapon_kind: usize,
    string_type: usize,
    string_bytes: usize,
    string_length: usize,
}

#[derive(Clone, Copy, Debug)]
struct RawWeaponChange {
    hero_pointer: usize,
    previous_pointer: usize,
    weapon_pointer: usize,
    kind_length: u16,
    kind: [u16; MAX_WEAPON_KIND_CODE_UNITS],
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WeaponSample {
    previous_pointer: usize,
    weapon_pointer: usize,
    kind: Option<String>,
}

static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_UPDATE: AtomicUsize = AtomicUsize::new(0);
static LAYOUT: OnceLock<WeaponLayout> = OnceLock::new();
static RAW: OnceLock<ArrayQueue<RawWeaponChange>> = OnceLock::new();
static LAST_SAMPLE: OnceLock<Mutex<Option<WeaponSample>>> = OnceLock::new();

static CHANGES: AtomicU64 = AtomicU64::new(0);
static CLEARED: AtomicU64 = AtomicU64::new(0);
static UNCHANGED: AtomicU64 = AtomicU64::new(0);
static FILTERED: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static DROPS: AtomicU64 = AtomicU64::new(0);

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
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    let method = runtime.resolve_method(hl, hero_type, &UPDATE_WEAPON_IN_HAND)?;
    let hero = validate_object(hl, hero_type, &HERO_SCHEMA)?;
    let weapon_type = hero
        .field_type_address("weaponInHand")
        .ok_or_else(|| "validated Hero layout omitted weaponInHand type".to_owned())?;
    let weapon = validate_object(hl, weapon_type, &WEAPON_SCHEMA)?;
    let string_type = weapon
        .field_type_address("kind")
        .ok_or_else(|| "validated Weapon layout omitted kind type".to_owned())?;
    let string = validate_object(hl, string_type, &STRING_SCHEMA)?;
    LAYOUT
        .set(WeaponLayout {
            hero_type: hero.type_address,
            weapon_type: weapon.type_address,
            weapon_in_hand: hero
                .offset("weaponInHand")
                .ok_or_else(|| "validated Hero layout omitted weaponInHand".to_owned())?,
            weapon_kind: weapon
                .offset("kind")
                .ok_or_else(|| "validated Weapon layout omitted kind".to_owned())?,
            string_type: string.type_address,
            string_bytes: string
                .offset("bytes")
                .ok_or_else(|| "validated String layout omitted bytes".to_owned())?,
            string_length: string
                .offset("length")
                .ok_or_else(|| "validated String layout omitted length".to_owned())?,
        })
        .map_err(|_| "weapon hook layout was already initialized".to_owned())?;
    Ok(method)
}

fn install_hook(method: ValidatedHashLinkMethod) -> Result<(), String> {
    let target = method.target() as *mut c_void;
    // SAFETY: resolution validated `(ent.Hero, st.item.Weapon) -> Void` and
    // every object/string field copied by the callback.
    let original = std::panic::catch_unwind(|| unsafe {
        MinHook::create_hook(target, hook_update_weapon_in_hand as *mut c_void)
    })
    .map_err(|_| "MinHook initialization panicked for updateWeaponInHand".to_owned())?
    .map_err(|status| format!("create updateWeaponInHand hook returned {status:?}"))?;
    ORIGINAL_UPDATE.store(original as usize, Ordering::Release);
    // SAFETY: the hook and trampoline were created immediately above.
    if let Err(status) = unsafe { MinHook::enable_hook(target) } {
        // SAFETY: the target is the hook created immediately above.
        let _ = unsafe { MinHook::remove_hook(target) };
        ORIGINAL_UPDATE.store(0, Ordering::Release);
        return Err(format!(
            "enable updateWeaponInHand hook returned {status:?}"
        ));
    }
    HOOK_TARGET.store(target as usize, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

unsafe extern "C" fn hook_update_weapon_in_hand(hero: *mut c_void, weapon: *mut c_void) {
    let before = if ACTIVE.load(Ordering::Relaxed) {
        unsafe { read_previous_weapon(hero) }
    } else {
        None
    };

    let original = ORIGINAL_UPDATE.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: MinHook returned this trampoline for the validated method.
        let original: HlUpdateWeaponInHand = unsafe { std::mem::transmute(original) };
        unsafe { original(hero, weapon) };
    }

    let observation =
        before.and_then(|(previous, layout)| unsafe { copy_observation(hero, previous, layout) });
    if let Some(observation) = observation {
        if RAW
            .get()
            .is_none_or(|queue| queue.push(observation).is_err())
        {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

unsafe fn read_previous_weapon(hero: *mut c_void) -> Option<(usize, WeaponLayout)> {
    if hero.is_null() || hero as usize != crate::player_hooks::local_hero_pointer() {
        FILTERED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let Some(layout) = LAYOUT.get().copied() else {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if !unsafe { object_has_exact_type(hero, layout.hero_type) } {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let previous = unsafe {
        std::ptr::read_unaligned(hero.cast::<u8>().add(layout.weapon_in_hand).cast::<usize>())
    };
    Some((previous, layout))
}

unsafe fn copy_observation(
    hero: *mut c_void,
    previous: usize,
    layout: WeaponLayout,
) -> Option<RawWeaponChange> {
    let current = unsafe {
        std::ptr::read_unaligned(hero.cast::<u8>().add(layout.weapon_in_hand).cast::<usize>())
    };
    if current == previous {
        UNCHANGED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut raw = RawWeaponChange {
        hero_pointer: hero as usize,
        previous_pointer: previous,
        weapon_pointer: current,
        kind_length: 0,
        kind: [0; MAX_WEAPON_KIND_CODE_UNITS],
    };
    if current == 0 {
        return Some(raw);
    }
    if !unsafe { object_has_exact_type(current as *const c_void, layout.weapon_type) } {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let kind = unsafe {
        std::ptr::read_unaligned(
            (current as *const u8)
                .add(layout.weapon_kind)
                .cast::<usize>(),
        )
    };
    if !unsafe { object_has_exact_type(kind as *const c_void, layout.string_type) } {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let string = kind as *const u8;
    let length =
        unsafe { std::ptr::read_unaligned(string.add(layout.string_length).cast::<i32>()) };
    if !(1..=MAX_WEAPON_KIND_CODE_UNITS as i32).contains(&length) {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let bytes =
        unsafe { std::ptr::read_unaligned(string.add(layout.string_bytes).cast::<usize>()) };
    if bytes < 0x1_0000 {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // SAFETY: the String layout is validated and length is bounded by the
    // fixed destination array.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes as *const u16, raw.kind.as_mut_ptr(), length as usize);
    }
    raw.kind_length = length as u16;
    Some(raw)
}

pub(crate) fn decode_pending() {
    let Some(queue) = RAW.get() else {
        return;
    };
    for _ in 0..MAX_DECODE_PER_TICK {
        let Some(raw) = queue.pop() else {
            break;
        };
        if raw.hero_pointer != crate::player_hooks::local_hero_pointer() {
            FILTERED.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let Some(kind) = decode_kind(&raw) else {
            INVALID.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        CHANGES.fetch_add(1, Ordering::Relaxed);
        if raw.weapon_pointer == 0 {
            CLEARED.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(last) = LAST_SAMPLE.get() {
            if let Ok(mut last) = last.lock() {
                *last = Some(WeaponSample {
                    previous_pointer: raw.previous_pointer,
                    weapon_pointer: raw.weapon_pointer,
                    kind,
                });
            }
        }
    }
}

fn decode_kind(raw: &RawWeaponChange) -> Option<Option<String>> {
    if raw.weapon_pointer == 0 {
        return (raw.kind_length == 0).then_some(None);
    }
    let length = usize::from(raw.kind_length);
    if length == 0 || length > raw.kind.len() {
        return None;
    }
    let value = String::from_utf16(&raw.kind[..length]).ok()?;
    (!value.is_empty()).then_some(Some(value))
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
            || "weapon_last=unknown weapon_last_transition=unknown".to_owned(),
            |sample| {
                let kind = sample
                    .kind
                    .as_deref()
                    .map_or_else(|| "none".to_owned(), metric_token);
                format!(
                    "weapon_last={kind} weapon_last_transition={:x}->{:x}",
                    sample.previous_pointer, sample.weapon_pointer
                )
            },
        );
    format!(
        "weapon_hook={} weapon_mode=shadow-only weapon_changes={} weapon_cleared={} weapon_unchanged={} weapon_filtered={} weapon_invalid={} weapon_queue_drops={} {}",
        status_name(status()),
        CHANGES.load(Ordering::Relaxed),
        CLEARED.load(Ordering::Relaxed),
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

    fn raw(pointer: usize, kind: &[u16]) -> RawWeaponChange {
        let mut value = [0_u16; MAX_WEAPON_KIND_CODE_UNITS];
        value[..kind.len()].copy_from_slice(kind);
        RawWeaponChange {
            hero_pointer: 0x10_000,
            previous_pointer: 0x20_000,
            weapon_pointer: pointer,
            kind_length: kind.len() as u16,
            kind: value,
        }
    }

    #[test]
    fn method_spec_matches_verified_active_weapon_boundary() {
        assert_eq!(UPDATE_WEAPON_IN_HAND.name, c"updateWeaponInHand");
        assert_eq!(UPDATE_WEAPON_IN_HAND.arguments, UPDATE_WEAPON_ARGUMENTS);
        assert_eq!(
            UPDATE_WEAPON_IN_HAND.result,
            HashLinkTypeSpec::Kind(HashLinkKind::Void)
        );
    }

    #[test]
    fn decoder_preserves_weapon_kind_and_clear_transition() {
        let kind = "Weapon_Sword".encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            decode_kind(&raw(0x30_000, &kind)),
            Some(Some("Weapon_Sword".to_owned()))
        );
        assert_eq!(decode_kind(&raw(0, &[])), Some(None));
        assert_eq!(decode_kind(&raw(0x30_000, &[])), None);
    }

    #[test]
    fn metric_tokens_do_not_break_the_space_delimited_metrics_line() {
        assert_eq!(metric_token("Weapon Sword:Ⅱ"), "Weapon_Sword__");
    }
}
