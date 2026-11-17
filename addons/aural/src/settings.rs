use super::*;
use serde::{Deserialize, Serialize};

pub const KEY: &str = "audio_settings_v1";
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub name: String,
}
impl DeviceIdentity {
    pub fn resolve(&self, names: impl Iterator<Item = String>) -> Option<usize> {
        let matches: Vec<_> = names
            .enumerate()
            .filter(|(_, name)| name == &self.name)
            .collect();
        (matches.len() == 1).then(|| matches[0].0)
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceSettings {
    pub enabled: Option<bool>,
    pub gain_db: f32,
    pub volume: f32,
    pub outputs: [Option<DeviceIdentity>; 2],
    pub routes: [bool; 2],
}
impl Default for DeviceSettings {
    fn default() -> Self {
        Self {
            enabled: None,
            gain_db: 0.0,
            volume: 50.0,
            outputs: [None, None],
            routes: [false; 2],
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioPreferences {
    pub auto_enable: bool,
    pub remember_devices: bool,
    pub default_input: Option<DeviceIdentity>,
    pub last_input: Option<DeviceIdentity>,
    pub devices: BTreeMap<String, DeviceSettings>,
}
impl AudioPreferences {
    pub fn startup_enabled(&self) -> bool {
        let saved = self
            .remember_devices
            .then(|| {
                self.default_input
                    .as_ref()
                    .or(self.last_input.as_ref())
                    .and_then(|id| self.devices.get(&id.name))
                    .and_then(|profile| profile.enabled)
            })
            .flatten();
        saved.unwrap_or(self.auto_enable)
    }
}
struct Inventory {
    inputs: Vec<Device>,
    outputs: Vec<Device>,
}

const DISCOVERY_PERIOD: Duration = Duration::from_secs(2);

/// Active endpoints as Windows lists them, without opening them: `host.devices()` only
/// enumerates endpoints and reads their names. No audio client is activated and no format is
/// queried, so this is safe to repeat every couple of seconds.
#[cfg_attr(test, allow(dead_code))]
struct Endpoints(Vec<Device>);
impl PartialEq for Endpoints {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self
                .0
                .iter()
                .zip(&other.0)
                .all(|(a, b)| a.name == b.name && a.same_endpoint(b))
    }
}
#[cfg_attr(test, allow(dead_code))]
fn endpoints(host: &cpal::Host) -> Result<Endpoints, String> {
    Ok(Endpoints(
        host.devices()
            .map_err(|e| e.to_string())?
            .map(Device::listed)
            .collect(),
    ))
}

/// The full input and output lists. CPAL's `input_devices`/`output_devices` decide what each
/// endpoint is by activating an `IAudioClient` on it and asking it about ~70 formats. Those
/// questions are answered by the audio drivers inside `audiodg.exe`, and on some PCs every
/// round leaks memory and handles there. So this runs only when the endpoints change.
#[cfg_attr(test, allow(dead_code))]
fn inventory(host: &cpal::Host) -> Result<Inventory, String> {
    Ok(Inventory {
        inputs: host
            .input_devices()
            .map_err(|e| e.to_string())?
            .map(Device::listed)
            .collect(),
        outputs: host
            .output_devices()
            .map_err(|e| e.to_string())?
            .map(Device::listed)
            .collect(),
    })
}

/// Decides, once per discovery pass, whether the expensive inventory is needed.
struct Watch<S> {
    last: Option<S>,
    rescan: bool,
}
impl<S: PartialEq> Watch<S> {
    fn new() -> Self {
        Self {
            last: None,
            rescan: true,
        }
    }
    fn rescan(&mut self) {
        self.rescan = true;
    }
    /// `snapshot` is the cheap endpoint list; `full` is called only on the first pass, when
    /// the endpoints changed, after a rescan request, or after the previous inventory failed.
    fn pass<I>(
        &mut self,
        snapshot: Result<S, String>,
        full: impl FnOnce() -> Result<I, String>,
    ) -> Option<Result<I, String>> {
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.last = None;
                return Some(Err(error));
            }
        };
        if !self.rescan && self.last.as_ref() == Some(&snapshot) {
            return None;
        }
        let result = full();
        self.rescan = false;
        self.last = result.is_ok().then_some(snapshot);
        Some(result)
    }
}

/// Backs off reopening streams that keep failing. The first loss is retried on the next
/// discovery pass, as before; each further loss within `CALM` waits twice as long, up to a
/// minute. A failing stream is not rebuilt every 2 seconds forever.
#[derive(Default)]
pub struct Recovery {
    losses: u32,
    last_loss: Option<Instant>,
    retry_at: Option<Instant>,
}
impl Recovery {
    const CALM: Duration = Duration::from_secs(300);
    const MAX: Duration = Duration::from_secs(60);
    pub fn lost(&mut self, now: Instant) {
        if self
            .last_loss
            .is_none_or(|last| now.duration_since(last) > Self::CALM)
        {
            self.losses = 0;
        }
        self.losses += 1;
        self.last_loss = Some(now);
        let delay = if self.losses == 1 {
            Duration::ZERO
        } else {
            (DISCOVERY_PERIOD * 2u32.saturating_pow(self.losses - 1)).min(Self::MAX)
        };
        self.retry_at = Some(now + delay);
    }
    /// True while a scheduled retry is still in the future.
    pub fn waiting(&self, now: Instant) -> bool {
        self.retry_at.is_some_and(|at| at > now)
    }
    /// True once when the scheduled retry is due.
    pub fn due(&mut self, now: Instant) -> bool {
        let due = self.retry_at.is_some_and(|at| at <= now);
        if due {
            self.retry_at = None;
        }
        due
    }
}

enum Wake {
    Rescan,
    Stop,
}
type Mailbox = Arc<Mutex<Option<Result<Inventory, String>>>>;
pub struct Discovery {
    wake: std::sync::mpsc::Sender<Wake>,
    // The newest inventory replaces an unread older one, so a change is never dropped.
    inventory: Mailbox,
    thread: std::thread::JoinHandle<()>,
}
impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.wake.send(Wake::Stop);
    }
}
impl Discovery {
    fn start() -> Self {
        Self::spawn(watch_devices)
    }
    fn spawn(body: fn(std::sync::mpsc::Receiver<Wake>, Mailbox)) -> Self {
        let (wake, woken) = std::sync::mpsc::channel();
        let inventory = Arc::new(Mutex::new(None));
        let slot = inventory.clone();
        let thread = std::thread::spawn(move || body(woken, slot));
        Self {
            wake,
            inventory,
            thread,
        }
    }
    /// Asks for one fresh inventory on the next pass, so a stream lost to an error can be
    /// reopened even when the endpoints did not change.
    pub fn rescan(&self) {
        let _ = self.wake.send(Wake::Rescan);
    }
    fn alive(&self) -> bool {
        !self.thread.is_finished()
    }
}

