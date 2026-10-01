//! Observe completed left mouse clicks in the local backpack. Native behavior
//! decides whether a release is a click (including drag suppression). Hooks copy
//! the item before forwarding; UTF-8 decoding and bus delivery run on the worker.
use crate::hashlink::{
    object_has_exact_type, validate_object, HashLink, HashLinkFieldSpec as Field,
    HashLinkKind as Kind, HashLinkMethodSpec, HashLinkObjectSpec, HashLinkRuntime,
    HashLinkTypeSpec as Type, ValidatedHashLinkMethod,
};
use crossbeam_queue::ArrayQueue;
use minhook::MinHook;
use std::{
    cell::Cell,
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        OnceLock,
    },
};

pub(crate) const INVENTORY_CLICK_TOPIC: &str = "farever.inventory-click@1";
const CAPACITY: usize = 64;
const MAX_KIND: usize = 128;
const SLOT: &str = "ui.win.InventorySlot";
const SLOT_FIELDS: &[Field] = &[
    Field::object("item", "st.Item"),
    Field::object("inventory", "st.Inventory"),
    Field::scalar("enable", Kind::Bool),
];
const ITEM_FIELDS: &[Field] = &[Field::object("kind", "String")];
const STRING_FIELDS: &[Field] = &[
    Field::scalar("bytes", Kind::Bytes),
    Field::scalar("length", Kind::I32),
];
const EVENT_FIELDS: &[Field] = &[Field::scalar("button", Kind::I32)];
const HERO_FIELDS: &[Field] = &[Field::object("loadout", "st.Loadout")];
const LOADOUT_FIELDS: &[Field] = &[Field::object("inventory", "st.Inventory")];
const INVENTORY_FIELDS: &[Field] = &[Field::object("owner", "ent.Unit")];
const CLICK: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: SLOT,
    name: c"click",
    arguments: &[Type::Object("ui.UIElement"), Type::Object("String")],
    result: Type::Kind(Kind::Bool),
};
const RELEASE: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: SLOT,
    name: c"release",
    arguments: &[
        Type::Object("ui.UIElement"),
        Type::Object("String"),
        Type::Object("hxd.Event"),
    ],
    result: Type::Kind(Kind::Void),
};

static SLOT_TYPE: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static STATUS: AtomicUsize = AtomicUsize::new(0);
static ERROR: OnceLock<String> = OnceLock::new();
static CLICK_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static RELEASE_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static CLICK_TARGET: AtomicUsize = AtomicUsize::new(0);
static RELEASE_TARGET: AtomicUsize = AtomicUsize::new(0);
static LAYOUT: OnceLock<Layout> = OnceLock::new();
static QUEUE: OnceLock<ArrayQueue<RawClick>> = OnceLock::new();
static DROPS: AtomicU64 = AtomicU64::new(0);
thread_local! { static MOUSE_RELEASE: Cell<usize> = const { Cell::new(0) }; }

#[derive(Clone, Copy)]
struct Layout {
    slot_type: usize,
    item_type: usize,
    inventory_type: usize,
    string_type: usize,
    event_type: usize,
    hero_type: usize,
    loadout_type: usize,
    slot_item: usize,
    slot_inventory: usize,
    enabled: usize,
    event_button: usize,
    hero_loadout: usize,
    loadout_inventory: usize,
    inventory_owner: usize,
    item_kind: usize,
    string_bytes: usize,
    string_length: usize,
}
#[derive(Clone, Copy)]
struct RawClick {
    hero: usize,
    length: usize,
    kind: [u16; MAX_KIND],
}

pub(crate) fn observe_slot_type(address: usize) {
    if address >= 0x1_0000 {
        SLOT_TYPE.store(address, Ordering::Release);
    }
}
pub(crate) fn prepare_queue() {
    let _ = QUEUE.get_or_init(|| ArrayQueue::new(CAPACITY));
}

