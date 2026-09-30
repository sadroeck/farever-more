use crate::model::{
    ApiSnapshot, BindingDefinition, CallableDefinition, CallableKind, EnumVariantDefinition,
    ExtractOptions, ExtractionSelection, FieldDefinition, FileFingerprint, GlobalDefinition,
    MethodDefinition, SnapshotScope, SnapshotSelection, SnapshotSummary, SourceMetadata,
    SteamRelease, TypeDefinition, TypeKind, SNAPSHOT_SCHEMA_VERSION,
};
use hlbc::types::{RefFun, RefString, RefType, Type, TypeFun, TypeObj};
use hlbc::{Bytecode, Resolve};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_NAMESPACES: &[&str] = &[];
const DEFAULT_ROOT_TYPES: &[&str] = &[
    "App",
    "GameApp",
    "String",
    "ent.GameObject",
    "ent.Hero",
    "ent.Unit",
    "ent.UnitAttributes",
    "h2d.Object",
    "h2d.Scene",
    "hl.types.ArrayObj",
    "hxbit.MapData",
    "st.Activity",
    "st.Channel",
    "st.Equipment",
    "st.GameLayer",
    "st.Inventory",
    "st.Loadout",
    "st.Player",
    "st.item.Armor",
    "st.item.Gear",
    "st.item.Weapon",
    "st.skill.BaseSkill",
    "st.skill.DamageResult",
    "st.skill.SkillContext",
    "ui.BaseUI",
    "ui.GameUI",
    "ui.UIElement",
    "ui.hud.ChatBox",
    "ui.win.BaseWindow",
];

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse HashLink bytecode from {path}: {source}")]
    Bytecode {
        path: PathBuf,
        #[source]
        source: hlbc::Error,
    },
}

pub fn extract_game_directory(
    game_directory: impl AsRef<Path>,
    selection: ExtractionSelection,
) -> Result<ApiSnapshot, ExtractError> {
    let game_directory = game_directory.as_ref();
    if !game_directory.is_dir() {
        return Err(ExtractError::InvalidInput(format!(
            "game directory does not exist: {}",
            game_directory.display()
        )));
    }

    let steam_manifest_path = game_directory
        .parent()
        .and_then(Path::parent)
        .map(|steamapps| steamapps.join("appmanifest_3672400.acf"))
        .filter(|path| path.is_file());

    extract_hlboot(ExtractOptions {
        hlboot_path: game_directory.join("hlboot.dat"),
        farever_exe_path: optional_file(game_directory.join("Farever.exe")),
        libhl_path: optional_file(game_directory.join("libhl.dll")),
        steam_manifest_path,
        selection,
    })
}

