//! AI integration (PROMPT.md, Phase 3 item 3): health check, install/repair with backups and rollback of the gate
//! scripts and the Claude Code hook. Target folders: `TESTDECK_GATE_DIR` and `TESTDECK_CLAUDE_DIR`, else
//! `<claude dir>/gate` and `~/.claude`. Every install is recorded in `<data dir>/install-log.json`; rollback undoes
//! the last one and leaves alone every file that changed again since.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A file Testdeck ships and installs, with its content embedded at build time.
pub struct Managed {
    pub name: &'static str,
    /// Installed under `<claude dir>/hooks` instead of the gate folder.
    pub hook: bool,
    pub content: &'static str,
}

pub const MANAGED: [Managed; 5] = [
    Managed {
        name: "slots.sh",
        hook: false,
        content: include_str!("../gate/slots.sh"),
    },
    Managed {
        name: "jt.sh",
        hook: false,
        content: include_str!("../gate/jt.sh"),
    },
    Managed {
        name: "testdeck-client.sh",
        hook: false,
        content: include_str!("../testdeck-client.sh"),
    },
    Managed {
        name: "testdeck-sequencer.js",
        hook: false,
        content: include_str!("../gate/testdeck-sequencer.js"),
    },
    Managed {
        name: "test-queue-wrap.js",
        hook: true,
        content: include_str!("../gate/hooks/test-queue-wrap.js"),
    },
];

#[derive(Clone, Debug)]
pub struct Dirs {
    pub gate: PathBuf,
    pub claude: PathBuf,
    pub data: PathBuf,
}

impl Dirs {
    pub fn from_env(data: PathBuf) -> Self {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_default();
        let claude = std::env::var_os("TESTDECK_CLAUDE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let gate = std::env::var_os("TESTDECK_GATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| claude.join("gate"));
        Self { gate, claude, data }
    }

    pub fn target(&self, m: &Managed) -> PathBuf {
        if m.hook {
            self.claude.join("hooks").join(m.name)
        } else {
            self.gate.join(m.name)
        }
    }

    pub fn settings(&self) -> PathBuf {
        self.claude.join("settings.json")
    }

    fn hook_command(&self) -> String {
        format!(
            "node \"{}\"",
            slashes(&self.claude.join("hooks").join("test-queue-wrap.js"))
        )
    }

