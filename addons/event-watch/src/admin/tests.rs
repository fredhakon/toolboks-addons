//! The latency check's administrator step against a fake system. Nothing here starts a program,
//! records a trace or asks for administrator rights. The fake keeps the state that matters: is
//! the kernel session running, which files exist, does the working folder exist.

use super::*;
use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::rc::Rc;

const BASIC: &[u8] = include_bytes!("../../../../crates/latency-trace/tests/fixtures/basic.xml");
const DIR: &str = r"C:\Windows\Temp\toolboks-latency-test";

struct State {
    calls: Vec<String>,
    waits: Vec<Duration>,
    /// The kernel session is running (ours, or another program's when set up front).
    session: bool,
    files: BTreeSet<PathBuf>,
    workspace: Option<PathBuf>,
    workspace_fails: bool,
    /// Answers to free-space checks, in order; empty means plenty.
    free: VecDeque<Option<u64>>,
    start: Run,
    /// After a start that timed out, did the session start anyway?
    start_timed_out_but_ran: bool,
    /// Answers to stop attempts, in order; empty means success.
    stops: VecDeque<bool>,
    /// A failed stop still ended the session (someone else stopped it).
    stop_failure_ends_session: bool,
    etl_bytes: u64,
    tracerpt: Run,
    tracerpt_writes_xml: bool,
    xml: &'static [u8],
    analyze_fails: bool,
    remove_fails: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            calls: vec![],
            waits: vec![],
            session: false,
            files: BTreeSet::new(),
            workspace: None,
            workspace_fails: false,
            free: VecDeque::new(),
            start: ok(),
            start_timed_out_but_ran: false,
            stops: VecDeque::new(),
            stop_failure_ends_session: false,
            etl_bytes: 1 << 20,
            tracerpt: ok(),
            tracerpt_writes_xml: true,
            xml: BASIC,
            analyze_fails: false,
            remove_fails: false,
        }
    }
}

fn ok() -> Run {
    Run::Exited {
        code: 0,
        output: "The command completed successfully.".into(),
    }
}

fn exit(code: i32) -> Run {
    Run::Exited {
        code,
        output: "Error:\r\nSomething went wrong.\r\n".into(),
    }
}

#[derive(Clone, Default)]
struct Fake(Rc<RefCell<State>>);

impl Fake {
    fn with(set: impl FnOnce(&mut State)) -> Self {
        let fake = Fake::default();
        set(&mut fake.0.borrow_mut());
        fake
    }
    fn s(&self) -> std::cell::RefMut<'_, State> {
        self.0.borrow_mut()
    }
    fn calls(&self) -> Vec<String> {
        self.0.borrow().calls.clone()
    }
    fn count(&self, prefix: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
    /// What must hold after every run: our session ended, no files, no folder.
    fn assert_clean(&self) {
        let s = self.0.borrow();
        assert!(
            !s.session,
            "the kernel session was left running: {:?}",
            s.calls
        );
        assert!(s.files.is_empty(), "files left behind: {:?}", s.files);
        assert!(s.workspace.is_none(), "the working folder was left behind");
    }
}