pub(crate) fn try_install_hooks(hl: &HashLink<'_>) {
    let slot_type = SLOT_TYPE.load(Ordering::Acquire);
    let Some(hero_type) = crate::player_hooks::hero_type() else {
        return;
    };
    if slot_type == 0
        || STATUS
            .compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return;
    }
    let result = resolve(hl, slot_type, hero_type).and_then(|(click, release, layout)| {
        LAYOUT
            .set(layout)
            .map_err(|_| "inventory click layout already initialized".to_owned())?;
        prepare_queue();
        install(&click, hook_click as *mut c_void, &CLICK_ORIGINAL)?;
        CLICK_TARGET.store(click.target(), Ordering::Release);
        if let Err(error) = install(&release, hook_release as *mut c_void, &RELEASE_ORIGINAL) {
            // SAFETY: the click hook was installed above; callbacks still forward.
            let _ = unsafe { MinHook::disable_hook(click.target() as *mut c_void) };
            return Err(error);
        }
        RELEASE_TARGET.store(release.target(), Ordering::Release);
        ACTIVE.store(true, Ordering::Release);
        Ok(())
    });
    match result {
        Ok(()) => STATUS.store(1, Ordering::Release),
        Err(error) => {
            let _ = ERROR.set(error);
            STATUS.store(3, Ordering::Release);
        }
    }
}

fn resolve(
    hl: &HashLink<'_>,
    slot_type: usize,
    hero_type: usize,
) -> Result<(ValidatedHashLinkMethod, ValidatedHashLinkMethod, Layout), String> {
    let runtime = HashLinkRuntime::loaded().ok_or("libhl.dll is not loaded")?;
    let click = runtime.resolve_method(hl, slot_type, &CLICK)?;
    let release = runtime.resolve_method(hl, slot_type, &RELEASE)?;
    let slot = validate_object(
        hl,
        slot_type,
        &HashLinkObjectSpec {
            name: SLOT,
            kind: Kind::Object,
            fields: SLOT_FIELDS,
        },
    )?;
    let item_type = slot.field_type_address("item").ok_or("missing item type")?;
    let item = validate_object(
        hl,
        item_type,
        &HashLinkObjectSpec {
            name: "st.Item",
            kind: Kind::Object,
            fields: ITEM_FIELDS,
        },
    )?;
    let string_type = item.field_type_address("kind").ok_or("missing kind type")?;
    let string = validate_object(
        hl,
        string_type,
        &HashLinkObjectSpec {
            name: "String",
            kind: Kind::Object,
            fields: STRING_FIELDS,
        },
    )?;
    if click.argument_type(1) != Some(string_type) || release.argument_type(1) != Some(string_type)
    {
        return Err("click input and item kind String types differ".into());
    }
    let event_type = release.argument_type(2).ok_or("missing event type")?;
    let event = validate_object(
        hl,
        event_type,
        &HashLinkObjectSpec {
            name: "hxd.Event",
            kind: Kind::Object,
            fields: EVENT_FIELDS,
        },
    )?;
    let hero = validate_object(
        hl,
        hero_type,
        &HashLinkObjectSpec {
            name: "ent.Hero",
            kind: Kind::Object,
            fields: HERO_FIELDS,
        },
    )?;
    let loadout_type = hero
        .field_type_address("loadout")
        .ok_or("missing loadout type")?;
    let loadout = validate_object(
        hl,
        loadout_type,
        &HashLinkObjectSpec {
            name: "st.Loadout",
            kind: Kind::Object,
            fields: LOADOUT_FIELDS,
        },
    )?;
    let inventory_type = loadout
        .field_type_address("inventory")
        .ok_or("missing inventory type")?;
    if slot.field_type_address("inventory") != Some(inventory_type) {
        return Err("slot and Hero inventory types differ".into());
    }
    let inventory = validate_object(
        hl,
        inventory_type,
        &HashLinkObjectSpec {
            name: "st.Inventory",
            kind: Kind::Object,
            fields: INVENTORY_FIELDS,
        },
    )?;
    let offset = |obj: &crate::hashlink::ValidatedHashLinkObject, name: &str| {
        obj.offset(name)
            .ok_or_else(|| format!("missing {name} offset"))
    };
    Ok((
        click,
        release,
        Layout {
            slot_type,
            item_type,
            inventory_type,
            string_type,
            event_type,
            hero_type,
            loadout_type,
            slot_item: offset(&slot, "item")?,
            slot_inventory: offset(&slot, "inventory")?,
            enabled: offset(&slot, "enable")?,
            event_button: offset(&event, "button")?,
            hero_loadout: offset(&hero, "loadout")?,
            loadout_inventory: offset(&loadout, "inventory")?,
            inventory_owner: offset(&inventory, "owner")?,
            item_kind: offset(&item, "kind")?,
            string_bytes: offset(&string, "bytes")?,
            string_length: offset(&string, "length")?,
        },
    ))
}

