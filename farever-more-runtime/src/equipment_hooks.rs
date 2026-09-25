//! Internal shadow capture for local equipment-slot changes.
//!
//! `Hero.onEquip` is the shared notification reached by Equipment's equip and
//! unequip implementations after proxy membership changes. Equip supplies the
//! new item and slot; unequip supplies a null item and the cleared slot.

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
const MAX_SLOT_CODE_UNITS: usize = 64;
const MAX_ITEM_KIND_CODE_UNITS: usize = 128;

const ON_EQUIP_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ent.Hero"),
    HashLinkTypeSpec::Object("st.Item"),
    HashLinkTypeSpec::Object("String"),
];
const ON_EQUIP: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.Hero",
    name: c"onEquip",
    arguments: ON_EQUIP_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const ITEM_FIELDS: &[HashLinkFieldSpec] = &[HashLinkFieldSpec::object("kind", "String")];
const ITEM_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.Item",
    kind: HashLinkKind::Object,
    fields: ITEM_FIELDS,
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

type HlOnEquip = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);

#[derive(Clone, Copy, Debug)]
struct EquipmentLayout {
    hero_type: usize,
    item_kind: usize,
    string_type: usize,
    string_bytes: usize,
    string_length: usize,
}

#[derive(Clone, Copy, Debug)]
struct RawEquipmentChange {
    hero_pointer: usize,
    equipped: bool,
    slot_length: u16,
    slot: [u16; MAX_SLOT_CODE_UNITS],
    item_kind_length: u16,
    item_kind: [u16; MAX_ITEM_KIND_CODE_UNITS],
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EquipmentSample {
    equipped: bool,
    slot: String,
    item_kind: Option<String>,
}

static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_ON_EQUIP: AtomicUsize = AtomicUsize::new(0);
static LAYOUT: OnceLock<EquipmentLayout> = OnceLock::new();
static RAW: OnceLock<ArrayQueue<RawEquipmentChange>> = OnceLock::new();
static LAST_SAMPLE: OnceLock<Mutex<Option<EquipmentSample>>> = OnceLock::new();

static EQUIPPED: AtomicU64 = AtomicU64::new(0);
static UNEQUIPPED: AtomicU64 = AtomicU64::new(0);
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
    let method = runtime.resolve_method(hl, hero_type, &ON_EQUIP)?;
    let item_type = method
        .argument_type(1)
        .ok_or_else(|| "validated onEquip signature omitted Item".to_owned())?;
    let item = validate_object(hl, item_type, &ITEM_SCHEMA)?;
    let string_type = item
        .field_type_address("kind")
        .ok_or_else(|| "validated Item layout omitted kind type".to_owned())?;
    let slot_type = method
        .argument_type(2)
        .ok_or_else(|| "validated onEquip signature omitted slot String".to_owned())?;
    if string_type != slot_type {
        return Err("onEquip slot and Item.kind use different String types".to_owned());
    }
    let string = validate_object(hl, string_type, &STRING_SCHEMA)?;
    LAYOUT
        .set(EquipmentLayout {
            hero_type,
            item_kind: item
                .offset("kind")
                .ok_or_else(|| "validated Item layout omitted kind".to_owned())?,
            string_type: string.type_address,
            string_bytes: string
                .offset("bytes")
                .ok_or_else(|| "validated String layout omitted bytes".to_owned())?,
            string_length: string
                .offset("length")
                .ok_or_else(|| "validated String layout omitted length".to_owned())?,
        })
        .map_err(|_| "equipment hook layout was already initialized".to_owned())?;
    Ok(method)
}

