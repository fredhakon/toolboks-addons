//! Pure queue and admission logic. No IO and no clock: callers pass the time (`Secs` on their own monotonic
//! clock) and the machine load in, so every rule in docs/PLAN.md "Admission rules" stays deterministic.
use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

pub type Ticket = u64;
pub type Secs = f64;

/// A queued ticket that is not re-polled for this long is dropped (the client is gone).
pub const QUEUE_POLL_TIMEOUT: Secs = 30.0;
/// When nothing runs, the head is granted after waiting this long, whatever the load.
pub const STARVATION_SECS: Secs = 120.0;
/// At most one load-based grant per this many seconds: the CPU average needs time to see the new job.
pub const GRANT_SPACING_SECS: Secs = 3.0;
/// A running ticket whose Windows pid is dead is released once its heartbeats are this late (clients beat every
/// 5 s, so two beats may be lost or slow).
pub const DEAD_PID_GRACE_SECS: Secs = 12.0;
/// Expected duration of a command without history.
pub const DEFAULT_EXPECTED_SECS: Secs = 120.0;
/// "Later" on the gate ask holds the gate this long.
pub const GATE_SNOOZE_SECS: Secs = 1800.0;
/// How long a cancelled ticket still answers "cancelled" to a re-poll.
const CANCEL_MEMORY_SECS: Secs = 120.0;

