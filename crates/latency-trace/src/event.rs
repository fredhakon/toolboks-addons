//! Reads the few fields this crate needs out of one tracerpt XML `<Event>` block.
//!
//! This is deliberately not a general XML parser: it looks for the fixed element and attribute
//! names tracerpt writes (see the crate docs) inside a block whose size the scanner has already
//! bounded. Anything it does not recognise becomes [`Parsed::Other`] or [`Parsed::Malformed`].

use crate::time::{parse_u64, parse_wall};
use crate::ExecKind;

/// Class GUIDs from Microsoft's MOF documentation (`PerfInfo`, `Image`, `EventTrace`).
const PERFINFO_GUID: &str = "ce1dbfb4-137e-4da6-87b0-3f59aa102cbc";
const IMAGE_GUID: &str = "2cb15d1d-5fc1-11d2-abe1-00a0c911f518";
const EVENTTRACE_GUID: &str = "68fdd900-4a3e-11d1-84f4-0000f80464e3";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Parsed {
    /// The log file header (`EventTrace`, opcode 0).
    Header {
        wall: Option<u64>,
        start_utc: Option<u64>,
        end_utc: Option<u64>,
        events_lost: Option<u64>,
        buffers_lost: Option<u64>,
    },
    /// A DPC or ISR that finished at `end_wall` (wall-clock FILETIME) and started at
    /// `initial` (FILETIME as written in `InitialTime`).
    Exec {
        kind: ExecKind,
        cpu: u32,
        end_wall: u64,
        initial: u64,
        routine: u64,
    },
    /// An image (module) load, unload or rundown.
    Image {
        base: u64,
        size: u64,
        pid: Option<u64>,
        file_name: String,
    },
    /// A well-formed event this crate does not use.
    Other,
    /// An event this crate should understand but whose fields are missing or unreadable.
    Malformed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    PerfInfo,
    Image,
    EventTrace,
    Unknown,
}

/// Text between the first `open` and the following `close`.
fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let i = s.find(open)? + open.len();
    let j = s[i..].find(close)? + i;
    Some(&s[i..j])
}

/// Value of the first `pattern"value"` occurrence, where `pattern` ends in `="`.
fn attr<'a>(s: &'a str, pattern: &str) -> Option<&'a str> {
    between(s, pattern, "\"")
}

fn class_of(block: &str) -> Class {
    if let Some(g) = between(block, "<EventGuid>", "</EventGuid>") {
        let g = g.trim().trim_start_matches('{').trim_end_matches('}');
        return if g.eq_ignore_ascii_case(PERFINFO_GUID) {
            Class::PerfInfo
        } else if g.eq_ignore_ascii_case(IMAGE_GUID) {
            Class::Image
        } else if g.eq_ignore_ascii_case(EVENTTRACE_GUID) {
            Class::EventTrace
        } else {
            Class::Unknown
        };
    }
    // No class GUID: fall back to the rendered event name.
    match between(block, "<EventName", "</EventName>")
        .and_then(|s| s.split('>').nth(1))
        .map(str::trim)
    {
        Some("PerfInfo") => Class::PerfInfo,
        Some("Image") => Class::Image,
        Some("EventTrace") => Class::EventTrace,
        _ => Class::Unknown,
    }
}

/// The numeric opcode from `<System>`, or the rendered opcode name mapped to its number.
fn opcode_of(system: &str, rendering: Option<&str>) -> Option<u32> {
    if let Some(n) = between(system, "<Opcode>", "</Opcode>").and_then(parse_u64) {
        return u32::try_from(n).ok();
    }
    let name = rendering.and_then(|r| between(r, "<Opcode>", "</Opcode>"))?;
    Some(match name.trim() {
        "Header" => 0,
        "Unload" => 2,
        "DCStart" => 3,
        "DCEnd" => 4,
        "Load" => 10,
        "ThreadDPC" => 66,
        "ISR" => 67,
        "DPC" => 68,
        "TimerDPC" => 69,
        _ => return None,
    })
}

/// Calls `f(name, value)` for each `<Data Name="...">value</Data>` in `s`.
fn for_each_data<'a>(s: &'a str, mut f: impl FnMut(&'a str, &'a str)) {
    const OPEN: &str = "<Data Name=\"";
    let mut rest = s;
    while let Some(i) = rest.find(OPEN) {
        rest = &rest[i + OPEN.len()..];
        let Some(q) = rest.find('"') else { return };
        let name = &rest[..q];
        rest = &rest[q + 1..];
        let after = rest.trim_start();
        if after.starts_with("/>") {
            f(name, "");
            continue;
        }
        let Some(body) = after.strip_prefix('>') else {
            continue;
        };
        let Some(e) = body.find("</Data>") else {
            return;
        };
        f(name, &body[..e]);
        rest = &body[e + "</Data>".len()..];
    }
}

/// Decodes the five predefined XML entities; anything else is kept as written.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

