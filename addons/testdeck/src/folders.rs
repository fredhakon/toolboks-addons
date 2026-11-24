//! Jest disk use: finds the folders jest and `jt.sh` leave behind, measures them, and decides what a cleanup may
//! delete. Sources: `%TEMP%` (`jt-<pid>-<time>` run folders, `jest*` caches) and, from the job history, the
//! `--cacheDirectory` values, `node_modules/.cache/jest*` and `.jest` under each requester cwd. Nothing here
//! follows a junction or symlink, and a delete never leaves the discovered roots or touches a running or queued job.
use crate::state::AppState;
use std::collections::HashSet;
use std::fs::Metadata;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const GB: u64 = 1 << 30;
/// A `jt-*` run folder must be this old (by its newest file and its name's time) before a cleanup removes it.
pub const JT_MIN_AGE_SECS: u64 = 3600;
/// Deepest level a measurement descends; deeper files count as a partial result.
const MAX_DEPTH: usize = 24;
/// Time for one refresh over all folders; a folder that does not finish is shown as partial ("at least").
pub const REFRESH_BUDGET: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `jt-<pid>-<time>`: the folder of one `jt.sh` run.
    Slot,
    /// A jest cache folder.
    Cache,
}

#[derive(Clone, Debug)]
pub struct Folder {
    pub path: PathBuf,
    pub kind: Kind,
    /// The project (requester cwd) it belongs to, or "%TEMP%".
    pub owner: String,
    pub size: u64,
    pub files: u64,
    pub newest_unix: u64,
    /// The folder itself is a junction or symlink: it is listed with no size and deleted without following it.
    pub link: bool,
    /// The measurement hit the depth or time cap: the size is "at least".
    pub partial: bool,
}

/// One distinct (cwd, requester, command) of the job history.
#[derive(Clone, Debug)]
pub struct JobPath {
    pub cwd: String,
    pub requester: String,
    pub command: String,
}

/// Where to look.
pub struct Sources {
    pub temp: PathBuf,
    pub jobs: Vec<JobPath>,
}

/// What the running and queued jobs use right now.
#[derive(Default, Clone, Debug)]
pub struct Busy {
    /// Windows and shell pids of every running or queued job.
    pub pids: HashSet<u32>,
    pub cwds: Vec<PathBuf>,
    pub cache_dirs: Vec<PathBuf>,
    /// A jest or gate job is running or queued: it may use a default cache in `%TEMP%` (`jest_<hash>`), which
    /// the job's own cwd does not tell.
    pub jest_any: bool,
}

pub struct Ctx<'a> {
    pub now_unix: u64,
    pub temp: PathBuf,
    /// Project folders (a requester cwd and its two parents): `.jest` and `node_modules/.cache/jest*` may sit below.
    pub projects: Vec<PathBuf>,
    /// Explicit `--cacheDirectory` folders, deletable as a whole.
    pub cache_dirs: Vec<PathBuf>,
    pub busy: Busy,
    pub alive: &'a dyn Fn(u32) -> bool,
}

/// Lowercase, backslash form for comparing paths (Windows paths ignore case).
fn norm(p: &Path) -> PathBuf {
    PathBuf::from(p.to_string_lossy().to_lowercase().replace('/', "\\"))
}

/// `/c/Users/x` (a bash path in a job's cwd or command) as `C:\Users\x`.
fn from_msys(s: &str) -> PathBuf {
    let b = s.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b'/' {
        PathBuf::from(format!(
            "{}:{}",
            s[1..2].to_uppercase(),
            s[2..].replace('/', "\\")
        ))
    } else {
        PathBuf::from(s)
    }
}

/// `jt-<pid>-<unix time>`; the file queue folder `jt-slots` does not match.
pub fn parse_slot(name: &str) -> Option<(u32, u64)> {
    let rest = name.strip_prefix("jt-")?;
    let (pid, t) = rest.split_once('-')?;
    Some((pid.parse().ok()?, t.parse().ok()?))
}

fn jest_name(name: &str) -> bool {
    name.to_lowercase().starts_with("jest")
}

