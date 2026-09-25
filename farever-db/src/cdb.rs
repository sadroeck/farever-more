use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::pak::{HeapsPak, PakError};

const APP_ID: &str = "3672400";

#[derive(Debug, Error)]
/// Failures produced while locating or decoding Farever's packaged game data.
pub enum GameDataError {
    #[error("Farever installation was not found")]
    NotFound,
    #[error("game data input/output error: {0}")]
    Io(#[from] std::io::Error),
    #[error("PAK error: {0}")]
    Pak(#[from] PakError),
    #[error("data.cdb is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("data.cdb does not contain a sheets array")]
    MissingSheets,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Values that identify the exact client-data revision used by a result.
pub struct BuildFingerprint {
    pub steam_build_id: Option<String>,
    pub hlboot_sha256: String,
    pub cdb_checksum: u32,
    pub cdb_size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Inventory and physical location of the packaged `data.cdb` document.
pub struct CdbSummary {
    pub archive_path: PathBuf,
    pub cdb_absolute_offset: u64,
    pub cdb_size: u64,
    pub sheet_count: usize,
    pub item_count: usize,
    pub loot_table_count: usize,
    pub fingerprint: BuildFingerprint,
}

#[derive(Clone, Debug)]
/// Validated path to a Farever installation.
pub struct GameInstall {
    pub directory: PathBuf,
}

impl GameInstall {
    /// Opens a directory containing the expected Farever executable and PAK.
    ///
    /// # Errors
    ///
    /// Returns [`GameDataError::NotFound`] when either required file is absent.
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, GameDataError> {
        let directory = directory.as_ref().to_path_buf();
        if directory.join("Farever.exe").is_file() && directory.join("res.light.pak").is_file() {
            Ok(Self { directory })
        } else {
            Err(GameDataError::NotFound)
        }
    }

    /// Summarizes the embedded `CastleDB` and fingerprints its client build.
    ///
    /// # Errors
    ///
    /// Returns an I/O, PAK, JSON, or schema error when the packaged document
    /// cannot be read and inventoried.
    pub fn summarize_cdb(&self) -> Result<CdbSummary, GameDataError> {
        let pak = HeapsPak::open(self.directory.join("res.light.pak"))?;
        let entry = pak.file("data.cdb")?.clone();
        let document: Value = serde_json::from_slice(&pak.read("data.cdb")?)?;
        let sheets = document
            .get("sheets")
            .and_then(Value::as_array)
            .ok_or(GameDataError::MissingSheets)?;
        let mut item_count = 0;
        let mut loot_table_count = 0;
        for sheet in sheets {
            let name = sheet.get("name").and_then(Value::as_str);
            let count = sheet
                .get("lines")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            match name {
                Some("item") => item_count = count,
                Some("lootTable") => loot_table_count = count,
                _ => {}
            }
        }
        Ok(CdbSummary {
            archive_path: pak.path().to_path_buf(),
            cdb_absolute_offset: pak.header_size + entry.offset,
            cdb_size: entry.size,
            sheet_count: sheets.len(),
            item_count,
            loot_table_count,
            fingerprint: BuildFingerprint {
                steam_build_id: self.steam_build_id(),
                hlboot_sha256: sha256(self.directory.join("hlboot.dat"))?,
                cdb_checksum: entry.checksum,
                cdb_size: entry.size,
            },
        })
    }

    /// Reads the embedded `CastleDB` document without extracting or changing the
    /// game installation.
    ///
    /// # Errors
    ///
    /// Returns an I/O, PAK, or JSON error when `data.cdb` cannot be decoded.
    pub fn load_cdb(&self) -> Result<Value, GameDataError> {
        let pak = HeapsPak::open(self.directory.join("res.light.pak"))?;
        Ok(serde_json::from_slice(&pak.read("data.cdb")?)?)
    }

    fn steam_build_id(&self) -> Option<String> {
        let manifest = self
            .directory
            .parent()?
            .parent()?
            .join(format!("appmanifest_{APP_ID}.acf"));
        let text = std::fs::read_to_string(manifest).ok()?;
        quoted_value(&text, "buildid")
    }
}

/// Finds the Steam installation, honoring `FAREVER_GAME_DIR` first.
///
/// # Errors
///
/// Returns [`GameDataError::NotFound`] when none of the configured or common
/// Steam-library locations contains a valid Farever installation.
pub fn discover_game() -> Result<GameInstall, GameDataError> {
    if let Some(configured) = std::env::var_os("FAREVER_GAME_DIR") {
        if let Ok(game) = GameInstall::open(PathBuf::from(configured)) {
            return Ok(game);
        }
    }

    let mut steam_roots = BTreeSet::new();
    steam_roots.insert(PathBuf::from(r"C:\Program Files (x86)\Steam"));
    steam_roots.insert(PathBuf::from(r"C:\Program Files\Steam"));
    for letter in b'C'..=b'Z' {
        let drive = letter as char;
        steam_roots.insert(PathBuf::from(format!(r"{drive}:\SteamLibrary")));
        steam_roots.insert(PathBuf::from(format!(r"{drive}:\Games\SteamLibrary")));
    }

    let initial: Vec<PathBuf> = steam_roots.iter().cloned().collect();
    for root in initial {
        let libraries = root.join("steamapps").join("libraryfolders.vdf");
        if let Ok(text) = std::fs::read_to_string(libraries) {
            for path in quoted_values(&text, "path") {
                steam_roots.insert(PathBuf::from(path.replace(r"\\", r"\")));
            }
        }
    }

    for root in steam_roots {
        let candidate = root.join("steamapps").join("common").join("Farever");
        if let Ok(game) = GameInstall::open(candidate) {
            return Ok(game);
        }
    }
    Err(GameDataError::NotFound)
}

fn sha256(path: impl AsRef<Path>) -> Result<String, std::io::Error> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    // Keep the large streaming buffer off the relatively small Windows thread
    // stack. A boxed slice retains a single allocation and the same read loop.
    let mut buffer = vec![0_u8; 1024 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn quoted_value(text: &str, key: &str) -> Option<String> {
    quoted_values(text, key).into_iter().next()
}

fn quoted_values(text: &str, key: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let mut values = line.split('"').filter(|part| !part.trim().is_empty());
            let found_key = values.next()?.trim();
            let value = values.next()?.trim();
            (found_key.eq_ignore_ascii_case(key)).then(|| value.to_owned())
        })
        .collect()
}
