use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractOptions {
    pub hlboot_path: PathBuf,
    pub farever_exe_path: Option<PathBuf>,
    pub libhl_path: Option<PathBuf>,
    pub steam_manifest_path: Option<PathBuf>,
    pub selection: ExtractionSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractionSelection {
    Focused {
        additional_namespaces: Vec<String>,
        additional_root_types: Vec<String>,
        include_direct_references: bool,
    },
    All,
}

impl Default for ExtractionSelection {
    fn default() -> Self {
        Self::Focused {
            additional_namespaces: Vec::new(),
            additional_root_types: Vec::new(),
            include_direct_references: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiSnapshot {
    pub schema_version: u32,
    pub tool_version: String,
    pub source: SourceMetadata,
    pub scope: SnapshotScope,
    pub summary: SnapshotSummary,
    pub types: Vec<TypeDefinition>,
    pub globals: Vec<GlobalDefinition>,
    pub callables: Vec<CallableDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceMetadata {
    pub bytecode_version: u8,
    pub hlboot: FileFingerprint,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub farever_exe: Option<FileFingerprint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libhl: Option<FileFingerprint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steam: Option<SteamRelease>,
    pub extracted_at_unix_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub file_name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteamRelease {
    pub app_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_updated: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotScope {
    pub selection: SnapshotSelection,
    pub includes: Vec<String>,
    pub limitations: Vec<String>,
}

impl SnapshotScope {
    pub(crate) fn new(selection: SnapshotSelection) -> Self {
        Self {
            selection,
            includes: vec![
                "named HashLink objects, structs, abstracts, and enums".to_owned(),
                "anonymous virtual record shapes".to_owned(),
                "declared field slots and types with explicit inheritance metadata".to_owned(),
                "object prototypes, function bindings, signatures, globals, and native functions"
                    .to_owned(),
            ],
            limitations: vec![
                "offline bytecode metadata contains no live values or runtime addresses".to_owned(),
                "field slots are logical HashLink indexes, not validated native byte offsets"
                    .to_owned(),
                "presence in bytecode does not prove that a type is instantiated or reachable from a stable root"
                    .to_owned(),
                "a hook still requires exact ABI and live runtime-shape validation".to_owned(),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSelection {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespace_prefixes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub root_types: Vec<String>,
    pub includes_direct_references: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSummary {
    pub type_pool_entries: usize,
    pub exported_type_definitions: usize,
    pub object_definitions: usize,
    pub struct_definitions: usize,
    pub enum_definitions: usize,
    pub abstract_definitions: usize,
    pub virtual_definitions: usize,
    pub fields: usize,
    pub methods: usize,
    pub bindings: usize,
    pub globals: usize,
    pub bytecode_functions: usize,
    pub native_functions: usize,
    pub exported_callables: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TypeKind {
    Object,
    Struct,
    Enum,
    Abstract,
    Virtual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeDefinition {
    pub id: String,
    pub kind: TypeKind,
    pub name: String,
    pub type_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub super_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_index: Option<usize>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub inherited_field_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldDefinition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<MethodDefinition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<BindingDefinition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<EnumVariantDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDefinition {
    pub slot: usize,
    pub name: String,
    pub r#type: String,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodDefinition {
    pub name: String,
    pub findex: usize,
    pub prototype_index: i32,
    pub arguments: Vec<String>,
    pub return_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingDefinition {
    pub field_slot: usize,
    pub field_name: String,
    pub findex: usize,
    pub arguments: Vec<String>,
    pub return_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumVariantDefinition {
    pub name: String,
    pub parameters: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalDefinition {
    pub index: usize,
    pub r#type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CallableKind {
    Bytecode,
    Native,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallableDefinition {
    pub id: String,
    pub kind: CallableKind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library: Option<String>,
    pub findex: usize,
    pub arguments: Vec<String>,
    pub return_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffReport {
    pub schema_version: u32,
    pub old_source_sha256: String,
    pub new_source_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_build_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_build_id: Option<String>,
    pub summary: DiffSummary,
    pub changes: Vec<DiffChange>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    pub added: usize,
    pub removed: usize,
    pub modified: usize,
}

impl DiffSummary {
    pub fn total(&self) -> usize {
        self.added + self.removed + self.modified
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffChange {
    pub change: ChangeKind,
    pub entity: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}