/// The `--cacheDirectory` value in a command line, if any.
pub fn cache_dir_arg(cmd: &str) -> Option<PathBuf> {
    let i = cmd.find("--cacheDirectory")?;
    let rest = cmd[i + "--cacheDirectory".len()..].trim_start_matches(['=', ' ']);
    let (quote, body) = match rest.chars().next()? {
        q @ ('"' | '\'') => (Some(q), &rest[1..]),
        _ => (None, rest),
    };
    let end = body
        .find(|c: char| quote.map_or(c.is_whitespace(), |q| c == q))
        .unwrap_or(body.len());
    let p = from_msys(&body[..end]);
    (p.is_absolute() && p.components().count() >= 3).then_some(p)
}

fn is_link(md: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if md.file_attributes() & 0x400 != 0 {
            return true; // FILE_ATTRIBUTE_REPARSE_POINT: junction, symlink, mount point
        }
    }
    md.file_type().is_symlink()
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A project folder and its two parents (the jest config may sit above the cwd the job ran in).
fn project_dirs(cwd: &Path) -> Vec<PathBuf> {
    cwd.ancestors()
        .take(3)
        .filter(|p| p.components().count() >= 3)
        .map(Path::to_path_buf)
        .collect()
}

impl Sources {
    pub fn ctx<'a>(&self, busy: Busy, alive: &'a dyn Fn(u32) -> bool) -> Ctx<'a> {
        let cwds: Vec<PathBuf> = self.jobs.iter().map(|j| from_msys(&j.cwd)).collect();
        let mut projects: Vec<PathBuf> = cwds.iter().flat_map(|c| project_dirs(c)).collect();
        projects.sort();
        projects.dedup();
        let mut cache_dirs: Vec<PathBuf> = self
            .jobs
            .iter()
            .filter_map(|j| cache_dir_arg(&j.command))
            // A cache dir that holds a project would let a cleanup delete the project.
            .filter(|d| !cwds.iter().any(|c| norm(c).starts_with(norm(d))))
            .collect();
        cache_dirs.sort();
        cache_dirs.dedup();
        Ctx {
            now_unix: unix(SystemTime::now()),
            temp: self.temp.clone(),
            projects,
            cache_dirs,
            busy,
            alive,
        }
    }

    /// The folders that exist, unmeasured (size 0). Reads directory names only.
    pub fn discover(&self, ctx: &Ctx) -> Vec<Folder> {
        let mut out: Vec<Folder> = Vec::new();
        let mut add = |path: PathBuf, kind: Kind, owner: &str| {
            let Ok(md) = std::fs::symlink_metadata(&path) else {
                return;
            };
            if !md.is_dir() && !is_link(&md) {
                return;
            }
            if out.iter().any(|f| norm(&f.path) == norm(&path)) {
                return;
            }
            out.push(Folder {
                path,
                kind,
                owner: owner.to_string(),
                size: 0,
                files: 0,
                newest_unix: md.modified().map_or(0, unix),
                link: is_link(&md),
                partial: false,
            });
        };
        if let Ok(rd) = std::fs::read_dir(&self.temp) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if parse_slot(&name).is_some() {
                    add(e.path(), Kind::Slot, "jt.sh run");
                } else if jest_name(&name) {
                    add(e.path(), Kind::Cache, "%TEMP%");
                }
            }
        }
        for j in &self.jobs {
            let owner = if j.requester.is_empty() {
                &j.cwd
            } else {
                &j.requester
            };
            let owner = format!("{owner} ({})", j.cwd);
            for proj in project_dirs(&from_msys(&j.cwd)) {
                add(proj.join(".jest"), Kind::Cache, &owner);
                if let Ok(rd) = std::fs::read_dir(proj.join("node_modules").join(".cache")) {
                    for e in rd.flatten() {
                        if jest_name(&e.file_name().to_string_lossy()) {
                            add(e.path(), Kind::Cache, &owner);
                        }
                    }
                }
            }
            if let Some(d) = cache_dir_arg(&j.command).filter(|d| ctx.cache_dirs.contains(d)) {
                add(d, Kind::Cache, &owner);
            }
        }
        out
    }
}

