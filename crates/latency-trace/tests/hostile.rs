//! Hostile and oversized input: nothing may panic, and memory must not grow with the input.

use std::io::{self, Read};
use std::time::Duration;

use latency_trace::{analyze_reader, Limits, ModuleName};

/// FILETIME of 2026-11-16 09:00:00 UTC; wall-clock text below is UTC + 1 h.
const T0: u64 = 134_392_932_000_000_000;

/// Wall-clock text (UTC + 1 h) for `t0_offset` ticks after `T0`.
fn wall(ticks_after_t0: u64) -> String {
    let total = ticks_after_t0 + 36_000_000_000; // + 1 hour
    let secs = total / 10_000_000;
    let frac = total % 10_000_000;
    let (h, m, s) = (9 + secs / 3600, (secs / 60) % 60, secs % 60);
    assert!(h < 24, "keep generated traces within one day");
    format!("2026-11-16T{h:02}:{m:02}:{s:02}.{frac:07}00+00:59")
}

fn run(opcode: u32, cpu: u32, start: u64, len: u64, routine: u64) -> String {
    format!(
        "<Event xmlns=\"http://schemas.microsoft.com/win/2004/08/events/event\">\r\n\
         \t<System>\r\n\t\t<Opcode>{opcode}</Opcode>\r\n\
         \t\t<TimeCreated SystemTime=\"{}\" />\r\n\
         \t\t<Execution ProcessID=\"0\" ThreadID=\"0\" ProcessorID=\"{cpu}\" KernelTime=\"0\" UserTime=\"0\" />\r\n\
         \t</System>\r\n\t<EventData>\r\n\
         \t\t<Data Name=\"InitialTime\">{}</Data>\r\n\
         \t\t<Data Name=\"Routine\">0x{routine:X}</Data>\r\n\
         \t</EventData>\r\n\
         \t<ExtendedTracingInfo xmlns=\"http://schemas.microsoft.com/win/2004/08/events/trace\">\r\n\
         \t\t<EventGuid>{{ce1dbfb4-137e-4da6-87b0-3f59aa102cbc}}</EventGuid>\r\n\
         \t</ExtendedTracingInfo>\r\n</Event>\r\n",
        wall(start + len),
        T0 + start,
    )
}

fn image(name: &str, base: u64, size: u64) -> String {
    format!(
        "<Event>\r\n\t<System>\r\n\t\t<Opcode>3</Opcode>\r\n\
         \t\t<TimeCreated SystemTime=\"{}\" />\r\n\
         \t\t<Execution ProcessID=\"4\" ThreadID=\"0\" ProcessorID=\"0\" />\r\n\t</System>\r\n\
         \t<EventData>\r\n\t\t<Data Name=\"ImageBase\">0x{base:X}</Data>\r\n\
         \t\t<Data Name=\"ImageSize\">0x{size:X}</Data>\r\n\
         \t\t<Data Name=\"ProcessId\">       0</Data>\r\n\
         \t\t<Data Name=\"FileName\">\\SystemRoot\\System32\\drivers\\{name}</Data>\r\n\
         \t</EventData>\r\n\
         \t<ExtendedTracingInfo>\r\n\t\t<EventGuid>{{2cb15d1d-5fc1-11d2-abe1-00a0c911f518}}</EventGuid>\r\n\
         \t</ExtendedTracingInfo>\r\n</Event>\r\n",
        wall(0)
    )
}

const NDIS: u64 = 0xFFFF_F800_1000_0000;

/// A reader that produces `total` bytes from a generator without holding them all.
struct Generated<F> {
    next: F,
    pending: Vec<u8>,
    pos: usize,
    left: u64,
}

impl<F: FnMut() -> Vec<u8>> Generated<F> {
    fn new(total: u64, next: F) -> Self {
        Self {
            next,
            pending: Vec::new(),
            pos: 0,
            left: total,
        }
    }
}

impl<F: FnMut() -> Vec<u8>> Read for Generated<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        if self.pos >= self.pending.len() {
            self.pending = (self.next)();
            self.pos = 0;
        }
        let n = (self.pending.len() - self.pos)
            .min(buf.len())
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
        self.pos += n;
        self.left -= n as u64;
        Ok(n)
    }
}

