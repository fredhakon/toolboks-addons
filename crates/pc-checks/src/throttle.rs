//! Check 4: is the processor being held back? Uses the built-in performance counters
//! `% Performance Limit` (100 means nothing is holding the CPU back) and `% of Maximum Frequency`.

use std::time::Duration;

use crate::result::{outcome_for_error, Finding, Outcome, Reading, Severity};

/// How many readings are taken, and how far apart. Together they are the sample window:
/// 6 readings, 500 ms apart, so the check takes about 3 seconds.
pub const SAMPLE_COUNT: usize = 6;
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// The average `% Performance Limit` below which the CPU counts as throttled. 100 is
/// "not limited"; Windows dips a little below 100 for brief boosts and idle states, so a
/// clear margin avoids crying wolf.
pub const THROTTLED_BELOW_PERCENT: f64 = 90.0;
/// Below this average the CPU is held to half its allowed speed or less: a serious case.
pub const SEVERELY_THROTTLED_BELOW_PERCENT: f64 = 50.0;

/// One reading of the two counters. `None` means that counter gave no usable value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThrottleSample {
    pub performance_limit: Option<f64>,
    pub max_frequency: Option<f64>,
}

pub trait ThrottleSource {
    /// Take `count` readings `interval` apart (the real source also waits that long).
    /// `Err(NotPresent)` when the counters do not exist on this PC.
    fn sample(&self, count: usize, interval: Duration) -> Reading<Vec<ThrottleSample>>;
}

fn average(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = values
        .filter(|v| v.is_finite() && *v >= 0.0)
        .fold((0.0, 0u32), |(s, n), v| (s + v, n + 1));
    (n > 0).then(|| sum / f64::from(n))
}

pub fn check_throttling(source: &dyn ThrottleSource) -> Outcome {
    let samples = match source.sample(SAMPLE_COUNT, SAMPLE_INTERVAL) {
        Ok(s) => s,
        Err(e) => return outcome_for_error(&e, "the processor performance counters"),
    };
    let window_secs = (SAMPLE_INTERVAL * SAMPLE_COUNT as u32).as_secs_f64();

    let Some(limit) = average(samples.iter().filter_map(|s| s.performance_limit)) else {
        return Outcome::not_checked(
            "The % Performance Limit counter returned no usable value during the sample.",
        );
    };
    if limit >= THROTTLED_BELOW_PERCENT {
        return Outcome::Ok;
    }

    let freq = average(samples.iter().filter_map(|s| s.max_frequency));
    let mut observation = format!(
        "The processor was held to {limit:.0}% of its allowed performance during a {window_secs:.0} second sample"
    );
    if let Some(freq) = freq {
        observation.push_str(&format!(" and ran at {freq:.0}% of its maximum frequency"));
    }
    observation.push_str(". Heat or a power limit is the usual cause.");
    let severity = if limit < SEVERELY_THROTTLED_BELOW_PERCENT {
        Severity::High
    } else {
        Severity::Medium
    };
    Outcome::Finding(Finding::new(
        severity,
        &observation,
        "Check that the fans spin and the vents are not blocked or dusty, and on a laptop that the charger is connected, then run this check again while the PC is busy.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ReadError;
    use std::cell::Cell;

    struct Fake {
        result: Reading<Vec<ThrottleSample>>,
        asked: Cell<Option<(usize, Duration)>>,
    }
    impl Fake {
        fn new(result: Reading<Vec<ThrottleSample>>) -> Self {
            Fake {
                result,
                asked: Cell::new(None),
            }
        }
    }
    impl ThrottleSource for Fake {
        fn sample(&self, count: usize, interval: Duration) -> Reading<Vec<ThrottleSample>> {
            self.asked.set(Some((count, interval)));
            self.result.clone()
        }
    }

    fn s(limit: Option<f64>, freq: Option<f64>) -> ThrottleSample {
        ThrottleSample {
            performance_limit: limit,
            max_frequency: freq,
        }
    }

    fn run(samples: Vec<ThrottleSample>) -> Outcome {
        check_throttling(&Fake::new(Ok(samples)))
    }

    #[test]
    fn asks_for_the_documented_window() {
        let fake = Fake::new(Ok(vec![s(Some(100.0), Some(100.0))]));
        check_throttling(&fake);
        assert_eq!(fake.asked.get(), Some((SAMPLE_COUNT, SAMPLE_INTERVAL)));
        assert_eq!(
            SAMPLE_INTERVAL * SAMPLE_COUNT as u32,
            Duration::from_secs(3)
        );
    }

    #[test]
    fn unlimited_cpu_is_ok_even_at_low_idle_frequency() {
        assert_eq!(run(vec![s(Some(100.0), Some(28.0)); 6]), Outcome::Ok);
    }

    #[test]
    fn small_dips_are_ok() {
        assert_eq!(run(vec![s(Some(92.0), Some(80.0)); 4]), Outcome::Ok);
    }

    #[test]
    fn throttled_cpu_is_a_medium_finding() {
        let Outcome::Finding(f) = run(vec![s(Some(70.0), Some(55.0)); 6]) else {
            panic!("expected a finding");
        };
        assert_eq!(f.severity, Severity::Medium);
        assert!(f.observation.contains("70%"));
        assert!(f.observation.contains("55%"));
        assert!(f.observation.contains("3 second"));
    }

    #[test]
    fn badly_throttled_cpu_is_high() {
        let Outcome::Finding(f) = run(vec![s(Some(30.0), None); 3]) else {
            panic!("expected a finding");
        };
        assert_eq!(f.severity, Severity::High);
        assert!(!f.observation.contains("maximum frequency"));
    }

    #[test]
    fn the_average_decides_not_one_reading() {
        // One bad reading among good ones averages to 96.7: fine.
        let mut v = vec![s(Some(100.0), None); 5];
        v.push(s(Some(80.0), None));
        assert_eq!(run(v), Outcome::Ok);
        // Mostly bad readings average below the line.
        let mut v = vec![s(Some(60.0), None); 5];
        v.push(s(Some(100.0), None));
        assert!(matches!(run(v), Outcome::Finding(_)));
    }

    #[test]
    fn readings_without_a_value_are_skipped_in_the_average() {
        let v = vec![s(None, None), s(Some(40.0), None), s(Some(60.0), None)];
        let Outcome::Finding(f) = run(v) else {
            panic!("expected a finding");
        };
        assert!(f.observation.contains("50%"));
    }

    #[test]
    fn missing_limit_counter_is_not_checked_even_with_a_frequency() {
        let out = run(vec![s(None, Some(100.0)); 6]);
        let Outcome::NotChecked { reason } = out else {
            panic!("expected NotChecked");
        };
        assert!(reason.contains("Performance Limit"));
    }

    #[test]
    fn no_samples_is_not_checked() {
        assert!(matches!(run(vec![]), Outcome::NotChecked { .. }));
    }

    #[test]
    fn nonsense_values_are_ignored_not_trusted() {
        let v = vec![s(Some(f64::NAN), None), s(Some(-5.0), None)];
        assert!(matches!(run(v), Outcome::NotChecked { .. }));
    }

    #[test]
    fn read_errors_are_not_checked_or_unavailable() {
        for err in [
            ReadError::AccessDenied,
            ReadError::NotPresent("counter missing".into()),
            ReadError::Failed("pdh".into()),
        ] {
            assert!(matches!(
                check_throttling(&Fake::new(Err(err))),
                Outcome::NotChecked { .. }
            ));
        }
        assert!(matches!(
            check_throttling(&Fake::new(Err(ReadError::Unsupported("no".into())))),
            Outcome::Unavailable { .. }
        ));
    }
}