/// Fills size, file count, newest write time. Never follows a link; stops at `MAX_DEPTH` or `deadline` (then
/// `partial`). Blocking: call it off the UI thread.
pub fn measure(f: &mut Folder, deadline: Instant) {
    if f.link {
        return;
    }
    let (mut size, mut files, mut newest, mut partial) = (0u64, 0u64, f.newest_unix, false);
    let mut stack = vec![(f.path.clone(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if Instant::now() >= deadline || depth > MAX_DEPTH {
            partial = true;
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if is_link(&md) {
                continue;
            }
            newest = newest.max(md.modified().map_or(0, unix));
            if md.is_dir() {
                stack.push((e.path(), depth + 1));
            } else {
                size += md.len();
                files += 1;
            }
        }
    }
    (f.size, f.files, f.newest_unix, f.partial) = (size, files, newest, partial);
}

/// Measures every folder within `budget` shared by all of them; a folder not reached keeps size 0, partial.
pub fn measure_all(folders: &mut [Folder], budget: Duration) {
    let deadline = Instant::now() + budget;
    for f in folders.iter_mut() {
        if Instant::now() >= deadline {
            f.partial = true;
        } else {
            measure(f, deadline);
        }
    }
}

fn under(p: &Path, root: &Path) -> bool {
    norm(p).starts_with(norm(root))
}

/// Hard rules, for a user-ticked folder as well as an automatic one: inside a discovered root, shaped like what the
/// finder lists, and not used by a running or queued job. `Err` is the reason it must stay.
pub fn guard(f: &Folder, ctx: &Ctx) -> Result<(), &'static str> {
    let p = &f.path;
    if p.components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("not a plain path");
    }
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let np = norm(p);
    let in_temp = np.parent() == Some(norm(&ctx.temp).as_path());
    let explicit = ctx.cache_dirs.iter().any(|d| under(p, d));
    // Below a project folder: only `.jest` or `node_modules\.cache\jest*`.
    let project = ctx.projects.iter().find(|r| {
        np.starts_with(norm(r)) && np != norm(r) && {
            let rel = np
                .strip_prefix(norm(r))
                .unwrap_or(&np)
                .to_string_lossy()
                .into_owned();
            rel == ".jest"
                || (rel.starts_with("node_modules\\.cache\\jest") && rel.matches('\\').count() == 2)
        }
    });
    match f.kind {
        Kind::Slot => {
            let (pid, _) = parse_slot(&name)
                .filter(|_| in_temp)
                .ok_or("outside the discovered roots")?;
            if ctx.busy.pids.contains(&pid) || (ctx.alive)(pid) {
                return Err("its process is still running");
            }
        }
        Kind::Cache => {
            if explicit {
                let busy_dir = ctx
                    .busy
                    .cache_dirs
                    .iter()
                    .any(|b| under(p, b) || under(b, p));
                if busy_dir {
                    return Err("a running or queued job uses this cache folder");
                }
            } else if in_temp && jest_name(&name) {
                if ctx.busy.jest_any {
                    return Err("a jest job is running or queued");
                }
            } else if let Some(root) = project {
                // The folder belongs to the project at `root` (a parent of the cwd of a job).
                let own = p
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::parent)
                    .unwrap_or(root);
                let proj = if name == ".jest" {
                    p.parent().unwrap_or(own)
                } else {
                    own
                };
                if ctx
                    .busy
                    .cwds
                    .iter()
                    .any(|c| under(c, proj) || under(proj, c))
                {
                    return Err("a running or queued job runs in this project");
                }
            } else {
                return Err("outside the discovered roots");
            }
            if ctx
                .busy
                .cache_dirs
                .iter()
                .any(|b| under(p, b) || under(b, p))
            {
                return Err("a running or queued job uses this cache folder");
            }
        }
    }
    Ok(())
}

/// What a cleanup removes without being asked: finished `jt-*` folders older than an hour, and every jest cache no
/// running or queued job uses.
pub fn auto_safe(f: &Folder, ctx: &Ctx) -> bool {
    guard(f, ctx).is_ok()
        && match f.kind {
            Kind::Slot => {
                let t = parse_slot(
                    &f.path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
                .map_or(u64::MAX, |(_, t)| t);
                ctx.now_unix.saturating_sub(f.newest_unix.max(t)) >= JT_MIN_AGE_SECS
            }
            Kind::Cache => true,
        }
}

/// The folders a cleanup deletes: the automatic ones plus those the user ticked (which must still pass `guard`).
pub fn plan<'a>(folders: &'a [Folder], ticked: &HashSet<PathBuf>, ctx: &Ctx) -> Vec<&'a Folder> {
    folders
        .iter()
        .filter(|f| auto_safe(f, ctx) || (ticked.contains(&f.path) && guard(f, ctx).is_ok()))
        .collect()
}

