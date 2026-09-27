use crate::config::{
    ConfigPropertyAccess, ConfigPropertyDescriptor, ConfigRegistry, ConfigStatus, ConfigValue,
    ConfigValueKind,
};
use farever_more_api as api;
use farever_more_manifest::api::ApiVersion;
pub(crate) use farever_more_manifest::{
    is_valid_name, unit_name, AddonDependency, AddonManifest, MANIFEST_FILE_NAME, WASM_FILE_NAME,
};
use image::{ImageFormat, ImageReader, Limits};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wasm_metadata::Payload;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

wasmtime::component::bindgen!({
    path: "../wit",
    world: "farever-addon",
});

/// Short names for WIT types used by the host.
mod model {
    pub use super::exports::farever::addon::plugin::{ActivationContext, DeactivationReason, Tick};
    pub use super::farever::addon::assets::{ImageRef, TextStyle};
    pub use super::farever::addon::bus::{AddonMessage, MessageError, MessageTarget};
    pub use super::farever::addon::camera::{CameraSnapshot, CameraState};
    pub use super::farever::addon::combat::{
        ActorRelation, CombatActorRef, CombatEvent, CombatReference, CombatReferenceSlot,
        CombatSnapshot, CombatState, DamageEvent,
    };
    pub use super::farever::addon::common::{EventHeader, StateStatus, UnavailableReason, Vec3};
    pub use super::farever::addon::config::{
        ConfigPropertyAccess, ConfigPropertyDescriptor, ConfigStatus, ConfigValue, ConfigValueKind,
    };
    pub use super::farever::addon::dependencies::{CallError, OpenError, ServiceHandle};
    pub use super::farever::addon::events::{
        DisconnectReason, Event, EventBatch, PlayerDisconnectedEvent,
    };
    pub use super::farever::addon::game::{ObservationMetadata, SessionState};
    pub use super::farever::addon::instance_session::{
        InstanceEvent, InstanceKind, InstanceSnapshot, InstanceState,
    };
    pub use super::farever::addon::overlay::{
        ButtonPressed, CanvasCommand, CanvasPressed, CanvasPrimitive, ConfigMenu, ContainerStyle,
        HorizontalAlignment, LayoutDirection, Point, Rgba, Stroke, SurfaceAnchor,
        TableColumnSizing, TableRowKind, UiEvent, UiFrame, UiNode, UiSurface, UiUpdate, UiView,
        Widget,
    };
    #[cfg(test)]
    pub use super::farever::addon::overlay::{
        ButtonWidget, CanvasWidget, CheckboxWidget, ContainerWidget, DropdownOption,
        DropdownWidget, LinePrimitive, SectionWidget, Size, SliderWidget, SurfaceStyle,
        TableCellWidget, TableColumn, TableRowProgress, TableRowWidget, TableWidget, TextWidget,
    };
    pub use super::farever::addon::party::{PartyEvent, PartyMember, PartySnapshot, PartyState};
    pub use super::farever::addon::player::{PlayerSnapshot, PlayerState};
    pub use super::farever::addon::runtime::LogLevel;
    pub use super::farever::addon::windows::{WindowEvent, WindowsSnapshot, WindowsState};
    pub use super::farever::addon::zone::{ZoneEvent, ZoneSnapshot, ZoneState};

    #[derive(Clone)]
    pub struct CallbackSnapshot {
        pub observation: ObservationMetadata,
        pub session: SessionState,
        pub player: PlayerSnapshot,
        pub party: PartySnapshot,
        pub camera: CameraSnapshot,
        pub combat: CombatSnapshot,
        pub instance_session: InstanceSnapshot,
        pub zone: ZoneSnapshot,
        pub windows: WindowsSnapshot,
    }
}

// Component instantiation charges fuel for initializing embedded data
// segments. Activation additionally performs one page-capped seed walk
// after lifting the registered images, and any tick may re-walk a full
// dense window the same way, so both budgets cover a bounded fetch (at
// most 64 pages of at most 64 records); per-call provider work stays
// under its own callback cap either way.
const FUEL_PER_ACTIVATION: u64 = 60_000_000;
const FUEL_PER_CALLBACK: u64 = 40_000_000;
const FUEL_PER_DEACTIVATION: u64 = 5_000_000;
const MAX_COMPONENT_MEMORY: usize = 128 * 1024 * 1024;
const MAX_FAILURES: u32 = 3;
pub const MIN_TICK_INTERVAL_MS: u32 = 16;
pub const MAX_TICK_INTERVAL_MS: u32 = 60_000;
const MAX_ADDON_NAME_BYTES: usize = 96;
const MAX_ADDON_VERSION_BYTES: usize = 48;
const MAX_TOPIC_BYTES: usize = 128;
const MAX_SUBSCRIPTIONS_PER_ADDON: usize = 64;
const MAX_MESSAGE_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_OUTBOUND_MESSAGES_PER_CALLBACK: usize = 32;
const MAX_OUTBOUND_PAYLOAD_BYTES_PER_CALLBACK: usize = 256 * 1024;
const MAX_CHAT_OUTPUTS_PER_CALLBACK: usize = 8;
const MAX_CHAT_OUTPUT_CODE_UNITS_PER_CALLBACK: usize = 1_000;
const MAX_PENDING_MESSAGES_PER_ADDON: usize = 256;
const MAX_PENDING_PAYLOAD_BYTES_PER_ADDON: usize = 1024 * 1024;
const HOST_MESSAGE_SOURCE_ID: &str = "farever.host";
const MAX_SERVICE_HANDLES_PER_ADDON: usize = 32;
const MAX_SERVICE_CALLS_PER_CALLBACK: usize = 64;
const MAX_SERVICE_PAYLOAD_BYTES: usize = 256 * 1024;

type PluginResult<T> = std::result::Result<T, String>;

/// Human-readable identity embedded in a WebAssembly component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddonInfo {
    /// Component name, falling back to the `.wasm` filename stem.
    pub name: String,
    /// Standard WebAssembly `version` annotation, when present.
    pub version: Option<String>,
}

pub struct Plugins {
    loaded: Vec<Plugin>,
    diagnostics: Vec<String>,
    started: Instant,
    epoch_ms: u64,
    idle_batch: api::EventBatch,
    bus_directory: Arc<Mutex<BusDirectory>>,
    service_directory: Rc<RefCell<ServiceDirectory>>,
    message_queues: HashMap<String, MessageQueue>,
    next_message_id: u64,
}

impl Plugins {
    #[cfg(test)]
    pub fn load(directory: &Path, config_directory: &Path, snapshot: &api::GameSnapshot) -> Self {
        match create_engine() {
            Ok(engine) => Self::load_with_engine(&engine, directory, config_directory, snapshot),
            Err(error) => {
                Self::unavailable(snapshot, format!("Wasm engine creation failed: {error:#}"))
            }
        }
    }

    pub(crate) fn empty(snapshot: &api::GameSnapshot) -> Self {
        Self {
            loaded: Vec::new(),
            diagnostics: Vec::new(),
            started: Instant::now(),
            epoch_ms: snapshot.captured_at_ms,
            idle_batch: empty_batch(snapshot.session.process_session),
            bus_directory: Arc::new(Mutex::new(BusDirectory::default())),
            service_directory: Rc::new(RefCell::new(ServiceDirectory::default())),
            message_queues: HashMap::new(),
            next_message_id: 1,
        }
    }

    pub(crate) fn unavailable(snapshot: &api::GameSnapshot, diagnostic: String) -> Self {
        let mut plugins = Self::empty(snapshot);
        plugins.diagnostics.push(diagnostic);
        plugins
    }

    #[cfg(test)]
    pub(crate) fn load_with_engine(
        engine: &Engine,
        directory: &Path,
        config_directory: &Path,
        snapshot: &api::GameSnapshot,
    ) -> Self {
        let started = Instant::now();
        let epoch_ms = snapshot.captured_at_ms;
        let bus_directory = Arc::new(Mutex::new(BusDirectory::default()));
        let service_directory = Rc::new(RefCell::new(ServiceDirectory::default()));
        let mut diagnostics = Vec::new();
        let mut candidates = discover_components(directory);
        order_components_by_dependencies(&mut candidates);
        let loaded = candidates
            .iter()
            .enumerate()
            .filter_map(|(index, path)| {
                match Plugin::load(
                    engine,
                    path,
                    PluginLoadContext {
                        config_directory,
                        snapshot,
                        instance_id: u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
                        clock_started: started,
                        epoch_ms,
                        bus_directory: Arc::clone(&bus_directory),
                        service_directory: Rc::clone(&service_directory),
                    },
                ) {
                    Ok(mut plugin) => {
                        if let Err(error) = service_directory.borrow_mut().register(
                            &plugin.namespace,
                            &plugin.version,
                            &plugin.provides,
                            &plugin.guest,
                        ) {
                            diagnostics.push(format!(
                                "Wasm component rejected path={} error={error:#}",
                                path.display()
                            ));
                            return None;
                        }
                        diagnostics.push(format!("Wasm component loaded path={}", path.display()));
                        diagnostics.push(match plugin.tick {
                            Some(tick) => format!(
                                "Wasm tick registered path={} interval_ms={}",
                                path.display(),
                                tick.interval_ms
                            ),
                            None => format!("Wasm tick disabled path={}", path.display()),
                        });
                        for message in plugin.take_logs() {
                            diagnostics.push(format!(
                                "Wasm log path={} message={message}",
                                path.display()
                            ));
                        }
                        Some(plugin)
                    }
                    Err(error) => {
                        diagnostics.push(format!(
                            "Wasm component rejected path={} error={error:#}",
                            path.display()
                        ));
                        None
                    }
                }
            })
            .collect();
        Self {
            loaded,
            diagnostics,
            started,
            epoch_ms,
            idle_batch: empty_batch(snapshot.session.process_session),
            bus_directory,
            service_directory,
            message_queues: HashMap::new(),
            next_message_id: 1,
        }
    }

    pub(crate) fn contains_path(&self, path: &Path) -> bool {
        self.loaded.iter().any(|plugin| plugin.path == path)
    }

    /// Reports whether an add-on id currently provides anything, used for
    /// ordering-only dependencies that name no service.
    pub(crate) fn provider_present(&self, addon_id: &str) -> bool {
        self.service_directory
            .borrow()
            .providers
            .contains_key(addon_id)
    }

    /// Reports whether one required service is currently provided at a
    /// compatible version. Used to decide if a consumer with unsatisfied
    /// dependencies should wait for its provider or fail loudly.
    pub(crate) fn provider_available(
        &self,
        addon_id: &str,
        service_id: &str,
        version: &str,
    ) -> bool {
        self.service_directory
            .borrow()
            .providers
            .get(addon_id)
            .is_some_and(|provider| {
                provider.services.contains(service_id)
                    && service_version_matches(version, &provider.version)
            })
    }

    /// Reuses the activation pre-check so the manager reports the same
    /// player-readable error it would fail activation with.
    pub(crate) fn check_required(&self, manifest: &AddonManifest) -> PluginResult<()> {
        check_required_dependencies(&manifest.dependencies, &self.service_directory.borrow())
    }

    pub(crate) fn paths(&self) -> impl Iterator<Item = &Path> {
        self.loaded.iter().map(|plugin| plugin.path.as_path())
    }

    pub(crate) fn current_frame(&self) -> api::UiFrame {
        api::UiFrame {
            surfaces: self
                .loaded
                .iter()
                .filter(|plugin| !plugin.disabled)
                .flat_map(|plugin| plugin.last_frame.surfaces.iter().cloned())
                .collect(),
            config_menus: self
                .loaded
                .iter()
                .filter(|plugin| !plugin.disabled)
                .flat_map(|plugin| plugin.last_frame.config_menus.iter().cloned())
                .collect(),
        }
    }

    fn deactivate_path(&mut self, path: &Path, reason: model::DeactivationReason) -> bool {
        let Some(index) = self.loaded.iter().position(|plugin| plugin.path == path) else {
            return false;
        };
        let mut plugin = self.loaded.remove(index);
        let namespace = plugin.namespace.clone();
        if let Err(error) = plugin.deactivate(reason) {
            self.diagnostics.push(format!(
                "Wasm deactivation failed path={} error={error:#}",
                plugin.path.display()
            ));
        }
        for message in plugin.take_logs() {
            self.diagnostics.push(format!(
                "Wasm log path={} message={message}",
                plugin.path.display()
            ));
        }
        self.message_queues.remove(&namespace);
        true
    }

    pub(crate) fn teardown_for_reload(&mut self, path: &Path) -> bool {
        self.deactivate_path(path, model::DeactivationReason::Reloaded)
    }

    pub(crate) fn teardown_for_removal(&mut self, path: &Path) -> bool {
        self.deactivate_path(path, model::DeactivationReason::Removed)
    }

    pub(crate) fn shutdown_for_menu(&mut self) {
        while let Some(path) = self.loaded.last().map(|plugin| plugin.path.clone()) {
            self.deactivate_path(&path, model::DeactivationReason::HostShutdown);
        }
    }

    pub(crate) fn activate_compiled(
        &mut self,
        engine: &Engine,
        config_directory: &Path,
        snapshot: &api::GameSnapshot,
        compiled: CompiledPlugin,
        instance_id: u64,
    ) -> PluginResult<()> {
        let path = compiled.path.clone();
        let mut plugin = Plugin::activate(
            engine,
            compiled,
            PluginLoadContext {
                config_directory,
                snapshot,
                instance_id,
                clock_started: self.started,
                epoch_ms: self.epoch_ms,
                bus_directory: Arc::clone(&self.bus_directory),
                service_directory: Rc::clone(&self.service_directory),
            },
        )?;
        self.service_directory.borrow_mut().register(
            &plugin.namespace,
            &plugin.version,
            &plugin.provides,
            &plugin.guest,
        )?;
        self.diagnostics
            .push(format!("Wasm component loaded path={}", path.display()));
        self.diagnostics.push(match plugin.tick {
            Some(tick) => format!(
                "Wasm tick registered path={} interval_ms={}",
                path.display(),
                tick.interval_ms
            ),
            None => format!("Wasm tick disabled path={}", path.display()),
        });
        for message in plugin.take_logs() {
            self.diagnostics.push(format!(
                "Wasm log path={} message={message}",
                path.display()
            ));
        }
        self.loaded.push(plugin);
        self.loaded
            .sort_by(|left, right| left.path.cmp(&right.path));
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.loaded.len()
    }

