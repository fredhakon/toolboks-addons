//! Windows toasts through WinRT (`ToastNotificationManager`), shown from one background thread so no caller
//! blocks. Testdeck is an unpackaged exe, so it registers its AppUserModelID under
//! `HKCU\Software\Classes\AppUserModelId\<id>` (display name only) before the first toast; Windows drops toasts
//! from an unregistered id. Toasts are plain: buttons in a toast cannot call back into an unpackaged exe without a
//! COM activator and a Start menu shortcut, so questions (the gate ask) are answered in the window or tray menu.
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;
use windows::core::{w, HSTRING};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

pub const AUMID: &str = "Testdeck.Broker";

static TX: OnceLock<Sender<(String, String)>> = OnceLock::new();

/// Queues a toast and returns at once. The outcome goes to stderr (the `--headless` log).
pub fn toast(title: &str, body: &str) {
    let tx = TX.get_or_init(|| {
        let (tx, rx) = channel::<(String, String)>();
        std::thread::spawn(move || {
            // WinRT needs an initialized apartment on this thread.
            let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
            if let Err(e) = register() {
                eprintln!("testdeck: cannot register the toast app id: {e}");
            }
            for (title, body) in rx {
                match show(&title, &body) {
                    Ok(()) => eprintln!("testdeck: toast shown: {title}"),
                    Err(e) => eprintln!("testdeck: toast failed: {title}: {e}"),
                }
            }
        });
        tx
    });
    let _ = tx.send((title.to_string(), body.to_string()));
}

/// The display name Windows shows on the toast, under HKCU only.
fn register() -> windows::core::Result<()> {
    let key = HSTRING::from(format!("Software\\Classes\\AppUserModelId\\{AUMID}"));
    let value: Vec<u16> = "Testdeck".encode_utf16().chain([0]).collect();
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &key,
            w!("DisplayName"),
            REG_SZ.0,
            Some(value.as_ptr().cast()),
            (value.len() * 2) as u32,
        )
        .ok()
    }
}

fn show(title: &str, body: &str) -> windows::core::Result<()> {
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        esc(title),
        esc(body)
    );
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
