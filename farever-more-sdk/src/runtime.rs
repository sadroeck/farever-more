//! Logging, timers, and lifecycle values.

use crate::__wit::exports::farever::addon::plugin as raw_plugin;
use crate::__wit::farever::addon::runtime as raw_runtime;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Tick {
    pub scheduled_at_ms: u64,
    pub delivered_at_ms: u64,
    pub elapsed: Duration,
    pub interval: Duration,
    pub missed: u32,
}

impl Tick {
    #[doc(hidden)]
    pub fn from_raw(value: raw_plugin::Tick) -> Self {
        Self {
            scheduled_at_ms: value.scheduled_at_ms,
            delivered_at_ms: value.delivered_at_ms,
            elapsed: Duration::from_millis(value.elapsed_ms),
            interval: Duration::from_millis(u64::from(value.interval_ms)),
            missed: value.missed,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownReason {
    HostShutdown,
    Reloaded,
    Disabled,
    Removed,
    RepeatedFailure,
}

impl ShutdownReason {
    #[doc(hidden)]
    pub fn from_raw(value: raw_plugin::DeactivationReason) -> Self {
        match value {
            raw_plugin::DeactivationReason::HostShutdown => Self::HostShutdown,
            raw_plugin::DeactivationReason::Reloaded => Self::Reloaded,
            raw_plugin::DeactivationReason::Disabled => Self::Disabled,
            raw_plugin::DeactivationReason::Removed => Self::Removed,
            raw_plugin::DeactivationReason::RepeatedFailure => Self::RepeatedFailure,
        }
    }
}

pub struct Logger {
    _private: (),
}

impl Logger {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self { _private: () }
    }

    pub fn trace(&self, message: &str) {
        raw_runtime::log(raw_runtime::LogLevel::Trace, message);
    }

    pub fn info(&self, message: &str) {
        raw_runtime::log(raw_runtime::LogLevel::Info, message);
    }

    pub fn warning(&self, message: &str) {
        raw_runtime::log(raw_runtime::LogLevel::Warning, message);
    }

    pub fn error(&self, message: &str) {
        raw_runtime::log(raw_runtime::LogLevel::Error, message);
    }
}

pub struct Timer {
    _private: (),
}

impl Timer {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Starts or replaces the add-on's recurring timer and returns the host's
    /// accepted (possibly clamped) interval.
    pub fn schedule(&self, interval: Duration) -> Duration {
        let requested = interval.as_millis().min(u128::from(u32::MAX)) as u32;
        Duration::from_millis(u64::from(raw_runtime::schedule_tick(requested)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_exposes_durations_instead_of_unit_named_integers() {
        let tick = Tick::from_raw(raw_plugin::Tick {
            scheduled_at_ms: 500,
            delivered_at_ms: 505,
            elapsed_ms: 100,
            interval_ms: 50,
            missed: 1,
        });

        assert_eq!(tick.elapsed, Duration::from_millis(100));
        assert_eq!(tick.interval, Duration::from_millis(50));
        assert_eq!(tick.missed, 1);
    }

    #[test]
    fn shutdown_reasons_preserve_manager_lifecycle_intent() {
        assert_eq!(
            ShutdownReason::from_raw(raw_plugin::DeactivationReason::HostShutdown),
            ShutdownReason::HostShutdown
        );
        assert_eq!(
            ShutdownReason::from_raw(raw_plugin::DeactivationReason::Reloaded),
            ShutdownReason::Reloaded
        );
        assert_eq!(
            ShutdownReason::from_raw(raw_plugin::DeactivationReason::Disabled),
            ShutdownReason::Disabled
        );
        assert_eq!(
            ShutdownReason::from_raw(raw_plugin::DeactivationReason::Removed),
            ShutdownReason::Removed
        );
        assert_eq!(
            ShutdownReason::from_raw(raw_plugin::DeactivationReason::RepeatedFailure),
            ShutdownReason::RepeatedFailure
        );
    }
}
