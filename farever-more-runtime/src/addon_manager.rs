use crate::chat_output::{self, ChatOutput, ChatOutputStyle, MAX_CHAT_CODE_UNITS};
use crate::plugin::{self, AddonInfo, CompiledPlugin, Plugins};
use farever_more_api as api;
use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use wasmtime::Engine;

const MAX_COMPONENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 2048;

/// Host-owned lifecycle state for one discovered add-on artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManagedAddonState {
    Active,
    Compiling,
    Disabled,
}

/// Status exposed by the first-class add-on manager.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedAddonStatus {
    pub path: PathBuf,
    pub info: AddonInfo,
    pub state: ManagedAddonState,
    pub error: Option<String>,
}

struct DisabledAddon {
    info: AddonInfo,
    error: String,
}

enum FileChange {
    Published(PathBuf),
    Removed(PathBuf),
    Rescan,
    Error(String),
}

struct AddonWatcher {
    _watcher: RecommendedWatcher,
    receiver: Receiver<notify::Result<Event>>,
}

impl AddonWatcher {
    fn new(directory: &Path) -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(event);
        })
        .map_err(|error| format!("create add-on directory watcher: {error}"))?;
        watcher
            .watch(directory, RecursiveMode::Recursive)
            .map_err(|error| format!("watch add-on directory {}: {error}", directory.display()))?;
        Ok(Self {
            _watcher: watcher,
            receiver,
        })
    }

    fn drain(&self) -> Vec<FileChange> {
        coalesce_file_events(self.receiver.try_iter())
    }
}

fn coalesce_file_events(events: impl Iterator<Item = notify::Result<Event>>) -> Vec<FileChange> {
    let mut paths = BTreeSet::new();
    let mut errors = Vec::new();
    let mut rescan = false;
    for event in events {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                errors.push(FileChange::Error(error.to_string()));
                continue;
            }
        };
        if event.need_rescan() {
            rescan = true;
            continue;
        }
        let published_artifact_event = matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
        );
        if !published_artifact_event {
            continue;
        }
        paths.extend(
            event
                .paths
                .into_iter()
                .filter(|path| plugin::is_component(path)),
        );
    }
    if rescan {
        errors.push(FileChange::Rescan);
        return errors;
    }
    errors.extend(paths.into_iter().map(|path| {
        if path.is_file() {
            FileChange::Published(path)
        } else {
            FileChange::Removed(path)
        }
    }));
    errors
}

struct CompileRequest {
    generation: u64,
    path: PathBuf,
}

enum CompileOutcome {
    Ready {
        generation: u64,
        hash: String,
        compiled: CompiledPlugin,
    },
    Failed {
        generation: u64,
        path: PathBuf,
        error: String,
    },
}

/// A compiled component held back because a required provider is still on
/// its way. Retried on later dispatches; failed loudly once the provider is
/// definitively absent.
struct WaitingComponent {
    hash: String,
    compiled: CompiledPlugin,
}

/// Whether a compiled consumer may activate now.
enum DependencyReadiness {
    /// Every required provider is active at a compatible version.
    Satisfied,
    /// A required provider is still compiling; hold the consumer.
    Waiting(Vec<String>),
    /// A required provider is definitively absent; fail with this error.
    Failed(String),
}

struct Compiler {
    sender: Option<Sender<CompileRequest>>,
    receiver: Receiver<CompileOutcome>,
    thread: Option<JoinHandle<()>>,
}