/// Cache folders to delete, oldest first, until the caches fit in `cap` bytes; only ones that pass `guard`.
pub fn cap_victims<'a>(folders: &'a [Folder], ctx: &Ctx, cap: u64) -> Vec<&'a Folder> {
    let mut total: u64 = folders
        .iter()
        .filter(|f| f.kind == Kind::Cache)
        .map(|f| f.size)
        .sum();
    let mut caches: Vec<&Folder> = folders
        .iter()
        .filter(|f| f.kind == Kind::Cache && guard(f, ctx).is_ok())
        .collect();
    caches.sort_by_key(|f| f.newest_unix);
    let mut out = Vec::new();
    for f in caches {
        if total <= cap {
            break;
        }
        total = total.saturating_sub(f.size);
        out.push(f);
    }
    out
}

#[derive(Default, Debug)]
pub struct Outcome {
    pub freed: u64,
    pub removed: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
}

/// Deletes one folder. A link is removed itself, never entered; `remove_dir_all` does not follow links either.
fn remove(path: &Path) -> std::io::Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    if is_link(&md) {
        std::fs::remove_dir(path).or_else(|_| std::fs::remove_file(path))
    } else {
        std::fs::remove_dir_all(path)
    }
}

/// Deletes `todo`, re-checking `guard` against `ctx` (taken just now) first. Freed bytes are the measured sizes.
pub fn clean(todo: &[&Folder], ctx: &Ctx) -> Outcome {
    let mut out = Outcome::default();
    for f in todo {
        if let Err(why) = guard(f, ctx) {
            out.failed.push((f.path.clone(), why.to_string()));
            continue;
        }
        match remove(&f.path) {
            Ok(()) => {
                out.freed += f.size;
                out.removed.push(f.path.clone());
            }
            Err(e) => out.failed.push((f.path.clone(), e.to_string())),
        }
    }
    out
}

/// What the running and queued jobs use. Locks the scheduler briefly.
pub fn busy_now(s: &AppState) -> Busy {
    let g = s.lock();
    let reqs = g
        .sched
        .running
        .iter()
        .map(|r| &r.req)
        .chain(g.sched.queue.iter().map(|q| &q.req));
    let mut b = Busy::default();
    for r in reqs {
        b.pids.extend(r.win_pid);
        b.pids.extend(r.shell_pid);
        b.cwds.push(from_msys(&r.cwd));
        b.cache_dirs.extend(cache_dir_arg(&r.command));
        b.jest_any |= matches!(
            r.kind,
            crate::scheduler::Kind::Jest | crate::scheduler::Kind::Gate
        );
    }
    b
}

/// Runs `f` with the rules context for `jobs` and what the jobs use right now.
pub fn with_ctx<R>(s: &AppState, jobs: &[JobPath], f: impl FnOnce(&Sources, &Ctx) -> R) -> R {
    let src = Sources {
        temp: std::env::temp_dir(),
        jobs: jobs.to_vec(),
    };
    let alive = |p: u32| s.proc_ctl.is_alive(p);
    let ctx = src.ctx(busy_now(s), &alive);
    f(&src, &ctx)
}

/// Discovers and measures everything (blocking, up to `REFRESH_BUDGET`), biggest first.
pub fn scan(s: &AppState, jobs: &[JobPath]) -> Vec<Folder> {
    let mut folders = with_ctx(s, jobs, |src, ctx| src.discover(ctx));
    measure_all(&mut folders, REFRESH_BUDGET);
    folders.sort_by_key(|f| std::cmp::Reverse(f.size));
    folders
}

