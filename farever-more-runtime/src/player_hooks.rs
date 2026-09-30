//! Build-gated local Player/Hero binding hooks.
//!
//! The callbacks run on arbitrary game threads. They forward the original
//! call, copy only validated fixed-size fields, and enqueue a bounded record.
//! Binding publication, reconciliation, diagnostics, and consumers remain off
//! the game thread.

use crate::game_build::GameBuildProfile;
use crate::hashlink::{
    object_has_exact_type, validate_object, HashLink, HashLinkFieldSpec, HashLinkKind,
    HashLinkMethodSpec, HashLinkObjectSpec, HashLinkRuntime, HashLinkTypeSpec,
    ValidatedHashLinkMethod,
};
use crossbeam_queue::ArrayQueue;
use farever_more_api::{
    EventHeader, HostEvent, PlayerDisconnectReason, PlayerDisconnectedEvent, SourceQuality,
};
use minhook::MinHook;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

const RAW_QUEUE_CAPACITY: usize = 128;
const MAX_DECODE_PER_TICK: usize = 32;

const GAME_APP_FIELDS: &[HashLinkFieldSpec] = &[HashLinkFieldSpec::object("hero", "ent.Hero")];
const GAME_APP_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "GameApp",
    kind: HashLinkKind::Object,
    fields: GAME_APP_FIELDS,
};
const PLAYER_FIELDS: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::object("hero", "ent.Hero"),
    HashLinkFieldSpec::scalar("isMe", HashLinkKind::Bool),
];
const PLAYER_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.Player",
    kind: HashLinkKind::Object,
    fields: PLAYER_FIELDS,
};

const SYNC_PLAYER_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("GameApp"),
    HashLinkTypeSpec::Object("st.Player"),
];
const SET_HERO_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.Player"),
    HashLinkTypeSpec::Object("ent.Hero"),
];
const GAME_APP_ARGUMENTS: &[HashLinkTypeSpec] = &[HashLinkTypeSpec::Object("GameApp")];
const PLAYER_DISCONNECT_STABLE_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.Player"),
    HashLinkTypeSpec::Kind(HashLinkKind::Bool),
    HashLinkTypeSpec::Kind(HashLinkKind::Function),
    HashLinkTypeSpec::Reference(HashLinkKind::Bool),
];
const PLAYER_DISCONNECT_BETA_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("st.Player"),
    HashLinkTypeSpec::Enum("st.DisconnectReason"),
    HashLinkTypeSpec::Kind(HashLinkKind::Function),
    HashLinkTypeSpec::Reference(HashLinkKind::Bool),
];

const SYNC_PLAYER: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "GameApp",
    name: c"syncPlayer",
    arguments: SYNC_PLAYER_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const SET_HERO: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.Player",
    name: c"set_hero",
    arguments: SET_HERO_ARGUMENTS,
    result: HashLinkTypeSpec::Object("ent.Hero"),
};
const DISCONNECT: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "GameApp",
    name: c"disconnect",
    arguments: GAME_APP_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const PLAYER_DISCONNECT_STABLE: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.Player",
    name: c"disconnect",
    arguments: PLAYER_DISCONNECT_STABLE_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const PLAYER_DISCONNECT_BETA: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "st.Player",
    name: c"disconnect",
    arguments: PLAYER_DISCONNECT_BETA_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};

