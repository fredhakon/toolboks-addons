//! Check 1: is the memory running slower than it is rated for (XMP or EXPO not switched on)?

use crate::result::{outcome_for_error, Finding, Outcome, Reading, Severity};

/// How far below the rated speed a module may run before it counts as slow, in percent.
/// Some BIOSes round (2666 reported as 2667 or 2664), so exact equality would raise false
/// alarms; a real missing XMP/EXPO profile is a gap of 15% or more (2133 vs 3200 is 33%).
pub const RAM_SPEED_TOLERANCE_PERCENT: u32 = 3;

/// One memory stick as the BIOS describes it. Speeds are in MT/s (what Windows calls
/// `Speed` and `ConfiguredClockSpeed`); `None` or `0` means the BIOS did not say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryModule {
    /// The slot label, e.g. `DIMM1`.
    pub slot: String,
    /// What the module is rated for.
    pub rated_mts: Option<u32>,
    /// What it actually runs at.
    pub configured_mts: Option<u32>,
}

/// Where the module list comes from. The real one reads the SMBIOS memory table.
pub trait MemorySource {
    fn modules(&self) -> Reading<Vec<MemoryModule>>;
}

fn known(speed: Option<u32>) -> Option<u32> {
    speed.filter(|s| *s > 0)
}

