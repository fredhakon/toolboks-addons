//! Reads the memory devices (SMBIOS type 17) out of the raw SMBIOS table. This is the same
//! table Windows turns into `Win32_PhysicalMemory`: `Speed` is the rated speed and
//! `ConfiguredClockSpeed` is the speed the BIOS runs the module at. Pure byte parsing, so it
//! is tested with fixtures and compiles on every platform.

use crate::ram::MemoryModule;

const TYPE_MEMORY_DEVICE: u8 = 17;
const TYPE_END_OF_TABLE: u8 = 127;

// Offsets inside a type 17 structure (SMBIOS spec 3.x, "Memory Device").
const OFF_SIZE: usize = 0x0C;
const OFF_LOCATOR: usize = 0x10;
const OFF_SPEED: usize = 0x15;
const OFF_CONFIGURED_SPEED: usize = 0x20;
const OFF_EXTENDED_SPEED: usize = 0x54;
const OFF_EXTENDED_CONFIGURED_SPEED: usize = 0x58;

fn u16_at(s: &[u8], off: usize) -> Option<u16> {
    s.get(off..off + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(s: &[u8], off: usize) -> Option<u32> {
    s.get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// A speed field: `0` means unknown, `0xFFFF` means "look in the extended 32-bit field".
fn speed(s: &[u8], off: usize, extended_off: usize) -> Option<u32> {
    match u16_at(s, off)? {
        0 => None,
        0xFFFF => u32_at(s, extended_off).filter(|v| *v > 0),
        v => Some(u32::from(v)),
    }
}

/// `data` is the SMBIOS structure table (without the 8-byte `RawSMBIOSData` header that
/// `GetSystemFirmwareTable` puts in front). Empty slots are skipped.
pub fn parse_memory_modules(data: &[u8]) -> Vec<MemoryModule> {
    let mut modules = Vec::new();
    let mut i = 0usize;
    while i + 4 <= data.len() {
        let kind = data[i];
        let len = data[i + 1] as usize;
        if len < 4 || i + len > data.len() {
            break; // a damaged table: keep what was read
        }
        let formatted = &data[i..i + len];
        // The string set follows the formatted area and ends with an empty string (two NULs).
        let strings_start = i + len;
        let mut end = strings_start;
        while end + 1 < data.len() && !(data[end] == 0 && data[end + 1] == 0) {
            end += 1;
        }
        if end + 1 >= data.len() {
            // No terminator: still use this structure, then stop.
            if kind == TYPE_MEMORY_DEVICE {
                push_module(&mut modules, formatted, &data[strings_start..]);
            }
            break;
        }
        let strings = &data[strings_start..end];
        if kind == TYPE_MEMORY_DEVICE {
            push_module(&mut modules, formatted, strings);
        }
        if kind == TYPE_END_OF_TABLE {
            break;
        }
        i = end + 2;
    }
    modules
}

fn push_module(out: &mut Vec<MemoryModule>, s: &[u8], strings: &[u8]) {
    // Size 0 is an empty slot. 0xFFFF means unknown size, so it is kept.
    if u16_at(s, OFF_SIZE) == Some(0) {
        return;
    }
    let slot = s
        .get(OFF_LOCATOR)
        .copied()
        .filter(|n| *n > 0)
        .and_then(|n| nth_string(strings, n as usize))
        .unwrap_or_else(|| format!("slot {}", out.len() + 1));
    out.push(MemoryModule {
        slot,
        rated_mts: speed(s, OFF_SPEED, OFF_EXTENDED_SPEED),
        configured_mts: speed(s, OFF_CONFIGURED_SPEED, OFF_EXTENDED_CONFIGURED_SPEED),
    });
}

/// The `n`th string (1-based) of an SMBIOS string set.
fn nth_string(strings: &[u8], n: usize) -> Option<String> {
    strings
        .split(|b| *b == 0)
        .nth(n.checked_sub(1)?)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A type 17 structure of `len` formatted bytes with the given fields and strings.
    fn memory_device(
        len: usize,
        size: u16,
        locator_idx: u8,
        speed_v: u16,
        configured: u16,
        strings: &[&str],
    ) -> Vec<u8> {
        let mut s = vec![0u8; len];
        s[0] = TYPE_MEMORY_DEVICE;
        s[1] = len as u8;
        s[OFF_SIZE..OFF_SIZE + 2].copy_from_slice(&size.to_le_bytes());
        s[OFF_LOCATOR] = locator_idx;
        s[OFF_SPEED..OFF_SPEED + 2].copy_from_slice(&speed_v.to_le_bytes());
        if len >= OFF_CONFIGURED_SPEED + 2 {
            s[OFF_CONFIGURED_SPEED..OFF_CONFIGURED_SPEED + 2]
                .copy_from_slice(&configured.to_le_bytes());
        }
        append_strings(&mut s, strings);
        s
    }

    fn append_strings(s: &mut Vec<u8>, strings: &[&str]) {
        if strings.is_empty() {
            s.extend_from_slice(&[0, 0]);
        } else {
            for t in strings {
                s.extend_from_slice(t.as_bytes());
                s.push(0);
            }
            s.push(0);
        }
    }

    fn end_marker() -> Vec<u8> {
        vec![TYPE_END_OF_TABLE, 4, 0, 0, 0, 0]
    }

    #[test]
    fn reads_speed_configured_speed_and_slot() {
        let mut table = memory_device(0x5C, 8192, 1, 3600, 2133, &["DIMM1"]);
        table.extend(memory_device(0x5C, 8192, 1, 3600, 3600, &["DIMM2"]));
        table.extend(end_marker());
        let m = parse_memory_modules(&table);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].slot, "DIMM1");
        assert_eq!(m[0].rated_mts, Some(3600));
        assert_eq!(m[0].configured_mts, Some(2133));
        assert_eq!(m[1].configured_mts, Some(3600));
    }

    #[test]
    fn skips_other_structures_and_empty_slots() {
        let mut table = vec![1, 4, 0, 0, b'x', 0, 0]; // type 1 with one string
        table.extend(memory_device(0x5C, 0, 1, 3200, 3200, &["EMPTY"])); // size 0: empty slot
        table.extend(memory_device(0x5C, 16384, 1, 3200, 3200, &["DIMM3"]));
        table.extend(end_marker());
        let m = parse_memory_modules(&table);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].slot, "DIMM3");
    }

    #[test]
    fn zero_speeds_are_none_not_zero() {
        let mut table = memory_device(0x5C, 8192, 1, 0, 0, &["DIMM1"]);
        table.extend(end_marker());
        let m = parse_memory_modules(&table);
        assert_eq!(m[0].rated_mts, None);
        assert_eq!(m[0].configured_mts, None);
    }

    #[test]
    fn old_smbios_without_configured_speed_field() {
        // A 0x15-byte structure ends before the speed field; a 0x1C one has Speed but no
        // Configured Memory Speed (added in SMBIOS 2.7).
        let mut table = memory_device(0x1C, 8192, 1, 1600, 0, &["DIMM1"]);
        table.extend(end_marker());
        let m = parse_memory_modules(&table);
        assert_eq!(m[0].rated_mts, Some(1600));
        assert_eq!(m[0].configured_mts, None);
    }

    #[test]
    fn extended_speed_is_used_when_the_short_field_is_ffff() {
        let mut s = memory_device(0x5C, 8192, 1, 0xFFFF, 0xFFFF, &["DIMM1"]);
        s[OFF_EXTENDED_SPEED..OFF_EXTENDED_SPEED + 4].copy_from_slice(&40_000u32.to_le_bytes());
        s[OFF_EXTENDED_CONFIGURED_SPEED..OFF_EXTENDED_CONFIGURED_SPEED + 4]
            .copy_from_slice(&35_000u32.to_le_bytes());
        let m = parse_memory_modules(&s);
        assert_eq!(m[0].rated_mts, Some(40_000));
        assert_eq!(m[0].configured_mts, Some(35_000));
    }

    #[test]
    fn missing_locator_string_gets_a_numbered_name() {
        let mut table = memory_device(0x5C, 8192, 0, 3200, 3200, &[]);
        table.extend(end_marker());
        assert_eq!(parse_memory_modules(&table)[0].slot, "slot 1");
    }

    #[test]
    fn a_damaged_table_does_not_panic() {
        let mut table = memory_device(0x5C, 8192, 1, 3200, 3200, &["DIMM1"]);
        table.truncate(table.len() - 3);
        let _ = parse_memory_modules(&table);
        let _ = parse_memory_modules(&[17, 200, 0, 0]);
        let _ = parse_memory_modules(&[]);
        let _ = parse_memory_modules(&[17, 2, 0, 0, 0]);
    }
}
