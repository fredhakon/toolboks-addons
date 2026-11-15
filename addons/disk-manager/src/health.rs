//! Drive health from the NVMe health log (page 0x02) and ATA SMART data.
//!
//! The parser and the verdict are pure functions over the raw 512-byte pages so they can be
//! tested on recorded data. The Windows read only returns those bytes or a typed error, and the
//! UI reads it off the frame thread through [`Cache`].
use crate::theme::{self, card, weak};
use egui::RichText;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use toolboks_shared::ui::{AMBER, GREEN, RED};

/// Size of both the NVMe health log and the ATA SMART READ DATA page.
pub const PAGE: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Nvme,
    Ata,
}

/// Values a drive reported. `None` means the page did not contain a usable value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveHealth {
    pub protocol: Protocol,
    /// NVMe critical warning bit field.
    pub critical_warning: Option<u8>,
    /// Rated endurance used, in percent. NVMe may report more than 100.
    pub wear_used: Option<u8>,
    pub spare: Option<u8>,
    pub spare_threshold: Option<u8>,
    pub temperature_c: Option<i16>,
    pub media_errors: Option<u64>,
    pub unsafe_shutdowns: Option<u64>,
    pub power_on_hours: Option<u64>,
    pub reallocated: Option<u64>,
    pub pending: Option<u64>,
    pub uncorrectable: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    Short(usize),
    Empty,
}

fn le(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .rev()
        .fold(0u64, |v, b| v.saturating_mul(256).saturating_add(*b as u64))
}

/// Parses the NVMe SMART / health information log (log page 0x02).
pub fn parse_nvme(page: &[u8]) -> Result<DriveHealth, ParseError> {
    if page.len() < PAGE {
        return Err(ParseError::Short(page.len()));
    }
    if page[..PAGE].iter().all(|b| *b == 0) {
        return Err(ParseError::Empty);
    }
    // 128-bit counters: anything beyond u64 is out of reach for real drives, so saturate.
    let counter = |at: usize| Some(le(&page[at..at + 16]));
    let kelvin = u16::from_le_bytes([page[1], page[2]]);
    let percent = |v: u8| (v <= 100).then_some(v);
    Ok(DriveHealth {
        protocol: Protocol::Nvme,
        critical_warning: Some(page[0]),
        wear_used: Some(page[5]),
        spare: percent(page[3]),
        spare_threshold: percent(page[4]),
        temperature_c: (1..=473).contains(&kelvin).then(|| kelvin as i16 - 273),
        media_errors: counter(160),
        unsafe_shutdowns: counter(144),
        power_on_hours: counter(128),
        reallocated: None,
        pending: None,
        uncorrectable: None,
    })
}

/// Parses the ATA SMART READ DATA page: 30 attribute entries of 12 bytes from offset 2
/// (id, 2 flag bytes, normalized value, worst, 6 raw bytes, reserved).
pub fn parse_ata(page: &[u8]) -> Result<DriveHealth, ParseError> {
    if page.len() < PAGE {
        return Err(ParseError::Short(page.len()));
    }
    // The checksum byte is not enforced: many drives leave it wrong, and every value below
    // is range-checked instead.
    let mut attrs = BTreeMap::new();
    for entry in page[2..2 + 30 * 12].as_chunks::<12>().0 {
        if entry[0] != 0 {
            attrs
                .entry(entry[0])
                .or_insert((entry[3], le(&entry[5..11])));
        }
    }
    if attrs.is_empty() {
        return Err(ParseError::Empty);
    }
    let raw32 = |id: u8| attrs.get(&id).map(|(_, raw)| raw & 0xFFFF_FFFF);
    let life_left = [231u8, 233, 177]
        .iter()
        .find_map(|id| attrs.get(id).map(|(value, _)| *value))
        .filter(|v| (1..=100).contains(v));
    let temperature = [194u8, 190]
        .iter()
        .find_map(|id| attrs.get(id).map(|(_, raw)| (raw & 0xFF) as i16))
        .filter(|c| (1..=125).contains(c));
    Ok(DriveHealth {
        protocol: Protocol::Ata,
        critical_warning: None,
        wear_used: life_left.map(|v| 100 - v),
        spare: None,
        spare_threshold: None,
        temperature_c: temperature,
        media_errors: None,
        // Only 174 (unexpected power loss); 192 is often a power-off retract count instead.
        unsafe_shutdowns: raw32(174),
        power_on_hours: raw32(9),
        reallocated: raw32(5),
        pending: raw32(197),
        uncorrectable: raw32(198),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Healthy,
    Watch,
    ReplaceSoon,
}
impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Healthy => "Healthy",
            Verdict::Watch => "Watch",
            Verdict::ReplaceSoon => "Replace soon",
        }
    }
}