    pub fn addon_infos(&self) -> Vec<AddonInfo> {
        self.loaded
            .iter()
            .map(|plugin| plugin.info.clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn tick_interval_for_path(&self, path: &Path) -> Option<u32> {
        self.loaded
            .iter()
            .find(|plugin| plugin.path == path)
            .and_then(|plugin| plugin.tick.as_ref())
            .map(|tick| tick.interval_ms)
    }

    /// Returns the immutable font faces registered by successfully loaded
    /// add-ons. The byte buffers remain shared with the plugin stores.
    pub fn font_assets(&self) -> Vec<api::FontAsset> {
        self.loaded
            .iter()
            .flat_map(|plugin| plugin.font_assets.iter().cloned())
            .collect()
    }

    /// Returns the immutable images registered by successfully loaded add-ons.
    /// Decoded RGBA buffers remain shared with the plugin stores.
    pub fn image_assets(&self) -> Vec<api::ImageAsset> {
        self.loaded
            .iter()
            .flat_map(|plugin| plugin.image_assets.iter().cloned())
            .collect()
    }

    /// Returns property descriptors in add-on load and registration order.
    pub fn config_properties(&self) -> Vec<api::ConfigPropertyDescriptor> {
        self.loaded
            .iter()
            .filter(|plugin| !plugin.disabled)
            .flat_map(|plugin| plugin.config_properties.iter().cloned())
            .collect()
    }

    /// Delivers event work and due ticks through distinct serialized callbacks.
    ///
    /// Passing an empty event batch only refreshes the tick-side continuity
    /// metadata; it does not invoke event-only add-ons.
    pub fn dispatch(
        &mut self,
        snapshot: &api::GameSnapshot,
        events: Option<&api::EventBatch>,
        ui_events: &[api::RoutedUiEvent],
    ) -> api::UiFrame {
        if let Some(events) = events {
            self.idle_batch = empty_batch_from(events);
        }
        let event_work = events.is_some_and(has_event_work);
        let events = events.unwrap_or(&self.idle_batch);
        let epoch_ms = self.epoch_ms;
        let started = self.started;
        let now_ms = || epoch_ms.saturating_add(monotonic_millis(started.elapsed()));
        let mut ready_messages = std::mem::take(&mut self.message_queues);
        let mut outbound_messages = Vec::new();

        for plugin in &mut self.loaded {
            if plugin.disabled {
                continue;
            }

            let mut ui_succeeded = true;
            let namespace = plugin.namespace.clone();
            for routed in ui_events.iter().filter(|event| event.owner == namespace) {
                if !ui_succeeded || plugin.disabled {
                    break;
                }
                let result = plugin.on_ui_event(snapshot, &routed.event);
                ui_succeeded = apply_callback_result(
                    plugin,
                    "on-ui-event",
                    result,
                    now_ms(),
                    &mut self.diagnostics,
                    &mut outbound_messages,
                );
            }

            let event_succeeded = if ui_succeeded && event_work {
                let result = plugin.on_event(snapshot, events);
                apply_callback_result(
                    plugin,
                    "on-event",
                    result,
                    now_ms(),
                    &mut self.diagnostics,
                    &mut outbound_messages,
                )
            } else {
                ui_succeeded
            };
            let message_succeeded = if event_succeeded && !plugin.disabled {
                ready_messages
                    .remove(&plugin.namespace)
                    .is_none_or(|queue| {
                        let (dropped_before, messages) = queue.into_batch();
                        let result = plugin.on_message(snapshot, dropped_before, &messages);
                        apply_callback_result(
                            plugin,
                            "on-message",
                            result,
                            now_ms(),
                            &mut self.diagnostics,
                            &mut outbound_messages,
                        )
                    })
            } else {
                false
            };
            if message_succeeded && !plugin.disabled {
                if let Some(tick) = plugin.take_due_tick(now_ms()) {
                    let result = plugin.on_tick(snapshot, tick);
                    apply_callback_result(
                        plugin,
                        "on-tick",
                        result,
                        now_ms(),
                        &mut self.diagnostics,
                        &mut outbound_messages,
                    );
                }
            }
            for message in plugin.take_logs() {
                self.diagnostics.push(format!(
                    "Wasm log path={} message={message}",
                    plugin.path.display()
                ));
            }
        }
        self.restore_undelivered_messages(ready_messages);
        self.enqueue_messages(outbound_messages, now_ms());

        self.current_frame()
    }

    fn restore_undelivered_messages(&mut self, ready_messages: HashMap<String, MessageQueue>) {
        let active_addons = self
            .bus_directory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .active_addons
            .clone();
        for (addon_id, queue) in ready_messages {
            if active_addons.contains(&addon_id) {
                self.message_queues.insert(addon_id, queue);
            }
        }
    }

    fn enqueue_messages(&mut self, staged_messages: Vec<StagedMessage>, monotonic_ms: u64) {
        let active_addons = self
            .bus_directory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .active_addons
            .clone();
        for staged in staged_messages {
            let id = self.next_message_id;
            self.next_message_id = self.next_message_id.saturating_add(1);
            let source_addon_id: Arc<str> = Arc::from(staged.source_addon_id);
            let topic: Arc<str> = Arc::from(staged.topic);
            let payload: Arc<[u8]> = Arc::from(staged.payload);
            let message = BusMessage {
                id,
                monotonic_ms,
                source_addon_id,
                topic,
                correlation_id: staged.correlation_id,
                payload,
            };
            for recipient in staged.recipients {
                if active_addons.contains(&recipient) {
                    self.message_queues
                        .entry(recipient)
                        .or_default()
                        .push(message.clone());
                }
            }
        }
    }

    /// Time until the next enabled add-on tick, if any.
    pub fn next_tick_delay(&self) -> Option<Duration> {
        let now_ms = self.now_ms();
        self.loaded
            .iter()
            .filter(|plugin| !plugin.disabled)
            .filter_map(|plugin| plugin.tick.as_ref())
            .map(|tick| Duration::from_millis(tick.next_due_ms.saturating_sub(now_ms)))
            .min()
    }

    fn now_ms(&self) -> u64 {
        self.epoch_ms
            .saturating_add(monotonic_millis(self.started.elapsed()))
    }

    pub fn take_diagnostics(&mut self) -> Vec<String> {
        std::mem::take(&mut self.diagnostics)
    }

    /// Broadcasts one bounded host-originated message to every active exact
    /// subscriber. Host messages use the same queueing, ordering, and loss
    /// policy as add-on publications, but do not originate in a Wasm callback.
    pub fn broadcast_host_message(
        &mut self,
        topic: String,
        payload: Vec<u8>,
    ) -> Result<u32, String> {
        validate_topic(&topic)
            .map_err(|_| format!("invalid lowercase message-bus topic {topic:?}"))?;
        if payload.len() > MAX_MESSAGE_PAYLOAD_BYTES {
            return Err(format!(
                "message-bus payload bytes={} exceeds limit={MAX_MESSAGE_PAYLOAD_BYTES}",
                payload.len()
            ));
        }
        let recipients = self
            .bus_directory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recipients(
                HOST_MESSAGE_SOURCE_ID,
                &topic,
                &model::MessageTarget::Subscribers,
            );
        let recipient_count = u32::try_from(recipients.len()).unwrap_or(u32::MAX);
        if !recipients.is_empty() {
            self.enqueue_messages(
                vec![StagedMessage {
                    source_addon_id: HOST_MESSAGE_SOURCE_ID.to_owned(),
                    recipients,
                    topic,
                    correlation_id: None,
                    payload,
                }],
                self.now_ms(),
            );
        }
        Ok(recipient_count)
    }
}

fn apply_callback_result(
    plugin: &mut Plugin,
    callback: &str,
    result: PluginResult<CallbackEffects>,
    now_ms: u64,
    diagnostics: &mut Vec<String>,
    outbound_messages: &mut Vec<StagedMessage>,
) -> bool {
    match result {
        Ok(mut update) => {
            plugin.failures = 0;
            match update.ui {
                UiEffect::Unchanged => {}
                UiEffect::Replace(frame) => plugin.last_frame = frame,
                UiEffect::Clear => plugin.last_frame = api::UiFrame::default(),
            }
            if let Some(message) = plugin.apply_tick_effect(
                update.scheduled_tick_interval_ms,
                update.continue_ticking,
                now_ms,
            ) {
                diagnostics.push(message);
            }
            outbound_messages.append(&mut update.outbound);
            for mut output in update.chat.drain(..) {
                output.sender_name = Some(plugin.chat_name.clone());
                if let Err(error) = crate::chat_output::enqueue(output) {
                    diagnostics.push(format!(
                        "Wasm chat output dropped path={} error={error}",
                        plugin.path.display()
                    ));
                }
            }
            true
        }
        Err(error) => {
            plugin.failures += 1;
            diagnostics.push(format!(
                "Wasm callback failed path={} callback={callback} failures={} error={error:#}",
                plugin.path.display(),
                plugin.failures
            ));
            if plugin.failures >= MAX_FAILURES {
                if let Err(error) = plugin.deactivate(model::DeactivationReason::RepeatedFailure) {
                    diagnostics.push(format!(
                        "Wasm deactivation failed path={} error={error:#}",
                        plugin.path.display()
                    ));
                }
                plugin.disabled = true;
                plugin.tick = None;
                plugin.last_frame = api::UiFrame::default();
                diagnostics.push(format!(
                    "Wasm component disabled path={} reason=repeated-failures",
                    plugin.path.display()
                ));
            }
            false
        }
    }
}

/// Discovers display identities without compiling or instantiating components.
///
/// This keeps the startup panel useful while preserving the rule that untrusted
/// add-on code does not run until the game adapter reports live world state.
pub fn discover_addon_infos(directory: &Path) -> Vec<AddonInfo> {
    let mut candidates = discover_components(directory);
    candidates.sort();
    candidates
        .iter()
        .map(|path| read_addon_info(path))
        .collect()
}

pub(crate) fn create_engine() -> PluginResult<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    Engine::new(&config).map_err(|error| format!("create Wasmtime engine: {error}"))
}

fn monotonic_millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

fn has_event_work(batch: &api::EventBatch) -> bool {
    !batch.events.is_empty() || batch.dropped_before > 0 || batch.snapshot_required
}

fn empty_batch(process_session: u64) -> api::EventBatch {
    api::EventBatch {
        process_session,
        ..api::EventBatch::default()
    }
}

fn empty_batch_from(batch: &api::EventBatch) -> api::EventBatch {
    api::EventBatch {
        process_session: batch.process_session,
        first_sequence: None,
        next_sequence: batch.next_sequence,
        dropped_before: 0,
        snapshot_required: false,
        events: Vec::new(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TickSchedule {
    interval_ms: u32,
    next_due_ms: u64,
    last_delivered_ms: Option<u64>,
}

impl TickSchedule {
    fn new(requested_interval_ms: u32, now_ms: u64) -> (Self, bool) {
        let interval_ms = requested_interval_ms.clamp(MIN_TICK_INTERVAL_MS, MAX_TICK_INTERVAL_MS);
        (
            Self {
                interval_ms,
                next_due_ms: now_ms.saturating_add(u64::from(interval_ms)),
                last_delivered_ms: None,
            },
            interval_ms != requested_interval_ms,
        )
    }

    fn take_due(&mut self, now_ms: u64) -> Option<model::Tick> {
        if now_ms < self.next_due_ms {
            return None;
        }

        let interval_ms = u64::from(self.interval_ms);
        let missed = now_ms.saturating_sub(self.next_due_ms) / interval_ms;
        let scheduled_at_ms = self
            .next_due_ms
            .saturating_add(missed.saturating_mul(interval_ms));
        self.next_due_ms = scheduled_at_ms.saturating_add(interval_ms);
        let elapsed_ms = self
            .last_delivered_ms
            .map_or(interval_ms, |last| now_ms.saturating_sub(last));
        self.last_delivered_ms = Some(now_ms);

        Some(model::Tick {
            scheduled_at_ms,
            delivered_at_ms: now_ms,
            elapsed_ms,
            interval_ms: self.interval_ms,
            missed: u32::try_from(missed).unwrap_or(u32::MAX),
        })
    }
}

enum UiEffect {
    Unchanged,
    Replace(api::UiFrame),
    Clear,
}

struct CallbackEffects {
    scheduled_tick_interval_ms: Option<u32>,
    continue_ticking: bool,
    ui: UiEffect,
    outbound: Vec<StagedMessage>,
    chat: Vec<crate::chat_output::ChatOutput>,
}

struct HostCallbackEffects {
    scheduled_tick_interval_ms: Option<u32>,
    outbound: Vec<StagedMessage>,
    chat: Vec<crate::chat_output::ChatOutput>,
}

struct GuestInstance {
    store: Store<HostState>,
    bindings: FareverAddon,
}

#[derive(Clone)]
struct RegisteredProvider {
    guest: Weak<RefCell<GuestInstance>>,
    /// Service names this provider offers, every one served at `version`.
    services: HashSet<String>,
    /// The provider's own release version. Consumers therefore gate on the
    /// provider add-on's version instead of a second number per service.
    version: String,
}

#[derive(Default)]
struct ServiceDirectory {
    providers: HashMap<String, RegisteredProvider>,
}

impl ServiceDirectory {
    fn register(
        &mut self,
        addon_id: &str,
        version: &str,
        provides: &[String],
        guest: &Rc<RefCell<GuestInstance>>,
    ) -> PluginResult<()> {
        if self.providers.contains_key(addon_id) {
            return Err(format!("duplicate service provider add-on id {addon_id:?}"));
        }
        self.providers.insert(
            addon_id.to_owned(),
            RegisteredProvider {
                guest: Rc::downgrade(guest),
                services: provides.iter().cloned().collect(),
                version: version.to_owned(),
            },
        );
        Ok(())
    }

    fn unregister(&mut self, addon_id: &str) {
        self.providers.remove(addon_id);
    }
}

#[derive(Clone)]
struct OpenedService {
    provider_addon_id: String,
    service_id: String,
    version: String,
}

#[derive(Default)]
struct BusDirectory {
    active_addons: HashSet<String>,
    subscribers: HashMap<String, Vec<String>>,
}

impl BusDirectory {
    fn register(&mut self, addon_id: &str, topics: &[String]) -> PluginResult<()> {
        if !self.active_addons.insert(addon_id.to_owned()) {
            return Err(format!("duplicate add-on id {addon_id:?}"));
        }
        for topic in topics {
            self.subscribers
                .entry(topic.clone())
                .or_default()
                .push(addon_id.to_owned());
        }
        Ok(())
    }

    fn unregister(&mut self, addon_id: &str) {
        self.active_addons.remove(addon_id);
        self.subscribers.retain(|_, subscribers| {
            subscribers.retain(|subscriber| subscriber != addon_id);
            !subscribers.is_empty()
        });
    }

    fn recipients(
        &self,
        source_addon_id: &str,
        topic: &str,
        target: &model::MessageTarget,
    ) -> Vec<String> {
        let Some(subscribers) = self.subscribers.get(topic) else {
            return Vec::new();
        };
        match target {
            model::MessageTarget::Subscribers => subscribers
                .iter()
                .filter(|subscriber| subscriber.as_str() != source_addon_id)
                .cloned()
                .collect(),
            model::MessageTarget::Addon(addon_id) => subscribers
                .iter()
                .find(|subscriber| *subscriber == addon_id)
                .cloned()
                .into_iter()
                .collect(),
        }
    }
}

struct StagedMessage {
    source_addon_id: String,
    recipients: Vec<String>,
    topic: String,
    correlation_id: Option<u64>,
    payload: Vec<u8>,
}

#[derive(Clone)]
struct BusMessage {
    id: u64,
    monotonic_ms: u64,
    source_addon_id: Arc<str>,
    topic: Arc<str>,
    correlation_id: Option<u64>,
    payload: Arc<[u8]>,
}

impl BusMessage {
    fn to_wit(&self) -> model::AddonMessage {
        model::AddonMessage {
            id: self.id,
            monotonic_ms: self.monotonic_ms,
            source_addon_id: self.source_addon_id.to_string(),
            topic: self.topic.to_string(),
            correlation_id: self.correlation_id,
            payload: self.payload.to_vec(),
        }
    }
}

#[derive(Default)]
struct MessageQueue {
    messages: VecDeque<BusMessage>,
    payload_bytes: usize,
    dropped_before: u64,
}

impl MessageQueue {
    fn push(&mut self, message: BusMessage) {
        let payload_bytes = message.payload.len();
        if self.messages.len() >= MAX_PENDING_MESSAGES_PER_ADDON
            || self.payload_bytes.saturating_add(payload_bytes)
                > MAX_PENDING_PAYLOAD_BYTES_PER_ADDON
        {
            self.dropped_before = self.dropped_before.saturating_add(1);
            return;
        }
        self.payload_bytes = self.payload_bytes.saturating_add(payload_bytes);
        self.messages.push_back(message);
    }

    fn into_batch(self) -> (u64, Vec<model::AddonMessage>) {
        (
            self.dropped_before,
            self.messages
                .into_iter()
                .map(|message| message.to_wit())
                .collect(),
        )
    }
}

struct HostState {
    namespace: String,
    snapshot: model::CallbackSnapshot,
    config: ConfigRegistry,
    limits: StoreLimits,
    logs: Vec<String>,
    accepting_font_registrations: bool,
    accepting_image_registrations: bool,
    accepting_config_registrations: bool,
    font_bytes: usize,
    font_assets: Vec<api::FontAsset>,
    font_families: HashMap<api::TextStyle, String>,
    image_registration_attempts: usize,
    image_assets: Vec<api::ImageAsset>,
    bus_directory: Arc<Mutex<BusDirectory>>,
    service_directory: Rc<RefCell<ServiceDirectory>>,
    declared_dependencies: Vec<AddonDependency>,
    service_handles: Vec<OpenedService>,
    service_call_count: usize,
    subscriptions: Vec<String>,
    accepting_subscriptions: bool,
    accepting_publications: bool,
    accepting_tick_scheduling: bool,
    pending_tick_interval_ms: Option<u32>,
    outbound_messages: Vec<StagedMessage>,
    outbound_message_count: usize,
    outbound_payload_bytes: usize,
    accepting_chat_output: bool,
    chat_outputs: Vec<crate::chat_output::ChatOutput>,
    chat_output_count: usize,
    chat_output_code_units: usize,
}

impl HostState {
    fn new(
        namespace: String,
        snapshot: &api::GameSnapshot,
        bus_directory: Arc<Mutex<BusDirectory>>,
        service_directory: Rc<RefCell<ServiceDirectory>>,
        declared_dependencies: Vec<AddonDependency>,
        config: ConfigRegistry,
    ) -> Self {
        Self {
            namespace,
            snapshot: to_wit_snapshot(snapshot),
            config,
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_COMPONENT_MEMORY)
                .instances(8)
                .memories(4)
                .tables(8)
                .build(),
            logs: Vec::new(),
            accepting_font_registrations: true,
            accepting_image_registrations: true,
            accepting_config_registrations: true,
            font_bytes: 0,
            font_assets: Vec::new(),
            font_families: HashMap::new(),
            image_registration_attempts: 0,
            image_assets: Vec::new(),
            bus_directory,
            service_directory,
            declared_dependencies,
            service_handles: Vec::new(),
            service_call_count: 0,
            subscriptions: Vec::new(),
            accepting_subscriptions: true,
            accepting_publications: false,
            accepting_tick_scheduling: false,
            pending_tick_interval_ms: None,
            outbound_messages: Vec::new(),
            outbound_message_count: 0,
            outbound_payload_bytes: 0,
            accepting_chat_output: false,
            chat_outputs: Vec::new(),
            chat_output_count: 0,
            chat_output_code_units: 0,
        }
    }

    fn begin_callback(&mut self, snapshot: &api::GameSnapshot) {
        self.snapshot = to_wit_snapshot(snapshot);
        self.accepting_publications = true;
        self.accepting_tick_scheduling = true;
        self.pending_tick_interval_ms = None;
        self.outbound_messages.clear();
        self.outbound_message_count = 0;
        self.outbound_payload_bytes = 0;
        self.accepting_chat_output = true;
        self.chat_outputs.clear();
        self.chat_output_count = 0;
        self.chat_output_code_units = 0;
        self.service_call_count = 0;
    }

    fn finish_callback(&mut self, succeeded: bool) -> HostCallbackEffects {
        self.accepting_publications = false;
        self.accepting_tick_scheduling = false;
        self.outbound_message_count = 0;
        self.outbound_payload_bytes = 0;
        self.accepting_chat_output = false;
        self.chat_output_count = 0;
        self.chat_output_code_units = 0;
        if succeeded {
            HostCallbackEffects {
                scheduled_tick_interval_ms: self.pending_tick_interval_ms.take(),
                outbound: std::mem::take(&mut self.outbound_messages),
                chat: std::mem::take(&mut self.chat_outputs),
            }
        } else {
            self.pending_tick_interval_ms = None;
            self.outbound_messages.clear();
            self.chat_outputs.clear();
            HostCallbackEffects {
                scheduled_tick_interval_ms: None,
                outbound: Vec::new(),
                chat: Vec::new(),
            }
        }
    }

    fn stage_chat_output(&mut self, style: crate::chat_output::ChatOutputStyle, text: String) {
        if !self.accepting_chat_output {
            if self.logs.len() < 64 {
                self.logs.push(
                    "Warning: chat output is only accepted during an add-on callback".to_owned(),
                );
            }
            return;
        }
        let (text, truncated) = truncate_utf16(&text, crate::chat_output::MAX_CHAT_CODE_UNITS);
        let code_units = text.encode_utf16().count();
        let next_code_units = self.chat_output_code_units.saturating_add(code_units);
        if self.chat_output_count >= MAX_CHAT_OUTPUTS_PER_CALLBACK
            || next_code_units > MAX_CHAT_OUTPUT_CODE_UNITS_PER_CALLBACK
        {
            if self.logs.len() < 64 {
                self.logs.push(format!(
                    "Warning: chat output dropped because this callback exceeded {MAX_CHAT_OUTPUTS_PER_CALLBACK} messages or {MAX_CHAT_OUTPUT_CODE_UNITS_PER_CALLBACK} UTF-16 code units"
                ));
            }
            return;
        }
        if truncated && self.logs.len() < 64 {
            self.logs.push(format!(
                "Warning: chat output truncated to {} UTF-16 code units",
                crate::chat_output::MAX_CHAT_CODE_UNITS
            ));
        }
        self.chat_output_count += 1;
        self.chat_output_code_units = next_code_units;
        self.chat_outputs.push(crate::chat_output::ChatOutput {
            style,
            text,
            sender_name: None,
        });
    }
}

impl farever::addon::common::Host for HostState {}
impl farever::addon::events::Host for HostState {}
impl farever::addon::overlay::Host for HostState {}

impl farever::addon::dependencies::Host for HostState {
    fn open(
        &mut self,
        dependency: String,
        service: String,
    ) -> Result<model::ServiceHandle, model::OpenError> {
        if self.service_handles.len() >= MAX_SERVICE_HANDLES_PER_ADDON {
            return Err(model::OpenError::QuotaExceeded);
        }
        let declaration = self
            .declared_dependencies
            .iter()
            .find(|candidate| candidate.addon == dependency)
            .ok_or(model::OpenError::UndeclaredDependency)?;
        let required = declaration
            .services
            .iter()
            .find(|candidate| candidate.id == service)
            .ok_or(model::OpenError::UnavailableService)?;
        let directory = self.service_directory.borrow();
        let provider = directory
            .providers
            .get(&declaration.addon)
            .ok_or(model::OpenError::Unavailable)?;
        if !provider.services.contains(&service) {
            return Err(model::OpenError::UnavailableService);
        }
        // A service has no version of its own: it is served at its provider's
        // add-on version, which is what the consumer's requirement gates on.
        let version = &provider.version;
        if !service_version_matches(&required.version, version) {
            return Err(model::OpenError::IncompatibleVersion);
        }
        let id = u32::try_from(self.service_handles.len())
            .map_err(|_| model::OpenError::QuotaExceeded)?;
        self.service_handles.push(OpenedService {
            provider_addon_id: declaration.addon.clone(),
            service_id: service,
            version: version.clone(),
        });
        Ok(model::ServiceHandle {
            id,
            version: version.clone(),
        })
    }

    fn call(
        &mut self,
        service: model::ServiceHandle,
        operation: u32,
        request: Vec<u8>,
    ) -> Result<Vec<u8>, model::CallError> {
        if request.len() > MAX_SERVICE_PAYLOAD_BYTES {
            return Err(model::CallError::RequestTooLarge);
        }
        if self.service_call_count >= MAX_SERVICE_CALLS_PER_CALLBACK {
            return Err(model::CallError::QuotaExceeded);
        }
        let opened = self
            .service_handles
            .get(service.id as usize)
            .filter(|opened| opened.version == service.version)
            .cloned()
            .ok_or(model::CallError::InvalidHandle)?;
        self.service_call_count += 1;

        let provider = self
            .service_directory
            .borrow()
            .providers
            .get(&opened.provider_addon_id)
            .cloned()
            .ok_or(model::CallError::Unavailable)?;
        if !provider.services.contains(&opened.service_id) {
            return Err(model::CallError::Unavailable);
        }
        let guest = provider
            .guest
            .upgrade()
            .ok_or(model::CallError::Unavailable)?;
        let mut guest = guest.try_borrow_mut().map_err(|_| {
            model::CallError::ProviderFailed("provider is already executing".to_owned())
        })?;
        let response = guest.call_service(&opened.service_id, operation, &request)?;
        if response.len() > MAX_SERVICE_PAYLOAD_BYTES {
            return Err(model::CallError::ResponseTooLarge);
        }
        Ok(response)
    }
}

impl farever::addon::chat::Host for HostState {
    fn print(&mut self, text: String) {
        self.stage_chat_output(crate::chat_output::ChatOutputStyle::Normal, text);
    }

    fn print_error(&mut self, text: String) {
        self.stage_chat_output(crate::chat_output::ChatOutputStyle::Error, text);
    }
}

impl farever::addon::config::Host for HostState {
    fn register_property(
        &mut self,
        descriptor: model::ConfigPropertyDescriptor,
    ) -> Result<model::ConfigValue, String> {
        if !self.accepting_config_registrations {
            return Err(
                "config properties may only be registered during plugin.activate".to_owned(),
            );
        }
        self.config
            .register(from_wit_config_property(descriptor))
            .map(to_wit_config_value)
            .map_err(|error| error.to_string())
    }

    fn status(&mut self) -> model::ConfigStatus {
        to_wit_config_status(self.config.status())
    }

    fn get(&mut self, key: String) -> Result<model::ConfigValue, String> {
        self.config
            .get(&key)
            .map(to_wit_config_value)
            .map_err(|error| error.to_string())
    }

    fn set(
        &mut self,
        key: String,
        value: model::ConfigValue,
    ) -> Result<model::ConfigStatus, String> {
        let value = from_wit_config_value(value);
        self.config
            .set(key, value)
            .map(to_wit_config_status)
            .map_err(|error| error.to_string())
    }

    fn remove(&mut self, key: String) -> Result<model::ConfigStatus, String> {
        self.config
            .remove(key)
            .map(to_wit_config_status)
            .map_err(|error| error.to_string())
    }
}

impl farever::addon::game::Host for HostState {
    fn observation(&mut self) -> model::ObservationMetadata {
        self.snapshot.observation
    }

    fn session(&mut self) -> model::SessionState {
        self.snapshot.session
    }
}

impl farever::addon::player::Host for HostState {
    fn current(&mut self) -> model::PlayerSnapshot {
        self.snapshot.player.clone()
    }
}

impl farever::addon::party::Host for HostState {
    fn current(&mut self) -> model::PartySnapshot {
        self.snapshot.party.clone()
    }
}

impl farever::addon::camera::Host for HostState {
    fn current(&mut self) -> model::CameraSnapshot {
        self.snapshot.camera
    }
}

impl farever::addon::combat::Host for HostState {
    fn current(&mut self) -> model::CombatSnapshot {
        self.snapshot.combat.clone()
    }
}

impl farever::addon::instance_session::Host for HostState {
    fn current(&mut self) -> model::InstanceSnapshot {
        self.snapshot.instance_session.clone()
    }
}

impl farever::addon::zone::Host for HostState {
    fn current(&mut self) -> model::ZoneSnapshot {
        self.snapshot.zone.clone()
    }
}

impl farever::addon::windows::Host for HostState {
    fn current(&mut self) -> model::WindowsSnapshot {
        self.snapshot.windows.clone()
    }
}

impl farever::addon::runtime::Host for HostState {
    fn log(&mut self, level: model::LogLevel, message: String) {
        if self.logs.len() < 64 {
            let message = truncate_utf8(&message, 2048);
            self.logs.push(format!("{level:?}: {message}"));
        }
    }

    fn schedule_tick(&mut self, interval_ms: u32) -> u32 {
        let accepted = interval_ms.clamp(MIN_TICK_INTERVAL_MS, MAX_TICK_INTERVAL_MS);
        if self.accepting_tick_scheduling {
            self.pending_tick_interval_ms = Some(accepted);
        }
        accepted
    }
}

impl farever::addon::assets::Host for HostState {
    fn register_font(
        &mut self,
        id: String,
        styles: Vec<model::TextStyle>,
        bytes: Vec<u8>,
    ) -> Result<(), String> {
        if !self.accepting_font_registrations {
            return Err("fonts may only be registered during plugin.activate".to_owned());
        }
        let id = validate_font_id(&id)?;
        if styles.is_empty() {
            return Err(format!("font {id:?} must target at least one text style"));
        }
        if self.font_assets.len() >= api::MAX_FONTS_PER_ADDON {
            return Err(format!(
                "add-on may register at most {} font faces",
                api::MAX_FONTS_PER_ADDON
            ));
        }
        if self
            .font_assets
            .iter()
            .any(|asset| asset.family.ends_with(&format!("/{id}")))
        {
            return Err(format!("duplicate font id {id:?}"));
        }
        if bytes.is_empty() || bytes.len() > api::MAX_FONT_BYTES_PER_FACE {
            return Err(format!(
                "font {id:?} has {} bytes; expected 1..={}",
                bytes.len(),
                api::MAX_FONT_BYTES_PER_FACE
            ));
        }
        let total_bytes = self.font_bytes.saturating_add(bytes.len());
        if total_bytes > api::MAX_FONT_BYTES_PER_ADDON {
            return Err(format!(
                "registered fonts total {total_bytes} bytes; limit is {}",
                api::MAX_FONT_BYTES_PER_ADDON
            ));
        }
        ttf_parser::Face::parse(&bytes, 0)
            .map_err(|error| format!("font {id:?} is not a supported OpenType face: {error:?}"))?;

        let mut unique_styles = Vec::with_capacity(styles.len());
        for style in styles.into_iter().map(from_wit_text_style) {
            if unique_styles.contains(&style) {
                continue;
            }
            if self.font_families.contains_key(&style) {
                return Err(format!(
                    "text style {style:?} already has a registered font"
                ));
            }
            unique_styles.push(style);
        }

        let family = format!("addon-font/{}/{id}", self.namespace);
        for style in unique_styles {
            self.font_families.insert(style, family.clone());
        }
        self.font_bytes = total_bytes;
        self.font_assets.push(api::FontAsset {
            family,
            bytes: Arc::from(bytes),
        });
        Ok(())
    }

    fn register_image(&mut self, id: String, png: Vec<u8>) -> Result<model::ImageRef, String> {
        if !self.accepting_image_registrations {
            return Err("images may only be registered during plugin.activate".to_owned());
        }
        let id = validate_image_id(&id)?;
        if self.image_registration_attempts >= api::MAX_IMAGES_PER_ADDON {
            return Err(format!(
                "add-on may attempt at most {} image registrations",
                api::MAX_IMAGES_PER_ADDON
            ));
        }
        let namespaced_id = format!("addon-image/{}/{id}", self.namespace);
        if self
            .image_assets
            .iter()
            .any(|asset| asset.id == namespaced_id)
        {
            return Err(format!("duplicate image id {id:?}"));
        }
        if png.is_empty() || png.len() > api::MAX_IMAGE_ENCODED_BYTES {
            return Err(format!(
                "image {id:?} has {} encoded bytes; expected 1..={}",
                png.len(),
                api::MAX_IMAGE_ENCODED_BYTES
            ));
        }
        self.image_registration_attempts += 1;

        let mut reader = ImageReader::new(Cursor::new(png));
        reader.set_format(ImageFormat::Png);
        let mut limits = Limits::default();
        limits.max_image_width = Some(api::MAX_IMAGE_EDGE);
        limits.max_image_height = Some(api::MAX_IMAGE_EDGE);
        limits.max_alloc = Some(api::MAX_IMAGE_DECODED_BYTES_PER_IMAGE as u64);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|error| format!("image {id:?} is not a supported PNG: {error}"))?
            .into_rgba8();
        let reference = model::ImageRef {
            id: namespaced_id.clone(),
        };
        self.image_assets.push(api::ImageAsset {
            id: namespaced_id,
            width: image.width(),
            height: image.height(),
            rgba: Arc::from(image.into_raw()),
        });
        Ok(reference)
    }
}

impl farever::addon::bus::Host for HostState {
    fn subscribe(&mut self, topic: String) -> Result<(), model::MessageError> {
        if !self.accepting_subscriptions {
            return Err(model::MessageError::WrongLifecyclePhase);
        }
        validate_topic(&topic)?;
        if self.subscriptions.iter().any(|current| current == &topic) {
            return Ok(());
        }
        if self.subscriptions.len() >= MAX_SUBSCRIPTIONS_PER_ADDON {
            return Err(model::MessageError::TooManySubscriptions);
        }
        self.subscriptions.push(topic);
        Ok(())
    }

    fn publish(
        &mut self,
        topic: String,
        target: model::MessageTarget,
        correlation_id: Option<u64>,
        payload: Vec<u8>,
    ) -> Result<u32, model::MessageError> {
        if !self.accepting_publications {
            return Err(model::MessageError::WrongLifecyclePhase);
        }
        validate_topic(&topic)?;
        if payload.len() > MAX_MESSAGE_PAYLOAD_BYTES {
            return Err(model::MessageError::PayloadTooLarge);
        }
        let next_payload_bytes = self.outbound_payload_bytes.saturating_add(payload.len());
        if self.outbound_message_count >= MAX_OUTBOUND_MESSAGES_PER_CALLBACK
            || next_payload_bytes > MAX_OUTBOUND_PAYLOAD_BYTES_PER_CALLBACK
        {
            return Err(model::MessageError::QuotaExceeded);
        }

        self.outbound_message_count += 1;
        self.outbound_payload_bytes = next_payload_bytes;
        let recipients = self
            .bus_directory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recipients(&self.namespace, &topic, &target);
        let recipient_count = u32::try_from(recipients.len()).unwrap_or(u32::MAX);
        if !recipients.is_empty() {
            self.outbound_messages.push(StagedMessage {
                source_addon_id: self.namespace.clone(),
                recipients,
                topic,
                correlation_id,
                payload,
            });
        }
        Ok(recipient_count)
    }
}

impl GuestInstance {
    fn call_service(
        &mut self,
        service: &str,
        operation: u32,
        request: &[u8],
    ) -> Result<Vec<u8>, model::CallError> {
        let Self { store, bindings } = self;
        store.set_fuel(FUEL_PER_CALLBACK).map_err(|error| {
            model::CallError::ProviderFailed(format!("set service-call fuel: {error}"))
        })?;
        match bindings
            .farever_addon_plugin()
            .call_call_service(store, service, operation, request)
        {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => Err(model::CallError::ProviderError(truncate_utf8(&error, 2048))),
            Err(error) => Err(model::CallError::ProviderFailed(truncate_utf8(
                &error.to_string(),
                2048,
            ))),
        }
    }
}

struct Plugin {
    path: PathBuf,
    namespace: String,
    info: AddonInfo,
    chat_name: String,
    guest: Rc<RefCell<GuestInstance>>,
    /// Service names this add-on provides, all served at its own `version`.
    provides: Vec<String>,
    /// Manifest-declared release version: the version of this add-on and of
    /// every service it provides.
    version: String,
    failures: u32,
    disabled: bool,
    deactivated: bool,
    tick: Option<TickSchedule>,
    logs: Vec<String>,
    last_frame: api::UiFrame,
    font_assets: Vec<api::FontAsset>,
    font_families: HashMap<api::TextStyle, String>,
    image_assets: Vec<api::ImageAsset>,
    config_properties: Vec<api::ConfigPropertyDescriptor>,
}

/// Fails activation with a player-readable error when a required dependency
/// cannot be satisfied. Optional dependencies are the consumer's own
/// responsibility: the guest degrades gracefully instead of failing.
fn check_required_dependencies(
    dependencies: &[AddonDependency],
    directory: &ServiceDirectory,
) -> PluginResult<()> {
    for dependency in dependencies.iter().filter(|candidate| !candidate.optional) {
        let Some(provider) = directory.providers.get(&dependency.addon) else {
            return Err(format!(
                "missing required dependency {:?}: add-on is unavailable",
                dependency.addon
            ));
        };
        for required in &dependency.services {
            if !provider.services.contains(&required.id) {
                return Err(format!(
                    "missing required service {:?} from add-on {:?}",
                    required.id, dependency.addon
                ));
            }
            if !service_version_matches(&required.version, &provider.version) {
                return Err(format!(
                    "incompatible service {:?} version {:?} from add-on {:?}: required {:?}",
                    required.id, provider.version, dependency.addon, required.version
                ));
            }
        }
    }
    Ok(())
}

fn service_version_matches(requirement: &str, provided: &str) -> bool {
    let required_major = requirement.trim_start_matches('^').split('.').next();
    let provided_major = provided.split('.').next();
    required_major.is_some() && required_major == provided_major
}

/// One validated and compiled component that has not executed guest code.
///
/// Compilation is intentionally separate from activation so an add-on manager
/// can do the expensive Wasmtime work on a compiler thread, then move the
/// resulting component to the serialized runtime worker for activation.
pub(crate) struct CompiledPlugin {
    path: PathBuf,
    namespace: String,
    info: AddonInfo,
    manifest: AddonManifest,
    component: Component,
}

impl CompiledPlugin {
    #[cfg(test)]
    pub(crate) fn from_path(engine: &Engine, path: &Path) -> PluginResult<Self> {
        let bytes = fs::read(path)
            .map_err(|error| format!("read component {}: {error}", path.display()))?;
        Self::from_bytes(engine, path, &bytes)
    }

    pub(crate) fn from_bytes(engine: &Engine, path: &Path, bytes: &[u8]) -> PluginResult<Self> {
        let fallback_namespace = unit_name(path).unwrap_or("addon").to_owned();
        let mut manifest = read_addon_manifest(path)?;
        let namespace = if manifest.id.is_empty() {
            fallback_namespace
        } else {
            manifest.id.clone()
        };
        if namespace == HOST_MESSAGE_SOURCE_ID {
            return Err(format!(
                "add-on id {namespace:?} is reserved for host-originated messages"
            ));
        }
        // The declared API version is checked before anything expensive happens:
        // an add-on built for another API can never link, and saying so here
        // costs one string comparison instead of a failed compilation.
        let declared =
            declared_api_version(&manifest).map_err(|error| format!("{namespace}: {error}"))?;
        if let Some(declared) = declared {
            require_compatible_api(&namespace, declared)?;
        }
        verify_component_fingerprint(&manifest, path, bytes)?;
        let component = Component::from_binary(engine, bytes)
            .map_err(|error| format!("compile component {}: {error}", path.display()))?;
        // The component's own imports are the authority: a manifest cannot
        // claim an API the component was not built for.
        let built_for = component_api_version(&component, engine);
        if let (Some(declared), Some(built_for)) = (declared, built_for) {
            if declared != built_for {
                return Err(format!(
                    "{namespace}: {MANIFEST_FILE_NAME} declares api-version {declared} but the \
                     component was built for {built_for}"
                ));
            }
        }
        if let Some(built_for) = built_for {
            require_compatible_api(&namespace, built_for)?;
        }
        manifest.id = namespace.clone();
        Ok(Self {
            path: path.to_owned(),
            namespace,
            info: read_addon_info_bytes(path, bytes),
            manifest,
            component,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn info(&self) -> &AddonInfo {
        &self.info
    }

    pub(crate) fn manifest(&self) -> &AddonManifest {
        &self.manifest
    }
}

struct PluginLoadContext<'a> {
    config_directory: &'a Path,
    snapshot: &'a api::GameSnapshot,
    instance_id: u64,
    clock_started: Instant,
    epoch_ms: u64,
    bus_directory: Arc<Mutex<BusDirectory>>,
    service_directory: Rc<RefCell<ServiceDirectory>>,
}

impl Plugin {
    #[cfg(test)]
    fn load(engine: &Engine, path: &Path, context: PluginLoadContext<'_>) -> PluginResult<Self> {
        let compiled = CompiledPlugin::from_path(engine, path)?;
        Self::activate(engine, compiled, context)
    }

    pub(crate) fn activate(
        engine: &Engine,
        compiled: CompiledPlugin,
        context: PluginLoadContext<'_>,
    ) -> PluginResult<Self> {
        let PluginLoadContext {
            config_directory,
            snapshot,
            instance_id,
            clock_started,
            epoch_ms,
            bus_directory,
            service_directory,
        } = context;
        let CompiledPlugin {
            path,
            namespace,
            info,
            manifest,
            component,
        } = compiled;
        let chat_name = manifest
            .name
            .as_deref()
            .map(sanitize_addon_name)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| info.name.clone());
        check_required_dependencies(&manifest.dependencies, &service_directory.borrow())?;
        let config = ConfigRegistry::open(config_directory, &namespace, info.version.as_deref())
            .map_err(|error| format!("open configuration for {namespace:?}: {error}"))?;
        let mut linker = Linker::new(engine);
        FareverAddon::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(|error| format!("link farever:addon host imports: {error}"))?;
        let mut store = Store::new(
            engine,
            HostState::new(
                namespace.clone(),
                snapshot,
                Arc::clone(&bus_directory),
                Rc::clone(&service_directory),
                manifest.dependencies.clone(),
                config,
            ),
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(FUEL_PER_ACTIVATION)
            .map_err(|error| format!("set activation fuel: {error}"))?;
        let bindings = FareverAddon::instantiate(&mut store, &component, &linker)
            .map_err(|error| format!("instantiate component: {error}"))?;
        let activation_now_ms = epoch_ms.saturating_add(monotonic_millis(clock_started.elapsed()));
        store.data_mut().accepting_tick_scheduling = true;
        store.data_mut().pending_tick_interval_ms = None;
        let activation = bindings
            .farever_addon_plugin()
            .call_activate(
                &mut store,
                &model::ActivationContext {
                    addon_id: namespace.clone(),
                    addon_version: info.version.clone(),
                    instance_id,
                    monotonic_ms: activation_now_ms,
                },
            )
            .map_err(|error| format!("call plugin.activate: {error}"))?
            .map_err(|error| format!("plugin.activate rejected load: {error}"))?;
        store.data_mut().accepting_font_registrations = false;
        store.data_mut().accepting_image_registrations = false;
        store.data_mut().accepting_config_registrations = false;
        store.data_mut().accepting_subscriptions = false;
        store.data_mut().accepting_tick_scheduling = false;
        let logs = std::mem::take(&mut store.data_mut().logs);
        let font_assets = store.data().font_assets.clone();
        let font_families = store.data().font_families.clone();
        let image_assets = store.data().image_assets.clone();
        let subscriptions = store.data().subscriptions.clone();
        let config_properties = store
            .data()
            .config
            .properties()
            .iter()
            .map(|descriptor| to_api_config_property(&namespace, descriptor))
            .collect();
        let last_frame = match validate_ui_update(activation.ui, &namespace, &font_families)? {
            UiEffect::Replace(frame) => frame,
            UiEffect::Unchanged | UiEffect::Clear => api::UiFrame::default(),
        };
        let schedule_now_ms = epoch_ms.saturating_add(monotonic_millis(clock_started.elapsed()));
        let tick = store
            .data_mut()
            .pending_tick_interval_ms
            .take()
            .map(|interval_ms| TickSchedule::new(interval_ms, schedule_now_ms).0);
        bus_directory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .register(&namespace, &subscriptions)?;
        Ok(Self {
            path,
            namespace,
            info,
            chat_name,
            guest: Rc::new(RefCell::new(GuestInstance { store, bindings })),
            provides: manifest.provides,
            version: manifest.version,
            failures: 0,
            disabled: false,
            deactivated: false,
            tick,
            logs,
            last_frame,
            font_assets,
            font_families,
            image_assets,
            config_properties,
        })
    }

    fn on_ui_event(
        &mut self,
        snapshot: &api::GameSnapshot,
        event: &api::UiEvent,
    ) -> PluginResult<CallbackEffects> {
        let mut guest = self.guest.borrow_mut();
        let GuestInstance { store, bindings } = &mut *guest;
        store
            .set_fuel(FUEL_PER_CALLBACK)
            .map_err(|error| format!("set on-ui-event fuel: {error}"))?;
        store.data_mut().begin_callback(snapshot);
        let event = to_wit_ui_event(event);
        let result = bindings
            .farever_addon_plugin()
            .call_on_ui_event(&mut *store, &event);
        self.logs.append(&mut store.data_mut().logs);
        let output = match result {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                store.data_mut().finish_callback(false);
                return Err(format!("plugin.on-ui-event returned error: {error}"));
            }
            Err(error) => {
                store.data_mut().finish_callback(false);
                return Err(format!("call plugin.on-ui-event: {error}"));
            }
        };
        let ui = match validate_ui_update(output.ui, &self.namespace, &self.font_families) {
            Ok(ui) => ui,
            Err(error) => {
                store.data_mut().finish_callback(false);
                return Err(error);
            }
        };
        let host_effects = store.data_mut().finish_callback(true);
        Ok(CallbackEffects {
            scheduled_tick_interval_ms: host_effects.scheduled_tick_interval_ms,
            continue_ticking: true,
            ui,
            outbound: host_effects.outbound,
            chat: host_effects.chat,
        })
    }

    fn on_event(
        &mut self,
        snapshot: &api::GameSnapshot,
        events: &api::EventBatch,
    ) -> PluginResult<CallbackEffects> {
        let mut guest = self.guest.borrow_mut();
        let GuestInstance { store, bindings } = &mut *guest;
        store
            .set_fuel(FUEL_PER_CALLBACK)
            .map_err(|error| format!("set on-event fuel: {error}"))?;
        store.data_mut().begin_callback(snapshot);
        let batch = to_wit_batch(events);
        let result = bindings
            .farever_addon_plugin()
            .call_on_event(&mut *store, &batch);
        self.logs.append(&mut store.data_mut().logs);
        let output = match result {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                store.data_mut().finish_callback(false);
                return Err(format!("plugin.on-event returned error: {error}"));
            }
            Err(error) => {
                store.data_mut().finish_callback(false);
                return Err(format!("call plugin.on-event: {error}"));
            }
        };
        let ui = match validate_ui_update(output.ui, &self.namespace, &self.font_families) {
            Ok(ui) => ui,
            Err(error) => {
                store.data_mut().finish_callback(false);
                return Err(error);
            }
        };
        let host_effects = store.data_mut().finish_callback(true);
        Ok(CallbackEffects {
            scheduled_tick_interval_ms: host_effects.scheduled_tick_interval_ms,
            continue_ticking: true,
            ui,
            outbound: host_effects.outbound,
            chat: host_effects.chat,
        })
    }

    fn on_message(
        &mut self,
        snapshot: &api::GameSnapshot,
        dropped_before: u64,
        messages: &[model::AddonMessage],
    ) -> PluginResult<CallbackEffects> {
        let mut guest = self.guest.borrow_mut();
        let GuestInstance { store, bindings } = &mut *guest;
        store
            .set_fuel(FUEL_PER_CALLBACK)
            .map_err(|error| format!("set on-message fuel: {error}"))?;
        store.data_mut().begin_callback(snapshot);
        let result =
            bindings
                .farever_addon_plugin()
                .call_on_message(&mut *store, dropped_before, messages);
        self.logs.append(&mut store.data_mut().logs);
        let output = match result {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                store.data_mut().finish_callback(false);
                return Err(format!("plugin.on-message returned error: {error}"));
            }
            Err(error) => {
                store.data_mut().finish_callback(false);
                return Err(format!("call plugin.on-message: {error}"));
            }
        };
        let ui = match validate_ui_update(output.ui, &self.namespace, &self.font_families) {
            Ok(ui) => ui,
            Err(error) => {
                store.data_mut().finish_callback(false);
                return Err(error);
            }
        };
        let host_effects = store.data_mut().finish_callback(true);
        Ok(CallbackEffects {
            scheduled_tick_interval_ms: host_effects.scheduled_tick_interval_ms,
            continue_ticking: true,
            ui,
            outbound: host_effects.outbound,
            chat: host_effects.chat,
        })
    }

    fn on_tick(
        &mut self,
        snapshot: &api::GameSnapshot,
        tick: model::Tick,
    ) -> PluginResult<CallbackEffects> {
        let mut guest = self.guest.borrow_mut();
        let GuestInstance { store, bindings } = &mut *guest;
        store
            .set_fuel(FUEL_PER_CALLBACK)
            .map_err(|error| format!("set on-tick fuel: {error}"))?;
        store.data_mut().begin_callback(snapshot);
        let result = bindings
            .farever_addon_plugin()
            .call_on_tick(&mut *store, tick);
        self.logs.append(&mut store.data_mut().logs);
        let output = match result {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                store.data_mut().finish_callback(false);
                return Err(format!("plugin.on-tick returned error: {error}"));
            }
            Err(error) => {
                store.data_mut().finish_callback(false);
                return Err(format!("call plugin.on-tick: {error}"));
            }
        };
        let ui = match validate_ui_update(output.ui, &self.namespace, &self.font_families) {
            Ok(ui) => ui,
            Err(error) => {
                store.data_mut().finish_callback(false);
                return Err(error);
            }
        };
        let host_effects = store.data_mut().finish_callback(true);
        Ok(CallbackEffects {
            scheduled_tick_interval_ms: host_effects.scheduled_tick_interval_ms,
            continue_ticking: output.continue_ticking,
            ui,
            outbound: host_effects.outbound,
            chat: host_effects.chat,
        })
    }

