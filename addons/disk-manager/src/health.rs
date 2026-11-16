//! Drive health from the NVMe health log (page 0x02) and ATA SMART data, with Windows' own
//! storage health as the fallback.
//!
//! The parser and the verdict are pure functions over the raw 512-byte pages so they can be
//! tested on recorded data. The Windows read only returns those bytes or a typed error, and the
//! UI reads it off the frame thread through [`Cache`].
//!
//! Drives behind a RAID controller (Intel RST/VMD) report bus type RAID and give no NVMe or SMART
//! data. For those, and for any drive whose direct read is unsupported, the read falls back to
//! Windows' own read-only rating (`MSFT_PhysicalDisk` HealthStatus and OperationalStatus). That
//! rating is shown as exactly that: no wear, temperature or SMART value is ever made up for it.
use crate::theme::{self, card, weak};
use egui::RichText;
use std::{
    collections::{BTreeMap, BTreeSet},
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
    /// Windows' own rating is Unhealthy or lost communication; only the Windows fallback gives it.
    Problem,
}
impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Healthy => "Healthy",
            Verdict::Watch => "Watch",
            Verdict::ReplaceSoon => "Replace soon",
            Verdict::Problem => "Problem",
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

/// How Windows rates a drive: its `HealthStatus` (and `OperationalStatus`) from the Storage
/// namespace, which any user may read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rating {
    Healthy,
    /// Windows "Warning" or "Degraded".
    Warning,
    /// Windows "Unhealthy", an error, or lost communication.
    Unhealthy,
}

/// The raw values Windows returned for one disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WindowsStatus {
    /// `HealthStatus`: 0 Healthy, 1 Warning, 2 Unhealthy, 5 Unknown. `None` when not returned.
    pub health: Option<u16>,
    /// `OperationalStatus` values (2 OK, 3 Degraded, 13 Lost Communication, ...).
    pub operational: Vec<u16>,
}

/// Windows' rating of a drive that gave no direct health data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsRating {
    pub rating: Rating,
    pub status: WindowsStatus,
    /// The drive reports bus type RAID (Intel RST/VMD style). Only changes the wording.
    pub raid: bool,
}

/// What a report holds: values the drive itself reported, or only Windows' own rating.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    Drive(DriveHealth),
    Windows(WindowsRating),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub model: String,
    pub health: Health,
}

/// The verdict and its reason for either kind of report.
pub fn assess(h: &Health) -> (Verdict, String) {
    match h {
        Health::Drive(d) => verdict(d),
        Health::Windows(w) => windows_verdict(w),
    }
}

fn health_name(v: u16) -> Option<&'static str> {
    match v {
        0 => Some("Healthy"),
        1 => Some("Warning"),
        2 => Some("Unhealthy"),
        _ => None,
    }
}

/// Windows' name for an `OperationalStatus` value, for the values that mean something is off.
fn operational_name(v: u16) -> Option<&'static str> {
    match v {
        3 => Some("Degraded"),
        4 => Some("Stressed"),
        5 => Some("Predictive failure"),
        6 => Some("Error"),
        7 => Some("Non-recoverable error"),
        12 => Some("No contact"),
        13 => Some("Lost communication"),
        _ => None,
    }
}

fn operational_rating(v: u16) -> Rating {
    match v {
        6 | 7 | 12 | 13 => Rating::Unhealthy,
        3..=5 => Rating::Warning,
        _ => Rating::Healthy,
    }
}

/// Windows' rating from its raw values: the worse of `HealthStatus` and any `OperationalStatus`
/// that means trouble. `None` when Windows reports no usable health (Unknown or missing).
pub fn rate(status: &WindowsStatus) -> Option<Rating> {
    let base = match status.health {
        Some(0) => Rating::Healthy,
        Some(1) => Rating::Warning,
        Some(2) => Rating::Unhealthy,
        _ => {
            // No usable HealthStatus: an operational status that means trouble still counts, a
            // clean one does not stand in for a health rating.
            return status
                .operational
                .iter()
                .map(|v| operational_rating(*v))
                .filter(|r| *r != Rating::Healthy)
                .max();
        }
    };
    Some(
        status
            .operational
            .iter()
            .map(|v| operational_rating(*v))
            .fold(base, Rating::max),
    )
}

