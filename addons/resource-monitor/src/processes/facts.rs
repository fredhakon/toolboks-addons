//! Detail pane facts (#67): start time, command line, user, parent, threads and handles of the one
//! selected process.
//!
//! Read on a short worker thread, never on the UI thread, and read again every few seconds while
//! the process stays selected (threads and handles change). Every value has a "Not available"
//! path with a plain reason; nothing is guessed.
//!
//! # Access
//!
//! The process is opened once, with `PROCESS_QUERY_LIMITED_INFORMATION` and nothing more, whatever
//! the process is, so a game or anti-cheat process on the skip list ([`protected_processes`], #48)
//! is never opened with more access than the page already uses on every refresh. The command line
//! comes from `NtQueryInformationProcess(ProcessCommandLineInformation)`, which needs only that
//! access on Windows 8.1 and later, so no other process's memory is read. The user comes from the
//! process token. Threads and the parent id come from a process snapshot, which opens nothing.
//! Handles come from `GetProcessHandleCount`. What Windows refuses is shown as "Not available",
//! not retried with more access.
//!
//! The parent's name is the one the page already verified ([`super::Proc::parent`]): a parent
//! that has ended or whose id was reused is not shown as the parent.

use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use egui::{self, RichText};
use windows::core::PWSTR;
use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, FILETIME, HANDLE, UNICODE_STRING};
use windows::Win32::Security::{
    GetTokenInformation, LookupAccountSidW, TokenUser, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessHandleCount, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

use toolboks_shared::win::Handle;

use super::{creation_time, kv, Proc, ProcessIdentity};

/// The only access this module ever asks a process for.
pub(super) const ACCESS: windows::Win32::System::Threading::PROCESS_ACCESS_RIGHTS =
    PROCESS_QUERY_LIMITED_INFORMATION;
/// Facts are read again this often while the process stays selected.
pub(super) const REREAD: Duration = Duration::from_secs(5);
/// A command line longer than this many characters is shortened for display.
pub(super) const MAX_COMMAND_LINE_SHOWN: usize = 1000;

/// Why a value is not shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Why {
    /// The process ended before it could be read.
    Ended,
    /// The process id now belongs to another process.
    Changed,
    /// Windows refused: the process belongs to another user, runs elevated or is protected.
    NeedsAdmin,
    /// Windows gave no value and no reason.
    Unreadable,
    /// The process has no command line.
    NoCommandLine,
    /// The parent has ended, its id was reused, or it could not be verified.
    ParentUnknown,
    /// Windows gives the process no parent.
    NoParent,
}

impl Why {
    pub(super) fn text(self) -> &'static str {
        match self {
            Why::Ended => "Not available: the process has ended.",
            Why::Changed => "Not available: the process id now belongs to another process.",
            Why::NeedsAdmin => "Not available: Windows needs administrator rights for this one.",
            Why::Unreadable => "Not available: Windows did not give this value.",
            Why::NoCommandLine => "Not available: this process has no command line.",
            Why::ParentUnknown => "Not available: the parent has ended or could not be verified.",
            Why::NoParent => "Not available: Windows gives this process no parent.",
        }
    }
}

/// A value, or the reason there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Fact<T> {
    Known(T),
    Missing(Why),
}

impl<T> Fact<T> {
    fn from(result: Result<T, Why>) -> Fact<T> {
        match result {
            Ok(v) => Fact::Known(v),
            Err(why) => Fact::Missing(why),
        }
    }
}

/// What was read about one process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Facts {
    pub(super) identity: ProcessIdentity,
    /// Local date and time, from the creation time the page already holds.
    pub(super) started: Fact<String>,
    pub(super) command_line: Fact<String>,
    pub(super) user: Fact<String>,
    /// "name (pid)".
    pub(super) parent: Fact<String>,
    pub(super) threads: Fact<u32>,
    pub(super) handles: Fact<u32>,
}

