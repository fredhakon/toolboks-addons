//! Real-DB test lanes: is each lane's MySQL and Redis port open, who holds the lane lock, and the one action,
//! starting a down server with its existing start script. Nothing here speaks to the databases: a port check is
//! a TCP connect that is dropped at once, and a lock is read from the files `rdb-lock-lanes4.sh` writes.
use crate::state::Shared;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const POLL_EVERY: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

pub struct Server {
    pub label: &'static str,
    pub port: u16,
    /// Git Bash path of the start script.
    pub script: &'static str,
    /// Log file in the data folder.
    pub log: &'static str,
}

pub struct LaneDef {
    pub name: &'static str,
    pub mysql: Server,
    pub redis: Server,
    /// Lock directory name under the gate folder.
    pub lock: &'static str,
}

impl Server {
    /// Whether the start script is on disk; the UI disables Start when it is not.
    pub fn script_exists(&self) -> bool {
        script_file(self.script).is_file()
    }
}

/// "/d/Workspace/x.sh" (Git Bash) as a Windows path.
fn script_file(p: &str) -> PathBuf {
    let b = p.as_bytes();
    if b.len() > 3 && b[0] == b'/' && b[2] == b'/' && b[1].is_ascii_alphabetic() {
        PathBuf::from(format!(
            "{}:{}",
            (b[1] as char).to_ascii_uppercase(),
            &p[2..]
        ))
    } else {
        PathBuf::from(p)
    }
}

pub const LANES: [LaneDef; 4] = [
    LaneDef {
        name: "A",
        mysql: Server {
            label: "MySQL",
            port: 3308,
            script: "/d/Workspace/mysql-test/start-testdb.sh",
            log: "lane-a-mysql.log",
        },
        redis: Server {
            label: "Redis",
            port: 6380,
            script: "/d/Workspace/redis-test/start-redis.sh",
            log: "lane-a-redis.log",
        },
        lock: ".rdb-lock",
    },
    LaneDef {
        name: "B",
        mysql: Server {
            label: "MySQL",
            port: 3309,
            script: "/d/Workspace/mysql-test-b/start-testdb-b.sh",
            log: "lane-b-mysql.log",
        },
        redis: Server {
            label: "Redis",
            port: 6381,
            script: "/d/Workspace/redis-test/start-redis-b.sh",
            log: "lane-b-redis.log",
        },
        lock: ".rdb-lock-b",
    },
    LaneDef {
        name: "C",
        mysql: Server {
            label: "MySQL",
            port: 3310,
            script: "/d/Workspace/mysql-test-c/start-testdb-c.sh",
            log: "lane-c-mysql.log",
        },
        redis: Server {
            label: "Redis",
            port: 6382,
            script: "/d/Workspace/redis-test/start-redis-c.sh",
            log: "lane-c-redis.log",
        },
        lock: ".rdb-lock-c",
    },
    LaneDef {
        name: "D",
        mysql: Server {
            label: "MySQL",
            port: 3311,
            script: "/d/Workspace/mysql-test-d/start-testdb-d.sh",
            log: "lane-d-mysql.log",
        },
        redis: Server {
            label: "Redis",
            port: 6383,
            script: "/d/Workspace/redis-test/start-redis-d.sh",
            log: "lane-d-redis.log",
        },
        lock: ".rdb-lock-d",
    },
];

#[derive(Clone, Debug, Default)]
pub struct Holder {
    /// The Git Bash pid of the lock wrapper, as written in the `pid` file.
    pub pid: String,
    pub age_secs: Option<u64>,
    /// "<cwd>: <command>" as recorded; empty while the holder is still starting.
    pub cmd: String,
}

#[derive(Clone, Debug, Default)]
pub struct LaneState {
    pub mysql_open: bool,
    pub redis_open: bool,
    pub holder: Option<Holder>,
}

/// Polls both lanes every 5 s on its own thread; the thread ends when the poller is dropped.
pub struct Poller {
    state: Arc<Mutex<Vec<LaneState>>>,
}

impl Poller {
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(Vec::new()));
        let weak: Weak<Mutex<Vec<LaneState>>> = Arc::downgrade(&state);
        std::thread::spawn(move || {
            while let Some(s) = weak.upgrade() {
                let fresh: Vec<LaneState> = LANES.iter().map(read_lane).collect();
                if let Ok(mut g) = s.lock() {
                    *g = fresh;
                }
                drop(s);
                std::thread::sleep(POLL_EVERY);
            }
        });
        Self { state }
    }

    /// Empty until the first poll is done.
    pub fn snapshot(&self) -> Vec<LaneState> {
        self.state.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

fn port_open(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        CONNECT_TIMEOUT,
    )
    .is_ok()
}

fn read_lane(def: &LaneDef) -> LaneState {
    LaneState {
        mysql_open: port_open(def.mysql.port),
        redis_open: port_open(def.redis.port),
        holder: read_holder(def),
    }
}

/// The gate folder: `RDB_LOCK_ROOT` as the lock script reads it, else `~/.claude/gate`.
fn gate_dir() -> Option<PathBuf> {
    if let Some(r) = std::env::var_os("RDB_LOCK_ROOT") {
        return Some(PathBuf::from(r));
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join(".claude").join("gate"))
}

fn read_holder(def: &LaneDef) -> Option<Holder> {
    let dir = gate_dir()?.join(def.lock);
    if !dir.is_dir() {
        return None;
    }
    let read = |name: &str| {
        std::fs::read_to_string(dir.join(name))
            .map(|t| t.trim().to_string())
            .unwrap_or_default()
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // `since` is written once the lock is taken; before that the directory's own age is the best guide.
    let started = read("since").parse::<u64>().ok().or_else(|| {
        let m = std::fs::metadata(&dir).ok()?.modified().ok()?;
        Some(m.duration_since(UNIX_EPOCH).ok()?.as_secs())
    });
    Some(Holder {
        pid: read("pid"),
        age_secs: started.map(|s| now.saturating_sub(s)),
        cmd: read("cmd"),
    })
}

/// Testdeck's data folder: beside the active config file, else the add-on's data folder.
pub fn data_dir(shared: &Shared) -> PathBuf {
    let cfg = shared
        .lock()
        .config_path
        .clone()
        .unwrap_or_else(crate::config::Config::default_path);
    cfg.parent().map(Path::to_path_buf).unwrap_or_default()
}

/// Runs a start script through Git Bash, detached and without a console window; its output goes to the log.
pub fn start_server(server: &Server, data_dir: &Path) -> Result<(), String> {
    use std::process::{Command, Stdio};
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let log = std::fs::File::create(data_dir.join(server.log))
        .map_err(|e| format!("cannot write {}: {e}", server.log))?;
    let log2 = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(git_sh());
    cmd.arg(server.script)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log2);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    cmd.spawn()
        .map(drop)
        .map_err(|e| format!("cannot run sh: {e}"))
}

fn git_sh() -> PathBuf {
    ["ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|p| PathBuf::from(p).join("Git").join("bin").join("sh.exe"))
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("sh"))
}