impl Compiler {
    fn new(engine: Engine) -> Result<Self, String> {
        let (request_sender, request_receiver) = mpsc::channel::<CompileRequest>();
        let (outcome_sender, outcome_receiver) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("farever-addon-compiler".to_owned())
            .spawn(move || {
                while let Ok(request) = request_receiver.recv() {
                    let outcome = compile_request(&engine, request);
                    if outcome_sender.send(outcome).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| format!("start add-on compiler thread: {error}"))?;
        Ok(Self {
            sender: Some(request_sender),
            receiver: outcome_receiver,
            thread: Some(thread),
        })
    }

    fn submit(&self, request: CompileRequest) -> Result<(), String> {
        self.sender
            .as_ref()
            .ok_or_else(|| "add-on compiler is stopped".to_owned())?
            .send(request)
            .map_err(|_| "add-on compiler thread stopped".to_owned())
    }

    fn drain(&self) -> impl Iterator<Item = CompileOutcome> + '_ {
        self.receiver.try_iter()
    }
}

impl Drop for Compiler {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn compile_request(engine: &Engine, request: CompileRequest) -> CompileOutcome {
    let CompileRequest { generation, path } = request;
    let result = fs::read(&path)
        .map_err(|error| format!("read component {}: {error}", path.display()))
        .and_then(|bytes| {
            if bytes.len() > MAX_COMPONENT_BYTES {
                return Err(format!(
                    "component bytes={} exceeds limit={MAX_COMPONENT_BYTES}",
                    bytes.len()
                ));
            }
            let hash = hex::encode(Sha256::digest(&bytes));
            CompiledPlugin::from_bytes(engine, &path, &bytes).map(|compiled| (hash, compiled))
        });
    match result {
        Ok((hash, compiled)) => CompileOutcome::Ready {
            generation,
            hash,
            compiled,
        },
        Err(error) => CompileOutcome::Failed {
            generation,
            path,
            error,
        },
    }
}

/// Discovers, watches, compiles, activates, reloads, and removes Wasm add-ons.
///
/// Filesystem and compiler threads only enqueue work. All guest lifecycle calls
/// remain serialized on the runtime worker through [`Self::dispatch`].
pub struct AddonManager {
    addon_directory: PathBuf,
    config_directory: PathBuf,
    engine: Option<Engine>,
    plugins: Plugins,
    watcher: Option<AddonWatcher>,
    compiler: Option<Compiler>,
    generations: HashMap<PathBuf, u64>,
    pending_removals: BTreeMap<PathBuf, u64>,
    loaded_hashes: HashMap<PathBuf, String>,
    disabled: HashMap<PathBuf, DisabledAddon>,
    waiting: HashMap<PathBuf, WaitingComponent>,
    diagnostics: Vec<String>,
    next_generation: u64,
    next_instance_id: u64,
    resource_revision: u64,
}

impl AddonManager {
    #[cfg(test)]
    pub fn load(
        addon_directory: &Path,
        config_directory: &Path,
        snapshot: &api::GameSnapshot,
    ) -> Self {
        Self::load_with_instance_id(addon_directory, config_directory, snapshot, 1)
    }

    pub(crate) fn load_with_instance_id(
        addon_directory: &Path,
        config_directory: &Path,
        snapshot: &api::GameSnapshot,
        next_instance_id: u64,
    ) -> Self {
        let mut diagnostics = Vec::new();
        if let Err(error) = fs::create_dir_all(addon_directory) {
            diagnostics.push(format!(
                "create add-on directory {}: {error}",
                addon_directory.display()
            ));
        }
        let mut candidates = plugin::discover_components(addon_directory);
        plugin::order_components_by_dependencies(&mut candidates);
        let (engine, plugins, compiler, unavailable_error) = match plugin::create_engine() {
            Ok(engine) => {
                let compiler = match Compiler::new(engine.clone()) {
                    Ok(compiler) => Some(compiler),
                    Err(error) => {
                        diagnostics.push(error.clone());
                        return Self::unavailable(
                            addon_directory,
                            config_directory,
                            snapshot,
                            candidates,
                            diagnostics,
                            error,
                            next_instance_id,
                        );
                    }
                };
                (Some(engine), Plugins::empty(snapshot), compiler, None)
            }
            Err(error) => {
                let message = format!("Wasm engine creation failed: {error:#}");
                diagnostics.push(message.clone());
                (
                    None,
                    Plugins::unavailable(snapshot, message.clone()),
                    None,
                    Some(message),
                )
            }
        };
        let watcher = match AddonWatcher::new(addon_directory) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                diagnostics.push(error);
                None
            }
        };
        let mut manager = Self {
            addon_directory: addon_directory.to_owned(),
            config_directory: config_directory.to_owned(),
            engine,
            plugins,
            watcher,
            compiler,
            generations: HashMap::new(),
            pending_removals: BTreeMap::new(),
            loaded_hashes: HashMap::new(),
            disabled: HashMap::new(),
            waiting: HashMap::new(),
            diagnostics,
            next_generation: 1,
            next_instance_id,
            resource_revision: 1,
        };
        manager
            .diagnostics
            .extend(manager.plugins.take_diagnostics());
        if let Some(error) = unavailable_error {
            for path in candidates {
                manager.disabled.insert(
                    path.clone(),
                    DisabledAddon {
                        info: fallback_info(&path),
                        error: error.clone(),
                    },
                );
            }
        } else {
            for path in candidates {
                manager.request_compile(path);
            }
        }
        manager
    }

