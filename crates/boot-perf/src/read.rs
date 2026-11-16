//! Reading the boot performance log. The log reader is behind a trait so the logic that turns
//! "could not read" into words, and keeps an unreadable log from looking like a fast boot, is
//! tested with a fake. Reading is read-only and never needs a Windows setting changed.

use crate::{parse_xml, BootLog, ParseError, Parsed};

/// How many records one read asks for at most (boots plus slow items). Windows keeps about a
/// megabyte of this log, which is far fewer.
const MAX_RECORDS: usize = 5_000;

/// Whether the log could be read, in words the views show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// The log was read. An empty log still reads as readable: see [`BootLog::is_empty`].
    Readable,
    /// Windows refuses the read. On a normal Windows setup only administrators may read this log,
    /// and add-ons never run as administrator.
    Denied,
    /// The log does not exist on this PC.
    Missing,
    /// Anything else, as a plain sentence.
    Failed(String),
}

/// The log and how the read went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootRead {
    pub log: BootLog,
    pub access: Access,
}

/// Why a fetch failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    Denied,
    Missing,
    Other(String),
}

/// What a source returned: the XML of each event, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fetched {
    pub xml: Vec<String>,
    /// Records that were found but could not be rendered.
    pub unreadable: usize,
    /// The read stopped before the end of the log.
    pub interrupted: bool,
}

pub trait BootSource {
    /// Up to `max` boot performance events from the last `lookback_ms`, newest first.
    fn fetch(&self, lookback_ms: i64, max: usize) -> Result<Fetched, FetchError>;
}

/// Reads the log through `src`. A failed read gives an empty log and the reason in `access`; an
/// empty but readable log gives `Access::Readable` with `log.is_empty()`. The views must treat
/// those three cases differently, and none of them as "the boots were fast".
pub fn load(src: &dyn BootSource, lookback_ms: i64) -> BootRead {
    let fetched = match src.fetch(lookback_ms, MAX_RECORDS) {
        Ok(f) => f,
        Err(e) => {
            return BootRead {
                log: BootLog::default(),
                access: match e {
                    FetchError::Denied => Access::Denied,
                    FetchError::Missing => Access::Missing,
                    FetchError::Other(m) => Access::Failed(m),
                },
            }
        }
    };
    let mut log = BootLog {
        skipped: fetched.unreadable,
        incomplete: fetched.interrupted || fetched.xml.len() >= MAX_RECORDS,
        ..Default::default()
    };
    for xml in &fetched.xml {
        match parse_xml(xml) {
            Ok(Parsed::Boot(b)) => log.boots.push(b),
            Ok(Parsed::Slow(s)) => log.slow.push(s),
            Err(ParseError::NotBootEvent) => {}
            Err(_) => log.skipped += 1,
        }
    }
    log.boots.sort_by_key(|b| std::cmp::Reverse(b.logged_ms));
    log.slow.sort_by_key(|s| std::cmp::Reverse(s.logged_ms));
    BootRead {
        log,
        access: Access::Readable,
    }
}

/// The real log. Calls may be slow: use a worker thread, never the UI thread.
pub struct WinBootSource;

#[cfg(not(windows))]
impl BootSource for WinBootSource {
    fn fetch(&self, _lookback_ms: i64, _max: usize) -> Result<Fetched, FetchError> {
        Err(FetchError::Other(
            "Boot timings are only available on Windows".into(),
        ))
    }
}

#[cfg(windows)]
impl BootSource for WinBootSource {
    fn fetch(&self, lookback_ms: i64, max: usize) -> Result<Fetched, FetchError> {
        #[cfg(debug_assertions)]
        if let Some(fixture) = fixture() {
            return fixture;
        }
        win::fetch(lookback_ms, max)
    }
}

/// Debug builds only: when `TOOLBOKS_BOOT_FIXTURE` names a text file, one event XML per line, the
/// events come from it instead of the log. `denied` or `missing` in place of a path pretends that
/// result. It lets a real window show the views without the log; release builds never read it.
#[cfg(all(windows, debug_assertions))]
fn fixture() -> Option<Result<Fetched, FetchError>> {
    let path = std::env::var("TOOLBOKS_BOOT_FIXTURE").ok()?;
    match path.as_str() {
        "denied" => return Some(Err(FetchError::Denied)),
        "missing" => return Some(Err(FetchError::Missing)),
        _ => {}
    }
    let text = std::fs::read_to_string(&path).ok()?;
    Some(Ok(Fetched {
        xml: text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        ..Default::default()
    }))
}

