//! The eight Mem Reduct clean regions as one opt-in action, "Clean selected regions" (issue #229).
//!
//! Nothing here runs unless the user presses the button and confirms in the administrator
//! window, and none of it is reachable from automatic mode (`scheduler.rs` never calls this
//! module). Every region is labelled "Unproven": `docs/RESEARCH-memory-cleaning.md` found no
//! measured benefit for trimming and the other regions were never measured.
//!
//! The numbers below were checked against the `ntapi` 0.4.3 crate (a copy of the phnt headers,
//! `ntexapi.rs`) on 2026-11-20, not by running them: `SystemMemoryListInformation` = 80,
//! `SystemFileCacheInformationEx` = 81, `SystemCombinePhysicalMemoryInformation` = 130,
//! `SystemRegistryReconciliationInformation` = 155, and the `SYSTEM_FILECACHE_INFORMATION` and
//! `MEMORY_COMBINE_INFORMATION_EX` field order. The ntdll exports are resolved at runtime, like
//! `memlist.rs`, and every failure is returned, never swallowed.

use std::ffi::c_void;

use windows::core::{s, w, PCWSTR};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, FlushFileBuffers,
    GetDriveTypeW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA,
    OPEN_EXISTING,
};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

use toolboks_shared::win::Handle;

use super::admin::failure_text;
use super::memlist::{self, Command, MemListError};

/// `GetDriveTypeW` result for a fixed disk (documented value 3; the crate has no constant).
const DRIVE_FIXED: u32 = 3;

/// The preference key the selection is saved under (a number: bit 0 is region 1).
const PREFERENCE_KEY: &str = "regions_mask";

/// Windows version as `RtlGetVersion` reports it (Windows 11 is still 10.0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WindowsVersion {
    pub major: u32,
    pub minor: u32,
}

const WINDOWS_8_1: WindowsVersion = WindowsVersion { major: 6, minor: 3 };
const WINDOWS_10: WindowsVersion = WindowsVersion {
    major: 10,
    minor: 0,
};

/// The running Windows version, or `None` if `RtlGetVersion` cannot be called.
pub fn windows_version() -> Option<WindowsVersion> {
    type RtlGetVersion = unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32;
    let export = memlist::ntdll_export(s!("RtlGetVersion"))?;
    // SAFETY: `RtlGetVersion` has exactly this signature.
    let rtl_get_version = unsafe {
        std::mem::transmute::<unsafe extern "system" fn() -> isize, RtlGetVersion>(export)
    };
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a valid out-parameter with its size field set.
    let status = unsafe { rtl_get_version(&mut info) };
    (status >= 0).then_some(WindowsVersion {
        major: info.dwMajorVersion,
        minor: info.dwMinorVersion,
    })
}

/// The regions, numbered 1 to 8 as in Mem Reduct (the order is the order they run in).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    WorkingSet,
    SystemFileCache,
    ModifiedFileCache,
    ModifiedPageList,
    StandbyList,
    LowPriorityStandby,
    RegistryCache,
    CombineLists,
}

pub const ALL: [Region; 8] = [
    Region::WorkingSet,
    Region::SystemFileCache,
    Region::ModifiedFileCache,
    Region::ModifiedPageList,
    Region::StandbyList,
    Region::LowPriorityStandby,
    Region::RegistryCache,
    Region::CombineLists,
];

/// `NtSetSystemInformation` classes that take a buffer of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Info {
    FileCache,
    RegistryReconciliation,
    CombinePhysicalMemory,
}

impl Info {
    fn class(self) -> u32 {
        match self {
            Info::FileCache => 81,
            Info::RegistryReconciliation => 155,
            Info::CombinePhysicalMemory => 130,
        }
    }

    /// The file cache call needs the quota privilege; the other two need the profile privilege,
    /// like the memory list commands. An administrator holds both; neither is ever added.
    fn privilege(self) -> PCWSTR {
        match self {
            Info::FileCache => w!("SeIncreaseQuotaPrivilege"),
            _ => w!("SeProfileSingleProcessPrivilege"),
        }
    }
}

