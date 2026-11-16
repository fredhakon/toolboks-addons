//! The latency check's administrator step against a fake system. Nothing here starts a program,
//! records a trace or asks for administrator rights. The fake keeps the state that matters: is
//! the kernel session running, which files exist, does the working folder exist.

use super::*;
use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::rc::Rc;

const BASIC: &[u8] = include_bytes!("../../../../crates/latency-trace/tests/fixtures/basic.xml");
const DIR: &str = r"C:\Windows\Temp\toolboks-latency-0000162c-5eed";
/// Where another program's kernel session writes.
const OTHER: &str = r"C:\PerfLogs\Admin\other-tool.etl";

struct State {
    calls: Vec<String>,
    waits: Vec<Duration>,
    /// The running kernel session's output file (ours, or another program's), if any.
    session: Option<String>,
    /// During the recording wait, the session is replaced by this (another program stops ours
    /// and starts its own, or just stops ours).
    during_wait: Option<Option<String>>,
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
    /// Replaces the answer of `logman query`.
    query: Option<Run>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            calls: vec![],
            waits: vec![],
            session: None,
            during_wait: None,
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
            query: None,
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
    fn session(&self) -> Option<String> {
        self.0.borrow().session.clone()
    }
    /// What must hold after every run: our session ended, no files, no folder.
    fn assert_clean(&self) {
        let s = self.0.borrow();
        assert!(
            s.session.is_none(),
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
                let ours = || etl.to_string_lossy().into_owned();
                if s.start == Run::TimedOut {
                    if s.start_timed_out_but_ran && s.session.is_none() {
                        s.session = Some(ours());
                        s.files.insert(etl.clone());
                    }
                    return Run::TimedOut;
                }
                if s.session.is_some() {
                    // The single kernel logger is taken.
                    return Run::Exited {
                        code: 0x8007_00B7_u32 as i32,
                        output: "Data Collector Set already exists.".into(),
                    };
                }
                let run = s.start.clone();
                if run.ok() {
                    s.session = Some(ours());
                    if s.etl_bytes > 0 {
                        s.files.insert(etl);
                    }
                }
                run
            }
            (Tool::Logman, "stop") => {
                // Like the real one: it stops whatever NT Kernel Logger session runs.
                if s.session.is_none() {
                    return exit(0x8030_0002_u32 as i32);
                }
                if s.stops.pop_front().unwrap_or(true) {
                    s.session = None;
                    ok()
                } else {
                    if s.stop_failure_ends_session {
                        s.session = None;
                    }
                    exit(5)
                }
            }
            (Tool::Logman, "query") => {
                if let Some(run) = s.query.clone() {
                    return run;
                }
                match &s.session {
                    // The shape of real `logman query "NT Kernel Logger" -ets` output.
                    Some(path) => Run::Exited {
                        code: 0,
                        output: format!(
                            "\r\nName:                 NT Kernel Logger\r\nStatus:               \
                             Running\r\nOutput Location:      {path}\r\nLog Mode:             \
                             Sequential\r\n\r\nThe command completed successfully.\r\n"
                        ),
                    },
                    None => exit(0x8030_0002_u32 as i32),
                }
            }
            (Tool::Tracerpt, _) => {
                assert!(
                    s.session.as_deref() != Some(&*etl.to_string_lossy()),
                    "converted while still recording"
                );
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
        if let Some(next) = s.during_wait.take() {
            s.session = next;
        }
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
    assert!(text.contains("written to the Windows Temp folder (it needs about 4 GB free)"));
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
            "Logman query | NT Kernel Logger | -ets".into(),
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
    let fake = Fake::with(|s| s.session = Some(OTHER.into()));
    let err = run(&fake, 10).unwrap_err();
    assert!(err.contains("Another program"), "{err}");
    assert_eq!(fake.count("Logman stop"), 0);
    assert_eq!(
        fake.session().as_deref(),
        Some(OTHER),
        "the other program's session was stopped"
    );
}

#[test]
fn a_start_that_timed_out_is_stopped_only_if_it_is_ours() {
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
        assert_eq!(fake.count("Logman query"), 1);
        assert_eq!(fake.count("Logman stop"), usize::from(ran));
        assert_eq!(fake.count("Tracerpt"), 0);
        fake.assert_clean();
    }
}

#[test]
fn another_programs_session_after_our_start_timed_out_is_left_alone() {
    // Our start hung; the kernel logger that runs belongs to another program.
    let fake = Fake::with(|s| {
        s.start = Run::TimedOut;
        s.session = Some(OTHER.into());
    });
    let err = run(&fake, 10).unwrap_err();
    assert!(
        err.contains("in time") && err.contains("left it alone"),
        "{err}"
    );
    assert_eq!(fake.count("Logman stop"), 0);
    assert_eq!(fake.session().as_deref(), Some(OTHER));
    let s = fake.0.borrow();
    assert!(s.files.is_empty() && s.workspace.is_none());
}

#[test]
fn a_session_replaced_during_the_wait_is_left_alone() {
    // Another program stopped ours during the recording and started its own.
    let fake = Fake::with(|s| s.during_wait = Some(Some(OTHER.into())));
    let err = run(&fake, 10).unwrap_err();
    assert!(err.contains("left it alone"), "{err}");
    assert_eq!(fake.count("Logman stop"), 0);
    assert_eq!(fake.count("Tracerpt"), 0);
    assert_eq!(fake.session().as_deref(), Some(OTHER));
    let s = fake.0.borrow();
    assert!(s.files.is_empty() && s.workspace.is_none());
}

#[test]
fn ownership_needs_our_folder_name_in_the_query_output() {
    // A session whose path merely shares the prefix, or no path at all, is not ours.
    for output in [
        r"Output Location: C:\Windows\Temp\toolboks-latency-0000162c-5eee\latency.etl".to_string(),
        r"Output Location: C:\Windows\Temp\toolboks-latency-0000162c\latency.etl".to_string(),
        "Name: NT Kernel Logger\r\nStatus: Running".to_string(),
        String::new(),
    ] {
        let fake = Fake::with(|s| s.query = Some(Run::Exited { code: 0, output }));
        assert_eq!(owner(&mut fake.clone(), Path::new(DIR)), Owner::Other);
    }
    // Case does not matter.
    let output = format!("Output Location: {}\\LATENCY.ETL", DIR.to_ascii_uppercase());
    let fake = Fake::with(|s| s.query = Some(Run::Exited { code: 0, output }));
    assert_eq!(owner(&mut fake.clone(), Path::new(DIR)), Owner::Ours);
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
    assert_eq!(fake.count("Logman query"), STOP_ATTEMPTS);
    assert_eq!(fake.count("Tracerpt"), 0);
    let s = fake.0.borrow();
    assert!(s.files.is_empty());
    assert!(s.workspace.is_none());
}

#[test]
fn a_failed_stop_that_took_effect_counts_as_stopped() {
    let fake = Fake::with(|s| {
        s.stops = VecDeque::from([false]);
        s.stop_failure_ends_session = true;
    });
    run(&fake, 10).unwrap();
    assert_eq!(fake.count("Logman stop"), 1);
    fake.assert_clean();
}

#[test]
fn a_session_ended_by_someone_else_during_the_wait_is_reported_plainly() {
    let fake = Fake::with(|s| s.during_wait = Some(None));
    let err = run(&fake, 10).unwrap_err();
    assert!(err.contains("ended by another program"), "{err}");
    assert_eq!(fake.count("Logman stop"), 0);
    fake.assert_clean();
}

#[test]
fn an_unclear_query_is_never_reported_as_nothing_left_running() {
    // The query hangs, errors or cannot start: ownership is unknown, so nothing is stopped, and
    // the person is told to restart, never that nothing is left.
    for query in [Run::TimedOut, exit(5), Run::NotStarted("denied".into())] {
        let fake = Fake::with(|s| s.query = Some(query.clone()));
        let err = run(&fake, 10).unwrap_err();
        assert!(err.contains("could not stop the kernel recording"), "{err}");
        assert!(!err.contains("Nothing is left running"), "{err}");
        assert_eq!(fake.count("Logman query"), STOP_ATTEMPTS);
        assert_eq!(fake.count("Logman stop"), 0);

        let fake = Fake::with(|s| {
            s.start = Run::TimedOut;
            s.start_timed_out_but_ran = true;
            s.query = Some(query);
        });
        let err = run(&fake, 10).unwrap_err();
        assert!(err.contains("could not stop the kernel recording"), "{err}");
        assert_eq!(fake.count("Logman stop"), 0);
    }
    assert!(session_not_found(0x8030_0002_u32 as i32));
    assert!(!session_not_found(0) && !session_not_found(1) && !session_not_found(-1));
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
    // The text names the Windows Temp folder, never our folder's path.
    let no_path = |text: &str| {
        let lower = text.to_ascii_lowercase();
        assert!(!lower.contains("toolboks-latency"), "{text}");
        assert!(!lower.contains(r"c:\"), "{text}");
        assert!(!lower.contains(r"c:\\"), "{text}");
        assert!(
            !lower.contains("latency.etl") && !lower.contains("latency.xml"),
            "{text}"
        );
    };
    let fake = Fake::with(|s| s.remove_fails = true);
    let json = run(&fake, 10).unwrap();
    no_path(&json);
    let result = LatencyResult::parse(&json).unwrap();
    let warning = result.cleanup_warning.unwrap();
    assert!(
        warning.contains("could not be deleted from the Windows Temp folder"),
        "{warning}"
    );
    assert!(fake.session().is_none());

    let fake = Fake::with(|s| {
        s.remove_fails = true;
        s.analyze_fails = true;
    });
    let err = run(&fake, 10).unwrap_err();
    assert!(
        err.contains("could not be read") && err.contains("could not be deleted"),
        "{err}"
    );
    no_path(&err);
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
