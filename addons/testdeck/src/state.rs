//! Shared broker state: the scheduler plus what surrounds it (config, load, mirror), and the operations that the
//! HTTP API, the tick loop and the UI call. Every operation locks `core` briefly and wakes the long-polls.
use crate::config::Config;
use crate::mirror::Mirror;
use crate::scheduler::{
    EndReason, Env, Finished, Kind, Poll, Request, Scheduler, Secs, Ticket, DEFAULT_EXPECTED_SECS,
};
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

/// Process control by Windows pid. Phase 2 implements it in `winproc.rs`.
pub trait ProcCtl: Send + Sync {
    fn is_alive(&self, win_pid: u32) -> bool;
    /// BELOW_NORMAL when `below_normal`, else NORMAL, on the process and all its descendants.
    fn set_tree_priority(&self, win_pid: u32, below_normal: bool);
    fn kill_tree(&self, win_pid: u32);
    /// Kills every descendant of the process but not the process itself (RAM requeue: the client shell survives).
    /// Returns how many native process trees it killed (0 = nothing was stopped, so no RAM was freed).
    fn kill_descendants(&self, _win_pid: u32) -> usize {
        0
    }
    /// Locks the process and all its descendants to the CPU cores in `mask`; their new children inherit it.
    fn set_tree_affinity(&self, _win_pid: u32, _mask: u64) {}
}

