//! Check 6: how old is the graphics driver?

use crate::date::Date;
use crate::result::{outcome_for_error, Finding, Outcome, Reading, Severity};

/// A display driver older than this many days is reported as old.
///
/// Why 365: NVIDIA, AMD and Intel ship a new driver roughly every month, with fixes for new
/// games, so a few months behind is normal and not worth a card. A year is when a driver
/// misses a whole year of game fixes and security updates. Windows Update also installs
/// older, vendor-approved drivers, so a tighter limit would flag healthy PCs.
pub const GPU_DRIVER_OLD_AFTER_DAYS: i64 = 365;

/// Past this many days the finding is Medium instead of Low.
pub const GPU_DRIVER_VERY_OLD_AFTER_DAYS: i64 = 2 * GPU_DRIVER_OLD_AFTER_DAYS;

/// A driver date this far in the future is a clock or registry problem, not a fresh driver.
const FUTURE_TOLERANCE_DAYS: i64 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuDriver {
    /// E.g. `NVIDIA GeForce RTX 3080`.
    pub name: String,
    /// The driver vendor, e.g. `NVIDIA`.
    pub provider: String,
    /// `None` when the registry holds no readable date.
    pub date: Option<Date>,
}

pub trait GpuDriverSource {
    fn drivers(&self) -> Reading<Vec<GpuDriver>>;
}

/// Names of display adapters that are not a graphics card (remote sessions, virtual machines).
const VIRTUAL_ADAPTER_MARKERS: &[&str] = &[
    "remote display",
    "indirect display",
    "hyper-v",
    "virtual",
    "vmware",
    "virtualbox",
    "parsec",
];

fn is_virtual(name: &str) -> bool {
    let n = name.to_lowercase();
    VIRTUAL_ADAPTER_MARKERS.iter().any(|m| n.contains(m))
}

fn is_basic_display(name: &str) -> bool {
    name.to_lowercase().contains("basic display")
}

fn update_action(provider: &str) -> &'static str {
    let p = provider.to_lowercase();
    if p.contains("nvidia") {
        "Install the latest driver with the NVIDIA app or from nvidia.com/drivers."
    } else if p.contains("intel") {
        "Install the latest driver with the Intel Driver and Support Assistant or from intel.com/support."
    } else if p.contains("amd") || p.contains("advanced micro") || p.contains("ati tech") {
        "Install the latest driver with AMD Software or from amd.com/support."
    } else {
        "Install the latest driver from the graphics card maker's website, or from your PC maker's support page on a laptop."
    }
}

