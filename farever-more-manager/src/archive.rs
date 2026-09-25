//! Add-on archive (`.zip`) format and extractor.
//!
//! The archive is the distribution unit for add-ons: produced by hand, by CI,
//! or (later) fetched from the remote index. Layout inside the archive:
//!
//! ```text
//! <anything>.zip
//!   addon.json           required: the typed manifest (see
//!                        `farever-more-manifest` for the spec)
//!   addon.wasm           required: the single component; the format fixes
//!                        its name
//!   ...                  anything else ships along (assets/, docs, ...)
//! ```
//!
//! A single top-level folder holding the same layout is accepted too (that is
//! what "compress this folder" produces); anything more ambiguous is
//! rejected. Only stored (method 0) and deflated (method 8) entries are
//! supported: no encryption, spanning, or zip64, and entry names must be
//! UTF-8. Paths are confined to the destination — absolute paths, `..`,
//! drive prefixes, and backslashes are rejected — every file's CRC32 is
//! verified, and totals are capped (see `MAX_*`) before anything is written.
//!
//! The manifest itself is validated strictly here (`validate`): schema
//! version, single component reference, SHA-256 fingerprint, and relative
//! file references. Discovery (`backend::scan`) stays lenient on purpose.

use farever_more_manifest::{MANIFEST_FILE_NAME, WASM_FILE_NAME};
use std::fs;
use std::path::{Path, PathBuf};
const MAX_ENTRIES: usize = 4096;
const MAX_TOTAL_UNCOMPRESSED: u64 = 256 * 1024 * 1024;
const MAX_FILE_UNCOMPRESSED: u64 = 128 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

const LOCAL_HEADER_SIGNATURE: u32 = 0x0403_4b50;
const CENTRAL_HEADER_SIGNATURE: u32 = 0x0201_4b50;
const END_OF_CENTRAL_DIR_SIGNATURE: u32 = 0x0605_4b50;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATED: u16 = 8;
const FLAG_ENCRYPTED: u16 = 1 << 0;

/// What extraction validated: the manifest id plus the component file names
/// at the unit root, so the caller can place and report them.
#[derive(Debug)]
pub(crate) struct ExtractedArchive {
    pub(crate) id: String,
    pub(crate) wasm_files: Vec<String>,
}

/// Validates `archive` and extracts the add-on unit into `dest_dir`.
/// Returns the manifest id and the `.wasm` file names at the unit root.
pub(crate) fn extract_archive(archive: &Path, dest_dir: &Path) -> Result<ExtractedArchive, String> {
    let bytes = fs::metadata(archive)
        .map_err(|error| format!("read {}: {error}", archive.display()))
        .and_then(|meta| {
            if meta.len() > MAX_ARCHIVE_BYTES {
                Err(format!(
                    "archive too large ({} bytes, limit {MAX_ARCHIVE_BYTES})",
                    meta.len()
                ))
            } else {
                fs::read(archive).map_err(|error| format!("read {}: {error}", archive.display()))
            }
        })?;
    let entries = read_central_directory(&bytes)?;
    fs::create_dir_all(dest_dir)
        .map_err(|error| format!("create {}: {error}", dest_dir.display()))?;
    let mut total_uncompressed: u64 = 0;
    for entry in &entries {
        total_uncompressed = total_uncompressed.saturating_add(entry.uncompressed_size);
        if total_uncompressed > MAX_TOTAL_UNCOMPRESSED {
            return Err(format!(
                "archive uncompressed size exceeds {MAX_TOTAL_UNCOMPRESSED} bytes"
            ));
        }
        extract_entry(&bytes, entry, dest_dir)?;
    }
    let unit_root = resolve_unit_root(dest_dir)?;
    let bytes = fs::read(unit_root.join(MANIFEST_FILE_NAME))
        .map_err(|_| format!("archive must contain {MANIFEST_FILE_NAME} at its root"))?;
    let mut manifest = farever_more_manifest::parse(&bytes)?;
    let component = single_component(&unit_root)?;
    if component != WASM_FILE_NAME {
        return Err(format!(
            "an add-on holds its component as {WASM_FILE_NAME}, found {component}"
        ));
    }
    manifest.validate(&unit_root)?;
    require_compatible_api(&manifest)?;
    verify_fingerprint(&manifest, &unit_root.join(&component))?;
    Ok(ExtractedArchive {
        id: manifest.id,
        wasm_files: vec![component],
    })
}