fn windows_verdict(w: &WindowsRating) -> (Verdict, String) {
    let verdict = match w.rating {
        Rating::Healthy => Verdict::Healthy,
        Rating::Warning => Verdict::Watch,
        Rating::Unhealthy => Verdict::Problem,
    };
    let notes: Vec<&str> = w
        .status
        .operational
        .iter()
        .filter_map(|v| operational_name(*v))
        .collect();
    let mut text = match w.status.health.and_then(health_name) {
        Some(name) => format!("Windows rates this drive {name}."),
        None => "Windows reports this drive's status.".to_string(),
    };
    if !notes.is_empty() {
        text += &format!(" Status: {}.", notes.join(", "));
    }
    text += if w.raid {
        " This PC runs its drives in RAID mode, so detailed drive data (wear, temperature, reallocated sectors) is not available."
    } else {
        " This drive gives Windows no detailed drive data (wear, temperature, reallocated sectors), so only Windows' own rating is shown."
    };
    (verdict, text)
}

/// Rows for the Details view of a Windows rating.
pub fn windows_rows(w: &WindowsRating) -> Vec<(&'static str, String)> {
    let health = match w.status.health {
        Some(v) => health_name(v).map_or_else(|| "Unknown".to_string(), str::to_string),
        None => "Not returned".to_string(),
    };
    let operational = match w.status.operational.as_slice() {
        [] => "Not returned".to_string(),
        all => all
            .iter()
            .map(|v| match *v {
                2 => "OK".to_string(),
                v => operational_name(v).map_or_else(|| format!("Code {v}"), str::to_string),
            })
            .collect::<Vec<_>>()
            .join(", "),
    };
    vec![
        ("Windows health status", health),
        ("Windows operational status", operational),
    ]
}

/// What the direct read found for a drive.
enum Probe {
    /// Raw NVMe or SMART bytes.
    Page {
        protocol: Protocol,
        model: String,
        bytes: Vec<u8>,
        disk: u32,
    },
    /// The drive has no direct health path (RAID, USB, ...) or refused it: ask Windows.
    NoDirect {
        disk: u32,
        model: String,
        raid: bool,
    },
}

/// Turns a probe into a report. Windows' own rating is asked for only when the direct read gave
/// nothing usable; `windows` reads it for a disk number and returns why it could not on failure.
fn resolve(
    probe: Result<Probe, ReadError>,
    windows: &dyn Fn(u32) -> Result<WindowsStatus, String>,
) -> Result<Report, ReadError> {
    let (disk, model, raid) = match probe? {
        Probe::Page {
            protocol,
            model,
            bytes,
            disk,
        } => {
            let parsed = match protocol {
                Protocol::Nvme => parse_nvme(&bytes),
                Protocol::Ata => parse_ata(&bytes),
            };
            match parsed {
                Ok(health) => {
                    return Ok(Report {
                        model,
                        health: Health::Drive(health),
                    })
                }
                Err(_) => (disk, model, false),
            }
        }
        Probe::NoDirect { disk, model, raid } => (disk, model, raid),
    };
    let unreadable = |why: &str| {
        if raid {
            ReadError::Other(format!(
                "This PC runs its drives in RAID mode and Windows' own health rating is not available: {why}"
            ))
        } else {
            ReadError::Unsupported
        }
    };
    match windows(disk) {
        Err(why) => Err(unreadable(&why)),
        Ok(status) => match rate(&status) {
            None => Err(unreadable(
                "Windows reports this drive's health as Unknown.",
            )),
            Some(rating) => Ok(Report {
                model,
                health: Health::Windows(WindowsRating {
                    rating,
                    status,
                    raid,
                }),
            }),
        },
    }
}

/// Reads and parses health for a drive letter such as `C:`.
pub fn report(letter: &str) -> Result<Report, ReadError> {
    resolve(read(letter), &windows_status)
}

