//! Local-only add-on output for Farever's chat window.
//!
//! Add-on callbacks stage bounded Rust strings on the runtime thread. A hook on
//! `ui.hud.ChatBox.hasFocus` drains at most a few messages whenever Farever runs
//! its chat update on the game/UI thread. Normal messages call the validated
//! `receiveMessage` path with a local structural message; errors call the
//! game's own `chatError` method.

use crate::hashlink::{
    object_has_exact_type, validate_object, validate_virtual, HashLink, HashLinkFieldSpec,
    HashLinkKind, HashLinkMethodSpec, HashLinkObjectSpec, HashLinkRuntime, HashLinkTypeSpec,
    HashLinkVirtualFieldSpec, HashLinkVirtualSpec, ValidatedHashLinkMethod,
};
use crossbeam_queue::ArrayQueue;
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
use minhook::MinHook;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

const CHAT_BOX_TYPE_NAME: &str = "ui.hud.ChatBox";
const STRING_TYPE_NAME: &str = "String";
const CHANNEL_TYPE_NAME: &str = "st.Channel";
const FRAMEWORK_SENDER_NAME: &str = "Farever-More";
pub(crate) const MAX_CHAT_CODE_UNITS: usize = 250;
const MAX_CHAT_SENDER_CODE_UNITS: usize = 96;
const OUTPUT_QUEUE_CAPACITY: usize = 128;
const MAX_OUTPUTS_PER_FRAME: usize = 4;