    fn take_due_tick(&mut self, now_ms: u64) -> Option<model::Tick> {
        self.tick.as_mut().and_then(|tick| tick.take_due(now_ms))
    }

    fn apply_tick_effect(
        &mut self,
        scheduled_tick_interval_ms: Option<u32>,
        continue_ticking: bool,
        now_ms: u64,
    ) -> Option<String> {
        if let Some(interval_ms) = scheduled_tick_interval_ms {
            let (tick, _) = TickSchedule::new(interval_ms, now_ms);
            self.tick = Some(tick);
            return Some(format!(
                "Wasm tick registered path={} interval_ms={interval_ms}",
                self.path.display()
            ));
        }
        if !continue_ticking {
            self.tick = None;
            return Some(format!("Wasm tick disabled path={}", self.path.display()));
        }
        None
    }

    fn deactivate(&mut self, reason: model::DeactivationReason) -> PluginResult<()> {
        if self.deactivated {
            return Ok(());
        }
        self.deactivated = true;
        let mut guest = self.guest.borrow_mut();
        let GuestInstance { store, bindings } = &mut *guest;
        store.data_mut().accepting_publications = false;
        let result = store
            .set_fuel(FUEL_PER_DEACTIVATION)
            .map_err(|error| format!("set deactivation fuel: {error}"))
            .and_then(|()| {
                bindings
                    .farever_addon_plugin()
                    .call_deactivate(&mut *store, reason)
                    .map_err(|error| format!("call plugin.deactivate: {error}"))
            });
        self.logs.append(&mut store.data_mut().logs);
        store
            .data()
            .bus_directory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unregister(&self.namespace);
        store
            .data()
            .service_directory
            .borrow_mut()
            .unregister(&self.namespace);
        result
    }

    fn take_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.logs)
    }
}

fn read_addon_info(path: &Path) -> AddonInfo {
    let fallback_name = unit_name(path)
        .filter(|name| !name.is_empty())
        .map_or_else(|| "addon".to_owned(), sanitize_addon_name);
    let Ok(bytes) = fs::read(path) else {
        return AddonInfo {
            name: fallback_name,
            version: None,
        };
    };
    read_addon_info_bytes(path, &bytes)
}

pub(crate) fn read_addon_manifest(path: &Path) -> PluginResult<AddonManifest> {
    let manifest_path = path.with_file_name(MANIFEST_FILE_NAME);
    let bytes = match fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AddonManifest::default());
        }
        Err(error) => {
            return Err(format!(
                "read add-on manifest {}: {error}",
                manifest_path.display()
            ));
        }
    };
    // Load-time stays lenient on purpose: the loader reads the fields it
    // needs and ignores the rest. Strict shape checks belong to installers,
    // which run `AddonManifest::validate`.
    let manifest: AddonManifest = farever_more_manifest::parse(&bytes)
        .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
    if manifest.id == HOST_MESSAGE_SOURCE_ID {
        return Err(format!("add-on id {:?} is reserved", manifest.id));
    }
    if !manifest.id.is_empty() && !is_valid_name(&manifest.id) {
        return Err(format!("invalid add-on id {:?}", manifest.id));
    }
    for service in &manifest.provides {
        if !is_valid_name(service) {
            return Err(format!("invalid provided service {service:?}"));
        }
    }
    for dependency in &manifest.dependencies {
        if !is_valid_name(&dependency.addon) {
            return Err(format!("invalid dependency add-on {:?}", dependency.addon));
        }
    }
    Ok(manifest)
}

/// Reads the add-on API version a manifest declares, if it declares one.
///
/// A source tree and a hand-built component declare nothing; the component's
/// own imports still have to agree with this host before it runs.
fn declared_api_version(manifest: &AddonManifest) -> PluginResult<Option<ApiVersion>> {
    let Some(declared) = manifest.api_version.as_deref() else {
        return Ok(None);
    };
    let source = format!("{MANIFEST_FILE_NAME} api-version");
    farever_more_manifest::api::parse_api_version(declared, &source).map(Some)
}

/// The add-on API version a component was built against, read from the
/// versioned `farever:addon/…@MAJOR.MINOR.PATCH` names in its own type.
///
/// The component is the authority here: the version is part of every interface
/// name it imports and of the plugin interface it exports, so no manifest can
/// pretend a component targets an API it does not. A component with no
/// `farever:addon` names at all is not an add-on component and reports nothing;
/// instantiation explains that case better than a version message would.
fn component_api_version(component: &Component, engine: &Engine) -> Option<ApiVersion> {
    let component_type = component.component_type();
    let names = component_type
        .imports(engine)
        .map(|(name, _)| name)
        .chain(component_type.exports(engine).map(|(name, _)| name));
    let mut highest: Option<ApiVersion> = None;
    for name in names {
        let Some(version) = farever_addon_api_version(name) else {
            continue;
        };
        if highest.is_none_or(|current| version > current) {
            highest = Some(version);
        }
    }
    highest
}

/// Extracts the version from one interface name, e.g. `1.0.0` from
/// `farever:addon/dependencies@1.0.0`.
fn farever_addon_api_version(name: &str) -> Option<ApiVersion> {
    let interface = name.strip_prefix("farever:addon/")?;
    let (_, version) = interface.rsplit_once('@')?;
    ApiVersion::parse(version)
}

/// Refuses a component whose API this host cannot satisfy.
fn require_compatible_api(namespace: &str, required: ApiVersion) -> PluginResult<()> {
    farever_more_manifest::api::compatibility(required, ApiVersion::host())
        .map_err(|error| format!("{namespace}: {error}"))
}

/// Rejects component bytes that do not match the fingerprint their manifest
/// declares.
///
/// Installed units are the ones the pack step stamped, so a mismatch means the
/// file changed on disk after it was installed. Units that declare nothing - a
/// source tree, or a folder with no manifest at all - are accepted: the runtime
/// verifies a claim that was made, it never invents one. Verification runs
/// before compilation, so tampered bytes never reach Wasmtime.
fn verify_component_fingerprint(
    manifest: &AddonManifest,
    path: &Path,
    bytes: &[u8],
) -> PluginResult<()> {
    if manifest.sha256.is_empty() {
        return Ok(());
    }
    let digest = hex::encode(Sha256::digest(bytes));
    if !digest.eq_ignore_ascii_case(&manifest.sha256) {
        return Err(format!(
            "{} does not match the fingerprint in {}: expected {}, computed {digest}",
            path.display(),
            MANIFEST_FILE_NAME,
            manifest.sha256
        ));
    }
    Ok(())
}

pub(crate) fn order_components_by_dependencies(candidates: &mut Vec<PathBuf>) {
    let manifests = candidates
        .iter()
        .map(|path| {
            let manifest = read_addon_manifest(path).unwrap_or_default();
            let id = if manifest.id.is_empty() {
                unit_name(path).unwrap_or("addon").to_owned()
            } else {
                manifest.id.clone()
            };
            (path.clone(), id, manifest.dependencies)
        })
        .collect::<Vec<_>>();
    let paths_by_id = manifests
        .iter()
        .map(|(path, id, _)| (id.clone(), path.clone()))
        .collect::<HashMap<_, _>>();
    let mut remaining = manifests
        .into_iter()
        .map(|(path, id, dependencies)| {
            let dependencies = dependencies
                .into_iter()
                .filter_map(|dependency| paths_by_id.get(&dependency.addon).cloned())
                .collect::<HashSet<_>>();
            (path, id, dependencies)
        })
        .collect::<Vec<_>>();
    let mut ordered = Vec::with_capacity(remaining.len());
    let mut emitted = HashSet::new();
    while !remaining.is_empty() {
        remaining.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)));
        let Some(index) = remaining
            .iter()
            .position(|(_, _, dependencies)| dependencies.is_subset(&emitted))
        else {
            remaining.sort_by(|left, right| left.0.cmp(&right.0));
            ordered.extend(remaining.drain(..).map(|(path, _, _)| path));
            break;
        };
        let (path, _, _) = remaining.remove(index);
        emitted.insert(path.clone());
        ordered.push(path);
    }
    *candidates = ordered;
}

fn read_addon_info_bytes(path: &Path, bytes: &[u8]) -> AddonInfo {
    let fallback_name = unit_name(path)
        .filter(|name| !name.is_empty())
        .map_or_else(|| "addon".to_owned(), sanitize_addon_name);
    let Ok(payload) = Payload::from_binary(bytes) else {
        return AddonInfo {
            name: fallback_name,
            version: None,
        };
    };
    let metadata = payload.metadata();
    let name = metadata
        .name
        .as_deref()
        .map(sanitize_addon_name)
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback_name);
    AddonInfo {
        name,
        version: metadata
            .version
            .as_ref()
            .map(ToString::to_string)
            .map(|version| sanitize_metadata(&version, MAX_ADDON_VERSION_BYTES))
            .filter(|version| !version.is_empty()),
    }
}

fn sanitize_addon_name(name: &str) -> String {
    sanitize_metadata(name, MAX_ADDON_NAME_BYTES)
}

fn sanitize_metadata(value: &str, capacity: usize) -> String {
    let printable = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    truncate_utf8(printable.trim(), capacity)
}

fn validate_topic(topic: &str) -> Result<(), model::MessageError> {
    let bytes = topic.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_TOPIC_BYTES {
        return Err(model::MessageError::InvalidTopic);
    }
    if !bytes.first().is_some_and(u8::is_ascii_lowercase)
        || !bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        || !bytes.iter().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'-' | b'_' | b'/' | b'@')
        })
        || topic.contains("//")
    {
        return Err(model::MessageError::InvalidTopic);
    }
    Ok(())
}

fn validate_font_id(id: &str) -> PluginResult<String> {
    if id.is_empty() || id.len() > 64 {
        return Err("font id must contain 1..=64 ASCII bytes".to_owned());
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "font id {id:?} may contain only ASCII letters, digits, '-', '_', and '.'"
        ));
    }
    Ok(id.to_owned())
}

fn validate_image_id(id: &str) -> PluginResult<String> {
    if id.is_empty() || id.len() > 64 {
        return Err("image id must contain 1..=64 ASCII bytes".to_owned());
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "image id {id:?} may contain only ASCII letters, digits, '-', '_', and '.'"
        ));
    }
    Ok(id.to_owned())
}

