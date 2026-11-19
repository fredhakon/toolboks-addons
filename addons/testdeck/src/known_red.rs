//! Suite lists that jt.sh tags its failures with (phase 2 features 2 and 3), answered in plain text, one suite path
//! or path fragment per line.
use crate::state::Shared;
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Serializes the appends from concurrent gate lanes.
static WRITE: Mutex<()> = Mutex::new(());

/// `GET /flaky`: the `flaky_suites` config list. jt.sh reruns a failure once alone when every failing suite matches.
pub async fn flaky(State(s): State<Shared>) -> String {
    s.lock().config.flaky_suites.join("\n")
}

/// `known-red.txt` in the data folder: one suite path or path fragment per line, `#` comments. The UI edits it.
pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("known-red.txt")
}

/// The entries of the file (no blank lines, no comments); empty when it does not exist.
pub fn read(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// `GET /known_red`: the entries of `known-red.txt`.
pub async fn get(State(s): State<Shared>) -> String {
    read(&path(&s.data_dir())).join("\n")
}

#[derive(Deserialize)]
pub struct AddBody {
    suites: Vec<String>,
}

/// `POST /known_red {suites}`: appends the suites that no entry matches yet (an entry matches as a substring).
/// jt.sh sends the failing suites of a gate run on a clean `origin/staging`.
pub async fn add(State(s): State<Shared>, Json(b): Json<AddBody>) -> Json<Value> {
    let file = path(&s.data_dir());
    let _guard = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut have = read(&file);
    let mut new = Vec::new();
    for suite in b.suites.iter().map(|x| x.trim()) {
        // One line per entry: a newline in a suite name would split it into two entries.
        if suite.is_empty()
            || suite.contains(['\n', '\r'])
            || suite.starts_with('#')
            || have.iter().any(|h| suite.contains(h.as_str()))
        {
            continue;
        }
        have.push(suite.to_string());
        new.push(suite.to_string());
    }
    if !new.is_empty() {
        let r = std::fs::create_dir_all(file.parent().unwrap_or(Path::new("."))).and_then(|_| {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file)?;
            let text: String = new.iter().map(|x| format!("{x}\n")).collect();
            // A file the user saved without a final newline must not glue the first new entry to its last line.
            let len = f.metadata()?.len();
            let lead = if len > 0 && !std::fs::read(&file)?.ends_with(b"\n") {
                "\n"
            } else {
                ""
            };
            f.write_all(format!("{lead}{text}").as_bytes())
        });
        if let Err(e) = r {
            eprintln!("testdeck: known-red {}: {e}", file.display());
        }
    }
    Json(json!({"status": "ok", "added": new.len()}))
}