pub fn extract_hlboot(options: ExtractOptions) -> Result<ApiSnapshot, ExtractError> {
    validate_hlboot_header(&options.hlboot_path)?;
    let code =
        Bytecode::from_file(&options.hlboot_path).map_err(|source| ExtractError::Bytecode {
            path: options.hlboot_path.clone(),
            source,
        })?;

    let signatures = SignatureIndex::new(&code);
    let all_types = extract_types(&code, &signatures);
    let (mut types, snapshot_selection) = select_types(all_types, &options.selection)?;
    assign_unique_type_ids(&mut types);
    types.sort_by(|left, right| left.id.cmp(&right.id));

    let selected_type_names = types
        .iter()
        .map(|definition| definition.name.clone())
        .collect::<HashSet<_>>();
    let selected_global_indexes = types
        .iter()
        .filter_map(|definition| definition.global_index)
        .collect::<HashSet<_>>();
    let globals = code
        .globals
        .iter()
        .enumerate()
        .map(|(index, ty)| GlobalDefinition {
            index,
            r#type: format_type_ref(&code, *ty),
        })
        .filter(|global| {
            matches!(options.selection, ExtractionSelection::All)
                || selected_global_indexes.contains(&global.index)
                || type_expression_mentions(&global.r#type, &selected_type_names)
        })
        .collect::<Vec<_>>();

    let mut callables = extract_callables(&code);
    if !matches!(options.selection, ExtractionSelection::All) {
        let selected_findexes = types
            .iter()
            .flat_map(|definition| {
                definition
                    .methods
                    .iter()
                    .map(|method| method.findex)
                    .chain(definition.bindings.iter().map(|binding| binding.findex))
            })
            .collect::<HashSet<_>>();
        // Selected object methods and bindings already carry their bytecode
        // signatures inline. Keep only matching native entries here, where the
        // library/name metadata adds information that is not otherwise present.
        callables.retain(|callable| {
            matches!(callable.kind, CallableKind::Native)
                && selected_findexes.contains(&callable.findex)
        });
    }
    assign_unique_callable_ids(&mut callables);
    callables.sort_by(|left, right| left.id.cmp(&right.id));

    let summary = summarize(&code, &types, globals.len(), callables.len());
    let source = SourceMetadata {
        bytecode_version: code.version,
        hlboot: fingerprint(&options.hlboot_path)?,
        farever_exe: fingerprint_optional(options.farever_exe_path.as_deref())?,
        libhl: fingerprint_optional(options.libhl_path.as_deref())?,
        steam: read_steam_release(options.steam_manifest_path.as_deref())?,
        extracted_at_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };

    Ok(ApiSnapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        source,
        scope: SnapshotScope::new(snapshot_selection),
        summary,
        types,
        globals,
        callables,
    })
}

