//! Internal shadow capture for local inventory mutations.
//!
//! Inventory content can change through a full replicated replacement, a slot
//! assignment/removal, or an in-place stack-count write. These three exact
//! mutation boundaries cover the state without traversing the inventory from
//! a game-thread callback.

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
const MAX_ITEM_KIND_CODE_UNITS: usize = 128;

const SET_CONTENT_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.Inventory"),
    HashLinkTypeSpec::Kind(HashLinkKind::Array),
];
const SET_CONTENT_BETA_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.Inventory"),
    HashLinkTypeSpec::Object("hl.types.ArrayObj"),
];
const SET_CONTENT: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.Inventory",
    name: c"set_content",
    arguments: SET_CONTENT_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Array),
};
const SET_CONTENT_BETA: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.Inventory",
    name: c"set_content",
    arguments: SET_CONTENT_BETA_ARGUMENTS,
    result: HashLinkTypeSpec::Object("hl.types.ArrayObj"),
};
const SET_INDEX_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.Inventory"),
    HashLinkTypeSpec::Kind(HashLinkKind::I32),
    HashLinkTypeSpec::Object("st.Item"),
    HashLinkTypeSpec::Reference(HashLinkKind::I32),
];
const SET_INDEX_IMPL: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.Inventory",
    name: c"setIndex__impl",
    arguments: SET_INDEX_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const SET_COUNT_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.Inventory"),
    HashLinkTypeSpec::Kind(HashLinkKind::I32),
    HashLinkTypeSpec::Kind(HashLinkKind::I32),
];
const DO_SET_STACK_COUNT: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.Inventory",
    name: c"doSetStackCount",
    arguments: SET_COUNT_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const HERO_FIELDS: &[HashLinkFieldSpec] = &[HashLinkFieldSpec::object("loadout", "st.Loadout")];
const HERO_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "ent.Hero",
    kind: HashLinkKind::Object,
    fields: HERO_FIELDS,
};
const LOADOUT_FIELDS: &[HashLinkFieldSpec] =
    &[HashLinkFieldSpec::object("inventory", "st.Inventory")];
const LOADOUT_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.Loadout",
    kind: HashLinkKind::Object,
    fields: LOADOUT_FIELDS,
};
const INVENTORY_FIELDS: &[HashLinkFieldSpec] = &[HashLinkFieldSpec::object("owner", "ent.Unit")];
const INVENTORY_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.Inventory",
    kind: HashLinkKind::Object,
    fields: INVENTORY_FIELDS,
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

type HlSetContent = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
type HlSetIndex = unsafe extern "C" fn(*mut c_void, i32, *mut c_void, *mut i32);
type HlSetStackCount = unsafe extern "C" fn(*mut c_void, i32, i32);

