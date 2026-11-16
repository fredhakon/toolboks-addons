//! Turns a recorded Windows kernel trace of DPC and ISR events into a plain-language ranking
//! of the drivers that held the CPU the longest.
//!
//! This crate only reads a trace that was already recorded and exported to text. Recording it
//! (the NT Kernel Logger with DPC, interrupt and image-load events, which needs administrator
//! rights) is a separate step. A collector hands this crate a file path ([`analyze_file`]) or any
//! reader ([`analyze_reader`]). Parsing is pure Rust with no Windows API, so it builds and is
//! tested on any machine.
//!
//! # Input format: tracerpt XML
//!
//! The input is the XML that Windows' built-in `tracerpt` writes:
//! `tracerpt trace.etl -o trace.xml -of XML` (XML is tracerpt's default output format;
//! [tracerpt reference](https://learn.microsoft.com/windows-server/administration/windows-commands/tracerpt)).
//! XML was chosen over tracerpt's CSV because every payload field is named
//! (`<Data Name="InitialTime">`), while CSV puts the payload of all event types in unnamed
//! positional columns that differ per event type.
//!
//! Each event is one `<Event>` element; the parts read here are:
//!
//! - `System/Opcode` (event type number), `System/TimeCreated/@SystemTime` (when the event was
//!   logged), `System/Execution/@ProcessorID` (the CPU), and
//!   `ExtendedTracingInfo/EventGuid` (the MOF class GUID). Without a GUID or numeric opcode,
//!   the rendered `RenderingInfo/EventName` and `RenderingInfo/Opcode` names are used instead.
//! - **DPC** events, class `PerfInfo` `{ce1dbfb4-137e-4da6-87b0-3f59aa102cbc}`, event types
//!   66 ThreadDPC, 68 DPC, 69 TimerDPC; fields `InitialTime` (entry time) and `Routine`
//!   (address of the routine). [DPC class](https://learn.microsoft.com/windows/win32/etw/dpc),
//!   [PerfInfo class](https://learn.microsoft.com/windows/win32/etw/perfinfo).
//! - **ISR** events, class `PerfInfo`, event type 67; fields `InitialTime`, `Routine`
//!   (also `ReturnValue` and `Vector`, unused here).
//!   [ISR class](https://learn.microsoft.com/windows/win32/etw/isr).
//! - **Image** events, class `Image` `{2cb15d1d-5fc1-11d2-abe1-00a0c911f518}`, event types
//!   10 Load, 2 Unload, 3 DCStart, 4 DCEnd; fields `ImageBase`, `ImageSize`, `ProcessId`,
//!   `FileName`. [Image_Load class](https://learn.microsoft.com/windows/win32/etw/image-load),
//!   [Image class](https://learn.microsoft.com/windows/win32/etw/image).
//! - The log **header**, class `EventTrace` `{68fdd900-4a3e-11d1-84f4-0000f80464e3}`, event
//!   type 0; fields `StartTime`, `EndTime` (UTC FILETIME), `EventsLost`, `BuffersLost`.
//!   [EventTrace_Header class](https://learn.microsoft.com/windows/win32/etw/eventtrace-header).
//!
//! A DPC or ISR event is logged when the routine returns and carries its entry time in
//! `InitialTime`, so its elapsed time is the event's timestamp minus `InitialTime` (Microsoft:
//! "DPC and ISR events have an entry time stamp which is used to compute the duration of the
//! routines"). tracerpt prints `InitialTime` as a FILETIME number (100 ns since 1601, UTC; see
//! [PerfView issue 234](https://github.com/microsoft/perfview/issues/234) for a real sample)
//! and `TimeCreated` as local wall-clock text with a time-zone suffix. The suffix is not
//! reliable (a real tracerpt run on Windows 11 build 26300 printed `+00:59` on a UTC+1 machine),
//! so it is ignored: the crate takes the difference between wall-clock and UTC from the header
//! (`TimeCreated` minus `StartTime`, rounded to whole minutes), or, without a header, from the
//! first DPC/ISR whose elapsed time is plausible. The trace must be recorded with the
//! performance-counter clock; Microsoft notes the system timer is too coarse for DPC/ISR timing
//! ([Measuring DPC/ISR Time](https://learn.microsoft.com/windows-hardware/drivers/devtest/example-15--measuring-dpc-isr-time)).
//!
//! # What is measured
//!
//! Every DPC/ISR run gets two durations:
//!
//! - **Inclusive** (elapsed): from entry to return, including time spent in interrupts that
//!   arrived during the run.
//! - **Exclusive** (own execution): the inclusive time minus every DPC/ISR run that started and
//!   finished inside it on the same CPU (for example an ISR that interrupted a DPC). Exclusive
//!   times never overlap, so they can be added up into a CPU-time total without counting a
//!   nested interval twice. Microsoft's
//!   [WPA CPU analysis guide](https://learn.microsoft.com/windows-hardware/test/wpt/cpu-analysis)
//!   makes the same inclusive/exclusive distinction.
//!
//! Drivers are **ranked by the longest single exclusive run** ([`DriverLatency::max_exclusive`]),
//! ties broken by total exclusive time. The longest single run is what delays everything else
//! on that CPU; inclusive figures are shown beside it. Totals and coverage use exclusive time.
//!
//! Routine addresses are matched to the kernel-mode images in the same trace. Runs whose
//! address is in no known image are kept in an explicit [`ModuleName::Unresolved`] bucket that
//! is ranked like any driver, and [`Coverage`] says what share of the time was matched.
//!
//! The worst runs keep their CPU and time from the trace start, so a UI can say "a 2 ms spike
//! at 14.2 s on CPU 3". That is a fact about the recording, not proof the driver caused a
//! stutter the user noticed.
//!
//! # Safety on bad input
//!
//! The input is streamed in 64 KiB chunks and split into `<Event>` blocks of at most
//! [`Limits::max_event_bytes`]; damaged, oversized, non-UTF-8 or unknown events are counted in
//! [`InputStats`] and skipped. Only per-routine sums and a few worst runs are kept, all with
//! fixed caps, so memory use does not grow with the size of the file. Module names keep the file
//! name only (no folders, so no user names). Nothing is written anywhere; the [`Report`] is all
//! that is left.

