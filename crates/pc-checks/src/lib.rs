//! Read-only checks for the settings that most often make a PC slow.
//!
//! Each check is a function over a small trait for its data source, so tests use fakes. On
//! Windows, [`windows_source::WindowsSource`] is the real source. Nothing here writes to the
//! registry, changes a setting, starts a program or needs administrator rights; a reading
//! that cannot be taken comes back as `NotChecked` or `Unavailable`, never as `Ok`.
//!
//! The result types are provisional, see [`result`].

pub mod date;
pub mod gpu_driver;
pub mod memory_integrity;
pub mod power;
pub mod ram;
pub mod restart;
pub mod result;
pub mod smbios;
pub mod throttle;
#[cfg(windows)]
pub mod windows_source;

pub use gpu_driver::{check_gpu_driver_age, GpuDriverSource};
pub use memory_integrity::{check_memory_integrity, MemoryIntegritySource};
pub use power::{check_power, PowerSource};
pub use ram::{check_ram_speed, MemorySource};
pub use restart::{check_pending_restart, RestartSource};
pub use result::{unix_now, CheckId, CheckResult, Finding, Outcome, ReadError, Reading, Severity};
pub use throttle::{check_throttling, ThrottleSource};

/// Everything the six checks read.
pub trait Sources:
    MemorySource
    + PowerSource
    + MemoryIntegritySource
    + ThrottleSource
    + RestartSource
    + GpuDriverSource
{
}

impl<T> Sources for T where
    T: MemorySource
        + PowerSource
        + MemoryIntegritySource
        + ThrottleSource
        + RestartSource
        + GpuDriverSource
{
}

/// Run one check by id. `clock` gives the current Unix time in seconds; it is read again after
/// the reading finishes, so `measured_at` is when the reading was taken, not when it started.
pub fn run_check(id: CheckId, sources: &dyn Sources, clock: &dyn Fn() -> u64) -> CheckResult {
    let outcome = match id {
        CheckId::RamSpeed => check_ram_speed(sources),
        CheckId::PowerPlan => check_power(sources),
        CheckId::MemoryIntegrity => check_memory_integrity(sources),
        CheckId::CpuThrottling => check_throttling(sources),
        CheckId::PendingRestart => check_pending_restart(sources),
        CheckId::GpuDriverAge => check_gpu_driver_age(sources, clock()),
    };
    CheckResult {
        check: id,
        measured_at: clock(),
        outcome,
    }
}

/// Run all six checks, one after another (the throttling check takes about 3 seconds).
pub fn run_all(sources: &dyn Sources, clock: &dyn Fn() -> u64) -> Vec<CheckResult> {
    CheckId::ALL
        .iter()
        .map(|id| run_check(*id, sources, clock))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu_driver::GpuDriver;
    use crate::memory_integrity::MemoryIntegrity;
    use crate::power::{PlanKind, PowerMode, PowerStatus};
    use crate::ram::MemoryModule;
    use crate::throttle::ThrottleSample;
    use std::cell::Cell;
    use std::time::Duration;

    /// A PC where everything reads fine and nothing is wrong, except what a test changes.
    struct Healthy;
    impl MemorySource for Healthy {
        fn modules(&self) -> Reading<Vec<MemoryModule>> {
            Ok(vec![MemoryModule {
                slot: "DIMM1".into(),
                rated_mts: Some(3200),
                configured_mts: Some(3200),
            }])
        }
    }
    impl PowerSource for Healthy {
        fn active_plan(&self) -> Reading<PlanKind> {
            Ok(PlanKind::Balanced)
        }
        fn power_mode(&self) -> Reading<PowerMode> {
            Ok(PowerMode::Balanced)
        }
        fn status(&self) -> Reading<power::PowerStatus> {
            Ok(PowerStatus {
                on_ac: Some(true),
                battery_saver: false,
            })
        }
    }
    impl MemoryIntegritySource for Healthy {
        fn state(&self) -> Reading<MemoryIntegrity> {
            Ok(MemoryIntegrity::Off)
        }
    }
    impl ThrottleSource for Healthy {
        fn sample(&self, count: usize, _interval: Duration) -> Reading<Vec<ThrottleSample>> {
            Ok(vec![
                ThrottleSample {
                    performance_limit: Some(100.0),
                    max_frequency: Some(100.0),
                };
                count
            ])
        }
    }
    impl RestartSource for Healthy {
        fn windows_update_reboot_required(&self) -> Reading<bool> {
            Ok(false)
        }
        fn servicing_reboot_pending(&self) -> Reading<bool> {
            Ok(false)
        }
    }
    impl GpuDriverSource for Healthy {
        fn drivers(&self) -> Reading<Vec<GpuDriver>> {
            Ok(vec![GpuDriver {
                name: "NVIDIA GeForce RTX 3080".into(),
                provider: "NVIDIA".into(),
                date: date::Date::new(2026, 8, 20),
            }])
        }
    }

    #[test]
    fn run_all_returns_one_result_per_check_in_order() {
        let ticks = Cell::new(1_000u64);
        let clock = || {
            ticks.set(ticks.get() + 1);
            ticks.get()
        };
        // 2026-11-16, so the 2026-08-20 driver is fresh.
        let fixed = date::days_from_civil(2026, 11, 16) as u64 * 86_400;
        let clock_after = || fixed + clock();
        let results = run_all(&Healthy, &clock_after);
        let ids: Vec<_> = results.iter().map(|r| r.check).collect();
        assert_eq!(ids, CheckId::ALL.to_vec());
        for r in &results {
            assert_eq!(r.outcome, Outcome::Ok, "{:?}", r.check);
            assert!(r.measured_at >= fixed);
            assert!(r.measured_at_iso().starts_with("2026-11-16T"));
        }
        // Each result carries its own, later, timestamp.
        assert!(results
            .windows(2)
            .all(|w| w[0].measured_at < w[1].measured_at));
    }
}