    fn unavailable(
        addon_directory: &Path,
        config_directory: &Path,
        snapshot: &api::GameSnapshot,
        candidates: Vec<PathBuf>,
        mut diagnostics: Vec<String>,
        error: String,
        next_instance_id: u64,
    ) -> Self {
        let watcher = match AddonWatcher::new(addon_directory) {
            Ok(watcher) => Some(watcher),
            Err(watch_error) => {
                diagnostics.push(watch_error);
                None
            }
        };
        Self {
            addon_directory: addon_directory.to_owned(),
            config_directory: config_directory.to_owned(),
            engine: None,
            plugins: Plugins::unavailable(snapshot, error.clone()),
            watcher,
            compiler: None,
            generations: HashMap::new(),
            pending_removals: BTreeMap::new(),
            loaded_hashes: HashMap::new(),
            waiting: HashMap::new(),
            disabled: candidates
                .into_iter()
                .map(|path| {
                    let info = fallback_info(&path);
                    (
                        path,
                        DisabledAddon {
                            info,
                            error: error.clone(),
                        },
                    )
                })
                .collect(),
            diagnostics,
            next_generation: 1,
            next_instance_id,
            resource_revision: 1,
        }
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn addon_infos(&self) -> Vec<AddonInfo> {
        self.plugins.addon_infos()
    }

    pub fn font_assets(&self) -> Vec<api::FontAsset> {
        self.plugins.font_assets()
    }

    pub fn image_assets(&self) -> Vec<api::ImageAsset> {
        self.plugins.image_assets()
    }

    pub fn config_properties(&self) -> Vec<api::ConfigPropertyDescriptor> {
        self.plugins.config_properties()
    }

    pub fn resource_revision(&self) -> u64 {
        self.resource_revision
    }

    pub(crate) fn next_instance_id(&self) -> u64 {
        self.next_instance_id
    }

    pub(crate) fn shutdown_for_menu(&mut self) {
        self.plugins.shutdown_for_menu();
    }

    pub fn statuses(&self) -> Vec<ManagedAddonStatus> {
        let compiling = self
            .generations
            .keys()
            .filter(|path| !self.pending_removals.contains_key(*path))
            .cloned()
            .chain(self.waiting.keys().cloned())
            .collect::<BTreeSet<_>>();
        let mut statuses = self
            .plugins
            .paths()
            .zip(self.plugins.addon_infos())
            .map(|(path, info)| ManagedAddonStatus {
                path: path.to_owned(),
                info,
                state: if compiling.contains(path) {
                    ManagedAddonState::Compiling
                } else {
                    ManagedAddonState::Active
                },
                error: None,
            })
            .chain(
                self.disabled
                    .iter()
                    .map(|(path, disabled)| ManagedAddonStatus {
                        path: path.clone(),
                        info: disabled.info.clone(),
                        state: if compiling.contains(path) {
                            ManagedAddonState::Compiling
                        } else {
                            ManagedAddonState::Disabled
                        },
                        error: Some(disabled.error.clone()),
                    }),
            )
            .chain(
                compiling
                    .iter()
                    .filter(|path| {
                        !self.plugins.contains_path(path) && !self.disabled.contains_key(*path)
                    })
                    .map(|path| ManagedAddonStatus {
                        path: path.clone(),
                        info: self
                            .waiting
                            .get(path.as_path())
                            .map(|waiting| waiting.compiled.info().clone())
                            .unwrap_or_else(|| fallback_info(path)),
                        state: ManagedAddonState::Compiling,
                        error: None,
                    }),
            )
            .collect::<Vec<_>>();
        statuses.sort_by(|left, right| left.path.cmp(&right.path));
        statuses
    }

    pub fn dispatch(
        &mut self,
        snapshot: &api::GameSnapshot,
        events: Option<&api::EventBatch>,
        ui_events: &[api::RoutedUiEvent],
    ) -> api::UiFrame {
        self.observe_filesystem();
        let frame = self.plugins.dispatch(snapshot, events, ui_events);
        let previous_revision = self.resource_revision;
        self.apply_ready_lifecycle_work(snapshot);
        if self.resource_revision == previous_revision {
            frame
        } else {
            self.plugins.current_frame()
        }
    }

    pub fn next_tick_delay(&self) -> Option<Duration> {
        self.plugins.next_tick_delay()
    }

    pub fn take_diagnostics(&mut self) -> Vec<String> {
        self.diagnostics.extend(self.plugins.take_diagnostics());
        std::mem::take(&mut self.diagnostics)
    }

    pub fn broadcast_host_message(
        &mut self,
        topic: String,
        payload: Vec<u8>,
    ) -> Result<u32, String> {
        self.plugins.broadcast_host_message(topic, payload)
    }

    fn observe_filesystem(&mut self) {
        let changes = self
            .watcher
            .as_ref()
            .map(AddonWatcher::drain)
            .unwrap_or_default();
        for change in changes {
            match change {
                FileChange::Published(path) => self.request_compile(path),
                FileChange::Removed(path) => self.request_removal(path),
                FileChange::Rescan => self.reconcile_directory(),
                FileChange::Error(error) => self
                    .diagnostics
                    .push(format!("Wasm directory watch failed: {error}")),
            }
        }
    }

    fn reconcile_directory(&mut self) {
        let present = plugin::discover_components(&self.addon_directory)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let known = self
            .plugins
            .paths()
            .map(Path::to_owned)
            .chain(self.disabled.keys().cloned())
            .collect::<BTreeSet<_>>();
        for path in &present {
            self.request_compile(path.clone());
        }
        for path in known.difference(&present) {
            self.request_removal(path.clone());
        }
    }

    fn allocate_generation(&mut self, path: &Path) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        self.generations.insert(path.to_owned(), generation);
        generation
    }