/// Treats every process as alive and does nothing; staleness then comes from missed heartbeats only.
pub struct NoProcCtl;
impl ProcCtl for NoProcCtl {
    fn is_alive(&self, _win_pid: u32) -> bool {
        true
    }
    fn set_tree_priority(&self, _win_pid: u32, _below_normal: bool) {}
    fn kill_tree(&self, _win_pid: u32) {}
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct LoadPoint {
    pub t: Secs,
    pub cpu: f32,
    pub ram_free_gb: f32,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct JobLoad {
    /// Percent of the whole machine.
    pub cpu: f32,
    pub ram_mb: f32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Load {
    /// Rolling 10 s CPU average in percent (0-100).
    pub cpu_avg: f32,
    pub ram_free_gb: f32,
    pub ram_total_gb: f32,
    /// One point per tick for the last `LOAD_HISTORY_SECS`.
    pub history: VecDeque<LoadPoint>,
    /// Process-tree CPU/RAM per running ticket.
    pub per_ticket: HashMap<Ticket, JobLoad>,
}

pub const LOAD_HISTORY_SECS: Secs = 300.0;

impl Load {
    /// Appends a point and drops the ones older than `LOAD_HISTORY_SECS`.
    pub fn push(&mut self, p: LoadPoint) {
        self.history.push_back(p);
        while self
            .history
            .front()
            .is_some_and(|f| p.t - f.t > LOAD_HISTORY_SECS)
        {
            self.history.pop_front();
        }
    }
}

/// Samples the machine once per tick. Phase 2 implements it in `sysmon.rs`.
pub trait LoadSource: Send {
    /// Updates `load` in place; `pids` are the (ticket, Windows pid) pairs of the running jobs.
    fn refresh(&mut self, now: Secs, pids: &[(Ticket, u32)], load: &mut Load);
}

/// A constant load, for tests and until the real sampler exists.
pub struct FixedLoad {
    pub cpu: f32,
    pub ram_free_gb: f32,
}
impl LoadSource for FixedLoad {
    fn refresh(&mut self, now: Secs, _pids: &[(Ticket, u32)], load: &mut Load) {
        load.cpu_avg = self.cpu;
        load.ram_free_gb = self.ram_free_gb;
        load.push(LoadPoint {
            t: now,
            cpu: self.cpu,
            ram_free_gb: self.ram_free_gb,
        });
    }
}

/// Seconds since the last keyboard or mouse input. Phase 2 implements it in `idle.rs` (`GetLastInputInfo`).
pub trait IdleSource: Send + Sync {
    fn idle_secs(&self) -> Option<u64>;
}
pub struct NoIdle;
impl IdleSource for NoIdle {
    fn idle_secs(&self) -> Option<u64> {
        None
    }
}

/// One ended job, handed to the `JobSink` (history) when it is released, killed or reaped.
#[derive(Clone, Debug, Serialize)]
pub struct FinishedJob {
    pub ticket: Ticket,
    pub kind: Kind,
    pub requester: String,
    pub command: String,
    pub cwd: String,
    pub workers: u32,
    pub started_unix: f64,
    pub ended_unix: f64,
    pub duration_secs: f64,
    pub exit_code: Option<i32>,
    pub end_reason: EndReason,
    /// From jest's `result.json` next to the job's log, when the log glob resolves.
    pub passed: Option<u32>,
    pub failed: Option<u32>,
    pub suites_total: Option<u32>,
    pub failing_suites: Vec<String>,
}

pub trait JobSink: Send + Sync {
    fn record(&self, job: &FinishedJob);
}
pub struct NullSink;
impl JobSink for NullSink {
    fn record(&self, _job: &FinishedJob) {}
}

pub struct Deps {
    pub proc_ctl: Arc<dyn ProcCtl>,
    pub sink: Arc<dyn JobSink>,
    pub idle: Arc<dyn IdleSource>,
    pub load: Box<dyn LoadSource>,
}

impl Deps {
    /// No process control, no history, no idle detection and a quiet fixed load.
    pub fn fake() -> Self {
        Self {
            proc_ctl: Arc::new(NoProcCtl),
            sink: Arc::new(NullSink),
            idle: Arc::new(NoIdle),
            load: Box::new(FixedLoad {
                cpu: 10.0,
                ram_free_gb: 16.0,
            }),
        }
    }
}

pub struct Core {
    pub sched: Scheduler,
    pub config: Config,
    /// Where `set_config` saves; None in tests.
    pub config_path: Option<PathBuf>,
    pub load: Load,
    /// Average duration per command, filled from history; feeds the ETA.
    pub job_stats: HashMap<String, Secs>,
    /// The tray toggle.
    pub user_mode_manual: bool,
    pub idle_secs: Option<u64>,
    pub foreign_busy: u32,
    pub mirror: Mirror,
    /// The port the listener is bound to (differs from `config.port` when that is 0).
    pub bound_port: Option<u16>,
}

impl Core {
    /// Manual toggle, or auto-detect and input within the last `idle_minutes`.
    pub fn user_mode(&self) -> bool {
        self.user_mode_manual
            || (self.config.auto_user_mode
                && self
                    .idle_secs
                    .is_some_and(|s| s < u64::from(self.config.idle_minutes) * 60))
    }

    pub fn expected_secs(&self, command: &str) -> Secs {
        self.job_stats
            .get(command)
            .copied()
            .unwrap_or(DEFAULT_EXPECTED_SECS)
    }

    pub fn slot_limit(&self) -> u32 {
        if self.user_mode() {
            1
        } else {
            self.config.slots.max(1)
        }
    }

    pub fn env(&self, now: Secs) -> Env {
        Env {
            now,
            cpu_avg: self.load.cpu_avg,
            ram_free_gb: self.load.ram_free_gb,
            user_mode: self.user_mode(),
            foreign_busy: self.foreign_busy,
            gates_held: self.config.night_window_enabled
                && !crate::scheduler::in_window(
                    crate::winproc::local_minutes(),
                    &self.config.night_start,
                    &self.config.night_end,
                ),
            gate_ask: self.config.gate_ask_in_user_mode && self.user_mode(),
            cores: crate::config::threads(),
        }
    }

    pub fn eta_secs(&self, t: Ticket, now: Secs) -> Option<Secs> {
        self.sched
            .eta_secs(t, now, self.slot_limit(), &|c| self.expected_secs(c))
    }
}

pub struct AppState {
    pub core: Mutex<Core>,
    /// Woken on every state change; long-polls wait on it.
    pub notify: Notify,
    pub proc_ctl: Arc<dyn ProcCtl>,
    pub sink: Arc<dyn JobSink>,
    pub idle: Arc<dyn IdleSource>,
    load_source: Mutex<Box<dyn LoadSource>>,
    /// Stall and RAM guard state (`guard.rs`).
    pub guard: Mutex<crate::guard::Guard>,
    epoch: Instant,
    epoch_unix: f64,
}

pub type Shared = Arc<AppState>;

pub const CANCEL_REASON: &str = "cancelled in the Testdeck queue";

impl AppState {
    pub fn new(config: Config, config_path: Option<PathBuf>, deps: Deps) -> Shared {
        let mut load = Load::default();
        let mut source = deps.load;
        source.refresh(0.0, &[], &mut load);
        let core = Core {
            sched: Scheduler::new(),
            mirror: Mirror::new(config.mirror_path()),
            config,
            config_path,
            load,
            job_stats: HashMap::new(),
            user_mode_manual: false,
            idle_secs: deps.idle.idle_secs(),
            foreign_busy: 0,
            bound_port: None,
        };
        Arc::new(Self {
            core: Mutex::new(core),
            notify: Notify::new(),
            proc_ctl: deps.proc_ctl,
            sink: deps.sink,
            idle: deps.idle,
            load_source: Mutex::new(source),
            guard: Mutex::new(crate::guard::Guard::default()),
            epoch: Instant::now(),
            epoch_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Seconds on the scheduler clock.
    pub fn now(&self) -> Secs {
        self.epoch.elapsed().as_secs_f64()
    }

    pub fn to_unix(&self, t: Secs) -> f64 {
        self.epoch_unix + t
    }

    fn changed(&self) {
        self.notify.notify_waiters();
    }

    /// Runs admission and mirrors each grant into the slot dir.
    fn admit_locked(c: &mut Core, now: Secs) {
        c.foreign_busy = c.mirror.foreign_busy();
        let env = c.env(now);
        for (t, _) in c.sched.admit(&c.config, &env) {
            if let Some(r) = c.sched.running.iter().find(|r| r.ticket == t) {
                let gate = r.req.kind == Kind::Gate && c.config.gate_exclusive;
                // shortcut: when an old script took the slot dir first, the grant stands without a mirror dir.
                c.mirror
                    .grant(t, gate, c.config.slots, r.req.shell_pid, r.req.win_pid);
            }
        }
    }

    /// Runs `f` on the core, then admission, then wakes the long-polls.
    fn with_admit<T>(&self, f: impl FnOnce(&mut Core, Secs) -> T) -> T {
        let now = self.now();
        let out = {
            let mut g = self.lock();
            let out = f(&mut g, now);
            Self::admit_locked(&mut g, now);
            out
        };
        self.changed();
        out
    }

    pub fn acquire(&self, req: Request) -> Ticket {
        self.with_admit(|c, now| {
            let gate_first = c.config.gate_first;
            c.sched.enqueue(req, now, gate_first)
        })
    }

    /// The ticket's state and, while it waits, its ETA in seconds.
    pub fn poll(&self, t: Ticket) -> (Poll, Option<Secs>) {
        let now = self.now();
        let mut g = self.lock();
        let p = g.sched.poll(t, now);
        let eta = if matches!(p, Poll::Waiting { .. }) {
            g.eta_secs(t, now)
        } else {
            None
        };
        (p, eta)
    }

    pub fn started(&self, t: Ticket, win_pid: Option<u32>, log_glob: Option<String>) -> bool {
        let now = self.now();
        self.lock().sched.started(t, win_pid, log_glob, now)
    }

    pub fn heartbeat(&self, t: Ticket) -> bool {
        let now = self.now();
        self.lock().sched.heartbeat(t, now)
    }

    pub fn release(&self, t: Ticket, exit_code: Option<i32>) -> bool {
        let f = self.with_admit(|c, now| {
            let f = c.sched.release(t, exit_code, now);
            c.mirror.release(t);
            f
        });
        let Some(f) = f else {
            return self.lock().sched.forget_requeued(t);
        };
        self.finish(f);
        true
    }

    /// Cancels a queued request; its client gets a "cancelled" answer.
    pub fn cancel(&self, t: Ticket) -> bool {
        self.with_admit(|c, now| c.sched.cancel(t, CANCEL_REASON, now))
    }

    /// Moves a queued request one place up or down.
    pub fn move_ticket(&self, t: Ticket, up: bool) -> bool {
        self.with_admit(|c, _| c.sched.move_ticket(t, up))
    }

    pub fn set_priority(&self, t: Ticket, priority: i32) -> bool {
        self.with_admit(|c, _| {
            let gate_first = c.config.gate_first;
            c.sched.set_priority(t, priority, gate_first)
        })
    }

    pub fn set_paused(&self, paused: bool) {
        self.with_admit(|c, _| c.sched.paused = paused)
    }

    /// Kills a running job's process tree, or cancels a queued request.
    pub fn kill(&self, t: Ticket) -> bool {
        let f = self.with_admit(|c, now| {
            let f = c.sched.kill(t, now);
            c.mirror.release(t);
            f
        });
        let Some(f) = f else { return self.cancel(t) };
        if let Some(pid) = f.run.req.win_pid {
            self.proc_ctl.kill_tree(pid);
        }
        self.finish(f);
        true
    }

    /// The tray toggle; the next tick applies the process priorities.
    pub fn set_user_mode(&self, on: bool) {
        self.with_admit(|c, _| c.user_mode_manual = on)
    }

    /// Applies a new config live and saves it; a port change rebinds on the next tick.
    pub fn set_config(&self, mut cfg: Config) -> anyhow::Result<()> {
        // A hand edit of a preset's limits turns the test load slider to Custom.
        cfg.load_level = cfg.matching_load(crate::config::threads());
        let path = self.with_admit(|c, _| {
            c.config = cfg.clone();
            c.config_path.clone()
        });
        if let Some(p) = path {
            cfg.save(&p)?;
        }
        Ok(())
    }

    /// Once a second: sample load and idle, reap stale tickets, admit, re-apply process priorities.
    pub fn tick(&self) {
        let now = self.now();
        let (mut load, pids) = {
            let c = self.lock();
            let pids: Vec<(Ticket, u32)> = c
                .sched
                .running
                .iter()
                .filter_map(|r| r.req.win_pid.map(|p| (r.ticket, p)))
                .collect();
            (c.load.clone(), pids)
        };
        self.load_source
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .refresh(now, &pids, &mut load);
        let idle = self.idle.idle_secs();
        let dead: Vec<u32> = pids
            .iter()
            .map(|&(_, p)| p)
            .filter(|&p| !self.proc_ctl.is_alive(p))
            .collect();
        let mut affinities = Vec::new();
        let (finished, priorities) = {
            let mut g = self.lock();
            let c = &mut *g;
            c.load = load;
            c.idle_secs = idle;
            let timeout = c.config.heartbeat_timeout_secs as Secs;
            let finished = c.sched.reap(now, timeout, &|p| !dead.contains(&p));
            for f in &finished {
                c.mirror.release(f.run.ticket);
            }
            Self::admit_locked(c, now);
            let user_mode = c.user_mode();
            let mut priorities = Vec::new();
            affinities.extend(assign_cores(c));
            for r in c.sched.running.iter_mut() {
                let Some(p) = r.req.win_pid else { continue };
                if r.cpu_mask != 0 {
                    // Re-applied every tick, like the priority, so no process of the tree escapes the lock.
                    affinities.push((p, r.cpu_mask));
                }
                if user_mode {
                    // Re-applied every tick so new child processes are lowered too.
                    priorities.push((p, true));
                    r.below_normal = true;
                } else if r.below_normal {
                    priorities.push((p, false));
                    r.below_normal = false;
                }
            }
            (finished, priorities)
        };
        for (p, below) in priorities {
            self.proc_ctl.set_tree_priority(p, below);
        }
        for (p, mask) in affinities {
            self.proc_ctl.set_tree_affinity(p, mask);
        }
        self.changed();
        for f in finished {
            self.finish(f);
        }
        crate::guard::tick(self, now);
    }

    /// RAM guard: ends a `requeue_ok` job as requeued and kills only its descendants; the client shell survives,
    /// sees "requeue" on its next heartbeat and asks again with `requeue_of`.
    /// Returns false, and leaves the ticket running, when it has no native work to stop (e.g. a `--rdb` run still
    /// waiting for a lane): requeueing it would free no RAM and leave its client running without a slot.
    pub fn requeue(&self, t: Ticket) -> bool {
        let pid = self
            .lock()
            .sched
            .running
            .iter()
            .find(|r| r.ticket == t)
            .and_then(|r| r.req.win_pid);
        let Some(pid) = pid else { return false };
        if self.proc_ctl.kill_descendants(pid) == 0 {
            return false;
        }
        let f = self.with_admit(|c, now| {
            let f = c.sched.requeue(t, now);
            c.mirror.release(t);
            f
        });
        // None: the run ended on its own meanwhile; its work was still stopped.
        if let Some(f) = f {
            self.finish(f);
        }
        true
    }

    /// Gate ask "Allow": the waiting gate may start.
    pub fn allow_gate(&self, t: Ticket) -> bool {
        self.with_admit(|c, _| c.sched.allow_gate(t))
    }

    /// Gate ask "Later": the gate waits 30 minutes, like outside the night window.
    pub fn snooze_gate(&self, t: Ticket) -> bool {
        self.with_admit(|c, now| c.sched.snooze_gate(t, now))
    }

    /// The gate waiting for Allow / Later and its requester.
    pub fn gate_asking(&self) -> Option<(Ticket, String)> {
        let c = self.lock();
        let t = c.sched.gate_asking?;
        c.sched
            .queue
            .iter()
            .find(|q| q.ticket == t)
            .map(|q| (t, q.req.requester.clone()))
    }

    /// Toast when a gate, or any job that ran at least `toast_min_minutes`, ends by itself. Kills are a click
    /// and the RAM guard toasts its own stops.
    fn toast_end(&self, job: &FinishedJob) {
        let min_secs = f64::from(self.lock().config.toast_min_minutes) * 60.0;
        let ended_itself = matches!(job.end_reason, EndReason::Released | EndReason::Stale);
        if !ended_itself || (job.kind != Kind::Gate && job.duration_secs < min_secs) {
            return;
        }
        let outcome = match (job.end_reason, job.exit_code) {
            (EndReason::Stale, _) => "lost (no heartbeat)".to_string(),
            (_, Some(0)) => "passed".to_string(),
            (_, Some(code)) => format!("failed (exit {code})"),
            (_, None) => "ended (exit code unknown)".to_string(),
        };
        let mut body = format!(
            "{}, {}",
            job.requester,
            crate::ui::fmt_dur(job.duration_secs)
        );
        if let (Some(p), Some(f)) = (job.passed, job.failed) {
            body.push_str(&format!(", {p} tests passed, {f} failed"));
        }
        crate::notify::toast(
            &format!("Testdeck: {} {outcome}", crate::history::kind_str(job.kind)),
            &body,
        );
    }

    /// Builds the history record (with jest counts when the log glob resolves) and hands it to the sink.
    fn finish(&self, f: Finished) {
        let r = &f.run;
        let mut job = FinishedJob {
            ticket: r.ticket,
            kind: r.req.kind,
            requester: r.req.requester.clone(),
            command: r.req.command.clone(),
            cwd: r.req.cwd.clone(),
            workers: r.workers,
            started_unix: self.to_unix(r.granted_at),
            ended_unix: self.to_unix(f.ended_at),
            duration_secs: f.ended_at - r.granted_at,
            exit_code: f.exit_code,
            end_reason: f.reason,
            passed: None,
            failed: None,
            suites_total: None,
            failing_suites: Vec::new(),
        };
        if let Some(res) = r
            .req
            .log_glob
            .as_deref()
            .and_then(crate::broker::jest_result)
        {
            job.passed = Some(res.passed);
            job.failed = Some(res.failed);
            job.suites_total = Some(res.suites_total);
            job.failing_suites = res.failing_suites;
        }
        self.sink.record(&job);
        self.toast_end(&job);
    }

    /// `GET /status`: running jobs, queue with position and ETA, paused, user mode, load, and a one-line summary.
    pub fn status_json(&self) -> serde_json::Value {
        let now = self.now();
        let c = self.lock();
        let running: Vec<_> = c
            .sched
            .running
            .iter()
            .map(|r| {
                json!({
                    "ticket": r.ticket, "kind": r.req.kind, "requester": r.req.requester, "command": r.req.command,
                    "workers": r.workers, "elapsed_secs": (now - r.granted_at).round() as u64,
                    "win_pid": r.req.win_pid, "below_normal": r.below_normal,
                    "stalled": r.stalled,
                })
            })
            .collect();
        let queue: Vec<_> = c
            .sched
            .queue
            .iter()
            .enumerate()
            .map(|(i, q)| {
                json!({
                    "ticket": q.ticket, "kind": q.req.kind, "requester": q.req.requester, "command": q.req.command,
                    "priority": q.req.priority, "position": i + 1,
                    "eta_secs": c.eta_secs(q.ticket, now).map(|e| e.round() as u64),
                })
            })
            .collect();
        let user_mode = c.user_mode();
        let line = format!(
            "testdeck: {} running, {} queued{}{}, cpu {:.0}%, free RAM {:.1} GB",
            running.len(),
            queue.len(),
            if c.sched.paused { ", paused" } else { "" },
            if user_mode { ", user mode" } else { "" },
            c.load.cpu_avg,
            c.load.ram_free_gb
        );
        // Slots a new non-gate request could take right now (the PreToolUse hook reads it); load limits not included.
        let limit = if user_mode {
            1
        } else {
            c.config.slots as usize
        };
        let gate_running = c
            .sched
            .running
            .iter()
            .any(|r| r.req.kind == crate::scheduler::Kind::Gate);
        let free_slots = if c.sched.paused || gate_running {
            0
        } else {
            let tsc = c
                .sched
                .running
                .iter()
                .filter(|r| r.req.kind == crate::scheduler::Kind::Tsc)
                .count();
            limit.saturating_sub(running.len() - tsc + c.foreign_busy as usize)
        };
        json!({
            "line": line, "paused": c.sched.paused, "user_mode": user_mode, "foreign_busy": c.foreign_busy,
            "free_slots": free_slots,
            "load": { "cpu_avg": c.load.cpu_avg, "ram_free_gb": c.load.ram_free_gb },
            "running": running, "queue": queue,
            "gate_asking": c.sched.gate_asking, "gates_held": c.env(now).gates_held,
        })
    }
}

impl AppState {
    /// Testdeck's data folder (`docs/PHASE2.md`): the folder of the active config file, else the add-on's data folder.
    pub fn data_dir(&self) -> PathBuf {
        let config = self
            .lock()
            .config_path
            .clone()
            .unwrap_or_else(Config::default_path);
        config.parent().map(PathBuf::from).unwrap_or_default()
    }
}

/// Gives each running non-gate job without cores its `workers` least-used cores, highest first, so core 0 stays free
/// for Windows and the Testdeck window. With `cpu_cap` off, locked jobs get every core back: returns those trees.
fn assign_cores(c: &mut Core) -> Vec<(u32, u64)> {
    let n = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(64);
    let all = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
    if !c.config.cpu_cap {
        return c
            .sched
            .running
            .iter_mut()
            .filter(|r| r.cpu_mask != 0)
            .filter_map(|r| {
                r.cpu_mask = 0;
                r.req.win_pid.map(|p| (p, all))
            })
            .collect();
    }
    // The gate keeps every core. The others get one block each, from the highest core down in grant order, so the low
    // cores stay free for Windows and a job keeps its place while the ones after it come and go.
    let budget = (crate::scheduler::worker_budget(&c.config, n as u32) as usize).min(n);
    let running = &mut c.sched.running;
    let mut jobs: Vec<usize> = (0..running.len())
        .filter(|&i| running[i].req.kind != Kind::Gate)
        .collect();
    jobs.sort_by(|&a, &b| running[a].granted_at.total_cmp(&running[b].granted_at));
    let total: usize = jobs
        .iter()
        .map(|&i| running[i].workers.max(1) as usize)
        .sum();
    let mut top = n;
    for i in jobs {
        let w = running[i].workers.max(1) as usize;
        // Alone, or with room for everyone: a core per worker. Otherwise a share of the budget in proportion to the
        // workers, so a job that started alone with every worker gives cores back to the next one.
        let share = if total <= budget {
            w
        } else {
            (budget * w / total).max(1)
        }
        .min(n);
        if share > top {
            // More jobs than cores: start again from the top and let blocks overlap.
            top = n;
        }
        running[i].cpu_mask = (top - share..top).fold(0, |m, k| m | 1 << k);
        top -= share;
    }
    Vec::new()
}
