//! Private, version-stamped configuration persistence for one add-on.
//!
//! This module deliberately owns only the trusted storage boundary. The WIT
//! transport and configuration-menu descriptors are layered on separately so
//! the on-disk transaction and recovery rules can be tested in isolation.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub(crate) const MAX_CONFIG_BYTES_PER_ADDON: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_CONFIG_PROPERTIES_PER_ADDON: usize = 1_024;
const MAX_CONFIG_KEY_BYTES: usize = 1_024;
const MAX_CONFIG_LABEL_BYTES: usize = 256;
const MAX_CONFIG_DESCRIPTION_BYTES: usize = 2_048;
const MAX_CONFIG_DESCRIPTOR_BYTES: usize = 64 * 1024;
const MAX_CONFIG_DEFAULT_BYTES: usize = 64 * 1024 * 1024;
const MAX_ADDON_ID_BYTES: usize = 256;
const MAX_ADDON_VERSION_BYTES: usize = 256;
const FILE_MAGIC: &[u8; 8] = b"FMCONF\0\x01";
const FILE_FORMAT_VERSION: u32 = 1;
const CHECKSUM_BYTES: usize = 32;
const MAX_CONFIG_FILE_OVERHEAD: u64 = 1_024;
const MAX_CONFIG_FILE_BYTES: u64 = MAX_CONFIG_BYTES_PER_ADDON + MAX_CONFIG_FILE_OVERHEAD;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ConfigValue {
    Boolean(bool),
    Integer(i64),
    Number(f64),
    Text(String),
    Bytes(Vec<u8>),
}

impl ConfigValue {
    pub(crate) fn kind(&self) -> ConfigValueKind {
        match self {
            Self::Boolean(_) => ConfigValueKind::Boolean,
            Self::Integer(_) => ConfigValueKind::Integer,
            Self::Number(_) => ConfigValueKind::Number,
            Self::Text(_) => ConfigValueKind::Text,
            Self::Bytes(_) => ConfigValueKind::Bytes,
        }
    }

    fn encoded_len(&self) -> usize {
        match self {
            Self::Boolean(_) => 1,
            Self::Integer(_) | Self::Number(_) => 8,
            Self::Text(value) => 4_usize.saturating_add(value.len()),
            Self::Bytes(value) => 4_usize.saturating_add(value.len()),
        }
    }

