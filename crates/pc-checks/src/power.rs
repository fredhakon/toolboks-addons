//! Check 2: power plan, power mode (the slider in Settings) and battery saver.

use crate::result::{outcome_for_error, Finding, Outcome, ReadError, Reading, Severity};

/// The built-in power plans. Anything else (an OEM or custom plan) is `Other` and is never
/// judged: we cannot know what it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanKind {
    PowerSaver,
    Balanced,
    HighPerformance,
    UltimatePerformance,
    Other,
}

/// The Power mode slider (a plan "overlay" since Windows 10 1709).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerMode {
    /// "Best power efficiency" (Windows 11) / "Better battery" (Windows 10).
    BestEfficiency,
    /// "Balanced" / "Recommended": no overlay.
    Balanced,
    BetterPerformance,
    BestPerformance,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerStatus {
    /// `Some(false)` means the PC runs on its battery. `None` means Windows could not say;
    /// that is treated like plugged in, because a desktop reports it this way.
    pub on_ac: Option<bool>,
    /// Battery saver is on right now.
    pub battery_saver: bool,
}

pub trait PowerSource {
    fn active_plan(&self) -> Reading<PlanKind>;
    /// `Err(Unsupported)` on a Windows without the Power mode slider.
    fn power_mode(&self) -> Reading<PowerMode>;
    fn status(&self) -> Reading<PowerStatus>;
}

// Well-known GUIDs, as u128 (the same digits as the text form).
const GUID_POWER_SAVER: u128 = 0xa1841308_3541_4fab_bc81_f71556f20b4a;
const GUID_BALANCED: u128 = 0x381b4222_f694_41f0_9685_ff5bb260df2e;
const GUID_HIGH_PERFORMANCE: u128 = 0x8c5e7fda_e8bf_4a96_9a85_a6e23a8c635c;
const GUID_ULTIMATE: u128 = 0xe9a42b02_d5df_448d_aa00_03f14749eb61;

const OVERLAY_NONE: u128 = 0;
const OVERLAY_BEST_EFFICIENCY: u128 = 0x961cc777_2547_4f9d_8174_7d86181b8a7a;
const OVERLAY_BETTER_PERFORMANCE: u128 = 0x3af9b8d9_7c97_431d_ad78_34a8bfea439f;
const OVERLAY_BEST_PERFORMANCE: u128 = 0xded574b5_45a0_4f42_8737_46345c09c238;

pub fn plan_kind_from_guid(guid: u128) -> PlanKind {
    match guid {
        GUID_POWER_SAVER => PlanKind::PowerSaver,
        GUID_BALANCED => PlanKind::Balanced,
        GUID_HIGH_PERFORMANCE => PlanKind::HighPerformance,
        GUID_ULTIMATE => PlanKind::UltimatePerformance,
        _ => PlanKind::Other,
    }
}

pub fn power_mode_from_guid(guid: u128) -> PowerMode {
    match guid {
        OVERLAY_NONE => PowerMode::Balanced,
        OVERLAY_BEST_EFFICIENCY => PowerMode::BestEfficiency,
        OVERLAY_BETTER_PERFORMANCE => PowerMode::BetterPerformance,
        OVERLAY_BEST_PERFORMANCE => PowerMode::BestPerformance,
        _ => PowerMode::Other,
    }
}