    fn request_compile(&mut self, path: PathBuf) {
        self.pending_removals.remove(&path);
        let generation = self.allocate_generation(&path);
        let Some(compiler) = &self.compiler else {
            self.generations.remove(&path);
            self.diagnostics.push(format!(
                "Wasm component compile skipped path={} reason=compiler-unavailable",
                path.display()
            ));
            return;
        };
        if let Err(error) = compiler.submit(CompileRequest {
            generation,
            path: path.clone(),
        }) {
            self.generations.remove(&path);
            self.diagnostics.push(format!(
                "Wasm component compile rejected path={} error={error}",
                path.display()
            ));
        }
    }

    fn request_removal(&mut self, path: PathBuf) {
        let generation = self.allocate_generation(&path);
        self.pending_removals.insert(path, generation);
    }

    fn apply_ready_lifecycle_work(&mut self, snapshot: &api::GameSnapshot) {
        let outcomes = self
            .compiler
            .as_ref()
            .map(|compiler| compiler.drain().collect::<Vec<_>>())
            .unwrap_or_default();
        // Providers activate before their consumers within one dispatch so a
        // batch that completes together loads in dependency order.
        let mut ready = Vec::new();
        let mut failed = Vec::new();
        for outcome in outcomes {
            match outcome {
                CompileOutcome::Ready { .. } => ready.push(outcome),
                CompileOutcome::Failed { .. } => failed.push(outcome),
            }
        }
        let mut ready_paths: Vec<PathBuf> = ready
            .iter()
            .map(|outcome| match outcome {
                CompileOutcome::Ready { compiled, .. } => compiled.path().to_owned(),
                CompileOutcome::Failed { .. } => unreachable!("ready outcomes only"),
            })
            .collect();
        plugin::order_components_by_dependencies(&mut ready_paths);
        for path in ready_paths {
            let Some(index) = ready.iter().position(|outcome| match outcome {
                CompileOutcome::Ready { compiled, .. } => compiled.path() == path,
                CompileOutcome::Failed { .. } => false,
            }) else {
                continue;
            };
            let outcome = ready.remove(index);
            self.apply_compile_outcome(snapshot, outcome);
        }
        for outcome in failed {
            self.apply_compile_outcome(snapshot, outcome);
        }
        self.retry_waiting_components(snapshot);
        let removals = std::mem::take(&mut self.pending_removals);
        for (path, generation) in removals {
            if self.generations.get(&path) != Some(&generation) {
                continue;
            }
            self.generations.remove(&path);
            let removed_active = self.plugins.teardown_for_removal(&path);
            let removed_disabled = self.disabled.remove(&path).is_some();
            let removed_waiting = self.waiting.remove(&path).is_some();
            self.loaded_hashes.remove(&path);
            if removed_active || removed_disabled || removed_waiting {
                self.resource_revision = self.resource_revision.saturating_add(1);
                self.diagnostics
                    .push(format!("Wasm component removed path={}", path.display()));
            }
        }
    }

