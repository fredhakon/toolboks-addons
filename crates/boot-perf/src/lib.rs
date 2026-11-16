//! Boot time and slow-startup items, from the Windows boot performance log.
//!
//! Source: the event log `Microsoft-Windows-Diagnostics-Performance/Operational`, provider
//! `Microsoft-Windows-Diagnostics-Performance`, task "Boot Performance Monitoring", event ids
//! 100 to 110. Everything here is read-only and needs no changes to Windows.
//!
//! What the ids mean (Microsoft Learn, archived TechNet wiki "Event ID 100: Windows Diagnostics
//! Performance", <https://learn.microsoft.com/en-us/archive/technet-wiki/11559.event-id-100-windows-diagnostics-performance>,
//! section "Boot Performance Events 100-110"):
//!
//! | id  | meaning                                                     |
//! |-----|-------------------------------------------------------------|
//! | 100 | Windows has started up (boot summary, "Boot Duration")      |
//! | 101 | an application took longer than usual to start              |
//! | 102 | a driver took longer to initialize                          |
//! | 103 | a startup service took longer than expected to start        |
//! | 106 | the background optimization operation took longer           |
//! | 107 | applying machine policy slowed startup                      |
//! | 108 | applying user policy slowed startup                         |
//! | 109 | a device took longer to initialize                          |
//! | 110 | session manager initialization slowed startup               |
//!
//! Microsoft's page names the ids and, for event 100, says `BootTime` = `MainPathBootTime` +
//! `BootPostBootTime`, both in milliseconds. It does not publish the field list of 101 to 110.
//! The field names used below (event 100: `BootTime`, `MainPathBootTime`, `BootPostBootTime`,
//! `BootNumStartupApps`, `BootIsDegradation`; events 101 to 103 and 109: `Name`, `FriendlyName`,
//! `Path`, `CompanyName`, `TotalTime`, `DegradationTime`; event 110: `Name`, `TotalTime`,
//! `DegradationTime`) come from the provider's event manifest as mirrored by
//! <https://detection-wiki-dev.pages.dev/microsoft-windows-diagnostics-performance/100> (and
//! `/101`, `/103`, `/109`, `/110`), and the example on the Microsoft Learn page for event 102
//! ("Total Time: 1027ms", "Degradation Time: 1026ms") confirms they are milliseconds. They were NOT
//! checked against a real log on the PC this was written on, because that log can only be read by
//! administrators (see [`Access::Denied`]). Every field is therefore read defensively: a missing or
//! damaged optional field becomes `None`, never a made-up number.
//!
//! Two things the data cannot promise, and the views must say so:
//! * Microsoft's page describes event 100 as logged when Windows "has started up and degradation
//!   has been detected", so the log may hold a boot time only for some boots. A boot with no entry
//!   is not known to be fast.
//! * The log can be unreadable (access denied), missing or switched off. That is not "fast boot".

mod read;

pub use read::{load, Access, BootRead, BootSource, FetchError, Fetched, WinBootSource};

/// The channel the events are in.
pub const CHANNEL: &str = "Microsoft-Windows-Diagnostics-Performance/Operational";
/// The provider every boot performance event comes from.
pub const PROVIDER: &str = "Microsoft-Windows-Diagnostics-Performance";

/// The longest boot time believed (24 hours). Anything above is a damaged value.
const MAX_BOOT_MS: u64 = 86_400_000;

/// What Windows named as slow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SlowKind {
    App,
    Driver,
    Service,
    Device,
    BackgroundOptimization,
    MachinePolicy,
    UserPolicy,
    SessionManager,
}

impl SlowKind {
    /// The event id to kind, `None` for 100 (the boot summary) and ids that are not slow items.
    pub fn from_event_id(id: u32) -> Option<Self> {
        Some(match id {
            101 => Self::App,
            102 => Self::Driver,
            103 => Self::Service,
            106 => Self::BackgroundOptimization,
            107 => Self::MachinePolicy,
            108 => Self::UserPolicy,
            109 => Self::Device,
            110 => Self::SessionManager,
            _ => return None,
        })
    }

    /// A plain word for the kind, lower case.
    pub fn plain(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Driver => "driver",
            Self::Service => "service",
            Self::Device => "device",
            Self::BackgroundOptimization => "background optimization",
            Self::MachinePolicy => "machine policy",
            Self::UserPolicy => "user policy",
            Self::SessionManager => "session manager",
        }
    }

    /// Kinds that carry a program or service name of their own (events 101 to 103 and 109). The
    /// other kinds may have no name, and the page then names the kind.
    fn needs_name(self) -> bool {
        matches!(
            self,
            Self::App | Self::Driver | Self::Service | Self::Device
        )
    }
}