type HlSyncPlayer = unsafe extern "C" fn(*mut c_void, *mut c_void);
type HlSetHero = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
type HlDisconnect = unsafe extern "C" fn(*mut c_void);
type HlPlayerDisconnectStable = unsafe extern "C" fn(*mut c_void, u8, *mut c_void, *mut c_void);
type HlPlayerDisconnectBeta =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LocalPlayerBinding {
    pub(crate) app: usize,
    pub(crate) player: usize,
    pub(crate) hero: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawBindingKind {
    SetHero,
    SyncPlayer,
    Disconnect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RawBinding {
    sequence: u64,
    kind: RawBindingKind,
    app: usize,
    player: usize,
    hero: usize,
}

#[derive(Clone, Copy, Debug)]
struct BindingLayout {
    game_app_type: usize,
    player_type: usize,
    hero_type: usize,
    app_hero: usize,
    player_hero: usize,
    player_is_me: usize,
}

#[derive(Clone, Copy, Debug)]
struct BindingReconciliation {
    through_sequence: u64,
    app: usize,
    player: usize,
    hero: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BindingUpdate {
    Bind(LocalPlayerBinding),
    Clear,
}

// 0 = waiting for GameApp type, 1 = active, 3 = failed, 4 = installing.
static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_ERROR: OnceLock<String> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static GAME_APP_TYPE: AtomicUsize = AtomicUsize::new(0);
static OBSERVED_APP: AtomicUsize = AtomicUsize::new(0);
static SYNC_TARGET: AtomicUsize = AtomicUsize::new(0);
static SET_HERO_TARGET: AtomicUsize = AtomicUsize::new(0);
static DISCONNECT_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_SYNC: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_SET_HERO: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_DISCONNECT: AtomicUsize = AtomicUsize::new(0);
static PLAYER_DISCONNECT_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_PLAYER_DISCONNECT: AtomicUsize = AtomicUsize::new(0);
static PLAYER_DISCONNECT_REASON_TYPE: AtomicUsize = AtomicUsize::new(0);
static DISCONNECT_EVENTS: OnceLock<ArrayQueue<PlayerDisconnectReason>> = OnceLock::new();
static LAYOUT: OnceLock<BindingLayout> = OnceLock::new();
static RAW_BINDINGS: OnceLock<ArrayQueue<RawBinding>> = OnceLock::new();
static RECONCILIATION: OnceLock<Mutex<Option<BindingReconciliation>>> = OnceLock::new();

// A single observer worker writes this seqlock. Readers can therefore obtain a
// coherent app/player/hero tuple without locking a hook callback.
static BINDING_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static BINDING_APP: AtomicUsize = AtomicUsize::new(0);
static BINDING_PLAYER: AtomicUsize = AtomicUsize::new(0);
static BINDING_HERO: AtomicUsize = AtomicUsize::new(0);
static RAW_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static DISCARD_THROUGH: AtomicU64 = AtomicU64::new(0);

static RAW_DROPS: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static FILTERED: AtomicU64 = AtomicU64::new(0);
static BINDS: AtomicU64 = AtomicU64::new(0);
static CLEARS: AtomicU64 = AtomicU64::new(0);
static DUPLICATES: AtomicU64 = AtomicU64::new(0);
static RECONCILES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn prepare_queue() {
    let _ = RAW_BINDINGS.get_or_init(|| ArrayQueue::new(RAW_QUEUE_CAPACITY));
    let _ = RECONCILIATION.get_or_init(|| Mutex::new(None));
    let _ = DISCONNECT_EVENTS.get_or_init(|| ArrayQueue::new(RAW_QUEUE_CAPACITY));
}

pub(crate) fn observe_game_app_type(type_pointer: usize) {
    if type_pointer >= 0x1_0000 {
        let _ =
            GAME_APP_TYPE.compare_exchange(0, type_pointer, Ordering::AcqRel, Ordering::Acquire);
    }
}

pub(crate) fn observe_current_app(app: Option<usize>) {
    OBSERVED_APP.store(app.unwrap_or_default(), Ordering::Release);
}

/// Supplies one sampled baseline after direct-provider activation, root
/// replacement, or queue loss. The worker applies it before later raw edges.
pub(crate) fn capture_sequence() -> u64 {
    RAW_SEQUENCE.load(Ordering::Acquire)
}

pub(crate) fn reconcile_binding(
    app: usize,
    binding: Option<(usize, Option<usize>)>,
    through_sequence: u64,
) {
    let (player, hero) = binding
        .map(|(player, hero)| (player, hero.unwrap_or_default()))
        .unwrap_or_default();
    let slot = RECONCILIATION.get_or_init(|| Mutex::new(None));
    if let Ok(mut pending) = slot.lock() {
        *pending = Some(BindingReconciliation {
            through_sequence,
            app,
            player,
            hero,
        });
    }
}

pub(crate) fn try_install_hooks(hl: &HashLink<'_>, profile: GameBuildProfile) {
    if HOOK_STATUS.load(Ordering::Acquire) != 0 {
        return;
    }
    let game_app_type = GAME_APP_TYPE.load(Ordering::Acquire);
    if game_app_type == 0 {
        return;
    }
    if HOOK_STATUS
        .compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let result = resolve_hooks(hl, game_app_type, profile).and_then(|hooks| install_hooks(&hooks));
    match result {
        Ok(()) => HOOK_STATUS.store(1, Ordering::Release),
        Err(error) => {
            let _ = HOOK_ERROR.set(error);
            HOOK_STATUS.store(3, Ordering::Release);
        }
    }
}

struct ResolvedHooks {
    sync: ValidatedHashLinkMethod,
    set_hero: ValidatedHashLinkMethod,
    disconnect: ValidatedHashLinkMethod,
    player_disconnect: ValidatedHashLinkMethod,
    profile: GameBuildProfile,
    disconnect_reason_type: usize,
}

fn resolve_hooks(
    hl: &HashLink<'_>,
    game_app_type: usize,
    profile: GameBuildProfile,
) -> Result<ResolvedHooks, String> {
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    let sync = runtime.resolve_method(hl, game_app_type, &SYNC_PLAYER)?;
    let player_type = sync
        .argument_type(1)
        .ok_or_else(|| "validated syncPlayer signature omitted Player".to_owned())?;
    let set_hero = runtime.resolve_method(hl, player_type, &SET_HERO)?;
    let disconnect = runtime.resolve_method(hl, game_app_type, &DISCONNECT)?;
    let player_disconnect_spec = if profile.uses_beta_abi() {
        &PLAYER_DISCONNECT_BETA
    } else {
        &PLAYER_DISCONNECT_STABLE
    };
    let player_disconnect = runtime.resolve_method(hl, player_type, player_disconnect_spec)?;
    let disconnect_reason_type = if profile.uses_beta_abi() {
        let reason_type = player_disconnect
            .argument_type(1)
            .ok_or_else(|| "validated beta Player.disconnect omitted its reason".to_owned())?;
        if hl.named_type_name(reason_type).as_deref() != Some("st.DisconnectReason") {
            return Err("beta Player.disconnect reason type is not st.DisconnectReason".to_owned());
        }
        for (index, expected_name, expected_parameters) in [
            (0, "ManualExit", 0),
            (1, "Kick", 0),
            (2, "Timeout", 0),
            (3, "SwitchingServer", 1),
        ] {
            if hl.enum_constructor(reason_type, index)
                != Some((expected_name.to_owned(), expected_parameters))
            {
                return Err(format!(
                    "beta st.DisconnectReason constructor {index} does not match {expected_name}"
                ));
            }
        }
        reason_type
    } else {
        0
    };

    let hero_type = set_hero
        .argument_type(1)
        .ok_or_else(|| "validated set_hero signature omitted Hero".to_owned())?;
    let app = validate_object(hl, game_app_type, &GAME_APP_SCHEMA)?;
    let player = validate_object(hl, player_type, &PLAYER_SCHEMA)?;
    if app.field_type_address("hero") != Some(hero_type)
        || player.field_type_address("hero") != Some(hero_type)
    {
        return Err("Player/GameApp Hero field types disagree with set_hero".to_owned());
    }
    LAYOUT
        .set(BindingLayout {
            game_app_type,
            player_type,
            hero_type,
            app_hero: app
                .offset("hero")
                .ok_or_else(|| "validated GameApp layout omitted hero".to_owned())?,
            player_hero: player
                .offset("hero")
                .ok_or_else(|| "validated Player layout omitted hero".to_owned())?,
            player_is_me: player
                .offset("isMe")
                .ok_or_else(|| "validated Player layout omitted isMe".to_owned())?,
        })
        .map_err(|_| "local Player binding layout was already initialized".to_owned())?;

    Ok(ResolvedHooks {
        sync,
        set_hero,
        disconnect,
        player_disconnect,
        profile,
        disconnect_reason_type,
    })
}

fn install_hooks(hooks: &ResolvedHooks) -> Result<(), String> {
    let sync_target = hooks.sync.target() as *mut c_void;
    let set_hero_target = hooks.set_hero.target() as *mut c_void;
    let disconnect_target = hooks.disconnect.target() as *mut c_void;
    let player_disconnect_target = hooks.player_disconnect.target() as *mut c_void;
    let targets = [
        sync_target,
        set_hero_target,
        disconnect_target,
        player_disconnect_target,
    ];
    for (index, target) in targets.iter().copied().enumerate() {
        if targets[..index].contains(&target) {
            return Err("local Player hook methods resolved to duplicate targets".to_owned());
        }
    }

    let sync_original = create_hook(sync_target, hook_sync_player as *mut c_void, "syncPlayer")?;
    let set_hero_original =
        match create_hook(set_hero_target, hook_set_hero as *mut c_void, "set_hero") {
            Ok(original) => original,
            Err(error) => {
                remove_hook(sync_target);
                return Err(error);
            }
        };
    let disconnect_original = match create_hook(
        disconnect_target,
        hook_disconnect as *mut c_void,
        "GameApp.disconnect",
    ) {
        Ok(original) => original,
        Err(error) => {
            remove_hook(set_hero_target);
            remove_hook(sync_target);
            return Err(error);
        }
    };
    let player_detour = if hooks.profile.uses_beta_abi() {
        hook_player_disconnect_beta as *mut c_void
    } else {
        hook_player_disconnect_stable as *mut c_void
    };
    let player_disconnect_original = match create_hook(
        player_disconnect_target,
        player_detour,
        "st.Player.disconnect",
    ) {
        Ok(original) => original,
        Err(error) => {
            remove_hook(disconnect_target);
            remove_hook(set_hero_target);
            remove_hook(sync_target);
            return Err(error);
        }
    };

    ORIGINAL_SYNC.store(sync_original, Ordering::Release);
    ORIGINAL_SET_HERO.store(set_hero_original, Ordering::Release);
    ORIGINAL_DISCONNECT.store(disconnect_original, Ordering::Release);
    ORIGINAL_PLAYER_DISCONNECT.store(player_disconnect_original, Ordering::Release);
    PLAYER_DISCONNECT_REASON_TYPE.store(hooks.disconnect_reason_type, Ordering::Release);

    let hook_names = [
        "syncPlayer",
        "set_hero",
        "GameApp.disconnect",
        "st.Player.disconnect",
    ];
    for (index, (target, name)) in targets.iter().copied().zip(hook_names).enumerate() {
        if let Err(status) = unsafe { MinHook::enable_hook(target) } {
            for enabled in targets[..index].iter().copied() {
                let _ = unsafe { MinHook::disable_hook(enabled) };
            }
            for created in targets {
                remove_hook(created);
            }
            ORIGINAL_SYNC.store(0, Ordering::Release);
            ORIGINAL_SET_HERO.store(0, Ordering::Release);
            ORIGINAL_DISCONNECT.store(0, Ordering::Release);
            ORIGINAL_PLAYER_DISCONNECT.store(0, Ordering::Release);
            return Err(format!("enable {name} hook returned {status:?}"));
        }
    }

    SYNC_TARGET.store(sync_target as usize, Ordering::Release);
    SET_HERO_TARGET.store(set_hero_target as usize, Ordering::Release);
    DISCONNECT_TARGET.store(disconnect_target as usize, Ordering::Release);
    PLAYER_DISCONNECT_TARGET.store(player_disconnect_target as usize, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}
fn create_hook(target: *mut c_void, detour: *mut c_void, name: &str) -> Result<usize, String> {
    // SAFETY: the caller resolved and signature-validated this exact method.
    std::panic::catch_unwind(|| unsafe { MinHook::create_hook(target, detour) })
        .map_err(|_| format!("MinHook initialization panicked for {name}"))?
        .map(|original| original as usize)
        .map_err(|status| format!("create {name} hook returned {status:?}"))
}

fn remove_hook(target: *mut c_void) {
    // SAFETY: callers pass only targets successfully created in this function.
    let _ = unsafe { MinHook::remove_hook(target) };
}

unsafe extern "C" fn hook_set_hero(
    player: *mut c_void,
    requested_hero: *mut c_void,
) -> *mut c_void {
    let original = ORIGINAL_SET_HERO.load(Ordering::Acquire);
    if original == 0 {
        return std::ptr::null_mut();
    }
    // SAFETY: MinHook returned this trampoline for the signature-validated
    // `(st.Player, ent.Hero) -> ent.Hero` target.
    let original: HlSetHero = unsafe { std::mem::transmute(original) };
    let result = unsafe { original(player, requested_hero) };

    if ACTIVE.load(Ordering::Relaxed) {
        let app = OBSERVED_APP.load(Ordering::Relaxed);
        let observation = unsafe { copy_local_player(player) };
        match observation {
            Some((true, hero)) if app >= 0x1_0000 && hero == requested_hero as usize => {
                queue_raw(RawBinding {
                    sequence: 0,
                    kind: RawBindingKind::SetHero,
                    app,
                    player: player as usize,
                    hero,
                });
            }
            Some((false, _)) => {
                FILTERED.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                INVALID.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    result
}

unsafe extern "C" fn hook_sync_player(app: *mut c_void, player: *mut c_void) {
    let original = ORIGINAL_SYNC.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: MinHook returned this trampoline for the signature-validated
    // `(GameApp, st.Player) -> Void` target.
    let original: HlSyncPlayer = unsafe { std::mem::transmute(original) };
    unsafe { original(app, player) };

    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let Some(layout) = LAYOUT.get().copied() else {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if !unsafe { object_has_exact_type(app, layout.game_app_type) } {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let Some((is_local, player_hero)) = (unsafe { copy_local_player(player) }) else {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if !is_local {
        FILTERED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: the receiver type and GameApp.hero field offset were validated
    // before this detour was enabled.
    let app_hero =
        unsafe { std::ptr::read_unaligned(app.cast::<u8>().add(layout.app_hero).cast::<usize>()) };
    if app_hero != player_hero || !valid_optional_hero(app_hero, layout.hero_type) {
        INVALID.fetch_add(1, Ordering::Relaxed);
        return;
    }
    queue_raw(RawBinding {
        sequence: 0,
        kind: RawBindingKind::SyncPlayer,
        app: app as usize,
        player: player as usize,
        hero: app_hero,
    });
}

unsafe extern "C" fn hook_disconnect(app: *mut c_void) {
    let valid_app = LAYOUT
        .get()
        .is_some_and(|layout| unsafe { object_has_exact_type(app, layout.game_app_type) });
    let original = ORIGINAL_DISCONNECT.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: MinHook returned this trampoline for the signature-validated
    // `(GameApp) -> Void` target.
    let original: HlDisconnect = unsafe { std::mem::transmute(original) };
    unsafe { original(app) };

    if ACTIVE.load(Ordering::Relaxed) && valid_app {
        queue_raw(RawBinding {
            sequence: 0,
            kind: RawBindingKind::Disconnect,
            app: app as usize,
            player: 0,
            hero: 0,
        });
        crate::lifecycle_hooks::observe_disconnect(app as usize);
    } else if ACTIVE.load(Ordering::Relaxed) {
        INVALID.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe extern "C" fn hook_player_disconnect_stable(
    player: *mut c_void,
    reason: u8,
    on_done: *mut c_void,
    save_db: *mut c_void,
) {
    let local_player = ACTIVE.load(Ordering::Relaxed)
        && unsafe { copy_local_player(player) }.is_some_and(|(is_local, _)| is_local);
    let original = ORIGINAL_PLAYER_DISCONNECT.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    let original: HlPlayerDisconnectStable = unsafe { std::mem::transmute(original) };
    unsafe { original(player, reason, on_done, save_db) };

    if local_player {
        // The previous build exposed only a boolean. True identifies the
        // explicit manual-exit path; false has no recoverable specific cause.
        enqueue_disconnect_reason(if reason == 1 {
            PlayerDisconnectReason::ManualExit
        } else {
            PlayerDisconnectReason::Unknown
        });
    }
}

unsafe extern "C" fn hook_player_disconnect_beta(
    player: *mut c_void,
    reason: *mut c_void,
    on_done: *mut c_void,
    save_db: *mut c_void,
) {
    let local_player = ACTIVE.load(Ordering::Relaxed)
        && unsafe { copy_local_player(player) }.is_some_and(|(is_local, _)| is_local);
    let decoded_reason = unsafe { decode_beta_disconnect_reason(reason) };
    let original = ORIGINAL_PLAYER_DISCONNECT.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    let original: HlPlayerDisconnectBeta = unsafe { std::mem::transmute(original) };
    unsafe { original(player, reason, on_done, save_db) };

    if local_player {
        if let Some(reason) = decoded_reason {
            enqueue_disconnect_reason(reason);
        } else {
            INVALID.fetch_add(1, Ordering::Relaxed);
        }
    }
}

unsafe fn decode_beta_disconnect_reason(reason: *mut c_void) -> Option<PlayerDisconnectReason> {
    let expected_type = PLAYER_DISCONNECT_REASON_TYPE.load(Ordering::Acquire);
    if reason.is_null() || expected_type < 0x1_0000 {
        return None;
    }
    let actual_type = unsafe { std::ptr::read_unaligned(reason.cast::<usize>()) };
    if actual_type != expected_type {
        return None;
    }
    let constructor = unsafe {
        std::ptr::read_unaligned(reason.cast::<u8>().add(size_of::<usize>()).cast::<i32>())
    };
    match constructor {
        0 => Some(PlayerDisconnectReason::ManualExit),
        1 => Some(PlayerDisconnectReason::Kick),
        2 => Some(PlayerDisconnectReason::Timeout),
        3 => Some(PlayerDisconnectReason::SwitchingServer),
        _ => Some(PlayerDisconnectReason::Unknown),
    }
}

fn enqueue_disconnect_reason(reason: PlayerDisconnectReason) {
    if DISCONNECT_EVENTS
        .get()
        .is_none_or(|queue| queue.push(reason).is_err())
    {
        RAW_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Moves bounded disconnect observations into the ordered host event stream.
pub(crate) fn drain_disconnect_events() -> Vec<HostEvent> {
    let Some(queue) = DISCONNECT_EVENTS.get() else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for _ in 0..MAX_DECODE_PER_TICK {
        let Some(reason) = queue.pop() else {
            break;
        };
        events.push(HostEvent::PlayerDisconnected(PlayerDisconnectedEvent {
            header: EventHeader {
                sequence: 0,
                monotonic_ms: 0,
                quality: SourceQuality::Observed,
            },
            reason,
        }));
    }
    events
}
/// Receives an already validated GameApp loss edge from another callback
/// owner. This only appends a fixed-size record to the bounded queue.
pub(crate) fn observe_app_loss(app: usize) {
    if ACTIVE.load(Ordering::Relaxed) && app >= 0x1_0000 {
        queue_raw(RawBinding {
            sequence: 0,
            kind: RawBindingKind::Disconnect,
            app,
            player: 0,
            hero: 0,
        });
    }
}

unsafe fn copy_local_player(player: *mut c_void) -> Option<(bool, usize)> {
    let layout = LAYOUT.get().copied()?;
    if !unsafe { object_has_exact_type(player, layout.player_type) } {
        return None;
    }
    let base = player.cast::<u8>();
    // SAFETY: both offsets belong to the exact validated st.Player layout.
    let is_me = unsafe { std::ptr::read_unaligned(base.add(layout.player_is_me)) };
    if is_me > 1 {
        return None;
    }
    let hero = unsafe { std::ptr::read_unaligned(base.add(layout.player_hero).cast::<usize>()) };
    valid_optional_hero(hero, layout.hero_type).then_some((is_me == 1, hero))
}

fn valid_optional_hero(hero: usize, hero_type: usize) -> bool {
    hero == 0
        || (hero >= 0x1_0000 && unsafe { object_has_exact_type(hero as *const c_void, hero_type) })
}

fn queue_raw(mut raw: RawBinding) {
    raw.sequence = RAW_SEQUENCE.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    if RAW_BINDINGS
        .get()
        .is_none_or(|queue| queue.push(raw).is_err())
    {
        RAW_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn decode_pending() {
    let reconciliation = take_reconciliation();
    if let Some(reconciliation) = reconciliation {
        DISCARD_THROUGH.fetch_max(reconciliation.through_sequence, Ordering::AcqRel);
        if reconciliation.app == OBSERVED_APP.load(Ordering::Acquire) {
            let binding = (reconciliation.player >= 0x1_0000).then_some(LocalPlayerBinding {
                app: reconciliation.app,
                player: reconciliation.player,
                hero: (reconciliation.hero >= 0x1_0000).then_some(reconciliation.hero),
            });
            publish_binding(binding);
            RECONCILES.fetch_add(1, Ordering::Relaxed);
        } else {
            FILTERED.fetch_add(1, Ordering::Relaxed);
        }
    }

    let Some(queue) = RAW_BINDINGS.get() else {
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
        match binding_update(raw, OBSERVED_APP.load(Ordering::Acquire)) {
            Some(BindingUpdate::Bind(binding)) => publish_binding(Some(binding)),
            Some(BindingUpdate::Clear) => publish_binding(None),
            None => {
                FILTERED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn take_reconciliation() -> Option<BindingReconciliation> {
    RECONCILIATION
        .get()
        .and_then(|slot| slot.lock().ok()?.take())
}

fn binding_update(raw: RawBinding, current_app: usize) -> Option<BindingUpdate> {
    if raw.app < 0x1_0000 || raw.app != current_app {
        return None;
    }
    match raw.kind {
        RawBindingKind::SetHero | RawBindingKind::SyncPlayer if raw.player >= 0x1_0000 => {
            Some(BindingUpdate::Bind(LocalPlayerBinding {
                app: raw.app,
                player: raw.player,
                hero: (raw.hero >= 0x1_0000).then_some(raw.hero),
            }))
        }
        RawBindingKind::Disconnect => Some(BindingUpdate::Clear),
        RawBindingKind::SetHero | RawBindingKind::SyncPlayer => None,
    }
}

fn publish_binding(binding: Option<LocalPlayerBinding>) {
    let previous = current_binding_unfiltered();
    if previous == binding {
        DUPLICATES.fetch_add(1, Ordering::Relaxed);
        return;
    }
    BINDING_SEQUENCE.fetch_add(1, Ordering::AcqRel);
    BINDING_APP.store(binding.map_or(0, |value| value.app), Ordering::Relaxed);
    BINDING_PLAYER.store(binding.map_or(0, |value| value.player), Ordering::Relaxed);
    BINDING_HERO.store(
        binding.and_then(|value| value.hero).unwrap_or_default(),
        Ordering::Relaxed,
    );
    BINDING_SEQUENCE.fetch_add(1, Ordering::Release);
    if binding.is_some() {
        BINDS.fetch_add(1, Ordering::Relaxed);
    } else {
        CLEARS.fetch_add(1, Ordering::Relaxed);
    }
}

fn current_binding_unfiltered() -> Option<LocalPlayerBinding> {
    for _ in 0..8 {
        let before = BINDING_SEQUENCE.load(Ordering::Acquire);
        if before & 1 != 0 {
            std::hint::spin_loop();
            continue;
        }
        let app = BINDING_APP.load(Ordering::Relaxed);
        let player = BINDING_PLAYER.load(Ordering::Relaxed);
        let hero = BINDING_HERO.load(Ordering::Relaxed);
        let after = BINDING_SEQUENCE.load(Ordering::Acquire);
        if before == after {
            return (app >= 0x1_0000 && player >= 0x1_0000).then_some(LocalPlayerBinding {
                app,
                player,
                hero: (hero >= 0x1_0000).then_some(hero),
            });
        }
    }
    None
}

pub(crate) fn current_binding() -> Option<LocalPlayerBinding> {
    let current_app = OBSERVED_APP.load(Ordering::Acquire);
    current_binding_unfiltered().filter(|binding| binding.app == current_app)
}

pub(crate) fn current_app() -> Option<usize> {
    let app = OBSERVED_APP.load(Ordering::Acquire);
    (app >= 0x1_0000).then_some(app)
}

/// Callback-safe local Hero identity. Consumers must still compare exact
/// receiver identity and must not dereference this value after their callback.
pub(crate) fn local_hero_pointer() -> usize {
    BINDING_HERO.load(Ordering::Acquire)
}

pub(crate) fn local_player_pointer() -> usize {
    BINDING_PLAYER.load(Ordering::Acquire)
}

pub(crate) fn hero_type() -> Option<usize> {
    LAYOUT.get().map(|layout| layout.hero_type)
}

pub(crate) fn player_type() -> Option<usize> {
    LAYOUT.get().map(|layout| layout.player_type)
}

pub(crate) fn root_available() -> bool {
    OBSERVED_APP.load(Ordering::Acquire) >= 0x1_0000
}

pub(crate) fn status() -> usize {
    HOOK_STATUS.load(Ordering::Acquire)
}

pub(crate) fn status_name(status: usize) -> &'static str {
    match status {
        0 => "waiting-for-game-app-type",
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
    RAW_DROPS.load(Ordering::Acquire)
}

pub(crate) fn metrics() -> String {
    format!(
        "player_hooks={} player_binding={} player_binds={} player_clears={} player_duplicates={} player_reconciles={} player_filtered={} player_invalid={} player_raw_drops={}",
        status_name(status()),
        if current_binding().is_some() { "bound" } else { "unbound" },
        BINDS.load(Ordering::Relaxed),
        CLEARS.load(Ordering::Relaxed),
        DUPLICATES.load(Ordering::Relaxed),
        RECONCILES.load(Ordering::Relaxed),
        FILTERED.load(Ordering::Relaxed),
        INVALID.load(Ordering::Relaxed),
        RAW_DROPS.load(Ordering::Relaxed),
    )
}

pub(crate) fn shutdown_hooks() {
    ACTIVE.store(false, Ordering::Release);
    for target in [
        SYNC_TARGET.load(Ordering::Acquire),
        SET_HERO_TARGET.load(Ordering::Acquire),
        DISCONNECT_TARGET.load(Ordering::Acquire),
        PLAYER_DISCONNECT_TARGET.load(Ordering::Acquire),
    ] {
        if target != 0 {
            // SAFETY: targets are published only after their build-gated,
            // signature-validated hooks have been enabled.
            let _ = unsafe { MinHook::disable_hook(target as *mut c_void) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_specs_match_verified_local_binding_boundaries() {
        assert_eq!(SYNC_PLAYER.lookup_type, "GameApp");
        assert_eq!(SYNC_PLAYER.name, c"syncPlayer");
        assert_eq!(SYNC_PLAYER.arguments, SYNC_PLAYER_ARGUMENTS);
        assert_eq!(SET_HERO.lookup_type, "st.Player");
        assert_eq!(SET_HERO.name, c"set_hero");
        assert_eq!(SET_HERO.arguments, SET_HERO_ARGUMENTS);
        assert_eq!(DISCONNECT.lookup_type, "GameApp");
        assert_eq!(DISCONNECT.name, c"disconnect");
        assert_eq!(DISCONNECT.arguments, GAME_APP_ARGUMENTS);
    }

    #[test]
    fn binding_edges_are_filtered_to_the_current_game_app() {
        let bind = RawBinding {
            sequence: 1,
            kind: RawBindingKind::SyncPlayer,
            app: 0x10_000,
            player: 0x20_000,
            hero: 0x30_000,
        };
        assert_eq!(
            binding_update(bind, 0x10_000),
            Some(BindingUpdate::Bind(LocalPlayerBinding {
                app: 0x10_000,
                player: 0x20_000,
                hero: Some(0x30_000),
            }))
        );
        assert_eq!(binding_update(bind, 0x40_000), None);
    }

    #[test]
    fn disconnect_clears_only_the_current_game_app() {
        let disconnect = RawBinding {
            sequence: 1,
            kind: RawBindingKind::Disconnect,
            app: 0x10_000,
            player: 0,
            hero: 0,
        };
        assert_eq!(
            binding_update(disconnect, 0x10_000),
            Some(BindingUpdate::Clear)
        );
        assert_eq!(binding_update(disconnect, 0x20_000), None);
    }

    #[test]
    fn set_hero_can_publish_a_local_player_without_a_hero() {
        let clear_hero = RawBinding {
            sequence: 1,
            kind: RawBindingKind::SetHero,
            app: 0x10_000,
            player: 0x20_000,
            hero: 0,
        };
        assert_eq!(
            binding_update(clear_hero, 0x10_000),
            Some(BindingUpdate::Bind(LocalPlayerBinding {
                app: 0x10_000,
                player: 0x20_000,
                hero: None,
            }))
        );
    }
}