fn install(
    method: &ValidatedHashLinkMethod,
    detour: *mut c_void,
    original: &AtomicUsize,
) -> Result<(), String> {
    let target = method.target() as *mut c_void;
    // SAFETY: the metadata resolver checked the complete receiver/argument/return ABI.
    let trampoline = std::panic::catch_unwind(|| unsafe { MinHook::create_hook(target, detour) })
        .map_err(|_| "MinHook initialization panicked".to_owned())?
        .map_err(|s| format!("create inventory click hook: {s:?}"))?;
    original.store(trampoline as usize, Ordering::Release);
    // SAFETY: the hook was just created and its forwarding trampoline is published.
    if let Err(status) = unsafe { MinHook::enable_hook(target) } {
        // SAFETY: remove the disabled hook created above.
        let _ = unsafe { MinHook::remove_hook(target) };
        original.store(0, Ordering::Release);
        return Err(format!("enable inventory click hook: {status:?}"));
    }
    Ok(())
}

unsafe fn pointer(object: *mut c_void, offset: usize) -> *mut c_void {
    // SAFETY: caller validates the live object type and fixed layout first.
    unsafe { std::ptr::read_unaligned(object.cast::<u8>().add(offset).cast()) }
}
unsafe fn left_release(slot: *mut c_void, event: *mut c_void, layout: Layout) -> bool {
    // SAFETY: these are live arguments of the validated native release callback.
    unsafe {
        object_has_exact_type(slot, layout.slot_type)
            && object_has_exact_type(event, layout.event_type)
            && std::ptr::read_unaligned(event.cast::<u8>().add(layout.event_button).cast::<i32>())
                == 0
    }
}

unsafe extern "C" fn hook_release(slot: *mut c_void, input: *mut c_void, event: *mut c_void) {
    let mouse_slot = if ACTIVE.load(Ordering::Acquire)
        && LAYOUT
            .get()
            .is_some_and(|layout| unsafe { left_release(slot, event, *layout) })
    {
        slot as usize
    } else {
        0
    };
    let previous = MOUSE_RELEASE.with(|scope| scope.replace(mouse_slot));
    let original = RELEASE_ORIGINAL.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: (UIElement, String, Event) -> Void matches the resolved method.
        let original: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) =
            unsafe { std::mem::transmute(original) };
        unsafe { original(slot, input, event) };
    }
    MOUSE_RELEASE.with(|scope| scope.set(previous));
}