mod analyze;
mod event;
mod scan;
mod text;
mod time;

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

/// Hard ceiling on [`Limits::worst_per_driver`].
pub const MAX_WORST: usize = 32;
/// Hard ceiling on [`Limits::max_drivers`].
pub const MAX_DRIVERS: usize = 200;
/// Hard ceiling on [`Limits::max_plausible_duration`]: the clock-offset detection rounds to
/// whole minutes, so a single run must be well under 30 seconds.
pub const MAX_PLAUSIBLE: Duration = Duration::from_secs(20);

/// Caps that keep the work and memory bounded. [`Limits::default`] suits a check of a few
/// seconds to minutes.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Largest single `<Event>` block read; bigger blocks are skipped and counted.
    pub max_event_bytes: usize,
    /// Distinct routine addresses tracked; runs of further routines go to the unresolved bucket.
    pub max_routines: usize,
    /// Kernel images remembered for address matching.
    pub max_images: usize,
    /// CPUs whose nesting is tracked; runs on further CPUs count as exclusive = inclusive.
    pub max_cpus: usize,
    /// Finished runs remembered per CPU for nesting.
    pub max_pending_per_cpu: usize,
    /// Worst runs kept per driver (at most [`MAX_WORST`]).
    pub worst_per_driver: usize,
    /// Drivers in the report (at most [`MAX_DRIVERS`]); the rest are only counted.
    pub max_drivers: usize,
    /// Runs longer than this are treated as broken timestamps (at most [`MAX_PLAUSIBLE`]).
    pub max_plausible_duration: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_event_bytes: 64 * 1024,
            max_routines: 16 * 1024,
            max_images: 8 * 1024,
            max_cpus: 1024,
            max_pending_per_cpu: 128,
            worst_per_driver: 5,
            max_drivers: 25,
            max_plausible_duration: Duration::from_secs(1),
        }
    }
}

