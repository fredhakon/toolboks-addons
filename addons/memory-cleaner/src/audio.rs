//! Which apps are playing sound, read from the Windows audio sessions (read-only).
//!
//! An app counts as playing when an audio session on an active output device is in the Active
//! state and is owned by the app, by a process the app started, by a process that started the
//! app, or by another process of the same app (same image name under the same app root, such
//! as a browser's audio process next to its tabs). Sessions are read once per run on the
//! sampling thread, never on the UI thread. When they cannot be read, every app counts as
//! playing, so the run leaves every app alone (fails closed).
//!
//! The reading is behind [`SessionSource`]; [`WindowsSessions`] is the real one and the tests
//! use fakes. [`AudioState::playing`] is pure.

use std::collections::{HashMap, HashSet};

use protected_processes::Parent;

use super::stats::ProcInfo;

/// One audio session as Windows reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session {
    /// The process that owns the session; 0 for the system sounds session.
    pub pid: u32,
    /// True when the session's state is Active (a stream is running).
    pub active: bool,
}

/// The audio sessions could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionsUnavailable;

/// Where the sessions come from.
pub trait SessionSource {
    /// Every session on every active output device, or an error when any of them could not be
    /// read.
    fn sessions(&self) -> Result<Vec<Session>, SessionsUnavailable>;
}

/// The audio sessions of one run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioState {
    /// Pids that own an Active session.
    Read(HashSet<u32>),
    /// Not read (yet) or could not be read: every app may be playing.
    Unknown,
}

impl AudioState {
    /// Reads the sessions once. Any failure gives [`AudioState::Unknown`].
    pub fn read(source: &dyn SessionSource) -> Self {
        match source.sessions() {
            Ok(sessions) => AudioState::Read(
                sessions
                    .into_iter()
                    .filter(|s| s.active && s.pid != 0)
                    .map(|s| s.pid)
                    .collect(),
            ),
            Err(SessionsUnavailable) => AudioState::Unknown,
        }
    }

    pub fn is_unknown(&self) -> bool {
        *self == AudioState::Unknown
    }

    /// Pids in `procs` that count as playing sound. With [`AudioState::Unknown`] that is every
    /// pid. Only parents the snapshot vouched for link processes together. Pure.
    pub fn playing(&self, procs: &[ProcInfo]) -> HashSet<u32> {
        let owners = match self {
            AudioState::Unknown => return procs.iter().map(|p| p.pid).collect(),
            AudioState::Read(owners) => owners,
        };
        let by_pid: HashMap<u32, &ProcInfo> = procs.iter().map(|p| (p.pid, p)).collect();
        // A parent counts only when the snapshot vouched for it (its name is known).
        let parent_of = |p: &ProcInfo| -> Option<&ProcInfo> {
            match (&p.parent, p.parent_pid) {
                (Parent::Known(_), Some(ppid)) if ppid != p.pid => by_pid.get(&ppid).copied(),
                _ => None,
            }
        };
        // Every process above `p`, nearest first. Bounded, so a loop in the data cannot hang.
        let ancestors = |p: &ProcInfo| -> Vec<u32> {
            let mut chain = Vec::new();
            let mut at = p;
            while let Some(up) = parent_of(at) {
                if chain.contains(&up.pid) || chain.len() >= 64 {
                    break;
                }
                chain.push(up.pid);
                at = up;
            }
            chain
        };
        // The top of a run of same-named processes: a browser's main process for its tabs.
        let app_root = |p: &ProcInfo| -> u32 {
            let mut at = p;
            let mut steps = 0;
            while let Some(up) = parent_of(at) {
                if !up.name.eq_ignore_ascii_case(&p.name) || steps >= 64 {
                    break;
                }
                at = up;
                steps += 1;
            }
            at.pid
        };

        let mut playing: HashSet<u32> = owners.clone();
        let mut roots = HashSet::new();
        for owner in owners.iter().filter_map(|pid| by_pid.get(pid)) {
            // The apps that started the owner.
            playing.extend(ancestors(owner));
            roots.insert(app_root(owner));
        }
        for p in procs {
            if roots.contains(&app_root(p)) || ancestors(p).iter().any(|a| owners.contains(a)) {
                playing.insert(p.pid);
            }
        }
        playing
    }
}

/// The real audio sessions, through the Windows Core Audio APIs. Only reads.
pub struct WindowsSessions;

impl SessionSource for WindowsSessions {
    fn sessions(&self) -> Result<Vec<Session>, SessionsUnavailable> {
        windows_sessions().map_err(|_| SessionsUnavailable)
    }
}

