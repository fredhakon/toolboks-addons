//! The answers on the page, the folders a project may not go into, and [`create`], the one
//! function the Create button runs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use toolboks_addon_templates::{write_project, Params, DEFAULT_ACCENT, PLACEHOLDER_DESCRIPTION};

/// The commit of `Gusseti/toolboks` a new project gets its SDK from. It must equal the `rev` of
/// the host crates in the workspace `Cargo.toml` (a test checks it), so a new project uses the
/// same SDK this add-on was built and tested with.
pub const HOST_REV: &str = "7c8e531f4ead178192c391729345aed5726a87ce";

/// What the author typed. The title and the id follow the name until the author changes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Form {
    pub name: String,
    pub title: String,
    pub author: String,
    pub id: String,
    pub accent: String,
    title_edited: bool,
    id_edited: bool,
}

impl Default for Form {
    fn default() -> Self {
        Form {
            name: String::new(),
            title: String::new(),
            author: String::new(),
            id: String::new(),
            accent: DEFAULT_ACCENT.into(),
            title_edited: false,
            id_edited: false,
        }
    }
}

impl Form {
    /// A form with `name` and `author` filled in, and the title and id suggested for the name.
    pub fn new(name: &str, author: &str) -> Form {
        let mut form = Form {
            name: name.into(),
            author: author.into(),
            ..Form::default()
        };
        form.name_changed();
        form
    }

    /// The name changed: suggest a title and an id for it, unless the author typed their own.
    pub fn name_changed(&mut self) {
        let suggested = Params::suggest(self.name.trim(), "");
        if !self.title_edited {
            self.title = suggested.title;
        }
        if !self.id_edited {
            self.id = suggested.id;
        }
    }

    /// The author typed a title. Cleared again, it follows the name.
    pub fn title_changed(&mut self) {
        self.title_edited = !self.title.trim().is_empty();
        if !self.title_edited {
            self.name_changed();
        }
    }

    /// The author typed an id. Cleared again, it follows the name.
    pub fn id_changed(&mut self) {
        self.id_edited = !self.id.trim().is_empty();
        if !self.id_edited {
            self.name_changed();
        }
    }

    /// The answers for the templates crate. The description is a placeholder the README tells
    /// the author to replace.
    pub fn params(&self) -> Params {
        Params {
            name: self.name.trim().into(),
            id: self.id.trim().into(),
            title: self.title.trim().into(),
            description: PLACEHOLDER_DESCRIPTION.into(),
            author: self.author.trim().into(),
            accent: self.accent.trim().into(),
            host_rev: HOST_REV.into(),
        }
    }

    /// The first thing to fix, in the templates crate's words, or `None` when every answer is
    /// good.
    pub fn problem(&self) -> Option<String> {
        self.params().validate().err()
    }
}