#[derive(Clone, Copy, Debug)]
struct InventoryLayout {
    inventory_type: usize,
    owner: usize,
    item_kind: usize,
    string_type: usize,
    string_bytes: usize,
    string_length: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawKind {
    ContentReplaced,
    SlotSet,
    CountSet,
}

#[derive(Clone, Copy, Debug)]
struct RawInventoryChange {
    kind: RawKind,
    inventory_pointer: usize,
    index: i32,
    count: i32,
    item_present: bool,
    item_kind_length: u16,
    item_kind: [u16; MAX_ITEM_KIND_CODE_UNITS],
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InventorySample {
    kind: RawKind,
    index: Option<i32>,
    count: Option<i32>,
    item_kind: Option<String>,
}

struct ResolvedHooks {
    content: ValidatedHashLinkMethod,
    index: ValidatedHashLinkMethod,
    count: ValidatedHashLinkMethod,
}

static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static CONTENT_TARGET: AtomicUsize = AtomicUsize::new(0);
static INDEX_TARGET: AtomicUsize = AtomicUsize::new(0);
static COUNT_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_CONTENT: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_INDEX: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_COUNT: AtomicUsize = AtomicUsize::new(0);
static LAYOUT: OnceLock<InventoryLayout> = OnceLock::new();
static RAW: OnceLock<ArrayQueue<RawInventoryChange>> = OnceLock::new();
static LAST_SAMPLE: OnceLock<Mutex<Option<InventorySample>>> = OnceLock::new();

static CONTENT_REPLACED: AtomicU64 = AtomicU64::new(0);
static SLOT_SET: AtomicU64 = AtomicU64::new(0);
static COUNT_SET: AtomicU64 = AtomicU64::new(0);
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
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    let hero = validate_object(hl, hero_type, &HERO_SCHEMA)?;
    let loadout_type = hero
        .field_type_address("loadout")
        .ok_or_else(|| "validated Hero layout omitted loadout type".to_owned())?;
    let loadout = validate_object(hl, loadout_type, &LOADOUT_SCHEMA)?;
    let inventory_type = loadout
        .field_type_address("inventory")
        .ok_or_else(|| "validated Loadout layout omitted inventory type".to_owned())?;
    let inventory = validate_object(hl, inventory_type, &INVENTORY_SCHEMA)?;
    let content_spec = if profile.is_beta() {
        &SET_CONTENT_BETA
    } else {
        &SET_CONTENT
    };
    let content = runtime.resolve_method(hl, inventory_type, content_spec)?;
    let index = runtime.resolve_method(hl, inventory_type, &SET_INDEX_IMPL)?;
    let count = runtime.resolve_method(hl, inventory_type, &DO_SET_STACK_COUNT)?;
    let item_type = index
        .argument_type(2)
        .ok_or_else(|| "validated setIndex__impl signature omitted Item".to_owned())?;
    let item = validate_object(hl, item_type, &ITEM_SCHEMA)?;
    let string_type = item
        .field_type_address("kind")
        .ok_or_else(|| "validated Item layout omitted kind type".to_owned())?;
    let string = validate_object(hl, string_type, &STRING_SCHEMA)?;
    if !hl.type_is_a(hero.type_address, "ent.Unit") {
        return Err("Inventory.owner is incompatible with the validated Hero type".to_owned());
    }
    LAYOUT
        .set(InventoryLayout {
            inventory_type,
            owner: inventory
                .offset("owner")
                .ok_or_else(|| "validated Inventory layout omitted owner".to_owned())?,
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
        .map_err(|_| "inventory hook layout was already initialized".to_owned())?;
    Ok(ResolvedHooks {
        content,
        index,
        count,
    })
}

fn install_hooks(hooks: &ResolvedHooks) -> Result<(), String> {
    let content = hooks.content.target() as *mut c_void;
    let index = hooks.index.target() as *mut c_void;
    let count = hooks.count.target() as *mut c_void;
    install_one(content, hook_set_content as *mut c_void, &ORIGINAL_CONTENT)?;
    if let Err(error) = install_one(index, hook_set_index as *mut c_void, &ORIGINAL_INDEX) {
        remove_one(content, &ORIGINAL_CONTENT);
        return Err(error);
    }
    if let Err(error) = install_one(count, hook_set_stack_count as *mut c_void, &ORIGINAL_COUNT) {
        remove_one(index, &ORIGINAL_INDEX);
        remove_one(content, &ORIGINAL_CONTENT);
        return Err(error);
    }
    CONTENT_TARGET.store(content as usize, Ordering::Release);
    INDEX_TARGET.store(index as usize, Ordering::Release);
    COUNT_TARGET.store(count as usize, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

fn install_one(
    target: *mut c_void,
    detour: *mut c_void,
    original_slot: &AtomicUsize,
) -> Result<(), String> {
    // SAFETY: all targets and detours were exact-signature validated.
    let original = std::panic::catch_unwind(|| unsafe { MinHook::create_hook(target, detour) })
        .map_err(|_| "MinHook initialization panicked for inventory hooks".to_owned())?
        .map_err(|status| format!("create inventory hook returned {status:?}"))?;
    original_slot.store(original as usize, Ordering::Release);
    // SAFETY: hook and trampoline were created immediately above.
    if let Err(status) = unsafe { MinHook::enable_hook(target) } {
        // SAFETY: target names the hook created immediately above.
        let _ = unsafe { MinHook::remove_hook(target) };
        original_slot.store(0, Ordering::Release);
        return Err(format!("enable inventory hook returned {status:?}"));
    }
    Ok(())
}

fn remove_one(target: *mut c_void, original_slot: &AtomicUsize) {
    // SAFETY: called only for a target successfully installed by install_one.
    let _ = unsafe { MinHook::disable_hook(target) };
    // SAFETY: called only for a target successfully installed by install_one.
    let _ = unsafe { MinHook::remove_hook(target) };
    original_slot.store(0, Ordering::Release);
}

unsafe extern "C" fn hook_set_content(inventory: *mut c_void, content: *mut c_void) -> *mut c_void {
    let local = ACTIVE.load(Ordering::Relaxed) && unsafe { is_local_inventory(inventory) };
    let original = ORIGINAL_CONTENT.load(Ordering::Acquire);
    let result = if original == 0 {
        std::ptr::null_mut()
    } else {
        // SAFETY: trampoline matches the validated method.
        let original: HlSetContent = unsafe { std::mem::transmute(original) };
        unsafe { original(inventory, content) }
    };
    if local {
        queue(RawInventoryChange {
            kind: RawKind::ContentReplaced,
            inventory_pointer: inventory as usize,
            index: -1,
            count: 0,
            item_present: false,
            item_kind_length: 0,
            item_kind: [0; MAX_ITEM_KIND_CODE_UNITS],
        });
    }
    result
}

unsafe extern "C" fn hook_set_index(
    inventory: *mut c_void,
    index: i32,
    item: *mut c_void,
    count: *mut i32,
) {
    let observation = if ACTIVE.load(Ordering::Relaxed) && index >= 0 {
        unsafe { copy_slot_change(inventory, index, item, count) }
    } else {
        None
    };
    let original = ORIGINAL_INDEX.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: trampoline matches the validated method.
        let original: HlSetIndex = unsafe { std::mem::transmute(original) };
        unsafe { original(inventory, index, item, count) };
    }
    if let Some(observation) = observation {
        queue(observation);
    }
}

unsafe extern "C" fn hook_set_stack_count(inventory: *mut c_void, index: i32, count: i32) {
    let local =
        ACTIVE.load(Ordering::Relaxed) && index >= 0 && unsafe { is_local_inventory(inventory) };
    let original = ORIGINAL_COUNT.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: trampoline matches the validated method.
        let original: HlSetStackCount = unsafe { std::mem::transmute(original) };
        unsafe { original(inventory, index, count) };
    }
    if local {
        queue(RawInventoryChange {
            kind: RawKind::CountSet,
            inventory_pointer: inventory as usize,
            index,
            count,
            item_present: false,
            item_kind_length: 0,
            item_kind: [0; MAX_ITEM_KIND_CODE_UNITS],
        });
    }
}

unsafe fn is_local_inventory(inventory: *mut c_void) -> bool {
    let Some(layout) = LAYOUT.get().copied() else {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    if !unsafe { object_has_exact_type(inventory, layout.inventory_type) } {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let owner = unsafe {
        std::ptr::read_unaligned(inventory.cast::<u8>().add(layout.owner).cast::<usize>())
    };
    let local = crate::player_hooks::local_hero_pointer();
    if local < 0x1_0000 || owner != local {
        FILTERED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    true
}

unsafe fn copy_slot_change(
    inventory: *mut c_void,
    index: i32,
    item: *mut c_void,
    count: *mut i32,
) -> Option<RawInventoryChange> {
    if !unsafe { is_local_inventory(inventory) } {
        return None;
    }
    let layout = LAYOUT.get().copied()?;
    let mut raw = RawInventoryChange {
        kind: RawKind::SlotSet,
        inventory_pointer: inventory as usize,
        index,
        count: if count.is_null() {
            0
        } else {
            unsafe { std::ptr::read_unaligned(count) }
        },
        item_present: !item.is_null(),
        item_kind_length: 0,
        item_kind: [0; MAX_ITEM_KIND_CODE_UNITS],
    };
    if item.is_null() {
        return Some(raw);
    }
    // `kind` is inherited at this validated base-Item offset by every concrete
    // Item subtype accepted by the method ABI.
    let kind = unsafe {
        std::ptr::read_unaligned(item.cast::<u8>().add(layout.item_kind).cast::<usize>())
    } as *const c_void;
    if !unsafe { object_has_exact_type(kind, layout.string_type) } {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let string = kind.cast::<u8>();
    let length =
        unsafe { std::ptr::read_unaligned(string.add(layout.string_length).cast::<i32>()) };
    if !(1..=MAX_ITEM_KIND_CODE_UNITS as i32).contains(&length) {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let bytes =
        unsafe { std::ptr::read_unaligned(string.add(layout.string_bytes).cast::<usize>()) };
    if bytes < 0x1_0000 {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // SAFETY: String layout and length are validated and bounded.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes as *const u16,
            raw.item_kind.as_mut_ptr(),
            length as usize,
        );
    }
    raw.item_kind_length = length as u16;
    Some(raw)
}

fn queue(raw: RawInventoryChange) {
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
        let Some(sample) = decode_sample(&raw) else {
            INVALID.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        match sample.kind {
            RawKind::ContentReplaced => CONTENT_REPLACED.fetch_add(1, Ordering::Relaxed),
            RawKind::SlotSet => SLOT_SET.fetch_add(1, Ordering::Relaxed),
            RawKind::CountSet => COUNT_SET.fetch_add(1, Ordering::Relaxed),
        };
        if let Some(last) = LAST_SAMPLE.get() {
            if let Ok(mut last) = last.lock() {
                *last = Some(sample);
            }
        }
    }
}

fn decode_sample(raw: &RawInventoryChange) -> Option<InventorySample> {
    if raw.inventory_pointer < 0x1_0000 {
        return None;
    }
    match raw.kind {
        RawKind::ContentReplaced => Some(InventorySample {
            kind: raw.kind,
            index: None,
            count: None,
            item_kind: None,
        }),
        RawKind::CountSet if raw.index >= 0 => Some(InventorySample {
            kind: raw.kind,
            index: Some(raw.index),
            count: Some(raw.count),
            item_kind: None,
        }),
        RawKind::SlotSet if raw.index >= 0 => {
            let item_kind = if raw.item_present {
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
            Some(InventorySample {
                kind: raw.kind,
                index: Some(raw.index),
                count: Some(raw.count),
                item_kind,
            })
        }
        RawKind::SlotSet | RawKind::CountSet => None,
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
            || "inventory_last=unknown".to_owned(),
            |sample| match sample.kind {
                RawKind::ContentReplaced => "inventory_last=content-replaced".to_owned(),
                RawKind::CountSet => format!(
                    "inventory_last=count:{}:{}",
                    sample.index.unwrap_or(-1),
                    sample.count.unwrap_or(0)
                ),
                RawKind::SlotSet => format!(
                    "inventory_last=slot:{}:{}:{}",
                    sample.index.unwrap_or(-1),
                    sample.count.unwrap_or(0),
                    sample
                        .item_kind
                        .as_deref()
                        .map_or_else(|| "none".to_owned(), metric_token)
                ),
            },
        );
    format!(
        "inventory_hooks={} inventory_mode=shadow-only inventory_content_replaced={} inventory_slot_set={} inventory_count_set={} inventory_filtered={} inventory_invalid={} inventory_queue_drops={} {}",
        status_name(status()),
        CONTENT_REPLACED.load(Ordering::Relaxed),
        SLOT_SET.load(Ordering::Relaxed),
        COUNT_SET.load(Ordering::Relaxed),
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
    for target in [&CONTENT_TARGET, &INDEX_TARGET, &COUNT_TARGET] {
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

    fn raw(kind: RawKind, index: i32, count: i32, item: Option<&str>) -> RawInventoryChange {
        let mut result = RawInventoryChange {
            kind,
            inventory_pointer: 0x10_000,
            index,
            count,
            item_present: item.is_some(),
            item_kind_length: 0,
            item_kind: [0; MAX_ITEM_KIND_CODE_UNITS],
        };
        if let Some(item) = item {
            let item = item.encode_utf16().collect::<Vec<_>>();
            result.item_kind_length = item.len() as u16;
            result.item_kind[..item.len()].copy_from_slice(&item);
        }
        result
    }

    #[test]
    fn method_specs_match_verified_inventory_mutations() {
        assert_eq!(SET_CONTENT.name, c"set_content");
        assert_eq!(SET_INDEX_IMPL.name, c"setIndex__impl");
        assert_eq!(DO_SET_STACK_COUNT.name, c"doSetStackCount");
        assert_eq!(SET_CONTENT.arguments, SET_CONTENT_ARGUMENTS);
        assert_eq!(SET_INDEX_IMPL.arguments, SET_INDEX_ARGUMENTS);
        assert_eq!(DO_SET_STACK_COUNT.arguments, SET_COUNT_ARGUMENTS);
    }

    #[test]
    fn decoder_distinguishes_bulk_slot_and_count_mutations() {
        assert_eq!(
            decode_sample(&raw(RawKind::ContentReplaced, -1, 0, None)),
            Some(InventorySample {
                kind: RawKind::ContentReplaced,
                index: None,
                count: None,
                item_kind: None,
            })
        );
        assert_eq!(
            decode_sample(&raw(RawKind::SlotSet, 4, 3, Some("Item_Potion")))
                .and_then(|sample| sample.item_kind),
            Some("Item_Potion".to_owned())
        );
        assert_eq!(
            decode_sample(&raw(RawKind::CountSet, 4, 2, None)).and_then(|sample| sample.count),
            Some(2)
        );
    }
}