/// Conservative verdict over the reported values only; a missing value never raises it.
///
/// Replace soon: an NVMe critical warning for spare, reliability, read-only or backup
/// (bits 0, 2, 3, 4); spare below the drive's own threshold; NVMe wear at 90% or more; or 100
/// or more reallocated, pending and uncorrectable sectors together.
/// Watch: the NVMe temperature warning (bit 1); wear at 70% or more; any media error; any
/// reallocated, pending or uncorrectable sector; or a temperature of 70 °C or more.
/// SATA wear comes from vendor-defined attributes (231, 233, 177), so it is an estimate and
/// never raises the verdict beyond Watch on its own; Replace soon on SATA needs sector evidence.
pub fn verdict(h: &DriveHealth) -> (Verdict, String) {
    let mut found: Vec<(Verdict, String)> = vec![];
    let mut add = |v, text: String| found.push((v, text));
    if let Some(w) = h.critical_warning.filter(|w| *w != 0) {
        if w & 0b1_1101 != 0 {
            add(
                Verdict::ReplaceSoon,
                "The drive reports a critical warning about its reliability or spare space.".into(),
            );
        }
        if w & 0b10 != 0 {
            add(
                Verdict::Watch,
                "The drive reports that it is too hot or too cold.".into(),
            );
        }
    }
    if let (Some(spare), Some(threshold)) = (h.spare, h.spare_threshold) {
        if threshold > 0 && spare < threshold {
            add(
                Verdict::ReplaceSoon,
                format!("Spare space is {spare}%, below the drive's own limit of {threshold}%."),
            );
        }
    }
    match h.wear_used {
        Some(w) if w >= 90 && h.protocol == Protocol::Nvme => add(
            Verdict::ReplaceSoon,
            format!("{w}% of the rated write endurance is used."),
        ),
        Some(w) if w >= 70 => add(
            Verdict::Watch,
            format!("{w}% of the rated write endurance is used."),
        ),
        _ => {}
    }
    if let Some(n) = h.media_errors.filter(|n| *n > 0) {
        add(
            Verdict::Watch,
            format!("{n} media or data integrity errors recorded."),
        );
    }
    let sectors: u64 = [h.reallocated, h.pending, h.uncorrectable]
        .iter()
        .flatten()
        .sum();
    if sectors >= 100 {
        add(
            Verdict::ReplaceSoon,
            format!("{sectors} reallocated, pending or unreadable sectors."),
        );
    } else if sectors > 0 {
        add(
            Verdict::Watch,
            format!("{sectors} reallocated, pending or unreadable sectors."),
        );
    }
    if let Some(c) = h.temperature_c.filter(|c| *c >= 70) {
        add(Verdict::Watch, format!("The drive is at {c} °C."));
    }
    let worst = found
        .iter()
        .map(|(v, _)| *v)
        .max()
        .unwrap_or(Verdict::Healthy);
    let reason = if found.is_empty() {
        "No warning signs in the values this drive reports.".to_string()
    } else {
        found
            .into_iter()
            .filter(|(v, _)| *v == worst)
            .map(|(_, t)| t)
            .collect::<Vec<_>>()
            .join(" ")
    };
    (worst, reason)
}

