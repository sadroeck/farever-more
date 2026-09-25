use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
/// Failures produced while parsing or reading a Heaps PAK archive.
pub enum PakError {
    #[error("PAK input/output error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a Heaps PAK archive")]
    BadMagic,
    #[error("invalid PAK header: {0}")]
    BadHeader(&'static str),
    #[error("entry is not present: {0}")]
    MissingEntry(String),
    #[error("entry name is not UTF-8")]
    BadName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Location and integrity metadata for one PAK payload entry.
pub struct PakFile {
    pub offset: u64,
    pub size: u64,
    pub checksum: u32,
}

#[derive(Debug)]
/// Read-only index over a Heaps PAK archive.
pub struct HeapsPak {
    path: PathBuf,
    pub version: u8,
    pub header_size: u64,
    pub data_size: u64,
    files: BTreeMap<String, PakFile>,
}

impl HeapsPak {
    /// Parses the archive header and file tree without loading payloads.
    ///
    /// # Errors
    ///
    /// Returns [`PakError`] when the file cannot be read or its header/tree is
    /// malformed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PakError> {
        let path = path.as_ref().to_path_buf();
        let mut input = File::open(&path)?;
        let mut magic = [0_u8; 3];
        input.read_exact(&mut magic)?;
        if &magic != b"PAK" {
            return Err(PakError::BadMagic);
        }
        let version = read_u8(&mut input)?;
        let header_size = u64::try_from(read_i32(&mut input)?)
            .map_err(|_| PakError::BadHeader("negative header extent"))?;
        let encoded_data_size = read_i32(&mut input)?;
        if header_size < 16 {
            return Err(PakError::BadHeader("negative or undersized extent"));
        }
        let archive_size = input.metadata()?.len();
        let data_size = decode_data_size(encoded_data_size, header_size, archive_size)?;

        let tree_size = usize::try_from(header_size - 16)
            .map_err(|_| PakError::BadHeader("header exceeds addressable memory"))?;
        let mut tree = vec![0_u8; tree_size];
        input.read_exact(&mut tree)?;
        let mut tree = Cursor::new(tree);
        let mut files = BTreeMap::new();
        read_entry(&mut tree, "", &mut files)?;

        let mut marker = [0_u8; 4];
        input.read_exact(&mut marker)?;
        if &marker != b"DATA" {
            return Err(PakError::BadHeader("missing DATA marker"));
        }
        Ok(Self {
            path,
            version,
            header_size,
            data_size,
            files,
        })
    }

    /// Returns the source archive path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Looks up one normalized, slash-separated entry name.
    ///
    /// # Errors
    ///
    /// Returns [`PakError::MissingEntry`] when the name is not indexed.
    pub fn file(&self, name: &str) -> Result<&PakFile, PakError> {
        let key = name.replace('\\', "/").trim_matches('/').to_owned();
        self.files.get(&key).ok_or(PakError::MissingEntry(key))
    }

    /// Reads one complete payload after validating its declared bounds.
    ///
    /// # Errors
    ///
    /// Returns [`PakError`] if the entry is absent, out of bounds, or cannot be
    /// read from disk.
    pub fn read(&self, name: &str) -> Result<Vec<u8>, PakError> {
        let entry = self.file(name)?;
        let entry_end = entry
            .offset
            .checked_add(entry.size)
            .ok_or(PakError::BadHeader("file extent overflow"))?;
        if entry_end > self.data_size {
            return Err(PakError::BadHeader("file extends outside payload"));
        }
        let mut input = File::open(&self.path)?;
        let absolute_offset = self
            .header_size
            .checked_add(entry.offset)
            .ok_or(PakError::BadHeader("absolute file offset overflow"))?;
        input.seek(SeekFrom::Start(absolute_offset))?;
        let entry_size = usize::try_from(entry.size)
            .map_err(|_| PakError::BadHeader("file exceeds addressable memory"))?;
        let mut bytes = vec![0_u8; entry_size];
        input.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    /// Iterates over normalized entry names in lexical order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }
}

fn decode_data_size(
    encoded_data_size: i32,
    header_size: u64,
    archive_size: u64,
) -> Result<u64, PakError> {
    let available = archive_size
        .checked_sub(header_size)
        .ok_or(PakError::BadHeader("header extends past end of archive"))?;
    let encoded_low_word = u64::from(encoded_data_size.cast_unsigned());
    if available & u64::from(u32::MAX) != encoded_low_word {
        return Err(PakError::BadHeader("payload extent does not match archive"));
    }
    Ok(available)
}

fn read_entry(
    input: &mut Cursor<Vec<u8>>,
    parent: &str,
    files: &mut BTreeMap<String, PakFile>,
) -> Result<(), PakError> {
    let name_len = usize::from(read_u8(input)?);
    let mut raw_name = vec![0_u8; name_len];
    input.read_exact(&mut raw_name)?;
    let name = String::from_utf8(raw_name).map_err(|_| PakError::BadName)?;
    let flags = read_u8(input)?;
    let path = match (parent.is_empty(), name.is_empty()) {
        (_, true) => parent.to_owned(),
        (true, false) => name,
        (false, false) => format!("{parent}/{name}"),
    };
    if flags & 1 != 0 {
        let count = read_i32(input)?;
        if !(0..=1_000_000).contains(&count) {
            return Err(PakError::BadHeader("invalid directory entry count"));
        }
        for _ in 0..count {
            read_entry(input, &path, files)?;
        }
        return Ok(());
    }

    let position = if flags & 2 != 0 {
        wide_offset(read_f64(input)?)?
    } else {
        let value = read_i32(input)?;
        u64::try_from(value).map_err(|_| PakError::BadHeader("negative entry offset"))?
    };
    let size = read_i32(input)?;
    let size = u64::try_from(size).map_err(|_| PakError::BadHeader("negative entry size"))?;
    // Checksums are stored in a signed Castle/Heaps integer field but represent
    // all 32 bits of an unsigned checksum.
    let checksum = read_i32(input)?.cast_unsigned();
    files.insert(
        path,
        PakFile {
            offset: position,
            size,
            checksum,
        },
    );
    Ok(())
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn wide_offset(value: f64) -> Result<u64, PakError> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 {
        return Err(PakError::BadHeader("invalid wide entry offset"));
    }
    // The Heaps format stores wide offsets as integral f64 values. Rust's
    // float-to-integer cast saturates values outside u64; the read-time
    // entry-extent validation rejects any saturated result as out of bounds.
    Ok(value as u64)
}

fn read_u8(input: &mut impl Read) -> std::io::Result<u8> {
    let mut value = [0_u8; 1];
    input.read_exact(&mut value)?;
    Ok(value[0])
}

fn read_i32(input: &mut impl Read) -> std::io::Result<i32> {
    let mut value = [0_u8; 4];
    input.read_exact(&mut value)?;
    Ok(i32::from_le_bytes(value))
}

fn read_f64(input: &mut impl Read) -> std::io::Result<f64> {
    let mut value = [0_u8; 8];
    input.read_exact(&mut value)?;
    Ok(f64::from_le_bytes(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_file_archive() {
        let body = br#"{"sheets":[]}"#;
        let body_len = i32::try_from(body.len()).expect("fixture body fits in i32");
        let mut tree = vec![0, 1];
        tree.extend_from_slice(&1_i32.to_le_bytes());
        tree.push(8);
        tree.extend_from_slice(b"data.cdb");
        tree.push(0);
        tree.extend_from_slice(&0_i32.to_le_bytes());
        tree.extend_from_slice(&body_len.to_le_bytes());
        tree.extend_from_slice(&123_i32.to_le_bytes());
        let header_size = 64_i32;
        let mut bytes = b"PAK\0".to_vec();
        bytes.extend_from_slice(&header_size.to_le_bytes());
        bytes.extend_from_slice(&body_len.to_le_bytes());
        bytes.extend_from_slice(&tree);
        let header_size = usize::try_from(header_size).expect("fixture header fits in usize");
        bytes.resize(header_size - 4, 0);
        bytes.extend_from_slice(b"DATA");
        bytes.extend_from_slice(body);

        let path =
            std::env::temp_dir().join(format!("farever-pak-test-{}.pak", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let pak = HeapsPak::open(&path).unwrap();
        assert_eq!(pak.read("data.cdb").unwrap(), body);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reconstructs_wrapped_payload_sizes_for_archives_over_four_gibibytes() {
        let header_size = 679_936;
        let payload_size = 5_152_403_456_u64;
        let encoded = payload_size as u32 as i32;

        assert_eq!(
            decode_data_size(encoded, header_size, header_size + payload_size).unwrap(),
            payload_size
        );
    }

    #[test]
    fn current_install_reads_a_high_offset_skill_atlas_when_available() {
        let Ok(game) = crate::discover_game() else {
            return;
        };
        let pak = HeapsPak::open(game.directory.join("res.pak")).unwrap();
        let bytes = pak
            .read("UI/icons/atlas_weapon_GreatSword_96PX.png")
            .unwrap();

        assert_eq!(bytes.get(..8), Some(b"\x89PNG\r\n\x1a\n".as_slice()));
    }
}