fn optional_file(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

pub(crate) fn validate_hlboot_header(path: &Path) -> Result<(), ExtractError> {
    let mut file = File::open(path).map_err(|source| ExtractError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut header = [0_u8; 3];
    file.read_exact(&mut header)
        .map_err(|source| ExtractError::Io {
            path: path.to_owned(),
            source,
        })?;
    if header != *b"HLB" {
        return Err(ExtractError::InvalidInput(format!(
            "{} is not a HashLink bytecode file (missing HLB header)",
            path.display()
        )));
    }
    Ok(())
}

struct SignatureIndex {
    types_by_findex: HashMap<usize, RefType>,
}

impl SignatureIndex {
    fn new(code: &Bytecode) -> Self {
        let mut types_by_findex = HashMap::with_capacity(code.functions.len() + code.natives.len());
        for function in &code.functions {
            types_by_findex.insert(function.findex.0, function.t);
        }
        for native in &code.natives {
            types_by_findex.insert(native.findex.0, native.t);
        }
        Self { types_by_findex }
    }

    fn get(&self, code: &Bytecode, findex: RefFun) -> (Vec<String>, String) {
        self.types_by_findex
            .get(&findex.0)
            .copied()
            .map(|ty| signature_from_type(code, ty))
            .unwrap_or_else(|| (Vec::new(), "<unresolved>".to_owned()))
    }
}

fn extract_types(code: &Bytecode, signatures: &SignatureIndex) -> Vec<TypeDefinition> {
    extract_types_matching(code, signatures, None)
}

fn extract_types_matching(
    code: &Bytecode,
    signatures: &SignatureIndex,
    names: Option<&HashSet<&str>>,
) -> Vec<TypeDefinition> {
    code.types
        .iter()
        .enumerate()
        .filter(|(_, ty)| {
            names.is_none_or(|names| {
                let name = match ty {
                    Type::Obj(object) | Type::Struct(object) => object_name(code, object),
                    Type::Enum { name, .. } | Type::Abstract { name } => string_at(code, *name),
                    _ => return false,
                };
                names.contains(name.as_str())
            })
        })
        .filter_map(|(type_index, ty)| match ty {
            Type::Obj(object) => Some(extract_object(
                code,
                signatures,
                type_index,
                object,
                TypeKind::Object,
            )),
            Type::Struct(object) => Some(extract_object(
                code,
                signatures,
                type_index,
                object,
                TypeKind::Struct,
            )),
            Type::Enum {
                name,
                global,
                constructs,
            } => {
                let name = string_at(code, *name);
                Some(TypeDefinition {
                    id: format!("enum:{name}"),
                    kind: TypeKind::Enum,
                    name,
                    type_index,
                    super_type: None,
                    global_index: global.0.checked_sub(1),
                    inherited_field_count: 0,
                    fields: Vec::new(),
                    methods: Vec::new(),
                    bindings: Vec::new(),
                    variants: constructs
                        .iter()
                        .map(|construct| EnumVariantDefinition {
                            name: string_at(code, construct.name),
                            parameters: construct
                                .params
                                .iter()
                                .map(|ty| format_type_ref(code, *ty))
                                .collect(),
                        })
                        .collect(),
                })
            }
            Type::Abstract { name } => {
                let name = string_at(code, *name);
                Some(TypeDefinition {
                    id: format!("abstract:{name}"),
                    kind: TypeKind::Abstract,
                    name,
                    type_index,
                    super_type: None,
                    global_index: None,
                    inherited_field_count: 0,
                    fields: Vec::new(),
                    methods: Vec::new(),
                    bindings: Vec::new(),
                    variants: Vec::new(),
                })
            }
            Type::Virtual { fields } => {
                let shape = format_type_ref(code, RefType(type_index));
                let shape_hash = hex::encode(Sha256::digest(shape.as_bytes()));
                Some(TypeDefinition {
                    id: format!("virtual:{shape_hash}"),
                    kind: TypeKind::Virtual,
                    name: format!("<virtual:{}>", &shape_hash[..16]),
                    type_index,
                    super_type: None,
                    global_index: None,
                    inherited_field_count: 0,
                    fields: fields
                        .iter()
                        .enumerate()
                        .map(|(slot, field)| FieldDefinition {
                            slot,
                            name: string_at(code, field.name),
                            r#type: format_type_ref(code, field.t),
                        })
                        .collect(),
                    methods: Vec::new(),
                    bindings: Vec::new(),
                    variants: Vec::new(),
                })
            }
            _ => None,
        })
        .collect()
}

fn select_types(
    all_types: Vec<TypeDefinition>,
    selection: &ExtractionSelection,
) -> Result<(Vec<TypeDefinition>, SnapshotSelection), ExtractError> {
    let ExtractionSelection::Focused {
        additional_namespaces,
        additional_root_types,
        include_direct_references,
    } = selection
    else {
        return Ok((
            all_types,
            SnapshotSelection {
                mode: "all".to_owned(),
                namespace_prefixes: Vec::new(),
                root_types: Vec::new(),
                includes_direct_references: false,
            },
        ));
    };

    let mut namespace_prefixes = DEFAULT_NAMESPACES
        .iter()
        .map(|value| (*value).to_owned())
        .chain(additional_namespaces.iter().cloned())
        .collect::<Vec<_>>();
    namespace_prefixes.sort();
    namespace_prefixes.dedup();
    if namespace_prefixes.iter().any(|prefix| prefix.is_empty()) {
        return Err(ExtractError::InvalidInput(
            "namespace prefixes may not be empty".to_owned(),
        ));
    }

    let mut root_types = DEFAULT_ROOT_TYPES
        .iter()
        .map(|value| (*value).to_owned())
        .chain(additional_root_types.iter().cloned())
        .collect::<Vec<_>>();
    root_types.sort();
    root_types.dedup();

    let mut indexes_by_name = HashMap::<String, Vec<usize>>::new();
    for definition in &all_types {
        indexes_by_name
            .entry(definition.name.clone())
            .or_default()
            .push(definition.type_index);
    }
    for requested in additional_root_types {
        if !indexes_by_name.contains_key(requested) {
            return Err(ExtractError::InvalidInput(format!(
                "root type was not found in bytecode: {requested}"
            )));
        }
    }

    let root_set = root_types
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut selected = all_types
        .iter()
        .filter(|definition| {
            root_set.contains(definition.name.as_str())
                || namespace_prefixes
                    .iter()
                    .any(|prefix| definition.name.starts_with(prefix))
        })
        .map(|definition| definition.type_index)
        .collect::<HashSet<_>>();

    // Pull in definitions directly named by stored fields, enum payloads, and
    // base types. Method/binding signatures remain visible on the selected
    // types, but following them would quickly reach editor, renderer, and
    // standard-library implementation details unrelated to inspectable state.
    if *include_direct_references {
        let initial_selection = selected.clone();
        for definition in all_types
            .iter()
            .filter(|definition| initial_selection.contains(&definition.type_index))
        {
            for expression in type_expressions(definition) {
                for token in type_expression_tokens(expression) {
                    if let Some(indexes) = indexes_by_name.get(token) {
                        selected.extend(indexes);
                    }
                }
            }
        }
    }

    // Always retain complete inheritance chains for selected object types.
    loop {
        let mut changed = false;
        let super_types = all_types
            .iter()
            .filter(|definition| selected.contains(&definition.type_index))
            .filter_map(|definition| definition.super_type.clone())
            .collect::<Vec<_>>();
        for super_type in super_types {
            if let Some(indexes) = indexes_by_name.get(&super_type) {
                let before = selected.len();
                selected.extend(indexes);
                changed |= selected.len() != before;
            }
        }
        if !changed {
            break;
        }
    }

    let focused = all_types
        .into_iter()
        .filter(|definition| selected.contains(&definition.type_index))
        .collect();
    Ok((
        focused,
        SnapshotSelection {
            mode: "focused".to_owned(),
            namespace_prefixes,
            root_types,
            includes_direct_references: *include_direct_references,
        },
    ))
}

fn type_expressions(definition: &TypeDefinition) -> Vec<&str> {
    let mut expressions = Vec::new();
    if let Some(super_type) = definition.super_type.as_deref() {
        expressions.push(super_type);
    }
    for field in &definition.fields {
        expressions.push(field.r#type.as_str());
    }
    for variant in &definition.variants {
        expressions.extend(variant.parameters.iter().map(String::as_str));
    }
    expressions
}

fn type_expression_tokens(expression: &str) -> impl Iterator<Item = &str> {
    expression
        .split(|character: char| {
            !(character.is_ascii_alphanumeric()
                || character == '_'
                || character == '.'
                || character == '$')
        })
        .filter(|token| !token.is_empty())
}

fn type_expression_mentions(expression: &str, names: &HashSet<String>) -> bool {
    type_expression_tokens(expression).any(|token| names.contains(token))
}

fn extract_object(
    code: &Bytecode,
    signatures: &SignatureIndex,
    type_index: usize,
    object: &TypeObj,
    kind: TypeKind,
) -> TypeDefinition {
    let name = object_name(code, object);
    let kind_name = match kind {
        TypeKind::Object => "object",
        TypeKind::Struct => "struct",
        _ => unreachable!("only object-like kinds are passed here"),
    };
    let inherited_field_count = object.fields.len().saturating_sub(object.own_fields.len());
    let fields = object
        .own_fields
        .iter()
        .enumerate()
        .map(|(index, field)| FieldDefinition {
            slot: inherited_field_count + index,
            name: string_at(code, field.name),
            r#type: format_type_ref(code, field.t),
        })
        .collect();
    let mut methods = object
        .protos
        .iter()
        .map(|prototype| {
            let (arguments, return_type) = signatures.get(code, prototype.findex);
            MethodDefinition {
                name: string_at(code, prototype.name),
                findex: prototype.findex.0,
                prototype_index: prototype.pindex,
                arguments,
                return_type,
            }
        })
        .collect::<Vec<_>>();
    methods.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.prototype_index.cmp(&right.prototype_index))
            .then(left.findex.cmp(&right.findex))
    });

    let mut bindings = object
        .bindings
        .iter()
        .map(|(field, findex)| {
            let (arguments, return_type) = signatures.get(code, *findex);
            BindingDefinition {
                field_slot: field.0,
                field_name: object
                    .fields
                    .get(field.0)
                    .map(|field| string_at(code, field.name))
                    .unwrap_or_else(|| format!("<invalid-field:{}>", field.0)),
                findex: findex.0,
                arguments,
                return_type,
            }
        })
        .collect::<Vec<_>>();
    bindings.sort_by(|left, right| {
        left.field_slot
            .cmp(&right.field_slot)
            .then(left.findex.cmp(&right.findex))
    });

    TypeDefinition {
        id: format!("{kind_name}:{name}"),
        kind,
        name,
        type_index,
        super_type: object.super_.map(|ty| format_type_ref(code, ty)),
        global_index: object.global.0.checked_sub(1),
        inherited_field_count,
        fields,
        methods,
        bindings,
        variants: Vec::new(),
    }
}