/// Rows for the Details view; `None` is shown as not reported.
pub fn rows(h: &DriveHealth) -> Vec<(&'static str, Option<String>)> {
    let n = |v: Option<u64>| v.map(|v| v.to_string());
    let wear = h
        .wear_used
        .map(|w| format!("{w}% used ({}% left)", 100u8.saturating_sub(w)));
    let temperature = h.temperature_c.map(|c| format!("{c} °C"));
    match h.protocol {
        Protocol::Nvme => vec![
            ("Wear", wear),
            (
                "Available spare",
                h.spare.map(|s| match h.spare_threshold {
                    Some(t) => format!("{s}% (drive limit {t}%)"),
                    None => format!("{s}%"),
                }),
            ),
            ("Temperature", temperature),
            ("Media errors", n(h.media_errors)),
            ("Unsafe shutdowns", n(h.unsafe_shutdowns)),
            ("Power-on hours", n(h.power_on_hours)),
            (
                "Critical warning",
                h.critical_warning.map(|w| {
                    if w == 0 {
                        "None".into()
                    } else {
                        format!("0x{w:02X}")
                    }
                }),
            ),
        ],
        Protocol::Ata => vec![
            ("Wear (estimated)", wear),
            ("Temperature", temperature),
            ("Reallocated sectors", n(h.reallocated)),
            ("Pending sectors", n(h.pending)),
            ("Unreadable sectors", n(h.uncorrectable)),
            ("Unsafe shutdowns", n(h.unsafe_shutdowns)),
            ("Power-on hours", n(h.power_on_hours)),
        ],
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    AccessDenied,
    Unsupported,
    NotFound,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub model: String,
    pub health: DriveHealth,
}

/// Reads and parses health for a drive letter such as `C:`.
pub fn report(letter: &str) -> Result<Report, ReadError> {
    let (protocol, model, bytes) = read(letter)?;
    let health = match protocol {
        Protocol::Nvme => parse_nvme(&bytes),
        Protocol::Ata => parse_ata(&bytes),
    }
    .map_err(|_| ReadError::Unsupported)?;
    Ok(Report { model, health })
}

#[cfg(windows)]
fn read(letter: &str) -> Result<(Protocol, String, Vec<u8>), ReadError> {
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE},
            Storage::{
                FileSystem::{
                    BusTypeAta, BusTypeNvme, BusTypeSata, CreateFileW, FILE_FLAGS_AND_ATTRIBUTES,
                    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
                },
                IscsiDisc::{
                    ATA_FLAGS_DATA_IN, ATA_FLAGS_DRDY_REQUIRED, ATA_PASS_THROUGH_EX,
                    IOCTL_ATA_PASS_THROUGH,
                },
            },
            System::{Ioctl::*, IO::DeviceIoControl},
        },
    };
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
    fn map(e: windows::core::Error) -> ReadError {
        match e.code().0 as u32 & 0xFFFF {
            5 => ReadError::AccessDenied,
            2 | 3 | 15 | 21 => ReadError::NotFound,
            1 | 50 | 87 | 1117 => ReadError::Unsupported,
            _ => ReadError::Other(e.message()),
        }
    }
    fn open(path: &str, access: u32) -> Result<Handle, ReadError> {
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .map(Handle)
        .map_err(map)
    }
    fn ioctl(h: &Handle, code: u32, input: &[u8], out: &mut [u8]) -> Result<usize, ReadError> {
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                h.0,
                code,
                Some(input.as_ptr().cast()),
                input.len() as u32,
                Some(out.as_mut_ptr().cast()),
                out.len() as u32,
                Some(&mut returned),
                None,
            )
        }
        .map_err(map)?;
        Ok(returned as usize)
    }
    let u32_at = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    let b = letter.as_bytes();
    if b.len() != 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
        return Err(ReadError::NotFound);
    }
    let volume = open(&format!(r"\\.\{letter}"), 0)?;
    let mut number = [0u8; std::mem::size_of::<STORAGE_DEVICE_NUMBER>()];
    if ioctl(&volume, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut number)? < number.len() {
        return Err(ReadError::Unsupported);
    }
    drop(volume);
    let disk = format!(r"\\.\PhysicalDrive{}", u32_at(&number, 4));
    // Access 0 is enough for storage queries and needs no administrator rights.
    let query = open(&disk, 0)?;
    let mut descriptor = [0u8; 1024];
    let mut q = [0u8; 12];
    q[..4].copy_from_slice(&StorageDeviceProperty.0.to_le_bytes());
    q[4..8].copy_from_slice(&PropertyStandardQuery.0.to_le_bytes());
    let got = ioctl(&query, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut descriptor)?;
    if got < 32 {
        return Err(ReadError::Unsupported);
    }
    let product = u32_at(&descriptor, 16) as usize;
    let model = descriptor
        .get(product..got)
        .filter(|_| product > 0)
        .and_then(|b| b.split(|c| *c == 0).next())
        .map(|b| String::from_utf8_lossy(b).trim().to_string())
        .unwrap_or_default();
    let bus = u32_at(&descriptor, 28) as i32;
    if bus == BusTypeNvme.0 {
        // STORAGE_PROPERTY_QUERY header, STORAGE_PROTOCOL_SPECIFIC_DATA (40 bytes), then the log.
        let mut buf = vec![0u8; 8 + 40 + PAGE];
        let header = [
            StorageDeviceProtocolSpecificProperty.0 as u32,
            PropertyStandardQuery.0 as u32,
            ProtocolTypeNvme.0 as u32,
            NVMeDataTypeLogPage.0 as u32,
            0x02,
            0,
            40,
            PAGE as u32,
        ];
        for (i, v) in header.iter().enumerate() {
            buf[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        let input = buf.clone();
        let got = ioctl(&query, IOCTL_STORAGE_QUERY_PROPERTY, &input, &mut buf)?;
        // Output: STORAGE_PROTOCOL_DATA_DESCRIPTOR (version, size, protocol data).
        let start = 8 + u32_at(&buf, 24) as usize;
        let len = u32_at(&buf, 28) as usize;
        let page = buf
            .get(start..(start + len).min(got))
            .ok_or(ReadError::Unsupported)?;
        return Ok((Protocol::Nvme, model, page.to_vec()));
    }
    if bus != BusTypeSata.0 && bus != BusTypeAta.0 {
        return Err(ReadError::Unsupported);
    }
    drop(query);
    // SMART READ DATA through ATA pass-through; Windows requires read/write (administrator).
    let disk = open(&disk, GENERIC_READ.0 | GENERIC_WRITE.0)?;
    #[repr(C)]
    struct Smart {
        header: ATA_PASS_THROUGH_EX,
        data: [u8; PAGE],
    }
    let mut smart = Smart {
        header: ATA_PASS_THROUGH_EX {
            Length: std::mem::size_of::<ATA_PASS_THROUGH_EX>() as u16,
            AtaFlags: (ATA_FLAGS_DATA_IN | ATA_FLAGS_DRDY_REQUIRED) as u16,
            DataTransferLength: PAGE as u32,
            TimeOutValue: 5,
            DataBufferOffset: std::mem::offset_of!(Smart, data),
            CurrentTaskFile: [0xD0, 1, 1, 0x4F, 0xC2, 0xA0, 0xB0, 0],
            ..Default::default()
        },
        data: [0; PAGE],
    };
    let mut returned = 0u32;
    let size = std::mem::size_of::<Smart>() as u32;
    let ptr: *mut Smart = &mut smart;
    unsafe {
        DeviceIoControl(
            disk.0,
            IOCTL_ATA_PASS_THROUGH,
            Some(ptr.cast_const().cast()),
            size,
            Some(ptr.cast()),
            size,
            Some(&mut returned),
            None,
        )
    }
    .map_err(map)?;
    if !ata_complete(returned as usize, smart.header.DataTransferLength as usize) {
        return Err(ReadError::Unsupported);
    }
    // Status register error bit: the drive refused the SMART command.
    if smart.header.CurrentTaskFile[6] & 1 != 0 {
        return Err(ReadError::Unsupported);
    }
    Ok((Protocol::Ata, model, smart.data.to_vec()))
}
/// True when an ATA pass-through returned the header and a full SMART page.
fn ata_complete(returned: usize, transferred: usize) -> bool {
    #[cfg(windows)]
    let header = std::mem::size_of::<windows::Win32::Storage::IscsiDisc::ATA_PASS_THROUGH_EX>();
    #[cfg(not(windows))]
    let header = 0;
    returned >= header + PAGE && transferred >= PAGE
}
#[cfg(not(windows))]
fn read(_: &str) -> Result<(Protocol, String, Vec<u8>), ReadError> {
    Err(ReadError::Unsupported)
}

/// Health per drive letter. `None` while the background read runs.
#[derive(Default)]
pub struct State {
    generation: u64,
    results: BTreeMap<String, Option<Result<Report, ReadError>>>,
}
pub type Cache = Arc<Mutex<State>>;

/// Forgets every result; reads still running from before are discarded when they finish.
pub fn clear(cache: &Cache) {
    let mut s = cache.lock().unwrap();
    s.generation += 1;
    s.results.clear();
}

/// Stores a finished read unless the cache was cleared after it started.
fn store(cache: &Cache, generation: u64, letter: String, result: Result<Report, ReadError>) {
    let mut s = cache.lock().unwrap();
    if s.generation == generation {
        s.results.insert(letter, Some(result));
    }
}

/// Returns the cached result, starting a background read the first time a letter is asked for.
pub fn get(cache: &Cache, letter: &str) -> Option<Result<Report, ReadError>> {
    let mut s = cache.lock().unwrap();
    if let Some(entry) = s.results.get(letter) {
        return entry.clone();
    }
    s.results.insert(letter.into(), None);
    let (cache, letter, generation) = (cache.clone(), letter.to_string(), s.generation);
    std::thread::spawn(move || {
        let result = report(&letter);
        store(&cache, generation, letter, result);
    });
    None
}

fn chip(ui: &mut egui::Ui, verdict: Verdict) {
    let color = match verdict {
        Verdict::Healthy => GREEN,
        Verdict::Watch => AMBER,
        Verdict::ReplaceSoon => RED,
    };
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(verdict.label()).color(color).strong());
        });
}