fn windows_sessions() -> windows::core::Result<Vec<Session>> {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
        IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    /// Balances a successful `CoInitializeEx` on this thread.
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            // SAFETY: only created after CoInitializeEx succeeded on this thread.
            unsafe { CoUninitialize() };
        }
    }

    // SAFETY: plain COM initialisation for this thread; balanced by `Com` when it succeeds. A
    // thread that already uses another apartment model fails here and the whole read fails.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let _com = Com;

    let mut sessions = Vec::new();
    // SAFETY: COM is initialised on this thread; every call below only reads, and every
    // interface is released when it goes out of scope.
    unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let outputs = devices.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..outputs.GetCount()? {
            let device = outputs.Item(i)?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
            let list = manager.GetSessionEnumerator()?;
            for j in 0..list.GetCount()? {
                let control = list.GetSession(j)?.cast::<IAudioSessionControl2>()?;
                sessions.push(Session {
                    pid: control.GetProcessId()?,
                    active: control.GetState()? == AudioSessionStateActive,
                });
            }
        }
    }
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Result<Vec<Session>, SessionsUnavailable>);

    impl SessionSource for Fake {
        fn sessions(&self) -> Result<Vec<Session>, SessionsUnavailable> {
            self.0.clone()
        }
    }

    fn session(pid: u32, active: bool) -> Session {
        Session { pid, active }
    }

    /// A process with a parent the snapshot vouched for (`parent` = its pid and name).
    fn proc(pid: u32, name: &str, parent: Option<(u32, &str)>) -> ProcInfo {
        ProcInfo {
            pid,
            name: name.to_string(),
            working_set: 0,
            private: 0,
            count: 1,
            parent: parent.map_or(Parent::Unverified, |(_, n)| Parent::known(n)),
            parent_pid: parent.map(|(p, _)| p),
            created: Some(u64::from(pid)),
        }
    }

    fn procs() -> Vec<ProcInfo> {
        vec![
            proc(1, "explorer.exe", None),
            proc(10, "spotify.exe", Some((1, "explorer.exe"))),
            proc(11, "spotify.exe", Some((10, "spotify.exe"))),
            proc(12, "helper.exe", Some((10, "spotify.exe"))),
            proc(20, "chrome.exe", Some((1, "explorer.exe"))),
            proc(21, "chrome.exe", Some((20, "chrome.exe"))), // audio process
            proc(22, "chrome.exe", Some((20, "chrome.exe"))), // a tab
            proc(30, "notepad.exe", Some((1, "explorer.exe"))),
        ]
    }

    fn playing(sessions: Vec<Session>) -> Vec<u32> {
        let mut v: Vec<u32> = AudioState::read(&Fake(Ok(sessions)))
            .playing(&procs())
            .into_iter()
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn an_active_session_marks_its_app_as_playing() {
        let p = playing(vec![session(30, true)]);
        // Notepad plays; explorer started it, so it is left alone too. Nothing else.
        assert_eq!(p, vec![1, 30]);
    }

    #[test]
    fn an_inactive_session_does_not_count() {
        assert!(playing(vec![session(30, false), session(10, false)]).is_empty());
        // The system sounds session (pid 0) does not count either.
        assert!(playing(vec![session(0, true)]).is_empty());
    }

    #[test]
    fn a_failed_read_means_every_app_may_be_playing() {
        let state = AudioState::read(&Fake(Err(SessionsUnavailable)));
        assert!(state.is_unknown());
        let all: HashSet<u32> = procs().iter().map(|p| p.pid).collect();
        assert_eq!(state.playing(&procs()), all);
    }

    #[test]
    fn children_of_an_audio_app_count_as_playing() {
        // Spotify's main process plays: its own child and its helper are left alone.
        assert_eq!(playing(vec![session(10, true)]), vec![1, 10, 11, 12]);
    }

    #[test]
    fn other_processes_of_the_same_app_count_as_playing() {
        // Chrome's audio process plays: the main process and its tabs are left alone, while
        // notepad and spotify are not.
        assert_eq!(playing(vec![session(21, true)]), vec![1, 20, 21, 22]);
    }

    #[test]
    fn an_unverified_parent_does_not_link_processes() {
        // The tab's parent could not be vouched for (exited or reused pid): it is not tied to
        // the audio process through that parent.
        let mut list = procs();
        list[6].parent = Parent::Unverified;
        let p: HashSet<u32> = AudioState::read(&Fake(Ok(vec![session(21, true)]))).playing(&list);
        assert!(!p.contains(&22));
        assert!(p.contains(&20) && p.contains(&21));
    }
}