/// Refuses an archive whose component was built for an add-on API this
/// framework cannot satisfy.
///
/// The pack step read the version out of the component itself, so this is the
/// version the WebAssembly linker would insist on. Catching it here means a
/// mismatched add-on never lands in `addons/`, where it would only fail later,
/// inside the game.
fn require_compatible_api(manifest: &farever_more_manifest::AddonManifest) -> Result<(), String> {
    let Some(declared) = manifest.api_version.as_deref() else {
        return Err(format!(
            "{MANIFEST_FILE_NAME} has no api-version: the archive was not packed by a \
             framework build"
        ));
    };
    let declared = farever_more_manifest::api::parse_api_version(declared, "api-version")?;
    farever_more_manifest::api::compatibility(
        declared,
        farever_more_manifest::api::ApiVersion::host(),
    )
    .map_err(|error| format!("{}: {error}", manifest.id))
}

/// An add-on is exactly one component at the unit root. Nested components
/// are not discoverable, so they fail loudly instead of installing invisibly.
fn single_component(unit_root: &Path) -> Result<String, String> {
    let mut root_wasms = Vec::new();
    let mut nested_wasms = 0u32;
    collect_wasms(unit_root, 0, &mut root_wasms, &mut nested_wasms)?;
    if nested_wasms > 0 {
        return Err("nested .wasm files are not supported".to_owned());
    }
    if root_wasms.len() != 1 {
        return Err(format!(
            "an add-on holds a single .wasm component, found {}",
            root_wasms.len()
        ));
    }
    Ok(root_wasms.pop().expect("single component"))
}

fn collect_wasms(
    directory: &Path,
    depth: usize,
    root_wasms: &mut Vec<String>,
    nested_wasms: &mut u32,
) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("read {}: {error}", directory.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_wasms(&path, depth + 1, root_wasms, nested_wasms)?;
            continue;
        }
        let is_wasm = path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("wasm"));
        if !is_wasm {
            continue;
        }
        if depth > 0 {
            *nested_wasms += 1;
            continue;
        }
        let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            return Err(format!("component name is not UTF-8: {}", path.display()));
        };
        root_wasms.push(name.to_owned());
    }
    root_wasms.sort();
    Ok(())
}

/// Compares the component bytes against the fingerprint the pack step
/// stamped into the manifest.
fn verify_fingerprint(
    manifest: &farever_more_manifest::AddonManifest,
    wasm_path: &Path,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let bytes =
        fs::read(wasm_path).map_err(|error| format!("read {}: {error}", wasm_path.display()))?;
    let digest = hex::encode(Sha256::digest(&bytes));
    if digest != manifest.sha256 {
        return Err(format!(
            "{WASM_FILE_NAME} does not match its manifest fingerprint"
        ));
    }
    Ok(())
}

struct CentralEntry {
    name: String,
    method: u16,
    crc: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    local_header_offset: u64,
}

