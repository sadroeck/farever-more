use crate::state::InstanceObservation;
use farever_more_api::{
    Availability, CameraState, CombatEvent, CombatReference, CombatReferenceSlot,
    CombatReferencesState, EventBatch, EventHeader, FasSnapshotV0, GameSnapshot, HostEvent,
    InstanceState, MapState, PartyState, PlayerState, SessionState, SourceQuality, StateSnapshot,
    UiState, UnavailableReason, Vec3, ADAPTER_LIVE, COMBAT_REFERENCE_AUTO_TARGET,
    COMBAT_REFERENCE_LOCKED_TARGET, COMBAT_REFERENCE_TARGET, MAX_EVENT_BATCH,
};
use std::collections::BTreeSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FALLBACK_COMBAT_IDLE: Duration = Duration::from_secs(60);
const SAMPLED_COMBAT_END_GRACE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
struct PartyRosterIdentity {
    party_id: Option<String>,
    members: Vec<(String, bool)>,
}

impl From<&PartyState> for PartyRosterIdentity {
    fn from(party: &PartyState) -> Self {
        let mut members = party
            .members
            .iter()
            .map(|member| (member.actor_id.clone(), member.is_local))
            .collect::<Vec<_>>();
        members.sort_unstable();
        Self {
            party_id: party.party_id.clone(),
            members,
        }
    }
}

pub struct EventTracker {
    started: Instant,
    process_session: u64,
    next_event_sequence: u64,
    previous_area: Option<String>,
    previous_windows: BTreeSet<String>,
    previous_focused_window: Option<String>,
    map_revision: u64,
    ui_revision: u64,
    player_revision: u64,
    camera_revision: u64,
    combat_references_revision: u64,
    party_revision: u64,
    instance_revision: u64,
    previous_player: Option<PlayerState>,
    previous_camera: Option<CameraState>,
    previous_combat_references: Option<CombatReferencesState>,
    previous_party: Option<PartyState>,
    previous_instance: Option<InstanceState>,
    zone_hook_available: bool,
    zone_hook_edges: Vec<Option<String>>,
    zone_sampled: bool,
    window_hook_available: bool,
    window_hook_edges: Vec<(bool, String)>,
    window_hook_focus: Option<Option<String>>,
    windows_sampled: bool,
    party_provider_enabled: bool,
    party_observation: Option<PartyState>,
    party_hook_available: bool,
    party_hook_dirty: bool,
    current_party_roster: Option<PartyRosterIdentity>,
    instance_provider_enabled: bool,
    instance_observation: Option<InstanceObservation>,
    instance_hook_available: bool,
    instance_hook_observations: Vec<InstanceObservation>,
    current_instance_key: Option<String>,
    current_instance: Option<InstanceState>,
    next_instance_id: u64,
    active_fight: Option<u64>,
    next_fight_id: u64,
    fight_started_ms: Option<u64>,
    last_damage_ms: Option<u64>,
    sampled_idle_since_ms: Option<u64>,
    damage_this_update: bool,
    direct_combat_available: bool,
    direct_combat_started: bool,
    direct_combat_end_pending: bool,
    initialized: bool,
}

