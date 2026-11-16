//! Processes that Toolboks add-ons must never open for write access, trim, re-prioritise or
//! end: anti-cheat services, game launchers, well-known games and anything a launcher started.
//!
//! Touching these can get a player flagged by anti-cheat or make a game stutter. The tables are
//! a best-effort subset of well-known names, not a complete list of every game: an unlisted game
//! is still protected when a listed launcher started it, and Memory Cleaner's automatic runs
//! also skip any full-screen app (a runtime check the callers make themselves).

/// Anti-cheat services and helpers. Lowercase image names without `.exe`.
const ANTI_CHEAT: &[&str] = &[
    "easyanticheat",
    "easyanticheat_eos",
    "eaanticheat.gameservice",
    "beservice",
    "beservice_x64",
    "bedaisy",
    "vgc",
    "vgtray",
    "faceit",
    "faceitservice",
    "eseaclient",
    "gamemon.des",
    "gamemon64.des",
    "pnkbstra",
    "pnkbstrb",
];

/// Game launchers and their background services. Steam's VAC runs inside these.
const LAUNCHERS: &[&str] = &[
    "steam",
    "steamservice",
    "steamwebhelper",
    "epicgameslauncher",
    "epicwebhelper",
    "battle.net",
    "eadesktop",
    "eabackgroundservice",
    "origin",
    "upc",
    "ubisoftconnect",
    "uplaywebcore",
    "galaxyclient",
    "riotclientservices",
    "riotclientux",
    "riotclientuxrender",
    "rockstarservice",
    "socialclubhelper",
    "xboxpcapp",
    "gamingservices",
    "gamingservicesnet",
];

/// A modest list of very common game executables.
const GAMES: &[&str] = &[
    "cs2",
    "csgo",
    "dota2",
    "valorant",
    "valorant-win64-shipping",
    "league of legends",
    "fortnitelauncher",
    "fortniteclient-win64-shipping",
    "r5apex",
    "overwatch",
    "gta5",
    "rdr2",
    "robloxplayerbeta",
    "minecraft.windows",
    "tslgame",
    "rocketleague",
    "eldenring",
    "destiny2",
    "modernwarfare",
    "cod",
    "rainbowsix",
    "rainbowsix_be",
    "wow",
    "witcher3",
    "cyberpunk2077",
    "bf2042",
    "rustclient",
    "deadbydaylight-win64-shipping",
];

/// Why a process is left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protected {
    AntiCheat,
    GameLauncher,
    KnownGame,
    /// Started by a game launcher, so most likely a game.
    ChildOfLauncher,
    /// The program that started it has exited or cannot be checked, so it may have been a
    /// launcher.
    UnverifiedParent,
}

impl Protected {
    /// One plain-English sentence for the user.
    pub fn describe(self) -> &'static str {
        match self {
            Protected::AntiCheat => "Protected: anti-cheat process, Toolboks leaves it alone.",
            Protected::GameLauncher => "Protected: game launcher, Toolboks leaves it alone.",
            Protected::KnownGame => "Protected: game process, Toolboks leaves it alone.",
            Protected::ChildOfLauncher => {
                "Protected: started by a game launcher, Toolboks leaves it alone."
            }
            Protected::UnverifiedParent => {
                "Protected: the program that started it has closed or cannot be checked, \
                 Toolboks leaves it alone."
            }
        }
    }
}

/// `C:\Games\CS2.EXE` and `cs2` both become `cs2`.
fn normalize(image_name: &str) -> String {
    let file = image_name.rsplit(['\\', '/']).next().unwrap_or_default();
    let lower = file.trim().to_lowercase();
    match lower.strip_suffix(".exe") {
        Some(stem) => stem.to_string(),
        None => lower,
    }
}

/// The parent of a process, as far as a process snapshot can vouch for it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Parent {
    /// Image name of a parent that was running when the snapshot was taken and was created
    /// before the child, so it is the process that started the child.
    Known(String),
    /// No parent the snapshot can vouch for: it has exited, its pid now belongs to a younger
    /// process, or a creation time could not be read. Treated as possibly a game launcher.
    Unverified,
}

