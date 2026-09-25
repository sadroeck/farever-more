#![deny(clippy::missing_safety_doc, clippy::undocumented_unsafe_blocks)]

//! Exact-export `dinput8.dll` proxy used to bootstrap Farever More.
//!
//! Every public function preserves the Windows `DirectInput` ABI and forwards to
//! the copy in the system directory. The module never searches the ordinary DLL
//! load path, which prevents it from recursively loading itself.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{HINSTANCE, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

const E_FAIL: i32 = -2_147_467_259;
static SYSTEM_DINPUT8: OnceLock<usize> = OnceLock::new();

type DirectInput8CreateFn =
    unsafe extern "system" fn(HINSTANCE, u32, *const GUID, *mut *mut c_void, *mut c_void) -> i32;
type DllCanUnloadNowFn = unsafe extern "system" fn() -> i32;
type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> i32;
type DllSimpleFn = unsafe extern "system" fn() -> i32;
type GetdfDIJoystickFn = unsafe extern "system" fn() -> *const c_void;

#[no_mangle]
/// Forwards `DirectInput8Create` to the system `DirectInput` implementation.
///
/// # Safety
///
/// All pointer arguments must satisfy the contract of the Windows
/// `DirectInput8Create` export. They are passed through without dereferencing.
pub unsafe extern "system" fn DirectInput8Create(
    instance: HINSTANCE,
    version: u32,
    iid: *const GUID,
    output: *mut *mut c_void,
    outer: *mut c_void,
) -> i32 {
    // SAFETY: the symbol name is NUL-terminated and names the exact export
    // whose ABI is represented by `DirectInput8CreateFn`.
    let Some(address) = (unsafe { system_symbol(b"DirectInput8Create\0") }) else {
        return E_FAIL;
    };
    // SAFETY: Windows guarantees this export uses the declared DirectInput ABI.
    let function: DirectInput8CreateFn = unsafe { std::mem::transmute(address) };
    // SAFETY: the caller owns the underlying Windows pointer contract.
    unsafe { function(instance, version, iid, output, outer) }
}

#[no_mangle]
/// Forwards `DllCanUnloadNow` to the system `DirectInput` implementation.
///
/// # Safety
///
/// Must be invoked through the exported Windows system ABI.
pub unsafe extern "system" fn DllCanUnloadNow() -> i32 {
    // SAFETY: the static symbol name is NUL-terminated and has the ABI below.
    let Some(address) = (unsafe { system_symbol(b"DllCanUnloadNow\0") }) else {
        return 1;
    };
    // SAFETY: Windows guarantees this export uses the declared COM ABI.
    let function: DllCanUnloadNowFn = unsafe { std::mem::transmute(address) };
    // SAFETY: the function has no pointer arguments or additional preconditions.
    unsafe { function() }
}

#[no_mangle]
/// Forwards `DllGetClassObject` to the system `DirectInput` implementation.
///
/// # Safety
///
/// `class`, `iid`, and `output` must satisfy the Windows `DllGetClassObject`
/// contract. The pointers are passed through without dereferencing.
pub unsafe extern "system" fn DllGetClassObject(
    class: *const GUID,
    iid: *const GUID,
    output: *mut *mut c_void,
) -> i32 {
    // SAFETY: the static symbol name is NUL-terminated and has the ABI below.
    let Some(address) = (unsafe { system_symbol(b"DllGetClassObject\0") }) else {
        return E_FAIL;
    };
    // SAFETY: Windows guarantees this export uses the declared COM ABI.
    let function: DllGetClassObjectFn = unsafe { std::mem::transmute(address) };
    // SAFETY: the caller owns the underlying Windows pointer contract.
    unsafe { function(class, iid, output) }
}

#[no_mangle]
/// Forwards the optional COM registration entry point.
///
/// # Safety
///
/// Must be invoked through the exported Windows system ABI.
pub unsafe extern "system" fn DllRegisterServer() -> i32 {
    // SAFETY: the static symbol name is NUL-terminated and identifies a no-arg
    // HRESULT-returning export.
    forward_simple(b"DllRegisterServer\0")
}

#[no_mangle]
/// Forwards the optional COM unregistration entry point.
///
/// # Safety
///
/// Must be invoked through the exported Windows system ABI.
pub unsafe extern "system" fn DllUnregisterServer() -> i32 {
    // SAFETY: the static symbol name is NUL-terminated and identifies a no-arg
    // HRESULT-returning export.
    forward_simple(b"DllUnregisterServer\0")
}

#[no_mangle]
/// Forwards the `DirectInput` joystick data-format accessor.
///
/// # Safety
///
/// Must be invoked through the exported Windows system ABI. The returned
/// pointer remains owned by the system `DirectInput` module.
pub unsafe extern "system" fn GetdfDIJoystick() -> *const c_void {
    // SAFETY: the static symbol name is NUL-terminated and has the ABI below.
    let Some(address) = (unsafe { system_symbol(b"GetdfDIJoystick\0") }) else {
        return std::ptr::null();
    };
    // SAFETY: Windows guarantees this export uses the declared DirectInput ABI.
    let function: GetdfDIJoystickFn = unsafe { std::mem::transmute(address) };
    // SAFETY: the function has no pointer arguments or additional preconditions.
    unsafe { function() }
}

unsafe fn forward_simple(name: &[u8]) -> i32 {
    // SAFETY: callers provide a NUL-terminated name for a no-argument export.
    let Some(address) = (unsafe { system_symbol(name) }) else {
        return E_FAIL;
    };
    // SAFETY: `forward_simple` is used only for exports with `DllSimpleFn` ABI.
    let function: DllSimpleFn = unsafe { std::mem::transmute(address) };
    // SAFETY: the forwarded function has no caller-provided pointers.
    unsafe { function() }
}

// `name` must be NUL-terminated and the caller must cast the result only to the
// signature belonging to that exact export name.
unsafe fn system_symbol(name: &[u8]) -> Option<unsafe extern "system" fn() -> isize> {
    let module = *SYSTEM_DINPUT8.get_or_init(|| load_system_dinput8() as usize) as HMODULE;
    if module.is_null() {
        return None;
    }
    // SAFETY: the function contract requires a NUL-terminated symbol name, and
    // `module` is a successfully loaded system DLL handle.
    unsafe { GetProcAddress(module, name.as_ptr()) }
}

fn load_system_dinput8() -> HMODULE {
    let mut directory = vec![0_u16; 32_768];
    let capacity = u32::try_from(directory.len()).expect("system path buffer fits in u32");
    // SAFETY: `directory` provides writable capacity for the length passed to
    // Windows; the return value is checked before any initialized prefix is read.
    let length = unsafe { GetSystemDirectoryW(directory.as_mut_ptr(), capacity) };
    if length == 0 || length as usize >= directory.len() {
        return std::ptr::null_mut();
    }
    directory.truncate(length as usize);
    let mut path = PathBuf::from(String::from_utf16_lossy(&directory));
    path.push("dinput8.dll");
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    // SAFETY: `wide` is NUL-terminated and names an absolute path under the
    // Windows system directory, preventing recursive proxy loading.
    unsafe { LoadLibraryW(wide.as_ptr()) }
}
