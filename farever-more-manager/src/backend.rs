//! Filesystem backend for the add-on manager.
//!
//! This module owns the real on-disk contract so the manager can install,
//! update, remove, enable, and disable add-ons:
//!
//! ```text
//! <addon-root>/
//!   addons/            live add-ons (`<unit>/*.wasm`, or a bare `<name>.wasm`)
//!   .disabled-addons/  same layout, kept off the runtime's discovery path
//!   config/            per-add-on settings; never touched by remove/disable
//! ```
//!
//! New add-ons arrive as `.zip` archives (see [`crate::archive`] for the
//! format): the archive is validated, extracted into a hidden staging folder
//! inside the add-on root, then moved to `addons/<unit>/`. Installing over an
//! existing unit replaces it, which is also the update path.
//!
//! Toggling an add-on moves its whole unit (folder or single file) between
//! `addons/` and `.disabled-addons/`. Removing deletes the unit only;
//! `config/` is deliberately left alone so reinstalling restores settings.

use crate::archive;
pub(crate) use farever_more_manifest::{unit_name, MANIFEST_FILE_NAME};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const ADDONS_DIR_NAME: &str = "addons";
pub(crate) const DISABLED_DIR_NAME: &str = ".disabled-addons";
/// Version record stamped next to the installed runtime by whoever installs
/// it (`{"formatVersion": 1, "runtimeVersion": "0.1.0"}`). The manager never
/// invents this version: it only reads what installers wrote.
pub(crate) const RUNTIME_SIDECAR_FILE_NAME: &str = "runtime.json";
const RUNTIME_SIDECAR_FORMAT_VERSION: u64 = 1;
const MAX_NAME_BYTES: usize = 96;

/// One `.wasm` component found on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiscoveredAddon {
    /// Stable id: `addon.json` `id` when present, else the file stem.
    pub(crate) id: String,
    /// Human-readable name derived from the component metadata or the id.
    pub(crate) name: String,
    pub(crate) version: Option<String>,
    /// Add-on API version the component was built against, from `addon.json`.
    /// `None` for a unit that was never packed, which cannot be checked here.
    pub(crate) api_version: Option<String>,
    /// File name of the component, e.g. `minimap.wasm`.
    pub(crate) artifact: String,
    /// Exact component path.
    pub(crate) wasm_path: PathBuf,
    /// Move/delete granularity: the containing folder, or the file itself
    /// when it sits directly under `addons/` / `.disabled-addons/`.
    pub(crate) unit_path: PathBuf,
    pub(crate) enabled: bool,
}

pub(crate) fn addons_dir(addon_root: &Path) -> PathBuf {
    addon_root.join(ADDONS_DIR_NAME)
}

pub(crate) fn disabled_dir(addon_root: &Path) -> PathBuf {
    addon_root.join(DISABLED_DIR_NAME)
}

/// Resolves the add-on root (`<game-dir>/farever-addons`).
///
/// Precedence: `FAREVER_ADDONS_DIR`, then `<configured|FAREVER_GAME_DIR>`
/// `/farever-addons`, then `<manager-exe-dir>/farever-addons` (matches the
/// in-process host when the manager lives next to the game), then
/// `./farever-addons`. Prefer [`crate::config::ManagerConfig::addon_root`],
/// which feeds the configured game directory in.
pub(crate) fn resolve_addon_root_from(
    addons_env: Option<PathBuf>,
    game_env: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    cwd: Option<PathBuf>,
) -> PathBuf {
    if let Some(dir) = addons_env.filter(|dir| !dir.as_os_str().is_empty()) {
        return dir;
    }
    if let Some(game) = game_env.filter(|dir| !dir.as_os_str().is_empty()) {
        return game.join("farever-addons");
    }
    if let Some(dir) = exe_dir {
        return dir.join("farever-addons");
    }
    cwd.map(|dir| dir.join("farever-addons"))
        .unwrap_or_else(|| PathBuf::from("farever-addons"))
}

/// Scans `addons/` (enabled) and `.disabled-addons/` (disabled).
/// A missing root scans as empty; it is created lazily on install, not here.
pub(crate) fn scan(addon_root: &Path) -> Vec<DiscoveredAddon> {
    let mut found = Vec::new();
    collect_from(&addons_dir(addon_root), true, &mut found);
    collect_from(&disabled_dir(addon_root), false, &mut found);
    found.sort_by(|left, right| {
        left.enabled
            .cmp(&right.enabled)
            .reverse()
            .then(left.id.cmp(&right.id))
            .then(left.wasm_path.cmp(&right.wasm_path))
    });
    found
}

