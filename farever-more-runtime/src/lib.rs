//! Trusted Farever state adapter, WebAssembly host, and overlay runtime.
//!
//! Game-memory pointers stay inside this crate. Add-ons receive copied,
//! validated values through the versioned WIT component boundary.

mod activity_hooks;
mod addon_manager;
mod chat_output;
mod combat_hooks;
mod config;
mod cooldown_hooks;
mod cpu;
mod damage;
mod diagnostics;
mod equipment_hooks;
mod events;
mod game_build;
mod hashlink;
mod healing_hooks;
mod host_settings;
mod inventory_clicks;
mod inventory_hooks;
mod kill_hooks;
mod lifecycle_hooks;
mod loot_hooks;
mod map_clicks;
mod map_view;
mod memory;
mod overlay;
mod party_hooks;
mod player_hooks;
mod plugin;
mod readiness;
mod shield_hooks;
mod skill_images;
mod slash_commands;
mod state;
mod status_hooks;
mod target_cast_hooks;
mod ui_windows;
mod weapon_hooks;

use cpu::ThreadCpuMeter;
use diagnostics::{Level, RuntimeLog};
use events::EventTracker;
use farever_db::GameInstall;
use farever_more_api::{
    FasSnapshotV0, HostEvent, ImageAsset, PlayerDisconnectReason, SurfaceAnchor, TextStyle,
    TextWidget, UiFrame, UiNode, UiSurface, Widget, ADAPTER_LIVE, ADAPTER_SEARCHING,
    ADAPTER_WAITING_TO_SCAN,
};
use skill_images::SkillImages;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub use state::TargetProcess;

const MAX_STATUS_ADDONS: usize = 5;
// Continuous player/camera state is sampled at the display cadence. Heavier
// state domains retain their own hook/reconciliation policies inside Poller.
const MEMORY_POLL_INTERVAL: Duration = Duration::from_micros(16_667);
const MAX_RUNTIME_SLEEP: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowProviderMode {
    AwaitingHook,
    DirectHook,
    SampledFallback,
}

struct WindowProviderState {
    in_process: bool,
    mode: WindowProviderMode,
    reconciliation_required: bool,
}

impl WindowProviderState {
    fn new(in_process: bool) -> Self {
        Self {
            in_process,
            mode: if in_process {
                WindowProviderMode::AwaitingHook
            } else {
                WindowProviderMode::SampledFallback
            },
            reconciliation_required: false,
        }
    }

    fn begin_update(&mut self, hook_status: usize) -> bool {
        let next_mode = if !self.in_process || hook_status == 3 {
            WindowProviderMode::SampledFallback
        } else if hook_status == 1 {
            WindowProviderMode::DirectHook
        } else {
            WindowProviderMode::AwaitingHook
        };
        let entering_direct = next_mode == WindowProviderMode::DirectHook
            && self.mode != WindowProviderMode::DirectHook;
        self.mode = next_mode;
        if entering_direct {
            self.reconciliation_required = true;
        }

        match self.mode {
            WindowProviderMode::AwaitingHook => false,
            WindowProviderMode::DirectHook => self.reconciliation_required,
            WindowProviderMode::SampledFallback => true,
        }
    }

    fn direct_is_authoritative(&self, hook_available: bool, dropped: u64) -> bool {
        self.mode == WindowProviderMode::DirectHook && hook_available && dropped == 0
    }

