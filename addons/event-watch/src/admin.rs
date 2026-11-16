//! The latency check's administrator step (Gusseti/toolboks#59).
//!
//! It runs only in the separate administrator helper, after the person confirmed in the SDK's
//! own window. The drawing process never runs any of this. The request carries one number, the
//! recording length, from a fixed list; every program path, argument and file path is built
//! here. The sequence:
//!
//! 1. Create a fresh folder under `%windir%\Temp` that only SYSTEM and Administrators can open,
//!    and hold it open so it cannot be renamed or swapped for a link while the check runs.
//! 2. `logman start "NT Kernel Logger"` with DPC, interrupt and image-load events, the
//!    performance-counter clock and a size cap on the file.
//! 3. Wait the chosen time, then `logman stop "NT Kernel Logger"`.
//! 4. `tracerpt <etl> -o <xml> -of XML -y`, then read the XML with `latency-trace`.
//! 5. Delete both files and the folder, and return the ranking as JSON (at most 32 KiB).
//!
//! The kernel session is stopped on every path after a successful start. When the start itself
//! fails, nothing is stopped: Windows has a single NT Kernel Logger, and a failed start usually
//! means another program owns it.
//!
//! Microsoft documentation behind the commands:
//! - logman create trace (the options `logman start ... -ets` takes: `-p`, `-o`, `-ct`, `-bs`,
//!   `-nb`, `-max`): <https://learn.microsoft.com/windows-server/administration/windows-commands/logman-create-trace>
//! - logman start and stop: <https://learn.microsoft.com/windows-server/administration/windows-commands/logman-start-stop>
//!   (`logman -?` on Windows 11 also lists the example
//!   `logman start "NT Kernel Logger" -p "Windows Kernel Trace" (process,thread) -ets`).
//! - Kernel flags `img` 0x4, `dpc` 0x20, `isr` 0x40 (EVENT_TRACE_FLAG_IMAGE_LOAD, _DPC,
//!   _INTERRUPT): <https://learn.microsoft.com/windows/win32/api/evntrace/ns-evntrace-event_trace_properties>;
//!   the keyword names were checked with `logman query providers "Windows Kernel Trace"`.
//! - Only one NT Kernel Logger session exists; starting it while in use fails with
//!   ERROR_ALREADY_EXISTS: <https://learn.microsoft.com/windows/win32/etw/configuring-and-starting-the-nt-kernel-logger-session>
//! - Use the performance-counter clock and bigger buffers for DPC/ISR timing:
//!   <https://learn.microsoft.com/windows-hardware/drivers/devtest/example-15--measuring-dpc-isr-time>
//! - tracerpt `-o`, `-of XML`, `-y`: <https://learn.microsoft.com/windows-server/administration/windows-commands/tracerpt_1>

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use latency_trace::{Limits, Report};
use serde::{Deserialize, Serialize};
use toolboks_addon_sdk::admin::AdminChange;
use toolboks_addon_sdk::AdminRequest;

/// The one administrator operation Event Watch knows.
pub const OPERATION: &str = "latency_trace";
/// The capability the change asks for. It is in the manifest; the host shows it at install.
pub const CAPABILITY: &str = "administrator";
/// The only recording lengths accepted, in seconds.
pub const DURATIONS: [u32; 3] = [10, 20, 30];
/// The largest result handed back to the drawing process.
pub const OUTPUT_LIMIT: usize = 32 * 1024;

/// The kernel session name (there is only one).
const SESSION: &str = "NT Kernel Logger";
const ETL_NAME: &str = "latency.etl";
const XML_NAME: &str = "latency.xml";
/// Cap on the recording file (MB), so even a session that is never stopped cannot fill the
/// disk: a sequential log stops writing at this size (EVENT_TRACE_FILE_MODE_SEQUENTIAL,
/// <https://learn.microsoft.com/windows/win32/etw/logging-mode-constants>).
const ETL_MAX_MB: u32 = 64;
/// Free space needed before the recording starts.
const MIN_FREE_BYTES: u64 = 4 << 30;
/// The XML text is many times larger than the binary recording.
const XML_PER_ETL_BYTE: u64 = 25;
const CONVERT_MARGIN_BYTES: u64 = 512 << 20;
const LOGMAN_TIMEOUT: Duration = Duration::from_secs(60);
const TRACERPT_TIMEOUT: Duration = Duration::from_secs(600);
/// Stop is tried this many times, one second apart.
const STOP_ATTEMPTS: usize = 3;

