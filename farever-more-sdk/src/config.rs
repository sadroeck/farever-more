//! Typed add-on settings.

use crate::__wit::farever::addon::config as raw;
use crate::SdkResult;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    Editable,
    Readonly,
    Hidden,
}

impl From<Access> for raw::ConfigPropertyAccess {
    fn from(value: Access) -> Self {
        match value {
            Access::Editable => Self::Editable,
            Access::Readonly => Self::Readonly,
            Access::Hidden => Self::Hidden,
        }
    }
}

pub struct Setting<T> {
    key: &'static str,
    label: &'static str,
    description: Option<&'static str>,
    default: T,
    access: Access,
}

impl<T> Setting<T> {
    #[must_use]
    pub const fn label(mut self, label: &'static str) -> Self {
        self.label = label;
        self
    }

    #[must_use]
    pub const fn description(mut self, description: &'static str) -> Self {
        self.description = Some(description);
        self
    }

    #[must_use]
    pub const fn readonly(mut self) -> Self {
        self.access = Access::Readonly;
        self
    }

    #[must_use]
    pub const fn hidden(mut self) -> Self {
        self.access = Access::Hidden;
        self
    }

    #[must_use]
    pub const fn key(&self) -> &'static str {
        self.key
    }
}

impl Setting<bool> {
    #[must_use]
    pub const fn boolean(key: &'static str, default: bool) -> Self {
        Self {
            key,
            label: key,
            description: None,
            default,
            access: Access::Editable,
        }
    }
}

impl Setting<i64> {
    #[must_use]
    pub const fn integer(key: &'static str, default: i64) -> Self {
        Self {
            key,
            label: key,
            description: None,
            default,
            access: Access::Editable,
        }
    }
}

impl Setting<f64> {
    #[must_use]
    pub const fn number(key: &'static str, default: f64) -> Self {
        Self {
            key,
            label: key,
            description: None,
            default,
            access: Access::Editable,
        }
    }
}

impl Setting<String> {
    #[must_use]
    pub fn text(key: &'static str, default: impl Into<String>) -> Self {
        Self {
            key,
            label: key,
            description: None,
            default: default.into(),
            access: Access::Editable,
        }
    }
}

impl Setting<Vec<u8>> {
    #[must_use]
    pub fn bytes(key: &'static str, default: impl Into<Vec<u8>>) -> Self {
        Self {
            key,
            label: key,
            description: None,
            default: default.into(),
            access: Access::Editable,
        }
    }
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for bool {}
    impl Sealed for i64 {}
    impl Sealed for f64 {}
    impl Sealed for String {}
    impl Sealed for Vec<u8> {}
}

pub trait Value: sealed::Sealed {
    fn kind() -> raw::ConfigValueKind;
    fn to_raw(&self) -> raw::ConfigValue;
    fn from_raw(value: raw::ConfigValue) -> SdkResult<Self>
    where
        Self: Sized;
}

macro_rules! scalar_value {
    ($type:ty, $kind:ident, $variant:ident) => {
        impl Value for $type {
            fn kind() -> raw::ConfigValueKind {
                raw::ConfigValueKind::$kind
            }

            fn to_raw(&self) -> raw::ConfigValue {
                raw::ConfigValue::$variant(self.clone())
            }

            fn from_raw(value: raw::ConfigValue) -> SdkResult<Self> {
                match value {
                    raw::ConfigValue::$variant(value) => Ok(value),
                    _ => Err("host returned the wrong type for a registered setting".to_owned()),
                }
            }
        }
    };
}

scalar_value!(bool, Boolean, Boolean);
scalar_value!(i64, Integer, Integer);
scalar_value!(f64, Number, Number);
scalar_value!(String, Text, Text);
scalar_value!(Vec<u8>, Bytes, Bytes);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Status {
    pub revision: u64,
    pub saved_by_addon_version: Option<String>,
    pub used_bytes: u64,
    pub quota_bytes: u64,
}

impl From<raw::ConfigStatus> for Status {
    fn from(value: raw::ConfigStatus) -> Self {
        Self {
            revision: value.revision,
            saved_by_addon_version: value.saved_by_addon_version,
            used_bytes: value.used_bytes,
            quota_bytes: value.quota_bytes,
        }
    }
}

pub struct Config {
    _private: (),
}

impl Config {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self { _private: () }
    }

    pub fn get<T: Value>(&self, setting: &Setting<T>) -> SdkResult<T> {
        T::from_raw(raw::get(setting.key)?)
    }

    pub fn set<T: Value>(&self, setting: &Setting<T>, value: &T) -> SdkResult<Status> {
        raw::set(setting.key, &value.to_raw()).map(Into::into)
    }

    pub fn remove<T>(&self, setting: &Setting<T>) -> SdkResult<Status> {
        raw::remove(setting.key).map(Into::into)
    }

    #[must_use]
    pub fn status(&self) -> Status {
        raw::status().into()
    }
}

pub struct ActivationConfig {
    config: Config,
}

impl ActivationConfig {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self {
            config: Config::new(),
        }
    }

    pub fn register<T: Value>(&self, setting: &Setting<T>) -> SdkResult<T> {
        T::from_raw(raw::register_property(&raw::ConfigPropertyDescriptor {
            key: setting.key.to_owned(),
            label: setting.label.to_owned(),
            description: setting.description.map(str::to_owned),
            value_kind: T::kind(),
            default_value: setting.default.to_raw(),
            access: setting.access.into(),
        })?)
    }

    pub fn get<T: Value>(&self, setting: &Setting<T>) -> SdkResult<T> {
        self.config.get(setting)
    }

    pub fn set<T: Value>(&self, setting: &Setting<T>, value: &T) -> SdkResult<Status> {
        self.config.set(setting, value)
    }
}