impl Facts {
    /// Everything but the start time missing for one reason.
    fn all_missing(identity: ProcessIdentity, why: Why) -> Facts {
        Facts {
            identity,
            started: Fact::from(local_time(identity.created).ok_or(Why::Unreadable)),
            command_line: Fact::Missing(why),
            user: Fact::Missing(why),
            parent: Fact::Missing(why),
            threads: Fact::Missing(why),
            handles: Fact::Missing(why),
        }
    }
}

/// What reading one process needs from Windows. Fakes implement it in the tests.
pub(super) trait Os {
    fn command_line(&self) -> Result<String, Why>;
    fn user(&self) -> Result<String, Why>;
    fn handles(&self) -> Result<u32, Why>;
}

/// The process table entry of a pid: (parent pid, thread count). `None` when it is not there.
pub(super) type Entry = Option<(u32, u32)>;

/// Puts the pieces together. `open` fails with the reason when the process cannot be opened or
/// is no longer the one that was selected; `entry` is the process table entry of the pid.
pub(super) fn assemble(
    identity: ProcessIdentity,
    vouched_parent: Option<&str>,
    entry: Entry,
    open: Result<&dyn Os, Why>,
) -> Facts {
    let mut facts = Facts::all_missing(identity, Why::Unreadable);
    match open {
        Ok(os) => {
            facts.command_line = Fact::from(os.command_line());
            facts.user = Fact::from(os.user());
            facts.handles = Fact::from(os.handles());
        }
        Err(why) => {
            facts.command_line = Fact::Missing(why);
            facts.user = Fact::Missing(why);
            facts.handles = Fact::Missing(why);
        }
    }
    match entry {
        Some((ppid, threads)) => {
            facts.threads = Fact::Known(threads);
            facts.parent = match (ppid, vouched_parent) {
                (0, _) => Fact::Missing(Why::NoParent),
                (pid, Some(name)) => Fact::Known(format!("{name} ({pid})")),
                (_, None) => Fact::Missing(Why::ParentUnknown),
            };
        }
        None => {
            let why = match &facts.command_line {
                Fact::Missing(why @ (Why::Ended | Why::Changed)) => *why,
                _ => Why::Ended,
            };
            facts.threads = Fact::Missing(why);
            facts.parent = Fact::Missing(why);
        }
    }
    facts
}

/// Reads the facts of a live process.
pub(super) fn read_live(identity: ProcessIdentity, vouched_parent: Option<&str>) -> Facts {
    let entry = process_entry(identity.pid);
    match open(identity) {
        Ok(live) => assemble(identity, vouched_parent, entry, Ok(&live)),
        Err(why) => assemble(identity, vouched_parent, entry, Err(why)),
    }
}

struct Live {
    handle: Handle,
}

/// Opens the process for reading and checks it is still the one that was selected.
fn open(identity: ProcessIdentity) -> Result<Live, Why> {
    // SAFETY: Windows validates the pid. Only the limited query access is asked for.
    let handle = unsafe { OpenProcess(ACCESS, false, identity.pid) }
        .map(Handle)
        .map_err(|e| {
            if e.code() == windows::core::HRESULT::from_win32(ERROR_ACCESS_DENIED.0) {
                Why::NeedsAdmin
            } else {
                // No such process (it ended), or an id that cannot be opened.
                Why::Ended
            }
        })?;
    match creation_time(handle.0) {
        Ok(created) if created == identity.created => Ok(Live { handle }),
        Ok(_) => Err(Why::Changed),
        Err(_) => Err(Why::Unreadable),
    }
}

const ACCESS_DENIED_STATUS: i32 = 0xC000_0022_u32 as i32;
const LENGTH_MISMATCH: [i32; 3] = [
    0xC000_0004_u32 as i32,
    0x8000_0005_u32 as i32,
    0xC000_0023_u32 as i32,
];