    fn finish_update(&mut self, windows_sampled: bool, dropped: u64) {
        if self.mode == WindowProviderMode::DirectHook && windows_sampled && dropped == 0 {
            self.reconciliation_required = false;
        }
        if dropped != 0 {
            self.reconciliation_required = true;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlayerProviderMode {
    AwaitingHook,
    DirectHook,
    SampledFallback,
}

struct PlayerProviderState {
    in_process: bool,
    mode: PlayerProviderMode,
    reconciliation_required: bool,
    observed_drops: u64,
}

impl PlayerProviderState {
    fn new(in_process: bool) -> Self {
        Self {
            in_process,
            mode: if in_process {
                PlayerProviderMode::AwaitingHook
            } else {
                PlayerProviderMode::SampledFallback
            },
            reconciliation_required: false,
            observed_drops: 0,
        }
    }

    fn begin_update(&mut self, hook_status: usize, total_drops: u64) -> bool {
        let next_mode = if !self.in_process || hook_status == 3 {
            PlayerProviderMode::SampledFallback
        } else if hook_status == 1 {
            PlayerProviderMode::DirectHook
        } else {
            PlayerProviderMode::AwaitingHook
        };
        let entering_direct = next_mode == PlayerProviderMode::DirectHook
            && self.mode != PlayerProviderMode::DirectHook;
        self.mode = next_mode;
        if entering_direct || total_drops != self.observed_drops {
            self.reconciliation_required = true;
        }

        match self.mode {
            PlayerProviderMode::AwaitingHook => false,
            PlayerProviderMode::DirectHook => self.reconciliation_required,
            PlayerProviderMode::SampledFallback => true,
        }
    }

    fn finish_update(&mut self, binding_sampled: bool, total_drops: u64) {
        if self.mode == PlayerProviderMode::DirectHook && binding_sampled {
            self.reconciliation_required = false;
        }
        self.observed_drops = total_drops;
    }

    fn direct_is_authoritative(&self, hook_available: bool) -> bool {
        self.mode == PlayerProviderMode::DirectHook
            && hook_available
            && !self.reconciliation_required
    }
}

type LifecycleProviderState = PlayerProviderState;
type CombatProviderState = PlayerProviderState;

/// Paths and process-selection policy for one runtime instance.
pub struct RuntimeConfig {
    /// Root containing `addons/` and the runtime `logs/` directory.
    pub addon_root: PathBuf,
    /// Game process whose state should be observed.
    pub target: TargetProcess,
}

/// Runs the state adapter, Wasm component host, and click-through visualizer.
///
/// This function owns the runtime loop and blocks until the overlay renderer
/// stops. The DLL host therefore calls it from a dedicated thread.
pub fn run(config: RuntimeConfig) {
    let target_description = format!("{:?}", config.target);
    let in_process = match &config.target {
        TargetProcess::Current => true,
        TargetProcess::ProcessId(pid) => *pid == std::process::id(),
        TargetProcess::Named(_) => false,
    };
    let mut diagnostics = RuntimeLog::open(&config.addon_root);
    diagnostics.info(&format!(
        "runtime started target={target_description} log_level={}",
        diagnostics.level().as_str()
    ));
    let addon_directory = config.addon_root.join("addons");
    let config_directory = config.addon_root.join("config");
    let mut host_settings = match host_settings::HostSettings::open(&config_directory) {
        Ok(settings) => Some(settings),
        Err(error) => {
            diagnostics.error(&format!("host settings unavailable error={error}"));
            None
        }
    };
    let mut slash_commands_enabled = host_settings
        .as_ref()
        .is_none_or(host_settings::HostSettings::slash_commands_enabled);
    slash_commands::set_enabled(slash_commands_enabled);
    diagnostics.info(&format!(
        "chat slash commands enabled={slash_commands_enabled}"
    ));
    let game_directory = config.addon_root.parent().map(PathBuf::from);
    let mut displayed_addons = plugin::discover_addon_infos(&addon_directory);
    diagnostics.info(&format!(
        "Wasm components discovered={}",
        displayed_addons.len()
    ));
    for addon in &displayed_addons {
        diagnostics.info(&format!(
            "Wasm component identity name={} version={}",
            addon.name,
            addon.version.as_deref().unwrap_or("unavailable")
        ));
    }
    let mut plugins = None;
    let mut manager_resource_revision = None::<u64>;
    let mut plugin_resource_revision = 0_u64;
    let mut next_plugin_instance_id = 1_u64;
    let mut awaiting_world_after_logout = false;
    let mut saw_world_exit_after_logout = false;
    let mut addon_fonts = Arc::new(Vec::new());
    let mut addon_images = Vec::new();
    let mut poller = state::Poller::new(config.target);
    let mut damage = damage::DamageCapture::new(&config.addon_root);
    let mut slash_commands = slash_commands::SlashCommandCapture::new();
    let mut map_clicks = map_clicks::MapClickCapture::new();
    let mut map_view = map_view::MapViewCapture::new();
    let mut logged_map_view = None;
    let mut next_map_view_log = Instant::now();
    let mut inventory_clicks = inventory_clicks::InventoryClickCapture::new();
    let mut chat_output_diagnostics = chat_output::ChatOutputDiagnostics::new();
    if in_process {
        let armed = damage.arm();
        diagnostics.info(&format!(
            "readiness world_probe=validated_local_hero armed={armed} injected_fallback=disabled"
        ));
        for event in damage.take_diagnostics() {
            diagnostics.info(&format!("damage {event}"));
        }
        for event in slash_commands.take_diagnostics() {
            diagnostics.info(&format!("chat {event}"));
        }
        for event in map_clicks.take_diagnostics() {
            diagnostics.info(&event);
        }
        for event in map_view.take_diagnostics() {
            diagnostics.info(&event);
        }
        for event in chat_output_diagnostics.take_diagnostics() {
            diagnostics.info(&format!("chat {event}"));
        }
        if let Some(error) = damage.startup_error() {
            diagnostics.error(&format!("add-on loading failed: {error}"));
            show_loading_error(error, &config.addon_root);
            return;
        }
    }
    let mut skill_images = game_directory.and_then(|directory| {
        let game = match GameInstall::open(directory) {
            Ok(game) => game,
            Err(error) => {
                diagnostics.warn(&format!("skill images unavailable error={error}"));
                return None;
            }
        };
        match SkillImages::open(&game) {
            Ok(images) => Some(images),
            Err(error) => {
                diagnostics.warn(&format!("skill images unavailable error={error}"));
                None
            }
        }
    });
    diagnostics.info(&format!(
        "skill catalog names={} icons={}",
        farever_db::Inventory::skills()
            .iter()
            .filter(|skill| skill.name.is_some())
            .count(),
        farever_db::Inventory::skills()
            .iter()
            .filter(|skill| skill.icon.is_some())
            .count(),
    ));
    let mut game_images = Vec::new();
    let mut renderer_images = Arc::new(Vec::new());
    let mut events = EventTracker::new();
    let mut overlay = overlay::Overlay::create("Farever add-on host");
    let mut cpu_meter = ThreadCpuMeter::new();
    diagnostics
        .info("CPU meter started source=GetThreadTimes interval_ms=1000 scope=runtime_thread");
    for (level, event) in overlay.take_diagnostics() {
        diagnostics.log(level, &format!("overlay {event}"));
    }
    let mut sequence = 0_u64;
    let mut loading_status = AddonLoadingStatus::default();
    let mut runtime_failure = None;
    let mut previous_combat_state = None::<Option<bool>>;
    let mut previous_spatial_state = None::<(u8, u8, u8, u8)>;
    let mut previous_party_provider_state = None::<String>;
    let mut previous_instance_provider_state = None::<String>;
    let mut previous_activity_hook_status = usize::MAX;
    let mut reported_activity_hook_losses = 0_u64;
    let mut reported_player_hook_drops = 0_u64;
    let mut reported_lifecycle_hook_drops = 0_u64;
    let mut reported_combat_hook_drops = 0_u64;
    let mut reported_party_hook_drops = 0_u64;
    let mut party_reconciliation_pending = false;
    let mut force_instance_reconciliation = false;
    let mut player_provider = PlayerProviderState::new(in_process);
    let mut lifecycle_provider = LifecycleProviderState::new(in_process);
    let mut combat_provider = CombatProviderState::new(in_process);
    let mut window_provider = WindowProviderState::new(in_process);
    let mut latest_raw = FasSnapshotV0::default();
    let mut latest_snapshot = farever_more_api::GameSnapshot::default();
    let mut last_map_snapshot = farever_more_api::StateSnapshot::default();
    let mut game_menu_geometry = None;
    let mut game_menu_geometry_revision = 0_u64;
    let mut next_poll = Instant::now();

    loop {
        if let Some(error) = damage.startup_error() {
            diagnostics.error(&format!("add-on loading failed: {error}"));
            runtime_failure = Some(error.to_owned());
            break;
        }
        let mut batch_for_plugins = None;
        let mut manual_logout = false;
        let poll_started = Instant::now();
        if poll_started >= next_poll {
            sequence = sequence.wrapping_add(1);
            let party_membership_dirty = party_hooks::drain_dirty();
            let party_hook_drops = party_hooks::total_drops();
            let party_hook_drop_delta = party_hook_drops.saturating_sub(reported_party_hook_drops);
            party_reconciliation_pending |= party_hook_drop_delta != 0;
            let activity_hook_status_before_poll = activity_hooks::status();
            let activity_hook_losses = activity_hooks::total_losses();
            let activity_hook_loss_delta =
                activity_hook_losses.saturating_sub(reported_activity_hook_losses);
            let sample_instance = activity_hook_status_before_poll != 1
                || latest_raw.in_world == 0
                || force_instance_reconciliation
                || activity_hook_loss_delta != 0;
            let player_hook_drops = player_hooks::total_drops();
            let player_hook_drop_delta =
                player_hook_drops.saturating_sub(reported_player_hook_drops);
            let sample_player =
                player_provider.begin_update(player_hooks::status(), player_hook_drops);
            let lifecycle_hook_drops = lifecycle_hooks::total_drops();
            let lifecycle_hook_drop_delta =
                lifecycle_hook_drops.saturating_sub(reported_lifecycle_hook_drops);
            let sample_lifecycle = lifecycle_provider
                .begin_update(lifecycle_hooks::provider_status(), lifecycle_hook_drops);
            let combat_hook_drops = combat_hooks::total_drops();
            let combat_hook_drop_delta =
                combat_hook_drops.saturating_sub(reported_combat_hook_drops);
            let sample_combat =
                combat_provider.begin_update(combat_hooks::provider_status(), combat_hook_drops);
            let sample_windows = window_provider.begin_update(ui_windows::status())
                || ui_windows::geometry_reconciliation_required();
            let mut raw = poller.poll(
                if in_process {
                    damage.world_probe()
                } else {
                    None
                },
                sample_lifecycle,
                sample_player,
                sample_combat,
                sample_instance,
                sample_windows,
            );
            player_provider.finish_update(poller.local_binding_sampled(), player_hook_drops);
            lifecycle_provider.finish_update(poller.lifecycle_sampled(), lifecycle_hook_drops);
            combat_provider.finish_update(poller.combat_provider_sampled(), combat_hook_drops);
            if player_hook_drop_delta != 0 {
                diagnostics.warn(&format!(
                    "local Player hooks dropped_records={player_hook_drop_delta}; scheduled binding reconciliation"
                ));
                reported_player_hook_drops = player_hook_drops;
            }
            if lifecycle_hook_drop_delta != 0 {
                diagnostics.warn(&format!(
                    "lifecycle hooks dropped_records={lifecycle_hook_drop_delta}; scheduled state reconciliation"
                ));
                reported_lifecycle_hook_drops = lifecycle_hook_drops;
            }
            if combat_hook_drop_delta != 0 {
                diagnostics.warn(&format!(
                    "combat setter hooks dropped_records={combat_hook_drop_delta}; scheduled state reconciliation"
                ));
                reported_combat_hook_drops = combat_hook_drops;
            }
            if party_hook_drop_delta != 0 {
                diagnostics.warn(&format!(
                    "party hooks dropped_records={party_hook_drop_delta}; sampled membership fallback selected"
                ));
                reported_party_hook_drops = party_hook_drops;
            }
            raw.sequence = sequence;
            // Hook installation runs on the decoder worker and may complete
            // while `poll` is reading the startup snapshot. Use the status
            // after polling when selecting the authoritative provider so an
            // edge decoded in this update cannot be discarded as unavailable.
            let activity_hook_status = activity_hooks::status();
            for event in poller.take_diagnostics() {
                diagnostics.info(&format!("poller {event}"));
            }
            if previous_activity_hook_status != activity_hook_status {
                let mut message = format!(
                    "activity hook state={}",
                    activity_hooks::status_name(activity_hook_status)
                );
                let level = if activity_hook_status == 3 {
                    if let Some(error) = activity_hooks::error() {
                        message.push_str(&format!(" error={error}"));
                    }
                    Level::Warn
                } else {
                    Level::Info
                };
                diagnostics.log(level, &message);
                previous_activity_hook_status = activity_hook_status;
            }
            if activity_hook_loss_delta != 0 {
                diagnostics.warn(&format!(
                    "activity hook lost_edges={activity_hook_loss_delta}; scheduled instance reconciliation"
                ));
                reported_activity_hook_losses = activity_hook_losses;
            }
            let spatial_state = (
                raw.player_position_valid,
                raw.camera_heading_valid,
                raw.combat_references_available,
                raw.combat_reference_active_mask,
            );
            if previous_spatial_state != Some(spatial_state) {
                diagnostics.info(&format!(
                    "spatial player={} camera={} combat_refs={} active_mask=0x{:02x}",
                    raw.player_position_valid,
                    raw.camera_heading_valid,
                    raw.combat_references_available,
                    raw.combat_reference_active_mask,
                ));
                previous_spatial_state = Some(spatial_state);
            }
            let party_provider_state = poller.party().map_or_else(
                || provider_unavailable_diagnostic(&raw).to_owned(),
                |party| format!("live members={}", party.members.len()),
            );
            if previous_party_provider_state.as_ref() != Some(&party_provider_state) {
                diagnostics.info(&format!("party provider {party_provider_state}"));
                previous_party_provider_state = Some(party_provider_state);
            }
            if poller.instance_sampled() {
                let instance_provider_state = poller.instance().map_or_else(
                    || provider_unavailable_diagnostic(&raw).to_owned(),
                    |instance| format!("live kind={:?}", instance.kind),
                );
                if previous_instance_provider_state.as_ref() != Some(&instance_provider_state) {
                    diagnostics.info(&format!("instance provider {instance_provider_state}"));
                    previous_instance_provider_state = Some(instance_provider_state);
                }
            }
            chat_output::observe_player_position(
                raw.player_position_valid != 0,
                raw.player_position,
            );
            damage.update(
                poller.process_id(),
                raw.adapter_status == ADAPTER_LIVE,
                raw.in_world != 0,
                poller.current_hero(),
            );
            for event in damage.take_diagnostics() {
                diagnostics.info(&format!("damage {event}"));
            }
            let (window_hooks_available, window_edges, window_edge_drops) =
                damage.drain_window_edges();
            if !window_edges.is_empty() || window_edge_drops != 0 {
                let lifecycle_edge_count = window_edges
                    .iter()
                    .filter(|edge| !edge.geometry_refresh)
                    .count();
                let geometry_refresh_count = window_edges
                    .iter()
                    .filter(|edge| edge.geometry_refresh)
                    .count();
                diagnostics.debug(&format!(
                    "ui window hooks available={window_hooks_available} edges={lifecycle_edge_count} geometry_refreshes={geometry_refresh_count} drops={window_edge_drops}",
                ));
                for edge in &window_edges {
                    let captured_geometry = edge.geometry;
                    if let Some(geometry) = captured_geometry {
                        game_menu_geometry_revision = game_menu_geometry_revision.wrapping_add(1);
                        let geometry = overlay::GameMenuGeometry {
                            x: geometry.x,
                            y: geometry.y,
                            width: geometry.width,
                            height: geometry.height,
                        };
                        game_menu_geometry = Some(geometry);
                    }
                    let geometry_diagnostic = captured_geometry.map_or_else(
                        || {
                            if !edge.geometry_refresh
                                && edge.opened
                                && edge.runtime_type == "ui.win.EscapeMenu"
                            {
                                format!(
                                    " runtime_rect=pending geometry_error={}",
                                    ui_windows::geometry_error()
                                        .unwrap_or("object-not-readable-or-layout-not-ready")
                                )
                            } else {
                                "".to_owned()
                            }
                        },
                        |geometry| {
                            format!(
                                " runtime_rect={:.1},{:.1},{:.1},{:.1} viewport_scale={:.3},{:.3} viewport_offset={:.1},{:.1} physical_rect={:.1},{:.1},{:.1},{:.1} geometry_revision={game_menu_geometry_revision}",
                                geometry.runtime_x,
                                geometry.runtime_y,
                                geometry.runtime_width,
                                geometry.runtime_height,
                                geometry.viewport_scale_x,
                                geometry.viewport_scale_y,
                                geometry.viewport_offset_x,
                                geometry.viewport_offset_y,
                                geometry.x,
                                geometry.y,
                                geometry.width,
                                geometry.height,
                            )
                        },
                    );
                    diagnostics.debug(&format!(
                        "ui window hook observation={} runtime_type={} pointer=0x{:X}{geometry_diagnostic}",
                        if edge.geometry_refresh {
                            "geometry-refreshed"
                        } else if edge.opened {
                            "opened"
                        } else {
                            "closed"
                        },
                        edge.runtime_type,
                        edge.window_pointer,
                    ));
                }
            }
            let combat_state = poller.current_combat_state();
            if previous_combat_state != Some(combat_state) {
                diagnostics.info(&format!(
                    "combat flag={}",
                    combat_state.map_or("unavailable", |active| if active {
                        "active"
                    } else {
                        "idle"
                    })
                ));
                previous_combat_state = Some(combat_state);
            }
            let (direct_combat_available, mut combat_edges, combat_edge_drops) =
                damage.drain_combat_edges();
            let combat_hooks_authoritative = combat_provider
                .direct_is_authoritative(direct_combat_available)
                && combat_edge_drops == 0;
            if !combat_hooks_authoritative {
                combat_edges.clear();
            }
            // `combat flag` above already carries the transition the host cares
            // about; the per-edge breakdown only helps while debugging a hook.
            if !combat_edges.is_empty() {
                diagnostics.debug(&format!(
                    "combat hooks edges={}",
                    combat_edges
                        .iter()
                        .map(|active| if *active { "started" } else { "ended" })
                        .collect::<Vec<_>>()
                        .join(",")
                ));
            }
            events.observe_direct_combat(combat_hooks_authoritative, &combat_edges);
            let window_hooks_authoritative =
                window_provider.direct_is_authoritative(window_hooks_available, window_edge_drops);
            events.observe_window_hook(
                window_hooks_authoritative,
                poller.windows_sampled(),
                window_hooks_authoritative
                    .then(ui_windows::current_focus)
                    .flatten(),
                if window_hooks_authoritative {
                    window_edges
                        .into_iter()
                        .filter(|edge| !edge.geometry_refresh)
                        .map(|edge| (edge.opened, edge.runtime_type))
                        .collect()
                } else {
                    Vec::new()
                },
            );
            window_provider.finish_update(poller.windows_sampled(), window_edge_drops);
            let lifecycle_hooks_authoritative =
                lifecycle_provider.direct_is_authoritative(lifecycle_hooks::provider_available());
            let mut zone_edges = lifecycle_hooks::drain_zone_edges();
            if lifecycle_hook_drop_delta != 0 {
                zone_edges.clear();
            }
            events.observe_zone_hook(
                lifecycle_hooks_authoritative,
                poller.lifecycle_sampled(),
                if lifecycle_hooks_authoritative {
                    zone_edges
                } else {
                    Vec::new()
                },
            );
            events.observe_party_hook(
                party_hooks::provider_available() && !party_reconciliation_pending,
                party_membership_dirty,
            );
            events.observe_party(poller.party().cloned());
            if poller.instance_sampled() {
                events.observe_instance(poller.instance().cloned());
            }
            let instance_reconciliation_succeeded =
                poller.instance_sampled() && poller.instance().is_some();
            let hooked_instances = poller
                .take_activity_hook_edges()
                .into_iter()
                .filter_map(|edge| edge.observation)
                .collect::<Vec<_>>();
            let has_hooked_instance = !hooked_instances.is_empty();
            events.observe_instance_hook(activity_hook_status == 1, hooked_instances);
            force_instance_reconciliation |= activity_hook_loss_delta != 0;
            if sample_instance && instance_reconciliation_succeeded && !has_hooked_instance {
                force_instance_reconciliation = false;
            }
            let (snapshot, mut batch) = events.update(&raw, combat_state);
            if party_reconciliation_pending && poller.party().is_some() {
                party_reconciliation_pending = false;
            }
            let (mut damage_events, damage_drops) = damage.drain(poller.party_damage_sources());
            add_local_actor_identity(&mut damage_events, &snapshot.party);
            let previous_image_count = skill_images.as_ref().map_or(0, SkillImages::len);
            add_skill_metadata(&mut damage_events, skill_images.as_mut(), &mut diagnostics);
            if let Some(images) = &skill_images {
                let assets = images.assets();
                if assets.len() != previous_image_count {
                    diagnostics.info(&format!("skill images loaded={}", assets.len()));
                    game_images = assets;
                    renderer_images = Arc::new(combine_images(&game_images, &addon_images));
                }
            }
            events.note_dropped(
                &mut batch,
                damage_drops
                    .saturating_add(combat_edge_drops)
                    .saturating_add(activity_hook_loss_delta)
                    .saturating_add(window_edge_drops)
                    .saturating_add(lifecycle_hook_drop_delta)
                    .saturating_add(party_hook_drop_delta),
            );
            events.append_damage(&mut batch, damage_events);
            events.append(&mut batch, player_hooks::drain_disconnect_events());
            events.finish_update(&mut batch, &raw, combat_state);
            for event in &batch.events {
                if let Some((level, message)) = host_event_diagnostic(event) {
                    diagnostics.log(level, &message);
                }
            }
            manual_logout = batch.events.iter().any(|event| {
                matches!(
                    event,
                    HostEvent::PlayerDisconnected(disconnected)
                        if disconnected.reason == PlayerDisconnectReason::ManualExit
                )
            });
            if manual_logout {
                awaiting_world_after_logout = true;
                saw_world_exit_after_logout = false;
            } else if awaiting_world_after_logout && !world_is_loaded(&raw) {
                saw_world_exit_after_logout = true;
            }
            if raw.adapter_status == ADAPTER_LIVE
                && plugins.is_none()
                && !manual_logout
                && (!awaiting_world_after_logout
                    || (saw_world_exit_after_logout && world_is_loaded(&raw)))
            {
                let mut loaded = addon_manager::AddonManager::load_with_instance_id(
                    &addon_directory,
                    &config_directory,
                    &snapshot,
                    next_plugin_instance_id,
                );
                for event in loaded.take_diagnostics() {
                    diagnostics.log(plugin_diagnostic_level(&event), &format!("plugin {event}"));
                }
                diagnostics.info(&format!("Wasm components loaded={}", loaded.len()));
                diagnostics.info(&format!(
                    "Wasm config properties registered={}",
                    loaded.config_properties().len()
                ));
                displayed_addons = loaded.addon_infos();
                addon_fonts = Arc::new(loaded.font_assets());
                addon_images = loaded.image_assets();
                renderer_images = Arc::new(combine_images(&game_images, &addon_images));
                manager_resource_revision = Some(loaded.resource_revision());
                plugin_resource_revision = plugin_resource_revision.saturating_add(1);
                plugins = Some(loaded);
                awaiting_world_after_logout = false;
                saw_world_exit_after_logout = false;
            }

            // The capture counters describe steady state, not a change: they are
            // one long line every 100 polls (~1.7 s) and only earn their place
            // while hook work is being debugged.
            if diagnostics.allows(Level::Debug) && (sequence == 1 || sequence.is_multiple_of(100)) {
                diagnostics.debug(&format!("damage {}", damage.metrics()));
            }
            diagnostics.record_snapshot(&raw);
            latest_raw = raw;
            latest_snapshot = snapshot;
            batch_for_plugins = Some(batch);
            while next_poll <= poll_started {
                next_poll += MEMORY_POLL_INTERVAL;
            }
        }

        let ui_events = overlay.take_ui_events(plugin_resource_revision);
        for event in overlay.take_host_ui_events() {
            match event {
                overlay::HostUiEvent::SetSlashCommandsEnabled(enabled) => {
                    let result = host_settings.as_mut().map_or_else(
                        || Err("host settings store is unavailable".to_owned()),
                        |settings| {
                            settings
                                .set_slash_commands_enabled(enabled)
                                .map_err(|error| error.to_string())
                        },
                    );
                    match result {
                        Ok(()) => {
                            slash_commands_enabled = enabled;
                            slash_commands::set_enabled(enabled);
                            diagnostics.info(&format!(
                                "chat slash commands enabled={enabled} source=config-menu"
                            ));
                        }
                        Err(error) => diagnostics.warn(&format!(
                            "host setting update rejected key=slash-commands.enabled error={error}"
                        )),
                    }
                }
            }
        }
        let commands = slash_commands.drain();
        if !commands.is_empty() {
            if let Some(plugins) = &mut plugins {
                for command in commands {
                    if let Err(error) =
                        plugins.broadcast_host_message(command.topic, command.payload)
                    {
                        diagnostics.warn(&format!("chat slash command rejected: {error}"));
                    }
                }
            } else {
                diagnostics.warn(&format!(
                    "chat slash commands dropped={} reason=add-on-runtime-unavailable",
                    commands.len()
                ));
            }
        }
        for event in slash_commands.take_diagnostics() {
            diagnostics.info(&format!("chat {event}"));
        }
        // Native item/map clicks are observations, not explicit slash commands.
        // Discard them if the player has left the world before delivery.
        let inventory_click_payloads = inventory_clicks.drain();
        if latest_snapshot.session.in_world {
            if let Some(plugins) = &mut plugins {
                for payload in inventory_click_payloads {
                    if let Err(error) = plugins.broadcast_host_message(
                        inventory_clicks::INVENTORY_CLICK_TOPIC.to_owned(),
                        payload,
                    ) {
                        diagnostics.warn(&format!("inventory click rejected: {error}"));
                    }
                }
            }
        }
        for event in inventory_clicks.take_diagnostics() {
            diagnostics.info(&event);
        }
        let clicks = map_clicks.drain();
        if latest_snapshot.session.in_world {
            if let Some(plugins) = &mut plugins {
                for click in clicks {
                    if let Err(error) = plugins.broadcast_host_message(
                        map_clicks::MAP_CLICK_TOPIC.to_owned(),
                        click.payload(),
                    ) {
                        diagnostics.warn(&format!("map click rejected: {error}"));
                    }
                }
            }
        }
        for event in map_clicks.take_diagnostics() {
            diagnostics.info(&event);
        }
        let captured_map_view = map_view.refresh(latest_snapshot.session.in_world);
        let visible_map = captured_map_view
            .zip(overlay.pixels_per_point())
            .zip(latest_snapshot.map.area_id.as_ref())
            .filter(|_| {
                latest_snapshot
                    .ui
                    .focused_window
                    .as_deref()
                    .is_some_and(|name| matches!(name, "ui.win.MapWindow" | "MapWindow"))
            })
            .map(
                |((view, pixels_per_point), world)| farever_more_api::VisibleMap {
                    world: world.clone(),
                    bounds: view.bounds,
                    world_to_client: view.world_to_client,
                    pixels_per_point,
                },
            );
        let revision = last_map_snapshot
            .revision
            .wrapping_add(u64::from(last_map_snapshot.value != visible_map));
        last_map_snapshot = match visible_map {
            Some(value) => farever_more_api::StateSnapshot::live(
                latest_snapshot.captured_at_ms,
                revision,
                value,
            ),
            None => farever_more_api::StateSnapshot::unavailable(
                revision,
                map_view.unavailable_reason(),
            ),
        };
        latest_snapshot.map_view = last_map_snapshot.clone();
        let visibility_changed = captured_map_view.is_some() != logged_map_view.is_some();
        if visibility_changed
            || (captured_map_view != logged_map_view && Instant::now() >= next_map_view_log)
        {
            if let Some(view) = captured_map_view {
                diagnostics.info(&format!(
                    "map viewport {} bounds={:?} world_to_client={:?}",
                    if visibility_changed {
                        "visible"
                    } else {
                        "updated"
                    },
                    view.bounds,
                    view.world_to_client
                ));
            } else {
                diagnostics.info("map viewport unavailable");
            }
            logged_map_view = captured_map_view;
            next_map_view_log = Instant::now() + Duration::from_secs(1);
        }
        for event in map_view.take_diagnostics() {
            diagnostics.info(&event);
        }
        for event in chat_output_diagnostics.take_diagnostics() {
            diagnostics.info(&format!("chat {event}"));
        }
        let mut addon_frame = if let Some(plugins) = &mut plugins {
            dispatch_addons(
                plugins,
                &latest_snapshot,
                batch_for_plugins.as_ref(),
                &ui_events,
            )
        } else {
            UiFrame::default()
        };
        if manual_logout {
            if let Some(mut manager) = plugins.take() {
                manager.shutdown_for_menu();
                next_plugin_instance_id = manager.next_instance_id();
                for event in manager.take_diagnostics() {
                    diagnostics.log(plugin_diagnostic_level(&event), &format!("plugin {event}"));
                }
                diagnostics.info(&format!(
                    "Wasm add-on session stopped reason=manual-logout next_instance_id={next_plugin_instance_id}"
                ));
            }
            addon_frame = UiFrame::default();
            displayed_addons = plugin::discover_addon_infos(&addon_directory);
            addon_fonts = Arc::new(Vec::new());
            addon_images.clear();
            renderer_images = Arc::new(combine_images(&game_images, &addon_images));
            manager_resource_revision = None;
            plugin_resource_revision = plugin_resource_revision.saturating_add(1);
            loading_status = AddonLoadingStatus::default();
        }
        let addon_frame_available = latest_raw.adapter_status != ADAPTER_WAITING_TO_SCAN
            && latest_raw.adapter_status != ADAPTER_SEARCHING;
        let addon_surface_count = if addon_frame_available {
            addon_frame.surfaces.len()
        } else {
            0
        };
        let mut ui_frame = if latest_raw.adapter_status == ADAPTER_WAITING_TO_SCAN
            || latest_raw.adapter_status == ADAPTER_SEARCHING
        {
            UiFrame::default()
        } else {
            addon_frame
        };

        if let Some(plugins) = &plugins {
            let revision = plugins.resource_revision();
            if Some(revision) != manager_resource_revision {
                displayed_addons = plugins.addon_infos();
                addon_fonts = Arc::new(plugins.font_assets());
                addon_images = plugins.image_assets();
                renderer_images = Arc::new(combine_images(&game_images, &addon_images));
                manager_resource_revision = Some(revision);
                plugin_resource_revision = plugin_resource_revision.saturating_add(1);
                diagnostics.info(&format!(
                    "Wasm manager resources refreshed revision={revision} active={} disabled={}",
                    plugins.len(),
                    plugins
                        .statuses()
                        .iter()
                        .filter(|status| matches!(
                            status.state,
                            addon_manager::ManagedAddonState::Disabled
                        ))
                        .count()
                ));
            }
        }

        if let Some(plugins) = &mut plugins {
            for event in plugins.take_diagnostics() {
                diagnostics.log(plugin_diagnostic_level(&event), &format!("plugin {event}"));
            }
        }

        if let Some(plugins) = &plugins {
            if let Some(error) = loading_status.update(&plugins.statuses()) {
                diagnostics.error(&format!("add-on loading failed: {error}"));
                show_loading_error(&error, &config.addon_root);
            }
        }

        // A manual logout ends the add-on session. Publish an empty frame for
        // the menu instead of replacing the cleared add-on UI with the startup
        // status surface.
        if awaiting_world_after_logout {
            ui_frame = UiFrame::default();
        } else if !loading_status.finished {
            append_runtime_status(&mut ui_frame, &displayed_addons);
        }
        // One CPU sample every ten seconds is a health signal, not a fact about
        // the session; keep it reachable, out of the default log.
        let cpu_updated = cpu_meter.update();
        if diagnostics.allows(Level::Debug)
            && cpu_updated
            && (cpu_meter.sample_count() == 1 || cpu_meter.sample_count().is_multiple_of(10))
        {
            match cpu_meter.percent() {
                Some(percent) => diagnostics.debug(&format!(
                    "performance host_thread_cpu_percent={percent:.2} basis=one_logical_core"
                )),
                None => diagnostics.debug("performance host_thread_cpu_percent=unavailable"),
            }
        }
        let show_overlay = !awaiting_world_after_logout
            && latest_raw.process_found != 0
            && (latest_raw.adapter_status == ADAPTER_WAITING_TO_SCAN
                || latest_raw.adapter_status == ADAPTER_SEARCHING
                || (latest_raw.adapter_status == ADAPTER_LIVE && latest_raw.app_found != 0));
        let game_menu_open = game_menu_is_open(&latest_snapshot);
        let keep_running = overlay.update(
            poller.process_id(),
            plugin_resource_revision,
            &ui_frame,
            &addon_fonts,
            &renderer_images,
            overlay::OverlayHostState {
                visible: show_overlay,
                addon_surface_count,
                game_menu_open,
                game_menu_geometry,
                game_menu_geometry_revision,
                slash_commands_enabled,
            },
        );
        for (level, event) in overlay.take_diagnostics() {
            diagnostics.log(level, &format!("overlay {event}"));
        }
        if !keep_running {
            break;
        }
        let now = Instant::now();
        let mut sleep_for = next_poll
            .saturating_duration_since(now)
            .min(MAX_RUNTIME_SLEEP);
        if let Some(tick_delay) = plugins
            .as_ref()
            .and_then(addon_manager::AddonManager::next_tick_delay)
        {
            sleep_for = sleep_for.min(tick_delay);
        }
        if !sleep_for.is_zero() {
            thread::sleep(sleep_for);
        }
    }
    diagnostics.info(&format!("runtime stopped after {sequence} polls"));
    // Close both overlay windows before presenting a terminal startup error.
    drop(overlay);
    if let Some(error) = runtime_failure {
        show_loading_error(&error, &config.addon_root);
    }
}

#[derive(Default)]
struct AddonLoadingStatus {
    finished: bool,
    reported_errors: std::collections::BTreeMap<PathBuf, String>,
}

impl AddonLoadingStatus {
    fn update(&mut self, statuses: &[addon_manager::ManagedAddonStatus]) -> Option<String> {
        use addon_manager::ManagedAddonState;
        // Initial compilation/activation is asynchronous. A failed component
        // is terminal too; it must not leave the loading panel visible.
        if statuses
            .iter()
            .any(|status| status.state == ManagedAddonState::Compiling)
        {
            return None;
        }
        self.finished = true;
        let errors = statuses
            .iter()
            .filter(|status| status.state == ManagedAddonState::Disabled)
            .map(|status| {
                (
                    status.path.clone(),
                    status
                        .error
                        .clone()
                        .unwrap_or_else(|| "Unknown add-on error".to_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let new_errors = errors
            .iter()
            .filter(|(path, error)| self.reported_errors.get(*path) != Some(*error))
            .map(|(path, error)| format!("{}: {error}", path.display()))
            .collect::<Vec<_>>();
        self.reported_errors = errors;
        (!new_errors.is_empty()).then(|| new_errors.join("\n\n"))
    }
}

fn show_loading_error(error: &str, addon_root: &std::path::Path) {
    // A collection of component errors can be long. Keep the dialog readable;
    // the log retains every complete reason.
    let detail = error.chars().take(1200).collect::<String>();
    let truncated = if detail.len() < error.len() {
        "\n… See the host log for the full errors."
    } else {
        ""
    };
    let message = format!(
        "Farever More could not load one or more add-ons.\n\n{detail}{truncated}\n\nDetails: {}",
        addon_root.join("logs/host.log").display()
    );
    // The error must be visible even when game-state discovery or overlay
    // creation failed. Keep the native dialog off the game and runtime workers.
    #[cfg(windows)]
    let _ = thread::Builder::new()
        .name("farever-loading-error".to_owned())
        .spawn(move || {
            use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
            let message = message.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            let title = "Farever More — add-on loading failed"
                .encode_utf16()
                .chain(Some(0))
                .collect::<Vec<_>>();
            // SAFETY: the terminated UTF-16 buffers live until MessageBoxW returns.
            unsafe {
                MessageBoxW(
                    std::ptr::null_mut(),
                    message.as_ptr(),
                    title.as_ptr(),
                    MB_OK | MB_ICONERROR,
                );
            }
        });
    #[cfg(not(windows))]
    eprintln!("{message}");
}

fn combine_images(game: &[ImageAsset], addon: &[ImageAsset]) -> Vec<ImageAsset> {
    game.iter().chain(addon).cloned().collect()
}

fn add_local_actor_identity(
    events: &mut [HostEvent],
    party: &farever_more_api::StateSnapshot<farever_more_api::PartyState>,
) {
    let Some(actor_id) = party
        .value
        .as_ref()
        .and_then(|party| party.members.iter().find(|member| member.is_local))
        .map(|member| member.actor_id.as_str())
    else {
        return;
    };
    for event in events {
        let HostEvent::Damage(damage) = event else {
            continue;
        };
        if damage.source.relation == farever_more_api::ActorRelation::LocalPlayer
            && damage.source.actor_id.is_none()
        {
            damage.source.actor_id = Some(actor_id.to_owned());
        }
        if damage.target.relation == farever_more_api::ActorRelation::LocalPlayer
            && damage.target.actor_id.is_none()
        {
            damage.target.actor_id = Some(actor_id.to_owned());
        }
    }
}

fn add_skill_metadata(
    events: &mut [HostEvent],
    mut images: Option<&mut SkillImages>,
    diagnostics: &mut RuntimeLog,
) {
    for event in events {
        let HostEvent::Damage(damage) = event else {
            continue;
        };
        let Some(skill) = farever_db::Inventory::skill(&damage.skill_id) else {
            continue;
        };
        if damage.skill_display_name.is_none() {
            damage.skill_display_name = skill.name.map(str::to_owned);
        }
        if damage.skill_icon.is_none() {
            let Some(icon) = &skill.icon else {
                continue;
            };
            let Some(images) = images.as_deref_mut() else {
                continue;
            };
            match images.resolve(icon) {
                Ok(reference) => damage.skill_icon = reference,
                Err(error) => diagnostics.warn(&format!(
                    "skill image rejected skill_id={} error={error}",
                    damage.skill_id
                )),
            }
        }
    }
}

/// Per-fight combat events are detail the damage meter already surfaces to the
/// player, so the host reports them only while debugging; party and instance
/// changes stay at `info` because they explain later provider behaviour.
fn host_event_diagnostic(event: &HostEvent) -> Option<(Level, String)> {
    match event {
        HostEvent::CombatStarted(event) => Some((
            Level::Debug,
            format!(
                "combat event=started fight_id={} quality={:?}",
                event.fight_id, event.header.quality
            ),
        )),
        HostEvent::CombatEnded(event) => Some((
            Level::Debug,
            format!(
                "combat event=ended fight_id={} quality={:?}",
                event.fight_id, event.header.quality
            ),
        )),
        HostEvent::PartyChanged(event) => Some((
            Level::Info,
            format!(
                "party event=changed revision={} quality={:?}",
                event.revision, event.header.quality
            ),
        )),
        HostEvent::InstanceChanged(event) => Some((
            Level::Info,
            format!(
                "instance event=changed previous={} current={} quality={:?}",
                instance_diagnostic(event.previous.as_ref()),
                instance_diagnostic(event.current.as_ref()),
                event.header.quality
            ),
        )),
        _ => None,
    }
}

/// Add-on manager diagnostics arrive as text. Component lifecycle facts stay at
/// `info`; the per-load tick table and add-on debug chatter are `debug`.
fn plugin_diagnostic_level(message: &str) -> Level {
    if message.starts_with("Wasm tick registered") || message.starts_with("Wasm tick disabled") {
        return Level::Debug;
    }
    if let Some(index) = message.find("message=") {
        let text = &message[index..];
        if text.starts_with("message=Debug: ") || text.starts_with("message=Trace: ") {
            return Level::Debug;
        }
    }
    Level::Info
}

fn provider_unavailable_diagnostic(raw: &FasSnapshotV0) -> &'static str {
    if raw.adapter_status != ADAPTER_LIVE {
        "unavailable reason=not-yet-observed"
    } else if raw.in_world == 0 && raw.loading_state >= 0 {
        "unavailable reason=loading"
    } else if raw.in_world == 0 {
        "unavailable reason=not-in-world"
    } else {
        "unavailable reason=provider-failed"
    }
}

fn instance_diagnostic(instance: Option<&farever_more_api::InstanceState>) -> String {
    instance.map_or_else(
        || "none".to_owned(),
        |instance| format!("{}:{:?}", instance.session_id, instance.kind),
    )
}

fn dispatch_addons(
    plugins: &mut addon_manager::AddonManager,
    snapshot: &farever_more_api::GameSnapshot,
    events: Option<&farever_more_api::EventBatch>,
    ui_events: &[farever_more_api::RoutedUiEvent],
) -> UiFrame {
    // Dispatch must run even before the first component is active. Initial
    // compilation is asynchronous, and dispatch is the runtime-thread boundary
    // that consumes compiler results and activates ready components.
    plugins.dispatch(snapshot, events, ui_events)
}

fn append_runtime_status(frame: &mut UiFrame, addons: &[plugin::AddonInfo]) {
    let text = runtime_status_text(addons);
    frame.surfaces.push(UiSurface {
        owner: "farever-host".to_owned(),
        id: "runtime-status".to_owned(),
        title: format!("Farever More v{}", env!("CARGO_PKG_VERSION")),
        anchor: SurfaceAnchor::BottomLeft,
        margin_x: 20.0,
        margin_y: 20.0,
        width: Some(420.0),
        style: None,
        nodes: vec![UiNode {
            id: "summary".to_owned(),
            parent: None,
            widget: Widget::Text(TextWidget {
                text,
                style: TextStyle::Small,
                font_family: None,
                color: None,
                outline: None,
                wrap: true,
            }),
        }],
        canvas: Vec::new(),
    });
}

fn world_is_loaded(snapshot: &FasSnapshotV0) -> bool {
    snapshot.adapter_status == ADAPTER_LIVE && snapshot.in_world != 0
}

fn game_menu_is_open(snapshot: &farever_more_api::GameSnapshot) -> bool {
    snapshot.ui.open_windows.iter().any(|window| {
        window.rsplit(['.', ':']).next().is_some_and(|name| {
            name.eq_ignore_ascii_case("GameMenu") || name.eq_ignore_ascii_case("EscapeMenu")
        })
    })
}

fn runtime_status_text(addons: &[plugin::AddonInfo]) -> String {
    let mut text = format!("Add-ons: {}", addons.len());
    for addon in addons.iter().take(MAX_STATUS_ADDONS) {
        text.push_str("\n• ");
        text.push_str(&addon.name);
        match &addon.version {
            Some(version) => {
                text.push_str(" v");
                text.push_str(version);
            }
            None => text.push_str(" (version unavailable)"),
        }
    }
    if addons.len() > MAX_STATUS_ADDONS {
        text.push_str(&format!(
            "\n… and {} more",
            addons.len() - MAX_STATUS_ADDONS
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use farever_more_api::{
        ActorRelation, CombatActorRef, DamageEvent, EventHeader, PartyMember, PartyState,
        SourceQuality, StateSnapshot,
    };
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ADDON_MANAGER_ROOT: AtomicU64 = AtomicU64::new(1);

    struct AddonManagerTestRoot(PathBuf);

    impl AddonManagerTestRoot {
        fn new() -> Self {
            let serial = NEXT_ADDON_MANAGER_ROOT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "farever-runtime-addon-manager-test-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temporary runtime root");
            Self(path)
        }
    }

    impl Drop for AddonManagerTestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn window_provider_samples_only_for_fallback_activation_and_reconciliation() {
        let mut provider = WindowProviderState::new(true);

        assert!(!provider.begin_update(0));
        assert!(!provider.begin_update(4));

        assert!(provider.begin_update(1));
        provider.finish_update(false, 0);
        assert!(provider.begin_update(1));
        provider.finish_update(true, 0);
        assert!(!provider.begin_update(1));

        provider.finish_update(false, 2);
        assert!(provider.begin_update(1));
        provider.finish_update(true, 0);
        assert!(!provider.begin_update(1));

        assert!(provider.begin_update(3));
        assert!(provider.begin_update(3));
        assert!(provider.begin_update(1));
    }

    #[test]
    fn external_window_provider_uses_sampled_fallback() {
        let mut provider = WindowProviderState::new(false);
        assert!(provider.begin_update(0));
        assert!(provider.begin_update(1));
    }

    #[test]
    fn player_provider_samples_only_for_activation_loss_and_fallback() {
        let mut provider = PlayerProviderState::new(true);

        assert!(!provider.begin_update(0, 0));
        assert!(!provider.begin_update(4, 0));

        assert!(provider.begin_update(1, 0));
        assert!(!provider.direct_is_authoritative(true));
        provider.finish_update(false, 0);
        assert!(provider.begin_update(1, 0));
        provider.finish_update(true, 0);
        assert!(provider.direct_is_authoritative(true));
        assert!(!provider.begin_update(1, 0));

        assert!(provider.begin_update(1, 2));
        assert!(!provider.direct_is_authoritative(true));
        provider.finish_update(false, 2);
        assert!(provider.begin_update(1, 2));
        provider.finish_update(true, 2);
        assert!(provider.direct_is_authoritative(true));
        assert!(!provider.begin_update(1, 2));

        assert!(provider.begin_update(3, 2));
        assert!(provider.begin_update(3, 2));
        assert!(provider.begin_update(1, 2));
    }

    #[test]
    fn external_player_provider_uses_sampled_fallback() {
        let mut provider = PlayerProviderState::new(false);
        assert!(provider.begin_update(0, 0));
        assert!(provider.begin_update(1, 0));
    }

    #[test]
    fn local_damage_uses_the_same_actor_id_as_the_party_snapshot() {
        let party = StateSnapshot::live(
            10,
            1,
            PartyState {
                party_id: Some("party-1".to_owned()),
                members: vec![PartyMember {
                    actor_id: "actor-7".to_owned(),
                    is_local: true,
                    name: Some("Local".to_owned()),
                    class_id: Some("Warrior".to_owned()),
                    class_icon: None,
                    in_combat: Some(true),
                }],
            },
        );
        let mut events = vec![HostEvent::Damage(DamageEvent {
            header: EventHeader {
                sequence: 0,
                monotonic_ms: 0,
                quality: SourceQuality::Observed,
            },
            source: CombatActorRef {
                actor_id: None,
                relation: ActorRelation::LocalPlayer,
                kind: None,
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
        })];

        add_local_actor_identity(&mut events, &party);

        let HostEvent::Damage(damage) = &events[0] else {
            panic!("expected damage");
        };
        assert_eq!(damage.source.actor_id.as_deref(), Some("actor-7"));
    }

    #[test]
    fn runtime_status_lists_at_most_five_addons() {
        let addons = (1..=7)
            .map(|index| plugin::AddonInfo {
                name: format!("addon-{index}"),
                version: Some(format!("1.0.{index}")),
            })
            .collect::<Vec<_>>();

        let text = runtime_status_text(&addons);

        assert!(text.starts_with("Add-ons: 7"));
        assert!(text.contains("• addon-5 v1.0.5"));
        assert!(!text.contains("• addon-6"));
        assert!(text.ends_with("… and 2 more"));
    }

    #[test]
    fn runtime_status_marks_missing_versions() {
        let text = runtime_status_text(&[plugin::AddonInfo {
            name: "local-addon".to_owned(),
            version: None,
        }]);

        assert_eq!(text, "Add-ons: 1\n• local-addon (version unavailable)");
    }

    #[test]
    fn empty_manager_dispatches_initial_compiler_results() {
        let root = AddonManagerTestRoot::new();
        let addons = root.0.join("addons");
        let config = root.0.join("config");
        fs::create_dir_all(&addons).expect("create add-on directory");
        let component = addons.join("broken.wasm");
        fs::write(&component, b"not a WebAssembly component")
            .expect("write invalid component fixture");
        let snapshot = farever_more_api::GameSnapshot::default();
        let mut manager = addon_manager::AddonManager::load(&addons, &config, &snapshot);

        assert!(manager.statuses().iter().any(|status| {
            status.path == component && status.state == addon_manager::ManagedAddonState::Compiling
        }));
        let mut loading = AddonLoadingStatus::default();
        assert!(loading.update(&manager.statuses()).is_none());
        assert!(!loading.finished);

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && !manager.statuses().iter().any(|status| {
                status.path == component
                    && status.state == addon_manager::ManagedAddonState::Disabled
            })
        {
            let _ = dispatch_addons(&mut manager, &snapshot, None, &[]);
            thread::sleep(Duration::from_millis(10));
        }

        assert!(manager.statuses().iter().any(|status| {
            status.path == component
                && status.state == addon_manager::ManagedAddonState::Disabled
                && status.error.is_some()
        }));
        let error = loading
            .update(&manager.statuses())
            .expect("visible load error");
        assert!(error.contains("broken.wasm"));
        assert!(loading.finished, "failure closes the loading panel");
        assert!(loading.update(&manager.statuses()).is_none(), "report once");
    }

    #[test]
    fn empty_manager_renders_no_host_surface() {
        let root = AddonManagerTestRoot::new();
        let addons = root.0.join("addons");
        let config = root.0.join("config");
        let snapshot = farever_more_api::GameSnapshot::default();
        let mut manager = addon_manager::AddonManager::load(&addons, &config, &snapshot);

        let frame = dispatch_addons(&mut manager, &snapshot, None, &[]);

        assert!(frame.surfaces.is_empty());
        assert!(frame.config_menus.is_empty());
        let mut loading = AddonLoadingStatus::default();
        assert!(loading.update(&manager.statuses()).is_none());
        assert!(loading.finished);
    }

    #[test]
    fn loading_finishes_after_activation_and_reports_new_failures_once() {
        use addon_manager::{ManagedAddonState, ManagedAddonStatus};
        let mut loading = AddonLoadingStatus::default();
        let mut status = ManagedAddonStatus {
            path: PathBuf::from("test.wasm"),
            info: plugin::AddonInfo {
                name: "test".to_owned(),
                version: None,
            },
            state: ManagedAddonState::Active,
            error: None,
        };
        assert!(loading.update(&[status.clone()]).is_none());
        assert!(loading.finished);
        status.state = ManagedAddonState::Disabled;
        status.error = Some("activation failed".to_owned());
        assert!(loading
            .update(&[status.clone()])
            .unwrap()
            .contains("activation failed"));
        assert!(loading.update(&[status.clone()]).is_none());
        status.error = Some("different failure".to_owned());
        assert!(loading
            .update(&[status.clone()])
            .unwrap()
            .contains("different failure"));
        status.state = ManagedAddonState::Active;
        status.error = None;
        assert!(loading.update(&[status.clone()]).is_none());
        status.state = ManagedAddonState::Disabled;
        status.error = Some("activation failed".to_owned());
        assert!(loading.update(&[status]).is_some());
    }

    #[test]
    fn runtime_status_surface_inherits_the_host_foreground_color() {
        let mut frame = UiFrame::default();
        append_runtime_status(&mut frame, &[]);
        let Widget::Text(runtime_status) = &frame.surfaces[0].nodes[0].widget else {
            panic!("runtime status should be text");
        };
        assert_eq!(runtime_status.color, None);
    }

    #[test]
    fn world_reentry_requires_live_in_world_state() {
        let in_world_but_not_live = FasSnapshotV0 {
            in_world: 1,
            ..FasSnapshotV0::default()
        };
        assert!(!world_is_loaded(&in_world_but_not_live));

        let live_in_world = FasSnapshotV0 {
            adapter_status: ADAPTER_LIVE,
            in_world: 1,
            ..FasSnapshotV0::default()
        };
        assert!(world_is_loaded(&live_in_world));
    }

    #[test]
    fn config_entry_point_tracks_only_the_game_menu_window() {
        let mut snapshot = farever_more_api::GameSnapshot::default();

        for window in [
            "GameMenu",
            "ui.win.GameMenu",
            "ui.win.EscapeMenu",
            "ui:menu:gamemenu",
            "ui:menu:escapemenu",
        ] {
            snapshot.ui.open_windows = vec![window.to_owned()];
            assert!(game_menu_is_open(&snapshot), "{window}");
        }

        for window in ["GamepadMenu", "OptionsWindow", "ui.win.InventoryUI"] {
            snapshot.ui.open_windows = vec![window.to_owned()];
            assert!(!game_menu_is_open(&snapshot), "{window}");
        }
    }

    #[test]
    fn renderer_image_set_contains_game_and_addon_resources() {
        let image = |id: &str| ImageAsset {
            id: id.to_owned(),
            width: 1,
            height: 1,
            rgba: Arc::from([0, 0, 0, 0]),
        };

        let combined = combine_images(&[image("game/icon")], &[image("addon-image/test/icon")]);

        assert_eq!(combined.len(), 2);
        assert_eq!(combined[0].id, "game/icon");
        assert_eq!(combined[1].id, "addon-image/test/icon");
    }
}
