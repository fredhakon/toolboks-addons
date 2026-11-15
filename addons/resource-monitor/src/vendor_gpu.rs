//! Temperature, power and clocks for AMD and Intel GPUs from the vendor driver libraries,
//! loaded at run time from System32 only, like `nvml.rs`. Nothing is installed or bundled.
//!
//! AMD: the display library `atiadlxx.dll` (ADL). The PM log (RX 5000 and newer) comes first;
//! Overdrive N temperature and Overdrive 6 power cover older cards.
//! Intel: the control library `ControlLib.dll` (IGCL) that recent Arc and Iris Xe drivers
//! install. It gives temperature and an energy counter, so power appears from the second
//! reading on; it gives no clocks here.
//!
//! Every call here runs on the sampler thread, never the UI thread.

use std::alloc::Layout;
use std::ffi::{c_void, CStr};
use std::ptr::null_mut;
use std::sync::Mutex;

use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

use super::nvml::{export, NvmlReading};

/// Who made the GPU, from the names sysinfo reports.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Other,
}

impl Vendor {
    pub fn of(name: &str, vendor: Option<&str>) -> Vendor {
        let text = format!("{name} {}", vendor.unwrap_or_default()).to_lowercase();
        if text.contains("nvidia") {
            Vendor::Nvidia
        } else if ["amd", "radeon", "advanced micro devices"]
            .iter()
            .any(|k| text.contains(k))
        {
            Vendor::Amd
        } else if text.contains("intel") {
            Vendor::Intel
        } else {
            Vendor::Other
        }
    }
}

/// Which of temperature and power the GPU's driver gives.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Shares {
    pub temp: bool,
    pub power: bool,
}

impl Shares {
    /// Stat labels of the readings this GPU does not share, shown as "Not shared".
    pub fn missing(self) -> Vec<&'static str> {
        [(self.temp, "Temp"), (self.power, "Power")]
            .into_iter()
            .filter(|(has, _)| !has)
            .map(|(_, label)| label)
            .collect()
    }
}

/// An opened AMD or Intel driver library.
pub enum VendorGpu {
    Amd(Adl),
    Intel(Igcl),
}

impl VendorGpu {
    /// `None` when the vendor's library is missing, fails to start, cannot tell which of its
    /// GPUs is the one named `name`, or has no sensor for it.
    pub fn open(vendor: Vendor, name: &str) -> Option<VendorGpu> {
        match vendor {
            Vendor::Amd => Adl::open(name).map(VendorGpu::Amd),
            Vendor::Intel => Igcl::open().map(VendorGpu::Intel),
            Vendor::Nvidia | Vendor::Other => None,
        }
    }

    pub fn shares(&self) -> Shares {
        match self {
            VendorGpu::Amd(a) => a.shares,
            VendorGpu::Intel(i) => i.shares,
        }
    }

    pub fn read(&mut self) -> Option<NvmlReading> {
        match self {
            VendorGpu::Amd(a) => a.read(),
            VendorGpu::Intel(i) => i.read(),
        }
    }
}

/// The library for the GPU the sampler shows, reopened when that GPU changes.
#[derive(Default)]
pub struct VendorCache {
    key: Option<(Vendor, String)>,
    gpu: Option<VendorGpu>,
}

impl VendorCache {
    pub fn get(&mut self, vendor: Vendor, name: &str) -> Option<&mut VendorGpu> {
        self.get_with(vendor, name, VendorGpu::open)
    }

    fn get_with(
        &mut self,
        vendor: Vendor,
        name: &str,
        open: impl FnOnce(Vendor, &str) -> Option<VendorGpu>,
    ) -> Option<&mut VendorGpu> {
        if self.key.as_ref().map(|(v, n)| (*v, n.as_str())) != Some((vendor, name)) {
            // Drop the old library before opening: only one `Adl` may live at a time.
            self.gpu = None;
            self.gpu = open(vendor, name);
            self.key = Some((vendor, name.to_string()));
        }
        self.gpu.as_mut()
    }
}