#[test]
fn one_gigabyte_of_events_is_streamed_with_bounded_state() {
    const GIB: u64 = 1 << 30;
    let limits = Limits {
        max_routines: 1000,
        ..Limits::default()
    };
    // One 64 KiB block of runs (each routine distinct, so the routine cap is hit), followed by
    // filler whitespace so the test stays quick in debug builds while still streaming 1 GiB.
    let mut i: u64 = 0;
    let mut first = true;
    let reader = Generated::new(GIB, move || {
        let mut out = String::with_capacity(1 << 20);
        if first {
            first = false;
            out.push_str("<Events>\r\n");
            out.push_str(&image("ndis.sys", NDIS, 0x0100_0000));
        }
        while out.len() < 64 * 1024 {
            let start = (i % 30_000_000) * 1000; // stays inside one day
            out.push_str(&run(68, (i % 16) as u32, start, 10 + i % 90, NDIS + i * 16));
            i += 1;
        }
        out.push_str(&" ".repeat(960 * 1024));
        out.into_bytes()
    });
    let r = analyze_reader(reader, &limits).unwrap();
    assert_eq!(r.input.bytes_read, GIB);
    assert!(r.input.dpc_isr_events > 10_000, "{:?}", r.input);
    assert!(r.input.routines_over_cap > 0, "{:?}", r.input);
    assert!(r.drivers.len() <= limits.max_drivers);
    assert!(r
        .drivers
        .iter()
        .all(|d| d.worst.len() <= limits.worst_per_driver));
    // Runs past the routine cap are counted as unresolved, never dropped.
    let total: u64 = r.drivers.iter().map(|d| d.count()).sum();
    assert_eq!(total, r.input.dpc_isr_events);
    assert!(r.drivers.iter().any(|d| d.module == ModuleName::Unresolved));
    assert!(r.to_plain_text().len() < 8 * 1024);
}

#[test]
fn one_gigabyte_event_without_an_end_is_skipped() {
    const GIB: u64 = 1 << 30;
    let mut first = true;
    let reader = Generated::new(GIB, move || {
        if first {
            first = false;
            let mut v = run(68, 0, 1000, 10, NDIS).into_bytes();
            v.extend_from_slice(b"<Event>");
            v
        } else {
            vec![b'a'; 1 << 20]
        }
    });
    let r = analyze_reader(reader, &Limits::default()).unwrap();
    assert_eq!(r.input.bytes_read, GIB);
    assert_eq!(r.input.dpc_isr_events, 1);
    assert_eq!(r.input.oversized, 1);
    assert_eq!(r.drivers.len(), 1);
}

#[test]
fn empty_and_junk_inputs() {
    for input in [
        &b""[..],
        b"\xFF\xFE",
        b"\xEF\xBB\xBF",
        b"<",
        b"<Event",
        b"<Event>",
        b"</Event></Event>",
        b"<Events></Events>",
        b"\x00\x00\x00\x00",
        b"<\x00E\x00",
    ] {
        let r = analyze_reader(input, &Limits::default()).unwrap();
        assert!(r.drivers.is_empty());
        assert_eq!(r.coverage.share(), None);
        assert!(r
            .to_plain_text()
            .contains("No DPC or ISR runs could be measured"));
    }
}

#[test]
fn random_bytes_and_spliced_events_never_panic() {
    let pieces: Vec<String> = vec![
        run(68, 0, 1000, 10, NDIS),
        run(67, 1, 2000, 5, NDIS + 8),
        image("ndis.sys", NDIS, 0x1000),
        "<Data Name=\"Routine\">0x".into(),
        "<Data Name=\"InitialTime\">-1</Data>".into(),
        "</EventData></Event>".into(),
        "<Event>".into(),
        "<System><Opcode>67</Opcode>".into(),
        "SystemTime=\"9999-12-31T23:59:59.9999999\"".into(),
        "ProcessorID=\"4294967295\"".into(),
    ];
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..50 {
        let mut input = Vec::new();
        while input.len() < 256 * 1024 {
            let r = rnd();
            if r % 3 == 0 {
                let len = (r >> 8) as usize % 64;
                input.extend((0..len).map(|_| rnd() as u8));
            } else {
                let p = &pieces[(r >> 8) as usize % pieces.len()];
                let cut = (r >> 32) as usize % (p.len() + 1);
                let take = if r % 2 == 0 { p.len() } else { cut };
                input.extend_from_slice(&p.as_bytes()[..take]);
            }
        }
        let r = analyze_reader(input.as_slice(), &Limits::default()).unwrap();
        let _ = r.to_plain_text();
    }
}

