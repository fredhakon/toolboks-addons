//! The real data source: reads this PC. Everything here is read-only. It opens registry keys
//! with `KEY_READ`, asks Windows for the SMBIOS table, the power status and performance
//! counters, and never writes, never starts a program, and does not need administrator rights.

#![allow(clippy::missing_safety_doc)]

use std::iter::once;
use std::time::Duration;

use windows::core::{s, w, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_PATH_NOT_FOUND,
    ERROR_SUCCESS, HLOCAL, WIN32_ERROR,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};
use windows::Win32::System::Power::{
    GetSystemPowerStatus, PowerGetActiveScheme, SYSTEM_POWER_STATUS,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE,
    KEY_READ, KEY_WOW64_64KEY, REG_BINARY, REG_DWORD, REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::System::SystemInformation::{GetSystemFirmwareTable, FIRMWARE_TABLE_PROVIDER};

use crate::date::Date;
use crate::gpu_driver::{GpuDriver, GpuDriverSource};
use crate::memory_integrity::{MemoryIntegrity, MemoryIntegritySource};
use crate::power::{
    plan_kind_from_guid, power_mode_from_guid, PlanKind, PowerMode, PowerSource, PowerStatus,
};
use crate::ram::{MemoryModule, MemorySource};
use crate::restart::RestartSource;
use crate::result::{ReadError, Reading};
use crate::smbios::parse_memory_modules;
use crate::throttle::{ThrottleSample, ThrottleSource};

/// The registry keys the checks read (all under `HKEY_LOCAL_MACHINE`). Listed in the README.
pub const KEY_MEMORY_INTEGRITY: &str =
    r"SYSTEM\CurrentControlSet\Control\DeviceGuard\Scenarios\HypervisorEnforcedCodeIntegrity";
pub const KEY_UPDATE_REBOOT_REQUIRED: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired";
pub const KEY_SERVICING_REBOOT_PENDING: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending";
pub const KEY_DISPLAY_CLASS: &str =
    r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";

const COUNTER_PERFORMANCE_LIMIT: PCWSTR = w!(r"\Processor Information(_Total)\% Performance Limit");
const COUNTER_MAX_FREQUENCY: PCWSTR = w!(r"\Processor Information(_Total)\% of Maximum Frequency");

/// Reads this PC. Has no state.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsSource;

// ---- registry helpers ------------------------------------------------------------------

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(once(0)).collect()
}

fn map_error(code: WIN32_ERROR, what: &str) -> ReadError {
    match code {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => ReadError::NotPresent(what.to_string()),
        ERROR_ACCESS_DENIED => ReadError::AccessDenied,
        other => ReadError::Failed(format!("{what}: Windows error {}", other.0)),
    }
}

/// An open registry key, closed when dropped.
struct Key {
    handle: HKEY,
    path: String,
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: `handle` was opened by `RegOpenKeyExW` and is closed exactly once here.
        unsafe {
            let _ = RegCloseKey(self.handle);
        }
    }
}

fn open_key(path: &str) -> Reading<Key> {
    let name = wide(path);
    let mut handle = HKEY::default();
    // SAFETY: `name` is NUL-terminated and outlives the call; `handle` is a valid out pointer.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(name.as_ptr()),
            None,
            KEY_READ | KEY_WOW64_64KEY,
            &mut handle,
        )
    };
    if status == ERROR_SUCCESS {
        Ok(Key {
            handle,
            path: path.to_string(),
        })
    } else {
        Err(map_error(status, &format!("registry key {path}")))
    }
}