    fn apply_compile_outcome(&mut self, snapshot: &api::GameSnapshot, outcome: CompileOutcome) {
        match outcome {
            CompileOutcome::Ready {
                generation,
                hash,
                compiled,
            } => {
                let path = compiled.path().to_owned();
                if self.generations.get(&path) != Some(&generation) {
                    return;
                }
                self.generations.remove(&path);
                if self.loaded_hashes.get(&path) == Some(&hash) {
                    self.diagnostics.push(format!(
                        "Wasm component unchanged path={} hash={hash}",
                        path.display()
                    ));
                    return;
                }
                // A newer artifact supersedes any held-back one.
                self.waiting.remove(&path);
                match self.dependency_readiness(compiled.manifest()) {
                    DependencyReadiness::Satisfied => {
                        self.activate_ready(snapshot, path, hash, compiled);
                    }
                    DependencyReadiness::Waiting(providers) => {
                        self.waiting
                            .insert(path.clone(), WaitingComponent { hash, compiled });
                        self.diagnostics.push(format!(
                            "Wasm component deferred path={} waiting-for={}",
                            path.display(),
                            providers.join(",")
                        ));
                    }
                    DependencyReadiness::Failed(error) => {
                        let info = compiled.info().clone();
                        self.disable(path, info, error);
                    }
                }
            }
            CompileOutcome::Failed {
                generation,
                path,
                error,
            } => {
                if self.generations.get(&path) != Some(&generation) {
                    return;
                }
                self.generations.remove(&path);
                let error = bounded_error(error);
                if self.plugins.contains_path(&path) {
                    self.diagnostics.push(format!(
                        "Wasm component reload compile failed path={} active_generation_retained=true error={error}",
                        path.display()
                    ));
                } else {
                    let info = fallback_info(&path);
                    self.disable(path, info, error);
                }
            }
        }
    }

    /// Shared activation body for fresh compile outcomes and retried
    /// held-back components.
    fn activate_ready(
        &mut self,
        snapshot: &api::GameSnapshot,
        path: PathBuf,
        hash: String,
        compiled: CompiledPlugin,
    ) {
        let info = compiled.info().clone();
        if self.plugins.contains_path(&path) {
            self.plugins.teardown_for_reload(&path);
        }
        self.disabled.remove(&path);
        let instance_id = self.next_instance_id;
        self.next_instance_id = self.next_instance_id.saturating_add(1);
        let Some(engine) = &self.engine else {
            self.disable(path, info, "Wasm engine is unavailable".to_owned());
            return;
        };
        match self.plugins.activate_compiled(
            engine,
            &self.config_directory,
            snapshot,
            compiled,
            instance_id,
        ) {
            Ok(()) => {
                self.loaded_hashes.insert(path.clone(), hash.clone());
                self.resource_revision = self.resource_revision.saturating_add(1);
                self.diagnostics.push(format!(
                    "Wasm component activated path={} instance_id={instance_id} hash={hash}",
                    path.display()
                ));
            }
            Err(error) => self.disable(path, info, error),
        }
    }

    /// Decides whether a compiled consumer may activate now. Required
    /// providers that are still compiling hold the consumer; required
    /// providers that are definitively absent fail it loudly. Optional
    /// dependencies never block.
    fn dependency_readiness(&self, manifest: &plugin::AddonManifest) -> DependencyReadiness {
        let mut waiting = Vec::new();
        for dependency in manifest
            .dependencies
            .iter()
            .filter(|candidate| !candidate.optional)
        {
            let satisfied = if dependency.services.is_empty() {
                self.plugins.provider_present(&dependency.addon)
            } else {
                dependency.services.iter().all(|service| {
                    self.plugins.provider_available(
                        &dependency.addon,
                        &service.id,
                        &service.version,
                    )
                })
            };
            if satisfied {
                continue;
            }
            if self.provider_expected(&dependency.addon) {
                waiting.push(dependency.addon.clone());
            } else {
                let error = self
                    .plugins
                    .check_required(manifest)
                    .err()
                    .unwrap_or_else(|| {
                        format!("missing required dependency {:?}", dependency.addon)
                    });
                return DependencyReadiness::Failed(error);
            }
        }
        if waiting.is_empty() {
            DependencyReadiness::Satisfied
        } else {
            DependencyReadiness::Waiting(waiting)
        }
    }

    /// A provider is expected when one of its component files still has a
    /// compile in flight. Anything else means it will not arrive on its own.
    fn provider_expected(&self, addon_id: &str) -> bool {
        self.generations.keys().any(|path| {
            plugin::read_addon_manifest(path)
                .map(|manifest| manifest.id == addon_id)
                .unwrap_or(false)
        })
    }

