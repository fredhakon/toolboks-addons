//! Real `IdleSource`: seconds since the last keyboard or mouse input in this session (`GetLastInputInfo`).
use crate::state::IdleSource;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

pub struct WinIdle;

impl IdleSource for WinIdle {
    fn idle_secs(&self) -> Option<u64> {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        unsafe {
            if !GetLastInputInfo(&mut info).as_bool() {
                return None;
            }
            // Both are 32-bit tick counts, so wrapping_sub stays right across the 49-day wrap.
            Some(u64::from(GetTickCount().wrapping_sub(info.dwTime)) / 1000)
        }
    }
}