impl Parent {
    /// The parent to trust for a child. `child_created` is the child's creation time and
    /// `parent` the image name and creation time of the process the snapshot found at the
    /// child's parent pid (`None` when there was none). Creation times are FILETIME values read
    /// for the processes the snapshot saw, `None` when they could not be read.
    ///
    /// Windows keeps a child's parent pid after the parent exits and may reuse that pid, but a
    /// process that took over the pid was created after the real parent exited, so after the
    /// child. Only a parent created strictly before the child is known; anything else fails
    /// closed. Pure.
    pub fn vouch(child_created: Option<u64>, parent: Option<(&str, Option<u64>)>) -> Parent {
        match (child_created, parent) {
            (Some(child), Some((name, Some(created)))) if created < child => {
                Parent::Known(name.to_string())
            }
            _ => Parent::Unverified,
        }
    }

    /// Shorthand for a known parent (mostly for tests and fixtures).
    pub fn known(name: &str) -> Parent {
        Parent::Known(name.to_string())
    }

    /// The parent's image name when known.
    pub fn name(&self) -> Option<&str> {
        match self {
            Parent::Known(name) => Some(name),
            Parent::Unverified => None,
        }
    }
}

/// Why a process with this name must never be changed, whatever its parent, or `None` when the
/// name matches no table. For a first check before the parent is looked at; the full gate is
/// [`protected_reason`].
pub fn protected_name(image_name: &str) -> Option<Protected> {
    let name = normalize(image_name);
    let is = |list: &[&str]| list.contains(&name.as_str());
    if is(ANTI_CHEAT) {
        Some(Protected::AntiCheat)
    } else if is(LAUNCHERS) {
        Some(Protected::GameLauncher)
    } else if is(GAMES) {
        Some(Protected::KnownGame)
    } else {
        None
    }
}

