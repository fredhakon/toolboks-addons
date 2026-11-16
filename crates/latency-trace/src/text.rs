//! The plain-language summary. Plain text only; its size is bounded by the report's caps
//! (drivers, worst runs per driver, 64-character names).

use std::fmt::Write;
use std::time::Duration;

use crate::{ModuleName, Report};

/// Worst runs listed per driver in the text (the report may keep more).
const WORST_IN_TEXT: usize = 3;

/// `412.0 µs`, `1.25 ms`, `10.0 s`.
pub(crate) fn fmt_duration(d: Duration) -> String {
    let us = d.as_secs_f64() * 1_000_000.0;
    if us < 1000.0 {
        format!("{us:.1} µs")
    } else if us < 1_000_000.0 {
        format!("{:.2} ms", us / 1000.0)
    } else {
        format!("{:.1} s", us / 1_000_000.0)
    }
}

/// `1 DPC run`, `2 DPC runs`.
fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `3.214 s` for a moment in the trace.
fn fmt_at(d: Duration) -> String {
    format!("{:.3} s", d.as_secs_f64())
}

pub(crate) fn render(r: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Latency check: {} recorded, {} and {} measured.",
        fmt_duration(r.trace_length),
        count(r.dpc_runs, "DPC run", "DPC runs"),
        count(r.isr_runs, "ISR run", "ISR runs")
    );
    if r.drivers.is_empty() {
        let _ = writeln!(
            out,
            "No DPC or ISR runs could be measured in this recording, so no driver can be named."
        );
        notes(r, &mut out);
        return out;
    }
    let _ = writeln!(
        out,
        "Drivers are ranked by their longest single run of their own code (time spent in \
         interrupts that arrived during the run is not counted). The full elapsed time is \
         shown in brackets."
    );
    if let Some(share) = r.coverage.share() {
        let pct = share * 100.0;
        if pct >= 99.95 {
            let _ = writeln!(out, "All measured time was matched to a driver.");
        } else {
            let _ = writeln!(
                out,
                "{pct:.1}% of the measured time was matched to a driver; the rest is listed as \
                 unknown code."
            );
        }
    }
    let _ = writeln!(out);

    for (i, d) in r.drivers.iter().enumerate() {
        let guideline = if d.above_guideline() {
            ", longer than Microsoft's guidance (DPC 100 µs, ISR 25 µs)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "{}. {}: longest run {} (elapsed {}){}.",
            i + 1,
            d.module,
            fmt_duration(d.max_exclusive()),
            fmt_duration(d.max_inclusive()),
            guideline
        );
        let runs = d.count().max(1);
        let avg = d.total_exclusive() / u32::try_from(runs).unwrap_or(u32::MAX);
        let _ = writeln!(
            out,
            "   {}, {}; average {}, total {} (elapsed total {}).",
            count(d.dpc.count, "DPC run", "DPC runs"),
            count(d.isr.count, "ISR run", "ISR runs"),
            fmt_duration(avg),
            fmt_duration(d.total_exclusive()),
            fmt_duration(d.total_inclusive())
        );
        let worst: Vec<String> = d
            .worst
            .iter()
            .take(WORST_IN_TEXT)
            .map(|w| {
                format!(
                    "{} at {} on CPU {} ({})",
                    fmt_duration(w.exclusive),
                    fmt_at(w.at),
                    w.cpu,
                    w.kind.label()
                )
            })
            .collect();
        if !worst.is_empty() {
            let _ = writeln!(out, "   Longest runs: {}.", worst.join("; "));
        }
    }
    if r.drivers_not_shown > 0 {
        let _ = writeln!(
            out,
            "{} had shorter runs and {} not listed.",
            count(r.drivers_not_shown as u64, "more driver", "more drivers"),
            if r.drivers_not_shown == 1 {
                "is"
            } else {
                "are"
            }
        );
    }
    let _ = writeln!(out);
    notes(r, &mut out);
    out
}

fn notes(r: &Report, out: &mut String) {
    if let Some(l) = r.losses {
        if l.events_lost > 0 || l.buffers_lost > 0 {
            let _ = writeln!(
                out,
                "The recording dropped {} and {}, so some runs may be missing.",
                count(l.events_lost, "event", "events"),
                count(l.buffers_lost, "buffer", "buffers")
            );
        }
    }
    let i = &r.input;
    let skipped = i
        .malformed
        .saturating_add(i.oversized)
        .saturating_add(i.broken)
        .saturating_add(i.bad_timing);
    if skipped > 0 || i.truncated_tail {
        let _ = writeln!(
            out,
            "{} skipped{}.",
            if skipped == 1 {
                "1 damaged event was".to_string()
            } else {
                format!("{skipped} damaged events were")
            },
            if i.truncated_tail {
                " and the file ended in the middle of an event"
            } else {
                ""
            }
        );
    }
    if r.drivers.iter().any(|d| d.module == ModuleName::Unresolved) {
        let _ = writeln!(
            out,
            "Unknown code means the address was not inside any driver listed in the recording."
        );
    }
    let _ = writeln!(
        out,
        "A long run is a fact about this recording, not proof that the driver caused a stutter \
         you noticed. Compare the times above with when the stutter happened."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(fmt_duration(Duration::from_nanos(412_000)), "412.0 µs");
        assert_eq!(fmt_duration(Duration::from_micros(1250)), "1.25 ms");
        assert_eq!(fmt_duration(Duration::from_secs(10)), "10.0 s");
    }
}