fn install_hook(method: ValidatedHashLinkMethod) -> Result<(), String> {
    let target = method.target() as *mut c_void;
    // SAFETY: resolution validated `(ent.Hero, st.Item, String) -> Void` and
    // the inherited Item/String layouts copied by the callback.
    let original = std::panic::catch_unwind(|| unsafe {
        MinHook::create_hook(target, hook_on_equip as *mut c_void)
    })
    .map_err(|_| "MinHook initialization panicked for Hero.onEquip".to_owned())?
    .map_err(|status| format!("create Hero.onEquip hook returned {status:?}"))?;
    ORIGINAL_ON_EQUIP.store(original as usize, Ordering::Release);
    // SAFETY: the hook and trampoline were created immediately above.
    if let Err(status) = unsafe { MinHook::enable_hook(target) } {
        // SAFETY: the target is the hook created immediately above.
        let _ = unsafe { MinHook::remove_hook(target) };
        ORIGINAL_ON_EQUIP.store(0, Ordering::Release);
        return Err(format!("enable Hero.onEquip hook returned {status:?}"));
    }
    HOOK_TARGET.store(target as usize, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

unsafe extern "C" fn hook_on_equip(hero: *mut c_void, item: *mut c_void, slot: *mut c_void) {
    let observation = if ACTIVE.load(Ordering::Relaxed) {
        unsafe { copy_observation(hero, item, slot) }
    } else {
        None
    };

    let original = ORIGINAL_ON_EQUIP.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: MinHook returned this trampoline for the validated method.
        let original: HlOnEquip = unsafe { std::mem::transmute(original) };
        unsafe { original(hero, item, slot) };
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

unsafe fn copy_observation(
    hero: *mut c_void,
    item: *mut c_void,
    slot: *mut c_void,
) -> Option<RawEquipmentChange> {
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
    let mut raw = RawEquipmentChange {
        hero_pointer: hero as usize,
        equipped: !item.is_null(),
        slot_length: 0,
        slot: [0; MAX_SLOT_CODE_UNITS],
        item_kind_length: 0,
        item_kind: [0; MAX_ITEM_KIND_CODE_UNITS],
    };
    raw.slot_length = unsafe { copy_string(slot, &mut raw.slot, layout, MAX_SLOT_CODE_UNITS)? };
    if !item.is_null() {
        // The validated st.Item field is inherited at the same offset by all
        // concrete equippable subclasses accepted by this method's ABI.
        let kind = unsafe {
            std::ptr::read_unaligned(item.cast::<u8>().add(layout.item_kind).cast::<usize>())
        } as *mut c_void;
        raw.item_kind_length =
            unsafe { copy_string(kind, &mut raw.item_kind, layout, MAX_ITEM_KIND_CODE_UNITS)? };
    }
    Some(raw)
}

unsafe fn copy_string<const N: usize>(
    string: *mut c_void,
    destination: &mut [u16; N],
    layout: EquipmentLayout,
    limit: usize,
) -> Option<u16> {
    if !unsafe { object_has_exact_type(string, layout.string_type) } {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let base = string.cast::<u8>();
    let length = unsafe { std::ptr::read_unaligned(base.add(layout.string_length).cast::<i32>()) };
    if !(1..=limit as i32).contains(&length) || length as usize > N {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let bytes = unsafe { std::ptr::read_unaligned(base.add(layout.string_bytes).cast::<usize>()) };
    if bytes < 0x1_0000 {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // SAFETY: String layout and positive length are validated and bounded.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes as *const u16,
            destination.as_mut_ptr(),
            length as usize,
        );
    }
    Some(length as u16)
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
        let Some(sample) = decode_sample(&raw) else {
            INVALID.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        if sample.equipped {
            EQUIPPED.fetch_add(1, Ordering::Relaxed);
        } else {
            UNEQUIPPED.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(last) = LAST_SAMPLE.get() {
            if let Ok(mut last) = last.lock() {
                *last = Some(sample);
            }
        }
    }
}

fn decode_sample(raw: &RawEquipmentChange) -> Option<EquipmentSample> {
    let slot_length = usize::from(raw.slot_length);
    if slot_length == 0 || slot_length > raw.slot.len() {
        return None;
    }
    let slot = String::from_utf16(&raw.slot[..slot_length]).ok()?;
    let item_kind = if raw.equipped {
        let length = usize::from(raw.item_kind_length);
        if length == 0 || length > raw.item_kind.len() {
            return None;
        }
        Some(String::from_utf16(&raw.item_kind[..length]).ok()?)
    } else if raw.item_kind_length == 0 {
        None
    } else {
        return None;
    };
    (!slot.is_empty() && item_kind.as_ref().is_none_or(|kind| !kind.is_empty())).then_some(
        EquipmentSample {
            equipped: raw.equipped,
            slot,
            item_kind,
        },
    )
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
            || "equipment_last=unknown".to_owned(),
            |sample| {
                let action = if sample.equipped { "equip" } else { "unequip" };
                let item = sample
                    .item_kind
                    .as_deref()
                    .map_or_else(|| "none".to_owned(), metric_token);
                format!(
                    "equipment_last={action}:{}:{item}",
                    metric_token(&sample.slot)
                )
            },
        );
    format!(
        "equipment_hook={} equipment_mode=shadow-only equipment_equipped={} equipment_unequipped={} equipment_filtered={} equipment_invalid={} equipment_queue_drops={} {}",
        status_name(status()),
        EQUIPPED.load(Ordering::Relaxed),
        UNEQUIPPED.load(Ordering::Relaxed),
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

    fn raw(equipped: bool, slot: &str, item_kind: &str) -> RawEquipmentChange {
        let slot = slot.encode_utf16().collect::<Vec<_>>();
        let item_kind = item_kind.encode_utf16().collect::<Vec<_>>();
        let mut raw = RawEquipmentChange {
            hero_pointer: 0x10_000,
            equipped,
            slot_length: slot.len() as u16,
            slot: [0; MAX_SLOT_CODE_UNITS],
            item_kind_length: item_kind.len() as u16,
            item_kind: [0; MAX_ITEM_KIND_CODE_UNITS],
        };
        raw.slot[..slot.len()].copy_from_slice(&slot);
        raw.item_kind[..item_kind.len()].copy_from_slice(&item_kind);
        raw
    }

    #[test]
    fn method_spec_matches_verified_equipment_notification() {
        assert_eq!(ON_EQUIP.name, c"onEquip");
        assert_eq!(ON_EQUIP.arguments, ON_EQUIP_ARGUMENTS);
        assert_eq!(ON_EQUIP.result, HashLinkTypeSpec::Kind(HashLinkKind::Void));
    }

    #[test]
    fn decoder_distinguishes_equip_and_unequip() {
        assert_eq!(
            decode_sample(&raw(true, "Slot_Weapon1", "Weapon_Sword")),
            Some(EquipmentSample {
                equipped: true,
                slot: "Slot_Weapon1".to_owned(),
                item_kind: Some("Weapon_Sword".to_owned()),
            })
        );
        assert_eq!(
            decode_sample(&raw(false, "Slot_Head", "")),
            Some(EquipmentSample {
                equipped: false,
                slot: "Slot_Head".to_owned(),
                item_kind: None,
            })
        );
    }
}
