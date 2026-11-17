//! History on disk (#40): seven days that survive a restart.
//!
//! # Where
//!
//! `<Toolboks data folder>/addon-data/toolboks.resource-monitor/history/`, the add-on's own
//! removable data folder (see [`directory`]). `TOOLBOKS_DATA_DIR` moves it for tests.
//!
//! # Files
//!
//! Three levels of detail, each a rolling window, each in its own append-only files split by
//! time, so pruning is deleting whole files:
//!
//! | Level | Holds | One file per | Kept |
//! |---|---|---|---|
//! | `fine` | every step (about 5 s) | 10 minutes | 1 hour |
//! | `minute` | per-minute buckets | hour | 24 hours |
//! | `day` | 5-minute buckets, app events, markers | day | 7 days |
//!
//! A file is deleted once everything it can hold is older than its level's window, so at most
//! one file span more than the window is on disk. Readers drop the records outside the window.
//! Each run of the recorder writes its own files (the run id is in the name), so a crash can
//! only cut the end of its own files, and the app ids inside a file belong to one run.
//!
//! # Format (version [`VERSION`])
//!
//! Little-endian. A 28-byte header: magic `TBRMHIST`, version (u16), level (u8), a zero byte,
//! run id (u64), period start (UTC ms, i64). Then frames: tag (u8), payload length (u32),
//! payload, FNV-1a checksum (u32) of tag, length and payload. A name frame maps an app id to
//! its exe name before the first record of the file that uses it, so every file reads alone.
//! A reader stops at the first frame that is cut short or fails its checksum and keeps the
//! frames before it; a file with a bad header or a newer version is skipped. Either way the
//! problem is reported and the other files still load.
//!
//! # Writes
//!
//! On the recorder thread only, never on the UI thread. Records are buffered and written
//! together once a minute (when a minute bucket closes), when a file is switched and when
//! recording stops, so a crash loses at most about a minute. Old files are pruned when the
//! writer starts and once an hour. A total over [`MAX_DISK_BYTES`] deletes the oldest files.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::record::{
    AppEvent, AppId, AppUsage, EventKind, Folded, Marker, MarkerKind, NameTable, StepRecord,
    SystemTotals, Usage, TOTAL_FIGURES, USAGE_FIGURES,
};
use super::store::{Bucket, RolledApp, Sink, Stat, Tier};

pub const MAGIC: &[u8; 8] = b"TBRMHIST";
pub const VERSION: u16 = 1;
const HEADER_LEN: usize = 28;
const EXT: &str = "rmh";
/// A safety net far above the expected size (a full worst-case week is 6.8 MB, see the tests).
pub const MAX_DISK_BYTES: u64 = 64 * 1024 * 1024;
/// A frame longer than this is damage, not data (a bucket with 32 apps is under 2 KB).
const MAX_FRAME: u32 = 256 * 1024;
const PRUNE_EVERY_MS: i64 = 3_600_000;
/// Buffered bytes that force a write before the minute is over.
const FLUSH_BYTES: usize = 256 * 1024;

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    /// Every step.
    Fine,
    /// Per-minute buckets.
    Minute,
    /// 5-minute buckets, events and markers.
    Day,
}

pub const LEVELS: [Level; 3] = [Level::Fine, Level::Minute, Level::Day];

impl Level {
    fn prefix(self) -> &'static str {
        match self {
            Level::Fine => "fine",
            Level::Minute => "minute",
            Level::Day => "day",
        }
    }

    fn code(self) -> u8 {
        match self {
            Level::Fine => 1,
            Level::Minute => 2,
            Level::Day => 3,
        }
    }

    fn from_code(c: u8) -> Option<Level> {
        LEVELS.into_iter().find(|l| l.code() == c)
    }

    /// The time one file covers.
    pub fn span_ms(self) -> i64 {
        match self {
            Level::Fine => 10 * 60_000,
            Level::Minute => HOUR_MS,
            Level::Day => DAY_MS,
        }
    }

    /// How far back this level is kept.
    pub fn keep_ms(self) -> i64 {
        match self {
            Level::Fine => HOUR_MS,
            Level::Minute => DAY_MS,
            Level::Day => 7 * DAY_MS,
        }
    }

    fn period(self, utc_ms: i64) -> i64 {
        utc_ms.div_euclid(self.span_ms()) * self.span_ms()
    }
}

/// The history folder: the add-on's data folder next to the host's (Preview builds included),
/// or the shared Toolboks data folder for a standalone run.
pub fn directory() -> PathBuf {
    directory_for(
        &std::env::current_exe().unwrap_or_default(),
        toolboks_shared::storage::data_root(),
    )
}

fn directory_for(exe: &Path, fallback: PathBuf) -> PathBuf {
    // Installed add-ons live in `<root>/addons/<id>/<exe>`.
    let installed = exe
        .parent()
        .and_then(Path::parent)
        .filter(|p| p.file_name().is_some_and(|n| n == crate::ID))
        .and_then(Path::parent)
        .filter(|p| p.file_name().is_some_and(|n| n == "addons"))
        .and_then(Path::parent);
    installed
        .map(Path::to_owned)
        .unwrap_or(fallback)
        .join("addon-data")
        .join(crate::ID)
        .join("history")
}