pub fn check_gpu_driver_age(source: &dyn GpuDriverSource, now_unix: u64) -> Outcome {
    let drivers = match source.drivers() {
        Ok(d) => d,
        Err(e) => return outcome_for_error(&e, "the display driver"),
    };
    let today = (now_unix / 86_400) as i64;

    let real: Vec<&GpuDriver> = drivers.iter().filter(|d| !is_virtual(&d.name)).collect();
    if real.is_empty() {
        return Outcome::unavailable("No display adapter driver was found on this PC.");
    }
    let (basic, cards): (Vec<&GpuDriver>, Vec<&GpuDriver>) =
        real.into_iter().partition(|d| is_basic_display(&d.name));
    if cards.is_empty() {
        return Outcome::Finding(Finding::new(
            Severity::Medium,
            "Windows is using the basic Microsoft display driver, not a driver for your graphics card. Games and video will run far slower than they should.",
            "Install the driver from the graphics card maker's website, or from your PC maker's support page on a laptop.",
        ));
    }
    let _ = basic; // a basic adapter beside a real card is a leftover and says nothing

    let mut unread = 0usize;
    let mut oldest: Option<(&GpuDriver, i64)> = None;
    for d in &cards {
        match d.date.map(|date| today - date.days_since_epoch()) {
            Some(age) if age >= -FUTURE_TOLERANCE_DAYS => {
                if oldest.is_none_or(|(_, a)| age > a) {
                    oldest = Some((d, age));
                }
            }
            _ => unread += 1,
        }
    }

    if let Some((driver, age)) = oldest {
        if age > GPU_DRIVER_OLD_AFTER_DAYS {
            let severity = if age > GPU_DRIVER_VERY_OLD_AFTER_DAYS {
                Severity::Medium
            } else {
                Severity::Low
            };
            let date = driver.date.expect("an aged driver has a date");
            let observation = format!(
                "The graphics driver for {} is dated {:04}-{:02}-{:02}, about {} months old. Newer drivers carry game fixes and security updates.",
                driver.name,
                date.year,
                date.month,
                date.day,
                age / 30
            );
            return Outcome::Finding(Finding::new(
                severity,
                &observation,
                update_action(&driver.provider),
            ));
        }
    }
    if unread > 0 {
        return Outcome::not_checked(&format!(
            "The registry holds no usable driver date for {unread} of {} graphics adapters.",
            cards.len()
        ));
    }
    Outcome::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::date::days_from_civil;
    use crate::result::ReadError;

    struct Fake(Reading<Vec<GpuDriver>>);
    impl GpuDriverSource for Fake {
        fn drivers(&self) -> Reading<Vec<GpuDriver>> {
            self.0.clone()
        }
    }

    /// 2026-11-16 at noon UTC.
    fn now() -> u64 {
        days_from_civil(2026, 11, 16) as u64 * 86_400 + 43_200
    }

    fn gpu(name: &str, provider: &str, date: Option<(i32, u32, u32)>) -> GpuDriver {
        GpuDriver {
            name: name.into(),
            provider: provider.into(),
            date: date.map(|(y, m, d)| Date::new(y, m, d).unwrap()),
        }
    }

    fn run(drivers: Vec<GpuDriver>) -> Outcome {
        check_gpu_driver_age(&Fake(Ok(drivers)), now())
    }

    #[test]
    fn fresh_driver_is_ok() {
        assert_eq!(
            run(vec![gpu(
                "NVIDIA GeForce RTX 3080",
                "NVIDIA",
                Some((2026, 8, 20))
            )]),
            Outcome::Ok
        );
    }

    #[test]
    fn exactly_at_the_limit_is_ok_one_day_over_is_not() {
        // 2025-11-16 is exactly 365 days before 2026-11-16.
        assert_eq!(
            run(vec![gpu("GPU", "NVIDIA", Some((2025, 11, 16)))]),
            Outcome::Ok
        );
        assert!(matches!(
            run(vec![gpu("GPU", "NVIDIA", Some((2025, 11, 15)))]),
            Outcome::Finding(_)
        ));
    }

    #[test]
    fn old_driver_is_a_low_finding_with_a_vendor_action() {
        let Outcome::Finding(f) = run(vec![gpu(
            "NVIDIA GeForce RTX 3080",
            "NVIDIA",
            Some((2025, 3, 1)),
        )]) else {
            panic!("expected a finding");
        };
        assert_eq!(f.severity, Severity::Low);
        assert!(f.observation.contains("RTX 3080"));
        assert!(f.observation.contains("2025-03-01"));
        assert!(f.action.contains("NVIDIA"));
    }

    #[test]
    fn very_old_driver_is_medium() {
        let Outcome::Finding(f) = run(vec![gpu(
            "Radeon RX 580",
            "Advanced Micro Devices",
            Some((2022, 1, 1)),
        )]) else {
            panic!("expected a finding");
        };
        assert_eq!(f.severity, Severity::Medium);
        assert!(f.action.contains("AMD"));
    }

    #[test]
    fn unknown_vendor_gets_a_generic_action() {
        let Outcome::Finding(f) = run(vec![gpu("Mystery GPU", "Acme", Some((2020, 1, 1)))]) else {
            panic!("expected a finding");
        };
        assert!(f.action.contains("maker"));
        let Outcome::Finding(f) = run(vec![gpu(
            "Arc A770",
            "Intel Corporation",
            Some((2020, 1, 1)),
        )]) else {
            panic!("expected a finding");
        };
        assert!(f.action.contains("Intel"));
    }

    #[test]
    fn the_oldest_of_two_cards_is_reported() {
        let Outcome::Finding(f) = run(vec![
            gpu("Intel UHD Graphics", "Intel", Some((2023, 1, 1))),
            gpu("NVIDIA RTX", "NVIDIA", Some((2026, 8, 20))),
        ]) else {
            panic!("expected a finding");
        };
        assert!(f.observation.contains("Intel UHD"));
    }

    #[test]
    fn an_old_card_wins_over_an_unreadable_date() {
        let out = run(vec![
            gpu("Old", "Intel", Some((2020, 1, 1))),
            gpu("Other", "NVIDIA", None),
        ]);
        assert!(matches!(out, Outcome::Finding(_)));
    }

    #[test]
    fn missing_date_is_not_checked_never_ok() {
        let out = run(vec![gpu("NVIDIA RTX", "NVIDIA", None)]);
        assert!(matches!(out, Outcome::NotChecked { .. }));
    }

    #[test]
    fn a_fresh_card_beside_an_unreadable_one_is_not_ok() {
        let out = run(vec![
            gpu("A", "NVIDIA", Some((2026, 8, 20))),
            gpu("B", "Intel", None),
        ]);
        assert!(matches!(out, Outcome::NotChecked { .. }));
    }

    #[test]
    fn a_date_in_the_future_is_not_trusted() {
        let out = run(vec![gpu("GPU", "NVIDIA", Some((2027, 6, 1)))]);
        assert!(matches!(out, Outcome::NotChecked { .. }));
    }

    #[test]
    fn basic_display_driver_alone_is_a_finding() {
        let Outcome::Finding(f) = run(vec![gpu(
            "Microsoft Basic Display Adapter",
            "Microsoft",
            Some((2006, 6, 21)),
        )]) else {
            panic!("expected a finding");
        };
        assert!(f.observation.contains("basic Microsoft display driver"));
    }

    #[test]
    fn basic_display_beside_a_real_card_is_ignored() {
        let out = run(vec![
            gpu(
                "Microsoft Basic Display Adapter",
                "Microsoft",
                Some((2006, 6, 21)),
            ),
            gpu("NVIDIA RTX", "NVIDIA", Some((2026, 8, 20))),
        ]);
        assert_eq!(out, Outcome::Ok);
    }

    #[test]
    fn virtual_adapters_are_ignored_and_alone_mean_unavailable() {
        let out = run(vec![
            gpu("Microsoft Hyper-V Video", "Microsoft", Some((2006, 6, 21))),
            gpu("NVIDIA RTX", "NVIDIA", Some((2026, 8, 20))),
        ]);
        assert_eq!(out, Outcome::Ok);
        let out = run(vec![gpu(
            "Microsoft Remote Display Adapter",
            "Microsoft",
            None,
        )]);
        assert!(matches!(out, Outcome::Unavailable { .. }));
    }

    #[test]
    fn no_adapters_is_unavailable() {
        assert!(matches!(run(vec![]), Outcome::Unavailable { .. }));
    }

    #[test]
    fn read_errors_are_never_ok() {
        for err in [
            ReadError::AccessDenied,
            ReadError::NotPresent("class key".into()),
            ReadError::Failed("x".into()),
        ] {
            assert!(matches!(
                check_gpu_driver_age(&Fake(Err(err)), now()),
                Outcome::NotChecked { .. }
            ));
        }
        assert!(matches!(
            check_gpu_driver_age(&Fake(Err(ReadError::Unsupported("no".into()))), now()),
            Outcome::Unavailable { .. }
        ));
    }
}