/// Which kind of routine ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecKind {
    /// A normal deferred procedure call (event type 68).
    Dpc,
    /// A threaded DPC (event type 66).
    ThreadedDpc,
    /// A DPC run because a timer expired (event type 69).
    TimerDpc,
    /// An interrupt service routine (event type 67).
    Isr,
}

impl ExecKind {
    /// Plain-language name.
    pub fn label(self) -> &'static str {
        match self {
            ExecKind::Dpc => "DPC",
            ExecKind::ThreadedDpc => "threaded DPC",
            ExecKind::TimerDpc => "timer DPC",
            ExecKind::Isr => "ISR",
        }
    }
}

/// The module a group of runs belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleName {
    /// A driver or kernel image, by file name only (for example `ndis.sys`).
    Driver(String),
    /// Runs whose routine address matched no image in the trace.
    Unresolved,
}

impl fmt::Display for ModuleName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleName::Driver(n) => f.write_str(n),
            ModuleName::Unresolved => f.write_str("Unknown code (address not in any driver)"),
        }
    }
}

/// Maximum, average and total of one measure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stat {
    pub max: Duration,
    pub average: Duration,
    pub total: Duration,
}

/// Figures for one kind of routine (DPC or ISR) of one driver.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KindStats {
    pub count: u64,
    /// Own execution time (nested runs on the same CPU subtracted). The ranked measure.
    pub exclusive: Stat,
    /// Elapsed time from entry to return.
    pub inclusive: Stat,
}

/// One of the longest runs, with where and when it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorstEvent {
    pub kind: ExecKind,
    pub cpu: u32,
    /// Start of the run, measured from the start of the trace.
    pub at: Duration,
    /// Start of the run as a UTC FILETIME (100 ns since 1601), for lining up with other clocks.
    pub start_filetime_utc: u64,
    pub exclusive: Duration,
    pub inclusive: Duration,
    /// Address of the routine.
    pub routine: u64,
}

/// One driver's line in the ranking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverLatency {
    pub module: ModuleName,
    /// All DPC kinds together (normal, threaded and timer).
    pub dpc: KindStats,
    pub isr: KindStats,
    /// The longest runs by exclusive time, longest first.
    pub worst: Vec<WorstEvent>,
}

impl DriverLatency {
    /// The ranked measure: the longest single exclusive run (DPC or ISR).
    pub fn max_exclusive(&self) -> Duration {
        self.dpc.exclusive.max.max(self.isr.exclusive.max)
    }

    /// The longest single inclusive run.
    pub fn max_inclusive(&self) -> Duration {
        self.dpc.inclusive.max.max(self.isr.inclusive.max)
    }

    pub fn total_exclusive(&self) -> Duration {
        self.dpc
            .exclusive
            .total
            .saturating_add(self.isr.exclusive.total)
    }

    pub fn total_inclusive(&self) -> Duration {
        self.dpc
            .inclusive
            .total
            .saturating_add(self.isr.inclusive.total)
    }

    pub fn count(&self) -> u64 {
        self.dpc.count.saturating_add(self.isr.count)
    }

    /// Whether a run was longer than Microsoft's guidance: DPCs at most 100 µs, ISRs at most
    /// 25 µs ([source](https://learn.microsoft.com/windows-hardware/drivers/devtest/example-15--measuring-dpc-isr-time)).
    pub fn above_guideline(&self) -> bool {
        self.dpc.exclusive.max > Duration::from_micros(100)
            || self.isr.exclusive.max > Duration::from_micros(25)
    }
}

