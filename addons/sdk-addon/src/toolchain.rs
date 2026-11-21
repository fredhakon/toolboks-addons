//! Is Rust installed, and are the Visual C++ build tools? Checked on a worker thread: the
//! programs it starts can take a moment, and the page must keep answering Toolboks.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

/// The result of one check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Checking,
    /// Installed; the detail says which (a version or a product name).
    Found(String),
    Missing,
    /// It could not be checked; the reason in plain words.
    Unknown(String),
}

/// Both checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toolchain {
    pub rust: Status,
    pub build_tools: Status,
}

impl Toolchain {
    pub const CHECKING: Toolchain = Toolchain {
        rust: Status::Checking,
        build_tools: Status::Checking,
    };
}

/// Starts both checks on a worker thread. The answer arrives on the receiver, and the page is
/// asked to draw again.
pub fn start(ctx: egui::Context) -> mpsc::Receiver<Toolchain> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let var = |name: &str| std::env::var_os(name);
        let found = Toolchain {
            rust: rust(&var),
            build_tools: build_tools(&var),
        };
        let _ = tx.send(found);
        ctx.request_repaint();
    });
    rx
}

/// A console program run without a console window (the add-on has none), from the temporary
/// folder so no project's settings there change what it does.
fn quiet(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.current_dir(std::env::temp_dir());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Where cargo may be: on the PATH, or in rustup's own folder (the PATH Toolboks started with may
/// predate a fresh Rust install).
fn cargo_candidates(var: &dyn Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from("cargo")];
    if let Some(home) = var("CARGO_HOME") {
        candidates.push(Path::new(&home).join("bin").join("cargo.exe"));
    }
    if let Some(profile) = var("USERPROFILE") {
        candidates.push(
            Path::new(&profile)
                .join(".cargo")
                .join("bin")
                .join("cargo.exe"),
        );
    }
    candidates
}

fn rust(var: &dyn Fn(&str) -> Option<OsString>) -> Status {
    for cargo in cargo_candidates(var) {
        if let Ok(out) = quiet(&cargo).arg("--version").output() {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                return Status::Found(first_line(&text).unwrap_or("cargo").to_string());
            }
        }
    }
    Status::Missing
}

/// The Visual Studio Installer's own locator. It comes with every Visual Studio and Build Tools
/// install since 2017.
fn vswhere(var: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let base = var("ProgramFiles(x86)").unwrap_or_else(|| r"C:\Program Files (x86)".into());
    Some(
        Path::new(&base)
            .join("Microsoft Visual Studio")
            .join("Installer")
            .join("vswhere.exe"),
    )
    .filter(|path| path.is_file())
}

fn build_tools(var: &dyn Fn(&str) -> Option<OsString>) -> Status {
    let Some(vswhere) = vswhere(var) else {
        return Status::Unknown(
            "The Visual Studio Installer is not on this PC, so this could not be checked.".into(),
        );
    };
    let out = quiet(&vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "displayName",
            "-utf8",
        ])
        .output();
    build_tools_answer(out.ok().map(|out| (out.status.success(), out.stdout)))
}

/// What vswhere's answer means: (it ran fine, what it printed).
fn build_tools_answer(answer: Option<(bool, Vec<u8>)>) -> Status {
    match answer {
        Some((true, stdout)) => match first_line(&String::from_utf8_lossy(&stdout)) {
            Some(product) => Status::Found(product.to_string()),
            None => Status::Missing,
        },
        _ => Status::Unknown(
            "The Visual Studio Installer did not answer, so this could not be checked.".into(),
        ),
    }
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_is_looked_for_on_the_path_then_in_rustups_folder() {
        let var = |name: &str| match name {
            "CARGO_HOME" => Some(OsString::from(r"D:\rust\cargo")),
            "USERPROFILE" => Some(OsString::from(r"C:\Users\ada")),
            _ => None,
        };
        assert_eq!(
            cargo_candidates(&var),
            [
                PathBuf::from("cargo"),
                PathBuf::from(r"D:\rust\cargo\bin\cargo.exe"),
                PathBuf::from(r"C:\Users\ada\.cargo\bin\cargo.exe"),
            ]
        );
        assert_eq!(cargo_candidates(&|_| None), [PathBuf::from("cargo")]);
    }

    #[test]
    fn vswhere_answers_are_read_plainly() {
        assert_eq!(
            build_tools_answer(Some((
                true,
                b"\r\nVisual Studio Build Tools 2022\r\n".to_vec()
            ))),
            Status::Found("Visual Studio Build Tools 2022".into())
        );
        assert_eq!(
            build_tools_answer(Some((true, b"  \r\n".to_vec()))),
            Status::Missing
        );
        assert!(matches!(
            build_tools_answer(Some((false, Vec::new()))),
            Status::Unknown(_)
        ));
        assert!(matches!(build_tools_answer(None), Status::Unknown(_)));
    }

    #[test]
    fn without_the_installer_the_build_tools_could_not_be_checked() {
        let var = |name: &str| (name == "ProgramFiles(x86)").then(|| OsString::from(r"Z:\nowhere"));
        assert!(matches!(build_tools(&var), Status::Unknown(_)));
    }
}