impl TraceSystem for Fake {
    fn create_workspace(&mut self) -> Result<PathBuf, String> {
        let mut s = self.s();
        s.calls.push("create_workspace".into());
        if s.workspace_fails {
            return Err("A private working folder could not be made. Nothing was recorded.".into());
        }
        s.workspace = Some(PathBuf::from(DIR));
        Ok(PathBuf::from(DIR))
    }
    fn free_bytes(&mut self, dir: &Path) -> Option<u64> {
        let mut s = self.s();
        assert_eq!(dir, Path::new(DIR));
        s.calls.push("free_bytes".into());
        s.free.pop_front().unwrap_or(Some(100 << 30))
    }
    fn run(&mut self, tool: Tool, args: &[OsString], cwd: &Path, _timeout: Duration) -> Run {
        let mut s = self.s();
        assert_eq!(
            cwd,
            Path::new(DIR),
            "programs run inside the private folder"
        );
        let text: Vec<String> = args.iter().map(|a| a.to_string_lossy().into()).collect();
        s.calls.push(format!("{tool:?} {}", text.join(" | ")));
        let etl = PathBuf::from(DIR).join(ETL_NAME);
        match (tool, text[0].as_str()) {
            (Tool::Logman, "start") => {
                if s.session {
                    // The single kernel logger is taken.
                    return Run::Exited {
                        code: 0x8007_00B7_u32 as i32,
                        output: "Data Collector Set already exists.".into(),
                    };
                }
                let run = s.start.clone();
                if run.ok() || (run == Run::TimedOut && s.start_timed_out_but_ran) {
                    s.session = true;
                    if s.etl_bytes > 0 {
                        s.files.insert(etl);
                    }
                }
                run
            }
            (Tool::Logman, "stop") => {
                if !s.session {
                    return exit(-2144337918);
                }
                if s.stops.pop_front().unwrap_or(true) {
                    s.session = false;
                    ok()
                } else {
                    if s.stop_failure_ends_session {
                        s.session = false;
                    }
                    exit(5)
                }
            }
            (Tool::Logman, "query") => {
                if s.session {
                    ok()
                } else {
                    exit(-2144337918)
                }
            }
            (Tool::Tracerpt, _) => {
                assert!(!s.session, "converted while still recording");
                let run = s.tracerpt.clone();
                if run.ok() && s.tracerpt_writes_xml {
                    s.files.insert(PathBuf::from(DIR).join(XML_NAME));
                }
                run
            }
            other => panic!("unexpected command {other:?}"),
        }
    }
    fn wait(&mut self, duration: Duration) {
        let mut s = self.s();
        s.calls.push(format!("wait {}s", duration.as_secs()));
        s.waits.push(duration);
    }
    fn file_size(&mut self, path: &Path) -> Option<u64> {
        let s = self.0.borrow();
        if !s.files.contains(path) {
            return None;
        }
        Some(if path.ends_with(ETL_NAME) {
            s.etl_bytes
        } else {
            s.xml.len() as u64
        })
    }
    fn analyze(&mut self, xml: &Path, limits: &Limits) -> io::Result<Report> {
        let s = self.0.borrow();
        assert!(s.files.contains(xml));
        if s.analyze_fails {
            return Err(io::Error::other("disk read error"));
        }
        latency_trace::analyze_reader(s.xml, limits)
    }
    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        let mut s = self.s();
        if s.remove_fails && s.files.contains(path) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        if s.files.remove(path) {
            Ok(())
        } else {
            Err(io::ErrorKind::NotFound.into())
        }
    }
    fn remove_workspace(&mut self, dir: &Path) -> io::Result<()> {
        let mut s = self.s();
        s.calls.push("remove_workspace".into());
        if !s.files.is_empty() {
            return Err(io::ErrorKind::DirectoryNotEmpty.into());
        }
        assert_eq!(s.workspace.as_deref(), Some(dir));
        s.workspace = None;
        Ok(())
    }
}

fn request(seconds: u32) -> TraceRequest {
    TraceRequest { seconds }
}

fn admin_request(operation: &str, json: &str) -> AdminRequest {
    AdminRequest {
        id: 1,
        operation: operation.into(),
        json: json.into(),
    }
}

fn run(fake: &Fake, seconds: u32) -> Result<String, String> {
    record(&mut fake.clone(), request(seconds))
}

// --- the request -----------------------------------------------------------------------------

#[test]
fn only_the_three_lengths_are_accepted() {
    for seconds in DURATIONS {
        let json = request(seconds).to_json();
        assert_eq!(TraceRequest::parse(&json), Ok(request(seconds)));
    }
    for json in [
        r#"{"seconds":0}"#,
        r#"{"seconds":15}"#,
        r#"{"seconds":31}"#,
        r#"{"seconds":3600}"#,
        r#"{"seconds":4294967295}"#,
        r#"{"seconds":4294967296}"#,
        r#"{"seconds":-10}"#,
        r#"{"seconds":10.5}"#,
        r#"{"seconds":"10"}"#,
        r#"{"seconds":null}"#,
        r#"{"seconds":[10]}"#,
        r#"{}"#,
        r#"[]"#,
        r#""#,
        r#"10"#,
        r#"{"seconds":10,"seconds":20}"#,
        r#"{"seconds":10,"path":"C:\\Windows\\System32\\x.etl"}"#,
        r#"{"seconds":10,"args":["-o","C:\\x"]}"#,
        r#"{"Seconds":10}"#,
    ] {
        assert!(TraceRequest::parse(json).is_err(), "accepted {json}");
    }
    // A bad length that reaches `record` directly runs nothing.
    let fake = Fake::default();
    assert!(run(&fake, 25).is_err());
    assert!(fake.calls().is_empty());
}

