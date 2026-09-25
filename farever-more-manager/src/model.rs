use crate::backend::{self, DiscoveredAddon};
use crate::config::ManagerConfig;
use std::path::{Path, PathBuf};

/// Synthetic entry for the framework runtime itself. It is implicitly
/// required by every add-on: the runtime ships in lockstep with this manager
/// (same workspace version) and cannot be disabled or removed here.
pub(crate) const RUNTIME_ADDON_ID: &str = "farever.runtime";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LifecycleState {
    Active,
    Compiling,
    Disabled,
    /// On disk and enabled, but built for an add-on API this framework cannot
    /// satisfy. The host refuses it, so the row says so instead of claiming
    /// the add-on is running.
    Incompatible,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActivationError {
    pub(crate) message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AddonEntry {
    pub(crate) id: String,
    pub(crate) initials: String,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) artifact: String,
    pub(crate) detail: Option<String>,
    pub(crate) state: LifecycleState,
    pub(crate) enabled: bool,
    pub(crate) error: Option<ActivationError>,
    pub(crate) expanded: bool,
    /// Move/delete granularity on disk: the containing folder, or the file
    /// itself when it sits directly under `addons/` / `.disabled-addons/`.
    pub(crate) unit_path: PathBuf,
    pub(crate) wasm_path: PathBuf,
    /// Locked entries (the runtime) cannot be disabled or removed.
    pub(crate) locked: bool,
}

impl AddonEntry {
    pub(crate) fn metadata(&self) -> String {
        let mut metadata = format!("v{} · {}", self.version, self.artifact);
        if let Some(detail) = &self.detail {
            metadata.push_str(" · ");
            metadata.push_str(detail);
        }
        metadata
    }
}

impl From<DiscoveredAddon> for AddonEntry {
    fn from(addon: DiscoveredAddon) -> Self {
        let name = addon.name.clone();
        // The declared API version is the same number the host enforces at
        // load time, so an add-on that would be refused in game is refused
        // here too - visibly, before it is ever started.
        let incompatibility = addon.api_version.as_deref().map(|declared| {
            let required = farever_more_manifest::api::parse_api_version(declared, "api-version")?;
            farever_more_manifest::api::compatibility(
                required,
                farever_more_manifest::api::ApiVersion::host(),
            )
        });
        let error = match incompatibility {
            Some(Err(message)) => Some(ActivationError {
                message: format!("{name}: {message}"),
            }),
            _ => None,
        };
        let state = if error.is_some() {
            LifecycleState::Incompatible
        } else if addon.enabled {
            LifecycleState::Active
        } else {
            LifecycleState::Disabled
        };
        Self {
            id: addon.id,
            initials: backend::initials_for(&name),
            name,
            version: addon.version.unwrap_or_else(|| "unknown".to_owned()),
            artifact: addon.artifact,
            detail: None,
            state,
            enabled: addon.enabled,
            error,
            expanded: false,
            unit_path: addon.unit_path,
            wasm_path: addon.wasm_path,
            locked: false,
        }
    }
}

/// Inspects the framework runtime itself. The version is whatever the
/// installer stamped into `runtime.json` — never the manager's own version.
/// Installs predating the sidecar report as installed with unknown version.
pub(crate) struct RuntimeStatus {
    pub(crate) installed: bool,
    pub(crate) version: Option<String>,
    pub(crate) error: Option<ActivationError>,
}

pub(crate) fn runtime_status(addon_root: &Path) -> RuntimeStatus {
    let game_dir = game_dir_for(addon_root);
    let missing = [addon_root.join("host.dll"), game_dir.join("dinput8.dll")]
        .into_iter()
        .filter(|path| !path.is_file())
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return RuntimeStatus {
            installed: false,
            version: None,
            error: Some(ActivationError {
                message: format!("runtime files missing: {}", missing.join(", ")),
            }),
        };
    }
    RuntimeStatus {
        installed: true,
        version: backend::read_runtime_sidecar(addon_root),
        error: None,
    }
}

