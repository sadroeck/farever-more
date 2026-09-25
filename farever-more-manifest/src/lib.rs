//! `addon.json`: the well-known add-on manifest.
//!
//! One file, one struct, every reader: the runtime resolves load order from
//! it, the manager installs and lists from it, and the future remote index
//! searches it. The JSON shape is the spec; this crate is its serde encoding.
//!
//! ```json
//! {
//!   "manifest-version": 1,
//!   "id": "minimap",
//!   "version": "0.2.0",
//!   "name": "Minimap",
//!   "description": "A lightweight world minimap.",
//!   "tags": ["map", "overlay"],
//!   "sha256": "9f2c…",
//!   "api-version": "1.0.0",
//!   "icon": "assets/icon.png",
//!   "repository": "https://github.com/example/minimap",
//!   "provides": ["poi"],
//!   "dependencies": [
//!     { "addon": "poi-database", "services": [{ "id": "poi", "version": "^1" }] }
//!   ]
//! }
//! ```
//!
//! Rules that keep the format shareable and forward extensible:
//!
//! * Keys are kebab-case. Unknown keys are **ignored**, never rejected, so
//!   old readers keep working when new fields arrive. New readers reject
//!   `manifest-version` values they do not understand instead of guessing.
//! * Every path in the manifest (`icon`) is relative to the add-on's own
//!   folder and must stay inside it: no absolute paths, no `..`, no drive
//!   prefixes, no backslashes. Manifests are meant to be shared, so absolute
//!   paths are meaningless in them.
//! * An add-on is exactly one component, always named `addon.wasm`
//!   ([`WASM_FILE_NAME`]) at the unit root. The name is not configurable, so
//!   there is nothing to declare and nothing to disagree about.
//! * `sha256` pins those component bytes. Source trees omit it — the hash of
//!   a future build cannot be known upfront — and the pack step stamps it in.
//!   Distribution archives (what the manager installs) must carry it.
//! * `api-version` names the `farever:addon` API the component was built
//!   against ([`api`]). Source trees omit it for the same reason as `sha256`:
//!   the pack step reads it out of the built component and stamps it in.
//!   Installers and loaders refuse a collection whose API the running
//!   framework cannot satisfy, so an add-on never reaches activation only to
//!   fail inside the WebAssembly linker.
//! * A service carries no version of its own: `provides` lists service names
//!   and every one of them is served at this add-on's `version`. Consumers
//!   therefore gate on the provider's release instead of on a second number
//!   that has to be kept in step by hand.
//! * A dependency names the provider's add-on id, never a local alias: one
//!   edge, one name, and the guest opens the service under that same id.
//! * `version` is a free-form string with semver recommended; ordering and
//!   update policy belong to the index, not to this parser.

use serde::{Deserialize, Serialize};
use std::path::Path;

pub mod api;

/// File name of the manifest inside an add-on unit.
pub const MANIFEST_FILE_NAME: &str = "addon.json";
/// The only manifest version defined so far.
pub const MANIFEST_VERSION: u32 = 1;
/// File name of the add-on's single component at the unit root. Fixed by the
/// format: manifests never name it, so every reader looks in one place.
pub const WASM_FILE_NAME: &str = "addon.wasm";

pub const MAX_ID_LEN: usize = 128;
pub const MAX_VERSION_LEN: usize = 64;
pub const MAX_API_VERSION_LEN: usize = 32;
pub const MAX_NAME_LEN: usize = 96;
pub const MAX_DESCRIPTION_LEN: usize = 4096;
pub const MAX_TAGS: usize = 32;
pub const MAX_TAG_LEN: usize = 64;
pub const MAX_REPOSITORY_LEN: usize = 512;

