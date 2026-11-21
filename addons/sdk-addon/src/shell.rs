//! Windows' own folder picker, and opening the README or the project folder. Each runs on a
//! worker thread: the picker waits for the person, and Toolboks ends an add-on that stops
//! answering for 30 seconds.

use std::path::PathBuf;
use std::sync::mpsc;

/// What the folder picker gave: a folder, nothing (cancelled), or why it could not open.
pub type Picked = Result<Option<PathBuf>, String>;

/// Opens the folder picker, starting at `initial` when it is set.
pub fn pick_folder(initial: Option<PathBuf>, ctx: egui::Context) -> mpsc::Receiver<Picked> {
    let (tx, rx) = mpsc::channel();
    #[cfg(windows)]
    let owner =
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() }.0 as isize;
    #[cfg(not(windows))]
    let owner = 0isize;
    std::thread::spawn(move || {
        let _ = tx.send(choose(initial, owner));
        ctx.request_repaint();
    });
    rx
}

#[cfg(windows)]
fn choose(initial: Option<PathBuf>, owner: isize) -> Picked {
    use windows::{
        core::{w, HRESULT, PCWSTR},
        Win32::{
            Foundation::HWND,
            System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER},
            UI::Shell::{
                FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName,
                FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS,
                SIGDN_FILESYSPATH,
            },
        },
    };
    let _apartment =
        Apartment::enter().map_err(|e| format!("Could not open the folder picker: {e}"))?;
    // The dialog lives on this thread only. The path is copied before its memory is freed.
    let result = (|| -> windows::core::Result<Option<PathBuf>> {
        unsafe {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            dialog.SetOptions(
                dialog.GetOptions()?
                    | FOS_PICKFOLDERS
                    | FOS_FORCEFILESYSTEM
                    | FOS_PATHMUSTEXIST
                    | FOS_NOCHANGEDIR,
            )?;
            dialog.SetTitle(w!("Choose where to create the project"))?;
            if let Some(initial) = initial {
                let path = wide(initial.as_os_str());
                let item: windows::core::Result<IShellItem> =
                    SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None);
                if let Ok(item) = item {
                    let _ = dialog.SetFolder(&item);
                }
            }
            let parent = (owner != 0).then_some(HWND(owner as *mut _));
            match dialog.Show(parent) {
                Ok(()) => {}
                // ERROR_CANCELLED: the person closed the dialog.
                Err(e) if e.code() == HRESULT::from_win32(1223) => return Ok(None),
                Err(e) => return Err(e),
            }
            let raw = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
            let path = raw.to_string();
            CoTaskMemFree(Some(raw.0.cast()));
            Ok(Some(PathBuf::from(path?)))
        }
    })();
    result.map_err(|e| format!("Could not open the folder picker: {e}"))
}

#[cfg(not(windows))]
fn choose(_initial: Option<PathBuf>, _owner: isize) -> Picked {
    Err("The folder picker needs Windows.".into())
}

/// `text` as a NUL-terminated UTF-16 string for Windows.
#[cfg(windows)]
fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().chain(Some(0)).collect()
}

/// COM for this thread, ended when dropped.
#[cfg(windows)]
struct Apartment;

#[cfg(windows)]
impl Apartment {
    fn enter() -> windows::core::Result<Apartment> {
        use windows::Win32::System::Com::{
            CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
        };
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok()? };
        Ok(Apartment)
    }
}

#[cfg(windows)]
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { windows::Win32::System::Com::CoUninitialize() }
    }
}

/// Opens `readme` with the program Windows uses for `.md` files, or in Notepad when there is
/// none. The answer arrives on the receiver.
pub fn open_readme(readme: PathBuf, ctx: egui::Context) -> mpsc::Receiver<Result<(), String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let opened = shell_open(&readme).or_else(|_| {
            std::process::Command::new("notepad.exe")
                .arg(&readme)
                .spawn()
                .map(drop)
                .map_err(|e| format!("Could not open {}: {e}", readme.display()))
        });
        let _ = tx.send(opened);
        ctx.request_repaint();
    });
    rx
}

/// Shows `folder` in File Explorer.
pub fn open_folder(folder: &std::path::Path) -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .arg(folder)
        .spawn()
        .map(drop)
        .map_err(|e| format!("Could not open File Explorer: {e}"))
}

#[cfg(windows)]
fn shell_open(file: &std::path::Path) -> Result<(), String> {
    use windows::{
        core::{w, PCWSTR},
        Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    };
    let _apartment = Apartment::enter().map_err(|e| e.to_string())?;
    let path = wide(file.as_os_str());
    // SAFETY: a NUL-terminated path and fixed verb; no arguments are passed.
    let code = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Above 32 is success; anything else (such as no program for .md files) is an error code.
    if (code.0 as isize) > 32 {
        Ok(())
    } else {
        Err(format!("Windows has no program to open {}", file.display()))
    }
}

#[cfg(not(windows))]
fn shell_open(file: &std::path::Path) -> Result<(), String> {
    Err(format!("Cannot open {} here", file.display()))
}