#[cfg(windows)]
fn read(letter: &str) -> Result<Probe, ReadError> {
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE},
            Storage::{
                FileSystem::{
                    BusTypeAta, BusTypeNvme, BusTypeRAID, BusTypeSata, CreateFileW,
                    FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
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
    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }
    /// The NVMe health log through a handle opened with access 0.
    fn nvme_log(query: &Handle) -> Result<Vec<u8>, ReadError> {
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
        let got = ioctl(query, IOCTL_STORAGE_QUERY_PROPERTY, &input, &mut buf)?;
        // Output: STORAGE_PROTOCOL_DATA_DESCRIPTOR (version, size, protocol data).
        let start = 8 + u32_at(&buf, 24) as usize;
        let len = u32_at(&buf, 28) as usize;
        let page = buf
            .get(start..(start + len).min(got))
            .ok_or(ReadError::Unsupported)?;
        Ok(page.to_vec())
    }
    /// SMART READ DATA through ATA pass-through; Windows requires read/write (administrator).
    fn ata_smart(path: &str) -> Result<Vec<u8>, ReadError> {
        let disk = open(path, GENERIC_READ.0 | GENERIC_WRITE.0)?;
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
                // `data` follows the header in this repr(C) struct, and a u8 array needs no padding.
                DataBufferOffset: std::mem::size_of::<ATA_PASS_THROUGH_EX>(),
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
        Ok(smart.data.to_vec())
    }
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
    let number = u32_at(&number, 4);
    let path = format!(r"\\.\PhysicalDrive{number}");
    // Access 0 is enough for storage queries and needs no administrator rights.
    let query = open(&path, 0)?;
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
    let no_direct = |model: String, raid: bool| {
        Ok(Probe::NoDirect {
            disk: number,
            model,
            raid,
        })
    };
    let (protocol, bytes) = if bus == BusTypeNvme.0 {
        (Protocol::Nvme, nvme_log(&query))
    } else if bus == BusTypeSata.0 || bus == BusTypeAta.0 {
        drop(query);
        (Protocol::Ata, ata_smart(&path))
    } else {
        // RAID (Intel RST/VMD), USB, memory card, virtual: no NVMe or SMART read exists.
        return no_direct(model, bus == BusTypeRAID.0);
    };
    match bytes {
        Ok(bytes) => Ok(Probe::Page {
            protocol,
            model,
            bytes,
            disk: number,
        }),
        // The drive or its driver refuses the direct read: Windows may still rate it.
        Err(ReadError::Unsupported) => no_direct(model, false),
        Err(e) => Err(e),
    }
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
fn read(_: &str) -> Result<Probe, ReadError> {
    Err(ReadError::Unsupported)
}

/// Windows' own health values for physical disk `disk` (its number, as in `\\.\PhysicalDriveN`),
/// read-only through the Storage namespace of WMI. Needs no administrator rights.
#[cfg(windows)]
fn windows_status(disk: u32) -> Result<WindowsStatus, String> {
    use windows::{
        core::{BSTR, PCWSTR},
        Win32::{
            Foundation::{RPC_E_CHANGED_MODE, S_FALSE},
            System::{
                Com::{
                    CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CoUninitialize,
                    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
                    RPC_C_IMP_LEVEL_IMPERSONATE, SAFEARRAY,
                },
                Ole::{SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound},
                Variant::{VariantClear, VARIANT, VT_ARRAY, VT_I2, VT_I4, VT_UI2},
                Wmi::{
                    IWbemLocator, WbemLocator, WBEM_FLAG_CONNECT_USE_MAX_WAIT,
                    WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_S_TIMEDOUT,
                },
            },
        },
    };
    /// Balances a successful `CoInitializeEx` on this thread.
    struct Com(bool);
    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }
    /// Clears a VARIANT filled by `Get`.
    struct Var(VARIANT);
    impl Drop for Var {
        fn drop(&mut self) {
            let _ = unsafe { VariantClear(&mut self.0) };
        }
    }
    let err = |what: &str, e: windows::core::Error| {
        format!(
            "{what} (0x{:08X}) {}",
            e.code().0 as u32,
            e.message().trim()
        )
        .trim_end()
        .to_string()
    };
    const AUTHN_WINNT: u32 = 10;
    // A thread that already has COM in another mode keeps it; only a successful init is undone.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() && hr != RPC_E_CHANGED_MODE {
        return Err(format!("COM start failed (0x{:08X})", hr.0 as u32));
    }
    let _com = Com(hr.is_ok() || hr == S_FALSE);

    let locator: IWbemLocator =
        unsafe { CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| err("Windows storage service", e))?;
    let services = unsafe {
        locator.ConnectServer(
            &BSTR::from(r"ROOT\Microsoft\Windows\Storage"),
            &BSTR::new(),
            &BSTR::new(),
            &BSTR::new(),
            WBEM_FLAG_CONNECT_USE_MAX_WAIT.0,
            &BSTR::new(),
            None,
        )
    }
    .map_err(|e| err("Windows storage service", e))?;
    // WMI providers need the caller impersonated; the setting is per proxy, so every WMI
    // interface pointer gets it.
    macro_rules! blanket {
        ($proxy:expr) => {
            unsafe {
                CoSetProxyBlanket(
                    $proxy,
                    AUTHN_WINNT,
                    0,
                    PCWSTR::null(),
                    RPC_C_AUTHN_LEVEL_CALL,
                    RPC_C_IMP_LEVEL_IMPERSONATE,
                    None,
                    EOAC_NONE,
                )
            }
        };
    }
    blanket!(&services).map_err(|e| err("Windows storage service", e))?;
    // `disk` is a number, so the query text cannot be changed by a drive name.
    let query = format!(
        "SELECT HealthStatus, OperationalStatus FROM MSFT_PhysicalDisk WHERE DeviceId='{disk}'"
    );
    let rows = unsafe {
        services.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from(query.as_str()),
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )
    }
    .map_err(|e| err("Windows storage query", e))?;
    blanket!(&rows).map_err(|e| err("Windows storage query", e))?;
    let mut object = [None];
    let mut returned = 0u32;
    // Five seconds: a stuck WMI service must not hold the read forever.
    let hr = unsafe { rows.Next(5000, &mut object, &mut returned) };
    if hr.0 == WBEM_S_TIMEDOUT.0 {
        return Err("Windows did not answer in time".into());
    }
    hr.ok().map_err(|e| err("Windows storage query", e))?;
    let Some(Some(object)) = (returned == 1).then(|| object[0].take()) else {
        return Err(format!("Windows does not list disk {disk}"));
    };

    let get = |name: PCWSTR| -> Result<Var, String> {
        let mut v = Var(VARIANT::default());
        unsafe { object.Get(name, 0, &mut v.0, None, None) }
            .map_err(|e| err("Windows storage value", e))?;
        Ok(v)
    };
    let health = get(windows::core::w!("HealthStatus"))?;
    let status = get(windows::core::w!("OperationalStatus"))?;
    // SAFETY: the unions are read according to the `vt` tag that Windows set.
    let health = unsafe {
        let v = &health.0.Anonymous.Anonymous;
        match v.vt {
            t if t == VT_I4 => u16::try_from(v.Anonymous.lVal).ok(),
            t if t == VT_I2 => u16::try_from(v.Anonymous.iVal).ok(),
            t if t == VT_UI2 => Some(v.Anonymous.uiVal),
            _ => None,
        }
    };
    let operational = unsafe {
        let v = &status.0.Anonymous.Anonymous;
        let element = v.vt.0 & !VT_ARRAY.0;
        let array: *mut SAFEARRAY = v.Anonymous.parray;
        if v.vt.0 & VT_ARRAY.0 == 0 || array.is_null() {
            vec![]
        } else {
            let low = SafeArrayGetLBound(array, 1).unwrap_or(0);
            let high = SafeArrayGetUBound(array, 1).unwrap_or(-1);
            (low..=high.min(low.saturating_add(63)))
                .filter_map(|i| match element {
                    e if e == VT_I4.0 => {
                        let mut value = 0i32;
                        SafeArrayGetElement(array, &i, (&mut value as *mut i32).cast()).ok()?;
                        u16::try_from(value).ok()
                    }
                    e if e == VT_I2.0 || e == VT_UI2.0 => {
                        let mut value = 0u16;
                        SafeArrayGetElement(array, &i, (&mut value as *mut u16).cast()).ok()?;
                        Some(value)
                    }
                    _ => None,
                })
                .collect()
        }
    };
    Ok(WindowsStatus {
        health,
        operational,
    })
}
#[cfg(not(windows))]
fn windows_status(_: u32) -> Result<WindowsStatus, String> {
    Err("Windows only".into())
}