/// The Drive health card for one drive letter.
pub fn show(ui: &mut egui::Ui, cache: &Cache, letter: &str) {
    let result = get(cache, letter);
    if result.is_none() {
        ui.request_repaint_after(std::time::Duration::from_millis(250));
    }
    card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::section(ui, "Drive health");
        ui.add_space(theme::SPACE_S);
        match result {
            None => weak(ui, "Reading drive health..."),
            Some(Ok(r)) => {
                let (verdict, reason) = verdict(&r.health);
                ui.horizontal_wrapped(|ui| {
                    chip(ui, verdict);
                    if !r.model.is_empty() {
                        weak(ui, &r.model);
                    }
                });
                ui.label(reason);
                egui::CollapsingHeader::new("Details")
                    .id_salt(("health-details", letter))
                    .show(ui, |ui| {
                        egui::Grid::new(("health-grid", letter))
                            .num_columns(2)
                            .spacing([theme::SPACE_XL, theme::SPACE_S])
                            .show(ui, |ui| {
                                for (label, value) in rows(&r.health) {
                                    weak(ui, label);
                                    match value {
                                        Some(v) => ui.label(v),
                                        None => ui.label(
                                            RichText::new("Not reported by this drive")
                                                .color(theme::MUTED),
                                        ),
                                    };
                                    ui.end_row();
                                }
                            });
                        theme::caption(
                            ui,
                            "Values as reported by the drive when Disk Manager read them. The verdict uses only these values.",
                        );
                    });
            }
            Some(Err(ReadError::AccessDenied)) => weak(
                ui,
                "This drive only gives its health data to administrators. Disk Manager runs without administrator rights, so health is not shown for it.",
            ),
            Some(Err(ReadError::Unsupported)) => weak(
                ui,
                "This drive does not report health through Windows. USB, memory card, RAID and virtual drives often do not.",
            ),
            Some(Err(ReadError::NotFound)) => weak(
                ui,
                "This drive letter is not on a single physical drive Disk Manager can read.",
            ),
            Some(Err(ReadError::Other(e))) => {
                weak(ui, format!("Drive health could not be read: {}", e.trim()))
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NVMe health log fields at their offsets in log page 0x02.
    fn nvme(
        warning: u8,
        kelvin: u16,
        spare: u8,
        used: u8,
        hours: u64,
        unsafe_: u64,
        media: u64,
    ) -> Vec<u8> {
        let mut p = vec![0u8; PAGE];
        p[0] = warning;
        p[1..3].copy_from_slice(&kelvin.to_le_bytes());
        p[3] = spare;
        p[4] = 10;
        p[5] = used;
        p[32..40].copy_from_slice(&41_237_811u64.to_le_bytes()); // data units read
        p[112..120].copy_from_slice(&1_204u64.to_le_bytes()); // power cycles
        p[128..136].copy_from_slice(&hours.to_le_bytes());
        p[144..152].copy_from_slice(&unsafe_.to_le_bytes());
        p[160..168].copy_from_slice(&media.to_le_bytes());
        p
    }

    /// ATA SMART page from (id, normalized value, raw) entries, with a valid checksum.
    fn ata(attrs: &[(u8, u8, u64)]) -> Vec<u8> {
        let mut p = vec![0u8; PAGE];
        p[0] = 0x10;
        for (i, (id, value, raw)) in attrs.iter().enumerate() {
            let e = &mut p[2 + i * 12..14 + i * 12];
            e[0] = *id;
            e[1] = 0x32;
            e[3] = *value;
            e[4] = *value;
            e[5..11].copy_from_slice(&raw.to_le_bytes()[..6]);
        }
        let sum = p[..511].iter().fold(0u8, |s, b| s.wrapping_add(*b));
        p[511] = sum.wrapping_neg();
        p
    }

    #[test]
    fn healthy_nvme() {
        let h = parse_nvme(&nvme(0, 311, 100, 3, 8_760, 42, 0)).unwrap();
        assert_eq!(h.temperature_c, Some(38));
        assert_eq!(h.spare, Some(100));
        assert_eq!(h.spare_threshold, Some(10));
        assert_eq!(h.wear_used, Some(3));
        assert_eq!(h.power_on_hours, Some(8_760));
        assert_eq!(h.unsafe_shutdowns, Some(42));
        assert_eq!(h.media_errors, Some(0));
        assert_eq!(h.reallocated, None);
        let (v, reason) = verdict(&h);
        assert_eq!(v, Verdict::Healthy);
        assert!(reason.contains("No warning signs"));
        assert!(rows(&h).iter().all(|(_, v)| v.is_some()));
    }

    #[test]
    fn worn_nvme_with_media_errors() {
        let h = parse_nvme(&nvme(0, 318, 100, 74, 31_002, 310, 6)).unwrap();
        assert_eq!(verdict(&h).0, Verdict::Watch);
        let h = parse_nvme(&nvme(0, 318, 100, 93, 31_002, 310, 6)).unwrap();
        let (v, reason) = verdict(&h);
        assert_eq!(v, Verdict::ReplaceSoon);
        assert!(reason.contains("93%"), "{reason}");
        // Spare under the drive's own threshold, and the spare critical-warning bit.
        let h = parse_nvme(&nvme(0b1, 318, 8, 40, 31_002, 310, 0)).unwrap();
        assert_eq!(verdict(&h).0, Verdict::ReplaceSoon);
        // Temperature bit alone is only a watch.
        let h = parse_nvme(&nvme(0b10, 352, 100, 5, 10, 0, 0)).unwrap();
        assert_eq!(h.temperature_c, Some(79));
        assert_eq!(verdict(&h).0, Verdict::Watch);
    }

    #[test]
    fn nvme_counters_above_u64_saturate() {
        let mut p = nvme(0, 300, 100, 1, 1, 1, 0);
        p[128..144].fill(0xFF);
        assert_eq!(parse_nvme(&p).unwrap().power_on_hours, Some(u64::MAX));
    }

    #[test]
    fn healthy_sata_ssd() {
        let p = ata(&[
            (5, 100, 0),
            (9, 99, 12_345),
            (174, 100, 17),
            (177, 96, 41),
            (194, 67, 0x0028_0014_0021), // 33 °C current, min/max packed above
            (197, 100, 0),
            (198, 100, 0),
        ]);
        let h = parse_ata(&p).unwrap();
        assert_eq!(h.wear_used, Some(4));
        assert_eq!(h.temperature_c, Some(33));
        assert_eq!(h.power_on_hours, Some(12_345));
        assert_eq!(h.unsafe_shutdowns, Some(17));
        assert_eq!(
            (h.reallocated, h.pending, h.uncorrectable),
            (Some(0), Some(0), Some(0))
        );
        assert_eq!(h.spare, None);
        assert_eq!(verdict(&h).0, Verdict::Healthy);
    }

    #[test]
    fn sata_with_pending_sectors() {
        let h = parse_ata(&ata(&[
            (5, 100, 2),
            (9, 70, 40_000),
            (192, 100, 88),
            (197, 100, 8),
            (198, 100, 0),
        ]))
        .unwrap();
        assert_eq!(h.wear_used, None);
        assert_eq!(h.temperature_c, None);
        assert_eq!(h.unsafe_shutdowns, None, "192 is not unsafe shutdowns");
        let (v, reason) = verdict(&h);
        assert_eq!(v, Verdict::Watch);
        assert!(reason.contains("10 reallocated"), "{reason}");
        let shown = rows(&h);
        assert!(shown.contains(&("Temperature", None)));
        let h = parse_ata(&ata(&[(5, 80, 140), (197, 100, 12)])).unwrap();
        assert_eq!(verdict(&h).0, Verdict::ReplaceSoon);
        // SSD life left attribute 231 at 8% left.
        let h = parse_ata(&ata(&[(231, 8, 0)])).unwrap();
        assert_eq!(h.wear_used, Some(92));
        assert_eq!(
            verdict(&h).0,
            Verdict::Watch,
            "estimated SATA wear alone stays Watch"
        );
        assert!(rows(&h).contains(&("Wear (estimated)", Some("92% used (8% left)".into()))));
    }

    #[test]
    fn empty_and_short_pages_are_errors() {
        assert_eq!(parse_nvme(&[0; PAGE]), Err(ParseError::Empty));
        assert_eq!(parse_ata(&[0; PAGE]), Err(ParseError::Empty));
        assert_eq!(parse_nvme(&[1; 100]), Err(ParseError::Short(100)));
        assert_eq!(parse_ata(&[]), Err(ParseError::Short(0)));
    }

    #[test]
    fn out_of_range_values_are_not_reported() {
        let mut p = nvme(0, 0, 255, 0, 0, 0, 0);
        p[4] = 200;
        let h = parse_nvme(&p).unwrap();
        assert_eq!(
            (h.temperature_c, h.spare, h.spare_threshold),
            (None, None, None)
        );
        let h = parse_ata(&ata(&[(194, 100, 0), (231, 0, 0)])).unwrap();
        assert_eq!((h.temperature_c, h.wear_used), (None, None));
    }

    #[test]
    fn reads_from_before_a_refresh_are_discarded() {
        let cache = Cache::default();
        cache.lock().unwrap().results.insert("C:".into(), None);
        let old = cache.lock().unwrap().generation;
        clear(&cache);
        store(&cache, old, "C:".into(), Err(ReadError::NotFound));
        assert!(cache.lock().unwrap().results.is_empty());
        let now = cache.lock().unwrap().generation;
        store(&cache, now, "C:".into(), Err(ReadError::Unsupported));
        assert_eq!(
            cache.lock().unwrap().results.get("C:"),
            Some(&Some(Err(ReadError::Unsupported)))
        );
    }

    #[test]
    fn ata_pass_through_needs_a_full_page() {
        let header = if cfg!(windows) { 48 } else { 0 };
        assert!(ata_complete(header + PAGE, PAGE));
        assert!(!ata_complete(header + PAGE - 1, PAGE));
        assert!(!ata_complete(header + PAGE, PAGE - 1));
        assert!(!ata_complete(0, 0));
    }

    /// Prints what this PC reports for C: without administrator rights.
    #[test]
    #[ignore = "reads real hardware"]
    fn reads_this_pc() {
        match report("C:") {
            Ok(r) => println!("{} {:?} {:?}", r.model, r.health, verdict(&r.health)),
            Err(e) => println!("error {e:?}"),
        }
    }
}