fn load_system(name: PCWSTR) -> Option<HMODULE> {
    // SAFETY: a fixed file name; the module and its imports are searched in System32 only.
    unsafe { LoadLibraryExW(name, None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.ok()
}

/// A library from `load_system`, freed on drop. Tests use `Lib(None)` with fake functions.
struct Lib(Option<HMODULE>);

impl Drop for Lib {
    fn drop(&mut self) {
        if let Some(lib) = self.0 {
            // SAFETY: `lib` came from LoadLibraryExW and is freed exactly once.
            let _ = unsafe { FreeLibrary(lib) };
        }
    }
}

// ---- AMD (ADL) ----

const ADL_OK: i32 = 0;
const ADL_MAX_PATH: usize = 256;
const ADL_MAX_ROWS: i32 = 128;
const PM_SENSORS: usize = 256;
// Indices into the PM log, from ADL_PMLOG_SENSORS in the ADL SDK.
const PM_GFX_CLOCK: usize = 1;
const PM_MEM_CLOCK: usize = 2;
const PM_TEMP_EDGE: usize = 8;
const PM_ASIC_POWER: usize = 23;
const PM_TEMP_HOTSPOT: usize = 27;
const ODN_TEMP_CORE: i32 = 1;
const OD6_TOTAL_POWER: i32 = 0;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PmSensor {
    supported: i32,
    value: i32,
}

#[repr(C)]
struct PmLog {
    size: i32,
    sensors: [PmSensor; PM_SENSORS],
}

/// `AdapterInfo` from the ADL SDK, Windows layout (1572 bytes). Only some fields are read.
#[repr(C)]
#[allow(dead_code)]
struct AdapterInfo {
    size: i32,
    index: i32,
    udid: [u8; ADL_MAX_PATH],
    bus: i32,
    device: i32,
    function: i32,
    vendor_id: i32,
    name: [u8; ADL_MAX_PATH],
    display_name: [u8; ADL_MAX_PATH],
    present: i32,
    exist: i32,
    driver_path: [u8; ADL_MAX_PATH],
    driver_path_ext: [u8; ADL_MAX_PATH],
    pnp: [u8; ADL_MAX_PATH],
    os_display_index: i32,
}

type AdlAlloc = extern "system" fn(i32) -> *mut c_void;
type AdlCreate = unsafe extern "C" fn(AdlAlloc, i32, *mut *mut c_void) -> i32;
type AdlDestroy = unsafe extern "C" fn(*mut c_void) -> i32;
type AdlCount = unsafe extern "C" fn(*mut c_void, *mut i32) -> i32;
type AdlInfoGet = unsafe extern "C" fn(*mut c_void, *mut AdapterInfo, i32) -> i32;
type AdlPmLog = unsafe extern "C" fn(*mut c_void, i32, *mut PmLog) -> i32;
/// Overdrive N temperature and Overdrive 6 power share this shape.
type AdlSensor = unsafe extern "C" fn(*mut c_void, i32, i32, *mut i32) -> i32;

/// What ADL's allocator callback handed out, freed after the context is destroyed. ADL has
/// no free callback, so this memory is ours. Only one `Adl` lives at a time (`VendorCache`
/// drops the old one first), so everything listed belongs to it.
static ADL_ALLOCS: Mutex<Vec<(usize, Layout)>> = Mutex::new(Vec::new());

extern "system" fn adl_alloc(size: i32) -> *mut c_void {
    let Ok(layout) = Layout::from_size_align(size.max(1) as usize, 16) else {
        return null_mut();
    };
    // SAFETY: the layout has a non-zero size.
    let p = unsafe { std::alloc::alloc(layout) };
    if !p.is_null() {
        let mut allocs = ADL_ALLOCS.lock().unwrap_or_else(|e| e.into_inner());
        allocs.push((p as usize, layout));
    }
    p.cast()
}

fn free_adl_allocs() {
    let mut allocs = ADL_ALLOCS.lock().unwrap_or_else(|e| e.into_inner());
    for (p, layout) in allocs.drain(..) {
        // SAFETY: allocated by `adl_alloc` with this layout and freed once.
        unsafe { std::alloc::dealloc(p as *mut u8, layout) };
    }
}

/// Overdrive N reports millidegrees.
fn millidegrees_to_c(v: i32) -> f32 {
    v as f32 / 1000.0
}

/// Overdrive 6 reports watts as 8.8 fixed point.
fn fixed_8_8_to_watts(v: i32) -> f32 {
    v as f32 / 256.0
}

/// PM log values are already °C, W and MHz; unsupported sensors stay `None`.
fn from_pm_log(sensors: &[PmSensor]) -> NvmlReading {
    let get = |i: usize| sensors.get(i).filter(|s| s.supported != 0).map(|s| s.value);
    NvmlReading {
        temp_c: get(PM_TEMP_EDGE).or(get(PM_TEMP_HOTSPOT)).map(|v| v as f32),
        power_w: get(PM_ASIC_POWER).map(|v| v as f32),
        clock_mhz: get(PM_GFX_CLOCK).and_then(|v| u32::try_from(v).ok()),
        mem_clock_mhz: get(PM_MEM_CLOCK).and_then(|v| u32::try_from(v).ok()),
    }
}

/// The ADL calls made per reading; every one is optional because driver versions differ.
#[derive(Clone, Copy)]
struct AdlFns {
    pm_log: Option<AdlPmLog>,
    odn_temp: Option<AdlSensor>,
    od6_power: Option<AdlSensor>,
}

impl AdlFns {
    /// One reading from `adapter`: the PM log first, Overdrive N and 6 for what it lacks.
    fn read(&self, ctx: *mut c_void, adapter: i32) -> NvmlReading {
        let mut r = NvmlReading::default();
        if let Some(pm_log) = self.pm_log {
            let mut log = PmLog {
                size: std::mem::size_of::<PmLog>() as i32,
                sensors: [PmSensor::default(); PM_SENSORS],
            };
            // SAFETY: `log` is a writable PM log of the size the SDK declares.
            if unsafe { pm_log(ctx, adapter, &mut log) } == ADL_OK {
                r = from_pm_log(&log.sensors);
            }
        }
        let sensor = |f: Option<AdlSensor>, kind: i32| {
            let f = f?;
            let mut v = 0;
            // SAFETY: `v` is a valid out-parameter.
            (unsafe { f(ctx, adapter, kind, &mut v) } == ADL_OK).then_some(v)
        };
        r.temp_c = r
            .temp_c
            .or_else(|| sensor(self.odn_temp, ODN_TEMP_CORE).map(millidegrees_to_c));
        r.power_w = r
            .power_w
            .or_else(|| sensor(self.od6_power, OD6_TOTAL_POWER).map(fixed_8_8_to_watts));
        r
    }
}

/// One row of ADL's adapter list. ADL lists one row per display output, so a GPU can have
/// several rows that share its PCI location.
#[derive(Clone, Debug, PartialEq)]
struct AmdAdapter {
    index: i32,
    pci: (i32, i32, i32),
    name: String,
}

fn c_text(bytes: &[u8]) -> String {
    CStr::from_bytes_until_nul(bytes).map_or_else(
        |_| String::from_utf8_lossy(bytes).into_owned(),
        |c| c.to_string_lossy().into_owned(),
    )
}

/// The adapter rows ADL reports, keeping each row's own `iAdapterIndex`. Rows without a
/// UDID are placeholders and skipped. Rows are not filtered on "active": a GPU that drives
/// no display (a laptop's dedicated GPU) reports inactive but still has sensors.
fn adapter_rows(info: AdlInfoGet, ctx: *mut c_void, count: i32) -> Vec<AmdAdapter> {
    let n = count.clamp(0, ADL_MAX_ROWS) as usize;
    if n == 0 {
        return Vec::new();
    }
    let row_size = std::mem::size_of::<AdapterInfo>();
    let mut rows: Vec<AdapterInfo> = (0..n)
        .map(|_| {
            // SAFETY: AdapterInfo holds only integers and byte arrays, so zeros are valid.
            let mut row: AdapterInfo = unsafe { std::mem::zeroed() };
            row.size = row_size as i32;
            row
        })
        .collect();
    // SAFETY: `rows` holds `n` writable entries and the size passed says exactly that.
    if unsafe { info(ctx, rows.as_mut_ptr(), (n * row_size) as i32) } != ADL_OK {
        return Vec::new();
    }
    rows.iter()
        .filter(|r| r.udid[0] != 0)
        .map(|r| AmdAdapter {
            index: r.index,
            pci: (r.bus, r.device, r.function),
            name: c_text(&r.name),
        })
        .collect()
}

/// The ADL index of the GPU named `name`: the one GPU (rows grouped by PCI location) with
/// that name, or the only AMD GPU when no name matches. `None` when that is ambiguous, for
/// example two cards of the same model: no reading is better than another card's.
fn pick_adapter(rows: &[AmdAdapter], name: &str) -> Option<i32> {
    let mut gpus: Vec<&AmdAdapter> = Vec::new();
    for row in rows {
        if !gpus.iter().any(|g| g.pci == row.pci) {
            gpus.push(row);
        }
    }
    let named: Vec<&AmdAdapter> = gpus
        .iter()
        .copied()
        .filter(|g| g.name.trim().eq_ignore_ascii_case(name.trim()))
        .collect();
    match (named.as_slice(), gpus.as_slice()) {
        ([one], _) | ([], [one]) => Some(one.index),
        _ => None,
    }
}

/// A loaded `atiadlxx.dll` with the shown GPU's adapter chosen.
pub struct Adl {
    _lib: Lib,
    context: *mut c_void,
    destroy: Option<AdlDestroy>,
    fns: AdlFns,
    adapter: i32,
    shares: Shares,
}

impl Adl {
    fn open(name: &str) -> Option<Adl> {
        let lib = Lib(Some(load_system(w!("atiadlxx.dll"))?));
        let h = lib.0?;
        // SAFETY: every alias matches the ADL SDK prototype of that export.
        let (create, destroy, count, info, fns) = unsafe {
            (
                export::<AdlCreate>(h, s!("ADL2_Main_Control_Create"))?,
                export::<AdlDestroy>(h, s!("ADL2_Main_Control_Destroy"))?,
                export::<AdlCount>(h, s!("ADL2_Adapter_NumberOfAdapters_Get"))?,
                export::<AdlInfoGet>(h, s!("ADL2_Adapter_AdapterInfo_Get"))?,
                AdlFns {
                    pm_log: export(h, s!("ADL2_New_QueryPMLogData_Get")),
                    odn_temp: export(h, s!("ADL2_OverdriveN_Temperature_Get")),
                    od6_power: export(h, s!("ADL2_Overdrive6_CurrentPower_Get")),
                },
            )
        };
        let mut adl = Adl {
            _lib: lib,
            context: null_mut(),
            destroy: None,
            fns,
            adapter: 0,
            shares: Shares::default(),
        };
        // SAFETY: a valid callback and out-parameter; 1 lists connected adapters only.
        if unsafe { create(adl_alloc, 1, &mut adl.context) } != ADL_OK || adl.context.is_null() {
            return None;
        }
        adl.destroy = Some(destroy);
        let mut n = 0;
        // SAFETY: the context is live and `n` is a valid out-parameter.
        if unsafe { count(adl.context, &mut n) } != ADL_OK {
            return None;
        }
        adl.adapter = pick_adapter(&adapter_rows(info, adl.context, n), name)?;
        let r = adl.fns.read(adl.context, adl.adapter);
        adl.shares = Shares {
            temp: r.temp_c.is_some(),
            power: r.power_w.is_some(),
        };
        (adl.shares.temp || adl.shares.power).then_some(adl)
    }

    fn read(&self) -> Option<NvmlReading> {
        let r = self.fns.read(self.context, self.adapter);
        (r != NvmlReading::default()).then_some(r)
    }
}

impl Drop for Adl {
    fn drop(&mut self) {
        if let Some(destroy) = self.destroy {
            // SAFETY: the context was created in `open` and is destroyed once, before the
            // library is freed by `_lib`.
            unsafe { destroy(self.context) };
        }
        // Only a real library can have called `adl_alloc`; fakes in tests never do.
        if self._lib.0.is_some() {
            free_adl_allocs();
        }
    }
}

// ---- Intel (IGCL) ----

const CTL_OK: u32 = 0;
const CTL_INIT_USE_LEVEL_ZERO: u32 = 1;
const CTL_VERSION_1_1: u32 = (1 << 16) | 1;
const CTL_MAX_HANDLES: u32 = 16;

#[repr(C)]
struct CtlInitArgs {
    size: u32,
    version: u8,
    app_version: u32,
    flags: u32,
    supported_version: u32,
    app_uid: [u32; 4],
}

/// `ctl_power_energy_counter_t`: energy in microjoules, timestamp in microseconds.
#[repr(C)]
#[derive(Default)]
struct CtlEnergy {
    size: u32,
    version: u8,
    energy_uj: u64,
    timestamp_us: u64,
}

type CtlInit = unsafe extern "C" fn(*mut CtlInitArgs, *mut *mut c_void) -> u32;
type CtlClose = unsafe extern "C" fn(*mut c_void) -> u32;
/// ctlEnumerateDevices, ctlEnumTemperatureSensors and ctlEnumPowerDomains share this shape.
type CtlEnum = unsafe extern "C" fn(*mut c_void, *mut u32, *mut *mut c_void) -> u32;
type CtlTemp = unsafe extern "C" fn(*mut c_void, *mut f64) -> u32;
type CtlEnergyFn = unsafe extern "C" fn(*mut c_void, *mut CtlEnergy) -> u32;

/// The count-then-fill pattern of every IGCL enumerator, capped at 16 handles.
fn handles(f: CtlEnum, parent: *mut c_void) -> Vec<*mut c_void> {
    let mut n = 0u32;
    // SAFETY: a null array asks for the count only.
    if unsafe { f(parent, &mut n, null_mut()) } != CTL_OK || n == 0 {
        return Vec::new();
    }
    let mut out = vec![null_mut(); n.min(CTL_MAX_HANDLES) as usize];
    n = out.len() as u32;
    // SAFETY: `out` holds `n` writable handles.
    if unsafe { f(parent, &mut n, out.as_mut_ptr()) } != CTL_OK {
        return Vec::new();
    }
    out.truncate(n as usize);
    out
}

type IntelSensors = (Vec<*mut c_void>, Option<*mut c_void>);

/// Temperature sensors and the first power domain of the only Intel GPU.
/// shortcut: with two Intel GPUs (an integrated one and an Arc card) we cannot yet tell
/// which one the sampler shows, so there is no reading rather than a possibly wrong one.
/// Match the PCI location from `ctlGetDeviceProperties` to lift this.
fn pick_intel(devices: &[*mut c_void], temps: CtlEnum, power: CtlEnum) -> Option<IntelSensors> {
    let [device] = devices else {
        return None;
    };
    let t = handles(temps, *device);
    let p = handles(power, *device).first().copied();
    (!t.is_empty() || p.is_some()).then_some((t, p))
}

/// Average power between two energy counter samples (µJ, µs): µJ per µs is watts.
/// `None` when no time passed or the counter went backwards (a driver reset).
fn watts_between((e0, t0): (u64, u64), (e1, t1): (u64, u64)) -> Option<f32> {
    let dt = t1.checked_sub(t0).filter(|&d| d > 0)?;
    Some((e1.checked_sub(e0)? as f64 / dt as f64) as f32)
}

#[derive(Clone, Copy)]
struct IgclFns {
    temp: CtlTemp,
    energy: CtlEnergyFn,
}

/// A loaded `ControlLib.dll` with the sensors of one Intel GPU.
pub struct Igcl {
    _lib: Lib,
    api: *mut c_void,
    close: Option<CtlClose>,
    fns: IgclFns,
    temps: Vec<*mut c_void>,
    power: Option<*mut c_void>,
    /// The previous energy sample, for power.
    last: Option<(u64, u64)>,
    shares: Shares,
}

impl Igcl {
    fn open() -> Option<Igcl> {
        let lib = Lib(Some(load_system(w!("ControlLib.dll"))?));
        let h = lib.0?;
        // SAFETY: every alias matches the IGCL header prototype of that export.
        let (init, close, devices, temp_enum, power_enum, fns) = unsafe {
            (
                export::<CtlInit>(h, s!("ctlInit"))?,
                export::<CtlClose>(h, s!("ctlClose"))?,
                export::<CtlEnum>(h, s!("ctlEnumerateDevices"))?,
                export::<CtlEnum>(h, s!("ctlEnumTemperatureSensors"))?,
                export::<CtlEnum>(h, s!("ctlEnumPowerDomains"))?,
                IgclFns {
                    temp: export(h, s!("ctlTemperatureGetState"))?,
                    energy: export(h, s!("ctlPowerGetEnergyCounter"))?,
                },
            )
        };
        let mut args = CtlInitArgs {
            size: std::mem::size_of::<CtlInitArgs>() as u32,
            version: 0,
            app_version: CTL_VERSION_1_1,
            flags: CTL_INIT_USE_LEVEL_ZERO,
            supported_version: 0,
            app_uid: [0; 4],
        };
        let mut api = null_mut();
        // SAFETY: `args` is filled as the header documents; `api` is a valid out-parameter.
        if unsafe { init(&mut args, &mut api) } != CTL_OK || api.is_null() {
            return None;
        }
        let mut igcl = Igcl {
            _lib: lib,
            api,
            close: Some(close),
            fns,
            temps: Vec::new(),
            power: None,
            last: None,
            shares: Shares::default(),
        };
        let (temps, power) = pick_intel(&handles(devices, api), temp_enum, power_enum)?;
        igcl.shares = Shares {
            temp: !temps.is_empty(),
            power: power.is_some(),
        };
        igcl.temps = temps;
        igcl.power = power;
        Some(igcl)
    }

    fn read(&mut self) -> Option<NvmlReading> {
        // The hottest sensor: IGCL lists the GPU and memory sensors separately.
        let temp_c = self
            .temps
            .iter()
            .filter_map(|&h| {
                let mut c = 0.0;
                // SAFETY: `h` is a sensor handle from `open`; `c` is a valid out-parameter.
                (unsafe { (self.fns.temp)(h, &mut c) } == CTL_OK).then_some(c as f32)
            })
            .reduce(f32::max);
        let now = self.power.and_then(|h| {
            let mut e = CtlEnergy {
                size: std::mem::size_of::<CtlEnergy>() as u32,
                ..Default::default()
            };
            // SAFETY: `h` is a power domain handle from `open`; `e` is a valid out-parameter.
            (unsafe { (self.fns.energy)(h, &mut e) } == CTL_OK)
                .then_some((e.energy_uj, e.timestamp_us))
        });
        let power_w = self.last.zip(now).and_then(|(a, b)| watts_between(a, b));
        if now.is_some() {
            self.last = now;
        }
        let r = NvmlReading {
            temp_c,
            power_w,
            ..Default::default()
        };
        (r != NvmlReading::default()).then_some(r)
    }
}

impl Drop for Igcl {
    fn drop(&mut self) {
        if let Some(close) = self.close {
            // SAFETY: the API handle was opened in `open` and is closed once, before the
            // library is freed by `_lib`.
            unsafe { close(self.api) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::without_provenance_mut as fake_handle;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn adl(fns: AdlFns) -> Adl {
        Adl {
            _lib: Lib(None),
            context: null_mut(),
            destroy: None,
            fns,
            adapter: 0,
            shares: Shares::default(),
        }
    }

    const NO_ADL: AdlFns = AdlFns {
        pm_log: None,
        odn_temp: None,
        od6_power: None,
    };

    unsafe extern "C" fn pm_full(_: *mut c_void, _: i32, out: *mut PmLog) -> i32 {
        // SAFETY: the caller passes a valid PM log.
        let s = unsafe { &mut (*out).sensors };
        for (i, v) in [
            (PM_TEMP_EDGE, 61),
            (PM_ASIC_POWER, 120),
            (PM_GFX_CLOCK, 2400),
            (PM_MEM_CLOCK, 1000),
        ] {
            s[i] = PmSensor {
                supported: 1,
                value: v,
            };
        }
        ADL_OK
    }

    unsafe extern "C" fn odn_temp(_: *mut c_void, _: i32, kind: i32, v: *mut i32) -> i32 {
        assert_eq!(kind, ODN_TEMP_CORE);
        // SAFETY: valid out-parameter.
        unsafe { *v = 65_000 };
        ADL_OK
    }

    unsafe extern "C" fn od6_power(_: *mut c_void, _: i32, _: i32, v: *mut i32) -> i32 {
        // SAFETY: valid out-parameter.
        unsafe { *v = 100 * 256 };
        ADL_OK
    }

    /// Four rows with non-sequential indices: two outputs of one RX 7800 XT, one
    /// placeholder without a UDID, and an integrated Radeon.
    unsafe extern "C" fn info_rows(_: *mut c_void, out: *mut AdapterInfo, size: i32) -> i32 {
        assert_eq!(size as usize, 4 * std::mem::size_of::<AdapterInfo>());
        // SAFETY: the caller passes four writable rows.
        let rows = unsafe { std::slice::from_raw_parts_mut(out, 4) };
        for (row, (index, bus, udid, name)) in rows.iter_mut().zip([
            (5, 3, true, "AMD Radeon RX 7800 XT"),
            (7, 3, true, "AMD Radeon RX 7800 XT"),
            (8, 9, false, ""),
            (9, 12, true, "AMD Radeon(TM) Graphics"),
        ]) {
            row.index = index;
            row.bus = bus;
            row.udid[0] = udid as u8;
            row.name[..name.len()].copy_from_slice(name.as_bytes());
        }
        ADL_OK
    }

    fn row(index: i32, bus: i32, name: &str) -> AmdAdapter {
        AmdAdapter {
            index,
            pci: (bus, 0, 0),
            name: name.into(),
        }
    }

    #[test]
    fn vendors() {
        assert_eq!(Vendor::of("NVIDIA GeForce RTX 4070", None), Vendor::Nvidia);
        assert_eq!(
            Vendor::of("Radeon RX 7800 XT", Some("Advanced Micro Devices, Inc.")),
            Vendor::Amd
        );
        assert_eq!(
            Vendor::of("Intel(R) Arc(TM) A770 Graphics", None),
            Vendor::Intel
        );
        assert_eq!(
            Vendor::of("Microsoft Basic Render Driver", None),
            Vendor::Other
        );
    }

    #[test]
    fn units() {
        assert_eq!(millidegrees_to_c(65_000), 65.0);
        assert_eq!(fixed_8_8_to_watts(150 * 256 + 128), 150.5);
        assert_eq!(watts_between((0, 0), (2_000_000, 1_000_000)), Some(2.0));
        assert_eq!(watts_between((5, 10), (9, 10)), None);
        assert_eq!(watts_between((9, 10), (5, 20)), None);
    }

    #[test]
    fn missing_library_or_vendor_gives_none() {
        assert!(load_system(w!("toolboks-no-such-gpu-library.dll")).is_none());
        assert!(VendorGpu::open(Vendor::Other, "x").is_none());
        assert!(VendorGpu::open(Vendor::Nvidia, "x").is_none());
    }

    #[test]
    fn amd_pm_log_maps_to_the_ui_reading() {
        let fns = AdlFns {
            pm_log: Some(pm_full),
            ..NO_ADL
        };
        assert_eq!(
            adl(fns).read(),
            Some(NvmlReading {
                temp_c: Some(61.0),
                power_w: Some(120.0),
                clock_mhz: Some(2400),
                mem_clock_mhz: Some(1000),
            })
        );
    }

    #[test]
    fn amd_older_cards_use_overdrive() {
        let fns = AdlFns {
            pm_log: None,
            odn_temp: Some(odn_temp),
            od6_power: Some(od6_power),
        };
        let r = adl(fns).read().unwrap();
        assert_eq!((r.temp_c, r.power_w), (Some(65.0), Some(100.0)));
        assert_eq!(adl(NO_ADL).read(), None);
    }

    #[test]
    fn missing_sensor_is_not_shared() {
        let shares = Shares {
            temp: true,
            power: false,
        };
        assert_eq!(shares.missing(), vec!["Power"]);
        assert_eq!(Shares::default().missing(), vec!["Temp", "Power"]);
    }

    #[test]
    fn adapter_info_layout_and_rows() {
        assert_eq!(std::mem::size_of::<AdapterInfo>(), 1572);
        let rows = adapter_rows(info_rows, null_mut(), 4);
        assert_eq!(
            rows,
            vec![
                row(5, 3, "AMD Radeon RX 7800 XT"),
                row(7, 3, "AMD Radeon RX 7800 XT"),
                row(9, 12, "AMD Radeon(TM) Graphics"),
            ]
        );
        // The shown GPU is picked by name and the row's own index is used.
        assert_eq!(pick_adapter(&rows, "AMD Radeon RX 7800 XT"), Some(5));
        assert_eq!(pick_adapter(&rows, "AMD Radeon(TM) Graphics"), Some(9));
        // Two AMD GPUs and neither name matches: no guess.
        assert_eq!(pick_adapter(&rows, "Radeon"), None);
        assert!(adapter_rows(info_rows, null_mut(), 0).is_empty());
    }

    #[test]
    fn amd_pick_is_never_a_guess() {
        // The only AMD GPU, even when the names are spelled differently.
        let one = [
            row(0, 3, "AMD Radeon RX 6600"),
            row(1, 3, "AMD Radeon RX 6600"),
        ];
        assert_eq!(pick_adapter(&one, "Radeon RX 6600 (Microsoft)"), Some(0));
        // Two cards of the same model: unknown which one is shown.
        let twins = [
            row(0, 3, "AMD Radeon RX 6600"),
            row(1, 4, "AMD Radeon RX 6600"),
        ];
        assert_eq!(pick_adapter(&twins, "AMD Radeon RX 6600"), None);
        assert_eq!(pick_adapter(&[], "AMD Radeon RX 6600"), None);
    }

    #[test]
    fn adl_allocations_are_freed() {
        let p = adl_alloc(64);
        assert!(!p.is_null());
        let listed = |p: *mut c_void| {
            ADL_ALLOCS
                .lock()
                .unwrap()
                .iter()
                .any(|(a, _)| *a == p as usize)
        };
        assert!(listed(p));
        free_adl_allocs();
        assert!(!listed(p));
    }

    #[test]
    fn cache_reopens_only_when_the_gpu_changes() {
        let mut cache = VendorCache::default();
        let mut opens = 0;
        let mut open = |_: Vendor, _: &str| {
            opens += 1;
            Some(VendorGpu::Amd(adl(NO_ADL)))
        };
        assert!(cache.get_with(Vendor::Amd, "RX 6600", &mut open).is_some());
        assert!(cache.get_with(Vendor::Amd, "RX 6600", &mut open).is_some());
        assert!(cache
            .get_with(Vendor::Amd, "RX 7800 XT", &mut open)
            .is_some());
        assert!(cache
            .get_with(Vendor::Intel, "RX 7800 XT", &mut open)
            .is_some());
        assert_eq!(opens, 3);
        // A failed open is kept until the GPU changes, so it is not retried every second.
        let mut empty = VendorCache::default();
        assert!(empty.get_with(Vendor::Amd, "A", |_, _| None).is_none());
        assert!(empty
            .get_with(Vendor::Amd, "A", |_, _| panic!("reopened"))
            .is_none());
    }

    unsafe extern "C" fn intel_temp(h: *mut c_void, c: *mut f64) -> u32 {
        // SAFETY: valid out-parameter; the fake handles are 1 and 2.
        unsafe { *c = if h as usize == 1 { 50.0 } else { 70.0 } };
        CTL_OK
    }

    /// Every fake device has one sensor or domain, whose handle is the device's plus 100.
    unsafe extern "C" fn one_each(parent: *mut c_void, n: *mut u32, out: *mut *mut c_void) -> u32 {
        // SAFETY: valid out-parameters; `out` is null on the count call.
        unsafe {
            if !out.is_null() {
                *out = fake_handle(parent as usize + 100);
            }
            *n = 1;
        }
        CTL_OK
    }

    static ENERGY_CALLS: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn intel_energy(_: *mut c_void, e: *mut CtlEnergy) -> u32 {
        let n = ENERGY_CALLS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: valid out-parameter. 45 J per second is 45 W.
        unsafe {
            (*e).energy_uj = n * 45_000_000;
            (*e).timestamp_us = n * 1_000_000;
        }
        CTL_OK
    }

    #[test]
    fn intel_only_one_gpu_is_read() {
        let (temps, power) = pick_intel(&[fake_handle(1)], one_each, one_each).unwrap();
        assert_eq!(temps, vec![fake_handle::<c_void>(101)]);
        assert_eq!(power, Some(fake_handle(101)));
        assert!(pick_intel(&[fake_handle(1), fake_handle(2)], one_each, one_each).is_none());
        assert!(pick_intel(&[], one_each, one_each).is_none());
    }

    #[test]
    fn intel_hottest_sensor_and_power_from_energy() {
        let mut igcl = Igcl {
            _lib: Lib(None),
            api: null_mut(),
            close: None,
            fns: IgclFns {
                temp: intel_temp,
                energy: intel_energy,
            },
            temps: vec![fake_handle(1), fake_handle(2)],
            power: Some(fake_handle(3)),
            last: None,
            shares: Shares::default(),
        };
        let first = igcl.read().unwrap();
        assert_eq!((first.temp_c, first.power_w), (Some(70.0), None));
        let second = igcl.read().unwrap();
        assert_eq!(second.power_w, Some(45.0));
        assert_eq!((second.clock_mhz, second.mem_clock_mhz), (None, None));
    }

    #[test]
    #[ignore]
    fn live_read() {
        for v in [Vendor::Amd, Vendor::Intel] {
            let mut cache = VendorCache::default();
            let Some(g) = cache.get(v, "") else {
                println!("{v:?}: no library or no single GPU");
                continue;
            };
            println!("{v:?}: {:?}", g.shares());
            for _ in 0..2 {
                println!("{:?}", g.read());
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
}
