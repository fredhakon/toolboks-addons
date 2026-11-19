//! One Testdeck per data folder in a Windows session, through a named mutex. The default data folder keeps the old
//! standalone name `Local\Testdeck`, so the old exe and the add-on never both run against the real data; any other
//! folder (`TOOLBOKS_DATA_DIR` smoke runs, a Preview host) gets its own name.
use std::path::Path;
use windows::core::HSTRING;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;

/// True when this process is the only Testdeck for `data_dir`. The mutex handle stays open until the process exits.
pub fn acquire(data_dir: &Path) -> bool {
    unsafe {
        match CreateMutexW(None, true, &HSTRING::from(mutex_name(data_dir))) {
            Ok(_handle) => GetLastError() != ERROR_ALREADY_EXISTS,
            // Without a mutex we cannot tell; starting beats refusing to start.
            Err(_) => true,
        }
    }
}

fn mutex_name(data_dir: &Path) -> String {
    // `data_root()` only moves away from the real folder through TOOLBOKS_DATA_DIR.
    if std::env::var_os("TOOLBOKS_DATA_DIR").is_none() {
        return "Local\\Testdeck".into();
    }
    // Backslashes are not allowed in a mutex name after the namespace; a hash keeps the name short.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data_dir.to_string_lossy().to_lowercase().hash(&mut h);
    format!("Local\\Testdeck-{:016x}", h.finish())
}
