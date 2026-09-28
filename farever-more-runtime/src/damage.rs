//! Build-gated direct `HashLink` hook for exact local damage events.
//!
//! The detour callback may run on arbitrary game threads. It therefore performs
//! only atomic lookups and bounded lock-free queue writes; reflection, strings,
//! logging, and event construction remain on the dedicated decoder worker. The
//! earlier `DamageDisplay` observer remains available as the exclusive selected
//! or automatic polling provider.

use crate::game_build::{self, GameBuildProfile};
use crate::hashlink::{
    object_has_exact_type, validate_object, HashLink, HashLinkFieldSpec, HashLinkKind,
    HashLinkMethodSpec, HashLinkObjectSpec, HashLinkRuntime, HashLinkTypeSpec,
    ObservedHashLinkTypes, TypeObservation, ValidatedHashLinkMethod,
};
use crate::memory::ProcessMemory;
use crate::state::PartyDamageSource;
use crossbeam_queue::ArrayQueue;
use farever_more_api::{
    ActorRelation, CombatActorRef, DamageEvent, EventHeader, HostEvent, SourceQuality,
};
use minhook::MinHook;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const TYPE_SLOTS: usize = 16_384;
const TYPE_PROBE_LIMIT: usize = 16;
const TYPE_QUEUE_CAPACITY: usize = TYPE_SLOTS;
const RAW_QUEUE_CAPACITY: usize = 2048;
const EVENT_QUEUE_CAPACITY: usize = 4096;
const MAX_PENDING: usize = 256;
const MAX_LOCKED_HERO_PENDING: usize = 8;
const MAX_DECODE_PER_TICK: usize = 8;
const MAX_HERO_VALIDATE_PER_TICK: usize = 8;
const DIRECT_DAMAGE_QUEUE_CAPACITY: usize = 4096;
const MAX_SKILL_ID_CODE_UNITS: usize = 128;
const USE_POLLING_FLAG: &str = "use_polling.flag";
const HERO_CANDIDATE_LIFETIME: Duration = Duration::from_secs(100);
const LIBHL_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const LIBHL_WAIT_INTERVAL: Duration = Duration::from_millis(50);

const DAMAGE_RESULT_FIELDS_STABLE: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::object("baseSkill", "st.skill.BaseSkill"),
    HashLinkFieldSpec::object("target", "ent.GameObject"),
    HashLinkFieldSpec::scalar("_amount", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("_block", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("_hitCount", HashLinkKind::I32),
    HashLinkFieldSpec::scalar("_critical", HashLinkKind::Bool),
    HashLinkFieldSpec::scalar("_kill", HashLinkKind::Bool),
];
const DAMAGE_RESULT_FIELDS_BETA: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::object("skill", "st.skill.BaseSkill"),
    HashLinkFieldSpec::object("target", "ent.GameObject"),
    HashLinkFieldSpec::scalar("_amount", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("_block", HashLinkKind::F64),
    HashLinkFieldSpec::scalar("_hitCount", HashLinkKind::I32),
    HashLinkFieldSpec::scalar("_critical", HashLinkKind::Bool),
    HashLinkFieldSpec::scalar("_kill", HashLinkKind::Bool),
];
const DAMAGE_RESULT_SCHEMA_STABLE: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.skill.DamageResult",
    kind: HashLinkKind::Object,
    fields: DAMAGE_RESULT_FIELDS_STABLE,
};
const DAMAGE_RESULT_SCHEMA_BETA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "st.skill.DamageResult",
    kind: HashLinkKind::Object,
    fields: DAMAGE_RESULT_FIELDS_BETA,
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
const HERO_DAMAGE_SOURCE_FIELDS: &[HashLinkFieldSpec] =
    &[HashLinkFieldSpec::object("ownerPlayer", "st.Player")];
const HERO_DAMAGE_SOURCE_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "ent.Hero",
    kind: HashLinkKind::Object,
    fields: HERO_DAMAGE_SOURCE_FIELDS,
};
const INFLICT_DAMAGE_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object("ent.Unit"),
    HashLinkTypeSpec::Object("st.skill.DamageResult"),
];
const INFLICT_DAMAGE_METHOD: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: "ent.Hero",
    name: c"onInflictDamage",
    arguments: INFLICT_DAMAGE_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static ORIGINAL_ALLOC: AtomicUsize = AtomicUsize::new(0);
static HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static DIRECT_DAMAGE_HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_INFLICT_DAMAGE: AtomicUsize = AtomicUsize::new(0);
static DIRECT_DAMAGE_HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static USE_POLLING: AtomicBool = AtomicBool::new(false);
static DAMAGE_DISPLAY_TYPE: AtomicUsize = AtomicUsize::new(0);
static HERO_TYPE: AtomicUsize = AtomicUsize::new(0);
static GAME_APP_TYPE: AtomicUsize = AtomicUsize::new(0);
static HERO_POINTER: AtomicUsize = AtomicUsize::new(0);
static LOCKED_HERO_POINTER: AtomicUsize = AtomicUsize::new(0);
static LOCKED_PLAYER_POINTER: AtomicUsize = AtomicUsize::new(0);
static CAPTURE_STATUS: AtomicUsize = AtomicUsize::new(0);
static TYPE_QUEUE_DROPS: AtomicU64 = AtomicU64::new(0);
static TYPE_TABLE_DROPS: AtomicU64 = AtomicU64::new(0);
static HERO_DROPS: AtomicU64 = AtomicU64::new(0);
static HERO_CANDIDATES: AtomicU64 = AtomicU64::new(0);
static HERO_LOCKS: AtomicU64 = AtomicU64::new(0);
static DISPLAY_DROPS: AtomicU64 = AtomicU64::new(0);
static DIRECT_DAMAGE_DROPS: AtomicU64 = AtomicU64::new(0);
static DIRECT_DAMAGE_INVALID: AtomicU64 = AtomicU64::new(0);
static DIRECT_DAMAGE_DECODED: AtomicU64 = AtomicU64::new(0);
static HOST_DAMAGE_FILTERED: AtomicU64 = AtomicU64::new(0);
static HOST_DAMAGE_RECEIVED: AtomicU64 = AtomicU64::new(0);
static HOST_DAMAGE_DEALT: AtomicU64 = AtomicU64::new(0);
static HOST_GROUP_DAMAGE_DEALT: AtomicU64 = AtomicU64::new(0);
static POLLING_DAMAGE_DECODED: AtomicU64 = AtomicU64::new(0);
static DECODED: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static OBSERVED_TYPES: ObservedHashLinkTypes<TYPE_SLOTS, TYPE_PROBE_LIMIT> =
    ObservedHashLinkTypes::new();
static TYPE_CANDIDATES: OnceLock<ArrayQueue<Allocation>> = OnceLock::new();
static HEROES: OnceLock<ArrayQueue<usize>> = OnceLock::new();
static DISPLAYS: OnceLock<ArrayQueue<usize>> = OnceLock::new();
static EVENTS: OnceLock<ArrayQueue<RawDamage>> = OnceLock::new();
static DIRECT_DAMAGE_OBSERVATIONS: OnceLock<ArrayQueue<RawDirectDamage>> = OnceLock::new();
static DIRECT_DAMAGE_LAYOUT: OnceLock<DirectDamageLayout> = OnceLock::new();
static DIRECT_DAMAGE_HOOK_ERROR: OnceLock<String> = OnceLock::new();

type HlAllocObj = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
// x86_64 uses the platform C ABI on both Windows and Linux. Installation is
// currently Windows-only, but keeping the callback's ABI platform-native makes
// the eventual Linux port explicit rather than baking in Microsoft's spelling.
type HlInflictDamage = unsafe extern "C" fn(*mut c_void, *mut c_void);

#[derive(Clone, Copy)]
struct Allocation {
    type_pointer: usize,
    object: usize,
}

#[derive(Clone, Copy, Debug)]
struct DirectDamageLayout {
    result_type: usize,
    source_hero_type: usize,
    source_owner_player: usize,
    base_skill: usize,
    target: usize,
    base_skill_kind: usize,
    string_type: usize,
    string_bytes: usize,
    string_length: usize,
    amount: usize,
    blocked: usize,
    hit_ordinal: usize,
    critical: usize,
    killed: usize,
}

#[derive(Clone, Copy, Debug)]
struct RawDirectDamage {
    source_pointer: usize,
    source_owner: usize,
    target_pointer: usize,
    amount: f64,
    blocked: f64,
    hit_ordinal: i32,
    critical: u8,
    killed: u8,
    skill_id_length: u16,
    skill_id: [u16; MAX_SKILL_ID_CODE_UNITS],
}