pub(crate) fn parse_block(block: &str) -> Parsed {
    let class = class_of(block);
    if class == Class::Unknown {
        return Parsed::Other;
    }
    let system = between(block, "<System>", "</System>").unwrap_or(block);
    let rendering = between(block, "<RenderingInfo", "</RenderingInfo>");
    let Some(opcode) = opcode_of(system, rendering) else {
        return Parsed::Malformed;
    };
    let data = between(block, "<EventData>", "</EventData>").unwrap_or("");
    let wall = attr(system, "SystemTime=\"").and_then(parse_wall);

    match (class, opcode) {
        (Class::EventTrace, 0) => {
            let (mut start, mut end, mut lost, mut buffers) = (None, None, None, None);
            for_each_data(data, |name, value| match name {
                "StartTime" => start = parse_u64(value),
                "EndTime" => end = parse_u64(value),
                "EventsLost" => lost = parse_u64(value),
                "BuffersLost" => buffers = parse_u64(value),
                _ => {}
            });
            Parsed::Header {
                wall,
                start_utc: start,
                end_utc: end,
                events_lost: lost,
                buffers_lost: buffers,
            }
        }
        (Class::PerfInfo, 66..=69) => {
            let kind = match opcode {
                66 => ExecKind::ThreadedDpc,
                67 => ExecKind::Isr,
                68 => ExecKind::Dpc,
                _ => ExecKind::TimerDpc,
            };
            let (mut initial, mut routine) = (None, None);
            for_each_data(data, |name, value| match name {
                "InitialTime" => initial = parse_u64(value),
                "Routine" => routine = parse_u64(value),
                _ => {}
            });
            let cpu = attr(system, "ProcessorID=\"")
                .and_then(parse_u64)
                .and_then(|c| u32::try_from(c).ok());
            match (wall, cpu, initial, routine) {
                (Some(end_wall), Some(cpu), Some(initial), Some(routine)) => Parsed::Exec {
                    kind,
                    cpu,
                    end_wall,
                    initial,
                    routine,
                },
                _ => Parsed::Malformed,
            }
        }
        (Class::Image, 2 | 3 | 4 | 10) => {
            let (mut base, mut size, mut pid, mut name) = (None, None, None, None);
            for_each_data(data, |n, value| match n {
                "ImageBase" => base = parse_u64(value),
                "ImageSize" => size = parse_u64(value),
                "ProcessId" => pid = parse_u64(value),
                "FileName" => name = Some(value),
                _ => {}
            });
            match (base, size, name) {
                (Some(base), Some(size), Some(name)) if size > 0 => Parsed::Image {
                    base,
                    size,
                    pid,
                    file_name: unescape(name),
                },
                _ => Parsed::Malformed,
            }
        }
        _ => Parsed::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DPC: &str = r#"<Event xmlns="http://schemas.microsoft.com/win/2004/08/events/event">
	<System>
		<Provider Guid="{9e814aad-3204-11d2-9a82-006008a86939}" />
		<Opcode>68</Opcode>
		<TimeCreated SystemTime="2026-11-16T10:00:00.000150000+00:59" />
		<Execution ProcessID="0" ThreadID="0" ProcessorID="3" KernelTime="0" UserTime="0" />
	</System>
	<EventData>
		<Data Name="InitialTime">134076852000001000</Data>
		<Data Name="Routine">0xFFFFF80612345678</Data>
	</EventData>
	<RenderingInfo Culture="en-US">
		<Opcode>DPC</Opcode>
		<Provider>MSNT_SystemTrace</Provider>
		<EventName xmlns="http://schemas.microsoft.com/win/2004/08/events/trace">PerfInfo</EventName>
	</RenderingInfo>
	<ExtendedTracingInfo xmlns="http://schemas.microsoft.com/win/2004/08/events/trace">
		<EventGuid>{ce1dbfb4-137e-4da6-87b0-3f59aa102cbc}</EventGuid>
	</ExtendedTracingInfo>
</Event>"#;

    #[test]
    fn reads_a_dpc() {
        match parse_block(DPC) {
            Parsed::Exec {
                kind, cpu, routine, ..
            } => {
                assert_eq!(kind, ExecKind::Dpc);
                assert_eq!(cpu, 3);
                assert_eq!(routine, 0xFFFF_F806_1234_5678);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn falls_back_to_rendered_names_without_guid_or_numeric_opcode() {
        let s = DPC.replace("<Opcode>68</Opcode>", "").replace(
            "<EventGuid>{ce1dbfb4-137e-4da6-87b0-3f59aa102cbc}</EventGuid>",
            "",
        );
        assert!(matches!(parse_block(&s), Parsed::Exec { .. }));
    }

    #[test]
    fn missing_fields_are_malformed_and_unknown_events_are_other() {
        assert_eq!(
            parse_block(&DPC.replace("Routine", "Rout")),
            Parsed::Malformed
        );
        assert_eq!(
            parse_block(&DPC.replace("ce1dbfb4", "00000000")),
            Parsed::Other
        );
        assert_eq!(
            parse_block(&DPC.replace("<Opcode>68<", "<Opcode>46<")),
            Parsed::Other
        );
        assert_eq!(
            parse_block(&DPC.replace("ProcessorID=\"3\"", "ProcessorID=\"99999999999\"")),
            Parsed::Malformed
        );
    }

    #[test]
    fn unescapes_file_names() {
        assert_eq!(unescape("a&amp;b&lt;c"), "a&b<c");
    }
}