impl Os for Live {
    fn command_line(&self) -> Result<String, Why> {
        let mut needed = 0u32;
        // SAFETY: a zero-length query only reports the size needed.
        let status = unsafe {
            NtQueryInformationProcess(
                self.handle.0,
                ProcessCommandLineInformation,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        if status.0 == ACCESS_DENIED_STATUS {
            return Err(Why::NeedsAdmin);
        }
        if !LENGTH_MISMATCH.contains(&status.0) || needed == 0 || needed > 1 << 20 {
            return Err(Why::Unreadable);
        }
        // u64 units keep the buffer aligned for the UNICODE_STRING at its start.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8) + 1];
        let capacity = (buf.len() * 8) as u32;
        let mut written = 0u32;
        // SAFETY: the buffer is `capacity` bytes and aligned for UNICODE_STRING.
        let status = unsafe {
            NtQueryInformationProcess(
                self.handle.0,
                ProcessCommandLineInformation,
                buf.as_mut_ptr().cast(),
                capacity,
                &mut written,
            )
        };
        if status.0 == ACCESS_DENIED_STATUS {
            return Err(Why::NeedsAdmin);
        }
        if status.0 < 0 {
            return Err(Why::Unreadable);
        }
        let base = buf.as_ptr() as usize;
        // SAFETY: Windows wrote a UNICODE_STRING at the start of the buffer.
        let us: UNICODE_STRING = unsafe { std::ptr::read(buf.as_ptr().cast()) };
        let at = us.Buffer.0 as usize;
        let len = (us.Length / 2) as usize;
        if len == 0 {
            return Err(Why::NoCommandLine);
        }
        // The text must lie inside our own buffer.
        if at < base || at + len * 2 > base + capacity as usize || !at.is_multiple_of(2) {
            return Err(Why::Unreadable);
        }
        // SAFETY: checked above: `len` UTF-16 units inside the buffer, correctly aligned.
        let units = unsafe { std::slice::from_raw_parts(at as *const u16, len) };
        Ok(String::from_utf16_lossy(units))
    }

    fn user(&self) -> Result<String, Why> {
        let mut token = HANDLE::default();
        // SAFETY: the process handle has query access; the token handle is closed below.
        unsafe { OpenProcessToken(self.handle.0, TOKEN_QUERY, &mut token) }.map_err(|e| {
            if e.code() == windows::core::HRESULT::from_win32(ERROR_ACCESS_DENIED.0) {
                Why::NeedsAdmin
            } else {
                Why::Unreadable
            }
        })?;
        let token = Handle(token);
        let mut needed = 0u32;
        // SAFETY: a zero-length query only reports the size needed.
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut needed) };
        if needed == 0 || needed > 4096 {
            return Err(Why::Unreadable);
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: the buffer holds `needed` bytes and is aligned for TOKEN_USER.
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buf.as_mut_ptr().cast()),
                (buf.len() * 8) as u32,
                &mut needed,
            )
        }
        .map_err(|_| Why::Unreadable)?;
        // SAFETY: Windows wrote a TOKEN_USER at the start of the buffer; its SID lies inside it.
        let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut name = [0u16; 256];
        let mut domain = [0u16; 256];
        let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
        let mut kind = SID_NAME_USE::default();
        // SAFETY: both buffers are as long as the lengths say.
        unsafe {
            LookupAccountSidW(
                None,
                sid,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                Some(PWSTR(domain.as_mut_ptr())),
                &mut domain_len,
                &mut kind,
            )
        }
        .map_err(|_| Why::Unreadable)?;
        Ok(account_text(
            &String::from_utf16_lossy(&domain[..domain_len as usize]),
            &String::from_utf16_lossy(&name[..name_len as usize]),
        ))
    }

    fn handles(&self) -> Result<u32, Why> {
        let mut count = 0u32;
        // SAFETY: the handle has query access and `count` is a valid output.
        unsafe { GetProcessHandleCount(self.handle.0, &mut count) }.map_err(|e| {
            if e.code() == windows::core::HRESULT::from_win32(ERROR_ACCESS_DENIED.0) {
                Why::NeedsAdmin
            } else {
                Why::Unreadable
            }
        })?;
        Ok(count)
    }
}