#[derive(Clone)]
struct RawDamage {
    provider: DamageProvider,
    source_pointer: usize,
    source_owner: usize,
    target_pointer: usize,
    skill_id: String,
    amount: f64,
    critical: bool,
    killed: bool,
    target_id: Option<String>,
    blocked: Option<f64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DamageProvider {
    Direct,
    Polling,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DamageDelivery {
    Dealt {
        actor_id: Option<String>,
        relation: ActorRelation,
    },
    Received,
    Unrelated,
}

#[derive(Clone)]
struct Decoded {
    result_pointer: usize,
    target_pointer: usize,
    hit_ordinal: i32,
    outgoing: bool,
    raw: RawDamage,
}

#[derive(Clone, Hash, Eq, PartialEq)]
struct DedupeKey {
    result_pointer: usize,
    target_pointer: usize,
    amount_bits: u64,
    hit_ordinal: i32,
    skill_id: String,
    critical: bool,
    killed: bool,
}

struct Pending {
    display: usize,
    first_seen: Instant,
    previous: Option<Decoded>,
}

struct PendingHero {
    hero: usize,
    first_seen: Instant,
}

pub struct DamageCapture {
    root: PathBuf,
    attempted: bool,
    worker: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    diagnostics: Vec<String>,
    last_status: usize,
    last_locked_hero: usize,
    last_combat_hook_status: usize,
    last_direct_damage_hook_status: usize,
    last_healing_hook_status: usize,
    last_shield_hook_status: usize,
    last_kill_hook_status: usize,
    last_loot_hook_status: usize,
    last_target_cast_hook_status: usize,
    last_weapon_hook_status: usize,
    last_equipment_hook_status: usize,
    last_inventory_hook_status: usize,
    last_status_hook_status: usize,
    last_cooldown_hook_status: usize,
    last_player_hook_status: usize,
    last_lifecycle_hook_status: usize,
    last_party_hook_status: usize,
    last_ui_window_hook_status: usize,
    reported_event_drops: u64,
    reported_combat_edge_drops: u64,
    reported_ui_window_edge_drops: u64,
    last_party_audience: Vec<(String, bool, bool)>,
    observed_party_damage_sources: HashSet<String>,
}

impl DamageCapture {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            attempted: false,
            worker: None,
            stop: Arc::new(AtomicBool::new(false)),
            diagnostics: Vec::new(),
            last_status: 0,
            last_locked_hero: 0,
            last_combat_hook_status: 0,
            last_direct_damage_hook_status: usize::MAX,
            last_healing_hook_status: usize::MAX,
            last_shield_hook_status: usize::MAX,
            last_kill_hook_status: usize::MAX,
            last_loot_hook_status: usize::MAX,
            last_target_cast_hook_status: usize::MAX,
            last_weapon_hook_status: usize::MAX,
            last_equipment_hook_status: usize::MAX,
            last_inventory_hook_status: usize::MAX,
            last_status_hook_status: usize::MAX,
            last_cooldown_hook_status: usize::MAX,
            last_player_hook_status: usize::MAX,
            last_lifecycle_hook_status: usize::MAX,
            last_party_hook_status: usize::MAX,
            last_ui_window_hook_status: usize::MAX,
            reported_event_drops: 0,
            reported_combat_edge_drops: 0,
            reported_ui_window_edge_drops: 0,
            last_party_audience: Vec::new(),
            observed_party_damage_sources: HashSet::new(),
        }
    }