fn collect_from(directory: &Path, enabled: bool, found: &mut Vec<DiscoveredAddon>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            let Ok(children) = fs::read_dir(&path) else {
                continue;
            };
            let mut children = children.flatten().collect::<Vec<_>>();
            children.sort_by_key(|child| child.path());
            for child in children {
                let wasm = child.path();
                if wasm.is_file() && is_component(&wasm) {
                    found.push(describe(&wasm, &path, enabled));
                }
            }
        } else if path.is_file() && is_component(&path) {
            found.push(describe(&path, &path, enabled));
        }
    }
}

fn describe(wasm_path: &Path, unit_path: &Path, enabled: bool) -> DiscoveredAddon {
    let summary = read_manifest_summary(&wasm_path.with_file_name(MANIFEST_FILE_NAME));
    let stem = unit_name(wasm_path)
        .filter(|stem| !stem.is_empty())
        .unwrap_or("addon");
    let id = summary
        .as_ref()
        .map(|summary| summary.id.clone())
        .unwrap_or_else(|| stem.to_owned());
    let (meta_name, meta_version) = read_component_identity(wasm_path);
    let version = summary
        .as_ref()
        .map(|summary| summary.version.clone())
        .filter(|version| !version.is_empty())
        .or(meta_version);
    let name = summary
        .as_ref()
        .and_then(|summary| summary.name.clone())
        .or(meta_name)
        .unwrap_or_else(|| pretty_name(&id));
    DiscoveredAddon {
        id,
        name,
        version,
        api_version: summary
            .as_ref()
            .and_then(|summary| summary.api_version.clone()),
        artifact: wasm_path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("addon.wasm")
            .to_owned(),
        wasm_path: wasm_path.to_owned(),
        unit_path: unit_path.to_owned(),
        enabled,
    }
}

/// Moves an add-on unit between `addons/` and `.disabled-addons/`.
///
/// `unit_path` must be a path previously returned by [`scan`]. Returns the
/// new location. Disabling keeps `config/` untouched by design.
pub(crate) fn set_enabled(
    addon_root: &Path,
    unit_path: &Path,
    enabled: bool,
) -> Result<PathBuf, String> {
    let (live, parked) = (addons_dir(addon_root), disabled_dir(addon_root));
    let current = normalize_unit(addon_root, unit_path)?;
    let already = if enabled {
        current.parent() == Some(live.as_path()) || current == live
    } else {
        current.parent() == Some(parked.as_path()) || current == parked
    };
    if already {
        return Ok(current);
    }
    let file_name = current
        .file_name()
        .ok_or_else(|| format!("add-on path has no file name: {}", current.display()))?;
    let destination_root = if enabled { &live } else { &parked };
    fs::create_dir_all(destination_root)
        .map_err(|error| format!("create {}: {error}", destination_root.display()))?;
    let destination = destination_root.join(file_name);
    if destination.exists() {
        return Err(format!(
            "cannot move add-on to {}: destination already exists",
            destination.display()
        ));
    }
    fs::rename(&current, &destination)
        .map_err(|error| format!("move {}: {error}", current.display()))?;
    Ok(destination)
}

/// Deletes an add-on unit (folder or single file) from `addons/` or
/// `.disabled-addons/`. Settings under `config/` are preserved.
pub(crate) fn remove_unit(addon_root: &Path, unit_path: &Path) -> Result<(), String> {
    let current = normalize_unit(addon_root, unit_path)?;
    if current.is_dir() {
        fs::remove_dir_all(&current)
            .map_err(|error| format!("remove {}: {error}", current.display()))
    } else if current.is_file() {
        fs::remove_file(&current).map_err(|error| format!("remove {}: {error}", current.display()))
    } else {
        Err(format!("add-on not found: {}", current.display()))
    }
}