fn read_central_directory(bytes: &[u8]) -> Result<Vec<CentralEntry>, String> {
    if bytes.len() < 22 {
        return Err("not a zip archive: file too small".to_owned());
    }
    // The end record starts with the last occurrence of its signature; the
    // trailing comment (up to 64 KiB) may contain anything.
    let search_from = bytes.len().saturating_sub(22 + 0xFFFF);
    let end_offset = bytes[search_from..]
        .windows(4)
        .rposition(|window| window == END_OF_CENTRAL_DIR_SIGNATURE.to_le_bytes())
        .map(|position| search_from + position)
        .ok_or_else(|| "not a zip archive: end record not found".to_owned())?;
    let mut cursor = Cursor::at(bytes, end_offset + 4);
    let disk = cursor.u16()?;
    let directory_disk = cursor.u16()?;
    let entries_on_disk = cursor.u16()? as usize;
    let entries_total = cursor.u16()? as usize;
    let directory_size = cursor.u32()? as u64;
    let directory_offset = cursor.u32()? as u64;
    if disk != 0 || directory_disk != 0 {
        return Err("multi-disk archives are not supported".to_owned());
    }
    if entries_on_disk != entries_total {
        return Err("archive directory is inconsistent".to_owned());
    }
    if entries_total > MAX_ENTRIES {
        return Err(format!(
            "archive has too many entries (limit {MAX_ENTRIES})"
        ));
    }
    let directory_end = directory_offset
        .checked_add(directory_size)
        .filter(|end| *end as usize <= bytes.len())
        .ok_or_else(|| "archive directory points outside the file".to_owned())?;
    let _ = directory_end;
    let mut cursor = Cursor::at(bytes, directory_offset as usize);
    let mut entries = Vec::with_capacity(entries_total.min(1024));
    for _ in 0..entries_total {
        let signature = cursor.u32()?;
        if signature != CENTRAL_HEADER_SIGNATURE {
            return Err("archive directory is corrupt".to_owned());
        }
        let _made_by = cursor.u16()?;
        let _needed = cursor.u16()?;
        let flags = cursor.u16()?;
        let method = cursor.u16()?;
        let _time = cursor.u16()?;
        let _date = cursor.u16()?;
        let crc = cursor.u32()?;
        let compressed_size = cursor.u32()?;
        let uncompressed_size = cursor.u32()?;
        let name_len = cursor.u16()? as usize;
        let extra_len = cursor.u16()? as usize;
        let comment_len = cursor.u16()? as usize;
        let _disk_start = cursor.u16()?;
        let _internal_attrs = cursor.u16()?;
        let _external_attrs = cursor.u32()?;
        let local_header_offset = cursor.u32()? as u64;
        let name_bytes = cursor.take(name_len)?;
        let _extra = cursor.take(extra_len)?;
        let _comment = cursor.take(comment_len)?;
        if flags & FLAG_ENCRYPTED != 0 {
            return Err("encrypted entries are not supported".to_owned());
        }
        if method != METHOD_STORED && method != METHOD_DEFLATED {
            let name = String::from_utf8_lossy(name_bytes);
            return Err(format!(
                "unsupported compression method {method} for {name}"
            ));
        }
        if compressed_size == u32::MAX || uncompressed_size == u32::MAX {
            return Err("zip64 archives are not supported".to_owned());
        }
        let uncompressed_size = uncompressed_size as u64;
        if uncompressed_size > MAX_FILE_UNCOMPRESSED {
            let name = String::from_utf8_lossy(name_bytes);
            return Err(format!(
                "{name} is too large (limit {MAX_FILE_UNCOMPRESSED} bytes uncompressed)"
            ));
        }
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| "archive entry name is not UTF-8".to_owned())?
            .to_owned();
        entries.push(CentralEntry {
            name,
            method,
            crc,
            compressed_size: compressed_size as u64,
            uncompressed_size,
            local_header_offset,
        });
    }
    Ok(entries)
}

fn extract_entry(bytes: &[u8], entry: &CentralEntry, dest_dir: &Path) -> Result<(), String> {
    let relative = confined_path(&entry.name)?;
    let Some(relative) = relative else {
        // Directory entry: ensure it exists (files create parents anyway).
        let directory = confined_dir(&entry.name)?;
        fs::create_dir_all(dest_dir.join(directory))
            .map_err(|error| format!("create {}: {error}", entry.name))?;
        return Ok(());
    };
    let data_start = data_offset(bytes, entry)?;
    let data_end = data_start
        .checked_add(entry.compressed_size as usize)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| format!("{} points outside the archive", entry.name))?;
    let raw = &bytes[data_start..data_end];
    let plain = if entry.method == METHOD_STORED {
        if raw.len() as u64 != entry.uncompressed_size {
            return Err(format!("{} has a corrupt size", entry.name));
        }
        raw.to_vec()
    } else {
        inflate(raw, entry)?
    };
    let mut crc = flate2::Crc::new();
    crc.update(&plain);
    if crc.sum() != entry.crc {
        return Err(format!("{} failed its checksum", entry.name));
    }
    let destination = dest_dir.join(&relative);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    fs::write(&destination, &plain)
        .map_err(|error| format!("write {}: {error}", destination.display()))?;
    Ok(())
}