/// The whole request: a length from [`DURATIONS`]. Nothing else is accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceRequest {
    pub seconds: u32,
}

impl TraceRequest {
    pub fn parse(json: &str) -> Result<Self, String> {
        let request: Self = serde_json::from_str(json)
            .map_err(|_| "The latency check request is not valid.".to_string())?;
        request.check()?;
        Ok(request)
    }

    fn check(&self) -> Result<(), String> {
        if DURATIONS.contains(&self.seconds) {
            Ok(())
        } else {
            Err("The latency check can only record for 10, 20 or 30 seconds.".into())
        }
    }

    pub fn to_json(self) -> String {
        format!("{{\"seconds\":{}}}", self.seconds)
    }
}

/// One of the longest runs of a driver. No routine address: a kernel address must not be shown.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunLine {
    /// "DPC", "threaded DPC", "timer DPC" or "ISR".
    pub kind: String,
    pub cpu: u32,
    /// When the run started, from the start of the recording.
    pub at_ms: u64,
    /// Own time of the run.
    pub own_ns: u64,
}

/// One driver in the ranking.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DriverLine {
    /// The file name only (for example `ndis.sys`), or a plain description of an unknown bucket.
    pub name: String,
    /// False for code that could not be tied to a driver.
    pub known: bool,
    /// Longest single run of its own code (the ranked measure).
    pub longest_ns: u64,
    /// Elapsed time of that same run, interrupts included, when known.
    pub elapsed_ns: Option<u64>,
    pub dpc_runs: u64,
    pub isr_runs: u64,
    /// Longer than Microsoft's guidance (DPC 100 µs, ISR 25 µs).
    pub above_guideline: bool,
    pub worst: Vec<RunLine>,
}

/// What the helper returns to the drawing process. Held in memory only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LatencyResult {
    pub seconds: u32,
    pub recorded_ms: u64,
    pub dpc_runs: u64,
    pub isr_runs: u64,
    pub drivers: Vec<DriverLine>,
    pub drivers_not_shown: usize,
    /// Share of measured time matched to a driver, 0 to 100.
    pub coverage_percent: Option<f64>,
    pub events_lost: u64,
    /// The plain-language explanation from `latency-trace`.
    pub summary: String,
    /// Set when a temporary file or the folder could not be deleted.
    pub cleanup_warning: Option<String>,
}

impl LatencyResult {
    /// Reads a helper result in the drawing process; the text is data, so it is bounded.
    pub fn parse(json: &str) -> Result<Self, String> {
        let bad = || "The administrator helper returned an unreadable latency result.".to_string();
        if json.len() > OUTPUT_LIMIT {
            return Err(bad());
        }
        let result: Self = serde_json::from_str(json).map_err(|_| bad())?;
        if result.drivers.len() > latency_trace::MAX_DRIVERS
            || result
                .drivers
                .iter()
                .any(|d| d.worst.len() > latency_trace::MAX_WORST)
        {
            return Err(bad());
        }
        Ok(result)
    }
}

/// The ranking settings for this check: a short list fits the page and the size limit.
fn limits() -> Limits {
    Limits {
        max_drivers: 10,
        worst_per_driver: 3,
        ..Limits::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Commands (fixed literals; only the helper-built file paths are added)
// ---------------------------------------------------------------------------------------------

/// The two Windows programs used, always from System32.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Logman,
    Tracerpt,
}

impl Tool {
    fn file_name(self) -> &'static str {
        match self {
            Tool::Logman => "logman.exe",
            Tool::Tracerpt => "tracerpt.exe",
        }
    }
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn start_args(etl: &Path) -> Vec<OsString> {
    let mut args = os(&[
        "start",
        SESSION,
        "-p",
        "Windows Kernel Trace",
        "(dpc,isr,img)",
        "-o",
    ]);
    args.push(etl.as_os_str().to_owned());
    args.extend(os(&[
        "-ct", "perf", "-bs", "256", "-nb", "32", "256", "-max",
    ]));
    args.push(ETL_MAX_MB.to_string().into());
    args.push("-ets".into());
    args
}

fn stop_args() -> Vec<OsString> {
    os(&["stop", SESSION, "-ets"])
}

fn query_args() -> Vec<OsString> {
    os(&["query", SESSION, "-ets"])
}

fn convert_args(etl: &Path, xml: &Path) -> Vec<OsString> {
    let mut args = vec![etl.as_os_str().to_owned()];
    args.push("-o".into());
    args.push(xml.as_os_str().to_owned());
    args.extend(os(&["-of", "XML", "-y"]));
    args
}

// ---------------------------------------------------------------------------------------------
// The sequence, behind a trait so tests drive it with a fake
// ---------------------------------------------------------------------------------------------

/// How a program run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Run {
    Exited {
        code: i32,
        output: String,
    },
    /// Killed after the time limit.
    TimedOut,
    /// Could not be started.
    NotStarted(String),
}