/// "DOMAIN\name", or just the name when there is no domain.
pub(super) fn account_text(domain: &str, name: &str) -> String {
    if domain.is_empty() {
        name.to_string()
    } else {
        format!("{domain}\\{name}")
    }
}

/// The process table entry of `pid`: its parent id and thread count, from a snapshot that opens
/// no process.
fn process_entry(pid: u32) -> Entry {
    // SAFETY: a snapshot of the process table; the handle is closed by `Handle`.
    let snapshot = Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `entry` is a valid output with its size set.
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_ok();
    while more {
        if entry.th32ProcessID == pid {
            return Some((entry.th32ParentProcessID, entry.cntThreads));
        }
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot.0, &mut entry) }.is_ok();
    }
    None
}

/// "2026-11-17 14:03:22" in local time.
pub(super) fn local_time(filetime: u64) -> Option<String> {
    let ft = FILETIME {
        dwLowDateTime: filetime as u32,
        dwHighDateTime: (filetime >> 32) as u32,
    };
    let mut utc = Default::default();
    let mut local = Default::default();
    // SAFETY: both outputs are valid SYSTEMTIME buffers.
    unsafe {
        FileTimeToSystemTime(&ft, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond
    ))
}

/// The command line as shown: at most [`MAX_COMMAND_LINE_SHOWN`] characters.
pub(super) fn shown_command_line(text: &str) -> String {
    if text.chars().count() <= MAX_COMMAND_LINE_SHOWN {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX_COMMAND_LINE_SHOWN).collect();
    format!("{cut}... (shortened)")
}

// ---------------------------------------------------------------------------------------------
// The page's side: one read at a time, on a worker, for the one selected process.

pub(super) struct Reader {
    /// The process the facts are wanted for.
    want: Option<ProcessIdentity>,
    shown: Option<Facts>,
    read_at: Option<Instant>,
    job: Option<Receiver<Facts>>,
}

impl Reader {
    pub(super) fn new() -> Reader {
        Reader {
            want: None,
            shown: None,
            read_at: None,
            job: None,
        }
    }

    /// Called every frame with the one selected process (None for no or several). Starts a read
    /// when the selection changes or the last read is [`REREAD`] old. Never blocks.
    pub(super) fn follow(&mut self, ctx: &egui::Context, selected: Option<&Proc>) {
        if let Some(rx) = &self.job {
            if let Ok(facts) = rx.try_recv() {
                self.job = None;
                if Some(facts.identity) == self.want {
                    self.shown = Some(facts);
                    self.read_at = Some(Instant::now());
                }
            }
        }
        let identity = selected.and_then(|p| p.identity);
        let Some(identity) = identity else {
            // Nothing selected, several selected, or a process Windows could not verify: nothing
            // is kept for it.
            *self = Reader::new();
            return;
        };
        if self.want != Some(identity) {
            // Another process: drop what was read, and the read in flight (its answer is
            // ignored when it arrives).
            *self = Reader::new();
            self.want = Some(identity);
        }
        let stale = self.read_at.is_none_or(|at| at.elapsed() >= REREAD);
        if self.job.is_none() && stale {
            let parent = selected.and_then(|p| p.parent.name()).map(str::to_string);
            let (tx, rx) = mpsc::channel();
            let ctx = ctx.clone();
            let spawned = std::thread::Builder::new()
                .name("process-facts".into())
                .spawn(move || {
                    let facts = read_live(identity, parent.as_deref());
                    let _ = tx.send(facts);
                    ctx.request_repaint();
                });
            if spawned.is_ok() {
                self.job = Some(rx);
            } else {
                self.shown = Some(Facts::all_missing(identity, Why::Unreadable));
                self.read_at = Some(Instant::now());
            }
        }
        // Come back for the next read.
        ctx.request_repaint_after(REREAD);
    }

    pub(super) fn shown(&self) -> Option<&Facts> {
        self.shown.as_ref()
    }

    #[cfg(test)]
    pub(super) fn reading(&self) -> bool {
        self.job.is_some()
    }
}

