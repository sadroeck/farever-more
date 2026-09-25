use crate::memory::ProcessMemory;
use crate::overlay::largest_visible_client_area;
use std::time::{Duration, Instant};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// A repeated line only exists to prove a slow startup is still progressing, so
/// it is a heartbeat rather than a sample: phase changes are logged immediately.
const DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(30);
const FALLBACK_GRACE: Duration = Duration::from_secs(10);
const QUIET_SAMPLES_REQUIRED: u8 = 5;
const MIN_GAME_WINDOW_AREA: i64 = 200_000;
const MIN_QUIET_DELTA: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    WaitingForWindow,
    WaitingForWorld,
    WorldObserved,
    Loading,
    Settling,
    MemoryCounterUnavailable,
    Ready,
}

/// Cheap fallback pre-scan gate. The normal injected path resolves its anchor
/// directly from an allocator-learned type and never calls this after the Hero
/// signal. If that fast path is unavailable, injected fallback is gated by the
/// validated local Hero; the window metric exists only for the external,
/// read-only companion path, which cannot install the allocation hook.
pub struct ReadinessGate {
    phase: Option<Phase>,
    last_sample: Option<Instant>,
    last_diagnostic: Option<Instant>,
    visible_since: Option<Instant>,
    last_private_bytes: Option<usize>,
    quiet_samples: u8,
    world_signal_seen: bool,
    ready: bool,
}

impl ReadinessGate {
    pub fn new() -> Self {
        Self {
            phase: None,
            last_sample: None,
            last_diagnostic: None,
            visible_since: None,
            last_private_bytes: None,
            quiet_samples: 0,
            world_signal_seen: false,
            ready: false,
        }
    }

    pub fn check(
        &mut self,
        memory: &ProcessMemory,
        world_probe: Option<bool>,
        diagnostics: &mut Vec<String>,
    ) -> bool {
        if self.ready {
            return true;
        }

        let now = Instant::now();
        if self
            .last_sample
            .is_some_and(|last| now.duration_since(last) < SAMPLE_INTERVAL)
        {
            return false;
        }
        self.last_sample = Some(now);

        if world_probe == Some(false) && !self.world_signal_seen {
            self.last_private_bytes = None;
            self.quiet_samples = 0;
            self.record_phase(
                now,
                Phase::WaitingForWorld,
                "readiness phase=waiting_for_world signal=validated_local_hero".to_owned(),
                diagnostics,
            );
            return false;
        }
        if world_probe == Some(true) && !self.world_signal_seen {
            self.world_signal_seen = true;
            self.visible_since = Some(now);
            self.last_private_bytes = None;
            self.quiet_samples = 0;
            self.record_phase(
                now,
                Phase::WorldObserved,
                "readiness phase=world_observed signal=validated_local_hero settling_before_scan=true"
                    .to_owned(),
                diagnostics,
            );
            return false;
        }

        // External inspection has no in-process allocation signal. Preserve
        // its legacy compatibility gate, but never use window size to decide
        // readiness for the injected add-on host.
        let area = if world_probe.is_none() {
            let area = largest_visible_client_area(memory.pid());
            if area < MIN_GAME_WINDOW_AREA {
                self.visible_since = None;
                self.last_private_bytes = None;
                self.quiet_samples = 0;
                self.record_phase(
                    now,
                    Phase::WaitingForWindow,
                    format!("readiness phase=waiting_for_game_window largest_client_area={area}"),
                    diagnostics,
                );
                return false;
            }
            Some(area)
        } else {
            None
        };
        let visible_since = *self.visible_since.get_or_insert(now);

        let Some(private_bytes) = memory.private_bytes() else {
            self.record_phase(
                now,
                Phase::MemoryCounterUnavailable,
                format!(
                    "readiness phase=post_signal_grace memory_counter=unavailable client_area={} grace_seconds={}",
                    area.map_or("not_used".to_owned(), |value| value.to_string()),
                    FALLBACK_GRACE.as_secs()
                ),
                diagnostics,
            );
            if now.duration_since(visible_since) >= FALLBACK_GRACE {
                self.mark_ready(area, None, diagnostics);
            }
            return self.ready;
        };

        let delta = self
            .last_private_bytes
            .map(|last| last.abs_diff(private_bytes));
        self.last_private_bytes = Some(private_bytes);
        let quiet_limit = MIN_QUIET_DELTA.max(private_bytes / 100);
        let quiet = delta.is_some_and(|value| value <= quiet_limit);
        if quiet {
            self.quiet_samples = self.quiet_samples.saturating_add(1);
        } else {
            self.quiet_samples = 0;
        }

        let private_mib = private_bytes / (1024 * 1024);
        let delta_mib = delta.map_or(0, |value| value / (1024 * 1024));
        if quiet {
            self.record_phase(
                now,
                Phase::Settling,
                format!(
                    "readiness phase=settling client_area={} private_mib={private_mib} delta_mib={delta_mib} quiet_samples={}/{}",
                    area.map_or("not_used".to_owned(), |value| value.to_string()),
                    self.quiet_samples, QUIET_SAMPLES_REQUIRED
                ),
                diagnostics,
            );
        } else {
            self.record_phase(
                now,
                Phase::Loading,
                format!(
                    "readiness phase=loading client_area={} private_mib={private_mib} delta_mib={delta_mib} quiet_limit_mib={}",
                    area.map_or("not_used".to_owned(), |value| value.to_string()),
                    quiet_limit / (1024 * 1024)
                ),
                diagnostics,
            );
        }

        if self.quiet_samples >= QUIET_SAMPLES_REQUIRED {
            self.mark_ready(area, Some(private_mib), diagnostics);
        }
        self.ready
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    fn mark_ready(
        &mut self,
        area: Option<i64>,
        private_mib: Option<usize>,
        diagnostics: &mut Vec<String>,
    ) {
        self.ready = true;
        self.phase = Some(Phase::Ready);
        self.last_diagnostic = Some(Instant::now());
        diagnostics.push(format!(
            "readiness phase=ready client_area={} private_mib={} quiet_samples={} world_signal_seen={} starting_hashlink_locator=true",
            area.map_or("not_used".to_owned(), |value| value.to_string()),
            private_mib.map_or("unavailable".to_owned(), |value| value.to_string()),
            self.quiet_samples,
            self.world_signal_seen,
        ));
    }

    fn record_phase(
        &mut self,
        now: Instant,
        phase: Phase,
        message: String,
        diagnostics: &mut Vec<String>,
    ) {
        let periodic = self
            .last_diagnostic
            .is_none_or(|last| now.duration_since(last) >= DIAGNOSTIC_INTERVAL);
        if self.phase != Some(phase) || periodic {
            self.phase = Some(phase);
            self.last_diagnostic = Some(now);
            diagnostics.push(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_limit_has_a_floor_and_scales_for_large_processes() {
        assert_eq!(
            MIN_QUIET_DELTA.max(512 * 1024 * 1024 / 100),
            MIN_QUIET_DELTA
        );
        assert_eq!(
            MIN_QUIET_DELTA.max(8 * 1024 * 1024 * 1024 / 100),
            85_899_345
        );
    }
}