#[test]
fn unknown_operations_and_bad_requests_are_refused_before_anything_runs() {
    let fake = Fake::default();
    for (operation, json) in [
        ("drive_optimize", r#"{"seconds":10}"#),
        ("latency_trace2", r#"{"seconds":10}"#),
        ("Latency_trace", r#"{"seconds":10}"#),
        ("", r#"{"seconds":10}"#),
        (OPERATION, r#"{"seconds":11}"#),
        (OPERATION, r#"{"seconds":10,"etl":"C:\\x"}"#),
        (OPERATION, "not json"),
    ] {
        let result = prepare_with(&admin_request(operation, json), Box::new(fake.clone()));
        assert!(result.is_err(), "prepared {operation} {json}");
    }
    assert!(fake.calls().is_empty());
}

#[test]
fn preparing_and_previewing_runs_nothing_so_a_cancel_runs_nothing() {
    let fake = Fake::default();
    let change = prepare_with(
        &admin_request(OPERATION, r#"{"seconds":20}"#),
        Box::new(fake.clone()),
    )
    .unwrap();
    assert_eq!(change.capability(), "administrator");
    let text = change.description();
    assert!(text.starts_with("Run a 20-second latency check"));
    assert!(text.contains("Nothing on your PC is changed"));
    assert!(text.contains("deleted"));
    assert!(!text.trim().is_empty() && text.len() <= 8000 && !text.contains('\0'));
    // The SDK drops the change without calling apply when the person cancels.
    drop(change);
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

// --- the happy path --------------------------------------------------------------------------

#[test]
fn a_check_runs_the_fixed_commands_in_order_and_leaves_nothing_behind() {
    let fake = Fake::default();
    let change = prepare_with(
        &admin_request(OPERATION, r#"{"seconds":30}"#),
        Box::new(fake.clone()),
    )
    .unwrap();
    let json = change.apply().unwrap();
    fake.assert_clean();
    let etl = format!(r"{DIR}\latency.etl");
    let xml = format!(r"{DIR}\latency.xml");
    assert_eq!(
        fake.calls(),
        [
            "create_workspace".to_string(),
            "free_bytes".into(),
            format!(
                "Logman start | NT Kernel Logger | -p | Windows Kernel Trace | (dpc,isr,img) | -o | {etl} | -ct | perf | -bs | 256 | -nb | 32 | 256 | -max | 64 | -ets"
            ),
            "wait 30s".into(),
            "Logman stop | NT Kernel Logger | -ets".into(),
            "free_bytes".into(),
            format!("Tracerpt {etl} | -o | {xml} | -of | XML | -y"),
            "remove_workspace".into(),
        ]
    );
    assert_eq!(fake.0.borrow().waits, [Duration::from_secs(30)]);

    assert!(json.len() <= OUTPUT_LIMIT);
    let result = LatencyResult::parse(&json).unwrap();
    assert_eq!(result.seconds, 30);
    assert!(!result.drivers.is_empty());
    assert!(result.drivers.len() <= 10);
    assert!(result.cleanup_warning.is_none());
    assert!(result
        .summary
        .contains("not proof that the driver caused a stutter"));
    // Ranked longest first.
    for pair in result.drivers.windows(2) {
        assert!(pair[0].longest_ns >= pair[1].longest_ns);
    }
}

#[test]
fn the_result_never_carries_kernel_addresses_or_folders() {
    let fake = Fake::default();
    let json = run(&fake, 10).unwrap();
    assert!(!json.to_ascii_lowercase().contains("routine"));
    assert!(!json.contains("0x"), "{json}");
    assert!(!json.contains("\\\\"), "a path reached the result: {json}");
    let result = LatencyResult::parse(&json).unwrap();
    for d in &result.drivers {
        assert!(
            !d.name.contains('\\') && !d.name.contains('/'),
            "{}",
            d.name
        );
    }
}

// --- refusal and failure paths ---------------------------------------------------------------

#[test]
fn a_folder_that_cannot_be_made_runs_nothing() {
    let fake = Fake::with(|s| s.workspace_fails = true);
    assert!(run(&fake, 10).unwrap_err().contains("Nothing was recorded"));
    assert_eq!(fake.calls(), ["create_workspace"]);
}

#[test]
fn too_little_free_space_never_starts_the_recording() {
    for free in [Some(1 << 30), None] {
        let fake = Fake::with(|s| s.free = VecDeque::from([free]));
        let err = run(&fake, 10).unwrap_err();
        assert!(err.contains("Nothing was recorded"), "{err}");
        assert_eq!(fake.count("Logman"), 0);
        fake.assert_clean();
    }
}

#[test]
fn a_failed_start_stops_nothing() {
    for (start, words) in [
        (
            exit(1),
            "could not start the recording (code 1): Something went wrong.",
        ),
        (exit(183), "Another program is already recording"),
        (
            exit(0x8007_00B7_u32 as i32),
            "Another program is already recording",
        ),
        (
            Run::NotStarted("file not found".into()),
            "could not be started (file not found)",
        ),
    ] {
        let fake = Fake::with(|s| s.start = start);
        let err = run(&fake, 10).unwrap_err();
        assert!(err.contains(words), "{err}");
        assert!(err.contains("Nothing was recorded"), "{err}");
        assert_eq!(fake.count("Logman stop"), 0);
        assert_eq!(fake.count("Tracerpt"), 0);
        fake.assert_clean();
    }
}

#[test]
fn another_programs_kernel_session_is_left_alone() {
    // Someone else holds the single NT Kernel Logger: we must not stop it.
    let fake = Fake::with(|s| s.session = true);
    let err = run(&fake, 10).unwrap_err();
    assert!(err.contains("Another program"), "{err}");
    assert_eq!(fake.count("Logman stop"), 0);
    assert!(
        fake.0.borrow().session,
        "the other program's session was stopped"
    );
}

#[test]
fn a_start_that_timed_out_is_stopped() {
    for ran in [true, false] {
        let fake = Fake::with(|s| {
            s.start = Run::TimedOut;
            s.start_timed_out_but_ran = ran;
        });
        let err = run(&fake, 10).unwrap_err();
        assert!(
            err.contains("in time") && err.contains("Nothing is left running"),
            "{err}"
        );
        assert!(fake.count("Logman stop") >= 1);
        assert_eq!(fake.count("Tracerpt"), 0);
        fake.assert_clean();
    }
}

#[test]
fn a_stop_that_fails_once_is_retried() {
    let fake = Fake::with(|s| s.stops = VecDeque::from([false]));
    run(&fake, 10).unwrap();
    assert_eq!(fake.count("Logman stop"), 2);
    fake.assert_clean();
}

#[test]
fn a_stop_that_keeps_failing_is_reported_and_files_are_still_removed() {
    let fake = Fake::with(|s| s.stops = VecDeque::from([false, false, false]));
    let err = run(&fake, 10).unwrap_err();
    assert!(err.contains("could not stop the kernel recording"), "{err}");
    assert!(err.contains("64 MB"), "{err}");
    assert_eq!(fake.count("Logman stop"), STOP_ATTEMPTS);
    assert_eq!(fake.count("Logman query"), 1);
    assert_eq!(fake.count("Tracerpt"), 0);
    let s = fake.0.borrow();
    assert!(s.files.is_empty());
    assert!(s.workspace.is_none());
}

#[test]
fn a_session_ended_by_someone_else_is_reported_plainly() {
    let fake = Fake::with(|s| {
        s.stops = VecDeque::from([false]);
        s.stop_failure_ends_session = true;
    });
    let err = run(&fake, 10).unwrap_err();
    assert!(err.contains("ended by another program"), "{err}");
    fake.assert_clean();
}

#[test]
fn an_empty_recording_is_refused() {
    let fake = Fake::with(|s| s.etl_bytes = 0);
    assert!(run(&fake, 10).unwrap_err().contains("saved no recording"));
    assert_eq!(fake.count("Tracerpt"), 0);
    fake.assert_clean();
}

#[test]
fn too_little_space_to_convert_is_refused_after_stopping() {
    let fake = Fake::with(|s| s.free = VecDeque::from([Some(100 << 30), Some(1 << 20)]));
    let err = run(&fake, 10).unwrap_err();
    assert!(err.contains("not enough free space"), "{err}");
    assert_eq!(fake.count("Logman stop"), 1);
    assert_eq!(fake.count("Tracerpt"), 0);
    fake.assert_clean();
}

#[test]
fn a_failed_conversion_is_cleaned_up() {
    for (tracerpt, words) in [
        (exit(5), "could not convert the recording (code 5)"),
        (Run::TimedOut, "took too long"),
        (
            Run::NotStarted("denied".into()),
            "could not be started (denied)",
        ),
    ] {
        let fake = Fake::with(|s| s.tracerpt = tracerpt);
        let err = run(&fake, 10).unwrap_err();
        assert!(err.contains(words), "{err}");
        fake.assert_clean();
    }
    let fake = Fake::with(|s| s.tracerpt_writes_xml = false);
    assert!(run(&fake, 10)
        .unwrap_err()
        .contains("no readable recording"));
    fake.assert_clean();
}

#[test]
fn a_recording_that_cannot_be_read_is_cleaned_up() {
    let fake = Fake::with(|s| s.analyze_fails = true);
    assert!(run(&fake, 10).unwrap_err().contains("could not be read"));
    fake.assert_clean();
}

#[test]
fn garbage_xml_gives_an_honest_empty_result() {
    let fake = Fake::with(|s| s.xml = b"<Events><Event>garbage</Event>\0\xff");
    let result = LatencyResult::parse(&run(&fake, 10).unwrap()).unwrap();
    assert!(result.drivers.is_empty());
    assert!(result.summary.contains("no driver can be named"));
    fake.assert_clean();
}

#[test]
fn files_that_cannot_be_deleted_are_reported_not_hidden() {
    let fake = Fake::with(|s| s.remove_fails = true);
    let result = LatencyResult::parse(&run(&fake, 10).unwrap()).unwrap();
    let warning = result.cleanup_warning.unwrap();
    assert!(warning.contains("could not be deleted"), "{warning}");
    assert!(!fake.0.borrow().session);

    let fake = Fake::with(|s| {
        s.remove_fails = true;
        s.analyze_fails = true;
    });
    let err = run(&fake, 10).unwrap_err();
    assert!(
        err.contains("could not be read") && err.contains("could not be deleted"),
        "{err}"
    );
}

#[test]
fn a_result_over_the_limit_is_trimmed_then_refused() {
    let report = latency_trace::analyze_reader(BASIC, &limits()).unwrap();
    let full = encode(&report, 10, None, OUTPUT_LIMIT).unwrap();
    let parsed = LatencyResult::parse(&full).unwrap();
    assert!(parsed.drivers.iter().any(|d| !d.worst.is_empty()));

    // Just too big: the worst-run lists go first.
    let trimmed =
        LatencyResult::parse(&encode(&report, 10, None, full.len() - 1).unwrap()).unwrap();
    assert!(trimmed.drivers.iter().all(|d| d.worst.is_empty()));
    assert_eq!(trimmed.drivers.len(), parsed.drivers.len());

    // Smaller still: drivers from the bottom, counted as not shown.
    let no_worst = serde_json::to_string(&trimmed).unwrap().len();
    let fewer = LatencyResult::parse(&encode(&report, 10, None, no_worst - 1).unwrap()).unwrap();
    assert_eq!(fewer.drivers.len(), parsed.drivers.len() - 1);
    assert_eq!(fewer.drivers_not_shown, parsed.drivers_not_shown + 1);
    assert_eq!(fewer.drivers[..], trimmed.drivers[..fewer.drivers.len()]);

    // Too small for even the summary: refused.
    let err = encode(&report, 10, None, 64).unwrap_err();
    assert!(err.contains("too large"), "{err}");
}

#[test]
fn the_drawing_side_refuses_unreadable_or_oversized_results() {
    assert!(LatencyResult::parse("").is_err());
    assert!(LatencyResult::parse("{}").is_err());
    assert!(LatencyResult::parse(&" ".repeat(OUTPUT_LIMIT + 1)).is_err());
    let good = run(&Fake::default(), 10).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&good).unwrap();
    let mut one = value["drivers"][0].clone();
    one["worst"] = serde_json::json!([]);
    value["drivers"] = serde_json::Value::Array(vec![one; latency_trace::MAX_DRIVERS + 1]);
    value["summary"] = "".into();
    let many = serde_json::to_string(&value).unwrap();
    assert!(many.len() <= OUTPUT_LIMIT);
    assert!(LatencyResult::parse(&many).is_err());
}

#[test]
fn tool_output_in_messages_is_one_short_clean_line() {
    let noisy = format!("first\r\n\x1b[31m{}\u{7}\r\n\r\n", "x".repeat(500));
    let line = short_output(&noisy);
    assert_eq!(line.chars().count(), 200);
    assert!(line.chars().all(|c| !c.is_control()));
}

#[cfg(windows)]
#[test]
fn program_output_is_kept_to_a_bounded_tail() {
    let bytes: Vec<u8> = (0..20_000u32).map(|i| b'a' + (i % 26) as u8).collect();
    let tail = read_tail(bytes.as_slice());
    assert_eq!(tail.len(), 4096);
    assert!(bytes.ends_with(tail.as_bytes()));
}