/// Health per drive letter. `None` while the first background read runs.
#[derive(Default)]
pub struct State {
    generation: u64,
    results: BTreeMap<String, Option<Result<Report, ReadError>>>,
    /// Unix seconds when each stored result was read.
    read_at: BTreeMap<String, u64>,
    /// Letters with a read running that keep their previous result until it finishes.
    rereading: BTreeSet<String>,
}
pub type Cache = Arc<Mutex<State>>;
/// What reads one drive. [`report`] is the real one; tests pass a fake.
pub type ReadFn = fn(&str) -> Result<Report, ReadError>;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Forgets every result; reads still running from before are discarded when they finish.
pub fn clear(cache: &Cache) {
    let mut s = cache.lock().unwrap();
    s.generation += 1;
    s.results.clear();
    s.read_at.clear();
    s.rereading.clear();
}

/// Changes every time [`clear`] runs.
pub fn generation(cache: &Cache) -> u64 {
    cache.lock().unwrap().generation
}

/// Stores a finished read unless the cache was cleared after it started.
fn store(cache: &Cache, generation: u64, letter: String, result: Result<Report, ReadError>) {
    let mut s = cache.lock().unwrap();
    if s.generation == generation {
        s.rereading.remove(&letter);
        s.read_at.insert(letter.clone(), unix_now());
        s.results.insert(letter, Some(result));
    }
}