fn extract_callables(code: &Bytecode) -> Vec<CallableDefinition> {
    let mut result = Vec::with_capacity(code.functions.len() + code.natives.len());
    for function in &code.functions {
        let name = string_at(code, function.name);
        if name == "<none>" || name.is_empty() {
            continue;
        }
        let owner = function.parent.map(|parent| format_type_ref(code, parent));
        let (arguments, return_type) = signature_from_type(code, function.t);
        result.push(CallableDefinition {
            id: callable_base_id(CallableKind::Bytecode, owner.as_deref(), None, &name),
            kind: CallableKind::Bytecode,
            name,
            owner,
            library: None,
            findex: function.findex.0,
            arguments,
            return_type,
        });
    }
    for native in &code.natives {
        let name = string_at(code, native.name);
        let library = string_at(code, native.lib);
        let (arguments, return_type) = signature_from_type(code, native.t);
        result.push(CallableDefinition {
            id: callable_base_id(CallableKind::Native, None, Some(&library), &name),
            kind: CallableKind::Native,
            name,
            owner: None,
            library: Some(library),
            findex: native.findex.0,
            arguments,
            return_type,
        });
    }
    result
}

fn callable_base_id(
    kind: CallableKind,
    owner: Option<&str>,
    library: Option<&str>,
    name: &str,
) -> String {
    match kind {
        CallableKind::Bytecode => format!("bytecode:{}::{name}", owner.unwrap_or("<module>")),
        CallableKind::Native => format!("native:{}::{name}", library.unwrap_or("<unknown>")),
    }
}