/// A run id that no other recorder in this process has used, normally the start time (ms).
pub fn new_run_id(utc_ms: i64) -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let want = utc_ms.max(1) as u64;
    let mut prev = LAST.load(Ordering::Relaxed);
    loop {
        let next = want.max(prev + 1);
        match LAST.compare_exchange(prev, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(p) => prev = p,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// File names: `<level>-YYYYMMDD-HHMM-<run hex>.rmh`, the period start in UTC.

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's civil-from-days.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn file_name(level: Level, period_ms: i64, run: u64) -> String {
    let (y, mo, d) = civil(period_ms.div_euclid(DAY_MS));
    let in_day = period_ms.rem_euclid(DAY_MS) / 60_000;
    format!(
        "{}-{y:04}{mo:02}{d:02}-{:02}{:02}-{run:016x}.{EXT}",
        level.prefix(),
        in_day / 60,
        in_day % 60
    )
}

/// What a file name says: level, period start and run. `None` for any other file.
pub fn parse_name(name: &str) -> Option<(Level, i64, u64)> {
    let stem = name.strip_suffix(&format!(".{EXT}"))?;
    let mut parts = stem.split('-');
    let (lv, date, time, run) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || date.len() != 8 || time.len() != 4 || run.len() != 16 {
        return None;
    }
    let level = LEVELS.into_iter().find(|l| l.prefix() == lv)?;
    let num = |s: &str| -> Option<i64> {
        s.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| s.parse().ok())?
    };
    let (y, mo, d) = (num(&date[..4])?, num(&date[4..6])?, num(&date[6..])?);
    let (h, mi) = (num(&time[..2])?, num(&time[2..])?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 {
        return None;
    }
    let period = days_from_civil(y, mo as u32, d as u32) * DAY_MS + h * HOUR_MS + mi * 60_000;
    let run = u64::from_str_radix(run, 16).ok()?;
    Some((level, period, run))
}

// ---------------------------------------------------------------------------------------------
// Encoding

const TAG_NAME: u8 = 1;
const TAG_STEP: u8 = 2;
const TAG_BUCKET: u8 = 3;
const TAG_EVENT: u8 = 4;
const TAG_MARKER: u8 = 5;

fn fnv(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in bytes {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[derive(Default)]
struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn stat(&mut self, s: Stat) {
        self.f32(s.avg);
        self.f32(s.peak);
    }
    fn usage(&mut self, u: &Usage) {
        self.f32(u.cpu);
        self.u64(u.mem);
        self.f32(u.gpu);
        self.f32(u.disk_read);
        self.f32(u.disk_write);
    }
    fn totals(&mut self, t: &SystemTotals) {
        self.f32(t.cpu);
        self.u64(t.mem_used);
        self.u64(t.mem_total);
        let mask = u8::from(t.gpu_load.is_some())
            | u8::from(t.gpu_mem_used.is_some()) << 1
            | u8::from(t.gpu_temp_c.is_some()) << 2
            | u8::from(t.gpu_power_w.is_some()) << 3;
        self.u8(mask);
        if let Some(v) = t.gpu_load {
            self.f32(v);
        }
        if let Some(v) = t.gpu_mem_used {
            self.u64(v);
        }
        if let Some(v) = t.gpu_temp_c {
            self.f32(v);
        }
        if let Some(v) = t.gpu_power_w {
            self.f32(v);
        }
        self.f32(t.disk_read_bps);
        self.f32(t.disk_write_bps);
        self.f32(t.net_down_bps);
        self.f32(t.net_up_bps);
    }
}

/// Appends one frame (tag, length, payload, checksum) to `out`.
fn frame(out: &mut Vec<u8>, tag: u8, payload: &[u8]) {
    let start = out.len();
    out.push(tag);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    let sum = fnv(&out[start..]);
    out.extend_from_slice(&sum.to_le_bytes());
}

fn header(level: Level, run: u64, period: i64) -> Vec<u8> {
    let mut e = Enc::default();
    e.0.extend_from_slice(MAGIC);
    e.u16(VERSION);
    e.u8(level.code());
    e.u8(0);
    e.u64(run);
    e.i64(period);
    e.0
}

fn enc_step(r: &StepRecord) -> Vec<u8> {
    let mut e = Enc::default();
    e.u64(r.seq);
    e.i64(r.utc_ms);
    e.f32(r.secs);
    e.totals(&r.totals);
    e.u16(r.other.apps);
    e.u32(r.other.procs);
    e.usage(&r.other.usage);
    e.u16(r.apps.len() as u16);
    for a in &r.apps {
        e.u32(a.app);
        e.u16(a.procs);
        e.u8(u8::from(a.disk_known));
        e.usage(&a.usage);
    }
    e.0
}

fn enc_bucket(b: &Bucket) -> Vec<u8> {
    let mut e = Enc::default();
    e.i64(b.start_utc_ms);
    e.u64(b.first_seq);
    e.u64(b.last_seq);
    e.f32(b.covered_secs);
    let mut mask = 0u16;
    for (i, t) in b.totals.iter().enumerate() {
        if t.is_some() {
            mask |= 1 << i;
        }
    }
    e.u16(mask);
    for s in b.totals.iter().flatten() {
        e.stat(*s);
    }
    for s in b.other {
        e.stat(s);
    }
    e.u16(b.apps.len() as u16);
    for a in &b.apps {
        e.u32(a.app);
        e.f32(a.active_secs);
        e.u16(a.peak_procs);
        e.u8(u8::from(a.disk_known));
        for s in a.figures {
            e.stat(s);
        }
    }
    e.0
}

fn enc_event(ev: &AppEvent) -> Vec<u8> {
    let mut e = Enc::default();
    e.u64(ev.seq);
    e.i64(ev.utc_ms);
    e.u32(ev.app);
    e.u8(match ev.kind {
        EventKind::AlreadyRunning => 0,
        EventKind::Started => 1,
        EventKind::Exited => 2,
    });
    e.u8(u8::from(ev.during_gap));
    e.0
}

fn enc_marker(m: &Marker) -> Vec<u8> {
    let mut e = Enc::default();
    e.u64(m.seq);
    e.i64(m.utc_ms);
    let (kind, extra) = match m.kind {
        MarkerKind::Began => (0, 0),
        MarkerKind::Gap { since_utc_ms } => (1, since_utc_ms),
        MarkerKind::ClockChanged { before_utc_ms } => (2, before_utc_ms),
    };
    e.u8(kind);
    e.i64(extra);
    e.0
}

// ---------------------------------------------------------------------------------------------
// Decoding

struct Dec<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Dec<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, at: 0 }
    }
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let s = self.b.get(self.at..self.at + N)?;
        self.at += N;
        s.try_into().ok()
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.at..self.at + n)?;
        self.at += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take().map(u16::from_le_bytes)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }
    fn i64(&mut self) -> Option<i64> {
        self.take().map(i64::from_le_bytes)
    }
    fn f32(&mut self) -> Option<f32> {
        self.take().map(f32::from_le_bytes)
    }
    fn stat(&mut self) -> Option<Stat> {
        Some(Stat {
            avg: self.f32()?,
            peak: self.f32()?,
        })
    }
    fn usage(&mut self) -> Option<Usage> {
        Some(Usage {
            cpu: self.f32()?,
            mem: self.u64()?,
            gpu: self.f32()?,
            disk_read: self.f32()?,
            disk_write: self.f32()?,
        })
    }
    fn totals(&mut self) -> Option<SystemTotals> {
        let cpu = self.f32()?;
        let mem_used = self.u64()?;
        let mem_total = self.u64()?;
        let mask = self.u8()?;
        let gpu_load = if mask & 1 != 0 {
            Some(self.f32()?)
        } else {
            None
        };
        let gpu_mem_used = if mask & 2 != 0 {
            Some(self.u64()?)
        } else {
            None
        };
        let gpu_temp_c = if mask & 4 != 0 {
            Some(self.f32()?)
        } else {
            None
        };
        let gpu_power_w = if mask & 8 != 0 {
            Some(self.f32()?)
        } else {
            None
        };
        Some(SystemTotals {
            cpu,
            mem_used,
            mem_total,
            gpu_load,
            gpu_mem_used,
            gpu_temp_c,
            gpu_power_w,
            disk_read_bps: self.f32()?,
            disk_write_bps: self.f32()?,
            net_down_bps: self.f32()?,
            net_up_bps: self.f32()?,
        })
    }
}

