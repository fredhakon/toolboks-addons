//! Job history in SQLite (`history.db` in the add-on's data folder): one row per ended job, one row per failing suite.
use crate::scheduler::{EndReason, Kind};
use crate::state::{FinishedJob, JobSink};
use anyhow::Context;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ticket INTEGER NOT NULL,
    kind TEXT NOT NULL,
    requester TEXT NOT NULL,
    command TEXT NOT NULL,
    cwd TEXT NOT NULL,
    workers INTEGER NOT NULL,
    started_unix REAL NOT NULL,
    ended_unix REAL NOT NULL,
    duration_secs REAL NOT NULL,
    exit_code INTEGER,
    end_reason TEXT NOT NULL,
    passed INTEGER,
    failed INTEGER,
    suites_total INTEGER,
    failing_suites TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS jobs_command ON jobs (command, id);
CREATE INDEX IF NOT EXISTS jobs_ended ON jobs (ended_unix);
CREATE TABLE IF NOT EXISTS job_suites (
    job_id INTEGER NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    suite TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS job_suites_suite ON job_suites (suite);
";

pub fn kind_str(k: Kind) -> &'static str {
    match k {
        Kind::Jest => "jest",
        Kind::Build => "build",
        Kind::Tsc => "tsc",
        Kind::Lint => "lint",
        Kind::Gate => "gate",
    }
}

fn reason_str(r: EndReason) -> &'static str {
    match r {
        EndReason::Released => "released",
        EndReason::Stale => "stale",
        EndReason::Killed => "killed",
        EndReason::Requeued => "requeued",
    }
}

/// One ended job as the history table shows it.
#[derive(Clone, Debug)]
pub struct JobRow {
    pub kind: String,
    pub requester: String,
    pub command: String,
    pub ended_unix: f64,
    pub duration_secs: f64,
    pub exit_code: Option<i32>,
    pub end_reason: String,
    pub passed: Option<u32>,
    pub failed: Option<u32>,
    pub suites_total: Option<u32>,
    pub failing_suites: Vec<String>,
}

/// Averages over the jobs that ran to the end (released), per command.
#[derive(Clone, Debug)]
pub struct CommandStat {
    pub command: String,
    pub kind: String,
    pub runs: u32,
    pub avg_secs: f64,
    pub failed_runs: u32,
    pub last_unix: f64,
}

/// A suite that failed at least once, with the average duration of the jobs it failed in.
#[derive(Clone, Debug)]
pub struct SuiteStat {
    pub suite: String,
    pub failures: u32,
    pub avg_job_secs: f64,
    pub last_unix: f64,
}

pub struct History {
    conn: Mutex<Connection>,
}

impl History {
    /// `history.db` in the folder of the given config file.
    pub fn path_beside(config: &Path) -> PathBuf {
        config.with_file_name("history.db")
    }