const HAS_FOCUS_ARGUMENTS: &[HashLinkTypeSpec] = &[HashLinkTypeSpec::Object(CHAT_BOX_TYPE_NAME)];
const HAS_FOCUS_METHOD: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: CHAT_BOX_TYPE_NAME,
    name: c"hasFocus",
    arguments: HAS_FOCUS_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Bool),
};
const RECEIVE_MESSAGE_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object(CHAT_BOX_TYPE_NAME),
    HashLinkTypeSpec::Kind(HashLinkKind::Virtual),
];
const RECEIVE_MESSAGE_METHOD: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: CHAT_BOX_TYPE_NAME,
    name: c"receiveMessage",
    arguments: RECEIVE_MESSAGE_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const CHAT_ERROR_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object(CHAT_BOX_TYPE_NAME),
    HashLinkTypeSpec::Object(STRING_TYPE_NAME),
];
const CHAT_ERROR_METHOD: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: CHAT_BOX_TYPE_NAME,
    name: c"chatError",
    arguments: CHAT_ERROR_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const STRING_FIELDS: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::scalar("bytes", HashLinkKind::Bytes),
    HashLinkFieldSpec::scalar("length", HashLinkKind::I32),
];
const STRING_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: STRING_TYPE_NAME,
    kind: HashLinkKind::Object,
    fields: STRING_FIELDS,
};
const CHAT_MESSAGE_FIELDS_STABLE: &[HashLinkVirtualFieldSpec] = &[
    HashLinkVirtualFieldSpec::new("args", HashLinkTypeSpec::Kind(HashLinkKind::Dynamic)),
    HashLinkVirtualFieldSpec::new("channel", HashLinkTypeSpec::Enum(CHANNEL_TYPE_NAME)),
    HashLinkVirtualFieldSpec::new("localStamp", HashLinkTypeSpec::Nullable(HashLinkKind::F64)),
    HashLinkVirtualFieldSpec::new("localTextId", HashLinkTypeSpec::Object(STRING_TYPE_NAME)),
    HashLinkVirtualFieldSpec::new("notify", HashLinkTypeSpec::Object(STRING_TYPE_NAME)),
    HashLinkVirtualFieldSpec::new("sender", HashLinkTypeSpec::Object("ent.Unit")),
    HashLinkVirtualFieldSpec::new("text", HashLinkTypeSpec::Object(STRING_TYPE_NAME)),
];
const CHAT_MESSAGE_SCHEMA_STABLE: HashLinkVirtualSpec = HashLinkVirtualSpec {
    label: "stable ChatBox.receiveMessage argument",
    fields: CHAT_MESSAGE_FIELDS_STABLE,
};
const CHAT_MESSAGE_FIELDS_BETA: &[HashLinkVirtualFieldSpec] = &[
    HashLinkVirtualFieldSpec::new("args", HashLinkTypeSpec::Kind(HashLinkKind::Dynamic)),
    HashLinkVirtualFieldSpec::new("channel", HashLinkTypeSpec::Enum(CHANNEL_TYPE_NAME)),
    HashLinkVirtualFieldSpec::new("localStamp", HashLinkTypeSpec::Nullable(HashLinkKind::F64)),
    HashLinkVirtualFieldSpec::new("localTextId", HashLinkTypeSpec::Object(STRING_TYPE_NAME)),
    HashLinkVirtualFieldSpec::new("notify", HashLinkTypeSpec::Object(STRING_TYPE_NAME)),
    HashLinkVirtualFieldSpec::new("sender", HashLinkTypeSpec::Kind(HashLinkKind::Virtual)),
    HashLinkVirtualFieldSpec::new("text", HashLinkTypeSpec::Object(STRING_TYPE_NAME)),
];
const CHAT_MESSAGE_SCHEMA_BETA: HashLinkVirtualSpec = HashLinkVirtualSpec {
    label: "beta ChatBox.receiveMessage argument",
    fields: CHAT_MESSAGE_FIELDS_BETA,
};
const CHAT_SENDER_FIELDS: &[HashLinkVirtualFieldSpec] = &[
    HashLinkVirtualFieldSpec::new("name", HashLinkTypeSpec::Object(STRING_TYPE_NAME)),
    HashLinkVirtualFieldSpec::new("uid", HashLinkTypeSpec::Object(STRING_TYPE_NAME)),
];
const CHAT_SENDER_SCHEMA: HashLinkVirtualSpec = HashLinkVirtualSpec {
    label: "beta ChatBox.receiveMessage sender",
    fields: CHAT_SENDER_FIELDS,
};
const LOCAL_POSITION_FIELDS: &[HashLinkVirtualFieldSpec] = &[
    HashLinkVirtualFieldSpec::new("x", HashLinkTypeSpec::Kind(HashLinkKind::F64)),
    HashLinkVirtualFieldSpec::new("y", HashLinkTypeSpec::Kind(HashLinkKind::F64)),
    HashLinkVirtualFieldSpec::new("z", HashLinkTypeSpec::Kind(HashLinkKind::F64)),
];
const LOCAL_POSITION_SCHEMA: HashLinkVirtualSpec = HashLinkVirtualSpec {
    label: "beta st.Channel.Local position",
    fields: LOCAL_POSITION_FIELDS,
};
static CHAT_BOX_TYPE: AtomicUsize = AtomicUsize::new(0);
static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_HAS_FOCUS: AtomicUsize = AtomicUsize::new(0);
static DISPATCHING: AtomicBool = AtomicBool::new(false);
static QUEUE_DROPS: AtomicU64 = AtomicU64::new(0);
static DISPATCH_FAILURES: AtomicU64 = AtomicU64::new(0);
static DELIVERED: AtomicU64 = AtomicU64::new(0);
static OUTPUTS: OnceLock<ArrayQueue<RawChatOutput>> = OnceLock::new();
static PLAYER_POSITION_VALID: AtomicBool = AtomicBool::new(false);
static PLAYER_POSITION: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static DISPATCH: OnceLock<DispatchBindings> = OnceLock::new();
static HOOK_ERROR: OnceLock<String> = OnceLock::new();