/// Ensures `unit_path` is the scanned unit inside this root and returns it.
/// Rejects anything outside `addons/` and `.disabled-addons/` so a remove or
/// move can never escape the managed directories.
fn normalize_unit(addon_root: &Path, unit_path: &Path) -> Result<PathBuf, String> {
    let (live, parked) = (addons_dir(addon_root), disabled_dir(addon_root));
    let direct = unit_path.to_path_buf();
    if is_unit_root(&direct, &live) || is_unit_root(&direct, &parked) {
        return Ok(direct);
    }
    // Accept a wasm path nested one level inside a unit folder.
    if let Some(parent) = unit_path.parent() {
        let parent = parent.to_path_buf();
        if is_unit_root(&parent, &live) || is_unit_root(&parent, &parked) {
            return Ok(parent);
        }
    }
    Err(format!(
        "refusing to modify unmanaged path: {}",
        unit_path.display()
    ))
}

fn is_unit_root(candidate: &Path, container: &Path) -> bool {
    candidate.parent() == Some(container) && candidate != container
}

/// Whether `Farever.exe` currently runs. Used to gate runtime self-updates:
/// `dinput8.dll` and `farever-addons/host.dll` are locked while the game
/// holds them.
pub(crate) fn is_game_running() -> bool {
    #[cfg(windows)]
    {
        let Ok(output) = Command::new("tasklist")
            .args(["/NH", "/FI", "IMAGENAME eq Farever.exe"])
            .output()
        else {
            return false;
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.to_lowercase().contains("farever.exe")
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Replaces the runtime files from locally built artifacts.
///
/// Copies `proxy_source` to `<game-dir>/dinput8.dll` and `host_source` to
/// `<game-dir>/farever-addons/host.dll`. Refuses while the game runs because
/// both files are locked by the loaded game process.
pub(crate) fn update_runtime(
    game_dir: &Path,
    proxy_source: &Path,
    host_source: &Path,
) -> Result<(), String> {
    if is_game_running() {
        return Err("Farever is running. Close the game before updating the runtime.".to_owned());
    }
    install_runtime_files(game_dir, proxy_source, host_source)
}

fn install_runtime_files(
    game_dir: &Path,
    proxy_source: &Path,
    host_source: &Path,
) -> Result<(), String> {
    for source in [proxy_source, host_source] {
        if !source.is_file() {
            return Err(format!("runtime artifact not found: {}", source.display()));
        }
    }
    let proxy_destination = game_dir.join("dinput8.dll");
    let host_destination = game_dir.join("farever-addons").join("host.dll");
    if let Some(parent) = host_destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    fs::copy(proxy_source, &proxy_destination)
        .map_err(|error| format!("install {}: {error}", proxy_destination.display()))?;
    fs::copy(host_source, &host_destination)
        .map_err(|error| format!("install {}: {error}", host_destination.display()))?;
    refresh_runtime_sidecar(
        &host_destination
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| game_dir.join("farever-addons")),
        [proxy_source, host_source],
    )?;
    Ok(())
}

/// Reads the installed runtime version stamped by the installer, if any.
pub(crate) fn read_runtime_sidecar(addon_root: &Path) -> Option<String> {
    let bytes = fs::read(addon_root.join(RUNTIME_SIDECAR_FILE_NAME)).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if value
        .get("formatVersion")
        .and_then(serde_json::Value::as_u64)
        != Some(RUNTIME_SIDECAR_FORMAT_VERSION)
    {
        return None;
    }
    value
        .get("runtimeVersion")
        .and_then(serde_json::Value::as_str)
        .filter(|version| !version.is_empty())
        .map(ToOwned::to_owned)
}

/// Carries a shipped sidecar next to the new DLLs, or drops a stale record.
/// DLLs of unknown provenance must not keep advertising an older version.
fn refresh_runtime_sidecar(addon_root: &Path, sources: [&Path; 2]) -> Result<(), String> {
    let destination = addon_root.join(RUNTIME_SIDECAR_FILE_NAME);
    let shipped = sources
        .into_iter()
        .filter_map(|source| source.parent())
        .map(|directory| directory.join(RUNTIME_SIDECAR_FILE_NAME))
        .find(|path| path.is_file());
    match shipped {
        Some(source) => {
            fs::copy(&source, &destination)
                .map_err(|error| format!("install {}: {error}", destination.display()))?;
            Ok(())
        }
        None => {
            // Best effort: absence already means "unknown version".
            let _ = fs::remove_file(&destination);
            Ok(())
        }
    }
}

/// Installs an add-on `.zip` archive (see [`crate::archive`]) as
/// `<addon-root>/addons/<unit>/`, where `<unit>` derives from the manifest
/// id. The archive is validated and extracted into a hidden staging folder
/// inside the add-on root — same volume, so the final move is a rename, and
/// invisible to [`scan`] — then moved into place. Installing over an existing
/// unit replaces it (the update path); `config/` is never touched.
pub(crate) fn install_archive(addon_root: &Path, archive: &Path) -> Result<Vec<PathBuf>, String> {
    if !archive.is_file() {
        return Err(format!("archive not found: {}", archive.display()));
    }
    let is_zip = archive
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"));
    if !is_zip {
        return Err(format!(
            "expected a .zip add-on archive: {}",
            archive.display()
        ));
    }
    fs::create_dir_all(addon_root)
        .map_err(|error| format!("create {}: {error}", addon_root.display()))?;
    let staging = staging_dir(addon_root)?;
    let result = install_staged(addon_root, archive, &staging);
    // Best effort: a successful install renamed the staging dir away, so this
    // is a no-op then; a failed one must not leave half a unit behind.
    let _ = fs::remove_dir_all(&staging);
    result
}

fn install_staged(
    addon_root: &Path,
    archive: &Path,
    staging: &Path,
) -> Result<Vec<PathBuf>, String> {
    let extracted = archive::extract_archive(archive, staging)?;
    let unit = archive::sanitize_unit_name(&extracted.id);
    let live = addons_dir(addon_root);
    fs::create_dir_all(&live).map_err(|error| format!("create {}: {error}", live.display()))?;
    let destination = live.join(&unit);
    if destination.is_dir() {
        fs::remove_dir_all(&destination)
            .map_err(|error| format!("replace {}: {error}", destination.display()))?;
    } else if destination.is_file() {
        fs::remove_file(&destination)
            .map_err(|error| format!("replace {}: {error}", destination.display()))?;
    }
    let unit_root = if staging.join(MANIFEST_FILE_NAME).is_file() {
        staging.to_path_buf()
    } else {
        // `extract_archive` guarantees the single-folder case resolves here.
        fs::read_dir(staging)
            .map_err(|error| format!("read {}: {error}", staging.display()))?
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.is_dir())
            .ok_or_else(|| "archive unit vanished during install".to_owned())?
    };
    fs::rename(&unit_root, &destination)
        .map_err(|error| format!("install {}: {error}", destination.display()))?;
    let installed: Vec<PathBuf> = extracted
        .wasm_files
        .iter()
        .map(|name| destination.join(name))
        .collect();
    if let Err(error) = verify_installed_unit(&destination, &extracted.wasm_files) {
        // A unit that fails its own integrity check must never survive as an
        // install: remove it instead of leaving a half-written add-on behind.
        let _ = fs::remove_dir_all(&destination);
        return Err(error);
    }
    Ok(installed)
}

/// Re-hashes an installed unit's components against the manifest that shipped
/// with them.
///
/// Extraction already checked the staged bytes; this proves the files that
/// actually landed in `addons/<unit>/` are the ones the archive declared, so a
/// short write or an interrupted copy fails the install instead of reaching the
/// runtime later.
fn verify_installed_unit(destination: &Path, wasm_files: &[String]) -> Result<(), String> {
    let manifest_path = destination.join(MANIFEST_FILE_NAME);
    let bytes = fs::read(&manifest_path)
        .map_err(|error| format!("read {}: {error}", manifest_path.display()))?;
    let manifest = farever_more_manifest::parse(&bytes)?;
    for name in wasm_files {
        let path = destination.join(name);
        let component =
            fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
        let digest = hex::encode(Sha256::digest(&component));
        if !digest.eq_ignore_ascii_case(&manifest.sha256) {
            return Err(format!(
                "{name} does not match the fingerprint in {MANIFEST_FILE_NAME} after install"
            ));
        }
    }
    Ok(())
}

fn staging_dir(addon_root: &Path) -> Result<PathBuf, String> {
    static NEXT_STAGING_ID: AtomicU64 = AtomicU64::new(1);
    for _ in 0..10 {
        let id = NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed);
        let staging = addon_root.join(format!(".install-{}-{id}", std::process::id()));
        match fs::create_dir(&staging) {
            Ok(()) => return Ok(staging),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("create {}: {error}", staging.display()));
            }
        }
    }
    Err("could not stage the archive install".to_owned())
}