    /// Retries held-back components in dependency order. A recompile in
    /// flight supersedes the held-back artifact and is left alone.
    fn retry_waiting_components(&mut self, snapshot: &api::GameSnapshot) {
        if self.waiting.is_empty() {
            return;
        }
        let mut paths: Vec<PathBuf> = self.waiting.keys().cloned().collect();
        plugin::order_components_by_dependencies(&mut paths);
        for path in paths {
            if self.generations.contains_key(&path) {
                continue;
            }
            let readiness = match self.waiting.get(&path) {
                Some(waiting) => self.dependency_readiness(waiting.compiled.manifest()),
                None => continue,
            };
            match readiness {
                DependencyReadiness::Satisfied => {
                    let Some(waiting) = self.waiting.remove(&path) else {
                        continue;
                    };
                    self.activate_ready(snapshot, path, waiting.hash, waiting.compiled);
                }
                DependencyReadiness::Waiting(_) => {}
                DependencyReadiness::Failed(error) => {
                    let Some(waiting) = self.waiting.remove(&path) else {
                        continue;
                    };
                    let info = waiting.compiled.info().clone();
                    self.disable(path, info, error);
                }
            }
        }
    }

    fn disable(&mut self, path: PathBuf, info: AddonInfo, error: String) {
        let error = bounded_error(error);
        self.loaded_hashes.remove(&path);
        self.disabled.insert(
            path.clone(),
            DisabledAddon {
                info: info.clone(),
                error: error.clone(),
            },
        );
        self.resource_revision = self.resource_revision.saturating_add(1);
        self.diagnostics.push(format!(
            "Wasm component disabled path={} reason=activation-failed error={error}",
            path.display()
        ));
        // A silently missing add-on is indistinguishable from a broken
        // install, so every load failure also pages the player locally.
        if let Err(queue_error) = chat_output::enqueue(load_failure_chat_output(&info.name, &error))
        {
            self.diagnostics.push(format!(
                "Wasm load-failure chat output dropped path={} error={queue_error}",
                path.display()
            ));
        }
    }
}

fn fallback_info(path: &Path) -> AddonInfo {
    AddonInfo {
        name: farever_more_manifest::unit_name(path)
            .filter(|name| !name.is_empty())
            .unwrap_or("addon")
            .to_owned(),
        version: None,
    }
}

fn bounded_error(mut error: String) -> String {
    if error.len() <= MAX_ERROR_BYTES {
        return error;
    }
    let mut end = MAX_ERROR_BYTES;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error.truncate(end);
    error
}