#[cfg(windows)]
mod win {
    use super::{FetchError, Fetched};
    use crate::{CHANNEL, PROVIDER};
    use windows::core::HSTRING;
    use windows::Win32::System::EventLog::{
        EvtClose, EvtNext, EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection, EvtRender,
        EvtRenderEventXml, EVT_HANDLE,
    };

    const ERROR_ACCESS_DENIED: u32 = 5;
    const ERROR_NO_MORE_ITEMS: u32 = 259;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    const ERROR_EVT_INVALID_CHANNEL_PATH: u32 = 15000;
    const ERROR_EVT_CHANNEL_NOT_FOUND: u32 = 15007;

    /// Closes an Event Log handle on drop.
    struct Handle(EVT_HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            if self.0 .0 != 0 {
                // SAFETY: the handle came from the Event Log API and is closed exactly once.
                unsafe {
                    let _ = EvtClose(self.0);
                }
            }
        }
    }

    fn win32_code(e: &windows::core::Error) -> u32 {
        let c = e.code().0 as u32;
        // HRESULT_FROM_WIN32 keeps the code in the low 16 bits.
        if c & 0xFFFF_0000 == 0x8007_0000 {
            c & 0xFFFF
        } else {
            c
        }
    }

    fn render_xml(ev: EVT_HANDLE) -> Option<String> {
        let mut used = 0u32;
        let mut props = 0u32;
        // SAFETY: size-probe call with no buffer; the API reports the needed size in bytes.
        let first = unsafe {
            EvtRender(
                None,
                ev,
                EvtRenderEventXml.0,
                0,
                None,
                &mut used,
                &mut props,
            )
        };
        if let Err(e) = first {
            if win32_code(&e) != ERROR_INSUFFICIENT_BUFFER {
                return None;
            }
        }
        let mut buf = vec![0u16; (used as usize) / 2 + 2];
        let bytes = (buf.len() * 2) as u32;
        // SAFETY: the buffer is `bytes` long and lives across the call.
        let r = unsafe {
            EvtRender(
                None,
                ev,
                EvtRenderEventXml.0,
                bytes,
                Some(buf.as_mut_ptr().cast()),
                &mut used,
                &mut props,
            )
        };
        r.ok()?;
        let chars = ((used as usize) / 2).min(buf.len());
        Some(
            String::from_utf16_lossy(&buf[..chars])
                .trim_end_matches('\0')
                .to_string(),
        )
    }

    /// The boot performance events (ids 100 to 110 of the provider) of the last `lookback_ms`.
    /// `timediff` is the Event Log XPath function: milliseconds since the record was written.
    fn xpath(lookback_ms: i64) -> String {
        format!(
            "*[System[Provider[@Name='{PROVIDER}'] and (EventID >= 100 and EventID <= 110) and TimeCreated[timediff(@SystemTime) <= {}]]]",
            lookback_ms.max(0)
        )
    }

    pub(super) fn fetch(lookback_ms: i64, max: usize) -> Result<Fetched, FetchError> {
        let mut out = Fetched::default();
        if max == 0 {
            return Ok(out);
        }
        // SAFETY: both strings outlive the call; the result is wrapped for closing.
        let results = unsafe {
            EvtQuery(
                None,
                &HSTRING::from(CHANNEL),
                &HSTRING::from(xpath(lookback_ms)),
                EvtQueryChannelPath.0 | EvtQueryReverseDirection.0,
            )
        }
        .map(Handle)
        .map_err(|e| match win32_code(&e) {
            ERROR_ACCESS_DENIED => FetchError::Denied,
            ERROR_EVT_CHANNEL_NOT_FOUND | ERROR_EVT_INVALID_CHANNEL_PATH => FetchError::Missing,
            c => FetchError::Other(format!(
                "Windows could not read the boot performance log (error {c})"
            )),
        })?;
        let mut batch = [0isize; 64];
        loop {
            let want = (max - out.xml.len()).min(batch.len());
            let mut returned = 0u32;
            // SAFETY: `batch[..want]` is valid for writes; `returned` receives the count.
            let r = unsafe { EvtNext(results.0, &mut batch[..want], u32::MAX, 0, &mut returned) };
            let got = (returned as usize).min(want);
            let handles: Vec<Handle> = batch[..got]
                .iter()
                .map(|h| Handle(EVT_HANDLE(*h)))
                .collect();
            for h in &handles {
                match render_xml(h.0) {
                    Some(xml) => out.xml.push(xml),
                    None => out.unreadable += 1,
                }
            }
            if let Err(e) = r {
                if win32_code(&e) != ERROR_NO_MORE_ITEMS {
                    out.interrupted = true;
                }
                break;
            }
            if got == 0 || out.xml.len() >= max {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Result<Fetched, FetchError>);

    impl BootSource for Fake {
        fn fetch(&self, _l: i64, _m: usize) -> Result<Fetched, FetchError> {
            self.0.clone()
        }
    }

    fn event(id: u32, time: &str, data: &str) -> String {
        format!(
            "<Event><System><Provider Name='Microsoft-Windows-Diagnostics-Performance'/><EventID>{id}</EventID><TimeCreated SystemTime='{time}'/></System><EventData>{data}</EventData></Event>"
        )
    }

    #[test]
    fn a_refused_log_is_denied_and_empty_never_a_boot() {
        let r = load(&Fake(Err(FetchError::Denied)), 1000);
        assert_eq!(r.access, Access::Denied);
        assert!(r.log.is_empty());
        assert!(r.log.boots.is_empty());
        let r = load(&Fake(Err(FetchError::Missing)), 1000);
        assert_eq!(r.access, Access::Missing);
        let r = load(&Fake(Err(FetchError::Other("boom".into()))), 1000);
        assert_eq!(r.access, Access::Failed("boom".into()));
        assert!(r.log.is_empty());
    }

    #[test]
    fn an_empty_readable_log_is_readable_and_empty() {
        let r = load(&Fake(Ok(Fetched::default())), 1000);
        assert_eq!(r.access, Access::Readable);
        assert!(r.log.is_empty());
        assert!(!r.log.incomplete);
    }

    #[test]
    fn mixed_events_are_sorted_newest_first_and_damage_is_counted() {
        let xml = vec![
            event(
                100,
                "2026-11-01T08:00:00Z",
                "<Data Name='BootTime'>60000</Data>",
            ),
            event(
                101,
                "2026-11-01T08:00:01Z",
                "<Data Name='Name'>a.exe</Data>",
            ),
            event(
                100,
                "2026-11-09T08:00:00Z",
                "<Data Name='BootTime'>50000</Data>",
            ),
            event(
                100,
                "2026-11-05T08:00:00Z",
                "<Data Name='BootTime'>junk</Data>",
            ),
            "not xml at all".to_string(),
            event(103, "2026-11-09T08:00:01Z", "<Data Name='Name'>svc</Data>"),
        ];
        let r = load(
            &Fake(Ok(Fetched {
                xml,
                unreadable: 2,
                interrupted: false,
            })),
            1000,
        );
        assert_eq!(r.access, Access::Readable);
        assert_eq!(r.log.boots.len(), 2);
        assert_eq!(r.log.boots[0].boot_ms, 50_000);
        assert_eq!(r.log.boots[1].boot_ms, 60_000);
        assert_eq!(r.log.slow.len(), 2);
        assert_eq!(r.log.slow[0].name, "svc");
        // 2 unreadable + 1 damaged BootTime + 1 not xml.
        assert_eq!(r.log.skipped, 4);
        assert!(!r.log.incomplete);
        assert_eq!(
            r.log.oldest_ms(),
            crate::parse_iso_time("2026-11-01T08:00:00Z")
        );
    }

    #[test]
    fn an_interrupted_read_is_marked_incomplete() {
        let r = load(
            &Fake(Ok(Fetched {
                interrupted: true,
                ..Default::default()
            })),
            1000,
        );
        assert!(r.log.incomplete);
    }

    #[test]
    fn other_provider_events_are_ignored_not_counted() {
        let other = event(
            100,
            "2026-11-01T08:00:00Z",
            "<Data Name='BootTime'>1</Data>",
        )
        .replace("Microsoft-Windows-Diagnostics-Performance", "EventLog");
        let r = load(
            &Fake(Ok(Fetched {
                xml: vec![other],
                ..Default::default()
            })),
            1000,
        );
        assert!(r.log.is_empty());
        assert_eq!(r.log.skipped, 0);
    }

    /// Read-only: reads this PC's real log. On a normal Windows setup this reports access denied.
    #[test]
    #[ignore]
    fn real_log_smoke() {
        let r = load(&WinBootSource, 30 * 86_400_000);
        println!(
            "access={:?} boots={} slow={} skipped={} incomplete={}",
            r.access,
            r.log.boots.len(),
            r.log.slow.len(),
            r.log.skipped,
            r.log.incomplete
        );
    }
}