#[repr(C)]
struct HlEnumHeader {
    type_address: usize,
    index: i32,
    padding: i32,
}
type HlHasFocus = unsafe extern "C" fn(*mut c_void) -> bool;
type HlChatMethod = unsafe extern "C" fn(*mut c_void, *mut c_void);
type HlAllocObj = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type HlAllocBytes = unsafe extern "C" fn(i32) -> *mut u8;
type HlAllocEnum = unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void;
type HlAllocDynObj = unsafe extern "C" fn() -> *mut c_void;
type HlDynSetPointer = unsafe extern "C" fn(*mut c_void, i32, *mut c_void, *mut c_void);
type HlDynSetDouble = unsafe extern "C" fn(*mut c_void, i32, f64);
type HlToVirtual = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
type HlHashUtf8 = unsafe extern "C" fn(*const std::ffi::c_char) -> i32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChatOutputStyle {
    Normal,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChatOutput {
    pub(crate) style: ChatOutputStyle,
    pub(crate) text: String,
    /// The host assigns this from the emitting add-on's manifest after a
    /// successful callback. `None` identifies framework-originated output.
    pub(crate) sender_name: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct RawChatOutput {
    style: ChatOutputStyle,
    length: u16,
    code_units: [u16; MAX_CHAT_CODE_UNITS],
    sender_length: u16,
    sender_code_units: [u16; MAX_CHAT_SENDER_CODE_UNITS],
}

#[derive(Clone, Copy, Debug)]
struct StringLayout {
    string_type: usize,
    bytes: usize,
    length: usize,
}

#[derive(Clone, Copy, Debug)]
struct NullFieldBinding {
    hash: i32,
    type_address: usize,
}

#[derive(Clone, Copy, Debug)]
struct SenderBinding {
    type_address: usize,
    name_field: NullFieldBinding,
    uid_field: NullFieldBinding,
    name: RawChatOutput,
    uid: RawChatOutput,
    hash: i32,
}

#[derive(Clone, Copy, Debug)]
struct LocalPositionBinding {
    type_address: usize,
    payload_offset: usize,
    hashes: [i32; 3],
}

#[derive(Clone, Copy, Debug)]
struct DispatchBindings {
    chat_box_type: usize,
    receive_message: usize,
    chat_error: usize,
    alloc_obj: usize,
    alloc_bytes: usize,
    alloc_enum: usize,
    alloc_dynobj: usize,
    dyn_set_pointer: usize,
    dyn_set_double: usize,
    to_virtual: usize,
    message_type: usize,
    channel_type: usize,
    null_fields: [Option<NullFieldBinding>; 5],
    sender: Option<SenderBinding>,
    local_position: Option<LocalPositionBinding>,
    channel_hash: i32,
    text_hash: i32,
    string: StringLayout,
}

pub(crate) struct ChatOutputDiagnostics {
    diagnostics: Vec<String>,
    last_status: usize,
    reported_queue_drops: u64,
    reported_dispatch_failures: u64,
}

impl ChatOutputDiagnostics {
    pub(crate) fn new() -> Self {
        prepare_queue();
        Self {
            diagnostics: Vec::new(),
            last_status: 0,
            reported_queue_drops: 0,
            reported_dispatch_failures: 0,
        }
    }

    pub(crate) fn take_diagnostics(&mut self) -> Vec<String> {
        let status = HOOK_STATUS.load(Ordering::Acquire);
        if status != self.last_status {
            self.diagnostics.push(match status {
                0 => "chat output state=waiting-for-chat".to_owned(),
                1 => "chat output state=active".to_owned(),
                3 => format!(
                    "chat output state=failed error={}",
                    HOOK_ERROR.get().map_or("unknown", String::as_str)
                ),
                4 => "chat output state=installing".to_owned(),
                _ => format!("chat output state=unknown-{status}"),
            });
            self.last_status = status;
        }
        let drops = QUEUE_DROPS.load(Ordering::Acquire);
        if drops != self.reported_queue_drops {
            self.diagnostics.push(format!(
                "chat output queue dropped={} total={drops}",
                drops.saturating_sub(self.reported_queue_drops)
            ));
            self.reported_queue_drops = drops;
        }
        let failures = DISPATCH_FAILURES.load(Ordering::Acquire);
        if failures != self.reported_dispatch_failures {
            self.diagnostics.push(format!(
                "chat output dispatch failed={} total={failures}",
                failures.saturating_sub(self.reported_dispatch_failures)
            ));
            self.reported_dispatch_failures = failures;
        }
        std::mem::take(&mut self.diagnostics)
    }
}

pub(crate) fn prepare_queue() {
    let _ = OUTPUTS.get_or_init(|| ArrayQueue::new(OUTPUT_QUEUE_CAPACITY));
}

#[cfg(test)]
pub(crate) fn drain_queued_for_tests() -> Vec<ChatOutput> {
    let Some(queue) = OUTPUTS.get() else {
        return Vec::new();
    };
    let mut outputs = Vec::new();
    while let Some(raw) = queue.pop() {
        outputs.push(ChatOutput {
            style: raw.style,
            text: String::from_utf16_lossy(&raw.code_units[..raw.length as usize]),
            sender_name: (raw.sender_length > 0).then(|| {
                String::from_utf16_lossy(&raw.sender_code_units[..raw.sender_length as usize])
            }),
        });
    }
    outputs
}

pub(crate) fn enqueue(output: ChatOutput) -> Result<(), String> {
    let raw = encode_output(&output)?;
    let queue = OUTPUTS.get_or_init(|| ArrayQueue::new(OUTPUT_QUEUE_CAPACITY));
    queue.push(raw).map_err(|_| {
        QUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
        "native chat output queue is full".to_owned()
    })
}

fn encode_output(output: &ChatOutput) -> Result<RawChatOutput, String> {
    let mut raw = RawChatOutput {
        style: output.style,
        length: 0,
        code_units: [0; MAX_CHAT_CODE_UNITS],
        sender_length: 0,
        sender_code_units: [0; MAX_CHAT_SENDER_CODE_UNITS],
    };
    let mut length = 0_usize;
    for code_unit in output.text.encode_utf16() {
        if length == MAX_CHAT_CODE_UNITS {
            return Err(format!(
                "chat output exceeds {MAX_CHAT_CODE_UNITS} UTF-16 code units"
            ));
        }
        raw.code_units[length] = code_unit;
        length += 1;
    }
    raw.length = u16::try_from(length).expect("chat output length is bounded to 250");
    if let Some(sender_name) = &output.sender_name {
        let mut sender_length = 0_usize;
        for code_unit in sender_name.encode_utf16() {
            if sender_length == MAX_CHAT_SENDER_CODE_UNITS {
                return Err(format!(
                    "chat sender exceeds {MAX_CHAT_SENDER_CODE_UNITS} UTF-16 code units"
                ));
            }
            raw.sender_code_units[sender_length] = code_unit;
            sender_length += 1;
        }
        raw.sender_length = u16::try_from(sender_length).expect("chat sender length is bounded");
    }
    Ok(raw)
}

pub(crate) fn observe_player_position(valid: bool, position: [f64; 3]) {
    if !valid || position.iter().any(|value| !value.is_finite()) {
        PLAYER_POSITION_VALID.store(false, Ordering::Release);
        return;
    }
    for (slot, value) in PLAYER_POSITION.iter().zip(position) {
        slot.store(value.to_bits(), Ordering::Relaxed);
    }
    PLAYER_POSITION_VALID.store(true, Ordering::Release);
}

fn current_player_position() -> [f64; 3] {
    if !PLAYER_POSITION_VALID.load(Ordering::Acquire) {
        return [0.0; 3];
    }
    std::array::from_fn(|index| f64::from_bits(PLAYER_POSITION[index].load(Ordering::Relaxed)))
}

pub(crate) fn observe_chat_box_type(type_pointer: usize) {
    if type_pointer >= 0x1_0000 {
        CHAT_BOX_TYPE.store(type_pointer, Ordering::Release);
    }
}

pub(crate) fn try_install_hook(hl: &HashLink<'_>, profile: crate::game_build::GameBuildProfile) {
    if HOOK_STATUS.load(Ordering::Acquire) != 0 {
        return;
    }
    let chat_box_type = CHAT_BOX_TYPE.load(Ordering::Acquire);
    if chat_box_type == 0 {
        return;
    }
    if HOOK_STATUS
        .compare_exchange(0, 4, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let result = resolve_dispatch(hl, chat_box_type, profile)
        .and_then(|(has_focus, bindings)| install_hook(has_focus, bindings));
    match result {
        Ok(()) => HOOK_STATUS.store(1, Ordering::Release),
        Err(error) => {
            let _ = HOOK_ERROR.set(error);
            HOOK_STATUS.store(3, Ordering::Release);
        }
    }
}

fn resolve_dispatch(
    hl: &HashLink<'_>,
    chat_box_type: usize,
    profile: crate::game_build::GameBuildProfile,
) -> Result<(ValidatedHashLinkMethod, DispatchBindings), String> {
    let runtime = HashLinkRuntime::loaded().ok_or_else(|| "libhl.dll is not loaded".to_owned())?;
    let has_focus = runtime.resolve_method(hl, chat_box_type, &HAS_FOCUS_METHOD)?;
    let receive_message = runtime.resolve_method(hl, chat_box_type, &RECEIVE_MESSAGE_METHOD)?;
    let chat_error = runtime.resolve_method(hl, chat_box_type, &CHAT_ERROR_METHOD)?;

    let string_type = chat_error
        .argument_type(1)
        .ok_or_else(|| "validated chatError signature omitted String argument".to_owned())?;
    let string = validate_object(hl, string_type, &STRING_SCHEMA)?;
    let string = StringLayout {
        string_type: string.type_address,
        bytes: string
            .offset("bytes")
            .ok_or_else(|| "validated String layout omitted bytes".to_owned())?,
        length: string
            .offset("length")
            .ok_or_else(|| "validated String layout omitted length".to_owned())?,
    };

    let message_type = receive_message
        .argument_type(1)
        .ok_or_else(|| "validated receiveMessage signature omitted virtual argument".to_owned())?;
    let message_schema = if profile.is_beta() {
        &CHAT_MESSAGE_SCHEMA_BETA
    } else {
        &CHAT_MESSAGE_SCHEMA_STABLE
    };
    let message = validate_virtual(hl, message_type, message_schema)?;
    let channel_type = message
        .field_type_address("channel")
        .ok_or_else(|| "validated chat message omitted channel".to_owned())?;
    let text_type = message
        .field_type_address("text")
        .ok_or_else(|| "validated chat message omitted text".to_owned())?;
    if text_type != string.string_type {
        return Err("chat message and chatError use different String types".to_owned());
    }

    let hash_utf8: HlHashUtf8 = function_pointer(runtime.export(c"hl_hash_utf8")?);
    let hash = |name: &std::ffi::CStr| unsafe { hash_utf8(name.as_ptr()) };
    let local_position = if profile.is_beta() {
        let (name, payload_size, parameters) = hl
            .enum_constructor_layout(channel_type, 0)
            .ok_or_else(|| "could not read beta st.Channel.Local constructor layout".to_owned())?;
        if name != "Local" || parameters.len() != 1 {
            return Err(format!(
                "beta st.Channel constructor 0 expected Local(position), found {name} with {} parameter(s)",
                parameters.len()
            ));
        }
        let (position_type, payload_offset) = parameters[0];
        if payload_offset
            .checked_add(size_of::<usize>())
            .is_none_or(|end| end > payload_size)
        {
            return Err(
                "beta st.Channel.Local parameter pointer lies outside its payload".to_owned(),
            );
        }
        let position = validate_virtual(hl, position_type, &LOCAL_POSITION_SCHEMA)?;
        Some(LocalPositionBinding {
            type_address: position.type_address,
            payload_offset,
            hashes: [hash(c"x"), hash(c"y"), hash(c"z")],
        })
    } else {
        match hl.enum_constructor(channel_type, 0) {
            Some((name, 0)) if name == "Local" => None,
            Some((name, parameters)) => {
                return Err(format!(
                    "stable st.Channel constructor 0 expected Local(), found {name} with {parameters} parameter(s)"
                ));
            }
            None => return Err("could not validate stable st.Channel.Local constructor".to_owned()),
        }
    };

    let sender = if profile.is_beta() {
        let sender_type = message
            .field_type_address("sender")
            .ok_or_else(|| "validated beta chat message omitted sender".to_owned())?;
        let sender = validate_virtual(hl, sender_type, &CHAT_SENDER_SCHEMA)?;
        Some(SenderBinding {
            type_address: sender.type_address,
            name_field: NullFieldBinding {
                hash: hash(c"name"),
                type_address: sender
                    .field_type_address("name")
                    .ok_or_else(|| "validated beta sender omitted name".to_owned())?,
            },
            uid_field: NullFieldBinding {
                hash: hash(c"uid"),
                type_address: sender
                    .field_type_address("uid")
                    .ok_or_else(|| "validated beta sender omitted uid".to_owned())?,
            },
            name: encode_literal(FRAMEWORK_SENDER_NAME)?,
            uid: encode_literal("")?,
            hash: hash(c"sender"),
        })
    } else {
        None
    };

    let null_field_names: &[(&std::ffi::CStr, &str)] = if profile.is_beta() {
        &[
            (c"args", "args"),
            (c"localStamp", "localStamp"),
            (c"localTextId", "localTextId"),
            (c"notify", "notify"),
        ]
    } else {
        &[
            (c"args", "args"),
            (c"localStamp", "localStamp"),
            (c"localTextId", "localTextId"),
            (c"notify", "notify"),
            (c"sender", "sender"),
        ]
    };
    let mut null_fields = [None; 5];
    for (index, (name, field)) in null_field_names.iter().enumerate() {
        null_fields[index] = Some(NullFieldBinding {
            hash: hash(name),
            type_address: message
                .field_type_address(field)
                .ok_or_else(|| format!("validated chat message omitted {field}"))?,
        });
    }

    Ok((
        has_focus,
        DispatchBindings {
            chat_box_type,
            receive_message: receive_message.target(),
            chat_error: chat_error.target(),
            alloc_obj: runtime.export(c"hl_alloc_obj")?,
            alloc_bytes: runtime.export(c"hl_alloc_bytes")?,
            alloc_enum: runtime.export(c"hl_alloc_enum")?,
            alloc_dynobj: runtime.export(c"hl_alloc_dynobj")?,
            dyn_set_pointer: runtime.export(c"hl_dyn_setp")?,
            dyn_set_double: if local_position.is_some() {
                runtime.export(c"hl_dyn_setd")?
            } else {
                0
            },
            to_virtual: runtime.export(c"hl_to_virtual")?,
            message_type: message.type_address,
            channel_type,
            null_fields,
            sender,
            local_position,
            channel_hash: hash(c"channel"),
            text_hash: hash(c"text"),
            string,
        },
    ))
}
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
fn install_hook(
    has_focus: ValidatedHashLinkMethod,
    bindings: DispatchBindings,
) -> Result<(), String> {
    let target = has_focus.target() as *mut c_void;
    // SAFETY: method resolution verified `ChatBox.hasFocus(ChatBox) -> Bool`
    // against the current build before reaching MinHook.
    let original = std::panic::catch_unwind(|| unsafe {
        MinHook::create_hook(target, hook_has_focus as *mut c_void)
    })
    .map_err(|_| "MinHook initialization panicked for ChatBox.hasFocus".to_owned())?
    .map_err(|status| format!("create ChatBox.hasFocus hook returned {status:?}"))?;
    DISPATCH
        .set(bindings)
        .map_err(|_| "chat output dispatch bindings were already initialized".to_owned())?;
    ORIGINAL_HAS_FOCUS.store(original as usize, Ordering::Release);
    // SAFETY: the hook and original trampoline were created immediately above.
    if let Err(status) = unsafe { MinHook::enable_hook(target) } {
        // SAFETY: the hook exists but was not successfully enabled.
        let _ = unsafe { MinHook::remove_hook(target) };
        ORIGINAL_HAS_FOCUS.store(0, Ordering::Release);
        return Err(format!("enable ChatBox.hasFocus hook returned {status:?}"));
    }
    HOOK_TARGET.store(target as usize, Ordering::Release);
    Ok(())
}

#[cfg(not(all(target_arch = "x86_64", target_os = "windows")))]
fn install_hook(
    _has_focus: ValidatedHashLinkMethod,
    _bindings: DispatchBindings,
) -> Result<(), String> {
    Err("chat output hooks currently require Windows x86_64".to_owned())
}

pub(crate) fn shutdown_hook() {
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    {
        let target = HOOK_TARGET.load(Ordering::Acquire);
        if target != 0 {
            // SAFETY: the target is published only after the build-gated,
            // signature-checked chat output hook has been enabled.
            let _ = unsafe { MinHook::disable_hook(target as *mut c_void) };
        }
    }
    if let Some(queue) = OUTPUTS.get() {
        while queue.pop().is_some() {}
    }
}

unsafe extern "C" fn hook_has_focus(chat_box: *mut c_void) -> bool {
    let original = ORIGINAL_HAS_FOCUS.load(Ordering::Acquire);
    if original == 0 {
        return false;
    }
    // SAFETY: MinHook returned this trampoline for the validated
    // `ChatBox.hasFocus(ChatBox) -> Bool` method.
    let original: HlHasFocus = unsafe { std::mem::transmute(original) };
    let result = unsafe { original(chat_box) };

    let Some(bindings) = DISPATCH.get().copied() else {
        return result;
    };
    // SAFETY: the live receiver came from the validated HashLink callback.
    if !unsafe { object_has_exact_type(chat_box, bindings.chat_box_type) }
        || DISPATCHING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return result;
    }
    if let Some(queue) = OUTPUTS.get() {
        for _ in 0..MAX_OUTPUTS_PER_FRAME {
            let Some(output) = queue.pop() else {
                break;
            };
            // SAFETY: installation validated every target, type, and field used
            // by the bounded dispatch against the current game build. Farever is
            // already executing on its chat UI thread in this callback.
            if unsafe { dispatch_one(chat_box, output, bindings) } {
                DELIVERED.fetch_add(1, Ordering::Relaxed);
            } else {
                DISPATCH_FAILURES.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    DISPATCHING.store(false, Ordering::Release);
    result
}

unsafe fn dispatch_one(
    chat_box: *mut c_void,
    output: RawChatOutput,
    bindings: DispatchBindings,
) -> bool {
    // Stable's sender field is an ent.Unit, so it cannot carry an add-on name.
    // chatError likewise has no sender argument. Prefix those lines in bounded
    // UTF-16 while beta normal lines use the validated sender virtual.
    let (text_units, text_length) = text_for_dispatch(
        &output,
        output.style == ChatOutputStyle::Error || bindings.sender.is_none(),
    );
    let Some(text) = (unsafe { allocate_string(&text_units[..text_length], bindings) }) else {
        return false;
    };
    match output.style {
        ChatOutputStyle::Error => {
            let chat_error: HlChatMethod = function_pointer(bindings.chat_error);
            // SAFETY: the method and both object arguments were validated above.
            unsafe { chat_error(chat_box, text) };
            true
        }
        ChatOutputStyle::Normal => {
            let alloc_enum: HlAllocEnum = function_pointer(bindings.alloc_enum);
            let alloc_dynobj: HlAllocDynObj = function_pointer(bindings.alloc_dynobj);
            let dyn_set_pointer: HlDynSetPointer = function_pointer(bindings.dyn_set_pointer);
            let to_virtual: HlToVirtual = function_pointer(bindings.to_virtual);
            // SAFETY: the selected build profile validated constructor zero and its payload layout.
            let channel = unsafe { alloc_enum(bindings.channel_type as *mut c_void, 0) };
            let message = unsafe { alloc_dynobj() };
            if channel.is_null()
                || message.is_null()
                || !unsafe { object_has_exact_type(channel, bindings.channel_type) }
            {
                return false;
            }

            if let Some(position_layout) = bindings.local_position {
                let position_object = unsafe { alloc_dynobj() };
                if position_object.is_null() {
                    return false;
                }
                let set_double: HlDynSetDouble = function_pointer(bindings.dyn_set_double);
                let values = current_player_position();
                for (hash, value) in position_layout.hashes.into_iter().zip(values) {
                    unsafe { set_double(position_object, hash, value) };
                }
                let position = unsafe {
                    to_virtual(position_layout.type_address as *mut c_void, position_object)
                };
                if position.is_null()
                    || !unsafe { object_has_exact_type(position, position_layout.type_address) }
                {
                    return false;
                }
                let payload = unsafe {
                    channel
                        .cast::<u8>()
                        .add(size_of::<HlEnumHeader>() + position_layout.payload_offset)
                };
                unsafe { std::ptr::write_unaligned(payload.cast::<usize>(), position as usize) };
            }

            unsafe {
                for field in bindings.null_fields.into_iter().flatten() {
                    dyn_set_pointer(
                        message,
                        field.hash,
                        field.type_address as *mut c_void,
                        std::ptr::null_mut(),
                    );
                }
                if let Some(sender) = bindings.sender {
                    let sender_object = alloc_dynobj();
                    if sender_object.is_null() {
                        return false;
                    }
                    let sender_units = if output.sender_length > 0 {
                        &output.sender_code_units[..usize::from(output.sender_length)]
                    } else {
                        &sender.name.code_units[..usize::from(sender.name.length)]
                    };
                    let Some(sender_name) = allocate_string(sender_units, bindings) else {
                        return false;
                    };
                    let Some(sender_uid) = allocate_string(
                        &sender.uid.code_units[..usize::from(sender.uid.length)],
                        bindings,
                    ) else {
                        return false;
                    };
                    dyn_set_pointer(
                        sender_object,
                        sender.name_field.hash,
                        sender.name_field.type_address as *mut c_void,
                        sender_name,
                    );
                    dyn_set_pointer(
                        sender_object,
                        sender.uid_field.hash,
                        sender.uid_field.type_address as *mut c_void,
                        sender_uid,
                    );
                    let sender = to_virtual(sender.type_address as *mut c_void, sender_object);
                    if sender.is_null()
                        || !object_has_exact_type(sender, bindings.sender.unwrap().type_address)
                    {
                        return false;
                    }
                    dyn_set_pointer(
                        message,
                        bindings.sender.unwrap().hash,
                        bindings.sender.unwrap().type_address as *mut c_void,
                        sender,
                    );
                }
                dyn_set_pointer(
                    message,
                    bindings.channel_hash,
                    bindings.channel_type as *mut c_void,
                    channel,
                );
                dyn_set_pointer(
                    message,
                    bindings.text_hash,
                    bindings.string.string_type as *mut c_void,
                    text,
                );
            }
            let message = unsafe { to_virtual(bindings.message_type as *mut c_void, message) };
            if message.is_null()
                || !unsafe { object_has_exact_type(message, bindings.message_type) }
            {
                return false;
            }
            let receive_message: HlChatMethod = function_pointer(bindings.receive_message);
            unsafe { receive_message(chat_box, message) };
            true
        }
    }
}

fn text_for_dispatch(
    output: &RawChatOutput,
    prefix_sender: bool,
) -> ([u16; MAX_CHAT_CODE_UNITS], usize) {
    let mut units = [0_u16; MAX_CHAT_CODE_UNITS];
    let mut length = 0;
    if prefix_sender && output.sender_length > 0 {
        let sender_length = usize::from(output.sender_length);
        units[..sender_length].copy_from_slice(&output.sender_code_units[..sender_length]);
        length = sender_length;
        units[length] = u16::from(b':');
        units[length + 1] = u16::from(b' ');
        length += 2;
    }
    let mut text_length = usize::from(output.length).min(MAX_CHAT_CODE_UNITS - length);
    if text_length < usize::from(output.length)
        && text_length > 0
        && (0xD800..=0xDBFF).contains(&output.code_units[text_length - 1])
    {
        text_length -= 1;
    }
    units[length..length + text_length].copy_from_slice(&output.code_units[..text_length]);
    (units, length + text_length)
}

unsafe fn allocate_string(units: &[u16], bindings: DispatchBindings) -> Option<*mut c_void> {
    let alloc_obj: HlAllocObj = function_pointer(bindings.alloc_obj);
    let alloc_bytes: HlAllocBytes = function_pointer(bindings.alloc_bytes);
    // SAFETY: both exports and the concrete String type were build-validated.
    let string = unsafe { alloc_obj(bindings.string.string_type as *mut c_void) };
    let byte_count = (units.len() + 1).checked_mul(size_of::<u16>())?;
    let byte_count = i32::try_from(byte_count).ok()?;
    // SAFETY: the bounded byte count includes one UTF-16 terminator.
    let bytes = unsafe { alloc_bytes(byte_count) };
    if string.is_null() || bytes.is_null() {
        return None;
    }
    // SAFETY: `bytes` is a fresh HashLink allocation large enough for the
    // bounded source and terminator. The object offsets came from String shape
    // validation before the dispatch hook was enabled.
    unsafe {
        std::ptr::copy_nonoverlapping(units.as_ptr(), bytes.cast::<u16>(), units.len());
        bytes.cast::<u16>().add(units.len()).write(0);
        std::ptr::write_unaligned(
            string
                .cast::<u8>()
                .add(bindings.string.bytes)
                .cast::<usize>(),
            bytes as usize,
        );
        std::ptr::write_unaligned(
            string
                .cast::<u8>()
                .add(bindings.string.length)
                .cast::<i32>(),
            i32::try_from(units.len()).ok()?,
        );
    }
    Some(string)
}

fn encode_literal(text: &str) -> Result<RawChatOutput, String> {
    encode_output(&ChatOutput {
        style: ChatOutputStyle::Normal,
        text: text.to_owned(),
        sender_name: None,
    })
}

fn function_pointer<T: Copy>(address: usize) -> T {
    assert_eq!(size_of::<T>(), size_of::<usize>());
    // SAFETY: every caller supplies a build-verified HashLink function target
    // whose C ABI is exactly `T`.
    unsafe { std::mem::transmute_copy(&address) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_encoding_counts_utf16_code_units() {
        let output = ChatOutput {
            style: ChatOutputStyle::Normal,
            text: format!("{}😀", "a".repeat(248)),
            sender_name: None,
        };
        let raw = encode_output(&output).expect("250 UTF-16 code units");
        assert_eq!(raw.length, 250);

        let oversized = ChatOutput {
            style: ChatOutputStyle::Error,
            text: format!("{}😀", "a".repeat(249)),
            sender_name: None,
        };
        assert!(encode_output(&oversized).is_err());
    }
}
