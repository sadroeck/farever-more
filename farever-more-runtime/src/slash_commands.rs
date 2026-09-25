//! Native chat slash-command interception for the supported Farever build.
//!
//! `ui.hud.ChatBox.processMessage` receives the original text before Farever
//! trims or routes it to `ChatClient.sendMessage`. The detour recognizes only
//! a slash in column zero, copies the bounded HashLink string into a lock-free
//! queue, and returns without calling the original method. Parsing, topic
//! validation, diagnostics, and Wasm delivery stay on the runtime thread.

use crate::hashlink::{
    object_has_exact_type, validate_object, HashLink, HashLinkFieldSpec, HashLinkKind,
    HashLinkMethodSpec, HashLinkObjectSpec, HashLinkRuntime, HashLinkTypeSpec,
    ValidatedHashLinkMethod,
};
use crossbeam_queue::ArrayQueue;
use minhook::MinHook;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

const CHAT_BOX_TYPE_NAME: &str = "ui.hud.ChatBox";
const MAX_CHAT_CODE_UNITS: usize = 250;
const COMMAND_QUEUE_CAPACITY: usize = 64;

const PROCESS_MESSAGE_ARGUMENTS: &[HashLinkTypeSpec] = &[
    HashLinkTypeSpec::Object(CHAT_BOX_TYPE_NAME),
    HashLinkTypeSpec::Object("String"),
];
const PROCESS_MESSAGE_METHOD: HashLinkMethodSpec = HashLinkMethodSpec {
    lookup_type: CHAT_BOX_TYPE_NAME,
    name: c"processMessage",
    arguments: PROCESS_MESSAGE_ARGUMENTS,
    result: HashLinkTypeSpec::Kind(HashLinkKind::Void),
};
const STRING_FIELDS: &[HashLinkFieldSpec] = &[
    HashLinkFieldSpec::scalar("bytes", HashLinkKind::Bytes),
    HashLinkFieldSpec::scalar("length", HashLinkKind::I32),
];
const STRING_SCHEMA: HashLinkObjectSpec = HashLinkObjectSpec {
    name: "String",
    kind: HashLinkKind::Object,
    fields: STRING_FIELDS,
};

static CHAT_BOX_TYPE: AtomicUsize = AtomicUsize::new(0);
static HOOK_STATUS: AtomicUsize = AtomicUsize::new(0);
static HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_PROCESS_MESSAGE: AtomicUsize = AtomicUsize::new(0);
static INTERCEPTED: AtomicU64 = AtomicU64::new(0);
static QUEUE_DROPS: AtomicU64 = AtomicU64::new(0);
static INVALID_COPIES: AtomicU64 = AtomicU64::new(0);
static ENABLED: AtomicBool = AtomicBool::new(true);
static COMMANDS: OnceLock<ArrayQueue<RawSlashCommand>> = OnceLock::new();
static STRING_LAYOUT: OnceLock<StringLayout> = OnceLock::new();
static HOOK_ERROR: OnceLock<String> = OnceLock::new();

type HlProcessMessage = unsafe extern "C" fn(*mut c_void, *mut c_void);

#[derive(Clone, Copy, Debug)]
struct StringLayout {
    string_type: usize,
    bytes: usize,
    length: usize,
}

#[derive(Clone, Copy)]
struct RawSlashCommand {
    length: u16,
    code_units: [u16; MAX_CHAT_CODE_UNITS],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SlashCommand {
    pub(crate) topic: String,
    pub(crate) payload: Vec<u8>,
}

pub(crate) struct SlashCommandCapture {
    diagnostics: Vec<String>,
    last_status: usize,
    reported_queue_drops: u64,
    reported_invalid_copies: u64,
}

impl SlashCommandCapture {
    pub(crate) fn new() -> Self {
        prepare_queue();
        Self {
            diagnostics: Vec::new(),
            last_status: 0,
            reported_queue_drops: 0,
            reported_invalid_copies: 0,
        }
    }

    pub(crate) fn drain(&mut self) -> Vec<SlashCommand> {
        self.update_diagnostics();
        let mut commands = Vec::new();
        let Some(queue) = COMMANDS.get() else {
            return commands;
        };
        if !ENABLED.load(Ordering::Acquire) {
            while queue.pop().is_some() {}
            return commands;
        }
        while let Some(raw) = queue.pop() {
            let units = &raw.code_units[..usize::from(raw.length)];
            match String::from_utf16(units) {
                Ok(message) => match parse_slash_command(&message) {
                    Ok(command) => commands.push(command),
                    Err(error) => self.diagnostics.push(error),
                },
                Err(_) => self
                    .diagnostics
                    .push("slash command rejected: chat text was not valid UTF-16".to_owned()),
            }
        }
        commands
    }

    pub(crate) fn take_diagnostics(&mut self) -> Vec<String> {
        self.update_diagnostics();
        std::mem::take(&mut self.diagnostics)
    }

