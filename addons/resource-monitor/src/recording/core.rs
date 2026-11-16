//! Turns readings into records: gaps, clock changes, per-app records, events and rollups.
//! Pure logic with the clocks passed in, so the tests drive it with made-up times.

use std::time::Duration;

use super::apps::{split, Presence, ProcReading};
use super::record::{Marker, MarkerKind, StepRecord, SystemTotals};
use super::store::{Sink, Store};

/// A reading that covers more real time than this is not recorded: it averages over a sleep,
/// a hibernate or a stall, so it becomes a gap (#37: never a flat line or a spike).
pub const GAP_AFTER: Duration = Duration::from_secs(60);
/// The wall clock may drift from the monotonic clock by this much before it counts as changed.
pub const CLOCK_SLACK_MS: i64 = 10_000;

/// One reading from a [`super::Source`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reading {
    pub totals: SystemTotals,
    pub procs: Vec<ProcReading>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Recorded,
    /// The reading covered a gap and was dropped (its app list still gave events).
    Gap,
    /// The monotonic clock did not move; nothing to record.
    Skipped,
}

pub struct Core {
    /// Monotonic time and wall time of the previous reading (or of the start).
    last_mono: Duration,
    last_utc_ms: i64,
    seq: u64,
    presence: Presence,
    sinks: Vec<Box<dyn Sink>>,
}

impl Core {
    /// Starts recording at `mono` / `utc_ms`: the moment the source took its baseline reading.
    pub fn new(
        store: &mut Store,
        mono: Duration,
        utc_ms: i64,
        mut sinks: Vec<Box<dyn Sink>>,
    ) -> Self {
        store.push_marker(
            Marker {
                seq: 0,
                utc_ms,
                kind: MarkerKind::Began,
            },
            &mut sinks,
        );
        Self {
            last_mono: mono,
            last_utc_ms: utc_ms,
            seq: 0,
            presence: Presence::default(),
            sinks,
        }
    }

    pub fn push(
        &mut self,
        store: &mut Store,
        mono: Duration,
        utc_ms: i64,
        reading: Reading,
    ) -> Outcome {
        let mono_ms = mono.saturating_sub(self.last_mono).as_millis() as i64;
        let wall_ms = utc_ms - self.last_utc_ms;
        let (before_utc, before_mono) = (self.last_utc_ms, self.last_mono);
        self.last_mono = mono;
        self.last_utc_ms = utc_ms;
        if mono <= before_mono {
            return Outcome::Skipped;
        }
        self.seq += 1;
        let seq = self.seq;
        let gap_ms = GAP_AFTER.as_millis() as i64;
        // A long real span, or the wall clock leaping far ahead of it (a sleep the monotonic
        // clock did not count, or the clock set forward): a gap, the reading is dropped.
        if mono_ms > gap_ms || wall_ms - mono_ms > gap_ms {
            store.close_buckets(&mut self.sinks);
            store.push_marker(
                Marker {
                    seq,
                    utc_ms,
                    kind: MarkerKind::Gap {
                        since_utc_ms: before_utc,
                    },
                },
                &mut self.sinks,
            );
            let s = split(&reading.procs, &mut store.names);
            for e in self.presence.update(s.present, seq, utc_ms, true) {
                store.push_event(e, &mut self.sinks);
            }
            return Outcome::Gap;
        }
        if (wall_ms - mono_ms).abs() > CLOCK_SLACK_MS {
            // Set backward, or forward by less than a gap: the figures are fine (they use the
            // monotonic span); only the times jump. Buckets do not span the jump.
            store.close_buckets(&mut self.sinks);
            store.push_marker(
                Marker {
                    seq,
                    utc_ms,
                    kind: MarkerKind::ClockChanged {
                        before_utc_ms: before_utc,
                    },
                },
                &mut self.sinks,
            );
        }
        let s = split(&reading.procs, &mut store.names);
        let events = self.presence.update(s.present, seq, utc_ms, false);
        let record = StepRecord {
            seq,
            utc_ms,
            secs: mono_ms as f32 / 1000.0,
            totals: reading.totals,
            apps: s.apps,
            other: s.other,
        };
        store.push_step(record, &mut self.sinks);
        for e in events {
            store.push_event(e, &mut self.sinks);
        }
        Outcome::Recorded
    }
}

#[cfg(test)]
mod tests {
    use super::super::apps::proc;
    use super::super::record::EventKind;
    use super::super::store::{totals, Tier};
    use super::*;

    /// An arbitrary UTC time in November 2025, not on a minute boundary.
    const T0: i64 = 1_763_000_000_000;

