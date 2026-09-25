//! Small, SDK-owned values shared by game snapshots and events.

use crate::__wit::farever::addon::common as raw;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl From<raw::Vec3> for Vec3 {
    fn from(value: raw::Vec3) -> Self {
        Self {
            x: value.x,
            y: value.y,
            z: value.z,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventHeader {
    pub sequence: u64,
    pub monotonic_ms: u64,
}

impl From<raw::EventHeader> for EventHeader {
    fn from(value: raw::EventHeader) -> Self {
        Self {
            sequence: value.sequence,
            monotonic_ms: value.monotonic_ms,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    NotInWorld,
    Loading,
    NotYetObserved,
    Unsupported,
    PermissionDenied,
    ProviderFailed,
}

impl From<raw::UnavailableReason> for UnavailableReason {
    fn from(value: raw::UnavailableReason) -> Self {
        match value {
            raw::UnavailableReason::NotInWorld => Self::NotInWorld,
            raw::UnavailableReason::Loading => Self::Loading,
            raw::UnavailableReason::NotYetObserved => Self::NotYetObserved,
            raw::UnavailableReason::Unsupported => Self::Unsupported,
            raw::UnavailableReason::PermissionDenied => Self::PermissionDenied,
            raw::UnavailableReason::ProviderFailed => Self::ProviderFailed,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StateStatus {
    pub observed_at_ms: Option<u64>,
    pub revision: u64,
    pub reason: Option<UnavailableReason>,
}

impl From<raw::StateStatus> for StateStatus {
    fn from(value: raw::StateStatus) -> Self {
        Self {
            observed_at_ms: value.observed_at_ms,
            revision: value.revision,
            reason: value.reason.map(Into::into),
        }
    }
}