pub fn check_ram_speed(source: &dyn MemorySource) -> Outcome {
    let modules = match source.modules() {
        Ok(m) => m,
        Err(e) => return outcome_for_error(&e, "the memory module list"),
    };
    if modules.is_empty() {
        return Outcome::unavailable("The BIOS reported no installed memory modules.");
    }

    let total = modules.len();
    let mut unreadable = 0usize;
    // (slow module count, worst module as (configured, rated))
    let mut slow = 0usize;
    let mut worst: Option<(u32, u32)> = None;
    for m in &modules {
        match (known(m.configured_mts), known(m.rated_mts)) {
            (Some(configured), Some(rated)) => {
                if u64::from(configured) * 100
                    < u64::from(rated) * u64::from(100 - RAM_SPEED_TOLERANCE_PERCENT)
                {
                    slow += 1;
                    let gap = rated - configured;
                    if worst.is_none_or(|(c, r)| gap > r - c) {
                        worst = Some((configured, rated));
                    }
                }
            }
            _ => unreadable += 1,
        }
    }

    if let Some((configured, rated)) = worst {
        let mut observation = format!(
            "Memory runs at {configured} MT/s but is rated for {rated} MT/s; XMP or EXPO looks off."
        );
        if total > 1 {
            observation.push_str(&format!(" This affects {slow} of {total} modules."));
        }
        return Outcome::Finding(Finding::new(
            Severity::Medium,
            &observation,
            "Restart into the BIOS or UEFI setup (often Del or F2 at power-on), turn on the memory profile (XMP on Intel boards, EXPO or DOCP on AMD boards), then save and exit. Check your motherboard manual first; Toolboks never changes BIOS settings.",
        ));
    }

    if unreadable > 0 {
        return Outcome::not_checked(&format!(
            "The BIOS did not report both the rated and the running speed for {unreadable} of {total} memory modules."
        ));
    }
    Outcome::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ReadError;

    struct Fake(Reading<Vec<MemoryModule>>);
    impl MemorySource for Fake {
        fn modules(&self) -> Reading<Vec<MemoryModule>> {
            self.0.clone()
        }
    }

    fn module(slot: &str, rated: Option<u32>, configured: Option<u32>) -> MemoryModule {
        MemoryModule {
            slot: slot.into(),
            rated_mts: rated,
            configured_mts: configured,
        }
    }

    fn run(modules: Vec<MemoryModule>) -> Outcome {
        check_ram_speed(&Fake(Ok(modules)))
    }

    #[test]
    fn matching_speeds_are_ok() {
        let out = run(vec![
            module("DIMM1", Some(3200), Some(3200)),
            module("DIMM2", Some(3200), Some(3200)),
        ]);
        assert_eq!(out, Outcome::Ok);
    }

    #[test]
    fn rounding_noise_is_not_a_finding() {
        assert_eq!(run(vec![module("A", Some(2667), Some(2666))]), Outcome::Ok);
        // Exactly at the tolerance edge: 3200 * 97% = 3104.
        assert_eq!(run(vec![module("A", Some(3200), Some(3104))]), Outcome::Ok);
    }

    #[test]
    fn running_above_rated_is_ok() {
        assert_eq!(run(vec![module("A", Some(2133), Some(3200))]), Outcome::Ok);
    }

    #[test]
    fn below_the_edge_is_a_finding_with_one_action() {
        let Outcome::Finding(f) = run(vec![module("A", Some(3200), Some(3103))]) else {
            panic!("expected a finding");
        };
        assert_eq!(f.severity, Severity::Medium);
        assert!(f.observation.contains("3103 MT/s"));
        assert!(f.observation.contains("3200 MT/s"));
        assert!(f.observation.contains("XMP or EXPO looks off"));
        assert!(f.action.contains("BIOS"));
        assert!(f.action.contains("never changes BIOS"));
    }

    #[test]
    fn classic_missing_profile() {
        let Outcome::Finding(f) = run(vec![
            module("DIMM1", Some(3600), Some(2133)),
            module("DIMM2", Some(3600), Some(2133)),
        ]) else {
            panic!("expected a finding");
        };
        assert!(f
            .observation
            .starts_with("Memory runs at 2133 MT/s but is rated for 3600 MT/s"));
        assert!(f.observation.contains("2 of 2 modules"));
    }

    #[test]
    fn single_module_has_no_count() {
        let Outcome::Finding(f) = run(vec![module("A", Some(3600), Some(2133))]) else {
            panic!("expected a finding");
        };
        assert!(!f.observation.contains(" of "));
    }

    #[test]
    fn the_worst_module_is_reported() {
        let Outcome::Finding(f) = run(vec![
            module("A", Some(3200), Some(3000)),
            module("B", Some(3600), Some(2133)),
            module("C", Some(3200), Some(3200)),
        ]) else {
            panic!("expected a finding");
        };
        assert!(f.observation.contains("2133 MT/s but is rated for 3600"));
        assert!(f.observation.contains("2 of 3 modules"));
    }

    #[test]
    fn a_finding_wins_over_an_unreadable_module() {
        let out = run(vec![
            module("A", Some(3600), Some(2133)),
            module("B", None, None),
        ]);
        assert!(matches!(out, Outcome::Finding(_)));
    }

    #[test]
    fn zero_or_missing_speed_is_not_checked_never_ok() {
        for (rated, configured) in [
            (Some(0), Some(0)),
            (None, None),
            (Some(3200), None),
            (Some(3200), Some(0)),
            (None, Some(3200)),
            (Some(0), Some(3200)),
        ] {
            let out = run(vec![module("A", rated, configured)]);
            assert!(
                matches!(out, Outcome::NotChecked { .. }),
                "{rated:?} {configured:?} -> {out:?}"
            );
        }
    }

    #[test]
    fn good_modules_plus_an_unreadable_one_is_not_ok() {
        let out = run(vec![
            module("A", Some(3200), Some(3200)),
            module("B", Some(0), Some(0)),
        ]);
        let Outcome::NotChecked { reason } = out else {
            panic!("expected NotChecked");
        };
        assert!(reason.contains("1 of 2"));
    }

    #[test]
    fn no_modules_is_unavailable() {
        assert!(matches!(run(vec![]), Outcome::Unavailable { .. }));
    }

    #[test]
    fn read_errors_are_never_ok() {
        for err in [
            ReadError::AccessDenied,
            ReadError::NotPresent("table".into()),
            ReadError::Failed("boom".into()),
        ] {
            assert!(matches!(
                check_ram_speed(&Fake(Err(err))),
                Outcome::NotChecked { .. }
            ));
        }
        assert!(matches!(
            check_ram_speed(&Fake(Err(ReadError::Unsupported("no table".into())))),
            Outcome::Unavailable { .. }
        ));
    }
}
