use crate::config::{
    ConfigError, ConfigPropertyAccess, ConfigPropertyDescriptor, ConfigRegistry, ConfigValue,
    ConfigValueKind,
};
use std::path::Path;

const HOST_CONFIG_ID: &str = "farever.host";
const SLASH_COMMANDS_KEY: &str = "slash-commands.enabled";
pub(crate) const DEFAULT_SLASH_COMMANDS_ENABLED: bool = true;

pub(crate) struct HostSettings {
    config: ConfigRegistry,
    slash_commands_enabled: bool,
}

impl HostSettings {
    pub(crate) fn open(config_root: &Path) -> Result<Self, ConfigError> {
        let mut config =
            ConfigRegistry::open(config_root, HOST_CONFIG_ID, Some(env!("CARGO_PKG_VERSION")))?;
        let stored = config.register(ConfigPropertyDescriptor {
            key: SLASH_COMMANDS_KEY.to_owned(),
            label: "Enable slash commands".to_owned(),
            description: Some(
                "Allow add-ons to receive commands entered in Farever chat.".to_owned(),
            ),
            value_kind: ConfigValueKind::Boolean,
            default_value: ConfigValue::Boolean(DEFAULT_SLASH_COMMANDS_ENABLED),
            access: ConfigPropertyAccess::Editable,
        })?;
        let slash_commands_enabled = match stored {
            ConfigValue::Boolean(enabled) => enabled,
            _ => {
                config.set(
                    SLASH_COMMANDS_KEY.to_owned(),
                    ConfigValue::Boolean(DEFAULT_SLASH_COMMANDS_ENABLED),
                )?;
                DEFAULT_SLASH_COMMANDS_ENABLED
            }
        };
        Ok(Self {
            config,
            slash_commands_enabled,
        })
    }

    pub(crate) fn slash_commands_enabled(&self) -> bool {
        self.slash_commands_enabled
    }

    pub(crate) fn set_slash_commands_enabled(&mut self, enabled: bool) -> Result<(), ConfigError> {
        self.config
            .set(SLASH_COMMANDS_KEY.to_owned(), ConfigValue::Boolean(enabled))?;
        self.slash_commands_enabled = enabled;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("farever-host-settings-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp root");
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn slash_command_setting_defaults_on_and_persists_immediately() {
        let root = TempRoot::new();
        let mut settings = HostSettings::open(&root.0).expect("open settings");
        assert!(settings.slash_commands_enabled());

        settings
            .set_slash_commands_enabled(false)
            .expect("disable slash commands");

        let reopened = HostSettings::open(&root.0).expect("reopen settings");
        assert!(!reopened.slash_commands_enabled());
    }
}