    fn reading(names: &[&str]) -> Reading {
        Reading {
            totals: totals(20.0),
            procs: names.iter().map(|n| proc(n, 5.0, 100)).collect(),
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn steady_steps_are_recorded_with_their_real_span() {
        let mut store = Store::default();
        let mut core = Core::new(&mut store, secs(0), T0, Vec::new());
        assert_eq!(
            core.push(&mut store, secs(5), T0 + 5000, reading(&["a.exe"])),
            Outcome::Recorded
        );
        // A slow refresh: 31 s real time, still below the gap limit.
        assert_eq!(
            core.push(&mut store, secs(36), T0 + 36_000, reading(&["a.exe"])),
            Outcome::Recorded
        );
        let recs: Vec<_> = store.fine().iter().map(|r| (r.seq, r.secs)).collect();
        assert_eq!(recs, vec![(1, 5.0), (2, 31.0)]);
        assert_eq!(
            store.markers().iter().next().unwrap().kind,
            MarkerKind::Began
        );
    }

    #[test]
    fn a_sleep_is_a_gap_not_a_flat_line() {
        let mut store = Store::default();
        let mut core = Core::new(&mut store, secs(0), T0, Vec::new());
        core.push(&mut store, secs(5), T0 + 5000, reading(&["a.exe", "b.exe"]));
        // Two hours asleep, counted by the monotonic clock.
        let out = core.push(
            &mut store,
            secs(7205),
            T0 + 7_205_000,
            reading(&["a.exe", "c.exe"]),
        );
        assert_eq!(out, Outcome::Gap);
        assert_eq!(store.fine().len(), 1, "the reading over the gap is dropped");
        let gap = store.markers().last().unwrap();
        assert_eq!(
            gap.kind,
            MarkerKind::Gap {
                since_utc_ms: T0 + 5000
            }
        );
        // Events across the gap are flagged as "some time during the gap".
        let after: Vec<_> = store
            .events()
            .iter()
            .filter(|e| e.during_gap)
            .map(|e| (store.names.name(e.app).unwrap(), e.kind))
            .collect();
        assert_eq!(
            after,
            vec![("b.exe", EventKind::Exited), ("c.exe", EventKind::Started)]
        );
        // The bucket before the gap is closed; recording resumes normally.
        assert_eq!(store.tier(Tier::Minute).len(), 1);
        assert_eq!(
            core.push(&mut store, secs(7210), T0 + 7_210_000, reading(&["a.exe"])),
            Outcome::Recorded
        );
    }

    #[test]
    fn a_sleep_the_monotonic_clock_missed_is_still_a_gap() {
        let mut store = Store::default();
        let mut core = Core::new(&mut store, secs(0), T0, Vec::new());
        let out = core.push(&mut store, secs(5), T0 + 3_600_000, reading(&["a.exe"]));
        assert_eq!(out, Outcome::Gap);
        assert!(store.fine().is_empty_for_test());
    }

    #[test]
    fn the_clock_set_backward_keeps_order_and_figures() {
        let mut store = Store::default();
        let mut core = Core::new(&mut store, secs(0), T0, Vec::new());
        core.push(&mut store, secs(5), T0 + 5000, reading(&["a.exe"]));
        // The user sets the clock back one hour; 5 s really passed.
        let out = core.push(
            &mut store,
            secs(10),
            T0 + 10_000 - 3_600_000,
            reading(&["a.exe"]),
        );
        assert_eq!(out, Outcome::Recorded);
        let last = store.fine().last().unwrap();
        assert_eq!((last.seq, last.secs), (2, 5.0));
        assert!(matches!(
            store.markers().last().unwrap().kind,
            MarkerKind::ClockChanged { .. }
        ));
        // Buckets did not merge the two sides of the jump.
        assert_eq!(store.tier(Tier::Minute).len(), 1);
        // No app events: nothing started or stopped.
        assert_eq!(store.events().len(), 1);
        // Small drift is not a change.
        core.push(
            &mut store,
            secs(15),
            T0 + 15_000 - 3_600_000 + 2000,
            reading(&["a.exe"]),
        );
        assert_eq!(store.markers().len(), 2);
    }

    #[test]
    fn a_stuck_monotonic_clock_records_nothing() {
        let mut store = Store::default();
        let mut core = Core::new(&mut store, secs(5), T0, Vec::new());
        assert_eq!(
            core.push(&mut store, secs(5), T0 + 5000, reading(&["a.exe"])),
            Outcome::Skipped
        );
        assert!(store.fine().is_empty_for_test());
    }

    #[test]
    fn a_full_day_rolls_into_minutes_and_five_minute_buckets() {
        let mut store = Store::default();
        // Start on a 5-minute boundary.
        let start = T0 - T0.rem_euclid(300_000);
        let mut core = Core::new(&mut store, secs(0), start, Vec::new());
        let steps = 24 * 3600 / 5;
        for i in 1..=steps {
            let names: &[&str] = if i % 720 < 360 {
                &["a.exe", "b.exe"]
            } else {
                &["a.exe"]
            };
            core.push(
                &mut store,
                secs(i * 5),
                start + i as i64 * 5000,
                reading(names),
            );
        }
        // Last hour at full detail, 24 h of minutes (the open one not yet closed).
        assert_eq!(store.fine().len(), 720);
        assert_eq!(store.tier(Tier::Minute).len(), 24 * 60);
        assert_eq!(store.tier(Tier::FiveMinutes).len(), 24 * 12);
        let m = store.tier(Tier::Minute).iter().nth(10).unwrap();
        assert_eq!(m.covered_secs, 60.0);
        assert_eq!(m.start_utc_ms.rem_euclid(60_000), 0);
        // b.exe starts and exits once per hour.
        let starts = store
            .events()
            .iter()
            .filter(|e| e.kind == EventKind::Started)
            .count();
        assert_eq!(starts, 24);
    }

    impl<T> super::super::store::Ring<T> {
        fn is_empty_for_test(&self) -> bool {
            self.len() == 0
        }
    }
}
