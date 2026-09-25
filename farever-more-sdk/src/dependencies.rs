//! Synchronous clients for services provided by declared add-on dependencies.

use crate::__wit::farever::addon::dependencies as raw;
use std::fmt;

pub struct Dependencies {
    _private: (),
}

impl Dependencies {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Opens a service provided by one of this add-on's declared dependencies.
    ///
    /// `dependency` is the provider's add-on id as declared in the manifest,
    /// `service` the service name it advertises. Both must be declared, or the
    /// host reports [`OpenError::UndeclaredDependency`] /
    /// [`OpenError::UnavailableService`].
    pub fn open(&self, dependency: &str, service: &str) -> Result<Service, OpenError> {
        raw::open(dependency, service)
            .map(Service::from_raw)
            .map_err(Into::into)
    }
}

#[derive(Clone, Debug)]
pub struct Service {
    raw: raw::ServiceHandle,
}

impl Service {
    fn from_raw(raw: raw::ServiceHandle) -> Self {
        Self { raw }
    }

    /// Version of the providing add-on. A service has no version of its own:
    /// it is served at the version of the add-on providing it.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.raw.version
    }

    pub fn call(&self, operation: u32, request: &[u8]) -> Result<Vec<u8>, CallError> {
        raw::call(&self.raw, operation, request).map_err(Into::into)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenError {
    UndeclaredDependency,
    Unavailable,
    UnavailableService,
    IncompatibleVersion,
    QuotaExceeded,
}

impl From<raw::OpenError> for OpenError {
    fn from(value: raw::OpenError) -> Self {
        match value {
            raw::OpenError::UndeclaredDependency => Self::UndeclaredDependency,
            raw::OpenError::Unavailable => Self::Unavailable,
            raw::OpenError::UnavailableService => Self::UnavailableService,
            raw::OpenError::IncompatibleVersion => Self::IncompatibleVersion,
            raw::OpenError::QuotaExceeded => Self::QuotaExceeded,
        }
    }
}

impl fmt::Display for OpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for OpenError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallError {
    InvalidHandle,
    Unavailable,
    RequestTooLarge,
    ResponseTooLarge,
    QuotaExceeded,
    ProviderFailed(String),
    ProviderError(String),
}

impl From<raw::CallError> for CallError {
    fn from(value: raw::CallError) -> Self {
        match value {
            raw::CallError::InvalidHandle => Self::InvalidHandle,
            raw::CallError::Unavailable => Self::Unavailable,
            raw::CallError::RequestTooLarge => Self::RequestTooLarge,
            raw::CallError::ResponseTooLarge => Self::ResponseTooLarge,
            raw::CallError::QuotaExceeded => Self::QuotaExceeded,
            raw::CallError::ProviderFailed(message) => Self::ProviderFailed(message),
            raw::CallError::ProviderError(message) => Self::ProviderError(message),
        }
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for CallError {}