fn dec_step(d: &mut Dec) -> Option<StepRecord> {
    let seq = d.u64()?;
    let utc_ms = d.i64()?;
    let secs = d.f32()?;
    let totals = d.totals()?;
    let other = Folded {
        apps: d.u16()?,
        procs: d.u32()?,
        usage: d.usage()?,
    };
    let n = d.u16()? as usize;
    let mut apps = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        apps.push(AppUsage {
            app: d.u32()?,
            procs: d.u16()?,
            disk_known: d.u8()? != 0,
            usage: d.usage()?,
        });
    }
    Some(StepRecord {
        seq,
        utc_ms,
        secs,
        totals,
        apps,
        other,
    })
}

fn dec_bucket(d: &mut Dec) -> Option<Bucket> {
    let start_utc_ms = d.i64()?;
    let first_seq = d.u64()?;
    let last_seq = d.u64()?;
    let covered_secs = d.f32()?;
    let mask = d.u16()?;
    let mut totals = [None; TOTAL_FIGURES];
    for (i, t) in totals.iter_mut().enumerate() {
        if mask & (1 << i) != 0 {
            *t = Some(d.stat()?);
        }
    }
    let mut other = [Stat::default(); USAGE_FIGURES];
    for s in &mut other {
        *s = d.stat()?;
    }
    let n = d.u16()? as usize;
    let mut apps = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        let app = d.u32()?;
        let active_secs = d.f32()?;
        let peak_procs = d.u16()?;
        let disk_known = d.u8()? != 0;
        let mut figures = [Stat::default(); USAGE_FIGURES];
        for s in &mut figures {
            *s = d.stat()?;
        }
        apps.push(RolledApp {
            app,
            active_secs,
            peak_procs,
            disk_known,
            figures,
        });
    }
    Some(Bucket {
        start_utc_ms,
        first_seq,
        last_seq,
        covered_secs,
        totals,
        apps,
        other,
    })
}

fn dec_event(d: &mut Dec) -> Option<AppEvent> {
    Some(AppEvent {
        seq: d.u64()?,
        utc_ms: d.i64()?,
        app: d.u32()?,
        kind: match d.u8()? {
            0 => EventKind::AlreadyRunning,
            1 => EventKind::Started,
            2 => EventKind::Exited,
            _ => return None,
        },
        during_gap: d.u8()? != 0,
    })
}

fn dec_name<'a>(d: &mut Dec<'a>) -> Option<(AppId, &'a str)> {
    let id = d.u32()?;
    let n = d.u8()? as usize;
    Some((id, std::str::from_utf8(d.bytes(n)?).ok()?))
}

fn dec_marker(d: &mut Dec) -> Option<Marker> {
    let seq = d.u64()?;
    let utc_ms = d.i64()?;
    let kind = d.u8()?;
    let extra = d.i64()?;
    let kind = match kind {
        0 => MarkerKind::Began,
        1 => MarkerKind::Gap {
            since_utc_ms: extra,
        },
        2 => MarkerKind::ClockChanged {
            before_utc_ms: extra,
        },
        _ => return None,
    };
    Some(Marker { seq, utc_ms, kind })
}

// ---------------------------------------------------------------------------------------------
// Writer

struct OpenFile {
    level: Level,
    period: i64,
    path: PathBuf,
    /// Bytes written to disk so far (0 = the header is still in `buf` or not written).
    exists: bool,
    buf: Vec<u8>,
    /// App ids whose name frame is already in this file.
    named: HashSet<AppId>,
}

/// The file writer: a [`Sink`] the recorder calls on its own thread.
pub struct Writer {
    dir: PathBuf,
    run: u64,
    files: HashMap<Level, OpenFile>,
    last_prune_ms: i64,
    /// The last write error, reported once until a write succeeds again.
    error: Option<String>,
}

impl Writer {
    /// Makes the folder and prunes it (`now_ms` is the wall clock, UTC).
    pub fn new(dir: PathBuf, run: u64, now_ms: i64) -> Writer {
        let mut w = Writer {
            dir,
            run,
            files: HashMap::new(),
            last_prune_ms: now_ms,
            error: None,
        };
        if let Err(e) = fs::create_dir_all(&w.dir) {
            w.fail(format!("cannot make the history folder: {e}"));
        }
        prune(&w.dir, now_ms, &HashSet::new());
        w
    }

