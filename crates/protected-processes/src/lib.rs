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

/// Why this process must never be changed, or `None` if it matches no table and its parent is
/// not a listed launcher. `None` does not prove it is not a game: the tables are a best-effort
/// subset. Names may be bare (`steam`), image names (`Steam.exe`) or full paths; case is ignored.
pub fn protected_reason(image_name: &str, parent_name: Option<&str>) -> Option<Protected> {
    let name = normalize(image_name);
    let is = |list: &[&str], n: &str| list.contains(&n);
    if is(ANTI_CHEAT, &name) {
        Some(Protected::AntiCheat)
    } else if is(LAUNCHERS, &name) {
        Some(Protected::GameLauncher)
    } else if is(GAMES, &name) {
        Some(Protected::KnownGame)
    } else if parent_name.is_some_and(|p| is(LAUNCHERS, &normalize(p))) {
        Some(Protected::ChildOfLauncher)
    } else {
        None
    }
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
            assert_eq!(
                protected_reason(name, None),
                Some(Protected::AntiCheat),
                "{name}"
            );
        }
        assert_eq!(
            protected_reason("GameMon.des", None),
            Some(Protected::AntiCheat)
        );
    }

    #[test]
    fn launchers_and_games() {
        assert_eq!(
            protected_reason("steam.exe", None),
            Some(Protected::GameLauncher)
        );
        assert_eq!(
            protected_reason("Battle.net.exe", None),
            Some(Protected::GameLauncher)
        );
        assert_eq!(
            protected_reason("League of Legends.exe", None),
            Some(Protected::KnownGame)
        );
        assert_eq!(
            protected_reason("cs2.exe", None),
            Some(Protected::KnownGame)
        );
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
            assert_eq!(
                protected_reason(name, None),
                Some(Protected::KnownGame),
                "{name}"
            );
        }
    }

    #[test]
    fn child_of_launcher() {
        assert_eq!(
            protected_reason("SomeIndieGame.exe", Some(r"C:\Steam\Steam.exe")),
            Some(Protected::ChildOfLauncher)
        );
        // Its own category wins over the parent.
        assert_eq!(
            protected_reason("steamwebhelper.exe", Some("steam.exe")),
            Some(Protected::GameLauncher)
        );
        assert_eq!(protected_reason("notepad.exe", Some("explorer.exe")), None);
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
            assert_eq!(protected_reason(name, None), None, "{name}");
        }
    }

    #[test]
    fn every_description_says_protected() {
        for p in [
            Protected::AntiCheat,
            Protected::GameLauncher,
            Protected::KnownGame,
            Protected::ChildOfLauncher,
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
}