#[test]
fn many_cpus_are_capped() {
    let mut input = String::from("<Events>");
    input.push_str(&image("ndis.sys", NDIS, 0x1000));
    for cpu in 0..3000u32 {
        input.push_str(&run(68, cpu, u64::from(cpu) * 1000, 10, NDIS));
    }
    let limits = Limits {
        max_cpus: 1024,
        ..Limits::default()
    };
    let r = analyze_reader(input.as_bytes(), &limits).unwrap();
    assert_eq!(r.input.dpc_isr_events, 3000);
    assert_eq!(r.input.nesting_untracked, 3000 - 1024);
    assert_eq!(r.drivers[0].dpc.count, 3000);
}

#[test]
fn many_interrupts_inside_one_long_dpc_stay_exact_with_a_small_pending_list() {
    // A 10 ms DPC on CPU 0 interrupted by 300 ISRs of 10 µs each.
    let mut input = String::from("<Events>");
    input.push_str(&image("ndis.sys", NDIS, 0x1000));
    input.push_str(&image("storport.sys", NDIS + 0x10_0000, 0x1000));
    for k in 0..300u64 {
        input.push_str(&run(67, 0, 1000 + k * 300, 100, NDIS + 0x10_0000));
    }
    input.push_str(&run(68, 0, 0, 100_000, NDIS));
    let limits = Limits {
        max_pending_per_cpu: 16,
        ..Limits::default()
    };
    let r = analyze_reader(input.as_bytes(), &limits).unwrap();
    let ndis = r
        .drivers
        .iter()
        .find(|d| d.module == ModuleName::Driver("ndis.sys".into()))
        .unwrap();
    assert_eq!(ndis.dpc.inclusive.max, Duration::from_millis(10));
    assert_eq!(ndis.dpc.exclusive.max, Duration::from_millis(7));
    assert_eq!(r.coverage.total_exclusive, Duration::from_millis(10));
}

#[test]
fn huge_and_odd_numbers_do_not_overflow() {
    let mut input = String::from("<Events>");
    // Header with absurd values.
    input.push_str(
        "<Event><System><Opcode>0</Opcode><TimeCreated SystemTime=\"2026-11-16T10:00:00.0000000+00:59\" /></System>\
         <EventData><Data Name=\"StartTime\">134392932000000000</Data>\
         <Data Name=\"EndTime\">18446744073709551615</Data>\
         <Data Name=\"EventsLost\">18446744073709551615</Data>\
         <Data Name=\"BuffersLost\">99999999999999999999999999</Data></EventData>\
         <ExtendedTracingInfo><EventGuid>{68fdd900-4a3e-11d1-84f4-0000f80464e3}</EventGuid></ExtendedTracingInfo></Event>",
    );
    // An image reaching the top of the address space.
    input.push_str(&image("top.sys", u64::MAX - 0x10, 0x1000));
    input.push_str(&run(68, 0, 1000, 20, u64::MAX - 1));
    input.push_str(&run(68, u32::MAX, 2000, 10, 0));
    let r = analyze_reader(input.as_bytes(), &Limits::default()).unwrap();
    let l = r.losses.unwrap();
    assert_eq!(l.events_lost, u64::MAX);
    assert_eq!(l.buffers_lost, 0);
    // The absurd EndTime is ignored; the length comes from the runs.
    assert_eq!(r.trace_length, Duration::from_nanos(101_000));
    assert_eq!(r.drivers[0].module, ModuleName::Driver("top.sys".into()));
    assert!(r.to_plain_text().contains("18446744073709551615 events"));
}