/// Starts a background read of `letter` unless one is already running. A result already stored
/// stays visible until the new one replaces it.
pub fn refresh(cache: &Cache, letter: &str, read: ReadFn) {
    let mut s = cache.lock().unwrap();
    let known = match s.results.get(letter) {
        // The first read of this letter is still running.
        Some(None) => return,
        Some(Some(_)) => true,
        None => false,
    };
    if known {
        if !s.rereading.insert(letter.to_string()) {
            return;
        }
    } else {
        s.results.insert(letter.into(), None);
    }
    let (cache, letter, generation) = (cache.clone(), letter.to_string(), s.generation);
    std::thread::spawn(move || {
        let result = read(&letter);
        store(&cache, generation, letter, result);
    });
}

/// Returns the cached result, starting a background read the first time a letter is asked for.
pub fn get(cache: &Cache, letter: &str) -> Option<Result<Report, ReadError>> {
    let known = cache.lock().unwrap().results.contains_key(letter);
    if !known {
        refresh(cache, letter, report);
    }
    cache.lock().unwrap().results.get(letter).cloned().flatten()
}

/// The stored result and when it was read, without starting a read.
pub fn peek(cache: &Cache, letter: &str) -> Option<(Result<Report, ReadError>, u64)> {
    let s = cache.lock().unwrap();
    let result = s.results.get(letter)?.clone()?;
    Some((result, s.read_at.get(letter).copied().unwrap_or(0)))
}

