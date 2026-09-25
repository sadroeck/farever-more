use std::mem::zeroed;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

pub struct ThreadCpuMeter {
    last_wall: Instant,
    last_cpu_100ns: Option<u64>,
    percent: Option<f64>,
    sample_count: u64,
}

impl ThreadCpuMeter {
    pub fn new() -> Self {
        Self {
            last_wall: Instant::now(),
            last_cpu_100ns: current_thread_cpu_100ns(),
            percent: None,
            sample_count: 0,
        }
    }

    /// Samples at most once per second. Returns true when the displayed value
    /// changed. One `GetThreadTimes` call is the only recurring OS query.
    pub fn update(&mut self) -> bool {
        let elapsed = self.last_wall.elapsed();
        if elapsed < SAMPLE_INTERVAL {
            return false;
        }

        let current_cpu = current_thread_cpu_100ns();
        self.percent = self
            .last_cpu_100ns
            .zip(current_cpu)
            .map(|(last, current)| utilization_percent(current.saturating_sub(last), elapsed));
        self.last_wall = Instant::now();
        self.last_cpu_100ns = current_cpu;
        self.sample_count += 1;
        true
    }

    pub fn percent(&self) -> Option<f64> {
        self.percent
    }

    pub fn sample_count(&self) -> u64 {
        self.sample_count
    }
}

fn current_thread_cpu_100ns() -> Option<u64> {
    let mut creation: FILETIME = unsafe { zeroed() };
    let mut exit: FILETIME = unsafe { zeroed() };
    let mut kernel: FILETIME = unsafe { zeroed() };
    let mut user: FILETIME = unsafe { zeroed() };
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    (ok != 0).then(|| filetime_value(kernel).saturating_add(filetime_value(user)))
}

fn filetime_value(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

fn utilization_percent(cpu_delta_100ns: u64, wall: Duration) -> f64 {
    let wall_100ns = wall.as_secs_f64() * 10_000_000.0;
    if wall_100ns <= 0.0 {
        return 0.0;
    }
    ((cpu_delta_100ns as f64 / wall_100ns) * 100.0).clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_fraction_of_one_logical_core() {
        let percent = utilization_percent(2_500_000, Duration::from_secs(1));
        assert!((percent - 25.0).abs() < f64::EPSILON);
    }
}