    fn fail(&mut self, message: String) {
        if self.error.as_deref() != Some(message.as_str()) {
            eprintln!("Resource Monitor history: {message}");
            self.error = Some(message);
        }
    }

    /// The open file of `level` for the time `utc_ms`, switching files when the period moved.
    fn file(&mut self, level: Level, utc_ms: i64) -> &mut OpenFile {
        let period = level.period(utc_ms);
        if self.files.get(&level).is_some_and(|f| f.period != period) {
            if let Some(mut old) = self.files.remove(&level) {
                self.write_out(&mut old);
            }
        }
        let (dir, run) = (&self.dir, self.run);
        self.files.entry(level).or_insert_with(|| {
            let path = dir.join(file_name(level, period, run));
            // The same run can come back to a period after the clock was set back: append.
            let exists = fs::metadata(&path).is_ok_and(|m| m.len() >= HEADER_LEN as u64);
            let buf = if exists {
                Vec::new()
            } else {
                header(level, run, period)
            };
            OpenFile {
                level,
                period,
                path,
                exists,
                buf,
                named: HashSet::new(),
            }
        })
    }

    fn write_out(&mut self, f: &mut OpenFile) {
        if f.buf.is_empty() {
            return;
        }
        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&f.path)
            .and_then(|mut file: File| file.write_all(&f.buf));
        match result {
            Ok(()) => {
                f.exists = true;
                self.error = None;
            }
            Err(e) => {
                let level = f.level.prefix();
                self.fail(format!("cannot write the {level} history: {e}"));
                // Keep only the header so the next write starts a readable file; the names
                // already sent are lost with the buffer.
                if !f.exists {
                    f.buf = header(f.level, self.run, f.period);
                } else {
                    f.buf.clear();
                }
                f.named.clear();
                return;
            }
        }
        f.buf.clear();
    }

    /// Writes every buffer to disk.
    pub fn flush(&mut self) {
        let mut files = std::mem::take(&mut self.files);
        for f in files.values_mut() {
            self.write_out(f);
        }
        self.files = files;
    }

    fn maybe_prune(&mut self, now_ms: i64) {
        if (now_ms - self.last_prune_ms).abs() >= PRUNE_EVERY_MS {
            self.last_prune_ms = now_ms;
            let open: HashSet<PathBuf> = self.files.values().map(|f| f.path.clone()).collect();
            prune(&self.dir, now_ms, &open);
        }
    }

    fn put(
        &mut self,
        level: Level,
        utc_ms: i64,
        names: &NameTable,
        ids: &[AppId],
        tag: u8,
        payload: Vec<u8>,
    ) {
        let f = self.file(level, utc_ms);
        for &id in ids {
            if f.named.insert(id) {
                if let Some(name) = names.name(id) {
                    let mut e = Enc::default();
                    e.u32(id);
                    e.u8(name.len() as u8);
                    e.0.extend_from_slice(name.as_bytes());
                    frame(&mut f.buf, TAG_NAME, &e.0);
                }
            }
        }
        frame(&mut f.buf, tag, &payload);
        if f.buf.len() >= FLUSH_BYTES {
            let mut f = self.files.remove(&level).expect("just used");
            self.write_out(&mut f);
            self.files.insert(level, f);
        }
    }
}

impl Sink for Writer {
    fn step(&mut self, names: &NameTable, r: &StepRecord) {
        let ids: Vec<AppId> = r.apps.iter().map(|a| a.app).collect();
        self.put(Level::Fine, r.utc_ms, names, &ids, TAG_STEP, enc_step(r));
        self.maybe_prune(r.utc_ms);
    }

    fn bucket(&mut self, names: &NameTable, tier: Tier, b: &Bucket) {
        let level = match tier {
            Tier::Minute => Level::Minute,
            Tier::FiveMinutes => Level::Day,
        };
        let ids: Vec<AppId> = b.apps.iter().map(|a| a.app).collect();
        self.put(
            level,
            b.start_utc_ms,
            names,
            &ids,
            TAG_BUCKET,
            enc_bucket(b),
        );
        // About once a minute: everything buffered goes to disk in one go.
        if tier == Tier::Minute {
            self.flush();
        }
    }

    fn event(&mut self, names: &NameTable, e: &AppEvent) {
        self.put(
            Level::Day,
            e.utc_ms,
            names,
            &[e.app],
            TAG_EVENT,
            enc_event(e),
        );
    }

    fn marker(&mut self, m: &Marker) {
        self.put(
            Level::Day,
            m.utc_ms,
            &NameTable::default(),
            &[],
            TAG_MARKER,
            enc_marker(m),
        );
    }

    fn finish(&mut self) {
        self.flush();
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.flush();
    }
}

// ---------------------------------------------------------------------------------------------
// Pruning and size

/// Our history files in `dir`: path, level, period start, run, size.
pub fn list(dir: &Path) -> Vec<(PathBuf, Level, i64, u64, u64)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some((level, period, run)) = name.to_str().and_then(parse_name) else {
            continue;
        };
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        out.push((e.path(), level, period, run, size));
    }
    out
}

/// Total bytes of the history files in `dir`.
pub fn size_on_disk(dir: &Path) -> u64 {
    list(dir).iter().map(|f| f.4).sum()
}