    pub fn update(
        &mut self,
        process_id: Option<u32>,
        _adapter_live: bool,
        _in_world: bool,
        hero: Option<usize>,
    ) {
        if process_id == Some(std::process::id()) && crate::player_hooks::status() != 1 {
            if let Some(hero) = hero {
                HERO_POINTER.store(hero, Ordering::Release);
            }
        }
        let status = CAPTURE_STATUS.load(Ordering::Acquire);
        if status != self.last_status {
            self.diagnostics
                .push(format!("capture state={}", status_name(status)));
            self.last_status = status;
        }
        let locked_hero = LOCKED_HERO_POINTER.load(Ordering::Acquire);
        if locked_hero != self.last_locked_hero {
            if locked_hero == 0 {
                self.diagnostics.push(
                    "hero tracker lost the local Hero lock; waiting for a replacement".to_owned(),
                );
            } else {
                self.diagnostics.push(format!(
                    "hero tracker locked local ent.Hero pointer=0x{locked_hero:X} validation=type+ownerPlayer+Player.hero+isMe+position"
                ));
            }
            self.last_locked_hero = locked_hero;
        }
        let combat_hook_status = crate::combat_hooks::status();
        if combat_hook_status != self.last_combat_hook_status {
            self.diagnostics.push(format!(
                "combat setter hooks state={} provider={}",
                crate::combat_hooks::status_name(combat_hook_status),
                if crate::combat_hooks::provider_available() {
                    "direct"
                } else {
                    "unavailable"
                }
            ));
            if combat_hook_status == 3 {
                if let Some(error) = crate::combat_hooks::error() {
                    self.diagnostics
                        .push(format!("combat setter hooks error={error}"));
                }
            }
            self.last_combat_hook_status = combat_hook_status;
        }
        let direct_damage_hook_status = DIRECT_DAMAGE_HOOK_STATUS.load(Ordering::Acquire);
        if direct_damage_hook_status != self.last_direct_damage_hook_status {
            let mut message = format!(
                "direct damage hook state={} provider={}",
                direct_damage_hook_status_name(direct_damage_hook_status),
                damage_provider_name(
                    direct_damage_hook_status,
                    USE_POLLING.load(Ordering::Acquire),
                ),
            );
            if direct_damage_hook_status == 3 {
                if let Some(error) = DIRECT_DAMAGE_HOOK_ERROR.get() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_direct_damage_hook_status = direct_damage_hook_status;
        }
        let healing_hook_status = crate::healing_hooks::status();
        if healing_hook_status != self.last_healing_hook_status {
            let mut message = format!(
                "healing hook state={} publication=shadow-only",
                crate::healing_hooks::status_name(healing_hook_status)
            );
            if healing_hook_status == 3 {
                if let Some(error) = crate::healing_hooks::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_healing_hook_status = healing_hook_status;
        }
        let shield_hook_status = crate::shield_hooks::status();
        if shield_hook_status != self.last_shield_hook_status {
            let mut message = format!(
                "shield hook state={} publication=shadow-only",
                crate::shield_hooks::status_name(shield_hook_status)
            );
            if shield_hook_status == 3 {
                if let Some(error) = crate::shield_hooks::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_shield_hook_status = shield_hook_status;
        }
        let kill_hook_status = crate::kill_hooks::status();
        if kill_hook_status != self.last_kill_hook_status {
            let mut message = format!(
                "kill hook state={} publication=shadow-only",
                crate::kill_hooks::status_name(kill_hook_status)
            );
            if kill_hook_status == 3 {
                if let Some(error) = crate::kill_hooks::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_kill_hook_status = kill_hook_status;
        }
        let loot_hook_status = crate::loot_hooks::status();
        if loot_hook_status != self.last_loot_hook_status {
            let mut message = format!(
                "loot hook state={} publication=shadow-only",
                crate::loot_hooks::status_name(loot_hook_status)
            );
            if loot_hook_status == 3 {
                if let Some(error) = crate::loot_hooks::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_loot_hook_status = loot_hook_status;
        }
        let target_cast_hook_status = crate::target_cast_hooks::status();
        if target_cast_hook_status != self.last_target_cast_hook_status {
            let mut message = format!(
                "target cast hooks state={} publication=shadow-only",
                crate::target_cast_hooks::status_name(target_cast_hook_status)
            );
            if target_cast_hook_status == 3 {
                if let Some(error) = crate::target_cast_hooks::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_target_cast_hook_status = target_cast_hook_status;
        }
        let weapon_hook_status = crate::weapon_hooks::status();
        if weapon_hook_status != self.last_weapon_hook_status {
            let mut message = format!(
                "active weapon hook state={} publication=shadow-only",
                crate::weapon_hooks::status_name(weapon_hook_status)
            );
            if weapon_hook_status == 3 {
                if let Some(error) = crate::weapon_hooks::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_weapon_hook_status = weapon_hook_status;
        }
        let equipment_hook_status = crate::equipment_hooks::status();
        if equipment_hook_status != self.last_equipment_hook_status {
            let mut message = format!(
                "equipment hook state={} publication=shadow-only",
                crate::equipment_hooks::status_name(equipment_hook_status)
            );
            if equipment_hook_status == 3 {
                if let Some(error) = crate::equipment_hooks::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_equipment_hook_status = equipment_hook_status;
        }
        let inventory_hook_status = crate::inventory_hooks::status();
        if inventory_hook_status != self.last_inventory_hook_status {
            let mut message = format!(
                "inventory hooks state={} publication=shadow-only",
                crate::inventory_hooks::status_name(inventory_hook_status)
            );
            if inventory_hook_status == 3 {
                if let Some(error) = crate::inventory_hooks::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_inventory_hook_status = inventory_hook_status;
        }
        let status_hook_status = crate::status_hooks::status();
        if status_hook_status != self.last_status_hook_status {
            let mut message = format!(
                "status hooks state={} publication=shadow-only",
                crate::status_hooks::status_name(status_hook_status)
            );
            if status_hook_status == 3 {
                if let Some(error) = crate::status_hooks::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_status_hook_status = status_hook_status;
        }
        let cooldown_hook_status = crate::cooldown_hooks::status();
        if cooldown_hook_status != self.last_cooldown_hook_status {
            let mut message = format!(
                "cooldown hooks state={} publication=shadow-only",
                crate::cooldown_hooks::status_name(cooldown_hook_status)
            );
            if cooldown_hook_status == 3 {
                if let Some(error) = crate::cooldown_hooks::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_cooldown_hook_status = cooldown_hook_status;
        }
        let player_hook_status = crate::player_hooks::status();
        if player_hook_status != self.last_player_hook_status {
            let mut message = format!(
                "local Player hooks state={}",
                crate::player_hooks::status_name(player_hook_status)
            );
            if player_hook_status == 3 {
                if let Some(error) = crate::player_hooks::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_player_hook_status = player_hook_status;
        }
        let ui_window_hook_status = crate::ui_windows::status();
        let party_hook_status = crate::party_hooks::status();
        if party_hook_status != self.last_party_hook_status {
            let mut message = format!(
                "party hooks state={} provider={}",
                crate::party_hooks::status_name(party_hook_status),
                if crate::party_hooks::provider_available() {
                    "hooked-membership"
                } else {
                    "sampled-membership"
                }
            );
            if party_hook_status == 3 {
                if let Some(error) = crate::party_hooks::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_party_hook_status = party_hook_status;
        }
        let lifecycle_hook_status = crate::lifecycle_hooks::status();
        if lifecycle_hook_status != self.last_lifecycle_hook_status {
            let mut message = format!(
                "lifecycle hooks state={} provider={}",
                crate::lifecycle_hooks::status_name(lifecycle_hook_status),
                if crate::lifecycle_hooks::provider_available() {
                    "direct"
                } else {
                    "unavailable"
                }
            );
            if lifecycle_hook_status == 3 {
                if let Some(error) = crate::lifecycle_hooks::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_lifecycle_hook_status = lifecycle_hook_status;
        }
        if ui_window_hook_status != self.last_ui_window_hook_status {
            let mut message = format!(
                "ui window hooks state={}",
                crate::ui_windows::status_name(ui_window_hook_status)
            );
            if ui_window_hook_status == 3 {
                if let Some(error) = crate::ui_windows::error() {
                    message.push_str(&format!(" error={error}"));
                }
            }
            self.diagnostics.push(message);
            self.last_ui_window_hook_status = ui_window_hook_status;
        }
    }

    pub fn drain(&mut self, party: &[PartyDamageSource]) -> (Vec<HostEvent>, u64) {
        self.update_party_audience_diagnostics(party);
        let total_drops = DISPLAY_DROPS
            .load(Ordering::Acquire)
            .saturating_add(DIRECT_DAMAGE_DROPS.load(Ordering::Acquire));
        let dropped = total_drops.saturating_sub(self.reported_event_drops);
        self.reported_event_drops = total_drops;
        let Some(events) = EVENTS.get() else {
            return (Vec::new(), dropped);
        };
        let mut result = Vec::new();
        let local_player = LOCKED_PLAYER_POINTER.load(Ordering::Acquire);
        let local_hero = LOCKED_HERO_POINTER.load(Ordering::Acquire);
        // The host drains at most one bounded observation batch per update.
        // Filtering here, rather than in the hook, leaves capture independent
        // of the eventual solo/group delivery policy.
        for _ in 0..farever_more_api::MAX_EVENT_BATCH {
            let Some(raw) = events.pop() else {
                break;
            };
            let (source_actor_id, source_relation) =
                match damage_delivery(&raw, local_player, local_hero, party) {
                    DamageDelivery::Dealt { actor_id, relation } => {
                        if relation == ActorRelation::GroupMember {
                            HOST_GROUP_DAMAGE_DEALT.fetch_add(1, Ordering::Relaxed);
                        } else {
                            HOST_DAMAGE_DEALT.fetch_add(1, Ordering::Relaxed);
                        }
                        if party.len() > 1 {
                            if let Some(actor_id) = actor_id.as_ref() {
                                self.note_party_damage_source(actor_id, relation);
                            }
                        }
                        (actor_id, relation)
                    }
                    DamageDelivery::Received => {
                        // Counted as a distinct host observation but excluded from
                        // the outgoing-damage delivery policy.
                        HOST_DAMAGE_RECEIVED.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    DamageDelivery::Unrelated => {
                        HOST_DAMAGE_FILTERED.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };
            let (target_actor_id, target_relation) = damage_target(&raw, local_hero, party);
            result.push(HostEvent::Damage(DamageEvent {
                header: EventHeader {
                    sequence: 0,
                    monotonic_ms: 0,
                    quality: SourceQuality::Observed,
                },
                source: CombatActorRef {
                    actor_id: source_actor_id,
                    relation: source_relation,
                    kind: None,
                },
                target: CombatActorRef {
                    actor_id: target_actor_id,
                    relation: target_relation,
                    kind: raw.target_id,
                },
                skill_id: raw.skill_id,
                skill_display_name: None,
                skill_icon: None,
                amount: raw.amount,
                hit_count: 1,
                critical: raw.critical,
                killed: raw.killed,
                blocked: raw.blocked,
            }));
        }
        (result, dropped)
    }

    fn update_party_audience_diagnostics(&mut self, party: &[PartyDamageSource]) {
        let mut audience = if party.len() > 1 {
            party
                .iter()
                .map(|member| {
                    (
                        member.actor_id.clone(),
                        member.is_local,
                        member.hero_pointer.is_some(),
                    )
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        audience.sort_unstable();
        if audience == self.last_party_audience {
            return;
        }
        self.observed_party_damage_sources.clear();
        if audience.is_empty() {
            self.diagnostics
                .push("party damage audience inactive; local-only".to_owned());
        } else {
            let actors = audience
                .iter()
                .map(|(actor_id, is_local, loaded)| {
                    format!(
                        "{actor_id}:{}:{}",
                        if *is_local { "local" } else { "group" },
                        if *loaded { "loaded" } else { "unloaded" }
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            self.diagnostics.push(format!(
                "party damage audience active members={} actors={actors}",
                audience.len()
            ));
        }
        self.last_party_audience = audience;
    }

    fn note_party_damage_source(&mut self, actor_id: &str, relation: ActorRelation) {
        if self
            .observed_party_damage_sources
            .insert(actor_id.to_owned())
        {
            self.diagnostics.push(format!(
                "party damage first observed actor_id={actor_id} relation={relation:?}"
            ));
        }
    }

    /// Returns local-player combat edges captured from Farever's exact
    /// `set_isInCombat` mutation boundary. The boolean reports whether this
    /// provider is active so callers can gate sampled fallbacks.
    pub fn drain_combat_edges(&mut self) -> (bool, Vec<bool>, u64) {
        let available = crate::combat_hooks::provider_available();
        let total_drops = crate::combat_hooks::total_drops();
        let dropped = total_drops.saturating_sub(self.reported_combat_edge_drops);
        self.reported_combat_edge_drops = total_drops;
        let edges = crate::combat_hooks::drain_combat_edges();
        (available, edges, dropped)
    }

    /// Returns build-validated UI lifecycle observations from the direct
    /// `BaseUI.displayWindow` / `removeWindow` hooks. The runtime selects these
    /// exclusively when healthy and retains sampled membership for direct
    /// activation, reconciliation, and hook-unavailable fallback.
    pub(crate) fn drain_window_edges(
        &mut self,
    ) -> (bool, Vec<crate::ui_windows::WindowHookEdge>, u64) {
        let available = crate::ui_windows::status() == 1;
        let total_drops = crate::ui_windows::total_drops();
        let dropped = total_drops.saturating_sub(self.reported_ui_window_edge_drops);
        self.reported_ui_window_edge_drops = total_drops;
        (available, crate::ui_windows::drain_edges(), dropped)
    }

    pub fn take_diagnostics(&mut self) -> Vec<String> {
        std::mem::take(&mut self.diagnostics)
    }

    pub fn metrics(&self) -> String {
        format!(
            "state={} hero={} combat_hooks={} damage_provider={} direct_damage_hook={} hero_candidates={} hero_locks={} decoded={} polling_decoded={} direct_decoded={} host_damage_dealt={} host_group_damage_dealt={} host_damage_received={} host_damage_filtered={} invalid={} type_queue_drops={} type_table_drops={} hero_queue_drops={} display_queue_drops={} direct_damage_invalid={} direct_damage_queue_drops={} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
            status_name(CAPTURE_STATUS.load(Ordering::Relaxed)),
            if LOCKED_HERO_POINTER.load(Ordering::Relaxed) == 0 {
                "waiting"
            } else {
                "locked"
            },
            crate::combat_hooks::status_name(crate::combat_hooks::status()),
            damage_provider_name(
                DIRECT_DAMAGE_HOOK_STATUS.load(Ordering::Relaxed),
                USE_POLLING.load(Ordering::Relaxed),
            ),
            direct_damage_hook_status_name(DIRECT_DAMAGE_HOOK_STATUS.load(Ordering::Relaxed)),
            HERO_CANDIDATES.load(Ordering::Relaxed),
            HERO_LOCKS.load(Ordering::Relaxed),
            DECODED.load(Ordering::Relaxed),
            POLLING_DAMAGE_DECODED.load(Ordering::Relaxed),
            DIRECT_DAMAGE_DECODED.load(Ordering::Relaxed),
            HOST_DAMAGE_DEALT.load(Ordering::Relaxed),
            HOST_GROUP_DAMAGE_DEALT.load(Ordering::Relaxed),
            HOST_DAMAGE_RECEIVED.load(Ordering::Relaxed),
            HOST_DAMAGE_FILTERED.load(Ordering::Relaxed),
            INVALID.load(Ordering::Relaxed),
            TYPE_QUEUE_DROPS.load(Ordering::Relaxed),
            TYPE_TABLE_DROPS.load(Ordering::Relaxed),
            HERO_DROPS.load(Ordering::Relaxed),
            DISPLAY_DROPS.load(Ordering::Relaxed),
            DIRECT_DAMAGE_INVALID.load(Ordering::Relaxed),
            DIRECT_DAMAGE_DROPS.load(Ordering::Relaxed),
            crate::player_hooks::metrics(),
            crate::healing_hooks::metrics(),
            crate::shield_hooks::metrics(),
            crate::kill_hooks::metrics(),
            crate::loot_hooks::metrics(),
            crate::target_cast_hooks::metrics(),
            crate::weapon_hooks::metrics(),
            crate::equipment_hooks::metrics(),
            crate::inventory_hooks::metrics(),
            crate::status_hooks::metrics(),
            crate::cooldown_hooks::metrics(),
            crate::lifecycle_hooks::metrics(),
            crate::combat_hooks::metrics(),
            crate::party_hooks::metrics(),
            crate::ui_windows::metrics(),
            crate::activity_hooks::metrics(),
        )
    }

    /// Installs the existing allocation observer before the state locator is
    /// allowed to scan. This is only called by the injected, known-build host.
    pub fn arm(&mut self) -> bool {
        if self.attempted {
            return self.worker.is_some() && CAPTURE_STATUS.load(Ordering::Acquire) != 3;
        }
        self.attempted = true;
        self.start();
        self.worker.is_some() && CAPTURE_STATUS.load(Ordering::Acquire) != 3
    }

    /// `Some(false)` means the in-process observer has not locked a validated
    /// local Hero. Once `arm` has been attempted, failure or an unsupported
    /// build remains `Some(false)` so injected startup never falls back to a
    /// window-size or process-growth heuristic.
    pub fn world_probe(&self) -> Option<bool> {
        self.attempted
            .then(|| LOCKED_HERO_POINTER.load(Ordering::Acquire) != 0)
    }

    fn start(&mut self) {
        let use_polling = self.root.join(USE_POLLING_FLAG).exists();
        USE_POLLING.store(use_polling, Ordering::Release);
        let game_directory = match std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(ToOwned::to_owned))
        {
            Some(path) => path,
            None => {
                self.diagnostics
                    .push("capture could not resolve the game directory".to_owned());
                return;
            }
        };
        let game_build = match game_build::verify_installed(&game_directory) {
            Ok((profile, hashes)) => {
                self.diagnostics.push(format!(
                    "capture build verified profile={} hashes={hashes}",
                    profile.name()
                ));
                profile
            }
            Err(error) => {
                self.diagnostics
                    .push(format!("capture unsupported: {error}"));
                return;
            }
        };
        DIRECT_DAMAGE_HOOK_STATUS.store(0, Ordering::Release);
        self.diagnostics.push(if use_polling {
            format!(
                "{USE_POLLING_FLAG} selected DamageDisplay polling; direct hooks remain installed"
            )
        } else {
            "direct hooks preferred; event types without an active hook use polling automatically"
                .to_owned()
        });
        prepare_queues();
        CAPTURE_STATUS.store(4, Ordering::Release);
        let stop = Arc::clone(&self.stop);
        self.worker = thread::Builder::new()
            .name("farever-hashlink-observer".to_owned())
            .spawn(move || {
                if wait_for_libhl_and_install(&stop).is_err() {
                    CAPTURE_STATUS.store(3, Ordering::Release);
                    return;
                }
                CAPTURE_STATUS.store(1, Ordering::Release);
                decode_worker(stop, game_build);
            })
            .ok();
        if self.worker.is_none() {
            ACTIVE.store(false, Ordering::Release);
            CAPTURE_STATUS.store(3, Ordering::Release);
            self.diagnostics
                .push("capture observer worker failed to start".to_owned());
        }
    }
}

/// Returns the exact `GameApp` runtime type learned by the process-global
/// allocation observer. The observer worker validates the type name before
/// publishing it; consumers must still validate any metadata/root chain they
/// derive from the pointer.
pub(crate) fn observed_game_app_type() -> Option<usize> {
    let type_pointer = GAME_APP_TYPE.load(Ordering::Acquire);
    (type_pointer >= 0x1_0000).then_some(type_pointer)
}

impl Drop for DamageCapture {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        crate::slash_commands::shutdown_hook();
        crate::chat_output::shutdown_hook();
        crate::player_hooks::shutdown_hooks();
        crate::healing_hooks::shutdown_hook();
        crate::shield_hooks::shutdown_hook();
        crate::kill_hooks::shutdown_hook();
        crate::loot_hooks::shutdown_hook();
        crate::target_cast_hooks::shutdown_hooks();
        crate::weapon_hooks::shutdown_hook();
        crate::equipment_hooks::shutdown_hook();
        crate::inventory_hooks::shutdown_hooks();
        crate::status_hooks::shutdown_hooks();
        crate::cooldown_hooks::shutdown_hooks();
        crate::lifecycle_hooks::shutdown_hooks();
        crate::combat_hooks::shutdown_hooks();
        crate::party_hooks::shutdown_hooks();
        crate::ui_windows::shutdown_hooks();
        crate::map_clicks::shutdown_hooks();
        crate::activity_hooks::shutdown_hook();
        let target = HOOK_TARGET.load(Ordering::Acquire);
        if target != 0 {
            // SAFETY: `target` is stored only after MinHook creates the
            // `hl_alloc_obj` detour, and the worker has been joined above.
            let _ = unsafe { MinHook::disable_hook(target as *mut c_void) };
        }
        let target = DIRECT_DAMAGE_HOOK_TARGET.load(Ordering::Acquire);
        if target != 0 {
            // SAFETY: the target is published only after the build-gated,
            // signature-checked damage hook has been created and enabled.
            let _ = unsafe { MinHook::disable_hook(target as *mut c_void) };
        }
    }
}

fn prepare_queues() {
    crate::activity_hooks::prepare_queue();
    crate::player_hooks::prepare_queue();
    crate::healing_hooks::prepare_queue();
    crate::shield_hooks::prepare_queue();
    crate::kill_hooks::prepare_queue();
    crate::loot_hooks::prepare_queue();
    crate::target_cast_hooks::prepare_queue();
    crate::weapon_hooks::prepare_queue();
    crate::equipment_hooks::prepare_queue();
    crate::inventory_hooks::prepare_queue();
    crate::status_hooks::prepare_queue();
    crate::cooldown_hooks::prepare_queue();
    crate::lifecycle_hooks::prepare_queues();
    crate::combat_hooks::prepare_queues();
    crate::party_hooks::prepare_queue();
    crate::slash_commands::prepare_queue();
    crate::chat_output::prepare_queue();
    crate::ui_windows::prepare_queues();
    crate::map_clicks::prepare_queue();
    let _ = TYPE_CANDIDATES.get_or_init(|| ArrayQueue::new(TYPE_QUEUE_CAPACITY));
    let _ = HEROES.get_or_init(|| ArrayQueue::new(MAX_PENDING));
    let _ = DISPLAYS.get_or_init(|| ArrayQueue::new(RAW_QUEUE_CAPACITY));
    let _ = EVENTS.get_or_init(|| ArrayQueue::new(EVENT_QUEUE_CAPACITY));
    let _ =
        DIRECT_DAMAGE_OBSERVATIONS.get_or_init(|| ArrayQueue::new(DIRECT_DAMAGE_QUEUE_CAPACITY));
}

fn wait_for_libhl_and_install(stop: &AtomicBool) -> Result<(), String> {
    let started = Instant::now();
    while !stop.load(Ordering::Acquire) {
        if let Some(runtime) = HashLinkRuntime::loaded() {
            return install_hook(&runtime);
        }
        if started.elapsed() >= LIBHL_WAIT_TIMEOUT {
            return Err("libhl.dll did not load within 30 seconds".to_owned());
        }
        thread::sleep(LIBHL_WAIT_INTERVAL);
    }
    Err("observer stopped while waiting for libhl.dll".to_owned())
}

fn install_hook(runtime: &HashLinkRuntime) -> Result<(), String> {
    // This allocator detour is process-global. Future direct providers must
    // share its bounded type-observation path instead of installing another
    // `hl_alloc_obj` hook.
    let target = runtime.export(c"hl_alloc_obj")? as *mut c_void;
    // SAFETY: build hashes are verified before this path, and the known libhl
    // export uses `HlAllocObj`'s C ABI. MinHook returns the original trampoline.
    let original = std::panic::catch_unwind(|| unsafe {
        MinHook::create_hook(target, hook_alloc_obj as *mut c_void)
    })
    .map_err(|_| "MinHook initialization panicked".to_owned())?
    .map_err(|status| format!("create_hook returned {status:?}"))?;
    ORIGINAL_ALLOC.store(original as usize, Ordering::Release);
    HOOK_TARGET.store(target as usize, Ordering::Release);
    // SAFETY: the hook and trampoline were created successfully above and both
    // remain process-global until `DamageCapture::drop` disables the hook.
    unsafe { MinHook::enable_hook(target) }
        .map_err(|status| format!("enable_hook returned {status:?}"))?;
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

unsafe extern "C" fn hook_alloc_obj(type_pointer: *mut c_void) -> *mut c_void {
    let original = ORIGINAL_ALLOC.load(Ordering::Acquire);
    if original == 0 {
        return std::ptr::null_mut();
    }
    // SAFETY: `ORIGINAL_ALLOC` is published only from a successful MinHook
    // installation against the build-verified `hl_alloc_obj` export.
    let original: HlAllocObj = unsafe { std::mem::transmute(original) };
    // SAFETY: MinHook invokes this callback with `hl_alloc_obj`'s original
    // argument; forwarding it before observation preserves allocator behavior.
    let object = unsafe { original(type_pointer) };
    if !ACTIVE.load(Ordering::Relaxed) || object.is_null() {
        return object;
    }
    let type_address = type_pointer as usize;
    let object_address = object as usize;
    let hero_type = HERO_TYPE.load(Ordering::Relaxed);
    let damage_type = DAMAGE_DISPLAY_TYPE.load(Ordering::Relaxed);
    let mut recognized = false;
    if hero_type != 0 && hero_type == type_address {
        queue_hero(object_address);
        recognized = true;
    }
    if damage_type != 0 && damage_type == type_address {
        queue_display(object_address);
        recognized = true;
    }
    if !recognized {
        match OBSERVED_TYPES.observe(type_address, object_address) {
            TypeObservation::FirstSeen => {
                let queued = TYPE_CANDIDATES.get().is_some_and(|queue| {
                    queue
                        .push(Allocation {
                            type_pointer: type_address,
                            object: object_address,
                        })
                        .is_ok()
                });
                if !queued {
                    // Do not permanently classify a type as seen when its only
                    // worker record was lost. A later allocation can retry.
                    OBSERVED_TYPES.forget(type_address);
                    TYPE_QUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
                }
            }
            TypeObservation::Full => {
                TYPE_TABLE_DROPS.fetch_add(1, Ordering::Relaxed);
            }
            TypeObservation::AlreadySeen => {}
        }
    }
    object
}

unsafe extern "C" fn hook_on_inflict_damage(source: *mut c_void, result: *mut c_void) {
    let capture_active = direct_damage_provider_selected();
    let observation = if capture_active {
        // SAFETY: installation validates this callback's exact two-argument
        // HashLink signature and every field/string offset before enabling the
        // detour. All reads are bounded copies made while HashLink still owns
        // the live callback arguments.
        unsafe { copy_direct_damage_observation(source, result) }
    } else {
        None
    };

    let original = ORIGINAL_INFLICT_DAMAGE.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: MinHook returned this trampoline for the exact
        // `(ent.Unit, st.skill.DamageResult) -> Void` target validated during
        // installation. `extern "C"` selects the platform x86_64 C ABI.
        let original: HlInflictDamage = unsafe { std::mem::transmute(original) };
        unsafe { original(source, result) };
    }

    if capture_active {
        if let Some(observation) = observation {
            if DIRECT_DAMAGE_OBSERVATIONS
                .get()
                .is_none_or(|queue| queue.push(observation).is_err())
            {
                DIRECT_DAMAGE_DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

unsafe fn copy_direct_damage_observation(
    source: *mut c_void,
    result: *mut c_void,
) -> Option<RawDirectDamage> {
    if source.is_null() || result.is_null() {
        DIRECT_DAMAGE_INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let Some(layout) = DIRECT_DAMAGE_LAYOUT.get().copied() else {
        DIRECT_DAMAGE_INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    let source_base = source.cast::<u8>();
    // The receiver is declared as ent.Unit and may be a monster or another
    // subclass. Read the Hero-only ownerPlayer field only after proving the
    // concrete runtime type; every other source still remains observable.
    let source_owner = if unsafe { object_has_exact_type(source, layout.source_hero_type) } {
        // SAFETY: the exact source type and the inherited ownerPlayer field
        // offset were both shape-validated before the hook was enabled.
        unsafe {
            std::ptr::read_unaligned(source_base.add(layout.source_owner_player).cast::<usize>())
        }
    } else {
        0
    };

    let result_base = result.cast::<u8>();
    // SAFETY: `result` is the live object argument of the signature-validated
    // HashLink callback for the duration of this bounded copy.
    if !unsafe { object_has_exact_type(result, layout.result_type) } {
        DIRECT_DAMAGE_INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // SAFETY: `layout` contains shape-validated in-object field offsets. The
    // base skill and its kind are strongly typed object fields kept alive by
    // the live DamageResult callback argument.
    let base_skill =
        unsafe { std::ptr::read_unaligned(result_base.add(layout.base_skill).cast::<usize>()) };
    let target_pointer =
        unsafe { std::ptr::read_unaligned(result_base.add(layout.target).cast::<usize>()) };
    if base_skill < 0x1_0000 {
        DIRECT_DAMAGE_INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let skill_string = unsafe {
        std::ptr::read_unaligned(
            (base_skill as *const u8)
                .add(layout.base_skill_kind)
                .cast::<usize>(),
        )
    };
    if !unsafe { object_has_exact_type(skill_string as *const c_void, layout.string_type) } {
        DIRECT_DAMAGE_INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let string_base = skill_string as *const u8;
    let skill_length =
        unsafe { std::ptr::read_unaligned(string_base.add(layout.string_length).cast::<i32>()) };
    if !(1..=MAX_SKILL_ID_CODE_UNITS as i32).contains(&skill_length) {
        DIRECT_DAMAGE_INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let skill_bytes =
        unsafe { std::ptr::read_unaligned(string_base.add(layout.string_bytes).cast::<usize>()) };
    if skill_bytes < 0x1_0000 {
        DIRECT_DAMAGE_INVALID.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut skill_id = [0_u16; MAX_SKILL_ID_CODE_UNITS];
    // SAFETY: the exact String layout is validated before hook installation;
    // `skill_length` is positive and capped to the destination array.
    unsafe {
        std::ptr::copy_nonoverlapping(
            skill_bytes as *const u16,
            skill_id.as_mut_ptr(),
            skill_length as usize,
        );
    }

    let amount = unsafe { std::ptr::read_unaligned(result_base.add(layout.amount).cast()) };
    let blocked = unsafe { std::ptr::read_unaligned(result_base.add(layout.blocked).cast()) };
    let hit_ordinal =
        unsafe { std::ptr::read_unaligned(result_base.add(layout.hit_ordinal).cast()) };
    let critical = unsafe { std::ptr::read_unaligned(result_base.add(layout.critical).cast()) };
    let killed = unsafe { std::ptr::read_unaligned(result_base.add(layout.killed).cast()) };
    Some(RawDirectDamage {
        source_pointer: source as usize,
        source_owner,
        target_pointer,
        amount,
        blocked,
        hit_ordinal,
        critical,
        killed,
        skill_id_length: skill_length as u16,
        skill_id,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SoloDamageRelation {
    Dealt,
    Received,
    Unrelated,
}

fn damage_delivery(
    damage: &RawDamage,
    local_player: usize,
    local_hero: usize,
    party: &[PartyDamageSource],
) -> DamageDelivery {
    // Only the direct hook copies the real source Hero and owner Player.
    // DamageDisplay polling synthesizes the local source for outgoing text and
    // therefore must never be used to attribute a remote party member.
    if damage.provider == DamageProvider::Direct && party.len() > 1 {
        if let Some(member) = validated_party_source(damage, party) {
            return DamageDelivery::Dealt {
                actor_id: Some(member.actor_id.clone()),
                relation: if member.is_local {
                    ActorRelation::LocalPlayer
                } else {
                    ActorRelation::GroupMember
                },
            };
        }
        return if damage.target_pointer == local_hero {
            DamageDelivery::Received
        } else {
            DamageDelivery::Unrelated
        };
    }

    match solo_damage_relation(damage, local_player, local_hero) {
        SoloDamageRelation::Dealt => DamageDelivery::Dealt {
            actor_id: None,
            relation: ActorRelation::LocalPlayer,
        },
        SoloDamageRelation::Received => DamageDelivery::Received,
        SoloDamageRelation::Unrelated => DamageDelivery::Unrelated,
    }
}

fn validated_party_source<'a>(
    damage: &RawDamage,
    party: &'a [PartyDamageSource],
) -> Option<&'a PartyDamageSource> {
    let by_owner = party
        .iter()
        .find(|member| damage.source_owner == member.player_pointer);
    let by_hero = party
        .iter()
        .find(|member| member.hero_pointer == Some(damage.source_pointer));
    match (by_owner, by_hero) {
        (Some(owner), Some(hero)) if owner.actor_id != hero.actor_id => None,
        (Some(member), _) | (_, Some(member)) => Some(member),
        (None, None) => None,
    }
}

fn damage_target(
    damage: &RawDamage,
    local_hero: usize,
    party: &[PartyDamageSource],
) -> (Option<String>, ActorRelation) {
    if party.len() > 1 {
        if let Some(member) = party
            .iter()
            .find(|member| member.hero_pointer == Some(damage.target_pointer))
        {
            return (
                Some(member.actor_id.clone()),
                if member.is_local {
                    ActorRelation::LocalPlayer
                } else {
                    ActorRelation::GroupMember
                },
            );
        }
    }
    if damage.target_pointer == local_hero {
        (None, ActorRelation::LocalPlayer)
    } else {
        (None, ActorRelation::Other)
    }
}

fn solo_damage_relation(
    damage: &RawDamage,
    local_player: usize,
    local_hero: usize,
) -> SoloDamageRelation {
    if local_hero < 0x1_0000 || local_player < 0x1_0000 {
        return SoloDamageRelation::Unrelated;
    }
    if damage.source_owner == local_player || damage.source_pointer == local_hero {
        SoloDamageRelation::Dealt
    } else if damage.target_pointer == local_hero {
        SoloDamageRelation::Received
    } else {
        SoloDamageRelation::Unrelated
    }
}

fn queue_hero(hero: usize) {
    HERO_CANDIDATES.fetch_add(1, Ordering::Relaxed);
    let Some(queue) = HEROES.get() else {
        HERO_DROPS.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if queue.force_push(hero).is_some() {
        // Keep the newest allocation rather than the oldest: zone changes
        // allocate bursts of proxy Heroes before the live local Hero appears,
        // so the oldest candidates are the least likely to be the real one.
        HERO_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

fn queue_display(display: usize) {
    if direct_damage_provider_selected() {
        return;
    }
    if DISPLAYS
        .get()
        .is_none_or(|queue| queue.push(display).is_err())
    {
        DISPLAY_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

fn decode_worker(stop: Arc<AtomicBool>, game_build: GameBuildProfile) {
    let Some(memory) = ProcessMemory::open(std::process::id()) else {
        CAPTURE_STATUS.store(3, Ordering::Release);
        return;
    };
    let mut pending = VecDeque::new();
    let mut pending_heroes = VecDeque::new();
    let mut dedupe = HashMap::<DedupeKey, Instant>::new();
    let mut consecutive_failures = 0_u32;
    let mut consecutive_direct_failures = 0_u64;
    let mut last_direct_invalid = 0_u64;
    let mut last_direct_decoded = 0_u64;
    let mut hero_tick = 0_u64;
    let mut direct_was_selected = false;
    let mut window_hook_decoder = crate::ui_windows::WindowHookDecoder::default();
    let mut lifecycle_hook_decoder = crate::lifecycle_hooks::LifecycleHookDecoder::default();
    let mut healing_hook_decoder = crate::healing_hooks::HealingHookDecoder::default();
    let mut shield_hook_decoder = crate::shield_hooks::ShieldHookDecoder;
    let mut target_cast_hook_decoder = crate::target_cast_hooks::TargetCastHookDecoder::default();
    let mut cooldown_hook_decoder = crate::cooldown_hooks::CooldownHookDecoder;
    while !stop.load(Ordering::Acquire) {
        let hl = HashLink::new(&memory);
        learn_observed_types(&hl);
        crate::slash_commands::try_install_hook(&hl);
        crate::chat_output::try_install_hook(&hl, game_build);
        crate::player_hooks::try_install_hooks(&hl, game_build);
        crate::player_hooks::decode_pending();
        crate::healing_hooks::try_install_hook(&hl, game_build);
        healing_hook_decoder.decode_pending();
        crate::shield_hooks::try_install_hook(&hl);
        shield_hook_decoder.decode_pending();
        crate::kill_hooks::try_install_hook(&hl);
        crate::kill_hooks::decode_pending();
        crate::loot_hooks::try_install_hook(&hl);
        crate::loot_hooks::decode_pending();
        crate::target_cast_hooks::try_install_hooks(&hl);
        target_cast_hook_decoder.decode_pending(&hl);
        crate::weapon_hooks::try_install_hook(&hl);
        crate::weapon_hooks::decode_pending();
        crate::equipment_hooks::try_install_hook(&hl);
        crate::equipment_hooks::decode_pending();
        crate::inventory_hooks::try_install_hooks(&hl, game_build);
        crate::inventory_hooks::decode_pending();
        crate::status_hooks::try_install_hooks(&hl, game_build);
        crate::status_hooks::decode_pending();
        crate::cooldown_hooks::try_install_hooks(&hl);
        cooldown_hook_decoder.decode_pending(&hl);
        crate::lifecycle_hooks::try_install_hooks(&hl, game_build);
        lifecycle_hook_decoder.decode_pending(&hl);
        crate::combat_hooks::try_install_hooks(&hl);
        crate::combat_hooks::decode_pending();
        crate::party_hooks::try_install_hooks(&hl, game_build);
        crate::ui_windows::try_install_hooks(&hl);
        crate::map_clicks::try_install_hooks(&hl, game_build);
        crate::activity_hooks::try_install_hook(&hl);
        window_hook_decoder.decode_pending(&hl);
        replay_latest_known_allocations();
        collect_hero_candidates(&mut pending_heroes);
        hero_tick = hero_tick.wrapping_add(1);
        update_local_hero(&hl, &mut pending_heroes, hero_tick);
        try_install_direct_damage_hook(&hl, game_build);
        let direct_selected = direct_damage_provider_selected();
        if direct_selected && !direct_was_selected {
            // Discard any DamageDisplay records captured before this atomic
            // provider handover so no logical hit can be published by both
            // providers. Normal startup resolves DamageResult from the method
            // signature and installs this hook before the first combat event.
            pending.clear();
            if let Some(queue) = DISPLAYS.get() {
                while queue.pop().is_some() {}
            }
            dedupe.clear();
            consecutive_failures = 0;
        }
        direct_was_selected = direct_selected;
        if direct_selected {
            drain_direct_damage_observations();
            let invalid = DIRECT_DAMAGE_INVALID.load(Ordering::Acquire);
            let decoded = DIRECT_DAMAGE_DECODED.load(Ordering::Acquire);
            if decoded != last_direct_decoded {
                consecutive_direct_failures = 0;
            } else {
                consecutive_direct_failures = consecutive_direct_failures
                    .saturating_add(invalid.saturating_sub(last_direct_invalid));
            }
            last_direct_invalid = invalid;
            last_direct_decoded = decoded;
            if consecutive_direct_failures >= 64 {
                let _ = DIRECT_DAMAGE_HOOK_ERROR.set(
                    "64 direct damage records failed validation; resumed DamageDisplay polling"
                        .to_owned(),
                );
                DIRECT_DAMAGE_HOOK_STATUS.store(3, Ordering::Release);
            }
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        if let Some(queue) = DISPLAYS.get() {
            while pending.len() < MAX_PENDING {
                let Some(display) = queue.pop() else {
                    break;
                };
                pending.push_back(Pending {
                    display,
                    first_seen: Instant::now(),
                    previous: None,
                });
            }
        }
        let now = Instant::now();
        dedupe.retain(|_, seen| now.duration_since(*seen) < Duration::from_secs(3));
        for _ in 0..MAX_DECODE_PER_TICK.min(pending.len()) {
            let Some(mut item) = pending.pop_front() else {
                break;
            };
            match decode_display(&hl, item.display, game_build) {
                Some(decoded) => {
                    if !decoded.outgoing {
                        // DamageDisplay is also used for incoming damage. A
                        // verified Hero target is a successful decode that is
                        // intentionally outside this provider's contract.
                        consecutive_failures = 0;
                        continue;
                    }
                    let confirmed = item.previous.as_ref().is_some_and(|previous| {
                        previous.result_pointer == decoded.result_pointer
                            && previous.target_pointer == decoded.target_pointer
                            && previous.hit_ordinal == decoded.hit_ordinal
                            && previous.raw.skill_id == decoded.raw.skill_id
                            && previous.raw.amount.to_bits() == decoded.raw.amount.to_bits()
                    });
                    if confirmed {
                        let key = DedupeKey {
                            result_pointer: decoded.result_pointer,
                            target_pointer: decoded.target_pointer,
                            amount_bits: decoded.raw.amount.to_bits(),
                            hit_ordinal: decoded.hit_ordinal,
                            skill_id: decoded.raw.skill_id.clone(),
                            critical: decoded.raw.critical,
                            killed: decoded.raw.killed,
                        };
                        if dedupe.insert(key, now).is_none() {
                            if EVENTS
                                .get()
                                .is_some_and(|events| events.push(decoded.raw).is_ok())
                            {
                                DECODED.fetch_add(1, Ordering::Relaxed);
                                POLLING_DAMAGE_DECODED.fetch_add(1, Ordering::Relaxed);
                            } else {
                                DISPLAY_DROPS.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        consecutive_failures = 0;
                    } else {
                        item.previous = Some(decoded);
                        pending.push_back(item);
                    }
                }
                None if item.first_seen.elapsed() < Duration::from_secs(1) => {
                    pending.push_back(item);
                }
                None => {
                    INVALID.fetch_add(1, Ordering::Relaxed);
                    consecutive_failures += 1;
                }
            }
        }
        if consecutive_failures >= 64 {
            ACTIVE.store(false, Ordering::Release);
            CAPTURE_STATUS.store(3, Ordering::Release);
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn try_install_direct_damage_hook(hl: &HashLink<'_>, game_build: GameBuildProfile) {
    if DIRECT_DAMAGE_HOOK_STATUS.load(Ordering::Acquire) != 0 {
        return;
    }
    let hero_type = HERO_TYPE.load(Ordering::Acquire);
    if hero_type == 0 {
        return;
    }
    if DIRECT_DAMAGE_HOOK_STATUS
        .compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let result = HashLinkRuntime::loaded()
        .ok_or_else(|| "libhl.dll is not loaded".to_owned())
        .and_then(|runtime| {
            let method = runtime.resolve_method(hl, hero_type, &INFLICT_DAMAGE_METHOD)?;
            let damage_result_type = method.argument_type(1).ok_or_else(|| {
                "validated onInflictDamage signature omitted DamageResult argument".to_owned()
            })?;
            if DIRECT_DAMAGE_LAYOUT.get().is_none() {
                let layout =
                    resolve_direct_damage_layout(hl, hero_type, damage_result_type, game_build)
                        .map_err(|error| {
                            format!("could not resolve direct damage object layouts: {error}")
                        })?;
                let _ = DIRECT_DAMAGE_LAYOUT.set(layout);
            }
            Ok(method)
        })
        .and_then(install_direct_damage_hook);
    match result {
        Ok(()) => DIRECT_DAMAGE_HOOK_STATUS.store(1, Ordering::Release),
        Err(error) => {
            let _ = DIRECT_DAMAGE_HOOK_ERROR.set(error);
            DIRECT_DAMAGE_HOOK_STATUS.store(3, Ordering::Release);
        }
    }
}

fn install_direct_damage_hook(method: ValidatedHashLinkMethod) -> Result<(), String> {
    let target = method.target() as *mut c_void;
    // SAFETY: resolution is build-gated, name-addressed, and verifies the full
    // two-object-argument/Void HashLink signature before reaching this call.
    let original = std::panic::catch_unwind(|| unsafe {
        MinHook::create_hook(target, hook_on_inflict_damage as *mut c_void)
    })
    .map_err(|_| {
        format!(
            "MinHook initialization panicked for {}",
            method.name().to_string_lossy()
        )
    })?
    .map_err(|status| {
        format!(
            "create {} hook returned {status:?}",
            method.name().to_string_lossy()
        )
    })?;
    ORIGINAL_INFLICT_DAMAGE.store(original as usize, Ordering::Release);
    // SAFETY: the hook and original trampoline were created immediately above.
    if let Err(status) = unsafe { MinHook::enable_hook(target) } {
        // SAFETY: the hook exists but was not successfully enabled.
        let _ = unsafe { MinHook::remove_hook(target) };
        ORIGINAL_INFLICT_DAMAGE.store(0, Ordering::Release);
        return Err(format!(
            "enable {} hook returned {status:?}",
            method.name().to_string_lossy()
        ));
    }
    DIRECT_DAMAGE_HOOK_TARGET.store(target as usize, Ordering::Release);
    Ok(())
}

fn drain_direct_damage_observations() {
    let Some(queue) = DIRECT_DAMAGE_OBSERVATIONS.get() else {
        return;
    };
    for _ in 0..farever_more_api::MAX_EVENT_BATCH {
        let Some(observation) = queue.pop() else {
            break;
        };
        let Some(raw) = decode_direct_damage_observation(&observation) else {
            DIRECT_DAMAGE_INVALID.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        if EVENTS.get().is_some_and(|events| events.push(raw).is_ok()) {
            DECODED.fetch_add(1, Ordering::Relaxed);
            DIRECT_DAMAGE_DECODED.fetch_add(1, Ordering::Relaxed);
        } else {
            DIRECT_DAMAGE_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn decode_direct_damage_observation(observation: &RawDirectDamage) -> Option<RawDamage> {
    let length = usize::from(observation.skill_id_length);
    if length == 0
        || length > observation.skill_id.len()
        || !observation.amount.is_finite()
        || !(0.0..100_000_000.0).contains(&observation.amount)
        || observation.amount == 0.0
    {
        return None;
    }
    if !observation.blocked.is_finite()
        || !(0.0..100_000_000.0).contains(&observation.blocked)
        || !(1..=10_000).contains(&observation.hit_ordinal)
        || observation.critical > 1
        || observation.killed > 1
    {
        return None;
    }
    let skill_id = String::from_utf16(&observation.skill_id[..length]).ok()?;
    if skill_id.is_empty() {
        return None;
    }
    Some(RawDamage {
        provider: DamageProvider::Direct,
        source_pointer: observation.source_pointer,
        source_owner: observation.source_owner,
        target_pointer: observation.target_pointer,
        skill_id,
        amount: observation.amount,
        critical: observation.critical == 1,
        killed: observation.killed == 1,
        target_id: None,
        blocked: Some(observation.blocked),
    })
}

fn collect_hero_candidates(pending: &mut VecDeque<PendingHero>) {
    let Some(queue) = HEROES.get() else {
        return;
    };
    let capacity = if LOCKED_HERO_POINTER.load(Ordering::Acquire) == 0 {
        MAX_PENDING
    } else {
        MAX_LOCKED_HERO_PENDING
    };
    while let Some(hero) = queue.pop() {
        pending.push_back(PendingHero {
            hero,
            first_seen: Instant::now(),
        });
        while pending.len() > capacity {
            pending.pop_front();
            HERO_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn update_local_hero(hl: &HashLink<'_>, pending: &mut VecDeque<PendingHero>, tick: u64) {
    if crate::player_hooks::status() == 1 && crate::player_hooks::root_available() {
        pending.clear();
        if let Some(binding) = crate::player_hooks::current_binding() {
            if let Some(hero) = binding.hero {
                lock_local_hero(hero, binding.player);
                return;
            }
        }
        clear_local_hero();
        return;
    }

    let locked = LOCKED_HERO_POINTER.load(Ordering::Acquire);
    let mut found = None;
    for _ in 0..MAX_HERO_VALIDATE_PER_TICK.min(pending.len()) {
        let Some(candidate) = pending.pop_front() else {
            break;
        };
        if let Some(owner) = local_hero_owner(hl, candidate.hero) {
            // Farever creates a burst of proxy/template Heroes during zone
            // changes. Prefer the newest valid candidate in this batch.
            found = Some((candidate.hero, owner));
        } else if candidate.first_seen.elapsed() < HERO_CANDIDATE_LIFETIME {
            pending.push_back(candidate);
        }
    }

    if let Some((hero, owner)) = found {
        lock_local_hero(hero, owner);
        while pending.len() > MAX_LOCKED_HERO_PENDING {
            pending.pop_front();
            HERO_DROPS.fetch_add(1, Ordering::Relaxed);
        }
        return;
    }

    // Revalidate the lock every fourth tick: follow the stable Player.hero
    // back-reference first, because it survives zone transitions, and only then
    // consider the allocation path again.
    if locked == 0 || !tick.is_multiple_of(4) {
        return;
    }
    if local_hero_owner(hl, locked).is_some() {
        return;
    }
    let player = LOCKED_PLAYER_POINTER.load(Ordering::Acquire);
    let relocked = (player != 0)
        .then(|| hl.pointer_field(player, "hero"))
        .flatten()
        .and_then(|hero| local_hero_owner(hl, hero).map(|owner| (hero, owner)));
    if let Some((hero, owner)) = relocked {
        lock_local_hero(hero, owner);
    } else {
        clear_local_hero();
    }
}

fn clear_local_hero() {
    LOCKED_HERO_POINTER.store(0, Ordering::Release);
    LOCKED_PLAYER_POINTER.store(0, Ordering::Release);
    HERO_POINTER.store(0, Ordering::Release);
}

fn lock_local_hero(hero: usize, owner: usize) {
    let previous = LOCKED_HERO_POINTER.swap(hero, Ordering::AcqRel);
    LOCKED_PLAYER_POINTER.store(owner, Ordering::Release);
    HERO_POINTER.store(hero, Ordering::Release);
    if previous != hero {
        HERO_LOCKS.fetch_add(1, Ordering::Relaxed);
    }
}

fn local_hero_owner(hl: &HashLink<'_>, hero: usize) -> Option<usize> {
    let hero_type = HERO_TYPE.load(Ordering::Acquire);
    if hero_type == 0 || hl.memory.u64(hero)? != hero_type {
        return None;
    }
    let owner = hl.pointer_field(hero, "ownerPlayer")?;
    if hl.pointer_field(owner, "hero")? != hero || !read_bool_field(hl, owner, "isMe")? {
        return None;
    }
    position_is_plausible(hl, hero).then_some(owner)
}

fn position_is_plausible(hl: &HashLink<'_>, hero: usize) -> bool {
    let Some(x) = read_f64_field(hl, hero, "posx") else {
        return false;
    };
    let Some(y) = read_f64_field(hl, hero, "posy") else {
        return false;
    };
    let Some(z) = read_f64_field(hl, hero, "posz") else {
        return false;
    };
    plausible_position(x, y, z)
}

fn plausible_position(x: f64, y: f64, z: f64) -> bool {
    x.is_finite()
        && y.is_finite()
        && z.is_finite()
        && (-10_000.0..=10_000.0).contains(&x)
        && (-10_000.0..=10_000.0).contains(&y)
        && (-500.0..=1_500.0).contains(&z)
        && (x.abs() >= 0.01 || y.abs() >= 0.01)
}

fn learn_observed_types(hl: &HashLink<'_>) {
    let Some(queue) = TYPE_CANDIDATES.get() else {
        return;
    };
    for _ in 0..64 {
        let Some(candidate) = queue.pop() else {
            break;
        };
        let Some(type_name) = hl.type_name(candidate.type_pointer) else {
            // A concurrent bootstrap can expose the allocation before every
            // metadata page is readable. Let the next object of this type
            // enqueue it again rather than negatively caching a transient.
            OBSERVED_TYPES.forget(candidate.type_pointer);
            continue;
        };
        match type_name.as_str() {
            "ent.Hero" => {
                HERO_TYPE.store(candidate.type_pointer, Ordering::Release);
                queue_hero(
                    OBSERVED_TYPES
                        .take_latest(candidate.type_pointer)
                        .unwrap_or(candidate.object),
                );
            }
            "ui.comp.DamageDisplay" => {
                DAMAGE_DISPLAY_TYPE.store(candidate.type_pointer, Ordering::Release);
                CAPTURE_STATUS.store(2, Ordering::Release);
                queue_display(
                    OBSERVED_TYPES
                        .take_latest(candidate.type_pointer)
                        .unwrap_or(candidate.object),
                );
            }
            "ui.hud.ChatBox" => {
                crate::slash_commands::observe_chat_box_type(candidate.type_pointer);
                crate::chat_output::observe_chat_box_type(candidate.type_pointer);
            }
            "ui.GameUI" | "ui.BaseUI" => {
                crate::ui_windows::observe_ui_type(candidate.type_pointer);
            }
            "ui.win.MapWindow" => {
                crate::map_clicks::observe_map_type(candidate.type_pointer);
            }
            "st.GameLayer" => {
                crate::activity_hooks::observe_game_layer_type(candidate.type_pointer);
            }
            "GameApp" => {
                GAME_APP_TYPE.store(candidate.type_pointer, Ordering::Release);
                crate::player_hooks::observe_game_app_type(candidate.type_pointer);
            }
            _ => {}
        }
    }
}

fn resolve_direct_damage_layout(
    hl: &HashLink<'_>,
    hero_type: usize,
    result_type: usize,
    game_build: GameBuildProfile,
) -> Result<DirectDamageLayout, String> {
    let hero = validate_object(hl, hero_type, &HERO_DAMAGE_SOURCE_SCHEMA)?;
    let result_schema = if game_build.is_beta() {
        &DAMAGE_RESULT_SCHEMA_BETA
    } else {
        &DAMAGE_RESULT_SCHEMA_STABLE
    };
    let result = validate_object(hl, result_type, result_schema)?;
    let skill_field = game_build.base_skill_access_field();
    let result_offset = |name| {
        result
            .offset(name)
            .ok_or_else(|| format!("validated DamageResult layout omitted field {name}"))
    };
    let base_skill_type = result
        .field_type_address(skill_field)
        .ok_or_else(|| format!("validated DamageResult layout omitted {skill_field} type"))?;
    let base_skill = validate_object(hl, base_skill_type, &BASE_SKILL_SCHEMA)?;
    let string_type = base_skill
        .field_type_address("kind")
        .ok_or_else(|| "validated BaseSkill layout omitted kind type".to_owned())?;
    let string = validate_object(hl, string_type, &STRING_SCHEMA)?;

    Ok(DirectDamageLayout {
        result_type: result.type_address,
        source_hero_type: hero.type_address,
        source_owner_player: hero
            .offset("ownerPlayer")
            .ok_or_else(|| "validated Hero layout omitted ownerPlayer".to_owned())?,
        base_skill: result_offset(skill_field)?,
        target: result_offset("target")?,
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
        amount: result_offset("_amount")?,
        blocked: result_offset("_block")?,
        hit_ordinal: result_offset("_hitCount")?,
        critical: result_offset("_critical")?,
        killed: result_offset("_kill")?,
    })
}

fn replay_latest_known_allocations() {
    let hero_type = HERO_TYPE.load(Ordering::Acquire);
    if hero_type != 0 {
        if let Some(hero) = OBSERVED_TYPES.take_latest(hero_type) {
            queue_hero(hero);
        }
    }
    let damage_type = DAMAGE_DISPLAY_TYPE.load(Ordering::Acquire);
    if damage_type != 0 {
        if let Some(display) = OBSERVED_TYPES.take_latest(damage_type) {
            queue_display(display);
        }
    }
}

fn decode_display(
    hl: &HashLink<'_>,
    display: usize,
    game_build: GameBuildProfile,
) -> Option<Decoded> {
    if hl.class_of(display).as_deref() != Some("ui.comp.DamageDisplay") {
        return None;
    }
    let result = hl.pointer_field(display, "dmg")?;
    if hl.class_of(result).as_deref() != Some("st.skill.DamageResult") {
        return None;
    }
    let amount = read_f64_field(hl, result, "_amount")?;
    if !amount.is_finite() || !(0.0..100_000_000.0).contains(&amount) || amount == 0.0 {
        return None;
    }
    let hit_ordinal = read_i32_field(hl, result, "_hitCount")?;
    if !(1..=10_000).contains(&hit_ordinal) {
        return None;
    }
    let critical = read_bool_field(hl, result, "_critical")?;
    let killed = read_bool_field(hl, result, "_kill")?;
    let base_skill = hl.pointer_field(result, game_build.base_skill_access_field())?;
    let skill_string = hl.pointer_field(base_skill, "kind")?;
    let skill_id = hl.string(skill_string)?;
    if skill_id.is_empty() || skill_id.len() > 128 {
        return None;
    }
    let target_pointer = hl.pointer_field(result, "target").unwrap_or(0);
    let outgoing = target_pointer == 0 || target_pointer != HERO_POINTER.load(Ordering::Acquire);
    let target_id = (target_pointer != 0)
        .then(|| {
            hl.pointer_field(target_pointer, "kind")
                .and_then(|value| hl.string(value))
                .or_else(|| hl.class_of(target_pointer))
        })
        .flatten();
    let blocked = read_f64_field(hl, result, "_block").filter(|value| value.is_finite());

    let result_again = hl.pointer_field(display, "dmg")?;
    if result_again != result || hl.class_of(result).as_deref() != Some("st.skill.DamageResult") {
        return None;
    }
    Some(Decoded {
        result_pointer: result,
        target_pointer,
        hit_ordinal,
        outgoing,
        raw: RawDamage {
            provider: DamageProvider::Polling,
            source_pointer: if outgoing {
                HERO_POINTER.load(Ordering::Acquire)
            } else {
                0
            },
            source_owner: if outgoing {
                LOCKED_PLAYER_POINTER.load(Ordering::Acquire)
            } else {
                0
            },
            target_pointer,
            skill_id,
            amount,
            critical,
            killed,
            target_id,
            blocked,
        },
    })
}

fn read_f64_field(hl: &HashLink<'_>, object: usize, name: &str) -> Option<f64> {
    let offset = hl.field_offset(object, name)?;
    hl.memory.f64(object + offset)
}

fn read_i32_field(hl: &HashLink<'_>, object: usize, name: &str) -> Option<i32> {
    let offset = hl.field_offset(object, name)?;
    hl.memory.i32(object + offset)
}

fn read_bool_field(hl: &HashLink<'_>, object: usize, name: &str) -> Option<bool> {
    let offset = hl.field_offset(object, name)?;
    match hl.memory.u8(object + offset)? {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn status_name(status: usize) -> &'static str {
    match status {
        1 => "learning-type",
        2 => "active",
        3 => "tripped",
        4 => "waiting-libhl",
        _ => "unavailable",
    }
}

fn direct_damage_hook_status_name(status: usize) -> &'static str {
    match status {
        1 => "active",
        3 => "failed",
        4 => "installing",
        _ => "waiting-for-hero-type",
    }
}

fn direct_damage_provider_selected() -> bool {
    damage_provider_is_direct(
        DIRECT_DAMAGE_HOOK_STATUS.load(Ordering::Relaxed),
        USE_POLLING.load(Ordering::Relaxed),
    )
}

fn damage_provider_is_direct(direct_hook_status: usize, use_polling: bool) -> bool {
    !use_polling && direct_hook_status == 1
}

fn damage_provider_name(direct_hook_status: usize, use_polling: bool) -> &'static str {
    if use_polling {
        "polling-selected"
    } else if damage_provider_is_direct(direct_hook_status, use_polling) {
        "direct-hook"
    } else {
        "polling-auto"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hero_position_rejects_uninitialized_and_invalid_values() {
        assert!(plausible_position(12.0, -7.5, 3.0));
        assert!(!plausible_position(0.0, 0.0, 3.0));
        assert!(!plausible_position(f64::NAN, 2.0, 3.0));
        assert!(!plausible_position(2.0, 3.0, 1_501.0));
    }

    #[test]
    fn host_classifies_captured_damage_for_solo_delivery() {
        let mut damage = decode_direct_damage_observation(&direct_observation("Mage_RayOfSpark"))
            .expect("valid direct observation");
        let local_player = 0x1000_0000;
        let local_hero = 0x2000_0000;

        damage.source_owner = local_player;
        assert_eq!(
            solo_damage_relation(&damage, local_player, local_hero),
            SoloDamageRelation::Dealt
        );

        damage.source_owner = 0x3000_0000;
        damage.source_pointer = 0x4000_0000;
        damage.target_pointer = local_hero;
        assert_eq!(
            solo_damage_relation(&damage, local_player, local_hero),
            SoloDamageRelation::Received
        );

        damage.target_pointer = 0x5000_0000;
        assert_eq!(
            solo_damage_relation(&damage, local_player, local_hero),
            SoloDamageRelation::Unrelated
        );
    }

    #[test]
    fn direct_damage_is_attributed_only_to_validated_multi_member_parties() {
        let local_player = 0x1000_0000;
        let local_hero = 0x2000_0000;
        let remote_player = 0x3000_0000;
        let remote_hero = 0x4000_0000;
        let party = vec![
            PartyDamageSource {
                actor_id: "actor-1".to_owned(),
                player_pointer: local_player,
                hero_pointer: Some(local_hero),
                is_local: true,
            },
            PartyDamageSource {
                actor_id: "actor-2".to_owned(),
                player_pointer: remote_player,
                hero_pointer: Some(remote_hero),
                is_local: false,
            },
        ];
        let mut damage = decode_direct_damage_observation(&direct_observation("Mage_RayOfSpark"))
            .expect("valid direct observation");
        damage.source_owner = remote_player;
        damage.source_pointer = remote_hero;
        damage.target_pointer = 0x5000_0000;

        assert_eq!(
            damage_delivery(&damage, local_player, local_hero, &party),
            DamageDelivery::Dealt {
                actor_id: Some("actor-2".to_owned()),
                relation: ActorRelation::GroupMember,
            }
        );
        assert_eq!(
            damage_delivery(&damage, local_player, local_hero, &party[..1]),
            DamageDelivery::Unrelated,
            "a solo roster must not widen the capture audience"
        );

        damage.source_owner = local_player;
        damage.source_pointer = local_hero;
        assert_eq!(
            damage_delivery(&damage, local_player, local_hero, &party),
            DamageDelivery::Dealt {
                actor_id: Some("actor-1".to_owned()),
                relation: ActorRelation::LocalPlayer,
            }
        );

        damage.source_pointer = remote_hero;
        assert_eq!(
            damage_delivery(&damage, local_player, local_hero, &party),
            DamageDelivery::Unrelated,
            "conflicting owner and Hero identities fail closed"
        );

        damage.source_owner = 0x6000_0000;
        damage.source_pointer = 0x7000_0000;
        assert_eq!(
            damage_delivery(&damage, local_player, local_hero, &party),
            DamageDelivery::Unrelated,
            "nearby non-party Heroes remain outside the audience"
        );
    }

    #[test]
    fn polling_damage_cannot_claim_remote_party_attribution() {
        let local_player = 0x1000_0000;
        let local_hero = 0x2000_0000;
        let remote_player = 0x3000_0000;
        let remote_hero = 0x4000_0000;
        let party = vec![
            PartyDamageSource {
                actor_id: "actor-1".to_owned(),
                player_pointer: local_player,
                hero_pointer: Some(local_hero),
                is_local: true,
            },
            PartyDamageSource {
                actor_id: "actor-2".to_owned(),
                player_pointer: remote_player,
                hero_pointer: Some(remote_hero),
                is_local: false,
            },
        ];
        let mut damage = decode_direct_damage_observation(&direct_observation("Mage_RayOfSpark"))
            .expect("valid direct observation");
        damage.provider = DamageProvider::Polling;
        damage.source_owner = remote_player;
        damage.source_pointer = remote_hero;
        damage.target_pointer = 0x5000_0000;

        assert_eq!(
            damage_delivery(&damage, local_player, local_hero, &party),
            DamageDelivery::Unrelated
        );
    }

    #[test]
    fn party_damage_diagnostics_report_transitions_and_each_actor_once() {
        let mut capture = DamageCapture::new(Path::new("."));
        let mut party = vec![
            PartyDamageSource {
                actor_id: "actor-1".to_owned(),
                player_pointer: 0x1000_0000,
                hero_pointer: Some(0x2000_0000),
                is_local: true,
            },
            PartyDamageSource {
                actor_id: "actor-2".to_owned(),
                player_pointer: 0x3000_0000,
                hero_pointer: Some(0x4000_0000),
                is_local: false,
            },
        ];

        capture.update_party_audience_diagnostics(&party);
        capture.note_party_damage_source("actor-2", ActorRelation::GroupMember);
        capture.note_party_damage_source("actor-2", ActorRelation::GroupMember);
        let first = capture.take_diagnostics();
        assert_eq!(first.len(), 2);
        assert!(first[0].contains("actor-1:local:loaded"));
        assert!(first[0].contains("actor-2:group:loaded"));
        assert_eq!(
            first[1],
            "party damage first observed actor_id=actor-2 relation=GroupMember"
        );

        capture.update_party_audience_diagnostics(&party);
        capture.note_party_damage_source("actor-2", ActorRelation::GroupMember);
        assert!(capture.take_diagnostics().is_empty());

        party[1].hero_pointer = None;
        capture.update_party_audience_diagnostics(&party);
        capture.note_party_damage_source("actor-2", ActorRelation::GroupMember);
        let changed = capture.take_diagnostics();
        assert_eq!(changed.len(), 2);
        assert!(changed[0].contains("actor-2:group:unloaded"));
        assert_eq!(
            changed[1],
            "party damage first observed actor_id=actor-2 relation=GroupMember"
        );
    }

    #[test]
    fn group_targets_share_the_same_validated_actor_identity() {
        let local_hero = 0x2000_0000;
        let remote_hero = 0x4000_0000;
        let party = vec![
            PartyDamageSource {
                actor_id: "actor-1".to_owned(),
                player_pointer: 0x1000_0000,
                hero_pointer: Some(local_hero),
                is_local: true,
            },
            PartyDamageSource {
                actor_id: "actor-2".to_owned(),
                player_pointer: 0x3000_0000,
                hero_pointer: Some(remote_hero),
                is_local: false,
            },
        ];
        let mut damage = decode_direct_damage_observation(&direct_observation("Mage_RayOfSpark"))
            .expect("valid direct observation");
        damage.target_pointer = remote_hero;

        assert_eq!(
            damage_target(&damage, local_hero, &party),
            (Some("actor-2".to_owned()), ActorRelation::GroupMember)
        );
    }

    #[test]
    fn direct_damage_observation_decodes_the_copied_skill_id() {
        let mut observation = direct_observation("Mage_RayOfSpark");
        let decoded = decode_direct_damage_observation(&observation).expect("valid damage");
        assert_eq!(decoded.skill_id, "Mage_RayOfSpark");
        assert_eq!(decoded.amount, 10.0);
        assert_eq!(decoded.blocked, Some(0.0));
        assert_eq!(decoded.source_pointer, 0x1000_0000);
        assert_eq!(decoded.source_owner, 0x2000_0000);
        assert_eq!(decoded.target_pointer, 0x3000_0000);

        observation.amount = 0.0;
        assert!(decode_direct_damage_observation(&observation).is_none());
        observation.amount = 10.0;
        observation.critical = 2;
        assert!(decode_direct_damage_observation(&observation).is_none());
        observation.critical = 0;
        observation.hit_ordinal = 0;
        assert!(decode_direct_damage_observation(&observation).is_none());
    }

    #[test]
    fn exactly_one_damage_provider_is_authoritative() {
        assert_eq!(damage_provider_name(0, false), "polling-auto");
        assert_eq!(damage_provider_name(4, false), "polling-auto");
        assert_eq!(damage_provider_name(1, false), "direct-hook");
        assert_eq!(damage_provider_name(3, false), "polling-auto");
        assert_eq!(damage_provider_name(1, true), "polling-selected");
        assert!(damage_provider_is_direct(1, false));
        assert!(!damage_provider_is_direct(1, true));
    }

    fn direct_observation(skill_id: &str) -> RawDirectDamage {
        let mut copied = [0_u16; MAX_SKILL_ID_CODE_UNITS];
        let encoded = skill_id.encode_utf16().collect::<Vec<_>>();
        copied[..encoded.len()].copy_from_slice(&encoded);
        RawDirectDamage {
            source_pointer: 0x1000_0000,
            source_owner: 0x2000_0000,
            target_pointer: 0x3000_0000,
            amount: 10.0,
            blocked: 0.0,
            hit_ordinal: 1,
            critical: 0,
            killed: 0,
            skill_id_length: encoded.len() as u16,
            skill_id: copied,
        }
    }
}