fn assign_unique_type_ids(types: &mut [TypeDefinition]) {
    let mut totals = HashMap::<String, usize>::new();
    for definition in types.iter() {
        *totals.entry(definition.id.clone()).or_default() += 1;
    }
    let mut seen = HashMap::<String, usize>::new();
    for definition in types {
        if totals[&definition.id] > 1 {
            let ordinal = seen.entry(definition.id.clone()).or_default();
            *ordinal += 1;
            definition.id = format!("{}#{}", definition.id, ordinal);
        }
    }
}

fn assign_unique_callable_ids(callables: &mut [CallableDefinition]) {
    callables.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then(left.arguments.cmp(&right.arguments))
            .then(left.return_type.cmp(&right.return_type))
            .then(left.findex.cmp(&right.findex))
    });
    let mut totals = HashMap::<String, usize>::new();
    for callable in callables.iter() {
        *totals.entry(callable.id.clone()).or_default() += 1;
    }
    let mut seen = HashMap::<String, usize>::new();
    for callable in callables {
        if totals[&callable.id] > 1 {
            let ordinal = seen.entry(callable.id.clone()).or_default();
            *ordinal += 1;
            callable.id = format!("{}#{}", callable.id, ordinal);
        }
    }
}

fn signature_from_type(code: &Bytecode, type_ref: RefType) -> (Vec<String>, String) {
    match code.types.get(type_ref.0) {
        Some(Type::Fun(function)) | Some(Type::Method(function)) => signature(code, function),
        _ => (Vec::new(), format_type_ref(code, type_ref)),
    }
}