/// The add-on manifest. Parses leniently (every field except the version has
/// a default, unknown fields are ignored); strictness lives in
/// [`AddonManifest::validate`], which installers and packers run.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct AddonManifest {
    /// Schema version. Required: without it the shape is unknown.
    pub manifest_version: u32,
    /// Stable id, e.g. `farever.minimap`. Lowercase ASCII that starts
    /// with a letter; dots, dashes, and underscores allowed.
    #[serde(default)]
    pub id: String,
    /// Publisher-declared release string. Semver recommended, not enforced.
    #[serde(default)]
    pub version: String,
    /// Display name. Falls back to a prettified id when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Short human description for listings and search.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Indexing keywords. Normalized (trimmed, lowercased, sorted,
    /// deduplicated) by [`AddonManifest::validate`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Lowercase hex SHA-256 of the add-on's component, `addon.wasm`
    /// ([`WASM_FILE_NAME`]). Stamped by the pack step; verified by the
    /// installer.
    #[serde(default)]
    pub sha256: String,
    /// Add-on API version the component was built against, e.g. `1.0.0`
    /// ([`api::ADDON_API_VERSION`]). Stamped by the pack step from the
    /// component's own imports; installers and loaders refuse an add-on whose
    /// API the running framework cannot satisfy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    /// Display image inside the unit, e.g. `assets/icon.png`. Nested paths
    /// are fine; confinement rules apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Provenance pointer, e.g. the source repository URL. Informational
    /// only; free-form locator, `https://` recommended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Names of the services this add-on offers to others. Read by the
    /// runtime; each is served at this add-on's own `version`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<String>,
    /// Load-ordering and required-service declarations. Read by the runtime.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<AddonDependency>,
}

/// A load-ordering dependency on another add-on.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AddonDependency {
    /// Add-on id of the provider. The guest opens its services under this
    /// same id, so there is nothing else to name the edge.
    pub addon: String,
    #[serde(default)]
    pub services: Vec<RequiredService>,
    /// Best-effort dependency: skipped by the host's required-dependency
    /// pre-check and never blocks activation; the consumer degrades
    /// gracefully when the provider is unavailable. Defaults to false.
    #[serde(default)]
    pub optional: bool,
}

/// A service version range required from a dependency.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RequiredService {
    pub id: String,
    pub version: String,
}

/// Parses a manifest. Checks syntax only; semantic rules (including the
/// version gate) are [`AddonManifest::validate`]'s job.
pub fn parse(bytes: &[u8]) -> Result<AddonManifest, String> {
    serde_json::from_slice(bytes).map_err(|error| format!("parse {MANIFEST_FILE_NAME}: {error}"))
}

/// Add-on and service identifier rule, shared with the runtime loader:
/// lowercase ASCII starting with a letter, then letters, digits, dots,
/// dashes, and underscores.
pub fn is_valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_LEN
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

/// Names a component's unit on disk.
///
/// A canonical unit holds its component as `addon.wasm` ([`WASM_FILE_NAME`]),
/// so the component's own file name cannot name the add-on: the folder does.
/// A loose component keeps its file stem. Readers use this only as a fallback —
/// a manifest `id` always wins.
#[must_use]
pub fn unit_name(component: &Path) -> Option<&str> {
    let file_name = component.file_name()?.to_str()?;
    if file_name.eq_ignore_ascii_case(WASM_FILE_NAME) {
        return component.parent()?.file_name()?.to_str();
    }
    component.file_stem()?.to_str()
}

/// Whether a manifest path reference stays inside the add-on folder:
/// relative, no navigation, no drive prefixes, no backslashes.
pub fn is_confined_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains('\0')
        && !path.split('/').any(|component| {
            component.is_empty() || component == "." || component == ".." || component.contains(':')
        })
}

fn has_forbidden_control(value: &str) -> bool {
    value
        .chars()
        .any(|char| char.is_control() && !matches!(char, '\n' | '\t' | '\r'))
}