fn chip(ui: &mut egui::Ui, verdict: Verdict) {
    let color = match verdict {
        Verdict::Healthy => GREEN,
        Verdict::Watch => AMBER,
        Verdict::ReplaceSoon | Verdict::Problem => RED,
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
                let (verdict, reason) = assess(&r.health);
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
                            .show(ui, |ui| match &r.health {
                                Health::Drive(h) => {
                                    for (label, value) in rows(h) {
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
                                }
                                Health::Windows(w) => {
                                    for (label, value) in windows_rows(w) {
                                        weak(ui, label);
                                        ui.label(value);
                                        ui.end_row();
                                    }
                                }
                            });
                        theme::caption(
                            ui,
                            match &r.health {
                                Health::Drive(_) => "Values as reported by the drive when Disk Manager read them. The verdict uses only these values.",
                                Health::Windows(_) => "Windows' own health rating, read without changing anything. Disk Manager did not read NVMe or SMART data for this drive, so no wear, temperature or sector values are shown.",
                            },
                        );
                    });
            }
            Some(Err(ReadError::AccessDenied)) => weak(
                ui,
                "This drive only gives its health data to administrators. Disk Manager runs without administrator rights, so health is not shown for it.",
            ),
            Some(Err(ReadError::Unsupported)) => weak(
                ui,
                "This drive does not report health through Windows. USB, memory card and virtual drives often do not.",
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

    fn win(health: Option<u16>, operational: &[u16]) -> WindowsStatus {
        WindowsStatus {
            health,
            operational: operational.to_vec(),
        }
    }

    #[test]
    fn windows_health_maps_to_the_three_ratings() {
        assert_eq!(rate(&win(Some(0), &[2])), Some(Rating::Healthy));
        assert_eq!(rate(&win(Some(0), &[])), Some(Rating::Healthy));
        assert_eq!(rate(&win(Some(1), &[2])), Some(Rating::Warning));
        assert_eq!(rate(&win(Some(2), &[2])), Some(Rating::Unhealthy));
        // Degraded and predictive failure raise a Healthy status to Warning.
        assert_eq!(rate(&win(Some(0), &[3])), Some(Rating::Warning));
        assert_eq!(rate(&win(Some(0), &[2, 5])), Some(Rating::Warning));
        // Lost communication and no contact are a problem whatever HealthStatus says.
        assert_eq!(rate(&win(Some(0), &[13])), Some(Rating::Unhealthy));
        assert_eq!(rate(&win(Some(1), &[12])), Some(Rating::Unhealthy));
        // Never lowered by a clean operational status.
        assert_eq!(rate(&win(Some(2), &[2])), Some(Rating::Unhealthy));
    }

    #[test]
    fn windows_unknown_or_missing_health_is_not_a_rating() {
        assert_eq!(rate(&win(Some(5), &[])), None, "Unknown");
        assert_eq!(rate(&win(Some(5), &[2])), None, "OK alone is not a rating");
        assert_eq!(rate(&win(None, &[])), None);
        assert_eq!(rate(&win(Some(99), &[])), None);
        // Trouble in the operational status is still reported.
        assert_eq!(rate(&win(Some(5), &[13])), Some(Rating::Unhealthy));
    }

    fn rating(status: WindowsStatus, raid: bool) -> WindowsRating {
        WindowsRating {
            rating: rate(&status).unwrap(),
            status,
            raid,
        }
    }

    #[test]
    fn windows_verdicts_and_wording() {
        let (v, text) = assess(&Health::Windows(rating(win(Some(0), &[2]), true)));
        assert_eq!(v, Verdict::Healthy);
        assert_eq!(
            text,
            "Windows rates this drive Healthy. This PC runs its drives in RAID mode, so detailed drive data (wear, temperature, reallocated sectors) is not available."
        );
        let (v, text) = assess(&Health::Windows(rating(win(Some(1), &[3]), true)));
        assert_eq!(v, Verdict::Watch);
        assert!(text.starts_with("Windows rates this drive Warning. Status: Degraded."));
        let (v, text) = assess(&Health::Windows(rating(win(Some(2), &[13]), false)));
        assert_eq!(v, Verdict::Problem);
        assert!(text.starts_with("Windows rates this drive Unhealthy. Status: Lost communication."));
        assert!(!text.contains("RAID"), "{text}");
        // No wording ever claims a value that was not read.
        for raid in [true, false] {
            let (_, text) = assess(&Health::Windows(rating(win(Some(0), &[2]), raid)));
            assert!(!text.contains("SMART") && !text.contains("°C") && !text.contains('%'));
        }
    }

    #[test]
    fn windows_rows_show_only_what_windows_returned() {
        let rows = windows_rows(&rating(win(Some(0), &[2]), true));
        assert_eq!(
            rows,
            vec![
                ("Windows health status", "Healthy".to_string()),
                ("Windows operational status", "OK".to_string()),
            ]
        );
        let rows = windows_rows(&rating(win(Some(1), &[3, 0xD010]), false));
        assert_eq!(rows[1].1, "Degraded, Code 53264");
    }

    /// A direct read that found raw bytes on disk 4.
    fn page(protocol: Protocol, bytes: Vec<u8>) -> Result<Probe, ReadError> {
        Ok(Probe::Page {
            protocol,
            model: "Direct".into(),
            bytes,
            disk: 4,
        })
    }

    fn no_direct(raid: bool) -> Result<Probe, ReadError> {
        Ok(Probe::NoDirect {
            disk: 7,
            model: "Behind RAID".into(),
            raid,
        })
    }

    /// Fails the test if Windows is asked.
    fn never(_: u32) -> Result<WindowsStatus, String> {
        panic!("Windows must not be asked when the drive gave its own data");
    }

    #[test]
    fn direct_data_wins_and_windows_is_not_asked() {
        let r = resolve(
            page(Protocol::Nvme, nvme(0, 311, 100, 3, 8_760, 42, 0)),
            &never,
        )
        .unwrap();
        assert!(matches!(r.health, Health::Drive(_)));
        assert_eq!(r.model, "Direct");
        let r = resolve(page(Protocol::Ata, ata(&[(5, 100, 0)])), &never).unwrap();
        assert!(matches!(r.health, Health::Drive(_)));
    }

    #[test]
    fn errors_other_than_unsupported_do_not_fall_back() {
        assert_eq!(
            resolve(Err(ReadError::AccessDenied), &never),
            Err(ReadError::AccessDenied)
        );
        assert_eq!(
            resolve(Err(ReadError::NotFound), &never),
            Err(ReadError::NotFound)
        );
    }

    #[test]
    fn a_raid_drive_gets_windows_health_for_its_own_disk_number() {
        let ask = |disk: u32| {
            assert_eq!(disk, 7, "the disk the drive letter is on");
            Ok(win(Some(0), &[2]))
        };
        let r = resolve(no_direct(true), &ask).unwrap();
        assert_eq!(r.model, "Behind RAID");
        match r.health {
            Health::Windows(w) => {
                assert_eq!(w.rating, Rating::Healthy);
                assert!(w.raid);
            }
            Health::Drive(_) => panic!("no SMART or NVMe data exists for this drive"),
        }
        // Warning and Unhealthy come through as such.
        let warn = |_| Ok(win(Some(1), &[2]));
        let r = resolve(no_direct(true), &warn).unwrap();
        assert_eq!(assess(&r.health).0, Verdict::Watch);
        let bad = |_| Ok(win(Some(2), &[13]));
        let r = resolve(no_direct(true), &bad).unwrap();
        assert_eq!(assess(&r.health).0, Verdict::Problem);
    }

    #[test]
    fn an_unsupported_direct_read_also_falls_back_without_the_raid_wording() {
        let ok = |_| Ok(win(Some(0), &[2]));
        let r = resolve(no_direct(false), &ok).unwrap();
        let (v, text) = assess(&r.health);
        assert_eq!(v, Verdict::Healthy);
        assert!(!text.contains("RAID"), "{text}");
        // A drive whose bytes did not parse is the same case.
        let r = resolve(page(Protocol::Nvme, vec![0; PAGE]), &ok).unwrap();
        assert!(matches!(r.health, Health::Windows(_)));
        let r = resolve(page(Protocol::Ata, vec![0; PAGE]), &|disk| {
            assert_eq!(disk, 4);
            Ok(win(Some(0), &[]))
        })
        .unwrap();
        assert!(matches!(
            r.health,
            Health::Windows(WindowsRating { raid: false, .. })
        ));
    }

    #[test]
    fn unreadable_windows_health_is_unavailable_with_an_honest_reason() {
        let fail = |_| Err("Windows does not list disk 7".to_string());
        match resolve(no_direct(true), &fail) {
            Err(ReadError::Other(e)) => {
                assert!(e.contains("RAID mode"), "{e}");
                assert!(e.contains("Windows does not list disk 7"), "{e}");
            }
            other => panic!("{other:?}"),
        }
        let unknown = |_| Ok(win(Some(5), &[]));
        match resolve(no_direct(true), &unknown) {
            Err(ReadError::Other(e)) => assert!(e.contains("Unknown"), "{e}"),
            other => panic!("{other:?}"),
        }
        // Not RAID: the plain "does not report health" answer stays.
        assert_eq!(
            resolve(no_direct(false), &fail),
            Err(ReadError::Unsupported)
        );
        assert_eq!(
            resolve(no_direct(false), &unknown),
            Err(ReadError::Unsupported)
        );
    }

    /// Prints what Windows' own storage health says for disks 0 to 5, read only.
    #[test]
    #[ignore = "reads real hardware"]
    fn reads_windows_health_of_this_pc() {
        for disk in 0..6 {
            let status = windows_status(disk);
            println!(
                "disk {disk}: {status:?} -> {:?}",
                status.as_ref().ok().and_then(rate)
            );
        }
    }

    /// Prints what this PC reports for C: without administrator rights.
    #[test]
    #[ignore = "reads real hardware"]
    fn reads_this_pc() {
        match report("C:") {
            Ok(r) => println!("{} {:?} {:?}", r.model, r.health, assess(&r.health)),
            Err(e) => println!("error {e:?}"),
        }
    }
}