fn from_wit_config_property(
    descriptor: model::ConfigPropertyDescriptor,
) -> ConfigPropertyDescriptor {
    ConfigPropertyDescriptor {
        key: descriptor.key,
        label: descriptor.label,
        description: descriptor.description,
        value_kind: match descriptor.value_kind {
            model::ConfigValueKind::Boolean => ConfigValueKind::Boolean,
            model::ConfigValueKind::Integer => ConfigValueKind::Integer,
            model::ConfigValueKind::Number => ConfigValueKind::Number,
            model::ConfigValueKind::Text => ConfigValueKind::Text,
            model::ConfigValueKind::Bytes => ConfigValueKind::Bytes,
        },
        default_value: from_wit_config_value(descriptor.default_value),
        access: match descriptor.access {
            model::ConfigPropertyAccess::Editable => ConfigPropertyAccess::Editable,
            model::ConfigPropertyAccess::Readonly => ConfigPropertyAccess::Readonly,
            model::ConfigPropertyAccess::Hidden => ConfigPropertyAccess::Hidden,
        },
    }
}

fn to_api_config_property(
    owner: &str,
    descriptor: &ConfigPropertyDescriptor,
) -> api::ConfigPropertyDescriptor {
    api::ConfigPropertyDescriptor {
        owner: owner.to_owned(),
        key: descriptor.key.clone(),
        label: descriptor.label.clone(),
        description: descriptor.description.clone(),
        value_kind: match descriptor.value_kind {
            ConfigValueKind::Boolean => api::ConfigValueKind::Boolean,
            ConfigValueKind::Integer => api::ConfigValueKind::Integer,
            ConfigValueKind::Number => api::ConfigValueKind::Number,
            ConfigValueKind::Text => api::ConfigValueKind::Text,
            ConfigValueKind::Bytes => api::ConfigValueKind::Bytes,
        },
        default_value: to_api_config_value(&descriptor.default_value),
        access: match descriptor.access {
            ConfigPropertyAccess::Editable => api::ConfigPropertyAccess::Editable,
            ConfigPropertyAccess::Readonly => api::ConfigPropertyAccess::Readonly,
            ConfigPropertyAccess::Hidden => api::ConfigPropertyAccess::Hidden,
        },
    }
}

fn to_api_config_value(value: &ConfigValue) -> api::ConfigValue {
    match value {
        ConfigValue::Boolean(value) => api::ConfigValue::Boolean(*value),
        ConfigValue::Integer(value) => api::ConfigValue::Integer(*value),
        ConfigValue::Number(value) => api::ConfigValue::Number(*value),
        ConfigValue::Text(value) => api::ConfigValue::Text(value.clone()),
        ConfigValue::Bytes(value) => api::ConfigValue::Bytes(value.clone()),
    }
}

fn from_wit_config_value(value: model::ConfigValue) -> ConfigValue {
    match value {
        model::ConfigValue::Boolean(value) => ConfigValue::Boolean(value),
        model::ConfigValue::Integer(value) => ConfigValue::Integer(value),
        model::ConfigValue::Number(value) => ConfigValue::Number(value),
        model::ConfigValue::Text(value) => ConfigValue::Text(value),
        model::ConfigValue::Bytes(value) => ConfigValue::Bytes(value),
    }
}

fn to_wit_config_value(value: ConfigValue) -> model::ConfigValue {
    match value {
        ConfigValue::Boolean(value) => model::ConfigValue::Boolean(value),
        ConfigValue::Integer(value) => model::ConfigValue::Integer(value),
        ConfigValue::Number(value) => model::ConfigValue::Number(value),
        ConfigValue::Text(value) => model::ConfigValue::Text(value),
        ConfigValue::Bytes(value) => model::ConfigValue::Bytes(value),
    }
}

fn to_wit_config_status(status: ConfigStatus) -> model::ConfigStatus {
    model::ConfigStatus {
        revision: status.revision,
        saved_by_addon_version: status.saved_by_addon_version,
        used_bytes: status.used_bytes,
        quota_bytes: status.quota_bytes,
    }
}

fn from_wit_text_style(style: model::TextStyle) -> api::TextStyle {
    match style {
        model::TextStyle::Body => api::TextStyle::Body,
        model::TextStyle::Small => api::TextStyle::Small,
        model::TextStyle::Strong => api::TextStyle::Strong,
        model::TextStyle::Heading => api::TextStyle::Heading,
        model::TextStyle::Monospace => api::TextStyle::Monospace,
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        let _ = self.deactivate(model::DeactivationReason::HostShutdown);
    }
}

fn to_wit_snapshot(snapshot: &api::GameSnapshot) -> model::CallbackSnapshot {
    let status = |revision| {
        let (observed_at_ms, unavailable_reason, has_value) = match snapshot.session.adapter {
            api::Availability::Live if snapshot.session.in_world => {
                (Some(snapshot.captured_at_ms), None, true)
            }
            api::Availability::Live if snapshot.session.loading_state.is_some() => {
                (None, Some(model::UnavailableReason::Loading), false)
            }
            api::Availability::Live => (None, Some(model::UnavailableReason::NotInWorld), false),
            api::Availability::Stale | api::Availability::Unavailable => {
                (None, Some(model::UnavailableReason::NotYetObserved), false)
            }
        };
        (
            model::StateStatus {
                observed_at_ms,
                revision,
                reason: unavailable_reason,
            },
            has_value,
        )
    };
    let (map_status, map_available) = status(snapshot.map.data_revision);
    let map_value = map_available.then(|| model::ZoneState {
        area_id: snapshot.map.area_id.clone(),
        display_name: snapshot.map.display_name.clone(),
    });
    let (ui_status, ui_available) = status(snapshot.ui.revision);
    let ui_value = ui_available.then(|| model::WindowsState {
        open_windows: snapshot.ui.open_windows.clone(),
        focused_window: snapshot.ui.focused_window.clone(),
    });
    model::CallbackSnapshot {
        observation: model::ObservationMetadata {
            sequence: snapshot.sequence,
            captured_at_ms: snapshot.captured_at_ms,
            process_session: snapshot.session.process_session,
        },
        session: model::SessionState {
            process_session: snapshot.session.process_session,
            in_world: snapshot.session.in_world,
        },
        player: model::PlayerSnapshot {
            status: to_wit_state_status(&snapshot.player),
            value: snapshot
                .player
                .value
                .as_ref()
                .map(|player| model::PlayerState {
                    runtime_id: player.runtime_id.clone(),
                    name: player.name.clone(),
                    class_id: player.class_id.clone(),
                    level: None,
                    position: Some(to_wit_vec3(player.position)),
                    heading_radians: player.heading_radians,
                    health: None,
                    max_health: None,
                }),
        },
        party: model::PartySnapshot {
            status: to_wit_state_status(&snapshot.party),
            value: snapshot
                .party
                .value
                .as_ref()
                .map(|party| model::PartyState {
                    party_id: party.party_id.clone(),
                    members: party
                        .members
                        .iter()
                        .map(|member| model::PartyMember {
                            actor_id: member.actor_id.clone(),
                            is_local: member.is_local,
                            name: member.name.clone(),
                            class_id: member.class_id.clone(),
                            class_icon: member.class_icon.as_ref().map(|icon| model::ImageRef {
                                id: icon.id.clone(),
                            }),
                            in_combat: member.in_combat,
                        })
                        .collect(),
                }),
        },
        camera: model::CameraSnapshot {
            status: to_wit_state_status(&snapshot.camera),
            value: snapshot.camera.value.map(|camera| model::CameraState {
                heading_radians: camera.heading_radians,
            }),
        },
        combat: model::CombatSnapshot {
            status: to_wit_state_status(&snapshot.combat_references),
            value: snapshot
                .combat_references
                .value
                .as_ref()
                .map(|state| model::CombatState {
                    in_combat: snapshot
                        .player
                        .value
                        .as_ref()
                        .and_then(|player| player.in_combat),
                    references: state
                        .references
                        .iter()
                        .map(|reference| model::CombatReference {
                            slot: match reference.slot {
                                api::CombatReferenceSlot::Target => {
                                    model::CombatReferenceSlot::Target
                                }
                                api::CombatReferenceSlot::LockedTarget => {
                                    model::CombatReferenceSlot::LockedTarget
                                }
                                api::CombatReferenceSlot::AutoTarget => {
                                    model::CombatReferenceSlot::AutoTarget
                                }
                            },
                            position: reference.position.map(to_wit_vec3),
                        })
                        .collect(),
                }),
        },
        instance_session: model::InstanceSnapshot {
            status: to_wit_state_status(&snapshot.instance_session),
            value: snapshot
                .instance_session
                .value
                .as_ref()
                .map(to_wit_instance_state),
        },
        zone: model::ZoneSnapshot {
            status: map_status,
            value: map_value,
        },
        windows: model::WindowsSnapshot {
            status: ui_status,
            value: ui_value,
        },
    }
}

fn to_wit_instance_state(value: &api::InstanceState) -> model::InstanceState {
    model::InstanceState {
        session_id: value.session_id,
        kind: match value.kind {
            api::InstanceKind::OpenWorld => model::InstanceKind::OpenWorld,
            api::InstanceKind::Dungeon => model::InstanceKind::Dungeon,
            api::InstanceKind::Other => model::InstanceKind::Other,
            api::InstanceKind::Unknown => model::InstanceKind::Unknown,
        },
        area_id: value.area_id.clone(),
    }
}

fn to_wit_vec3(value: api::Vec3) -> model::Vec3 {
    model::Vec3 {
        x: value.x,
        y: value.y,
        z: value.z,
    }
}

fn to_wit_state_status<T>(snapshot: &api::StateSnapshot<T>) -> model::StateStatus {
    model::StateStatus {
        observed_at_ms: snapshot.observed_at_ms,
        revision: snapshot.revision,
        reason: snapshot.unavailable_reason.map(|reason| match reason {
            api::UnavailableReason::NotInWorld => model::UnavailableReason::NotInWorld,
            api::UnavailableReason::Loading => model::UnavailableReason::Loading,
            api::UnavailableReason::NotYetObserved => model::UnavailableReason::NotYetObserved,
            api::UnavailableReason::Unsupported => model::UnavailableReason::Unsupported,
            api::UnavailableReason::PermissionDenied => model::UnavailableReason::PermissionDenied,
            api::UnavailableReason::ProviderFailed => model::UnavailableReason::ProviderFailed,
        }),
    }
}

fn to_wit_header(header: &api::EventHeader) -> model::EventHeader {
    model::EventHeader {
        sequence: header.sequence,
        monotonic_ms: header.monotonic_ms,
    }
}

fn to_wit_actor(value: &api::CombatActorRef) -> model::CombatActorRef {
    model::CombatActorRef {
        actor_id: value.actor_id.clone(),
        relation: match value.relation {
            api::ActorRelation::LocalPlayer => model::ActorRelation::LocalPlayer,
            api::ActorRelation::GroupMember => model::ActorRelation::GroupMember,
            api::ActorRelation::Other => model::ActorRelation::Other,
            api::ActorRelation::Unknown => model::ActorRelation::Unknown,
        },
        kind: value.kind.clone(),
    }
}

fn to_wit_event(event: &api::HostEvent) -> model::Event {
    match event {
        api::HostEvent::Damage(event) => model::Event::Damage(model::DamageEvent {
            header: to_wit_header(&event.header),
            source: to_wit_actor(&event.source),
            target: to_wit_actor(&event.target),
            skill_id: event.skill_id.clone(),
            skill_display_name: event.skill_display_name.clone(),
            skill_icon: event.skill_icon.as_ref().map(|icon| model::ImageRef {
                id: icon.id.clone(),
            }),
            amount: event.amount,
            hit_count: event.hit_count,
            critical: event.critical,
            killed: event.killed,
            blocked: event.blocked,
        }),
        api::HostEvent::CombatStarted(event) => model::Event::CombatStarted(model::CombatEvent {
            header: to_wit_header(&event.header),
            fight_id: event.fight_id,
        }),
        api::HostEvent::CombatEnded(event) => model::Event::CombatEnded(model::CombatEvent {
            header: to_wit_header(&event.header),
            fight_id: event.fight_id,
        }),
        api::HostEvent::PartyChanged(event) => model::Event::PartyChanged(model::PartyEvent {
            header: to_wit_header(&event.header),
            revision: event.revision,
        }),
        api::HostEvent::InstanceChanged(event) => {
            model::Event::InstanceChanged(model::InstanceEvent {
                header: to_wit_header(&event.header),
                previous: event.previous.as_ref().map(to_wit_instance_state),
                current: event.current.as_ref().map(to_wit_instance_state),
            })
        }
        api::HostEvent::ZoneChanged {
            header,
            previous_area_id,
            area_id,
        } => model::Event::ZoneChanged(model::ZoneEvent {
            header: to_wit_header(header),
            previous_area_id: previous_area_id.clone(),
            area_id: area_id.clone(),
        }),
        api::HostEvent::UiWindowOpened { header, window_id } => {
            model::Event::WindowOpened(model::WindowEvent {
                header: to_wit_header(header),
                window_id: window_id.clone(),
            })
        }
        api::HostEvent::UiWindowClosed { header, window_id } => {
            model::Event::WindowClosed(model::WindowEvent {
                header: to_wit_header(header),
                window_id: window_id.clone(),
            })
        }
        api::HostEvent::PlayerDisconnected(event) => {
            model::Event::PlayerDisconnected(model::PlayerDisconnectedEvent {
                header: to_wit_header(&event.header),
                reason: match event.reason {
                    api::PlayerDisconnectReason::ManualExit => model::DisconnectReason::ManualExit,
                    api::PlayerDisconnectReason::Kick => model::DisconnectReason::Kick,
                    api::PlayerDisconnectReason::Timeout => model::DisconnectReason::Timeout,
                    api::PlayerDisconnectReason::SwitchingServer => {
                        model::DisconnectReason::SwitchingServer
                    }
                    api::PlayerDisconnectReason::Unknown => model::DisconnectReason::Unknown,
                },
            })
        }
    }
}

fn to_wit_batch(batch: &api::EventBatch) -> model::EventBatch {
    model::EventBatch {
        process_session: batch.process_session,
        first_sequence: batch.first_sequence,
        next_sequence: batch.next_sequence,
        dropped_before: batch.dropped_before,
        snapshot_required: batch.snapshot_required,
        events: batch.events.iter().map(to_wit_event).collect(),
    }
}

fn to_wit_ui_event(event: &api::UiEvent) -> model::UiEvent {
    match event {
        api::UiEvent::ConfigMenuShown(menu_id) => model::UiEvent::ConfigMenuShown(menu_id.clone()),
        api::UiEvent::ConfigMenuHidden(menu_id) => {
            model::UiEvent::ConfigMenuHidden(menu_id.clone())
        }
        api::UiEvent::ButtonPressed { view, node_id } => {
            model::UiEvent::ButtonPressed(model::ButtonPressed {
                view: match view {
                    api::UiView::Surface(surface_id) => model::UiView::Surface(surface_id.clone()),
                    api::UiView::ConfigMenu(menu_id) => model::UiView::ConfigMenu(menu_id.clone()),
                },
                node_id: node_id.clone(),
            })
        }
        api::UiEvent::CanvasPressed {
            view,
            node_id,
            x,
            y,
        } => model::UiEvent::CanvasPressed(model::CanvasPressed {
            view: match view {
                api::UiView::Surface(surface_id) => model::UiView::Surface(surface_id.clone()),
                api::UiView::ConfigMenu(menu_id) => model::UiView::ConfigMenu(menu_id.clone()),
            },
            node_id: node_id.clone(),
            position: (*x, *y),
        }),
        api::UiEvent::CheckboxChanged { node_id, checked } => {
            model::UiEvent::CheckboxChanged((node_id.clone(), *checked))
        }
        api::UiEvent::DropdownChanged {
            node_id,
            selected_id,
        } => model::UiEvent::DropdownChanged((node_id.clone(), selected_id.clone())),
        api::UiEvent::SliderChanged { node_id, value } => {
            model::UiEvent::SliderChanged((node_id.clone(), *value))
        }
    }
}

fn validate_ui_update(
    update: model::UiUpdate,
    owner: &str,
    font_families: &HashMap<api::TextStyle, String>,
) -> PluginResult<UiEffect> {
    match update {
        model::UiUpdate::Unchanged => Ok(UiEffect::Unchanged),
        model::UiUpdate::Replace(frame) => {
            validate_wit_frame_with_fonts(frame, owner, font_families).map(UiEffect::Replace)
        }
        model::UiUpdate::Clear => Ok(UiEffect::Clear),
    }
}

#[cfg(test)]
fn validate_wit_frame(frame: model::UiFrame, owner: &str) -> PluginResult<api::UiFrame> {
    validate_wit_frame_with_fonts(frame, owner, &HashMap::new())
}

fn validate_wit_frame_with_fonts(
    frame: model::UiFrame,
    owner: &str,
    font_families: &HashMap<api::TextStyle, String>,
) -> PluginResult<api::UiFrame> {
    if frame.surfaces.len() > api::MAX_UI_SURFACES_PER_ADDON {
        return Err(format!(
            "UI frame has {} surfaces; limit is {}",
            frame.surfaces.len(),
            api::MAX_UI_SURFACES_PER_ADDON
        ));
    }
    if frame.config_menus.len() > api::MAX_CONFIG_MENUS_PER_ADDON {
        return Err(format!(
            "UI frame has {} config menus; limit is {}",
            frame.config_menus.len(),
            api::MAX_CONFIG_MENUS_PER_ADDON
        ));
    }

    let mut surface_ids = HashSet::new();
    let mut node_count = 0_usize;
    let mut canvas_count = 0_usize;
    let mut text_bytes = 0_usize;
    let mut surfaces = Vec::with_capacity(frame.surfaces.len());
    for surface in frame.surfaces {
        let surface = validate_wit_surface(
            surface,
            owner,
            font_families,
            &mut node_count,
            &mut canvas_count,
            &mut text_bytes,
        )?;
        if !surface_ids.insert(surface.id.clone()) {
            return Err(format!("duplicate UI surface id {:?}", surface.id));
        }
        surfaces.push(surface);
    }

    let mut config_menu_ids = HashSet::new();
    let mut config_menus = Vec::with_capacity(frame.config_menus.len());
    for menu in frame.config_menus {
        let menu = validate_wit_config_menu(
            menu,
            owner,
            font_families,
            &mut node_count,
            &mut canvas_count,
            &mut text_bytes,
        )?;
        if !config_menu_ids.insert(menu.id.clone()) {
            return Err(format!("duplicate config menu id {:?}", menu.id));
        }
        config_menus.push(menu);
    }
    Ok(api::UiFrame {
        surfaces,
        config_menus,
    })
}

fn validate_wit_surface(
    surface: model::UiSurface,
    owner: &str,
    font_families: &HashMap<api::TextStyle, String>,
    node_count: &mut usize,
    canvas_count: &mut usize,
    text_bytes: &mut usize,
) -> PluginResult<api::UiSurface> {
    let id = checked_text(surface.id, 128, text_bytes, "surface id")?;
    let title = checked_text(surface.title, 256, text_bytes, "surface title")?;
    let anchor = match surface.anchor {
        model::SurfaceAnchor::TopLeft => api::SurfaceAnchor::TopLeft,
        model::SurfaceAnchor::TopRight => api::SurfaceAnchor::TopRight,
        model::SurfaceAnchor::BottomLeft => api::SurfaceAnchor::BottomLeft,
        model::SurfaceAnchor::BottomRight => api::SurfaceAnchor::BottomRight,
        model::SurfaceAnchor::Center => api::SurfaceAnchor::Center,
        model::SurfaceAnchor::TopCenter => api::SurfaceAnchor::TopCenter,
    };
    let minimum_margin_x = if matches!(
        anchor,
        api::SurfaceAnchor::Center | api::SurfaceAnchor::TopCenter
    ) {
        -4_096.0
    } else {
        0.0
    };
    let minimum_margin_y = if anchor == api::SurfaceAnchor::Center {
        -4_096.0
    } else {
        0.0
    };
    let margin_x =
        checked_f32(surface.margin_x, "surface margin-x")?.clamp(minimum_margin_x, 4_096.0);
    let margin_y =
        checked_f32(surface.margin_y, "surface margin-y")?.clamp(minimum_margin_y, 4_096.0);
    let width = surface
        .width
        .map(|value| checked_f32(value, "surface width").map(|value| value.clamp(64.0, 4_096.0)))
        .transpose()?;
    let style = surface
        .style
        .map(|style| {
            Ok::<_, String>(api::SurfaceStyle {
                title_bar: style.title_bar,
                fill: from_wit_color(style.fill)?,
                stroke: style.stroke.map(from_wit_stroke).transpose()?,
                corner_radius: checked_f32(style.corner_radius, "surface corner-radius")?
                    .clamp(0.0, 64.0),
                padding: checked_f32(style.padding, "surface padding")?.clamp(0.0, 64.0),
            })
        })
        .transpose()?;

    let document = validate_wit_document(
        surface.nodes,
        surface.canvas,
        DocumentValidation {
            name: format!("surface {id:?}"),
            allow_config_controls: false,
        },
        font_families,
        node_count,
        canvas_count,
        text_bytes,
    )?;

    Ok(api::UiSurface {
        owner: truncate_utf8(owner, 128),
        id,
        title,
        anchor,
        margin_x,
        margin_y,
        width,
        style,
        nodes: document.nodes,
        canvas: document.canvas,
    })
}

fn validate_wit_config_menu(
    menu: model::ConfigMenu,
    owner: &str,
    font_families: &HashMap<api::TextStyle, String>,
    node_count: &mut usize,
    canvas_count: &mut usize,
    text_bytes: &mut usize,
) -> PluginResult<api::ConfigMenu> {
    let id = checked_text(menu.id, 128, text_bytes, "config menu id")?;
    let title = checked_text(menu.title, 256, text_bytes, "config menu title")?;
    let document = validate_wit_document(
        menu.nodes,
        menu.canvas,
        DocumentValidation {
            name: format!("config menu {id:?}"),
            allow_config_controls: true,
        },
        font_families,
        node_count,
        canvas_count,
        text_bytes,
    )?;
    Ok(api::ConfigMenu {
        owner: truncate_utf8(owner, 128),
        id,
        title,
        nodes: document.nodes,
        canvas: document.canvas,
    })
}

struct ValidatedDocument {
    nodes: Vec<api::UiNode>,
    canvas: Vec<api::CanvasCommand>,
}

struct DocumentValidation {
    name: String,
    allow_config_controls: bool,
}

fn validate_wit_document(
    source_nodes: Vec<model::UiNode>,
    source_canvas: Vec<model::CanvasCommand>,
    validation: DocumentValidation,
    font_families: &HashMap<api::TextStyle, String>,
    node_count: &mut usize,
    canvas_count: &mut usize,
    text_bytes: &mut usize,
) -> PluginResult<ValidatedDocument> {
    *node_count = node_count.saturating_add(source_nodes.len());
    if *node_count > api::MAX_UI_NODES_PER_ADDON {
        return Err(format!(
            "UI frame has more than {} nodes",
            api::MAX_UI_NODES_PER_ADDON
        ));
    }

    let mut nodes: Vec<api::UiNode> = Vec::with_capacity(source_nodes.len());
    let mut node_ids: HashMap<String, usize> = HashMap::with_capacity(source_nodes.len());
    let mut depths: Vec<usize> = Vec::with_capacity(source_nodes.len());
    for source in source_nodes {
        let node_id = checked_text(source.id, 128, text_bytes, "node id")?;
        if node_ids.contains_key(&node_id) {
            return Err(format!(
                "duplicate node id {node_id:?} in {}",
                validation.name
            ));
        }
        let parent = source
            .parent
            .map(|parent_id| {
                node_ids.get(&parent_id).copied().ok_or_else(|| {
                    format!("node {node_id:?} refers to missing or later parent {parent_id:?}")
                })
            })
            .transpose()?;
        let depth = parent.map_or(0, |parent| depths[parent] + 1);
        if depth > api::MAX_UI_DEPTH {
            return Err(format!(
                "node {node_id:?} exceeds maximum UI depth {}",
                api::MAX_UI_DEPTH
            ));
        }
        let widget = from_wit_widget(source.widget, font_families, text_bytes)?;
        if matches!(
            widget,
            api::Widget::Checkbox(_) | api::Widget::Dropdown(_) | api::Widget::Slider(_)
        ) && !validation.allow_config_controls
        {
            return Err(format!(
                "config control node {node_id:?} is only valid in a config menu"
            ));
        }
        validate_widget_parent(
            &widget,
            parent.map(|parent| &nodes[parent].widget),
            &node_id,
        )?;
        let index = nodes.len();
        node_ids.insert(node_id.clone(), index);
        depths.push(depth);
        nodes.push(api::UiNode {
            id: node_id,
            parent,
            widget,
        });
    }
    validate_table_structure(&nodes)?;

    *canvas_count = canvas_count.saturating_add(source_canvas.len());
    if *canvas_count > api::MAX_CANVAS_COMMANDS_PER_ADDON {
        return Err(format!(
            "UI frame has more than {} canvas commands",
            api::MAX_CANVAS_COMMANDS_PER_ADDON
        ));
    }
    let mut canvas = Vec::with_capacity(source_canvas.len());
    for command in source_canvas {
        let canvas_index = node_ids.get(&command.canvas_id).copied().ok_or_else(|| {
            format!(
                "canvas command targets missing node {:?}",
                command.canvas_id
            )
        })?;
        if !matches!(nodes[canvas_index].widget, api::Widget::Canvas(_)) {
            return Err(format!(
                "canvas command target {:?} is not a canvas widget",
                command.canvas_id
            ));
        }
        canvas.push(api::CanvasCommand {
            canvas: canvas_index,
            primitive: from_wit_canvas_primitive(command.primitive, text_bytes)?,
        });
    }
    Ok(ValidatedDocument { nodes, canvas })
}