/// Why this process must never be changed, or `None` if it matches no table and its parent is
/// known and not a listed launcher. `None` does not prove it is not a game: the tables are a
/// best-effort subset. Names may be bare (`steam`), image names (`Steam.exe`) or full paths;
/// case is ignored. A parent the snapshot could not vouch for protects the process.
pub fn protected_reason(image_name: &str, parent: &Parent) -> Option<Protected> {
    protected_name(image_name).or(match parent {
        Parent::Known(p) if LAUNCHERS.contains(&normalize(p).as_str()) => {
            Some(Protected::ChildOfLauncher)
        }
        Parent::Known(_) => None,
        Parent::Unverified => Some(Protected::UnverifiedParent),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anti_cheat_services() {
        for name in [
            "EasyAntiCheat.exe",
            "BEService.exe",
            "vgc.exe",
            "PnkBstrA.exe",
        ] {
            assert_eq!(protected_name(name), Some(Protected::AntiCheat), "{name}");
        }
        assert_eq!(protected_name("GameMon.des"), Some(Protected::AntiCheat));
    }

    #[test]
    fn launchers_and_games() {
        assert_eq!(protected_name("steam.exe"), Some(Protected::GameLauncher));
        assert_eq!(
            protected_name("Battle.net.exe"),
            Some(Protected::GameLauncher)
        );
        assert_eq!(
            protected_name("League of Legends.exe"),
            Some(Protected::KnownGame)
        );
        assert_eq!(protected_name("cs2.exe"), Some(Protected::KnownGame));
    }

    #[test]
    fn ignores_case_paths_and_extension() {
        for name in [
            "CS2.EXE",
            "cs2",
            r"C:\Program Files (x86)\Steam\steamapps\common\cs2\cs2.exe",
            "D:/Games/cs2.exe",
            " cs2.exe ",
        ] {
            assert_eq!(protected_name(name), Some(Protected::KnownGame), "{name}");
        }
    }

    #[test]
    fn child_of_launcher() {
        assert_eq!(
            protected_reason("SomeIndieGame.exe", &Parent::known(r"C:\Steam\Steam.exe")),
            Some(Protected::ChildOfLauncher)
        );
        // Its own category wins over the parent.
        assert_eq!(
            protected_reason("steamwebhelper.exe", &Parent::known("steam.exe")),
            Some(Protected::GameLauncher)
        );
        assert_eq!(
            protected_reason("notepad.exe", &Parent::known("explorer.exe")),
            None
        );
    }

    #[test]
    fn ordinary_apps_are_not_protected() {
        for name in [
            "chrome.exe",
            "firefox.exe",
            "Code.exe",
            "explorer.exe",
            "",
            "steam.exe.bak",
        ] {
            assert_eq!(protected_name(name), None, "{name}");
        }
    }

    #[test]
    fn every_description_says_protected() {
        for p in [
            Protected::AntiCheat,
            Protected::GameLauncher,
            Protected::KnownGame,
            Protected::ChildOfLauncher,
            Protected::UnverifiedParent,
        ] {
            assert!(p.describe().starts_with("Protected: "));
            assert!(p.describe().ends_with("Toolboks leaves it alone."));
        }
    }

    #[test]
    fn tables_are_lowercase_without_extension() {
        for n in ANTI_CHEAT.iter().chain(LAUNCHERS).chain(GAMES) {
            assert_eq!(*n, n.to_lowercase());
            assert!(!n.ends_with(".exe"), "{n}");
        }
    }

    // FILETIME values: the launcher started at 100, its game at 200.
    const LAUNCHER_STARTED: u64 = 100;
    const GAME_STARTED: u64 = 200;

    #[test]
    fn a_parent_created_before_the_child_is_known() {
        let parent = Parent::vouch(
            Some(GAME_STARTED),
            Some(("steam.exe", Some(LAUNCHER_STARTED))),
        );
        assert_eq!(parent, Parent::known("steam.exe"));
        assert_eq!(parent.name(), Some("steam.exe"));
        assert_eq!(
            protected_reason("IndieGame.exe", &parent),
            Some(Protected::ChildOfLauncher)
        );
        // A normal parent leaves an ordinary app unprotected.
        let explorer = Parent::vouch(Some(GAME_STARTED), Some(("explorer.exe", Some(1))));
        assert_eq!(protected_reason("notepad.exe", &explorer), None);
    }

    #[test]
    fn a_reused_parent_pid_is_not_trusted() {
        // Steam started the game and exited; notepad then got Steam's pid. Notepad is younger
        // than the game, so it cannot be the game's parent.
        let parent = Parent::vouch(Some(GAME_STARTED), Some(("notepad.exe", Some(300))));
        assert_eq!(parent, Parent::Unverified);
        assert_eq!(parent.name(), None);
        assert_eq!(
            protected_reason("IndieGame.exe", &parent),
            Some(Protected::UnverifiedParent)
        );
        // The same creation time cannot prove the order either.
        let same = Parent::vouch(
            Some(GAME_STARTED),
            Some(("notepad.exe", Some(GAME_STARTED))),
        );
        assert_eq!(same, Parent::Unverified);
    }

    #[test]
    fn an_exited_or_unreadable_parent_fails_closed() {
        // The launcher exited and nothing holds its pid.
        assert_eq!(Parent::vouch(Some(GAME_STARTED), None), Parent::Unverified);
        // The parent's creation time could not be read.
        assert_eq!(
            Parent::vouch(Some(GAME_STARTED), Some(("steam.exe", None))),
            Parent::Unverified
        );
        // The child's own creation time is unknown.
        assert_eq!(
            Parent::vouch(None, Some(("explorer.exe", Some(LAUNCHER_STARTED)))),
            Parent::Unverified
        );
        assert_eq!(
            protected_reason("notepad.exe", &Parent::Unverified),
            Some(Protected::UnverifiedParent)
        );
        // A listed name keeps its own reason.
        assert_eq!(
            protected_reason("cs2.exe", &Parent::Unverified),
            Some(Protected::KnownGame)
        );
    }
}