/// Deletes the files whose whole period is older than their level's window, files dated more
/// than a week ahead (a clock that was wrong), and then the oldest files while the total is
/// over [`MAX_DISK_BYTES`]. Files in `open` are kept. Returns the number deleted.
pub fn prune(dir: &Path, now_ms: i64, open: &HashSet<PathBuf>) -> usize {
    let mut files = list(dir);
    let mut removed = 0;
    files.retain(|(path, level, period, _, _)| {
        let old = period + level.span_ms() <= now_ms - level.keep_ms();
        let future = *period > now_ms + 7 * DAY_MS;
        if (old || future) && !open.contains(path) && fs::remove_file(path).is_ok() {
            removed += 1;
            return false;
        }
        true
    });
    let mut total: u64 = files.iter().map(|f| f.4).sum();
    if total > MAX_DISK_BYTES {
        files.sort_by_key(|f| (f.2, f.1));
        for (path, _, _, _, size) in &files {
            if total <= MAX_DISK_BYTES {
                break;
            }
            if !open.contains(path) && fs::remove_file(path).is_ok() {
                total -= size;
                removed += 1;
            }
        }
    }
    removed
}

// ---------------------------------------------------------------------------------------------
// Reader

/// A record with the run that wrote it.
#[derive(Clone, Debug, PartialEq)]
pub struct Stamped<T> {
    pub run: u64,
    pub rec: T,
}

/// History read back from disk. App ids are those of `names` (every file's own ids are mapped
/// into it, so one exe name has one id across runs). Each list is in time order.
#[derive(Debug, Default)]
pub struct Loaded {
    pub names: NameTable,
    pub steps: Vec<Stamped<StepRecord>>,
    pub minutes: Vec<Stamped<Bucket>>,
    pub five_minutes: Vec<Stamped<Bucket>>,
    pub events: Vec<Stamped<AppEvent>>,
    pub markers: Vec<Stamped<Marker>>,
    /// One line per file that could not be read in full. The rest still loaded.
    pub problems: Vec<String>,
}

/// What to read.
#[derive(Clone, Debug)]
pub struct Query {
    pub from_ms: i64,
    pub to_ms: i64,
    pub levels: Vec<Level>,
    /// Leave out this run (the one recording now, which the reader takes from memory).
    pub skip_run: Option<u64>,
    /// The wall clock now: records older than their level's window are left out.
    pub now_ms: i64,
}

