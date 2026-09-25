use farever_more_api::{
    FasSnapshotV0, ADAPTER_LIVE, ADAPTER_SEARCHING, ADAPTER_UNAVAILABLE, ADAPTER_WAITING_FOR_GAME,
    ADAPTER_WAITING_TO_SCAN,
};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Verbosity of the host log.
///
/// The level decides whether a line is written, never how it looks: the
/// `[unix_ms=…] <category> …` shape is unchanged so existing greps keep
/// working. `Info` carries lifecycle facts, state transitions, and failures.
/// Steady-state telemetry that used to be written once per poll lives at
/// `Debug` and is opt-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    const fn order(self) -> u8 {
        match self {
            Self::Error => 0,
            Self::Warn => 1,
            Self::Info => 2,
            Self::Debug => 3,
        }
    }

    /// Parses a [`LOG_LEVEL_ENV`] value. Unknown text is ignored so a typo
    /// cannot silence the log.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" | "verbose" | "trace" => Some(Self::Debug),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }

    const fn allows(self, level: Self) -> bool {
        level.order() <= self.order()
    }
}

/// Environment override for the log level, read once when the host starts.
pub const LOG_LEVEL_ENV: &str = "FAREVER_LOG_LEVEL";

const LOG_FILE_NAME: &str = "host.log";
const LOG_BACKUP_COUNT: u32 = 3;
/// Rotation bounds a long-running host instead of one file that grows without
/// limit; the previous build reached 60 MB in a single `host.log`.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

pub struct RuntimeLog {
    file: Option<File>,
    directory: Option<PathBuf>,
    level: Level,
    written: u64,
    last_state: Option<String>,
}

impl RuntimeLog {
    pub fn open(addon_root: &Path) -> Self {
        let level = std::env::var(LOG_LEVEL_ENV)
            .ok()
            .as_deref()
            .and_then(Level::parse)
            .unwrap_or(Level::Info);
        Self::with_level(addon_root, level)
    }

    fn with_level(addon_root: &Path, level: Level) -> Self {
        let directory = addon_root.join("logs");
        let mut log = Self {
            file: None,
            directory: None,
            level,
            written: 0,
            last_state: None,
        };
        if fs::create_dir_all(&directory).is_err() {
            return log;
        }
        let path = directory.join(LOG_FILE_NAME);
        if fs::metadata(&path).is_ok_and(|metadata| metadata.len() >= MAX_LOG_BYTES) {
            rotate(&directory);
        }
        log.written = fs::metadata(&path).map_or(0, |metadata| metadata.len());
        log.file = open_append(&path);
        log.directory = Some(directory);
        log
    }

    pub fn level(&self) -> Level {
        self.level
    }

    /// For call sites that must not pay for formatting a line nobody will read.
    pub fn allows(&self, level: Level) -> bool {
        self.level.allows(level)
    }

    pub fn error(&mut self, message: &str) {
        self.write(Level::Error, message);
    }

    pub fn warn(&mut self, message: &str) {
        self.write(Level::Warn, message);
    }

    pub fn info(&mut self, message: &str) {
        self.write(Level::Info, message);
    }

    pub fn debug(&mut self, message: &str) {
        self.write(Level::Debug, message);
    }

    pub fn log(&mut self, level: Level, message: &str) {
        self.write(level, message);
    }

    fn write(&mut self, level: Level, message: &str) {
        if !self.allows(level) {
            return;
        }
        let Some(file) = &mut self.file else {
            return;
        };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let line = format!("[unix_ms={timestamp}] {message}");
        if writeln!(file, "{line}").is_err() {
            return;
        }
        let _ = file.flush();
        self.written = self.written.saturating_add(line.len() as u64 + 2);
        if self.written >= MAX_LOG_BYTES {
            self.rotate();
        }
    }

    fn rotate(&mut self) {
        let Some(directory) = self.directory.clone() else {
            return;
        };
        self.file = None;
        rotate(&directory);
        self.file = open_append(&directory.join(LOG_FILE_NAME));
        self.written = 0;
    }