impl Run {
    fn ok(&self) -> bool {
        matches!(self, Run::Exited { code: 0, .. })
    }
}

/// Everything the check does to the PC. The real one is [`WindowsTrace`].
pub trait TraceSystem {
    /// Creates a new private working folder and keeps it from being renamed until
    /// [`TraceSystem::remove_workspace`].
    fn create_workspace(&mut self) -> Result<PathBuf, String>;
    fn free_bytes(&mut self, dir: &Path) -> Option<u64>;
    fn run(&mut self, tool: Tool, args: &[OsString], cwd: &Path, timeout: Duration) -> Run;
    fn wait(&mut self, duration: Duration);
    /// Size of a regular file; `None` when it is missing or not a plain file.
    fn file_size(&mut self, path: &Path) -> Option<u64>;
    fn analyze(&mut self, xml: &Path, limits: &Limits) -> io::Result<Report>;
    fn remove_file(&mut self, path: &Path) -> io::Result<()>;
    fn remove_workspace(&mut self, dir: &Path) -> io::Result<()>;
}

fn stop_failed() -> String {
    format!(
        "Windows could not stop the kernel recording. It ends by itself when the PC restarts, \
         and its file cannot grow past {ETL_MAX_MB} MB. Restart the PC to be sure it has ended."
    )
}

/// Runs the whole check and returns the result JSON, or a plain reason.
pub fn record(sys: &mut dyn TraceSystem, request: TraceRequest) -> Result<String, String> {
    request.check()?;
    let dir = sys.create_workspace()?;
    let etl = dir.join(ETL_NAME);
    let xml = dir.join(XML_NAME);
    let report = record_in(sys, &dir, &etl, &xml, request.seconds);
    let leftover = cleanup(sys, &dir, &[&etl, &xml]).err();
    match report {
        Ok(report) => encode(&report, request.seconds, leftover, OUTPUT_LIMIT),
        Err(reason) => Err(match leftover {
            Some(left) => format!("{reason} {left}"),
            None => reason,
        }),
    }
}

fn record_in(
    sys: &mut dyn TraceSystem,
    dir: &Path,
    etl: &Path,
    xml: &Path,
    seconds: u32,
) -> Result<Report, String> {
    match sys.free_bytes(dir) {
        Some(free) if free >= MIN_FREE_BYTES => {}
        Some(_) => {
            return Err(
                "There is not enough free space on the Windows drive for the latency check \
                 (it needs about 4 GB while it runs). Nothing was recorded."
                    .into(),
            )
        }
        None => {
            return Err(
                "Free space on the Windows drive could not be checked. Nothing was recorded."
                    .into(),
            )
        }
    }

    match sys.run(Tool::Logman, &start_args(etl), dir, LOGMAN_TIMEOUT) {
        Run::Exited { code: 0, .. } => {}
        Run::Exited { code, output } => return Err(start_failed(code, &output)),
        Run::NotStarted(e) => {
            return Err(format!(
                "Windows' recording tool could not be started ({e}). Nothing was recorded."
            ))
        }
        Run::TimedOut => {
            // It may have started anyway: stop it before giving up.
            return Err(match stop(sys, dir) {
                Err(Stop::StillRunning) => stop_failed(),
                Ok(()) | Err(Stop::NotRunning) => {
                    "Windows did not start the recording in time. Nothing is left running.".into()
                }
            });
        }
    }

    sys.wait(Duration::from_secs(u64::from(seconds)));
    stop(sys, dir).map_err(|stop| match stop {
        Stop::StillRunning => stop_failed(),
        Stop::NotRunning => "The recording was ended by another program before the check \
                             finished. Nothing is left running. Try again."
            .to_string(),
    })?;

    let size = sys
        .file_size(etl)
        .filter(|s| *s > 0)
        .ok_or("Windows saved no recording. Try again.")?;
    let needed = size
        .saturating_mul(XML_PER_ETL_BYTE)
        .saturating_add(CONVERT_MARGIN_BYTES);
    if sys.free_bytes(dir).is_none_or(|free| free < needed) {
        return Err(
            "There is not enough free space on the Windows drive to read the recording. \
             It was deleted."
                .into(),
        );
    }
    match sys.run(
        Tool::Tracerpt,
        &convert_args(etl, xml),
        dir,
        TRACERPT_TIMEOUT,
    ) {
        Run::Exited { code: 0, .. } => {}
        Run::Exited { code, .. } => {
            return Err(format!(
                "Windows could not convert the recording (code {code}). It was deleted."
            ))
        }
        Run::TimedOut => {
            return Err(
                "Converting the recording took too long and was stopped. It was deleted.".into(),
            )
        }
        Run::NotStarted(e) => {
            return Err(format!(
                "Windows' conversion tool could not be started ({e}). The recording was deleted."
            ))
        }
    }
    // Free the space early; the clean-up below tries again if this fails.
    let _ = sys.remove_file(etl);
    if sys.file_size(xml).is_none() {
        return Err("Windows produced no readable recording. It was deleted.".into());
    }
    sys.analyze(xml, &limits())
        .map_err(|_| "The recording could not be read. It was deleted.".to_string())
}