/// One boot summary (event 100).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boot {
    /// When Windows logged it, ms since 1970 UTC. This is after the boot, not the power-on time.
    pub logged_ms: i64,
    /// `BootTime`: the whole boot, in ms.
    pub boot_ms: u64,
    /// `MainPathBootTime`, ms.
    pub main_path_ms: Option<u64>,
    /// `BootPostBootTime`, ms.
    pub post_boot_ms: Option<u64>,
    /// `BootNumStartupApps`.
    pub startup_apps: Option<u32>,
    /// `BootIsDegradation`: Windows judged this boot slower than usual. `None` = not recorded.
    pub slower_than_usual: Option<bool>,
}

/// One program, service, driver or device Windows named as slow (events 101 to 103, 106 to 110).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlowItem {
    pub kind: SlowKind,
    pub logged_ms: i64,
    /// `Name`: for an app the file name, for a service its short name. May be empty for the
    /// policy, session manager and optimization kinds.
    pub name: String,
    pub friendly_name: Option<String>,
    pub path: Option<String>,
    pub company: Option<String>,
    /// `TotalTime`, ms.
    pub total_ms: Option<u64>,
    /// `DegradationTime`: how much longer than expected, ms.
    pub delay_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    Boot(Boot),
    Slow(SlowItem),
}