/// Best-effort game directory for an add-on root. A root named
/// `farever-addons` belongs to its parent; a custom root stands alone.
fn game_dir_for(addon_root: &Path) -> PathBuf {
    if addon_root
        .file_name()
        .is_some_and(|name| name == "farever-addons")
    {
        addon_root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| addon_root.to_owned())
    } else {
        addon_root.to_owned()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AddonSummary {
    pub(crate) active: usize,
    pub(crate) compiling: usize,
    pub(crate) disabled: usize,
    pub(crate) incompatible: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagerSnapshot {
    pub(crate) config: ManagerConfig,
    pub(crate) addon_root: PathBuf,
    pub(crate) addon_directory: PathBuf,
    /// Effective game directory derived from the configured location.
    pub(crate) game_dir: PathBuf,
    /// Installed add-ons only; the runtime lives in the fields below.
    pub(crate) addons: Vec<AddonEntry>,
    /// Whether `dinput8.dll` and `host.dll` are both present.
    pub(crate) runtime_installed: bool,
    /// Stamped runtime version, if the installer recorded one.
    pub(crate) runtime_version: String,
    /// Why the runtime counts as missing, if it does.
    pub(crate) runtime_error: Option<ActivationError>,
    /// Last operation result, shown in the UI footer. Empty when idle.
    pub(crate) status: String,
    /// Persistent hint (e.g. nothing found), shown only while status is empty.
    pub(crate) notice: String,
}

impl ManagerSnapshot {
    /// Loads the live on-disk state, honoring the saved manager config.
    pub(crate) fn load() -> Self {
        let config = ManagerConfig::load();
        Self::load_from(&config.addon_root(), config)
    }

    pub(crate) fn load_from(addon_root: &Path, config: ManagerConfig) -> Self {
        let mut snapshot = Self {
            game_dir: game_dir_for(addon_root),
            addon_root: addon_root.to_owned(),
            addon_directory: backend::addons_dir(addon_root),
            config,
            addons: Vec::new(),
            runtime_installed: false,
            runtime_version: String::new(),
            runtime_error: None,
            status: String::new(),
            notice: String::new(),
        };
        snapshot.refresh();
        snapshot
    }

    /// Footer text: the last operation result wins, otherwise the hint.
    pub(crate) fn status_message(&self) -> String {
        if self.status.is_empty() {
            self.notice.clone()
        } else {
            self.status.clone()
        }
    }

    /// Explains an empty-looking install instead of showing a bare list.
    fn update_notice(&mut self) {
        if self.addons.is_empty() && !self.runtime_installed {
            self.notice = format!(
                "No add-ons found in {}. Set the game location in Settings.",
                self.addon_directory.display()
            );
        } else {
            self.notice.clear();
        }
    }

    /// Where the current game directory came from, for the settings UI.
    pub(crate) fn game_dir_source(&self) -> &'static str {
        self.config.game_dir_source()
    }

    #[cfg(test)]
    pub(crate) fn preview() -> Self {
        Self {
            config: ManagerConfig::default(),
            addon_root: PathBuf::from(r"farever-addons"),
            addon_directory: PathBuf::from(r"farever-addons\addons"),
            game_dir: PathBuf::from(r"farever-addons"),
            runtime_installed: true,
            runtime_version: "0.1.0".to_owned(),
            runtime_error: None,
            addons: vec![
                AddonEntry {
                    id: "dyno".to_owned(),
                    initials: "D".to_owned(),
                    name: "Dyno".to_owned(),
                    version: "0.1.0".to_owned(),
                    artifact: "addon.wasm".to_owned(),
                    detail: None,
                    state: LifecycleState::Active,
                    enabled: true,
                    error: None,
                    expanded: false,
                    unit_path: PathBuf::from(r"farever-addons\addons\dyno"),
                    wasm_path: PathBuf::from(
                        r"farever-addons\addons\dyno\dyno.wasm",
                    ),
                    locked: false,
                },
                AddonEntry {
                    id: "minimap".to_owned(),
                    initials: "M".to_owned(),
                    name: "Minimap".to_owned(),
                    version: "0.1.0".to_owned(),
                    artifact: "addon.wasm".to_owned(),
                    detail: Some("16 ms tick".to_owned()),
                    state: LifecycleState::Active,
                    enabled: true,
                    error: None,
                    expanded: false,
                    unit_path: PathBuf::from(r"farever-addons\addons\minimap"),
                    wasm_path: PathBuf::from(
                        r"farever-addons\addons\minimap\minimap.wasm",
                    ),
                    locked: false,
                },
                AddonEntry {
                    id: "gps".to_owned(),
                    initials: "G".to_owned(),
                    name: "GPS".to_owned(),
                    version: "0.1.0".to_owned(),
                    artifact: "addon.wasm".to_owned(),
                    detail: Some("compiling replacement in background".to_owned()),
                    state: LifecycleState::Compiling,
                    enabled: true,
                    error: None,
                    expanded: false,
                    unit_path: PathBuf::from(r"farever-addons\addons\gps"),
                    wasm_path: PathBuf::from(
                        r"farever-addons\addons\gps\gps.wasm",
                    ),
                    locked: false,
                },
                AddonEntry {
                    id: "encounter-notes".to_owned(),
                    initials: "EN".to_owned(),
                    name: "Encounter Notes".to_owned(),
                    version: "0.4.2".to_owned(),
                    artifact: "addon.wasm".to_owned(),
                    detail: None,
                    state: LifecycleState::Disabled,
                    enabled: false,
                    error: Some(ActivationError {
                        message: "plugin.activate rejected load: configuration schema version 7 is unsupported"
                            .to_owned(),
                    }),
                    expanded: true,
                    unit_path: PathBuf::from(r"farever-addons\.disabled-addons\encounter-notes"),
                    wasm_path: PathBuf::from(
                        r"farever-addons\.disabled-addons\encounter-notes\encounter-notes.wasm",
                    ),
                    locked: false,
                },
            ],
            status: String::new(),
            notice: String::new(),
        }
    }

    /// Content fingerprint for cheap change detection by the refresh timer.
    fn fingerprint(&self) -> Vec<(String, bool, String, String, u8, bool)> {
        let mut fingerprint = vec![(
            RUNTIME_ADDON_ID.to_owned(),
            self.runtime_installed,
            self.runtime_version.clone(),
            String::new(),
            0,
            self.runtime_error.is_some(),
        )];
        fingerprint.extend(self.addons.iter().map(|addon| {
            (
                addon.id.clone(),
                addon.enabled,
                addon.version.clone(),
                addon.artifact.clone(),
                match addon.state {
                    LifecycleState::Active => 0,
                    LifecycleState::Compiling => 1,
                    LifecycleState::Disabled => 2,
                    LifecycleState::Incompatible => 3,
                },
                addon.error.is_some(),
            )
        }));
        fingerprint
    }

    /// Rescans disk, preserving row order, expansion, and errors by id.
    /// Returns true when the visible content changed. Order is stable across
    /// refreshes: surviving rows keep their positions (a toggle must not
    /// relocate the row under the user's cursor), vanished rows drop out,
    /// brand-new ids append at the end.
    pub(crate) fn refresh(&mut self) -> bool {
        let before = self.fingerprint();
        let runtime = runtime_status(&self.addon_root);
        self.runtime_installed = runtime.installed;
        self.runtime_version = runtime.version.unwrap_or_default();
        self.runtime_error = runtime.error;
        let scanned = backend::scan(&self.addon_root);
        let fresh = scanned
            .iter()
            .map(|discovered| (discovered.id.clone(), AddonEntry::from(discovered.clone())))
            .collect::<std::collections::HashMap<_, _>>();
        let mut next = Vec::with_capacity(scanned.len());
        for old in std::mem::take(&mut self.addons) {
            if let Some(new) = fresh.get(&old.id) {
                let mut merged = new.clone();
                // Disk truth wins for data (paths move on enable/disable);
                // UI state survives the rescan.
                merged.expanded = old.expanded;
                merged.error = old.error;
                next.push(merged);
            }
        }
        for discovered in &scanned {
            if !next
                .iter()
                .any(|addon: &AddonEntry| addon.id == discovered.id)
            {
                if let Some(entry) = fresh.get(&discovered.id) {
                    next.push(entry.clone());
                }
            }
        }
        self.addons = next;
        self.update_notice();
        self.fingerprint() != before
    }

    /// Drops a row's error once an operation on it succeeds, so a resolved
    /// failure does not linger through subsequent refreshes.
    fn clear_error(&mut self, id: &str) {
        if let Some(addon) = self.addons.iter_mut().find(|addon| addon.id == id) {
            addon.error = None;
            addon.expanded = false;
        }
    }

    pub(crate) fn summary(&self) -> AddonSummary {
        self.addons
            .iter()
            .fold(AddonSummary::default(), |mut summary, addon| {
                match addon.state {
                    LifecycleState::Active => summary.active += 1,
                    LifecycleState::Compiling => summary.compiling += 1,
                    LifecycleState::Disabled => summary.disabled += 1,
                    LifecycleState::Incompatible => summary.incompatible += 1,
                }
                summary
            })
    }

    fn fail_at(&mut self, index: usize, message: String) {
        self.status = message.clone();
        if let Some(addon) = self.addons.get_mut(index) {
            addon.error = Some(ActivationError { message });
            addon.expanded = true;
        }
    }

    /// Moves the add-on between `addons/` and `.disabled-addons/`, then
    /// rescans. Settings under `config/` are untouched. Locked entries
    /// (the runtime) are refused.
    pub(crate) fn set_enabled(&mut self, index: usize, enabled: bool) {
        let Some(addon) = self.addons.get(index) else {
            return;
        };
        if addon.locked {
            self.status = format!("{} is required by all add-ons", addon.name);
            return;
        }
        let id = addon.id.clone();
        let unit = addon.unit_path.clone();
        match backend::set_enabled(&self.addon_root, &unit, enabled) {
            Ok(_) => {
                self.status = if enabled {
                    format!("{id} enabled")
                } else {
                    format!("{id} disabled")
                };
                self.refresh();
                self.clear_error(&id);
            }
            Err(error) => self.fail_at(index, error),
        }
    }

    pub(crate) fn toggle_enabled(&mut self, index: usize) {
        let enabled = self.addons.get(index).is_some_and(|addon| !addon.enabled);
        self.set_enabled(index, enabled);
    }

    /// Deletes the add-on folder (or file) from disk. Settings in `config/`
    /// are preserved so a reinstall restores them. Locked entries (the
    /// runtime) are refused.
    pub(crate) fn remove(&mut self, index: usize) {
        let Some(addon) = self.addons.get(index) else {
            return;
        };
        if addon.locked {
            self.status = format!("{} is required by all add-ons", addon.name);
            return;
        }
        let id = addon.id.clone();
        let unit = addon.unit_path.clone();
        match backend::remove_unit(&self.addon_root, &unit) {
            Ok(()) => {
                self.status = format!("{id} removed; settings kept");
                self.refresh();
            }
            Err(error) => self.fail_at(index, error),
        }
    }

    /// Installs an add-on `.zip` archive picked through the filesystem
    /// selector (or pasted as a path) into `addons/`.
    pub(crate) fn install_archive(&mut self, input: &str) {
        match backend::install_archive(&self.addon_root, Path::new(input.trim())) {
            Ok(installed) => {
                self.status = format!("installed {} component(s)", installed.len());
                self.refresh();
            }
            Err(error) => {
                self.status = error;
            }
        }
    }

    /// Saves the game install location from the settings dialog and
    /// re-roots the snapshot onto it.
    pub(crate) fn save_game_dir(&mut self, input: &str) {
        let path = PathBuf::from(input.trim());
        if path.as_os_str().is_empty() {
            self.status = "enter a game folder".to_owned();
            return;
        }
        if !path.is_dir() {
            self.status = format!("game folder not found: {}", path.display());
            return;
        }
        self.config.game_dir = Some(path);
        if let Err(error) = self.config.save() {
            self.status = error;
            return;
        }
        self.addon_root = self.config.addon_root();
        self.addon_directory = backend::addons_dir(&self.addon_root);
        self.game_dir = game_dir_for(&self.addon_root);
        self.status = format!("game location saved: {}", self.game_dir.display());
        self.refresh();
    }

    /// Replaces `dinput8.dll` and `farever-addons/host.dll` from local build
    /// artifacts. Refuses while the game runs. When sources are `None`, looks
    /// next to the manager executable.
    pub(crate) fn update_runtime(
        &mut self,
        proxy_source: Option<&Path>,
        host_source: Option<&Path>,
    ) {
        let game_dir = self.game_dir.clone();
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        let proxy_default = exe_dir
            .as_ref()
            .map(|dir| dir.join("farever_more_proxy.dll"));
        let host_default = exe_dir
            .as_ref()
            .map(|dir| dir.join("farever_more_host.dll"));
        let proxy = proxy_source
            .map(Path::to_path_buf)
            .or(proxy_default)
            .unwrap_or_default();
        let host = host_source
            .map(Path::to_path_buf)
            .or(host_default)
            .unwrap_or_default();
        match backend::update_runtime(&game_dir, &proxy, &host) {
            Ok(()) => self.status = "runtime updated".to_owned(),
            Err(error) => self.status = error,
        }
    }

    pub(crate) fn toggle_expanded(&mut self, index: usize) {
        let Some(addon) = self.addons.get_mut(index) else {
            return;
        };
        if addon.error.is_some() {
            addon.expanded = !addon.expanded;
        }
    }

    pub(crate) fn expand_error(&mut self, index: usize) {
        let Some(addon) = self.addons.get_mut(index) else {
            return;
        };
        if addon.error.is_some() {
            addon.expanded = true;
        }
    }

    pub(crate) fn retry_activation(&mut self, index: usize) {
        // Without a live runtime bridge the manager cannot recompile; retry
        // re-enables a disabled add-on (moving it back to `addons/`) and
        // surfaces backend errors inline.
        let enabled = self.addons.get(index).is_some_and(|addon| !addon.enabled);
        if enabled {
            self.set_enabled(index, true);
            return;
        }
        let Some(addon) = self.addons.get_mut(index) else {
            return;
        };
        addon.enabled = true;
        addon.state = LifecycleState::Compiling;
        addon.error = None;
        addon.expanded = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::TempRoot;
    use std::fs;

    fn write_wasm(path: &Path) {
        fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        fs::write(path, b"\0asm\x01\0\0\0").expect("wasm");
    }

    fn load(root: &TempRoot) -> ManagerSnapshot {
        ManagerSnapshot::load_from(&root.addon_root(), ManagerConfig::default())
    }

    fn index_of(snapshot: &ManagerSnapshot, id: &str) -> usize {
        snapshot
            .addons
            .iter()
            .position(|addon| addon.id == id)
            .unwrap_or_else(|| panic!("missing {id}"))
    }

    #[test]
    fn preview_summary_matches_lifecycle_states() {
        let snapshot = ManagerSnapshot::preview();

        assert_eq!(
            snapshot.summary(),
            AddonSummary {
                active: 2,
                compiling: 1,
                disabled: 1,
                incompatible: 0,
            }
        );
    }

    #[test]
    fn load_reports_missing_runtime_outside_the_list() {
        let root = TempRoot::new();
        let snapshot = load(&root);

        assert!(snapshot.addons.is_empty());
        assert!(!snapshot.runtime_installed);
        assert!(snapshot.runtime_version.is_empty());
        assert!(snapshot.runtime_error.is_some());
        assert!(!snapshot
            .addons
            .iter()
            .any(|addon| addon.id == RUNTIME_ADDON_ID));
    }

    #[test]
    fn runtime_version_comes_from_the_sidecar_only() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        fs::create_dir_all(&addon_root).expect("root");
        fs::write(addon_root.join("host.dll"), b"host").expect("host");
        fs::create_dir_all(addon_root.parent().expect("game")).expect("game");
        fs::write(
            addon_root.parent().expect("game").join("dinput8.dll"),
            b"proxy",
        )
        .expect("proxy");
        let mut snapshot = load(&root);

        // Files without a stamped record: installed, version unknown.
        assert!(snapshot.runtime_installed);
        assert!(snapshot.runtime_version.is_empty());
        assert!(snapshot.runtime_error.is_none());

        fs::write(
            addon_root.join("runtime.json"),
            r#"{"formatVersion": 1, "runtimeVersion": "0.2.0"}"#,
        )
        .expect("sidecar");
        assert!(snapshot.refresh());

        assert_eq!(snapshot.runtime_version, "0.2.0");
        assert!(!snapshot
            .addons
            .iter()
            .any(|addon| addon.id == RUNTIME_ADDON_ID));
    }

    #[test]
    fn refresh_discovers_filesystem_changes() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        let mut snapshot = load(&root);
        assert!(snapshot.addons.is_empty());

        write_wasm(&addon_root.join("addons").join("mod-a").join("mod-a.wasm"));

        assert!(snapshot.refresh());
        assert_eq!(snapshot.addons.len(), 1);
        assert_eq!(snapshot.addons[0].id, "mod-a");
        assert!(snapshot.addons[0].enabled);
        assert!(!snapshot.refresh());
    }

    #[test]
    fn toggle_persists_through_disabled_dir() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        write_wasm(&addon_root.join("addons").join("mod-a").join("mod-a.wasm"));
        let mut snapshot = load(&root);

        snapshot.toggle_enabled(index_of(&snapshot, "mod-a"));

        let index = index_of(&snapshot, "mod-a");
        assert!(!snapshot.addons[index].enabled);
        assert_eq!(snapshot.addons[index].state, LifecycleState::Disabled);
        assert!(addon_root.join(".disabled-addons").join("mod-a").is_dir());

        snapshot.toggle_enabled(index_of(&snapshot, "mod-a"));

        let index = index_of(&snapshot, "mod-a");
        assert!(snapshot.addons[index].enabled);
        assert!(addon_root.join("addons").join("mod-a").is_dir());
    }

    #[test]
    fn remove_deletes_folder_and_keeps_status() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        write_wasm(&addon_root.join("addons").join("mod-a").join("mod-a.wasm"));
        let mut snapshot = load(&root);

        snapshot.remove(index_of(&snapshot, "mod-a"));

        assert!(snapshot.addons.is_empty());
        assert!(snapshot.status.contains("removed"));
    }

    #[test]
    fn retry_enables_a_disabled_addon() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        write_wasm(&addon_root.join("addons").join("mod-a").join("mod-a.wasm"));
        let mut snapshot = load(&root);
        snapshot.toggle_enabled(index_of(&snapshot, "mod-a"));
        let index = index_of(&snapshot, "mod-a");
        assert!(!snapshot.addons[index].enabled);

        snapshot.retry_activation(index_of(&snapshot, "mod-a"));

        assert!(snapshot.addons[index_of(&snapshot, "mod-a")].enabled);
    }

    #[test]
    fn empty_install_points_at_settings() {
        let root = TempRoot::new();
        let snapshot = load(&root);

        assert!(snapshot.status_message().contains("Settings"));
    }

    #[test]
    fn notice_clears_once_addons_appear() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        let mut snapshot = load(&root);
        assert!(!snapshot.status_message().is_empty());

        write_wasm(&addon_root.join("addons").join("mod-a").join("mod-a.wasm"));
        assert!(snapshot.refresh());

        assert!(snapshot.status_message().is_empty());
    }

    #[test]
    fn toggle_keeps_row_in_place() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        write_wasm(&addon_root.join("addons").join("mod-a").join("mod-a.wasm"));
        write_wasm(&addon_root.join("addons").join("mod-b").join("mod-b.wasm"));
        let mut snapshot = load(&root);
        assert_eq!(ids(&snapshot), ["mod-a", "mod-b"]);

        // Disabling must flip the row where it stands, not relocate it to
        // the bottom of the list.
        snapshot.toggle_enabled(index_of(&snapshot, "mod-a"));

        assert_eq!(ids(&snapshot), ["mod-a", "mod-b"]);
        let index = index_of(&snapshot, "mod-a");
        assert!(!snapshot.addons[index].enabled);
        assert_eq!(snapshot.addons[index].state, LifecycleState::Disabled);

        snapshot.toggle_enabled(index_of(&snapshot, "mod-a"));

        assert_eq!(ids(&snapshot), ["mod-a", "mod-b"]);
        assert!(snapshot.addons[index_of(&snapshot, "mod-a")].enabled);
    }

    #[test]
    fn refresh_preserves_error_and_expansion_quietly() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        write_wasm(&addon_root.join("addons").join("mod-a").join("mod-a.wasm"));
        let mut snapshot = load(&root);
        let index = index_of(&snapshot, "mod-a");
        snapshot.addons[index].error = Some(ActivationError {
            message: "boom".to_owned(),
        });
        snapshot.addons[index].expanded = true;

        // UI-only state is not a content change, so the refresh itself is
        // quiet — and stays quiet on the tick after that.
        assert!(!snapshot.refresh());
        let index = index_of(&snapshot, "mod-a");
        assert!(snapshot.addons[index].expanded);
        assert!(snapshot.addons[index].error.is_some());
        assert!(!snapshot.refresh());
    }

    #[test]
    fn successful_toggle_clears_row_error() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        write_wasm(&addon_root.join("addons").join("mod-a").join("mod-a.wasm"));
        let mut snapshot = load(&root);
        let index = index_of(&snapshot, "mod-a");
        snapshot.addons[index].error = Some(ActivationError {
            message: "stale".to_owned(),
        });
        snapshot.addons[index].expanded = true;

        snapshot.toggle_enabled(index_of(&snapshot, "mod-a"));

        let index = index_of(&snapshot, "mod-a");
        assert!(snapshot.addons[index].error.is_none());
        assert!(!snapshot.addons[index].expanded);
    }

    #[test]
    fn refresh_drops_vanished_and_appends_new_at_end() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        write_wasm(&addon_root.join("addons").join("mod-a").join("mod-a.wasm"));
        write_wasm(&addon_root.join("addons").join("mod-b").join("mod-b.wasm"));
        let mut snapshot = load(&root);

        fs::remove_dir_all(addon_root.join("addons").join("mod-a")).expect("remove");
        write_wasm(&addon_root.join("addons").join("mod-c").join("mod-c.wasm"));

        assert!(snapshot.refresh());
        assert_eq!(ids(&snapshot), ["mod-b", "mod-c"]);
    }

    fn ids(snapshot: &ManagerSnapshot) -> Vec<String> {
        snapshot
            .addons
            .iter()
            .map(|addon| addon.id.clone())
            .collect()
    }

    #[test]
    fn save_game_dir_rejects_missing_folders() {
        let root = TempRoot::new();
        let mut snapshot = load(&root);

        snapshot.save_game_dir("/tmp/does-not-exist-farever");

        assert!(snapshot.status.contains("not found"));
    }

    #[test]
    fn selecting_a_failed_addon_expands_its_error() {
        let mut snapshot = ManagerSnapshot::preview();
        snapshot.addons[3].expanded = false;

        snapshot.expand_error(3);

        assert!(snapshot.addons[3].expanded);
    }

    #[test]
    fn install_archive_reports_and_lists_the_addon() {
        use crate::archive::test_support::{manifest_json, write_test_zip, TestEntry};
        let root = TempRoot::new();
        let archive = root.dir().join("demo.zip");
        let wasm = b"\0asm\x01\0\0\0".to_vec();
        write_test_zip(
            &archive,
            &[
                TestEntry::stored("addon.json", &manifest_json("demo", &wasm)),
                TestEntry::stored("addon.wasm", &wasm),
            ],
        );
        let mut snapshot = load(&root);

        snapshot.install_archive(archive.to_str().expect("str"));

        assert!(
            snapshot.status.contains("1 component"),
            "{}",
            snapshot.status
        );
        assert_eq!(ids(&snapshot), ["demo"]);
    }

    #[test]
    fn install_archive_surfaces_backend_errors_as_status() {
        let root = TempRoot::new();
        let mut snapshot = load(&root);

        snapshot.install_archive(root.dir().join("missing.zip").to_str().expect("str"));

        assert!(snapshot.status.contains("not found"), "{}", snapshot.status);
        assert!(snapshot.addons.is_empty());
    }

    #[test]
    fn an_installed_addon_from_another_api_is_flagged_not_listed_as_active() {
        let root = TempRoot::new();
        let host = farever_more_manifest::api::ApiVersion::host();
        let live = backend::addons_dir(&root.addon_root());
        let wasm = b"\0asm\x01\0\0\0";
        for (id, declared) in [
            ("current", host.to_string()),
            ("future", format!("{}.0.0", host.major() + 1)),
        ] {
            // A unit can reach `addons/` without going through this manager's
            // installer (an older runtime was replaced under an existing
            // install, or the folder was copied in), so the scan has to judge
            // the declared API as well.
            let unit = live.join(id);
            std::fs::create_dir_all(&unit).expect("unit");
            std::fs::write(unit.join("addon.wasm"), wasm).expect("component");
            std::fs::write(
                unit.join("addon.json"),
                format!(
                    r#"{{"manifest-version": 1, "id": "{id}", "version": "0.1.0", "api-version": "{declared}"}}"#
                ),
            )
            .expect("manifest");
        }

        let snapshot = load(&root);
        let current = &snapshot.addons[index_of(&snapshot, "current")];
        assert_eq!(current.state, LifecycleState::Active, "{:?}", current.error);

        let future = &snapshot.addons[index_of(&snapshot, "future")];
        assert_eq!(future.state, LifecycleState::Incompatible);
        let message = future.error.as_ref().expect("error").message.clone();
        assert!(message.starts_with("Future: "), "{message}");
        assert!(message.contains(&host.to_string()), "{message}");
        assert_eq!(snapshot.summary().incompatible, 1);
        assert_eq!(snapshot.summary().active, 1);
    }
}
