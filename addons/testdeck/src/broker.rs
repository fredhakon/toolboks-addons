//! HTTP API on 127.0.0.1 (loopback peers only), the 1 s tick loop and live rebinding when `config.port` changes.
use crate::scheduler::{Kind, Poll, Request, Ticket};
use crate::state::Shared;
use axum::extract::{ConnectInfo, Request as HttpRequest, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// How long `/acquire` holds a waiting request before it answers "waiting".
pub const LONG_POLL: Duration = Duration::from_secs(20);

pub fn router(shared: Shared) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/acquire", post(acquire))
        .route("/started", post(started))
        .route("/heartbeat", post(heartbeat))
        .route("/release", post(release))
        .route("/cancel", post(cancel))
        .route("/pause", post(pause))
        .route("/resume", post(resume))
        .route("/orphans", get(orphans_list))
        .route("/orphans/kill", post(orphans_kill))
        .route("/cache/lookup", post(crate::cache::lookup))
        .route("/cache/wait", post(crate::cache::wait))
        .route("/cache/store", post(crate::cache::store))
        .route("/failed_first", post(crate::cache::failed_first))
        .route("/flaky", get(crate::known_red::flaky))
        .route(
            "/known_red",
            get(crate::known_red::get).post(crate::known_red::add),
        )
        .layer(middleware::from_fn(loopback_only))
        .with_state(shared)
}

async fn loopback_only(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: HttpRequest,
    next: Next,
) -> Response {
    if peer.ip().is_loopback() {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, Json(json!({"status": "forbidden"}))).into_response()
    }
}

fn ok(found: bool) -> Response {
    if found {
        Json(json!({"status": "ok"})).into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(json!({"status": "unknown"}))).into_response()
    }
}

async fn health() -> Json<Value> {
    Json(json!({"app": "testdeck", "version": env!("CARGO_PKG_VERSION")}))
}

async fn status(State(s): State<Shared>) -> Json<Value> {
    Json(s.status_json())
}

#[derive(Deserialize)]
struct AcquireBody {
    ticket: Option<Ticket>,
    kind: Option<Kind>,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    command: String,
    #[serde(default)]
    requester: String,
    #[serde(default)]
    priority: i32,
    log_glob: Option<String>,
    shell_pid: Option<u32>,
    win_pid: Option<u32>,
    #[serde(default)]
    requeue_ok: bool,
    requeue_of: Option<Ticket>,
}

/// New request (`kind` ...) or re-poll (`ticket`). A new waiting request answers at once so the client can show
/// its position; a re-poll waits up to `LONG_POLL` for a grant, a cancel or a position change.
async fn acquire(State(s): State<Shared>, Json(b): Json<AcquireBody>) -> Response {
    let (ticket, fresh) = match (b.ticket, b.kind) {
        (Some(t), _) => (t, false),
        (None, Some(kind)) => {
            let req = Request {
                kind,
                cwd: b.cwd,
                command: b.command,
                requester: b.requester,
                priority: b.priority,
                log_glob: b.log_glob,
                shell_pid: b.shell_pid,
                win_pid: b.win_pid,
                requeue_ok: b.requeue_ok,
                requeue_of: b.requeue_of,
            };
            (s.acquire(req), true)
        }
        (None, None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"status": "error", "reason": "kind or ticket required"})),
            )
                .into_response()
        }
    };
    let deadline = tokio::time::Instant::now() + LONG_POLL;
    let mut first_position = None;
    loop {
        let notified = s.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let reply = match s.poll(ticket) {
            (Poll::Granted { workers }, _) => {
                json!({"status": "granted", "ticket": ticket, "workers": workers})
            }
            (Poll::Cancelled { reason }, _) => {
                json!({"status": "cancelled", "ticket": ticket, "reason": reason})
            }
            (Poll::Unknown, _) => return ok(false),
            (Poll::Waiting { position }, eta) => {
                let moved = first_position.is_some_and(|p| p != position);
                if !(fresh || moved || tokio::time::Instant::now() >= deadline) {
                    first_position.get_or_insert(position);
                    tokio::select! {
                        _ = &mut notified => {}
                        _ = tokio::time::sleep_until(deadline) => {}
                    }
                    continue;
                }
                let eta = eta.unwrap_or(0.0).round() as u64;
                json!({"status": "waiting", "ticket": ticket, "position": position, "eta_secs": eta})
            }
        };
        return Json(reply).into_response();
    }
}

#[derive(Deserialize)]
struct StartedBody {
    ticket: Ticket,
    pid: Option<u32>,
    log_glob: Option<String>,
}

async fn started(State(s): State<Shared>, Json(b): Json<StartedBody>) -> Response {
    ok(s.started(b.ticket, b.pid, b.log_glob))
}

#[derive(Deserialize)]
struct TicketBody {
    ticket: Ticket,
}

async fn heartbeat(State(s): State<Shared>, Json(b): Json<TicketBody>) -> Response {
    if s.lock().sched.is_requeued(b.ticket) {
        return Json(json!({"status": "requeue"})).into_response();
    }
    ok(s.heartbeat(b.ticket))
}

