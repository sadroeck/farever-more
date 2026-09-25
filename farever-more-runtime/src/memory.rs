//! Guarded, read-only Windows process-memory access.
//!
//! Callers receive copied bytes only. Address validity is treated as transient:
//! every read may fail as `HashLink` allocates, frees, or moves objects.

use memchr::memmem::Finder;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Memory::{
    VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_READWRITE, PAGE_WRITECOPY,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
};

#[derive(Clone, Copy, Debug)]
/// One committed virtual-memory range eligible for scanning.
pub struct Region {
    pub base: usize,
    pub size: usize,
    pub writable: bool,
}

/// Aggregate result and cost counters for one memory search.
pub struct SearchResult {
    pub hits: Vec<usize>,
    pub bytes_read: usize,
    pub read_failures: usize,
}

/// Read-only process handle with bounded primitive and region-scan helpers.
pub struct ProcessMemory {
    handle: HANDLE,
    pid: u32,
    owned: bool,
}

impl ProcessMemory {
    /// Uses the current-process pseudo-handle, which must not be closed.
    pub fn current() -> Self {
        Self {
            handle: unsafe { GetCurrentProcess() },
            pid: unsafe { GetCurrentProcessId() },
            owned: false,
        }
    }

    /// Opens another process with query and read permissions only.
    pub fn open(pid: u32) -> Option<Self> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
        if handle.is_null() {
            return None;
        }
        Some(Self {
            handle,
            pid,
            owned: true,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Returns committed private bytes using one cheap kernel query. This is
    /// used only to avoid starting the expensive HashLink scan while the game
    /// is still rapidly allocating during boot or a loading transition.
    pub fn private_bytes(&self) -> Option<usize> {
        let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { zeroed() };
        counters.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let ok = unsafe {
            K32GetProcessMemoryInfo(
                self.handle,
                (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX)
                    .cast::<PROCESS_MEMORY_COUNTERS>(),
                counters.cb,
            )
        };
        (ok != 0).then_some(counters.PrivateUsage)
    }

    /// Reads exactly `size` bytes or returns `None` on a partial/failed read.
    ///
    /// Reads are capped at 128 MiB to prevent a corrupted runtime length from
    /// becoming an unbounded host allocation.
    pub fn read(&self, address: usize, size: usize) -> Option<Vec<u8>> {
        if address < 0x1_0000 || size > 128 * 1024 * 1024 {
            return None;
        }
        let mut bytes = vec![0_u8; size];
        self.read_into(address, &mut bytes).then_some(bytes)
    }

    pub(crate) fn read_into(&self, address: usize, bytes: &mut [u8]) -> bool {
        if address < 0x1_0000 || bytes.len() > 128 * 1024 * 1024 {
            return false;
        }
        let mut read = 0_usize;
        let ok = unsafe {
            ReadProcessMemory(
                self.handle,
                address as *const c_void,
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                &mut read,
            )
        };
        ok != 0 && read == bytes.len()
    }

    pub fn u64(&self, address: usize) -> Option<usize> {
        let bytes = self.read(address, 8)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?) as usize)
    }

    pub fn i32(&self, address: usize) -> Option<i32> {
        let bytes = self.read(address, 4)?;
        Some(i32::from_le_bytes(bytes.try_into().ok()?))
    }

    pub fn u8(&self, address: usize) -> Option<u8> {
        let mut byte = [0_u8; 1];
        self.read_into(address, &mut byte).then_some(byte[0])
    }

    pub fn f64(&self, address: usize) -> Option<f64> {
        let bytes = self.read(address, 8)?;
        Some(f64::from_le_bytes(bytes.try_into().ok()?))
    }