/// What a region does to the system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    List(Command),
    Info(Info),
    FlushVolumes,
}

impl Region {
    fn action(self) -> Action {
        match self {
            Region::WorkingSet => Action::List(Command::EmptyWorkingSets),
            Region::SystemFileCache => Action::Info(Info::FileCache),
            Region::ModifiedFileCache => Action::FlushVolumes,
            Region::ModifiedPageList => Action::List(Command::FlushModifiedList),
            Region::StandbyList => Action::List(Command::PurgeStandbyList),
            Region::LowPriorityStandby => Action::List(Command::PurgeLowPriorityStandbyList),
            Region::RegistryCache => Action::Info(Info::RegistryReconciliation),
            Region::CombineLists => Action::Info(Info::CombinePhysicalMemory),
        }
    }

    fn bit(self) -> u8 {
        1 << (self as u8)
    }

    pub fn label(self) -> &'static str {
        match self {
            Region::WorkingSet => "Working set (all apps)",
            Region::SystemFileCache => "System file cache",
            Region::ModifiedFileCache => "Modified file cache",
            Region::ModifiedPageList => "Modified page list",
            Region::StandbyList => "Standby list (everything)",
            Region::LowPriorityStandby => "Standby list (lowest priority)",
            Region::RegistryCache => "Registry cache",
            Region::CombineLists => "Combine memory lists",
        }
    }

    /// One plain sentence on what the region is and what it does.
    pub fn note(self) -> &'static str {
        match self {
            Region::WorkingSet => "Asks Windows to move the memory every app is not using right now out of RAM. Apps can be slower for a moment when they need it back.",
            Region::SystemFileCache => "Shrinks the memory Windows itself uses to keep recently read files at hand. Windows refills it as files are used.",
            Region::ModifiedFileCache => "Writes file data that is still waiting in memory out to every fixed drive. Nothing is lost; it only moves the writing forward.",
            Region::ModifiedPageList => "Writes memory pages that changed but were not saved yet out to disk, so they can be reused.",
            Region::StandbyList => "Drops all the cached files Windows keeps in spare memory, however much it values them.",
            Region::LowPriorityStandby => "Drops only the cached files Windows values least. This is the same step as Clear low-priority cache above.",
            Region::RegistryCache => "Asks Windows to write its registry changes to disk and release the memory that held them.",
            Region::CombineLists => "Asks Windows to merge memory pages that hold identical data, so they share one copy.",
        }
    }

    /// The evidence sentence; every region reads "Unproven" until a benchmark says otherwise.
    pub fn evidence(self) -> &'static str {
        match self {
            Region::WorkingSet => "On one 32 GB PC a similar trim found no gain beyond the noise and apps were slower to read their memory back, so no benefit is shown.",
            Region::LowPriorityStandby => super::style::PURGE_EVIDENCE,
            _ => "Not measured yet, so no benefit is shown.",
        }
    }

    /// Mem Reduct documents that the modified and the full standby list can freeze the PC.
    pub fn freeze_warning(self) -> Option<&'static str> {
        match self {
            Region::ModifiedPageList => Some("Can make the PC stop responding for several seconds while Windows writes the pages to disk."),
            Region::StandbyList => Some("Can make the PC freeze or stutter for a moment while apps and games read their files from disk again."),
            _ => None,
        }
    }

    /// Why this region cannot run on `version`, as a clause that follows the region name.
    pub fn unavailable(self, version: Option<WindowsVersion>) -> Option<&'static str> {
        let (minimum, reason) = match self {
            Region::RegistryCache => (WINDOWS_8_1, "needs Windows 8.1 or later"),
            Region::CombineLists => (WINDOWS_10, "needs Windows 10 or later"),
            _ => return None,
        };
        match version {
            Some(v) if v >= minimum => None,
            Some(_) => Some(reason),
            None => Some("needs a Windows version Toolboks can read, and reading it failed"),
        }
    }
}