    fn update_diagnostics(&mut self) {
        let status = HOOK_STATUS.load(Ordering::Acquire);
        if status != self.last_status {
            self.diagnostics.push(match status {
                0 => "slash command capture state=waiting-for-chat".to_owned(),
                1 => "slash command capture state=active".to_owned(),
                3 => format!(
                    "slash command capture state=failed error={}",
                    HOOK_ERROR.get().map_or("unknown", String::as_str)
                ),
                4 => "slash command capture state=installing".to_owned(),
                _ => format!("slash command capture state=unknown-{status}"),
            });
            self.last_status = status;
        }
        let drops = QUEUE_DROPS.load(Ordering::Acquire);
        if drops != self.reported_queue_drops {
            self.diagnostics.push(format!(
                "slash command queue dropped={} total={drops}",
                drops.saturating_sub(self.reported_queue_drops)
            ));
            self.reported_queue_drops = drops;
        }
        let invalid = INVALID_COPIES.load(Ordering::Acquire);
        if invalid != self.reported_invalid_copies {
            self.diagnostics.push(format!(
                "slash command copies rejected={} total={invalid}",
                invalid.saturating_sub(self.reported_invalid_copies)
            ));
            self.reported_invalid_copies = invalid;
        }
    }
}

pub(crate) fn prepare_queue() {
    let _ = COMMANDS.get_or_init(|| ArrayQueue::new(COMMAND_QUEUE_CAPACITY));
}

pub(crate) fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Release);
}

pub(crate) fn observe_chat_box_type(type_pointer: usize) {
    if type_pointer >= 0x1_0000 {
        CHAT_BOX_TYPE.store(type_pointer, Ordering::Release);
    }
}

pub(crate) fn try_install_hook(hl: &HashLink<'_>) {
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
    let result = HashLinkRuntime::loaded()
        .ok_or_else(|| "libhl.dll is not loaded".to_owned())
        .and_then(|runtime| runtime.resolve_method(hl, chat_box_type, &PROCESS_MESSAGE_METHOD))
        .and_then(|method| resolve_string_layout(hl, &method).map(|layout| (method, layout)))
        .and_then(|(method, layout)| {
            let _ = STRING_LAYOUT.set(layout);
            install_hook(method)
        });
    match result {
        Ok(()) => HOOK_STATUS.store(1, Ordering::Release),
        Err(error) => {
            let _ = HOOK_ERROR.set(error);
            HOOK_STATUS.store(3, Ordering::Release);
        }
    }
}

pub(crate) fn shutdown_hook() {
    let target = HOOK_TARGET.load(Ordering::Acquire);
    if target != 0 {
        // SAFETY: the target is published only after the build-gated,
        // signature-checked chat hook has been created and enabled.
        let _ = unsafe { MinHook::disable_hook(target as *mut c_void) };
    }
}

fn resolve_string_layout(
    hl: &HashLink<'_>,
    method: &ValidatedHashLinkMethod,
) -> Result<StringLayout, String> {
    let string_type = method
        .argument_type(1)
        .ok_or_else(|| "validated processMessage signature omitted String argument".to_owned())?;
    let string = validate_object(hl, string_type, &STRING_SCHEMA)?;
    Ok(StringLayout {
        string_type: string.type_address,
        bytes: string
            .offset("bytes")
            .ok_or_else(|| "validated String layout omitted bytes".to_owned())?,
        length: string
            .offset("length")
            .ok_or_else(|| "validated String layout omitted length".to_owned())?,
    })
}

fn install_hook(method: ValidatedHashLinkMethod) -> Result<(), String> {
    let target = method.target() as *mut c_void;
    // SAFETY: resolution is build-gated, name-addressed, and verifies the full
    // `(ChatBox, String) -> Void` HashLink signature before reaching this call.
    let original = std::panic::catch_unwind(|| unsafe {
        MinHook::create_hook(target, hook_process_message as *mut c_void)
    })
    .map_err(|_| {
        format!(
            "MinHook initialization panicked for {}",
            method.name().to_string_lossy()
        )
    })?
    .map_err(|status| {
        format!(
            "create {} hook returned {status:?}",
            method.name().to_string_lossy()
        )
    })?;
    ORIGINAL_PROCESS_MESSAGE.store(original as usize, Ordering::Release);
    // SAFETY: the hook and original trampoline were created immediately above.
    if let Err(status) = unsafe { MinHook::enable_hook(target) } {
        // SAFETY: the hook exists but was not successfully enabled.
        let _ = unsafe { MinHook::remove_hook(target) };
        ORIGINAL_PROCESS_MESSAGE.store(0, Ordering::Release);
        return Err(format!(
            "enable {} hook returned {status:?}",
            method.name().to_string_lossy()
        ));
    }
    HOOK_TARGET.store(target as usize, Ordering::Release);
    Ok(())
}