fn signature(code: &Bytecode, function: &TypeFun) -> (Vec<String>, String) {
    (
        function
            .args
            .iter()
            .map(|ty| format_type_ref(code, *ty))
            .collect(),
        format_type_ref(code, function.ret),
    )
}

fn format_type_ref(code: &Bytecode, type_ref: RefType) -> String {
    format_type_ref_inner(code, type_ref, &mut HashSet::new(), 0)
}

pub(crate) fn compatibility_types(code: &Bytecode, names: &HashSet<&str>) -> Vec<TypeDefinition> {
    extract_types_matching(code, &SignatureIndex::new(code), Some(names))
}

fn format_type_ref_inner(
    code: &Bytecode,
    type_ref: RefType,
    active: &mut HashSet<usize>,
    depth: usize,
) -> String {
    if depth >= 24 || !active.insert(type_ref.0) {
        return format!("type#{}", type_ref.0);
    }
    let formatted = match code.types.get(type_ref.0) {
        None => format!("<invalid-type:{}>", type_ref.0),
        Some(Type::Void) => "void".to_owned(),
        Some(Type::UI8) => "u8".to_owned(),
        Some(Type::UI16) => "u16".to_owned(),
        Some(Type::I32) => "i32".to_owned(),
        Some(Type::I64) => "i64".to_owned(),
        Some(Type::F32) => "f32".to_owned(),
        Some(Type::F64) => "f64".to_owned(),
        Some(Type::Bool) => "bool".to_owned(),
        Some(Type::Bytes) => "bytes".to_owned(),
        Some(Type::Dyn) => "dynamic".to_owned(),
        Some(Type::Array) => "array".to_owned(),
        Some(Type::Type) => "type".to_owned(),
        Some(Type::DynObj) => "dynamic-object".to_owned(),
        Some(Type::Guid) => "guid".to_owned(),
        Some(Type::Obj(object)) | Some(Type::Struct(object)) => object_name(code, object),
        Some(Type::Abstract { name }) => string_at(code, *name),
        Some(Type::Enum { name, .. }) => string_at(code, *name),
        Some(Type::Ref(inner)) => format!(
            "ref<{}>",
            format_type_ref_inner(code, *inner, active, depth + 1)
        ),
        Some(Type::Null(inner)) => format!(
            "nullable<{}>",
            format_type_ref_inner(code, *inner, active, depth + 1)
        ),
        Some(Type::Packed(inner)) => format!(
            "packed<{}>",
            format_type_ref_inner(code, *inner, active, depth + 1)
        ),
        Some(Type::Fun(function)) => format_function_type(code, "fn", function, active, depth),
        Some(Type::Method(function)) => {
            format_function_type(code, "method", function, active, depth)
        }
        Some(Type::Virtual { fields }) => {
            let fields = fields
                .iter()
                .map(|field| {
                    format!(
                        "{}:{}",
                        string_at(code, field.name),
                        format_type_ref_inner(code, field.t, active, depth + 1)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("virtual{{{fields}}}")
        }
    };
    active.remove(&type_ref.0);
    formatted
}

fn format_function_type(
    code: &Bytecode,
    label: &str,
    function: &TypeFun,
    active: &mut HashSet<usize>,
    depth: usize,
) -> String {
    let arguments = function
        .args
        .iter()
        .map(|ty| format_type_ref_inner(code, *ty, active, depth + 1))
        .collect::<Vec<_>>()
        .join(",");
    let return_type = format_type_ref_inner(code, function.ret, active, depth + 1);
    format!("{label}({arguments})->{return_type}")
}

fn string_at(code: &Bytecode, string_ref: RefString) -> String {
    code.get(string_ref).to_string()
}

fn object_name(code: &Bytecode, object: &TypeObj) -> String {
    // HashLink's built-in string object has no bytecode name. Recognize the
    // same exact bytes/length shape as the live reader, never any anonymous obj.
    if object.name.is_null()
        && object.super_.is_none()
        && object.own_fields.len() == 2
        && object.own_fields.iter().any(|field| {
            string_at(code, field.name) == "bytes"
                && matches!(code.types.get(field.t.0), Some(Type::Bytes))
        })
        && object.own_fields.iter().any(|field| {
            string_at(code, field.name) == "length"
                && matches!(code.types.get(field.t.0), Some(Type::I32))
        })
    {
        "String".to_owned()
    } else {
        string_at(code, object.name)
    }
}

fn summarize(
    code: &Bytecode,
    types: &[TypeDefinition],
    exported_globals: usize,
    exported_callables: usize,
) -> SnapshotSummary {
    let mut summary = SnapshotSummary {
        type_pool_entries: code.types.len(),
        exported_type_definitions: types.len(),
        globals: exported_globals,
        bytecode_functions: code.functions.len(),
        native_functions: code.natives.len(),
        exported_callables,
        ..SnapshotSummary::default()
    };
    for definition in types {
        match definition.kind {
            TypeKind::Object => summary.object_definitions += 1,
            TypeKind::Struct => summary.struct_definitions += 1,
            TypeKind::Enum => summary.enum_definitions += 1,
            TypeKind::Abstract => summary.abstract_definitions += 1,
            TypeKind::Virtual => summary.virtual_definitions += 1,
        }
        summary.fields += definition.fields.len();
        summary.methods += definition.methods.len();
        summary.bindings += definition.bindings.len();
    }
    summary
}

fn fingerprint(path: &Path) -> Result<FileFingerprint, ExtractError> {
    let file = File::open(path).map_err(|source| ExtractError::Io {
        path: path.to_owned(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| ExtractError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|source| ExtractError::Io {
                path: path.to_owned(),
                source,
            })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(FileFingerprint {
        file_name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        size: metadata.len(),
        sha256: hex::encode_upper(hasher.finalize()),
    })
}

fn fingerprint_optional(path: Option<&Path>) -> Result<Option<FileFingerprint>, ExtractError> {
    path.map(fingerprint).transpose()
}

fn read_steam_release(path: Option<&Path>) -> Result<Option<SteamRelease>, ExtractError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let mut contents = String::new();
    File::open(path)
        .and_then(|mut file| file.read_to_string(&mut contents))
        .map_err(|source| ExtractError::Io {
            path: path.to_owned(),
            source,
        })?;
    Ok(Some(SteamRelease {
        app_id: "3672400".to_owned(),
        build_id: vdf_value(&contents, "buildid"),
        last_updated: vdf_value(&contents, "LastUpdated"),
    }))
}

fn vdf_value(contents: &str, key: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let mut quoted = line.split('"');
        let _before = quoted.next()?;
        let candidate_key = quoted.next()?;
        let _between = quoted.next()?;
        let value = quoted.next()?;
        (candidate_key == key).then(|| value.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::{object_name, select_types, vdf_value};
    use crate::model::{ExtractionSelection, FieldDefinition, TypeDefinition, TypeKind};

    #[test]
    fn reads_vdf_values() {
        let input = "\"AppState\"\n{\n  \"buildid\"  \"25257040\"\n}";
        assert_eq!(vdf_value(input, "buildid").as_deref(), Some("25257040"));
        assert_eq!(vdf_value(input, "missing"), None);
    }

    #[test]
    fn native_string_name_requires_exact_anonymous_bytes_length_schema() {
        use hlbc::types::{ObjField, RefGlobal, RefString, RefType, Type, TypeObj};
        let mut code = hlbc::Bytecode::default();
        code.strings
            .extend(["<none>".into(), "bytes".into(), "length".into()]);
        code.types.extend([Type::Bytes, Type::I32]);
        let mut object = TypeObj {
            name: RefString(0),
            super_: None,
            global: RefGlobal(0),
            own_fields: vec![
                ObjField {
                    name: RefString(1),
                    t: RefType(0),
                },
                ObjField {
                    name: RefString(2),
                    t: RefType(1),
                },
            ],
            protos: vec![],
            bindings: Default::default(),
            fields: vec![],
        };
        assert_eq!(object_name(&code, &object), "String");
        code.types[1] = Type::F64;
        assert_eq!(object_name(&code, &object), "<none>");
        code.types[1] = Type::I32;
        object.super_ = Some(RefType(1));
        assert_eq!(object_name(&code, &object), "<none>");
    }

    #[test]
    fn focused_selection_keeps_direct_references_and_ancestors() {
        let definition =
            |type_index, name: &str, super_type: Option<&str>, fields| TypeDefinition {
                id: format!("object:{name}"),
                kind: TypeKind::Object,
                name: name.to_owned(),
                type_index,
                super_type: super_type.map(str::to_owned),
                global_index: None,
                inherited_field_count: 0,
                fields,
                methods: Vec::new(),
                bindings: Vec::new(),
                variants: Vec::new(),
            };
        let definitions = vec![
            definition(
                1,
                "ent.Hero",
                None,
                vec![FieldDefinition {
                    slot: 0,
                    name: "payload".to_owned(),
                    r#type: "custom.Payload".to_owned(),
                }],
            ),
            definition(2, "custom.Payload", Some("framework.Base"), Vec::new()),
            definition(3, "framework.Base", None, Vec::new()),
            definition(4, "editor.Unrelated", None, Vec::new()),
        ];

        let (selected, metadata) = select_types(
            definitions,
            &ExtractionSelection::Focused {
                additional_namespaces: Vec::new(),
                additional_root_types: Vec::new(),
                include_direct_references: true,
            },
        )
        .unwrap();
        let names = selected
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert!(names.contains("ent.Hero"));
        assert!(names.contains("custom.Payload"));
        assert!(names.contains("framework.Base"));
        assert!(!names.contains("editor.Unrelated"));
        assert_eq!(metadata.mode, "focused");
        assert!(metadata.includes_direct_references);
    }

    #[test]
    fn focused_selection_does_not_expand_fields_by_default() {
        let definition =
            |type_index, name: &str, super_type: Option<&str>, fields| TypeDefinition {
                id: format!("object:{name}"),
                kind: TypeKind::Object,
                name: name.to_owned(),
                type_index,
                super_type: super_type.map(str::to_owned),
                global_index: None,
                inherited_field_count: 0,
                fields,
                methods: Vec::new(),
                bindings: Vec::new(),
                variants: Vec::new(),
            };
        let definitions = vec![
            definition(
                1,
                "ent.Hero",
                Some("ent.Unit"),
                vec![FieldDefinition {
                    slot: 0,
                    name: "payload".to_owned(),
                    r#type: "custom.Payload".to_owned(),
                }],
            ),
            definition(2, "ent.Unit", None, Vec::new()),
            definition(3, "custom.Payload", None, Vec::new()),
        ];

        let (selected, metadata) = select_types(
            definitions,
            &ExtractionSelection::Focused {
                additional_namespaces: Vec::new(),
                additional_root_types: Vec::new(),
                include_direct_references: false,
            },
        )
        .unwrap();
        let names = selected
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert!(names.contains("ent.Hero"));
        assert!(names.contains("ent.Unit"));
        assert!(!names.contains("custom.Payload"));
        assert!(!metadata.includes_direct_references);
    }
}