/// Player-local error surfaced in chat when an add-on fails to load.
/// Truncated to the chat queue's UTF-16 budget at a character boundary so the
/// message is always enqueueable.
fn load_failure_chat_output(addon_name: &str, error: &str) -> ChatOutput {
    let mut text = format!("Add-on \"{addon_name}\" failed to load: {error}");
    let mut code_units = text.encode_utf16().count();
    while code_units > MAX_CHAT_CODE_UNITS {
        text.pop();
        code_units = text.encode_utf16().count();
    }
    ChatOutput {
        style: ChatOutputStyle::Error,
        text,
        sender_name: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, RenameMode};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    static NEXT_TEMP_ROOT: AtomicU64 = AtomicU64::new(1);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let serial = NEXT_TEMP_ROOT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "farever-more-manager-test-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temporary add-on manager root");
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn event(kind: EventKind, path: &str) -> notify::Result<Event> {
        Ok(Event::new(kind).add_path(PathBuf::from(path)))
    }

    #[test]
    fn watcher_accepts_publication_events_and_ignores_in_place_writes() {
        let published = PathBuf::from("new.wasm");
        let events = vec![
            event(
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                "ignored.wasm",
            ),
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                "new.wasm",
            ),
            event(EventKind::Create(CreateKind::File), "note.txt"),
        ];
        let changes = coalesce_file_events(events.into_iter());

        assert_eq!(changes.len(), 1);
        assert!(matches!(
            &changes[0],
            FileChange::Removed(path) if path == &published
        ));
    }

    #[test]
    fn errors_are_bounded_on_utf8_boundaries() {
        let error = "🙂".repeat(MAX_ERROR_BYTES);
        let bounded = bounded_error(error);

        assert!(bounded.len() <= MAX_ERROR_BYTES);
        assert!(bounded.is_char_boundary(bounded.len()));
    }

    #[test]
    fn atomic_publication_disables_an_invalid_new_component_and_removal_forgets_it() {
        let root = TempRoot::new();
        let addons = root.0.join("addons");
        let config = root.0.join("config");
        fs::create_dir_all(addons.join("broken")).expect("create add-on directory");
        let snapshot = api::GameSnapshot::default();
        let mut manager = AddonManager::load(&addons, &config, &snapshot);
        let temporary = addons.join("broken/addon.wasm.tmp");
        let published = addons.join("broken/addon.wasm");
        fs::write(&temporary, b"not a WebAssembly component").expect("write staged component");
        fs::rename(&temporary, &published).expect("atomically publish component");

        wait_until(Duration::from_secs(5), || {
            let _ = manager.dispatch(&snapshot, None, &[]);
            manager.statuses().iter().any(|status| {
                status.path == published
                    && status.state == ManagedAddonState::Disabled
                    && status.error.is_some()
            })
        });

        fs::remove_file(&published).expect("remove published component");
        wait_until(Duration::from_secs(5), || {
            let _ = manager.dispatch(&snapshot, None, &[]);
            manager.statuses().is_empty()
        });
    }

    #[test]
    #[ignore = "set FAREVER_ADDON_SMOKE_DIR to a built add-ons directory"]
    fn built_component_replacement_recreates_one_active_instance() {
        let built = std::env::var_os("FAREVER_ADDON_SMOKE_DIR")
            .map(PathBuf::from)
            .expect("FAREVER_ADDON_SMOKE_DIR");
        let damage = built.join("dyno/addon.wasm");
        let wayfinder = built.join("gps/addon.wasm");
        let root = TempRoot::new();
        let addons = root.0.join("addons");
        let config = root.0.join("config");
        fs::create_dir_all(addons.join("smoke")).expect("create add-on directory");
        let published = addons.join("smoke/addon.wasm");
        fs::copy(&damage, &published).expect("install initial component");
        let snapshot = api::GameSnapshot::default();
        let mut manager = AddonManager::load(&addons, &config, &snapshot);
        wait_until(Duration::from_secs(15), || {
            let _ = manager.dispatch(&snapshot, None, &[]);
            manager.len() == 1
                && manager
                    .statuses()
                    .iter()
                    .all(|status| status.state == ManagedAddonState::Active)
        });
        let initial_revision = manager.resource_revision();

        let temporary = addons.join("smoke/addon.wasm.tmp");
        fs::copy(&wayfinder, &temporary).expect("stage replacement component");
        fs::remove_file(&published).expect("remove previous artifact before test rename");
        fs::rename(&temporary, &published).expect("publish replacement component");

        wait_until(Duration::from_secs(15), || {
            let _ = manager.dispatch(&snapshot, None, &[]);
            manager.resource_revision() > initial_revision
                && manager.len() == 1
                && manager.plugins.tick_interval_for_path(&published) == Some(50)
        });
        assert_eq!(manager.plugins.tick_interval_for_path(&published), Some(50));
        assert!(manager
            .statuses()
            .iter()
            .all(|status| status.state == ManagedAddonState::Active));

        let temporary = addons.join("smoke/addon.wasm.tmp");
        fs::write(&temporary, b"\0asm\x0d\0\x01\0")
            .expect("stage component without the add-on world");
        fs::remove_file(&published).expect("remove active artifact before failing rename");
        fs::rename(&temporary, &published).expect("publish component that cannot activate");

        wait_until(Duration::from_secs(15), || {
            let _ = manager.dispatch(&snapshot, None, &[]);
            manager.len() == 0
                && manager.statuses().iter().any(|status| {
                    status.path == published
                        && status.state == ManagedAddonState::Disabled
                        && status
                            .error
                            .as_deref()
                            .is_some_and(|error| error.contains("instantiate component"))
                })
        });
    }

    #[test]
    fn load_failure_chat_output_is_bounded_player_error() {
        let output = load_failure_chat_output("minimap", "open typed POI service");
        assert_eq!(output.style, ChatOutputStyle::Error);
        assert!(output.text.contains("minimap"));
        assert!(output.text.encode_utf16().count() <= MAX_CHAT_CODE_UNITS);

        let output = load_failure_chat_output("addon", &"e".repeat(2048));
        assert_eq!(output.style, ChatOutputStyle::Error);
        assert!(output.text.encode_utf16().count() <= MAX_CHAT_CODE_UNITS);
        // Truncation pops whole characters, so multi-byte text stays valid.
        let output = load_failure_chat_output("addon", &"😀".repeat(300));
        assert!(output.text.encode_utf16().count() <= MAX_CHAT_CODE_UNITS);
        assert!(output.text.chars().all(|character| character != '\u{FFFD}'));
    }

    #[test]
    fn disable_pages_the_player_with_a_chat_error() {
        let marker = format!("chat-failure-probe-{}", std::process::id());
        let root = TempRoot::new();
        let snapshot = api::GameSnapshot::default();
        let mut manager =
            AddonManager::load(&root.0.join("addons"), &root.0.join("config"), &snapshot);
        manager.disable(
            PathBuf::from("probe/addon.wasm"),
            AddonInfo {
                name: marker.clone(),
                version: None,
            },
            "open typed POI service: Unavailable".to_owned(),
        );

        let queued = chat_output::drain_queued_for_tests();
        let reported = queued.iter().find(|output| output.text.contains(&marker));
        let Some(reported) = reported else {
            panic!("missing player chat error for {marker}");
        };
        assert_eq!(reported.style, ChatOutputStyle::Error);
    }

    #[test]
    fn provider_expected_tracks_pending_compiles_by_manifest_id() {
        let root = TempRoot::new();
        let snapshot = api::GameSnapshot::default();
        let mut manager =
            AddonManager::load(&root.0.join("addons"), &root.0.join("config"), &snapshot);
        assert!(!manager.provider_expected("poi-database"));

        let dir = root.0.join("addons").join("poi-database");
        fs::create_dir_all(&dir).expect("create provider directory");
        let wasm = dir.join("addon.wasm");
        fs::write(&wasm, b"placeholder").expect("stage provider path");
        fs::write(
            dir.join("addon.json"),
            r#"{"manifest-version": 1, "id": "poi-database"}"#,
        )
        .expect("stage provider manifest");
        manager.generations.insert(wasm, 7);
        assert!(manager.provider_expected("poi-database"));
        assert!(!manager.provider_expected("minimap"));
    }

    #[test]
    #[ignore = "set FAREVER_ADDON_SMOKE_DIR to a built add-ons directory"]
    fn dependency_graph_holds_consumer_until_provider_activates() {
        let built = std::env::var_os("FAREVER_ADDON_SMOKE_DIR")
            .map(PathBuf::from)
            .expect("FAREVER_ADDON_SMOKE_DIR");
        let root = TempRoot::new();
        let snapshot = api::GameSnapshot::default();
        let mut manager =
            AddonManager::load(&root.0.join("addons"), &root.0.join("config"), &snapshot);

        // Stage the real components outside the watched directory and drive
        // the outcomes by hand so the test is deterministic.
        let staging = root.0.join("staging");
        let consumer_dir = staging.join("minimap-only");
        fs::create_dir_all(&consumer_dir).expect("create consumer directory");
        let consumer = consumer_dir.join("addon.wasm");
        fs::copy(built.join("minimap/addon.wasm"), &consumer).expect("stage consumer component");
        fs::copy(
            built.join("minimap/addon.json"),
            consumer_dir.join("addon.json"),
        )
        .expect("stage consumer manifest");
        let provider_dir = staging.join("poi-database");
        fs::create_dir_all(&provider_dir).expect("create provider directory");
        let provider = provider_dir.join("addon.wasm");
        fs::copy(built.join("poi-database/addon.wasm"), &provider)
            .expect("stage provider component");
        fs::copy(
            built.join("poi-database/addon.json"),
            provider_dir.join("addon.json"),
        )
        .expect("stage provider manifest");

        let engine = manager.engine.as_ref().expect("test engine").clone();
        let compile = |path: &Path| {
            let bytes = fs::read(path).expect("read staged component");
            plugin::CompiledPlugin::from_bytes(&engine, path, &bytes)
                .expect("compile staged component")
        };
        // The provider compile is still in flight when the consumer outcome
        // arrives.
        manager.generations.insert(provider.clone(), 1);
        manager.generations.insert(consumer.clone(), 2);
        manager.apply_compile_outcome(
            &snapshot,
            CompileOutcome::Ready {
                generation: 2,
                hash: "consumer-hash".to_owned(),
                compiled: compile(&consumer),
            },
        );

        assert!(manager.waiting.contains_key(&consumer));
        assert!(manager.statuses().iter().any(|status| {
            status.path == consumer && status.state == ManagedAddonState::Compiling
        }));

        // The provider lands: the held-back consumer activates right after.
        let compiled = compile(&provider);
        manager.apply_compile_outcome(
            &snapshot,
            CompileOutcome::Ready {
                generation: 1,
                hash: "provider-hash".to_owned(),
                compiled,
            },
        );
        let _ = manager.dispatch(&snapshot, None, &[]);
        assert_eq!(manager.len(), 2);
        assert!(manager
            .statuses()
            .iter()
            .all(|status| status.state == ManagedAddonState::Active));
    }

    fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            predicate(),
            "condition did not become true within {timeout:?}"
        );
    }
}