fn from_wit_widget(
    widget: model::Widget,
    font_families: &HashMap<api::TextStyle, String>,
    text_bytes: &mut usize,
) -> PluginResult<api::Widget> {
    Ok(match widget {
        model::Widget::Container(widget) => api::Widget::Container(api::ContainerWidget {
            direction: match widget.direction {
                model::LayoutDirection::Vertical => api::LayoutDirection::Vertical,
                model::LayoutDirection::Horizontal => api::LayoutDirection::Horizontal,
            },
            style: match widget.style {
                model::ContainerStyle::Plain => api::ContainerStyle::Plain,
                model::ContainerStyle::Group => api::ContainerStyle::Group,
                model::ContainerStyle::Scroll => api::ContainerStyle::Scroll,
            },
            spacing: widget
                .spacing
                .map(|value| {
                    checked_f32(value, "container spacing").map(|value| value.clamp(0.0, 64.0))
                })
                .transpose()?,
            max_height: widget
                .max_height
                .map(|value| {
                    checked_f32(value, "container max-height")
                        .map(|value| value.clamp(32.0, 4_096.0))
                })
                .transpose()?,
        }),
        model::Widget::Section(widget) => api::Widget::Section(api::SectionWidget {
            title: checked_text(widget.title, 4 * 1024, text_bytes, "section title")?,
            description: widget
                .description
                .map(|text| checked_text(text, 4 * 1024, text_bytes, "section description"))
                .transpose()?,
        }),
        model::Widget::Text(widget) => {
            let style = from_wit_text_style(widget.style);
            api::Widget::Text(api::TextWidget {
                text: checked_text(widget.text, 16 * 1024, text_bytes, "text widget")?,
                style,
                font_family: font_families.get(&style).cloned(),
                color: widget.color.map(from_wit_color).transpose()?,
                outline: widget.outline.map(from_wit_text_outline).transpose()?,
                wrap: widget.wrap,
            })
        }
        model::Widget::Image(widget) => api::Widget::Image(api::ImageWidget {
            source: api::ImageRef {
                id: checked_text(widget.source.id, 256, text_bytes, "image reference")?,
            },
            size: api::Size {
                width: checked_f32(widget.size.width, "image width")?.clamp(1.0, 4_096.0),
                height: checked_f32(widget.size.height, "image height")?.clamp(1.0, 4_096.0),
            },
            tint: widget.tint.map(from_wit_color).transpose()?,
        }),
        model::Widget::Button(widget) => api::Widget::Button(api::ButtonWidget {
            label: checked_text(widget.label, 4 * 1024, text_bytes, "button label")?,
            enabled: widget.enabled,
        }),
        model::Widget::Checkbox(widget) => api::Widget::Checkbox(api::CheckboxWidget {
            label: checked_text(widget.label, 4 * 1024, text_bytes, "checkbox label")?,
            checked: widget.checked,
            enabled: widget.enabled,
        }),
        model::Widget::Dropdown(widget) => {
            if widget.options.is_empty() {
                return Err("dropdown must declare at least one option".to_owned());
            }
            if widget.options.len() > api::MAX_DROPDOWN_OPTIONS {
                return Err(format!(
                    "dropdown has {} options; limit is {}",
                    widget.options.len(),
                    api::MAX_DROPDOWN_OPTIONS
                ));
            }
            let selected_id = checked_text(
                widget.selected_id,
                128,
                text_bytes,
                "dropdown selected option id",
            )?;
            let mut option_ids = HashSet::with_capacity(widget.options.len());
            let mut options = Vec::with_capacity(widget.options.len());
            for option in widget.options {
                let id = checked_text(option.id, 128, text_bytes, "dropdown option id")?;
                if id.is_empty() {
                    return Err("dropdown option id must not be empty".to_owned());
                }
                if !option_ids.insert(id.clone()) {
                    return Err(format!("duplicate dropdown option id {id:?}"));
                }
                options.push(api::DropdownOption {
                    id,
                    label: checked_text(
                        option.label,
                        4 * 1024,
                        text_bytes,
                        "dropdown option label",
                    )?,
                });
            }
            if !option_ids.contains(&selected_id) {
                return Err(format!(
                    "dropdown selected option {selected_id:?} is not declared"
                ));
            }
            api::Widget::Dropdown(api::DropdownWidget {
                label: checked_text(widget.label, 4 * 1024, text_bytes, "dropdown label")?,
                selected_id,
                options,
                enabled: widget.enabled,
            })
        }
        model::Widget::Slider(widget) => {
            let value = checked_f64(widget.value, "slider value")?;
            let minimum = checked_f64(widget.minimum, "slider minimum")?;
            let maximum = checked_f64(widget.maximum, "slider maximum")?;
            if minimum >= maximum {
                return Err("slider minimum must be less than maximum".to_owned());
            }
            if !(maximum - minimum).is_finite() {
                return Err("slider range span must be finite".to_owned());
            }
            if value < minimum || value > maximum {
                return Err(format!(
                    "slider value {value} must be within {minimum}..={maximum}"
                ));
            }
            let step = widget
                .step
                .map(|step| checked_f64(step, "slider step"))
                .transpose()?;
            if step.is_some_and(|step| step <= 0.0) {
                return Err("slider step must be positive".to_owned());
            }
            api::Widget::Slider(api::SliderWidget {
                label: checked_text(widget.label, 4 * 1024, text_bytes, "slider label")?,
                value,
                minimum,
                maximum,
                step,
                enabled: widget.enabled,
            })
        }
        model::Widget::Progress(widget) => api::Widget::Progress(api::ProgressWidget {
            fraction: checked_f32(widget.fraction, "progress fraction")?.clamp(0.0, 1.0),
            label: widget
                .label
                .map(|text| checked_text(text, 4 * 1024, text_bytes, "progress label"))
                .transpose()?,
            color: widget.color.map(from_wit_color).transpose()?,
        }),
        model::Widget::Separator => api::Widget::Separator,
        model::Widget::Spacer(widget) => {
            api::Widget::Spacer(checked_f32(widget.size, "spacer size")?.clamp(0.0, 1_024.0))
        }
        model::Widget::Canvas(widget) => api::Widget::Canvas(api::Size {
            width: checked_f32(widget.size.width, "canvas width")?.clamp(1.0, 4_096.0),
            height: checked_f32(widget.size.height, "canvas height")?.clamp(1.0, 4_096.0),
        }),
        model::Widget::Table(widget) => {
            if widget.columns.is_empty() {
                return Err("table must declare at least one column".to_owned());
            }
            if widget.columns.len() > api::MAX_UI_TABLE_COLUMNS {
                return Err(format!(
                    "table has {} columns; limit is {}",
                    widget.columns.len(),
                    api::MAX_UI_TABLE_COLUMNS
                ));
            }
            let columns = widget
                .columns
                .into_iter()
                .map(|column| {
                    Ok(api::TableColumn {
                        sizing: match column.sizing {
                            model::TableColumnSizing::Auto => api::TableColumnSizing::Auto,
                            model::TableColumnSizing::Exact(width) => {
                                api::TableColumnSizing::Exact(
                                    checked_f32(width, "exact table column width")?
                                        .clamp(8.0, 4_096.0),
                                )
                            }
                            model::TableColumnSizing::Remainder => {
                                api::TableColumnSizing::Remainder
                            }
                        },
                        alignment: match column.alignment {
                            model::HorizontalAlignment::Left => api::HorizontalAlignment::Left,
                            model::HorizontalAlignment::Center => api::HorizontalAlignment::Center,
                            model::HorizontalAlignment::Right => api::HorizontalAlignment::Right,
                        },
                        visible_from_width: column
                            .visible_from_width
                            .map(|width| {
                                checked_f32(width, "table column visible-from-width")
                                    .map(|width| width.clamp(0.0, 4_096.0))
                            })
                            .transpose()?,
                        content_padding: column
                            .content_padding
                            .map(|padding| {
                                checked_f32(padding, "table column content-padding")
                                    .map(|padding| padding.clamp(0.0, 64.0))
                            })
                            .transpose()?
                            .unwrap_or(0.0),
                    })
                })
                .collect::<PluginResult<Vec<_>>>()?;
            if !columns
                .iter()
                .any(|column| column.visible_from_width.is_none())
            {
                return Err("table must declare at least one always-visible column".to_owned());
            }
            api::Widget::Table(api::TableWidget {
                columns,
                striped: widget.striped,
                max_body_height: widget
                    .max_body_height
                    .map(|height| {
                        checked_f32(height, "table max-body-height")
                            .map(|height| height.clamp(32.0, 4_096.0))
                    })
                    .transpose()?,
            })
        }
        model::Widget::TableRow(widget) => api::Widget::TableRow(api::TableRowWidget {
            kind: match widget.kind {
                model::TableRowKind::Header => api::TableRowKind::Header,
                model::TableRowKind::Body => api::TableRowKind::Body,
            },
            height: checked_f32(widget.height, "table row height")?.clamp(12.0, 512.0),
            background: widget.background.map(from_wit_color).transpose()?,
            progress: widget
                .progress
                .map(|progress| {
                    Ok::<_, String>(api::TableRowProgress {
                        fraction: checked_f32(progress.fraction, "table row progress fraction")?
                            .clamp(0.0, 1.0),
                        color: from_wit_color(progress.color)?,
                        start_column: progress.start_column.map(|column| column as usize),
                    })
                })
                .transpose()?,
        }),
        model::Widget::TableCell(widget) => api::Widget::TableCell(api::TableCellWidget {
            column: widget.column as usize,
        }),
    })
}

fn validate_widget_parent(
    widget: &api::Widget,
    parent: Option<&api::Widget>,
    node_id: &str,
) -> PluginResult<()> {
    match (widget, parent) {
        (api::Widget::TableRow(_), Some(api::Widget::Table(_)))
        | (api::Widget::TableCell(_), Some(api::Widget::TableRow(_))) => Ok(()),
        (api::Widget::TableRow(_), _) => Err(format!(
            "table row node {node_id:?} must be a direct child of a table"
        )),
        (api::Widget::TableCell(_), _) => Err(format!(
            "table cell node {node_id:?} must be a direct child of a table row"
        )),
        (
            _,
            None
            | Some(api::Widget::Container(_) | api::Widget::Section(_) | api::Widget::TableCell(_)),
        ) => Ok(()),
        (_, Some(_)) => Err(format!(
            "node {node_id:?} parent cannot contain ordinary widget children"
        )),
    }
}

fn validate_table_structure(nodes: &[api::UiNode]) -> PluginResult<()> {
    for (table_index, table_node) in nodes.iter().enumerate() {
        let api::Widget::Table(table) = &table_node.widget else {
            continue;
        };
        let mut saw_header = false;
        let mut saw_body = false;
        for (row_index, row_node) in nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.parent == Some(table_index))
        {
            let api::Widget::TableRow(row) = row_node.widget else {
                unreachable!("table parent validation permits only rows");
            };
            match row.kind {
                api::TableRowKind::Header if saw_header => {
                    return Err(format!(
                        "table {:?} has more than one header row",
                        table_node.id
                    ));
                }
                api::TableRowKind::Header if saw_body => {
                    return Err(format!(
                        "table {:?} header row must precede body rows",
                        table_node.id
                    ));
                }
                api::TableRowKind::Header => saw_header = true,
                api::TableRowKind::Body => saw_body = true,
            }

            if let Some(start_column) = row.progress.and_then(|progress| progress.start_column) {
                if start_column >= table.columns.len() {
                    return Err(format!(
                        "table row {:?} progress starts at column {}; table {:?} has {} columns",
                        row_node.id,
                        start_column,
                        table_node.id,
                        table.columns.len()
                    ));
                }
                if table.columns[start_column].visible_from_width.is_some() {
                    return Err(format!(
                        "table row {:?} progress start column {} must always be visible",
                        row_node.id, start_column
                    ));
                }
            }

            let mut occupied_columns = HashSet::new();
            for cell_node in nodes.iter().filter(|node| node.parent == Some(row_index)) {
                let api::Widget::TableCell(cell) = cell_node.widget else {
                    unreachable!("table row parent validation permits only cells");
                };
                if cell.column >= table.columns.len() {
                    return Err(format!(
                        "table cell {:?} refers to column {}; table {:?} has {} columns",
                        cell_node.id,
                        cell.column,
                        table_node.id,
                        table.columns.len()
                    ));
                }
                if !occupied_columns.insert(cell.column) {
                    return Err(format!(
                        "table row {:?} contains column {} more than once",
                        row_node.id, cell.column
                    ));
                }
            }
        }
    }
    Ok(())
}

fn from_wit_canvas_primitive(
    primitive: model::CanvasPrimitive,
    text_bytes: &mut usize,
) -> PluginResult<api::CanvasPrimitive> {
    Ok(match primitive {
        model::CanvasPrimitive::Line(line) => api::CanvasPrimitive::Line {
            from: from_wit_point(line.start)?,
            to: from_wit_point(line.end)?,
            stroke: from_wit_stroke(line.stroke)?,
        },
        model::CanvasPrimitive::Rect(rect) => api::CanvasPrimitive::Rect {
            min: from_wit_point(rect.min)?,
            max: from_wit_point(rect.max)?,
            corner_radius: checked_f32(rect.corner_radius, "rectangle corner radius")?
                .clamp(0.0, 256.0),
            fill: rect.fill.map(from_wit_color).transpose()?,
            stroke: rect.stroke.map(from_wit_stroke).transpose()?,
        },
        model::CanvasPrimitive::Circle(circle) => api::CanvasPrimitive::Circle {
            center: from_wit_point(circle.center)?,
            radius: checked_f32(circle.radius, "circle radius")?.clamp(0.0, 4_096.0),
            fill: circle.fill.map(from_wit_color).transpose()?,
            stroke: circle.stroke.map(from_wit_stroke).transpose()?,
        },
        model::CanvasPrimitive::Path(path) => {
            if path.points.len() > api::MAX_CANVAS_POINTS_PER_PATH {
                return Err(format!(
                    "canvas path has {} points; limit is {}",
                    path.points.len(),
                    api::MAX_CANVAS_POINTS_PER_PATH
                ));
            }
            api::CanvasPrimitive::Path {
                points: path
                    .points
                    .into_iter()
                    .map(from_wit_point)
                    .collect::<PluginResult<_>>()?,
                closed: path.closed,
                fill: path.fill.map(from_wit_color).transpose()?,
                stroke: path.stroke.map(from_wit_stroke).transpose()?,
            }
        }
        model::CanvasPrimitive::Text(text) => api::CanvasPrimitive::Text {
            position: from_wit_point(text.position)?,
            text: checked_text(text.text, 4 * 1024, text_bytes, "canvas text")?,
            color: from_wit_color(text.color)?,
            size: checked_f32(text.size, "canvas text size")?.clamp(6.0, 96.0),
        },
        model::CanvasPrimitive::Image(image) => api::CanvasPrimitive::Image {
            source: api::ImageRef {
                id: checked_text(image.source.id, 256, text_bytes, "canvas image reference")?,
            },
            destination_min: from_wit_point(image.destination_min)?,
            destination_max: from_wit_point(image.destination_max)?,
            uv_min: from_wit_uv_point(image.uv_min)?,
            uv_max: from_wit_uv_point(image.uv_max)?,
            rotation_radians: checked_f32(image.rotation_radians, "canvas image rotation")?
                .clamp(-1_000_000.0, 1_000_000.0),
            tint: image.tint.map(from_wit_color).transpose()?,
            corner_radius: checked_f32(image.corner_radius, "image corner radius")?
                .clamp(0.0, 4_096.0),
        },
    })
}

fn from_wit_point(point: model::Point) -> PluginResult<api::Point> {
    Ok(api::Point {
        x: checked_f32(point.x, "canvas x")?.clamp(-100_000.0, 100_000.0),
        y: checked_f32(point.y, "canvas y")?.clamp(-100_000.0, 100_000.0),
    })
}

fn from_wit_uv_point(point: model::Point) -> PluginResult<api::Point> {
    Ok(api::Point {
        x: checked_f32(point.x, "image U")?.clamp(0.0, 1.0),
        y: checked_f32(point.y, "image V")?.clamp(0.0, 1.0),
    })
}

fn from_wit_stroke(stroke: model::Stroke) -> PluginResult<api::Stroke> {
    Ok(api::Stroke {
        width: checked_f32(stroke.width, "stroke width")?.clamp(0.0, 64.0),
        color: from_wit_color(stroke.color)?,
    })
}

fn from_wit_text_outline(stroke: model::Stroke) -> PluginResult<api::Stroke> {
    let mut stroke = from_wit_stroke(stroke)?;
    stroke.width = stroke.width.min(4.0);
    Ok(stroke)
}

fn from_wit_color(color: model::Rgba) -> PluginResult<api::Rgba> {
    Ok(api::Rgba {
        red: checked_f32(color.red, "color red")?.clamp(0.0, 1.0),
        green: checked_f32(color.green, "color green")?.clamp(0.0, 1.0),
        blue: checked_f32(color.blue, "color blue")?.clamp(0.0, 1.0),
        alpha: checked_f32(color.alpha, "color alpha")?.clamp(0.0, 1.0),
    })
}

fn checked_f32(value: f32, label: &str) -> PluginResult<f32> {
    value
        .is_finite()
        .then_some(value)
        .ok_or_else(|| format!("{label} must be finite"))
}

fn checked_f64(value: f64, label: &str) -> PluginResult<f64> {
    value
        .is_finite()
        .then_some(value)
        .ok_or_else(|| format!("{label} must be finite"))
}

fn checked_text(
    value: String,
    field_limit: usize,
    total: &mut usize,
    label: &str,
) -> PluginResult<String> {
    if value.len() > field_limit {
        return Err(format!(
            "{label} is {} UTF-8 bytes; field limit is {field_limit}",
            value.len()
        ));
    }
    *total = total.saturating_add(value.len());
    if *total > api::MAX_UI_TEXT_BYTES_PER_ADDON {
        return Err(format!(
            "UI frame text exceeds {} UTF-8 bytes",
            api::MAX_UI_TEXT_BYTES_PER_ADDON
        ));
    }
    Ok(value)
}

fn truncate_utf8(value: &str, capacity: usize) -> String {
    let mut end = value.len().min(capacity);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn truncate_utf16(value: &str, capacity: usize) -> (String, bool) {
    let mut code_units = 0_usize;
    let mut end = 0_usize;
    for (index, character) in value.char_indices() {
        let next = code_units.saturating_add(character.len_utf16());
        if next > capacity {
            return (value[..end].to_owned(), true);
        }
        code_units = next;
        end = index + character.len_utf8();
    }
    (value.to_owned(), false)
}

pub(crate) fn discover_components(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // A unit holds exactly one component and the format fixes its
            // name, so there is nothing to search for.
            let component = path.join(WASM_FILE_NAME);
            if component.is_file() {
                result.push(component);
            }
        } else if is_component(&path) {
            result.push(path);
        }
    }
    result
}

