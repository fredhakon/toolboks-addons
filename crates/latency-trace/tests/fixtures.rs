//! Tests on the synthetic recorded-trace fixtures in `tests/fixtures` (see its README).

use std::path::PathBuf;
use std::time::Duration;

use latency_trace::{analyze_file, analyze_reader, ExecKind, Limits, ModuleName, Report};

/// FILETIME of the fixtures' trace start, 2026-11-16 09:00:00 UTC.
const T0: u64 = 134_392_932_000_000_000;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn report(name: &str) -> Report {
    analyze_file(&fixture(name), &Limits::default()).unwrap()
}

fn us(n: u64) -> Duration {
    Duration::from_micros(n)
}

fn names(r: &Report) -> Vec<String> {
    r.drivers.iter().map(|d| d.module.to_string()).collect()
}

fn driver<'a>(r: &'a Report, name: &str) -> &'a latency_trace::DriverLatency {
    r.drivers
        .iter()
        .find(|d| d.module == ModuleName::Driver(name.to_string()))
        .unwrap_or_else(|| panic!("{name} not in {:?}", names(r)))
}

#[test]
fn basic_ranking_counts_and_figures() {
    let r = report("basic.xml");
    assert_eq!(r.trace_length, Duration::from_secs(2));
    assert_eq!(r.trace_start_filetime_utc, Some(T0));
    assert_eq!(r.input.clock_offset_minutes, Some(60));
    assert_eq!(
        names(&r),
        vec![
            "nvlddmkm.sys".to_string(),
            "ndis.sys".into(),
            "storport.sys".into(),
            ModuleName::Unresolved.to_string(),
            "coolmouse.sys".into(),
        ]
    );
    assert_eq!((r.dpc_runs, r.isr_runs), (5, 2));

    let nv = driver(&r, "nvlddmkm.sys");
    assert_eq!((nv.dpc.count, nv.isr.count), (1, 1));
    assert_eq!(nv.max_exclusive(), us(250));
    assert_eq!(nv.dpc.exclusive.average, us(250));
    assert_eq!(nv.isr.inclusive.max, us(8));
    assert!(nv.above_guideline());
    assert_eq!(nv.worst.len(), 2);
    assert_eq!(nv.worst[0].kind, ExecKind::Dpc);
    assert_eq!(nv.worst[0].cpu, 1);
    assert_eq!(nv.worst[0].at, Duration::from_millis(700));
    assert_eq!(nv.worst[1].kind, ExecKind::Isr);

    let ndis = driver(&r, "ndis.sys");
    assert_eq!(ndis.dpc.count, 2);
    assert_eq!(ndis.dpc.exclusive.max, us(35));
    assert_eq!(ndis.dpc.exclusive.total, us(55));
    assert_eq!(ndis.dpc.exclusive.average, Duration::from_nanos(27_500));
    assert!(!ndis.above_guideline());
    assert!(driver(&r, "storport.sys").above_guideline()); // 30 µs ISR > 25 µs

    let mouse = driver(&r, "coolmouse.sys");
    assert_eq!(mouse.worst[0].kind, ExecKind::TimerDpc);

    // The 12 µs run in a user-mode DLL's range is not credited to that DLL.
    let unresolved = r
        .drivers
        .iter()
        .find(|d| d.module == ModuleName::Unresolved)
        .unwrap();
    assert_eq!(unresolved.max_exclusive(), us(12));

    assert_eq!(r.coverage.total_events, 7);
    assert_eq!(r.coverage.resolved_events, 6);
    assert_eq!(r.coverage.total_exclusive, us(360));
    assert_eq!(r.coverage.resolved_exclusive, us(348));
    let share = r.coverage.share().unwrap();
    assert!((share - 348.0 / 360.0).abs() < 1e-9);

    let i = &r.input;
    assert_eq!(i.events_seen, 14);
    assert_eq!(i.dpc_isr_events, 7);
    assert_eq!(i.image_events, 6);
    assert_eq!(
        (
            i.malformed,
            i.oversized,
            i.broken,
            i.bad_timing,
            i.out_of_order
        ),
        (0, 0, 0, 0, 0)
    );
    assert!(!i.truncated_tail);
    assert_eq!(
        r.losses.map(|l| (l.events_lost, l.buffers_lost)),
        Some((0, 0))
    );
}