/// The regions the user ticked. Bit 0 is region 1 (working set), bit 7 is region 8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection(u8);

impl Default for Selection {
    /// Mem Reduct's own default: regions 1, 2, 3, 6, 7 and 8. Only a starting tick; nothing
    /// runs until the user presses the button.
    fn default() -> Self {
        Selection(
            [
                Region::WorkingSet,
                Region::SystemFileCache,
                Region::ModifiedFileCache,
                Region::LowPriorityStandby,
                Region::RegistryCache,
                Region::CombineLists,
            ]
            .iter()
            .fold(0, |bits, region| bits | region.bit()),
        )
    }
}

impl Selection {
    pub fn from_bits(bits: u8) -> Self {
        Selection(bits)
    }

    pub fn bits(self) -> u8 {
        self.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn contains(self, region: Region) -> bool {
        self.0 & region.bit() != 0
    }

    pub fn set(&mut self, region: Region, on: bool) {
        if on {
            self.0 |= region.bit();
        } else {
            self.0 &= !region.bit();
        }
    }

    /// The ticked regions, in run order.
    pub fn regions(self) -> impl Iterator<Item = Region> {
        ALL.into_iter().filter(move |r| self.contains(*r))
    }

    /// Only the regions that can run on `version`.
    pub fn available(self, version: Option<WindowsVersion>) -> Self {
        let mut kept = self;
        for region in self.regions().filter(|r| r.unavailable(version).is_some()) {
            kept.set(region, false);
        }
        kept
    }

    /// The saved selection, or the default if nothing valid is saved.
    pub fn load(preferences: &std::collections::BTreeMap<String, serde_json::Value>) -> Self {
        preferences
            .get(PREFERENCE_KEY)
            .and_then(serde_json::Value::as_u64)
            .and_then(|bits| u8::try_from(bits).ok())
            .map_or_else(Self::default, Selection)
    }

    /// Saves the selection; true if the preferences changed. The untouched default is not
    /// written, so an unused add-on stores nothing.
    pub fn store(
        self,
        preferences: &mut std::collections::BTreeMap<String, serde_json::Value>,
    ) -> bool {
        let value = serde_json::Value::from(self.0);
        let changed = match preferences.get(PREFERENCE_KEY) {
            Some(saved) => *saved != value,
            None => self != Self::default(),
        };
        if changed {
            preferences.insert(PREFERENCE_KEY.into(), value);
        }
        changed
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionError {
    /// A system call failed; the text comes from `admin::failure_text`.
    Memory(MemListError),
    /// The region cannot run on this Windows version (the clause from `Region::unavailable`).
    Unavailable(&'static str),
    /// Flushing the drives did not fully work. `code` is the last failing HRESULT, or 0 if no
    /// error was reported (no fixed drive was found).
    Volumes {
        flushed: u32,
        failed: u32,
        code: i32,
    },
}

impl RegionError {
    /// A clause that follows the region name.
    fn text(self) -> String {
        match self {
            RegionError::Memory(error) => failure_text(error),
            RegionError::Unavailable(reason) => reason.to_string(),
            RegionError::Volumes {
                flushed: 0,
                failed: 0,
                code: 0,
            } => "found no fixed drive to flush".to_string(),
            RegionError::Volumes {
                flushed: 0,
                failed: 0,
                code,
            } => {
                format!("could not list the drives: Windows error {code:#010x}")
            }
            RegionError::Volumes {
                flushed,
                failed,
                code,
            } => format!(
                "flushed {flushed} of {} drives: Windows error {code:#010x}",
                flushed + failed
            ),
        }
    }
}

pub struct Outcome {
    pub region: Region,
    pub result: Result<(), RegionError>,
}

/// The result line, and whether at least one region worked (so something changed).
pub fn summary(outcomes: &[Outcome]) -> (String, bool) {
    let done: Vec<&str> = outcomes
        .iter()
        .filter(|o| o.result.is_ok())
        .map(|o| o.region.label())
        .collect();
    let failed: Vec<String> = outcomes
        .iter()
        .filter_map(|o| Some(format!("{} {}", o.region.label(), o.result.err()?.text())))
        .collect();
    let text = match (done.is_empty(), failed.is_empty()) {
        (true, true) => "Nothing was selected.".to_string(),
        (false, true) => format!("Cleaned: {}.", done.join(", ")),
        (true, false) => format!("Nothing was cleaned. {}.", failed.join("; ")),
        (false, false) => format!(
            "Cleaned: {}. Not done: {}.",
            done.join(", "),
            failed.join("; ")
        ),
    };
    (text, !done.is_empty())
}

/// The system steps, so the order, the mapping and the error text are testable without
/// touching the system.
trait Backend {
    fn version(&self) -> Option<WindowsVersion>;
    fn list_command(&mut self, command: Command) -> Result<(), MemListError>;
    fn set_info(&mut self, info: Info) -> Result<(), MemListError>;
    fn flush_volumes(&mut self) -> Result<(), RegionError>;
}

/// Runs the ticked regions in order. A failing region never stops the ones after it.
fn run_selected(backend: &mut dyn Backend, selection: Selection) -> Vec<Outcome> {
    let version = backend.version();
    selection
        .regions()
        .map(|region| {
            let result = match region.unavailable(version) {
                Some(reason) => Err(RegionError::Unavailable(reason)),
                None => match region.action() {
                    Action::List(command) => {
                        backend.list_command(command).map_err(RegionError::Memory)
                    }
                    Action::Info(info) => backend.set_info(info).map_err(RegionError::Memory),
                    Action::FlushVolumes => backend.flush_volumes(),
                },
            };
            Outcome { region, result }
        })
        .collect()
}

/// Runs the ticked regions on this PC. Needs the administrator helper; only ever called after
/// the user confirmed. Never call it automatically.
pub fn clean(selection: Selection) -> Vec<Outcome> {
    run_selected(&mut RealBackend, selection)
}

/// `SYSTEM_FILECACHE_INFORMATION`: sizes are pointer-sized, counts are 32-bit.
#[repr(C)]
#[derive(Default)]
struct RawFileCache {
    current_size: usize,
    peak_size: usize,
    page_fault_count: u32,
    minimum_working_set: usize,
    maximum_working_set: usize,
    current_size_including_transition_in_pages: usize,
    peak_size_including_transition_in_pages: usize,
    transition_repurpose_count: u32,
    flags: u32,
}

/// `MEMORY_COMBINE_INFORMATION_EX`, sent zeroed (as Mem Reduct does).
#[repr(C)]
#[derive(Default)]
struct RawCombine {
    handle: usize,
    pages_combined: usize,
    flags: u32,
}

type NtSet = unsafe extern "system" fn(u32, *mut c_void, u32) -> i32;

struct RealBackend;

impl Backend for RealBackend {
    fn version(&self) -> Option<WindowsVersion> {
        windows_version()
    }

    fn list_command(&mut self, command: Command) -> Result<(), MemListError> {
        memlist::run(command)
    }

    fn set_info(&mut self, info: Info) -> Result<(), MemListError> {
        if !memlist::enable_privilege(info.privilege()) {
            return Err(MemListError::NotElevated);
        }
        let export =
            memlist::ntdll_export(s!("NtSetSystemInformation")).ok_or(MemListError::Unavailable)?;
        // SAFETY: `NtSetSystemInformation` has exactly this signature.
        let nt_set =
            unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, NtSet>(export) };
        let status = match info {
            Info::FileCache => {
                // Minimum and maximum of MAXSIZE_T ask Windows to trim the system cache's
                // working set as far as it can.
                let mut buffer = RawFileCache {
                    minimum_working_set: usize::MAX,
                    maximum_working_set: usize::MAX,
                    ..Default::default()
                };
                // SAFETY: `buffer` matches SYSTEM_FILECACHE_INFORMATION and its size is passed.
                unsafe {
                    nt_set(
                        info.class(),
                        &mut buffer as *mut RawFileCache as *mut c_void,
                        size_of::<RawFileCache>() as u32,
                    )
                }
            }
            // SAFETY: this class takes no buffer.
            Info::RegistryReconciliation => unsafe {
                nt_set(info.class(), std::ptr::null_mut(), 0)
            },
            Info::CombinePhysicalMemory => {
                let mut buffer = RawCombine::default();
                // SAFETY: `buffer` matches MEMORY_COMBINE_INFORMATION_EX and its size is passed.
                unsafe {
                    nt_set(
                        info.class(),
                        &mut buffer as *mut RawCombine as *mut c_void,
                        size_of::<RawCombine>() as u32,
                    )
                }
            }
        };
        memlist::classify(status)
    }

    fn flush_volumes(&mut self) -> Result<(), RegionError> {
        // `\\?\Volume{GUID}\` is 49 characters; 64 leaves room for the terminator.
        let mut name = [0u16; 64];
        // SAFETY: `name` is a valid buffer; the find handle is closed below.
        let find = unsafe { FindFirstVolumeW(&mut name) }.map_err(|e| RegionError::Volumes {
            flushed: 0,
            failed: 0,
            code: e.code().0,
        })?;
        let (mut flushed, mut failed, mut code) = (0u32, 0u32, 0i32);
        loop {
            match flush_volume(&mut name) {
                Some(Ok(())) => flushed += 1,
                Some(Err(error)) => {
                    failed += 1;
                    code = error;
                }
                None => {}
            }
            // SAFETY: `find` is the live handle from FindFirstVolumeW; `name` is a valid buffer.
            if unsafe { FindNextVolumeW(find, &mut name) }.is_err() {
                break;
            }
        }
        // SAFETY: `find` is closed exactly once, and not used afterwards.
        let _ = unsafe { FindVolumeClose(find) };
        if failed == 0 && flushed > 0 {
            Ok(())
        } else {
            Err(RegionError::Volumes {
                flushed,
                failed,
                code,
            })
        }
    }
}

/// Flushes one volume given as `\\?\Volume{GUID}\`. `None`: not a fixed drive, skipped (network,
/// removable and optical drives are left alone). `Some(Err(hresult))`: it could not be flushed.
fn flush_volume(name: &mut [u16; 64]) -> Option<Result<(), i32>> {
    let len = name.iter().position(|&c| c == 0)?;
    // SAFETY: `name` holds a NUL-terminated path.
    if unsafe { GetDriveTypeW(PCWSTR(name.as_ptr())) } != DRIVE_FIXED {
        return None;
    }
    // The volume device is only opened without the trailing backslash.
    if len > 0 && name[len - 1] == u16::from(b'\\') {
        name[len - 1] = 0;
    }
    // SAFETY: `name` holds a NUL-terminated path; FILE_WRITE_DATA is what a flush needs.
    let opened = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_WRITE_DATA.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    };
    let handle = match opened {
        Ok(handle) => Handle(handle),
        Err(error) => return Some(Err(error.code().0)),
    };
    // SAFETY: `handle` is an open volume handle, closed when it drops.
    Some(unsafe { FlushFileBuffers(handle.0) }.map_err(|e| e.code().0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const V7: WindowsVersion = WindowsVersion { major: 6, minor: 1 };
    const V8: WindowsVersion = WindowsVersion { major: 6, minor: 2 };
    const V11: WindowsVersion = WindowsVersion {
        major: 10,
        minor: 0,
    };

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        List(Command),
        Info(Info),
        Volumes,
    }

    struct Fake {
        version: Option<WindowsVersion>,
        calls: Vec<Call>,
        error: Option<MemListError>,
        volumes: Result<(), RegionError>,
    }

    fn fake(version: Option<WindowsVersion>) -> Fake {
        Fake {
            version,
            calls: Vec::new(),
            error: None,
            volumes: Ok(()),
        }
    }

    impl Backend for Fake {
        fn version(&self) -> Option<WindowsVersion> {
            self.version
        }
        fn list_command(&mut self, command: Command) -> Result<(), MemListError> {
            self.calls.push(Call::List(command));
            self.error.map_or(Ok(()), Err)
        }
        fn set_info(&mut self, info: Info) -> Result<(), MemListError> {
            self.calls.push(Call::Info(info));
            self.error.map_or(Ok(()), Err)
        }
        fn flush_volumes(&mut self) -> Result<(), RegionError> {
            self.calls.push(Call::Volumes);
            self.volumes
        }
    }

    #[test]
    fn regions_map_to_the_documented_commands() {
        let expected = [
            Action::List(Command::EmptyWorkingSets),
            Action::Info(Info::FileCache),
            Action::FlushVolumes,
            Action::List(Command::FlushModifiedList),
            Action::List(Command::PurgeStandbyList),
            Action::List(Command::PurgeLowPriorityStandbyList),
            Action::Info(Info::RegistryReconciliation),
            Action::Info(Info::CombinePhysicalMemory),
        ];
        for (region, action) in ALL.into_iter().zip(expected) {
            assert_eq!(region.action(), action, "{region:?}");
        }
        // Command values 2 to 5 are asserted in memlist.rs; these are the information classes.
        assert_eq!(Info::FileCache.class(), 81);
        assert_eq!(Info::CombinePhysicalMemory.class(), 130);
        assert_eq!(Info::RegistryReconciliation.class(), 155);
    }

    #[test]
    fn raw_layouts_match_the_headers() {
        // 5 pointer-sized fields and 2 + 2 32-bit ones; on 64-bit the first 32-bit field is
        // padded to 8 bytes: 8 + 8 + (4 + 4 pad) + 8 * 4 + (4 + 4) = 64.
        #[cfg(target_pointer_width = "64")]
        assert_eq!(size_of::<RawFileCache>(), 64);
        // HANDLE, ULONG_PTR, ULONG (padded).
        #[cfg(target_pointer_width = "64")]
        assert_eq!(size_of::<RawCombine>(), 24);
    }

    #[test]
    fn default_selection_is_mem_reducts_and_never_the_two_freezing_regions() {
        let selection = Selection::default();
        let ticked: Vec<Region> = selection.regions().collect();
        assert_eq!(
            ticked,
            vec![
                Region::WorkingSet,
                Region::SystemFileCache,
                Region::ModifiedFileCache,
                Region::LowPriorityStandby,
                Region::RegistryCache,
                Region::CombineLists,
            ]
        );
        assert_eq!(selection.bits(), 0b1110_0111);
        assert!(!selection.contains(Region::ModifiedPageList));
        assert!(!selection.contains(Region::StandbyList));
    }

    #[test]
    fn the_mask_round_trips_for_every_value() {
        for bits in 0..=u8::MAX {
            let selection = Selection::from_bits(bits);
            assert_eq!(selection.bits(), bits);
            let mut rebuilt = Selection::from_bits(0);
            for region in selection.regions() {
                rebuilt.set(region, true);
            }
            assert_eq!(rebuilt, selection);
        }
        let mut selection = Selection::from_bits(0);
        assert!(selection.is_empty());
        selection.set(Region::StandbyList, true);
        assert_eq!(selection.bits(), 0b0001_0000);
        selection.set(Region::StandbyList, false);
        assert!(selection.is_empty());
    }

    #[test]
    fn the_selection_is_saved_loaded_and_the_default_is_not_written() {
        let mut preferences = BTreeMap::new();
        assert_eq!(Selection::load(&preferences), Selection::default());
        assert!(!Selection::default().store(&mut preferences));
        assert!(preferences.is_empty());

        let mut picked = Selection::default();
        picked.set(Region::StandbyList, true);
        assert!(picked.store(&mut preferences));
        assert!(!picked.store(&mut preferences), "unchanged: no second save");
        assert_eq!(Selection::load(&preferences), picked);
        // Going back to the default writes it, so the saved value is replaced.
        assert!(Selection::default().store(&mut preferences));
        assert_eq!(Selection::load(&preferences), Selection::default());

        for bad in [
            serde_json::json!(300),
            serde_json::json!(-1),
            serde_json::json!("x"),
        ] {
            preferences.insert(PREFERENCE_KEY.into(), bad);
            assert_eq!(Selection::load(&preferences), Selection::default());
        }
    }

    #[test]
    fn registry_cache_needs_8_1_and_combine_needs_10() {
        assert!(Region::RegistryCache.unavailable(Some(V7)).is_some());
        assert!(Region::RegistryCache.unavailable(Some(V8)).is_some());
        assert!(Region::RegistryCache
            .unavailable(Some(WINDOWS_8_1))
            .is_none());
        assert!(Region::CombineLists
            .unavailable(Some(WINDOWS_8_1))
            .is_some());
        assert!(Region::CombineLists.unavailable(Some(WINDOWS_10)).is_none());
        assert!(Region::CombineLists.unavailable(Some(V11)).is_none());
        // Unknown version: the gated two stay off, the others are unaffected.
        assert!(Region::RegistryCache.unavailable(None).is_some());
        assert!(Region::CombineLists.unavailable(None).is_some());
        for region in [
            Region::WorkingSet,
            Region::SystemFileCache,
            Region::ModifiedFileCache,
            Region::ModifiedPageList,
            Region::StandbyList,
            Region::LowPriorityStandby,
        ] {
            assert!(region.unavailable(Some(V7)).is_none(), "{region:?}");
            assert!(region.unavailable(None).is_none(), "{region:?}");
        }
        assert!(Region::CombineLists
            .unavailable(Some(WINDOWS_8_1))
            .unwrap()
            .contains("Windows 10"));
    }

    #[test]
    fn available_drops_only_the_gated_regions() {
        let all = Selection::from_bits(0xFF);
        assert_eq!(all.available(Some(V11)), all);
        let on_8_1 = all.available(Some(WINDOWS_8_1));
        assert!(on_8_1.contains(Region::RegistryCache) && !on_8_1.contains(Region::CombineLists));
        let on_7 = all.available(Some(V7));
        assert_eq!(
            on_7.bits(),
            0xFF & !(Region::RegistryCache.bit() | Region::CombineLists.bit())
        );
    }

    #[test]
    fn the_default_run_sends_only_the_ticked_regions_in_order() {
        let mut backend = fake(Some(V11));
        let outcomes = run_selected(&mut backend, Selection::default());
        assert_eq!(
            backend.calls,
            vec![
                Call::List(Command::EmptyWorkingSets),
                Call::Info(Info::FileCache),
                Call::Volumes,
                Call::List(Command::PurgeLowPriorityStandbyList),
                Call::Info(Info::RegistryReconciliation),
                Call::Info(Info::CombinePhysicalMemory),
            ]
        );
        assert!(outcomes.iter().all(|o| o.result.is_ok()));
        assert_eq!(
            summary(&outcomes),
            (
                "Cleaned: Working set (all apps), System file cache, Modified file cache, \
                 Standby list (lowest priority), Registry cache, Combine memory lists."
                    .to_string(),
                true
            )
        );
    }

    #[test]
    fn nothing_selected_sends_nothing() {
        let mut backend = fake(Some(V11));
        let outcomes = run_selected(&mut backend, Selection::from_bits(0));
        assert!(backend.calls.is_empty());
        assert_eq!(
            summary(&outcomes),
            ("Nothing was selected.".to_string(), false)
        );
    }

    #[test]
    fn a_gated_region_sends_nothing_and_says_why() {
        let mut backend = fake(Some(V7));
        let outcomes = run_selected(&mut backend, Selection::from_bits(0xFF));
        assert!(!backend.calls.iter().any(|c| matches!(
            c,
            Call::Info(Info::RegistryReconciliation | Info::CombinePhysicalMemory)
        )));
        let (text, any_ok) = summary(&outcomes);
        assert!(any_ok);
        assert!(
            text.contains("Registry cache needs Windows 8.1 or later"),
            "{text}"
        );
        assert!(
            text.contains("Combine memory lists needs Windows 10 or later"),
            "{text}"
        );
    }

    #[test]
    fn one_failure_does_not_stop_the_rest_and_the_text_names_it() {
        let mut backend = fake(Some(V11));
        backend.volumes = Err(RegionError::Volumes {
            flushed: 1,
            failed: 1,
            code: 0x8007_0005_u32 as i32,
        });
        let outcomes = run_selected(&mut backend, Selection::default());
        assert_eq!(backend.calls.len(), 6, "every ticked region was tried");
        let (text, any_ok) = summary(&outcomes);
        assert!(any_ok);
        assert!(
            text.contains("Modified file cache flushed 1 of 2 drives: Windows error 0x80070005"),
            "{text}"
        );
        assert!(
            text.starts_with("Cleaned: Working set (all apps), System file cache, "),
            "{text}"
        );
    }

    #[test]
    fn all_failing_means_nothing_changed() {
        let mut backend = fake(Some(V11));
        backend.error = Some(MemListError::NotElevated);
        backend.volumes = Err(RegionError::Volumes {
            flushed: 0,
            failed: 0,
            code: 0,
        });
        let outcomes = run_selected(&mut backend, Selection::default());
        let (text, any_ok) = summary(&outcomes);
        assert!(!any_ok);
        assert!(text.starts_with("Nothing was cleaned. "), "{text}");
        assert!(
            text.contains("Working set (all apps) needs admin rights"),
            "{text}"
        );
        assert!(
            text.contains("Modified file cache found no fixed drive to flush"),
            "{text}"
        );
    }

    #[test]
    fn error_text_covers_every_failure() {
        assert_eq!(
            RegionError::Memory(MemListError::Unavailable).text(),
            "is not available on this version of Windows"
        );
        assert!(RegionError::Memory(MemListError::Status(-1073741823))
            .text()
            .contains("0xc0000001"));
        assert!(RegionError::Volumes {
            flushed: 0,
            failed: 0,
            code: 0x8007_0003_u32 as i32
        }
        .text()
        .contains("could not list the drives: Windows error 0x80070003"));
        assert_eq!(
            RegionError::Unavailable("needs Windows 10 or later").text(),
            "needs Windows 10 or later"
        );
    }

    #[test]
    fn every_region_has_a_note_an_unproven_evidence_sentence_and_a_unique_label() {
        let mut labels: Vec<&str> = ALL.iter().map(|r| r.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 8);
        for region in ALL {
            assert!(region.note().ends_with('.'), "{region:?}");
            assert!(
                region.evidence().contains("no benefit is shown"),
                "{region:?}"
            );
            for word in ["faster", "frees", "boost", "improves"] {
                assert!(!region.note().contains(word) && !region.evidence().contains(word));
            }
        }
    }

    #[test]
    fn only_the_modified_list_and_the_full_standby_list_carry_a_freeze_warning() {
        for region in ALL {
            let warned = matches!(region, Region::ModifiedPageList | Region::StandbyList);
            assert_eq!(region.freeze_warning().is_some(), warned, "{region:?}");
        }
    }
}