/// Reads the records in `[from_ms, to_ms)` of the chosen levels. Never fails: a file that
/// cannot be read is reported in [`Loaded::problems`] and skipped.
pub fn load(dir: &Path, q: &Query) -> Loaded {
    let mut out = Loaded::default();
    let mut files = list(dir);
    files.sort_by_key(|f| (f.2, f.3, f.1));
    for (path, level, period, run, _) in files {
        if !q.levels.contains(&level) || Some(run) == q.skip_run {
            continue;
        }
        let oldest = q.from_ms.max(q.now_ms - level.keep_ms());
        if period + level.span_ms() <= oldest || period >= q.to_ms {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match fs::read(&path) {
            Ok(bytes) => {
                if let Err(problem) = read_file(&bytes, level, run, oldest, q.to_ms, &mut out) {
                    out.problems.push(format!("{name}: {problem}"));
                }
            }
            Err(e) => out.problems.push(format!("{name}: {e}")),
        }
    }
    out.steps.sort_by_key(|s| (s.rec.utc_ms, s.run, s.rec.seq));
    out.minutes.sort_by_key(|s| (s.rec.start_utc_ms, s.run));
    out.five_minutes
        .sort_by_key(|s| (s.rec.start_utc_ms, s.run));
    out.events.sort_by_key(|s| (s.rec.utc_ms, s.run, s.rec.seq));
    out.markers
        .sort_by_key(|s| (s.rec.utc_ms, s.run, s.rec.seq));
    out
}

/// Reads one file into `out`, keeping the records in `[from, to)`. On damage the frames read
/// so far are kept and the problem is returned.
fn read_file(
    bytes: &[u8],
    level: Level,
    run: u64,
    from: i64,
    to: i64,
    out: &mut Loaded,
) -> Result<(), String> {
    let mut d = Dec::new(bytes);
    if d.bytes(8) != Some(MAGIC.as_slice()) {
        return Err("not a history file; skipped".into());
    }
    let version = d.u16().ok_or("cut short; skipped")?;
    if version != VERSION {
        return Err(format!(
            "format version {version} is not supported; skipped"
        ));
    }
    let (Some(code), Some(_), Some(file_run), Some(_)) = (d.u8(), d.u8(), d.u64(), d.i64()) else {
        return Err("cut short; skipped".into());
    };
    if Level::from_code(code) != Some(level) || file_run != run {
        return Err("header does not match the file name; skipped".into());
    }
    // File id -> id in `out.names`.
    let mut ids: HashMap<AppId, AppId> = HashMap::new();
    let mut frames = 0usize;
    while d.at < bytes.len() {
        let start = d.at;
        let (Some(tag), Some(len)) = (d.u8(), d.u32()) else {
            return Err(format!("cut short after {frames} records"));
        };
        if len > MAX_FRAME {
            return Err(format!("damaged after {frames} records"));
        }
        let Some(payload) = d.bytes(len as usize) else {
            return Err(format!("cut short after {frames} records"));
        };
        let Some(sum) = d.u32() else {
            return Err(format!("cut short after {frames} records"));
        };
        if fnv(&bytes[start..start + 5 + len as usize]) != sum {
            return Err(format!("damaged after {frames} records"));
        }
        frames += 1;
        let mut p = Dec::new(payload);
        let ok = match tag {
            TAG_NAME => dec_name(&mut p).map(|(id, name)| {
                if let Some(global) = out.names.intern(name) {
                    ids.insert(id, global);
                }
            }),
            TAG_STEP => dec_step(&mut p).map(|mut r| {
                if (from..to).contains(&r.utc_ms) {
                    remap_step(&mut r, &ids);
                    out.steps.push(Stamped { run, rec: r });
                }
            }),
            TAG_BUCKET => dec_bucket(&mut p).map(|mut b| {
                if (from..to).contains(&b.start_utc_ms) {
                    remap_bucket(&mut b, &ids);
                    let list = if level == Level::Minute {
                        &mut out.minutes
                    } else {
                        &mut out.five_minutes
                    };
                    list.push(Stamped { run, rec: b });
                }
            }),
            TAG_EVENT => dec_event(&mut p).map(|mut e| {
                if (from..to).contains(&e.utc_ms) {
                    if let Some(g) = ids.get(&e.app) {
                        e.app = *g;
                        out.events.push(Stamped { run, rec: e });
                    }
                }
            }),
            TAG_MARKER => dec_marker(&mut p).map(|m| {
                if (from..to).contains(&m.utc_ms) {
                    out.markers.push(Stamped { run, rec: m });
                }
            }),
            // A record kind from a later build of the same version: skip it.
            _ => Some(()),
        };
        if ok.is_none() {
            return Err(format!("damaged after {} records", frames - 1));
        }
    }
    Ok(())
}

/// Maps file ids to loaded ids. An app whose name was never seen (or did not fit the name
/// table) is added to "Everything else", so the totals still add up.
fn remap_step(r: &mut StepRecord, ids: &HashMap<AppId, AppId>) {
    let mut kept = Vec::with_capacity(r.apps.len());
    for mut a in r.apps.drain(..) {
        match ids.get(&a.app) {
            Some(g) => {
                a.app = *g;
                kept.push(a);
            }
            None => {
                r.other.apps += 1;
                r.other.procs += a.procs as u32;
                r.other.usage.add(&a.usage);
            }
        }
    }
    r.apps = kept;
}

fn remap_bucket(b: &mut Bucket, ids: &HashMap<AppId, AppId>) {
    let mut kept = Vec::with_capacity(b.apps.len());
    for mut a in b.apps.drain(..) {
        match ids.get(&a.app) {
            Some(g) => {
                a.app = *g;
                kept.push(a);
            }
            None => {
                for (o, s) in b.other.iter_mut().zip(a.figures) {
                    o.avg += s.avg;
                    o.peak += s.peak;
                }
            }
        }
    }
    b.apps = kept;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::store::Store;
    use super::*;

    /// A fresh folder next to the test exe (the build disk, never C:).
    pub(crate) fn test_dir(name: &str) -> PathBuf {
        let base = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_owned))
            .unwrap_or_else(std::env::temp_dir);
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = base.join("rm-history-tests").join(format!(
            "{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 2026-11-17 00:00 UTC.
    const DAY0: i64 = 20_774 * DAY_MS;

    fn usage(cpu: f32, mem: u64) -> Usage {
        Usage {
            cpu,
            mem,
            gpu: 1.5,
            disk_read: 1000.0,
            disk_write: 2000.0,
        }
    }

    fn step(seq: u64, utc_ms: i64, apps: &[(AppId, f32)]) -> StepRecord {
        StepRecord {
            seq,
            utc_ms,
            secs: 5.0,
            totals: SystemTotals {
                cpu: 30.0,
                mem_used: 8 << 30,
                mem_total: 16 << 30,
                gpu_load: Some(12.0),
                gpu_mem_used: Some(1 << 30),
                gpu_temp_c: None,
                gpu_power_w: None,
                disk_read_bps: 1e6,
                disk_write_bps: 2e6,
                net_down_bps: 3e5,
                net_up_bps: 4e4,
            },
            apps: apps
                .iter()
                .map(|&(app, cpu)| AppUsage {
                    app,
                    procs: 3,
                    disk_known: app % 2 == 0,
                    usage: usage(cpu, 200 << 20),
                })
                .collect(),
            other: Folded {
                apps: 100,
                procs: 300,
                usage: usage(4.0, 1 << 30),
            },
        }
    }

    fn bucket(start: i64, napps: u32) -> Bucket {
        let stat = Stat {
            avg: 10.0,
            peak: 50.0,
        };
        Bucket {
            start_utc_ms: start,
            first_seq: 1,
            last_seq: 12,
            covered_secs: 60.0,
            totals: [
                Some(stat),
                Some(stat),
                None,
                Some(stat),
                None,
                None,
                Some(stat),
                Some(stat),
                Some(stat),
                Some(stat),
            ],
            apps: (0..napps)
                .map(|app| RolledApp {
                    app,
                    active_secs: 60.0,
                    peak_procs: 4,
                    disk_known: true,
                    figures: [stat; USAGE_FIGURES],
                })
                .collect(),
            other: [stat; USAGE_FIGURES],
        }
    }

    fn names(n: usize) -> NameTable {
        let mut t = NameTable::default();
        for i in 0..n {
            t.intern(&format!("application-number-{i}.exe"));
        }
        t
    }

    fn all(now: i64) -> Query {
        Query {
            from_ms: i64::MIN / 2,
            to_ms: i64::MAX / 2,
            levels: LEVELS.to_vec(),
            skip_run: None,
            now_ms: now,
        }
    }

    #[test]
    fn file_names_round_trip_and_others_are_ignored() {
        let t = DAY0 + 13 * HOUR_MS + 40 * 60_000;
        let name = file_name(Level::Fine, t, 0xabc);
        assert_eq!(name, "fine-20261117-1340-0000000000000abc.rmh");
        assert_eq!(parse_name(&name), Some((Level::Fine, t, 0xabc)));
        assert_eq!(parse_name(&file_name(Level::Day, 0, 1)).unwrap().1, 0);
        for bad in [
            "fine-20261117-1340-abc.rmh",
            "x-20261117-1340-0000000000000abc.rmh",
            "fine-20261317-1340-0000000000000abc.rmh",
            "notes.txt",
        ] {
            assert_eq!(parse_name(bad), None, "{bad}");
        }
        assert_eq!(civil(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(days_from_civil(2026, 11, 17), DAY0 / DAY_MS);
    }

    #[test]
    fn the_folder_is_in_the_add_on_data_folder() {
        let installed =
            Path::new("D:/x/Toolboks Preview/addons/toolboks.resource-monitor/0.2.0/addon.exe");
        assert_eq!(
            directory_for(installed, PathBuf::from("C:/other")),
            PathBuf::from("D:/x/Toolboks Preview/addon-data/toolboks.resource-monitor/history")
        );
        assert_eq!(
            directory_for(
                Path::new("E:/target/debug/resource-monitor.exe"),
                PathBuf::from("C:/data")
            ),
            PathBuf::from("C:/data/addon-data/toolboks.resource-monitor/history")
        );
    }

    #[test]
    fn history_written_by_one_run_is_read_back_by_the_next() {
        let dir = test_dir("roundtrip");
        let t = DAY0 + 10 * HOUR_MS;
        let mut store = Store::default();
        let chrome = store.names.intern("chrome.exe").unwrap();
        let code = store.names.intern("Code.exe").unwrap();
        let first = step(1, t + 5000, &[(chrome, 40.0), (code, 3.0)]);
        let ev = AppEvent {
            seq: 1,
            utc_ms: t + 5000,
            app: code,
            kind: EventKind::Started,
            during_gap: false,
        };
        let marker = Marker {
            seq: 2,
            utc_ms: t + 70_000,
            kind: MarkerKind::Gap {
                since_utc_ms: t + 10_000,
            },
        };
        {
            let mut sinks: Vec<Box<dyn Sink>> = vec![Box::new(Writer::new(dir.clone(), 7, t))];
            store.push_step(first.clone(), &mut sinks);
            store.push_step(step(2, t + 10_000, &[(code, 9.0)]), &mut sinks);
            store.push_event(ev, &mut sinks);
            store.push_marker(marker, &mut sinks);
            store.close_buckets(&mut sinks);
            // Dropping the writer (recording stopped, add-on quit) writes what is buffered.
        }
        // The next run has its own name table: chrome.exe gets another id there.
        let loaded = load(&dir, &all(t + 60_000));
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(loaded.steps.len(), 2);
        let s = &loaded.steps[0];
        assert_eq!(s.run, 7);
        let name = |id| loaded.names.name(id).unwrap();
        assert_eq!(name(s.rec.apps[0].app), "chrome.exe");
        assert_eq!(name(s.rec.apps[1].app), "Code.exe");
        // Every figure comes back exactly.
        let mut expected = first;
        for a in &mut expected.apps {
            a.app = loaded.names.id(store.names.name(a.app).unwrap()).unwrap();
        }
        assert_eq!(s.rec, expected);
        assert_eq!(loaded.minutes.len(), 1);
        assert_eq!(loaded.five_minutes.len(), 1);
        let m = &loaded.minutes[0].rec;
        assert_eq!((m.covered_secs, m.apps.len()), (10.0, 2));
        assert_eq!(m, &{
            let mut b = store.tier(Tier::Minute).last().unwrap().clone();
            for a in &mut b.apps {
                a.app = loaded.names.id(store.names.name(a.app).unwrap()).unwrap();
            }
            b
        });
        assert_eq!(loaded.events.len(), 1);
        assert_eq!(name(loaded.events[0].rec.app), "Code.exe");
        assert_eq!(loaded.markers[0].rec, marker);
        // Leaving out the run that is recording now.
        let q = Query {
            skip_run: Some(7),
            ..all(t)
        };
        assert!(load(&dir, &q).steps.is_empty());
    }

    #[test]
    fn records_are_buffered_and_written_once_a_minute() {
        let dir = test_dir("batch");
        let t = DAY0 + HOUR_MS;
        let mut store = Store::default();
        let a = store.names.intern("a.exe").unwrap();
        let mut sinks: Vec<Box<dyn Sink>> = vec![Box::new(Writer::new(dir.clone(), 1, t))];
        for i in 0..11 {
            store.push_step(step(i + 1, t + i as i64 * 5000, &[(a, 1.0)]), &mut sinks);
        }
        assert_eq!(size_on_disk(&dir), 0, "nothing written inside the minute");
        // The first step of the next minute closes the minute bucket: one write of everything.
        store.push_step(step(13, t + 60_000, &[(a, 1.0)]), &mut sinks);
        let after_minute = load(&dir, &all(t));
        assert_eq!(after_minute.steps.len(), 11);
        assert_eq!(after_minute.minutes.len(), 1);
        drop(sinks);
        assert_eq!(load(&dir, &all(t)).steps.len(), 12);
    }

    /// A week of history at the worst case the recorder can make (24 apps in every step, 32
    /// in every bucket, every step and bucket present) stays within the 20 MB budget (#40).
    #[test]
    fn a_week_stays_within_the_size_budget_and_old_records_are_pruned() {
        let dir = test_dir("week");
        let names = names(400);
        let start = DAY0;
        let end = start + 7 * DAY_MS;
        let mut w = Writer::new(dir.clone(), 3, start);
        let step_apps: Vec<(AppId, f32)> = (0..24).map(|a| (a, 1.0)).collect();
        // Buckets for the whole week; steps for the last two hours only (older steps would be
        // pruned anyway, and writing them all costs 100 MB of test I/O).
        let mut seq = 0;
        let mut t = start;
        while t < end {
            if t % 300_000 == 0 {
                w.bucket(&names, Tier::FiveMinutes, &bucket(t, 32));
            }
            if t >= end - 2 * HOUR_MS {
                for s in 0..12 {
                    seq += 1;
                    w.step(&names, &step(seq, t + s * 5000, &step_apps));
                }
            } else {
                // Keep the hourly prune going as the recorder would.
                w.maybe_prune(t);
            }
            if t % 3_600_000 == 0 {
                w.event(
                    &names,
                    &AppEvent {
                        seq,
                        utc_ms: t,
                        app: (t / HOUR_MS % 400) as u32,
                        kind: EventKind::Started,
                        during_gap: false,
                    },
                );
            }
            w.bucket(&names, Tier::Minute, &bucket(t, 32));
            t += 60_000;
        }
        drop(w);
        let now = end;
        prune(&dir, now, &HashSet::new());
        let bytes = size_on_disk(&dir);
        println!(
            "a week on disk: {:.1} MB in {} files",
            bytes as f64 / 1e6,
            list(&dir).len()
        );
        assert!(bytes < 20 * 1024 * 1024, "{bytes} bytes");
        // Records older than each window are gone: no file holds only records past its window,
        // and the reader returns nothing older than the window.
        for (path, level, period, _, _) in list(&dir) {
            assert!(
                period + level.span_ms() > now - level.keep_ms(),
                "{path:?} should be pruned"
            );
        }
        let loaded = load(&dir, &all(now));
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert!(loaded.steps.iter().all(|s| s.rec.utc_ms >= now - HOUR_MS));
        assert_eq!(loaded.steps.len(), 720, "the last hour of 5 s steps");
        assert!(loaded
            .minutes
            .iter()
            .all(|b| b.rec.start_utc_ms >= now - DAY_MS));
        assert_eq!(loaded.minutes.len(), 24 * 60);
        assert_eq!(loaded.five_minutes.len(), 7 * 24 * 12);
        assert!(loaded
            .five_minutes
            .iter()
            .all(|b| b.rec.start_utc_ms >= now - 7 * DAY_MS));
        // On disk the oldest minute file ends within the window: at most one file span more.
        let oldest_minute = list(&dir)
            .iter()
            .filter(|f| f.1 == Level::Minute)
            .map(|f| f.2)
            .min()
            .unwrap();
        assert!(oldest_minute >= now - DAY_MS - HOUR_MS);
    }

    #[test]
    fn prune_keeps_the_open_files_and_caps_the_total() {
        let dir = test_dir("prune");
        let now = DAY0 + 10 * DAY_MS;
        let open_old = dir.join(file_name(Level::Fine, now - 3 * HOUR_MS, 1));
        let old = dir.join(file_name(Level::Fine, now - 2 * HOUR_MS, 1));
        let future = dir.join(file_name(Level::Day, now + 9 * DAY_MS, 1));
        let fresh = dir.join(file_name(Level::Day, now - DAY_MS, 1));
        let other = dir.join("readme.txt");
        for p in [&open_old, &old, &future, &fresh, &other] {
            fs::write(p, b"x").unwrap();
        }
        let open: HashSet<PathBuf> = [open_old.clone()].into();
        assert_eq!(prune(&dir, now, &open), 2);
        assert!(open_old.exists() && fresh.exists() && other.exists());
        assert!(!old.exists() && !future.exists());
        // Over the cap: the oldest go first.
        let big = vec![0u8; (MAX_DISK_BYTES / 2 + 1) as usize];
        let a = dir.join(file_name(Level::Day, now - 3 * DAY_MS, 2));
        let b = dir.join(file_name(Level::Day, now - 2 * DAY_MS, 2));
        fs::write(&a, &big).unwrap();
        fs::write(&b, &big).unwrap();
        prune(&dir, now, &HashSet::new());
        assert!(!a.exists() && b.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_or_damaged_file_is_skipped_while_the_others_still_load() {
        let dir = test_dir("damage");
        let t = DAY0 + 5 * HOUR_MS;
        // Three runs, one file each (fine level).
        for run in 1..=3u64 {
            let mut store = Store::default();
            let a = store.names.intern("a.exe").unwrap();
            let mut sinks: Vec<Box<dyn Sink>> = vec![Box::new(Writer::new(dir.clone(), run, t))];
            for i in 0..4 {
                store.push_step(
                    step(
                        i + 1,
                        t + run as i64 * 60_000 + i as i64 * 5000,
                        &[(a, 1.0)],
                    ),
                    &mut sinks,
                );
            }
        }
        let path = |run| dir.join(file_name(Level::Fine, t, run));
        // Run 1: cut in the middle of its third record (a crash during a write).
        let bytes = fs::read(path(1)).unwrap();
        fs::write(path(1), &bytes[..bytes.len() - bytes.len() / 3]).unwrap();
        // Run 2: a byte flipped in the header.
        let mut bytes = fs::read(path(2)).unwrap();
        bytes[3] ^= 0xff;
        fs::write(path(2), &bytes).unwrap();
        // Run 3 is fine; plus a file damaged inside a record.
        let mut bytes = fs::read(path(3)).unwrap();
        let n = bytes.len();
        bytes[n - 40] ^= 0x55;
        fs::write(dir.join(file_name(Level::Fine, t, 4)), &bytes).unwrap();
        let loaded = load(&dir, &all(t + 5 * 60_000));
        let runs = |r| loaded.steps.iter().filter(|s| s.run == r).count();
        assert_eq!(runs(3), 4, "the good file loads in full");
        assert!(
            runs(1) >= 1 && runs(1) < 4,
            "the cut file keeps the records before the cut"
        );
        assert_eq!(runs(2), 0, "a bad header skips the file");
        assert!(
            runs(4) == 0,
            "the run id in the copy's header does not match its name"
        );
        assert_eq!(loaded.problems.len(), 3, "{:?}", loaded.problems);
        assert!(loaded.problems.iter().any(|p| p.contains("cut short")));
        // A damaged record (good header) keeps what came before it.
        let mut bytes = fs::read(path(3)).unwrap();
        let n = bytes.len();
        bytes[n - 40] ^= 0x55;
        fs::write(path(3), &bytes).unwrap();
        let loaded = load(&dir, &all(t + 5 * 60_000));
        assert_eq!(loaded.steps.iter().filter(|s| s.run == 3).count(), 3);
        assert!(
            loaded
                .problems
                .iter()
                .any(|p| p.contains("damaged after 4 records")),
            "{:?}",
            loaded.problems
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_format_is_skipped_not_misread() {
        let dir = test_dir("version");
        let t = DAY0;
        let mut h = header(Level::Day, 9, t);
        h[8] = 2;
        fs::write(dir.join(file_name(Level::Day, t, 9)), &h).unwrap();
        let loaded = load(&dir, &all(t));
        assert!(loaded.problems[0].contains("version 2"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_ids_are_unique_in_a_process() {
        let a = new_run_id(1000);
        let b = new_run_id(1000);
        assert!(b > a);
    }
}