/// Why a record could not be used. Never turned into a number: the record is skipped and counted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Not a boot performance event (other provider, or an id this module does not read).
    NotBootEvent,
    /// No event id, provider or time, or a time that does not parse.
    DamagedHeader,
    /// A field the record cannot do without is missing, empty or not a number.
    MissingField(&'static str),
    /// A value outside what is possible (a boot of zero ms, or longer than a day).
    Implausible(&'static str),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotBootEvent => write!(f, "not a boot performance event"),
            Self::DamagedHeader => write!(f, "the event header is missing or damaged"),
            Self::MissingField(n) => write!(f, "the field {n} is missing or damaged"),
            Self::Implausible(n) => write!(f, "the field {n} holds an impossible value"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing

/// Parses the XML of one event as Event Log renders it (EvtRender, event XML).
pub fn parse_xml(xml: &str) -> Result<Parsed, ParseError> {
    let provider = open_tag(xml, "Provider")
        .and_then(|attrs| attr_value(attrs, "Name"))
        .ok_or(ParseError::DamagedHeader)?;
    if provider != PROVIDER {
        return Err(ParseError::NotBootEvent);
    }
    let id: u32 = element_text(xml, "EventID")
        .and_then(|t| t.trim().parse().ok())
        .ok_or(ParseError::DamagedHeader)?;
    let logged_ms = open_tag(xml, "TimeCreated")
        .and_then(|attrs| attr_value(attrs, "SystemTime"))
        .and_then(|t| parse_iso_time(&t))
        .ok_or(ParseError::DamagedHeader)?;
    parse_fields(id, logged_ms, &data_pairs(xml))
}

/// Parses an event whose header is already known (id, time) and whose data values are named
/// (`EventData` / `Data Name='...'`). Names are compared without regard to case.
pub fn parse_fields(
    id: u32,
    logged_ms: i64,
    data: &[(String, String)],
) -> Result<Parsed, ParseError> {
    let get = |name: &str| -> Option<&str> {
        data.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    };
    let number = |name: &str| -> Option<u64> { get(name).and_then(|v| v.parse().ok()) };
    let text = |name: &str| -> Option<String> { get(name).map(str::to_string) };

    if id == 100 {
        let boot_ms = number("BootTime").ok_or(ParseError::MissingField("BootTime"))?;
        if boot_ms == 0 || boot_ms > MAX_BOOT_MS {
            return Err(ParseError::Implausible("BootTime"));
        }
        return Ok(Parsed::Boot(Boot {
            logged_ms,
            boot_ms,
            main_path_ms: number("MainPathBootTime").filter(|v| *v <= MAX_BOOT_MS),
            post_boot_ms: number("BootPostBootTime").filter(|v| *v <= MAX_BOOT_MS),
            startup_apps: number("BootNumStartupApps").and_then(|v| u32::try_from(v).ok()),
            slower_than_usual: get("BootIsDegradation").and_then(parse_bool),
        }));
    }

    let kind = SlowKind::from_event_id(id).ok_or(ParseError::NotBootEvent)?;
    let name = text("Name");
    let friendly_name = text("FriendlyName");
    if kind.needs_name() && name.is_none() && friendly_name.is_none() {
        return Err(ParseError::MissingField("Name"));
    }
    Ok(Parsed::Slow(SlowItem {
        kind,
        logged_ms,
        name: name.unwrap_or_default(),
        friendly_name,
        path: text("Path"),
        company: text("CompanyName"),
        total_ms: number("TotalTime"),
        delay_ms: number("DegradationTime"),
    }))
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

/// The attribute text of the first `<name ...>` tag (without the angle brackets).
fn open_tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("<{name}");
    let mut from = 0;
    while let Some(p) = xml[from..].find(&needle) {
        let start = from + p + needle.len();
        let next = xml[start..].chars().next()?;
        if next.is_whitespace() || next == '>' || next == '/' {
            let end = xml[start..].find('>')? + start;
            return Some(xml[start..end].trim_end_matches('/'));
        }
        from = start;
    }
    None
}

/// The value of `key='...'` or `key="..."` in a tag's attribute text, unescaped.
fn attr_value(attrs: &str, key: &str) -> Option<String> {
    let mut rest = attrs;
    loop {
        let eq = rest.find('=')?;
        let k = rest[..eq].trim();
        let after = rest[eq + 1..].trim_start();
        let quote = after.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let body = &after[1..];
        let end = body.find(quote)?;
        if k == key {
            return Some(unescape(&body[..end]));
        }
        rest = &body[end + 1..];
    }
}

/// The text between `<name ...>` and `</name>` of the first such element.
fn element_text<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let attrs = open_tag(xml, name)?;
    let start = (attrs.as_ptr() as usize - xml.as_ptr() as usize) + attrs.len();
    let from = xml[start..].find('>')? + start + 1;
    let close = format!("</{name}>");
    let to = xml[from..].find(&close)? + from;
    Some(&xml[from..to])
}

/// Every `<Data Name='x'>value</Data>` of the event, in order. Unnamed values are `Data<N>`.
fn data_pairs(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(p) = rest.find("<Data") {
        rest = &rest[p + "<Data".len()..];
        let Some(next) = rest.chars().next() else {
            break;
        };
        if !(next.is_whitespace() || next == '>' || next == '/') {
            continue;
        }
        let Some(gt) = rest.find('>') else { break };
        let attrs = &rest[..gt];
        let name = attr_value(attrs.trim_end_matches('/'), "Name")
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("Data{}", out.len() + 1));
        if attrs.trim_end().ends_with('/') {
            out.push((name, String::new()));
            rest = &rest[gt + 1..];
            continue;
        }
        let body = &rest[gt + 1..];
        let Some(end) = body.find("</Data>") else {
            break;
        };
        out.push((name, unescape(&body[..end])));
        rest = &body[end + "</Data>".len()..];
    }
    out
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let decoded = rest.find(';').filter(|e| *e <= 10).and_then(|e| {
            let ent = &rest[1..e];
            let ch = match ent {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => ent.strip_prefix('#').and_then(|n| {
                    let v = match n.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok(),
                        None => n.parse().ok(),
                    };
                    v.and_then(char::from_u32)
                }),
            };
            ch.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2026-10-29T21:45:49.1234567Z` to ms since 1970 (fraction optional).
fn parse_iso_time(s: &str) -> Option<i64> {
    let s = s.trim().strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let m: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    if dp.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut tp = hms.split(':');
    let hh: i64 = tp.next()?.parse().ok()?;
    let mm: i64 = tp.next()?.parse().ok()?;
    let ss: i64 = tp.next()?.parse().ok()?;
    if tp.next().is_some() || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    if frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ms: i64 = frac
        .bytes()
        .take(3)
        .zip([100, 10, 1])
        .map(|(b, w)| i64::from(b - b'0') * w)
        .sum();
    Some(days_from_civil(y, m, d) * 86_400_000 + hh * 3_600_000 + mm * 60_000 + ss * 1000 + ms)
}

// ---------------------------------------------------------------------------------------------
// What the log holds, summarised

/// Everything read from the log, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BootLog {
    pub boots: Vec<Boot>,
    pub slow: Vec<SlowItem>,
    /// Records that could not be used (damaged, or unreadable). Never counted as boots.
    pub skipped: usize,
    /// The read stopped early or hit its record limit: older entries may be missing.
    pub incomplete: bool,
}

impl BootLog {
    /// The oldest record of any kind, ms since 1970.
    pub fn oldest_ms(&self) -> Option<i64> {
        self.boots
            .iter()
            .map(|b| b.logged_ms)
            .chain(self.slow.iter().map(|s| s.logged_ms))
            .min()
    }

    pub fn is_empty(&self) -> bool {
        self.boots.is_empty() && self.slow.is_empty()
    }
}

/// One program, service, driver or device, folded over every time Windows named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlowSummary {
    pub kind: SlowKind,
    /// What to call it: the friendly name, else the name, else the kind.
    pub label: String,
    /// `Name` as Windows wrote it (may be empty).
    pub name: String,
    pub friendly_name: Option<String>,
    pub path: Option<String>,
    /// How many times it was named in the period.
    pub times: usize,
    pub worst_total_ms: Option<u64>,
    pub worst_delay_ms: Option<u64>,
    pub last_ms: i64,
}

fn summary_key(item: &SlowItem) -> (SlowKind, String) {
    let key = [
        Some(item.name.as_str()),
        item.friendly_name.as_deref(),
        item.path.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find(|v| !v.trim().is_empty())
    .unwrap_or("")
    .to_ascii_lowercase();
    (item.kind, key)
}

/// Folds the slow items by kind and name; worst delay first, then most often named, then name.
pub fn summarize(items: &[SlowItem]) -> Vec<SlowSummary> {
    let mut out: Vec<((SlowKind, String), SlowSummary)> = Vec::new();
    for item in items {
        let key = summary_key(item);
        let at = match out.iter().position(|(k, _)| *k == key) {
            Some(at) => at,
            None => {
                let label = item
                    .friendly_name
                    .clone()
                    .or_else(|| (!item.name.is_empty()).then(|| item.name.clone()))
                    .unwrap_or_else(|| item.kind.plain().to_string());
                out.push((
                    key,
                    SlowSummary {
                        kind: item.kind,
                        label,
                        name: item.name.clone(),
                        friendly_name: item.friendly_name.clone(),
                        path: item.path.clone(),
                        times: 0,
                        worst_total_ms: None,
                        worst_delay_ms: None,
                        last_ms: item.logged_ms,
                    },
                ));
                out.len() - 1
            }
        };
        let s = &mut out[at].1;
        s.times += 1;
        s.worst_total_ms = s.worst_total_ms.max(item.total_ms);
        s.worst_delay_ms = s.worst_delay_ms.max(item.delay_ms);
        s.last_ms = s.last_ms.max(item.logged_ms);
        if s.path.is_none() {
            s.path = item.path.clone();
        }
    }
    let mut list: Vec<SlowSummary> = out.into_iter().map(|(_, s)| s).collect();
    list.sort_by(|a, b| {
        b.worst_delay_ms
            .cmp(&a.worst_delay_ms)
            .then(b.times.cmp(&a.times))
            .then(a.label.to_lowercase().cmp(&b.label.to_lowercase()))
    });
    list
}

/// Median of the boot times, ms. `None` for no boots.
pub fn median_boot_ms(boots: &[Boot]) -> Option<u64> {
    if boots.is_empty() {
        return None;
    }
    let mut v: Vec<u64> = boots.iter().map(|b| b.boot_ms).collect();
    v.sort_unstable();
    let mid = v.len() / 2;
    Some(if v.len() % 2 == 1 {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2
    })
}

/// `850 ms`, `9.4 s`, `42 s`, `1 min 12 s`, `1 h 5 min`.
pub fn duration_text(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 10_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else if ms < 60_000 {
        format!("{} s", (ms + 500) / 1000)
    } else if ms < 3_600_000 {
        let s = (ms + 500) / 1000;
        if s.is_multiple_of(60) {
            format!("{} min", s / 60)
        } else {
            format!("{} min {} s", s / 60, s % 60)
        }
    } else {
        let m = (ms + 30_000) / 60_000;
        if m.is_multiple_of(60) {
            format!("{} h", m / 60)
        } else {
            format!("{} h {} min", m / 60, m % 60)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Matching a startup entry to what Windows measured

/// The file name of the program in a command line, as written in the Run key, the startup
/// folder or the log's `Path` value: `"C:\A B\x.exe" --y` and `C:\A B\x.exe /y` both give `x.exe`.
/// Lower case. `None` when the command names no file.
pub fn exe_name(command: &str) -> Option<String> {
    let command = command.trim();
    let path = if let Some(rest) = command.strip_prefix('"') {
        rest.split('"').next().unwrap_or("")
    } else {
        let lower = command.to_ascii_lowercase();
        match lower.find(".exe") {
            Some(p) => &command[..p + 4],
            None => command.split_whitespace().next().unwrap_or(""),
        }
    };
    let file = path.rsplit(['\\', '/']).next().unwrap_or("").trim();
    (!file.is_empty()).then(|| file.to_ascii_lowercase())
}

fn stem(name: &str) -> &str {
    for ext in [".exe", ".sys", ".dll"] {
        if let Some(s) = name.strip_suffix(ext) {
            return s;
        }
    }
    name
}

/// Lower-case names a summary can be known by.
fn summary_names(s: &SlowSummary) -> Vec<String> {
    let mut v = Vec::new();
    let mut add = |n: &str| {
        let n = n.trim().to_ascii_lowercase();
        if !n.is_empty() {
            v.push(stem(&n).to_string());
            v.push(n);
        }
    };
    add(&s.name);
    if let Some(f) = &s.friendly_name {
        add(f);
    }
    if let Some(e) = s.path.as_deref().and_then(exe_name) {
        add(&e);
    }
    v
}

/// The measured cost for a startup entry, or `None`. Only exact matches count (never a guess):
/// the entry's program file name, or its name, must equal the name, friendly name or program file
/// of an app or service Windows named as slow, ignoring case and a trailing `.exe`. When both an
/// app and a service match, the one with the larger delay is returned.
pub fn find_match<'a>(
    summaries: &'a [SlowSummary],
    entry_name: &str,
    command: &str,
) -> Option<&'a SlowSummary> {
    let mut keys = Vec::new();
    for k in [
        Some(entry_name.trim().to_ascii_lowercase()),
        exe_name(command),
    ]
    .into_iter()
    .flatten()
    {
        if !k.is_empty() {
            keys.push(stem(&k).to_string());
            keys.push(k);
        }
    }
    summaries
        .iter()
        .filter(|s| matches!(s.kind, SlowKind::App | SlowKind::Service))
        .filter(|s| {
            let names = summary_names(s);
            keys.iter().any(|k| names.contains(k))
        })
        .max_by_key(|s| (s.worst_delay_ms, s.times))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xml(id: u32, time: &str, data: &str) -> String {
        format!(
            "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><Provider Name='Microsoft-Windows-Diagnostics-Performance' Guid='{{CFC18EC0-96B1-4EBA-961B-622CAEE05B0A}}'/><EventID>{id}</EventID><Version>2</Version><Level>3</Level><Task>4002</Task><Opcode>34</Opcode><Keywords>0x8000000000000000</Keywords><TimeCreated SystemTime='{time}'/><EventRecordID>1234</EventRecordID><Channel>Microsoft-Windows-Diagnostics-Performance/Operational</Channel><Computer>PC</Computer></System><EventData>{data}</EventData></Event>"
        )
    }

    fn d(name: &str, value: &str) -> String {
        format!("<Data Name='{name}'>{value}</Data>")
    }

    const T: &str = "2026-11-10T08:15:30.1234567Z";

    #[test]
    fn event_100_boot_summary() {
        let data = [
            d("BootTime", "72340"),
            d("MainPathBootTime", "41000"),
            d("BootPostBootTime", "31340"),
            d("BootNumStartupApps", "9"),
            d("BootIsDegradation", "true"),
        ]
        .concat();
        let Ok(Parsed::Boot(b)) = parse_xml(&xml(100, T, &data)) else {
            panic!("not a boot");
        };
        assert_eq!(b.boot_ms, 72_340);
        assert_eq!(b.main_path_ms, Some(41_000));
        assert_eq!(b.post_boot_ms, Some(31_340));
        assert_eq!(b.startup_apps, Some(9));
        assert_eq!(b.slower_than_usual, Some(true));
        assert_eq!(
            b.logged_ms,
            parse_iso_time("2026-11-10T08:15:30.123Z").unwrap()
        );
    }

    #[test]
    fn event_100_with_only_the_boot_time() {
        let Ok(Parsed::Boot(b)) = parse_xml(&xml(100, T, &d("BootTime", "54000"))) else {
            panic!("not a boot");
        };
        assert_eq!(b.boot_ms, 54_000);
        assert_eq!(b.main_path_ms, None);
        assert_eq!(b.post_boot_ms, None);
        assert_eq!(b.startup_apps, None);
        assert_eq!(b.slower_than_usual, None);
    }

    #[test]
    fn event_100_damaged_or_missing_boot_time_is_an_error_never_zero() {
        assert_eq!(
            parse_xml(&xml(100, T, &d("MainPathBootTime", "41000"))),
            Err(ParseError::MissingField("BootTime"))
        );
        assert_eq!(
            parse_xml(&xml(100, T, &d("BootTime", "fast"))),
            Err(ParseError::MissingField("BootTime"))
        );
        assert_eq!(
            parse_xml(&xml(100, T, &d("BootTime", ""))),
            Err(ParseError::MissingField("BootTime"))
        );
        assert_eq!(
            parse_xml(&xml(100, T, &d("BootTime", "-5"))),
            Err(ParseError::MissingField("BootTime"))
        );
        assert_eq!(
            parse_xml(&xml(100, T, &d("BootTime", "0"))),
            Err(ParseError::Implausible("BootTime"))
        );
        assert_eq!(
            parse_xml(&xml(100, T, &d("BootTime", "99999999999"))),
            Err(ParseError::Implausible("BootTime"))
        );
        assert_eq!(
            parse_xml(&xml(100, T, "")),
            Err(ParseError::MissingField("BootTime"))
        );
    }

    #[test]
    fn event_100_damaged_optional_fields_become_none() {
        let data = [
            d("BootTime", "60000"),
            d("MainPathBootTime", "x"),
            d("BootPostBootTime", "999999999999"),
            d("BootNumStartupApps", "-1"),
            d("BootIsDegradation", "maybe"),
        ]
        .concat();
        let Ok(Parsed::Boot(b)) = parse_xml(&xml(100, T, &data)) else {
            panic!("not a boot");
        };
        assert_eq!(b.boot_ms, 60_000);
        assert_eq!(b.main_path_ms, None);
        assert_eq!(b.post_boot_ms, None);
        assert_eq!(b.startup_apps, None);
        assert_eq!(b.slower_than_usual, None);
    }

    #[test]
    fn event_101_slow_app() {
        let data = [
            d("StartTime", "2026-11-10T08:14:10.0000000Z"),
            d("NameLength", "11"),
            d("Name", "Spotify.exe"),
            d("FriendlyName", "Spotify"),
            d("Version", "1.2.3"),
            d("TotalTime", "21000"),
            d("DegradationTime", "14000"),
            d(
                "Path",
                r#""C:\Users\x\AppData\Roaming\Spotify\Spotify.exe""#,
            ),
            d("CompanyName", "Spotify AB"),
        ]
        .concat();
        let Ok(Parsed::Slow(s)) = parse_xml(&xml(101, T, &data)) else {
            panic!("not slow");
        };
        assert_eq!(s.kind, SlowKind::App);
        assert_eq!(s.name, "Spotify.exe");
        assert_eq!(s.friendly_name.as_deref(), Some("Spotify"));
        assert_eq!(s.total_ms, Some(21_000));
        assert_eq!(s.delay_ms, Some(14_000));
        assert_eq!(s.company.as_deref(), Some("Spotify AB"));
        assert!(s.path.unwrap().ends_with("Spotify.exe\""));
    }

    #[test]
    fn event_102_driver_103_service_109_device() {
        let drv = [
            d("Name", "nvlddmkm.sys"),
            d("TotalTime", "1027"),
            d("DegradationTime", "1026"),
        ]
        .concat();
        let Ok(Parsed::Slow(s)) = parse_xml(&xml(102, T, &drv)) else {
            panic!()
        };
        assert_eq!(
            (s.kind, s.total_ms, s.delay_ms),
            (SlowKind::Driver, Some(1027), Some(1026))
        );

        let svc = [
            d("Name", "windefend"),
            d("FriendlyName", ""),
            d("TotalTime", "9000"),
        ]
        .concat();
        let Ok(Parsed::Slow(s)) = parse_xml(&xml(103, T, &svc)) else {
            panic!()
        };
        assert_eq!(s.kind, SlowKind::Service);
        assert_eq!(s.name, "windefend");
        assert_eq!(s.friendly_name, None);
        assert_eq!(s.delay_ms, None);

        let dev = [d("FriendlyName", "USB Root Hub"), d("TotalTime", "3000")].concat();
        let Ok(Parsed::Slow(s)) = parse_xml(&xml(109, T, &dev)) else {
            panic!()
        };
        assert_eq!(s.kind, SlowKind::Device);
        assert_eq!(s.name, "");
        assert_eq!(s.friendly_name.as_deref(), Some("USB Root Hub"));
    }

    #[test]
    fn events_106_107_108_110_may_have_no_name() {
        for (id, kind) in [
            (106, SlowKind::BackgroundOptimization),
            (107, SlowKind::MachinePolicy),
            (108, SlowKind::UserPolicy),
            (110, SlowKind::SessionManager),
        ] {
            let data = [d("TotalTime", "8000"), d("DegradationTime", "5000")].concat();
            let Ok(Parsed::Slow(s)) = parse_xml(&xml(id, T, &data)) else {
                panic!("{id}")
            };
            assert_eq!(s.kind, kind);
            assert_eq!(s.name, "");
            assert_eq!(s.delay_ms, Some(5000));
        }
    }

    #[test]
    fn slow_item_without_a_name_is_an_error_for_apps_services_drivers_devices() {
        for id in [101, 102, 103, 109] {
            assert_eq!(
                parse_xml(&xml(id, T, &d("TotalTime", "8000"))),
                Err(ParseError::MissingField("Name")),
                "{id}"
            );
        }
    }

    #[test]
    fn slow_item_with_damaged_times_keeps_the_name_and_no_numbers() {
        let data = [
            d("Name", "x.exe"),
            d("TotalTime", "lots"),
            d("DegradationTime", ""),
        ]
        .concat();
        let Ok(Parsed::Slow(s)) = parse_xml(&xml(101, T, &data)) else {
            panic!()
        };
        assert_eq!((s.total_ms, s.delay_ms), (None, None));
    }

    #[test]
    fn escapes_unnamed_values_and_self_closing_data() {
        let data = "<Data Name='Name'>A &amp; B&apos;s.exe</Data><Data Name='Version'/><Data>stray</Data><Data Name='TotalTime'>5</Data>";
        let Ok(Parsed::Slow(s)) = parse_xml(&xml(101, T, data)) else {
            panic!()
        };
        assert_eq!(s.name, "A & B's.exe");
        assert_eq!(s.total_ms, Some(5));
        let pairs = data_pairs(&xml(101, T, data));
        assert_eq!(pairs[1], ("Version".to_string(), String::new()));
        assert_eq!(pairs[2].0, "Data3");
    }

    #[test]
    fn double_quoted_attributes_and_event_id_qualifiers() {
        let x = r#"<Event><System><Provider Name="Microsoft-Windows-Diagnostics-Performance"/><EventID Qualifiers="16384">103</EventID><TimeCreated SystemTime="2026-11-10T08:15:30Z"/></System><EventData><Data Name="Name">wuauserv</Data></EventData></Event>"#;
        let Ok(Parsed::Slow(s)) = parse_xml(x) else {
            panic!()
        };
        assert_eq!(s.kind, SlowKind::Service);
        assert_eq!(s.name, "wuauserv");
    }

    #[test]
    fn other_providers_and_other_ids_are_not_boot_events() {
        let other = xml(100, T, &d("BootTime", "1000"))
            .replace("Microsoft-Windows-Diagnostics-Performance'", "EventLog'");
        assert_eq!(parse_xml(&other), Err(ParseError::NotBootEvent));
        assert_eq!(
            parse_xml(&xml(200, T, &d("ShutdownTime", "1000"))),
            Err(ParseError::NotBootEvent)
        );
        assert_eq!(
            parse_xml(&xml(105, T, &d("Name", "x"))),
            Err(ParseError::NotBootEvent)
        );
    }

    #[test]
    fn damaged_headers() {
        assert_eq!(parse_xml(""), Err(ParseError::DamagedHeader));
        assert_eq!(
            parse_xml("<Event><System></System></Event>"),
            Err(ParseError::DamagedHeader)
        );
        assert_eq!(
            parse_xml(&xml(100, "yesterday", &d("BootTime", "1000"))),
            Err(ParseError::DamagedHeader)
        );
        let no_id = xml(100, T, &d("BootTime", "1000")).replace("<EventID>100</EventID>", "");
        assert_eq!(parse_xml(&no_id), Err(ParseError::DamagedHeader));
        let cut = &xml(100, T, &d("BootTime", "1000"))[..120];
        assert!(parse_xml(cut).is_err());
    }

    #[test]
    fn time_parsing() {
        assert_eq!(parse_iso_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_time("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(
            parse_iso_time("2024-02-29T00:00:00Z"),
            Some(1_709_164_800_000)
        );
        assert_eq!(parse_iso_time("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso_time("2026-01-01 00:00:00"), None);
    }

    fn item(
        kind: SlowKind,
        name: &str,
        friendly: Option<&str>,
        path: Option<&str>,
        total: Option<u64>,
        delay: Option<u64>,
        at: i64,
    ) -> SlowItem {
        SlowItem {
            kind,
            logged_ms: at,
            name: name.into(),
            friendly_name: friendly.map(str::to_string),
            path: path.map(str::to_string),
            company: None,
            total_ms: total,
            delay_ms: delay,
        }
    }

    #[test]
    fn summarize_folds_by_name_and_sorts_by_worst_delay() {
        let items = [
            item(
                SlowKind::App,
                "Spotify.exe",
                Some("Spotify"),
                None,
                Some(20_000),
                Some(10_000),
                1,
            ),
            item(
                SlowKind::App,
                "spotify.exe",
                None,
                Some("C:\\a\\spotify.exe"),
                Some(25_000),
                Some(15_000),
                5,
            ),
            item(
                SlowKind::Service,
                "WSearch",
                None,
                None,
                Some(30_000),
                Some(2_000),
                3,
            ),
            item(SlowKind::MachinePolicy, "", None, None, Some(9000), None, 2),
            item(SlowKind::MachinePolicy, "", None, None, None, None, 4),
        ];
        let s = summarize(&items);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].label, "Spotify");
        assert_eq!(s[0].times, 2);
        assert_eq!(s[0].worst_total_ms, Some(25_000));
        assert_eq!(s[0].worst_delay_ms, Some(15_000));
        assert_eq!(s[0].last_ms, 5);
        assert_eq!(s[0].path.as_deref(), Some("C:\\a\\spotify.exe"));
        assert_eq!(s[1].label, "WSearch");
        assert_eq!(s[2].label, "machine policy");
        assert_eq!(s[2].times, 2);
        assert_eq!(s[2].worst_total_ms, Some(9000));
        assert_eq!(s[2].worst_delay_ms, None);
        assert!(summarize(&[]).is_empty());
    }

    #[test]
    fn same_name_different_kind_stays_apart() {
        let items = [
            item(SlowKind::App, "x.exe", None, None, None, Some(1), 1),
            item(SlowKind::Driver, "x.exe", None, None, None, Some(1), 1),
        ];
        assert_eq!(summarize(&items).len(), 2);
    }

    #[test]
    fn median_and_durations() {
        let b = |ms| Boot {
            logged_ms: 0,
            boot_ms: ms,
            main_path_ms: None,
            post_boot_ms: None,
            startup_apps: None,
            slower_than_usual: None,
        };
        assert_eq!(median_boot_ms(&[]), None);
        assert_eq!(median_boot_ms(&[b(30), b(10), b(20)]), Some(20));
        assert_eq!(median_boot_ms(&[b(10), b(20)]), Some(15));
        assert_eq!(duration_text(850), "850 ms");
        assert_eq!(duration_text(9_400), "9.4 s");
        assert_eq!(duration_text(42_000), "42 s");
        assert_eq!(duration_text(60_000), "1 min");
        assert_eq!(duration_text(72_340), "1 min 12 s");
        assert_eq!(duration_text(3_900_000), "1 h 5 min");
        assert_eq!(duration_text(7_200_000), "2 h");
    }

    #[test]
    fn exe_names_from_commands() {
        assert_eq!(
            exe_name(r#""C:\Program Files\A B\App.EXE" --silent"#).as_deref(),
            Some("app.exe")
        );
        assert_eq!(
            exe_name(r"C:\Program Files\A B\App.exe /background").as_deref(),
            Some("app.exe")
        );
        assert_eq!(exe_name(r"app.exe").as_deref(), Some("app.exe"));
        assert_eq!(exe_name(r"C:\x\tool.cmd arg").as_deref(), Some("tool.cmd"));
        assert_eq!(exe_name("").as_deref(), None);
        assert_eq!(exe_name(r"C:\x\").as_deref(), None);
    }

    fn sums() -> Vec<SlowSummary> {
        summarize(&[
            item(
                SlowKind::App,
                "Spotify.exe",
                Some("Spotify"),
                Some(r#""C:\Users\x\Spotify.exe""#),
                Some(20_000),
                Some(10_000),
                1,
            ),
            item(
                SlowKind::App,
                "OneDrive.exe",
                Some("Microsoft OneDrive"),
                None,
                Some(8_000),
                Some(3_000),
                1,
            ),
            item(
                SlowKind::Service,
                "WSearch",
                Some("Windows Search"),
                None,
                Some(8_000),
                Some(3_000),
                1,
            ),
            item(
                SlowKind::Driver,
                "steam.sys",
                None,
                None,
                Some(8_000),
                Some(9_000),
                1,
            ),
        ])
    }

    #[test]
    fn matching_by_program_file_or_entry_name() {
        let s = sums();
        let m = find_match(
            &s,
            "Spotify",
            r#""C:\Users\me\AppData\Roaming\Spotify\Spotify.exe" --autostart"#,
        );
        assert_eq!(m.map(|m| m.label.as_str()), Some("Spotify"));
        // The entry name alone is enough, with or without .exe.
        assert!(find_match(&s, "OneDrive", r"C:\x\y.exe").is_some());
        assert!(find_match(&s, "OneDrive.exe", "").is_some());
        assert!(find_match(&s, "Microsoft OneDrive", "").is_some());
        // Match on the program file even when the entry has another name.
        assert!(find_match(&s, "Music player", r"C:\a\SPOTIFY.EXE").is_some());
        // A service by its short or friendly name.
        assert!(find_match(&s, "WSearch", "").is_some());
        assert!(find_match(&s, "Windows Search", "").is_some());
    }

    #[test]
    fn no_match_means_none_never_a_guess() {
        let s = sums();
        assert!(find_match(&s, "Spot", r"C:\x\Spot.exe").is_none());
        assert!(find_match(&s, "Spotify Helper", r"C:\x\SpotifyHelper.exe").is_none());
        assert!(find_match(&s, "Discord", r"C:\x\Update.exe --processStart Discord.exe").is_none());
        assert!(find_match(&s, "", "").is_none());
        assert!(find_match(&[], "Spotify", "spotify.exe").is_none());
        // Drivers are not startup entries.
        assert!(find_match(&s, "steam", r"C:\x\steam.exe").is_none());
    }

    #[test]
    fn matching_prefers_the_larger_delay() {
        let s = summarize(&[
            item(SlowKind::App, "tool.exe", None, None, None, Some(1_000), 1),
            item(SlowKind::Service, "tool", None, None, None, Some(5_000), 1),
        ]);
        let m = find_match(&s, "tool", "").unwrap();
        assert_eq!(m.kind, SlowKind::Service);
    }
}