/// Skips a local header and returns the offset of the entry's data.
fn data_offset(bytes: &[u8], entry: &CentralEntry) -> Result<usize, String> {
    let offset = entry.local_header_offset as usize;
    let mut cursor = Cursor::at(bytes, offset);
    if cursor.u32()? != LOCAL_HEADER_SIGNATURE {
        return Err(format!("{} has a corrupt local header", entry.name));
    }
    // Version, flags, method, time, date, crc, sizes: 22 bytes of fields we
    // already trust from the central directory; name/extra lengths follow.
    cursor.skip(22)?;
    let name_len = cursor.u16()? as usize;
    let extra_len = cursor.u16()? as usize;
    cursor.skip(name_len + extra_len)?;
    Ok(cursor.position())
}

fn inflate(raw: &[u8], entry: &CentralEntry) -> Result<Vec<u8>, String> {
    use flate2::bufread::DeflateDecoder;
    use std::io::Read;
    let expected = entry.uncompressed_size as usize;
    let mut plain = Vec::new();
    plain.try_reserve(expected).map_err(|_| {
        format!(
            "cannot stage {} ({} bytes)",
            entry.name, entry.uncompressed_size
        )
    })?;
    // `take` bounds a lying stream: anything but exactly the declared size
    // fails below instead of ballooning memory.
    let decoded = DeflateDecoder::new(raw)
        .take(entry.uncompressed_size + 1)
        .read_to_end(&mut plain)
        .map_err(|error| format!("cannot decompress {}: {error}", entry.name))?;
    if decoded as u64 != entry.uncompressed_size {
        return Err(format!("{} has a corrupt size", entry.name));
    }
    Ok(plain)
}

/// Maps an entry name to a path confined under the destination.
/// Returns `None` for directory entries. Rejects absolute paths, parent
/// navigation, drive prefixes, backslashes, and control characters.
fn confined_path(name: &str) -> Result<Option<PathBuf>, String> {
    if name.ends_with('/') {
        confined_dir(name)?;
        return Ok(None);
    }
    let mut relative = PathBuf::new();
    for component in name.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains('\\')
            || component.contains(':')
            || component.contains('\0')
            || component.chars().any(|char| char.is_control())
        {
            return Err(format!("archive entry escapes its folder: {name}"));
        }
        relative.push(component);
    }
    if relative.as_os_str().is_empty() {
        return Err(format!("archive entry has no path: {name}"));
    }
    Ok(Some(relative))
}

fn confined_dir(name: &str) -> Result<PathBuf, String> {
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    if trimmed.is_empty() || trimmed.starts_with('/') {
        return Err(format!("archive entry escapes its folder: {name}"));
    }
    let mut relative = PathBuf::new();
    for component in trimmed.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains('\\')
            || component.contains(':')
            || component.contains('\0')
            || component.chars().any(|char| char.is_control())
        {
            return Err(format!("archive entry escapes its folder: {name}"));
        }
        relative.push(component);
    }
    Ok(relative)
}

/// The unit root is the destination itself when it holds the manifest;
/// otherwise a single top-level folder holding it (what "compress this
/// folder" produces). Anything else is ambiguous and rejected.
fn resolve_unit_root(dest_dir: &Path) -> Result<PathBuf, String> {
    if dest_dir.join(MANIFEST_FILE_NAME).is_file() {
        return Ok(dest_dir.to_path_buf());
    }
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let entries =
        fs::read_dir(dest_dir).map_err(|error| format!("read {}: {error}", dest_dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            directories.push(path);
        } else {
            files.push(path);
        }
    }
    if files.is_empty()
        && directories.len() == 1
        && directories[0].join(MANIFEST_FILE_NAME).is_file()
    {
        return Ok(directories.pop().expect("single directory"));
    }
    Err(format!(
        "archive must contain {MANIFEST_FILE_NAME} at its root"
    ))
}

