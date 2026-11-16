//! Check 3: is Memory integrity (hypervisor-protected code integrity, HVCI) turned on?
//!
//! This check reports the state and the honest trade-off. It never suggests turning it off:
//! it is a security protection, and the cost is small on recent hardware.

use crate::result::{outcome_for_error, Finding, Outcome, Reading, Severity};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryIntegrity {
    On,
    Off,
}

pub trait MemoryIntegritySource {
    /// The setting as Windows stores it. A change made since the last restart is not running yet.
    fn state(&self) -> Reading<MemoryIntegrity>;
}

pub fn check_memory_integrity(source: &dyn MemoryIntegritySource) -> Outcome {
    match source.state() {
        Err(e) => outcome_for_error(&e, "the Memory integrity setting"),
        // Off is not a performance problem, so there is nothing to report.
        Ok(MemoryIntegrity::Off) => Outcome::Ok,
        Ok(MemoryIntegrity::On) => Outcome::Finding(Finding::new(
            Severity::Info,
            "Memory integrity is on. It protects Windows from malicious drivers, and in some games, mostly on older processors, it can cost a few percent of speed.",
            "No change needed. Keep it on unless a driver or game you rely on needs it off; it is under Windows Security, Device security, Core isolation.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ReadError;

    struct Fake(Reading<MemoryIntegrity>);
    impl MemoryIntegritySource for Fake {
        fn state(&self) -> Reading<MemoryIntegrity> {
            self.0.clone()
        }
    }

    #[test]
    fn off_is_ok() {
        assert_eq!(
            check_memory_integrity(&Fake(Ok(MemoryIntegrity::Off))),
            Outcome::Ok
        );
    }

    #[test]
    fn on_is_an_info_finding_with_both_sides_and_no_advice_to_turn_it_off() {
        let Outcome::Finding(f) = check_memory_integrity(&Fake(Ok(MemoryIntegrity::On))) else {
            panic!("expected a finding");
        };
        assert_eq!(f.severity, Severity::Info);
        assert!(f.observation.contains("protects"));
        assert!(f.observation.contains("cost"));
        assert!(f.action.starts_with("No change needed"));
        assert!(!f.action.to_lowercase().contains("turn it off"));
        assert!(!f.action.to_lowercase().contains("disable"));
    }

    #[test]
    fn unreadable_setting_is_not_checked_never_ok() {
        for err in [
            ReadError::AccessDenied,
            ReadError::NotPresent("no saved setting".into()),
            ReadError::Failed("x".into()),
        ] {
            assert!(matches!(
                check_memory_integrity(&Fake(Err(err))),
                Outcome::NotChecked { .. }
            ));
        }
    }

    #[test]
    fn unsupported_windows_is_unavailable() {
        assert!(matches!(
            check_memory_integrity(&Fake(Err(ReadError::Unsupported("old Windows".into())))),
            Outcome::Unavailable { .. }
        ));
    }
}