    pub fn open(path: &Path) -> anyhow::Result<History> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        // Foreign keys are per connection; the cascade removes a job's suites with the job.
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.execute_batch(SCHEMA)?;
        Ok(History {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn insert(&self, j: &FinishedJob) -> rusqlite::Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        let suites = serde_json::to_string(&j.failing_suites).unwrap_or_else(|_| "[]".into());
        tx.execute(
            "INSERT INTO jobs (ticket, kind, requester, command, cwd, workers, started_unix, ended_unix, duration_secs,
                exit_code, end_reason, passed, failed, suites_total, failing_suites)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                j.ticket,
                kind_str(j.kind),
                j.requester,
                j.command,
                j.cwd,
                j.workers,
                j.started_unix,
                j.ended_unix,
                j.duration_secs,
                j.exit_code,
                reason_str(j.end_reason),
                j.passed,
                j.failed,
                j.suites_total,
                suites
            ],
        )?;
        let id = tx.last_insert_rowid();
        for s in &j.failing_suites {
            tx.execute(
                "INSERT INTO job_suites (job_id, suite) VALUES (?1, ?2)",
                params![id, s],
            )?;
        }
        tx.commit()
    }

    /// Newest first; `filter` matches requester, command or kind (case-insensitive substring).
    pub fn recent(&self, filter: &str, limit: u32) -> Vec<JobRow> {
        let pat = format!("%{}%", filter.trim().replace(['%', '_'], ""));
        let c = self.conn();
        let run = || -> rusqlite::Result<Vec<JobRow>> {
            let mut st = c.prepare_cached(
                "SELECT kind, requester, command, ended_unix, duration_secs, exit_code, end_reason, passed, failed,
                    suites_total, failing_suites
                 FROM jobs WHERE requester LIKE ?1 OR command LIKE ?1 OR kind LIKE ?1
                 ORDER BY id DESC LIMIT ?2",
            )?;
            let rows = st.query_map(params![pat, limit], |r| {
                let suites: String = r.get(10)?;
                Ok(JobRow {
                    kind: r.get(0)?,
                    requester: r.get(1)?,
                    command: r.get(2)?,
                    ended_unix: r.get(3)?,
                    duration_secs: r.get(4)?,
                    exit_code: r.get(5)?,
                    end_reason: r.get(6)?,
                    passed: r.get(7)?,
                    failed: r.get(8)?,
                    suites_total: r.get(9)?,
                    failing_suites: serde_json::from_str(&suites).unwrap_or_default(),
                })
            })?;
            rows.collect()
        };
        log_err("recent", run())
    }

    pub fn command_stats(&self) -> Vec<CommandStat> {
        let c = self.conn();
        let run = || -> rusqlite::Result<Vec<CommandStat>> {
            let mut st = c.prepare_cached(
                "SELECT command, kind, COUNT(*), AVG(duration_secs),
                    SUM(CASE WHEN COALESCE(exit_code, 1) != 0 THEN 1 ELSE 0 END), MAX(ended_unix)
                 FROM jobs WHERE end_reason = 'released' GROUP BY command, kind ORDER BY MAX(ended_unix) DESC",
            )?;
            let rows = st.query_map([], |r| {
                Ok(CommandStat {
                    command: r.get(0)?,
                    kind: r.get(1)?,
                    runs: r.get(2)?,
                    avg_secs: r.get(3)?,
                    failed_runs: r.get(4)?,
                    last_unix: r.get(5)?,
                })
            })?;
            rows.collect()
        };
        log_err("command_stats", run())
    }

    /// Per failing suite; there is no per-suite timing in a job record, so the duration is the job's.
    pub fn suite_stats(&self) -> Vec<SuiteStat> {
        let c = self.conn();
        let run = || -> rusqlite::Result<Vec<SuiteStat>> {
            let mut st = c.prepare_cached(
                "SELECT s.suite, COUNT(*), AVG(j.duration_secs), MAX(j.ended_unix)
                 FROM job_suites s JOIN jobs j ON j.id = s.job_id
                 GROUP BY s.suite ORDER BY COUNT(*) DESC, MAX(j.ended_unix) DESC LIMIT 200",
            )?;
            let rows = st.query_map([], |r| {
                Ok(SuiteStat {
                    suite: r.get(0)?,
                    failures: r.get(1)?,
                    avg_job_secs: r.get(2)?,
                    last_unix: r.get(3)?,
                })
            })?;
            rows.collect()
        };
        log_err("suite_stats", run())
    }

    /// Average duration per command; a thread in `main.rs` copies it into `Core::job_stats` for the queue ETA.
    pub fn job_averages(&self) -> HashMap<String, f64> {
        self.command_stats()
            .into_iter()
            .map(|s| (s.command, s.avg_secs))
            .collect()
    }

    /// The suite count of the newest job of this command that had one (the expected total for progress).
    pub fn last_suite_total(&self, command: &str) -> Option<u32> {
        let c = self.conn();
        let r = c.query_row(
            "SELECT suites_total FROM jobs WHERE command = ?1 AND suites_total > 0 ORDER BY id DESC LIMIT 1",
            params![command],
            |r| r.get::<_, u32>(0),
        );
        match r {
            Ok(n) => Some(n),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => {
                eprintln!("testdeck: history last_suite_total: {e}");
                None
            }
        }
    }
}

fn log_err<T: Default>(what: &str, r: rusqlite::Result<T>) -> T {
    r.unwrap_or_else(|e| {
        eprintln!("testdeck: history {what}: {e}");
        T::default()
    })
}

impl JobSink for History {
    fn record(&self, job: &FinishedJob) {
        if let Err(e) = self.insert(job) {
            eprintln!(
                "testdeck: history record failed for ticket {}: {e}",
                job.ticket
            );
        }
    }
}
