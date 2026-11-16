//! Check 5: is Windows waiting for a restart to finish installing updates?

use crate::result::{outcome_for_error, Finding, Outcome, Reading, Severity};

/// The two standard markers. A missing key means "no restart needed", so the real source
/// returns `Ok(false)` for it; only a key that exists but cannot be read is an error.
pub trait RestartSource {
    /// `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired`
    fn windows_update_reboot_required(&self) -> Reading<bool>;
    /// `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending`
    fn servicing_reboot_pending(&self) -> Reading<bool>;
}

pub fn check_pending_restart(source: &dyn RestartSource) -> Outcome {
    let update = source.windows_update_reboot_required();
    let servicing = source.servicing_reboot_pending();

    if update == Ok(true) || servicing == Ok(true) {
        return Outcome::Finding(Finding::new(
            Severity::Low,
            "Windows is waiting for a restart to finish installing updates. Until then the update is not fully applied and the PC can run slower or behave oddly.",
            "Save your work and restart the PC.",
        ));
    }
    // Both markers must be read to say "no restart pending".
    if let Err(e) = &update {
        return outcome_for_error(e, "the Windows Update restart marker");
    }
    if let Err(e) = &servicing {
        return outcome_for_error(e, "the servicing restart marker");
    }
    Outcome::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ReadError;

    struct Fake {
        update: Reading<bool>,
        servicing: Reading<bool>,
    }
    impl RestartSource for Fake {
        fn windows_update_reboot_required(&self) -> Reading<bool> {
            self.update.clone()
        }
        fn servicing_reboot_pending(&self) -> Reading<bool> {
            self.servicing.clone()
        }
    }

    fn run(update: Reading<bool>, servicing: Reading<bool>) -> Outcome {
        check_pending_restart(&Fake { update, servicing })
    }

    #[test]
    fn neither_marker_is_ok() {
        assert_eq!(run(Ok(false), Ok(false)), Outcome::Ok);
    }

    #[test]
    fn either_marker_is_a_finding() {
        assert!(matches!(run(Ok(true), Ok(false)), Outcome::Finding(_)));
        assert!(matches!(run(Ok(false), Ok(true)), Outcome::Finding(_)));
        let Outcome::Finding(f) = run(Ok(true), Ok(true)) else {
            panic!("expected a finding");
        };
        assert_eq!(f.action, "Save your work and restart the PC.");
    }

    #[test]
    fn a_marker_that_is_set_wins_over_an_unreadable_one() {
        assert!(matches!(
            run(Ok(true), Err(ReadError::AccessDenied)),
            Outcome::Finding(_)
        ));
        assert!(matches!(
            run(Err(ReadError::Failed("x".into())), Ok(true)),
            Outcome::Finding(_)
        ));
    }

    #[test]
    fn one_unreadable_marker_means_not_checked_never_ok() {
        assert!(matches!(
            run(Ok(false), Err(ReadError::AccessDenied)),
            Outcome::NotChecked { .. }
        ));
        assert!(matches!(
            run(Err(ReadError::Failed("x".into())), Ok(false)),
            Outcome::NotChecked { .. }
        ));
        assert!(matches!(
            run(Err(ReadError::AccessDenied), Err(ReadError::AccessDenied)),
            Outcome::NotChecked { .. }
        ));
    }

    #[test]
    fn unsupported_is_unavailable() {
        assert!(matches!(
            run(Err(ReadError::Unsupported("no".into())), Ok(false)),
            Outcome::Unavailable { .. }
        ));
    }
}