/// How much of the measured time could be tied to a driver.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Coverage {
    pub resolved_exclusive: Duration,
    pub total_exclusive: Duration,
    pub resolved_events: u64,
    pub total_events: u64,
}

impl Coverage {
    /// Share of exclusive time matched to a driver, from 0.0 to 1.0; `None` without any runs.
    pub fn share(&self) -> Option<f64> {
        if self.total_events == 0 {
            return None;
        }
        let total = self.total_exclusive.as_secs_f64();
        if total <= 0.0 {
            return Some(self.resolved_events as f64 / self.total_events as f64);
        }
        Some((self.resolved_exclusive.as_secs_f64() / total).clamp(0.0, 1.0))
    }
}

/// Losses the trace itself reported in its header.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TraceLosses {
    pub events_lost: u64,
    pub buffers_lost: u64,
}

/// What was read and what was skipped. Every count saturates instead of overflowing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputStats {
    pub bytes_read: u64,
    /// `<Event>` blocks handed to the event reader.
    pub events_seen: u64,
    /// DPC/ISR runs that were measured.
    pub dpc_isr_events: u64,
    pub image_events: u64,
    /// Well-formed events of other types (ignored).
    pub other_events: u64,
    /// Known event types with missing or unreadable fields.
    pub malformed: u64,
    /// Events bigger than [`Limits::max_event_bytes`].
    pub oversized: u64,
    /// Events cut off by the start of the next one.
    pub broken: u64,
    /// DPC/ISR runs with impossible timing (negative, too long, unknown clock).
    pub bad_timing: u64,
    /// Runs that finished earlier than the previous run on the same CPU.
    pub out_of_order: u64,
    /// Runs on CPUs beyond [`Limits::max_cpus`] (nesting not tracked).
    pub nesting_untracked: u64,
    /// Runs of routines beyond [`Limits::max_routines`] (counted as unresolved).
    pub routines_over_cap: u64,
    pub images_over_cap: u64,
    /// Events that were not valid UTF-8 (read with replacement characters).
    pub non_utf8: u64,
    /// The input ended in the middle of an event.
    pub truncated_tail: bool,
    /// The input was UTF-16 and was converted.
    pub utf16: bool,
    /// Wall-clock time minus UTC in the trace, in minutes, once known.
    pub clock_offset_minutes: Option<i64>,
}

/// The result: a bounded, plain-data ranking of drivers by latency.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// From the header's start to end time, or else from the first to the last measured run.
    pub trace_length: Duration,
    pub trace_start_filetime_utc: Option<u64>,
    /// Ranked by [`DriverLatency::max_exclusive`], longest first; includes the unresolved
    /// bucket when there is one. At most [`Limits::max_drivers`] entries.
    pub drivers: Vec<DriverLatency>,
    /// Drivers left out of `drivers` by the cap.
    pub drivers_not_shown: usize,
    /// DPC runs (all kinds) and ISR runs measured, over all drivers including those not shown.
    pub dpc_runs: u64,
    pub isr_runs: u64,
    pub coverage: Coverage,
    pub losses: Option<TraceLosses>,
    pub input: InputStats,
}

impl Report {
    /// A plain-language summary (plain text, no markup), bounded in size.
    pub fn to_plain_text(&self) -> String {
        text::render(self)
    }
}

/// Analyses tracerpt XML from any reader. Only I/O errors are returned; bad content is skipped
/// and counted in [`Report::input`].
pub fn analyze_reader<R: Read>(reader: R, limits: &Limits) -> io::Result<Report> {
    let mut limits = limits.clone();
    limits.max_plausible_duration = limits.max_plausible_duration.min(MAX_PLAUSIBLE);
    analyze::analyze(reader, &limits)
}

/// Analyses a tracerpt XML file. The file is streamed, never read whole.
pub fn analyze_file(path: &Path, limits: &Limits) -> io::Result<Report> {
    analyze_reader(File::open(path)?, limits)
}