/// `#RRGGBB` as a colour, for the swatch.
pub fn accent_colour(hex: &str) -> Option<[u8; 3]> {
    let hex = hex.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

/// Why a project may not go into `folder`, or `None`. Refused: the top of a drive (or of a
/// network share), and the Windows and Program Files folders with everything in them. `var`
/// reads an environment variable (`std::env::var_os` outside tests).
pub fn folder_problem(folder: &Path, var: &dyn Fn(&str) -> Option<OsString>) -> Option<String> {
    let shown = folder.display();
    if folder.parent().is_none() {
        return Some(format!(
            "{shown} is the top of a drive. Choose a folder on it, for example a Projects folder in Documents."
        ));
    }
    let here = comparable(folder);
    let folders = |names: &[&str], fallback: &str| -> Vec<String> {
        let mut found: Vec<String> = names
            .iter()
            .filter_map(|name| var(name))
            .map(|path| comparable(Path::new(&path)))
            .filter(|path| !path.is_empty())
            .collect();
        found.push(comparable(Path::new(fallback)));
        found
    };
    let inside = |root: &String| here == *root || here.starts_with(&format!("{root}\\"));
    if folders(&["SystemRoot", "windir"], r"C:\Windows")
        .iter()
        .any(inside)
    {
        return Some(format!(
            "{shown} is in the Windows folder, which belongs to Windows. Choose a folder of your own, for example a Projects folder in Documents."
        ));
    }
    if folders(
        &["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"],
        r"C:\Program Files",
    )
    .iter()
    .chain(&folders(&[], r"C:\Program Files (x86)"))
    .any(inside)
    {
        return Some(format!(
            "{shown} is in Program Files, where installed programs live. Choose a folder of your own, for example a Projects folder in Documents."
        ));
    }
    None
}

/// A path for comparing folders: lowercase, backslashes, no slash at the end.
fn comparable(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// Writes a new project for `params` into a new folder `<folder>/<name>` and returns it. What
/// the Create button runs (on a worker thread). Refuses the folders [`folder_problem`] names, and
/// (in the templates crate) a project folder that exists and is not empty; if writing fails part
/// way, what this call created is removed again.
pub fn create(params: &Params, folder: &Path) -> Result<PathBuf, String> {
    if let Some(problem) = folder_problem(folder, &|name| std::env::var_os(name)) {
        return Err(problem);
    }
    write_project(params, folder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn form(name: &str, author: &str) -> Form {
        Form::new(name, author)
    }

    #[test]
    fn title_and_id_follow_the_name_until_edited() {
        let mut f = form("word-counter", "Ada");
        assert_eq!(f.title, "Word counter");
        assert_eq!(f.id, "local.word-counter");
        assert_eq!(f.accent, DEFAULT_ACCENT);
        assert_eq!(f.problem(), None);
        f.title = "Counter".into();
        f.title_changed();
        f.id = "com.ada.counter".into();
        f.id_changed();
        f.name = "counter".into();
        f.name_changed();
        assert_eq!(
            (f.title.as_str(), f.id.as_str()),
            ("Counter", "com.ada.counter")
        );
        // Cleared again, they follow the name.
        f.title.clear();
        f.title_changed();
        f.id.clear();
        f.id_changed();
        assert_eq!(
            (f.title.as_str(), f.id.as_str()),
            ("Counter", "local.counter")
        );
    }

    #[test]
    fn params_use_the_pinned_commit_and_a_placeholder_description() {
        let p = form("  counter ", " Ada ").params();
        assert_eq!(p.name, "counter");
        assert_eq!(p.author, "Ada");
        assert_eq!(p.host_rev, HOST_REV);
        assert_eq!(p.description, PLACEHOLDER_DESCRIPTION);
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn problems_are_the_templates_plain_words() {
        let problem = |change: &dyn Fn(&mut Form)| {
            let mut f = form("counter", "Ada");
            change(&mut f);
            f.problem().unwrap_or_default()
        };
        assert!(problem(&|f| f.name.clear()).starts_with("The name must be 2 to 40"));
        assert!(problem(&|f| f.name = "Counter".into()).starts_with("The name must"));
        assert!(problem(&|f| f.name = "1counter".into()).starts_with("The name must"));
        assert!(problem(&|f| f.id = "Bad Id".into()).starts_with("The id must"));
        assert!(problem(&|f| f.title = "Two\nlines".into()).starts_with("The title must"));
        assert!(problem(&|f| f.author.clear()).starts_with("The author must"));
        assert!(problem(&|f| f.accent = "green".into()).starts_with("The accent must"));
        assert!(problem(&|f| f.accent = "#12345".into()).starts_with("The accent must"));
        assert_eq!(problem(&|_| {}), "");
    }

    #[test]
    fn accent_swatch_reads_hex_colours() {
        assert_eq!(accent_colour("#3ecf8e"), Some([0x3e, 0xcf, 0x8e]));
        assert_eq!(accent_colour(" #A48ACA "), Some([0xa4, 0x8a, 0xca]));
        for bad in ["3ecf8e", "#3ecf8", "#3ecf8eff", "#zzzzzz", "", "#é1234"] {
            assert_eq!(accent_colour(bad), None, "{bad}");
        }
    }

    fn windows_vars(name: &str) -> Option<OsString> {
        match name {
            "SystemRoot" => Some(r"D:\WinOS".into()),
            "ProgramFiles" => Some(r"D:\Apps".into()),
            "ProgramFiles(x86)" => Some(r"D:\Apps (x86)".into()),
            _ => None,
        }
    }

    #[test]
    fn drive_roots_windows_and_program_files_are_refused() {
        let refused = |path: &str| folder_problem(Path::new(path), &windows_vars);
        for root in [r"C:\", "C:", r"D:\", r"\\server\share\"] {
            assert!(refused(root).unwrap().contains("top of a drive"), "{root}");
        }
        for windows in [
            r"D:\WinOS",
            r"d:\winos\System32",
            r"D:/WinOS/Temp/",
            r"C:\Windows",
            r"C:\WINDOWS\system32",
        ] {
            assert!(
                refused(windows).unwrap().contains("Windows folder"),
                "{windows}"
            );
        }
        for programs in [
            r"D:\Apps",
            r"D:\Apps\Tool",
            r"D:\Apps (x86)\Tool",
            r"C:\Program Files",
            r"C:\Program Files (x86)\x",
        ] {
            assert!(
                refused(programs).unwrap().contains("Program Files"),
                "{programs}"
            );
        }
        for fine in [
            r"C:\Users\ada\Documents\Projects",
            r"D:\WinOS-projects",
            r"D:\Applications",
            r"E:\code",
        ] {
            assert_eq!(refused(fine), None, "{fine}");
        }
    }

    #[test]
    fn create_writes_a_complete_project() {
        let dir = tempfile::tempdir().unwrap();
        let params = form("counter", "Ada").params();
        let project = create(&params, dir.path()).unwrap();
        assert_eq!(project, dir.path().join("counter"));
        for file in [
            "Cargo.toml",
            "counter.tbaddon",
            "icon.png",
            "src/main.rs",
            "tests/protocol.rs",
            "README.md",
            "AGENTS.md",
            "CLAUDE.md",
            ".cargo/config.toml",
            ".gitignore",
        ] {
            assert!(project.join(file).is_file(), "{file}");
        }
        let manifest = fs::read(project.join("counter.tbaddon")).unwrap();
        let package = toolboks_addon_sdk::Package::parse(&manifest, "0.21.0").unwrap();
        assert_eq!(package.manifest.id, "local.counter");
        assert_eq!(package.manifest.title, "Counter");
        assert_eq!(package.manifest.icon, "custom");
        let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
        assert!(cargo.contains(&format!("rev = \"{HOST_REV}\"")));
        let readme = fs::read_to_string(project.join("README.md")).unwrap();
        assert!(readme.contains("cargo test"));
    }

    #[test]
    fn create_never_overwrites_a_project_folder_with_files_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("counter");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("notes.txt"), b"mine").unwrap();
        let error = create(&form("counter", "Ada").params(), dir.path()).unwrap_err();
        assert!(error.contains("already exists and is not empty"), "{error}");
        let left: Vec<_> = fs::read_dir(&existing).unwrap().collect();
        assert_eq!(left.len(), 1);
        assert_eq!(fs::read(existing.join("notes.txt")).unwrap(), b"mine");
        // An empty folder of that name is used.
        let empty = tempfile::tempdir().unwrap();
        fs::create_dir(empty.path().join("counter")).unwrap();
        assert!(create(&form("counter", "Ada").params(), empty.path()).is_ok());
    }

    #[test]
    fn create_refuses_bad_answers_missing_and_forbidden_folders() {
        let dir = tempfile::tempdir().unwrap();
        let mut bad = form("counter", "Ada").params();
        bad.name = "Bad Name".into();
        assert!(create(&bad, dir.path())
            .unwrap_err()
            .starts_with("The name must"));
        let gone = dir.path().join("missing");
        assert!(create(&form("counter", "Ada").params(), &gone)
            .unwrap_err()
            .contains("does not exist"));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(create(&form("counter", "Ada").params(), Path::new(r"C:\"))
            .unwrap_err()
            .contains("top of a drive"));
    }

    /// The commit a new project gets its SDK from is the one this workspace is pinned to.
    #[test]
    fn host_rev_equals_the_workspace_pin() {
        let workspace = include_str!("../../../Cargo.toml");
        let pins: Vec<&str> = workspace
            .lines()
            .filter(|line| line.contains("git = \"https://github.com/Gusseti/toolboks\""))
            .collect();
        assert!(pins.len() >= 6, "{pins:?}");
        for line in pins {
            assert!(
                line.contains(&format!("rev = \"{HOST_REV}\"")),
                "HOST_REV in src/project.rs must equal the host pin in Cargo.toml: {line}"
            );
        }
    }
}