/// Why a stop did not succeed.
#[derive(Debug, PartialEq, Eq)]
enum Stop {
    /// The session is still running after every attempt.
    StillRunning,
    /// No kernel session is running (it never started, or something else ended it).
    NotRunning,
}

/// Stops the session, retrying; on failure asks Windows whether it is still running.
fn stop(sys: &mut dyn TraceSystem, dir: &Path) -> Result<(), Stop> {
    for attempt in 0..STOP_ATTEMPTS {
        if attempt > 0 {
            sys.wait(Duration::from_secs(1));
        }
        if sys
            .run(Tool::Logman, &stop_args(), dir, LOGMAN_TIMEOUT)
            .ok()
        {
            return Ok(());
        }
    }
    if sys
        .run(Tool::Logman, &query_args(), dir, LOGMAN_TIMEOUT)
        .ok()
    {
        Err(Stop::StillRunning)
    } else {
        Err(Stop::NotRunning)
    }
}

/// ERROR_ALREADY_EXISTS (183), as a plain code or as an HRESULT.
fn already_running(code: i32) -> bool {
    code == 183 || code as u32 == 0x8007_00B7
}

fn start_failed(code: i32, output: &str) -> String {
    if already_running(code) {
        return "Another program is already recording kernel events (for example LatencyMon, \
                Windows Performance Recorder or Process Explorer). Close it and try again. \
                Nothing was recorded."
            .into();
    }
    let said = short_output(output);
    if said.is_empty() {
        format!("Windows could not start the recording (code {code}). Nothing was recorded.")
    } else {
        format!("Windows could not start the recording (code {code}): {said} Nothing was recorded.")
    }
}

/// The last line of a tool's output, without control characters, at most 200 characters.
fn short_output(output: &str) -> String {
    output
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect()
}