    fn log_path(&self) -> PathBuf {
        self.data.join("install-log.json")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Bad,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub label: String,
    pub level: Level,
    pub reason: String,
}

#[derive(Clone, Debug, Default)]
pub struct Health {
    pub rows: Vec<Row>,
    /// What an install would change now, one line per file.
    pub plan: Vec<String>,
}

/// Checks everything the AI setup page shows. Blocking (it runs bash for the fallback check): call it off the UI thread.
pub fn check(dirs: &Dirs, port: u16) -> Health {
    let mut h = Health::default();
    let mut row = |label: &str, level, reason: String| {
        h.rows.push(Row {
            label: label.into(),
            level,
            reason,
        })
    };
    if broker_answers(port) {
        row(
            "Broker",
            Level::Ok,
            format!("answering on 127.0.0.1:{port}"),
        );
    } else {
        row(
            "Broker",
            Level::Bad,
            format!("nothing answers on 127.0.0.1:{port}; agents use the file queue"),
        );
    }
    let mut plan = Vec::new();
    for m in &MANAGED {
        let path = dirs.target(m);
        match read(&path) {
            None => {
                row(m.name, Level::Bad, format!("missing: {}", path.display()));
                plan.push(format!("create {}", path.display()));
            }
            Some(text) if norm(&text) == norm(m.content) => {
                row(m.name, Level::Ok, "installed and current".into())
            }
            Some(_) => {
                row(
                    m.name,
                    Level::Warn,
                    "installed, differs from this Testdeck version".into(),
                );
                plan.push(format!("replace {} (backup kept)", path.display()));
            }
        }
    }
    match settings_value(&dirs.settings()) {
        Ok(v) if registered(&v) => row(
            "Claude hook",
            Level::Ok,
            "registered in settings.json (PreToolUse, Bash)".into(),
        ),
        Ok(_) => {
            row(
                "Claude hook",
                Level::Bad,
                "not registered in settings.json PreToolUse (Bash)".into(),
            );
            plan.push(format!(
                "add the Bash PreToolUse hook to {} (backup kept)",
                dirs.settings().display()
            ));
        }
        Err(e) => row("Claude hook", Level::Bad, e),
    }
    match fallback_works(&dirs.gate) {
        Ok(()) => row(
            "File-queue fallback",
            Level::Ok,
            "slots.sh parses and falls back when the broker is down".into(),
        ),
        Err(e) => row("File-queue fallback", Level::Bad, e),
    }
    h.plan = plan;
    h
}

/// Installs or repairs every managed file that is missing or differs and registers the hook when it is missing.
/// Returns one line per change; the changes made before an error are logged, so rollback still covers them.
pub fn install(dirs: &Dirs) -> Result<Vec<String>, String> {
    // Parse settings.json before writing anything: a broken file aborts the whole install.
    let settings_path = dirs.settings();
    let settings = settings_value(&settings_path)?;
    let stamp = local_stamp();
    let mut changes = Vec::new();
    let result = (|| {
        for m in &MANAGED {
            let path = dirs.target(m);
            if read(&path).is_some_and(|t| norm(&t) == norm(m.content)) {
                continue;
            }
            changes.push(replace(&path, &norm(m.content), &stamp)?);
        }
        if !registered(&settings) {
            let mut v = settings;
            let hooks = v
                .as_object_mut()
                .ok_or("settings.json is not a JSON object")?
                .entry("hooks")
                .or_insert(json!({}));
            let pre = hooks
                .as_object_mut()
                .ok_or("settings.json: hooks is not an object")?
                .entry("PreToolUse")
                .or_insert(json!([]));
            pre.as_array_mut()
                .ok_or("settings.json: hooks.PreToolUse is not an array")?
                .push(json!({
                    "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": dirs.hook_command() }],
                }));
            let text = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())? + "\n";
            changes.push(replace(&settings_path, &text, &stamp)?);
        }
        Ok::<(), String>(())
    })();
    let lines = changes.iter().map(Change::describe).collect();
    if !changes.is_empty() {
        let mut log = read_log(dirs);
        log.installs.push(Install { at: stamp, changes });
        write_log(dirs, &log)?;
    }
    result.map(|()| lines)
}

/// Undoes the last logged install: restores each backup (or removes a file the install created), skipping files that
/// changed again since.
pub fn rollback(dirs: &Dirs) -> Result<Vec<String>, String> {
    let mut log = read_log(dirs);
    let last = log.installs.pop().ok_or("no install to roll back")?;
    let mut lines = Vec::new();
    for c in last.changes.iter().rev() {
        let p = c.path.display();
        if read(&c.path).map(|t| norm(&t)) != Some(norm(&c.written)) {
            lines.push(format!("left alone {p}: changed since the install"));
            continue;
        }
        let done = match &c.backup {
            Some(b) => std::fs::read(b)
                .map_err(|e| e.to_string())
                .and_then(|bytes| atomic_write(&c.path, &bytes)),
            None => std::fs::remove_file(&c.path).map_err(|e| e.to_string()),
        };
        lines.push(match (done, &c.backup) {
            (Ok(()), Some(b)) => format!("restored {p} from {}", b.display()),
            (Ok(()), None) => format!("removed {p} (the install created it)"),
            (Err(e), _) => format!("FAILED {p}: {e}"),
        });
    }
    write_log(dirs, &log)?;
    Ok(lines)
}

#[derive(Default, Serialize, Deserialize)]
struct Log {
    installs: Vec<Install>,
}

#[derive(Serialize, Deserialize)]
struct Install {
    at: String,
    changes: Vec<Change>,
}

#[derive(Serialize, Deserialize)]
struct Change {
    path: PathBuf,
    /// None: the install created the file.
    backup: Option<PathBuf>,
    /// The text the install wrote, so rollback can see whether the file changed again since.
    written: String,
}

impl Change {
    fn describe(&self) -> String {
        match &self.backup {
            Some(b) => format!("replaced {} (backup {})", self.path.display(), b.display()),
            None => format!("created {}", self.path.display()),
        }
    }
}

