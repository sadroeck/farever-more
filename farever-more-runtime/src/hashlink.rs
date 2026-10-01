//! Reusable, fail-closed HashLink runtime verification helpers.
//!
//! Hook callbacks should use only `object_has_exact_type` plus fixed-size reads
//! from a previously validated layout. Metadata traversal, string handling,
//! symbol lookup, and schema validation belong on an observer worker.

pub(crate) use crate::state::HashLink;
use crate::state::{HashLinkFieldShape, HashLinkObjectShape};
use sha2::{Digest, Sha256};
use std::ffi::{c_void, CStr};
use std::fs::File;
use std::io::Read;
use std::mem::size_of;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
use std::mem::zeroed;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
use windows_sys::Win32::System::Memory::{
    VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_EXECUTE, PAGE_EXECUTE_READ,
    PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS,
};

const HL_TYPE_KIND: usize = 0;
const HL_TYPE_UNION: usize = 0x08;
const HL_FUN_ARGS: usize = 0;
const HL_FUN_RETURN: usize = 0x08;
const HL_FUN_ARG_COUNT: usize = 0x10;
const HL_RUNTIME_FIELD_COUNT: usize = 0x08;
const HL_RUNTIME_METHOD_COUNT: usize = 0x14;
const HL_RUNTIME_BINDING_COUNT: usize = 0x18;
const HL_RUNTIME_METHODS: usize = 0x20;
const HL_RUNTIME_FIELD_OFFSETS: usize = 0x28;
const HL_RUNTIME_BINDINGS: usize = 0x30;
const HL_RUNTIME_PARENT: usize = 0x38;
const HL_RUNTIME_LOOKUP_COUNT: usize = 0x60;
const HL_RUNTIME_LOOKUP: usize = 0x68;
const HL_LOOKUP_TYPE: usize = 0;
const HL_LOOKUP_FIELD_INDEX: usize = 0x0C;
const HL_BINDING_SIZE: usize = 0x18;
const HL_BINDING_TYPE: usize = 0x08;
const HL_BINDING_FIELD: usize = 0x10;

type HlGetObjProto = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type HlHashUtf8 = unsafe extern "C" fn(*const std::ffi::c_char) -> i32;
type HlLookupFind = unsafe extern "C" fn(*mut c_void, i32, i32) -> *mut c_void;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BuildFileSpec {
    pub(crate) name: &'static str,
    pub(crate) sha256: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HashLinkBuildSpec {
    pub(crate) profile: &'static str,
    pub(crate) files: &'static [BuildFileSpec],
}

pub(crate) fn verify_build(directory: &Path, spec: &HashLinkBuildSpec) -> Result<String, String> {
    if spec.files.is_empty() {
        return Err(format!("build profile {} contains no files", spec.profile));
    }
    let mut observed = Vec::with_capacity(spec.files.len());
    let mut matches = true;
    for (index, expected) in spec.files.iter().enumerate() {
        if expected.name.is_empty() || Path::new(expected.name).components().count() != 1 {
            return Err(format!(
                "build profile contains invalid file {}",
                expected.name
            ));
        }
        if spec.files[..index]
            .iter()
            .any(|previous| previous.name == expected.name)
        {
            return Err(format!(
                "build profile contains duplicate file {}",
                expected.name
            ));
        }
        if expected.sha256.len() != 64
            || !expected.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(format!(
                "build profile contains invalid SHA-256 for {}",
                expected.name
            ));
        }
        let actual = hash_file(&directory.join(expected.name))?;
        observed.push(format!("{}={actual}", expected.name));
        if !actual.eq_ignore_ascii_case(expected.sha256) {
            matches = false;
        }
    }
    let hashes = observed.join(" ");
    if matches {
        Ok(hashes)
    } else {
        Err(format!(
            "unknown game build profile={} {hashes}",
            spec.profile
        ))
    }
}

fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub(crate) enum HashLinkKind {
    Void = 0,
    I32 = 3,
    I64 = 4,
    F64 = 6,
    Bool = 7,
    Bytes = 8,
    Dynamic = 9,
    Function = 10,
    Object = 11,
    Array = 12,
    Reference = 14,
    Virtual = 15,
    Enum = 18,
    Null = 19,
}