unsafe extern "C" fn hook_click(slot: *mut c_void, input: *mut c_void) -> bool {
    // A native click dispatched inside a left mouse release is the authority:
    // release already rejects drags, release-outside, and duplicate frame clicks.
    let mouse_click = MOUSE_RELEASE.with(|scope| {
        if scope.get() == slot as usize {
            scope.set(0);
            true
        } else {
            false
        }
    });
    let raw = if mouse_click && ACTIVE.load(Ordering::Acquire) {
        LAYOUT
            .get()
            .and_then(|layout| unsafe { copy_click(slot, *layout) })
    } else {
        None
    };
    let original = CLICK_ORIGINAL.load(Ordering::Acquire);
    let result = if original != 0 {
        // SAFETY: (UIElement, String) -> Bool matches the resolved method. Copying
        // preceded this call because the native action can move/consume the item.
        let original: unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool =
            unsafe { std::mem::transmute(original) };
        unsafe { original(slot, input) }
    } else {
        false
    };
    if let Some(raw) = raw {
        if QUEUE.get().is_some_and(|queue| queue.push(raw).is_err()) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
    result
}

unsafe fn copy_click(slot: *mut c_void, layout: Layout) -> Option<RawClick> {
    // SAFETY: slot is rooted by the validated callback; the Hero is only
    // compared as an identity until reached through the slot's live inventory.
    unsafe { copy_click_for_hero(slot, layout, crate::player_hooks::local_hero_pointer()) }
}

unsafe fn copy_click_for_hero(
    slot: *mut c_void,
    layout: Layout,
    expected_hero: usize,
) -> Option<RawClick> {
    // SAFETY: every subsequent field read is preceded by its exact type check;
    // only the validated local backpack is accepted, excluding bank/shop slots.
    unsafe {
        if !object_has_exact_type(slot, layout.slot_type)
            || std::ptr::read_unaligned(slot.cast::<u8>().add(layout.enabled)) == 0u8
        {
            return None;
        }
        let inventory = pointer(slot, layout.slot_inventory);
        if !object_has_exact_type(inventory, layout.inventory_type) {
            return None;
        }
        // Follow the live slot's owner before dereferencing the cached identity:
        // the old cached Hero may already be gone during a character change.
        let hero = pointer(inventory, layout.inventory_owner);
        if hero as usize != expected_hero || !object_has_exact_type(hero, layout.hero_type) {
            return None;
        }
        let loadout = pointer(hero, layout.hero_loadout);
        if !object_has_exact_type(loadout, layout.loadout_type) {
            return None;
        }
        if pointer(loadout, layout.loadout_inventory) != inventory {
            return None;
        }
        let item = pointer(slot, layout.slot_item);
        if !object_has_exact_type(item, layout.item_type) {
            return None;
        }
        let kind = pointer(item, layout.item_kind);
        if !object_has_exact_type(kind, layout.string_type) {
            return None;
        }
        let length =
            std::ptr::read_unaligned(kind.cast::<u8>().add(layout.string_length).cast::<i32>());
        if !(1..=MAX_KIND as i32).contains(&length) {
            return None;
        }
        let bytes = pointer(kind, layout.string_bytes).cast::<u16>();
        if (bytes as usize) < 0x1_0000 {
            return None;
        }
        let mut raw = RawClick {
            hero: hero as usize,
            length: length as usize,
            kind: [0; MAX_KIND],
        };
        std::ptr::copy_nonoverlapping(bytes, raw.kind.as_mut_ptr(), raw.length);
        Some(raw)
    }
}

pub(crate) struct InventoryClickCapture {
    status: usize,
    drops: u64,
}
impl InventoryClickCapture {
    pub(crate) fn new() -> Self {
        prepare_queue();
        Self {
            status: usize::MAX,
            drops: 0,
        }
    }
    pub(crate) fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut messages = Vec::new();
        if let Some(queue) = QUEUE.get() {
            for _ in 0..CAPACITY {
                let Some(raw) = queue.pop() else {
                    break;
                };
                if !ACTIVE.load(Ordering::Acquire)
                    || raw.hero != crate::player_hooks::local_hero_pointer()
                {
                    continue;
                }
                if let Ok(kind) = String::from_utf16(&raw.kind[..raw.length]) {
                    messages.push(kind.into_bytes());
                }
            }
        }
        messages
    }
    pub(crate) fn take_diagnostics(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        let status = STATUS.load(Ordering::Acquire);
        if status != self.status {
            self.status = status;
            lines.push(format!(
                "inventory click capture state={}{}",
                match status {
                    0 => "waiting-for-inventory",
                    1 => "active",
                    3 => "failed",
                    _ => "installing",
                },
                ERROR
                    .get()
                    .map(|e| format!(" reason={e}"))
                    .unwrap_or_default()
            ));
        }
        let drops = DROPS.load(Ordering::Relaxed);
        if drops != self.drops {
            self.drops = drops;
            lines.push(format!("inventory click capture queue-drops={drops}"));
        }
        lines
    }
}
pub(crate) fn shutdown_hooks() {
    ACTIVE.store(false, Ordering::Release);
    for target in [&CLICK_TARGET, &RELEASE_TARGET] {
        let target = target.load(Ordering::Acquire);
        if target != 0 {
            // SAFETY: published targets refer to installed native hooks.
            let _ = unsafe { MinHook::disable_hook(target as *mut c_void) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_layout() -> Layout {
        Layout {
            slot_type: 0x10000,
            item_type: 0x20000,
            inventory_type: 0x30000,
            string_type: 0x40000,
            event_type: 0x50000,
            hero_type: 0x60000,
            loadout_type: 0x70000,
            slot_item: 8,
            slot_inventory: 16,
            enabled: 24,
            event_button: 8,
            hero_loadout: 8,
            loadout_inventory: 8,
            inventory_owner: 8,
            item_kind: 8,
            string_bytes: 8,
            string_length: 16,
        }
    }

    #[test]
    fn copies_only_current_heroes_live_backpack_and_bounded_item_kind() {
        let layout = fixture_layout();
        let text: Vec<u16> = "ImpDemon_Z2_Soulstone".encode_utf16().collect();
        let mut kind = [layout.string_type, text.as_ptr() as usize, text.len()];
        let mut item = [layout.item_type, kind.as_mut_ptr() as usize];
        let mut loadout = [layout.loadout_type, 0];
        let mut hero = [layout.hero_type, loadout.as_mut_ptr() as usize];
        let hero_address = hero.as_mut_ptr() as usize;
        let mut inventory = [layout.inventory_type, hero_address];
        loadout[1] = inventory.as_mut_ptr() as usize;
        let mut slot = [layout.slot_type, item.as_mut_ptr() as usize, loadout[1], 1];
        let read = |slot: &mut [usize; 4], expected| unsafe {
            copy_click_for_hero(slot.as_mut_ptr().cast(), layout, expected)
        };
        let raw = read(&mut slot, hero_address).expect("local backpack click");
        assert_eq!(
            String::from_utf16(&raw.kind[..raw.length]).unwrap(),
            "ImpDemon_Z2_Soulstone"
        );
        assert_eq!(raw.hero, hero_address);
        // Cached identity is compared without dereferencing a stale address.
        assert!(read(&mut slot, 0xdeadbeef).is_none());
        let mut bank = [layout.inventory_type, hero_address];
        slot[2] = bank.as_mut_ptr() as usize;
        assert!(read(&mut slot, hero_address).is_none());
        slot[2] = loadout[1];
        inventory[1] = 0;
        assert!(read(&mut slot, hero_address).is_none());
        inventory[1] = hero_address;
        std::hint::black_box(&inventory);
        slot[3] = 0;
        assert!(read(&mut slot, hero_address).is_none());
        slot[3] = 1;
        for length in [0, MAX_KIND + 1, usize::MAX] {
            kind[2] = length;
            assert!(read(&mut slot, hero_address).is_none());
        }
        kind[2] = text.len();
        kind[1] = 0;
        std::hint::black_box(&kind);
        assert!(read(&mut slot, hero_address).is_none());
        slot[1] = 0;
        assert!(read(&mut slot, hero_address).is_none());
    }

    #[test]
    fn release_context_excludes_right_clicks_and_unrelated_widgets() {
        let layout = fixture_layout();
        let mut slot = [layout.slot_type];
        let mut event = [layout.event_type, 0];
        assert!(unsafe {
            left_release(slot.as_mut_ptr().cast(), event.as_mut_ptr().cast(), layout)
        });
        event[1] = 1;
        assert!(!unsafe {
            left_release(slot.as_mut_ptr().cast(), event.as_mut_ptr().cast(), layout)
        });
        event[1] = 0;
        slot[0] = 0x90000;
        assert!(!unsafe {
            left_release(slot.as_mut_ptr().cast(), event.as_mut_ptr().cast(), layout)
        });
        assert!(!unsafe { left_release(std::ptr::null_mut(), event.as_mut_ptr().cast(), layout) });
    }
}