async fn cancel(State(s): State<Shared>, Json(b): Json<TicketBody>) -> Response {
    ok(s.cancel(b.ticket))
}

#[derive(Deserialize)]
struct ReleaseBody {
    ticket: Ticket,
    exit_code: Option<i32>,
}

async fn release(State(s): State<Shared>, Json(b): Json<ReleaseBody>) -> Response {
    // The sink (SQLite in phase 2) and result.json reading block, so keep them off the async workers.
    ok(
        tokio::task::spawn_blocking(move || s.release(b.ticket, b.exit_code))
            .await
            .unwrap_or(false),
    )
}

async fn pause(State(s): State<Shared>) -> Response {
    s.set_paused(true);
    ok(true)
}

async fn resume(State(s): State<Shared>) -> Response {
    s.set_paused(false);
    ok(true)
}

/// `GET /orphans`: the same list as the Orphans tab (a process snapshot, so off the async workers).
async fn orphans_list(State(s): State<Shared>) -> Json<Value> {
    let rows = tokio::task::spawn_blocking(move || crate::orphans::scan(&s))
        .await
        .unwrap_or_default();
    Json(json!({ "orphans": rows }))
}

#[derive(Deserialize)]
struct OrphanKillBody {
    pid: u32,
    start_unix: u64,
}

/// `POST /orphans/kill {pid, start_unix}`: the Kill button's code path; kills only a process a fresh scan still
/// lists as the same orphan, so it cannot reach a job's tree or a reused pid.
async fn orphans_kill(State(s): State<Shared>, Json(b): Json<OrphanKillBody>) -> Response {
    ok(
        tokio::task::spawn_blocking(move || crate::orphans::kill(&s, b.pid, b.start_unix))
            .await
            .unwrap_or(false),
    )
}

/// Binds `config.port` (0 = any free port) on 127.0.0.1, starts the server and the tick loop, and returns the
/// bound port. The tasks run on the current tokio runtime until it shuts down.
pub async fn start(shared: Shared) -> anyhow::Result<u16> {
    let port = shared.lock().config.port;
    let (bound, stop) = bind(&shared, port).await?;
    tokio::spawn(tick_loop(shared, port, stop));
    Ok(bound)
}

async fn bind(shared: &Shared, port: u16) -> anyhow::Result<(u16, oneshot::Sender<()>)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let bound = listener.local_addr()?.port();
    let (stop, stopped) = oneshot::channel::<()>();
    let app = router(shared.clone()).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await;
    });
    shared.lock().bound_port = Some(bound);
    Ok((bound, stop))
}

async fn tick_loop(shared: Shared, mut port: u16, mut stop: oneshot::Sender<()>) {
    let mut every = tokio::time::interval(Duration::from_secs(1));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        let s = shared.clone();
        let _ = tokio::task::spawn_blocking(move || s.tick()).await;
        let want = shared.lock().config.port;
        if want != port {
            // One attempt per change, so a taken port does not log every second.
            port = want;
            match bind(&shared, want).await {
                Ok((bound, new_stop)) => {
                    let _ = std::mem::replace(&mut stop, new_stop).send(());
                    eprintln!("testdeck: now listening on 127.0.0.1:{bound}");
                }
                Err(e) => {
                    eprintln!("testdeck: cannot bind port {want}: {e}; keeping the old listener")
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct JestResult {
    pub passed: u32,
    pub failed: u32,
    pub suites_total: u32,
    pub failing_suites: Vec<String>,
}

/// Resolves a jest log glob and reads jest's `result.json` next to the log (written by jt.sh).
pub fn jest_result(log_glob: &str) -> Option<JestResult> {
    let log = resolve_glob(log_glob)?;
    let text = std::fs::read_to_string(log.with_file_name("result.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
    let failing_suites = v
        .get("testResults")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|t| t.get("status").and_then(Value::as_str) == Some("failed"))
                .filter_map(|t| t.get("name").and_then(Value::as_str).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Some(JestResult {
        passed: n("numPassedTests"),
        failed: n("numFailedTests"),
        suites_total: n("numTotalTestSuites"),
        failing_suites,
    })
}

/// The newest existing path that matches `pattern`.
/// shortcut: one `*` in one path component (the shape of the jt.sh log dirs); use a glob crate if more is needed.
pub fn resolve_glob(pattern: &str) -> Option<PathBuf> {
    let path = Path::new(pattern);
    let comps: Vec<_> = path.components().collect();
    let Some(i) = comps
        .iter()
        .position(|c| c.as_os_str().to_string_lossy().contains('*'))
    else {
        return path.exists().then(|| path.to_path_buf());
    };
    let base: PathBuf = comps[..i].iter().collect();
    let rest: PathBuf = comps[i + 1..].iter().collect();
    let name = comps[i].as_os_str().to_string_lossy().into_owned();
    let (pre, suf) = name.split_once('*')?;
    std::fs::read_dir(&base)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.len() >= pre.len() + suf.len() && n.starts_with(pre) && n.ends_with(suf)
        })
        .map(|e| e.path().join(&rest))
        .filter(|p| p.exists())
        .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())
}