impl HashLinkKind {
    fn storage_width(self) -> Option<usize> {
        match self {
            Self::Bool => Some(1),
            Self::I32 => Some(4),
            Self::I64 => Some(8),
            Self::F64 => Some(8),
            Self::Bytes => Some(size_of::<usize>()),
            Self::Object => Some(size_of::<usize>()),
            Self::Array => Some(size_of::<usize>()),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HashLinkFieldSpec {
    pub(crate) name: &'static str,
    pub(crate) kind: HashLinkKind,
    pub(crate) object_type_name: Option<&'static str>,
}

impl HashLinkFieldSpec {
    pub(crate) const fn scalar(name: &'static str, kind: HashLinkKind) -> Self {
        Self {
            name,
            kind,
            object_type_name: None,
        }
    }

    pub(crate) const fn object(name: &'static str, object_type_name: &'static str) -> Self {
        Self {
            name,
            kind: HashLinkKind::Object,
            object_type_name: Some(object_type_name),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HashLinkObjectSpec {
    pub(crate) name: &'static str,
    pub(crate) kind: HashLinkKind,
    pub(crate) fields: &'static [HashLinkFieldSpec],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedHashLinkField {
    pub(crate) name: &'static str,
    pub(crate) offset: usize,
    pub(crate) width: usize,
    pub(crate) type_address: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidatedHashLinkObject {
    pub(crate) type_address: usize,
    pub(crate) size: usize,
    fields: Vec<ResolvedHashLinkField>,
}

impl ValidatedHashLinkObject {
    pub(crate) fn offset(&self, name: &str) -> Option<usize> {
        self.fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.offset)
    }

    pub(crate) fn field_type_address(&self, name: &str) -> Option<usize> {
        self.fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.type_address)
    }
}

pub(crate) fn validate_object(
    hl: &HashLink<'_>,
    type_address: usize,
    spec: &HashLinkObjectSpec,
) -> Result<ValidatedHashLinkObject, String> {
    let shape = hl.object_shape_for_type(type_address).map_err(|error| {
        format!(
            "could not read the complete {} object shape: {error}",
            spec.name
        )
    })?;
    validate_object_shape(type_address, &shape, spec)
}

fn validate_object_shape(
    type_address: usize,
    shape: &HashLinkObjectShape,
    spec: &HashLinkObjectSpec,
) -> Result<ValidatedHashLinkObject, String> {
    if shape.name != spec.name || shape.kind != spec.kind as i32 {
        return Err(format!(
            "expected {} {}, found {} {}",
            kind_name(spec.kind as i32),
            spec.name,
            kind_name(shape.kind),
            shape.name
        ));
    }
    for (index, field) in spec.fields.iter().enumerate() {
        if spec.fields[..index]
            .iter()
            .any(|previous| previous.name == field.name)
        {
            return Err(format!("schema contains duplicate field {}", field.name));
        }
    }

    let mut resolved = Vec::with_capacity(spec.fields.len());
    for expected in spec.fields {
        resolved.push(validate_field(&shape.fields, shape.size, expected)?);
    }
    let mut ranges = resolved.clone();
    ranges.sort_unstable_by_key(|field| field.offset);
    for pair in ranges.windows(2) {
        let left = &pair[0];
        let right = &pair[1];
        if left.offset + left.width > right.offset {
            return Err(format!(
                "fields {} and {} overlap in {} object storage",
                left.name, right.name, spec.name
            ));
        }
    }

    Ok(ValidatedHashLinkObject {
        type_address,
        size: shape.size,
        fields: resolved,
    })
}

fn validate_field(
    fields: &[HashLinkFieldShape],
    object_size: usize,
    expected: &HashLinkFieldSpec,
) -> Result<ResolvedHashLinkField, String> {
    // HashLink bytecode can encode an object field with a null string
    // reference, which the runtime exposes as an empty name. Such fields are
    // opaque to a name-addressed projection, but do not make the required
    // named fields unsafe to use. Validate uniqueness only for fields that the
    // provider actually reads.
    let mut matches = fields.iter().filter(|field| field.name == expected.name);
    let field = matches
        .next()
        .ok_or_else(|| format!("missing field {}", expected.name))?;
    if matches.next().is_some() {
        return Err(format!("duplicate required field {}", expected.name));
    }
    if field.kind != expected.kind as i32 {
        return Err(format!(
            "field {} expected {}, found {}",
            expected.name,
            kind_name(expected.kind as i32),
            kind_name(field.kind)
        ));
    }
    if field.object_type_name.as_deref() != expected.object_type_name {
        return Err(format!(
            "field {} expected object type {}, found {}",
            expected.name,
            expected.object_type_name.unwrap_or("<none>"),
            field.object_type_name.as_deref().unwrap_or("<none>")
        ));
    }
    let width = expected.kind.storage_width().ok_or_else(|| {
        format!(
            "field {} uses unsupported inline kind {}",
            expected.name,
            kind_name(expected.kind as i32)
        )
    })?;
    if field.offset < size_of::<usize>() || !field.offset.is_multiple_of(width) {
        return Err(format!(
            "field {} has invalid offset {} for width {width}",
            expected.name, field.offset
        ));
    }
    let end = field
        .offset
        .checked_add(width)
        .ok_or_else(|| format!("field {} range overflow", expected.name))?;
    if end > object_size {
        return Err(format!(
            "field {} range {}..{end} exceeds object size {object_size}",
            expected.name, field.offset
        ));
    }
    Ok(ResolvedHashLinkField {
        name: expected.name,
        offset: field.offset,
        width,
        type_address: field.type_address,
    })
}

/// Performs the only object-header read permitted in a hook before its fixed
/// shape has been established by `validate_object` on a worker thread.
///
/// # Safety
///
/// `object` must be a live non-null HashLink object supplied by the validated
/// callback ABI for the duration of this read.
pub(crate) unsafe fn object_has_exact_type(object: *const c_void, expected_type: usize) -> bool {
    if object.is_null() || expected_type < 0x1_0000 {
        return false;
    }
    // SAFETY: guaranteed by the caller; the `hl_type*` is the first object word.
    unsafe { std::ptr::read_unaligned(object.cast::<usize>()) == expected_type }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HashLinkTypeSpec {
    Kind(HashLinkKind),
    Object(&'static str),
    Enum(&'static str),
    Nullable(HashLinkKind),
    Reference(HashLinkKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HashLinkVirtualFieldSpec {
    pub(crate) name: &'static str,
    pub(crate) value_type: HashLinkTypeSpec,
}

impl HashLinkVirtualFieldSpec {
    pub(crate) const fn new(name: &'static str, value_type: HashLinkTypeSpec) -> Self {
        Self { name, value_type }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HashLinkVirtualSpec {
    pub(crate) label: &'static str,
    pub(crate) fields: &'static [HashLinkVirtualFieldSpec],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidatedHashLinkVirtual {
    pub(crate) type_address: usize,
    fields: Vec<(&'static str, usize)>,
}

impl ValidatedHashLinkVirtual {
    pub(crate) fn field_type_address(&self, name: &str) -> Option<usize> {
        self.fields
            .iter()
            .find(|(field_name, _)| *field_name == name)
            .map(|(_, type_address)| *type_address)
    }
}

pub(crate) fn validate_virtual(
    hl: &HashLink<'_>,
    type_address: usize,
    spec: &HashLinkVirtualSpec,
) -> Result<ValidatedHashLinkVirtual, String> {
    let shape = hl
        .virtual_shape_for_type(type_address)
        .ok_or_else(|| format!("could not read the complete {} virtual shape", spec.label))?;
    if shape.fields.len() != spec.fields.len() {
        return Err(format!(
            "{} virtual field count mismatch: expected {}, found {}",
            spec.label,
            spec.fields.len(),
            shape.fields.len()
        ));
    }
    for (index, expected) in spec.fields.iter().enumerate() {
        if spec.fields[..index]
            .iter()
            .any(|previous| previous.name == expected.name)
        {
            return Err(format!(
                "{} schema contains duplicate field {}",
                spec.label, expected.name
            ));
        }
    }

    let mut resolved = Vec::with_capacity(spec.fields.len());
    for expected in spec.fields {
        let mut matches = shape
            .fields
            .iter()
            .filter(|field| field.name == expected.name);
        let field = matches
            .next()
            .ok_or_else(|| format!("{} is missing field {}", spec.label, expected.name))?;
        if matches.next().is_some() {
            return Err(format!(
                "{} has duplicate field {}",
                spec.label, expected.name
            ));
        }
        validate_type(hl, field.type_address, expected.value_type)
            .map_err(|error| format!("{} field {} mismatch: {error}", spec.label, expected.name))?;
        resolved.push((expected.name, field.type_address));
    }

    Ok(ValidatedHashLinkVirtual {
        type_address,
        fields: resolved,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HashLinkMethodSpec {
    pub(crate) lookup_type: &'static str,
    pub(crate) name: &'static CStr,
    pub(crate) arguments: &'static [HashLinkTypeSpec],
    pub(crate) result: HashLinkTypeSpec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidatedHashLinkMethod {
    target: usize,
    name: &'static CStr,
    argument_types: Vec<usize>,
}

impl ValidatedHashLinkMethod {
    pub(crate) fn target(&self) -> usize {
        self.target
    }

    pub(crate) fn name(&self) -> &'static CStr {
        self.name
    }

    pub(crate) fn argument_type(&self, index: usize) -> Option<usize> {
        self.argument_types.get(index).copied()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct HashLinkRuntime {
    module: usize,
}

impl HashLinkRuntime {
    pub(crate) fn loaded() -> Option<Self> {
        current_libhl_module().map(|module| Self { module })
    }

    pub(crate) fn export(&self, name: &CStr) -> Result<usize, String> {
        libhl_export(self.module, name)
            .ok_or_else(|| format!("libhl does not export {}", name.to_string_lossy()))
    }

    pub(crate) fn resolve_method(
        &self,
        hl: &HashLink<'_>,
        concrete_type: usize,
        spec: &HashLinkMethodSpec,
    ) -> Result<ValidatedHashLinkMethod, String> {
        let (original_runtime, member) = self.resolve_member(hl, concrete_type, spec)?;
        let field_index = hl
            .memory
            .i32(member + HL_LOOKUP_FIELD_INDEX)
            .ok_or_else(|| "could not read HashLink member index".to_owned())?;
        if field_index >= 0 {
            return Err(format!(
                "HashLink member {} resolved to a data field",
                spec.name.to_string_lossy()
            ));
        }
        let method_index = usize::try_from(-i64::from(field_index) - 1)
            .map_err(|_| "invalid HashLink method index".to_owned())?;
        let argument_types = validate_signature(hl, member, spec)?;
        let method_count =
            hl.memory
                .i32(original_runtime + HL_RUNTIME_METHOD_COUNT)
                .filter(|count| (0..=65_536).contains(count))
                .ok_or_else(|| "invalid HashLink method count".to_owned())? as usize;
        if method_index >= method_count {
            return Err(format!(
                "HashLink method index {method_index} is outside table size {method_count}"
            ));
        }
        let methods = hl
            .memory
            .u64(original_runtime + HL_RUNTIME_METHODS)
            .ok_or_else(|| "could not read HashLink method table".to_owned())?;
        let target = hl
            .memory
            .u64(
                methods
                    .checked_add(method_index * size_of::<usize>())
                    .ok_or_else(|| "HashLink method slot overflow".to_owned())?,
            )
            .ok_or_else(|| "could not read HashLink method target".to_owned())?;
        executable_method(target, spec, argument_types)
    }

    /// Resolves the default receiver-bound implementation of a function field.
    /// Unlike ordinary methods, these entries live in the runtime binding table.
    pub(crate) fn resolve_bound_method(
        &self,
        hl: &HashLink<'_>,
        concrete_type: usize,
        spec: &HashLinkMethodSpec,
    ) -> Result<ValidatedHashLinkMethod, String> {
        let (runtime, member) = self.resolve_member(hl, concrete_type, spec)?;
        let receiver = spec.arguments.first().copied();
        if receiver != Some(HashLinkTypeSpec::Object(spec.lookup_type)) {
            return Err("bound HashLink method must declare its receiver first".to_owned());
        }
        // The stored field signature excludes the bound receiver, while the
        // implementation's signature includes it. Validate both before hooking.
        let field_spec = HashLinkMethodSpec {
            arguments: &spec.arguments[1..],
            ..*spec
        };
        validate_signature(hl, member, &field_spec)?;
        let offset = hl
            .memory
            .i32(member + HL_LOOKUP_FIELD_INDEX)
            .filter(|offset| *offset >= size_of::<usize>() as i32)
            .ok_or_else(|| "bound HashLink member is not an object field".to_owned())?;
        let binding = resolve_binding(hl, runtime, offset)?;
        let function_type = hl
            .memory
            .u64(binding + HL_BINDING_TYPE)
            .filter(|address| *address >= 0x1_0000)
            .ok_or_else(|| "HashLink field has no receiver-bound implementation".to_owned())?;
        let argument_types = validate_function_signature(hl, function_type, spec)?;
        let target = hl
            .memory
            .u64(binding)
            .ok_or_else(|| "could not read HashLink bound method target".to_owned())?;
        executable_method(target, spec, argument_types)
    }

    fn resolve_member(
        &self,
        hl: &HashLink<'_>,
        concrete_type: usize,
        spec: &HashLinkMethodSpec,
    ) -> Result<(usize, usize), String> {
        if !cfg!(all(target_arch = "x86_64", target_os = "windows")) {
            return Err("direct HashLink hooks currently require Windows x86_64".to_owned());
        }
        let actual_lookup_type = hl
            .type_name(concrete_type)
            .ok_or_else(|| "HashLink method lookup type is unreadable".to_owned())?;
        if actual_lookup_type != spec.lookup_type {
            return Err(format!(
                "HashLink method lookup type mismatch: expected {}, found {actual_lookup_type}",
                spec.lookup_type
            ));
        }
        let get_obj_proto: HlGetObjProto = function_pointer(self.export(c"hl_get_obj_proto")?);
        let hash_utf8: HlHashUtf8 = function_pointer(self.export(c"hl_hash_utf8")?);
        let lookup_find: HlLookupFind = function_pointer(self.export(c"hl_lookup_find")?);
        // SAFETY: the exports and their C signatures are defined by the
        // build-verified HashLink runtime. This runs only on the worker.
        let original_runtime = unsafe { get_obj_proto(concrete_type as *mut c_void) } as usize;
        if original_runtime < 0x1_0000 {
            return Err("hl_get_obj_proto returned no runtime object".to_owned());
        }
        // SAFETY: `spec.name` is a static NUL-terminated C string.
        let hash = unsafe { hash_utf8(spec.name.as_ptr()) };
        let mut runtime = original_runtime;
        let mut member = 0_usize;
        for _ in 0..32 {
            let count = hl
                .memory
                .i32(runtime + HL_RUNTIME_LOOKUP_COUNT)
                .filter(|count| (0..=65_536).contains(count))
                .ok_or_else(|| "invalid HashLink runtime lookup count".to_owned())?;
            let lookup = hl
                .memory
                .u64(runtime + HL_RUNTIME_LOOKUP)
                .unwrap_or_default();
            if count > 0 && lookup >= 0x1_0000 {
                // SAFETY: `lookup` and `count` came from the verified runtime.
                member = unsafe { lookup_find(lookup as *mut c_void, count, hash) } as usize;
                if member != 0 {
                    break;
                }
            }
            runtime = hl
                .memory
                .u64(runtime + HL_RUNTIME_PARENT)
                .unwrap_or_default();
            if runtime == 0 {
                break;
            }
        }
        if member == 0 {
            return Err(format!(
                "HashLink member {} was not found",
                spec.name.to_string_lossy()
            ));
        }
        Ok((original_runtime, member))
    }
}

fn executable_method(
    target: usize,
    spec: &HashLinkMethodSpec,
    argument_types: Vec<usize>,
) -> Result<ValidatedHashLinkMethod, String> {
    if target < 0x1_0000 || !is_executable_address(target) {
        return Err("HashLink method target is not committed executable memory".to_owned());
    }
    Ok(ValidatedHashLinkMethod {
        target,
        name: spec.name,
        argument_types,
    })
}

fn resolve_binding(hl: &HashLink<'_>, runtime: usize, offset: i32) -> Result<usize, String> {
    let count = |field| {
        hl.memory
            .i32(runtime + field)
            .filter(|count| (0..=65_536).contains(count))
            .ok_or_else(|| "invalid HashLink binding table count".to_owned())
    };
    let field_count = count(HL_RUNTIME_FIELD_COUNT)?;
    let binding_count = count(HL_RUNTIME_BINDING_COUNT)?;
    let field_offsets = hl
        .memory
        .u64(runtime + HL_RUNTIME_FIELD_OFFSETS)
        .filter(|address| *address >= 0x1_0000)
        .ok_or_else(|| "could not read HashLink field offset table".to_owned())?;
    let bindings = hl
        .memory
        .u64(runtime + HL_RUNTIME_BINDINGS)
        .filter(|address| *address >= 0x1_0000)
        .ok_or_else(|| "could not read HashLink binding table".to_owned())?;
    let mut found = None;
    for index in 0..binding_count as usize {
        let binding = bindings
            .checked_add(index * HL_BINDING_SIZE)
            .ok_or_else(|| "HashLink binding table overflow".to_owned())?;
        let field = hl
            .memory
            .i32(binding + HL_BINDING_FIELD)
            .filter(|field| (0..field_count).contains(field))
            .ok_or_else(|| "invalid HashLink bound field index".to_owned())?;
        let slot = field_offsets
            .checked_add(field as usize * size_of::<i32>())
            .ok_or_else(|| "HashLink field offset table overflow".to_owned())?;
        let field_offset = hl
            .memory
            .i32(slot)
            .ok_or_else(|| "could not read HashLink bound field offset".to_owned())?;
        if field_offset == offset && found.replace(binding).is_some() {
            return Err("duplicate HashLink field bindings".to_owned());
        }
    }
    found.ok_or_else(|| "HashLink field has no default binding".to_owned())
}

fn validate_signature(
    hl: &HashLink<'_>,
    member: usize,
    spec: &HashLinkMethodSpec,
) -> Result<Vec<usize>, String> {
    let function_type = hl
        .memory
        .u64(member + HL_LOOKUP_TYPE)
        .ok_or_else(|| "could not read HashLink method type".to_owned())?;
    validate_function_signature(hl, function_type, spec)
}

fn validate_function_signature(
    hl: &HashLink<'_>,
    function_type: usize,
    spec: &HashLinkMethodSpec,
) -> Result<Vec<usize>, String> {
    validate_type(
        hl,
        function_type,
        HashLinkTypeSpec::Kind(HashLinkKind::Function),
    )?;
    let function = hl
        .memory
        .u64(function_type + HL_TYPE_UNION)
        .ok_or_else(|| "could not read HashLink function metadata".to_owned())?;
    let argument_count = hl
        .memory
        .i32(function + HL_FUN_ARG_COUNT)
        .ok_or_else(|| "could not read HashLink argument count".to_owned())?;
    if argument_count != spec.arguments.len() as i32 {
        return Err(format!(
            "HashLink method argument count mismatch: expected {}, found {argument_count}",
            spec.arguments.len()
        ));
    }
    let arguments = hl
        .memory
        .u64(function + HL_FUN_ARGS)
        .ok_or_else(|| "could not read HashLink argument types".to_owned())?;
    let mut argument_types = Vec::with_capacity(spec.arguments.len());
    for (index, expected) in spec.arguments.iter().copied().enumerate() {
        let argument_type = hl
            .memory
            .u64(arguments + index * size_of::<usize>())
            .ok_or_else(|| format!("could not read HashLink argument {index}"))?;
        validate_type(hl, argument_type, expected)
            .map_err(|error| format!("HashLink argument {index} mismatch: {error}"))?;
        argument_types.push(argument_type);
    }
    let result_type = hl
        .memory
        .u64(function + HL_FUN_RETURN)
        .ok_or_else(|| "could not read HashLink return type".to_owned())?;
    validate_type(hl, result_type, spec.result)
        .map_err(|error| format!("HashLink return type mismatch: {error}"))?;
    Ok(argument_types)
}

fn validate_type(
    hl: &HashLink<'_>,
    type_address: usize,
    expected: HashLinkTypeSpec,
) -> Result<(), String> {
    let actual_kind = hl
        .memory
        .i32(type_address + HL_TYPE_KIND)
        .ok_or_else(|| "could not read type kind".to_owned())?;
    match expected {
        HashLinkTypeSpec::Kind(kind) if actual_kind == kind as i32 => Ok(()),
        HashLinkTypeSpec::Kind(kind) => Err(format!(
            "expected {}, found {}",
            kind_name(kind as i32),
            kind_name(actual_kind)
        )),
        HashLinkTypeSpec::Object(name) if actual_kind == HashLinkKind::Object as i32 => {
            let actual_name = hl
                .type_name(type_address)
                .ok_or_else(|| "object type name is unreadable".to_owned())?;
            (actual_name == name)
                .then_some(())
                .ok_or_else(|| format!("expected HOBJ {name}, found HOBJ {actual_name}"))
        }
        HashLinkTypeSpec::Object(name) => Err(format!(
            "expected HOBJ {name}, found {}",
            kind_name(actual_kind)
        )),
        HashLinkTypeSpec::Enum(name) if actual_kind == HashLinkKind::Enum as i32 => {
            let actual_name = hl
                .named_type_name(type_address)
                .ok_or_else(|| "enum type name is unreadable".to_owned())?;
            (actual_name == name)
                .then_some(())
                .ok_or_else(|| format!("expected HENUM {name}, found HENUM {actual_name}"))
        }
        HashLinkTypeSpec::Enum(name) => Err(format!(
            "expected HENUM {name}, found {}",
            kind_name(actual_kind)
        )),
        HashLinkTypeSpec::Nullable(kind) if actual_kind == HashLinkKind::Null as i32 => {
            let parameter_type = hl
                .memory
                .u64(type_address + HL_TYPE_UNION)
                .ok_or_else(|| "nullable parameter type is unreadable".to_owned())?;
            validate_type(hl, parameter_type, HashLinkTypeSpec::Kind(kind))
                .map_err(|error| format!("expected HNULL<{}>: {error}", kind_name(kind as i32)))
        }
        HashLinkTypeSpec::Nullable(kind) => Err(format!(
            "expected HNULL<{}>, found {}",
            kind_name(kind as i32),
            kind_name(actual_kind)
        )),
        HashLinkTypeSpec::Reference(kind) if actual_kind == HashLinkKind::Reference as i32 => {
            let parameter_type = hl
                .memory
                .u64(type_address + HL_TYPE_UNION)
                .ok_or_else(|| "reference parameter type is unreadable".to_owned())?;
            validate_type(hl, parameter_type, HashLinkTypeSpec::Kind(kind))
                .map_err(|error| format!("expected HREF<{}>: {error}", kind_name(kind as i32)))
        }
        HashLinkTypeSpec::Reference(kind) => Err(format!(
            "expected HREF<{}>, found {}",
            kind_name(kind as i32),
            kind_name(actual_kind)
        )),
    }
}

fn kind_name(kind: i32) -> &'static str {
    match kind {
        0 => "HVOID",
        1 => "HUI8",
        2 => "HUI16",
        3 => "HI32",
        4 => "HI64",
        5 => "HF32",
        6 => "HF64",
        7 => "HBOOL",
        8 => "HBYTES",
        9 => "HDYN",
        10 => "HFUN",
        11 => "HOBJ",
        12 => "HARRAY",
        13 => "HTYPE",
        14 => "HREF",
        15 => "HVIRTUAL",
        16 => "HDYNOBJ",
        17 => "HABSTRACT",
        18 => "HENUM",
        19 => "HNULL",
        20 => "HMETHOD",
        21 => "HSTRUCT",
        22 => "HPACKED",
        23 => "HGUID",
        _ => "unknown HashLink kind",
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
fn current_libhl_module() -> Option<usize> {
    let module_name = "libhl.dll".encode_utf16().chain([0]).collect::<Vec<_>>();
    // SAFETY: the name is NUL-terminated and readable for the duration of the call.
    let module = unsafe { GetModuleHandleW(module_name.as_ptr()) };
    (!module.is_null()).then_some(module as usize)
}

#[cfg(not(all(target_arch = "x86_64", target_os = "windows")))]
fn current_libhl_module() -> Option<usize> {
    None
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
fn libhl_export(module: usize, name: &CStr) -> Option<usize> {
    // SAFETY: `module` is a loaded libhl handle and `name` is NUL-terminated.
    unsafe { GetProcAddress(module as *mut c_void, name.as_ptr().cast()) }
        .map(|procedure| procedure as *const () as usize)
}

#[cfg(not(all(target_arch = "x86_64", target_os = "windows")))]
fn libhl_export(_module: usize, _name: &CStr) -> Option<usize> {
    None
}

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
fn is_executable_address(address: usize) -> bool {
    // SAFETY: `VirtualQuery` inspects one address and initializes `info`.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { zeroed() };
    let queried = unsafe {
        VirtualQuery(
            address as *const c_void,
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    queried != 0
        && info.State == MEM_COMMIT
        && info.Protect & (PAGE_GUARD | PAGE_NOACCESS) == 0
        && matches!(
            info.Protect & 0xff,
            PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
        )
}

#[cfg(not(all(target_arch = "x86_64", target_os = "windows")))]
fn is_executable_address(_address: usize) -> bool {
    false
}

fn function_pointer<T: Copy>(address: usize) -> T {
    assert_eq!(size_of::<T>(), size_of::<usize>());
    // SAFETY: callers provide a build-verified function export whose pointer
    // type is exactly `T`; the size assertion excludes non-pointer values.
    unsafe { std::mem::transmute_copy(&address) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TypeObservation {
    FirstSeen,
    AlreadySeen,
    Full,
}

pub(crate) struct ObservedHashLinkTypes<const SLOTS: usize, const PROBE: usize> {
    seen: [AtomicUsize; SLOTS],
    latest_objects: [AtomicUsize; SLOTS],
}

impl<const SLOTS: usize, const PROBE: usize> ObservedHashLinkTypes<SLOTS, PROBE> {
    pub(crate) const fn new() -> Self {
        Self {
            seen: [const { AtomicUsize::new(0) }; SLOTS],
            latest_objects: [const { AtomicUsize::new(0) }; SLOTS],
        }
    }

    pub(crate) fn observe(&self, type_address: usize, object: usize) -> TypeObservation {
        if SLOTS == 0 || PROBE == 0 || type_address < 0x1_0000 || object < 0x1_0000 {
            return TypeObservation::Full;
        }
        let start = type_slot::<SLOTS>(type_address);
        let mut empty = None;
        for offset in 0..PROBE.min(SLOTS) {
            let index = (start + offset) % SLOTS;
            let current = self.seen[index].load(Ordering::Relaxed);
            if current == type_address {
                self.latest_objects[index].store(object, Ordering::Release);
                return TypeObservation::AlreadySeen;
            }
            if current == 0 && empty.is_none() {
                empty = Some(index);
            }
        }
        if let Some(index) = empty {
            if self.seen[index]
                .compare_exchange(0, type_address, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                self.latest_objects[index].store(object, Ordering::Release);
                return TypeObservation::FirstSeen;
            }
            if self.seen[index].load(Ordering::Relaxed) == type_address {
                self.latest_objects[index].store(object, Ordering::Release);
                return TypeObservation::AlreadySeen;
            }
        }
        TypeObservation::Full
    }

    pub(crate) fn forget(&self, type_address: usize) {
        if SLOTS == 0 || PROBE == 0 {
            return;
        }
        let start = type_slot::<SLOTS>(type_address);
        for offset in 0..PROBE.min(SLOTS) {
            let index = (start + offset) % SLOTS;
            if self.seen[index].load(Ordering::Relaxed) != type_address {
                continue;
            }
            self.latest_objects[index].store(0, Ordering::Release);
            if self.seen[index]
                .compare_exchange(type_address, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                self.latest_objects[index].store(0, Ordering::Release);
                return;
            }
        }
    }

    pub(crate) fn take_latest(&self, type_address: usize) -> Option<usize> {
        if SLOTS == 0 || PROBE == 0 {
            return None;
        }
        let start = type_slot::<SLOTS>(type_address);
        for offset in 0..PROBE.min(SLOTS) {
            let index = (start + offset) % SLOTS;
            if self.seen[index].load(Ordering::Acquire) == type_address {
                let object = self.latest_objects[index].swap(0, Ordering::AcqRel);
                return (object != 0).then_some(object);
            }
        }
        None
    }
}

fn type_slot<const SLOTS: usize>(type_address: usize) -> usize {
    let mixed = (type_address >> 4).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (mixed ^ (mixed >> 33)) % SLOTS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::ProcessMemory;

    const DAMAGE_FIELDS: &[HashLinkFieldSpec] = &[
        HashLinkFieldSpec::object("serverSource", "ent.GameObject"),
        HashLinkFieldSpec::object("target", "ent.GameObject"),
        HashLinkFieldSpec::scalar("_amount", HashLinkKind::F64),
        HashLinkFieldSpec::scalar("_hitCount", HashLinkKind::I32),
        HashLinkFieldSpec::scalar("_block", HashLinkKind::F64),
        HashLinkFieldSpec::scalar("_kill", HashLinkKind::Bool),
        HashLinkFieldSpec::scalar("_critical", HashLinkKind::Bool),
    ];
    const DAMAGE_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
        name: "st.skill.DamageResult",
        kind: HashLinkKind::Object,
        fields: DAMAGE_FIELDS,
    };
    const TEST_METHOD_ARGUMENTS: &[HashLinkTypeSpec] = &[
        HashLinkTypeSpec::Object("ent.Unit"),
        HashLinkTypeSpec::Object("st.skill.DamageResult"),
    ];
    const TEST_METHOD: HashLinkMethodSpec = HashLinkMethodSpec {
        lookup_type: "ent.Hero",
        name: c"onInflictDamage",
        arguments: TEST_METHOD_ARGUMENTS,
        result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
    };

    #[test]
    fn observed_types_deduplicate_and_retain_the_latest_object() {
        let observed = ObservedHashLinkTypes::<64, 8>::new();
        let type_address = 0x1234_5000;
        assert_eq!(
            observed.observe(type_address, 0x1000_1000),
            TypeObservation::FirstSeen
        );
        assert_eq!(
            observed.observe(type_address, 0x1000_2000),
            TypeObservation::AlreadySeen
        );
        assert_eq!(observed.take_latest(type_address), Some(0x1000_2000));
        assert_eq!(observed.take_latest(type_address), None);
        observed.forget(type_address);
        assert_eq!(
            observed.observe(type_address, 0x1000_3000),
            TypeObservation::FirstSeen
        );
    }

    #[test]
    fn build_profiles_require_files_and_valid_hashes() {
        let empty = HashLinkBuildSpec {
            profile: "empty",
            files: &[],
        };
        assert!(verify_build(Path::new("."), &empty)
            .unwrap_err()
            .contains("contains no files"));

        let invalid_hash = HashLinkBuildSpec {
            profile: "invalid-hash",
            files: &[BuildFileSpec {
                name: "Farever.exe",
                sha256: "not-a-sha256",
            }],
        };
        assert!(verify_build(Path::new("."), &invalid_hash)
            .unwrap_err()
            .contains("invalid SHA-256"));
    }

    #[test]
    fn observed_type_hash_distributes_aligned_pointers() {
        let slots = (0..1024)
            .map(|index| type_slot::<16_384>(0x4000_0000 + index * 16))
            .collect::<std::collections::HashSet<_>>();
        assert!(slots.len() > 900, "only {} distinct slots", slots.len());
    }

    #[test]
    fn object_schema_validates_required_field_types() {
        let shape = valid_damage_result_shape();
        let layout = validate_object_shape(0x1234_5000, &shape, &DAMAGE_SCHEMA)
            .expect("valid object schema");
        assert_eq!(layout.offset("serverSource"), Some(32));
        assert_eq!(layout.field_type_address("serverSource"), Some(0x1234_0000));
        assert_eq!(layout.offset("_amount"), Some(88));

        let mut wrong_primitive = shape.clone();
        wrong_primitive
            .fields
            .iter_mut()
            .find(|field| field.name == "_amount")
            .unwrap()
            .kind = HashLinkKind::I32 as i32;
        assert!(
            validate_object_shape(0x1234_5000, &wrong_primitive, &DAMAGE_SCHEMA)
                .unwrap_err()
                .contains("field _amount expected HF64, found HI32")
        );

        let mut wrong_object = shape;
        wrong_object
            .fields
            .iter_mut()
            .find(|field| field.name == "serverSource")
            .unwrap()
            .object_type_name = Some("ent.Unit".to_owned());
        assert!(
            validate_object_shape(0x1234_5000, &wrong_object, &DAMAGE_SCHEMA)
                .unwrap_err()
                .contains("expected object type ent.GameObject, found ent.Unit")
        );
    }

    #[test]
    fn object_schema_allows_unnamed_unrelated_fields() {
        let mut shape = valid_damage_result_shape();
        shape.fields.push(field("", 120, HashLinkKind::Bool, None));

        let layout = validate_object_shape(0x1234_5000, &shape, &DAMAGE_SCHEMA)
            .expect("an anonymous field outside the projection is valid HashLink metadata");
        assert_eq!(layout.offset("serverSource"), Some(32));
        assert_eq!(layout.offset("_critical"), Some(113));
    }

    #[test]
    fn object_schema_rejects_duplicate_required_field_names() {
        let mut shape = valid_damage_result_shape();
        shape.fields.push(field(
            "serverSource",
            120,
            HashLinkKind::Object,
            Some("ent.GameObject"),
        ));

        assert_eq!(
            validate_object_shape(0x1234_5000, &shape, &DAMAGE_SCHEMA).unwrap_err(),
            "duplicate required field serverSource"
        );
    }

    #[test]
    fn object_schema_rejects_out_of_bounds_and_overlapping_fields() {
        let mut out_of_bounds = valid_damage_result_shape();
        out_of_bounds.size = 112;
        assert!(
            validate_object_shape(0x1234_5000, &out_of_bounds, &DAMAGE_SCHEMA)
                .unwrap_err()
                .contains("exceeds object size")
        );

        let mut overlapping = valid_damage_result_shape();
        overlapping
            .fields
            .iter_mut()
            .find(|field| field.name == "_critical")
            .unwrap()
            .offset = 112;
        assert!(
            validate_object_shape(0x1234_5000, &overlapping, &DAMAGE_SCHEMA)
                .unwrap_err()
                .contains("overlap in st.skill.DamageResult object storage")
        );
    }

    #[test]
    fn exact_type_guard_reads_only_the_object_header() {
        let object = [0x1234_5000_usize, 0xDEAD_BEEF];
        let pointer = object.as_ptr().cast::<c_void>();
        // SAFETY: `pointer` addresses the live local array for both calls.
        assert!(unsafe { object_has_exact_type(pointer, 0x1234_5000) });
        // SAFETY: `pointer` addresses the live local array for both calls.
        assert!(!unsafe { object_has_exact_type(pointer, 0x9999_0000) });
    }

    #[test]
    fn method_signature_validates_argument_and_return_types() {
        let unit = FakeObjectType::new("ent.Unit");
        let damage_result = FakeObjectType::new("st.skill.DamageResult");
        let mut void_type = Box::new([0_u8; 0x20]);
        write_i32(void_type.as_mut(), HL_TYPE_KIND, HashLinkKind::Void as i32);
        let arguments = Box::new([unit.address(), damage_result.address()]);
        let mut function = Box::new([0_u8; 0x20]);
        write_usize(function.as_mut(), HL_FUN_ARGS, arguments.as_ptr() as usize);
        write_usize(
            function.as_mut(),
            HL_FUN_RETURN,
            void_type.as_ptr() as usize,
        );
        write_i32(function.as_mut(), HL_FUN_ARG_COUNT, 2);
        let mut function_type = Box::new([0_u8; 0x20]);
        write_i32(
            function_type.as_mut(),
            HL_TYPE_KIND,
            HashLinkKind::Function as i32,
        );
        write_usize(
            function_type.as_mut(),
            HL_TYPE_UNION,
            function.as_ptr() as usize,
        );
        let mut member = Box::new([0_u8; 0x18]);
        write_usize(
            member.as_mut(),
            HL_LOOKUP_TYPE,
            function_type.as_ptr() as usize,
        );

        let memory = ProcessMemory::current();
        let hl = HashLink::new(&memory);
        let validated_arguments = validate_signature(&hl, member.as_ptr() as usize, &TEST_METHOD)
            .expect("matching method signature");
        assert_eq!(validated_arguments, arguments.as_slice());

        write_i32(function.as_mut(), HL_FUN_ARG_COUNT, 1);
        assert!(
            validate_signature(&hl, member.as_ptr() as usize, &TEST_METHOD)
                .unwrap_err()
                .contains("argument count mismatch")
        );
    }

    #[test]
    fn bound_fields_resolve_by_storage_offset_and_reject_invalid_tables() {
        let mut runtime = Box::new([0_u8; 0x70]);
        let offsets = [8_i32, 24, 16];
        let mut bindings = Box::new([0_u8; HL_BINDING_SIZE * 2]);
        write_i32(bindings.as_mut(), HL_BINDING_FIELD, 0);
        write_i32(bindings.as_mut(), HL_BINDING_SIZE + HL_BINDING_FIELD, 2);
        write_i32(runtime.as_mut(), HL_RUNTIME_FIELD_COUNT, 3);
        write_i32(runtime.as_mut(), HL_RUNTIME_BINDING_COUNT, 2);
        write_usize(
            runtime.as_mut(),
            HL_RUNTIME_FIELD_OFFSETS,
            offsets.as_ptr() as usize,
        );
        write_usize(
            runtime.as_mut(),
            HL_RUNTIME_BINDINGS,
            bindings.as_ptr() as usize,
        );
        let memory = ProcessMemory::current();
        let hl = HashLink::new(&memory);
        let runtime_address = runtime.as_ptr() as usize;
        assert_eq!(
            resolve_binding(&hl, runtime_address, 16),
            Ok(bindings.as_ptr() as usize + HL_BINDING_SIZE)
        );
        assert!(resolve_binding(&hl, runtime_address, 24)
            .unwrap_err()
            .contains("no default binding"));

        write_i32(bindings.as_mut(), HL_BINDING_SIZE + HL_BINDING_FIELD, 0);
        assert!(resolve_binding(&hl, runtime_address, 8)
            .unwrap_err()
            .contains("duplicate"));
        write_i32(bindings.as_mut(), HL_BINDING_SIZE + HL_BINDING_FIELD, 3);
        assert!(resolve_binding(&hl, runtime_address, 8)
            .unwrap_err()
            .contains("bound field index"));
        write_i32(runtime.as_mut(), HL_RUNTIME_BINDING_COUNT, -1);
        assert!(resolve_binding(&hl, runtime_address, 8)
            .unwrap_err()
            .contains("table count"));
    }

    #[test]
    fn bound_function_signature_includes_the_receiver_and_checks_the_return_type() {
        let receiver = FakeObjectType::new("ui.win.MapWindow");
        let wrong_receiver = FakeObjectType::new("ui.win.MapPinPicker");
        let mut scalar = Box::new([0_u8; 0x20]);
        write_i32(scalar.as_mut(), HL_TYPE_KIND, HashLinkKind::F64 as i32);
        let mut result = Box::new([0_u8; 0x20]);
        write_i32(result.as_mut(), HL_TYPE_KIND, HashLinkKind::Void as i32);
        let mut arguments = Box::new([
            receiver.address(),
            scalar.as_ptr() as usize,
            scalar.as_ptr() as usize,
        ]);
        let mut function = Box::new([0_u8; 0x20]);
        write_usize(function.as_mut(), HL_FUN_ARGS, arguments.as_ptr() as usize);
        write_usize(function.as_mut(), HL_FUN_RETURN, result.as_ptr() as usize);
        write_i32(function.as_mut(), HL_FUN_ARG_COUNT, 3);
        let mut function_type = Box::new([0_u8; 0x20]);
        write_i32(
            function_type.as_mut(),
            HL_TYPE_KIND,
            HashLinkKind::Function as i32,
        );
        write_usize(
            function_type.as_mut(),
            HL_TYPE_UNION,
            function.as_ptr() as usize,
        );
        let spec = HashLinkMethodSpec {
            lookup_type: "ui.win.MapWindow",
            name: c"onClickWorld",
            arguments: &[
                HashLinkTypeSpec::Object("ui.win.MapWindow"),
                HashLinkTypeSpec::Kind(HashLinkKind::F64),
                HashLinkTypeSpec::Kind(HashLinkKind::F64),
            ],
            result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
        };
        let memory = ProcessMemory::current();
        let hl = HashLink::new(&memory);
        let address = function_type.as_ptr() as usize;
        assert_eq!(
            validate_function_signature(&hl, address, &spec).unwrap(),
            arguments.as_slice()
        );
        arguments[0] = wrong_receiver.address();
        assert!(validate_function_signature(&hl, address, &spec)
            .unwrap_err()
            .contains("argument 0 mismatch"));
        arguments[0] = receiver.address();
        write_i32(result.as_mut(), HL_TYPE_KIND, HashLinkKind::F64 as i32);
        assert!(validate_function_signature(&hl, address, &spec)
            .unwrap_err()
            .contains("return type mismatch"));
        write_i32(function.as_mut(), HL_FUN_ARG_COUNT, 2);
        assert!(validate_function_signature(&hl, address, &spec)
            .unwrap_err()
            .contains("argument count mismatch"));
    }

    #[test]
    fn nullable_type_validation_checks_its_parameter_kind() {
        let mut f64_type = Box::new([0_u8; 0x20]);
        write_i32(f64_type.as_mut(), HL_TYPE_KIND, HashLinkKind::F64 as i32);
        let mut nullable_type = Box::new([0_u8; 0x20]);
        write_i32(
            nullable_type.as_mut(),
            HL_TYPE_KIND,
            HashLinkKind::Null as i32,
        );
        write_usize(
            nullable_type.as_mut(),
            HL_TYPE_UNION,
            f64_type.as_ptr() as usize,
        );

        let memory = ProcessMemory::current();
        let hl = HashLink::new(&memory);
        validate_type(
            &hl,
            nullable_type.as_ptr() as usize,
            HashLinkTypeSpec::Nullable(HashLinkKind::F64),
        )
        .expect("matching nullable type");
        assert!(validate_type(
            &hl,
            nullable_type.as_ptr() as usize,
            HashLinkTypeSpec::Nullable(HashLinkKind::I32),
        )
        .unwrap_err()
        .contains("expected HNULL<HI32>"));
    }

    fn valid_damage_result_shape() -> HashLinkObjectShape {
        HashLinkObjectShape {
            name: "st.skill.DamageResult".to_owned(),
            kind: HashLinkKind::Object as i32,
            size: 136,
            fields: vec![
                field(
                    "serverSource",
                    32,
                    HashLinkKind::Object,
                    Some("ent.GameObject"),
                ),
                field("target", 48, HashLinkKind::Object, Some("ent.GameObject")),
                field("_amount", 88, HashLinkKind::F64, None),
                field("_hitCount", 96, HashLinkKind::I32, None),
                field("_block", 104, HashLinkKind::F64, None),
                field("_kill", 112, HashLinkKind::Bool, None),
                field("_critical", 113, HashLinkKind::Bool, None),
            ],
        }
    }

    fn field(
        name: &str,
        offset: usize,
        kind: HashLinkKind,
        object_type_name: Option<&str>,
    ) -> HashLinkFieldShape {
        HashLinkFieldShape {
            name: name.to_owned(),
            offset,
            kind: kind as i32,
            type_address: 0x1234_0000,
            object_type_name: object_type_name.map(str::to_owned),
        }
    }

    struct FakeObjectType {
        _name: Box<[u16; 256]>,
        _descriptor: Box<[u8; 0x50]>,
        type_record: Box<[u8; 0x20]>,
    }

    impl FakeObjectType {
        fn new(name: &str) -> Self {
            let name = wide_buffer(name);
            let mut descriptor = Box::new([0_u8; 0x50]);
            write_usize(descriptor.as_mut(), 0x10, name.as_ptr() as usize);
            let mut type_record = Box::new([0_u8; 0x20]);
            write_i32(
                type_record.as_mut(),
                HL_TYPE_KIND,
                HashLinkKind::Object as i32,
            );
            write_usize(
                type_record.as_mut(),
                HL_TYPE_UNION,
                descriptor.as_ptr() as usize,
            );
            Self {
                _name: name,
                _descriptor: descriptor,
                type_record,
            }
        }

        fn address(&self) -> usize {
            self.type_record.as_ptr() as usize
        }
    }

    fn wide_buffer(value: &str) -> Box<[u16; 256]> {
        let mut buffer = Box::new([0_u16; 256]);
        for (destination, source) in buffer.iter_mut().zip(value.encode_utf16()) {
            *destination = source;
        }
        buffer
    }

    fn write_i32(bytes: &mut [u8], offset: usize, value: i32) {
        bytes[offset..offset + size_of::<i32>()].copy_from_slice(&value.to_le_bytes());
    }

    fn write_usize(bytes: &mut [u8], offset: usize, value: usize) {
        bytes[offset..offset + size_of::<usize>()].copy_from_slice(&value.to_le_bytes());
    }
}