/// The automatic cleanup the config asks for: old `jt-*` folders, and the oldest caches until they fit the cap.
/// Returns what it freed, or None when both are off.
pub fn auto_clean(s: &AppState, jobs: Vec<JobPath>) -> Option<Outcome> {
    let cfg = s.lock().config.clone();
    if !cfg.disk_auto_clean_jt && !cfg.disk_cache_cap_enabled {
        return None;
    }
    let folders = scan(s, &jobs);
    // The context is taken after the slow scan, so the busy check is as fresh as the delete.
    Some(with_ctx(s, &jobs, |_, ctx| {
        let mut todo: Vec<&Folder> = Vec::new();
        if cfg.disk_auto_clean_jt {
            todo.extend(
                folders
                    .iter()
                    .filter(|f| f.kind == Kind::Slot && auto_safe(f, ctx)),
            );
        }
        if cfg.disk_cache_cap_enabled {
            let cap = (f64::from(cfg.disk_cache_cap_gb.max(0.0)) * GB as f64) as u64;
            todo.extend(cap_victims(&folders, ctx, cap));
        }
        clean(&todo, ctx)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("testdeck-folders-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn file(p: &Path, bytes: usize) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![0u8; bytes]).unwrap();
    }

    fn src(temp: &Path, cwd: &str, command: &str) -> Sources {
        Sources {
            temp: temp.to_path_buf(),
            jobs: vec![JobPath {
                cwd: cwd.into(),
                requester: "me".into(),
                command: command.into(),
            }],
        }
    }

    fn scanned<'a>(
        src: &Sources,
        busy: Busy,
        alive: &'a dyn Fn(u32) -> bool,
        now: u64,
    ) -> (Vec<Folder>, Ctx<'a>) {
        let mut ctx = src.ctx(busy, alive);
        let mut f = src.discover(&ctx);
        measure_all(&mut f, Duration::from_secs(10));
        ctx.now_unix = now;
        (f, ctx)
    }

    fn by_name<'a>(f: &'a [Folder], name: &str) -> &'a Folder {
        f.iter()
            .find(|f| f.path.file_name().unwrap() == name)
            .unwrap_or_else(|| panic!("no {name}"))
    }

    #[test]
    fn parses_names() {
        assert_eq!(parse_slot("jt-123-1795452809"), Some((123, 1795452809)));
        assert_eq!(parse_slot("jt-slots"), None);
        assert_eq!(parse_slot("jt-1-x"), None);
        assert_eq!(
            cache_dir_arg(r#"npx jest --cacheDirectory="C:\a b\c" x"#),
            Some(PathBuf::from(r"C:\a b\c"))
        );
        assert_eq!(
            cache_dir_arg("jest --cacheDirectory=/c/tmp/cc x"),
            Some(PathBuf::from(r"C:\tmp\cc"))
        );
        assert_eq!(cache_dir_arg("jest --cacheDirectory=rel"), None);
        assert_eq!(cache_dir_arg("jest"), None);
    }

    #[test]
    fn measures_and_discovers_temp_and_project_folders() {
        let t = tmp("measure");
        file(&t.join("jest").join("a").join("x.bin"), 100);
        file(&t.join("jest").join("y.bin"), 50);
        file(&t.join("jt-slots").join("1"), 5);
        file(&t.join("other").join("z"), 5);
        file(&t.join("jt-9-100").join("jest.log"), 7);
        let proj = t.join("proj");
        file(
            &proj
                .join("tests")
                .join("node_modules")
                .join(".cache")
                .join("jest-x")
                .join("c"),
            30,
        );
        file(
            &proj
                .join("tests")
                .join("node_modules")
                .join(".cache")
                .join("jiti")
                .join("c"),
            30,
        );
        file(&proj.join(".jest").join("c"), 11);
        let cwd = proj.join("tests").to_string_lossy().into_owned();
        let s = src(&t, &cwd, "jest");
        let alive = |_| false;
        let (f, _) = scanned(&s, Busy::default(), &alive, 0);
        let mut names: Vec<_> = f
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, [".jest", "jest", "jest-x", "jt-9-100"]);
        let j = by_name(&f, "jest");
        assert_eq!((j.size, j.files), (150, 2));
        assert_eq!(by_name(&f, "jest-x").size, 30);
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn size_walk_stops_at_the_deadline() {
        let t = tmp("deadline");
        file(&t.join("jest").join("a"), 10);
        let mut f = Folder {
            path: t.join("jest"),
            kind: Kind::Cache,
            owner: String::new(),
            size: 0,
            files: 0,
            newest_unix: 0,
            link: false,
            partial: false,
        };
        measure(&mut f, Instant::now());
        assert!(f.partial && f.files == 0);
        measure(&mut f, Instant::now() + Duration::from_secs(5));
        assert!(!f.partial && f.size == 10);
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn classifier_keeps_running_jobs_live_pids_and_young_folders() {
        let t = tmp("classify");
        file(&t.join("jt-1-100").join("a"), 1); // dead, old
        file(&t.join("jt-2-100").join("a"), 1); // alive pid
        file(&t.join("jt-3-100").join("a"), 1); // a running job's shell
        file(&t.join("jt-4-9999999999").join("a"), 1); // too young
        file(&t.join("jest_dx").join("a"), 1);
        let s = src(&t, "D:\\nowhere\\tests", "jest");
        let alive = |p| p == 2;
        let now = 9_999_999_999 + 10;
        let busy = Busy {
            pids: HashSet::from([3]),
            ..Busy::default()
        };
        let (f, ctx) = scanned(&s, busy, &alive, now);
        let ok = |n: &str| auto_safe(by_name(&f, n), &ctx);
        assert!(ok("jt-1-100"));
        assert!(!ok("jt-2-100"));
        assert!(!ok("jt-3-100"));
        assert!(!ok("jt-4-9999999999"), "10 s old");
        assert!(ok("jest_dx"));
        // A running jest job may use the default cache in %TEMP%.
        let busy = Busy {
            jest_any: true,
            ..Busy::default()
        };
        let (f, ctx) = scanned(&s, busy, &alive, now);
        assert!(!auto_safe(by_name(&f, "jest_dx"), &ctx));
        assert_eq!(
            guard(by_name(&f, "jest_dx"), &ctx),
            Err("a jest job is running or queued")
        );
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn cache_of_a_busy_project_or_cache_dir_stays() {
        let t = tmp("busy");
        let proj = t.join("proj");
        file(
            &proj
                .join("tests")
                .join("node_modules")
                .join(".cache")
                .join("jest")
                .join("c"),
            5,
        );
        let other = t.join("other");
        file(
            &other
                .join("tests")
                .join("node_modules")
                .join(".cache")
                .join("jest")
                .join("c"),
            5,
        );
        let cd = t.join("cd").join("x").join("cache");
        file(&cd.join("c"), 5);
        let s = Sources {
            temp: t.join("empty"),
            jobs: vec![
                JobPath {
                    cwd: proj.join("tests").to_string_lossy().into_owned(),
                    requester: "a".into(),
                    command: "jest".into(),
                },
                JobPath {
                    cwd: other.join("tests").to_string_lossy().into_owned(),
                    requester: "b".into(),
                    command: format!("jest --cacheDirectory={}", cd.display()),
                },
            ],
        };
        let alive = |_| false;
        let busy = Busy {
            cwds: vec![proj.join("tests")],
            cache_dirs: vec![cd.clone()],
            ..Busy::default()
        };
        let (f, ctx) = scanned(&s, busy, &alive, 0);
        assert_eq!(f.len(), 3);
        let verdicts: Vec<(String, bool)> = f
            .iter()
            .map(|x| (x.path.to_string_lossy().into_owned(), auto_safe(x, &ctx)))
            .collect();
        for (p, ok) in verdicts {
            if p.contains("other") {
                assert!(ok, "{p}");
            } else {
                assert!(!ok, "{p}");
            }
        }
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn paths_outside_the_roots_are_refused() {
        let t = tmp("outside");
        let victim = tmp("outside-victim");
        file(&victim.join("keep.txt"), 5);
        let s = src(&t, "D:\\nowhere\\tests", "jest");
        let alive = |_| false;
        let (_, ctx) = scanned(&s, Busy::default(), &alive, 0);
        let mk = |path: PathBuf, kind| Folder {
            path,
            kind,
            owner: String::new(),
            size: 5,
            files: 1,
            newest_unix: 0,
            link: false,
            partial: false,
        };
        for f in [
            mk(victim.clone(), Kind::Cache),
            mk(
                t.join("..").join(victim.file_name().unwrap()).join("jest"),
                Kind::Cache,
            ),
            mk(t.clone(), Kind::Cache),
            mk(t.join("jt-1-1").join("sub").join("jt-2-2"), Kind::Slot),
            mk(t.join("not-jest"), Kind::Cache),
        ] {
            assert!(guard(&f, &ctx).is_err(), "{}", f.path.display());
            let out = clean(&[&f], &ctx);
            assert_eq!((out.freed, out.removed.len(), out.failed.len()), (0, 0, 1));
        }
        assert!(victim.join("keep.txt").exists() && t.exists());
        let _ = std::fs::remove_dir_all(&t);
        let _ = std::fs::remove_dir_all(&victim);
    }

    #[test]
    fn ticked_folders_still_pass_the_guard() {
        let t = tmp("ticked");
        file(&t.join("jt-8-100").join("a"), 1);
        file(&t.join("jest_a").join("a"), 1);
        let s = src(&t, "D:\\nowhere\\tests", "jest");
        let alive = |p| p == 8;
        let (f, ctx) = scanned(&s, Busy::default(), &alive, 10);
        let ticked: HashSet<PathBuf> = f.iter().map(|f| f.path.clone()).collect();
        let names: Vec<_> = plan(&f, &ticked, &ctx)
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["jest_a"], "a live pid stays even when ticked");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn size_cap_removes_the_oldest_caches_first() {
        let t = tmp("cap");
        for n in ["jest_old", "jest_mid", "jest_new"] {
            file(&t.join(n).join("a"), 100);
        }
        let s = src(&t, "D:\\nowhere\\tests", "jest");
        let alive = |_| false;
        let (mut f, ctx) = scanned(&s, Busy::default(), &alive, 0);
        for x in &mut f {
            x.newest_unix = match x.path.file_name().unwrap().to_str().unwrap() {
                "jest_old" => 10,
                "jest_mid" => 20,
                _ => 30,
            };
        }
        let names = |v: Vec<&Folder>| {
            v.iter()
                .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert!(cap_victims(&f, &ctx, 300).is_empty());
        assert_eq!(names(cap_victims(&f, &ctx, 250)), ["jest_old"]);
        assert_eq!(names(cap_victims(&f, &ctx, 100)), ["jest_old", "jest_mid"]);
        let busy = Busy {
            jest_any: true,
            ..Busy::default()
        };
        let (f2, ctx2) = scanned(&s, busy, &alive, 0);
        assert!(
            cap_victims(&f2, &ctx2, 0).is_empty(),
            "a running jest job keeps the default caches"
        );
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn cleanup_frees_only_the_allowed_folders() {
        let t = tmp("clean");
        file(&t.join("jt-1-100").join("a"), 10); // finished and old
        file(&t.join("jt-2-100").join("a"), 20); // pid alive
        file(&t.join("jest_gone").join("a").join("b"), 40);
        file(&t.join("keep").join("a"), 80);
        let s = src(&t, "D:\\nowhere\\tests", "jest");
        let alive = |p| p == 2;
        let (f, ctx) = scanned(&s, Busy::default(), &alive, 4_000_000_000);
        let todo = plan(&f, &HashSet::new(), &ctx);
        let out = clean(&todo, &ctx);
        assert_eq!(out.freed, 50);
        assert_eq!(out.removed.len(), 2);
        assert!(!t.join("jt-1-100").exists() && !t.join("jest_gone").exists());
        assert!(t.join("jt-2-100").exists() && t.join("keep").exists());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_is_removed_itself_and_never_followed() {
        let t = tmp("junction");
        let target = tmp("junction-target");
        file(&target.join("precious.txt"), 1000);
        let link = t.join("jest_link");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(made, "mklink /J failed");
        let s = src(&t, "D:\\nowhere\\tests", "jest");
        let alive = |_| false;
        let (f, ctx) = scanned(&s, Busy::default(), &alive, 0);
        let l = by_name(&f, "jest_link");
        assert!(
            l.link && l.size == 0 && l.files == 0,
            "not followed when measuring"
        );
        assert!(auto_safe(l, &ctx));
        let out = clean(&[l], &ctx);
        assert_eq!(out.removed.len(), 1);
        assert!(!link.exists());
        assert!(
            target.join("precious.txt").exists(),
            "the junction target is untouched"
        );
        let _ = std::fs::remove_dir_all(&t);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_inside_a_folder_is_not_followed_by_measure_or_delete() {
        let t = tmp("junction-in");
        let target = tmp("junction-in-target");
        file(&target.join("precious.txt"), 1000);
        file(&t.join("jest_a").join("x"), 3);
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(t.join("jest_a").join("inner"))
            .arg(&target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(made, "mklink /J failed");
        let s = src(&t, "D:\\nowhere\\tests", "jest");
        let alive = |_| false;
        let (f, ctx) = scanned(&s, Busy::default(), &alive, 0);
        assert_eq!(by_name(&f, "jest_a").size, 3);
        let out = clean(&plan(&f, &HashSet::new(), &ctx), &ctx);
        assert_eq!(out.removed.len(), 1);
        assert!(target.join("precious.txt").exists());
        let _ = std::fs::remove_dir_all(&t);
        let _ = std::fs::remove_dir_all(&target);
    }
}