#[test]
fn report_keeps_file_names_only() {
    let r = report("basic.xml");
    let all = format!("{r:?}\n{}", r.to_plain_text());
    for leaked in [
        "alice",
        "Users",
        "Downloads",
        "SystemRoot",
        "\\",
        "helper.dll",
    ] {
        assert!(!all.contains(leaked), "report contains {leaked:?}");
    }
}

#[test]
fn nested_runs_are_not_counted_twice() {
    let r = report("nested.xml");
    let stor = driver(&r, "storport.sys");
    let ndis = driver(&r, "ndis.sys");
    let nv = driver(&r, "nvlddmkm.sys");

    // ndis DPC on CPU 0: 100 µs elapsed, 30 µs of it in a storport ISR.
    // ndis DPC on CPU 1: 100 µs elapsed, 30 µs of it in a storport ISR.
    assert_eq!(ndis.dpc.inclusive.max, us(100));
    assert_eq!(ndis.dpc.exclusive.max, us(70));
    assert_eq!(ndis.dpc.inclusive.total, us(200));
    assert_eq!(ndis.dpc.exclusive.total, us(140));
    // The ISR on CPU 2 at the same moment is not subtracted from anything.
    assert_eq!(stor.isr.count, 4);
    assert_eq!(stor.isr.exclusive.total, us(30 + 10 + 80 + 30));
    assert_eq!(stor.isr.exclusive, stor.isr.inclusive);
    // Threaded DPC containing the CPU 1 DPC (which contains an ISR): 500 elapsed, 400 own.
    assert_eq!(nv.dpc.inclusive.max, us(500));
    assert_eq!(nv.dpc.exclusive.max, us(400));
    assert_eq!(nv.worst[0].kind, ExecKind::ThreadedDpc);

    // Ranked by exclusive: storport's 80 µs beats ndis' own 70 µs, although ndis' elapsed
    // 100 µs is longer.
    assert_eq!(names(&r), vec!["nvlddmkm.sys", "storport.sys", "ndis.sys"]);
    assert!(ndis.max_inclusive() > stor.max_inclusive());

    // CPU-time total: the exclusive times add up to the top-level elapsed intervals only.
    assert_eq!(r.coverage.total_exclusive, us(100 + 10 + 80 + 500));
    assert_eq!(r.coverage.share(), Some(1.0));
}

#[test]
fn unresolved_runs_are_kept_and_coverage_is_honest() {
    let r = report("unresolved.xml");
    assert_eq!(r.drivers[0].module, ModuleName::Unresolved);
    let u = &r.drivers[0];
    assert_eq!((u.dpc.count, u.isr.count), (1, 1));
    assert_eq!(u.max_exclusive(), us(50));
    assert_eq!(u.total_exclusive(), us(60));
    assert_eq!(driver(&r, "ndis.sys").total_exclusive(), us(60));
    assert_eq!(r.coverage.share(), Some(0.5));
    assert_eq!(
        (r.coverage.resolved_events, r.coverage.total_events),
        (3, 5)
    );
    let text = r.to_plain_text();
    assert!(
        text.contains("50.0% of the measured time was matched to a driver"),
        "{text}"
    );
    assert!(text.contains("Unknown code"), "{text}");
}

#[test]
fn spike_keeps_time_and_cpu_without_a_header() {
    let r = report("spike.xml");
    assert_eq!(r.losses, None);
    assert_eq!(r.input.clock_offset_minutes, Some(60));
    assert_eq!(r.trace_start_filetime_utc, Some(T0));
    assert_eq!(r.trace_length, Duration::from_micros(4_000_015));
    let audio = &r.drivers[0];
    assert_eq!(audio.module, ModuleName::Driver("audiodrv.sys".into()));
    let w = audio.worst[0];
    assert_eq!(w.exclusive, us(900));
    assert_eq!(w.cpu, 5);
    assert_eq!(w.at, Duration::from_millis(3250));
    assert_eq!(w.start_filetime_utc, T0 + 32_500_000);
    assert_eq!(w.routine, 0xFFFF_F800_5000_2000);
    let text = r.to_plain_text();
    assert!(
        text.contains("900.0 µs at 3.250 s on CPU 5 (DPC)"),
        "{text}"
    );
    assert!(
        text.contains("not proof that the driver caused a stutter"),
        "{text}"
    );
}