unsafe extern "C" fn hook_process_message(chat_box: *mut c_void, message: *mut c_void) {
    // SAFETY: installation validates this callback's exact two-object-argument
    // signature and String layout. The bounded copy is made while HashLink owns
    // both live callback arguments.
    if unsafe { capture_if_slash_command(message) } {
        return;
    }

    let original = ORIGINAL_PROCESS_MESSAGE.load(Ordering::Acquire);
    if original != 0 {
        // SAFETY: MinHook returned this trampoline for the validated
        // `(ui.hud.ChatBox, String) -> Void` target.
        let original: HlProcessMessage = unsafe { std::mem::transmute(original) };
        unsafe { original(chat_box, message) };
    }
}

/// Returns true only when the original message starts with `/`. Once that
/// decision is made the caller must suppress normal chat, even if the bounded
/// queue is full or later parsing rejects the command name.
unsafe fn capture_if_slash_command(message: *mut c_void) -> bool {
    if !ENABLED.load(Ordering::Acquire) {
        return false;
    }
    let Some(layout) = STRING_LAYOUT.get().copied() else {
        return false;
    };
    if !unsafe { object_has_exact_type(message, layout.string_type) } {
        INVALID_COPIES.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let base = message.cast::<u8>();
    // SAFETY: both offsets were shape-validated before the hook was enabled.
    let length = unsafe { std::ptr::read_unaligned(base.add(layout.length).cast::<i32>()) };
    if length <= 0 {
        return false;
    }
    // SAFETY: same validated String layout as above.
    let bytes = unsafe { std::ptr::read_unaligned(base.add(layout.bytes).cast::<usize>()) };
    if bytes < 0x1_0000 {
        INVALID_COPIES.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    // SAFETY: a positive live String length guarantees at least one UTF-16
    // code unit at its validated bytes pointer.
    if unsafe { std::ptr::read_unaligned(bytes as *const u16) } != u16::from(b'/') {
        return false;
    }

    INTERCEPTED.fetch_add(1, Ordering::Relaxed);
    let Ok(length) = usize::try_from(length) else {
        INVALID_COPIES.fetch_add(1, Ordering::Relaxed);
        return true;
    };
    if length > MAX_CHAT_CODE_UNITS {
        INVALID_COPIES.fetch_add(1, Ordering::Relaxed);
        return true;
    }
    let mut raw = RawSlashCommand {
        length: u16::try_from(length).unwrap_or(u16::MAX),
        code_units: [0; MAX_CHAT_CODE_UNITS],
    };
    // SAFETY: length is capped to the fixed destination and the source is the
    // live String buffer for the duration of this callback.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes as *const u16, raw.code_units.as_mut_ptr(), length);
    }
    if COMMANDS.get().is_none_or(|queue| queue.push(raw).is_err()) {
        QUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
    }
    true
}

fn parse_slash_command(message: &str) -> Result<SlashCommand, String> {
    let body = message
        .strip_prefix('/')
        .ok_or_else(|| "slash command parser received non-command text".to_owned())?;
    let command_end = body.find(' ').unwrap_or(body.len());
    let topic = &body[..command_end];
    let payload = body[command_end..].trim_start_matches(' ');
    if topic.is_empty() {
        return Err("slash command rejected: command topic is empty".to_owned());
    }
    Ok(SlashCommand {
        topic: topic.to_owned(),
        payload: payload.as_bytes().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_topic_and_utf8_payload_without_the_implied_space() {
        assert_eq!(
            parse_slash_command("/gps  1 2 "),
            Ok(SlashCommand {
                topic: "gps".to_owned(),
                payload: b"1 2 ".to_vec(),
            })
        );
    }

    #[test]
    fn preserves_payload_after_leading_separator_spaces() {
        assert_eq!(
            parse_slash_command("/note   meet  here"),
            Ok(SlashCommand {
                topic: "note".to_owned(),
                payload: b"meet  here".to_vec(),
            })
        );
    }

    #[test]
    fn supports_empty_payload_and_rejects_an_empty_topic() {
        assert_eq!(
            parse_slash_command("/gps"),
            Ok(SlashCommand {
                topic: "gps".to_owned(),
                payload: Vec::new(),
            })
        );
        assert!(parse_slash_command("/").is_err());
        assert!(parse_slash_command("/ 1 2").is_err());
    }

    #[test]
    fn does_not_treat_a_leading_space_as_a_command() {
        assert!(parse_slash_command(" /gps 1 2").is_err());
    }

    #[test]
    fn disabled_interception_forwards_without_reading_the_message() {
        set_enabled(false);
        // SAFETY: the disabled branch returns before dereferencing the pointer.
        assert!(!unsafe { capture_if_slash_command(std::ptr::null_mut()) });
        set_enabled(true);
    }
}