fn query_value(key: &Key, name: &str) -> Reading<(REG_VALUE_TYPE, Vec<u8>)> {
    let value = wide(name);
    let what = format!("registry value {} in {}", name, key.path);
    let mut kind = REG_VALUE_TYPE::default();
    let mut size = 0u32;
    // SAFETY: first call only asks for the type and size; all pointers are valid for the call.
    let status = unsafe {
        RegQueryValueExW(
            key.handle,
            PCWSTR(value.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(map_error(status, &what));
    }
    let mut data = vec![0u8; size as usize];
    // SAFETY: `data` has `size` bytes, which is what is passed as the buffer size.
    let status = unsafe {
        RegQueryValueExW(
            key.handle,
            PCWSTR(value.as_ptr()),
            None,
            Some(&mut kind),
            Some(data.as_mut_ptr()),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(map_error(status, &what));
    }
    data.truncate(size as usize);
    Ok((kind, data))
}

fn read_dword(key: &Key, name: &str) -> Reading<u32> {
    let (kind, data) = query_value(key, name)?;
    if kind != REG_DWORD || data.len() < 4 {
        return Err(ReadError::Failed(format!(
            "registry value {name} is not a number"
        )));
    }
    Ok(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

fn read_string(key: &Key, name: &str) -> Reading<String> {
    let (kind, data) = query_value(key, name)?;
    if kind != REG_SZ {
        return Err(ReadError::Failed(format!(
            "registry value {name} is not text"
        )));
    }
    let (pairs, _) = data.as_chunks::<2>();
    let units: Vec<u16> = pairs
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|u| *u != 0)
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

fn subkey_names(key: &Key) -> Reading<Vec<String>> {
    let mut names = Vec::new();
    for index in 0u32.. {
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        // SAFETY: `buf` holds `len` UTF-16 units; the other pointers are None.
        let status = unsafe {
            RegEnumKeyExW(
                key.handle,
                index,
                Some(PWSTR(buf.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        if status != ERROR_SUCCESS {
            return Err(map_error(status, &format!("subkeys of {}", key.path)));
        }
        names.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    Ok(names)
}

// ---- 1. memory speed -------------------------------------------------------------------

impl MemorySource for WindowsSource {
    fn modules(&self) -> Reading<Vec<MemoryModule>> {
        // 'RSMB': the raw SMBIOS table, the same data Win32_PhysicalMemory is built from.
        let provider = FIRMWARE_TABLE_PROVIDER(0x5253_4D42);
        // SAFETY: a None buffer only asks for the needed size.
        let size = unsafe { GetSystemFirmwareTable(provider, 0, None) } as usize;
        if size < 8 {
            return Err(ReadError::NotPresent(
                "Windows did not return an SMBIOS table".into(),
            ));
        }
        let mut buf = vec![0u8; size];
        // SAFETY: `buf` is `size` bytes long.
        let written = unsafe { GetSystemFirmwareTable(provider, 0, Some(&mut buf)) } as usize;
        if written < 8 || written > buf.len() {
            return Err(ReadError::Failed(
                "the SMBIOS table could not be read".into(),
            ));
        }
        // RawSMBIOSData: used-20-calling-method, major, minor, dmi revision, u32 length, table.
        let table_len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        let end = (8 + table_len).min(written);
        Ok(parse_memory_modules(&buf[8..end]))
    }
}

// ---- 2. power plan, power mode, battery saver ---------------------------------------------

type OverlayFn = unsafe extern "system" fn(*mut GUID) -> u32;

impl PowerSource for WindowsSource {
    fn active_plan(&self) -> Reading<PlanKind> {
        let mut guid: *mut GUID = std::ptr::null_mut();
        // SAFETY: `guid` receives a pointer to a GUID that Windows allocated for us.
        let status = unsafe { PowerGetActiveScheme(None, &mut guid) };
        if status != ERROR_SUCCESS || guid.is_null() {
            return Err(map_error(status, "the active power scheme"));
        }
        // SAFETY: Windows returned a valid GUID pointer; it must be released with LocalFree.
        let kind = unsafe {
            let value = (*guid).to_u128();
            let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(guid.cast())));
            plan_kind_from_guid(value)
        };
        Ok(kind)
    }

    fn power_mode(&self) -> Reading<PowerMode> {
        // PowerGetEffectiveOverlayScheme is not in the generated bindings, so it is looked up
        // by name. A Windows without the Power mode slider does not export it.
        let missing = || ReadError::Unsupported("This Windows has no Power mode setting.".into());
        // SAFETY: powrprof.dll is a system DLL; the looked-up function has the documented
        // signature `DWORD PowerGetEffectiveOverlayScheme(GUID*)`.
        unsafe {
            let module = LoadLibraryW(w!("powrprof.dll")).map_err(|_| missing())?;
            let proc =
                GetProcAddress(module, s!("PowerGetEffectiveOverlayScheme")).ok_or_else(missing)?;
            let call: OverlayFn = std::mem::transmute(proc);
            let mut guid = GUID::zeroed();
            let status = call(&mut guid);
            if status != 0 {
                return Err(ReadError::Failed(format!(
                    "reading the power mode: Windows error {status}"
                )));
            }
            Ok(power_mode_from_guid(guid.to_u128()))
        }
    }

    fn status(&self) -> Reading<PowerStatus> {
        let mut status = SYSTEM_POWER_STATUS::default();
        // SAFETY: `status` is a valid out pointer.
        unsafe { GetSystemPowerStatus(&mut status) }
            .map_err(|e| ReadError::Failed(format!("reading the power status: {e}")))?;
        Ok(PowerStatus {
            on_ac: match status.ACLineStatus {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            },
            battery_saver: status.SystemStatusFlag & 1 != 0,
        })
    }
}

// ---- 3. memory integrity -------------------------------------------------------------------

impl MemoryIntegritySource for WindowsSource {
    fn state(&self) -> Reading<MemoryIntegrity> {
        let key = open_key(KEY_MEMORY_INTEGRITY)?;
        match read_dword(&key, "Enabled")? {
            0 => Ok(MemoryIntegrity::Off),
            1 => Ok(MemoryIntegrity::On),
            other => Err(ReadError::Failed(format!(
                "the Memory integrity setting has the unexpected value {other}"
            ))),
        }
    }
}

// ---- 4. throttling --------------------------------------------------------------------------

/// Closes the PDH query when dropped.
struct Query(PDH_HQUERY);

impl Drop for Query {
    fn drop(&mut self) {
        // SAFETY: the query was opened by `PdhOpenQueryW` and is closed exactly once here.
        unsafe {
            let _ = PdhCloseQuery(self.0);
        }
    }
}

fn add_counter(query: &Query, path: PCWSTR) -> Option<PDH_HCOUNTER> {
    let mut counter = PDH_HCOUNTER::default();
    // SAFETY: `path` is a NUL-terminated literal; `counter` is a valid out pointer.
    let status = unsafe { PdhAddEnglishCounterW(query.0, path, 0, &mut counter) };
    (status == 0).then_some(counter)
}

fn read_counter(counter: Option<PDH_HCOUNTER>) -> Option<f64> {
    let counter = counter?;
    let mut value = PDH_FMT_COUNTERVALUE::default();
    // SAFETY: `value` is a valid out pointer for a double-formatted counter.
    let status = unsafe { PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, None, &mut value) };
    // CStatus 0 is valid data, 1 is valid data from a new instance.
    if status == 0 && value.CStatus <= 1 {
        // SAFETY: the counter was formatted as a double, so that union field is the one set.
        Some(unsafe { value.Anonymous.doubleValue })
    } else {
        None
    }
}

impl ThrottleSource for WindowsSource {
    fn sample(&self, count: usize, interval: Duration) -> Reading<Vec<ThrottleSample>> {
        let mut handle = PDH_HQUERY::default();
        // SAFETY: a null data source means the live counters; `handle` is a valid out pointer.
        let status = unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut handle) };
        if status != 0 {
            return Err(ReadError::Failed(format!(
                "opening the performance counters: error {status:#x}"
            )));
        }
        let query = Query(handle);
        let limit = add_counter(&query, COUNTER_PERFORMANCE_LIMIT);
        let freq = add_counter(&query, COUNTER_MAX_FREQUENCY);
        if limit.is_none() && freq.is_none() {
            return Err(ReadError::NotPresent(
                "the Processor Information counters do not exist on this PC".into(),
            ));
        }
        // SAFETY: the query is open.
        let primed = unsafe { PdhCollectQueryData(query.0) };
        if primed != 0 {
            return Err(ReadError::Failed(format!(
                "reading the performance counters: error {primed:#x}"
            )));
        }
        let mut samples = Vec::with_capacity(count);
        for _ in 0..count {
            std::thread::sleep(interval);
            // SAFETY: the query is open.
            if unsafe { PdhCollectQueryData(query.0) } != 0 {
                samples.push(ThrottleSample {
                    performance_limit: None,
                    max_frequency: None,
                });
                continue;
            }
            samples.push(ThrottleSample {
                performance_limit: read_counter(limit),
                max_frequency: read_counter(freq),
            });
        }
        Ok(samples)
    }
}

// ---- 5. pending restart ---------------------------------------------------------------------

/// A marker key: present means "restart needed", missing means "not needed".
fn marker_present(path: &str) -> Reading<bool> {
    match open_key(path) {
        Ok(_) => Ok(true),
        Err(ReadError::NotPresent(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

impl RestartSource for WindowsSource {
    fn windows_update_reboot_required(&self) -> Reading<bool> {
        marker_present(KEY_UPDATE_REBOOT_REQUIRED)
    }

    fn servicing_reboot_pending(&self) -> Reading<bool> {
        marker_present(KEY_SERVICING_REBOOT_PENDING)
    }
}

// ---- 6. graphics driver ---------------------------------------------------------------------

fn driver_date(key: &Key) -> Option<Date> {
    if let Ok(text) = read_string(key, "DriverDate") {
        if let Some(date) = Date::parse_month_day_year(&text) {
            return Some(date);
        }
    }
    match query_value(key, "DriverDateData") {
        Ok((kind, data)) if kind == REG_BINARY && data.len() >= 8 => {
            let ticks = u64::from_le_bytes(data[..8].try_into().ok()?);
            Date::from_filetime(ticks)
        }
        _ => None,
    }
}

impl GpuDriverSource for WindowsSource {
    fn drivers(&self) -> Reading<Vec<GpuDriver>> {
        let class = open_key(KEY_DISPLAY_CLASS)?;
        let mut drivers = Vec::new();
        let mut first_error: Option<ReadError> = None;
        // Adapters are the numbered subkeys 0000, 0001, ...; "Configuration" and "Properties"
        // are not adapters.
        for name in subkey_names(&class)? {
            if name.len() != 4 || !name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let adapter = match open_key(&format!(r"{KEY_DISPLAY_CLASS}\{name}")) {
                Ok(k) => k,
                Err(e) => {
                    first_error.get_or_insert(e);
                    continue;
                }
            };
            let Ok(desc) = read_string(&adapter, "DriverDesc") else {
                continue; // a leftover key with no driver
            };
            drivers.push(GpuDriver {
                name: desc,
                provider: read_string(&adapter, "ProviderName").unwrap_or_default(),
                date: driver_date(&adapter),
            });
        }
        match (drivers.is_empty(), first_error) {
            (true, Some(e)) => Err(e),
            _ => Ok(drivers),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Run against the PC the tests run on. They only check that each check returns without
    //! panicking and that the answer is one of the four allowed shapes; they cannot fail a
    //! machine that lacks the thing being checked (no battery, no Power mode slider, no
    //! counters), because that is a valid `NotChecked` or `Unavailable`.
    use super::*;
    use crate::result::{CheckId, Outcome};
    use crate::{run_check, unix_now};

    fn real(id: CheckId) -> Outcome {
        let result = run_check(id, &WindowsSource, &unix_now);
        assert_eq!(result.check, id);
        assert!(result.measured_at > 1_700_000_000, "the clock is read");
        result.outcome
    }

    #[test]
    fn real_ram_speed_returns() {
        let _ = real(CheckId::RamSpeed);
    }

    #[test]
    fn real_power_returns() {
        let _ = real(CheckId::PowerPlan);
    }

    #[test]
    fn real_memory_integrity_returns() {
        let _ = real(CheckId::MemoryIntegrity);
    }

    #[test]
    fn real_throttling_returns() {
        let _ = real(CheckId::CpuThrottling);
    }

    #[test]
    fn real_pending_restart_returns() {
        let _ = real(CheckId::PendingRestart);
    }

    #[test]
    fn real_gpu_driver_returns() {
        let _ = real(CheckId::GpuDriverAge);
    }

    #[test]
    fn real_texts_are_plain_and_bounded() {
        for id in [
            CheckId::RamSpeed,
            CheckId::PendingRestart,
            CheckId::GpuDriverAge,
        ] {
            match real(id) {
                Outcome::Finding(f) => {
                    assert!(f.observation.chars().count() <= crate::result::MAX_FINDING_CHARS);
                    assert!(!f.action.contains('<'));
                }
                Outcome::NotChecked { reason } | Outcome::Unavailable { reason } => {
                    assert!(reason.chars().count() <= crate::result::MAX_REASON_CHARS);
                }
                Outcome::Ok => {}
            }
        }
    }
}