fn is_component(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wasm"))
}

struct ManifestSummary {
    id: String,
    version: String,
    api_version: Option<String>,
    name: Option<String>,
}

/// Lenient manifest read for discovery. A full typed parse wins when the
/// file is a valid manifest; legacy or minimal files fall back to a bare
/// `id` salvage, exactly like before. Anything unreadable degrades to
/// file-stem identity. Strictness is the installer's job
/// (`AddonManifest::validate`), not the scanner's.
fn read_manifest_summary(manifest_path: &Path) -> Option<ManifestSummary> {
    let bytes = fs::read(manifest_path).ok()?;
    if let Ok(manifest) = farever_more_manifest::parse(&bytes) {
        if farever_more_manifest::is_valid_name(&manifest.id) {
            return Some(ManifestSummary {
                id: manifest.id,
                version: sanitize(&manifest.version),
                api_version: manifest
                    .api_version
                    .as_deref()
                    .map(sanitize)
                    .filter(|version| !version.is_empty()),
                name: manifest
                    .name
                    .as_deref()
                    .map(sanitize)
                    .filter(|name| !name.is_empty()),
            });
        }
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_owned();
    Some(ManifestSummary {
        id,
        version: String::new(),
        api_version: None,
        name: None,
    })
}

fn read_component_identity(path: &Path) -> (Option<String>, Option<String>) {
    let Ok(bytes) = fs::read(path) else {
        return (None, None);
    };
    let Ok(payload) = wasm_metadata::Payload::from_binary(&bytes) else {
        return (None, None);
    };
    let metadata = payload.metadata();
    let name = metadata
        .name
        .as_deref()
        .map(|name| sanitize(name))
        .filter(|name| !name.is_empty());
    let version = metadata
        .version
        .as_ref()
        .map(ToString::to_string)
        .map(|version| sanitize(&version))
        .filter(|version| !version.is_empty());
    (name, version)
}

fn sanitize(value: &str) -> String {
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
    let trimmed = printable.trim();
    let mut end = trimmed.len().min(MAX_NAME_BYTES);
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    trimmed[..end].trim().to_owned()
}

pub(crate) fn pretty_name(id: &str) -> String {
    let spaced = id.replace(['.', '-', '_'], " ");
    let titled = spaced
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if titled.is_empty() {
        "Addon".to_owned()
    } else {
        titled
    }
}

/// Initials for the avatar tile, e.g. `minimap` -> `MP`.
/// The `farever` vendor prefix is skipped when a longer name follows it.
pub(crate) fn initials_for(name: &str) -> String {
    let mut words = name.split_whitespace().collect::<Vec<_>>();
    if words.len() > 1 && words[0].eq_ignore_ascii_case("farever") {
        words.remove(0);
    }
    let mut initials = String::new();
    for word in words.iter().take(2) {
        if let Some(first) = word.chars().find(|char| char.is_alphanumeric()) {
            initials.extend(first.to_uppercase());
        }
    }
    if initials.is_empty() {
        initials.push('A');
    }
    initials
}

/// Finds an add-on unit by id, file stem, or exact path.
/// Returns the unit path suitable for [`set_enabled`] and [`remove_unit`].
pub(crate) fn find_unit(addon_root: &Path, query: &str) -> Option<PathBuf> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }
    let direct = PathBuf::from(trimmed);
    if direct.is_absolute() || trimmed.contains(['/', '\\']) {
        return normalize_unit(addon_root, &direct).ok();
    }
    scan(addon_root)
        .into_iter()
        .find(|addon| {
            addon.id == trimmed || unit_name(&addon.wasm_path).is_some_and(|name| name == trimmed)
        })
        .map(|addon| addon.unit_path)
}