/// Maps a manifest id to a safe unit folder name: lowercase dashed ASCII,
/// capped at 96 bytes, never empty or Windows-reserved.
pub(crate) fn sanitize_unit_name(id: &str) -> String {
    const MAX_BYTES: usize = 96;
    let mut name = String::new();
    let mut last_dash = true;
    for char in id.chars().flat_map(|char| char.to_lowercase()) {
        if char.is_ascii_alphanumeric() {
            name.push(char);
            last_dash = false;
        } else if !last_dash {
            name.push('-');
            last_dash = true;
        }
        if name.len() >= MAX_BYTES {
            break;
        }
    }
    let trimmed = name.trim_matches(['-', '.']).to_owned();
    if trimmed.is_empty() {
        return "addon".to_owned();
    }
    let stem = trimmed.split('.').next().unwrap_or(&trimmed);
    const RESERVED: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    if RESERVED.contains(&stem) {
        return format!("addon-{trimmed}");
    }
    trimmed
}

struct Cursor<'bytes> {
    bytes: &'bytes [u8],
    position: usize,
}

impl<'bytes> Cursor<'bytes> {
    fn at(bytes: &'bytes [u8], position: usize) -> Self {
        Self { bytes, position }
    }

    fn position(&self) -> usize {
        self.position
    }

