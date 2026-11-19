//! jt.sh result cache and dedupe (phase 2 feature 1). jt.sh computes the key in bash (its arguments, cwd,
//! `git rev-parse HEAD` and a hash of `git diff HEAD` plus the untracked files under the cwd) before it takes a
//! slot, so a hit needs no slot. Entries live in `cache.db` in the data folder; the claims of runs in progress live
//! in memory only. Answers are plain text (the bash client has no `jq`): the first line is the status word.
use crate::broker::LONG_POLL;
use crate::state::Shared;
use axum::extract::State;
use axum::Json;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

/// A claim older than this is dropped even when its shell still runs (a hung run must not block the key forever).
const CLAIM_MAX_SECS: f64 = 6.0 * 3600.0;

struct Claim {
    win_pid: Option<u32>,
    since: f64,
}

struct Cache {
    db: Mutex<Connection>,
    running: Mutex<HashMap<String, Claim>>,
    /// Woken when a claim ends (stored or abandoned), so joined runs answer at once.
    done: Notify,
}

static CACHE: OnceLock<Cache> = OnceLock::new();

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn cache(s: &Shared) -> &'static Cache {
    CACHE.get_or_init(|| {
        let path = s.data_dir().join("cache.db");
        let open = || -> rusqlite::Result<Connection> {
            let c = Connection::open(&path)?;
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS results (key TEXT PRIMARY KEY, summary TEXT NOT NULL,
                 exit_code INTEGER NOT NULL, created_unix REAL NOT NULL)",
            )?;
            Ok(c)
        };
        // An unwritable data folder costs only persistence across broker restarts.
        let db = open().or_else(|e| {
            eprintln!("testdeck: cache {}: {e}; using an in-memory cache", path.display());
            let c = Connection::open_in_memory()?;
            c.execute_batch(
                "CREATE TABLE results (key TEXT PRIMARY KEY, summary TEXT NOT NULL, exit_code INTEGER NOT NULL,
                 created_unix REAL NOT NULL)",
            )?;
            Ok::<_, rusqlite::Error>(c)
        });
        Cache {
            db: Mutex::new(db.expect("in-memory SQLite")),
            running: Mutex::new(HashMap::new()),
            done: Notify::new(),
        }
    })
}