fn cleanup(sys: &mut dyn TraceSystem, dir: &Path, files: &[&Path]) -> Result<(), String> {
    let mut failed = false;
    for file in files {
        match sys.remove_file(file) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => failed = true,
        }
    }
    if sys.remove_workspace(dir).is_err() {
        failed = true;
    }
    if failed {
        Err(format!(
            "Some temporary recording files could not be deleted from {}. Windows Disk Cleanup \
             (Temporary files) removes them.",
            dir.display()
        ))
    } else {
        Ok(())
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Turns the report into the result JSON. Over `limit`, the worst-run lists go first, then
/// drivers from the bottom; if it still does not fit, the check fails.
fn encode(
    report: &Report,
    seconds: u32,
    cleanup_warning: Option<String>,
    limit: usize,
) -> Result<String, String> {
    let drivers = report
        .drivers
        .iter()
        .map(|d| DriverLine {
            name: d.module.to_string(),
            known: matches!(d.module, latency_trace::ModuleName::Driver(_)),
            longest_ns: nanos(d.max_exclusive()),
            elapsed_ns: d.longest_run().map(|w| nanos(w.inclusive)),
            dpc_runs: d.dpc.count,
            isr_runs: d.isr.count,
            above_guideline: d.above_guideline(),
            worst: d
                .worst
                .iter()
                .map(|w| RunLine {
                    kind: w.kind.label().into(),
                    cpu: w.cpu,
                    at_ms: millis(w.at),
                    own_ns: nanos(w.exclusive),
                })
                .collect(),
        })
        .collect();
    let mut result = LatencyResult {
        seconds,
        recorded_ms: millis(report.trace_length),
        dpc_runs: report.dpc_runs,
        isr_runs: report.isr_runs,
        drivers,
        drivers_not_shown: report.drivers_not_shown,
        coverage_percent: report.coverage.share().map(|s| s * 100.0),
        events_lost: report.losses.map_or(0, |l| l.events_lost),
        summary: report.to_plain_text(),
        cleanup_warning,
    };
    let fits = |r: &LatencyResult| {
        serde_json::to_string(r)
            .ok()
            .filter(|json| json.len() <= limit)
    };
    if let Some(json) = fits(&result) {
        return Ok(json);
    }
    for d in &mut result.drivers {
        d.worst.clear();
    }
    loop {
        if let Some(json) = fits(&result) {
            return Ok(json);
        }
        if result.drivers.pop().is_none() {
            return Err(
                "The latency result was too large to show. The recording was deleted.".into(),
            );
        }
        result.drivers_not_shown = result.drivers_not_shown.saturating_add(1);
    }
}

// ---------------------------------------------------------------------------------------------
// The administrator change
// ---------------------------------------------------------------------------------------------

struct PreparedTrace {
    request: TraceRequest,
    system: Box<dyn TraceSystem>,
}

/// Validates the request for the helper. Read-only: it runs no program and touches no file.
pub fn prepare_admin(request: &AdminRequest) -> Result<Box<dyn AdminChange>, String> {
    prepare_with(request, Box::new(WindowsTrace::default()))
}

fn prepare_with(
    request: &AdminRequest,
    system: Box<dyn TraceSystem>,
) -> Result<Box<dyn AdminChange>, String> {
    if request.operation != OPERATION {
        return Err("Event Watch does not know this administrator action.".into());
    }
    let request = TraceRequest::parse(&request.json)?;
    Ok(Box::new(PreparedTrace { request, system }))
}

impl AdminChange for PreparedTrace {
    fn capability(&self) -> &str {
        CAPABILITY
    }

    fn description(&self) -> String {
        let n = self.request.seconds;
        format!(
            "Run a {n}-second latency check\n\n\
             Windows will record how long each driver's interrupt and deferred work (ISR and DPC) \
             runs, for about {n} seconds, then convert and read the recording. The processor is \
             a little busier while it records.\n\n\
             Nothing on your PC is changed. The recording files are deleted as soon as they have \
             been read, and the result is kept only in Event Watch until you close it."
        )
    }

    fn apply(self: Box<Self>) -> Result<String, String> {
        let PreparedTrace {
            request,
            mut system,
        } = *self;
        record(system.as_mut(), request)
    }
}

// ---------------------------------------------------------------------------------------------
// The real system
// ---------------------------------------------------------------------------------------------

/// The real Windows side. Creating it does nothing; work starts in [`record`].
#[derive(Default)]
pub struct WindowsTrace {
    #[cfg(windows)]
    dir: Option<windows::Win32::Foundation::HANDLE>,
}

#[cfg(windows)]
impl Drop for WindowsTrace {
    fn drop(&mut self) {
        self.close_dir();
    }
}

#[cfg(windows)]
impl WindowsTrace {
    fn close_dir(&mut self) {
        if let Some(handle) = self.dir.take() {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
        }
    }
}

#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
fn windows_dir(system32: bool) -> Result<PathBuf, String> {
    use windows::Win32::System::SystemInformation::{
        GetSystemDirectoryW, GetSystemWindowsDirectoryW,
    };
    let mut buf = [0u16; 32768];
    let len = unsafe {
        if system32 {
            GetSystemDirectoryW(Some(&mut buf))
        } else {
            GetSystemWindowsDirectoryW(Some(&mut buf))
        }
    } as usize;
    if len == 0 || len >= buf.len() {
        return Err("The Windows folder could not be found. Nothing was recorded.".into());
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// A real folder, not a link or junction.
#[cfg(windows)]
fn plain_dir(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    std::fs::symlink_metadata(path)
        .is_ok_and(|m| m.is_dir() && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
}

#[cfg(windows)]
impl TraceSystem for WindowsTrace {
    fn create_workspace(&mut self) -> Result<PathBuf, String> {
        use windows::core::{w, PCWSTR};
        use windows::Win32::Foundation::{
            GetLastError, LocalFree, ERROR_ALREADY_EXISTS, HLOCAL, INVALID_HANDLE_VALUE,
        };
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
        use windows::Win32::Storage::FileSystem::{
            CreateDirectoryW, CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        };

        let refused = |what: &str| format!("{what} Nothing was recorded.");
        self.close_dir();
        let temp = windows_dir(false)?.join("Temp");
        if !plain_dir(&temp) {
            return Err(refused("The Windows Temp folder is missing or is a link."));
        }
        // Only SYSTEM and Administrators, not inherited from the Temp folder (where any user may
        // create entries). Files created inside inherit the same two entries.
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .map_err(|_| refused("The private folder's permissions could not be prepared."))?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let mut made = None;
        for attempt in 0..16u64 {
            let name = format!(
                "toolboks-latency-{:08x}-{:016x}",
                std::process::id(),
                seed.rotate_left(attempt as u32 * 7) ^ attempt
            );
            let path = temp.join(name);
            let w = wide(&path);
            // Fails when the name exists, so a folder made by someone else is never used.
            if unsafe { CreateDirectoryW(PCWSTR(w.as_ptr()), Some(&attributes)) }.is_ok() {
                made = Some(path);
                break;
            }
            if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
                break;
            }
        }
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        let path = made.ok_or_else(|| refused("A private working folder could not be made."))?;

        // Hold the folder open without delete sharing: nobody can rename or replace it while
        // the check runs. The handle is checked to be the real folder, not a link.
        let w = wide(&path);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        };
        let handle = match handle {
            Ok(h) if h != INVALID_HANDLE_VALUE => h,
            _ => {
                let _ = std::fs::remove_dir(&path);
                return Err(refused("The private working folder could not be opened."));
            }
        };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        let plain = unsafe { GetFileInformationByHandle(handle, &mut info) }.is_ok()
            && info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0
            && info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0;
        self.dir = Some(handle);
        if !plain {
            self.close_dir();
            return Err(refused("The private working folder is not a plain folder."));
        }
        Ok(path)
    }

    fn free_bytes(&mut self, dir: &Path) -> Option<u64> {
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let w = wide(dir);
        let mut free = 0u64;
        unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), None, None) }
            .ok()
            .map(|_| free)
    }

    fn run(&mut self, tool: Tool, args: &[OsString], cwd: &Path, timeout: Duration) -> Run {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let exe = match windows_dir(true) {
            Ok(dir) => dir.join(tool.file_name()),
            Err(e) => return Run::NotStarted(e),
        };
        let mut command = Command::new(exe);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => return Run::NotStarted(e.to_string()),
        };
        let readers: Vec<_> = [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn io::Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        .map(|stream| std::thread::spawn(move || read_tail(stream)))
        .collect();
        let started = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if started.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let output: String = readers
            .into_iter()
            .filter_map(|r| r.join().ok())
            .collect::<Vec<_>>()
            .join("\n");
        match status {
            Some(status) => Run::Exited {
                code: status.code().unwrap_or(-1),
                output,
            },
            None => Run::TimedOut,
        }
    }

    fn wait(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn file_size(&mut self, path: &Path) -> Option<u64> {
        std::fs::symlink_metadata(path)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len())
    }

    fn analyze(&mut self, xml: &Path, limits: &Limits) -> io::Result<Report> {
        latency_trace::analyze_file(xml, limits)
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn remove_workspace(&mut self, dir: &Path) -> io::Result<()> {
        // Close our handle first: it denies delete sharing, which would block the removal.
        self.close_dir();
        std::fs::remove_dir(dir)
    }
}

/// Keeps the last 4 KiB a program wrote, so a chatty program cannot fill memory.
#[cfg(windows)]
fn read_tail(mut stream: impl io::Read) -> String {
    const KEEP: usize = 4096;
    let mut kept = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                kept.extend_from_slice(&chunk[..n]);
                if kept.len() > KEEP {
                    kept.drain(..kept.len() - KEEP);
                }
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

#[cfg(not(windows))]
impl TraceSystem for WindowsTrace {
    fn create_workspace(&mut self) -> Result<PathBuf, String> {
        Err("The latency check needs Windows.".into())
    }
    fn free_bytes(&mut self, _: &Path) -> Option<u64> {
        None
    }
    fn run(&mut self, _: Tool, _: &[OsString], _: &Path, _: Duration) -> Run {
        Run::NotStarted("The latency check needs Windows.".into())
    }
    fn wait(&mut self, _: Duration) {}
    fn file_size(&mut self, _: &Path) -> Option<u64> {
        None
    }
    fn analyze(&mut self, _: &Path, _: &Limits) -> io::Result<Report> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn remove_file(&mut self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn remove_workspace(&mut self, _: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