pub fn check_power(source: &dyn PowerSource) -> Outcome {
    let plan = source.active_plan();
    let mode = source.power_mode();
    let status = source.status();

    // What is plugged in decides whether a saving setting is a problem: on battery it is the
    // user's own choice. Unknown counts as plugged in.
    let on_battery = matches!(
        status,
        Ok(PowerStatus {
            on_ac: Some(false),
            ..
        })
    );

    let mut candidates: Vec<Finding> = Vec::new();
    if !on_battery && plan == Ok(PlanKind::PowerSaver) {
        candidates.push(Finding::new(
            Severity::Medium,
            "The Power saver plan is active while this PC is plugged in. It caps how fast the processor runs.",
            "Open Settings, System, Power & battery and set Power mode to Balanced or Best performance (on a desktop: Control Panel, Power Options, Balanced).",
        ));
    }
    if !on_battery && mode == Ok(PowerMode::BestEfficiency) {
        candidates.push(Finding::new(
            Severity::Medium,
            "Power mode is set to Best power efficiency while this PC is plugged in. Windows then favours low power over speed.",
            "Open Settings, System, Power & battery and set Power mode to Balanced or Best performance.",
        ));
    }
    if matches!(
        status,
        Ok(PowerStatus {
            battery_saver: true,
            ..
        })
    ) {
        candidates.push(Finding::new(
            Severity::Low,
            "Battery saver is on. It lowers processor speed and pauses background work.",
            "Plug in the charger, or turn Battery saver off in Settings, System, Power & battery.",
        ));
    }
    // Several findings: report the most severe one (the first of equals), so there is one action.
    if let Some(best) = candidates
        .iter()
        .enumerate()
        .max_by_key(|(i, f)| (f.severity, std::cmp::Reverse(*i)))
        .map(|(_, f)| f.clone())
    {
        return Outcome::Finding(best);
    }

    // Nothing wrong was seen, but "Ok" needs every reading. A Windows without the Power mode
    // slider is fine; any other failure to read is not.
    if let Err(e) = &plan {
        return outcome_for_error(e, "the active power plan");
    }
    if let Err(e) = &status {
        return outcome_for_error(e, "the power status");
    }
    if let Err(e) = &mode {
        if !matches!(e, ReadError::Unsupported(_)) {
            return outcome_for_error(e, "the power mode");
        }
    }
    Outcome::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        plan: Reading<PlanKind>,
        mode: Reading<PowerMode>,
        status: Reading<PowerStatus>,
    }
    impl PowerSource for Fake {
        fn active_plan(&self) -> Reading<PlanKind> {
            self.plan.clone()
        }
        fn power_mode(&self) -> Reading<PowerMode> {
            self.mode.clone()
        }
        fn status(&self) -> Reading<PowerStatus> {
            self.status.clone()
        }
    }

    fn plugged() -> Reading<PowerStatus> {
        Ok(PowerStatus {
            on_ac: Some(true),
            battery_saver: false,
        })
    }
    fn on_battery(saver: bool) -> Reading<PowerStatus> {
        Ok(PowerStatus {
            on_ac: Some(false),
            battery_saver: saver,
        })
    }
    fn fake(
        plan: Reading<PlanKind>,
        mode: Reading<PowerMode>,
        status: Reading<PowerStatus>,
    ) -> Outcome {
        check_power(&Fake { plan, mode, status })
    }

    #[test]
    fn guid_tables() {
        assert_eq!(
            plan_kind_from_guid(0x381b4222_f694_41f0_9685_ff5bb260df2e),
            PlanKind::Balanced
        );
        assert_eq!(plan_kind_from_guid(1), PlanKind::Other);
        assert_eq!(power_mode_from_guid(0), PowerMode::Balanced);
        assert_eq!(
            power_mode_from_guid(0x961cc777_2547_4f9d_8174_7d86181b8a7a),
            PowerMode::BestEfficiency
        );
        assert_eq!(power_mode_from_guid(7), PowerMode::Other);
    }

    #[test]
    fn balanced_everything_is_ok() {
        assert_eq!(
            fake(Ok(PlanKind::Balanced), Ok(PowerMode::Balanced), plugged()),
            Outcome::Ok
        );
        assert_eq!(
            fake(
                Ok(PlanKind::UltimatePerformance),
                Ok(PowerMode::BestPerformance),
                plugged()
            ),
            Outcome::Ok
        );
    }

    #[test]
    fn custom_plan_is_not_judged() {
        assert_eq!(
            fake(Ok(PlanKind::Other), Ok(PowerMode::Other), plugged()),
            Outcome::Ok
        );
    }

    #[test]
    fn power_saver_on_ac_is_a_finding() {
        let Outcome::Finding(f) =
            fake(Ok(PlanKind::PowerSaver), Ok(PowerMode::Balanced), plugged())
        else {
            panic!("expected a finding");
        };
        assert_eq!(f.severity, Severity::Medium);
        assert!(f.observation.contains("Power saver"));
    }

    #[test]
    fn power_saver_with_unknown_ac_state_is_still_a_finding() {
        let status = Ok(PowerStatus {
            on_ac: None,
            battery_saver: false,
        });
        assert!(matches!(
            fake(Ok(PlanKind::PowerSaver), Ok(PowerMode::Balanced), status),
            Outcome::Finding(_)
        ));
    }

    #[test]
    fn power_saver_on_battery_is_the_users_choice() {
        assert_eq!(
            fake(
                Ok(PlanKind::PowerSaver),
                Ok(PowerMode::BestEfficiency),
                on_battery(false)
            ),
            Outcome::Ok
        );
    }

    #[test]
    fn best_efficiency_mode_on_ac_is_a_finding() {
        let Outcome::Finding(f) = fake(
            Ok(PlanKind::Balanced),
            Ok(PowerMode::BestEfficiency),
            plugged(),
        ) else {
            panic!("expected a finding");
        };
        assert!(f.observation.contains("Best power efficiency"));
    }

    #[test]
    fn battery_saver_is_a_low_finding_even_on_battery() {
        let Outcome::Finding(f) = fake(
            Ok(PlanKind::Balanced),
            Ok(PowerMode::Balanced),
            on_battery(true),
        ) else {
            panic!("expected a finding");
        };
        assert_eq!(f.severity, Severity::Low);
        assert!(f.observation.contains("Battery saver"));
    }

    #[test]
    fn the_most_severe_finding_wins_and_there_is_one_action() {
        let status = Ok(PowerStatus {
            on_ac: Some(true),
            battery_saver: true,
        });
        let Outcome::Finding(f) = fake(
            Ok(PlanKind::PowerSaver),
            Ok(PowerMode::BestEfficiency),
            status,
        ) else {
            panic!("expected a finding");
        };
        assert_eq!(f.severity, Severity::Medium);
        assert!(f.observation.contains("Power saver plan"));
    }

    #[test]
    fn a_finding_is_kept_when_another_reading_failed() {
        let out = fake(
            Ok(PlanKind::PowerSaver),
            Err(ReadError::Failed("x".into())),
            plugged(),
        );
        assert!(matches!(out, Outcome::Finding(_)));
    }

    #[test]
    fn missing_power_mode_slider_is_fine() {
        let out = fake(
            Ok(PlanKind::Balanced),
            Err(ReadError::Unsupported("no slider".into())),
            plugged(),
        );
        assert_eq!(out, Outcome::Ok);
    }

    #[test]
    fn unreadable_plan_is_not_checked() {
        for err in [ReadError::AccessDenied, ReadError::Failed("x".into())] {
            let out = fake(Err(err), Ok(PowerMode::Balanced), plugged());
            assert!(matches!(out, Outcome::NotChecked { .. }));
        }
    }

    #[test]
    fn unreadable_status_is_not_checked() {
        let out = fake(
            Ok(PlanKind::Balanced),
            Ok(PowerMode::Balanced),
            Err(ReadError::Failed("x".into())),
        );
        assert!(matches!(out, Outcome::NotChecked { .. }));
    }

    #[test]
    fn unreadable_power_mode_is_not_checked() {
        let out = fake(
            Ok(PlanKind::Balanced),
            Err(ReadError::NotPresent("export missing".into())),
            plugged(),
        );
        assert!(matches!(out, Outcome::NotChecked { .. }));
    }
}