/// `hit <exit code> <age secs>` and the stored summary, when a fresh entry exists.
fn hit(c: &Cache, key: &str, ttl_hours: u32) -> Option<String> {
    let min_created = unix_now() - f64::from(ttl_hours) * 3600.0;
    let row: Option<(String, i32, f64)> = lock(&c.db)
        .query_row(
            "SELECT summary, exit_code, created_unix FROM results WHERE key = ?1 AND created_unix >= ?2",
            params![key, min_created],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .unwrap_or(None);
    row.map(|(summary, code, created)| {
        format!(
            "hit {code} {}\n{summary}",
            (unix_now() - created).max(0.0) as u64
        )
    })
}

fn claimed(s: &Shared, c: &Cache, key: &str) -> bool {
    let mut running = lock(&c.running);
    let live = running.get(key).is_some_and(|cl| {
        unix_now() - cl.since < CLAIM_MAX_SECS && cl.win_pid.is_none_or(|p| s.proc_ctl.is_alive(p))
    });
    if !live {
        running.remove(key);
    }
    live
}

/// Never stored: a failed run whose suites did not run or that wrote no JSON (the next run may well differ).
/// Nor a run in which no test passed or failed (all skipped): on the `--rdb` path rdb-lock's jest guard turns that
/// exit 0 into 3 only after the nested jt.sh has stored it.
fn storable(code: i32, summary: &str) -> bool {
    let nothing_ran = summary
        .lines()
        .any(|l| l.starts_with("Tests:") && l.contains(" 0 passed") && !l.contains("failed"));
    !nothing_ran
        && (code == 0
            || ![
                "SUITE FAIL",
                "suites failed to run",
                "jt: jest wrote no JSON",
            ]
            .iter()
            .any(|m| summary.contains(m)))
}

#[derive(Deserialize)]
pub struct CacheBody {
    key: String,
    win_pid: Option<u32>,
    exit_code: Option<i32>,
    summary: Option<String>,
}

/// `POST /cache/lookup {key, win_pid?}` -> `hit ...`, `running` (an identical run is in progress: join it with
/// `/cache/wait`), `miss` (the caller now owns the key until `/cache/store`) or `off` (cache disabled).
pub async fn lookup(State(s): State<Shared>, Json(b): Json<CacheBody>) -> String {
    let (enabled, ttl) = {
        let core = s.lock();
        (core.config.cache_enabled, core.config.cache_ttl_hours)
    };
    if !enabled || b.key.is_empty() {
        return "off".into();
    }
    let c = cache(&s);
    if let Some(h) = hit(c, &b.key, ttl) {
        return h;
    }
    if claimed(&s, c, &b.key) {
        return "running".into();
    }
    lock(&c.running).insert(
        b.key,
        Claim {
            win_pid: b.win_pid,
            since: unix_now(),
        },
    );
    "miss".into()
}

/// `POST /cache/wait {key}`: long-polls up to `LONG_POLL` -> `hit ...`, `running` (poll again) or `gone` (the run
/// ended without a stored result or its shell died: run normally).
pub async fn wait(State(s): State<Shared>, Json(b): Json<CacheBody>) -> String {
    let c = cache(&s);
    let deadline = tokio::time::Instant::now() + LONG_POLL;
    loop {
        let notified = c.done.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let ttl = s.lock().config.cache_ttl_hours;
        if let Some(h) = hit(c, &b.key, ttl) {
            return h;
        }
        if !claimed(&s, c, &b.key) {
            return "gone".into();
        }
        if tokio::time::Instant::now() >= deadline {
            return "running".into();
        }
        // The 1 s tick notices a dead owner shell, which wakes nobody.
        tokio::select! {
            _ = &mut notified => {}
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

/// `POST /cache/store {key, exit_code?, summary?}`: ends the caller's claim and stores the result when both fields
/// are given and the result is storable; without them it only abandons the claim.
pub async fn store(State(s): State<Shared>, Json(b): Json<CacheBody>) -> Json<Value> {
    let (enabled, ttl) = {
        let core = s.lock();
        (core.config.cache_enabled, core.config.cache_ttl_hours)
    };
    let c = cache(&s);
    if let (true, Some(code), Some(summary)) = (enabled, b.exit_code, b.summary.as_deref()) {
        if storable(code, summary) {
            let now = unix_now();
            let db = lock(&c.db);
            let r = db
                .execute(
                    "INSERT OR REPLACE INTO results (key, summary, exit_code, created_unix) VALUES (?1, ?2, ?3, ?4)",
                    params![b.key, summary, code, now],
                )
                .and_then(|_| {
                    db.execute("DELETE FROM results WHERE created_unix < ?1", params![now - f64::from(ttl) * 3600.0])
                });
            if let Err(e) = r {
                eprintln!("testdeck: cache store: {e}");
            }
        }
    }
    lock(&c.running).remove(&b.key);
    c.done.notify_waiters();
    Json(json!({"status": "ok"}))
}

#[derive(Deserialize)]
pub struct FirstBody {
    #[serde(default)]
    requester: String,
    #[serde(default)]
    cwd: String,
}

/// `POST /failed_first {requester, cwd}` (phase 2 feature 4): the failing suites of the last jest job with the same
/// requester and cwd in `history.db`, one absolute path per line; empty when there is none.
pub async fn failed_first(State(s): State<Shared>, Json(b): Json<FirstBody>) -> String {
    let path = s.data_dir().join("history.db");
    let read = move || -> rusqlite::Result<Option<String>> {
        let c = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        c.busy_timeout(Duration::from_secs(2))?;
        c.query_row(
            "SELECT failing_suites FROM jobs WHERE kind = 'jest' AND requester = ?1 AND cwd = ?2
             ORDER BY id DESC LIMIT 1",
            params![b.requester, b.cwd],
            |r| r.get(0),
        )
        .optional()
    };
    let json = tokio::task::spawn_blocking(read)
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
        .unwrap_or_default();
    serde_json::from_str::<Vec<String>>(&json)
        .unwrap_or_default()
        .join("\n")
}