    /// Enumerates committed, accessible regions in ascending address order.
    pub fn regions(&self) -> Vec<Region> {
        let mut result = Vec::new();
        let mut address = 0_usize;
        loop {
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { zeroed() };
            let queried = unsafe {
                VirtualQueryEx(
                    self.handle,
                    address as *const c_void,
                    &mut info,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if queried == 0 {
                break;
            }
            let base = info.BaseAddress as usize;
            let size = info.RegionSize;
            if info.State == MEM_COMMIT
                && info.Protect & (PAGE_GUARD | PAGE_NOACCESS) == 0
                && size > 0
            {
                let writable = matches!(
                    info.Protect & 0xff,
                    PAGE_READWRITE
                        | PAGE_WRITECOPY
                        | PAGE_EXECUTE_READWRITE
                        | PAGE_EXECUTE_WRITECOPY
                );
                result.push(Region {
                    base,
                    size,
                    writable,
                });
            }
            let next = base.saturating_add(size.max(0x1000));
            if next <= address {
                break;
            }
            address = next;
        }
        result
    }

    /// Finds at most `max_hits` byte matches within the supplied ranges.
    pub fn find_bytes_in(&self, needle: &[u8], ranges: &[Region], max_hits: usize) -> SearchResult {
        self.find_bytes_in_with_progress(needle, ranges, max_hits, |_| true)
    }

    /// Scans `ranges` and reports cumulative bytes read after every chunk.
    /// Returning false from `on_progress` cancels the remaining work.
    pub fn find_bytes_in_with_progress<F>(
        &self,
        needle: &[u8],
        ranges: &[Region],
        max_hits: usize,
        mut on_progress: F,
    ) -> SearchResult
    where
        F: FnMut(usize) -> bool,
    {
        let mut result = SearchResult {
            hits: Vec::new(),
            bytes_read: 0,
            read_failures: 0,
        };
        if needle.is_empty() {
            return result;
        }
        let finder = Finder::new(needle);
        const CHUNK: usize = 4 * 1024 * 1024;
        let overlap = needle.len().saturating_sub(1);
        // Reuse one buffer for the complete scan. The overlap at its front
        // catches matches spanning a chunk boundary without copying each 4 MiB
        // read into a second temporary allocation.
        let mut buffer = vec![0_u8; CHUNK + overlap];
        for region in ranges {
            let mut offset = 0_usize;
            let mut tail_len = 0_usize;
            while offset < region.size {
                let amount = CHUNK.min(region.size - offset);
                let end = tail_len + amount;
                if !self.read_into(region.base + offset, &mut buffer[tail_len..end]) {
                    result.read_failures += 1;
                    break;
                }
                result.bytes_read += amount;
                for relative in finder.find_iter(&buffer[..end]) {
                    if relative + needle.len() <= tail_len {
                        continue;
                    }
                    let absolute = region.base + offset - tail_len + relative;
                    if absolute.is_multiple_of(2) {
                        result.hits.push(absolute);
                        if result.hits.len() >= max_hits {
                            return result;
                        }
                    }
                }
                let keep = overlap.min(end);
                buffer.copy_within(end - keep..end, 0);
                tail_len = keep;
                offset += amount;
                if !on_progress(result.bytes_read) {
                    return result;
                }
            }
        }
        result
    }

    pub fn find_qword_in(&self, value: usize, ranges: &[Region], max_hits: usize) -> SearchResult {
        let mut result = self.find_bytes_in(&(value as u64).to_le_bytes(), ranges, max_hits);
        result.hits.retain(|address| address % 8 == 0);
        result
    }
}

impl Drop for ProcessMemory {
    fn drop(&mut self) {
        if self.owned {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

/// Returns the first case-insensitive executable-name match.
pub fn find_process(executable: &str) -> Option<u32> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut found = None;
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) };
    while ok != 0 {
        let len = entry
            .szExeFile
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
        if name.eq_ignore_ascii_case(executable) {
            found = Some(entry.th32ProcessID);
            break;
        }
        ok = unsafe { Process32NextW(snapshot, &mut entry) };
    }
    unsafe {
        CloseHandle(snapshot);
    }
    found
}
