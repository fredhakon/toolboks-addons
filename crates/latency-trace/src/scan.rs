//! Streaming input: splits the byte stream into `<Event ...>...</Event>` blocks without ever
//! holding more than one bounded block (plus one read chunk) in memory. Text that is not inside
//! an event block (the `<Events>` wrapper, garbage) is skipped.

use std::io::{self, Read};

const CHUNK: usize = 64 * 1024;
const END: &[u8] = b"</Event>";

/// What the scanner noticed about the raw stream.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ScanStats {
    pub bytes_read: u64,
    pub oversized: u64,
    pub broken: u64,
    pub truncated_tail: bool,
    pub utf16: bool,
}

/// Finds `needle` in `hay` at or after `from`.
pub(crate) fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let first = *needle.first()?;
    let mut i = from;
    while i + needle.len() <= hay.len() {
        let p = hay[i..=hay.len() - needle.len()]
            .iter()
            .position(|&b| b == first)?;
        i += p;
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Finds the start of an `<Event` element (not `<Events>`, `<EventData>`, `<EventID>`...).
fn find_event_start(hay: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    loop {
        let p = find(hay, b"<Event", i)?;
        match hay.get(p + 6) {
            Some(b'>' | b' ' | b'\t' | b'\r' | b'\n') => return Some(p),
            // Not enough bytes yet to tell: report it so the caller keeps it for the next read.
            None => return Some(p),
            Some(_) => i = p + 1,
        }
    }
}

/// Calls `on_block` with every complete event block in `reader`, in order.
pub(crate) fn for_each_block<R: Read>(
    reader: R,
    max_block: usize,
    mut on_block: impl FnMut(&[u8]),
) -> io::Result<ScanStats> {
    let mut stats = ScanStats::default();
    let mut reader = Decoder::new(reader)?;
    let max_block = max_block.max(256);

    #[derive(PartialEq)]
    enum State {
        Outside,
        Inside,
        Skipping,
    }
    let mut state = State::Outside;
    let mut buf: Vec<u8> = Vec::with_capacity(CHUNK * 2);
    let mut chunk = vec![0u8; CHUNK];
    // Where to resume searching for `</Event>` and the next `<Event` inside `buf`, so an
    // event that arrives in many small reads is not rescanned from its start each time.
    let mut end_from = 0usize;
    let mut eof = false;

    loop {
        // Consume as much of `buf` as possible.
        loop {
            match state {
                State::Outside => match find_event_start(&buf, 0) {
                    Some(p) if p + 6 < buf.len() => {
                        buf.drain(..p);
                        state = State::Inside;
                        end_from = 1;
                    }
                    Some(p) => {
                        // Possible start at the very end: keep it and read more.
                        buf.drain(..p);
                        break;
                    }
                    None => {
                        let keep = buf.len().min(6);
                        buf.drain(..buf.len() - keep);
                        break;
                    }
                },
                State::Inside => {
                    let end = find(&buf, END, end_from.max(1));
                    let next =
                        find_event_start(&buf, end_from.max(1)).filter(|&n| n + 6 < buf.len());
                    // A new event starting before this one ends means this one was cut short.
                    let cut_short = match (end, next) {
                        (Some(e), Some(n)) => n < e,
                        (None, Some(_)) => true,
                        _ => false,
                    };
                    if let (true, Some(n)) = (cut_short, next) {
                        stats.broken += 1;
                        buf.drain(..n);
                        end_from = 1;
                    } else if let Some(e) = end {
                        let stop = e + END.len();
                        if stop <= max_block {
                            on_block(&buf[..stop]);
                        } else {
                            stats.oversized += 1;
                        }
                        buf.drain(..stop);
                        state = State::Outside;
                    } else {
                        if buf.len() > max_block {
                            stats.oversized += 1;
                            let keep = buf.len().min(END.len() - 1);
                            buf.drain(..buf.len() - keep);
                            state = State::Skipping;
                        } else {
                            end_from = buf.len().saturating_sub(END.len() - 1).max(1);
                        }
                        break;
                    }
                }
                State::Skipping => match find(&buf, END, 0) {
                    Some(e) => {
                        buf.drain(..e + END.len());
                        state = State::Outside;
                    }
                    None => {
                        let keep = buf.len().min(END.len() - 1);
                        buf.drain(..buf.len() - keep);
                        break;
                    }
                },
            }
        }
        if eof {
            break;
        }
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            eof = true;
            // One more pass over what is left, then stop.
            continue;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    if state == State::Inside && !buf.is_empty() {
        stats.truncated_tail = true;
    }
    stats.bytes_read = reader.bytes_read;
    stats.utf16 = reader.utf16.is_some();
    Ok(stats)
}

/// Passes UTF-8 through unchanged and converts UTF-16 (with a byte-order mark, or plainly
/// starting with `<` followed by a zero byte) to UTF-8 on the fly.
struct Decoder<R> {
    inner: R,
    bytes_read: u64,
    /// `Some(little_endian)` when the input is UTF-16.
    utf16: Option<bool>,
    head: Vec<u8>,
    head_pos: usize,
    raw: Vec<u8>,
    carry: Option<u8>,
    high: Option<u16>,
    out: Vec<u8>,
    out_pos: usize,
    inner_eof: bool,
}

impl<R: Read> Decoder<R> {
    fn new(mut inner: R) -> io::Result<Self> {
        let mut head = [0u8; 4];
        let mut got = 0;
        while got < head.len() {
            let n = inner.read(&mut head[got..])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        let h = &head[..got];
        let (utf16, skip) = if h.starts_with(&[0xFF, 0xFE]) {
            (Some(true), 2)
        } else if h.starts_with(&[0xFE, 0xFF]) {
            (Some(false), 2)
        } else if h.starts_with(&[0xEF, 0xBB, 0xBF]) {
            (None, 3)
        } else if h.starts_with(b"<\0") {
            (Some(true), 0)
        } else if h.starts_with(b"\0<") {
            (Some(false), 0)
        } else {
            (None, 0)
        };
        Ok(Self {
            inner,
            bytes_read: got as u64,
            utf16,
            head: h.to_vec(),
            head_pos: skip,
            raw: vec![0u8; CHUNK],
            carry: None,
            high: None,
            out: Vec::with_capacity(CHUNK * 3 / 2 + 8),
            out_pos: 0,
            inner_eof: false,
        })
    }

    fn read_raw(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.head_pos < self.head.len() {
            let n = (self.head.len() - self.head_pos).min(buf.len());
            buf[..n].copy_from_slice(&self.head[self.head_pos..self.head_pos + n]);
            self.head_pos += n;
            return Ok(n);
        }
        let n = self.inner.read(buf)?;
        self.bytes_read += n as u64;
        Ok(n)
    }

    fn push_unit(&mut self, unit: u16) {
        let mut tmp = [0u8; 4];
        if let Some(high) = self.high.take() {
            if (0xDC00..0xE000).contains(&unit) {
                let c = 0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
                let ch = char::from_u32(c).unwrap_or(char::REPLACEMENT_CHARACTER);
                self.out
                    .extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                return;
            }
            self.out.extend_from_slice("\u{FFFD}".as_bytes());
        }
        if (0xD800..0xDC00).contains(&unit) {
            self.high = Some(unit);
            return;
        }
        let ch = char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER);
        self.out
            .extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
    }
}

impl<R: Read> Read for Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(le) = self.utf16 else {
            return self.read_raw(buf);
        };
        while self.out_pos >= self.out.len() {
            if self.inner_eof {
                return Ok(0);
            }
            self.out.clear();
            self.out_pos = 0;
            let mut raw = std::mem::take(&mut self.raw);
            let n = self.read_raw(&mut raw)?;
            if n == 0 {
                self.inner_eof = true;
                if self.carry.take().is_some() || self.high.take().is_some() {
                    self.out.extend_from_slice("\u{FFFD}".as_bytes());
                }
            } else {
                let mut bytes = &raw[..n];
                if let Some(c) = self.carry.take() {
                    let pair = [c, bytes[0]];
                    let unit = if le {
                        u16::from_le_bytes(pair)
                    } else {
                        u16::from_be_bytes(pair)
                    };
                    self.push_unit(unit);
                    bytes = &bytes[1..];
                }
                let (pairs, rest) = bytes.as_chunks::<2>();
                for &pair in pairs {
                    let unit = if le {
                        u16::from_le_bytes(pair)
                    } else {
                        u16::from_be_bytes(pair)
                    };
                    self.push_unit(unit);
                }
                self.carry = rest.first().copied();
            }
            self.raw = raw;
        }
        let n = (self.out.len() - self.out_pos).min(buf.len());
        buf[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
        self.out_pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(input: &[u8], max: usize) -> (Vec<String>, ScanStats) {
        let mut out = Vec::new();
        let stats = for_each_block(input, max, |b| {
            out.push(String::from_utf8_lossy(b).into_owned())
        })
        .unwrap();
        (out, stats)
    }

    #[test]
    fn splits_events_and_skips_wrapper() {
        let (b, s) = blocks(
            b"<Events>\r\n<Event a=\"1\"><EventData/></Event>\r\n<Event>x</Event></Events>",
            1024,
        );
        assert_eq!(
            b,
            vec!["<Event a=\"1\"><EventData/></Event>", "<Event>x</Event>"]
        );
        assert!(!s.truncated_tail);
    }

    #[test]
    fn cut_short_event_is_dropped_and_the_next_one_kept() {
        let (b, s) = blocks(b"<Event>half <Event>whole</Event>", 1024);
        assert_eq!(b, vec!["<Event>whole</Event>"]);
        assert_eq!(s.broken, 1);
    }

    #[test]
    fn oversized_event_is_skipped() {
        let mut input = b"<Event>".to_vec();
        input.extend(std::iter::repeat_n(b'a', 5000));
        input.extend_from_slice(b"</Event><Event>ok</Event>");
        let (b, s) = blocks(&input, 1024);
        assert_eq!(b, vec!["<Event>ok</Event>"]);
        assert_eq!(s.oversized, 1);
    }

    #[test]
    fn truncated_tail_is_reported() {
        let (b, s) = blocks(b"<Event>ok</Event><Event>never ends", 1024);
        assert_eq!(b.len(), 1);
        assert!(s.truncated_tail);
    }

    /// Hands out one byte per read.
    struct Trickle<'a>(&'a [u8]);
    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match (self.0.split_first(), buf.first_mut()) {
                (Some((b, rest)), Some(slot)) => {
                    *slot = *b;
                    self.0 = rest;
                    Ok(1)
                }
                _ => Ok(0),
            }
        }
    }

    #[test]
    fn one_byte_reads_give_the_same_blocks() {
        let input = b"<Events><Event>a<Event>b</Event>junk<Event x=\"1\">c</Event></Events>";
        let mut out = Vec::new();
        let stats = for_each_block(Trickle(input), 1024, |b| {
            out.push(String::from_utf8_lossy(b).into_owned())
        })
        .unwrap();
        assert_eq!(out, vec!["<Event>b</Event>", "<Event x=\"1\">c</Event>"]);
        assert_eq!(stats.broken, 1);
        assert_eq!(stats.bytes_read, input.len() as u64);
    }

    #[test]
    fn utf16_input_is_decoded() {
        let text = "<Events><Event>ok \u{1F600}</Event></Events>";
        let mut le = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        let (b, s) = blocks(&le, 1024);
        assert_eq!(b, vec!["<Event>ok \u{1F600}</Event>"]);
        assert!(s.utf16);
        let mut be = Vec::new();
        for u in text.encode_utf16() {
            be.extend_from_slice(&u.to_be_bytes());
        }
        let (b, _) = blocks(&be, 1024);
        assert_eq!(b, vec!["<Event>ok \u{1F600}</Event>"]);
    }
}