    fn take(&mut self, len: usize) -> Result<&'bytes [u8], String> {
        let end = self
            .position
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| "archive ends unexpectedly".to_owned())?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn skip(&mut self, len: usize) -> Result<(), String> {
        self.take(len).map(|_| ())
    }

    fn u16(&mut self) -> Result<u16, String> {
        self.take(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, String> {
        self.take(4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Minimal zip writer for tests: stored/deflated entries plus header
    //! forging for the negative cases. It is not a general archiver.

    use super::*;

    pub(crate) struct TestEntry {
        pub(crate) name: &'static str,
        pub(crate) method: u16,
        pub(crate) data: Vec<u8>,
        /// Forge the sizes recorded in both headers (declared-size attacks).
        pub(crate) declared_size: Option<u32>,
        /// Forge the CRC recorded in both headers.
        pub(crate) corrupt_crc: bool,
        /// Forge the local/central method and flags fields.
        pub(crate) flags: u16,
    }

    impl TestEntry {
        pub(crate) fn stored(name: &'static str, data: &[u8]) -> Self {
            Self {
                name,
                method: METHOD_STORED,
                data: data.to_vec(),
                declared_size: None,
                corrupt_crc: false,
                flags: 0,
            }
        }

        pub(crate) fn deflated(name: &'static str, data: &[u8]) -> Self {
            Self {
                name,
                method: METHOD_DEFLATED,
                data: data.to_vec(),
                declared_size: None,
                corrupt_crc: false,
                flags: 0,
            }
        }
    }

    fn deflate(data: &[u8]) -> Vec<u8> {
        use flate2::write::DeflateEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).expect("deflate");
        encoder.finish().expect("finish")
    }

    pub(crate) fn write_test_zip(path: &Path, entries: &[TestEntry]) {
        let mut zip = Vec::new();
        let mut central = Vec::new();
        for entry in entries {
            let raw = if entry.method == METHOD_DEFLATED {
                deflate(&entry.data)
            } else {
                entry.data.clone()
            };
            let mut crc = flate2::Crc::new();
            crc.update(&entry.data);
            let crc = if entry.corrupt_crc {
                crc.sum().wrapping_add(1)
            } else {
                crc.sum()
            };
            let declared = entry.declared_size.unwrap_or(entry.data.len() as u32);
            let offset = zip.len() as u32;
            let name = entry.name.as_bytes();
            zip.extend(LOCAL_HEADER_SIGNATURE.to_le_bytes());
            zip.extend(20u16.to_le_bytes());
            zip.extend(entry.flags.to_le_bytes());
            zip.extend(entry.method.to_le_bytes());
            zip.extend([0u8; 4]);
            zip.extend(crc.to_le_bytes());
            zip.extend((raw.len() as u32).to_le_bytes());
            zip.extend(declared.to_le_bytes());
            zip.extend((name.len() as u16).to_le_bytes());
            zip.extend(0u16.to_le_bytes());
            zip.extend(name);
            zip.extend(&raw);
            central.extend(CENTRAL_HEADER_SIGNATURE.to_le_bytes());
            central.extend([0u8; 4]);
            central.extend(entry.flags.to_le_bytes());
            central.extend(entry.method.to_le_bytes());
            central.extend([0u8; 4]);
            central.extend(crc.to_le_bytes());
            central.extend((raw.len() as u32).to_le_bytes());
            central.extend(declared.to_le_bytes());
            central.extend((name.len() as u16).to_le_bytes());
            central.extend([0u8; 6]);
            central.extend([0u8; 6]);
            central.extend(offset.to_le_bytes());
            central.extend(name);
        }
        let directory_offset = zip.len() as u32;
        zip.extend(&central);
        let directory_size = (zip.len() as u32) - directory_offset;
        zip.extend(END_OF_CENTRAL_DIR_SIGNATURE.to_le_bytes());
        zip.extend([0u8; 4]);
        zip.extend((entries.len() as u16).to_le_bytes());
        zip.extend((entries.len() as u16).to_le_bytes());
        zip.extend(directory_size.to_le_bytes());
        zip.extend(directory_offset.to_le_bytes());
        zip.extend(0u16.to_le_bytes());
        fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        fs::write(path, &zip).expect("write test zip");
    }

    pub(crate) fn wasm_stub() -> Vec<u8> {
        b"\0asm\x01\0\0\0".to_vec()
    }

    /// Builds a valid distribution manifest for a component holding
    /// `wasm_data`, stamping the real fingerprint the pack step would.
    pub(crate) fn manifest_json(id: &str, wasm_data: &[u8]) -> Vec<u8> {
        manifest_json_for(id, wasm_data, farever_more_manifest::api::ADDON_API_VERSION)
    }

    /// The same manifest, declaring a different add-on API version.
    pub(crate) fn manifest_json_for(id: &str, wasm_data: &[u8], api_version: &str) -> Vec<u8> {
        use sha2::{Digest, Sha256};
        let sha256 = hex::encode(Sha256::digest(wasm_data));
        format!(
            r#"{{"manifest-version": 1, "id": "{id}", "version": "0.1.0", "sha256": "{sha256}", "api-version": "{api_version}"}}"#
        )
        .into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn scratch() -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("farever-archive-test-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[derive(Debug)]
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn extract(entries: &[TestEntry]) -> Result<(Scratch, ExtractedArchive), String> {
        let root = scratch();
        let archive = root.join("addon.zip");
        write_test_zip(&archive, entries);
        let dest = root.join("out");
        let extracted = extract_archive(&archive, &dest)?;
        Ok((Scratch(root), extracted))
    }

    #[test]
    fn valid_stored_archive_extracts() {
        let root = scratch();
        let _guard = Scratch(root.clone());
        let archive = root.join("addon.zip");
        let wasm = wasm_stub();
        write_test_zip(
            &archive,
            &[
                TestEntry::stored("addon.json", &manifest_json("minimap", &wasm)),
                TestEntry::stored(WASM_FILE_NAME, &wasm),
                TestEntry::stored("assets/icon.png", b"png"),
            ],
        );
        let dest = root.join("out");
        let extracted = extract_archive(&archive, &dest).expect("extract");
        assert_eq!(extracted.id, "minimap");
        assert_eq!(extracted.wasm_files, vec![WASM_FILE_NAME.to_owned()]);
        assert!(dest.join(WASM_FILE_NAME).is_file());
        assert!(dest.join("assets").join("icon.png").is_file());
    }

    #[test]
    fn single_top_folder_is_accepted() {
        let root = scratch();
        let _guard = Scratch(root.clone());
        let archive = root.join("addon.zip");
        let wasm = wasm_stub();
        write_test_zip(
            &archive,
            &[
                TestEntry::stored("my-addon/", b""),
                TestEntry::stored("my-addon/addon.json", &manifest_json("demo", &wasm)),
                TestEntry::stored("my-addon/addon.wasm", &wasm),
            ],
        );
        let extracted = extract_archive(&archive, &root.join("out")).expect("extract");
        assert_eq!(extracted.id, "demo");
        assert_eq!(extracted.wasm_files, vec![WASM_FILE_NAME.to_owned()]);
        assert!(root
            .join("out")
            .join("my-addon")
            .join(WASM_FILE_NAME)
            .is_file());
    }

    #[test]
    fn deflated_entries_round_trip() {
        let payload = vec![42u8; 4096];
        let (_root, extracted) = extract(&[
            TestEntry::deflated("addon.json", &manifest_json("demo", &payload)),
            TestEntry::deflated(WASM_FILE_NAME, &payload),
        ])
        .expect("extract");
        assert_eq!(extracted.wasm_files, vec![WASM_FILE_NAME.to_owned()]);
    }

    #[test]
    fn traversal_entries_are_rejected() {
        for evil in [
            "../evil.wasm",
            "sub/../../evil.wasm",
            "/abs.wasm",
            "C:/evil.wasm",
            "a\\b.wasm",
        ] {
            let wasm = wasm_stub();
            let error = extract(&[
                TestEntry::stored("addon.json", &manifest_json("demo", &wasm)),
                TestEntry::stored(WASM_FILE_NAME, &wasm),
                TestEntry::stored(evil, b"evil"),
            ])
            .expect_err("evil entry");
            assert!(error.contains("escapes"), "{evil}: {error}");
        }
    }

    #[test]
    fn missing_manifest_is_rejected() {
        let error =
            extract(&[TestEntry::stored(WASM_FILE_NAME, &wasm_stub())]).expect_err("manifest");
        assert!(error.contains(MANIFEST_FILE_NAME), "{error}");
    }

    #[test]
    fn manifest_without_id_is_rejected() {
        let wasm = wasm_stub();
        let error = extract(&[
            TestEntry::stored("addon.json", &manifest_json("", &wasm)),
            TestEntry::stored(WASM_FILE_NAME, &wasm),
        ])
        .expect_err("id");
        assert!(error.contains("invalid add-on id"), "{error}");
    }

    #[test]
    fn archive_without_wasm_is_rejected() {
        let wasm = wasm_stub();
        let error = extract(&[TestEntry::stored(
            "addon.json",
            &manifest_json("demo", &wasm),
        )])
        .expect_err("wasm");
        assert!(error.contains("single"), "{error}");
    }

    #[test]
    fn second_component_is_rejected() {
        let wasm = wasm_stub();
        let error = extract(&[
            TestEntry::stored("addon.json", &manifest_json("demo", &wasm)),
            TestEntry::stored(WASM_FILE_NAME, &wasm),
            TestEntry::stored("extra.wasm", &wasm),
        ])
        .expect_err("second wasm");
        assert!(error.contains("single"), "{error}");
    }

    #[test]
    fn nested_component_is_rejected() {
        let wasm = wasm_stub();
        let error = extract(&[
            TestEntry::stored("addon.json", &manifest_json("demo", &wasm)),
            TestEntry::stored(WASM_FILE_NAME, &wasm),
            TestEntry::stored("sub/nested.wasm", &wasm),
        ])
        .expect_err("nested wasm");
        assert!(error.contains("nested"), "{error}");
    }

    #[test]
    fn a_differently_named_component_is_rejected() {
        // The format fixes the component name, so a unit that calls it
        // anything else is not the add-on it claims to be.
        let wasm = wasm_stub();
        let error = extract(&[
            TestEntry::stored("addon.json", &manifest_json("demo", &wasm)),
            TestEntry::stored("other.wasm", &wasm),
        ])
        .expect_err("component name");
        assert!(error.contains("other.wasm"), "{error}");
    }

    #[test]
    fn fingerprint_mismatch_is_rejected() {
        let wasm = wasm_stub();
        let forged = format!(
            r#"{{"manifest-version": 1, "id": "demo", "version": "0.1.0", "sha256": "{}", "api-version": "{}"}}"#,
            "00".repeat(32),
            farever_more_manifest::api::ADDON_API_VERSION
        )
        .into_bytes();
        let error = extract(&[
            TestEntry::stored("addon.json", &forged),
            TestEntry::stored(WASM_FILE_NAME, &wasm),
        ])
        .expect_err("fingerprint");
        assert!(error.contains("fingerprint"), "{error}");
    }

    #[test]
    fn an_archive_for_another_add_on_api_is_refused() {
        let wasm = wasm_stub();
        let host = farever_more_manifest::api::ApiVersion::host();
        // A component from the next major generation, and one from a newer
        // minor of this one: neither can link, and installing would only move
        // the failure into the game.
        for declared in [
            format!("{}.0.0", host.major() + 1),
            format!("{}.{}.0", host.major(), host.minor() + 1),
        ] {
            let error = extract(&[
                TestEntry::stored("addon.json", &manifest_json_for("demo", &wasm, &declared)),
                TestEntry::stored(WASM_FILE_NAME, &wasm),
            ])
            .expect_err("incompatible api");
            assert!(error.contains(&declared), "{declared}: {error}");
            assert!(error.contains(&host.to_string()), "{declared}: {error}");
        }
    }

    #[test]
    fn corrupt_checksum_is_rejected() {
        let wasm = wasm_stub();
        let mut bad = TestEntry::stored(WASM_FILE_NAME, &wasm);
        bad.corrupt_crc = true;
        let error = extract(&[
            TestEntry::stored("addon.json", &manifest_json("demo", &wasm)),
            bad,
        ])
        .expect_err("crc");
        assert!(error.contains("checksum"), "{error}");
    }

    #[test]
    fn declared_size_lie_is_rejected() {
        let wasm = wasm_stub();
        let mut lying = TestEntry::stored(WASM_FILE_NAME, &wasm);
        lying.declared_size = Some(u32::MAX - 1);
        let error = extract(&[
            TestEntry::stored("addon.json", &manifest_json("demo", &wasm)),
            lying,
        ])
        .expect_err("size lie");
        assert!(
            error.contains("too large") || error.contains("zip64"),
            "{error}"
        );
    }

    #[test]
    fn unsupported_method_is_rejected() {
        let wasm = wasm_stub();
        let mut shrunk = TestEntry::stored(WASM_FILE_NAME, &wasm);
        shrunk.method = 12;
        let error = extract(&[
            TestEntry::stored("addon.json", &manifest_json("demo", &wasm)),
            shrunk,
        ])
        .expect_err("method");
        assert!(error.contains("method"), "{error}");
    }

    #[test]
    fn encrypted_entry_is_rejected() {
        let wasm = wasm_stub();
        let mut locked = TestEntry::stored(WASM_FILE_NAME, &wasm);
        locked.flags = FLAG_ENCRYPTED;
        let error = extract(&[
            TestEntry::stored("addon.json", &manifest_json("demo", &wasm)),
            locked,
        ])
        .expect_err("encrypted");
        assert!(error.contains("encrypted"), "{error}");
    }

    #[test]
    fn non_zip_is_rejected() {
        let root = scratch();
        let _guard = Scratch(root.clone());
        let archive = root.join("addon.zip");
        fs::write(&archive, b"definitely not a zip").expect("write");
        let error = extract_archive(&archive, &root.join("out")).expect_err("non-zip");
        assert!(error.contains("not a zip"), "{error}");
    }

    #[test]
    fn ambiguous_roots_are_rejected() {
        let wasm_a = wasm_stub();
        let wasm_b = wasm_stub();
        let error = extract(&[
            TestEntry::stored("a/addon.json", &manifest_json("a", &wasm_a)),
            TestEntry::stored("a/addon.wasm", &wasm_a),
            TestEntry::stored("b/addon.json", &manifest_json("b", &wasm_b)),
            TestEntry::stored("b/addon.wasm", &wasm_b),
        ])
        .expect_err("ambiguous");
        assert!(error.contains(MANIFEST_FILE_NAME), "{error}");
    }

    #[test]
    fn unit_names_are_safe() {
        assert_eq!(sanitize_unit_name("minimap"), "minimap");
        assert_eq!(sanitize_unit_name("My Addon 2.0!"), "my-addon-2-0");
        assert_eq!(sanitize_unit_name("..."), "addon");
        assert_eq!(sanitize_unit_name(""), "addon");
        assert_eq!(sanitize_unit_name("NUL"), "addon-nul");
        assert_eq!(sanitize_unit_name("aux"), "addon-aux");
        assert_eq!(sanitize_unit_name("aux.wasm"), "aux-wasm");
        assert!(sanitize_unit_name(&"x".repeat(200)).len() <= 96);
    }
}