    pub fn record_snapshot(&mut self, snapshot: &FasSnapshotV0) {
        let adapter = match snapshot.adapter_status {
            ADAPTER_LIVE => "live",
            ADAPTER_SEARCHING => "searching",
            ADAPTER_WAITING_FOR_GAME => "waiting_for_game",
            ADAPTER_UNAVAILABLE => "unavailable",
            ADAPTER_WAITING_TO_SCAN => "waiting_to_scan",
            _ => "unknown",
        };
        let windows = (0..snapshot.window_count as usize)
            .filter_map(|index| snapshot.window(index))
            .collect::<Vec<_>>();
        // The per-window detail is only worth its bytes while tracking a
        // window-topology problem, so `info` records how many windows exist.
        let window_detail = if self.allows(Level::Debug) {
            format!("windows={windows:?}")
        } else {
            format!("windows={}", windows.len())
        };
        let state = format!(
            "adapter={adapter} pid={} app_found={} in_world={} loading_state={} area={:?} {window_detail}",
            snapshot.process_id,
            snapshot.app_found != 0,
            snapshot.in_world != 0,
            snapshot.loading_state,
            snapshot.area(),
        );
        if self.last_state.as_deref() == Some(&state) {
            return;
        }
        self.info(&format!("state {state}"));
        self.last_state = Some(state);
    }
}

fn open_append(path: &Path) -> Option<File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// Shifts `host.log` into numbered backups, dropping the oldest.
fn rotate(directory: &Path) {
    let _ = fs::remove_file(backup_path(directory, LOG_BACKUP_COUNT));
    for index in (1..LOG_BACKUP_COUNT).rev() {
        let from = backup_path(directory, index);
        if from.exists() {
            let _ = fs::rename(&from, backup_path(directory, index + 1));
        }
    }
    let _ = fs::rename(directory.join(LOG_FILE_NAME), backup_path(directory, 1));
}

fn backup_path(directory: &Path, index: u32) -> PathBuf {
    directory.join(format!("{LOG_FILE_NAME}.{index}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("farever-runtime-log-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("create isolated log directory");
        directory
    }

    #[test]
    fn level_parsing_accepts_known_names_and_ignores_typos() {
        assert_eq!(Level::parse(" INFO "), Some(Level::Info));
        assert_eq!(Level::parse("Warn"), Some(Level::Warn));
        assert_eq!(Level::parse("verbose"), Some(Level::Debug));
        assert_eq!(Level::parse("error"), Some(Level::Error));
        assert_eq!(Level::parse("chatty"), None);
        assert_eq!(Level::parse(""), None);
    }

    #[test]
    fn a_silent_level_still_keeps_its_own_lines() {
        assert!(Level::Error.allows(Level::Error));
        assert!(!Level::Error.allows(Level::Warn));
        assert!(Level::Info.allows(Level::Error));
        assert!(Level::Info.allows(Level::Info));
        assert!(!Level::Info.allows(Level::Debug));
        assert!(Level::Debug.allows(Level::Debug));
    }

    #[test]
    fn rotation_shifts_backups_and_drops_the_oldest() {
        let directory = test_directory("rotate");
        for index in 0..=LOG_BACKUP_COUNT {
            let path = if index == 0 {
                directory.join(LOG_FILE_NAME)
            } else {
                backup_path(&directory, index)
            };
            fs::write(&path, format!("generation {index}")).expect("write log generation");
        }

        rotate(&directory);

        assert_eq!(
            fs::read_to_string(directory.join(LOG_FILE_NAME))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        for index in 1..=LOG_BACKUP_COUNT {
            let contents = fs::read_to_string(backup_path(&directory, index)).expect("read backup");
            let expected = if index == 1 { 0 } else { index - 1 };
            assert_eq!(contents, format!("generation {expected}"));
        }
    }

    #[test]
    fn an_oversized_log_is_rotated_when_it_is_opened() {
        let root = test_directory("oversized");
        let logs = root.join("logs");
        fs::create_dir_all(&logs).expect("create logs directory");
        let path = logs.join(LOG_FILE_NAME);
        let file = File::create(&path).expect("create oversized log");
        file.set_len(MAX_LOG_BYTES).expect("inflate log");
        drop(file);

        let mut log = RuntimeLog::open(&root);
        log.info("runtime started target=test");

        assert!(backup_path(&logs, 1).exists());
        let contents = fs::read_to_string(&path).expect("read live log");
        assert!(contents.contains("runtime started"));
        assert_eq!(contents.lines().count(), 1);
    }

    #[test]
    fn a_low_level_suppresses_lower_verbosity_lines() {
        let root = test_directory("filtered");
        let mut log = RuntimeLog::with_level(&root, Level::Info);
        assert_eq!(log.level(), Level::Info);
        log.info("kept");
        log.warn("kept warning");
        log.debug("dropped");
        assert!(!log.allows(Level::Debug));

        let contents = fs::read_to_string(root.join("logs").join(LOG_FILE_NAME)).expect("read log");
        assert!(contents.contains("kept"));
        assert!(contents.contains("kept warning"));
        assert!(!contents.contains("dropped"));
    }
}