impl AddonManifest {
    /// Strict gate for installers and packers: rejects unknown schema
    /// versions and malformed fields, normalizes tags and the fingerprint,
    /// and checks that every referenced file exists under `unit_root`.
    /// Loaders (the runtime) deliberately do not call this: they read the
    /// fields they need and ignore the rest.
    pub fn validate(&mut self, unit_root: &Path) -> Result<(), String> {
        if self.manifest_version != MANIFEST_VERSION {
            return Err(format!(
                "unsupported manifest version {}, expected {MANIFEST_VERSION}",
                self.manifest_version
            ));
        }
        if !is_valid_name(&self.id) {
            return Err(format!("invalid add-on id {:?}", self.id));
        }
        if self.version.is_empty() || self.version.len() > MAX_VERSION_LEN {
            return Err("add-on version must be a non-empty string".to_owned());
        }
        if self.version.chars().any(char::is_control) {
            return Err(format!("invalid add-on version {:?}", self.version));
        }
        if let Some(name) = self.name.take() {
            if !name.trim().is_empty() {
                if name.len() > MAX_NAME_LEN || name.chars().any(char::is_control) {
                    return Err(format!("invalid add-on name {name:?}"));
                }
                self.name = Some(name);
            }
        }
        if self.description.len() > MAX_DESCRIPTION_LEN {
            return Err(format!(
                "add-on description exceeds {MAX_DESCRIPTION_LEN} characters"
            ));
        }
        if has_forbidden_control(&self.description) {
            return Err("add-on description contains control characters".to_owned());
        }
        self.normalize_tags()?;
        self.validate_fingerprint(unit_root)?;
        self.validate_api_version()?;
        self.validate_icon(unit_root)?;
        self.validate_repository()?;
        let mut services: Vec<&str> = Vec::with_capacity(self.provides.len());
        for service in &self.provides {
            if !is_valid_name(service) {
                return Err(format!("invalid provided service {service:?}"));
            }
            if services.contains(&service.as_str()) {
                return Err(format!("duplicate provided service {service:?}"));
            }
            services.push(service);
        }
        for dependency in &self.dependencies {
            if !is_valid_name(&dependency.addon) {
                return Err(format!("invalid dependency add-on {:?}", dependency.addon));
            }
        }
        Ok(())
    }

