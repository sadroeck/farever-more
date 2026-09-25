//! Native "open file" picker for add-on archives.
//!
//! On Windows this shows the standard open dialog filtered to `*.zip` via
//! `GetOpenFileNameW`. Other platforms have no picker wired up yet and get
//! `None`: the install row still accepts a typed or pasted path, so the
//! archive flow keeps working everywhere.

use std::path::PathBuf;

/// Opens the archive picker. Returns the chosen `.zip` path, or `None` when
/// the user cancels (or the platform has no picker).
pub(crate) fn pick_archive() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        pick_archive_windows()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(windows)]
fn pick_archive_windows() -> Option<PathBuf> {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::Controls::Dialogs::*;

    const BUFFER_LEN: usize = 4096;
    // Double-NUL-terminated pairs of label + pattern; index 1 is the default.
    let filter: Vec<u16> = "Add-on archives (*.zip)\0*.zip\0All files (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title: Vec<u16> = "Install add-on archive\0".encode_utf16().collect();
    let default_ext: Vec<u16> = "zip\0".encode_utf16().collect();
    let mut file_buffer = vec![0u16; BUFFER_LEN];

    let mut dialog: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = 0 as HWND;
    dialog.lpstrFilter = filter.as_ptr();
    dialog.nFilterIndex = 1;
    dialog.lpstrFile = file_buffer.as_mut_ptr();
    dialog.nMaxFile = BUFFER_LEN as u32;
    dialog.lpstrTitle = title.as_ptr();
    dialog.lpstrDefExt = default_ext.as_ptr();
    dialog.Flags = OFN_EXPLORER
        | OFN_FILEMUSTEXIST
        | OFN_PATHMUSTEXIST
        | OFN_NOCHANGEDIR
        | OFN_DONTADDTORECENT;

    let chosen = unsafe { GetOpenFileNameW(&mut dialog) };
    if chosen == 0 {
        return None;
    }
    let end = file_buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(BUFFER_LEN);
    let path = String::from_utf16_lossy(&file_buffer[..end]);
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}