#[test]
fn damaged_input_is_skipped_and_counted() {
    let r = report("damaged.xml");
    let i = &r.input;
    assert_eq!(i.dpc_isr_events, 4, "{i:?}");
    assert_eq!(i.broken, 1, "{i:?}");
    assert_eq!(i.other_events, 2, "{i:?}");
    assert_eq!(i.malformed, 6, "{i:?}");
    assert_eq!(i.bad_timing, 3, "{i:?}");
    assert_eq!(i.out_of_order, 1, "{i:?}");
    assert!(i.truncated_tail);
    assert_eq!(
        r.losses.map(|l| (l.events_lost, l.buffers_lost)),
        Some((12, 1))
    );
    let ndis = driver(&r, "ndis.sys");
    assert_eq!(ndis.dpc.count, 3);
    assert_eq!(ndis.max_exclusive(), us(60));
    assert_eq!(driver(&r, "storport.sys").max_exclusive(), us(15));
    let text = r.to_plain_text();
    assert!(text.contains("dropped 12 events and 1 buffer,"), "{text}");
    assert!(
        text.contains("10 damaged events were skipped and the file ended"),
        "{text}"
    );
}

#[test]
fn invalid_utf8_is_read_lossily() {
    let r = report("non_utf8.xml");
    assert_eq!(r.input.non_utf8, 1);
    assert_eq!(names(&r), vec!["wird.sys", "ndis.sys"]);
    assert_eq!(r.coverage.share(), Some(1.0));
}

#[test]
fn utf16_export_gives_the_same_report() {
    let bytes = std::fs::read(fixture("basic.xml")).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let mut utf16 = vec![0xFF, 0xFE];
    for u in text.encode_utf16() {
        utf16.extend_from_slice(&u.to_le_bytes());
    }
    let a = report("basic.xml");
    let mut b = analyze_reader(utf16.as_slice(), &Limits::default()).unwrap();
    assert!(b.input.utf16);
    b.input.utf16 = false;
    b.input.bytes_read = a.input.bytes_read;
    assert_eq!(a, b);
}

#[test]
fn text_is_plain_and_bounded() {
    for name in [
        "basic.xml",
        "nested.xml",
        "unresolved.xml",
        "spike.xml",
        "damaged.xml",
        "non_utf8.xml",
    ] {
        let text = report(name).to_plain_text();
        assert!(!text.contains('<') && !text.contains('>'), "{name}: {text}");
        assert!(!text.contains('\u{FFFD}'), "{name}: {text}");
        assert!(text.len() < 8 * 1024, "{name}");
        assert!(text.lines().all(|l| l.chars().count() < 400), "{name}");
    }
    let text = report("basic.xml").to_plain_text();
    assert!(text.contains(
        "1. nvlddmkm.sys: longest run 250.0 µs (elapsed 250.0 µs), longer than Microsoft's guidance"
    ));
    assert!(text.contains("96.7% of the measured time was matched to a driver"));
}

#[test]
fn caps_on_drivers_and_worst_runs() {
    let limits = Limits {
        max_drivers: 2,
        worst_per_driver: 1,
        ..Limits::default()
    };
    let r = analyze_file(&fixture("basic.xml"), &limits).unwrap();
    assert_eq!(names(&r), vec!["nvlddmkm.sys", "ndis.sys"]);
    assert_eq!(r.drivers_not_shown, 3);
    assert!(r.drivers.iter().all(|d| d.worst.len() == 1));
    // Totals still cover every driver.
    assert_eq!((r.dpc_runs, r.isr_runs), (5, 2));
    assert!(r.to_plain_text().contains("3 more drivers"));
}

#[test]
fn same_input_same_report() {
    assert_eq!(report("nested.xml"), report("nested.xml"));
    assert_eq!(report("damaged.xml"), report("damaged.xml"));
}
