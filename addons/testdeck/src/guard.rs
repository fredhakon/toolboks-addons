//! Broker-side safety guards, run from the 1 s tick so they work while the window is hidden.
//! - Stall guard: a running jest job whose log shows no new PASS/FAIL line for `stall_minutes` is marked stalled
//!   (in `/status` and the UI) and toasted once.
//! - RAM guard: when free RAM falls under `ram_kill_free_gb`, the newest non-gate job is stopped, preferring one
//!   whose client can rerun it (`requeue_ok`); see docs/PHASE2.md "RAM requeue". At most one stop per minute.
use crate::notify;
use crate::progress::Tracker;
use crate::scheduler::{Kind, Secs, Ticket};
use crate::state::AppState;
use std::collections::{HashMap, HashSet};

/// At most one RAM-guard stop per this many seconds: freed memory takes a moment to show in the free RAM figure.
const RAM_KILL_SPACING_SECS: Secs = 60.0;
/// The RAM guard only stops jobs granted at least this long ago: a fresh grant has not started its work yet, so
/// stopping it frees nothing and its client would run the whole command before it learns of the requeue.
const RAM_MIN_AGE_SECS: Secs = 10.0;

#[derive(Default)]
pub struct Guard {
    tracker: Tracker,
    /// Per running jest ticket: suites done, and when that count last changed.
    seen: HashMap<Ticket, (u32, Secs)>,
    /// Tickets already toasted as stalled.
    toasted: HashSet<Ticket>,
    last_ram_kill: Option<Secs>,
    /// The gate already toasted as asking.
    asked: Option<Ticket>,
}

struct Job {
    ticket: Ticket,
    kind: Kind,
    log_glob: Option<String>,
    requester: String,
    granted_at: Secs,
    requeue_ok: bool,
}

pub fn tick(s: &AppState, now: Secs) {
    let (jobs, stall_minutes, ram_kill, ram_free) = {
        let c = s.lock();
        let jobs: Vec<Job> = c
            .sched
            .running
            .iter()
            .map(|r| Job {
                ticket: r.ticket,
                kind: r.req.kind,
                log_glob: r.req.log_glob.clone(),
                requester: r.req.requester.clone(),
                granted_at: r.granted_at,
                requeue_ok: r.req.requeue_ok,
            })
            .collect();
        (
            jobs,
            c.config.stall_minutes,
            c.config.ram_kill_free_gb,
            c.load.ram_free_gb,
        )
    };
    let asking = s.gate_asking();
    let mut guard = s.guard.lock().unwrap_or_else(|e| e.into_inner());
    let g = &mut *guard;
    // Gate ask: a plain toast; Allow / Later live in the window banner and the tray menu (see notify.rs).
    if let Some((t, who)) = asking.filter(|(t, _)| g.asked != Some(*t)) {
        g.asked = Some(t);
        let body = format!("{who}'s gate can start now. Open Testdeck or the tray menu to Allow it, or Later (30 min).");
        notify::toast("Testdeck: a gate waits for you", &body);
    }
    let live: HashSet<Ticket> = jobs.iter().map(|j| j.ticket).collect();
    g.seen.retain(|t, _| live.contains(t));
    g.toasted.retain(|t| live.contains(t));
    g.tracker.retain(&live);

    // Stall guard (0 minutes = off).
    let stall_secs = f64::from(stall_minutes) * 60.0;
    let mut stalled = HashSet::new();
    for j in jobs.iter().filter(|j| j.kind == Kind::Jest) {
        let Some(glob) = j.log_glob.as_deref() else {
            continue;
        };
        let done = g.tracker.done_count(j.ticket, glob);
        let e = g.seen.entry(j.ticket).or_insert((done, now));
        if e.0 != done {
            *e = (done, now);
        }
        if stall_secs > 0.0 && now - e.1 >= stall_secs {
            stalled.insert(j.ticket);
            if g.toasted.insert(j.ticket) {
                let body = format!(
                    "{}: no new PASS/FAIL line for {stall_minutes} min. Open Testdeck to kill it.",
                    j.requester
                );
                eprintln!("testdeck: ticket {} stalled ({body})", j.ticket);
                notify::toast("Testdeck: jest run stalled", &body);
            }
        }
    }
    for r in s.lock().sched.running.iter_mut() {
        r.stalled = stalled.contains(&r.ticket);
    }

    // RAM guard (0 = off; a zero reading means no sample yet).
    let low = ram_kill > 0.0 && ram_free > 0.0 && ram_free < ram_kill;
    if !low
        || g.last_ram_kill
            .is_some_and(|t| now - t < RAM_KILL_SPACING_SECS)
    {
        return;
    }
    // Candidates: requeue_ok jobs newest first, then the others newest first. A requeue_ok job with no native work
    // (nothing to stop) is skipped, never killed whole; only a real stop counts for the 60 s spacing.
    let mut cands: Vec<&Job> = jobs
        .iter()
        .filter(|j| j.kind != Kind::Gate && now - j.granted_at >= RAM_MIN_AGE_SECS)
        .collect();
    cands.sort_by(|a, b| {
        b.requeue_ok
            .cmp(&a.requeue_ok)
            .then(b.granted_at.total_cmp(&a.granted_at))
    });
    drop(guard);
    let mut stopped = None;
    for j in cands {
        if !j.requeue_ok || s.requeue(j.ticket) {
            stopped = Some(j);
            break;
        }
    }
    let Some(j) = stopped else { return };
    s.guard
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .last_ram_kill = Some(now);
    let (title, body) = if j.requeue_ok {
        let body = format!(
            "{}: stopped the {} run at {ram_free:.1} GB free RAM; it is requeued at the head of the queue.",
            j.requester,
            crate::history::kind_str(j.kind)
        );
        ("Testdeck: low RAM, job requeued", body)
    } else {
        s.kill(j.ticket);
        let body = format!(
            "{}: killed the {} run at {ram_free:.1} GB free RAM. Its client cannot rerun it, so this is final.",
            j.requester,
            crate::history::kind_str(j.kind)
        );
        ("Testdeck: low RAM, job killed", body)
    };
    eprintln!("testdeck: RAM guard, ticket {}: {body}", j.ticket);
    notify::toast(title, &body);
}