    fn normalize_tags(&mut self) -> Result<(), String> {
        let mut tags: Vec<String> = self
            .tags
            .iter()
            .map(|tag| tag.trim().to_lowercase())
            .filter(|tag| !tag.is_empty())
            .collect();
        tags.sort();
        tags.dedup();
        if tags.len() > MAX_TAGS {
            return Err(format!("too many tags (limit {MAX_TAGS})"));
        }
        for tag in &tags {
            let valid = tag.len() <= MAX_TAG_LEN
                && tag.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'-'
                        || byte == b'_'
                });
            if !valid {
                return Err(format!("invalid tag {tag:?}"));
            }
        }
        self.tags = tags;
        Ok(())
    }

    fn validate_fingerprint(&mut self, unit_root: &Path) -> Result<(), String> {
        if !unit_root.join(WASM_FILE_NAME).is_file() {
            return Err(format!("{WASM_FILE_NAME} not found"));
        }
        let sha256 = self.sha256.to_lowercase();
        if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("sha256 must be 64 hex characters".to_owned());
        }
        self.sha256 = sha256;
        Ok(())
    }

    /// A packed unit always declares the add-on API version its component was
    /// built against. `validate` requires it so an add-on the running framework
    /// cannot load is rejected at install time instead of inside the
    /// WebAssembly linker.
    fn validate_api_version(&mut self) -> Result<(), String> {
        let Some(declared) = self.api_version.as_deref() else {
            return Err(
                "api-version is required: the pack step stamps the add-on API version the \
                 component was built against"
                    .to_owned(),
            );
        };
        if declared.len() > MAX_API_VERSION_LEN {
            return Err(format!(
                "api-version exceeds {MAX_API_VERSION_LEN} characters"
            ));
        }
        let declared = declared.trim();
        api::parse_api_version(declared, "addon.json api-version")?;
        self.api_version = Some(declared.to_owned());
        Ok(())
    }

    fn validate_icon(&mut self, unit_root: &Path) -> Result<(), String> {
        let Some(icon) = self.icon.take() else {
            return Ok(());
        };
        if icon.trim().is_empty() {
            return Ok(());
        }
        if !is_confined_relative(&icon) {
            return Err(format!("icon escapes the add-on folder: {icon}"));
        }
        if !unit_root.join(&icon).is_file() {
            return Err(format!("icon not found: {icon}"));
        }
        self.icon = Some(icon);
        Ok(())
    }

    fn validate_repository(&mut self) -> Result<(), String> {
        let Some(repository) = self.repository.take() else {
            return Ok(());
        };
        let trimmed = repository.trim().to_owned();
        if trimmed.is_empty() {
            return Ok(());
        }
        if trimmed.len() > MAX_REPOSITORY_LEN
            || trimmed
                .chars()
                .any(|char| char.is_whitespace() || char.is_control())
        {
            return Err(format!("invalid repository {trimmed:?}"));
        }
        self.repository = Some(trimmed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn scratch() -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("farever-manifest-test-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    struct Guard(PathBuf);

    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn full_manifest() -> serde_json::Value {
        serde_json::json!({
            "manifest-version": 1,
            "id": "minimap",
            "version": "0.2.0",
            "name": "Minimap",
            "description": "A lightweight world minimap.",
            "tags": ["Overlay", "map", "map ", "", "overlay"],
            "sha256": "AB".repeat(32),
            "api-version": "1.0.0",
            "icon": "assets/icon.png",
            "repository": "https://github.com/example/minimap",
            "provides": ["poi"],
            "dependencies": [
                {
                    "addon": "poi-database",
                    "services": [{ "id": "poi", "version": "^1" }]
                }
            ],
            "future-field": {"nested": true}
        })
    }

    fn validate_manifest(value: serde_json::Value) -> Result<AddonManifest, String> {
        let root = scratch();
        let _guard = Guard(root.clone());
        std::fs::write(root.join(WASM_FILE_NAME), b"\0asm").expect("wasm");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        std::fs::write(root.join("assets").join("icon.png"), b"png").expect("icon");
        let bytes = serde_json::to_vec(&value).expect("json");
        let mut manifest = parse(&bytes)?;
        manifest.validate(&root)?;
        Ok(manifest)
    }

    #[test]
    fn full_manifest_parses_validates_and_normalizes() {
        let manifest = validate_manifest(full_manifest()).expect("valid");

        assert_eq!(manifest.manifest_version, 1);
        assert_eq!(manifest.id, "minimap");
        assert_eq!(manifest.version, "0.2.0");
        // Tags: trimmed, lowercased, deduplicated, sorted.
        assert_eq!(manifest.tags, vec!["map".to_owned(), "overlay".to_owned()]);
        // Fingerprint: case normalized. Services: names only, one each.
        assert_eq!(manifest.sha256, "ab".repeat(32));
        assert_eq!(manifest.provides, vec!["poi".to_owned()]);
        assert_eq!(manifest.icon.as_deref(), Some("assets/icon.png"));
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].addon, "poi-database");
        assert!(!manifest.dependencies[0].optional);
    }

    #[test]
    fn minimal_manifest_defaults_but_requires_fingerprint() {
        let root = scratch();
        let _guard = Guard(root.clone());
        std::fs::write(root.join(WASM_FILE_NAME), b"\0asm").expect("wasm");
        let mut manifest = parse(
            br#"{"manifest-version": 1, "id": "demo", "version": "0.1.0",
                 "sha256": "00"}"#,
        )
        .expect("parse");
        // Optional sections default to empty; only the fingerprint is short.
        assert!(manifest.tags.is_empty());
        assert!(manifest.provides.is_empty());
        assert!(manifest.dependencies.is_empty());
        assert!(manifest.icon.is_none());
        let error = manifest.validate(&root).expect_err("short hash");
        assert!(error.contains("sha256"), "{error}");
    }

    #[test]
    fn missing_version_field_fails_parse() {
        let error = parse(br#"{"id": "demo"}"#).expect_err("version");
        assert!(error.contains("manifest-version"), "{error}");
    }

    #[test]
    fn future_manifest_version_fails_validate() {
        let root = scratch();
        let _guard = Guard(root.clone());
        let mut manifest = parse(br#"{"manifest-version": 2, "id": "demo"}"#).expect("parse");
        let error = manifest.validate(&root).expect_err("version gate");
        assert!(error.contains("unsupported manifest version"), "{error}");
    }

    #[test]
    fn invalid_ids_are_rejected() {
        for id in [
            "",
            "Upper",
            "1abc",
            "has space",
            "a/b",
            "farever!",
            &"a".repeat(129),
        ] {
            let root = scratch();
            let _guard = Guard(root.clone());
            let bytes = serde_json::to_vec(&serde_json::json!({
                "manifest-version": 1, "id": id, "version": "1",
                "sha256": "00".repeat(32)
            }))
            .expect("json");
            let mut manifest = parse(&bytes).expect("parse");
            let error = manifest.validate(&root).expect_err("id");
            assert!(error.contains("invalid add-on id"), "{id}: {error}");
        }
    }

    #[test]
    fn the_component_is_always_addon_wasm() {
        // The name is not configurable, so the only way a unit can be wrong
        // is by not holding the component where every reader looks.
        let root = scratch();
        let _guard = Guard(root.clone());
        std::fs::write(root.join("minimap.wasm"), b"\0asm").expect("wasm");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "manifest-version": 1, "id": "demo", "version": "1",
            "sha256": "00".repeat(32)
        }))
        .expect("json");
        let mut manifest = parse(&bytes).expect("parse");
        let error = manifest.validate(&root).expect_err("component name");
        assert!(error.contains(WASM_FILE_NAME), "{error}");
    }

    #[test]
    fn fingerprint_rules_are_strict() {
        // A fingerprint that is short, long, or non-hex is rejected.
        for sha in ["00".repeat(31), "00".repeat(33), "zz".repeat(32)] {
            let root = scratch();
            let _guard = Guard(root.clone());
            std::fs::write(root.join(WASM_FILE_NAME), b"\0asm").expect("wasm");
            let bytes = serde_json::to_vec(&serde_json::json!({
                "manifest-version": 1, "id": "demo", "version": "1",
                "sha256": sha
            }))
            .expect("json");
            let mut manifest = parse(&bytes).expect("parse");
            let error = manifest.validate(&root).expect_err("sha");
            assert!(error.contains("sha256"), "{sha}: {error}");
        }
    }

    #[test]
    fn unit_name_prefers_the_folder_for_canonical_components() {
        // Every unit names its component `addon.wasm`, so the folder is what
        // names the add-on; a loose component keeps its own stem.
        assert_eq!(unit_name(Path::new("addons/gps/addon.wasm")), Some("gps"));
        assert_eq!(unit_name(Path::new("addons/gps/ADDON.WASM")), Some("gps"));
        assert_eq!(unit_name(Path::new("addons/dyno/addon.wasm")), Some("dyno"));
        assert_eq!(
            unit_name(Path::new("addons/legacy-arrow.wasm")),
            Some("legacy-arrow")
        );
    }

    #[test]
    fn provided_services_are_unique_names() {
        let root = scratch();
        let _guard = Guard(root.clone());
        std::fs::write(root.join(WASM_FILE_NAME), b"\0asm").expect("wasm");
        for provides in [vec!["poi", "poi"], vec!["POI"], vec![""]] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "manifest-version": 1, "id": "demo", "version": "1",
                "sha256": "00".repeat(32),
                "api-version": api::ADDON_API_VERSION,
                "provides": provides
            }))
            .expect("json");
            let mut manifest = parse(&bytes).expect("parse");
            let error = manifest.validate(&root).expect_err("provides");
            assert!(error.contains("provided service"), "{provides:?}: {error}");
        }
    }

    #[test]
    fn source_manifest_without_fingerprint_needs_pack() {
        // Source trees cannot know the hash of a future build, so they omit
        // it; distribution archives must carry it. The gate says so plainly.
        let root = scratch();
        let _guard = Guard(root.clone());
        std::fs::write(root.join(WASM_FILE_NAME), b"\0asm").expect("wasm");
        let mut manifest =
            parse(br#"{"manifest-version": 1, "id": "demo", "version": "0.1.0"}"#).expect("parse");
        let error = manifest.validate(&root).expect_err("fingerprint");
        assert!(error.contains("sha256"), "{error}");
    }

    #[test]
    fn a_packed_manifest_must_declare_the_add_on_api_version() {
        let root = scratch();
        let _guard = Guard(root.clone());
        std::fs::write(root.join(WASM_FILE_NAME), b"\0asm").expect("wasm");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "manifest-version": 1, "id": "demo", "version": "0.1.0",
            "sha256": "00".repeat(32)
        }))
        .expect("json");
        let mut manifest = parse(&bytes).expect("parse");
        let error = manifest.validate(&root).expect_err("api-version");
        assert!(error.contains("api-version is required"), "{error}");
    }

    #[test]
    fn the_declared_api_version_must_be_pinned_and_semantic() {
        let root = scratch();
        let _guard = Guard(root.clone());
        std::fs::write(root.join(WASM_FILE_NAME), b"\0asm").expect("wasm");
        for declared in ["0.15", "1.0.0-rc.1", "", " 1.0"] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "manifest-version": 1, "id": "demo", "version": "0.1.0",
                "sha256": "00".repeat(32),
                "api-version": declared
            }))
            .expect("json");
            let mut manifest = parse(&bytes).expect("parse");
            let error = manifest.validate(&root).expect_err("api-version");
            assert!(error.contains("MAJOR.MINOR.PATCH"), "{declared:?}: {error}");
        }

        // A padded value is normalized, like the fingerprint is.
        let bytes = serde_json::to_vec(&serde_json::json!({
            "manifest-version": 1, "id": "demo", "version": "0.1.0",
            "sha256": "00".repeat(32),
            "api-version": " 1.0.0 "
        }))
        .expect("json");
        let mut manifest = parse(&bytes).expect("parse");
        manifest.validate(&root).expect("normalized");
        assert_eq!(manifest.api_version.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn icon_must_stay_inside_and_exist() {
        let root = scratch();
        let _guard = Guard(root.clone());
        std::fs::write(root.join(WASM_FILE_NAME), b"\0asm").expect("wasm");
        for icon in ["../icon.png", "/icon.png", "missing.png"] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "manifest-version": 1, "id": "demo", "version": "1",
                "sha256": "00".repeat(32),
                "api-version": api::ADDON_API_VERSION,
                "icon": icon
            }))
            .expect("json");
            let mut manifest = parse(&bytes).expect("parse");
            let error = manifest.validate(&root).expect_err("icon");
            assert!(error.contains("icon"), "{icon}: {error}");
        }
    }

    #[test]
    fn text_caps_are_enforced() {
        let root = scratch();
        let _guard = Guard(root.clone());
        std::fs::write(root.join(WASM_FILE_NAME), b"\0asm").expect("wasm");
        let base = serde_json::json!({
            "manifest-version": 1, "id": "demo", "version": "1",
            "sha256": "00".repeat(32)
        });
        let mut overlong = base.clone();
        overlong["description"] = serde_json::Value::String("x".repeat(MAX_DESCRIPTION_LEN + 1));
        let mut manifest = parse(&serde_json::to_vec(&overlong).expect("json")).expect("parse");
        assert!(manifest.validate(&root).is_err());

        let mut many_tags: Vec<serde_json::Value> = (0..MAX_TAGS + 1)
            .map(|i| serde_json::Value::String(format!("t{i}")))
            .collect();
        let mut tagged = base.clone();
        tagged["tags"] = serde_json::Value::Array(std::mem::take(&mut many_tags));
        let mut manifest = parse(&serde_json::to_vec(&tagged).expect("json")).expect("parse");
        let error = manifest.validate(&root).expect_err("tags");
        assert!(error.contains("too many tags"), "{error}");

        let mut bad_tag = base.clone();
        bad_tag["tags"] = serde_json::json!(["has space"]);
        let mut manifest = parse(&serde_json::to_vec(&bad_tag).expect("json")).expect("parse");
        let error = manifest.validate(&root).expect_err("tag charset");
        assert!(error.contains("invalid tag"), "{error}");
    }
}
