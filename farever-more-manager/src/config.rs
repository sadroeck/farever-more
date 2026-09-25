//! Persistent manager options, e.g. the game install location.
//!
//! Stored as a small JSON document outside the game directory so reinstalling
//! or verifying the game never wipes it:
//! `%APPDATA%/Farever/addon-manager.json` on Windows,
//! `~/.config/farever/addon-manager.json` elsewhere, next to the manager
//! executable as a last resort.

use crate::backend;
use std::fs;
use std::path::PathBuf;

const CONFIG_FILE_NAME: &str = "addon-manager.json";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManagerConfig {
    /// Explicit game install location from the settings dialog or
    /// `--set-game-dir`. Takes precedence over `FAREVER_GAME_DIR`.
    pub(crate) game_dir: Option<PathBuf>,
}

impl ManagerConfig {
    pub(crate) fn load() -> Self {
        Self::load_from(&default_path())
    }

    pub(crate) fn load_from(path: &std::path::Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Self::default();
        };
        let game_dir = value
            .get("game-dir")
            .and_then(serde_json::Value::as_str)
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from);
        Self { game_dir }
    }

    pub(crate) fn save(&self) -> Result<PathBuf, String> {
        self.save_to(&default_path())
    }

    pub(crate) fn save_to(&self, path: &std::path::Path) -> Result<PathBuf, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let mut object = serde_json::Map::new();
        if let Some(game_dir) = &self.game_dir {
            object.insert(
                "game-dir".to_owned(),
                serde_json::Value::String(game_dir.display().to_string()),
            );
        }
        let text = serde_json::Value::Object(object).to_string();
        fs::write(path, text).map_err(|error| format!("write {}: {error}", path.display()))?;
        Ok(path.to_owned())
    }

    /// Resolves the add-on root:
    /// `FAREVER_ADDONS_DIR`, then the configured game directory, then real
    /// Steam auto-detection ([`farever_db::discover_game`], which also honors
    /// `FAREVER_GAME_DIR`), then `<manager-exe-dir>/farever-addons`, then
    /// `./farever-addons`.
    pub(crate) fn addon_root(&self) -> PathBuf {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(ToOwned::to_owned));
        Self::addon_root_from(
            std::env::var_os("FAREVER_ADDONS_DIR").map(PathBuf::from),
            self.game_dir.clone(),
            farever_db::discover_game().ok().map(|game| game.directory),
            std::env::var_os("FAREVER_GAME_DIR").map(PathBuf::from),
            exe_dir,
            std::env::current_dir().ok(),
        )
    }

    /// Pure resolution order, with the Steam result injected for testability.
    pub(crate) fn addon_root_from(
        addons_env: Option<PathBuf>,
        config_game: Option<PathBuf>,
        steam_game: Option<PathBuf>,
        game_env: Option<PathBuf>,
        exe_dir: Option<PathBuf>,
        cwd: Option<PathBuf>,
    ) -> PathBuf {
        if let Some(dir) = addons_env.filter(|dir| !dir.as_os_str().is_empty()) {
            return dir;
        }
        if let Some(game) = config_game.filter(|dir| !dir.as_os_str().is_empty()) {
            return game.join("farever-addons");
        }
        if let Some(game) = steam_game {
            return game.join("farever-addons");
        }
        backend::resolve_addon_root_from(None, game_env, exe_dir, cwd)
    }

    /// Where the add-on root's game directory came from, for the settings UI.
    pub(crate) fn game_dir_source(&self) -> &'static str {
        if self.game_dir.is_some() {
            "configured in settings"
        } else if std::env::var_os("FAREVER_GAME_DIR").is_some() {
            "FAREVER_GAME_DIR"
        } else {
            "auto-detected"
        }
    }
}

pub(crate) fn default_path() -> PathBuf {
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) {
        return appdata.join("Farever").join(CONFIG_FILE_NAME);
    }
    #[cfg(not(windows))]
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        return home.join(".config").join("farever").join(CONFIG_FILE_NAME);
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(ToOwned::to_owned))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(CONFIG_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::TempRoot;

    #[test]
    fn missing_config_loads_defaults() {
        let root = TempRoot::new();
        let config = ManagerConfig::load_from(&root.addon_root().join("missing.json"));

        assert_eq!(config, ManagerConfig::default());
    }

    #[test]
    fn corrupt_config_loads_defaults() {
        let root = TempRoot::new();
        let path = root.addon_root().join("config.json");
        fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        fs::write(&path, b"{not json").expect("write");

        assert_eq!(ManagerConfig::load_from(&path), ManagerConfig::default());
    }

    #[test]
    fn game_dir_roundtrips_through_disk() {
        let root = TempRoot::new();
        let path = root.addon_root().join("config.json");
        let config = ManagerConfig {
            game_dir: Some(PathBuf::from("/tmp/fake-game")),
        };

        config.save_to(&path).expect("save");
        assert_eq!(ManagerConfig::load_from(&path), config);
    }

    #[test]
    fn resolution_prefers_addons_dir_then_config_then_steam() {
        let explicit = PathBuf::from("/tmp/explicit-addons");
        let configured = PathBuf::from("/tmp/configured-game");
        let steam = PathBuf::from("/tmp/steam-game");
        let fallback_exe = PathBuf::from("/tmp/exe");

        assert_eq!(
            ManagerConfig::addon_root_from(
                Some(explicit.clone()),
                Some(configured.clone()),
                Some(steam.clone()),
                None,
                Some(fallback_exe.clone()),
                None,
            ),
            explicit
        );
        assert_eq!(
            ManagerConfig::addon_root_from(
                None,
                Some(configured.clone()),
                Some(steam.clone()),
                None,
                Some(fallback_exe.clone()),
                None,
            ),
            configured.join("farever-addons")
        );
        assert_eq!(
            ManagerConfig::addon_root_from(
                None,
                None,
                Some(steam.clone()),
                None,
                Some(fallback_exe.clone()),
                None,
            ),
            steam.join("farever-addons")
        );
        assert_eq!(
            ManagerConfig::addon_root_from(
                None,
                None,
                None,
                None,
                Some(fallback_exe.clone()),
                None
            ),
            fallback_exe.join("farever-addons")
        );
    }
}