impl EventTracker {
    pub fn new() -> Self {
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            started: Instant::now(),
            process_session: epoch.rotate_left(17) ^ u64::from(std::process::id()),
            next_event_sequence: 1,
            previous_area: None,
            previous_windows: BTreeSet::new(),
            previous_focused_window: None,
            map_revision: 0,
            ui_revision: 0,
            player_revision: 0,
            camera_revision: 0,
            combat_references_revision: 0,
            party_revision: 0,
            instance_revision: 0,
            previous_player: None,
            previous_camera: None,
            previous_combat_references: None,
            previous_party: None,
            previous_instance: None,
            zone_hook_available: false,
            zone_hook_edges: Vec::new(),
            zone_sampled: true,
            window_hook_available: false,
            window_hook_edges: Vec::new(),
            window_hook_focus: None,
            windows_sampled: true,
            party_provider_enabled: false,
            party_observation: None,
            party_hook_available: false,
            party_hook_dirty: false,
            current_party_roster: None,
            instance_provider_enabled: false,
            instance_observation: None,
            instance_hook_available: false,
            instance_hook_observations: Vec::new(),
            current_instance_key: None,
            current_instance: None,
            next_instance_id: 1,
            active_fight: None,
            next_fight_id: 1,
            fight_started_ms: None,
            last_damage_ms: None,
            sampled_idle_since_ms: None,
            damage_this_update: false,
            direct_combat_available: false,
            direct_combat_started: false,
            direct_combat_end_pending: false,
            initialized: false,
        }
    }

    /// Supplies one authoritative party-roster sample from the native state
    /// provider. `None` means this update could not validate the provider, not
    /// that the local player has an empty party.
    pub fn observe_party(&mut self, party: Option<PartyState>) {
        self.party_provider_enabled = true;
        self.party_observation = party;
    }

    /// Marks whether roster membership has a direct dirty boundary. Metadata
    /// still comes from the native roster sample, while membership events are
    /// observed only after one of these hook edges.
    pub fn observe_party_hook(&mut self, available: bool, dirty: bool) {
        self.party_hook_available = available;
        if available {
            self.party_hook_dirty |= dirty;
        } else {
            self.party_hook_dirty = false;
        }
    }

    /// Supplies the current native instance classification. The tracker owns
    /// the process-scoped monotonic session ID and derives transitions.
    pub fn observe_instance(&mut self, instance: Option<InstanceObservation>) {
        self.instance_provider_enabled = true;
        self.instance_observation = instance;
    }

    /// Supplies ordered, local-layer `set_mainActivity` observations. When the
    /// hook is available these are authoritative lifecycle edges; sampled
    /// state is used only to seed or reconcile the current identity.
    pub fn observe_instance_hook(
        &mut self,
        available: bool,
        observations: Vec<InstanceObservation>,
    ) {
        self.instance_provider_enabled |= available;
        self.instance_hook_available = available;
        self.instance_hook_observations = observations;
    }

    /// Supplies exact local-player combat edges captured by the injected
    /// provider. Once available, these edges supersede the sampled field and
    /// the idle timeout; a pending end is held until damage from the same host
    /// update has been ordered.
    pub fn observe_direct_combat(&mut self, available: bool, edges: &[bool]) {
        self.direct_combat_available = available;
        self.direct_combat_started = false;
        if !available {
            self.direct_combat_end_pending = false;
            return;
        }
        for &active in edges {
            if active {
                self.direct_combat_started = true;
                self.direct_combat_end_pending = false;
            } else {
                self.direct_combat_end_pending = true;
            }
        }
    }

    /// Supplies exact UI membership edges from the direct BaseUI hooks. A
    /// sampled set is used only while hooks are unavailable or to reconcile
    /// direct-provider activation, queue loss, or root replacement; it does
    /// not manufacture events while hooks are authoritative.
    pub fn observe_window_hook(
        &mut self,
        available: bool,
        windows_sampled: bool,
        focused_window: Option<Option<String>>,
        edges: Vec<(bool, String)>,
    ) {
        self.window_hook_available = available;
        self.windows_sampled = windows_sampled;
        self.window_hook_focus = focused_window;
        self.window_hook_edges = edges;
    }

    /// Supplies committed zone lifecycle edges. A sampled area is used only
    /// for direct-provider activation/reconciliation or sampled fallback; it
    /// never manufactures an event while the hook is authoritative.
    pub fn observe_zone_hook(
        &mut self,
        available: bool,
        zone_sampled: bool,
        edges: Vec<Option<String>>,
    ) {
        self.zone_hook_available = available;
        self.zone_sampled = zone_sampled;
        self.zone_hook_edges = edges;
    }

    /// Starts one host update and publishes snapshot-derived edges.
    ///
    /// Combat start is emitted here so it precedes damage captured during the
    /// same host update. Combat end is deliberately deferred to
    /// [`Self::finish_update`] so a late damage display cannot follow its end.
    pub fn update(
        &mut self,
        raw: &FasSnapshotV0,
        combat_state: Option<bool>,
    ) -> (GameSnapshot, EventBatch) {
        let captured_at_ms = self.started.elapsed().as_millis() as u64;
        self.damage_this_update = false;
        let sampled_area = (!raw.area().is_empty()).then(|| raw.area().to_owned());
        let mut area = self.previous_area.clone();
        let sampled_windows = (0..raw.window_count as usize)
            .filter_map(|index| raw.window(index).map(ToOwned::to_owned))
            .collect::<BTreeSet<_>>();
        // `BaseUI.displayWindow` inserts at index zero. The native list order
        // remains internal; the public open-window list is still unordered.
        let sampled_focused_window = raw.window(0).map(ToOwned::to_owned);
        let mut windows = self.previous_windows.clone();
        let mut focused_window = self.previous_focused_window.clone();
        let mut events = Vec::new();

        if self.zone_hook_available {
            if self.zone_sampled {
                if sampled_area != area {
                    self.map_revision = self.map_revision.wrapping_add(1);
                }
                area = sampled_area;
            }
            for next_area in std::mem::take(&mut self.zone_hook_edges) {
                if next_area == area {
                    continue;
                }
                let previous_area_id = area;
                area = next_area;
                self.map_revision = self.map_revision.wrapping_add(1);
                let header = self.header(captured_at_ms, SourceQuality::Observed);
                events.push(HostEvent::ZoneChanged {
                    header,
                    previous_area_id,
                    area_id: area.clone(),
                });
            }
        } else if self.zone_sampled {
            if self.initialized && sampled_area != area {
                self.map_revision = self.map_revision.wrapping_add(1);
                let header = self.header(captured_at_ms, SourceQuality::Sampled);
                events.push(HostEvent::ZoneChanged {
                    header,
                    previous_area_id: area,
                    area_id: sampled_area.clone(),
                });
            } else if !self.initialized && sampled_area.is_some() {
                self.map_revision = 1;
            }
            area = sampled_area;
        }

        if self.window_hook_available {
            if self.windows_sampled {
                if sampled_windows != windows || sampled_focused_window != focused_window {
                    self.ui_revision = self.ui_revision.wrapping_add(1);
                }
                windows = sampled_windows;
                focused_window = sampled_focused_window;
            }
            for (opened, window_id) in std::mem::take(&mut self.window_hook_edges) {
                let changed = if opened {
                    windows.insert(window_id.clone())
                } else {
                    windows.remove(&window_id)
                };
                if !changed {
                    continue;
                }
                self.ui_revision = self.ui_revision.wrapping_add(1);
                let header = self.header(captured_at_ms, SourceQuality::Observed);
                events.push(if opened {
                    HostEvent::UiWindowOpened { header, window_id }
                } else {
                    HostEvent::UiWindowClosed { header, window_id }
                });
            }
            if let Some(observed_focus) = self.window_hook_focus.take() {
                if observed_focus != focused_window {
                    self.ui_revision = self.ui_revision.wrapping_add(1);
                    focused_window = observed_focus;
                }
            }
        } else if self.windows_sampled {
            if self.initialized
                && (sampled_windows != windows || sampled_focused_window != focused_window)
            {
                self.ui_revision = self.ui_revision.wrapping_add(1);
                let opened = sampled_windows
                    .difference(&windows)
                    .cloned()
                    .collect::<Vec<_>>();
                let closed = windows
                    .difference(&sampled_windows)
                    .cloned()
                    .collect::<Vec<_>>();
                for window_id in opened {
                    let header = self.header(captured_at_ms, SourceQuality::Sampled);
                    events.push(HostEvent::UiWindowOpened { header, window_id });
                }
                for window_id in closed {
                    let header = self.header(captured_at_ms, SourceQuality::Sampled);
                    events.push(HostEvent::UiWindowClosed { header, window_id });
                }
            } else if !self.initialized && !sampled_windows.is_empty() {
                self.ui_revision = 1;
            }
            windows = sampled_windows;
            focused_window = sampled_focused_window;
        }

        let party_observation = if !self.party_provider_enabled {
            DomainObservation::Unavailable(UnavailableReason::Unsupported)
        } else if raw.adapter_status != ADAPTER_LIVE || raw.in_world == 0 {
            // Never carry a previously sampled roster across a loading or
            // disconnected state. A live party requires a live in-world
            // player object from this update.
            self.party_observation = None;
            DomainObservation::Unavailable(unavailable_reason(raw))
        } else {
            self.party_observation.take().map_or_else(
                || DomainObservation::Unavailable(unavailable_reason(raw)),
                DomainObservation::Live,
            )
        };
        let party_observation_live = matches!(&party_observation, DomainObservation::Live(_));
        let party_changed = match &party_observation {
            DomainObservation::Live(party) => {
                let roster = PartyRosterIdentity::from(party);
                let changed =
                    self.initialized && self.current_party_roster.as_ref() != Some(&roster);
                // A callback can race the worker's roster read. While hooks
                // are authoritative, do not advance the event baseline until
                // its matching dirty edge has been drained. The public
                // snapshot still receives the newest sampled roster below.
                if !self.party_hook_available
                    || self.party_hook_dirty
                    || self.current_party_roster.is_none()
                {
                    self.current_party_roster = Some(roster);
                    changed
                } else {
                    false
                }
            }
            DomainObservation::Unavailable(_) => {
                // Preserve the last validated roster across a transient read
                // or loading gap. The next live sample decides whether the
                // roster actually changed. A process boundary invalidates it.
                if raw.adapter_status != ADAPTER_LIVE {
                    self.current_party_roster = None;
                }
                false
            }
        };
        let party_change_quality = if !party_changed {
            None
        } else if self.party_hook_available {
            self.party_hook_dirty.then_some(SourceQuality::Observed)
        } else {
            Some(SourceQuality::Sampled)
        };
        if let Some(quality) = party_change_quality {
            let snapshot_will_advance = match &party_observation {
                DomainObservation::Live(party) => self.previous_party.as_ref() != Some(party),
                DomainObservation::Unavailable(_) => false,
            };
            let revision = if snapshot_will_advance {
                self.party_revision.wrapping_add(1)
            } else {
                self.party_revision
            };
            let header = self.header(captured_at_ms, quality);
            events.push(HostEvent::PartyChanged(farever_more_api::PartyEvent {
                header,
                revision,
            }));
        }
        if party_observation_live {
            self.party_hook_dirty = false;
        }

        let instance_observation =
            self.resolve_instance(raw, area.as_deref(), captured_at_ms, &mut events);

        if raw.adapter_status == ADAPTER_LIVE && raw.in_world != 0 && self.active_fight.is_none() {
            let quality = if self.direct_combat_available && self.direct_combat_started {
                Some(SourceQuality::Observed)
            } else if !self.direct_combat_available && combat_state == Some(true) {
                Some(SourceQuality::Sampled)
            } else {
                None
            };
            if let Some(quality) = quality {
                events.push(self.start_combat(captured_at_ms, quality));
            }
        }

        self.previous_area = area.clone();
        self.zone_sampled = true;
        self.previous_windows = windows.clone();
        self.previous_focused_window = focused_window.clone();
        self.windows_sampled = true;
        self.initialized = true;

        let local_party_member = match &party_observation {
            DomainObservation::Live(party) => party.members.iter().find(|member| member.is_local),
            DomainObservation::Unavailable(_) => None,
        };
        let player = if raw.player_position_valid != 0 {
            DomainObservation::Live(PlayerState {
                runtime_id: local_party_member.map(|member| member.actor_id.clone()),
                name: local_party_member.and_then(|member| member.name.clone()),
                class_id: local_party_member.and_then(|member| member.class_id.clone()),
                position: vec3(raw.player_position),
                heading_radians: (raw.player_heading_valid != 0)
                    .then_some(raw.player_heading_radians as f32),
                in_combat: combat_state,
            })
        } else {
            DomainObservation::Unavailable(unavailable_reason(raw))
        };
        let camera = if raw.camera_heading_valid != 0 {
            DomainObservation::Live(CameraState {
                heading_radians: raw.camera_heading_radians as f32,
            })
        } else {
            DomainObservation::Unavailable(unavailable_reason(raw))
        };
        let combat_references = if raw.combat_references_available != 0 {
            DomainObservation::Live(combat_references(raw))
        } else {
            DomainObservation::Unavailable(unavailable_reason(raw))
        };
        let player = domain_snapshot(
            player,
            &mut self.previous_player,
            &mut self.player_revision,
            captured_at_ms,
        );
        let camera = domain_snapshot(
            camera,
            &mut self.previous_camera,
            &mut self.camera_revision,
            captured_at_ms,
        );
        let combat_references = domain_snapshot(
            combat_references,
            &mut self.previous_combat_references,
            &mut self.combat_references_revision,
            captured_at_ms,
        );
        let party = domain_snapshot(
            party_observation,
            &mut self.previous_party,
            &mut self.party_revision,
            captured_at_ms,
        );
        let instance_session = domain_snapshot(
            instance_observation,
            &mut self.previous_instance,
            &mut self.instance_revision,
            captured_at_ms,
        );

        let first_sequence = events.first().map(event_sequence);
        let snapshot = GameSnapshot {
            sequence: raw.sequence,
            captured_at_ms,
            session: SessionState {
                process_session: self.process_session,
                adapter: if raw.adapter_status == ADAPTER_LIVE {
                    Availability::Live
                } else if raw.process_found != 0 {
                    Availability::Stale
                } else {
                    Availability::Unavailable
                },
                in_world: raw.in_world != 0,
                loading_state: (raw.loading_state >= 0).then_some(raw.loading_state),
            },
            player,
            party,
            camera,
            combat_references,
            instance_session,
            map: MapState {
                area_id: area.clone(),
                display_name: area.as_deref().map(area_display_name),
                data_revision: self.map_revision,
            },
            map_view: StateSnapshot::default(),
            ui: UiState {
                open_windows: windows.into_iter().collect(),
                focused_window,
                revision: self.ui_revision,
            },
        };
        let batch = EventBatch {
            process_session: self.process_session,
            first_sequence,
            next_sequence: self.next_event_sequence,
            dropped_before: 0,
            snapshot_required: false,
            events,
        };
        (snapshot, batch)
    }

    fn resolve_instance(
        &mut self,
        raw: &FasSnapshotV0,
        area_id: Option<&str>,
        captured_at_ms: u64,
        events: &mut Vec<HostEvent>,
    ) -> DomainObservation<InstanceState> {
        if !self.instance_provider_enabled {
            return DomainObservation::Unavailable(UnavailableReason::Unsupported);
        }
        if raw.adapter_status != ADAPTER_LIVE {
            self.instance_observation = None;
            self.instance_hook_observations.clear();
            self.current_instance_key = None;
            self.current_instance = None;
            return DomainObservation::Unavailable(unavailable_reason(raw));
        }
        if raw.in_world == 0 {
            // Loading is not an instance boundary. Preserve the private key
            // until a validated destination proves whether this was an
            // intra-instance load or an exit to another zone.
            self.instance_observation = None;
            self.instance_hook_observations.clear();
            return DomainObservation::Unavailable(unavailable_reason(raw));
        }

        if self.instance_hook_available {
            let hook_observations = std::mem::take(&mut self.instance_hook_observations);
            if hook_observations.is_empty() {
                // A sample is supplied only for startup, world re-entry, hook
                // loss, or explicit reconciliation. Never combine it with an
                // observed edge from the same tick: a concurrent sample may
                // describe the state immediately before that setter call.
                if let Some(observation) = self.instance_observation.take() {
                    self.apply_instance_observation(
                        observation,
                        area_id,
                        captured_at_ms,
                        SourceQuality::Derived,
                        events,
                    );
                }
            } else {
                self.instance_observation = None;
                for observation in hook_observations {
                    self.apply_instance_observation(
                        observation,
                        area_id,
                        captured_at_ms,
                        SourceQuality::Observed,
                        events,
                    );
                }
            }
        } else {
            self.instance_hook_observations.clear();
            let Some(observation) = self.instance_observation.take() else {
                // A transient provider read failure must not synthesize an
                // instance exit. Preserve the tracked identity for the next
                // valid sample.
                return DomainObservation::Unavailable(UnavailableReason::ProviderFailed);
            };
            self.apply_instance_observation(
                observation,
                area_id,
                captured_at_ms,
                SourceQuality::Derived,
                events,
            );
        }

        self.current_instance.clone().map_or_else(
            || DomainObservation::Unavailable(UnavailableReason::ProviderFailed),
            DomainObservation::Live,
        )
    }

    fn apply_instance_observation(
        &mut self,
        observation: InstanceObservation,
        area_id: Option<&str>,
        captured_at_ms: u64,
        quality: SourceQuality,
        events: &mut Vec<HostEvent>,
    ) {
        if self.current_instance_key.as_deref() == Some(&observation.key) {
            return;
        }
        let previous = self.current_instance.clone();
        let session_id = self.next_instance_id;
        self.next_instance_id = self.next_instance_id.wrapping_add(1).max(1);
        self.current_instance_key = Some(observation.key);
        self.current_instance = Some(InstanceState {
            session_id,
            kind: observation.kind,
            area_id: area_id.map(ToOwned::to_owned),
        });
        if self.initialized {
            if let Some(event) = self.end_combat(captured_at_ms, SourceQuality::Derived) {
                events.push(event);
            }
            let header = self.header(captured_at_ms, quality);
            events.push(HostEvent::InstanceChanged(
                farever_more_api::InstanceEvent {
                    header,
                    previous,
                    current: self.current_instance.clone(),
                },
            ));
        }
    }

    /// Adds normalized outgoing damage after ensuring that every hit belongs
    /// to an open host-owned fight. When Farever's combat flag is unavailable,
    /// the first positive observed hit starts a derived fallback fight.
    pub fn append_damage(&mut self, batch: &mut EventBatch, events: Vec<HostEvent>) {
        let observed_at_ms = self.started.elapsed().as_millis() as u64;
        self.append_damage_at(batch, events, observed_at_ms);
    }

    fn append_damage_at(
        &mut self,
        batch: &mut EventBatch,
        events: Vec<HostEvent>,
        observed_at_ms: u64,
    ) {
        let has_damage = events
            .iter()
            .any(|event| matches!(event, HostEvent::Damage(_)));
        if has_damage {
            if self.active_fight.is_none() {
                let event = self.start_combat(observed_at_ms, SourceQuality::Derived);
                self.push_event(batch, event);
            }
            self.last_damage_ms = Some(observed_at_ms);
            self.sampled_idle_since_ms = None;
            self.damage_this_update = true;
        }
        self.append(batch, events);
    }

    /// Closes a fight after all damage for this update has been ordered.
    ///
    /// A sampled `false` flag proposes an end rather than closing immediately:
    /// validated empty combat references must remain idle for five seconds.
    /// Occupied or unavailable references retain the fight until the
    /// 60-second no-damage safety fallback. Leaving the world closes
    /// immediately.
    pub fn finish_update(
        &mut self,
        batch: &mut EventBatch,
        raw: &FasSnapshotV0,
        combat_state: Option<bool>,
    ) {
        let now_ms = self.started.elapsed().as_millis() as u64;
        self.finish_update_at(batch, raw, combat_state, now_ms);
    }

    fn finish_update_at(
        &mut self,
        batch: &mut EventBatch,
        raw: &FasSnapshotV0,
        combat_state: Option<bool>,
        now_ms: u64,
    ) {
        let end_quality = if raw.in_world == 0 {
            Some(SourceQuality::Derived)
        } else if self.direct_combat_available
            && self.direct_combat_end_pending
            && !self.damage_this_update
        {
            Some(SourceQuality::Observed)
        } else if !self.direct_combat_available {
            self.sampled_end_quality(raw, combat_state, now_ms)
        } else {
            None
        };

        if let Some(quality) = end_quality {
            if let Some(event) = self.end_combat(now_ms, quality) {
                self.push_event(batch, event);
            }
            self.direct_combat_end_pending = false;
        }
        self.direct_combat_started = false;
        batch.next_sequence = self.next_event_sequence;
    }

    fn sampled_end_quality(
        &mut self,
        raw: &FasSnapshotV0,
        combat_state: Option<bool>,
        now_ms: u64,
    ) -> Option<SourceQuality> {
        if self.active_fight.is_none() {
            self.sampled_idle_since_ms = None;
            return None;
        }
        if combat_state == Some(true) || self.damage_this_update {
            self.sampled_idle_since_ms = None;
            return None;
        }

        match (combat_state, combat_references_occupied(raw)) {
            (Some(false), Some(false)) => {
                let idle_since_ms = *self.sampled_idle_since_ms.get_or_insert(now_ms);
                (now_ms.saturating_sub(idle_since_ms)
                    >= SAMPLED_COMBAT_END_GRACE.as_millis() as u64)
                    .then_some(SourceQuality::Sampled)
            }
            (Some(false), Some(true)) => {
                self.sampled_idle_since_ms = None;
                self.fallback_idle_elapsed(now_ms)
                    .then_some(SourceQuality::Derived)
            }
            _ => {
                self.sampled_idle_since_ms = None;
                self.fallback_idle_elapsed(now_ms)
                    .then_some(SourceQuality::Derived)
            }
        }
    }

    pub fn append(&mut self, batch: &mut EventBatch, mut events: Vec<HostEvent>) {
        for mut event in events.drain(..) {
            set_event_header(
                &mut event,
                self.header(
                    self.started.elapsed().as_millis() as u64,
                    SourceQuality::Observed,
                ),
            );
            self.push_event(batch, event);
        }
        batch.next_sequence = self.next_event_sequence;
    }

    pub fn note_dropped(&mut self, batch: &mut EventBatch, count: u64) {
        if count == 0 {
            return;
        }
        self.next_event_sequence = self.next_event_sequence.wrapping_add(count);
        batch.dropped_before = batch.dropped_before.saturating_add(count);
        batch.snapshot_required = true;
        batch.next_sequence = self.next_event_sequence;
    }

    fn header(&mut self, monotonic_ms: u64, quality: SourceQuality) -> EventHeader {
        let sequence = self.next_event_sequence;
        self.next_event_sequence = self.next_event_sequence.wrapping_add(1);
        EventHeader {
            sequence,
            monotonic_ms,
            quality,
        }
    }

    fn start_combat(&mut self, monotonic_ms: u64, quality: SourceQuality) -> HostEvent {
        let fight_id = self.next_fight_id;
        self.next_fight_id = self.next_fight_id.wrapping_add(1).max(1);
        self.active_fight = Some(fight_id);
        self.fight_started_ms = Some(monotonic_ms);
        self.last_damage_ms = None;
        self.sampled_idle_since_ms = None;
        HostEvent::CombatStarted(CombatEvent {
            header: self.header(monotonic_ms, quality),
            fight_id,
        })
    }

    fn end_combat(&mut self, monotonic_ms: u64, quality: SourceQuality) -> Option<HostEvent> {
        let fight_id = self.active_fight.take()?;
        self.fight_started_ms = None;
        self.last_damage_ms = None;
        self.sampled_idle_since_ms = None;
        Some(HostEvent::CombatEnded(CombatEvent {
            header: self.header(monotonic_ms, quality),
            fight_id,
        }))
    }

    fn fallback_idle_elapsed(&self, now_ms: u64) -> bool {
        let activity_ms = self.last_damage_ms.or(self.fight_started_ms);
        activity_ms.is_some_and(|activity_ms| {
            now_ms.saturating_sub(activity_ms) >= FALLBACK_COMBAT_IDLE.as_millis() as u64
        })
    }

    fn push_event(&mut self, batch: &mut EventBatch, event: HostEvent) {
        if batch.events.len() < MAX_EVENT_BATCH {
            if batch.first_sequence.is_none() {
                batch.first_sequence = Some(event_sequence(&event));
            }
            batch.events.push(event);
        } else {
            batch.dropped_before = batch.dropped_before.saturating_add(1);
            batch.snapshot_required = true;
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum DomainObservation<T> {
    Live(T),
    Unavailable(UnavailableReason),
}

fn domain_snapshot<T: Clone + PartialEq>(
    current: DomainObservation<T>,
    previous: &mut Option<T>,
    revision: &mut u64,
    observed_at_ms: u64,
) -> StateSnapshot<T> {
    match current {
        DomainObservation::Live(value) => {
            if previous.as_ref() != Some(&value) {
                *revision = revision.wrapping_add(1);
                *previous = Some(value.clone());
            }
            StateSnapshot::live(observed_at_ms, *revision, value)
        }
        DomainObservation::Unavailable(reason) => StateSnapshot::unavailable(*revision, reason),
    }
}

fn unavailable_reason(raw: &FasSnapshotV0) -> UnavailableReason {
    if raw.adapter_status != ADAPTER_LIVE {
        UnavailableReason::NotYetObserved
    } else if raw.in_world == 0 && raw.loading_state >= 0 {
        UnavailableReason::Loading
    } else if raw.in_world == 0 {
        UnavailableReason::NotInWorld
    } else {
        UnavailableReason::ProviderFailed
    }
}

fn vec3([x, y, z]: [f64; 3]) -> Vec3 {
    Vec3 {
        x: x as f32,
        y: y as f32,
        z: z as f32,
    }
}

fn combat_references(raw: &FasSnapshotV0) -> CombatReferencesState {
    let slots = [
        (
            COMBAT_REFERENCE_TARGET,
            0_usize,
            CombatReferenceSlot::Target,
        ),
        (
            COMBAT_REFERENCE_LOCKED_TARGET,
            1_usize,
            CombatReferenceSlot::LockedTarget,
        ),
        (
            COMBAT_REFERENCE_AUTO_TARGET,
            2_usize,
            CombatReferenceSlot::AutoTarget,
        ),
    ];
    let references = slots
        .into_iter()
        .filter(|(mask, _, _)| raw.combat_reference_active_mask & mask != 0)
        .map(|(mask, index, slot)| CombatReference {
            slot,
            position: (raw.combat_reference_position_mask & mask != 0)
                .then(|| vec3(raw.combat_reference_positions[index])),
        })
        .collect();
    CombatReferencesState { references }
}

fn event_sequence(event: &HostEvent) -> u64 {
    match event {
        HostEvent::Damage(event) => event.header.sequence,
        HostEvent::CombatStarted(event) | HostEvent::CombatEnded(event) => event.header.sequence,
        HostEvent::PartyChanged(event) => event.header.sequence,
        HostEvent::InstanceChanged(event) => event.header.sequence,
        HostEvent::ZoneChanged { header, .. }
        | HostEvent::UiWindowOpened { header, .. }
        | HostEvent::UiWindowClosed { header, .. } => header.sequence,
        HostEvent::PlayerDisconnected(event) => event.header.sequence,
    }
}

fn set_event_header(event: &mut HostEvent, header: EventHeader) {
    match event {
        HostEvent::Damage(event) => event.header = header,
        HostEvent::CombatStarted(event) | HostEvent::CombatEnded(event) => event.header = header,
        HostEvent::PartyChanged(event) => event.header = header,
        HostEvent::InstanceChanged(event) => event.header = header,
        HostEvent::ZoneChanged {
            header: current, ..
        }
        | HostEvent::UiWindowOpened {
            header: current, ..
        }
        | HostEvent::UiWindowClosed {
            header: current, ..
        } => *current = header,
        HostEvent::PlayerDisconnected(event) => event.header = header,
    }
}

fn area_display_name(area: &str) -> String {
    area.rsplit(['/', '\\'])
        .next()
        .unwrap_or(area)
        .replace('_', " ")
}

fn combat_references_occupied(raw: &FasSnapshotV0) -> Option<bool> {
    (raw.combat_references_available != 0).then(|| {
        let known_slots =
            COMBAT_REFERENCE_TARGET | COMBAT_REFERENCE_LOCKED_TARGET | COMBAT_REFERENCE_AUTO_TARGET;
        raw.combat_reference_active_mask & known_slots != 0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use farever_more_api::{ActorRelation, CombatActorRef, ImageRef, InstanceKind, PartyMember};

    fn live_raw(area: &str) -> FasSnapshotV0 {
        let mut raw = FasSnapshotV0 {
            adapter_status: ADAPTER_LIVE,
            in_world: 1,
            player_position_valid: 1,
            ..FasSnapshotV0::default()
        };
        raw.set_area(area);
        raw
    }

    fn party(members: &[(&str, bool, &str)]) -> PartyState {
        PartyState {
            party_id: Some("party-1".to_owned()),
            members: members
                .iter()
                .map(|(actor_id, is_local, name)| PartyMember {
                    actor_id: (*actor_id).to_owned(),
                    is_local: *is_local,
                    name: Some((*name).to_owned()),
                    class_id: Some("Warrior".to_owned()),
                    class_icon: None,
                    in_combat: Some(false),
                })
                .collect(),
        }
    }

    #[test]
    fn unimplemented_group_domains_report_unsupported_instead_of_empty_state() {
        let mut tracker = EventTracker::new();
        let (snapshot, _) = tracker.update(&FasSnapshotV0::default(), None);

        assert_eq!(
            snapshot.party.unavailable_reason,
            Some(UnavailableReason::Unsupported)
        );
        assert!(snapshot.party.value.is_none());
        assert_eq!(
            snapshot.instance_session.unavailable_reason,
            Some(UnavailableReason::Unsupported)
        );
        assert!(snapshot.instance_session.value.is_none());
    }

    #[test]
    fn party_provider_populates_local_identity_and_emits_roster_revisions() {
        let mut tracker = EventTracker::new();
        let raw = live_raw("World/W1_Siagarta");
        tracker.observe_party(Some(party(&[("actor-1", true, "Local")])));
        tracker.observe_instance(Some(InstanceObservation {
            key: "open-world".to_owned(),
            kind: InstanceKind::OpenWorld,
        }));
        let (first, first_batch) = tracker.update(&raw, None);
        assert!(first_batch.events.is_empty());
        assert_eq!(first.party.availability, Availability::Live);
        assert_eq!(first.party.revision, 1);
        assert_eq!(
            first
                .player
                .value
                .as_ref()
                .and_then(|player| player.runtime_id.as_deref()),
            Some("actor-1")
        );
        assert_eq!(
            first
                .player
                .value
                .as_ref()
                .and_then(|player| player.name.as_deref()),
            Some("Local")
        );

        tracker.observe_party(Some(party(&[("actor-1", true, "Local")])));
        tracker.observe_instance(Some(InstanceObservation {
            key: "open-world".to_owned(),
            kind: InstanceKind::OpenWorld,
        }));
        let (unchanged, unchanged_batch) = tracker.update(&raw, None);
        assert_eq!(unchanged.party.revision, 1);
        assert!(unchanged_batch.events.is_empty());

        tracker.observe_party(Some(party(&[
            ("actor-1", true, "Local"),
            ("actor-2", false, "Remote"),
        ])));
        tracker.observe_instance(Some(InstanceObservation {
            key: "open-world".to_owned(),
            kind: InstanceKind::OpenWorld,
        }));
        let (changed, changed_batch) = tracker.update(&raw, None);
        assert_eq!(changed.party.revision, 2);
        assert!(matches!(
            changed_batch.events.as_slice(),
            [HostEvent::PartyChanged(farever_more_api::PartyEvent {
                revision: 2,
                ..
            })]
        ));
    }

    #[test]
    fn hooked_party_dirty_edge_makes_roster_change_observed() {
        let mut tracker = EventTracker::new();
        let raw = live_raw("World/W1_Siagarta");
        tracker.observe_party_hook(true, false);
        tracker.observe_party(Some(party(&[("actor-1", true, "Local")])));
        let (_, baseline) = tracker.update(&raw, None);
        assert!(baseline.events.is_empty());

        tracker.observe_party_hook(true, true);
        tracker.observe_party(Some(party(&[
            ("actor-1", true, "Local"),
            ("actor-2", false, "Remote"),
        ])));
        let (_, changed) = tracker.update(&raw, None);
        assert!(matches!(
            changed.events.as_slice(),
            [HostEvent::PartyChanged(farever_more_api::PartyEvent {
                header,
                revision: 2,
            })] if header.quality == SourceQuality::Observed
        ));
    }

    #[test]
    fn hooked_party_change_waits_for_its_dirty_edge() {
        let mut tracker = EventTracker::new();
        let raw = live_raw("World/W1_Siagarta");
        tracker.observe_party_hook(true, false);
        tracker.observe_party(Some(party(&[("actor-1", true, "Local")])));
        let (_, baseline) = tracker.update(&raw, None);
        assert!(baseline.events.is_empty());

        tracker.observe_party_hook(true, false);
        tracker.observe_party(Some(party(&[
            ("actor-1", true, "Local"),
            ("actor-2", false, "Remote"),
        ])));
        let (early_snapshot, early) = tracker.update(&raw, None);
        assert_eq!(
            early_snapshot.party.value.as_ref().unwrap().members.len(),
            2
        );
        assert!(early.events.is_empty());

        tracker.observe_party_hook(true, true);
        tracker.observe_party(Some(party(&[
            ("actor-1", true, "Local"),
            ("actor-2", false, "Remote"),
        ])));
        let (_, changed) = tracker.update(&raw, None);
        assert!(matches!(
            changed.events.as_slice(),
            [HostEvent::PartyChanged(farever_more_api::PartyEvent {
                header,
                revision: 2,
            })] if header.quality == SourceQuality::Observed
        ));
    }

    #[test]
    fn direct_zone_edges_are_observed_and_reconciliation_is_silent() {
        let mut tracker = EventTracker::new();
        let initial = live_raw("World/Town");
        tracker.observe_zone_hook(true, true, Vec::new());
        let (baseline, baseline_batch) = tracker.update(&initial, None);
        assert!(baseline_batch.events.is_empty());
        assert_eq!(baseline.map.area_id.as_deref(), Some("World/Town"));

        tracker.observe_zone_hook(true, false, vec![Some("World/Rift".to_owned())]);
        let (changed, changed_batch) = tracker.update(&initial, None);
        assert_eq!(changed.map.area_id.as_deref(), Some("World/Rift"));
        assert!(matches!(
            changed_batch.events.as_slice(),
            [HostEvent::ZoneChanged {
                header,
                previous_area_id: Some(previous),
                area_id: Some(current),
            }] if header.quality == SourceQuality::Observed
                && previous == "World/Town"
                && current == "World/Rift"
        ));

        let reconciled = live_raw("World/Recovered");
        tracker.observe_zone_hook(true, true, Vec::new());
        let (snapshot, batch) = tracker.update(&reconciled, None);
        assert_eq!(snapshot.map.area_id.as_deref(), Some("World/Recovered"));
        assert!(batch.events.is_empty());
    }

    #[test]
    fn direct_window_edges_are_observed_and_supersede_unsampled_memory() {
        let mut tracker = EventTracker::new();
        let mut initial = live_raw("World/W1_Siagarta");
        initial.push_window("ui.win.GameMenu");
        tracker.observe_window_hook(
            true,
            true,
            Some(Some("ui.win.GameMenu".to_owned())),
            Vec::new(),
        );
        let (baseline, baseline_batch) = tracker.update(&initial, None);
        assert!(baseline_batch.events.is_empty());
        assert_eq!(baseline.ui.open_windows, ["ui.win.GameMenu".to_owned()]);

        let raw_without_a_window_sample = live_raw("World/W1_Siagarta");
        tracker.observe_window_hook(
            true,
            false,
            Some(Some("ui.win.Inventory".to_owned())),
            vec![(true, "ui.win.Inventory".to_owned())],
        );
        let (opened, opened_batch) = tracker.update(&raw_without_a_window_sample, None);
        assert_eq!(
            opened.ui.open_windows,
            ["ui.win.GameMenu".to_owned(), "ui.win.Inventory".to_owned()]
        );
        assert_eq!(
            opened.ui.focused_window.as_deref(),
            Some("ui.win.Inventory")
        );
        assert!(matches!(
            opened_batch.events.as_slice(),
            [HostEvent::UiWindowOpened { header, window_id }]
                if header.quality == SourceQuality::Observed
                    && window_id == "ui.win.Inventory"
        ));

        tracker.observe_window_hook(
            true,
            false,
            Some(Some("ui.win.Inventory".to_owned())),
            vec![(false, "ui.win.GameMenu".to_owned())],
        );
        let (closed, closed_batch) = tracker.update(&raw_without_a_window_sample, None);
        assert_eq!(closed.ui.open_windows, ["ui.win.Inventory".to_owned()]);
        assert!(matches!(
            closed_batch.events.as_slice(),
            [HostEvent::UiWindowClosed { header, window_id }]
                if header.quality == SourceQuality::Observed
                    && window_id == "ui.win.GameMenu"
        ));
    }

    #[test]
    fn direct_window_reconciliation_corrects_state_without_inventing_events() {
        let mut tracker = EventTracker::new();
        let mut initial = live_raw("World/W1_Siagarta");
        initial.push_window("ui.win.GameMenu");
        tracker.observe_window_hook(
            true,
            true,
            Some(Some("ui.win.GameMenu".to_owned())),
            Vec::new(),
        );
        let (baseline, _) = tracker.update(&initial, None);

        let mut reconciled = live_raw("World/W1_Siagarta");
        reconciled.push_window("ui.win.Inventory");
        tracker.observe_window_hook(
            true,
            true,
            Some(Some("ui.win.Inventory".to_owned())),
            Vec::new(),
        );
        let (snapshot, batch) = tracker.update(&reconciled, None);

        assert!(batch.events.is_empty());
        assert_eq!(snapshot.ui.open_windows, ["ui.win.Inventory".to_owned()]);
        assert_eq!(snapshot.ui.revision, baseline.ui.revision + 1);
    }

    #[test]
    fn party_metadata_changes_advance_snapshots_without_roster_events() {
        let mut tracker = EventTracker::new();
        let raw = live_raw("World/W1_Siagarta");
        let mut initial = party(&[("actor-1", true, "Local"), ("actor-2", false, "Remote")]);
        initial.members[1].name = None;
        initial.members[1].class_id = None;
        initial.members[1].in_combat = None;

        tracker.observe_party(Some(initial.clone()));
        let (first, first_batch) = tracker.update(&raw, None);
        assert_eq!(first.party.revision, 1);
        assert!(first_batch.events.is_empty());

        let mut combat_started = initial.clone();
        combat_started.members[0].in_combat = Some(true);
        tracker.observe_party(Some(combat_started.clone()));
        let (combat, combat_batch) = tracker.update(&raw, None);
        assert_eq!(combat.party.revision, 2);
        assert!(!combat_batch
            .events
            .iter()
            .any(|event| matches!(event, HostEvent::PartyChanged(_))));

        let mut remote_streamed = combat_started;
        remote_streamed.members[1].name = Some("Remote".to_owned());
        remote_streamed.members[1].class_id = Some("Mage".to_owned());
        remote_streamed.members[1].class_icon = Some(ImageRef {
            id: "class:mage".to_owned(),
        });
        remote_streamed.members[1].in_combat = Some(false);
        tracker.observe_party(Some(remote_streamed.clone()));
        let (streamed, streamed_batch) = tracker.update(&raw, None);
        assert_eq!(streamed.party.revision, 3);
        assert!(!streamed_batch
            .events
            .iter()
            .any(|event| matches!(event, HostEvent::PartyChanged(_))));

        remote_streamed.members[1].name = None;
        remote_streamed.members[1].class_id = None;
        remote_streamed.members[1].class_icon = None;
        remote_streamed.members[1].in_combat = None;
        tracker.observe_party(Some(remote_streamed));
        let (unstreamed, unstreamed_batch) = tracker.update(&raw, None);
        assert_eq!(unstreamed.party.revision, 4);
        assert!(!unstreamed_batch
            .events
            .iter()
            .any(|event| matches!(event, HostEvent::PartyChanged(_))));
    }

    #[test]
    fn party_changed_uses_canonical_roster_identity() {
        let mut tracker = EventTracker::new();
        let raw = live_raw("World/W1_Siagarta");
        let initial = party(&[("actor-1", true, "Local"), ("actor-2", false, "Remote")]);
        tracker.observe_party(Some(initial.clone()));
        let (first, first_batch) = tracker.update(&raw, None);
        assert_eq!(first.party.revision, 1);
        assert!(first_batch.events.is_empty());

        let mut reordered = initial;
        reordered.members.reverse();
        tracker.observe_party(Some(reordered.clone()));
        let (order_changed, order_batch) = tracker.update(&raw, None);
        assert_eq!(order_changed.party.revision, 2);
        assert!(!order_batch
            .events
            .iter()
            .any(|event| matches!(event, HostEvent::PartyChanged(_))));

        reordered.members[0].actor_id = "actor-3".to_owned();
        tracker.observe_party(Some(reordered.clone()));
        let (member_changed, member_batch) = tracker.update(&raw, None);
        assert_eq!(member_changed.party.revision, 3);
        assert!(matches!(
            member_batch.events.as_slice(),
            [HostEvent::PartyChanged(farever_more_api::PartyEvent {
                revision: 3,
                ..
            })]
        ));

        reordered.party_id = Some("party-2".to_owned());
        tracker.observe_party(Some(reordered.clone()));
        let (party_replaced, replacement_batch) = tracker.update(&raw, None);
        assert_eq!(party_replaced.party.revision, 4);
        assert!(matches!(
            replacement_batch.events.as_slice(),
            [HostEvent::PartyChanged(farever_more_api::PartyEvent {
                revision: 4,
                ..
            })]
        ));

        reordered.members.pop();
        tracker.observe_party(Some(reordered));
        let (member_left, leave_batch) = tracker.update(&raw, None);
        assert_eq!(member_left.party.revision, 5);
        assert!(matches!(
            leave_batch.events.as_slice(),
            [HostEvent::PartyChanged(farever_more_api::PartyEvent {
                revision: 5,
                ..
            })]
        ));
    }

    #[test]
    fn party_sample_is_not_published_outside_the_world() {
        let mut tracker = EventTracker::new();
        let mut raw = live_raw("World/W1_Siagarta");
        raw.in_world = 0;
        tracker.observe_party(Some(party(&[("actor-1", true, "Local")])));

        let (snapshot, _) = tracker.update(&raw, None);

        assert_eq!(snapshot.party.availability, Availability::Unavailable);
        assert_eq!(
            snapshot.party.unavailable_reason,
            Some(UnavailableReason::NotInWorld)
        );
        assert!(snapshot.party.value.is_none());
        assert!(snapshot
            .player
            .value
            .as_ref()
            .and_then(|player| player.runtime_id.as_ref())
            .is_none());
    }

    #[test]
    fn transient_party_failure_does_not_invent_a_roster_change() {
        let mut tracker = EventTracker::new();
        let raw = live_raw("World/W1_Siagarta");
        let roster = party(&[("actor-1", true, "Local")]);
        tracker.observe_party(Some(roster.clone()));
        let (first, first_batch) = tracker.update(&raw, None);
        assert_eq!(first.party.revision, 1);
        assert!(first_batch.events.is_empty());

        tracker.observe_party(None);
        let (failed, failed_batch) = tracker.update(&raw, None);
        assert_eq!(
            failed.party.unavailable_reason,
            Some(UnavailableReason::ProviderFailed)
        );
        assert_eq!(failed.party.revision, 1);
        assert!(failed_batch.events.is_empty());

        tracker.observe_party(Some(roster));
        let (recovered, recovered_batch) = tracker.update(&raw, None);
        assert_eq!(recovered.party.revision, 1);
        assert!(!recovered_batch
            .events
            .iter()
            .any(|event| matches!(event, HostEvent::PartyChanged(_))));

        tracker.observe_party(Some(party(&[
            ("actor-1", true, "Local"),
            ("actor-2", false, "Remote"),
        ])));
        let (changed, changed_batch) = tracker.update(&raw, None);
        assert_eq!(changed.party.revision, 2);
        assert!(matches!(
            changed_batch.events.as_slice(),
            [HostEvent::PartyChanged(farever_more_api::PartyEvent {
                revision: 2,
                ..
            })]
        ));
    }

    #[test]
    fn dungeon_session_survives_area_changes_and_reentry_gets_a_new_id() {
        let mut tracker = EventTracker::new();
        let mut raw = live_raw("World/Town");
        tracker.observe_party(Some(party(&[("actor-1", true, "Local")])));
        tracker.observe_instance(Some(InstanceObservation {
            key: "open-world".to_owned(),
            kind: InstanceKind::OpenWorld,
        }));
        let (open_world, _) = tracker.update(&raw, None);
        assert_eq!(
            open_world
                .instance_session
                .value
                .as_ref()
                .map(|instance| instance.session_id),
            Some(1)
        );

        raw.set_area("World/DungeonRoom1");
        tracker.observe_party(Some(party(&[("actor-1", true, "Local")])));
        tracker.observe_instance(Some(InstanceObservation {
            key: "dungeon:Ruins".to_owned(),
            kind: InstanceKind::Dungeon,
        }));
        let (dungeon, entered) = tracker.update(&raw, None);
        let first_dungeon_id = dungeon
            .instance_session
            .value
            .as_ref()
            .expect("dungeon session")
            .session_id;
        assert_eq!(first_dungeon_id, 2);
        assert!(entered.events.iter().any(|event| matches!(
            event,
            HostEvent::InstanceChanged(farever_more_api::InstanceEvent {
                current: Some(InstanceState {
                    kind: InstanceKind::Dungeon,
                    ..
                }),
                ..
            })
        )));

        raw.in_world = 0;
        raw.loading_state = 1;
        tracker.observe_party(None);
        tracker.observe_instance(None);
        let (loading, loading_batch) = tracker.update(&raw, None);
        assert_eq!(
            loading.instance_session.unavailable_reason,
            Some(UnavailableReason::Loading)
        );
        assert!(!loading_batch
            .events
            .iter()
            .any(|event| matches!(event, HostEvent::InstanceChanged(_))));

        raw.in_world = 1;
        raw.loading_state = -1;
        raw.set_area("World/DungeonRoom2");
        tracker.observe_party(Some(party(&[("actor-1", true, "Local")])));
        tracker.observe_instance(Some(InstanceObservation {
            key: "dungeon:Ruins".to_owned(),
            kind: InstanceKind::Dungeon,
        }));
        let (next_room, room_batch) = tracker.update(&raw, None);
        assert_eq!(
            next_room
                .instance_session
                .value
                .as_ref()
                .map(|instance| instance.session_id),
            Some(first_dungeon_id)
        );
        assert!(!room_batch
            .events
            .iter()
            .any(|event| matches!(event, HostEvent::InstanceChanged(_))));
        assert!(!room_batch
            .events
            .iter()
            .any(|event| matches!(event, HostEvent::PartyChanged(_))));

        tracker.observe_party(Some(party(&[("actor-1", true, "Local")])));
        tracker.observe_instance(None);
        let (failed, failed_batch) = tracker.update(&raw, None);
        assert_eq!(
            failed.instance_session.unavailable_reason,
            Some(UnavailableReason::ProviderFailed)
        );
        assert!(!failed_batch
            .events
            .iter()
            .any(|event| matches!(event, HostEvent::InstanceChanged(_))));

        raw.set_area("World/Town");
        tracker.observe_party(Some(party(&[("actor-1", true, "Local")])));
        tracker.observe_instance(Some(InstanceObservation {
            key: "open-world".to_owned(),
            kind: InstanceKind::OpenWorld,
        }));
        let (_, exited) = tracker.update(&raw, None);
        assert!(exited
            .events
            .iter()
            .any(|event| matches!(event, HostEvent::InstanceChanged(_))));

        raw.set_area("World/DungeonRoom1");
        tracker.observe_party(Some(party(&[("actor-1", true, "Local")])));
        tracker.observe_instance(Some(InstanceObservation {
            key: "dungeon:Ruins".to_owned(),
            kind: InstanceKind::Dungeon,
        }));
        let (reentered, _) = tracker.update(&raw, None);
        assert_eq!(
            reentered
                .instance_session
                .value
                .as_ref()
                .map(|instance| instance.session_id),
            Some(4)
        );
    }

    #[test]
    fn activity_hook_drives_instance_edges_without_repeated_samples() {
        let mut tracker = EventTracker::new();
        let mut raw = live_raw("World/Town");
        tracker.observe_instance(Some(InstanceObservation {
            key: "open-world".to_owned(),
            kind: InstanceKind::OpenWorld,
        }));
        tracker.observe_instance_hook(true, Vec::new());
        let (baseline, baseline_batch) = tracker.update(&raw, None);
        assert!(baseline_batch.events.is_empty());
        assert_eq!(
            baseline
                .instance_session
                .value
                .as_ref()
                .map(|instance| instance.session_id),
            Some(1)
        );

        raw.set_area("POI/Rifts/POI_Rift_01");
        tracker.observe_instance_hook(
            true,
            vec![InstanceObservation {
                key: "other:Rift".to_owned(),
                kind: InstanceKind::Other,
            }],
        );
        let (rift, entered) = tracker.update(&raw, None);
        assert_eq!(
            rift.instance_session
                .value
                .as_ref()
                .map(|instance| instance.session_id),
            Some(2)
        );
        assert!(matches!(
            entered.events.as_slice(),
            [
                HostEvent::ZoneChanged { .. },
                HostEvent::InstanceChanged(farever_more_api::InstanceEvent {
                    header: EventHeader {
                        quality: SourceQuality::Observed,
                        ..
                    },
                    current: Some(InstanceState {
                        kind: InstanceKind::Other,
                        ..
                    }),
                    ..
                })
            ]
        ));

        tracker.observe_instance_hook(true, Vec::new());
        let (unchanged, unchanged_batch) = tracker.update(&raw, None);
        assert_eq!(unchanged.instance_session.availability, Availability::Live);
        assert_eq!(
            unchanged
                .instance_session
                .value
                .as_ref()
                .map(|instance| instance.session_id),
            Some(2)
        );
        assert!(unchanged_batch.events.is_empty());
    }

    #[test]
    fn instance_transition_closes_an_active_fight_before_publication() {
        let mut tracker = EventTracker::new();
        let raw = live_raw("World/Town");
        tracker.observe_instance(Some(InstanceObservation {
            key: "open-world".to_owned(),
            kind: InstanceKind::OpenWorld,
        }));
        tracker.observe_instance_hook(true, Vec::new());
        let (_, baseline) = tracker.update(&raw, None);
        assert!(baseline.events.is_empty());

        tracker.observe_direct_combat(true, &[true]);
        let (_, started) = tracker.update(&raw, None);
        assert!(matches!(
            started.events.as_slice(),
            [HostEvent::CombatStarted(_)]
        ));

        tracker.observe_direct_combat(true, &[]);
        tracker.observe_instance_hook(
            true,
            vec![InstanceObservation {
                key: "dungeon:Ruins".to_owned(),
                kind: InstanceKind::Dungeon,
            }],
        );
        let (_, transitioned) = tracker.update(&raw, None);
        assert!(matches!(
            transitioned.events.as_slice(),
            [
                HostEvent::CombatEnded(CombatEvent {
                    header: EventHeader {
                        quality: SourceQuality::Derived,
                        ..
                    },
                    ..
                }),
                HostEvent::InstanceChanged(farever_more_api::InstanceEvent {
                    header: EventHeader {
                        quality: SourceQuality::Observed,
                        ..
                    },
                    ..
                })
            ]
        ));
    }

    #[test]
    fn reconciliation_after_hook_edges_does_not_suppress_observed_transition() {
        let mut tracker = EventTracker::new();
        let mut raw = live_raw("World/Town");
        tracker.observe_instance(Some(InstanceObservation {
            key: "open-world".to_owned(),
            kind: InstanceKind::OpenWorld,
        }));
        tracker.observe_instance_hook(true, Vec::new());
        let (_, _) = tracker.update(&raw, None);

        let rift = InstanceObservation {
            key: "other:Rift".to_owned(),
            kind: InstanceKind::Other,
        };
        raw.set_area("POI/Rifts/POI_Rift_01");
        tracker.observe_instance(Some(InstanceObservation {
            key: "open-world".to_owned(),
            kind: InstanceKind::OpenWorld,
        }));
        tracker.observe_instance_hook(true, vec![rift]);
        let (snapshot, batch) = tracker.update(&raw, None);
        let instance_events = batch
            .events
            .iter()
            .filter_map(|event| match event {
                HostEvent::InstanceChanged(event) => Some(event),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(instance_events.len(), 1);
        assert_eq!(instance_events[0].header.quality, SourceQuality::Observed);
        assert_eq!(
            snapshot
                .instance_session
                .value
                .as_ref()
                .map(|instance| instance.kind),
            Some(InstanceKind::Other)
        );
    }

    #[test]
    fn emits_zone_and_ui_edges_after_initial_snapshot() {
        let mut tracker = EventTracker::new();
        let mut raw = FasSnapshotV0::default();
        raw.set_area("World/W1_Siagarta");
        raw.push_window("ui.Character");
        let (_, first) = tracker.update(&raw, None);
        assert!(first.events.is_empty());

        raw.set_area("World/W2_Dungeon");
        raw.window_count = 0;
        let (snapshot, second) = tracker.update(&raw, None);
        assert_eq!(snapshot.map.display_name.as_deref(), Some("W2 Dungeon"));
        assert_eq!(second.events.len(), 2);
        assert_eq!(second.first_sequence, Some(1));
        assert_eq!(second.next_sequence, 3);
    }

    #[test]
    fn publishes_slot_tagged_spatial_state_without_selecting_a_target() {
        let mut tracker = EventTracker::new();
        let mut raw = FasSnapshotV0 {
            adapter_status: ADAPTER_LIVE,
            in_world: 1,
            player_position_valid: 1,
            player_heading_valid: 1,
            camera_heading_valid: 1,
            combat_references_available: 1,
            combat_reference_active_mask: COMBAT_REFERENCE_LOCKED_TARGET
                | COMBAT_REFERENCE_AUTO_TARGET,
            combat_reference_position_mask: COMBAT_REFERENCE_AUTO_TARGET,
            player_position: [10.0, 20.0, 3.0],
            player_heading_radians: 0.5,
            camera_heading_radians: 1.25,
            ..FasSnapshotV0::default()
        };
        raw.combat_reference_positions[2] = [30.0, 40.0, 4.0];

        let (snapshot, _) = tracker.update(&raw, Some(true));

        assert_eq!(snapshot.player.availability, Availability::Live);
        assert_eq!(
            snapshot.player.value.as_ref().map(|player| player.position),
            Some(Vec3 {
                x: 10.0,
                y: 20.0,
                z: 3.0,
            })
        );
        assert_eq!(
            snapshot
                .camera
                .value
                .as_ref()
                .map(|camera| camera.heading_radians),
            Some(1.25)
        );
        let references = &snapshot
            .combat_references
            .value
            .as_ref()
            .expect("live provider")
            .references;
        assert_eq!(references.len(), 2);
        assert_eq!(references[0].slot, CombatReferenceSlot::LockedTarget);
        assert_eq!(references[0].position, None);
        assert_eq!(references[1].slot, CombatReferenceSlot::AutoTarget);
        assert_eq!(
            references[1].position.map(|position| position.x),
            Some(30.0)
        );

        let first_revision = snapshot.combat_references.revision;
        let (unchanged, _) = tracker.update(&raw, Some(true));
        assert_eq!(unchanged.combat_references.revision, first_revision);

        raw.combat_reference_active_mask = 0;
        raw.combat_reference_position_mask = 0;
        let (empty, _) = tracker.update(&raw, Some(true));
        assert_eq!(empty.combat_references.availability, Availability::Live);
        assert!(empty
            .combat_references
            .value
            .expect("live empty state")
            .references
            .is_empty());
        assert_ne!(empty.combat_references.revision, first_revision);
    }

    #[test]
    fn distinguishes_an_empty_reference_set_from_provider_failure() {
        let mut tracker = EventTracker::new();
        let live_empty = FasSnapshotV0 {
            adapter_status: ADAPTER_LIVE,
            in_world: 1,
            combat_references_available: 1,
            ..FasSnapshotV0::default()
        };
        let (live, _) = tracker.update(&live_empty, None);
        assert_eq!(live.combat_references.availability, Availability::Live);
        assert!(live.combat_references.value.is_some());

        let unavailable = FasSnapshotV0 {
            combat_references_available: 0,
            ..live_empty
        };
        let (failed, _) = tracker.update(&unavailable, None);
        assert_eq!(
            failed.combat_references.unavailable_reason,
            Some(UnavailableReason::ProviderFailed)
        );
        assert!(failed.combat_references.value.is_none());
    }

    #[test]
    fn caps_batches_and_reports_a_gap() {
        let mut tracker = EventTracker::new();
        let mut batch = EventBatch::default();
        let events = (0..MAX_EVENT_BATCH + 3)
            .map(|_| {
                HostEvent::Damage(farever_more_api::DamageEvent {
                    header: EventHeader {
                        sequence: 0,
                        monotonic_ms: 0,
                        quality: SourceQuality::Observed,
                    },
                    source: CombatActorRef {
                        relation: ActorRelation::LocalPlayer,
                        ..CombatActorRef::default()
                    },
                    target: CombatActorRef::default(),
                    skill_id: "fixture".to_owned(),
                    skill_display_name: None,
                    skill_icon: None,
                    amount: 1.0,
                    hit_count: 1,
                    critical: false,
                    killed: false,
                    blocked: None,
                })
            })
            .collect();
        tracker.append(&mut batch, events);
        assert_eq!(batch.events.len(), MAX_EVENT_BATCH);
        assert_eq!(batch.dropped_before, 3);
        assert!(batch.snapshot_required);
        assert_eq!(batch.next_sequence, (MAX_EVENT_BATCH + 3) as u64 + 1);
    }

    #[test]
    fn sampled_combat_edges_share_a_fight_id() {
        let mut tracker = EventTracker::new();
        let mut raw = FasSnapshotV0 {
            adapter_status: ADAPTER_LIVE,
            in_world: 1,
            combat_references_available: 1,
            ..FasSnapshotV0::default()
        };
        let (_, first) = tracker.update(&raw, Some(false));
        assert!(first.events.is_empty());

        let (_, mut started) = tracker.update(&raw, Some(true));
        assert!(matches!(
            started.events.as_slice(),
            [HostEvent::CombatStarted(CombatEvent { fight_id: 1, .. })]
        ));
        tracker.finish_update_at(&mut started, &raw, Some(true), 1_000);

        let (_, mut candidate) = tracker.update(&raw, Some(false));
        tracker.finish_update_at(&mut candidate, &raw, Some(false), 2_000);
        assert!(candidate.events.is_empty());

        let (_, mut still_active) = tracker.update(&raw, Some(false));
        tracker.finish_update_at(&mut still_active, &raw, Some(false), 6_999);
        assert!(still_active.events.is_empty());

        let (_, mut ended) = tracker.update(&raw, Some(false));
        tracker.finish_update_at(&mut ended, &raw, Some(false), 7_000);
        assert!(matches!(
            ended.events.as_slice(),
            [HostEvent::CombatEnded(CombatEvent { fight_id: 1, .. })]
        ));

        raw.in_world = 0;
        let (_, mut idle) = tracker.update(&raw, None);
        tracker.finish_update(&mut idle, &raw, None);
        assert!(idle.events.is_empty());
    }

    #[test]
    fn observed_damage_starts_a_derived_fight_before_the_hit() {
        let mut tracker = EventTracker::new();
        let raw = FasSnapshotV0 {
            adapter_status: ADAPTER_LIVE,
            in_world: 1,
            ..FasSnapshotV0::default()
        };
        let (_, mut batch) = tracker.update(&raw, None);
        tracker.append_damage(
            &mut batch,
            vec![HostEvent::Damage(farever_more_api::DamageEvent {
                header: EventHeader {
                    sequence: 0,
                    monotonic_ms: 0,
                    quality: SourceQuality::Observed,
                },
                source: CombatActorRef {
                    relation: ActorRelation::LocalPlayer,
                    ..CombatActorRef::default()
                },
                target: CombatActorRef {
                    kind: Some("fixture".to_owned()),
                    ..CombatActorRef::default()
                },
                skill_id: "Mage_RayOfSpark".to_owned(),
                skill_display_name: Some("Ray of Spark".to_owned()),
                skill_icon: None,
                amount: 42.0,
                hit_count: 1,
                critical: false,
                killed: false,
                blocked: Some(0.0),
            })],
        );

        assert!(matches!(batch.events[0], HostEvent::CombatStarted(_)));
        assert!(matches!(batch.events[1], HostEvent::Damage(_)));
        assert_eq!(
            event_sequence(&batch.events[1]),
            event_sequence(&batch.events[0]) + 1
        );
    }

    #[test]
    fn sampled_idle_grace_keeps_delayed_damage_in_the_same_fight() {
        let mut tracker = EventTracker::new();
        let raw = FasSnapshotV0 {
            adapter_status: ADAPTER_LIVE,
            in_world: 1,
            combat_references_available: 1,
            ..FasSnapshotV0::default()
        };
        let (_, mut started) = tracker.update(&raw, Some(true));
        tracker.finish_update_at(&mut started, &raw, Some(true), 1_000);

        let (_, mut candidate) = tracker.update(&raw, Some(false));
        tracker.finish_update_at(&mut candidate, &raw, Some(false), 2_000);
        assert!(candidate.events.is_empty());

        let (_, mut hit_batch) = tracker.update(&raw, Some(false));
        tracker.append_damage_at(
            &mut hit_batch,
            vec![HostEvent::Damage(farever_more_api::DamageEvent {
                header: EventHeader {
                    sequence: 0,
                    monotonic_ms: 0,
                    quality: SourceQuality::Observed,
                },
                source: CombatActorRef {
                    relation: ActorRelation::LocalPlayer,
                    ..CombatActorRef::default()
                },
                target: CombatActorRef::default(),
                skill_id: "fixture".to_owned(),
                skill_display_name: None,
                skill_icon: None,
                amount: 1.0,
                hit_count: 1,
                critical: false,
                killed: false,
                blocked: None,
            })],
            2_100,
        );
        tracker.finish_update_at(&mut hit_batch, &raw, Some(false), 2_100);
        assert!(!hit_batch
            .events
            .iter()
            .any(|event| matches!(event, HostEvent::CombatEnded(_))));
        assert_eq!(tracker.active_fight, Some(1));

        let (_, mut skill_batch) = tracker.update(&raw, Some(true));
        tracker.append_damage_at(
            &mut skill_batch,
            vec![HostEvent::Damage(farever_more_api::DamageEvent {
                header: EventHeader {
                    sequence: 0,
                    monotonic_ms: 0,
                    quality: SourceQuality::Observed,
                },
                source: CombatActorRef {
                    relation: ActorRelation::LocalPlayer,
                    ..CombatActorRef::default()
                },
                target: CombatActorRef::default(),
                skill_id: "skill".to_owned(),
                skill_display_name: None,
                skill_icon: None,
                amount: 2.0,
                hit_count: 1,
                critical: false,
                killed: false,
                blocked: None,
            })],
            6_000,
        );
        tracker.finish_update_at(&mut skill_batch, &raw, Some(true), 6_000);
        assert!(skill_batch.events.iter().all(|event| !matches!(
            event,
            HostEvent::CombatStarted(_) | HostEvent::CombatEnded(_)
        )));
        assert_eq!(tracker.active_fight, Some(1));
    }

    #[test]
    fn occupied_combat_reference_defers_sampled_end_until_safety_fallback() {
        let mut tracker = EventTracker::new();
        let raw = FasSnapshotV0 {
            adapter_status: ADAPTER_LIVE,
            in_world: 1,
            combat_references_available: 1,
            combat_reference_active_mask: COMBAT_REFERENCE_LOCKED_TARGET,
            ..FasSnapshotV0::default()
        };
        let (_, mut started) = tracker.update(&raw, Some(true));
        tracker.finish_update_at(&mut started, &raw, Some(true), 1_000);
        tracker.last_damage_ms = Some(2_000);

        let (_, mut occupied) = tracker.update(&raw, Some(false));
        tracker.finish_update_at(&mut occupied, &raw, Some(false), 61_999);
        assert!(occupied.events.is_empty());

        let (_, mut safety_end) = tracker.update(&raw, Some(false));
        tracker.finish_update_at(&mut safety_end, &raw, Some(false), 62_000);
        assert!(matches!(
            safety_end.events.as_slice(),
            [HostEvent::CombatEnded(CombatEvent {
                header: EventHeader {
                    quality: SourceQuality::Derived,
                    ..
                },
                fight_id: 1,
            })]
        ));
    }

    #[test]
    fn unavailable_combat_references_use_sixty_second_fallback() {
        let mut tracker = EventTracker::new();
        let raw = FasSnapshotV0 {
            adapter_status: ADAPTER_LIVE,
            in_world: 1,
            ..FasSnapshotV0::default()
        };
        let (_, mut started) = tracker.update(&raw, Some(true));
        tracker.finish_update_at(&mut started, &raw, Some(true), 1_000);
        tracker.last_damage_ms = Some(2_000);

        let (_, mut early) = tracker.update(&raw, Some(false));
        tracker.finish_update_at(&mut early, &raw, Some(false), 61_999);
        assert!(early.events.is_empty());

        let (_, mut ended) = tracker.update(&raw, Some(false));
        tracker.finish_update_at(&mut ended, &raw, Some(false), 62_000);
        assert!(matches!(
            ended.events.as_slice(),
            [HostEvent::CombatEnded(CombatEvent {
                header: EventHeader {
                    quality: SourceQuality::Derived,
                    ..
                },
                fight_id: 1,
            })]
        ));
    }

    #[test]
    fn direct_combat_edges_supersede_the_sampled_flag_and_idle_timeout() {
        let mut tracker = EventTracker::new();
        let raw = FasSnapshotV0 {
            adapter_status: ADAPTER_LIVE,
            in_world: 1,
            ..FasSnapshotV0::default()
        };

        tracker.observe_direct_combat(true, &[true]);
        let (_, mut started) = tracker.update(&raw, Some(false));
        tracker.finish_update(&mut started, &raw, Some(false));
        assert!(matches!(
            started.events.as_slice(),
            [HostEvent::CombatStarted(CombatEvent {
                header: EventHeader {
                    quality: SourceQuality::Observed,
                    ..
                },
                ..
            })]
        ));

        tracker.observe_direct_combat(true, &[]);
        let (_, mut still_active) = tracker.update(&raw, Some(false));
        tracker.finish_update(&mut still_active, &raw, Some(false));
        assert!(still_active.events.is_empty());

        tracker.observe_direct_combat(true, &[false]);
        let (_, mut ended) = tracker.update(&raw, Some(true));
        tracker.finish_update(&mut ended, &raw, Some(true));
        assert!(matches!(
            ended.events.as_slice(),
            [HostEvent::CombatEnded(CombatEvent {
                header: EventHeader {
                    quality: SourceQuality::Observed,
                    ..
                },
                ..
            })]
        ));
    }

    #[test]
    fn direct_combat_end_waits_for_damage_from_the_same_update() {
        let mut tracker = EventTracker::new();
        let raw = FasSnapshotV0 {
            adapter_status: ADAPTER_LIVE,
            in_world: 1,
            ..FasSnapshotV0::default()
        };
        tracker.observe_direct_combat(true, &[true]);
        let (_, mut started) = tracker.update(&raw, None);
        tracker.finish_update(&mut started, &raw, None);

        tracker.observe_direct_combat(true, &[false]);
        let (_, mut hit_batch) = tracker.update(&raw, None);
        tracker.append_damage(
            &mut hit_batch,
            vec![HostEvent::Damage(farever_more_api::DamageEvent {
                header: EventHeader {
                    sequence: 0,
                    monotonic_ms: 0,
                    quality: SourceQuality::Observed,
                },
                source: CombatActorRef {
                    relation: ActorRelation::LocalPlayer,
                    ..CombatActorRef::default()
                },
                target: CombatActorRef::default(),
                skill_id: "fixture".to_owned(),
                skill_display_name: None,
                skill_icon: None,
                amount: 1.0,
                hit_count: 1,
                critical: false,
                killed: false,
                blocked: None,
            })],
        );
        tracker.finish_update(&mut hit_batch, &raw, None);
        assert!(!hit_batch
            .events
            .iter()
            .any(|event| matches!(event, HostEvent::CombatEnded(_))));

        tracker.observe_direct_combat(true, &[]);
        let (_, mut next) = tracker.update(&raw, None);
        tracker.finish_update(&mut next, &raw, None);
        assert!(matches!(
            next.events.as_slice(),
            [HostEvent::CombatEnded(_)]
        ));
    }

    #[test]
    fn fallback_idle_threshold_is_sixty_seconds() {
        let mut tracker = EventTracker::new();
        tracker.fight_started_ms = Some(1_000);
        tracker.last_damage_ms = Some(2_000);

        assert!(!tracker.fallback_idle_elapsed(61_999));
        assert!(tracker.fallback_idle_elapsed(62_000));
    }
}