pub(crate) fn is_component(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wasm"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use farever_more_manifest::RequiredService;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_CONFIG_ROOT: AtomicU64 = AtomicU64::new(1);

    struct TempConfigRoot(PathBuf);

    impl TempConfigRoot {
        fn new() -> Self {
            let serial = NEXT_CONFIG_ROOT.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "farever-plugin-config-test-{}-{serial}",
                std::process::id()
            )))
        }
    }

    impl Drop for TempConfigRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_host_state(
        namespace: String,
        snapshot: &api::GameSnapshot,
        bus_directory: Arc<Mutex<BusDirectory>>,
    ) -> HostState {
        let root = TempConfigRoot::new();
        let config = ConfigRegistry::open(&root.0, &namespace, Some("0.1.0"))
            .expect("open isolated test config registry");
        HostState::new(
            namespace,
            snapshot,
            bus_directory,
            Rc::new(RefCell::new(ServiceDirectory::default())),
            Vec::new(),
            config,
        )
    }

    fn live_snapshot() -> api::GameSnapshot {
        api::GameSnapshot {
            sequence: 7,
            captured_at_ms: 250,
            session: api::SessionState {
                process_session: 11,
                adapter: api::Availability::Live,
                in_world: true,
                loading_state: None,
            },
            player: api::StateSnapshot::live(
                250,
                2,
                api::PlayerState {
                    runtime_id: Some("player-fixture".to_owned()),
                    name: Some("Fixture".to_owned()),
                    class_id: Some("Warrior".to_owned()),
                    position: api::Vec3 {
                        x: 10.0,
                        y: 20.0,
                        z: 3.0,
                    },
                    heading_radians: Some(0.5),
                    in_combat: Some(true),
                },
            ),
            party: api::StateSnapshot::live(
                250,
                7,
                api::PartyState {
                    party_id: Some("party-fixture".to_owned()),
                    members: vec![api::PartyMember {
                        actor_id: "player-fixture".to_owned(),
                        is_local: true,
                        name: Some("Fixture".to_owned()),
                        class_id: Some("Warrior".to_owned()),
                        class_icon: Some(api::ImageRef {
                            id: "game/class-icon/warrior".to_owned(),
                        }),
                        in_combat: Some(true),
                    }],
                },
            ),
            camera: api::StateSnapshot::live(
                250,
                5,
                api::CameraState {
                    heading_radians: 1.25,
                },
            ),
            combat_references: api::StateSnapshot::live(
                250,
                6,
                api::CombatReferencesState {
                    references: vec![api::CombatReference {
                        slot: api::CombatReferenceSlot::AutoTarget,
                        position: Some(api::Vec3 {
                            x: 30.0,
                            y: 40.0,
                            z: 4.0,
                        }),
                    }],
                },
            ),
            instance_session: api::StateSnapshot::live(
                250,
                8,
                api::InstanceState {
                    session_id: 17,
                    kind: api::InstanceKind::Dungeon,
                    area_id: Some("World/W2_Dungeon".to_owned()),
                },
            ),
            map: api::MapState {
                area_id: Some("World/W1_Test".to_owned()),
                display_name: Some("Test".to_owned()),
                data_revision: 3,
            },
            ui: api::UiState {
                open_windows: vec!["ui.Character".to_owned()],
                focused_window: None,
                revision: 4,
            },
        }
    }

    #[test]
    #[ignore = "set FAREVER_ADDON_SMOKE_DIR to a built add-ons directory"]
    fn built_poi_provider_serves_typed_minimap_query_during_activation() {
        let built = std::env::var_os("FAREVER_ADDON_SMOKE_DIR")
            .map(PathBuf::from)
            .expect("FAREVER_ADDON_SMOKE_DIR");
        let config = TempConfigRoot::new();
        let mut plugins = Plugins::load(&built, &config.0, &live_snapshot());
        let diagnostics = plugins.take_diagnostics();

        assert_eq!(plugins.len(), 4, "diagnostics={diagnostics:#?}");
        assert!(
            diagnostics.iter().any(|message| {
                message.contains("Minimap PoC activated with POI service version=1.3.0")
                    && message.contains("records=1105")
            }),
            "diagnostics={diagnostics:#?}"
        );
    }

    #[test]
    #[ignore = "set FAREVER_ADDON_SMOKE_DIR to a built add-ons directory"]
    fn built_minimap_refetches_dense_window_under_tick_fuel() {
        let directory = std::env::var_os("FAREVER_ADDON_SMOKE_DIR")
            .map(PathBuf::from)
            .expect("FAREVER_ADDON_SMOKE_DIR");
        let config = TempConfigRoot::new();
        let initial = live_snapshot();
        let mut plugins = Plugins::load(&directory, &config.0, &initial);
        let _ = plugins.take_diagnostics();
        assert_eq!(plugins.len(), 4);
        // Teleport far outside the seeded window, then back: the return
        // re-walks the dense 1105-record origin window inside one tick,
        // which must fit the per-callback fuel budget without trapping.
        let mut far = live_snapshot();
        far.sequence += 1;
        far.player.value.as_mut().expect("player").position.x = 5000.0;
        far.player.value.as_mut().expect("player").position.y = 5000.0;
        plugins.started = plugins
            .started
            .checked_sub(Duration::from_millis(60))
            .expect("advance monotonic test clock");
        plugins.dispatch(&far, None, &[]);
        let diagnostics = plugins.take_diagnostics();
        assert!(
            diagnostics
                .iter()
                .any(|message| message.contains("Minimap PoC refreshed POI buffer")),
            "refresh away missing: {diagnostics:#?}"
        );
        let mut back = live_snapshot();
        back.sequence += 2;
        plugins.started = plugins
            .started
            .checked_sub(Duration::from_millis(60))
            .expect("advance monotonic test clock");
        plugins.dispatch(&back, None, &[]);
        let diagnostics = plugins.take_diagnostics();
        // The return re-walks the dense origin window: records=1105 proves
        // the full-volume re-fetch completed inside tick fuel. Surface
        // rendering itself is covered by the host-boundary smoke.
        assert!(
            diagnostics.iter().any(
                |message| message.contains("Minimap PoC refreshed POI buffer")
                    && message.contains("records=1105")
            ),
            "dense refresh missing: {diagnostics:#?}"
        );
    }

    #[test]
    fn interface_names_carry_the_add_on_api_version() {
        assert_eq!(
            farever_addon_api_version("farever:addon/dependencies@1.0.0"),
            ApiVersion::parse("1.0.0")
        );
        assert_eq!(
            farever_addon_api_version("farever:addon/plugin@1.4.2"),
            ApiVersion::parse("1.4.2")
        );
        // Another world, no version, or no pinned version: nothing to compare.
        assert_eq!(farever_addon_api_version("wasi:io/streams@1.0.0"), None);
        assert_eq!(farever_addon_api_version("farever:addon/plugin"), None);
        assert_eq!(farever_addon_api_version("farever:addon/plugin@1.0"), None);
        assert_eq!(farever_addon_api_version("farever-addon"), None);
    }

    #[test]
    fn a_declared_api_version_must_be_pinned() {
        let mut manifest = AddonManifest::default();
        assert_eq!(declared_api_version(&manifest).expect("absent"), None);

        manifest.api_version = Some("1.0.0".to_owned());
        assert_eq!(
            declared_api_version(&manifest).expect("valid"),
            ApiVersion::parse("1.0.0")
        );

        for declared in ["1.0", "", "latest", "1.0.0-rc.1"] {
            manifest.api_version = Some(declared.to_owned());
            let error = declared_api_version(&manifest).expect_err("unpinned");
            assert!(error.contains("api-version"), "{declared:?}: {error}");
        }
    }

    #[test]
    fn an_incompatible_api_is_refused_against_this_runtime() {
        let host = ApiVersion::host();
        assert!(require_compatible_api("demo", host).is_ok());

        let next_major = ApiVersion::parse(&format!("{}.0.0", host.major() + 1)).expect("parse");
        let error = require_compatible_api("demo", next_major).expect_err("major");
        assert!(error.starts_with("demo: "), "{error}");
        assert!(error.contains(&host.to_string()), "{error}");
        assert!(error.contains("major versions"), "{error}");
    }

    #[test]
    #[ignore = "set FAREVER_ADDON_SMOKE_DIR to a built add-ons directory"]
    fn built_units_declare_the_api_their_component_was_built_for() {
        let directory = std::env::var_os("FAREVER_ADDON_SMOKE_DIR")
            .map(PathBuf::from)
            .expect("FAREVER_ADDON_SMOKE_DIR");
        let engine = create_engine().expect("engine");
        let mut checked = 0;
        for path in discover_components(&directory) {
            let bytes = fs::read(&path).expect("read component");
            let manifest = read_addon_manifest(&path).expect("manifest");
            assert_eq!(
                manifest.api_version.as_deref(),
                Some(farever_more_manifest::api::ADDON_API_VERSION),
                "packed manifest must record the add-on API: {}",
                path.display()
            );
            let component = Component::from_binary(&engine, &bytes).expect("compile");
            assert_eq!(
                component_api_version(&component, &engine),
                Some(ApiVersion::host()),
                "component imports name the API they were built for: {}",
                path.display()
            );
            checked += 1;
        }
        assert!(checked > 0, "no components under {}", directory.display());
    }

    #[test]
    #[ignore = "set FAREVER_ADDON_SMOKE_DIR to a built add-ons directory"]
    fn a_component_for_another_api_never_reaches_activation() {
        let directory = std::env::var_os("FAREVER_ADDON_SMOKE_DIR")
            .map(PathBuf::from)
            .expect("FAREVER_ADDON_SMOKE_DIR");
        let host = ApiVersion::host();
        let source = directory.join("minimap");
        let engine = create_engine().expect("engine");
        for (declared, expected) in [
            (format!("{}.0.0", host.major() + 1), "major versions"),
            (
                format!("{}.{}.{}", host.major(), host.minor(), host.patch() + 1),
                "declares api-version",
            ),
        ] {
            let unit = TempConfigRoot::new();
            let root = unit.0.join("minimap");
            fs::create_dir_all(&root).expect("create unit");
            let component_path = root.join(WASM_FILE_NAME);
            fs::copy(source.join(WASM_FILE_NAME), &component_path).expect("copy component");
            let manifest = farever_more_manifest::parse(
                &fs::read(source.join(MANIFEST_FILE_NAME)).expect("read manifest"),
            )
            .expect("parse manifest");
            let mut value = serde_json::to_value(manifest).expect("json");
            value["api-version"] = serde_json::Value::String(declared.clone());
            fs::write(
                root.join(MANIFEST_FILE_NAME),
                serde_json::to_vec(&value).expect("json"),
            )
            .expect("write manifest");

            let bytes = fs::read(&component_path).expect("read component");
            let error = match CompiledPlugin::from_bytes(&engine, &component_path, &bytes) {
                Ok(_) => panic!("{declared} was accepted"),
                Err(error) => error,
            };
            assert!(error.contains(&declared), "{declared}: {error}");
            assert!(error.contains(expected), "{declared}: {error}");
        }
    }

    #[test]
    fn component_fingerprint_is_enforced_only_when_declared() {
        let bytes = b"\0asm\x01\0\0\0";
        let digest = hex::encode(Sha256::digest(bytes));
        let path = Path::new("demo/addon.wasm");
        let base = AddonManifest {
            manifest_version: 1,
            id: "demo".to_owned(),
            version: "0.1.0".to_owned(),
            sha256: digest.to_uppercase(),
            ..AddonManifest::default()
        };

        // A declared fingerprint is enforced case-insensitively.
        assert!(verify_component_fingerprint(&base, path, bytes).is_ok());

        let mismatched = AddonManifest {
            sha256: "00".repeat(32),
            ..base
        };
        let error = verify_component_fingerprint(&mismatched, path, bytes).expect_err("mismatch");
        assert!(error.contains("fingerprint"), "{error}");

        // No claim, nothing to verify.
        let silent = AddonManifest {
            sha256: String::new(),
            ..mismatched
        };
        assert!(verify_component_fingerprint(&silent, path, bytes).is_ok());
    }

    #[test]
    fn a_tampered_component_is_rejected_before_compilation() {
        let root = TempConfigRoot::new();
        fs::create_dir_all(&root.0).expect("create scratch unit");
        let component = root.0.join("addon.wasm");
        fs::write(&component, b"not a component, and not the declared bytes")
            .expect("write component");
        fs::write(
            root.0.join(MANIFEST_FILE_NAME),
            format!(
                r#"{{"manifest-version": 1, "id": "demo", "version": "0.1.0", "sha256": "{}"}}"#,
                "ab".repeat(32)
            ),
        )
        .expect("write manifest");
        let bytes = fs::read(&component).expect("read component");
        let engine = create_engine().expect("engine");

        // The fingerprint is checked first, so tampering is reported as such
        // instead of as a component that failed to compile.
        let error = match CompiledPlugin::from_bytes(&engine, &component, &bytes) {
            Ok(_) => panic!("a tampered component must not compile"),
            Err(error) => error,
        };
        assert!(error.contains("fingerprint"), "{error}");
    }

    #[test]
    fn dependency_declarations_are_required_by_default() {
        let manifest: AddonManifest = serde_json::from_str(
            r#"{
                "manifest-version": 1,
                "id": "minimap",
                "dependencies": [
                    {
                        "addon": "poi-database",
                        "services": [{ "id": "poi", "version": "^1" }]
                    }
                ]
            }"#,
        )
        .expect("parse manifest without an optional flag");
        assert!(!manifest.dependencies[0].optional);

        let manifest: AddonManifest = serde_json::from_str(
            r#"{
                "manifest-version": 1,
                "id": "farever.optional-consumer",
                "dependencies": [
                    {
                        "addon": "poi-database",
                        "services": [{ "id": "poi", "version": "^1" }],
                        "optional": true
                    }
                ]
            }"#,
        )
        .expect("parse manifest with an optional flag");
        assert!(manifest.dependencies[0].optional);
    }

    #[test]
    fn required_dependency_failures_name_the_missing_addon() {
        let directory = ServiceDirectory::default();
        let dependency = |optional| {
            vec![AddonDependency {
                addon: "poi-database".to_owned(),
                services: vec![RequiredService {
                    id: "poi".to_owned(),
                    version: "^1".to_owned(),
                }],
                optional,
            }]
        };
        assert_eq!(
            check_required_dependencies(&dependency(false), &directory),
            Err("missing required dependency \"poi-database\": add-on is unavailable".to_owned())
        );
        assert_eq!(
            check_required_dependencies(&dependency(true), &directory),
            Ok(())
        );
        assert_eq!(check_required_dependencies(&[], &directory), Ok(()));
    }

    #[test]
    #[ignore = "set FAREVER_ADDON_SMOKE_DIR to a built add-ons directory"]
    fn built_minimap_marker_press_opens_the_gps_arrow() {
        // Drives the whole marker-click path through the host boundary: the
        // minimap records its markers, a canvas press publishes a waypoint
        // request on the gps topic, and the gps add-on answers with its arrow
        // surface. Nothing else in the suite crosses the two add-ons.
        let directory = std::env::var_os("FAREVER_ADDON_SMOKE_DIR")
            .map(PathBuf::from)
            .expect("FAREVER_ADDON_SMOKE_DIR");
        // A world chest the bundled provider ships, with the player far enough
        // away that the arrow is not treated as already arrived.
        let chest = (-0.6901_f32, 1093.1557_f32);
        let mut snapshot = live_snapshot();
        snapshot.map.area_id = Some("World/W1_Siagarta".to_owned());
        snapshot.ui.open_windows.clear();
        snapshot
            .player
            .value
            .as_mut()
            .expect("player fixture")
            .position = api::Vec3 {
            x: chest.0 + 50.0,
            y: chest.1,
            z: 0.0,
        };
        let mut plugins = Plugins::load(&directory, &directory.join(".test-config"), &snapshot);
        assert_eq!(plugins.len(), 4, "{:?}", plugins.take_diagnostics());

        let advance = |plugins: &mut Plugins| {
            plugins.started = plugins
                .started
                .checked_sub(Duration::from_millis(20))
                .expect("advance monotonic test clock");
        };
        let has_arrow = |frame: &api::UiFrame| {
            frame
                .surfaces
                .iter()
                .any(|surface| surface.id.contains("wayfinder-arrow"))
        };

        // Render once so the minimap records the markers of this window.
        advance(&mut plugins);
        let initial = plugins.dispatch(&snapshot, None, &[]);
        assert!(!has_arrow(&initial), "the arrow starts hidden");

        // Press across the canvas until a marker is hit; the probe POI sits a
        // few dozen pixels from the player, and only a hit publishes.
        let mut opened = false;
        'sweep: for dx in (-48..=48).step_by(4) {
            for dy in (-48..=48).step_by(4) {
                let press = api::RoutedUiEvent {
                    owner: "minimap".to_owned(),
                    event: api::UiEvent::CanvasPressed {
                        view: api::UiView::Surface("minimap-poc".to_owned()),
                        node_id: "minimap-canvas".to_owned(),
                        x: 118.0 + f64::from(dx),
                        y: 130.0 + f64::from(dy),
                    },
                };
                let frame = plugins.dispatch(&snapshot, None, std::slice::from_ref(&press));
                if has_arrow(&frame) {
                    opened = true;
                    break 'sweep;
                }
            }
        }
        assert!(opened, "no marker press opened the GPS arrow");
    }

    #[test]
    #[ignore = "set FAREVER_ADDON_SMOKE_DIR to a built add-ons directory"]
    fn minimap_without_provider_is_rejected() {
        let built = std::env::var_os("FAREVER_ADDON_SMOKE_DIR")
            .map(PathBuf::from)
            .expect("FAREVER_ADDON_SMOKE_DIR");
        let staging =
            std::env::temp_dir().join(format!("farever-no-provider-test-{}", std::process::id()));
        let component_dir = staging.join("addons").join("minimap-only");
        fs::create_dir_all(&component_dir).expect("create isolated add-on directory");
        fs::copy(
            built.join("minimap/addon.wasm"),
            component_dir.join("addon.wasm"),
        )
        .expect("stage minimap component without its provider");
        fs::copy(
            built.join("minimap/addon.json"),
            component_dir.join("addon.json"),
        )
        .expect("stage minimap manifest without its provider");
        let config = TempConfigRoot::new();
        let mut plugins = Plugins::load(&staging.join("addons"), &config.0, &live_snapshot());
        let diagnostics = plugins.take_diagnostics();
        let _ = fs::remove_dir_all(&staging);

        // The minimap declares poi-database as a required dependency, so a
        // missing provider must fail the load loudly instead of running
        // degraded.
        assert_eq!(plugins.len(), 0, "diagnostics={diagnostics:#?}");
        assert!(
            diagnostics
                .iter()
                .any(|message| message.contains("addon.wasm")
                    && message.contains("rejected")
                    && message.contains("missing required dependency \"poi-database\"")),
            "diagnostics={diagnostics:#?}"
        );
    }

    #[test]
    fn snapshot_uses_value_presence_for_provider_readiness() {
        let snapshot = to_wit_snapshot(&live_snapshot());

        assert_eq!(snapshot.zone.status.observed_at_ms, Some(250));
        assert_eq!(snapshot.zone.status.reason, None);
        assert!(snapshot.zone.value.is_some());
        assert_eq!(snapshot.camera.status.reason, None);
        assert_eq!(
            snapshot
                .camera
                .value
                .as_ref()
                .map(|value| value.heading_radians),
            Some(1.25)
        );
        let references = &snapshot
            .combat
            .value
            .as_ref()
            .expect("live combat-reference provider")
            .references;
        assert!(matches!(
            references.as_slice(),
            [model::CombatReference {
                slot: model::CombatReferenceSlot::AutoTarget,
                position: Some(_),
            }]
        ));
        assert!(matches!(
            snapshot.party.value.as_ref().map(|party| party.members.as_slice()),
            Some([model::PartyMember {
                actor_id,
                is_local: true,
                class_id: Some(class_id),
                ..
            }]) if actor_id == "player-fixture" && class_id == "Warrior"
        ));
        assert!(matches!(
            snapshot.instance_session.value,
            Some(model::InstanceState {
                session_id: 17,
                kind: model::InstanceKind::Dungeon,
                ..
            })
        ));
    }

    #[test]
    fn damage_events_preserve_actor_attribution() {
        let event = api::HostEvent::Damage(api::DamageEvent {
            header: api::EventHeader {
                sequence: 23,
                monotonic_ms: 450,
                quality: api::SourceQuality::Observed,
            },
            source: api::CombatActorRef {
                actor_id: Some("party-member-2".to_owned()),
                relation: api::ActorRelation::GroupMember,
                kind: Some("ent.Hero".to_owned()),
            },
            target: api::CombatActorRef {
                actor_id: Some("enemy-9".to_owned()),
                relation: api::ActorRelation::Other,
                kind: Some("ent.Enemy".to_owned()),
            },
            skill_id: "Warrior_Charge".to_owned(),
            skill_display_name: Some("Charge".to_owned()),
            skill_icon: None,
            amount: 125.0,
            hit_count: 1,
            critical: false,
            killed: false,
            blocked: Some(0.0),
        });

        let model::Event::Damage(event) = to_wit_event(&event) else {
            panic!("damage event mapped to a different WIT variant");
        };
        assert_eq!(event.header.sequence, 23);
        assert_eq!(event.source.actor_id.as_deref(), Some("party-member-2"));
        assert_eq!(event.source.relation, model::ActorRelation::GroupMember);
        assert_eq!(event.target.actor_id.as_deref(), Some("enemy-9"));
        assert_eq!(event.target.relation, model::ActorRelation::Other);
    }

    #[test]
    fn group_state_events_preserve_revision_and_instance_boundaries() {
        let party = api::HostEvent::PartyChanged(api::PartyEvent {
            header: api::EventHeader {
                sequence: 31,
                monotonic_ms: 900,
                quality: api::SourceQuality::Observed,
            },
            revision: 12,
        });
        let model::Event::PartyChanged(party) = to_wit_event(&party) else {
            panic!("party event mapped to a different WIT variant");
        };
        assert_eq!(party.header.sequence, 31);
        assert_eq!(party.revision, 12);

        let instance = api::HostEvent::InstanceChanged(api::InstanceEvent {
            header: api::EventHeader {
                sequence: 32,
                monotonic_ms: 950,
                quality: api::SourceQuality::Observed,
            },
            previous: Some(api::InstanceState {
                session_id: 17,
                kind: api::InstanceKind::Dungeon,
                area_id: Some("World/W2_Dungeon".to_owned()),
            }),
            current: None,
        });
        let model::Event::InstanceChanged(instance) = to_wit_event(&instance) else {
            panic!("instance event mapped to a different WIT variant");
        };
        assert!(matches!(
            instance.previous,
            Some(model::InstanceState {
                session_id: 17,
                kind: model::InstanceKind::Dungeon,
                ..
            })
        ));
        assert!(instance.current.is_none());
    }

    #[test]
    fn domain_getters_share_one_callback_frozen_observation() {
        let mut source = live_snapshot();
        let mut host = test_host_state(
            "org.farever.domains".to_owned(),
            &source,
            Arc::new(Mutex::new(BusDirectory::default())),
        );

        let observation = farever::addon::game::Host::observation(&mut host);
        let session = farever::addon::game::Host::session(&mut host);
        let player = farever::addon::player::Host::current(&mut host);
        let party = farever::addon::party::Host::current(&mut host);
        let camera = farever::addon::camera::Host::current(&mut host);
        let combat = farever::addon::combat::Host::current(&mut host);
        let instance = farever::addon::instance_session::Host::current(&mut host);
        let zone = farever::addon::zone::Host::current(&mut host);
        let windows = farever::addon::windows::Host::current(&mut host);

        assert_eq!(observation.sequence, 7);
        assert_eq!(observation.process_session, session.process_session);
        assert_eq!(
            player.status.observed_at_ms,
            Some(observation.captured_at_ms)
        );
        assert_eq!(
            camera.status.observed_at_ms,
            Some(observation.captured_at_ms)
        );
        assert!(matches!(
            party.value.as_ref().map(|party| party.members.as_slice()),
            Some([model::PartyMember {
                actor_id,
                is_local: true,
                class_id: Some(class_id),
                ..
            }]) if actor_id == "player-fixture" && class_id == "Warrior"
        ));
        assert_eq!(
            combat.value.as_ref().and_then(|state| state.in_combat),
            Some(true)
        );
        assert!(matches!(
            instance.value,
            Some(model::InstanceState {
                session_id: 17,
                kind: model::InstanceKind::Dungeon,
                ..
            })
        ));
        assert_eq!(
            zone.value
                .as_ref()
                .and_then(|state| state.area_id.as_deref()),
            Some("World/W1_Test")
        );
        assert_eq!(
            windows
                .value
                .as_ref()
                .map(|state| state.open_windows.as_slice()),
            Some(["ui.Character".to_owned()].as_slice())
        );

        source.sequence = 8;
        source.captured_at_ms = 350;
        source
            .player
            .value
            .as_mut()
            .expect("live player")
            .position
            .x = 99.0;
        assert_eq!(
            farever::addon::game::Host::observation(&mut host).sequence,
            7,
            "mutating the adapter snapshot cannot alter the current callback observation"
        );

        host.begin_callback(&source);
        assert_eq!(
            farever::addon::game::Host::observation(&mut host).sequence,
            8
        );
        assert_eq!(
            farever::addon::player::Host::current(&mut host)
                .value
                .expect("updated player")
                .position
                .expect("updated position")
                .x,
            99.0
        );
    }

    #[test]
    fn unavailable_domains_never_carry_values() {
        let mut source = live_snapshot();
        source.session.in_world = false;
        source.player = api::StateSnapshot::unavailable(3, api::UnavailableReason::NotInWorld);
        source.party = api::StateSnapshot::unavailable(7, api::UnavailableReason::Unsupported);
        source.camera = api::StateSnapshot::unavailable(6, api::UnavailableReason::NotInWorld);
        source.combat_references =
            api::StateSnapshot::unavailable(7, api::UnavailableReason::NotInWorld);
        source.instance_session =
            api::StateSnapshot::unavailable(8, api::UnavailableReason::Unsupported);
        let snapshot = to_wit_snapshot(&source);

        assert_eq!(
            snapshot.zone.status.reason,
            Some(model::UnavailableReason::NotInWorld)
        );
        assert!(snapshot.zone.value.is_none());
        assert!(snapshot.windows.value.is_none());
        assert!(snapshot.player.value.is_none());
        assert_eq!(
            snapshot.party.status.reason,
            Some(model::UnavailableReason::Unsupported)
        );
        assert!(snapshot.party.value.is_none());
        assert!(snapshot.camera.value.is_none());
        assert!(snapshot.combat.value.is_none());
        assert_eq!(
            snapshot.instance_session.status.reason,
            Some(model::UnavailableReason::Unsupported)
        );
        assert!(snapshot.instance_session.value.is_none());
    }

    #[test]
    fn chat_output_is_bounded_and_commits_only_with_its_callback() {
        let snapshot = live_snapshot();
        let directory = Arc::new(Mutex::new(BusDirectory::default()));
        let mut host = test_host_state("org.farever.chat-test".to_owned(), &snapshot, directory);

        farever::addon::chat::Host::print(&mut host, "outside".to_owned());
        assert!(host.chat_outputs.is_empty());

        host.begin_callback(&snapshot);
        farever::addon::chat::Host::print(&mut host, format!("{}😀", "a".repeat(249)));
        farever::addon::chat::Host::print_error(&mut host, "invalid target".to_owned());
        let committed = host.finish_callback(true);
        assert_eq!(committed.chat.len(), 2);
        assert_eq!(
            committed.chat[0].text.encode_utf16().count(),
            crate::chat_output::MAX_CHAT_CODE_UNITS - 1
        );
        assert_eq!(
            committed.chat[0].style,
            crate::chat_output::ChatOutputStyle::Normal
        );
        assert_eq!(
            committed.chat[1].style,
            crate::chat_output::ChatOutputStyle::Error
        );

        host.begin_callback(&snapshot);
        farever::addon::chat::Host::print(&mut host, "discard me".to_owned());
        assert!(host.finish_callback(false).chat.is_empty());

        host.begin_callback(&snapshot);
        for index in 0..=MAX_CHAT_OUTPUTS_PER_CALLBACK {
            farever::addon::chat::Host::print(&mut host, format!("line {index}"));
        }
        assert_eq!(
            host.finish_callback(true).chat.len(),
            MAX_CHAT_OUTPUTS_PER_CALLBACK
        );
    }

    #[test]
    fn display_metadata_is_single_line_and_bounded() {
        let name = sanitize_metadata("damage\nmeter-0123456789", 12);

        assert_eq!(name, "damage meter");
        assert!(name.len() <= 12);
    }

    #[test]
    fn config_host_registers_and_persists_without_a_callback_write_limit() {
        let root = TempConfigRoot::new();
        let snapshot = live_snapshot();
        let directory = Arc::new(Mutex::new(BusDirectory::default()));
        let config = ConfigRegistry::open(&root.0, "org.farever.config", Some("2.4.0"))
            .expect("config registry");
        let mut host = HostState::new(
            "org.farever.config".to_owned(),
            &snapshot,
            directory,
            Rc::new(RefCell::new(ServiceDirectory::default())),
            Vec::new(),
            config,
        );
        let descriptor = model::ConfigPropertyDescriptor {
            key: "counter".to_owned(),
            label: "Counter".to_owned(),
            description: Some("A persisted counter".to_owned()),
            value_kind: model::ConfigValueKind::Integer,
            default_value: model::ConfigValue::Integer(0),
            access: model::ConfigPropertyAccess::Editable,
        };
        assert!(matches!(
            farever::addon::config::Host::register_property(&mut host, descriptor),
            Ok(model::ConfigValue::Integer(0))
        ));

        for value in 0..16 {
            let status = farever::addon::config::Host::set(
                &mut host,
                "counter".to_owned(),
                model::ConfigValue::Integer(value),
            )
            .expect("each update persists immediately");
            assert_eq!(status.revision, u64::try_from(value).unwrap() + 1);
        }
        let status = farever::addon::config::Host::status(&mut host);
        assert_eq!(status.revision, 16);
        assert_eq!(status.saved_by_addon_version.as_deref(), Some("2.4.0"));
        drop(host);

        let mut reopened = ConfigRegistry::open(&root.0, "org.farever.config", Some("3.0.0"))
            .expect("reopen config registry");
        let stored = reopened
            .register(ConfigPropertyDescriptor {
                key: "counter".to_owned(),
                label: "Counter".to_owned(),
                description: None,
                value_kind: ConfigValueKind::Integer,
                default_value: ConfigValue::Integer(0),
                access: ConfigPropertyAccess::Editable,
            })
            .expect("register persisted property after upgrade");
        assert_eq!(stored, ConfigValue::Integer(15));
        assert_eq!(
            reopened.status().saved_by_addon_version.as_deref(),
            Some("2.4.0")
        );
    }

    #[test]
    fn readonly_and_hidden_descriptors_are_inventoried_but_remain_addon_writable() {
        let root = TempConfigRoot::new();
        let snapshot = live_snapshot();
        let directory = Arc::new(Mutex::new(BusDirectory::default()));
        let config = ConfigRegistry::open(&root.0, "org.farever.config", Some("1.0.0"))
            .expect("config registry");
        let mut host = HostState::new(
            "org.farever.config".to_owned(),
            &snapshot,
            directory,
            Rc::new(RefCell::new(ServiceDirectory::default())),
            Vec::new(),
            config,
        );
        for (key, value_kind, default_value, access) in [
            (
                "detected-path",
                model::ConfigValueKind::Text,
                model::ConfigValue::Text(String::new()),
                model::ConfigPropertyAccess::Readonly,
            ),
            (
                "internal-cache",
                model::ConfigValueKind::Bytes,
                model::ConfigValue::Bytes(Vec::new()),
                model::ConfigPropertyAccess::Hidden,
            ),
        ] {
            farever::addon::config::Host::register_property(
                &mut host,
                model::ConfigPropertyDescriptor {
                    key: key.to_owned(),
                    label: key.to_owned(),
                    description: None,
                    value_kind,
                    default_value,
                    access,
                },
            )
            .expect("register property");
        }
        host.accepting_config_registrations = false;

        farever::addon::config::Host::set(
            &mut host,
            "detected-path".to_owned(),
            model::ConfigValue::Text("C:/Farever".to_owned()),
        )
        .expect("the addon may update readonly presentation data");
        farever::addon::config::Host::set(
            &mut host,
            "internal-cache".to_owned(),
            model::ConfigValue::Bytes(vec![1, 2, 3]),
        )
        .expect("the addon may update hidden state");
        let inventory = host
            .config
            .properties()
            .iter()
            .map(|descriptor| to_api_config_property(&host.namespace, descriptor))
            .collect::<Vec<_>>();
        assert_eq!(inventory[0].access, api::ConfigPropertyAccess::Readonly);
        assert_eq!(inventory[1].access, api::ConfigPropertyAccess::Hidden);

        let late_registration = farever::addon::config::Host::register_property(
            &mut host,
            model::ConfigPropertyDescriptor {
                key: "late".to_owned(),
                label: "Late".to_owned(),
                description: None,
                value_kind: model::ConfigValueKind::Boolean,
                default_value: model::ConfigValue::Boolean(false),
                access: model::ConfigPropertyAccess::Editable,
            },
        );
        assert!(late_registration
            .expect_err("registration closes after activation")
            .contains("plugin.activate"));
    }

    #[test]
    fn component_memory_can_transport_the_full_aggregate_config_quota() {
        assert!(MAX_COMPONENT_MEMORY > crate::config::MAX_CONFIG_BYTES_PER_ADDON as usize);
    }

    #[test]
    fn tick_schedule_clamps_and_waits_for_the_first_interval() {
        let (mut schedule, clamped) = TickSchedule::new(1, 1_000);

        assert!(clamped);
        assert_eq!(schedule.interval_ms, MIN_TICK_INTERVAL_MS);
        assert!(schedule.take_due(1_015).is_none());
        let tick = schedule.take_due(1_016).expect("first due tick");
        assert_eq!(tick.scheduled_at_ms, 1_016);
        assert_eq!(tick.delivered_at_ms, 1_016);
        assert_eq!(tick.elapsed_ms, u64::from(MIN_TICK_INTERVAL_MS));
        assert_eq!(tick.missed, 0);
    }

    #[test]
    fn late_ticks_are_coalesced_instead_of_replayed() {
        let (mut schedule, clamped) = TickSchedule::new(100, 0);
        assert!(!clamped);

        let tick = schedule.take_due(350).expect("coalesced tick");
        assert_eq!(tick.scheduled_at_ms, 300);
        assert_eq!(tick.delivered_at_ms, 350);
        assert_eq!(tick.elapsed_ms, 100);
        assert_eq!(tick.missed, 2);
        assert_eq!(schedule.next_due_ms, 400);

        let next = schedule.take_due(425).expect("next tick");
        assert_eq!(next.scheduled_at_ms, 400);
        assert_eq!(next.elapsed_ms, 75);
        assert_eq!(next.missed, 0);
    }

    #[test]
    fn scheduled_ticks_are_clamped_last_write_wins_and_failures_discard_them() {
        let snapshot = live_snapshot();
        let directory = Arc::new(Mutex::new(BusDirectory::default()));
        let mut host = test_host_state("org.farever.timer".to_owned(), &snapshot, directory);

        assert_eq!(
            farever::addon::runtime::Host::schedule_tick(&mut host, 1),
            MIN_TICK_INTERVAL_MS
        );
        assert_eq!(host.pending_tick_interval_ms, None);

        host.accepting_tick_scheduling = true;
        assert_eq!(
            farever::addon::runtime::Host::schedule_tick(&mut host, 1),
            MIN_TICK_INTERVAL_MS
        );
        assert_eq!(host.pending_tick_interval_ms, Some(MIN_TICK_INTERVAL_MS));

        host.begin_callback(&snapshot);
        assert_eq!(
            farever::addon::runtime::Host::schedule_tick(&mut host, u32::MAX),
            MAX_TICK_INTERVAL_MS
        );
        assert_eq!(
            farever::addon::runtime::Host::schedule_tick(&mut host, 250),
            250
        );
        let committed = host.finish_callback(true);
        assert_eq!(committed.scheduled_tick_interval_ms, Some(250));

        host.begin_callback(&snapshot);
        farever::addon::runtime::Host::schedule_tick(&mut host, 500);
        let discarded = host.finish_callback(false);
        assert_eq!(discarded.scheduled_tick_interval_ms, None);
        assert_eq!(host.pending_tick_interval_ms, None);
    }

    #[test]
    fn empty_event_batches_do_not_create_callback_work() {
        let mut batch = empty_batch(9);
        assert!(!has_event_work(&batch));

        batch.snapshot_required = true;
        assert!(has_event_work(&batch));
        batch.snapshot_required = false;
        batch.dropped_before = 2;
        assert!(has_event_work(&batch));
    }

    #[test]
    fn message_topics_are_bounded_lowercase_protocol_names() {
        assert_eq!(validate_topic("farever.wayfinding/set-target@1"), Ok(()));
        assert_eq!(
            validate_topic("Farever.Wayfinding/SetTarget@1"),
            Err(model::MessageError::InvalidTopic)
        );
        assert_eq!(
            validate_topic("farever.wayfinding//set-target@1"),
            Err(model::MessageError::InvalidTopic)
        );
        assert_eq!(
            validate_topic(&"a".repeat(MAX_TOPIC_BYTES + 1)),
            Err(model::MessageError::InvalidTopic)
        );
    }

    #[test]
    fn bus_routes_broadcast_and_direct_messages_to_exact_subscribers() {
        let directory = Arc::new(Mutex::new(BusDirectory::default()));
        let snapshot = live_snapshot();
        let topic = "farever.wayfinding/set-target@1";
        let mut sender = test_host_state(
            "org.farever.minimap".to_owned(),
            &snapshot,
            Arc::clone(&directory),
        );
        let mut receiver = test_host_state("org.gps".to_owned(), &snapshot, Arc::clone(&directory));
        farever::addon::bus::Host::subscribe(&mut sender, topic.to_owned())
            .expect("sender subscription");
        farever::addon::bus::Host::subscribe(&mut receiver, topic.to_owned())
            .expect("receiver subscription");
        {
            let mut directory = directory
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            directory
                .register(&sender.namespace, &sender.subscriptions)
                .expect("unique sender ID");
            directory
                .register(&receiver.namespace, &receiver.subscriptions)
                .expect("unique receiver ID");
        }
        sender.accepting_subscriptions = false;
        receiver.accepting_subscriptions = false;

        let wrong_phase = farever::addon::bus::Host::publish(
            &mut sender,
            topic.to_owned(),
            model::MessageTarget::Subscribers,
            None,
            vec![1],
        );
        assert_eq!(wrong_phase, Err(model::MessageError::WrongLifecyclePhase));

        sender.begin_callback(&snapshot);
        let broadcast = farever::addon::bus::Host::publish(
            &mut sender,
            topic.to_owned(),
            model::MessageTarget::Subscribers,
            Some(7),
            vec![1, 2, 3],
        );
        assert_eq!(broadcast, Ok(1), "broadcast excludes its publisher");
        let direct = farever::addon::bus::Host::publish(
            &mut sender,
            topic.to_owned(),
            model::MessageTarget::Addon(receiver.namespace.clone()),
            Some(8),
            vec![4, 5],
        );
        assert_eq!(direct, Ok(1));
        let missing = farever::addon::bus::Host::publish(
            &mut sender,
            topic.to_owned(),
            model::MessageTarget::Addon("org.farever.missing".to_owned()),
            None,
            Vec::new(),
        );
        assert_eq!(missing, Ok(0));

        let staged = sender.finish_callback(true);
        assert_eq!(staged.outbound.len(), 2);
        assert_eq!(staged.outbound[0].source_addon_id, "org.farever.minimap");
        assert_eq!(staged.outbound[0].recipients, ["org.gps"]);
        assert_eq!(staged.outbound[0].correlation_id, Some(7));

        let mut plugins = Plugins {
            loaded: Vec::new(),
            diagnostics: Vec::new(),
            started: Instant::now(),
            epoch_ms: snapshot.captured_at_ms,
            idle_batch: empty_batch(snapshot.session.process_session),
            bus_directory: Arc::clone(&directory),
            service_directory: Rc::new(RefCell::new(ServiceDirectory::default())),
            message_queues: HashMap::new(),
            next_message_id: 1,
        };
        plugins.enqueue_messages(staged.outbound, 300);
        let (dropped_before, delivered) = plugins
            .message_queues
            .remove("org.gps")
            .expect("receiver queue")
            .into_batch();
        assert_eq!(dropped_before, 0);
        assert_eq!(
            delivered
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(delivered[0].monotonic_ms, 300);
        assert_eq!(delivered[0].source_addon_id, "org.farever.minimap");
        assert_eq!(delivered[0].topic, topic);
        assert_eq!(delivered[0].payload, [1, 2, 3]);

        let late_subscription = farever::addon::bus::Host::subscribe(
            &mut receiver,
            "farever.wayfinding/clear-target@1".to_owned(),
        );
        assert_eq!(
            late_subscription,
            Err(model::MessageError::WrongLifecyclePhase)
        );
    }

    #[test]
    fn host_messages_broadcast_to_all_exact_subscribers() {
        let directory = Arc::new(Mutex::new(BusDirectory::default()));
        let snapshot = live_snapshot();
        {
            let mut entries = directory
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            entries
                .register("org.farever.first", &["gps".to_owned()])
                .expect("first subscriber");
            entries
                .register("org.farever.second", &["gps".to_owned()])
                .expect("second subscriber");
            entries
                .register("org.farever.other", &["note".to_owned()])
                .expect("other subscriber");
        }
        let mut plugins = Plugins {
            loaded: Vec::new(),
            diagnostics: Vec::new(),
            started: Instant::now(),
            epoch_ms: snapshot.captured_at_ms,
            idle_batch: empty_batch(snapshot.session.process_session),
            bus_directory: Arc::clone(&directory),
            service_directory: Rc::new(RefCell::new(ServiceDirectory::default())),
            message_queues: HashMap::new(),
            next_message_id: 1,
        };

        assert_eq!(
            plugins.broadcast_host_message("gps".to_owned(), b"1 2".to_vec()),
            Ok(2)
        );
        assert!(plugins
            .broadcast_host_message("GPS".to_owned(), Vec::new())
            .is_err());
        assert!(!plugins.message_queues.contains_key("org.farever.other"));

        for subscriber in ["org.farever.first", "org.farever.second"] {
            let (_, delivered) = plugins
                .message_queues
                .remove(subscriber)
                .expect("subscriber queue")
                .into_batch();
            assert_eq!(delivered.len(), 1);
            assert_eq!(delivered[0].source_addon_id, HOST_MESSAGE_SOURCE_ID);
            assert_eq!(delivered[0].topic, "gps");
            assert_eq!(delivered[0].payload, b"1 2");
            assert_eq!(delivered[0].correlation_id, None);
        }
    }

    #[test]
    fn failed_callbacks_discard_staged_messages() {
        let directory = Arc::new(Mutex::new(BusDirectory::default()));
        let snapshot = live_snapshot();
        let topic = "farever.test/update@1";
        let mut sender = test_host_state(
            "org.farever.sender".to_owned(),
            &snapshot,
            Arc::clone(&directory),
        );
        let mut receiver = test_host_state(
            "org.farever.receiver".to_owned(),
            &snapshot,
            Arc::clone(&directory),
        );
        farever::addon::bus::Host::subscribe(&mut receiver, topic.to_owned())
            .expect("receiver subscription");
        {
            let mut directory = directory
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            directory
                .register(&sender.namespace, &sender.subscriptions)
                .expect("unique sender ID");
            directory
                .register(&receiver.namespace, &receiver.subscriptions)
                .expect("unique receiver ID");
        }
        sender.accepting_subscriptions = false;
        sender.begin_callback(&snapshot);
        assert_eq!(
            farever::addon::bus::Host::publish(
                &mut sender,
                topic.to_owned(),
                model::MessageTarget::Subscribers,
                None,
                vec![9],
            ),
            Ok(1)
        );

        assert!(sender.finish_callback(false).outbound.is_empty());
        assert!(sender.outbound_messages.is_empty());
    }

    #[test]
    fn bus_enforces_subscription_and_publication_quotas() {
        let directory = Arc::new(Mutex::new(BusDirectory::default()));
        let snapshot = live_snapshot();
        let mut host = test_host_state(
            "org.farever.fixture".to_owned(),
            &snapshot,
            Arc::clone(&directory),
        );
        for index in 0..MAX_SUBSCRIPTIONS_PER_ADDON {
            farever::addon::bus::Host::subscribe(
                &mut host,
                format!("farever.test/topic-{index}@1"),
            )
            .expect("subscription within limit");
        }
        assert_eq!(host.subscriptions.len(), MAX_SUBSCRIPTIONS_PER_ADDON);
        assert_eq!(
            farever::addon::bus::Host::subscribe(
                &mut host,
                "farever.test/one-too-many@1".to_owned(),
            ),
            Err(model::MessageError::TooManySubscriptions)
        );

        host.accepting_subscriptions = false;
        host.begin_callback(&snapshot);
        assert_eq!(
            farever::addon::bus::Host::publish(
                &mut host,
                "farever.test/update@1".to_owned(),
                model::MessageTarget::Subscribers,
                None,
                vec![0; MAX_MESSAGE_PAYLOAD_BYTES + 1],
            ),
            Err(model::MessageError::PayloadTooLarge)
        );
        for _ in 0..MAX_OUTBOUND_MESSAGES_PER_CALLBACK {
            assert_eq!(
                farever::addon::bus::Host::publish(
                    &mut host,
                    "farever.test/update@1".to_owned(),
                    model::MessageTarget::Subscribers,
                    None,
                    Vec::new(),
                ),
                Ok(0)
            );
        }
        assert_eq!(
            farever::addon::bus::Host::publish(
                &mut host,
                "farever.test/update@1".to_owned(),
                model::MessageTarget::Subscribers,
                None,
                Vec::new(),
            ),
            Err(model::MessageError::QuotaExceeded)
        );
    }

    #[test]
    fn receiver_queue_is_fifo_and_reports_overflow() {
        let mut queue = MessageQueue::default();
        for id in 1..=u64::try_from(MAX_PENDING_MESSAGES_PER_ADDON).unwrap() + 1 {
            queue.push(BusMessage {
                id,
                monotonic_ms: 500 + id,
                source_addon_id: Arc::from("org.farever.sender"),
                topic: Arc::from("farever.test/update@1"),
                correlation_id: None,
                payload: Arc::from([]),
            });
        }

        let (dropped_before, messages) = queue.into_batch();
        assert_eq!(dropped_before, 1);
        assert_eq!(messages.len(), MAX_PENDING_MESSAGES_PER_ADDON);
        assert_eq!(messages.first().map(|message| message.id), Some(1));
        assert_eq!(
            messages.last().map(|message| message.id),
            Some(u64::try_from(MAX_PENDING_MESSAGES_PER_ADDON).unwrap())
        );
    }

    #[test]
    #[ignore = "set FAREVER_ADDON_SMOKE_DIR to a built add-ons directory"]
    fn built_components_load_and_run_through_the_host_boundary() {
        let directory = std::env::var_os("FAREVER_ADDON_SMOKE_DIR")
            .map(PathBuf::from)
            .expect("FAREVER_ADDON_SMOKE_DIR");
        let mut initial = live_snapshot();
        initial.map.area_id = Some("World/W1_Siagarta".to_owned());
        initial.ui.open_windows.clear();
        let mut plugins = Plugins::load(&directory, &directory.join(".test-config"), &initial);
        assert_eq!(plugins.len(), 4, "{:?}", plugins.take_diagnostics());
        let tick_interval = |plugins: &Plugins, namespace: &str| {
            plugins
                .loaded
                .iter()
                .find(|plugin| plugin.namespace == namespace)
                .and_then(|plugin| plugin.tick.as_ref())
                .map(|tick| tick.interval_ms)
        };
        assert_eq!(tick_interval(&plugins, "dyno"), None);
        assert_eq!(
            tick_interval(&plugins, "gps"),
            Some(50),
            "{:?}",
            plugins.diagnostics
        );
        assert_eq!(
            tick_interval(&plugins, "minimap"),
            Some(16),
            "{:?}",
            plugins.diagnostics
        );
        plugins
            .message_queues
            .entry("gps".to_owned())
            .or_default()
            .push(BusMessage {
                id: 1,
                monotonic_ms: 250,
                source_addon_id: Arc::from("org.farever.test-producer"),
                topic: Arc::from("farever.test/noop@1"),
                correlation_id: Some(42),
                payload: Arc::from([1, 2, 3]),
            });
        plugins.started = plugins
            .started
            .checked_sub(Duration::from_millis(20))
            .expect("advance monotonic test clock");
        let initial_frame = plugins.dispatch(&initial, None, &[]);
        assert_eq!(initial_frame.surfaces.len(), 1);
        assert!(initial_frame.surfaces[0].id.contains("minimap-poc"));
        assert!(initial_frame
            .config_menus
            .iter()
            .any(|menu| { menu.owner == "dyno" && menu.id == "settings" }));

        let toggle_visibility = api::RoutedUiEvent {
            owner: "dyno".to_owned(),
            event: api::UiEvent::CheckboxChanged {
                node_id: "meter-visible".to_owned(),
                checked: false,
            },
        };
        let hidden_frame =
            plugins.dispatch(&initial, None, std::slice::from_ref(&toggle_visibility));
        assert!(hidden_frame.config_menus.iter().any(|menu| {
            menu.owner == "dyno"
                && menu.nodes.iter().any(|node| {
                    matches!(
                        &node.widget,
                        api::Widget::Checkbox(checkbox)
                            if node.id == "meter-visible" && !checkbox.checked
                    )
                })
        }));

        drop(plugins);
        let mut plugins = Plugins::load(&directory, &directory.join(".test-config"), &initial);
        assert_eq!(plugins.len(), 4, "{:?}", plugins.take_diagnostics());
        let restored_frame = plugins.dispatch(&initial, None, &[]);
        assert!(restored_frame.config_menus.iter().any(|menu| {
            menu.owner == "dyno"
                && menu.nodes.iter().any(|node| {
                    matches!(
                        &node.widget,
                        api::Widget::Checkbox(checkbox)
                            if node.id == "meter-visible" && !checkbox.checked
                    )
                })
        }));

        let show_visibility = api::RoutedUiEvent {
            owner: "dyno".to_owned(),
            event: api::UiEvent::CheckboxChanged {
                node_id: "meter-visible".to_owned(),
                checked: true,
            },
        };
        let visible_frame = plugins.dispatch(&initial, None, &[show_visibility]);
        assert!(visible_frame.config_menus.iter().any(|menu| {
            menu.owner == "dyno"
                && menu.nodes.iter().any(|node| {
                    matches!(
                        &node.widget,
                        api::Widget::Checkbox(checkbox)
                            if node.id == "meter-visible" && checkbox.checked
                    )
                })
        }));

        plugins.started = plugins
            .started
            .checked_sub(Duration::from_millis(300))
            .expect("advance monotonic test clock");
        let mut event_batch = empty_batch(initial.session.process_session);
        event_batch.snapshot_required = true;
        let lost_frame = plugins.dispatch(&initial, Some(&event_batch), &[]);
        assert_eq!(lost_frame.surfaces.len(), 1);
        assert!(lost_frame.surfaces[0].id.contains("minimap-poc"));

        plugins
            .message_queues
            .entry("gps".to_owned())
            .or_default()
            .push(BusMessage {
                id: 2,
                monotonic_ms: 300,
                source_addon_id: Arc::from("farever.host"),
                topic: Arc::from("gps"),
                correlation_id: None,
                payload: Arc::from(b"100 100".as_slice()),
            });
        let mut moved = initial;
        moved.sequence += 1;
        moved.player.value.as_mut().expect("player").position.x += 10.0;
        plugins.started = plugins
            .started
            .checked_sub(Duration::from_millis(60))
            .expect("advance monotonic test clock");
        let frame = plugins.dispatch(&moved, None, &[]);

        assert_eq!(frame.surfaces.len(), 2, "{:?}", plugins.take_diagnostics());
        assert!(frame
            .surfaces
            .iter()
            .any(|surface| surface.id.contains("wayfinder-arrow")));
        assert!(frame
            .surfaces
            .iter()
            .any(|surface| surface.id.contains("minimap-poc")));

        let combat_started = api::EventBatch {
            process_session: moved.session.process_session,
            first_sequence: Some(20),
            next_sequence: 21,
            dropped_before: 0,
            snapshot_required: false,
            events: vec![api::HostEvent::CombatStarted(api::CombatEvent {
                header: api::EventHeader {
                    sequence: 20,
                    monotonic_ms: 1_000,
                    quality: api::SourceQuality::Observed,
                },
                fight_id: 1,
            })],
        };
        plugins.dispatch(&moved, Some(&combat_started), &[]);
        assert_eq!(tick_interval(&plugins, "dyno"), Some(250));

        let combat_ended = api::EventBatch {
            process_session: moved.session.process_session,
            first_sequence: Some(21),
            next_sequence: 22,
            dropped_before: 0,
            snapshot_required: false,
            events: vec![api::HostEvent::CombatEnded(api::CombatEvent {
                header: api::EventHeader {
                    sequence: 21,
                    monotonic_ms: 1_250,
                    quality: api::SourceQuality::Observed,
                },
                fight_id: 1,
            })],
        };
        plugins.dispatch(&moved, Some(&combat_ended), &[]);
        plugins.started = plugins
            .started
            .checked_sub(Duration::from_millis(260))
            .expect("advance through the dyno tick");
        plugins.dispatch(&moved, None, &[]);
        assert_eq!(tick_interval(&plugins, "dyno"), None);
        assert!(plugins
            .take_diagnostics()
            .iter()
            .all(|message| !message.contains("failed") && !message.contains("rejected")));
    }

    #[test]
    fn embedded_fonts_are_bounded_style_scoped_init_resources() {
        let regular = include_bytes!("../../addons/dyno/assets/fonts/NotoSans-Regular.ttf");
        let mut host = test_host_state(
            "fixture-addon".to_owned(),
            &live_snapshot(),
            Arc::new(Mutex::new(BusDirectory::default())),
        );
        farever::addon::assets::Host::register_font(
            &mut host,
            "noto-regular".to_owned(),
            vec![model::TextStyle::Body, model::TextStyle::Small],
            regular.to_vec(),
        )
        .expect("valid embedded OpenType font");

        assert_eq!(host.font_assets.len(), 1);
        assert_eq!(host.font_bytes, regular.len());
        assert_eq!(
            host.font_families.get(&api::TextStyle::Body),
            Some(&"addon-font/fixture-addon/noto-regular".to_owned())
        );

        let duplicate_style = farever::addon::assets::Host::register_font(
            &mut host,
            "other".to_owned(),
            vec![model::TextStyle::Body],
            regular.to_vec(),
        )
        .expect_err("one add-on face per semantic style");
        assert!(duplicate_style.contains("already has a registered font"));

        host.accepting_font_registrations = false;
        let after_activation = farever::addon::assets::Host::register_font(
            &mut host,
            "late".to_owned(),
            vec![model::TextStyle::Heading],
            regular.to_vec(),
        )
        .expect_err("registration is activation-only");
        assert!(after_activation.contains("only be registered during plugin.activate"));
    }

    #[test]
    fn embedded_images_are_decoded_namespaced_and_activation_scoped() {
        use image::ImageEncoder;

        let pixels = [
            255, 0, 0, 255, // opaque red
            0, 0, 255, 128, // translucent blue
        ];
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&pixels, 2, 1, image::ExtendedColorType::Rgba8)
            .expect("encode PNG fixture");
        let mut host = test_host_state(
            "fixture-addon".to_owned(),
            &live_snapshot(),
            Arc::new(Mutex::new(BusDirectory::default())),
        );

        let reference = farever::addon::assets::Host::register_image(
            &mut host,
            "class-warrior".to_owned(),
            png.clone(),
        )
        .expect("valid embedded PNG");

        assert_eq!(reference.id, "addon-image/fixture-addon/class-warrior");
        assert_eq!(host.image_assets.len(), 1);
        assert_eq!(host.image_assets[0].width, 2);
        assert_eq!(host.image_assets[0].height, 1);
        assert_eq!(host.image_assets[0].rgba.as_ref(), pixels);

        let duplicate = farever::addon::assets::Host::register_image(
            &mut host,
            "class-warrior".to_owned(),
            png.clone(),
        )
        .expect_err("duplicate add-on-local image ID");
        assert!(duplicate.contains("duplicate image id"));

        let invalid = farever::addon::assets::Host::register_image(
            &mut host,
            "not-png".to_owned(),
            b"not a PNG".to_vec(),
        )
        .expect_err("invalid PNG bytes");
        assert!(invalid.contains("not a supported PNG"));
        assert_eq!(host.image_registration_attempts, 2);

        host.accepting_image_registrations = false;
        let after_activation =
            farever::addon::assets::Host::register_image(&mut host, "late".to_owned(), png)
                .expect_err("registration is activation-only");
        assert!(after_activation.contains("only be registered during plugin.activate"));
    }

    #[test]
    fn embedded_image_decode_failures_are_work_bounded() {
        let mut host = test_host_state(
            "fixture-addon".to_owned(),
            &live_snapshot(),
            Arc::new(Mutex::new(BusDirectory::default())),
        );
        for index in 0..api::MAX_IMAGES_PER_ADDON {
            let error = farever::addon::assets::Host::register_image(
                &mut host,
                format!("broken-{index}"),
                vec![0],
            )
            .expect_err("malformed image");
            assert!(error.contains("not a supported PNG"));
        }

        let exhausted = farever::addon::assets::Host::register_image(
            &mut host,
            "one-too-many".to_owned(),
            vec![0],
        )
        .expect_err("decode attempt quota");
        assert!(exhausted.contains("may attempt at most"));
        assert_eq!(host.image_registration_attempts, api::MAX_IMAGES_PER_ADDON);
    }

    #[test]
    fn semantic_text_style_resolves_to_the_addon_font_family() {
        let frame = model::UiFrame {
            surfaces: vec![model::UiSurface {
                id: "main".to_owned(),
                title: "Font fixture".to_owned(),
                anchor: model::SurfaceAnchor::TopLeft,
                margin_x: 0.0,
                margin_y: 0.0,
                width: None,
                style: None,
                nodes: vec![model::UiNode {
                    id: "title".to_owned(),
                    parent: None,
                    widget: model::Widget::Text(model::TextWidget {
                        text: "Damage".to_owned(),
                        style: model::TextStyle::Strong,
                        color: None,
                        outline: None,
                        wrap: false,
                    }),
                }],
                canvas: Vec::new(),
            }],
            config_menus: Vec::new(),
        };
        let fonts = HashMap::from([(
            api::TextStyle::Strong,
            "addon-font/test-addon/noto-bold".to_owned(),
        )]);

        let validated = validate_wit_frame_with_fonts(frame, "test-addon", &fonts)
            .expect("valid font-mapped UI frame");
        let api::Widget::Text(text) = &validated.surfaces[0].nodes[0].widget else {
            panic!("fixture should remain text");
        };
        assert_eq!(
            text.font_family.as_deref(),
            Some("addon-font/test-addon/noto-bold")
        );
    }

    #[test]
    fn config_menu_section_owns_widget_children() {
        // A section is parent-capable like a container: the host styles the
        // header and renders ordinary widgets underneath it.
        let frame = model::UiFrame {
            surfaces: Vec::new(),
            config_menus: vec![model::ConfigMenu {
                id: "settings".to_owned(),
                title: "Fixture".to_owned(),
                nodes: vec![
                    model::UiNode {
                        id: "poi".to_owned(),
                        parent: None,
                        widget: model::Widget::Section(model::SectionWidget {
                            title: "Points of interest".to_owned(),
                            description: Some("Cached from the POI database".to_owned()),
                        }),
                    },
                    model::UiNode {
                        id: "markers".to_owned(),
                        parent: Some("poi".to_owned()),
                        widget: model::Widget::Checkbox(model::CheckboxWidget {
                            label: "Show on map".to_owned(),
                            checked: true,
                            enabled: true,
                        }),
                    },
                ],
                canvas: Vec::new(),
            }],
        };

        let validated = validate_wit_frame(frame, "fixture").expect("valid section frame");
        let menu = &validated.config_menus[0];
        assert!(matches!(
            &menu.nodes[0].widget,
            api::Widget::Section(section)
                if section.title == "Points of interest"
                    && section.description.as_deref() == Some("Cached from the POI database")
        ));
        assert_eq!(menu.nodes[1].parent, Some(0));

        // A leaf widget still cannot own children.
        let leaf_parent = model::UiFrame {
            surfaces: Vec::new(),
            config_menus: vec![model::ConfigMenu {
                id: "settings".to_owned(),
                title: "Fixture".to_owned(),
                nodes: vec![
                    model::UiNode {
                        id: "poi".to_owned(),
                        parent: None,
                        widget: model::Widget::Text(model::TextWidget {
                            text: "Points of interest".to_owned(),
                            style: model::TextStyle::Body,
                            color: None,
                            outline: None,
                            wrap: false,
                        }),
                    },
                    model::UiNode {
                        id: "markers".to_owned(),
                        parent: Some("poi".to_owned()),
                        widget: model::Widget::Checkbox(model::CheckboxWidget {
                            label: "Show on map".to_owned(),
                            checked: true,
                            enabled: true,
                        }),
                    },
                ],
                canvas: Vec::new(),
            }],
        };
        assert!(validate_wit_frame(leaf_parent, "fixture").is_err());
    }

    #[test]
    fn ui_frame_resolves_parent_and_canvas_ids() {
        let frame = model::UiFrame {
            surfaces: vec![model::UiSurface {
                id: "main".to_owned(),
                title: "Test".to_owned(),
                anchor: model::SurfaceAnchor::TopLeft,
                margin_x: 10.0,
                margin_y: 20.0,
                width: Some(300.0),
                style: None,
                nodes: vec![
                    model::UiNode {
                        id: "root".to_owned(),
                        parent: None,
                        widget: model::Widget::Container(model::ContainerWidget {
                            direction: model::LayoutDirection::Vertical,
                            style: model::ContainerStyle::Plain,
                            spacing: Some(4.0),
                            max_height: None,
                        }),
                    },
                    model::UiNode {
                        id: "plot".to_owned(),
                        parent: Some("root".to_owned()),
                        widget: model::Widget::Canvas(model::CanvasWidget {
                            size: model::Size {
                                width: 200.0,
                                height: 100.0,
                            },
                        }),
                    },
                ],
                canvas: vec![model::CanvasCommand {
                    canvas_id: "plot".to_owned(),
                    primitive: model::CanvasPrimitive::Line(model::LinePrimitive {
                        start: model::Point { x: 0.0, y: 0.0 },
                        end: model::Point { x: 10.0, y: 10.0 },
                        stroke: model::Stroke {
                            width: 1.0,
                            color: model::Rgba {
                                red: 1.0,
                                green: 1.0,
                                blue: 1.0,
                                alpha: 1.0,
                            },
                        },
                    }),
                }],
            }],
            config_menus: Vec::new(),
        };

        let validated = validate_wit_frame(frame, "test-addon").expect("valid UI frame");
        let surface = &validated.surfaces[0];
        assert_eq!(surface.owner, "test-addon");
        assert_eq!(surface.nodes[1].parent, Some(0));
        assert_eq!(surface.canvas[0].canvas, 1);
    }

    #[test]
    fn config_menu_is_namespaced_and_accepts_semantic_controls() {
        let frame = model::UiFrame {
            surfaces: Vec::new(),
            config_menus: vec![model::ConfigMenu {
                id: "settings".to_owned(),
                title: "Fixture".to_owned(),
                nodes: vec![
                    model::UiNode {
                        id: "enabled".to_owned(),
                        parent: None,
                        widget: model::Widget::Checkbox(model::CheckboxWidget {
                            label: "Enabled".to_owned(),
                            checked: true,
                            enabled: true,
                        }),
                    },
                    model::UiNode {
                        id: "run".to_owned(),
                        parent: None,
                        widget: model::Widget::Button(model::ButtonWidget {
                            label: "Run now".to_owned(),
                            enabled: true,
                        }),
                    },
                    model::UiNode {
                        id: "theme".to_owned(),
                        parent: None,
                        widget: model::Widget::Dropdown(model::DropdownWidget {
                            label: "Theme".to_owned(),
                            selected_id: "warm".to_owned(),
                            options: vec![
                                model::DropdownOption {
                                    id: "warm".to_owned(),
                                    label: "Warm".to_owned(),
                                },
                                model::DropdownOption {
                                    id: "cool".to_owned(),
                                    label: "Cool".to_owned(),
                                },
                            ],
                            enabled: true,
                        }),
                    },
                    model::UiNode {
                        id: "scale".to_owned(),
                        parent: None,
                        widget: model::Widget::Slider(model::SliderWidget {
                            label: "Scale".to_owned(),
                            value: 1.0,
                            minimum: 0.5,
                            maximum: 2.0,
                            step: Some(0.25),
                            enabled: true,
                        }),
                    },
                ],
                canvas: Vec::new(),
            }],
        };

        let validated =
            validate_wit_frame(frame, "org.farever.fixture").expect("valid config menu frame");
        let menu = &validated.config_menus[0];
        assert_eq!(menu.owner, "org.farever.fixture");
        assert_eq!(menu.id, "settings");
        assert!(matches!(
            &menu.nodes[1].widget,
            api::Widget::Button(button) if button.label == "Run now" && button.enabled
        ));
        assert!(matches!(
            &menu.nodes[0].widget,
            api::Widget::Checkbox(checkbox)
                if checkbox.label == "Enabled" && checkbox.checked && checkbox.enabled
        ));
        assert!(matches!(
            &menu.nodes[2].widget,
            api::Widget::Dropdown(dropdown)
                if dropdown.selected_id == "warm" && dropdown.options.len() == 2
        ));
        assert!(matches!(
            &menu.nodes[3].widget,
            api::Widget::Slider(slider)
                if slider.value == 1.0
                    && slider.minimum == 0.5
                    && slider.maximum == 2.0
                    && slider.step == Some(0.25)
        ));
    }

    #[test]
    fn malformed_dropdowns_and_sliders_are_rejected() {
        let config_frame = |widget| model::UiFrame {
            surfaces: Vec::new(),
            config_menus: vec![model::ConfigMenu {
                id: "settings".to_owned(),
                title: "Fixture".to_owned(),
                nodes: vec![model::UiNode {
                    id: "control".to_owned(),
                    parent: None,
                    widget,
                }],
                canvas: Vec::new(),
            }],
        };

        let missing_selection = model::Widget::Dropdown(model::DropdownWidget {
            label: "Theme".to_owned(),
            selected_id: "missing".to_owned(),
            options: vec![model::DropdownOption {
                id: "warm".to_owned(),
                label: "Warm".to_owned(),
            }],
            enabled: true,
        });
        assert!(
            validate_wit_frame(config_frame(missing_selection), "fixture")
                .expect_err("selected dropdown ID must exist")
                .contains("is not declared")
        );

        let inverted_range = model::Widget::Slider(model::SliderWidget {
            label: "Scale".to_owned(),
            value: 1.0,
            minimum: 2.0,
            maximum: 1.0,
            step: None,
            enabled: true,
        });
        assert!(validate_wit_frame(config_frame(inverted_range), "fixture")
            .expect_err("slider range must increase")
            .contains("minimum must be less"));

        let invalid_step = model::Widget::Slider(model::SliderWidget {
            label: "Scale".to_owned(),
            value: 1.0,
            minimum: 0.0,
            maximum: 2.0,
            step: Some(0.0),
            enabled: true,
        });
        assert!(validate_wit_frame(config_frame(invalid_step), "fixture")
            .expect_err("slider step must be positive")
            .contains("step must be positive"));
    }

    #[test]
    fn checkbox_widget_is_rejected_outside_config_menus() {
        let frame = model::UiFrame {
            surfaces: vec![model::UiSurface {
                id: "main".to_owned(),
                title: "Fixture".to_owned(),
                anchor: model::SurfaceAnchor::TopLeft,
                margin_x: 0.0,
                margin_y: 0.0,
                width: None,
                style: None,
                nodes: vec![model::UiNode {
                    id: "enabled".to_owned(),
                    parent: None,
                    widget: model::Widget::Checkbox(model::CheckboxWidget {
                        label: "Enabled".to_owned(),
                        checked: true,
                        enabled: true,
                    }),
                }],
                canvas: Vec::new(),
            }],
            config_menus: Vec::new(),
        };

        let error = validate_wit_frame(frame, "org.farever.fixture")
            .expect_err("surface checkboxes must be rejected");
        assert!(error.contains("only valid in a config menu"));
    }

    #[test]
    fn config_menu_ids_are_unique_per_addon_frame() {
        let menu = model::ConfigMenu {
            id: "settings".to_owned(),
            title: "Fixture".to_owned(),
            nodes: Vec::new(),
            canvas: Vec::new(),
        };
        let frame = model::UiFrame {
            surfaces: Vec::new(),
            config_menus: vec![menu.clone(), menu],
        };

        let error = validate_wit_frame(frame, "org.farever.fixture")
            .expect_err("duplicate config menu IDs must be rejected");
        assert!(error.contains("duplicate config menu id"));
    }

    #[test]
    fn centered_ui_surface_preserves_signed_offsets() {
        let frame = model::UiFrame {
            surfaces: vec![model::UiSurface {
                id: "wayfinder".to_owned(),
                title: "Wayfinder".to_owned(),
                anchor: model::SurfaceAnchor::Center,
                margin_x: 150.0,
                margin_y: -170.0,
                width: Some(176.0),
                style: None,
                nodes: Vec::new(),
                canvas: Vec::new(),
            }],
            config_menus: Vec::new(),
        };

        let validated = validate_wit_frame(frame, "test-addon").expect("valid UI frame");
        let surface = &validated.surfaces[0];
        assert_eq!(surface.anchor, api::SurfaceAnchor::Center);
        assert_eq!(surface.margin_x, 150.0);
        assert_eq!(surface.margin_y, -170.0);
    }

    #[test]
    fn top_center_ui_surface_preserves_horizontal_offset_and_clamps_top_inset() {
        let frame = model::UiFrame {
            surfaces: vec![model::UiSurface {
                id: "wayfinder".to_owned(),
                title: "Wayfinder".to_owned(),
                anchor: model::SurfaceAnchor::TopCenter,
                margin_x: -24.0,
                margin_y: -12.0,
                width: Some(176.0),
                style: None,
                nodes: Vec::new(),
                canvas: Vec::new(),
            }],
            config_menus: Vec::new(),
        };

        let validated = validate_wit_frame(frame, "test-addon").expect("valid UI frame");
        let surface = &validated.surfaces[0];
        assert_eq!(surface.anchor, api::SurfaceAnchor::TopCenter);
        assert_eq!(surface.margin_x, -24.0);
        assert_eq!(surface.margin_y, 0.0);
    }

    #[test]
    fn ui_frame_rejects_forward_parent_references() {
        let frame = model::UiFrame {
            surfaces: vec![model::UiSurface {
                id: "main".to_owned(),
                title: "Test".to_owned(),
                anchor: model::SurfaceAnchor::TopLeft,
                margin_x: 0.0,
                margin_y: 0.0,
                width: None,
                style: None,
                nodes: vec![
                    model::UiNode {
                        id: "child".to_owned(),
                        parent: Some("later".to_owned()),
                        widget: model::Widget::Separator,
                    },
                    model::UiNode {
                        id: "later".to_owned(),
                        parent: None,
                        widget: model::Widget::Container(model::ContainerWidget {
                            direction: model::LayoutDirection::Vertical,
                            style: model::ContainerStyle::Plain,
                            spacing: None,
                            max_height: None,
                        }),
                    },
                ],
                canvas: Vec::new(),
            }],
            config_menus: Vec::new(),
        };

        let error = validate_wit_frame(frame, "test-addon").expect_err("must reject frame");
        assert!(error.contains("missing or later parent"));
    }

    #[test]
    fn ui_frame_rejects_non_finite_values() {
        let frame = model::UiFrame {
            surfaces: vec![model::UiSurface {
                id: "main".to_owned(),
                title: "Test".to_owned(),
                anchor: model::SurfaceAnchor::TopLeft,
                margin_x: f32::NAN,
                margin_y: 0.0,
                width: None,
                style: None,
                nodes: Vec::new(),
                canvas: Vec::new(),
            }],
            config_menus: Vec::new(),
        };

        let error = validate_wit_frame(frame, "test-addon").expect_err("must reject frame");
        assert!(error.contains("must be finite"));
    }

    #[test]
    fn ui_frame_validates_responsive_table_structure() {
        let frame = responsive_table_frame(false);

        let validated = validate_wit_frame(frame, "test-addon").expect("valid table frame");
        let api::Widget::Table(table) = &validated.surfaces[0].nodes[0].widget else {
            panic!("root node should be a table");
        };
        assert_eq!(table.columns.len(), 2);
        assert_eq!(table.columns[0].content_padding, 6.0);
        assert_eq!(
            table.columns[1].visible_from_width,
            Some(320.0),
            "host retains the add-on's responsive breakpoint"
        );
        let style = validated.surfaces[0]
            .style
            .expect("fixture has custom surface chrome");
        assert!(!style.title_bar);
        assert_eq!(style.padding, 3.0);
        let api::Widget::TableRow(body) = validated.surfaces[0].nodes[4].widget else {
            panic!("fixture node should be a table row");
        };
        assert_eq!(body.progress.map(|progress| progress.fraction), Some(0.75));
        assert_eq!(
            body.progress.and_then(|progress| progress.start_column),
            Some(0)
        );
        assert_eq!(validated.surfaces[0].nodes[2].parent, Some(1));
    }

    #[test]
    fn ui_frame_rejects_duplicate_cells_in_one_table_row() {
        let frame = responsive_table_frame(true);

        let error = validate_wit_frame(frame, "test-addon").expect_err("must reject frame");
        assert!(error.contains("contains column 0 more than once"));
    }

    #[test]
    fn ui_frame_rejects_a_table_cell_outside_the_declared_columns() {
        let mut frame = responsive_table_frame(false);
        let model::Widget::TableCell(cell) = &mut frame.surfaces[0].nodes[7].widget else {
            panic!("fixture node should be a table cell");
        };
        cell.column = 2;

        let error = validate_wit_frame(frame, "test-addon").expect_err("must reject frame");
        assert!(error.contains("refers to column 2"));
        assert!(error.contains("has 2 columns"));
    }

    #[test]
    fn ui_frame_rejects_a_progress_start_outside_the_declared_columns() {
        let mut frame = responsive_table_frame(false);
        let model::Widget::TableRow(row) = &mut frame.surfaces[0].nodes[4].widget else {
            panic!("fixture node should be a table row");
        };
        row.progress
            .as_mut()
            .expect("fixture progress")
            .start_column = Some(2);

        let error = validate_wit_frame(frame, "test-addon").expect_err("must reject frame");
        assert!(error.contains("progress starts at column 2"));
        assert!(error.contains("has 2 columns"));
    }

    #[test]
    fn ui_frame_requires_an_always_visible_progress_start_column() {
        let mut frame = responsive_table_frame(false);
        let model::Widget::TableRow(row) = &mut frame.surfaces[0].nodes[4].widget else {
            panic!("fixture node should be a table row");
        };
        row.progress
            .as_mut()
            .expect("fixture progress")
            .start_column = Some(1);

        let error = validate_wit_frame(frame, "test-addon").expect_err("must reject frame");
        assert!(error.contains("progress start column 1 must always be visible"));
    }

    #[test]
    fn ui_frame_requires_the_table_header_before_body_rows() {
        let mut frame = responsive_table_frame(false);
        let model::Widget::TableRow(first_row) = &mut frame.surfaces[0].nodes[1].widget else {
            panic!("fixture node should be a table row");
        };
        first_row.kind = model::TableRowKind::Body;
        let model::Widget::TableRow(second_row) = &mut frame.surfaces[0].nodes[4].widget else {
            panic!("fixture node should be a table row");
        };
        second_row.kind = model::TableRowKind::Header;

        let error = validate_wit_frame(frame, "test-addon").expect_err("must reject frame");
        assert!(error.contains("header row must precede body rows"));
    }

    #[test]
    fn ui_frame_requires_an_always_visible_table_column() {
        let mut frame = responsive_table_frame(false);
        let model::Widget::Table(table) = &mut frame.surfaces[0].nodes[0].widget else {
            panic!("fixture root should be a table");
        };
        table.columns[0].visible_from_width = Some(200.0);

        let error = validate_wit_frame(frame, "test-addon").expect_err("must reject frame");
        assert!(error.contains("at least one always-visible column"));
    }

    fn responsive_table_frame(duplicate_first_cell: bool) -> model::UiFrame {
        let mut nodes = vec![
            model::UiNode {
                id: "skills".to_owned(),
                parent: None,
                widget: model::Widget::Table(model::TableWidget {
                    columns: vec![
                        model::TableColumn {
                            sizing: model::TableColumnSizing::Remainder,
                            alignment: model::HorizontalAlignment::Left,
                            visible_from_width: None,
                            content_padding: Some(6.0),
                        },
                        model::TableColumn {
                            sizing: model::TableColumnSizing::Auto,
                            alignment: model::HorizontalAlignment::Right,
                            visible_from_width: Some(320.0),
                            content_padding: None,
                        },
                    ],
                    striped: true,
                    max_body_height: Some(240.0),
                }),
            },
            model::UiNode {
                id: "header".to_owned(),
                parent: Some("skills".to_owned()),
                widget: model::Widget::TableRow(model::TableRowWidget {
                    kind: model::TableRowKind::Header,
                    height: 20.0,
                    background: None,
                    progress: None,
                }),
            },
            model::UiNode {
                id: "header-skill".to_owned(),
                parent: Some("header".to_owned()),
                widget: model::Widget::TableCell(model::TableCellWidget { column: 0 }),
            },
            model::UiNode {
                id: "header-skill-label".to_owned(),
                parent: Some("header-skill".to_owned()),
                widget: model::Widget::Text(model::TextWidget {
                    text: "Skill".to_owned(),
                    style: model::TextStyle::Strong,
                    color: None,
                    outline: None,
                    wrap: false,
                }),
            },
            model::UiNode {
                id: "row-1".to_owned(),
                parent: Some("skills".to_owned()),
                widget: model::Widget::TableRow(model::TableRowWidget {
                    kind: model::TableRowKind::Body,
                    height: 22.0,
                    background: Some(model::Rgba {
                        red: 0.04,
                        green: 0.04,
                        blue: 0.04,
                        alpha: 0.8,
                    }),
                    progress: Some(model::TableRowProgress {
                        fraction: 0.75,
                        color: model::Rgba {
                            red: 0.2,
                            green: 0.5,
                            blue: 0.8,
                            alpha: 0.8,
                        },
                        start_column: Some(0),
                    }),
                }),
            },
            model::UiNode {
                id: "row-1-skill".to_owned(),
                parent: Some("row-1".to_owned()),
                widget: model::Widget::TableCell(model::TableCellWidget { column: 0 }),
            },
            model::UiNode {
                id: "row-1-skill-label".to_owned(),
                parent: Some("row-1-skill".to_owned()),
                widget: model::Widget::Text(model::TextWidget {
                    text: "Base Attack".to_owned(),
                    style: model::TextStyle::Body,
                    color: None,
                    outline: None,
                    wrap: false,
                }),
            },
            model::UiNode {
                id: "row-1-dps".to_owned(),
                parent: Some("row-1".to_owned()),
                widget: model::Widget::TableCell(model::TableCellWidget { column: 1 }),
            },
        ];
        if duplicate_first_cell {
            nodes.push(model::UiNode {
                id: "row-1-skill-duplicate".to_owned(),
                parent: Some("row-1".to_owned()),
                widget: model::Widget::TableCell(model::TableCellWidget { column: 0 }),
            });
        }
        model::UiFrame {
            surfaces: vec![model::UiSurface {
                id: "main".to_owned(),
                title: "Test".to_owned(),
                anchor: model::SurfaceAnchor::TopLeft,
                margin_x: 0.0,
                margin_y: 0.0,
                width: Some(480.0),
                style: Some(model::SurfaceStyle {
                    title_bar: false,
                    fill: model::Rgba {
                        red: 0.02,
                        green: 0.02,
                        blue: 0.02,
                        alpha: 0.9,
                    },
                    stroke: Some(model::Stroke {
                        width: 1.0,
                        color: model::Rgba {
                            red: 0.2,
                            green: 0.2,
                            blue: 0.2,
                            alpha: 1.0,
                        },
                    }),
                    corner_radius: 2.0,
                    padding: 3.0,
                }),
                nodes,
                canvas: Vec::new(),
            }],
            config_menus: Vec::new(),
        }
    }
}