/// Whether `now_min` (local minutes after midnight) lies in the window `start`..`end` ("HH:MM"), which may cross
/// midnight. An unparsable or empty (start == end) window counts as open, so a typo never parks gates forever.
pub fn in_window(now_min: u32, start: &str, end: &str) -> bool {
    let parse = |s: &str| {
        let (h, m) = s.trim().split_once(':')?;
        let (h, m) = (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?);
        (h < 24 && m < 60).then_some(h * 60 + m)
    };
    match (parse(start), parse(end)) {
        (Some(s), Some(e)) if s < e => (s..e).contains(&now_min),
        (Some(s), Some(e)) if s > e => now_min >= s || now_min < e,
        _ => true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Jest,
    Build,
    Tsc,
    Lint,
    Gate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub kind: Kind,
    pub cwd: String,
    pub command: String,
    pub requester: String,
    pub priority: i32,
    pub log_glob: Option<String>,
    /// MSYS pid of the holding shell; written to the mirrored slot dir for the `kill -0` check of old scripts.
    pub shell_pid: Option<u32>,
    /// Windows pid used for liveness, priority and kill.
    pub win_pid: Option<u32>,
    /// The client can rerun its command, so the RAM guard may stop it and ask it to requeue (docs/PHASE2.md).
    #[serde(default)]
    pub requeue_ok: bool,
    /// This request replaces a ticket that was stopped for low RAM; it goes to the queue head.
    #[serde(default)]
    pub requeue_of: Option<Ticket>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Queued {
    pub ticket: Ticket,
    pub req: Request,
    pub seq: u64,
    pub enqueued_at: Secs,
    pub last_poll: Secs,
}

#[derive(Clone, Debug, Serialize)]
pub struct Running {
    pub ticket: Ticket,
    pub req: Request,
    pub workers: u32,
    pub granted_at: Secs,
    pub last_heartbeat: Secs,
    /// The process tree was set to BELOW_NORMAL (user mode) and must be restored when user mode ends.
    pub below_normal: bool,
    /// No new PASS/FAIL line in the jest log for `stall_minutes` (set by the guard each tick).
    pub stalled: bool,
    /// CPU cores the process tree is locked to (bit n = core n); 0 = not locked.
    pub cpu_mask: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndReason {
    Released,
    Stale,
    Killed,
    /// Stopped by the RAM guard; the client reruns it with `requeue_of`.
    Requeued,
}

#[derive(Clone, Debug)]
pub struct Finished {
    pub run: Running,
    pub ended_at: Secs,
    pub exit_code: Option<i32>,
    pub reason: EndReason,
}

/// What admission needs to know about the machine right now.
#[derive(Clone, Copy, Debug, Default)]
pub struct Env {
    pub now: Secs,
    pub cpu_avg: f32,
    pub ram_free_gb: f32,
    pub user_mode: bool,
    /// Slot dirs held by scripts that do not know the broker.
    pub foreign_busy: u32,
    /// Night window on and now outside it: queued gates are skipped.
    pub gates_held: bool,
    /// User mode with `gate_ask_in_user_mode`: a gate that could start waits for Allow.
    pub gate_ask: bool,
    /// Logical CPU cores of this PC.
    pub cores: u32,
}

/// Workers the running jobs share: the configured budget, or every logical core but two (one physical core stays free
/// for Windows and the UI).
pub fn worker_budget(cfg: &Config, cores: u32) -> u32 {
    if cfg.worker_budget > 0 {
        cfg.worker_budget
    } else {
        cores.saturating_sub(2).max(1)
    }
}

/// Jobs that share the worker budget: everything but type checks (one core each, own lane) and the gate (alone).
/// A job granted beside others gets this many times its fair share of workers (at most the budget). Free RAM still
/// caps each grant; `state::assign_cores` keeps the jobs to their share of the cores while they run together.
const SPLIT_OVERSUB: u32 = 2;

/// Cores the PC leaves idle by the 10 s CPU average, minus core 0, which stays free for Windows and the UI.
fn idle_cores(env: &Env) -> u32 {
    let idle = env.cores as f32 * (100.0 - env.cpu_avg).clamp(0.0, 100.0) / 100.0;
    (idle.floor() as u32).saturating_sub(1)
}

fn shares_budget(kind: Kind) -> bool {
    !matches!(kind, Kind::Tsc | Kind::Gate)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Poll {
    Granted { workers: u32 },
    Waiting { position: usize },
    Cancelled { reason: String },
    Unknown,
}

#[derive(Debug, Default)]
pub struct Scheduler {
    /// In grant order; index 0 is the head.
    pub queue: Vec<Queued>,
    pub running: Vec<Running>,
    pub paused: bool,
    next_ticket: Ticket,
    next_seq: u64,
    last_load_grant: Option<Secs>,
    cancelled: HashMap<Ticket, (String, Secs)>,
    /// Tickets stopped for low RAM; their heartbeat answers "requeue" for a while.
    requeued: HashMap<Ticket, Secs>,
    /// Gates pushed back with "Later", until the given time.
    pub gate_snooze: HashMap<Ticket, Secs>,
    /// Gates the user allowed through the gate ask.
    pub gate_allowed: HashSet<Ticket>,
    /// The gate that could start and waits for Allow / Later (set by `admit`).
    pub gate_asking: Option<Ticket>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            next_ticket: 1,
            ..Self::default()
        }
    }

    fn rank(gate_first: bool, q: &Queued) -> (bool, i32, Reverse<u64>) {
        (
            gate_first && q.req.kind == Kind::Gate,
            q.req.priority,
            Reverse(q.seq),
        )
    }

    /// Inserts before the first entry that ranks lower, so manual reorders of the others stay as they are.
    fn insert(&mut self, q: Queued, gate_first: bool) {
        let r = Self::rank(gate_first, &q);
        let i = self
            .queue
            .iter()
            .position(|e| r > Self::rank(gate_first, e))
            .unwrap_or(self.queue.len());
        self.queue.insert(i, q);
    }

    pub fn enqueue(&mut self, req: Request, now: Secs, gate_first: bool) -> Ticket {
        let ticket = self.next_ticket.max(1);
        self.next_ticket = ticket + 1;
        let seq = self.next_seq;
        self.next_seq += 1;
        let q = Queued {
            ticket,
            req,
            seq,
            enqueued_at: now,
            last_poll: now,
        };
        if q.req.requeue_of.is_some() {
            self.queue.insert(0, q);
        } else {
            self.insert(q, gate_first);
        }
        ticket
    }

    fn index(&self, t: Ticket) -> Option<usize> {
        self.queue.iter().position(|q| q.ticket == t)
    }

    /// 1-based queue position.
    pub fn position(&self, t: Ticket) -> Option<usize> {
        self.index(t).map(|i| i + 1)
    }

    /// Answers a client poll; a poll keeps a queued ticket alive.
    pub fn poll(&mut self, t: Ticket, now: Secs) -> Poll {
        if let Some(r) = self.running.iter().find(|r| r.ticket == t) {
            return Poll::Granted { workers: r.workers };
        }
        if let Some(i) = self.index(t) {
            self.queue[i].last_poll = now;
            return Poll::Waiting { position: i + 1 };
        }
        match self.cancelled.get(&t) {
            Some((reason, _)) => Poll::Cancelled {
                reason: reason.clone(),
            },
            None => Poll::Unknown,
        }
    }

    /// Workers for the head of the queue, of `kind`, called before it leaves the queue. jest cannot change its worker
    /// count once started, so the split is decided here: the gate, or a job alone with nothing waiting behind it, gets
    /// the whole budget; otherwise the budget is split evenly between the jobs that run or will run beside it (at most
    /// `slots`), times `SPLIT_OVERSUB`: the count is fixed for the life of the job but its cores are not, so a job
    /// that started beside others can use the whole PC once they end (`state::assign_cores`). In user mode the count
    /// follows the measured idle cores (core 0 stays free), at least `workers_user_mode`. Free RAM caps the count.
    fn workers_for(&self, cfg: &Config, env: &Env, kind: Kind) -> u32 {
        if kind == Kind::Tsc {
            return 1;
        }
        let budget = worker_budget(cfg, env.cores);
        let share = if env.user_mode {
            let floor = cfg.workers_user_mode.max(1);
            if cfg.workers_user_mode_idle {
                floor.max(idle_cores(env).min(budget))
            } else {
                floor
            }
        } else if kind == Kind::Gate {
            budget
        } else {
            let running = self
                .running
                .iter()
                .filter(|r| shares_budget(r.req.kind))
                .count() as u32
                + env.foreign_busy;
            // Includes the head itself.
            let waiting = self
                .queue
                .iter()
                .filter(|q| shares_budget(q.req.kind))
                .count() as u32;
            let fair = budget / (running + waiting).min(cfg.slots.max(1)).max(running + 1);
            (fair * SPLIT_OVERSUB).min(budget)
        };
        if env.ram_free_gb <= 0.0 {
            return share.max(1);
        }
        let ram_cap = ((env.ram_free_gb - cfg.ram_free_min_gb) / cfg.ram_per_worker_gb.max(0.1))
            .floor()
            .max(1.0) as u32;
        share.min(ram_cap).max(1)
    }

    /// Grants from the head of the queue while the rules allow; returns (ticket, workers) per grant.
    pub fn admit(&mut self, cfg: &Config, env: &Env) -> Vec<(Ticket, u32)> {
        let mut grants = Vec::new();
        self.gate_asking = None;
        if self.paused {
            return grants;
        }
        // Held gates (outside the night window, or snoozed with Later) neither sit at the head nor block others.
        // A request whose lane is full is skipped, so a type check never waits behind jest (or the other way round).
        let mut skipped: HashSet<Ticket> = HashSet::new();
        while let Some(i) = self
            .queue
            .iter()
            .position(|q| !self.gate_held(q, env) && !skipped.contains(&q.ticket))
        {
            let head = &self.queue[i];
            let kind = head.req.kind;
            if cfg.gate_exclusive && self.running.iter().any(|r| r.req.kind == Kind::Gate) {
                break;
            }
            let workers = if cfg.gate_exclusive && head.req.kind == Kind::Gate {
                // The gate ignores load: it only waits for the PC to drain.
                if !self.running.is_empty() || env.foreign_busy > 0 {
                    break;
                }
                if self.must_ask(head, env) {
                    self.gate_asking = Some(head.ticket);
                    break;
                }
                self.workers_for(cfg, env, kind)
            } else {
                let tsc = head.req.kind == Kind::Tsc;
                let tsc_running = self
                    .running
                    .iter()
                    .filter(|r| r.req.kind == Kind::Tsc)
                    .count() as u32;
                let (limit, used) = if tsc {
                    (
                        if env.user_mode {
                            1
                        } else {
                            cfg.tsc_slots.max(1)
                        },
                        tsc_running,
                    )
                } else {
                    (
                        if env.user_mode { 1 } else { cfg.slots.max(1) },
                        self.running.len() as u32 - tsc_running + env.foreign_busy,
                    )
                };
                if used >= limit {
                    skipped.insert(head.ticket);
                    continue;
                }
                let starving =
                    self.running.is_empty() && env.now - head.enqueued_at >= STARVATION_SECS;
                if !starving {
                    let load_ok =
                        env.cpu_avg < cfg.cpu_threshold && env.ram_free_gb >= cfg.ram_free_min_gb;
                    let spaced = self
                        .last_load_grant
                        .is_none_or(|t| env.now - t >= GRANT_SPACING_SECS);
                    if !(load_ok && spaced) {
                        break;
                    }
                }
                if self.must_ask(head, env) {
                    self.gate_asking = Some(head.ticket);
                    break;
                }
                self.last_load_grant = Some(env.now);
                self.workers_for(cfg, env, kind)
            };
            let q = self.queue.remove(i);
            grants.push((q.ticket, workers));
            self.running.push(Running {
                ticket: q.ticket,
                req: q.req,
                workers,
                granted_at: env.now,
                last_heartbeat: env.now,
                below_normal: false,
                stalled: false,
                cpu_mask: 0,
            });
        }
        grants
    }

    fn gate_held(&self, q: &Queued, env: &Env) -> bool {
        q.req.kind == Kind::Gate
            && (env.gates_held
                || self
                    .gate_snooze
                    .get(&q.ticket)
                    .is_some_and(|&until| env.now < until))
    }

    fn must_ask(&self, q: &Queued, env: &Env) -> bool {
        q.req.kind == Kind::Gate && env.gate_ask && !self.gate_allowed.contains(&q.ticket)
    }

    /// Gate ask "Allow": the queued gate may start.
    pub fn allow_gate(&mut self, t: Ticket) -> bool {
        if self.index(t).is_none() {
            return false;
        }
        self.gate_snooze.remove(&t);
        self.gate_allowed.insert(t)
    }

    /// Gate ask "Later": the queued gate is held for `GATE_SNOOZE_SECS`.
    pub fn snooze_gate(&mut self, t: Ticket, now: Secs) -> bool {
        if self.index(t).is_none() {
            return false;
        }
        self.gate_allowed.remove(&t);
        self.gate_snooze.insert(t, now + GATE_SNOOZE_SECS);
        true
    }

    /// The client started its command; also counts as a heartbeat. `None` fields keep their value.
    pub fn started(
        &mut self,
        t: Ticket,
        win_pid: Option<u32>,
        log_glob: Option<String>,
        now: Secs,
    ) -> bool {
        let Some(r) = self.running.iter_mut().find(|r| r.ticket == t) else {
            return false;
        };
        if win_pid.is_some() {
            r.req.win_pid = win_pid;
        }
        if log_glob.is_some() {
            r.req.log_glob = log_glob;
        }
        r.last_heartbeat = now;
        true
    }

    pub fn heartbeat(&mut self, t: Ticket, now: Secs) -> bool {
        self.started(t, None, None, now)
    }

    fn end(
        &mut self,
        t: Ticket,
        now: Secs,
        exit_code: Option<i32>,
        reason: EndReason,
    ) -> Option<Finished> {
        let i = self.running.iter().position(|r| r.ticket == t)?;
        Some(Finished {
            run: self.running.remove(i),
            ended_at: now,
            exit_code,
            reason,
        })
    }

    pub fn release(&mut self, t: Ticket, exit_code: Option<i32>, now: Secs) -> Option<Finished> {
        self.end(t, now, exit_code, EndReason::Released)
    }

    /// Ends a running ticket as killed; the caller kills the process tree.
    pub fn kill(&mut self, t: Ticket, now: Secs) -> Option<Finished> {
        self.end(t, now, None, EndReason::Killed)
    }

    /// Ends a running ticket as stopped for low RAM; the caller kills its descendants.
    pub fn requeue(&mut self, t: Ticket, now: Secs) -> Option<Finished> {
        let f = self.end(t, now, None, EndReason::Requeued)?;
        self.requeued.insert(t, now);
        Some(f)
    }

    pub fn is_requeued(&self, t: Ticket) -> bool {
        self.requeued.contains_key(&t)
    }

    /// The client releases a ticket the RAM guard already ended as requeued (history has it): stop answering
    /// "requeue", so its heartbeat loop gets 404 and ends.
    pub fn forget_requeued(&mut self, t: Ticket) -> bool {
        self.requeued.remove(&t).is_some()
    }

    /// Cancels a queued ticket; its next poll answers `Cancelled`. Running tickets are not cancelled (use kill).
    pub fn cancel(&mut self, t: Ticket, reason: &str, now: Secs) -> bool {
        let Some(i) = self.index(t) else { return false };
        self.queue.remove(i);
        self.cancelled.insert(t, (reason.to_string(), now));
        true
    }

    /// Drops queued tickets nobody polls and releases running ones whose process is gone or that miss heartbeats.
    pub fn reap(
        &mut self,
        now: Secs,
        heartbeat_timeout: Secs,
        alive: &dyn Fn(u32) -> bool,
    ) -> Vec<Finished> {
        self.queue
            .retain(|q| now - q.last_poll <= QUEUE_POLL_TIMEOUT);
        self.cancelled
            .retain(|_, (_, at)| now - *at <= CANCEL_MEMORY_SECS);
        self.requeued
            .retain(|_, at| now - *at <= CANCEL_MEMORY_SECS);
        let queued: HashSet<Ticket> = self.queue.iter().map(|q| q.ticket).collect();
        self.gate_snooze.retain(|t, _| queued.contains(t));
        self.gate_allowed.retain(|t| queued.contains(t));
        let stale: Vec<Ticket> = self
            .running
            .iter()
            .filter(|r| {
                let silent = now - r.last_heartbeat;
                // A dead Windows pid alone proves nothing: after `exec` (rdb-lock.sh, gate.sh) Git Bash runs the
                // workload in a new Windows process, while the heartbeat subshell still sees the MSYS pid alive.
                silent > heartbeat_timeout
                    || (silent > DEAD_PID_GRACE_SECS && r.req.win_pid.is_some_and(|p| !alive(p)))
            })
            .map(|r| r.ticket)
            .collect();
        stale
            .into_iter()
            .filter_map(|t| self.end(t, now, None, EndReason::Stale))
            .collect()
    }

    /// Moves a queued ticket one place up or down.
    pub fn move_ticket(&mut self, t: Ticket, up: bool) -> bool {
        let Some(i) = self.index(t) else { return false };
        let j = match up {
            true if i > 0 => i - 1,
            false if i + 1 < self.queue.len() => i + 1,
            _ => return false,
        };
        self.queue.swap(i, j);
        true
    }

    /// Changes a queued ticket's priority and re-sorts it by the queue rule.
    pub fn set_priority(&mut self, t: Ticket, priority: i32, gate_first: bool) -> bool {
        let Some(i) = self.index(t) else { return false };
        let mut q = self.queue.remove(i);
        q.req.priority = priority;
        self.insert(q, gate_first);
        true
    }

    /// Expected wait: remaining work of running jobs plus queued jobs ahead, divided by the slot limit.
    pub fn eta_secs(
        &self,
        t: Ticket,
        now: Secs,
        limit: u32,
        expected: &dyn Fn(&str) -> Secs,
    ) -> Option<Secs> {
        let i = self.index(t)?;
        let running: Secs = self
            .running
            .iter()
            .map(|r| (expected(&r.req.command) - (now - r.granted_at)).max(0.0))
            .sum();
        let ahead: Secs = self.queue[..i]
            .iter()
            .map(|q| expected(&q.req.command))
            .sum();
        Some((running + ahead) / limit.max(1) as Secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            worker_budget: 30,
            ..Config::default()
        }
    }

    fn env(user_mode: bool, cpu_avg: f32, ram_free_gb: f32) -> Env {
        Env {
            now: 1000.0,
            cpu_avg,
            ram_free_gb,
            user_mode,
            cores: 32,
            ..Env::default()
        }
    }

    fn req(kind: Kind) -> Request {
        Request {
            kind,
            cwd: String::new(),
            command: "jest".into(),
            requester: "test".into(),
            priority: 0,
            log_glob: None,
            shell_pid: None,
            win_pid: None,
            requeue_ok: false,
            requeue_of: None,
        }
    }

    /// Workers of the first grant after queueing `kinds`.
    fn first_grant(cfg: &Config, env: &Env, kinds: &[Kind]) -> u32 {
        let mut s = Scheduler::new();
        for &k in kinds {
            s.enqueue(req(k), 0.0, true);
        }
        s.admit(cfg, env)[0].1
    }

    #[test]
    fn user_mode_on_an_idle_pc_gets_the_idle_cores_but_not_core_0() {
        // 32 cores at 10% load: 28 idle, one kept for core 0.
        assert_eq!(
            first_grant(&cfg(), &env(true, 10.0, 100.0), &[Kind::Jest]),
            27
        );
    }

    #[test]
    fn user_mode_on_a_busy_pc_keeps_the_floor() {
        assert_eq!(
            first_grant(&cfg(), &env(true, 100.0, 100.0), &[Kind::Jest]),
            1
        );
        let c = Config {
            workers_user_mode: 3,
            ..cfg()
        };
        assert_eq!(first_grant(&c, &env(true, 100.0, 100.0), &[Kind::Jest]), 3);
    }

    #[test]
    fn user_mode_idle_grant_can_be_switched_off() {
        let c = Config {
            workers_user_mode_idle: false,
            ..cfg()
        };
        assert_eq!(first_grant(&c, &env(true, 0.0, 100.0), &[Kind::Jest]), 1);
    }

    #[test]
    fn user_mode_grant_stays_inside_the_budget() {
        let c = Config {
            worker_budget: 4,
            ..cfg()
        };
        assert_eq!(first_grant(&c, &env(true, 0.0, 100.0), &[Kind::Jest]), 4);
    }

    #[test]
    fn free_ram_still_caps_user_mode_and_normal_grants() {
        // (10 - 4 GB needed) / 0.75 GB per worker = 8.
        assert_eq!(first_grant(&cfg(), &env(true, 0.0, 10.0), &[Kind::Jest]), 8);
        assert_eq!(
            first_grant(&cfg(), &env(false, 0.0, 10.0), &[Kind::Jest]),
            8
        );
    }

    #[test]
    fn a_job_beside_others_gets_more_than_its_fair_share_up_to_the_budget() {
        let c = Config { slots: 3, ..cfg() };
        let e = env(false, 0.0, 100.0);
        // Three jobs: fair share 10, granted twice that.
        assert_eq!(first_grant(&c, &e, &[Kind::Jest; 3]), 20);
        // Two jobs on two slots: twice the fair 15 is the whole budget, not more.
        assert_eq!(first_grant(&cfg(), &e, &[Kind::Jest; 2]), 30);
        assert_eq!(first_grant(&cfg(), &e, &[Kind::Jest]), 30);
        // The RAM cap still holds for the split grant.
        assert_eq!(first_grant(&c, &env(false, 0.0, 10.0), &[Kind::Jest; 3]), 8);
    }

    #[test]
    fn type_checks_stay_at_one_worker() {
        assert_eq!(first_grant(&cfg(), &env(true, 0.0, 100.0), &[Kind::Tsc]), 1);
        assert_eq!(
            first_grant(&cfg(), &env(false, 0.0, 100.0), &[Kind::Tsc]),
            1
        );
    }

    #[test]
    fn the_gate_gets_the_budget() {
        assert_eq!(
            first_grant(&cfg(), &env(false, 0.0, 100.0), &[Kind::Gate]),
            30
        );
    }
}