/// The inspector lines of the facts, for the one process `facts` were wanted for.
pub(super) fn show(ui: &mut egui::Ui, reader: &Reader, identity: Option<ProcessIdentity>) {
    let facts = reader.shown().filter(|f| Some(f.identity) == identity);
    let waiting = "Reading...";
    let line = |ui: &mut egui::Ui, key: &str, text: Option<(String, bool)>| match text {
        Some((text, known)) if known => kv(ui, key, &text),
        Some((text, _)) => {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(key).small().weak());
            ui.add(egui::Label::new(RichText::new(text).weak().small()).wrap());
            ui.spacing_mut().item_spacing.y = 4.0;
        }
        None => {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(key).small().weak());
            ui.label(RichText::new(waiting).weak().small());
            ui.spacing_mut().item_spacing.y = 4.0;
        }
    };
    fn text<T: ToString>(fact: &Fact<T>, shape: impl Fn(String) -> String) -> (String, bool) {
        match fact {
            Fact::Known(v) => (shape(v.to_string()), true),
            Fact::Missing(why) => (why.text().to_string(), false),
        }
    }
    let same = |s: String| s;
    line(ui, "Started", facts.map(|f| text(&f.started, same)));
    line(
        ui,
        "Command line",
        facts.map(|f| text(&f.command_line, |s| shown_command_line(&s))),
    );
    line(ui, "User", facts.map(|f| text(&f.user, same)));
    line(ui, "Parent", facts.map(|f| text(&f.parent, same)));
    line(ui, "Threads", facts.map(|f| text(&f.threads, same)));
    line(ui, "Handles", facts.map(|f| text(&f.handles, same)));
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: ProcessIdentity = ProcessIdentity {
        pid: 4242,
        created: 133_500_000_000_000_000,
    };

    struct Fake {
        command_line: Result<String, Why>,
        user: Result<String, Why>,
        handles: Result<u32, Why>,
    }

    impl Os for Fake {
        fn command_line(&self) -> Result<String, Why> {
            self.command_line.clone()
        }
        fn user(&self) -> Result<String, Why> {
            self.user.clone()
        }
        fn handles(&self) -> Result<u32, Why> {
            self.handles
        }
    }

    fn ok() -> Fake {
        Fake {
            command_line: Ok("app.exe --flag".into()),
            user: Ok("PC\\me".into()),
            handles: Ok(120),
        }
    }

    #[test]
    fn a_readable_process_has_every_value() {
        let f = assemble(ID, Some("explorer.exe"), Some((100, 17)), Ok(&ok()));
        assert!(
            matches!(&f.started, Fact::Known(s) if s.len() == 19),
            "{:?}",
            f.started
        );
        assert_eq!(f.command_line, Fact::Known("app.exe --flag".into()));
        assert_eq!(f.user, Fact::Known("PC\\me".into()));
        assert_eq!(f.parent, Fact::Known("explorer.exe (100)".into()));
        assert_eq!(f.threads, Fact::Known(17));
        assert_eq!(f.handles, Fact::Known(120));
    }

    #[test]
    fn every_value_has_a_not_available_path_with_a_reason() {
        // Windows refuses the process: only the start time (known from the page) and the table
        // entry remain, and each missing value says why.
        let f = assemble(
            ID,
            Some("explorer.exe"),
            Some((100, 3)),
            Err(Why::NeedsAdmin),
        );
        assert!(matches!(f.started, Fact::Known(_)));
        assert_eq!(f.command_line, Fact::Missing(Why::NeedsAdmin));
        assert_eq!(f.user, Fact::Missing(Why::NeedsAdmin));
        assert_eq!(f.handles, Fact::Missing(Why::NeedsAdmin));
        assert_eq!(
            f.threads,
            Fact::Known(3),
            "the process table needs no access"
        );
        // Each value can be missing alone.
        let mixed = Fake {
            command_line: Err(Why::NoCommandLine),
            user: Err(Why::NeedsAdmin),
            handles: Err(Why::Unreadable),
        };
        let f = assemble(ID, Some("explorer.exe"), Some((100, 3)), Ok(&mixed));
        assert_eq!(f.command_line, Fact::Missing(Why::NoCommandLine));
        assert_eq!(f.user, Fact::Missing(Why::NeedsAdmin));
        assert_eq!(f.handles, Fact::Missing(Why::Unreadable));
        // The text of every reason is a plain sentence.
        for why in [
            Why::Ended,
            Why::Changed,
            Why::NeedsAdmin,
            Why::Unreadable,
            Why::NoCommandLine,
            Why::ParentUnknown,
            Why::NoParent,
        ] {
            assert!(why.text().starts_with("Not available: "), "{why:?}");
            assert!(why.text().ends_with('.'), "{why:?}");
        }
    }

    #[test]
    fn the_parent_is_shown_only_when_it_was_verified() {
        let read = |ppid, vouched| assemble(ID, vouched, Some((ppid, 1)), Ok(&ok())).parent;
        assert_eq!(read(100, Some("a.exe")), Fact::Known("a.exe (100)".into()));
        assert_eq!(read(100, None), Fact::Missing(Why::ParentUnknown));
        assert_eq!(read(0, Some("a.exe")), Fact::Missing(Why::NoParent));
    }

    #[test]
    fn an_ended_or_replaced_process_is_not_read() {
        // Not in the process table and not openable: everything says it ended.
        let f = assemble(ID, None, None, Err(Why::Ended));
        assert_eq!(f.threads, Fact::Missing(Why::Ended));
        assert_eq!(f.parent, Fact::Missing(Why::Ended));
        assert_eq!(f.command_line, Fact::Missing(Why::Ended));
        // The id reused by another process: the start time shown is still the selected one's, and
        // nothing of the new process is shown.
        let f = assemble(ID, Some("a.exe"), None, Err(Why::Changed));
        assert_eq!(f.command_line, Fact::Missing(Why::Changed));
        assert_eq!(f.threads, Fact::Missing(Why::Changed));
    }

    #[test]
    fn long_command_lines_are_shortened_and_accounts_are_joined() {
        assert_eq!(shown_command_line("a b"), "a b");
        let long = "x".repeat(MAX_COMMAND_LINE_SHOWN + 10);
        let shown = shown_command_line(&long);
        assert!(shown.starts_with(&"x".repeat(MAX_COMMAND_LINE_SHOWN)));
        assert!(shown.ends_with("(shortened)"));
        assert_eq!(account_text("PC", "me"), "PC\\me");
        assert_eq!(account_text("", "SYSTEM"), "SYSTEM");
    }

    #[test]
    fn the_start_time_is_local_and_readable() {
        // 2026-11-17 12:00:00 UTC.
        let unix = 20_774u64 * 86_400 + 12 * 3600;
        let t = local_time((unix + 11_644_473_600) * 10_000_000).unwrap();
        assert_eq!(t.len(), 19, "{t}");
        assert!(t.starts_with("2026-11-1"), "{t}");
        assert!(local_time(u64::MAX).is_none() || local_time(u64::MAX).is_some());
    }

    #[test]
    fn only_limited_query_access_is_ever_asked_for() {
        assert_eq!(ACCESS, PROCESS_QUERY_LIMITED_INFORMATION);
    }

    /// The identity of a running process, as the page would hold it.
    fn identity_of(pid: u32) -> ProcessIdentity {
        // SAFETY: plain FFI calls with limited query access.
        let handle = Handle(unsafe { OpenProcess(ACCESS, false, pid) }.unwrap());
        ProcessIdentity {
            pid,
            created: creation_time(handle.0).unwrap(),
        }
    }

    #[test]
    fn this_process_is_read_for_real() {
        let id = identity_of(std::process::id());
        let f = read_live(id, Some("parent.exe"));
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().to_lowercase();
        match &f.command_line {
            Fact::Known(c) => assert!(c.to_lowercase().contains(&name), "{c}"),
            other => panic!("own command line: {other:?}"),
        }
        assert!(
            matches!(&f.user, Fact::Known(u) if !u.is_empty()),
            "{:?}",
            f.user
        );
        assert!(
            matches!(f.threads, Fact::Known(n) if n >= 1),
            "{:?}",
            f.threads
        );
        assert!(
            matches!(f.handles, Fact::Known(n) if n >= 1),
            "{:?}",
            f.handles
        );
        assert!(
            matches!(&f.parent, Fact::Known(p) if p.starts_with("parent.exe (")),
            "{:?}",
            f.parent
        );
        assert!(matches!(f.started, Fact::Known(_)));
    }

    #[test]
    fn a_child_started_with_arguments_shows_them() {
        let mut child = super::super::spawn_idle_child();
        let f = read_live(identity_of(child.id()), None);
        let _ = child.kill();
        let _ = child.wait();
        match &f.command_line {
            Fact::Known(c) => assert!(c.contains("toolboks_test_wait"), "{c}"),
            other => panic!("child command line: {other:?}"),
        }
        // No verified parent was passed: it is not guessed.
        assert_eq!(f.parent, Fact::Missing(Why::ParentUnknown));
    }

    #[test]
    fn a_wrong_creation_time_or_an_ended_process_is_refused_not_read() {
        let mut id = identity_of(std::process::id());
        id.created += 1;
        let f = read_live(id, Some("x.exe"));
        assert_eq!(f.command_line, Fact::Missing(Why::Changed));
        assert_eq!(f.user, Fact::Missing(Why::Changed));
        assert_eq!(f.handles, Fact::Missing(Why::Changed));
        // A pid that is not running.
        let gone = ProcessIdentity {
            pid: u32::MAX - 1,
            created: 1,
        };
        let f = read_live(gone, None);
        assert_eq!(f.command_line, Fact::Missing(Why::Ended));
        assert_eq!(f.threads, Fact::Missing(Why::Ended));
    }

    #[test]
    fn protected_and_system_processes_never_panic_and_say_why() {
        // The System process (pid 4) and the Idle process (pid 0) are what Windows refuses most.
        for pid in [0u32, 4] {
            let f = read_live(ProcessIdentity { pid, created: 1 }, None);
            for fact in [&f.command_line, &f.user] {
                if let Fact::Missing(why) = fact {
                    assert!(!why.text().is_empty());
                }
            }
        }
    }

    fn proc_for(identity: ProcessIdentity) -> Proc {
        Proc {
            pid: identity.pid,
            identity: Some(identity),
            name: "test.exe".into(),
            lower_name: "test.exe".into(),
            cpu: 0.0,
            mem: 0,
            disk: None,
            gpu: None,
            conns: None,
            run_secs: 0,
            exe: None,
            parent: protected_processes::Parent::known("parent.exe"),
            kind: super::super::groups::Kind::Background,
            tree_parent: None,
        }
    }

    #[test]
    fn the_reader_reads_on_a_worker_follows_the_selection_and_drops_it() {
        let ctx = egui::Context::default();
        let me = proc_for(identity_of(std::process::id()));
        let mut reader = Reader::new();
        // Nothing selected: nothing read, nothing kept.
        reader.follow(&ctx, None);
        assert!(reader.shown().is_none() && !reader.reading());
        // One selected: a read starts at once and the call returns without waiting for it.
        let started = Instant::now();
        reader.follow(&ctx, Some(&me));
        assert!(reader.reading() || reader.shown().is_some());
        assert!(started.elapsed() < Duration::from_secs(1));
        let until = Instant::now() + Duration::from_secs(10);
        while reader.shown().is_none() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
            reader.follow(&ctx, Some(&me));
        }
        let facts = reader.shown().expect("facts arrive");
        assert_eq!(Some(facts.identity), me.identity);
        // Deselected (or several selected): everything is dropped.
        reader.follow(&ctx, None);
        assert!(reader.shown().is_none() && !reader.reading());
    }
}