    fn validate(&self, key: &str) -> Result<(), ConfigError> {
        if matches!(self, Self::Number(value) if !value.is_finite()) {
            return Err(ConfigError::InvalidValue {
                key: key.to_owned(),
                reason: "floating-point values must be finite".to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigValueKind {
    Boolean,
    Integer,
    Number,
    Text,
    Bytes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigPropertyAccess {
    Editable,
    Readonly,
    Hidden,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConfigPropertyDescriptor {
    pub(crate) key: String,
    pub(crate) label: String,
    pub(crate) description: Option<String>,
    pub(crate) value_kind: ConfigValueKind,
    pub(crate) default_value: ConfigValue,
    pub(crate) access: ConfigPropertyAccess,
}

impl ConfigPropertyDescriptor {
    fn text_bytes(&self) -> usize {
        self.key
            .len()
            .saturating_add(self.label.len())
            .saturating_add(self.description.as_ref().map_or(0, String::len))
    }

    fn validate(&self) -> Result<(), ConfigError> {
        validate_key(&self.key)?;
        validate_descriptor_text(&self.label, MAX_CONFIG_LABEL_BYTES, "label", false)?;
        if let Some(description) = self.description.as_deref() {
            validate_descriptor_text(
                description,
                MAX_CONFIG_DESCRIPTION_BYTES,
                "description",
                true,
            )?;
        }
        let actual = self.default_value.kind();
        if actual != self.value_kind {
            return Err(ConfigError::TypeMismatch {
                key: self.key.clone(),
                expected: self.value_kind,
                actual,
            });
        }
        self.default_value.validate(&self.key)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigStatus {
    pub(crate) revision: u64,
    pub(crate) saved_by_addon_version: Option<String>,
    pub(crate) used_bytes: u64,
    pub(crate) quota_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ConfigMutation {
    Set { key: String, value: ConfigValue },
    Delete { key: String },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ConfigSnapshot {
    pub(crate) revision: u64,
    pub(crate) saved_by_addon_version: Option<String>,
    pub(crate) values: BTreeMap<String, ConfigValue>,
}

impl ConfigSnapshot {
    pub(crate) fn usage_bytes(&self) -> u64 {
        self.values.iter().fold(0_u64, |total, (key, value)| {
            total
                .saturating_add(4) // encoded key length
                .saturating_add(u64::try_from(key.len()).unwrap_or(u64::MAX))
                .saturating_add(1) // value type tag
                .saturating_add(u64::try_from(value.encoded_len()).unwrap_or(u64::MAX))
        })
    }
}

#[derive(Debug)]
pub(crate) enum ConfigError {
    InvalidAddonId(String),
    InvalidVersion(String),
    InvalidKey(String),
    InvalidValue {
        key: String,
        reason: String,
    },
    InvalidDescriptor(String),
    DuplicateProperty(String),
    UnknownProperty(String),
    TooManyProperties {
        limit: usize,
    },
    DescriptorQuotaExceeded {
        requested: usize,
        quota: usize,
    },
    DefaultQuotaExceeded {
        requested: usize,
        quota: usize,
    },
    TypeMismatch {
        key: String,
        expected: ConfigValueKind,
        actual: ConfigValueKind,
    },
    QuotaExceeded {
        requested: u64,
        quota: u64,
    },
    RevisionConflict {
        expected: u64,
        actual: u64,
    },
    RevisionExhausted,
    Corrupt {
        addon_id: String,
        details: String,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAddonId(reason) => write!(formatter, "invalid add-on ID: {reason}"),
            Self::InvalidVersion(reason) => write!(formatter, "invalid add-on version: {reason}"),
            Self::InvalidKey(reason) => write!(formatter, "invalid config key: {reason}"),
            Self::InvalidValue { key, reason } => {
                write!(formatter, "invalid config value for {key:?}: {reason}")
            }
            Self::InvalidDescriptor(reason) => {
                write!(formatter, "invalid config property descriptor: {reason}")
            }
            Self::DuplicateProperty(key) => {
                write!(formatter, "config property {key:?} is already registered")
            }
            Self::UnknownProperty(key) => {
                write!(formatter, "config property {key:?} is not registered")
            }
            Self::TooManyProperties { limit } => {
                write!(
                    formatter,
                    "add-on may register at most {limit} config properties"
                )
            }
            Self::DescriptorQuotaExceeded { requested, quota } => write!(
                formatter,
                "config property descriptors use {requested} text bytes; quota is {quota} bytes"
            ),
            Self::DefaultQuotaExceeded { requested, quota } => write!(
                formatter,
                "config property defaults use {requested} encoded bytes; quota is {quota} bytes"
            ),
            Self::TypeMismatch {
                key,
                expected,
                actual,
            } => write!(
                formatter,
                "config property {key:?} expects {expected:?}, received {actual:?}"
            ),
            Self::QuotaExceeded { requested, quota } => write!(
                formatter,
                "configuration uses {requested} bytes; quota is {quota} bytes"
            ),
            Self::RevisionConflict { expected, actual } => write!(
                formatter,
                "configuration revision conflict: expected {expected}, actual {actual}"
            ),
            Self::RevisionExhausted => write!(formatter, "configuration revision is exhausted"),
            Self::Corrupt { addon_id, details } => {
                write!(
                    formatter,
                    "configuration for {addon_id:?} is corrupt: {details}"
                )
            }
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "configuration {operation} failed for {}: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct ConfigPaths {
    root: PathBuf,
    current: PathBuf,
    pending: PathBuf,
    previous: PathBuf,
}

impl ConfigPaths {
    fn new(root: &Path, addon_id: &str) -> Self {
        let digest = Sha256::digest(addon_id.as_bytes());
        let stem = hex::encode(digest);
        Self {
            root: root.to_owned(),
            current: root.join(format!("{stem}.fmc")),
            pending: root.join(format!("{stem}.pending")),
            previous: root.join(format!("{stem}.previous")),
        }
    }

    fn candidates(&self) -> [&Path; 3] {
        [&self.previous, &self.pending, &self.current]
    }
}

pub(crate) struct ConfigStore {
    addon_id: String,
    current_addon_version: Option<String>,
    paths: ConfigPaths,
    quota_bytes: u64,
    snapshot: ConfigSnapshot,
}

pub(crate) struct ConfigRegistry {
    store: ConfigStore,
    properties: Vec<ConfigPropertyDescriptor>,
    descriptor_bytes: usize,
    default_bytes: usize,
}

impl ConfigRegistry {
    pub(crate) fn open(
        root: &Path,
        addon_id: &str,
        current_addon_version: Option<&str>,
    ) -> Result<Self, ConfigError> {
        Ok(Self {
            store: ConfigStore::open(root, addon_id, current_addon_version)?,
            properties: Vec::new(),
            descriptor_bytes: 0,
            default_bytes: 0,
        })
    }

    pub(crate) fn register(
        &mut self,
        descriptor: ConfigPropertyDescriptor,
    ) -> Result<ConfigValue, ConfigError> {
        descriptor.validate()?;
        if self
            .properties
            .iter()
            .any(|registered| registered.key == descriptor.key)
        {
            return Err(ConfigError::DuplicateProperty(descriptor.key));
        }
        if self.properties.len() >= MAX_CONFIG_PROPERTIES_PER_ADDON {
            return Err(ConfigError::TooManyProperties {
                limit: MAX_CONFIG_PROPERTIES_PER_ADDON,
            });
        }
        let descriptor_bytes = self
            .descriptor_bytes
            .saturating_add(descriptor.text_bytes());
        if descriptor_bytes > MAX_CONFIG_DESCRIPTOR_BYTES {
            return Err(ConfigError::DescriptorQuotaExceeded {
                requested: descriptor_bytes,
                quota: MAX_CONFIG_DESCRIPTOR_BYTES,
            });
        }
        let default_bytes = self
            .default_bytes
            .saturating_add(descriptor.default_value.encoded_len());
        if default_bytes > MAX_CONFIG_DEFAULT_BYTES {
            return Err(ConfigError::DefaultQuotaExceeded {
                requested: default_bytes,
                quota: MAX_CONFIG_DEFAULT_BYTES,
            });
        }
        let value = self
            .store
            .snapshot()
            .values
            .get(&descriptor.key)
            .cloned()
            .unwrap_or_else(|| descriptor.default_value.clone());
        self.properties.push(descriptor);
        self.descriptor_bytes = descriptor_bytes;
        self.default_bytes = default_bytes;
        Ok(value)
    }

    pub(crate) fn properties(&self) -> &[ConfigPropertyDescriptor] {
        &self.properties
    }

    pub(crate) fn status(&self) -> ConfigStatus {
        ConfigStatus {
            revision: self.store.snapshot().revision,
            saved_by_addon_version: self.store.snapshot().saved_by_addon_version.clone(),
            used_bytes: self.store.snapshot().usage_bytes(),
            quota_bytes: self.store.quota_bytes(),
        }
    }

    pub(crate) fn get(&self, key: &str) -> Result<ConfigValue, ConfigError> {
        let property = self.property(key)?;
        Ok(self
            .store
            .snapshot()
            .values
            .get(key)
            .cloned()
            .unwrap_or_else(|| property.default_value.clone()))
    }

    pub(crate) fn set(
        &mut self,
        key: String,
        value: ConfigValue,
    ) -> Result<ConfigStatus, ConfigError> {
        let expected = self.property(&key)?.value_kind;
        let actual = value.kind();
        if actual != expected {
            return Err(ConfigError::TypeMismatch {
                key,
                expected,
                actual,
            });
        }
        if self.store.snapshot().values.get(&key) == Some(&value) {
            return Ok(self.status());
        }
        let revision = self.store.snapshot().revision;
        self.store
            .commit(revision, [ConfigMutation::Set { key, value }])?;
        Ok(self.status())
    }

    pub(crate) fn remove(&mut self, key: String) -> Result<ConfigStatus, ConfigError> {
        self.property(&key)?;
        if !self.store.snapshot().values.contains_key(&key) {
            return Ok(self.status());
        }
        let revision = self.store.snapshot().revision;
        self.store
            .commit(revision, [ConfigMutation::Delete { key }])?;
        Ok(self.status())
    }

    fn property(&self, key: &str) -> Result<&ConfigPropertyDescriptor, ConfigError> {
        self.properties
            .iter()
            .find(|descriptor| descriptor.key == key)
            .ok_or_else(|| ConfigError::UnknownProperty(key.to_owned()))
    }
}

impl ConfigStore {
    pub(crate) fn open(
        root: &Path,
        addon_id: &str,
        current_addon_version: Option<&str>,
    ) -> Result<Self, ConfigError> {
        Self::open_with_quota(
            root,
            addon_id,
            current_addon_version,
            MAX_CONFIG_BYTES_PER_ADDON,
        )
    }

    fn open_with_quota(
        root: &Path,
        addon_id: &str,
        current_addon_version: Option<&str>,
        quota_bytes: u64,
    ) -> Result<Self, ConfigError> {
        validate_addon_id(addon_id)?;
        let current_addon_version = current_addon_version
            .map(validate_addon_version)
            .transpose()?
            .map(str::to_owned);
        let paths = ConfigPaths::new(root, addon_id);
        let snapshot = load_latest(&paths, addon_id)?.unwrap_or_default();
        validate_snapshot(&snapshot, quota_bytes)?;
        Ok(Self {
            addon_id: addon_id.to_owned(),
            current_addon_version,
            paths,
            quota_bytes,
            snapshot,
        })
    }

    pub(crate) fn snapshot(&self) -> &ConfigSnapshot {
        &self.snapshot
    }

    pub(crate) fn quota_bytes(&self) -> u64 {
        self.quota_bytes
    }

    pub(crate) fn commit(
        &mut self,
        expected_revision: u64,
        mutations: impl IntoIterator<Item = ConfigMutation>,
    ) -> Result<&ConfigSnapshot, ConfigError> {
        if let Some(latest) = load_latest(&self.paths, &self.addon_id)? {
            if latest.revision != self.snapshot.revision {
                self.snapshot = latest;
            }
        }
        if expected_revision != self.snapshot.revision {
            return Err(ConfigError::RevisionConflict {
                expected: expected_revision,
                actual: self.snapshot.revision,
            });
        }

        let mut values = self.snapshot.values.clone();
        for mutation in mutations {
            match mutation {
                ConfigMutation::Set { key, value } => {
                    validate_key(&key)?;
                    value.validate(&key)?;
                    values.insert(key, value);
                }
                ConfigMutation::Delete { key } => {
                    validate_key(&key)?;
                    values.remove(&key);
                }
            }
        }
        let next = ConfigSnapshot {
            revision: self
                .snapshot
                .revision
                .checked_add(1)
                .ok_or(ConfigError::RevisionExhausted)?,
            saved_by_addon_version: self.current_addon_version.clone(),
            values,
        };
        validate_snapshot(&next, self.quota_bytes)?;
        let bytes = encode_snapshot(&self.addon_id, &next)?;
        persist(&self.paths, &bytes)?;
        self.snapshot = next;
        Ok(&self.snapshot)
    }
}

fn validate_addon_id(addon_id: &str) -> Result<(), ConfigError> {
    if addon_id.is_empty() {
        return Err(ConfigError::InvalidAddonId("must not be empty".to_owned()));
    }
    if addon_id.len() > MAX_ADDON_ID_BYTES {
        return Err(ConfigError::InvalidAddonId(format!(
            "must not exceed {MAX_ADDON_ID_BYTES} UTF-8 bytes"
        )));
    }
    if addon_id.chars().any(char::is_control) {
        return Err(ConfigError::InvalidAddonId(
            "must not contain control characters".to_owned(),
        ));
    }
    Ok(())
}

fn validate_addon_version(version: &str) -> Result<&str, ConfigError> {
    if version.is_empty() {
        return Err(ConfigError::InvalidVersion("must not be empty".to_owned()));
    }
    if version.len() > MAX_ADDON_VERSION_BYTES {
        return Err(ConfigError::InvalidVersion(format!(
            "must not exceed {MAX_ADDON_VERSION_BYTES} UTF-8 bytes"
        )));
    }
    if version.chars().any(char::is_control) {
        return Err(ConfigError::InvalidVersion(
            "must not contain control characters".to_owned(),
        ));
    }
    Ok(version)
}

fn validate_key(key: &str) -> Result<(), ConfigError> {
    if key.is_empty() {
        return Err(ConfigError::InvalidKey("must not be empty".to_owned()));
    }
    if key.len() > MAX_CONFIG_KEY_BYTES {
        return Err(ConfigError::InvalidKey(format!(
            "must not exceed {MAX_CONFIG_KEY_BYTES} UTF-8 bytes"
        )));
    }
    if key.chars().any(char::is_control) {
        return Err(ConfigError::InvalidKey(
            "must not contain control characters".to_owned(),
        ));
    }
    Ok(())
}

fn validate_descriptor_text(
    value: &str,
    maximum_bytes: usize,
    field: &str,
    allow_empty: bool,
) -> Result<(), ConfigError> {
    if !allow_empty && value.is_empty() {
        return Err(ConfigError::InvalidDescriptor(format!(
            "{field} must not be empty"
        )));
    }
    if value.len() > maximum_bytes {
        return Err(ConfigError::InvalidDescriptor(format!(
            "{field} must not exceed {maximum_bytes} UTF-8 bytes"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(ConfigError::InvalidDescriptor(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

fn validate_snapshot(snapshot: &ConfigSnapshot, quota: u64) -> Result<(), ConfigError> {
    for (key, value) in &snapshot.values {
        validate_key(key)?;
        value.validate(key)?;
    }
    let requested = snapshot.usage_bytes();
    if requested > quota {
        return Err(ConfigError::QuotaExceeded { requested, quota });
    }
    if let Some(version) = snapshot.saved_by_addon_version.as_deref() {
        validate_addon_version(version)?;
    }
    Ok(())
}

fn load_latest(
    paths: &ConfigPaths,
    expected_addon_id: &str,
) -> Result<Option<ConfigSnapshot>, ConfigError> {
    let mut found = false;
    let mut valid = Vec::new();
    let mut corrupt = Vec::new();
    for path in paths.candidates() {
        let bytes = match read_candidate(path) {
            Ok(bytes) => {
                found = true;
                bytes
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                found = true;
                corrupt.push(format!("{}: {error}", path.display()));
                continue;
            }
            Err(source) => {
                return Err(ConfigError::Io {
                    operation: "read",
                    path: path.to_owned(),
                    source,
                });
            }
        };
        match decode_snapshot(&bytes, expected_addon_id) {
            Ok(snapshot) => valid.push(snapshot),
            Err(error) => corrupt.push(format!("{}: {error}", path.display())),
        }
    }
    if let Some(snapshot) = valid.into_iter().max_by_key(|snapshot| snapshot.revision) {
        return Ok(Some(snapshot));
    }
    if found {
        return Err(ConfigError::Corrupt {
            addon_id: expected_addon_id.to_owned(),
            details: corrupt.join("; "),
        });
    }
    Ok(None)
}

fn read_candidate(path: &Path) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CONFIG_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file exceeds the maximum encoded size of {MAX_CONFIG_FILE_BYTES} bytes"),
        ));
    }
    Ok(bytes)
}

fn persist(paths: &ConfigPaths, bytes: &[u8]) -> Result<(), ConfigError> {
    fs::create_dir_all(&paths.root).map_err(|source| ConfigError::Io {
        operation: "create directory",
        path: paths.root.clone(),
        source,
    })?;
    let mut pending = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&paths.pending)
        .map_err(|source| ConfigError::Io {
            operation: "open pending file",
            path: paths.pending.clone(),
            source,
        })?;
    pending
        .write_all(bytes)
        .and_then(|()| pending.sync_all())
        .map_err(|source| ConfigError::Io {
            operation: "write pending file",
            path: paths.pending.clone(),
            source,
        })?;
    drop(pending);

    if paths.previous.exists() {
        fs::remove_file(&paths.previous).map_err(|source| ConfigError::Io {
            operation: "remove stale previous file",
            path: paths.previous.clone(),
            source,
        })?;
    }
    let moved_current = if paths.current.exists() {
        fs::rename(&paths.current, &paths.previous).map_err(|source| ConfigError::Io {
            operation: "rotate current file",
            path: paths.current.clone(),
            source,
        })?;
        true
    } else {
        false
    };
    if let Err(source) = fs::rename(&paths.pending, &paths.current) {
        if moved_current {
            let _ = fs::rename(&paths.previous, &paths.current);
        }
        return Err(ConfigError::Io {
            operation: "promote pending file",
            path: paths.pending.clone(),
            source,
        });
    }
    Ok(())
}

fn encode_snapshot(addon_id: &str, snapshot: &ConfigSnapshot) -> Result<Vec<u8>, ConfigError> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(FILE_MAGIC);
    put_u32(&mut bytes, FILE_FORMAT_VERSION);
    put_u64(&mut bytes, snapshot.revision);
    put_string(&mut bytes, addon_id)?;
    match snapshot.saved_by_addon_version.as_deref() {
        Some(version) => {
            bytes.push(1);
            put_string(&mut bytes, version)?;
        }
        None => bytes.push(0),
    }
    put_u32(
        &mut bytes,
        u32::try_from(snapshot.values.len()).map_err(|_| ConfigError::QuotaExceeded {
            requested: u64::MAX,
            quota: MAX_CONFIG_BYTES_PER_ADDON,
        })?,
    );
    for (key, value) in &snapshot.values {
        put_string(&mut bytes, key)?;
        match value {
            ConfigValue::Boolean(value) => {
                bytes.push(0);
                bytes.push(u8::from(*value));
            }
            ConfigValue::Integer(value) => {
                bytes.push(1);
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            ConfigValue::Number(value) => {
                bytes.push(2);
                bytes.extend_from_slice(&value.to_bits().to_le_bytes());
            }
            ConfigValue::Text(value) => {
                bytes.push(3);
                put_bytes(&mut bytes, value.as_bytes())?;
            }
            ConfigValue::Bytes(value) => {
                bytes.push(4);
                put_bytes(&mut bytes, value)?;
            }
        }
    }
    let checksum = Sha256::digest(&bytes);
    bytes.extend_from_slice(&checksum);
    Ok(bytes)
}

fn decode_snapshot(bytes: &[u8], expected_addon_id: &str) -> Result<ConfigSnapshot, String> {
    if bytes.len() < FILE_MAGIC.len() + CHECKSUM_BYTES {
        return Err("file is truncated".to_owned());
    }
    let (payload, checksum) = bytes.split_at(bytes.len() - CHECKSUM_BYTES);
    let actual_checksum = Sha256::digest(payload);
    if actual_checksum[..] != *checksum {
        return Err("checksum mismatch".to_owned());
    }
    let mut decoder = Decoder::new(payload);
    if decoder.take(FILE_MAGIC.len())? != FILE_MAGIC {
        return Err("unrecognized file magic".to_owned());
    }
    let format = decoder.u32()?;
    if format != FILE_FORMAT_VERSION {
        return Err(format!("unsupported file format {format}"));
    }
    let revision = decoder.u64()?;
    let addon_id = decoder.string()?;
    if addon_id != expected_addon_id {
        return Err(format!(
            "add-on ID mismatch: expected {expected_addon_id:?}, found {addon_id:?}"
        ));
    }
    let saved_by_addon_version = match decoder.u8()? {
        0 => None,
        1 => Some(decoder.string()?.to_owned()),
        marker => return Err(format!("invalid version marker {marker}")),
    };
    let count = usize::try_from(decoder.u32()?).map_err(|_| "entry count overflow")?;
    let mut values = BTreeMap::new();
    for _ in 0..count {
        let key = decoder.string()?.to_owned();
        let value = match decoder.u8()? {
            0 => match decoder.u8()? {
                0 => ConfigValue::Boolean(false),
                1 => ConfigValue::Boolean(true),
                marker => return Err(format!("invalid boolean marker {marker}")),
            },
            1 => ConfigValue::Integer(decoder.i64()?),
            2 => ConfigValue::Number(f64::from_bits(decoder.u64()?)),
            3 => ConfigValue::Text(
                std::str::from_utf8(decoder.bytes()?)
                    .map_err(|error| format!("config text is not UTF-8: {error}"))?
                    .to_owned(),
            ),
            4 => ConfigValue::Bytes(decoder.bytes()?.to_vec()),
            tag => return Err(format!("unknown config value tag {tag}")),
        };
        validate_key(&key).map_err(|error| error.to_string())?;
        value.validate(&key).map_err(|error| error.to_string())?;
        if values.insert(key.clone(), value).is_some() {
            return Err(format!("duplicate config key {key:?}"));
        }
    }
    if !decoder.is_empty() {
        return Err("trailing bytes after config entries".to_owned());
    }
    let snapshot = ConfigSnapshot {
        revision,
        saved_by_addon_version,
        values,
    };
    validate_snapshot(&snapshot, MAX_CONFIG_BYTES_PER_ADDON).map_err(|error| error.to_string())?;
    Ok(snapshot)
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_string(output: &mut Vec<u8>, value: &str) -> Result<(), ConfigError> {
    put_bytes(output, value.as_bytes())
}

fn put_bytes(output: &mut Vec<u8>, value: &[u8]) -> Result<(), ConfigError> {
    put_u32(
        output,
        u32::try_from(value.len()).map_err(|_| ConfigError::QuotaExceeded {
            requested: u64::MAX,
            quota: MAX_CONFIG_BYTES_PER_ADDON,
        })?,
    );
    output.extend_from_slice(value);
    Ok(())
}

struct Decoder<'a> {
    remaining: &'a [u8],
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        if self.remaining.len() < length {
            return Err("file is truncated".to_owned());
        }
        let (value, remaining) = self.remaining.split_at(length);
        self.remaining = remaining;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        let bytes = self.take(4)?.try_into().map_err(|_| "invalid u32")?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let bytes = self.take(8)?.try_into().map_err(|_| "invalid u64")?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn i64(&mut self) -> Result<i64, String> {
        let bytes = self.take(8)?.try_into().map_err(|_| "invalid i64")?;
        Ok(i64::from_le_bytes(bytes))
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let length = usize::try_from(self.u32()?).map_err(|_| "length overflow")?;
        self.take(length)
    }

    fn string(&mut self) -> Result<&'a str, String> {
        std::str::from_utf8(self.bytes()?).map_err(|error| format!("invalid UTF-8: {error}"))
    }

    fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "farever-config-test-{}-{label}-{serial}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create isolated config test directory");
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn set(key: &str, value: ConfigValue) -> ConfigMutation {
        ConfigMutation::Set {
            key: key.to_owned(),
            value,
        }
    }

    fn descriptor(
        key: &str,
        value_kind: ConfigValueKind,
        access: ConfigPropertyAccess,
    ) -> ConfigPropertyDescriptor {
        let default_value = match value_kind {
            ConfigValueKind::Boolean => ConfigValue::Boolean(false),
            ConfigValueKind::Integer => ConfigValue::Integer(0),
            ConfigValueKind::Number => ConfigValue::Number(0.0),
            ConfigValueKind::Text => ConfigValue::Text(String::new()),
            ConfigValueKind::Bytes => ConfigValue::Bytes(Vec::new()),
        };
        ConfigPropertyDescriptor {
            key: key.to_owned(),
            label: key.to_owned(),
            description: None,
            value_kind,
            default_value,
            access,
        }
    }

    #[test]
    fn commit_round_trips_values_and_host_stamped_version() {
        let root = TempRoot::new("round-trip");
        let mut store =
            ConfigStore::open(&root.0, "dyno", Some("1.2.3")).expect("open empty store");
        assert_eq!(store.snapshot(), &ConfigSnapshot::default());

        let committed = store
            .commit(
                0,
                [
                    set("visible", ConfigValue::Boolean(false)),
                    set("scale", ConfigValue::Number(1.25)),
                    set("layout", ConfigValue::Bytes(vec![1, 2, 3])),
                    set("title", ConfigValue::Text("Damage".to_owned())),
                    set("rows", ConfigValue::Integer(30)),
                ],
            )
            .expect("commit config")
            .clone();
        assert_eq!(committed.revision, 1);
        assert_eq!(committed.saved_by_addon_version.as_deref(), Some("1.2.3"));

        let reopened = ConfigStore::open(&root.0, "dyno", Some("2.0.0")).expect("reopen config");
        assert_eq!(reopened.snapshot(), &committed);
        assert_eq!(
            reopened.snapshot().saved_by_addon_version.as_deref(),
            Some("1.2.3"),
            "opening under a new add-on version does not rewrite provenance"
        );
    }

    #[test]
    fn one_value_may_consume_the_entire_remaining_quota() {
        let root = TempRoot::new("quota");
        let mut store = ConfigStore::open_with_quota(&root.0, "addon", Some("1.0.0"), 64)
            .expect("open small test store");

        store
            .commit(0, [set("x", ConfigValue::Bytes(vec![0; 54]))])
            .expect("one entry exactly fills the aggregate encoded-data quota");
        let error = store
            .commit(1, [set("x", ConfigValue::Bytes(vec![0; 55]))])
            .expect_err("aggregate quota must still apply");
        assert!(matches!(
            error,
            ConfigError::QuotaExceeded {
                requested: 65,
                quota: 64
            }
        ));
    }

    #[test]
    fn commits_are_atomic_and_revision_checked_across_store_instances() {
        let root = TempRoot::new("conflict");
        let mut first = ConfigStore::open(&root.0, "addon", Some("1.0.0")).expect("first store");
        let mut stale = ConfigStore::open(&root.0, "addon", Some("1.0.0")).expect("stale store");

        first
            .commit(0, [set("value", ConfigValue::Integer(1))])
            .expect("first commit");
        let error = stale
            .commit(0, [set("value", ConfigValue::Integer(2))])
            .expect_err("stale revision must conflict");
        assert!(matches!(
            error,
            ConfigError::RevisionConflict {
                expected: 0,
                actual: 1
            }
        ));
        assert_eq!(
            stale.snapshot().values.get("value"),
            Some(&ConfigValue::Integer(1))
        );
    }

    #[test]
    fn delete_is_persisted_as_part_of_the_same_revision() {
        let root = TempRoot::new("delete");
        let mut store = ConfigStore::open(&root.0, "addon", Some("1.0.0")).expect("store");
        store
            .commit(0, [set("old", ConfigValue::Boolean(true))])
            .expect("seed");
        store
            .commit(
                1,
                [
                    ConfigMutation::Delete {
                        key: "old".to_owned(),
                    },
                    set("new", ConfigValue::Boolean(false)),
                ],
            )
            .expect("replace keys");

        let reopened = ConfigStore::open(&root.0, "addon", Some("1.0.0")).expect("reopen");
        assert!(!reopened.snapshot().values.contains_key("old"));
        assert_eq!(
            reopened.snapshot().values.get("new"),
            Some(&ConfigValue::Boolean(false))
        );
    }

    #[test]
    fn valid_previous_revision_recovers_a_corrupt_current_file() {
        let root = TempRoot::new("recovery");
        let mut store = ConfigStore::open(&root.0, "addon", Some("1.0.0")).expect("store");
        store
            .commit(0, [set("value", ConfigValue::Integer(1))])
            .expect("revision one");
        store
            .commit(1, [set("value", ConfigValue::Integer(2))])
            .expect("revision two");
        fs::write(&store.paths.current, b"torn write").expect("corrupt current fixture");

        let recovered = ConfigStore::open(&root.0, "addon", Some("2.0.0"))
            .expect("previous valid revision should recover");
        assert_eq!(recovered.snapshot().revision, 1);
        assert_eq!(
            recovered.snapshot().values.get("value"),
            Some(&ConfigValue::Integer(1))
        );
    }

    #[test]
    fn corrupt_state_is_reported_and_preserved_when_no_revision_is_valid() {
        let root = TempRoot::new("corrupt");
        let paths = ConfigPaths::new(&root.0, "addon");
        fs::write(&paths.current, b"not a config file").expect("write corrupt fixture");

        let error = ConfigStore::open(&root.0, "addon", Some("1.0.0"))
            .err()
            .expect("corrupt store must fail");
        assert!(matches!(error, ConfigError::Corrupt { .. }));
        assert_eq!(
            fs::read(&paths.current).expect("corrupt data remains available"),
            b"not a config file"
        );
    }

    #[test]
    fn non_finite_numbers_are_rejected_without_writing() {
        let root = TempRoot::new("non-finite");
        let mut store = ConfigStore::open(&root.0, "addon", Some("1.0.0")).expect("store");

        let error = store
            .commit(0, [set("scale", ConfigValue::Number(f64::NAN))])
            .expect_err("NaN must be rejected");
        assert!(matches!(error, ConfigError::InvalidValue { .. }));
        assert_eq!(store.snapshot(), &ConfigSnapshot::default());
        assert!(!store.paths.current.exists());
    }

    #[test]
    fn registry_preserves_registration_order_and_presentation_access() {
        let root = TempRoot::new("registry-order");
        let mut registry = ConfigRegistry::open(&root.0, "addon", Some("1.0.0")).expect("registry");

        registry
            .register(descriptor(
                "editable",
                ConfigValueKind::Boolean,
                ConfigPropertyAccess::Editable,
            ))
            .expect("editable property");
        registry
            .register(descriptor(
                "readonly",
                ConfigValueKind::Text,
                ConfigPropertyAccess::Readonly,
            ))
            .expect("readonly property");
        registry
            .register(descriptor(
                "hidden",
                ConfigValueKind::Bytes,
                ConfigPropertyAccess::Hidden,
            ))
            .expect("hidden property");

        assert_eq!(
            registry
                .properties()
                .iter()
                .map(|property| (property.key.as_str(), property.access))
                .collect::<Vec<_>>(),
            [
                ("editable", ConfigPropertyAccess::Editable),
                ("readonly", ConfigPropertyAccess::Readonly),
                ("hidden", ConfigPropertyAccess::Hidden),
            ]
        );
    }

    #[test]
    fn registry_requires_unique_declared_properties_and_matching_types() {
        let root = TempRoot::new("registry-schema");
        let mut registry = ConfigRegistry::open(&root.0, "addon", Some("1.0.0")).expect("registry");
        let property = descriptor(
            "enabled",
            ConfigValueKind::Boolean,
            ConfigPropertyAccess::Editable,
        );
        registry
            .register(property.clone())
            .expect("first registration");

        assert!(matches!(
            registry.register(property),
            Err(ConfigError::DuplicateProperty(key)) if key == "enabled"
        ));
        assert!(matches!(
            registry.set("enabled".to_owned(), ConfigValue::Integer(1)),
            Err(ConfigError::TypeMismatch {
                expected: ConfigValueKind::Boolean,
                actual: ConfigValueKind::Integer,
                ..
            })
        ));
        assert!(matches!(
            registry.get("undeclared"),
            Err(ConfigError::UnknownProperty(key)) if key == "undeclared"
        ));
    }

    #[test]
    fn registered_defaults_resolve_without_becoming_persisted_overrides() {
        let root = TempRoot::new("registry-default");
        let mut registry = ConfigRegistry::open(&root.0, "addon", Some("1.0.0")).expect("registry");
        let mut property = descriptor(
            "enabled",
            ConfigValueKind::Boolean,
            ConfigPropertyAccess::Editable,
        );
        property.default_value = ConfigValue::Boolean(true);

        assert_eq!(
            registry.register(property).expect("register property"),
            ConfigValue::Boolean(true)
        );
        assert_eq!(registry.get("enabled").unwrap(), ConfigValue::Boolean(true));
        assert_eq!(registry.status().revision, 0);
        assert_eq!(registry.status().used_bytes, 0);

        registry
            .set("enabled".to_owned(), ConfigValue::Boolean(false))
            .expect("store override");
        assert_eq!(
            registry.get("enabled").unwrap(),
            ConfigValue::Boolean(false)
        );
        assert!(registry.status().used_bytes > 0);

        registry
            .remove("enabled".to_owned())
            .expect("remove override");
        assert_eq!(registry.get("enabled").unwrap(), ConfigValue::Boolean(true));
        assert_eq!(registry.status().used_bytes, 0);
    }

    #[test]
    fn property_defaults_must_match_the_declared_type_and_be_finite() {
        let root = TempRoot::new("registry-invalid-default");
        let mut registry = ConfigRegistry::open(&root.0, "addon", Some("1.0.0")).expect("registry");
        let mut wrong_type = descriptor(
            "enabled",
            ConfigValueKind::Boolean,
            ConfigPropertyAccess::Editable,
        );
        wrong_type.default_value = ConfigValue::Integer(1);
        assert!(matches!(
            registry.register(wrong_type),
            Err(ConfigError::TypeMismatch { .. })
        ));

        let mut not_finite = descriptor(
            "scale",
            ConfigValueKind::Number,
            ConfigPropertyAccess::Editable,
        );
        not_finite.default_value = ConfigValue::Number(f64::NAN);
        assert!(matches!(
            registry.register(not_finite),
            Err(ConfigError::InvalidValue { .. })
        ));
    }

    #[test]
    fn registry_writes_readonly_and_hidden_values_for_addon_owned_state() {
        let root = TempRoot::new("registry-access");
        let mut registry = ConfigRegistry::open(&root.0, "addon", Some("3.2.1")).expect("registry");
        registry
            .register(descriptor(
                "detected-path",
                ConfigValueKind::Text,
                ConfigPropertyAccess::Readonly,
            ))
            .expect("readonly property");
        registry
            .register(descriptor(
                "cache",
                ConfigValueKind::Bytes,
                ConfigPropertyAccess::Hidden,
            ))
            .expect("hidden property");

        let first = registry
            .set(
                "detected-path".to_owned(),
                ConfigValue::Text("C:/Farever".to_owned()),
            )
            .expect("the owning addon may update readonly presentation data");
        assert_eq!(first.revision, 1);
        let second = registry
            .set("cache".to_owned(), ConfigValue::Bytes(vec![1, 2, 3]))
            .expect("the owning addon may update hidden data");
        assert_eq!(second.revision, 2);
        assert_eq!(second.saved_by_addon_version.as_deref(), Some("3.2.1"));

        let mut reopened = ConfigRegistry::open(&root.0, "addon", Some("4.0.0")).expect("reopen");
        let detected = reopened
            .register(descriptor(
                "detected-path",
                ConfigValueKind::Text,
                ConfigPropertyAccess::Readonly,
            ))
            .expect("register after upgrade");
        assert_eq!(detected, ConfigValue::Text("C:/Farever".to_owned()));
        assert_eq!(
            reopened.status().saved_by_addon_version.as_deref(),
            Some("3.2.1"),
            "opening under a new component version does not rewrite provenance"
        );
    }

    #[test]
    fn unchanged_sets_and_missing_removals_do_not_create_revisions() {
        let root = TempRoot::new("registry-noop");
        let mut registry = ConfigRegistry::open(&root.0, "addon", Some("1.0.0")).expect("registry");
        registry
            .register(descriptor(
                "enabled",
                ConfigValueKind::Boolean,
                ConfigPropertyAccess::Editable,
            ))
            .expect("property");

        assert_eq!(registry.remove("enabled".to_owned()).unwrap().revision, 0);
        assert_eq!(
            registry
                .set("enabled".to_owned(), ConfigValue::Boolean(true))
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            registry
                .set("enabled".to_owned(), ConfigValue::Boolean(true))
                .unwrap()
                .revision,
            1
        );
    }
}