#[cfg(not(test))]
fn watch_devices(woken: std::sync::mpsc::Receiver<Wake>, slot: Mailbox) {
    let host = cpal::default_host();
    let mut watch = Watch::new();
    loop {
        if let Some(result) = watch.pass(endpoints(&host), || inventory(&host)) {
            *slot.lock().unwrap() = Some(result);
        }
        // A rescan request waits for the next pass, so streams are retried at most once
        // per period.
        let next = Instant::now() + DISCOVERY_PERIOD;
        loop {
            match woken.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Ok(Wake::Rescan) => watch.rescan(),
                Ok(Wake::Stop) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// Unit tests never touch real audio devices: this stand-in only waits to be stopped.
#[cfg(test)]
fn watch_devices(woken: std::sync::mpsc::Receiver<Wake>, _: Mailbox) {
    while let Ok(Wake::Rescan) = woken.recv() {}
}

impl Aural {
    pub fn request_settings_update(&mut self) {
        self.settings_update_requested = true;
    }
    pub fn begin_ui_preferences(&mut self, prefs: &BTreeMap<String, serde_json::Value>) {
        if !self.preferences_loaded {
            self.load_preferences(prefs);
        }
    }
    fn identity(&self) -> Option<DeviceIdentity> {
        let device = self.inputs.get(self.selected_input?)?;
        (self.inputs.iter().filter(|d| d.name == device.name).count() == 1).then(|| {
            DeviceIdentity {
                name: device.name.clone(),
            }
        })
    }
    fn snapshot(&mut self) {
        if !self.preferences.remember_devices {
            return;
        }
        if let Some(id) = self.profile_input.clone() {
            self.preferences.last_input = Some(id.clone());
            self.preferences.devices.insert(
                id.name.clone(),
                DeviceSettings {
                    enabled: Some(
                        self.input.is_some() || (self.want_input && !self.manual_disabled),
                    ),
                    gain_db: self.gain_db,
                    volume: self.volume,
                    outputs: std::array::from_fn(|i| {
                        self.selected_outputs[i]
                            .and_then(|n| self.outputs.get(n))
                            .map(|d| DeviceIdentity {
                                name: d.name.clone(),
                            })
                            .or_else(|| {
                                self.preferences
                                    .devices
                                    .get(&id.name)
                                    .and_then(|p| p.outputs[i].clone())
                            })
                    }),
                    routes: self.route_intent,
                },
            );
        }
    }
    pub fn finish_ui_preferences(
        &mut self,
        prefs: &mut BTreeMap<String, serde_json::Value>,
    ) -> bool {
        if !self.preferences_allowed {
            return false;
        }
        if self.input.is_some() {
            self.want_input = true;
        }
        for i in 0..2 {
            if self.routes[i].is_some() {
                self.route_intent[i] = true;
            }
        }
        self.profile_input = self.identity().or(self.profile_input.clone());
        self.snapshot();
        if self.input.is_some() && !self.manual_disabled {
            for i in 0..2 {
                if self.route_intent[i]
                    && self.routes[i].is_none()
                    && self.route_errors[i].is_none()
                    && self.selected_outputs[i].is_some()
                {
                    if let Err(error) = self.enable_route(i) {
                        self.route_errors[i] = Some(error);
                    }
                }
            }
        }
        self.ensure_discovery();
        if !prefs.contains_key(KEY) && self.preferences == AudioPreferences::default() {
            return false;
        }
        let value = serde_json::to_value(&self.preferences).unwrap();
        if prefs.get(KEY) == Some(&value) {
            false
        } else {
            prefs.insert(KEY.into(), value);
            true
        }
    }
    pub fn set_default_input(&mut self) {
        self.preferences.default_input = self.identity();
        self.default_was_available = false;
        self.ensure_discovery();
    }
    pub fn clear_default_input(&mut self) {
        self.preferences.default_input = None;
        self.ensure_discovery();
    }
    pub fn default_input_available(&self) -> bool {
        self.preferences
            .default_input
            .as_ref()
            .and_then(|id| id.resolve(self.inputs.iter().map(|d| d.name.clone())))
            .is_some()
    }
    pub fn restore_profile(&mut self) {
        self.profile_input = self.identity();
        self.restore_saved_profile();
    }
    fn restore_saved_profile(&mut self) {
        if !self.preferences.remember_devices {
            return;
        }
        let Some(saved) = self
            .profile_input
            .as_ref()
            .and_then(|id| self.preferences.devices.get(&id.name))
            .cloned()
        else {
            self.gain_db = 0.0;
            self.volume = 50.0;
            self.route_intent = [false; 2];
            return;
        };
        self.gain_db = if saved.gain_db.is_finite() {
            saved.gain_db.clamp(-30.0, 24.0)
        } else {
            0.0
        };
        self.volume = if saved.volume.is_finite() {
            saved.volume.clamp(0.0, 100.0)
        } else {
            50.0
        };
        self.monitor_level
            .store((self.volume / 100.0).to_bits(), Relaxed);
        self.selected_outputs = std::array::from_fn(|i| {
            saved.outputs[i]
                .as_ref()
                .and_then(|id| id.resolve(self.outputs.iter().map(|d| d.name.clone())))
        });
        self.route_intent = saved.routes;
        if let Some(enabled) = saved.enabled {
            self.want_input = enabled;
            self.manual_disabled = !enabled;
        }
    }
    pub fn change_microphone(&mut self) {
        self.snapshot();
        self.cancel();
        self.want_input = false;
        self.manual_disabled = true;
        self.restore_profile();
        self.route_errors = [None, None];
        self.message = None;
    }
    pub fn disable_input(&mut self) {
        self.cancel();
        self.want_input = false;
        self.manual_disabled = true;
    }
    pub fn route_stopped(&mut self, index: usize) {
        self.stop_route(index);
        self.route_intent[index] = false;
    }
    pub fn ensure_discovery(&mut self) {
        self.ensure_discovery_at(Instant::now());
    }
    fn ensure_discovery_at(&mut self, now: Instant) {
        if self.discovery.as_ref().is_some_and(|d| !d.alive()) {
            // The thread ended unexpectedly (a panic in a debug build; release builds abort).
            // Restart it with the same back-off as lost streams, since every new thread
            // starts with a full inventory.
            self.discovery = None;
            self.discovery_restart.lost(now);
        }
        let needed = self.allowed
            && self.preferences_allowed
            && (self.preferences.auto_enable
                || self.want_input
                || self.preferences.default_input.is_some());
        if needed && self.discovery.is_none() && !self.discovery_restart.waiting(now) {
            self.discovery_restart.due(now);
            self.discovery = Some(Discovery::start());
        }
        if !needed {
            self.discovery = None;
        }
    }
    pub fn poll_discovery(&mut self) {
        self.ensure_discovery();
        let result = self
            .discovery
            .as_ref()
            .and_then(|w| w.inventory.lock().unwrap().take());
        let Some(result) = result else {
            return;
        };
        let fresh = match result {
            Ok(fresh) => fresh,
            Err(error) => {
                self.microphone_refresh_error =
                    Some(format!("Could not refresh audio devices: {error}"));
                return;
            }
        };
        let target = self
            .preferences
            .default_input
            .as_ref()
            .or(self.preferences.last_input.as_ref())
            .cloned();
        let old = self.identity();
        let names: Vec<String> = fresh.inputs.iter().map(|d| d.name.clone()).collect();
        let preferred = target
            .as_ref()
            .and_then(|id| id.resolve(names.iter().cloned()));
        let selected = select_input(
            old.as_ref(),
            self.preferences.default_input.as_ref(),
            self.preferences.last_input.as_ref(),
            self.default_was_available,
            &names,
        );
        self.default_was_available = preferred.is_some();
        let next_id = selected
            .and_then(|i| fresh.inputs.get(i))
            .map(|d| DeviceIdentity {
                name: d.name.clone(),
            });
        let changed = old != next_id;
        if changed {
            self.snapshot();
            self.cancel();
        }
        let output_ids: [Option<DeviceIdentity>; 2] = std::array::from_fn(|i| {
            self.selected_outputs[i]
                .and_then(|n| self.outputs.get(n))
                .map(|d| DeviceIdentity {
                    name: d.name.clone(),
                })
        });
        self.selected_outputs = std::array::from_fn(|i| {
            output_ids[i]
                .as_ref()
                .and_then(|id| id.resolve(fresh.outputs.iter().map(|d| d.name.clone())))
        });
        self.inputs = fresh.inputs;
        self.outputs = fresh.outputs;
        self.selected_input = selected;
        self.loaded = true;
        if changed {
            self.restore_profile();
        } else if self.preferences.remember_devices {
            if let Some(saved) = self
                .profile_input
                .as_ref()
                .and_then(|id| self.preferences.devices.get(&id.name))
            {
                for i in 0..2 {
                    if self.selected_outputs[i].is_none() {
                        self.selected_outputs[i] = saved.outputs[i]
                            .as_ref()
                            .and_then(|id| id.resolve(self.outputs.iter().map(|d| d.name.clone())));
                    }
                }
            }
        }
        if target.is_some() && preferred.is_none() {
            self.microphone_refresh_error =
                Some("Waiting for the saved microphone to reconnect.".into());
        } else {
            self.microphone_refresh_error = None;
        }
        if self.want_input
            && !self.manual_disabled
            && self.input.is_none()
            && self.selected_input.is_some()
        {
            if let Err(error) = self.enable_input() {
                self.mic_fault = Some(error);
            }
        }
        if self.input.is_some() {
            for i in 0..2 {
                if self.route_intent[i] && self.routes[i].is_none() {
                    if let Err(error) = self.enable_route(i) {
                        self.route_errors[i] = Some(error);
                    }
                }
            }
        }
    }
}
#[cfg(test)]
mod discovery_tests {
    use super::*;
    use std::cell::Cell;

    /// Runs one discovery pass against a fake endpoint list and counts full inventories, the
    /// calls that open every endpoint and query its formats inside audiodg.exe.
    fn pass(
        watch: &mut Watch<Vec<&'static str>>,
        endpoints: Result<Vec<&'static str>, String>,
        full: &Cell<u32>,
        fails: bool,
    ) -> Option<Result<u32, String>> {
        watch.pass(endpoints, || {
            full.set(full.get() + 1);
            if fails {
                Err("busy".into())
            } else {
                Ok(full.get())
            }
        })
    }

    #[test]
    fn unchanged_endpoints_never_reopen_devices() {
        let full = Cell::new(0);
        let mut watch = Watch::new();
        let mics = || Ok(vec!["Mic", "Speakers"]);
        assert_eq!(pass(&mut watch, mics(), &full, false), Some(Ok(1)));
        // An hour of 2-second passes with nothing plugged in or out.
        for _ in 0..1800 {
            assert_eq!(pass(&mut watch, mics(), &full, false), None);
        }
        assert_eq!(full.get(), 1, "the inventory ran once, not every pass");
    }

    #[test]
    fn a_change_rescan_or_failure_runs_one_inventory() {
        let full = Cell::new(0);
        let mut watch = Watch::new();
        pass(&mut watch, Ok(vec!["Mic"]), &full, false);
        // A headset is plugged in: one inventory, then quiet again.
        assert!(pass(&mut watch, Ok(vec!["Mic", "Headset"]), &full, false).is_some());
        assert_eq!(
            pass(&mut watch, Ok(vec!["Mic", "Headset"]), &full, false),
            None
        );
        assert_eq!(full.get(), 2);
        // A stream error asks for a rescan: exactly one more inventory.
        watch.rescan();
        assert!(pass(&mut watch, Ok(vec!["Mic", "Headset"]), &full, false).is_some());
        assert_eq!(
            pass(&mut watch, Ok(vec!["Mic", "Headset"]), &full, false),
            None
        );
        assert_eq!(full.get(), 3);
        // A failed inventory is retried on the next pass until it succeeds.
        watch.rescan();
        assert_eq!(
            pass(&mut watch, Ok(vec!["Mic", "Headset"]), &full, true),
            Some(Err("busy".into()))
        );
        assert!(pass(&mut watch, Ok(vec!["Mic", "Headset"]), &full, false).is_some());
        assert_eq!(
            pass(&mut watch, Ok(vec!["Mic", "Headset"]), &full, false),
            None
        );
        assert_eq!(full.get(), 5);
        // A listing error is reported without opening devices, and recovery re-inventories.
        assert_eq!(
            pass(&mut watch, Err("gone".into()), &full, false),
            Some(Err("gone".into()))
        );
        assert_eq!(full.get(), 5);
        assert!(pass(&mut watch, Ok(vec!["Mic", "Headset"]), &full, false).is_some());
        assert_eq!(full.get(), 6);
    }

    #[test]
    fn repeated_stream_losses_back_off_and_calm_resets() {
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let mut recovery = Recovery::default();
        assert!(!recovery.due(start));
        // First loss: retried on the next pass, as before.
        recovery.lost(at(0));
        assert!(recovery.due(at(0)));
        assert!(!recovery.due(at(0)), "a retry is handed out once");
        // Each further loss waits twice as long: 4, 8, 16, 32, then 60 s at most.
        let mut now = 0;
        for wait in [4, 8, 16, 32, 60, 60] {
            now += 1;
            recovery.lost(at(now));
            assert!(
                !recovery.due(at(now + wait - 1)),
                "too early after {wait} s"
            );
            assert!(recovery.due(at(now + wait)));
            now += wait;
        }
        // After five quiet minutes a loss is treated as the first again.
        recovery.lost(at(now + 301));
        assert!(recovery.due(at(now + 301)));
    }

    /// Stand-in thread that answers each rescan with a numbered (error) inventory.
    fn counting(woken: std::sync::mpsc::Receiver<Wake>, slot: Mailbox) {
        let mut rescans = 0;
        while let Ok(Wake::Rescan) = woken.recv() {
            rescans += 1;
            *slot.lock().unwrap() = Some(Err(format!("rescan {rescans}")));
        }
    }

    fn wait_for(addon: &mut Aural, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while addon.microphone_refresh_error.as_deref()
            != Some(&format!("Could not refresh audio devices: {expected}"))
        {
            assert!(Instant::now() < deadline, "no {expected}");
            addon.poll_discovery();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn watching() -> Aural {
        Aural {
            allowed: true,
            preferences_allowed: true,
            preferences_loaded: true,
            want_input: true,
            ..Aural::default()
        }
    }

    #[test]
    fn a_lost_stream_asks_discovery_for_one_inventory_with_back_off() {
        let mut addon = watching();
        addon.discovery = Some(Discovery::spawn(counting));
        let start = Instant::now();
        // Nothing lost: no rescan, however often the add-on ticks.
        for tick in 0..50 {
            addon.retry_lost_streams(false, start + Duration::from_millis(200 * tick));
        }
        // A lost stream: one rescan, which delivers an inventory to poll_discovery.
        addon.retry_lost_streams(true, start);
        wait_for(&mut addon, "rescan 1");
        // Lost again right away: no rescan until the back-off has passed.
        addon.retry_lost_streams(true, start + Duration::from_secs(1));
        addon.retry_lost_streams(false, start + Duration::from_secs(4));
        std::thread::sleep(Duration::from_millis(50));
        addon.poll_discovery();
        assert_eq!(
            addon.microphone_refresh_error.as_deref(),
            Some("Could not refresh audio devices: rescan 1")
        );
        addon.retry_lost_streams(false, start + Duration::from_secs(5));
        wait_for(&mut addon, "rescan 2");
    }

    #[test]
    fn a_dead_discovery_thread_is_restarted_with_back_off() {
        fn die(addon: &mut Aural) {
            addon.discovery = Some(Discovery::spawn(|_, _| {}));
            let deadline = Instant::now() + Duration::from_secs(5);
            while addon.discovery.as_ref().unwrap().alive() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let mut addon = watching();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        // First death: restarted on the next tick.
        die(&mut addon);
        addon.ensure_discovery_at(at(0));
        assert!(addon.discovery.as_ref().unwrap().alive());
        // Dies again soon after: no new thread (and no full inventory) on every 200 ms tick.
        die(&mut addon);
        for tick in 1..20 {
            addon.ensure_discovery_at(at(200 * tick));
            assert!(
                addon.discovery.is_none(),
                "restarted too early at tick {tick}"
            );
        }
        addon.ensure_discovery_at(at(4200));
        assert!(addon.discovery.as_ref().unwrap().alive());
        // And it is stopped when nothing needs it.
        addon.want_input = false;
        addon.ensure_discovery_at(at(4400));
        assert!(addon.discovery.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn microphone_activation_is_saved_and_restored_per_device_and_on_restart() {
        let mut addon = Aural {
            preferences_allowed: true,
            preferences_loaded: true,
            ..Aural::default()
        };
        addon.preferences.remember_devices = true;
        addon.profile_input = Some(DeviceIdentity {
            name: "Razer".into(),
        });
        addon.want_input = true;
        addon.snapshot();
        addon.profile_input = Some(DeviceIdentity {
            name: "Webcam".into(),
        });
        addon.disable_input();
        addon.snapshot();
        assert_eq!(addon.preferences.devices["Razer"].enabled, Some(true));
        assert_eq!(addon.preferences.devices["Webcam"].enabled, Some(false));

        addon.profile_input = Some(DeviceIdentity {
            name: "Razer".into(),
        });
        addon.restore_saved_profile();
        assert!(addon.want_input && !addon.manual_disabled);
        let mut prefs = BTreeMap::new();
        addon.finish_ui_preferences(&mut prefs);
        assert!(Aural::needs_background_preferences(&prefs));
        let mut restarted = Aural {
            preferences_allowed: true,
            ..Aural::default()
        };
        restarted.load_preferences(&prefs);
        assert!(
            restarted.want_input,
            "saved on state restores without the startup switch"
        );

        addon.disable_input();
        addon.finish_ui_preferences(&mut prefs);
        assert!(!Aural::needs_background_preferences(&prefs));
        let mut restarted = Aural {
            preferences_allowed: true,
            ..Aural::default()
        };
        restarted.load_preferences(&prefs);
        assert!(!restarted.want_input, "saved off state survives restart");
    }

    #[test]
    fn legacy_profiles_keep_startup_behavior_until_activation_is_saved() {
        let mut settings: AudioPreferences = serde_json::from_value(serde_json::json!({
            "auto_enable": true,
            "remember_devices": true,
            "last_input": {"name": "Razer"},
            "devices": {"Razer": {"gain_db": 18.0}}
        }))
        .unwrap();
        assert!(settings.startup_enabled());
        settings.devices.get_mut("Razer").unwrap().enabled = Some(false);
        assert!(!settings.startup_enabled());
        settings.remember_devices = false;
        assert!(
            settings.startup_enabled(),
            "device states apply only when enabled"
        );
    }

    #[test]
    fn defaults_are_opt_in_and_opening_settings_does_not_write() {
        let mut a = Aural {
            preferences_allowed: true,
            ..Aural::default()
        };
        let mut prefs = BTreeMap::new();
        a.begin_ui_preferences(&prefs);
        assert!(!a.preferences.auto_enable);
        assert!(!a.preferences.remember_devices);
        assert!(!a.finish_ui_preferences(&mut prefs));
        assert!(prefs.is_empty());
        assert!(a.discovery.is_none());
    }
    #[test]
    fn preferences_round_trip_preserves_absent_default_and_separate_profiles() {
        let mut settings = AudioPreferences {
            remember_devices: true,
            default_input: Some(DeviceIdentity {
                name: "Disconnected Razer".into(),
            }),
            ..AudioPreferences::default()
        };
        settings.devices.insert(
            "Razer".into(),
            DeviceSettings {
                gain_db: 18.0,
                routes: [false, true],
                ..DeviceSettings::default()
            },
        );
        settings.devices.insert(
            "Webcam".into(),
            DeviceSettings {
                gain_db: 3.0,
                ..DeviceSettings::default()
            },
        );
        let value = serde_json::to_value(&settings).unwrap();
        let decoded: AudioPreferences = serde_json::from_value(value).unwrap();
        assert_eq!(settings, decoded);
        assert_eq!(decoded.devices["Razer"].gain_db, 18.0);
        assert_eq!(decoded.devices["Webcam"].gain_db, 3.0);
        assert_eq!(
            decoded.default_input.unwrap().resolve(std::iter::empty()),
            None
        );
    }
    #[test]
    fn identity_never_chooses_an_ambiguous_name_or_another_endpoint() {
        let id = DeviceIdentity { name: "Mic".into() };
        assert_eq!(
            id.resolve(["Other".to_string(), "Mic".to_string()].into_iter()),
            Some(1)
        );
        assert_eq!(
            id.resolve(["Mic".to_string(), "Mic".to_string()].into_iter()),
            None
        );
        assert_eq!(id.resolve(["Other".to_string()].into_iter()), None);
    }
    #[test]
    fn missing_permission_ignores_startup_and_manual_disable_suppresses_reopen() {
        let mut a = Aural::default();
        let prefs = BTreeMap::from([(KEY.into(), serde_json::json!({"auto_enable":true}))]);
        a.load_preferences(&prefs);
        assert!(!a.want_input);
        assert!(a.discovery.is_none());
        a.want_input = true;
        a.route_intent = [false, true];
        a.disable_input();
        assert_eq!(a.route_intent, [false, true]);
        assert!(a.manual_disabled);
        assert!(!a.want_input);
    }
}

/// Selection policy has no audio handles so reconnect behavior can be verified without hardware.
fn select_input(
    current: Option<&DeviceIdentity>,
    default: Option<&DeviceIdentity>,
    last: Option<&DeviceIdentity>,
    default_was_available: bool,
    names: &[String],
) -> Option<usize> {
    let resolve = |id: &DeviceIdentity| id.resolve(names.iter().cloned());
    let current = current.and_then(resolve);
    let target = default.or(last);
    let preferred = target.and_then(resolve);
    if default.is_some() {
        if preferred.is_some() && !default_was_available {
            preferred
        } else {
            current.or(preferred)
        }
    } else {
        current.or(preferred).or_else(|| {
            if target.is_none() {
                names
                    .iter()
                    .enumerate()
                    .find(|(_, name)| {
                        !virtual_cable(name) && names.iter().filter(|n| *n == *name).count() == 1
                    })
                    .map(|(i, _)| i)
            } else {
                None
            }
        })
    }
}
#[cfg(test)]
mod reconnect_policy_tests {
    use super::*;
    fn id(name: &str) -> DeviceIdentity {
        DeviceIdentity { name: name.into() }
    }
    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn absent_default_waits_at_startup_but_keeps_manual_fallback() {
        assert_eq!(
            select_input(None, Some(&id("Razer")), None, false, &names(&["Webcam"])),
            None
        );
        assert_eq!(
            select_input(
                Some(&id("Webcam")),
                Some(&id("Razer")),
                None,
                false,
                &names(&["Webcam"])
            ),
            Some(0)
        );
    }
    #[test]
    fn reconnect_switches_to_default_once_and_keeps_manual_selection_afterward() {
        let inventory = names(&["Webcam", "Razer"]);
        assert_eq!(
            select_input(
                Some(&id("Webcam")),
                Some(&id("Razer")),
                None,
                false,
                &inventory
            ),
            Some(1)
        );
        assert_eq!(
            select_input(
                Some(&id("Webcam")),
                Some(&id("Razer")),
                None,
                true,
                &inventory
            ),
            Some(0)
        );
        assert_eq!(
            select_input(
                None,
                Some(&id("Razer")),
                None,
                false,
                &names(&["Razer", "Razer"])
            ),
            None
        );
    }
    #[test]
    fn last_microphone_wins_startup_and_absent_last_never_falls_back() {
        assert_eq!(
            select_input(
                None,
                None,
                Some(&id("Razer")),
                false,
                &names(&["Webcam", "Razer"])
            ),
            Some(1)
        );
        assert_eq!(
            select_input(None, None, Some(&id("Razer")), false, &names(&["Webcam"])),
            None
        );
    }
    #[test]
    fn actual_profile_snapshot_and_restart_keep_gain_routes_and_device_separation() {
        let mut a = Aural {
            preferences_allowed: true,
            preferences_loaded: true,
            ..Aural::default()
        };
        a.preferences.remember_devices = true;
        a.profile_input = Some(id("Razer"));
        a.gain_db = 18.0;
        a.route_intent = [false, true];
        a.snapshot();
        a.profile_input = Some(id("Webcam"));
        a.gain_db = 3.0;
        a.route_intent = [true, false];
        a.snapshot();
        let mut prefs = BTreeMap::new();
        // snapshot each saved profile before invoking the same persistence path used by UI.
        a.finish_ui_preferences(&mut prefs);
        let mut restarted = Aural {
            preferences_allowed: true,
            ..Aural::default()
        };
        restarted.load_preferences(&prefs);
        assert_eq!(restarted.preferences.devices["Razer"].gain_db, 18.0);
        assert_eq!(restarted.preferences.devices["Razer"].routes, [false, true]);
        assert_eq!(restarted.preferences.devices["Webcam"].gain_db, 3.0);
        assert_eq!(
            restarted.preferences.devices["Webcam"].routes,
            [true, false]
        );
        assert!(restarted.discovery.is_none());
    }
}