#[cfg(test)]
pub(crate) struct TempRoot(PathBuf);

#[cfg(test)]
impl TempRoot {
    pub(crate) fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("farever-manager-test-{}-{id}", std::process::id()));
        fs::create_dir_all(&root).expect("create temp root");
        Self(root)
    }

    pub(crate) fn addon_root(&self) -> PathBuf {
        self.0.join("farever-addons")
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wasm(path: &Path) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, b"\0asm\x01\0\0\0").expect("write stub wasm");
    }

    #[test]
    fn missing_root_scans_as_empty() {
        let root = TempRoot::new();
        assert!(scan(&root.addon_root()).is_empty());
    }

    #[test]
    fn scan_sees_enabled_and_disabled_units() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        write_wasm(&addon_root.join("addons").join("mod-a").join("mod-a.wasm"));
        write_wasm(&addon_root.join("addons").join("loose.wasm"));
        write_wasm(
            &addon_root
                .join(".disabled-addons")
                .join("mod-b")
                .join("mod-b.wasm"),
        );

        let found = scan(&addon_root);

        assert_eq!(found.len(), 3);
        assert!(found.iter().all(|addon| !addon.id.is_empty()));
        let disabled = found
            .iter()
            .find(|addon| addon.id == "mod-b")
            .expect("disabled");
        assert!(!disabled.enabled);
        assert!(found.iter().filter(|addon| addon.enabled).count() == 2);
    }

    #[test]
    fn manifest_id_wins_over_file_stem() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        let dir = addon_root.join("addons").join("folder");
        write_wasm(&dir.join("component.wasm"));
        // Legacy salvage path: no manifest version, just a bare id.
        fs::write(dir.join(MANIFEST_FILE_NAME), r#"{"id": "minimap"}"#).expect("write manifest");

        let found = scan(&addon_root);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "minimap");
    }

    #[test]
    fn typed_manifest_supplies_version_and_name() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        let dir = addon_root.join("addons").join("folder");
        write_wasm(&dir.join("component.wasm"));
        fs::write(
            dir.join(MANIFEST_FILE_NAME),
            r#"{"manifest-version": 1, "id": "demo", "version": "2.3.4", "name": "Demo Addon"}"#,
        )
        .expect("write manifest");

        let found = scan(&addon_root);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "demo");
        assert_eq!(found[0].version.as_deref(), Some("2.3.4"));
        assert_eq!(found[0].name, "Demo Addon");
    }

    #[test]
    fn disable_moves_folder_and_enable_moves_it_back() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        let unit = addon_root.join("addons").join("mod-a");
        write_wasm(&unit.join("mod-a.wasm"));

        let parked = set_enabled(&addon_root, &unit, false).expect("disable");
        assert_eq!(parked, disabled_dir(&addon_root).join("mod-a"));
        assert!(parked.join("mod-a.wasm").is_file());
        let found = scan(&addon_root);
        assert_eq!(found.len(), 1);
        assert!(!found[0].enabled);

        let live = set_enabled(&addon_root, &parked, true).expect("enable");
        assert_eq!(live, addons_dir(&addon_root).join("mod-a"));
        assert!(scan(&addon_root)[0].enabled);
    }

    #[test]
    fn set_enabled_is_idempotent() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        let unit = addon_root.join("addons").join("mod-a");
        write_wasm(&unit.join("mod-a.wasm"));

        let same = set_enabled(&addon_root, &unit, true).expect("enable");
        assert_eq!(same, unit);
    }

    #[test]
    fn set_enabled_refuses_destination_collision() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        let live = addon_root.join("addons").join("mod-a");
        let parked = disabled_dir(&addon_root).join("mod-a");
        write_wasm(&live.join("mod-a.wasm"));
        write_wasm(&parked.join("other.wasm"));

        let error = set_enabled(&addon_root, &live, false).expect_err("collision");
        assert!(error.contains("already exists"), "{error}");
    }

    #[test]
    fn remove_deletes_unit_but_keeps_config() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        let unit = addon_root.join("addons").join("mod-a");
        write_wasm(&unit.join("mod-a.wasm"));
        let config = addon_root.join("config").join("settings.fmc");
        fs::create_dir_all(config.parent().expect("parent")).expect("config dir");
        fs::write(&config, b"settings").expect("config");

        remove_unit(&addon_root, &unit).expect("remove");

        assert!(!unit.exists());
        assert!(config.is_file());
        assert!(scan(&addon_root).is_empty());
    }

    #[test]
    fn remove_refuses_paths_outside_managed_dirs() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();

        let error = remove_unit(&addon_root, &root.0.join("elsewhere")).expect_err("escape");
        assert!(error.contains("unmanaged"), "{error}");
    }

    #[test]
    fn resolve_prefers_explicit_overrides() {
        let resolved = resolve_addon_root_from(
            Some(PathBuf::from("/tmp/custom-addons")),
            Some(PathBuf::from("/tmp/game")),
            Some(PathBuf::from("/tmp/exe")),
            Some(PathBuf::from("/tmp/cwd")),
        );
        assert_eq!(resolved, PathBuf::from("/tmp/custom-addons"));

        let resolved = resolve_addon_root_from(
            None,
            Some(PathBuf::from("/tmp/game")),
            Some(PathBuf::from("/tmp/exe")),
            Some(PathBuf::from("/tmp/cwd")),
        );
        assert_eq!(resolved, PathBuf::from("/tmp/game/farever-addons"));
    }

    #[test]
    fn install_archive_rejects_non_zip_and_missing() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        let notes = root.0.join("notes.txt");
        fs::write(&notes, b"not an archive").expect("notes");

        let error = install_archive(&addon_root, &notes).expect_err("extension");
        assert!(error.contains(".zip"), "{error}");
        let error = install_archive(&addon_root, &root.0.join("missing.zip")).expect_err("missing");
        assert!(error.contains("not found"), "{error}");
    }

    #[test]
    fn install_archive_installs_unit_visible_to_scan() {
        use crate::archive::test_support::{manifest_json, write_test_zip, TestEntry};
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        let archive = root.0.join("my-addon.zip");
        let wasm = b"\0asm\x01\0\0\0".to_vec();
        write_test_zip(
            &archive,
            &[
                TestEntry::stored("addon.json", &manifest_json("farever.my-addon", &wasm)),
                TestEntry::stored("addon.wasm", &wasm),
            ],
        );

        let installed = install_archive(&addon_root, &archive).expect("install");

        assert_eq!(
            installed,
            vec![addons_dir(&addon_root)
                .join("farever-my-addon")
                .join("addon.wasm")]
        );
        let found = scan(&addon_root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "farever.my-addon");
        assert_eq!(found[0].version.as_deref(), Some("0.1.0"));
        assert!(found[0].enabled);
    }

    #[test]
    fn install_archive_replaces_existing_unit_and_keeps_config() {
        use crate::archive::test_support::{manifest_json, write_test_zip, TestEntry};
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        let config = addon_root.join("config").join("settings.fmc");
        fs::create_dir_all(config.parent().expect("parent")).expect("config dir");
        fs::write(&config, b"settings").expect("config");
        for payload in [b"v1".as_slice(), b"v2".as_slice()] {
            let archive = root.0.join("my-addon.zip");
            let mut wasm = b"\0asm\x01\0\0\0".to_vec();
            wasm.extend_from_slice(payload);
            write_test_zip(
                &archive,
                &[
                    TestEntry::stored("addon.json", &manifest_json("demo", &wasm)),
                    TestEntry::stored("addon.wasm", &wasm),
                ],
            );
            install_archive(&addon_root, &archive).expect("install");
        }

        let unit = addons_dir(&addon_root).join("demo");
        let bytes = fs::read(unit.join("addon.wasm")).expect("wasm");
        assert!(bytes.ends_with(b"v2"));
        assert!(config.is_file());
        assert_eq!(scan(&addon_root).len(), 1);
    }

    #[test]
    fn installed_units_are_re_verified_after_they_land() {
        let root = TempRoot::new();
        let unit = addons_dir(&root.addon_root()).join("demo");
        fs::create_dir_all(&unit).expect("create unit");
        let wasm = b"\0asm\x01\0\0\0";
        fs::write(unit.join("addon.wasm"), wasm).expect("write component");
        let manifest = |sha256: String| {
            format!(
                r#"{{"manifest-version": 1, "id": "demo", "version": "0.1.0", "sha256": "{sha256}"}}"#
            )
        };
        let names = vec!["addon.wasm".to_owned()];

        fs::write(
            unit.join(MANIFEST_FILE_NAME),
            manifest(hex::encode(Sha256::digest(wasm))),
        )
        .expect("write manifest");
        assert!(verify_installed_unit(&unit, &names).is_ok());

        // A component that no longer matches what the manifest pinned - a
        // short write, a manual edit - fails instead of being installed.
        fs::write(unit.join(MANIFEST_FILE_NAME), manifest("00".repeat(32))).expect("tamper");
        let error = verify_installed_unit(&unit, &names).expect_err("mismatch");
        assert!(error.contains("fingerprint"), "{error}");
    }

    #[test]
    fn install_archive_rejects_bad_archive_without_residue() {
        use crate::archive::test_support::{write_test_zip, TestEntry};
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        let archive = root.0.join("bad.zip");
        write_test_zip(
            &archive,
            &[TestEntry::stored("demo.wasm", b"\0asm\x01\0\0\0")],
        );

        let error = install_archive(&addon_root, &archive).expect_err("manifest");
        assert!(error.contains(MANIFEST_FILE_NAME), "{error}");
        // Neither a unit nor a staging folder may survive a failed install.
        assert!(scan(&addon_root).is_empty());
        let leftovers = fs::read_dir(&addon_root)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn update_runtime_carries_a_shipped_sidecar() {
        let root = TempRoot::new();
        let game = root.0.join("game");
        let artifacts = root.0.join("artifacts");
        fs::create_dir_all(&artifacts).expect("artifacts");
        fs::write(artifacts.join("proxy.dll"), b"proxy").expect("proxy");
        fs::write(artifacts.join("host.dll"), b"host").expect("host");
        fs::write(
            artifacts.join(RUNTIME_SIDECAR_FILE_NAME),
            r#"{"formatVersion": 1, "runtimeVersion": "0.2.0"}"#,
        )
        .expect("sidecar");

        install_runtime_files(
            &game,
            &artifacts.join("proxy.dll"),
            &artifacts.join("host.dll"),
        )
        .expect("update");

        assert!(game.join("dinput8.dll").is_file());
        assert!(game.join("farever-addons").join("host.dll").is_file());
        assert_eq!(
            read_runtime_sidecar(&game.join("farever-addons")),
            Some("0.2.0".to_owned())
        );
    }

    #[test]
    fn update_runtime_drops_a_stale_sidecar() {
        let root = TempRoot::new();
        let game = root.0.join("game");
        let addon_root = game.join("farever-addons");
        fs::create_dir_all(&addon_root).expect("root");
        fs::write(
            addon_root.join(RUNTIME_SIDECAR_FILE_NAME),
            r#"{"formatVersion": 1, "runtimeVersion": "0.1.0"}"#,
        )
        .expect("stale");
        let artifacts = root.0.join("artifacts");
        fs::create_dir_all(&artifacts).expect("artifacts");
        fs::write(artifacts.join("proxy.dll"), b"proxy").expect("proxy");
        fs::write(artifacts.join("host.dll"), b"host").expect("host");

        install_runtime_files(
            &game,
            &artifacts.join("proxy.dll"),
            &artifacts.join("host.dll"),
        )
        .expect("update");

        assert!(read_runtime_sidecar(&addon_root).is_none());
    }

    #[test]
    fn sidecar_reader_rejects_unknown_shapes() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        fs::create_dir_all(&addon_root).expect("root");

        assert!(read_runtime_sidecar(&addon_root).is_none());
        fs::write(addon_root.join(RUNTIME_SIDECAR_FILE_NAME), b"{nope").expect("write");
        assert!(read_runtime_sidecar(&addon_root).is_none());
        fs::write(
            addon_root.join(RUNTIME_SIDECAR_FILE_NAME),
            r#"{"formatVersion": 999, "runtimeVersion": "9.9.9"}"#,
        )
        .expect("write");
        assert!(read_runtime_sidecar(&addon_root).is_none());
    }

    #[test]
    fn find_unit_matches_id_stem_and_rejects_blank() {
        let root = TempRoot::new();
        let addon_root = root.addon_root();
        write_wasm(&addon_root.join("addons").join("mod-a").join("mod-a.wasm"));

        assert!(find_unit(&addon_root, "").is_none());
        assert!(find_unit(&addon_root, "nope").is_none());
        let by_id = find_unit(&addon_root, "mod-a").expect("by id");
        assert_eq!(by_id, addon_root.join("addons").join("mod-a"));
        let parked = set_enabled(&addon_root, &by_id, false).expect("disable");
        assert_eq!(find_unit(&addon_root, "mod-a").expect("after move"), parked);
    }
}