/// Backs up an existing file as `<name>.bak-testdeck-<stamp>`, then writes `text` through `<name>.new` and a rename.
fn replace(path: &Path, text: &str, stamp: &str) -> Result<Change, String> {
    let backup = if path.exists() {
        let b = sibling(path, &format!(".bak-testdeck-{stamp}"));
        std::fs::copy(path, &b).map_err(|e| format!("backup of {}: {e}", path.display()))?;
        Some(b)
    } else {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        None
    };
    atomic_write(path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Change {
        path: path.into(),
        backup,
        written: text.into(),
    })
}

/// Running bash scripts may be sourcing the target, so never write it in place.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = sibling(path, ".new");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn norm(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn slashes(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// A missing settings.json counts as empty; an unreadable or broken one is an error.
fn settings_value(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => serde_json::from_str(&t)
            .map_err(|e| format!("{} is not valid JSON: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// A PreToolUse entry whose matcher covers Bash and that runs test-queue-wrap.js.
fn registered(v: &Value) -> bool {
    v.pointer("/hooks/PreToolUse")
        .and_then(Value::as_array)
        .is_some_and(|entries| {
            entries.iter().any(|e| {
                e["matcher"].as_str().is_some_and(|m| m.contains("Bash"))
                    && e["hooks"].as_array().is_some_and(|hs| {
                        hs.iter().any(|h| {
                            h["command"]
                                .as_str()
                                .is_some_and(|c| c.contains("test-queue-wrap.js"))
                        })
                    })
            })
        })
}

fn read_log(dirs: &Dirs) -> Log {
    read(&dirs.log_path())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_log(dirs: &Dirs, log: &Log) -> Result<(), String> {
    std::fs::create_dir_all(&dirs.data).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(log).map_err(|e| e.to_string())?;
    atomic_write(&dirs.log_path(), text.as_bytes())
}

/// The broker's `/health` answers `"app":"testdeck"` (the same probe the bash client uses).
fn broker_answers(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
    let mut body = String::new();
    s.write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .is_ok()
        && s.read_to_string(&mut body).is_ok()
        && body.contains("\"app\":\"testdeck\"")
}

/// `bash -n` on the installed slots.sh, then source it with the broker pointed at an unused port and a scratch TMPDIR
/// and check that the Testdeck path declines (the caller then takes the file queue). Takes no slot.
fn fallback_works(gate: &Path) -> Result<(), String> {
    let slots = gate.join("slots.sh");
    if !slots.is_file() {
        return Err("slots.sh is missing".into());
    }
    let syntax = run_bash(&["-n", &slashes(&slots)], &[])?;
    if !syntax.0 {
        return Err(format!("bash -n slots.sh failed: {}", syntax.1.trim()));
    }
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(1);
    let scratch = std::env::temp_dir().join(format!("testdeck-fallback-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let script = r#". "$1" || exit 9; declare -F slot_take >/dev/null || exit 9; if declare -F _slot_td >/dev/null; then _slot_td jest; echo "rc=$?"; else echo rc=1; fi"#;
    let out = run_bash(
        &["-c", script, "_", &slashes(&slots)],
        &[
            ("TESTDECK_PORT", port.to_string()),
            ("TMPDIR", slashes(&scratch)),
        ],
    );
    let _ = std::fs::remove_dir_all(&scratch);
    let (_, text) = out?;
    if text.contains("rc=1") {
        Ok(())
    } else {
        Err(format!(
            "slots.sh did not fall back with the broker down: {}",
            text.trim()
        ))
    }
}

/// Runs Git Bash (never the WSL bash.exe on PATH) for at most 15 s; returns (success, stdout + stderr).
fn run_bash(args: &[&str], env: &[(&str, String)]) -> Result<(bool, String), String> {
    let bash = ["ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|p| PathBuf::from(p).join("Git").join("bin").join("bash.exe"))
        .find(|p| p.is_file())
        .ok_or("Git Bash not found under Program Files")?;
    let mut cmd = Command::new(bash);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for v in [
        "TESTDECK_DISABLE",
        "JT_SLOT_HELD",
        "GATE_EXCL_HELD",
        "TD_TICKET",
    ] {
        cmd.env_remove(v);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot run bash: {e}"))?;
    let start = Instant::now();
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if start.elapsed() > Duration::from_secs(15) {
            let _ = child.kill();
            return Err("bash did not finish within 15 s".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    Ok((out.status.success(), text))
}

/// Local time as `YYYYMMDD-HHMMSS` for backup names.
fn local_stamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}
