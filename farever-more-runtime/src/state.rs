use crate::cpu::ThreadCpuMeter;
use crate::memory::{find_process, ProcessMemory, Region};
use crate::readiness::ReadinessGate;
use farever_more_api::{
    FasSnapshotV0, InstanceKind, PartyMember, PartyState, ADAPTER_LIVE, ADAPTER_SEARCHING,
    ADAPTER_WAITING_FOR_GAME, ADAPTER_WAITING_TO_SCAN, COMBAT_REFERENCE_AUTO_TARGET,
    COMBAT_REFERENCE_LOCKED_TARGET, COMBAT_REFERENCE_TARGET, WINDOW_CAPACITY,
};
use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

const HBYTES: i32 = 8;
const HI32: i32 = 3;
const HOBJ: i32 = 11;
const HVIRTUAL: i32 = 15;
const HENUM: i32 = 18;
const HSTRUCT: i32 = 21;
// These are 64-bit HashLink runtime-metadata offsets (`hl_type_obj` and field
// descriptors), not Farever gameplay-object offsets. Gameplay fields are
// resolved by name through this metadata so they can move between builds.
const TYPE_OBJECT: usize = 0x08;
const TYPE_OBJECT_NAME: usize = 0x10;
const TYPE_OBJECT_SUPER: usize = 0x18;
const TYPE_OBJECT_FIELDS: usize = 0x20;
const TYPE_OBJECT_GLOBAL_VALUE: usize = 0x38;
const TYPE_OBJECT_RUNTIME: usize = 0x48;
const FIELD_STRIDE: usize = 0x18;
const FIELD_TYPE: usize = 0x08;
const RUNTIME_OBJECT_FIELD_COUNT: usize = 0x08;
const RUNTIME_OBJECT_SIZE: usize = 0x10;
const RUNTIME_OBJECT_FIELD_OFFSETS: usize = 0x28;
const TYPE_VIRTUAL_FIELD_COUNT: usize = 0x08;
const TYPE_ENUM_CONSTRUCT_COUNT: usize = 0x08;
const TYPE_ENUM_CONSTRUCTS: usize = 0x10;
const ENUM_CONSTRUCT_STRIDE: usize = 0x28;
const ENUM_CONSTRUCT_PARAM_COUNT: usize = 0x08;
const ENUM_CONSTRUCT_PARAM_TYPES: usize = 0x10;
const ENUM_CONSTRUCT_SIZE: usize = 0x18;
const ENUM_CONSTRUCT_PARAM_OFFSETS: usize = 0x20;
const LOCATE_RETRY_DELAY: Duration = Duration::from_secs(5);
const CPU_UNAVAILABLE: u64 = u64::MAX;
const PARTY_MEMBER_LIMIT: usize = 16;
const HERO_SKILL_LIMIT: usize = 128;
const ACTIVITY_CONTEXT_LIMIT: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InstanceObservation {
    /// Internal provider key. It is never exposed to an add-on.
    pub(crate) key: String,
    pub(crate) kind: InstanceKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PartyDamageSource {
    pub(crate) actor_id: String,
    pub(crate) player_pointer: usize,
    pub(crate) hero_pointer: Option<usize>,
    pub(crate) is_local: bool,
}

#[derive(Clone, Debug)]
struct RawPartyMember {
    stable_key: String,
    player_pointer: usize,
    hero_pointer: Option<usize>,
    is_local: bool,
    name: Option<String>,
    class_id: Option<String>,
    in_combat: Option<bool>,
}

#[derive(Clone, Debug)]
struct RawPartyState {
    stable_key: Option<u64>,
    members: Vec<RawPartyMember>,
    source: PartyRosterSource,
    source_pointer: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PartyRosterSource {
    RiftLayer,
    Group,
}

#[derive(Default)]
struct WindowRootTracker {
    observed: bool,
    identity: Option<(usize, usize)>,
}

#[derive(Default)]
struct PlayerRootTracker {
    observed: bool,
    identity: Option<usize>,
}

#[derive(Default)]
struct HeroBindingTracker {
    observed: bool,
    identity: Option<usize>,
}

impl HeroBindingTracker {
    /// Every post-seed change invalidates state derived from the previous Hero,
    /// including an observed clear before a replacement is constructed.
    fn observe(&mut self, identity: Option<usize>) -> bool {
        let changed = self.observed && self.identity != identity;
        self.observed = true;
        self.identity = identity;
        changed
    }
}

impl PlayerRootTracker {
    /// The first root is seeded by direct-provider activation. Only a later
    /// replacement or reacquisition independently requests reconciliation.
    fn observe(&mut self, identity: Option<usize>) -> bool {
        if !self.observed && identity.is_none() {
            return false;
        }
        let reacquired = self.observed && self.identity != identity && identity.is_some();
        self.observed = true;
        self.identity = identity;
        reacquired
    }
}

impl WindowRootTracker {
    /// Returns true only when a previously observed root is replaced or
    /// reacquired. The first root establishes identity without independently
    /// requesting a sample; hook-provider activation owns the initial seed.
    fn observe(&mut self, identity: Option<(usize, usize)>) -> bool {
        let reacquired = self.observed && self.identity != identity && identity.is_some();
        self.observed = true;
        self.identity = identity;
        reacquired
    }
}

impl PartyRosterSource {
    fn diagnostic(self) -> &'static str {
        match self {
            Self::RiftLayer => "live source=rift-layer",
            Self::Group => "live source=group",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HashLinkFieldShape {
    pub(crate) name: String,
    pub(crate) offset: usize,
    pub(crate) kind: i32,
    pub(crate) type_address: usize,
    pub(crate) object_type_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HashLinkObjectShape {
    pub(crate) name: String,
    pub(crate) kind: i32,
    pub(crate) size: usize,
    pub(crate) fields: Vec<HashLinkFieldShape>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HashLinkVirtualFieldShape {
    pub(crate) name: String,
    pub(crate) type_address: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HashLinkVirtualShape {
    pub(crate) fields: Vec<HashLinkVirtualFieldShape>,
}

#[derive(Clone, Debug)]
/// Selects how the runtime obtains its read-only process handle.
pub enum TargetProcess {
    /// Observe the current process through the Windows pseudo-handle.
    Current,
    /// Observe one exact process ID.
    ProcessId(u32),
    /// Find the first process whose executable name matches case-insensitively.
    Named(String),
}

pub struct Poller {
    target: TargetProcess,
    memory: Option<ProcessMemory>,
    anchor: Option<GameAppAnchor>,
    locate_task: Option<LocateTask>,
    last_locate_completed: Option<Instant>,
    locate_attempt: u64,
    readiness: ReadinessGate,
    diagnostics: Vec<String>,
    party: Option<PartyState>,
    party_damage_sources: Vec<PartyDamageSource>,
    instance: Option<InstanceObservation>,
    instance_sampled: bool,
    lifecycle_sampled: bool,
    combat_provider_sampled: bool,
    local_binding_sampled: bool,
    player_root: PlayerRootTracker,
    hero_binding: HeroBindingTracker,
    combat_reconciliation_required: bool,
    current_player: Option<usize>,
    current_hero: Option<usize>,
    combat_state: Option<bool>,
    windows_sampled: bool,
    window_root: WindowRootTracker,
    current_layer: Option<usize>,
    activity_hook_decoder: crate::activity_hooks::ActivityHookDecoder,
    activity_hook_edges: Vec<crate::activity_hooks::ActivityHookEdge>,
    party_probe_state: Option<&'static str>,
    instance_probe_state: Option<&'static str>,
    actor_ids: HashMap<String, String>,
    party_ids: HashMap<u64, String>,
    hero_class_ids: HashMap<usize, String>,
    next_actor_id: u64,
    next_party_id: u64,
}

impl Poller {
    pub fn new(target: TargetProcess) -> Self {
        Self {
            target,
            memory: None,
            anchor: None,
            locate_task: None,
            last_locate_completed: None,
            locate_attempt: 0,
            readiness: ReadinessGate::new(),
            diagnostics: Vec::new(),
            party: None,
            party_damage_sources: Vec::new(),
            instance: None,
            instance_sampled: false,
            lifecycle_sampled: false,
            combat_provider_sampled: false,
            local_binding_sampled: false,
            player_root: PlayerRootTracker::default(),
            hero_binding: HeroBindingTracker::default(),
            combat_reconciliation_required: false,
            current_player: None,
            current_hero: None,
            combat_state: None,
            windows_sampled: false,
            window_root: WindowRootTracker::default(),
            current_layer: None,
            activity_hook_decoder: crate::activity_hooks::ActivityHookDecoder::default(),
            activity_hook_edges: Vec::new(),
            party_probe_state: None,
            instance_probe_state: None,
            actor_ids: HashMap::new(),
            party_ids: HashMap::new(),
            hero_class_ids: HashMap::new(),
            next_actor_id: 1,
            next_party_id: 1,
        }
    }

    pub fn process_id(&self) -> Option<u32> {
        self.memory.as_ref().map(ProcessMemory::pid)
    }

    pub fn current_hero(&self) -> Option<usize> {
        self.current_hero
    }

    pub fn current_combat_state(&self) -> Option<bool> {
        self.combat_state
    }

    pub fn take_diagnostics(&mut self) -> Vec<String> {
        std::mem::take(&mut self.diagnostics)
    }

    pub(crate) fn party(&self) -> Option<&PartyState> {
        self.party.as_ref()
    }

    pub(crate) fn party_damage_sources(&self) -> &[PartyDamageSource] {
        &self.party_damage_sources
    }

    pub(crate) fn instance(&self) -> Option<&InstanceObservation> {
        self.instance.as_ref()
    }

    pub(crate) fn instance_sampled(&self) -> bool {
        self.instance_sampled
    }

    pub(crate) fn local_binding_sampled(&self) -> bool {
        self.local_binding_sampled
    }

    pub(crate) fn lifecycle_sampled(&self) -> bool {
        self.lifecycle_sampled
    }

    pub(crate) fn combat_provider_sampled(&self) -> bool {
        self.combat_provider_sampled
    }

    pub(crate) fn windows_sampled(&self) -> bool {
        self.windows_sampled
    }

    pub(crate) fn take_activity_hook_edges(
        &mut self,
    ) -> Vec<crate::activity_hooks::ActivityHookEdge> {
        std::mem::take(&mut self.activity_hook_edges)
    }

    pub fn poll(
        &mut self,
        world_probe: Option<bool>,
        sample_lifecycle: bool,
        sample_local_binding: bool,
        sample_combat_provider: bool,
        sample_instance: bool,
        sample_windows: bool,
    ) -> FasSnapshotV0 {
        self.clear_provider_observations();
        let mut snapshot = FasSnapshotV0::default();
        if !self.ensure_process() {
            self.player_root.observe(None);
            crate::player_hooks::observe_current_app(None);
            snapshot.adapter_status = ADAPTER_WAITING_FOR_GAME;
            return snapshot;
        }

        snapshot.process_found = 1;
        snapshot.process_id = self.memory.as_ref().expect("process was ensured").pid();

        self.collect_locate_result();

        let pid = self.memory.as_ref().expect("process was ensured").pid();

        if self.anchor.is_none() && self.locate_task.is_none() && world_probe == Some(true) {
            if let Some(game_app_type) = crate::damage::observed_game_app_type() {
                let memory = self.memory.as_ref().expect("process was ensured");
                let hl = HashLink::new(memory);
                if let Some(anchor) = GameAppAnchor::from_type_address(&hl, game_app_type) {
                    self.diagnostics.push(format!(
                        "locator source=allocator_type accepted type=0x{game_app_type:X} global_slot=0x{:X} instance_offset=0x{:X}",
                        anchor.global_slot, anchor.instance_offset,
                    ));
                    self.anchor = Some(anchor);
                }
            }
        }

        if self.anchor.is_none() && self.locate_task.is_none() {
            let ready = self.readiness.check(
                self.memory.as_ref().expect("process was ensured"),
                world_probe,
                &mut self.diagnostics,
            );
            let retry_elapsed = self
                .last_locate_completed
                .is_none_or(|last| last.elapsed() >= LOCATE_RETRY_DELAY);
            if ready && retry_elapsed {
                self.start_locate(pid);
            } else {
                self.player_root.observe(None);
                crate::player_hooks::observe_current_app(None);
                snapshot.adapter_status = ADAPTER_WAITING_TO_SCAN;
                return snapshot;
            }
        }

        let Some(anchor) = &self.anchor else {
            self.player_root.observe(None);
            crate::player_hooks::observe_current_app(None);
            snapshot.adapter_status = ADAPTER_SEARCHING;
            snapshot.scan_progress = self.locate_task.as_ref().map_or(0, LocateTask::progress);
            return snapshot;
        };
        let memory = self.memory.as_ref().expect("process was ensured");
        let hl = HashLink::new(memory);
        let app = anchor.current(&hl);
        let Some(app) = app else {
            // HashLink objects may move during collection. The runtime global
            // slot is stable, but discard and rediscover the complete anchor if
            // either it or the current GameApp instance no longer validates.
            self.anchor = None;
            self.player_root.observe(None);
            crate::player_hooks::observe_current_app(None);
            self.window_root.observe(None);
            self.readiness.reset();
            self.last_locate_completed = Some(Instant::now());
            self.diagnostics
                .push("cached anchor failed validation; scheduling rediscovery".to_owned());
            snapshot.adapter_status = ADAPTER_SEARCHING;
            return snapshot;
        };
        snapshot.adapter_status = ADAPTER_LIVE;
        snapshot.app_found = 1;

        let player_root_changed = self.player_root.observe(Some(app));
        if player_root_changed {
            self.diagnostics.push(
                "GameApp root changed; scheduling one local Player reconciliation".to_owned(),
            );
        }
        if snapshot.process_id == std::process::id() {
            crate::player_hooks::observe_current_app(Some(app));
            if let Some(type_pointer) = memory.u64(app) {
                crate::player_hooks::observe_game_app_type(type_pointer);
                crate::lifecycle_hooks::observe_game_app_type(type_pointer);
            }
        }

        let direct_lifecycle_available = snapshot.process_id == std::process::id()
            && crate::lifecycle_hooks::provider_available();
        let sample_lifecycle =
            sample_lifecycle || (direct_lifecycle_available && player_root_changed);
        if sample_lifecycle {
            let lifecycle_sequence =
                direct_lifecycle_available.then(crate::lifecycle_hooks::capture_sequence);
            if let Some(lifecycle) = read_lifecycle_state(&hl, app) {
                self.lifecycle_sampled = true;
                snapshot.loading_state = lifecycle.loading_state;
                snapshot.in_world = u8::from(lifecycle.in_world);
                if let Some(area) = &lifecycle.area {
                    snapshot.set_area(area);
                }
                if let Some(lifecycle_sequence) = lifecycle_sequence {
                    crate::lifecycle_hooks::reconcile(lifecycle, lifecycle_sequence);
                }
            } else if direct_lifecycle_available {
                if let Some(lifecycle) =
                    crate::lifecycle_hooks::current().filter(|current| current.app == app)
                {
                    snapshot.loading_state = lifecycle.loading_state;
                    snapshot.in_world = u8::from(lifecycle.in_world);
                    if let Some(area) = lifecycle.area {
                        snapshot.set_area(&area);
                    }
                }
            }
        } else if direct_lifecycle_available {
            if let Some(lifecycle) =
                crate::lifecycle_hooks::current().filter(|current| current.app == app)
            {
                snapshot.loading_state = lifecycle.loading_state;
                snapshot.in_world = u8::from(lifecycle.in_world);
                if let Some(area) = lifecycle.area {
                    snapshot.set_area(&area);
                }
            }
        }

        let direct_player_available =
            snapshot.process_id == std::process::id() && crate::player_hooks::status() == 1;
        let sample_local_binding =
            sample_local_binding || (direct_player_available && player_root_changed);
        let local_binding = if sample_local_binding {
            self.local_binding_sampled = true;
            let binding_sequence =
                direct_player_available.then(crate::player_hooks::capture_sequence);
            let binding = local_binding_from_app(&hl, app);
            if let Some(binding_sequence) = binding_sequence {
                crate::player_hooks::reconcile_binding(app, binding, binding_sequence);
            }
            binding
        } else if direct_player_available {
            crate::player_hooks::current_binding()
                .filter(|binding| binding.app == app)
                .map(|binding| (binding.player, binding.hero))
        } else {
            None
        };
        let local_binding = (snapshot.in_world != 0).then_some(local_binding).flatten();
        self.current_player = local_binding.map(|(player, _)| player);
        self.current_hero = local_binding.and_then(|(_, hero)| hero);
        let hero_binding_changed = self.hero_binding.observe(self.current_hero);
        if hero_binding_changed {
            self.combat_reconciliation_required = true;
            self.diagnostics.push(match self.current_hero {
                Some(hero) => format!(
                    "local Hero binding changed; scheduling combat reconciliation hero=0x{hero:X}"
                ),
                None => "local Hero binding cleared; suspending the combat provider".to_owned(),
            });
        }

        let direct_combat_hooks_available =
            snapshot.process_id == std::process::id() && crate::combat_hooks::hooks_available();
        let sample_combat_provider = sample_combat_provider
            || (direct_combat_hooks_available && player_root_changed)
            || (direct_combat_hooks_available && self.combat_reconciliation_required);
        let (combat_references, combat_state) = if snapshot.in_world != 0 {
            self.current_hero.map_or((None, None), |hero| {
                if sample_combat_provider {
                    let combat_sequence =
                        direct_combat_hooks_available.then(crate::combat_hooks::capture_sequence);
                    let references = read_combat_membership(&hl, hero);
                    let combat = read_bool_field(&hl, hero, "isInCombat");
                    self.combat_provider_sampled = references.is_some() && combat.is_some();
                    if let (Some(combat_sequence), Some(references), Some(combat)) =
                        (combat_sequence, references, combat)
                    {
                        crate::combat_hooks::reconcile(
                            crate::combat_hooks::LocalCombatState {
                                hero,
                                references,
                                in_combat: Some(combat),
                            },
                            combat_sequence,
                        );
                        self.combat_reconciliation_required = false;
                    }
                    (references, combat)
                } else if crate::combat_hooks::provider_available() {
                    crate::combat_hooks::current()
                        .filter(|state| state.hero == hero)
                        .map_or((None, None), |state| {
                            (Some(state.references), state.in_combat)
                        })
                } else {
                    (None, None)
                }
            })
        } else {
            (None, None)
        };
        self.combat_state = combat_state;
        if snapshot.in_world == 0 {
            crate::party_hooks::observe_sources(None, None);
        }

        let previous_layer = self.current_layer;
        let (raw_party, instance, current_layer) = if snapshot.in_world != 0 {
            read_spatial_state(
                &hl,
                app,
                self.current_hero,
                combat_references,
                &mut snapshot,
            );
            let local_player = self.current_player;
            (
                local_player
                    .ok_or("unavailable stage=local-player")
                    .and_then(|player| read_party_state(&hl, player, &self.hero_class_ids)),
                sample_instance.then(|| {
                    local_player
                        .ok_or("unavailable stage=local-player")
                        .and_then(|player| read_instance_state(&hl, player))
                }),
                local_player.and_then(|player| player_layer(&hl, player)),
            )
        } else {
            (
                Err("unavailable stage=not-in-world"),
                sample_instance.then_some(Err("unavailable stage=not-in-world")),
                None,
            )
        };
        self.current_layer = current_layer;
        crate::activity_hooks::observe_local_layer(current_layer);
        let mut local_layers = [0_usize; 2];
        let mut local_layer_count = 0;
        for layer in [previous_layer, current_layer].into_iter().flatten() {
            if !local_layers[..local_layer_count].contains(&layer) {
                local_layers[local_layer_count] = layer;
                local_layer_count += 1;
            }
        }
        self.activity_hook_edges = self
            .activity_hook_decoder
            .decode_pending(&hl, &local_layers[..local_layer_count]);

        let base_ui = hl.pointer_field(app, "baseUI");
        let window_root_changed = self
            .window_root
            .observe(base_ui.map(|base_ui| (app, base_ui)));
        if window_root_changed {
            self.diagnostics
                .push("window root changed; scheduling one membership reconciliation".to_owned());
        }
        if let Some(base_ui) = base_ui {
            if snapshot.process_id == std::process::id() {
                if let Some(type_pointer) = hl.memory.u64(base_ui) {
                    crate::ui_windows::observe_ui_type(type_pointer);
                }
            }
            let sample_windows = sample_windows || window_root_changed;
            if sample_windows {
                self.windows_sampled = true;
            }
            if sample_windows {
                if let Some(windows) = hl.pointer_field(base_ui, "windows") {
                    let mut reconciliation = Vec::new();
                    for window in hl.object_array(windows, WINDOW_CAPACITY) {
                        if let Some(name) = hl.class_of(window) {
                            reconciliation.push((window, name.clone()));
                            snapshot.push_window(&name);
                        }
                    }
                    if snapshot.process_id == std::process::id() {
                        crate::ui_windows::reconcile_active_windows(reconciliation);
                    }
                }
            }
        }
        self.party = match raw_party {
            Ok(party) => {
                match party.source {
                    PartyRosterSource::Group => {
                        crate::party_hooks::observe_sources(Some(party.source_pointer), None)
                    }
                    PartyRosterSource::RiftLayer => {
                        crate::party_hooks::observe_sources(None, Some(party.source_pointer))
                    }
                }
                self.note_party_probe(party.source.diagnostic());
                Some(self.normalize_party(party))
            }
            Err(reason) => {
                self.note_party_probe(reason);
                self.party_damage_sources.clear();
                None
            }
        };
        self.instance_sampled = instance.is_some();
        self.instance = match instance {
            Some(Ok((instance, source))) => {
                self.note_instance_probe(source);
                Some(instance)
            }
            Some(Err(reason)) => {
                self.note_instance_probe(reason);
                None
            }
            None => None,
        };
        snapshot
    }

    fn normalize_party(&mut self, raw: RawPartyState) -> PartyState {
        let party_id = raw.stable_key.map(|key| self.opaque_party_id(key));
        let mut members = Vec::with_capacity(raw.members.len());
        let mut damage_sources = Vec::with_capacity(raw.members.len());
        let active_heroes = raw
            .members
            .iter()
            .filter_map(|member| member.hero_pointer)
            .collect::<HashSet<_>>();
        self.hero_class_ids
            .retain(|hero, _class_id| active_heroes.contains(hero));
        for member in raw.members {
            let class_changed = match (member.hero_pointer, member.class_id.as_deref()) {
                (Some(hero), Some(class_id)) => match self.hero_class_ids.get(&hero) {
                    Some(known) => known != class_id,
                    None => true,
                },
                _ => false,
            };
            let actor_id = self.opaque_actor_id(member.stable_key);
            if let (Some(hero), Some(class_id)) = (member.hero_pointer, member.class_id.as_ref()) {
                self.hero_class_ids.insert(hero, class_id.clone());
                if class_changed {
                    self.diagnostics.push(format!(
                        "party class resolved actor_id={actor_id} class_id={class_id}"
                    ));
                }
            }
            damage_sources.push(PartyDamageSource {
                actor_id: actor_id.clone(),
                player_pointer: member.player_pointer,
                hero_pointer: member.hero_pointer,
                is_local: member.is_local,
            });
            members.push(PartyMember {
                actor_id,
                is_local: member.is_local,
                name: member.name,
                class_id: member.class_id,
                class_icon: None,
                in_combat: member.in_combat,
            });
        }
        self.party_damage_sources = damage_sources;
        PartyState { party_id, members }
    }

    fn opaque_actor_id(&mut self, stable_key: String) -> String {
        if let Some(id) = self.actor_ids.get(&stable_key) {
            return id.clone();
        }
        let id = format!("actor-{}", self.next_actor_id);
        self.next_actor_id = self.next_actor_id.wrapping_add(1).max(1);
        self.actor_ids.insert(stable_key, id.clone());
        id
    }

    fn opaque_party_id(&mut self, stable_key: u64) -> String {
        if let Some(id) = self.party_ids.get(&stable_key) {
            return id.clone();
        }
        let id = format!("party-{}", self.next_party_id);
        self.next_party_id = self.next_party_id.wrapping_add(1).max(1);
        self.party_ids.insert(stable_key, id.clone());
        id
    }

    fn reset_provider_identities(&mut self) {
        self.clear_provider_observations();
        self.current_layer = None;
        crate::activity_hooks::clear_local_layer();
        self.activity_hook_decoder = crate::activity_hooks::ActivityHookDecoder::default();
        self.player_root = PlayerRootTracker::default();
        self.hero_binding = HeroBindingTracker::default();
        self.combat_reconciliation_required = false;
        self.current_player = None;
        self.current_hero = None;
        self.combat_state = None;
        crate::player_hooks::observe_current_app(None);
        crate::party_hooks::observe_sources(None, None);
        self.window_root = WindowRootTracker::default();
        self.party_probe_state = None;
        self.instance_probe_state = None;
        self.actor_ids.clear();
        self.party_ids.clear();
        self.hero_class_ids.clear();
        self.next_actor_id = 1;
        self.next_party_id = 1;
    }

    fn clear_provider_observations(&mut self) {
        self.party = None;
        self.party_damage_sources.clear();
        self.instance = None;
        self.instance_sampled = false;
        self.lifecycle_sampled = false;
        self.combat_provider_sampled = false;
        self.local_binding_sampled = false;
        self.current_player = None;
        self.current_hero = None;
        self.windows_sampled = false;
        self.activity_hook_edges.clear();
    }

    fn note_party_probe(&mut self, state: &'static str) {
        if self.party_probe_state == Some(state) {
            return;
        }
        self.party_probe_state = Some(state);
        self.diagnostics.push(format!("party probe {state}"));
    }

    fn note_instance_probe(&mut self, state: &'static str) {
        if self.instance_probe_state == Some(state) {
            return;
        }
        self.instance_probe_state = Some(state);
        self.diagnostics.push(format!("instance probe {state}"));
    }

    fn start_locate(&mut self, pid: u32) {
        self.locate_attempt += 1;
        let attempt = self.locate_attempt;
        let (sender, receiver) = mpsc::channel();
        let progress = Arc::new(AtomicU8::new(0));
        let scan_cpu = Arc::new(AtomicU64::new(CPU_UNAVAILABLE));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_progress = Arc::clone(&progress);
        let worker_cpu = Arc::clone(&scan_cpu);
        let worker_cancel = Arc::clone(&cancel);

        let spawn = thread::Builder::new()
            .name("farever-hashlink-locator".to_owned())
            .spawn(move || {
                let started = Instant::now();
                let Some(memory) = ProcessMemory::open(pid) else {
                    let _ = sender.send(LocateResult {
                        pid,
                        attempt,
                        anchor: None,
                        report: LocateReport::default(),
                        duration_ms: started.elapsed().as_millis(),
                        error: Some("could not open a read-only worker handle".to_owned()),
                    });
                    return;
                };
                let (anchor, report) =
                    GameAppAnchor::locate(&memory, &worker_progress, &worker_cpu, &worker_cancel);
                let _ = sender.send(LocateResult {
                    pid,
                    attempt,
                    anchor,
                    report,
                    duration_ms: started.elapsed().as_millis(),
                    error: None,
                });
            });

        match spawn {
            Ok(_) => {
                self.diagnostics.push(format!(
                    "locate attempt={attempt} started pid={pid} worker=farever-hashlink-locator"
                ));
                self.locate_task = Some(LocateTask {
                    progress,
                    scan_cpu,
                    cancel,
                    receiver,
                });
            }
            Err(error) => {
                self.last_locate_completed = Some(Instant::now());
                self.diagnostics.push(format!(
                    "locate attempt={attempt} pid={pid} worker_start_failed={error}"
                ));
            }
        }
    }

    fn collect_locate_result(&mut self) {
        let result = self
            .locate_task
            .as_ref()
            .map(|task| task.receiver.try_recv());
        match result {
            Some(Ok(result)) => {
                let task = self.locate_task.take();
                let cpu = task.as_ref().and_then(LocateTask::cpu_percent);
                self.last_locate_completed = Some(Instant::now());
                let current_pid = self.memory.as_ref().map(ProcessMemory::pid);
                let found = result.anchor.is_some() && current_pid == Some(result.pid);
                let error = result
                    .error
                    .as_deref()
                    .map_or(String::new(), |value| format!(" error={value:?}"));
                let cpu = cpu.map_or("unavailable".to_owned(), |value| format!("{value:.2}"));
                self.diagnostics.push(format!(
                    "locate attempt={} duration_ms={} found={} scan_cpu_percent={} {}{}",
                    result.attempt,
                    result.duration_ms,
                    found,
                    cpu,
                    result.report.summary(),
                    error,
                ));
                if current_pid == Some(result.pid) {
                    self.anchor = result.anchor;
                    if self.anchor.is_none() {
                        self.readiness.reset();
                    }
                }
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.locate_task = None;
                self.readiness.reset();
                self.last_locate_completed = Some(Instant::now());
                self.diagnostics
                    .push("locate worker disconnected before reporting a result".to_owned());
            }
            Some(Err(mpsc::TryRecvError::Empty)) | None => {}
        }
    }

    fn ensure_process(&mut self) -> bool {
        match &self.target {
            TargetProcess::Current => {
                if self.memory.is_none() {
                    let memory = ProcessMemory::current();
                    self.diagnostics.push(format!(
                        "using current-process pseudo handle pid={}",
                        memory.pid()
                    ));
                    self.memory = Some(memory);
                }
                true
            }
            TargetProcess::ProcessId(pid) => self.ensure_pid(*pid),
            TargetProcess::Named(name) => {
                let Some(pid) = find_process(name) else {
                    if self.memory.is_some() {
                        self.diagnostics
                            .push(format!("target process disappeared name={name:?}"));
                    }
                    self.memory = None;
                    self.reset_locator();
                    self.reset_provider_identities();
                    return false;
                };
                self.ensure_pid(pid)
            }
        }
    }

    fn ensure_pid(&mut self, pid: u32) -> bool {
        if self
            .memory
            .as_ref()
            .is_some_and(|memory| memory.pid() == pid)
        {
            return true;
        }
        self.memory = ProcessMemory::open(pid);
        self.reset_locator();
        self.reset_provider_identities();
        self.diagnostics.push(format!(
            "open process pid={pid} success={}",
            self.memory.is_some()
        ));
        self.memory.is_some()
    }

    fn reset_locator(&mut self) {
        if let Some(task) = self.locate_task.take() {
            task.cancel.store(true, Ordering::Relaxed);
        }
        self.anchor = None;
        self.last_locate_completed = None;
        self.locate_attempt = 0;
        self.readiness.reset();
    }
}

struct LocateTask {
    progress: Arc<AtomicU8>,
    scan_cpu: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<LocateResult>,
}

impl LocateTask {
    fn progress(&self) -> u8 {
        self.progress.load(Ordering::Relaxed).min(100)
    }

    fn cpu_percent(&self) -> Option<f64> {
        let bits = self.scan_cpu.load(Ordering::Relaxed);
        (bits != CPU_UNAVAILABLE).then(|| f64::from_bits(bits))
    }
}

impl Drop for LocateTask {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

struct LocateResult {
    pid: u32,
    attempt: u64,
    anchor: Option<GameAppAnchor>,
    report: LocateReport,
    duration_ms: u128,
    error: Option<String>,
}

#[derive(Default)]
struct LocateReport {
    cancelled: bool,
    regions: usize,
    eligible_regions: usize,
    eligible_bytes: usize,
    name_scan_bytes: usize,
    reference_scan_bytes: usize,
    read_failures: usize,
    name_hits: usize,
    validated_names: usize,
    references: usize,
    type_candidates: usize,
    app_super_matches: usize,
    static_app_matches: usize,
    instance_matches: usize,
}

impl LocateReport {
    fn summary(&self) -> String {
        format!(
            "cancelled={} regions={}/{} eligible_mib={} scanned_mib={}/{} read_failures={} name_hits={}/{} refs={} type_candidates={} app_supers={} static_apps={} instances={}",
            self.cancelled,
            self.eligible_regions,
            self.regions,
            self.eligible_bytes / (1024 * 1024),
            self.name_scan_bytes / (1024 * 1024),
            self.reference_scan_bytes / (1024 * 1024),
            self.read_failures,
            self.validated_names,
            self.name_hits,
            self.references,
            self.type_candidates,
            self.app_super_matches,
            self.static_app_matches,
            self.instance_matches,
        )
    }
}

struct GameAppAnchor {
    global_slot: usize,
    instance_offset: usize,
}

impl GameAppAnchor {
    /// Resolves the stable `$App -> GameApp` path from an exact runtime type
    /// already learned by the in-process allocation observer. This reuses the
    /// semantic locator's complete chain validation without searching process
    /// memory for the `GameApp` name first.
    fn from_type_address(hl: &HashLink<'_>, game_app_type: usize) -> Option<Self> {
        if hl.type_name(game_app_type).as_deref() != Some("GameApp") {
            return None;
        }
        let game_app_object = hl.memory.u64(game_app_type + TYPE_OBJECT)?;
        Self::from_type_object(hl, game_app_object, &mut LocateReport::default())
    }

    fn locate(
        memory: &ProcessMemory,
        progress: &AtomicU8,
        scan_cpu: &AtomicU64,
        cancel: &AtomicBool,
    ) -> (Option<Self>, LocateReport) {
        let hl = HashLink::new(memory);
        let needle = utf16z("GameApp");
        let regions = memory.regions();
        let eligible = regions
            .iter()
            .copied()
            .filter(|region| region.writable && region.size <= 64 * 1024 * 1024)
            .collect::<Vec<_>>();
        let mut report = LocateReport {
            regions: regions.len(),
            eligible_regions: eligible.len(),
            eligible_bytes: eligible.iter().map(|region| region.size).sum(),
            ..LocateReport::default()
        };
        let mut cpu_meter = ThreadCpuMeter::new();
        progress.store(1, Ordering::Relaxed);

        for region in &eligible {
            if cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                return (None, report);
            }
            let completed_before = report.name_scan_bytes;
            let eligible_bytes = report.eligible_bytes;
            let name_search =
                memory.find_bytes_in_with_progress(&needle, &[*region], 8, |region_bytes| {
                    if cpu_meter.update() {
                        if let Some(percent) = cpu_meter.percent() {
                            scan_cpu.store(percent.to_bits(), Ordering::Relaxed);
                        }
                    }
                    let scanned = completed_before.saturating_add(region_bytes);
                    let percent = scan_percent(scanned, eligible_bytes);
                    progress.fetch_max(percent, Ordering::Relaxed);
                    !cancel.load(Ordering::Relaxed)
                });
            report.name_scan_bytes += name_search.bytes_read;
            report.read_failures += name_search.read_failures;
            report.name_hits += name_search.hits.len();
            for name_address in name_search.hits {
                if hl.read_utf16z(name_address).as_deref() != Some("GameApp") {
                    continue;
                }
                report.validated_names += 1;
                let near = nearby_regions(&regions, name_address, 16 * 1024 * 1024);
                let reference_search = memory.find_qword_in(name_address, &near, 64);
                report.reference_scan_bytes += reference_search.bytes_read;
                report.read_failures += reference_search.read_failures;
                report.references += reference_search.hits.len();
                for reference in reference_search.hits {
                    let Some(type_object) = reference.checked_sub(TYPE_OBJECT_NAME) else {
                        continue;
                    };
                    if let Some(anchor) = Self::from_type_object(&hl, type_object, &mut report) {
                        progress.store(100, Ordering::Relaxed);
                        return (Some(anchor), report);
                    }
                }
            }
        }
        progress.store(100, Ordering::Relaxed);
        (None, report)
    }

    fn from_type_object(
        hl: &HashLink<'_>,
        game_app_object: usize,
        report: &mut LocateReport,
    ) -> Option<Self> {
        report.type_candidates += 1;
        let app_type = hl.memory.u64(game_app_object + TYPE_OBJECT_SUPER)?;
        if hl.type_name(app_type).as_deref() != Some("App") {
            return None;
        }
        report.app_super_matches += 1;
        let app_object = hl.memory.u64(app_type + TYPE_OBJECT)?;
        let global_slot = hl.memory.u64(app_object + TYPE_OBJECT_GLOBAL_VALUE)?;
        let static_app = hl.memory.u64(global_slot)?;
        if hl.class_of(static_app).as_deref() != Some("$App") {
            return None;
        }
        report.static_app_matches += 1;

        let bytes = hl.memory.read(static_app, 0x400)?;
        for offset in (8..bytes.len().saturating_sub(7)).step_by(8) {
            let candidate = u64::from_le_bytes(bytes[offset..offset + 8].try_into().ok()?) as usize;
            if hl.class_of(candidate).as_deref() == Some("GameApp") {
                report.instance_matches += 1;
                return Some(Self {
                    global_slot,
                    instance_offset: offset,
                });
            }
        }
        None
    }

    fn current(&self, hl: &HashLink<'_>) -> Option<usize> {
        let static_app = hl.memory.u64(self.global_slot)?;
        if hl.class_of(static_app).as_deref() != Some("$App") {
            return None;
        }
        let instance = hl.memory.u64(static_app + self.instance_offset)?;
        (hl.class_of(instance).as_deref() == Some("GameApp")).then_some(instance)
    }
}

fn scan_percent(scanned: usize, total: usize) -> u8 {
    if total == 0 {
        return 100;
    }
    let percent = (scanned as u128 * 99 / total as u128) as u8;
    percent.clamp(1, 99)
}

pub(crate) struct HashLink<'a> {
    pub(crate) memory: &'a ProcessMemory,
}

impl<'a> HashLink<'a> {
    pub(crate) fn new(memory: &'a ProcessMemory) -> Self {
        Self { memory }
    }

    pub(crate) fn class_of(&self, object: usize) -> Option<String> {
        self.type_name(self.memory.u64(object)?)
    }

    pub(crate) fn object_is_a(&self, object: usize, expected: &str) -> bool {
        let Some(concrete_type) = self.memory.u64(object) else {
            return false;
        };
        self.type_is_a(concrete_type, expected)
    }

    pub(crate) fn type_is_a(&self, current: usize, expected: &str) -> bool {
        self.type_address_named(current, expected).is_some()
    }

    pub(crate) fn type_address_named(&self, mut current: usize, expected: &str) -> Option<usize> {
        let mut seen = Vec::new();
        while is_pointer(current) && !seen.contains(&current) && seen.len() < 32 {
            seen.push(current);
            if self.type_name(current).as_deref() == Some(expected) {
                return Some(current);
            }
            let descriptor = self.memory.u64(current + TYPE_OBJECT)?;
            current = self.memory.u64(descriptor + TYPE_OBJECT_SUPER).unwrap_or(0);
        }
        None
    }

    pub(crate) fn type_name(&self, type_address: usize) -> Option<String> {
        if !is_pointer(type_address) || !matches!(self.memory.i32(type_address)?, HOBJ | HSTRUCT) {
            return None;
        }
        let object = self.memory.u64(type_address + TYPE_OBJECT)?;
        let name = self.memory.u64(object + TYPE_OBJECT_NAME)?;
        if let Some(name) = self
            .read_utf16z(name)
            .filter(|name| !name.is_empty() && name != "<none>")
        {
            return Some(name);
        }
        // Some bytecode builds leave the built-in String HOBJ unnamed. Promote
        // it only when the complete type descriptor matches its canonical shape.
        self.is_anonymous_string_type(type_address)
            .then(|| "String".to_owned())
    }

    fn is_anonymous_string_type(&self, type_address: usize) -> bool {
        if self.memory.i32(type_address) != Some(HOBJ) {
            return false;
        }
        let Some(object) = self.memory.u64(type_address + TYPE_OBJECT) else {
            return false;
        };
        if !is_pointer(object)
            || self
                .memory
                .u64(object + TYPE_OBJECT_SUPER)
                .is_none_or(is_pointer)
            || self.memory.i32(object) != Some(2)
        {
            return false;
        }
        let Some(fields) = self.memory.u64(object + TYPE_OBJECT_FIELDS) else {
            return false;
        };
        if !is_pointer(fields) {
            return false;
        }

        let mut has_bytes = false;
        let mut has_length = false;
        for index in 0..2 {
            let Some(field) = fields.checked_add(index * FIELD_STRIDE) else {
                return false;
            };
            let Some(name_address) = self.memory.u64(field) else {
                return false;
            };
            let Some(name) = self.read_utf16z(name_address) else {
                return false;
            };
            let Some(field_type_address) = field
                .checked_add(FIELD_TYPE)
                .and_then(|address| self.memory.u64(address))
            else {
                return false;
            };
            let Some(field_kind) = self.memory.i32(field_type_address) else {
                return false;
            };
            match (name.as_str(), field_kind) {
                ("bytes", HBYTES) => has_bytes = true,
                ("length", HI32) => has_length = true,
                _ => return false,
            }
        }
        has_bytes && has_length
    }

    /// Returns the declared name of an object, struct, or enum type.
    pub(crate) fn named_type_name(&self, type_address: usize) -> Option<String> {
        match self.memory.i32(type_address)? {
            HOBJ | HSTRUCT => self.type_name(type_address),
            HENUM => {
                let descriptor = self.memory.u64(type_address + TYPE_OBJECT)?;
                let name = self.memory.u64(descriptor)?;
                self.read_utf16z(name)
            }
            _ => None,
        }
    }

    /// Reads every declared field in one concrete HashLink virtual type.
    pub(crate) fn virtual_shape_for_type(
        &self,
        type_address: usize,
    ) -> Option<HashLinkVirtualShape> {
        if self.memory.i32(type_address)? != HVIRTUAL {
            return None;
        }
        let descriptor = self.memory.u64(type_address + TYPE_OBJECT)?;
        let count =
            usize::try_from(self.memory.i32(descriptor + TYPE_VIRTUAL_FIELD_COUNT)?).ok()?;
        if count >= 4096 {
            return None;
        }
        let fields = self.memory.u64(descriptor)?;
        if count > 0 && !is_pointer(fields) {
            return None;
        }
        let mut result = Vec::with_capacity(count);
        for index in 0..count {
            let field = fields.checked_add(index.checked_mul(FIELD_STRIDE)?)?;
            let name = self.read_utf16z(self.memory.u64(field)?)?;
            let field_type = self.memory.u64(field + FIELD_TYPE)?;
            if !is_pointer(field_type) {
                return None;
            }
            result.push(HashLinkVirtualFieldShape {
                name,
                type_address: field_type,
            });
        }
        Some(HashLinkVirtualShape { fields: result })
    }

    /// Returns one enum constructor's name and parameter count.
    pub(crate) fn enum_constructor(
        &self,
        type_address: usize,
        index: usize,
    ) -> Option<(String, usize)> {
        if self.memory.i32(type_address)? != HENUM {
            return None;
        }
        let descriptor = self.memory.u64(type_address + TYPE_OBJECT)?;
        let count =
            usize::try_from(self.memory.i32(descriptor + TYPE_ENUM_CONSTRUCT_COUNT)?).ok()?;
        if count >= 4096 || index >= count {
            return None;
        }
        let constructors = self.memory.u64(descriptor + TYPE_ENUM_CONSTRUCTS)?;
        let constructor = constructors.checked_add(index.checked_mul(ENUM_CONSTRUCT_STRIDE)?)?;
        let name = self.read_utf16z(self.memory.u64(constructor)?)?;
        let parameter_count =
            usize::try_from(self.memory.i32(constructor + ENUM_CONSTRUCT_PARAM_COUNT)?).ok()?;
        Some((name, parameter_count))
    }

    /// Reads one constructor's parameter type addresses and storage offsets.
    /// Offsets are relative to the payload following the `venum` header.
    pub(crate) fn enum_constructor_layout(
        &self,
        type_address: usize,
        index: usize,
    ) -> Option<(String, usize, Vec<(usize, usize)>)> {
        if self.memory.i32(type_address)? != HENUM {
            return None;
        }
        let descriptor = self.memory.u64(type_address + TYPE_OBJECT)?;
        let count =
            usize::try_from(self.memory.i32(descriptor + TYPE_ENUM_CONSTRUCT_COUNT)?).ok()?;
        if count >= 4096 || index >= count {
            return None;
        }
        let constructors = self.memory.u64(descriptor + TYPE_ENUM_CONSTRUCTS)?;
        let constructor = constructors.checked_add(index.checked_mul(ENUM_CONSTRUCT_STRIDE)?)?;
        let name = self.read_utf16z(self.memory.u64(constructor)?)?;
        let parameter_count =
            usize::try_from(self.memory.i32(constructor + ENUM_CONSTRUCT_PARAM_COUNT)?).ok()?;
        if parameter_count > 1024 {
            return None;
        }
        let size = usize::try_from(self.memory.i32(constructor + ENUM_CONSTRUCT_SIZE)?)
            .ok()
            .filter(|size| *size <= 1 << 20)?;
        let parameter_types = self.memory.u64(constructor + ENUM_CONSTRUCT_PARAM_TYPES)?;
        let parameter_offsets = self
            .memory
            .u64(constructor + ENUM_CONSTRUCT_PARAM_OFFSETS)?;
        if parameter_count > 0 && (!is_pointer(parameter_types) || !is_pointer(parameter_offsets)) {
            return None;
        }
        let mut parameters = Vec::with_capacity(parameter_count);
        for parameter in 0..parameter_count {
            let type_slot =
                parameter_types.checked_add(parameter.checked_mul(size_of::<usize>())?)?;
            let parameter_type = self.memory.u64(type_slot)?;
            let offset_slot = parameter_offsets.checked_add(parameter.checked_mul(4)?)?;
            let offset = usize::try_from(self.memory.i32(offset_slot)?).ok()?;
            if !is_pointer(parameter_type) || offset >= size {
                return None;
            }
            parameters.push((parameter_type, offset));
        }
        Some((name, size, parameters))
    }
    fn read_utf16z(&self, address: usize) -> Option<String> {
        let bytes = self.memory.read(address, 512)?;
        let end = (0..bytes.len().saturating_sub(1))
            .step_by(2)
            .find(|index| bytes[*index] == 0 && bytes[*index + 1] == 0)
            .unwrap_or(bytes.len() & !1);
        let (pairs, _) = bytes[..end].as_chunks::<2>();
        let words = pairs
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        String::from_utf16(&words).ok()
    }

    pub(crate) fn string(&self, object: usize) -> Option<String> {
        let chars = self.memory.u64(object + 8)?;
        let length = usize::try_from(self.memory.i32(object + 0x10)?).ok()?;
        if length > 4096 {
            return None;
        }
        let bytes = self.memory.read(chars, length * 2)?;
        let (pairs, _) = bytes.as_chunks::<2>();
        let words = pairs
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        String::from_utf16(&words).ok()
    }

    pub(crate) fn pointer_field(&self, object: usize, name: &str) -> Option<usize> {
        let offset = self.field_offset(object, name)?;
        let value = self.memory.u64(object + offset)?;
        is_pointer(value).then_some(value)
    }

    /// Reads a nullable object field while preserving the distinction between
    /// a readable null and an unavailable/corrupt field observation.
    fn optional_pointer_field(&self, object: usize, name: &str) -> Option<Option<usize>> {
        let offset = self.field_offset(object, name)?;
        let value = self.memory.u64(object + offset)?;
        if value == 0 {
            Some(None)
        } else {
            is_pointer(value).then_some(Some(value))
        }
    }

    pub(crate) fn field_offset(&self, object: usize, wanted: &str) -> Option<usize> {
        let concrete_type = self.memory.u64(object)?;
        self.field_offset_for_type(concrete_type, wanted)
    }

    /// Returns the declared HashLink field types together with their concrete
    /// runtime offsets. Unlike `field_offset_for_type`, this uses the documented
    /// x86-64 `hl_obj_field` and `hl_runtime_obj` layouts and is intended for
    /// fail-closed native callback validation rather than best-effort polling.
    pub(crate) fn object_shape_for_type(
        &self,
        concrete_type: usize,
    ) -> Option<HashLinkObjectShape> {
        let kind = self.memory.i32(concrete_type)?;
        if !matches!(kind, HOBJ | HSTRUCT) {
            return None;
        }
        let name = self.type_name(concrete_type)?;
        let mut levels = Vec::new();
        let mut current = concrete_type;
        let mut seen = Vec::new();

        while is_pointer(current) && !seen.contains(&current) && levels.len() < 32 {
            seen.push(current);
            if !matches!(self.memory.i32(current)?, HOBJ | HSTRUCT) {
                return None;
            }
            let object_type = self.memory.u64(current + TYPE_OBJECT)?;
            let count = usize::try_from(self.memory.i32(object_type)?).ok()?;
            if count >= 4096 {
                return None;
            }
            let fields = self.memory.u64(object_type + TYPE_OBJECT_FIELDS)?;
            let mut own = Vec::with_capacity(count);
            for index in 0..count {
                let descriptor = fields.checked_add(index.checked_mul(FIELD_STRIDE)?)?;
                let name_address = self.memory.u64(descriptor)?;
                let field_type = self.memory.u64(descriptor + FIELD_TYPE)?;
                if !is_pointer(field_type) {
                    return None;
                }
                own.push((self.read_utf16z(name_address)?, field_type));
            }
            levels.push(own);
            current = self
                .memory
                .u64(object_type + TYPE_OBJECT_SUPER)
                .unwrap_or(0);
        }

        if is_pointer(current) {
            return None;
        }
        let descriptors = levels.into_iter().rev().flatten().collect::<Vec<_>>();
        let object_type = self.memory.u64(concrete_type + TYPE_OBJECT)?;
        let runtime = self.memory.u64(object_type + TYPE_OBJECT_RUNTIME)?;
        let count = usize::try_from(self.memory.i32(runtime + RUNTIME_OBJECT_FIELD_COUNT)?).ok()?;
        let size = usize::try_from(self.memory.i32(runtime + RUNTIME_OBJECT_SIZE)?).ok()?;
        if count != descriptors.len()
            || count >= 4096
            || !(size_of::<usize>()..1 << 20).contains(&size)
        {
            return None;
        }
        let offsets_address = self.memory.u64(runtime + RUNTIME_OBJECT_FIELD_OFFSETS)?;
        let offsets = if count == 0 {
            Vec::new()
        } else {
            if !is_pointer(offsets_address) {
                return None;
            }
            let bytes = self.memory.read(offsets_address, count.checked_mul(4)?)?;
            let (chunks, remainder) = bytes.as_chunks::<4>();
            if !remainder.is_empty() {
                return None;
            }
            chunks
                .iter()
                .map(|part| usize::try_from(i32::from_le_bytes(*part)).ok())
                .collect::<Option<Vec<_>>>()?
        };
        if offsets
            .iter()
            .any(|offset| *offset < size_of::<usize>() || *offset >= size)
            || offsets.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return None;
        }

        let mut fields = Vec::with_capacity(count);
        for ((field_name, field_type), offset) in descriptors.into_iter().zip(offsets) {
            let field_kind = self.memory.i32(field_type)?;
            let object_type_name = matches!(field_kind, HOBJ | HSTRUCT)
                .then(|| self.type_name(field_type))
                .flatten();
            if matches!(field_kind, HOBJ | HSTRUCT) && object_type_name.is_none() {
                return None;
            }
            fields.push(HashLinkFieldShape {
                name: field_name,
                offset,
                kind: field_kind,
                type_address: field_type,
                object_type_name,
            });
        }
        Some(HashLinkObjectShape {
            name,
            kind,
            size,
            fields,
        })
    }

    pub(crate) fn field_offset_for_type(
        &self,
        concrete_type: usize,
        wanted: &str,
    ) -> Option<usize> {
        let mut levels = Vec::new();
        let mut current = concrete_type;
        let mut seen = Vec::new();

        while is_pointer(current) && !seen.contains(&current) && levels.len() < 32 {
            seen.push(current);
            if !matches!(self.memory.i32(current)?, HOBJ | HSTRUCT) {
                break;
            }
            let object_type = self.memory.u64(current + TYPE_OBJECT)?;
            let count = usize::try_from(self.memory.i32(object_type)?).ok()?;
            if count >= 4096 {
                return None;
            }
            let fields = self.memory.u64(object_type + TYPE_OBJECT_FIELDS)?;
            let mut own = Vec::with_capacity(count);
            for index in 0..count {
                let name_address = self.memory.u64(fields + index * FIELD_STRIDE)?;
                own.push(self.read_utf16z(name_address)?);
            }
            levels.push(own);
            current = self
                .memory
                .u64(object_type + TYPE_OBJECT_SUPER)
                .unwrap_or(0);
        }

        let names = levels.into_iter().rev().flatten().collect::<Vec<_>>();
        let index = names.iter().position(|name| name == wanted)?;

        let object_type = self.memory.u64(concrete_type + TYPE_OBJECT)?;
        let runtime = self.memory.u64(object_type + TYPE_OBJECT_RUNTIME)?;
        let count = usize::try_from(self.memory.i32(runtime + 8)?).ok()?;
        let size = usize::try_from(self.memory.i32(runtime + 0x10)?).ok()?;
        if count != names.len() || index >= count || count == 0 || count >= 4096 {
            return None;
        }

        for slot in [0x28, 0x20, 0x30, 0x18, 0x38] {
            let Some(offsets_address) = self.memory.u64(runtime + slot) else {
                continue;
            };
            let Some(bytes) = self.memory.read(offsets_address, count * 4) else {
                continue;
            };
            let (chunks, _) = bytes.as_chunks::<4>();
            let offsets = chunks
                .iter()
                .map(|part| i32::from_le_bytes(*part))
                .collect::<Vec<_>>();
            let plausible = offsets.first() == Some(&8)
                && offsets.windows(2).all(|pair| pair[0] < pair[1])
                && offsets
                    .iter()
                    .all(|offset| *offset > 0 && (*offset as usize) < 1 << 16);
            if !plausible {
                continue;
            }
            let offset = usize::try_from(offsets[index]).ok()?;
            if size > 0 && size < 1 << 20 && offset + 8 > size {
                return None;
            }
            return Some(offset);
        }
        None
    }

    fn object_array(&self, array: usize, limit: usize) -> Vec<usize> {
        let Some(length) = self
            .memory
            .i32(array + 8)
            .and_then(|value| usize::try_from(value).ok())
        else {
            return Vec::new();
        };
        let Some(backing) = self.memory.u64(array + 0x10) else {
            return Vec::new();
        };
        let Some(capacity) = self
            .memory
            .i32(backing + 0x10)
            .and_then(|value| usize::try_from(value).ok())
        else {
            return Vec::new();
        };
        let count = length.min(capacity).min(limit);
        let Some(bytes) = self.memory.read(backing + 0x18, count * 8) else {
            return Vec::new();
        };
        let (chunks, _) = bytes.as_chunks::<8>();
        chunks
            .iter()
            .map(|part| u64::from_le_bytes(*part) as usize)
            .filter(|value| is_pointer(*value))
            .collect()
    }

    fn object_array_checked(&self, array: usize, limit: usize) -> Option<Vec<usize>> {
        let length = self
            .memory
            .i32(array + 8)
            .and_then(|v| usize::try_from(v).ok())?;
        let backing = self.memory.u64(array + 0x10)?;
        let capacity = self
            .memory
            .i32(backing + 0x10)
            .and_then(|v| usize::try_from(v).ok())?;
        if length > capacity || length > limit {
            return None;
        }
        let count = length.min(capacity).min(limit);
        let bytes = self.memory.read(backing + 0x18, count * 8)?;
        let (chunks, _) = bytes.as_chunks::<8>();
        let values = chunks
            .iter()
            .map(|part| u64::from_le_bytes(*part) as usize)
            .collect::<Vec<_>>();
        values
            .iter()
            .all(|value| is_pointer(*value))
            .then_some(values)
    }

    /// Reads the object array carried by Farever's replicated
    /// `hxbit.ArrayProxyData` container.
    ///
    /// The proxy does not point directly at `hl.types.ArrayObj`: its `array`
    /// field points at `hl.types.ArrayDyn`, whose own `array` field contains
    /// the concrete object array. Keeping the two dereferences here prevents
    /// individual providers from accidentally interpreting the dynamic
    /// wrapper as an object-array header.
    fn object_array_proxy_checked(&self, proxy: usize, limit: usize) -> Option<Vec<usize>> {
        let array =
            array_proxy_object_array(proxy, |object, field| self.pointer_field(object, field))?;
        self.object_array_checked(array, limit)
    }
}

fn array_proxy_object_array(
    proxy: usize,
    mut pointer_field: impl FnMut(usize, &str) -> Option<usize>,
) -> Option<usize> {
    let dynamic_array = pointer_field(proxy, "array")?;
    pointer_field(dynamic_array, "array")
}

fn player_from_app(hl: &HashLink<'_>, app: usize) -> Option<usize> {
    let player = hl.pointer_field(app, "me").or_else(|| {
        hl.pointer_field(app, "hero")
            .and_then(|hero| hl.pointer_field(hero, "ownerPlayer"))
    })?;
    hl.object_is_a(player, "st.Player").then_some(player)
}

fn local_binding_from_app(hl: &HashLink<'_>, app: usize) -> Option<(usize, Option<usize>)> {
    let player = player_from_app(hl, app)?;
    let hero = hl
        .optional_pointer_field(player, "hero")
        .or_else(|| hl.optional_pointer_field(app, "hero"))?
        .filter(|hero| hl.class_of(*hero).as_deref() == Some("ent.Hero"));
    Some((player, hero))
}

fn read_party_state(
    hl: &HashLink<'_>,
    local_player: usize,
    known_hero_classes: &HashMap<usize, String>,
) -> Result<RawPartyState, &'static str> {
    // Farever deliberately uses the layer roster for Rift allies rather than
    // the ordinary social group. Matchmade Rift players therefore do not need
    // to appear in `Player.group` at all (see `Player.forGroupAllies`).
    let (stable_key, players_proxy, source, source_pointer) =
        match player_layer_main_activity(hl, local_player) {
            Some((layer, activity)) if hl.object_is_a(activity, "st.activity.Rift") => (
                u64::try_from(layer).ok(),
                hl.pointer_field(layer, "players")
                    .ok_or("unavailable stage=rift-layer-players")?,
                PartyRosterSource::RiftLayer,
                layer,
            ),
            _ => {
                let group = hl
                    .pointer_field(local_player, "group")
                    .ok_or("unavailable stage=group")?;
                if !hl.object_is_a(group, "st.Group") {
                    return Err("unavailable stage=group-type");
                }
                let stable_key = hl
                    .field_offset(group, "groupId")
                    .and_then(|offset| hl.memory.u64(group + offset))
                    .and_then(|value| u64::try_from(value).ok())
                    .filter(|value| *value != 0);
                (
                    stable_key,
                    hl.pointer_field(group, "players")
                        .ok_or("unavailable stage=group-players")?,
                    PartyRosterSource::Group,
                    group,
                )
            }
        };
    let players = hl
        .object_array_proxy_checked(players_proxy, PARTY_MEMBER_LIMIT)
        .ok_or("unavailable stage=roster-shape")?;
    if players.is_empty() {
        return Err("unavailable stage=roster-empty");
    }

    let mut members = Vec::with_capacity(players.len());
    let mut stable_keys = HashSet::with_capacity(players.len());
    let mut local_count = 0_usize;
    for player in players {
        if !hl.object_is_a(player, "st.Player") {
            return Err("unavailable stage=member-type");
        }
        let stable_key =
            read_string_field(hl, player, "uid").ok_or("unavailable stage=member-uid")?;
        if stable_key.is_empty() || !stable_keys.insert(stable_key.clone()) {
            return Err("unavailable stage=member-identity");
        }
        let is_local = player == local_player;
        local_count += usize::from(is_local);
        let name = read_string_field(hl, player, "name").filter(|value| !value.is_empty());
        let hero_pointer = hl
            .optional_pointer_field(player, "hero")
            .ok_or("unavailable stage=member-hero")?
            .filter(|hero| hl.object_is_a(*hero, "ent.Hero"));
        let class_id = read_player_class_id(hl, player, hero_pointer, known_hero_classes);
        let in_combat = hero_pointer.and_then(|hero| read_bool_field(hl, hero, "isInCombat"));
        members.push(RawPartyMember {
            stable_key,
            player_pointer: player,
            hero_pointer,
            is_local,
            name,
            class_id,
            in_combat,
        });
    }
    if local_count != 1 {
        return Err("unavailable stage=local-member");
    }

    Ok(RawPartyState {
        stable_key,
        members,
        source,
        source_pointer,
    })
}

fn read_player_class_id(
    hl: &HashLink<'_>,
    player: usize,
    hero: Option<usize>,
    known_hero_classes: &HashMap<usize, String>,
) -> Option<String> {
    let metadata_class = hl
        .pointer_field(player, "heroData")
        .and_then(|hero_data| read_string_field(hl, hero_data, "kind"))
        .and_then(|class_id| canonical_class_id(&class_id))
        .map(str::to_owned);
    if metadata_class.is_some() {
        return metadata_class;
    }

    let hero = hero?;
    known_hero_classes
        .get(&hero)
        .cloned()
        .or_else(|| read_hero_class_id(hl, hero))
}

fn read_hero_class_id(hl: &HashLink<'_>, hero: usize) -> Option<String> {
    let skills = hl.pointer_field(hero, "skills")?;
    let skill_kinds = hl
        .object_array_checked(skills, HERO_SKILL_LIMIT)?
        .into_iter()
        .filter(|skill| hl.object_is_a(*skill, "st.skill.BaseSkill"))
        .filter_map(|skill| read_string_field(hl, skill, "kind"));
    class_id_from_skill_kinds(skill_kinds).map(str::to_owned)
}

fn canonical_class_id(class_id: &str) -> Option<&'static str> {
    let class_id = class_id.trim();
    let class_id = class_id
        .get(..6)
        .filter(|prefix| {
            prefix.eq_ignore_ascii_case("class_") || prefix.eq_ignore_ascii_case("class-")
        })
        .and_then(|_| class_id.get(6..))
        .unwrap_or(class_id);

    ["Mage", "Priest", "Rogue", "Warrior"]
        .into_iter()
        .find(|known| known.eq_ignore_ascii_case(class_id))
}

fn class_id_from_skill_kinds<I, S>(skill_kinds: I) -> Option<&'static str>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    const CLASSES: [&str; 4] = ["Mage", "Priest", "Rogue", "Warrior"];
    let mut counts = [0_u16; CLASSES.len()];
    for kind in skill_kinds {
        let Some(prefix) = kind.as_ref().split_once('_').map(|(prefix, _)| prefix) else {
            continue;
        };
        let Some(class_id) = canonical_class_id(prefix) else {
            continue;
        };
        let Some(index) = CLASSES.iter().position(|known| *known == class_id) else {
            continue;
        };
        counts[index] = counts[index].saturating_add(1);
    }

    let best_count = counts.iter().copied().max().unwrap_or(0);
    if best_count == 0 || counts.iter().filter(|count| **count == best_count).count() != 1 {
        return None;
    }
    counts
        .iter()
        .position(|count| *count == best_count)
        .map(|index| CLASSES[index])
}

fn read_instance_state(
    hl: &HashLink<'_>,
    player: usize,
) -> Result<(InstanceObservation, &'static str), &'static str> {
    // `GameLayer.mainActivity` is the live world authority used by Farever's
    // own Rift behavior. Prefer it over the player's context collection, which
    // can be absent for a matchmade Rift even while the layer is active.
    if let Some((_layer, activity)) = player_layer_main_activity(hl, player) {
        return match classify_activity(hl, activity) {
            Ok(Some(instance)) => Ok((instance, "live source=main-activity")),
            Ok(None) => Ok((open_world_instance(), "live source=main-activity")),
            Err(()) => Err("unavailable stage=main-activity-kind"),
        };
    }

    // Retain the ActivityContext path as a best-effort fallback for moments
    // where a player is readable but its layer is not yet attached.
    let contexts_proxy = hl
        .pointer_field(player, "activityCtx")
        .ok_or("unavailable stage=activity-context")?;
    let contexts = hl
        .object_array_proxy_checked(contexts_proxy, ACTIVITY_CONTEXT_LIMIT)
        .ok_or("unavailable stage=activity-context-shape")?;

    for context in contexts {
        if !hl.object_is_a(context, "st.ActivityContext") {
            return Err("unavailable stage=activity-context-type");
        }
        let activity = hl
            .pointer_field(context, "activity")
            .ok_or("unavailable stage=activity-context-activity")?;
        match classify_activity(hl, activity) {
            Ok(Some(instance)) => {
                return Ok((instance, "live source=activity-context"));
            }
            Ok(None) => {}
            Err(()) => return Err("unavailable stage=activity-context-kind"),
        }
    }

    Ok((open_world_instance(), "live source=activity-context"))
}

fn player_layer_main_activity(hl: &HashLink<'_>, player: usize) -> Option<(usize, usize)> {
    let layer = player_layer(hl, player)?;
    let activity = hl.pointer_field(layer, "mainActivity")?;
    hl.object_is_a(activity, "st.Activity")
        .then_some((layer, activity))
}

fn player_layer(hl: &HashLink<'_>, player: usize) -> Option<usize> {
    let layer = hl.pointer_field(player, "layer")?;
    if !hl.object_is_a(layer, "st.GameLayer") {
        return None;
    }
    if let Some(game_layer_type) = hl
        .memory
        .u64(layer)
        .and_then(|concrete_type| hl.type_address_named(concrete_type, "st.GameLayer"))
    {
        // Method resolution deliberately starts from the declaring base type,
        // even if a future build stores a concrete GameLayer subtype here.
        crate::activity_hooks::observe_game_layer_type(game_layer_type);
    }
    Some(layer)
}

pub(crate) fn open_world_instance() -> InstanceObservation {
    InstanceObservation {
        key: "open-world".to_owned(),
        kind: InstanceKind::OpenWorld,
    }
}

fn classify_activity(
    hl: &HashLink<'_>,
    activity: usize,
) -> Result<Option<InstanceObservation>, ()> {
    let activity_type = hl.memory.u64(activity).ok_or(())?;
    let kind = read_string_field(hl, activity, "kind").ok_or(())?;
    classify_activity_type(hl, activity_type, &kind)
}

fn classify_activity_type(
    hl: &HashLink<'_>,
    activity_type: usize,
    kind: &str,
) -> Result<Option<InstanceObservation>, ()> {
    let classification = if hl.type_is_a(activity_type, "st.activity.Dungeon") {
        Some(("dungeon", InstanceKind::Dungeon))
    } else if [
        "st.activity.Rift",
        "st.activity.Ascension",
        "st.activity.MountRush",
    ]
    .iter()
    .any(|expected| hl.type_is_a(activity_type, expected))
    {
        Some(("other", InstanceKind::Other))
    } else {
        None
    };
    let Some((prefix, classification)) = classification else {
        return Ok(None);
    };
    if kind.is_empty() {
        return Err(());
    }
    Ok(Some(InstanceObservation {
        key: format!("{prefix}:{kind}"),
        kind: classification,
    }))
}

pub(crate) fn classify_main_activity_type(
    hl: &HashLink<'_>,
    activity_type: usize,
    kind: &str,
) -> Result<InstanceObservation, ()> {
    classify_activity_type(hl, activity_type, kind)
        .map(|instance| instance.unwrap_or_else(open_world_instance))
}

fn read_string_field(hl: &HashLink<'_>, object: usize, name: &str) -> Option<String> {
    hl.pointer_field(object, name)
        .and_then(|value| hl.string(value))
}

fn read_bool_field(hl: &HashLink<'_>, object: usize, name: &str) -> Option<bool> {
    let offset = hl.field_offset(object, name)?;
    match hl.memory.u8(object + offset)? {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn read_spatial_state(
    hl: &HashLink<'_>,
    app: usize,
    hero: Option<usize>,
    combat_references: Option<[Option<usize>; 3]>,
    snapshot: &mut FasSnapshotV0,
) {
    if let Some(camera) = hl.pointer_field(app, "gameCamera") {
        if hl.class_of(camera).as_deref() == Some("client.GameCamera") {
            if let Some(heading) =
                read_f64_field_any(hl, camera, &["curDirection", "rotationZ", "rotZ"])
                    .filter(|value| value.is_finite())
            {
                snapshot.camera_heading_radians = heading;
                snapshot.camera_heading_valid = 1;
            }
        }
    }

    let Some(hero) = hero else {
        return;
    };
    if let Some(position) = read_position(hl, hero) {
        snapshot.player_position = position;
        snapshot.player_position_valid = 1;
    }
    if let Some(heading) =
        read_f64_field_any(hl, hero, &["rotationZ", "rotZ"]).filter(|value| value.is_finite())
    {
        snapshot.player_heading_radians = heading;
        snapshot.player_heading_valid = 1;
    }

    let Some(combat_references) = combat_references else {
        return;
    };
    let slots = [
        (COMBAT_REFERENCE_TARGET, 0_usize),
        (COMBAT_REFERENCE_LOCKED_TARGET, 1_usize),
        (COMBAT_REFERENCE_AUTO_TARGET, 2_usize),
    ];
    let mut active_mask = 0_u8;
    let mut position_mask = 0_u8;
    let mut positions = [[0.0_f64; 3]; 3];

    for (mask, index) in slots {
        let Some(reference) = combat_references[index] else {
            continue;
        };
        if hl.class_of(reference).is_none() {
            return;
        }
        active_mask |= mask;
        if let Some(position) = read_position(hl, reference) {
            positions[index] = position;
            position_mask |= mask;
        }
    }

    snapshot.combat_references_available = 1;
    snapshot.combat_reference_active_mask = active_mask;
    snapshot.combat_reference_position_mask = position_mask;
    snapshot.combat_reference_positions = positions;
}

fn read_combat_membership(hl: &HashLink<'_>, hero: usize) -> Option<[Option<usize>; 3]> {
    let mut references = [None; 3];
    for (index, field) in ["target", "lockedTarget", "autoTarget"]
        .into_iter()
        .enumerate()
    {
        let reference = hl.optional_pointer_field(hero, field)?;
        if let Some(reference) = reference {
            hl.class_of(reference)?;
        }
        references[index] = reference;
    }
    Some(references)
}

fn read_lifecycle_state(
    hl: &HashLink<'_>,
    app: usize,
) -> Option<crate::lifecycle_hooks::LifecycleSnapshot> {
    let loading_state = hl
        .field_offset(app, "loadingState")
        .and_then(|offset| hl.memory.i32(app + offset))?;
    let world = hl.optional_pointer_field(app, "world")?;
    let (in_world, area) = match world {
        Some(world) => {
            let level = hl.pointer_field(world, "level")?;
            let area = hl.string(level).filter(|area| !area.is_empty())?;
            (true, Some(area))
        }
        None => (false, None),
    };
    Some(crate::lifecycle_hooks::LifecycleSnapshot {
        app,
        loading_state,
        in_world,
        area,
    })
}

fn read_position(hl: &HashLink<'_>, object: usize) -> Option<[f64; 3]> {
    let position = [
        read_f64_field(hl, object, "posx")?,
        read_f64_field(hl, object, "posy")?,
        read_f64_field(hl, object, "posz")?,
    ];
    plausible_position(position).then_some(position)
}

fn read_f64_field(hl: &HashLink<'_>, object: usize, name: &str) -> Option<f64> {
    let offset = hl.field_offset(object, name)?;
    hl.memory.f64(object + offset)
}

fn read_f64_field_any(hl: &HashLink<'_>, object: usize, names: &[&str]) -> Option<f64> {
    names
        .iter()
        .find_map(|name| read_f64_field(hl, object, name))
}

fn plausible_position([x, y, z]: [f64; 3]) -> bool {
    x.is_finite()
        && y.is_finite()
        && z.is_finite()
        && (-10_000.0..=10_000.0).contains(&x)
        && (-10_000.0..=10_000.0).contains(&y)
        && (-500.0..=1_500.0).contains(&z)
        && (x.abs() >= 0.01 || y.abs() >= 0.01)
}

fn nearby_regions(regions: &[Region], address: usize, span: usize) -> Vec<Region> {
    let low = address.saturating_sub(span);
    let high = address.saturating_add(span);
    regions
        .iter()
        .copied()
        .filter(|region| region.base.saturating_add(region.size) > low && region.base < high)
        .collect()
}

fn utf16z(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn is_pointer(value: usize) -> bool {
    (0x1_0000..0x0000_8000_0000_0000).contains(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_root_tracker_requests_only_replacement_or_reacquisition() {
        let mut roots = WindowRootTracker::default();

        assert!(!roots.observe(Some((0x1000_0000, 0x2000_0000))));
        assert!(!roots.observe(Some((0x1000_0000, 0x2000_0000))));
        assert!(roots.observe(Some((0x1000_1000, 0x2000_0000))));
        assert!(!roots.observe(None));
        assert!(roots.observe(Some((0x1000_1000, 0x2000_0000))));
    }

    #[test]
    fn player_root_tracker_requests_only_replacement_or_reacquisition() {
        let mut roots = PlayerRootTracker::default();

        assert!(!roots.observe(None));
        assert!(!roots.observe(Some(0x1000_0000)));
        assert!(!roots.observe(Some(0x1000_0000)));
        assert!(roots.observe(Some(0x1000_1000)));
        assert!(!roots.observe(None));
        assert!(roots.observe(Some(0x1000_1000)));
    }

    #[test]
    fn hero_binding_tracker_invalidates_on_clear_and_same_app_replacement() {
        let mut binding = HeroBindingTracker::default();

        assert!(!binding.observe(Some(0x1000_0000)));
        assert!(!binding.observe(Some(0x1000_0000)));
        assert!(binding.observe(None));
        assert!(binding.observe(Some(0x2000_0000)));
        assert!(!binding.observe(Some(0x2000_0000)));
        assert!(binding.observe(Some(0x3000_0000)));
    }

    fn raw_party(group: u64, members: &[(&str, bool, &str)]) -> RawPartyState {
        RawPartyState {
            stable_key: Some(group),
            members: members
                .iter()
                .enumerate()
                .map(|(index, (stable_key, is_local, name))| RawPartyMember {
                    stable_key: (*stable_key).to_owned(),
                    player_pointer: 0x1000_0000 + index * 0x1000,
                    hero_pointer: Some(0x2000_0000 + index * 0x1000),
                    is_local: *is_local,
                    name: Some((*name).to_owned()),
                    class_id: Some("Warrior".to_owned()),
                    in_combat: Some(false),
                })
                .collect(),
            source: PartyRosterSource::Group,
            source_pointer: group as usize,
        }
    }

    #[test]
    fn party_identity_is_opaque_and_stable_within_one_process_session() {
        let mut poller = Poller::new(TargetProcess::Current);
        let first = poller.normalize_party(raw_party(42, &[("private-local-uid", true, "Local")]));
        let second = poller.normalize_party(raw_party(
            42,
            &[
                ("private-local-uid", true, "Local"),
                ("private-remote-uid", false, "Remote"),
            ],
        ));

        assert_eq!(first.party_id.as_deref(), Some("party-1"));
        assert_eq!(first.members[0].actor_id, "actor-1");
        assert_eq!(second.party_id, first.party_id);
        assert_eq!(second.members[0].actor_id, first.members[0].actor_id);
        assert_eq!(second.members[1].actor_id, "actor-2");
        assert!(!second.members[0].actor_id.contains("private-local-uid"));
        assert_eq!(poller.party_damage_sources.len(), 2);
        assert_eq!(
            poller.party_damage_sources[1],
            PartyDamageSource {
                actor_id: "actor-2".to_owned(),
                player_pointer: 0x1000_1000,
                hero_pointer: Some(0x2000_1000),
                is_local: false,
            }
        );
        assert_eq!(poller.hero_class_ids.len(), 2);
        assert_eq!(
            poller
                .take_diagnostics()
                .into_iter()
                .filter(|entry| entry.starts_with("party class resolved"))
                .collect::<Vec<_>>(),
            [
                "party class resolved actor_id=actor-1 class_id=Warrior".to_owned(),
                "party class resolved actor_id=actor-2 class_id=Warrior".to_owned(),
            ]
        );

        poller.normalize_party(raw_party(
            42,
            &[
                ("private-local-uid", true, "Local"),
                ("private-remote-uid", false, "Remote"),
            ],
        ));
        assert!(poller.take_diagnostics().is_empty());

        poller.clear_provider_observations();
        assert!(poller.party.is_none());
        assert!(poller.party_damage_sources.is_empty());
        assert_eq!(poller.hero_class_ids.len(), 2);

        poller.reset_provider_identities();
        assert!(poller.hero_class_ids.is_empty());
    }

    #[test]
    fn array_proxy_unwraps_array_dyn_before_the_object_array() {
        const PROXY: usize = 0x1000_0000;
        const ARRAY_DYN: usize = 0x2000_0000;
        const ARRAY_OBJ: usize = 0x3000_0000;

        let mut reads = Vec::new();
        let resolved = array_proxy_object_array(PROXY, |object, field| {
            reads.push((object, field.to_owned()));
            match object {
                PROXY => Some(ARRAY_DYN),
                ARRAY_DYN => Some(ARRAY_OBJ),
                _ => None,
            }
        });

        assert_eq!(resolved, Some(ARRAY_OBJ));
        assert_eq!(
            reads,
            vec![(PROXY, "array".to_owned()), (ARRAY_DYN, "array".to_owned())]
        );
    }

    #[test]
    fn array_proxy_rejects_a_missing_dynamic_array_payload() {
        const PROXY: usize = 0x1000_0000;
        const ARRAY_DYN: usize = 0x2000_0000;

        assert_eq!(
            array_proxy_object_array(PROXY, |object, _field| {
                (object == PROXY).then_some(ARRAY_DYN)
            }),
            None
        );
    }

    #[test]
    fn class_metadata_is_normalized_to_the_public_class_ids() {
        assert_eq!(canonical_class_id("Warrior"), Some("Warrior"));
        assert_eq!(canonical_class_id(" class_MAGE "), Some("Mage"));
        assert_eq!(canonical_class_id("class-priest"), Some("Priest"));
        assert_eq!(canonical_class_id("GS"), None);
    }

    #[test]
    fn hero_skill_metadata_resolves_the_dominant_class() {
        assert_eq!(
            class_id_from_skill_kinds([
                "PhysicalBlock",
                "GS_Base_Attack",
                "Warrior_Rage",
                "Warrior_Charge",
                "Warrior_Hemorrhage",
                "Mage_RayOfSpark",
            ]),
            Some("Warrior")
        );
    }

    #[test]
    fn hero_skill_metadata_fails_closed_on_missing_or_tied_classes() {
        assert_eq!(
            class_id_from_skill_kinds(["PhysicalBlock", "GS_Base_Attack"]),
            None
        );
        assert_eq!(
            class_id_from_skill_kinds(["Warrior_Charge", "Mage_RayOfSpark"]),
            None
        );
    }

    #[test]
    fn utf16_needle_is_terminated() {
        assert_eq!(utf16z("A"), vec![65, 0, 0, 0]);
    }

    #[test]
    fn scan_progress_is_bounded_and_proportional() {
        assert_eq!(scan_percent(0, 100), 1);
        assert_eq!(scan_percent(50, 100), 49);
        assert_eq!(scan_percent(100, 100), 99);
        assert_eq!(scan_percent(0, 0), 100);
    }

    #[test]
    fn allocator_learned_game_app_type_resolves_the_global_anchor() {
        let game_app_name = wide_buffer("GameApp");
        let app_name = wide_buffer("App");
        let static_app_name = wide_buffer("$App");

        let mut static_app_descriptor = Box::new([0_u8; 0x50]);
        write_usize(
            static_app_descriptor.as_mut(),
            TYPE_OBJECT_NAME,
            static_app_name.as_ptr() as usize,
        );
        let mut static_app_type = Box::new([0_u8; 0x20]);
        write_i32(static_app_type.as_mut(), 0, HOBJ);
        write_usize(
            static_app_type.as_mut(),
            TYPE_OBJECT,
            static_app_descriptor.as_ptr() as usize,
        );

        let mut game_app_descriptor = Box::new([0_u8; 0x50]);
        write_usize(
            game_app_descriptor.as_mut(),
            TYPE_OBJECT_NAME,
            game_app_name.as_ptr() as usize,
        );
        let mut game_app_type = Box::new([0_u8; 0x20]);
        write_i32(game_app_type.as_mut(), 0, HOBJ);
        write_usize(
            game_app_type.as_mut(),
            TYPE_OBJECT,
            game_app_descriptor.as_ptr() as usize,
        );

        let mut game_app_instance = Box::new([0_u8; size_of::<usize>()]);
        write_usize(
            game_app_instance.as_mut(),
            0,
            game_app_type.as_ptr() as usize,
        );
        let mut static_app = Box::new([0_u8; 0x400]);
        write_usize(static_app.as_mut(), 0, static_app_type.as_ptr() as usize);
        write_usize(
            static_app.as_mut(),
            size_of::<usize>(),
            game_app_instance.as_ptr() as usize,
        );
        let global_slot = Box::new(static_app.as_ptr() as usize);

        let mut app_descriptor = Box::new([0_u8; 0x50]);
        write_usize(
            app_descriptor.as_mut(),
            TYPE_OBJECT_NAME,
            app_name.as_ptr() as usize,
        );
        write_usize(
            app_descriptor.as_mut(),
            TYPE_OBJECT_GLOBAL_VALUE,
            (&*global_slot) as *const usize as usize,
        );
        let mut app_type = Box::new([0_u8; 0x20]);
        write_i32(app_type.as_mut(), 0, HOBJ);
        write_usize(
            app_type.as_mut(),
            TYPE_OBJECT,
            app_descriptor.as_ptr() as usize,
        );
        write_usize(
            game_app_descriptor.as_mut(),
            TYPE_OBJECT_SUPER,
            app_type.as_ptr() as usize,
        );

        let memory = ProcessMemory::current();
        let hl = HashLink::new(&memory);
        let anchor = GameAppAnchor::from_type_address(&hl, game_app_type.as_ptr() as usize)
            .expect("allocator-learned GameApp type should resolve the global anchor");

        assert_eq!(anchor.global_slot, (&*global_slot) as *const usize as usize);
        assert_eq!(anchor.instance_offset, size_of::<usize>());
        assert_eq!(
            anchor.current(&hl),
            Some(game_app_instance.as_ptr() as usize)
        );
    }

    #[test]
    fn spatial_positions_reject_non_finite_and_implausible_values() {
        assert!(plausible_position([12.0, -7.5, 3.0]));
        assert!(!plausible_position([0.0, 0.0, 3.0]));
        assert!(!plausible_position([f64::NAN, 2.0, 3.0]));
        assert!(!plausible_position([2.0, 3.0, 1_501.0]));
    }

    #[test]
    fn object_shape_reads_declared_types_and_runtime_offsets() {
        let result_name = wide_buffer("st.skill.DamageResult");
        let source_name = wide_buffer("serverSource");
        let amount_name = wide_buffer("_amount");
        let critical_name = wide_buffer("_critical");
        let game_object_name = wide_buffer("ent.GameObject");

        let mut game_object_descriptor = Box::new([0_u8; 0x50]);
        write_usize(
            game_object_descriptor.as_mut(),
            TYPE_OBJECT_NAME,
            game_object_name.as_ptr() as usize,
        );
        let mut game_object_type = Box::new([0_u8; 0x20]);
        write_i32(game_object_type.as_mut(), 0, HOBJ);
        write_usize(
            game_object_type.as_mut(),
            TYPE_OBJECT,
            game_object_descriptor.as_ptr() as usize,
        );

        let mut f64_type = Box::new([0_u8; 0x20]);
        write_i32(f64_type.as_mut(), 0, 6);
        let mut bool_type = Box::new([0_u8; 0x20]);
        write_i32(bool_type.as_mut(), 0, 7);

        let mut field_descriptors = Box::new([0_u8; FIELD_STRIDE * 3]);
        write_field_descriptor(
            field_descriptors.as_mut(),
            0,
            source_name.as_ptr() as usize,
            game_object_type.as_ptr() as usize,
        );
        write_field_descriptor(
            field_descriptors.as_mut(),
            1,
            amount_name.as_ptr() as usize,
            f64_type.as_ptr() as usize,
        );
        write_field_descriptor(
            field_descriptors.as_mut(),
            2,
            critical_name.as_ptr() as usize,
            bool_type.as_ptr() as usize,
        );

        let field_offsets = Box::new([8_i32, 16, 24]);
        let mut runtime = Box::new([0_u8; 0x78]);
        write_i32(runtime.as_mut(), RUNTIME_OBJECT_FIELD_COUNT, 3);
        write_i32(runtime.as_mut(), RUNTIME_OBJECT_SIZE, 32);
        write_usize(
            runtime.as_mut(),
            RUNTIME_OBJECT_FIELD_OFFSETS,
            field_offsets.as_ptr() as usize,
        );

        let mut result_descriptor = Box::new([0_u8; 0x50]);
        write_i32(result_descriptor.as_mut(), 0, 3);
        write_usize(
            result_descriptor.as_mut(),
            TYPE_OBJECT_NAME,
            result_name.as_ptr() as usize,
        );
        write_usize(
            result_descriptor.as_mut(),
            TYPE_OBJECT_FIELDS,
            field_descriptors.as_ptr() as usize,
        );
        write_usize(
            result_descriptor.as_mut(),
            TYPE_OBJECT_RUNTIME,
            runtime.as_ptr() as usize,
        );
        let mut result_type = Box::new([0_u8; 0x20]);
        write_i32(result_type.as_mut(), 0, HOBJ);
        write_usize(
            result_type.as_mut(),
            TYPE_OBJECT,
            result_descriptor.as_ptr() as usize,
        );

        let memory = ProcessMemory::current();
        let shape = HashLink::new(&memory)
            .object_shape_for_type(result_type.as_ptr() as usize)
            .expect("synthetic HashLink shape");
        assert_eq!(shape.name, "st.skill.DamageResult");
        assert_eq!(shape.size, 32);
        assert_eq!(shape.fields[0].offset, 8);
        assert_eq!(shape.fields[0].kind, HOBJ);
        assert_eq!(
            shape.fields[0].object_type_name.as_deref(),
            Some("ent.GameObject")
        );
        assert_eq!(shape.fields[1].kind, 6);
        assert_eq!(shape.fields[2].kind, 7);
    }

    #[test]
    fn virtual_and_enum_shapes_read_declared_runtime_metadata() {
        let channel_field_name = wide_buffer("channel");
        let text_field_name = wide_buffer("text");
        let channel_type_name = wide_buffer("st.Channel");
        let local_constructor_name = wide_buffer("Local");
        let string_type_name = wide_buffer("String");

        let mut constructor = Box::new([0_u8; ENUM_CONSTRUCT_STRIDE]);
        write_usize(
            constructor.as_mut(),
            0,
            local_constructor_name.as_ptr() as usize,
        );
        write_i32(constructor.as_mut(), ENUM_CONSTRUCT_PARAM_COUNT, 0);
        let mut enum_descriptor = Box::new([0_u8; 0x20]);
        write_usize(
            enum_descriptor.as_mut(),
            0,
            channel_type_name.as_ptr() as usize,
        );
        write_i32(enum_descriptor.as_mut(), TYPE_ENUM_CONSTRUCT_COUNT, 1);
        write_usize(
            enum_descriptor.as_mut(),
            TYPE_ENUM_CONSTRUCTS,
            constructor.as_ptr() as usize,
        );
        let mut channel_type = Box::new([0_u8; 0x20]);
        write_i32(channel_type.as_mut(), 0, HENUM);
        write_usize(
            channel_type.as_mut(),
            TYPE_OBJECT,
            enum_descriptor.as_ptr() as usize,
        );

        let mut string_descriptor = Box::new([0_u8; 0x50]);
        write_usize(
            string_descriptor.as_mut(),
            TYPE_OBJECT_NAME,
            string_type_name.as_ptr() as usize,
        );
        let mut string_type = Box::new([0_u8; 0x20]);
        write_i32(string_type.as_mut(), 0, HOBJ);
        write_usize(
            string_type.as_mut(),
            TYPE_OBJECT,
            string_descriptor.as_ptr() as usize,
        );

        let mut fields = Box::new([0_u8; FIELD_STRIDE * 2]);
        write_field_descriptor(
            fields.as_mut(),
            0,
            channel_field_name.as_ptr() as usize,
            channel_type.as_ptr() as usize,
        );
        write_field_descriptor(
            fields.as_mut(),
            1,
            text_field_name.as_ptr() as usize,
            string_type.as_ptr() as usize,
        );
        let mut virtual_descriptor = Box::new([0_u8; 0x20]);
        write_usize(virtual_descriptor.as_mut(), 0, fields.as_ptr() as usize);
        write_i32(virtual_descriptor.as_mut(), TYPE_VIRTUAL_FIELD_COUNT, 2);
        let mut virtual_type = Box::new([0_u8; 0x20]);
        write_i32(virtual_type.as_mut(), 0, HVIRTUAL);
        write_usize(
            virtual_type.as_mut(),
            TYPE_OBJECT,
            virtual_descriptor.as_ptr() as usize,
        );

        let memory = ProcessMemory::current();
        let hl = HashLink::new(&memory);
        let shape = hl
            .virtual_shape_for_type(virtual_type.as_ptr() as usize)
            .expect("synthetic HashLink virtual shape");
        assert_eq!(shape.fields.len(), 2);
        assert_eq!(shape.fields[0].name, "channel");
        assert_eq!(shape.fields[0].type_address, channel_type.as_ptr() as usize);
        assert_eq!(shape.fields[1].name, "text");
        assert_eq!(shape.fields[1].type_address, string_type.as_ptr() as usize);
        assert_eq!(
            hl.named_type_name(channel_type.as_ptr() as usize)
                .as_deref(),
            Some("st.Channel")
        );
        assert_eq!(
            hl.enum_constructor(channel_type.as_ptr() as usize, 0),
            Some(("Local".to_owned(), 0))
        );
        assert_eq!(hl.enum_constructor(channel_type.as_ptr() as usize, 1), None);
    }

    fn wide_buffer(value: &str) -> Box<[u16; 256]> {
        let mut buffer = Box::new([0_u16; 256]);
        for (destination, source) in buffer.iter_mut().zip(value.encode_utf16()) {
            *destination = source;
        }
        buffer
    }

    fn write_field_descriptor(
        descriptors: &mut [u8],
        index: usize,
        name: usize,
        field_type: usize,
    ) {
        let start = index * FIELD_STRIDE;
        write_usize(descriptors, start, name);
        write_usize(descriptors, start + FIELD_TYPE, field_type);
    }

    fn write_i32(bytes: &mut [u8], offset: usize, value: i32) {
        bytes[offset..offset + size_of::<i32>()].copy_from_slice(&value.to_le_bytes());
    }

    fn write_usize(bytes: &mut [u8], offset: usize, value: usize) {
        bytes[offset..offset + size_of::<usize>()].copy_from_slice(&value.to_le_bytes());
    }
}
