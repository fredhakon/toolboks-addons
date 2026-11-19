//! Live progress of running jest jobs: reads the job's log incrementally and counts finished suites.
use crate::broker::resolve_glob;
use crate::history::History;
use crate::scheduler::Ticket;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// At most this much new log is read per poll, so a huge log cannot stall the UI thread.
const MAX_READ: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct JobProgress {
    /// Suites that printed a `PASS` or `FAIL` line.
    pub done: u32,
    /// Expected suite total: the `testdeck: suites N` line in the log, else the last run of the same command.
    pub expected: Option<u32>,
    /// Failing suite paths in the order they appeared.
    pub failing: Vec<String>,
}

#[derive(Default)]
struct LogState {
    path: Option<PathBuf>,
    offset: u64,
    seen: HashSet<String>,
    failing: Vec<String>,
    expected: Option<u32>,
    looked_up_history: bool,
}

#[derive(Default)]
pub struct Tracker {
    logs: HashMap<Ticket, LogState>,
}

impl Tracker {
    /// Reads the new part of the job's log and returns the counts so far. Cheap enough to call once a second.
    pub fn poll(
        &mut self,
        ticket: Ticket,
        log_glob: Option<&str>,
        command: &str,
        history: &History,
    ) -> JobProgress {
        let st = self.logs.entry(ticket).or_default();
        if let Some(path) = log_glob.and_then(resolve_glob) {
            if st.path.as_ref() != Some(&path) {
                // A nested jt.sh can switch to its own log dir; start over on the new file.
                *st = LogState {
                    path: Some(path),
                    expected: st.expected,
                    looked_up_history: st.looked_up_history,
                    ..Default::default()
                };
            }
            read_new(st);
        }
        // Until the log names its suite count, the last run of the same command is the best guess.
        if st.expected.is_none() && !st.looked_up_history {
            st.looked_up_history = true;
            st.expected = history.last_suite_total(command);
        }
        JobProgress {
            done: st.seen.len() as u32,
            expected: st.expected,
            failing: st.failing.clone(),
        }
    }

    /// Suites finished so far in the job's log, without the expected-count lookups (the broker's stall guard).
    pub fn done_count(&mut self, ticket: Ticket, log_glob: &str) -> u32 {
        let st = self.logs.entry(ticket).or_default();
        if let Some(path) = resolve_glob(log_glob) {
            if st.path.as_ref() != Some(&path) {
                *st = LogState {
                    path: Some(path),
                    ..Default::default()
                };
            }
            read_new(st);
        }
        st.seen.len() as u32
    }

    /// Forget tickets that are no longer running.
    pub fn retain(&mut self, live: &HashSet<Ticket>) {
        self.logs.retain(|t, _| live.contains(t));
    }
}

/// Reads whole new lines since `st.offset` and updates the counts; a partial last line waits for the next poll.
fn read_new(st: &mut LogState) {
    let Some(path) = st.path.clone() else { return };
    let Ok(mut f) = std::fs::File::open(&path) else {
        return;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < st.offset {
        // Truncated or replaced: count again from the start.
        st.offset = 0;
        st.seen.clear();
        st.failing.clear();
    }
    if len == st.offset || f.seek(SeekFrom::Start(st.offset)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if f.take(MAX_READ).read_to_end(&mut buf).is_err() {
        return;
    }
    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
        return;
    };
    st.offset += end as u64 + 1;
    for line in String::from_utf8_lossy(&buf[..end]).lines() {
        let line = strip_ansi(line);
        // testdeck-sequencer.js prints how many suites this run has before the first one starts.
        if let Some(n) = line
            .strip_prefix("testdeck: suites ")
            .and_then(|n| n.trim().parse().ok())
        {
            st.expected = Some(n);
            continue;
        }
        let (pass, rest) = match (line.strip_prefix("PASS "), line.strip_prefix("FAIL ")) {
            (Some(r), _) => (true, r),
            (_, Some(r)) => (false, r),
            _ => continue,
        };
        let suite = rest.trim().to_string();
        if suite.is_empty() {
            continue;
        }
        // jest prints every FAIL again in its summary, so a suite counts once.
        st.seen.insert(suite.clone());
        if !pass && !st.failing.contains(&suite) {
            st.failing.push(suite);
        }
    }
}

/// Drops ANSI escape sequences (`ESC [ ... final byte`) so colored `PASS`/`FAIL` markers still match.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for n in chars.by_ref() {
                if ('@'..='~').contains(&n) {
                    break;
                }
            }
        }
    }
    out
}
