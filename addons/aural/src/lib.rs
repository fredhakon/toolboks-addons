//! Aural's live audio controls inside the Toolboks workspace.
mod routing;
mod settings;
mod timeline;
use settings::{AudioPreferences, DeviceIdentity, Discovery, Recovery};

use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering::Relaxed},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use egui::{self, RichText};
use toolboks_addon_sdk::{tutorial, Capability, NativeAddon, TutorialStep};

use toolboks_shared::ui as look;

pub const ID: &str = "toolboks.aural";

const VB_CABLE: &str = "https://vb-audio.com/Cable/";
const ROUTE_LABELS: [&str; 2] = ["Hear myself", "Send to other apps"];

/// What the streams report: the raw device text. The screen shows a short sentence and keeps
/// this text behind "Technical details".
type AudioError = Arc<Mutex<Option<String>>>;

const MICROPHONE_LOST: &str = "Microphone disconnected. Reconnect it, then enable it again.";
const OUTPUT_LOST: &str = "Output disconnected. Reconnect it, then start this route again.";

/// A problem shown beside the control it belongs to: one short sentence that says what to do,
/// and the technical text kept apart for whoever needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fault {
    short: String,
    detail: Option<String>,
}

impl Fault {
    fn new(short: &str, raw: &str) -> Self {
        let detail = plain_detail(raw);
        Self {
            short: short.into(),
            detail: (!detail.is_empty()).then_some(detail),
        }
    }
}

impl From<&str> for Fault {
    fn from(short: &str) -> Self {
        Self {
            short: short.into(),
            detail: None,
        }
    }
}

impl From<String> for Fault {
    fn from(short: String) -> Self {
        Self {
            short,
            detail: None,
        }
    }
}

/// Device text without trailing punctuation, so it never ends in "..".
fn plain_detail(raw: &str) -> String {
    raw.trim()
        .trim_end_matches(|c: char| c.is_whitespace() || ".:;,".contains(c))
        .to_owned()
}

/// Where the "send to other apps" route stands. Choosing a device is not proof that sound
/// arrives, so "ready" says only that it can be started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CableState {
    /// No virtual cable among the output devices.
    Missing,
    /// A cable exists, but no destination is chosen yet.
    NoDestination,
    /// The chosen destination is an ordinary output, not a virtual cable.
    NotACable,
    /// A cable is chosen and the route is off.
    Ready,
    /// A cable is chosen, the route is off, and the microphone is off too.
    ReadyInputOff,
    /// The route is running.
    Running,
}

impl CableState {
    /// `cable_present` comes from the devices that exist, never from what is selected.
    fn of(
        cable_present: bool,
        selected_is_cable: Option<bool>,
        input_on: bool,
        running: bool,
    ) -> Self {
        match selected_is_cable {
            _ if !cable_present && selected_is_cable != Some(true) => Self::Missing,
            None => Self::NoDestination,
            Some(false) => Self::NotACable,
            Some(true) if running => Self::Running,
            Some(true) if input_on => Self::Ready,
            Some(true) => Self::ReadyInputOff,
        }
    }
    /// A short status in words, so it does not depend on colour.
    fn headline(self) -> &'static str {
        match self {
            Self::Missing => "Virtual cable not found",
            Self::NoDestination => "Virtual cable found, destination not chosen",
            Self::NotACable => "Destination is not a virtual cable",
            Self::Ready => "Ready to start",
            Self::ReadyInputOff => "Ready to start, microphone is off",
            Self::Running => "Running",
        }
    }
    fn hint(self) -> &'static str {
        match self {
            Self::Missing => "Install VB-Audio Virtual Cable, then refresh. Without it there is nothing to send the sound to.",
            Self::NoDestination => "Choose CABLE Input below.",
            Self::NotACable => "Other apps cannot use this as a microphone. Choose CABLE Input instead.",
            Self::Ready => "Start the route to send your microphone to the cable.",
            Self::ReadyInputOff => "Nothing is sent until you start the route.",
            Self::Running => "Sending your microphone to the cable. Choose CABLE Output as the microphone in the other app. Choosing a device does not prove sound arrives, so check the level there.",
        }
    }
}

#[derive(Default)]
struct Signal {
    gain: AtomicU32,
    metering: AtomicBool,
    peaks: [AtomicU32; 2],
    clipped: AtomicBool,
    routes: routing::Routes,
    error: AudioError,
}

fn db(amplitude: f32) -> f32 {
    if amplitude > 0.0 {
        (20.0 * amplitude.log10()).max(-60.0)
    } else {
        -60.0
    }
}

fn gain(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn process<T: cpal::Sample>(samples: &[T], channels: usize, signal: &Signal)
where
    f32: cpal::FromSample<T>,
{
    let gain = f32::from_bits(signal.gain.load(Relaxed));
    if signal.metering.load(Relaxed) {
        let mut peaks = [0.0_f32; 2];
        let mut clipped = false;
        for (index, sample) in samples.iter().enumerate() {
            let level = (sample.to_sample::<f32>() * gain).abs();
            if let Some(peak) = peaks.get_mut(index % channels) {
                *peak = peak.max(level);
            }
            clipped |= level >= 1.0;
        }
        if channels == 1 {
            peaks[1] = peaks[0];
        }
        for (slot, peak) in signal.peaks.iter().zip(peaks) {
            slot.fetch_max(peak.to_bits(), Relaxed);
        }
        if clipped {
            signal.clipped.store(true, Relaxed);
        }
    }
    routing::forward(samples, channels, gain, &signal.routes);
}

struct Input {
    _stream: cpal::Stream,
    config: cpal::StreamConfig,
    signal: Arc<Signal>,
}

impl Input {
    fn open(device: &cpal::Device, gain_db: f32) -> Result<Self, String> {
        let supported = device.default_input_config().map_err(|e| e.to_string())?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let channels = usize::from(config.channels);
        if channels == 0 || config.sample_rate.0 == 0 {
            return Err("The microphone reported an invalid format.".into());
        }
        let signal = Arc::new(Signal::default());
        signal.gain.store(gain(gain_db).to_bits(), Relaxed);
        signal.metering.store(true, Relaxed);
        macro_rules! build {
            ($t:ty) => {{
                let samples = signal.clone();
                let error = signal.error.clone();
                device.build_input_stream(
                    &config,
                    move |data: &[$t], _| process(data, channels, &samples),
                    move |e| *error.lock().unwrap() = Some(e.to_string()),
                    None,
                )
            }};
        }
        let stream = match format {
            cpal::SampleFormat::F32 => build!(f32),
            cpal::SampleFormat::F64 => build!(f64),
            cpal::SampleFormat::I8 => build!(i8),
            cpal::SampleFormat::I16 => build!(i16),
            cpal::SampleFormat::I32 => build!(i32),
            cpal::SampleFormat::I64 => build!(i64),
            cpal::SampleFormat::U8 => build!(u8),
            cpal::SampleFormat::U16 => build!(u16),
            cpal::SampleFormat::U32 => build!(u32),
            cpal::SampleFormat::U64 => build!(u64),
            _ => return Err("This microphone format is unsupported.".into()),
        }
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(Self {
            _stream: stream,
            config,
            signal,
        })
    }
}

struct Output {
    _stream: cpal::Stream,
    error: AudioError,
}

struct Device {
    device: cpal::Device,
    name: String,
}

impl Device {
    fn listed(device: cpal::Device) -> Self {
        let name = device.name().unwrap_or_else(|_| "Unnamed device".into());
        Self { device, name }
    }

    /// The same endpoint, looked up again without opening it. Listed devices can be hours old
    /// and still hold the audio client CPAL activated while sorting inputs from outputs; after
    /// a format change or driver reset that client is invalid and every open would fail.
    /// A fresh device activates a new client when the stream is built.
    fn fresh(&self) -> cpal::Device {
        cpal::default_host()
            .devices()
            .ok()
            .and_then(|devices| {
                fresh_match(self, devices.map(Device::listed), Device::same_endpoint)
            })
            .map_or_else(|| self.device.clone(), |device| device.device)
    }

    fn same_endpoint(&self, other: &Self) -> bool {
        #[cfg(windows)]
        {
            // CPAL's WASAPI equality compares IMMDevice endpoint IDs, not names.
            #[allow(irrefutable_let_patterns)]
            if let (
                cpal::platform::DeviceInner::Wasapi(a),
                cpal::platform::DeviceInner::Wasapi(b),
            ) = (self.device.as_inner(), other.device.as_inner())
            {
                return a == b;
            }
        }
        // Other backends expose no portable identity. Reconciliation below rejects
        // ambiguous names rather than switching to an arbitrary microphone.
        self.name == other.name
    }
}

/// The single current entry for `listed`'s endpoint, if there is exactly one.
fn fresh_match<T>(
    listed: &T,
    current: impl Iterator<Item = T>,
    same: impl Fn(&T, &T) -> bool,
) -> Option<T> {
    let mut matches = current.filter(|item| same(listed, item));
    let found = matches.next()?;
    matches.next().is_none().then_some(found)
}

fn refreshed_selection<T>(
    old: &[T],
    selected: Option<usize>,
    fresh: &[T],
    same: impl Fn(&T, &T) -> bool,
) -> Option<usize> {
    let previous = old.get(selected?)?;
    if old.iter().filter(|item| same(previous, item)).count() != 1 {
        return None;
    }
    let mut matches = fresh
        .iter()
        .enumerate()
        .filter(|(_, item)| same(previous, item));
    let index = matches.next()?.0;
    matches.next().is_none().then_some(index)
}

fn virtual_cable(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("virtual cable")
        || name.contains("vb-audio")
        || name.contains("cable input")
        || name.contains("cable output")
        || name.contains("voicemeeter")
}

pub struct Aural {
    allowed: bool,
    ui_active: bool,
    preferences_allowed: bool,
    preferences_loaded: bool,
    preferences: AudioPreferences,
    profile_input: Option<DeviceIdentity>,
    discovery: Option<Discovery>,
    want_input: bool,
    manual_disabled: bool,
    default_was_available: bool,
    route_intent: [bool; 2],
    recovery: Recovery,
    discovery_restart: Recovery,
    settings_update_requested: bool,
    loaded: bool,
    inputs: Vec<Device>,
    outputs: Vec<Device>,
    selected_input: Option<usize>,
    selected_outputs: [Option<usize>; 2],
    input: Option<Input>,
    routes: [Option<Output>; 2],
    route_errors: [Option<Fault>; 2],
    mic_fault: Option<Fault>,
    message: Option<String>,
    microphone_refresh_error: Option<String>,
    gain_db: f32,
    volume: f32,
    monitor_level: Arc<AtomicU32>,
    levels: [f32; 2],
    timeline: timeline::Timeline,
    meter_started: Instant,
    last_sample: Instant,
}

impl Default for Aural {
    fn default() -> Self {
        Self {
            allowed: false,
            ui_active: false,
            preferences_allowed: false,
            preferences_loaded: false,
            preferences: AudioPreferences::default(),
            profile_input: None,
            discovery: None,
            want_input: false,
            manual_disabled: false,
            default_was_available: false,
            route_intent: [false; 2],
            recovery: Recovery::default(),
            discovery_restart: Recovery::default(),
            settings_update_requested: false,
            loaded: false,
            inputs: Vec::new(),
            outputs: Vec::new(),
            selected_input: None,
            selected_outputs: [None; 2],
            input: None,
            routes: [None, None],
            route_errors: [None, None],
            mic_fault: None,
            message: None,
            microphone_refresh_error: None,
            gain_db: 0.0,
            volume: 50.0,
            monitor_level: Arc::new(AtomicU32::new(0.5_f32.to_bits())),
            levels: [-60.0; 2],
            timeline: timeline::Timeline::default(),
            meter_started: Instant::now(),
            last_sample: Instant::now(),
        }
    }
}

impl Aural {
    pub fn needs_background_preferences(prefs: &BTreeMap<String, serde_json::Value>) -> bool {
        prefs
            .get(settings::KEY)
            .and_then(|value| serde_json::from_value::<AudioPreferences>(value.clone()).ok())
            .is_some_and(|settings| settings.startup_enabled() || settings.default_input.is_some())
    }

    fn refresh_microphones(&mut self) {
        // Enumerate on demand with a fresh host. No cached device list or resident
        // watcher is kept, and existing streams retain their original endpoints.
        let host = cpal::default_host();
        let devices = host
            .input_devices()
            .map(|devices| devices.map(Device::listed).collect())
            .map_err(|error| format!("Could not refresh microphones: {error}. Try again."));
        self.apply_microphone_refresh(devices);
    }

    fn apply_microphone_refresh(&mut self, devices: Result<Vec<Device>, String>) {
        let fresh = match devices {
            Ok(devices) => devices,
            Err(error) => {
                self.microphone_refresh_error = Some(error);
                return;
            }
        };
        let selected = refreshed_selection(
            &self.inputs,
            self.selected_input,
            &fresh,
            Device::same_endpoint,
        );
        self.microphone_refresh_error = (self.selected_input.is_some() && selected.is_none())
            .then(|| "The selected microphone is unavailable. Choose a microphone.".into());
        self.inputs = fresh;
        self.selected_input = selected;
    }

    fn refresh(&mut self) {
        self.cancel();
        self.loaded = true;
        self.message = None;
        self.mic_fault = None;
        self.microphone_refresh_error = None;
        self.route_errors = [None, None];
        let host = cpal::default_host();
        let default_in = host.default_input_device().and_then(|d| d.name().ok());
        let default_out = host.default_output_device().and_then(|d| d.name().ok());
        self.inputs = match host.input_devices() {
            Ok(list) => list.map(Device::listed).collect(),
            Err(error) => {
                self.message = Some(format!("Could not list microphones: {error}"));
                Vec::new()
            }
        };
        self.outputs = match host.output_devices() {
            Ok(list) => list.map(Device::listed).collect(),
            Err(error) => {
                self.message
                    .get_or_insert_with(|| format!("Could not list audio outputs: {error}"));
                Vec::new()
            }
        };
        self.selected_input = self
            .inputs
            .iter()
            .position(|d| Some(&d.name) == default_in.as_ref())
            .filter(|&i| !virtual_cable(&self.inputs[i].name))
            .or_else(|| self.inputs.iter().position(|d| !virtual_cable(&d.name)));
        self.selected_outputs = [
            self.outputs
                .iter()
                .position(|d| Some(&d.name) == default_out.as_ref() && !virtual_cable(&d.name))
                .or_else(|| self.outputs.iter().position(|d| !virtual_cable(&d.name))),
            self.outputs.iter().position(|d| virtual_cable(&d.name)),
        ];
        if let Some(target) = self
            .preferences
            .default_input
            .as_ref()
            .or(self.preferences.last_input.as_ref())
        {
            self.selected_input = target.resolve(self.inputs.iter().map(|d| d.name.clone()));
        }
        self.restore_profile();
    }

    fn enable_input(&mut self) -> Result<(), Fault> {
        if !self.allowed {
            return Err("Live audio access is unavailable.".into());
        }
        let device = self
            .selected_input
            .and_then(|i| self.inputs.get(i))
            .ok_or("Choose a microphone first.")?;
        self.input = Some(Input::open(&device.fresh(), self.gain_db).map_err(|e| {
            Fault::new(
                "Could not open the microphone. Check the device and Windows microphone access.",
                &e,
            )
        })?);
        self.mic_fault = None;
        self.want_input = true;
        self.manual_disabled = false;
        if let Some(input) = &self.input {
            input.signal.metering.store(self.ui_active, Relaxed);
        }
        self.message = None;
        self.last_sample = Instant::now();
        self.meter_started = self.last_sample;
        self.timeline.reset(self.ui_active);
        Ok(())
    }

    fn stop_route(&mut self, index: usize) {
        if let Some(input) = &self.input {
            input.signal.routes.lock().unwrap()[index] = None;
        }
        self.routes[index] = None;
    }

    fn enable_route(&mut self, index: usize) -> Result<(), Fault> {
        let output = self.selected_outputs[index]
            .and_then(|i| self.outputs.get(i))
            .ok_or("Choose an audio output first.")?;
        if self.routes[1 - index].is_some()
            && self.selected_outputs[index] == self.selected_outputs[1 - index]
        {
            return Err("Choose a separate output for each route.".into());
        }
        if index == 1
            && self
                .selected_input
                .and_then(|i| self.inputs.get(i))
                .is_some_and(|d| virtual_cable(&d.name))
        {
            return Err(
                "Choose your physical microphone before sending audio to a virtual cable.".into(),
            );
        }
        let device = output.fresh();
        let started_input = self.input.is_none();
        if started_input {
            self.enable_input()?;
        }
        let input = self.input.as_ref().ok_or("Enable the microphone first.")?;
        let error = AudioError::default();
        let level = if index == 0 {
            self.monitor_level.clone()
        } else {
            Arc::new(AtomicU32::new(1.0_f32.to_bits()))
        };
        let (stream, bridge) =
            match routing::open(&device, input.config.sample_rate.0, level, error.clone()) {
                Ok(route) => route,
                Err(e) => {
                    if started_input {
                        self.cancel();
                    }
                    return Err(Fault::new(
                        "Could not open this output. Choose another device or refresh the list.",
                        &e,
                    ));
                }
            };
        input.signal.routes.lock().unwrap()[index] = Some(bridge);
        self.route_intent[index] = true;
        self.routes[index] = Some(Output {
            _stream: stream,
            error,
        });
        Ok(())
    }

    /// Discovery no longer sends an unchanged device list every pass, so a lost stream asks
    /// for one. Repeated losses back off (see `Recovery`) instead of rebuilding a failing
    /// stream every 2 seconds.
    fn retry_lost_streams(&mut self, lost: bool, now: Instant) {
        if lost {
            self.recovery.lost(now);
        }
        if self.recovery.due(now) {
            if let Some(discovery) = &self.discovery {
                discovery.rescan();
            }
        }
    }

    fn sample(&mut self) {
        if !self.ui_active {
            return;
        }
        let Some(input) = &self.input else {
            return;
        };
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample);
        self.last_sample = now;
        let peaks = std::array::from_fn(|channel| {
            db(f32::from_bits(input.signal.peaks[channel].swap(0, Relaxed)))
        });
        for (level, peak) in self.levels.iter_mut().zip(peaks) {
            *level = peak.max(*level - elapsed.as_secs_f32() * 48.0);
        }
        self.timeline
            .observe(now.duration_since(self.meter_started), peaks);
    }
}

fn muted(ui: &mut egui::Ui, message: impl Into<String>) {
    ui.label(RichText::new(message.into()).color(look::MUTED));
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(look::TEXT_SECTION).strong());
}

/// A problem beside the control it belongs to. The sentence says what to do; the device's own
/// wording is one click away.
fn fault_ui(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, fault: &Fault) {
    ui.label(RichText::new(&fault.short).color(look::AMBER).strong());
    if let Some(detail) = &fault.detail {
        egui::CollapsingHeader::new("Technical details")
            .id_salt(id)
            .show(ui, |ui| muted(ui, detail.as_str()));
    }
}

/// Plain words for the two cable ends, shown wherever the cable is explained.
const CABLE_EXPLAINED: &str = "CABLE Input is where Toolboks sends your voice. CABLE Output is what the other app listens to, so pick it as that app's microphone.";

fn picker(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    devices: &[Device],
    selected: &mut Option<usize>,
    empty: &str,
    filter: impl Fn(&Device) -> bool,
) -> bool {
    let old = *selected;
    let label = old
        .and_then(|i| devices.get(i))
        .map_or(empty, |d| d.name.as_str());
    egui::ComboBox::from_id_salt(id)
        .width((ui.available_width() - 22.0).clamp(110.0, 260.0))
        .selected_text(label)
        .truncate()
        .show_ui(ui, |ui| {
            for (index, device) in devices.iter().enumerate().filter(|(_, d)| filter(d)) {
                ui.selectable_value(selected, Some(index), &device.name);
            }
        });
    *selected != old
}

fn audio_setting_switch(ui: &mut egui::Ui, value: &mut bool, label: &str) {
    ui.horizontal_top(|ui| {
        let response = look::toggle_switch(ui, value);
        let text = ui.add(egui::Label::new(label).wrap().sense(egui::Sense::click()));
        if text.clicked() {
            *value = !*value;
        }
        let response = response.labelled_by(text.id);
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::Role::Switch, ui.is_enabled(), *value, label)
        });
        if response.has_focus() {
            ui.painter().rect_stroke(
                response.rect.expand(2.0),
                look::CONTROL_RADIUS,
                ui.visuals().selection.stroke,
                egui::StrokeKind::Outside,
            );
        }
    });
}

fn refreshing_picker(
    ui: &mut egui::Ui,
    id: &str,
    label: String,
    contents: impl FnOnce(&mut egui::Ui, bool) -> bool,
) -> bool {
    let enabled = ui.is_enabled();
    let was_open = egui::ComboBox::is_open(ui.ctx(), ui.make_persistent_id(egui::IdSalt::new(id)));
    egui::ComboBox::from_id_salt(id)
        .width((ui.available_width() - 22.0).clamp(110.0, 260.0))
        .selected_text(label)
        .truncate()
        .show_ui(ui, |ui| {
            // The popup's initial sizing pass disables its child Ui, but opening
            // still needs the fresh list. Only the parent control gates refresh.
            let opening = enabled && !was_open;
            contents(ui, opening)
        })
        .inner
        .unwrap_or(false)
}

fn status_pill(ui: &mut egui::Ui, text: &str, live: bool) {
    let color = if live { look::GREEN } else { look::MUTED };
    egui::Frame::new()
        .fill(look::SELECTED_BG)
        .corner_radius(20)
        .inner_margin(egui::Margin::symmetric(7, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                let (r, _) = ui.allocate_exact_size(egui::vec2(6.0, 12.0), egui::Sense::hover());
                ui.painter().circle_filled(r.center(), 3.0, color);
                ui.label(RichText::new(text).size(11.0).color(color));
            });
        });
}
fn named_switch(ui: &mut egui::Ui, value: &mut bool, name: &str) -> bool {
    let response = look::toggle_switch(ui, value);
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::Switch, ui.is_enabled(), *value, name)
    });
    if response.has_focus() {
        ui.painter().rect_stroke(
            response.rect.expand(2.0),
            look::CONTROL_RADIUS,
            egui::Stroke::new(1.5, look::ACCENT),
            egui::StrokeKind::Outside,
        );
    }
    response.changed()
}
fn info(ui: &mut egui::Ui, name: &str, text: &str) {
    let response = ui.small_button("i").on_hover_text(text);
    if response.has_focus() || response.clicked() {
        response.show_tooltip_text(text);
    }
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::Role::Button,
            ui.is_enabled(),
            format!("About {name}: {text}"),
        )
    });
}

impl Aural {
    fn microphone_picker(&mut self, ui: &mut egui::Ui) -> bool {
        let label = self
            .selected_input
            .and_then(|index| self.inputs.get(index))
            .map_or("Choose a microphone", |device| device.name.as_str())
            .to_owned();
        refreshing_picker(ui, "aural-input-picker", label, |ui, opening| {
            if opening {
                self.refresh_microphones();
            }
            let mut changed = false;
            for (index, device) in self.inputs.iter().enumerate() {
                changed |= ui
                    .selectable_value(&mut self.selected_input, Some(index), &device.name)
                    .changed();
            }
            if self.inputs.is_empty() {
                muted(
                    ui,
                    "No microphones found. Connect one, then reopen this list.",
                );
            }
            changed
        })
    }

    fn input_ui(&mut self, ui: &mut egui::Ui, interactive: bool) {
        tutorial::region(ui, "aural-input", |ui| {
            look::card(ui, ui.available_width(), 0.0, |ui| {
                section(ui, "Microphone");
                if ui
                    .add_enabled_ui(interactive, |ui| self.microphone_picker(ui))
                    .inner
                {
                    self.change_microphone();
                }
                ui.horizontal(|ui| {
                    let live = self.input.is_some();
                    status_pill(ui, if live { "Live" } else { "Off" }, live);
                    if let Some(input) = &self.input {
                        muted(
                            ui,
                            format!(
                                "{} kHz, {} channels",
                                input.config.sample_rate.0 / 1000,
                                input.config.channels
                            ),
                        );
                    }
                    if ui
                        .add_enabled(interactive, egui::Button::new("Refresh"))
                        .on_hover_text("Refresh microphones")
                        .clicked()
                    {
                        self.refresh_microphones();
                    }
                    let mut enabled = live;
                    if ui
                        .add_enabled_ui(
                            interactive && (live || self.selected_input.is_some()),
                            |ui| named_switch(ui, &mut enabled, "Microphone"),
                        )
                        .inner
                    {
                        if enabled {
                            if let Err(error) = self.enable_input() {
                                self.mic_fault = Some(error);
                            }
                        } else {
                            self.disable_input();
                        }
                    }
                });
                if let Some(fault) = &self.mic_fault {
                    fault_ui(ui, "aural-mic-fault", fault);
                }
                if let Some(error) = &self.microphone_refresh_error {
                    muted(ui, error);
                }
                if self.inputs.is_empty() {
                    muted(ui, "Connect a microphone, then refresh.");
                }
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    section(ui, "Input gain");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(format!("{:+.1} dB", self.gain_db)).strong());
                    });
                });
                ui.add_enabled_ui(interactive, |ui| {
                    ui.spacing_mut().slider_width = ui.available_width() - 8.0;
                    ui.add(
                        egui::Slider::new(&mut self.gain_db, -30.0..=24.0)
                            .show_value(false)
                            .fixed_decimals(1),
                    )
                    .widget_info(|| {
                        egui::WidgetInfo::labeled(egui::Role::Slider, ui.is_enabled(), "Input gain")
                    });
                    ui.horizontal(|ui| {
                        muted(ui, "Lower it if the bars reach red.");
                        if ui
                            .button("Reset")
                            .on_hover_text("Reset input gain")
                            .clicked()
                        {
                            self.gain_db = 0.0;
                        }
                    });
                });
                if let Some(input) = &self.input {
                    input
                        .signal
                        .gain
                        .store(gain(self.gain_db).to_bits(), Relaxed);
                }
            });
        });
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui, interactive: bool) {
        self.settings_card(ui, interactive, 0.0);
    }

    fn settings_card(&mut self, ui: &mut egui::Ui, interactive: bool, height: f32) {
        tutorial::region(ui, "aural-settings", |ui| {
            look::card(ui, ui.available_width(), height, |ui| {
                section(ui, "Remember my setup");
                let enabled = interactive && self.preferences_allowed;
                ui.add_enabled_ui(enabled, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        audio_setting_switch(ui, &mut self.preferences.auto_enable, "Turn on when Toolboks starts");
                        info(ui, "startup", "Starts live audio when Toolboks opens. A saved per-microphone on/off state takes priority when remembering settings is enabled.");
                    });
                    ui.horizontal_wrapped(|ui| {
                        audio_setting_switch(ui, &mut self.preferences.remember_devices, "Remember per microphone");
                        info(ui, "remembering", "Save microphone on/off, gain, listening volume and routing. Restore them when switching microphones or reopening Toolboks.");
                    });
                });
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
                muted(ui, "Default microphone");
                if let Some(default) = &self.preferences.default_input {
                    ui.add(egui::Label::new(RichText::new(&default.name).strong()).wrap());
                    let available = self.default_input_available();
                    status_pill(
                        ui,
                        if available {
                            "Connected"
                        } else {
                            "Disconnected"
                        },
                        available,
                    );
                    if !available {
                        muted(ui, "Waiting for this microphone. Toolboks will switch to it when it connects.");
                    }
                } else {
                    muted(ui, "No default set.");
                }
                ui.add_enabled_ui(enabled, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let uniquely_named = self
                            .selected_input
                            .and_then(|index| self.inputs.get(index))
                            .is_some_and(|selected| {
                                self.inputs
                                    .iter()
                                    .filter(|d| d.name == selected.name)
                                    .count()
                                    == 1
                            });
                        if ui
                            .add_enabled(uniquely_named, egui::Button::new("Use selected"))
                            .on_hover_text("Use selected microphone as default")
                            .clicked()
                        {
                            self.set_default_input();
                        }
                        if ui
                            .add_enabled(
                                self.preferences.default_input.is_some(),
                                egui::Button::new("Clear"),
                            )
                            .on_hover_text("Clear default microphone")
                            .clicked()
                        {
                            self.clear_default_input();
                        }
                    });
                });
                if !self.preferences_allowed {
                    muted(
                        ui,
                        "Review the Audio Gainer update to save settings on this PC.",
                    );
                    if ui
                        .add_enabled(
                            !tutorial::active(ui.ctx()),
                            egui::Button::new("Review update"),
                        )
                        .clicked()
                    {
                        self.request_settings_update();
                    }
                }
            });
        });
    }

    fn levels_ui(&mut self, ui: &mut egui::Ui, height: f32) {
        tutorial::region(ui, "aural-levels", |ui| {
            look::card(ui, ui.available_width(), height, |ui| {
                section(ui, "Input levels");
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 14.0;
                    for level in [
                        timeline::Level::Good,
                        timeline::Level::Close,
                        timeline::Level::Clipping,
                    ] {
                        // A dot and the word: the colour is never the only signal.
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 5.0;
                            let (r, _) =
                                ui.allocate_exact_size(egui::vec2(8.0, 12.0), egui::Sense::hover());
                            ui.painter().circle_filled(r.center(), 3.5, level.color());
                            ui.label(RichText::new(level.label()).size(11.0).color(look::MUTED));
                        });
                    }
                });
                let elapsed = self.meter_started.elapsed();
                let graph_height = (height - 174.0).max(160.0);
                self.timeline.draw(ui, graph_height, elapsed);
                ui.separator();
                ui.horizontal(|ui| {
                    for (name, level) in ["Left", "Right"].into_iter().zip(self.levels) {
                        ui.vertical(|ui| {
                            muted(ui, name);
                            ui.add_sized(
                                [(ui.available_width() * 0.5 - 4.0).max(70.0), 28.0],
                                egui::Label::new(
                                    RichText::new(format!("{level:.1} dB")).size(22.0).strong(),
                                ),
                            );
                        });
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(self.timeline.event_text(elapsed, false))
                            .size(11.0)
                            .color(look::MUTED),
                    );
                    ui.label(
                        RichText::new(self.timeline.event_text(elapsed, true))
                            .size(11.0)
                            .color(look::MUTED),
                    );
                });
            });
        });
    }

    fn route_ui(&mut self, ui: &mut egui::Ui, index: usize, interactive: bool, height: f32) {
        tutorial::region(ui, ["aural-monitor", "aural-cable"][index], |ui| {
            look::card(ui, ui.available_width(), height, |ui| {
                section(ui, ROUTE_LABELS[index]);
                let cable_present = self.outputs.iter().any(|d| virtual_cable(&d.name));
                let selected_is_cable = self.selected_outputs[index]
                    .and_then(|i| self.outputs.get(i))
                    .map(|d| virtual_cable(&d.name));
                if index == 1 {
                    ui.label(
                        RichText::new(if cable_present {
                            "Virtual cable found"
                        } else {
                            "Virtual cable not found"
                        })
                        .color(if cable_present {
                            look::GREEN
                        } else {
                            look::MUTED
                        }),
                    );
                } else {
                    muted(ui, "Use headphones to avoid feedback.");
                }
                let changed = ui
                    .add_enabled_ui(interactive, |ui| {
                        picker(
                            ui,
                            ("aural-output", index),
                            &self.outputs,
                            &mut self.selected_outputs[index],
                            if index == 0 {
                                "Choose headphones"
                            } else {
                                "Choose a virtual cable"
                            },
                            |d| index == 1 || !virtual_cable(&d.name),
                        )
                    })
                    .inner;
                if changed {
                    let live = self.routes[index].is_some();
                    self.stop_route(index);
                    self.route_errors[index] = if live {
                        self.enable_route(index).err()
                    } else {
                        None
                    };
                }
                if index == 0 {
                    ui.add_enabled_ui(interactive, |ui| {
                        ui.spacing_mut().slider_width = (ui.available_width() - 60.0).max(80.0);
                        ui.add(
                            egui::Slider::new(&mut self.volume, 0.0..=100.0)
                                .suffix("%")
                                .fixed_decimals(0),
                        )
                        .widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::Role::Slider,
                                ui.is_enabled(),
                                "Listening volume",
                            )
                        });
                    });
                    self.monitor_level
                        .store((self.volume / 100.0).to_bits(), Relaxed);
                }
                ui.horizontal(|ui| {
                    let live = self.routes[index].is_some();
                    let mut on = live;
                    if ui
                        .add_enabled_ui(
                            interactive && self.selected_outputs[index].is_some(),
                            |ui| named_switch(ui, &mut on, ROUTE_LABELS[index]),
                        )
                        .inner
                    {
                        self.route_errors[index] = None;
                        if on {
                            self.route_errors[index] = self.enable_route(index).err();
                        } else {
                            self.route_stopped(index);
                        }
                    }
                    let live = self.routes[index].is_some();
                    status_pill(ui, if live { "Live" } else { "Off" }, live);
                });
                if index == 1 {
                    let state = CableState::of(
                        cable_present,
                        selected_is_cable,
                        self.input.is_some(),
                        self.routes[index].is_some(),
                    );
                    muted(ui, state.headline());
                    if state == CableState::Missing {
                        ui.add_enabled_ui(interactive, |ui| {
                            ui.visuals_mut().hyperlink_color = look::DATA_MEMORY;
                            ui.hyperlink_to("Get VB-CABLE", VB_CABLE);
                        });
                    }
                    ui.label(CABLE_EXPLAINED).on_hover_text(state.hint());
                }
                if let Some(fault) = &self.route_errors[index] {
                    fault_ui(ui, ("aural-route-fault", index), fault);
                }
            });
        });
    }
}

/// Applies the Toolboks look (fonts, text sizes, spacing, colours) the first time the add-on
/// draws. The add-on runs in its own process, so the host's own set-up does not reach it.
fn style_once(ctx: &egui::Context) {
    let id = egui::Id::NULL.with("toolboks-style-applied");
    if !ctx.data(|data| data.get_temp::<bool>(id)).unwrap_or(false) {
        toolboks_shared::ui::apply_style(ctx);
        ctx.data_mut(|data| data.insert_temp(id, true));
    }
}

impl NativeAddon for Aural {
    fn set_capabilities(&mut self, capabilities: &[Capability]) {
        self.allowed = capabilities.contains(&Capability::LiveAudio);
        let preferences_allowed = capabilities.contains(&Capability::LocalPreferences);
        if preferences_allowed != self.preferences_allowed {
            self.preferences_loaded = false;
        }
        self.preferences_allowed = preferences_allowed;
        if !self.allowed || !self.preferences_allowed {
            self.discovery = None;
        }
        if !self.allowed && self.input.is_some() {
            self.cancel();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, prefs: &mut BTreeMap<String, serde_json::Value>) -> bool {
        style_once(ui.ctx());
        let tutorial_active = tutorial::active(ui.ctx());
        if !tutorial_active {
            self.begin_ui_preferences(prefs);
        }
        let interactive = self.allowed && !tutorial_active;
        if interactive && !self.loaded {
            self.refresh();
        }
        egui::ScrollArea::vertical().id_salt("aural-workspace")
            .animated(!tutorial_active).auto_shrink([false, false]).show(ui, |ui| {
            ui.label(RichText::new("Live audio").size(look::TEXT_TITLE).strong());
            muted(ui, "Check your microphone level and choose where the sound goes. Switching a route on also turns on the microphone.");
            if let Some(message) = self.message.as_deref() { if look::banner(ui, message) { self.message = None; } }
            ui.add_space(look::SPACE_M);
            let width = ui.available_width();
            let height = ui.available_height().max(520.0);
            if width >= 900.0 {
                ui.spacing_mut().item_spacing.x = look::GAP;
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(egui::vec2(300.0, height), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_width(300.0);
                        self.input_ui(ui, interactive);
                        ui.add_space(look::GAP);
                        self.settings_card(ui, interactive, ui.available_height().max(0.0));
                    });
                    ui.allocate_ui_with_layout(egui::vec2(width - 640.0 - 2.0 * look::GAP, height), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_width(width - 640.0 - 2.0 * look::GAP);
                        self.levels_ui(ui, height);
                    });
                    ui.allocate_ui_with_layout(egui::vec2(340.0, height), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_width(340.0);
                        self.route_ui(ui, 0, interactive, (height - look::GAP) * 0.48);
                        ui.add_space(look::GAP);
                        self.route_ui(ui, 1, interactive, ui.available_height().max(0.0));
                    });
                });
            } else {
                self.levels_ui(ui, 400.0);
                ui.add_space(look::GAP); self.input_ui(ui, interactive);
                ui.add_space(look::GAP);
                self.settings_ui(ui, interactive);
                for index in 0..2 { ui.add_space(look::GAP); self.route_ui(ui, index, interactive, 0.0); }
            }
        });
        // A switch can start audio during this frame; scheduling here also handles that case.
        if self.ui_active && self.input.is_some() && !tutorial_active {
            ui.request_repaint_after(timeline::FRAME);
        }
        if tutorial_active {
            false
        } else {
            self.finish_ui_preferences(prefs)
        }
    }
    fn load_preferences(&mut self, prefs: &BTreeMap<String, serde_json::Value>) {
        if self.preferences_loaded {
            return;
        }
        self.preferences_loaded = true;
        if self.preferences_allowed {
            self.preferences = prefs
                .get(settings::KEY)
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            self.want_input = self.preferences.startup_enabled();
            self.ensure_discovery();
        }
    }
    fn take_host_request(&mut self) -> Option<toolboks_addon_sdk::HostRequest> {
        std::mem::take(&mut self.settings_update_requested)
            .then_some(toolboks_addon_sdk::HostRequest::ReviewUpdate)
    }
    fn tick(&mut self, _: &egui::Context) {
        self.tick_background();
        self.sample();
    }

    fn suspend(&mut self) {
        self.ui_active = false;
        if let Some(input) = &self.input {
            input.signal.metering.store(false, Relaxed);
        }
        self.timeline.reset(false);
        self.levels = [-60.0; 2];
    }

    fn resume(&mut self) {
        self.ui_active = true;
        if let Some(input) = &self.input {
            for peak in &input.signal.peaks {
                peak.store(0, Relaxed);
            }
            input.signal.metering.store(true, Relaxed);
        }
        self.last_sample = Instant::now();
        self.meter_started = self.last_sample;
        self.timeline.reset(self.input.is_some());
    }

    fn has_background_work(&self) -> bool {
        self.input.is_some() || self.discovery.is_some()
    }

    fn tick_background(&mut self) {
        self.poll_discovery();
        let error = self
            .input
            .as_ref()
            .and_then(|input| input.signal.error.lock().unwrap().take());
        let mut lost = false;
        if let Some(raw) = error {
            self.cancel();
            self.mic_fault = Some(Fault::new(MICROPHONE_LOST, &raw));
            lost = true;
        }
        for index in 0..2 {
            let error = self.routes[index]
                .as_ref()
                .and_then(|route| route.error.lock().unwrap().take());
            if let Some(raw) = error {
                self.stop_route(index);
                self.route_errors[index] = Some(Fault::new(OUTPUT_LOST, &raw));
                lost = true;
            }
        }
        self.retry_lost_streams(lost, Instant::now());
    }

    fn cancel(&mut self) {
        for index in 0..2 {
            self.stop_route(index);
        }
        self.input = None;
        self.levels = [-60.0; 2];
        self.timeline.reset(false);
    }

    fn tutorial_location(&self) -> String {
        "0".into()
    }

    fn tutorial_steps(&self) -> Vec<TutorialStep> {
        [
            ("aural-input", "Choose your microphone", "Choose a physical input, then enable it. Input gain changes the level sent to both routes."),
            ("aural-settings", "Remember your setup", "Startup activation is optional. Save gain and routing separately for each microphone, and choose a default whose name stays saved while disconnected. Toolboks switches back when it reconnects."),
            ("aural-levels", "Check your levels", "Watch the last 10 seconds of input levels. Lower your gain when levels are close to clipping or clipping. Left and right readouts show the current levels."),
            ("aural-monitor", "Hear yourself", "Choose headphones and start the route. Listening volume affects only your headphones."),
            ("aural-cable", "Send audio to another app", "Install VB-CABLE separately. Choose CABLE Input here and CABLE Output as the microphone in your other app."),
        ].into_iter().map(|(id, title, text)| TutorialStep::new("0", id, title, text)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_frame(
        ctx: &egui::Context,
        addon: &mut Aural,
        width: f32,
        enabled: bool,
        events: Vec<egui::Event>,
    ) -> Vec<(String, egui::Rect)> {
        fn texts(shape: &egui::Shape, result: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(text) => result.push((
                    text.galley.text().to_owned(),
                    text.galley.rect.translate(text.pos.to_vec2()),
                )),
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        texts(shape, result);
                    }
                }
                _ => {}
            }
        }
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 1000.0),
                )),
                events,
                ..Default::default()
            },
            |ui| addon.settings_ui(ui, enabled),
        );
        let mut result = Vec::new();
        for shape in &output.shapes {
            texts(&shape.shape, &mut result);
        }
        output.textures_delta.clear();
        result
    }

    #[test]
    fn audio_settings_controls_change_only_when_interactive() {
        let ctx = egui::Context::default();
        look::apply_style(&ctx);
        let mut addon = Aural {
            preferences_allowed: true,
            ..Default::default()
        };
        let texts = settings_frame(&ctx, &mut addon, 290.0, true, Vec::new());
        let startup = texts
            .iter()
            .find(|(text, _)| text == "Turn on when Toolboks starts")
            .unwrap()
            .1
            .center();
        assert!(!addon.preferences.auto_enable);
        settings_frame(&ctx, &mut addon, 290.0, true, picker_pointer(startup, true));
        settings_frame(
            &ctx,
            &mut addon,
            290.0,
            true,
            picker_pointer(startup, false),
        );
        assert!(addon.preferences.auto_enable);
        let texts = settings_frame(&ctx, &mut addon, 290.0, true, Vec::new());
        let remember = texts
            .iter()
            .find(|(text, _)| text == "Remember per microphone")
            .unwrap()
            .1
            .center();
        settings_frame(
            &ctx,
            &mut addon,
            290.0,
            true,
            picker_pointer(remember, true),
        );
        settings_frame(
            &ctx,
            &mut addon,
            290.0,
            true,
            picker_pointer(remember, false),
        );
        assert!(addon.preferences.remember_devices);
        settings_frame(&ctx, &mut addon, 290.0, false, Vec::new());
        settings_frame(
            &ctx,
            &mut addon,
            290.0,
            false,
            picker_pointer(startup, true),
        );
        settings_frame(
            &ctx,
            &mut addon,
            290.0,
            false,
            picker_pointer(startup, false),
        );
        assert!(
            addon.preferences.auto_enable,
            "disabled settings must not change"
        );
        assert!(addon.input.is_none());
        assert!(
            addon.discovery.is_none(),
            "drawing settings must not start discovery"
        );
    }

    #[test]
    fn audio_settings_show_missing_default_and_fit_narrow_scaled_layouts() {
        for width in [270.0, 400.0, 610.0] {
            for scale in [1.0, 1.25, 2.0] {
                let ctx = egui::Context::default();
                ctx.set_pixels_per_point(scale);
                look::apply_style(&ctx);
                let mut addon = Aural {
                    preferences_allowed: true,
                    ..Default::default()
                };
                addon.preferences.default_input = Some(settings::DeviceIdentity {
                    name: "Disconnected Razer microphone".into(),
                });
                for _ in 0..2 {
                    settings_frame(&ctx, &mut addon, width, true, Vec::new());
                }
                let texts = settings_frame(&ctx, &mut addon, width, true, Vec::new());
                assert!(texts
                    .iter()
                    .any(|(text, _)| text == "Disconnected Razer microphone"));
                assert!(texts
                    .iter()
                    .any(|(text, _)| text.starts_with("Waiting for this microphone")));
                for (text, rect) in texts {
                    assert!(
                        rect.right() <= width + 1.0,
                        "{text} overflows at {width}, scale {scale}: {rect:?}"
                    );
                    assert!(rect.left() >= -1.0, "{text} starts outside settings card");
                }
                assert!(!addon.preferences.auto_enable);
                assert!(addon.discovery.is_none());
                assert!(addon.input.is_none());
            }
        }
    }

    #[test]
    fn audio_settings_startup_switch_can_be_toggled_by_keyboard() {
        let ctx = egui::Context::default();
        look::apply_style(&ctx);
        let mut addon = Aural {
            preferences_allowed: true,
            ..Default::default()
        };
        settings_frame(&ctx, &mut addon, 290.0, true, Vec::new());
        settings_frame(&ctx, &mut addon, 290.0, true, picker_key(egui::Key::Tab));
        assert!(ctx.memory(|memory| memory.focused()).is_some());
        settings_frame(&ctx, &mut addon, 290.0, true, picker_key(egui::Key::Space));
        assert!(addon.preferences.auto_enable);
        assert!(addon.input.is_none());
        assert!(addon.discovery.is_none());
    }

    #[test]
    fn audio_settings_tutorial_does_not_load_startup_preferences_or_start_audio() {
        let ctx = egui::Context::default();
        let mut addon = Aural::default();
        addon.set_capabilities(&[Capability::LiveAudio, Capability::LocalPreferences]);
        let mut prefs = BTreeMap::from([(
            settings::KEY.into(),
            serde_json::json!({
                "auto_enable": true,
                "remember_devices": true,
                "default_input": {"name": "Disconnected Razer microphone"}
            }),
        )]);
        let original = prefs.clone();
        for _ in 0..3 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 1000.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    tutorial::prepare(ui.ctx(), Some("aural-settings"), true);
                    assert!(!addon.ui(ui, &mut prefs));
                },
            );
            output.textures_delta.clear();
        }
        assert!(!addon.preferences_loaded);
        assert!(!addon.loaded);
        assert!(addon.discovery.is_none());
        assert!(addon.input.is_none());
        assert!(addon.routes.iter().all(Option::is_none));
        assert_eq!(prefs, original);
    }

    fn picker_frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        enabled: bool,
        refreshes: &mut usize,
    ) -> egui::Id {
        let mut id = egui::Id::NULL;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(500.0, 400.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                ui.add_enabled_ui(enabled, |ui| {
                    id = ui.make_persistent_id(egui::IdSalt::new("refresh-test-picker"));
                    refreshing_picker(
                        ui,
                        "refresh-test-picker",
                        "Microphone".into(),
                        |ui, opening| {
                            if opening {
                                *refreshes += 1;
                            }
                            ui.selectable_label(false, "Available microphone").clicked()
                        },
                    );
                });
            },
        );
        output.textures_delta.clear();
        id
    }

    fn picker_pointer(pos: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    fn picker_key(key: egui::Key) -> Vec<egui::Event> {
        vec![egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]
    }

    #[test]
    fn microphone_picker_refreshes_once_per_pointer_open_and_reopen() {
        let ctx = egui::Context::default();
        let mut refreshes = 0;
        let id = picker_frame(&ctx, Vec::new(), true, &mut refreshes);
        let center = ctx.read_response(id).unwrap().rect.center();
        assert_eq!(refreshes, 0);
        picker_frame(&ctx, picker_pointer(center, true), true, &mut refreshes);
        picker_frame(&ctx, picker_pointer(center, false), true, &mut refreshes);
        assert!(egui::ComboBox::is_open(&ctx, id));
        assert_eq!(refreshes, 1);
        for _ in 0..3 {
            picker_frame(&ctx, Vec::new(), true, &mut refreshes);
        }
        assert_eq!(refreshes, 1, "an open list must not rescan every frame");
        picker_frame(&ctx, picker_key(egui::Key::Escape), true, &mut refreshes);
        assert!(!egui::ComboBox::is_open(&ctx, id));
        picker_frame(&ctx, picker_pointer(center, true), true, &mut refreshes);
        picker_frame(&ctx, picker_pointer(center, false), true, &mut refreshes);
        assert!(egui::ComboBox::is_open(&ctx, id));
        assert_eq!(refreshes, 2, "reopening must refresh the current inventory");
    }

    #[test]
    fn microphone_picker_refreshes_on_keyboard_open_but_not_when_disabled() {
        let ctx = egui::Context::default();
        let mut refreshes = 0;
        let id = picker_frame(&ctx, Vec::new(), true, &mut refreshes);
        ctx.memory_mut(|memory| memory.request_focus(id));
        picker_frame(&ctx, picker_key(egui::Key::Enter), true, &mut refreshes);
        assert!(egui::ComboBox::is_open(&ctx, id));
        assert_eq!(refreshes, 1);
        picker_frame(&ctx, Vec::new(), true, &mut refreshes);
        assert_eq!(refreshes, 1);
        picker_frame(&ctx, picker_key(egui::Key::Escape), true, &mut refreshes);
        picker_frame(&ctx, Vec::new(), false, &mut refreshes);
        let center = ctx.read_response(id).unwrap().rect.center();
        picker_frame(&ctx, picker_pointer(center, true), false, &mut refreshes);
        picker_frame(&ctx, picker_pointer(center, false), false, &mut refreshes);
        assert!(!egui::ComboBox::is_open(&ctx, id));
        assert_eq!(refreshes, 1, "disabled controls must not enumerate devices");
    }

    #[test]
    fn microphone_refresh_preserves_endpoint_identity_across_reorder_and_names() {
        let old = [(10, "Microphone"), (20, "Microphone")];
        let fresh = [(20, "Renamed microphone"), (30, "New"), (10, "Microphone")];
        assert_eq!(
            refreshed_selection(&old, Some(1), &fresh, |a, b| a.0 == b.0),
            Some(0)
        );
        // A replacement with the same display name must not inherit selection.
        assert_eq!(
            refreshed_selection(&old, Some(1), &[(30, "Microphone")], |a, b| a.0 == b.0),
            None
        );
    }

    #[test]
    fn microphone_refresh_requires_unambiguous_selection_without_endpoint_ids() {
        let old = ["USB microphone", "USB microphone"];
        assert_eq!(
            refreshed_selection(&old, Some(0), &["USB microphone"], |a, b| a == b),
            None
        );
        assert_eq!(
            refreshed_selection(&["USB microphone"], Some(0), &old, |a, b| a == b),
            None
        );
        assert_eq!(refreshed_selection(&old, None, &old, |a, b| a == b), None);
    }

    #[test]
    fn microphone_refresh_failure_retains_state_and_retry_clears_only_refresh_error() {
        let mut addon = Aural {
            selected_outputs: [Some(2), Some(4)],
            route_errors: [Some("Existing route error".into()), None],
            message: Some("Existing audio error".into()),
            ..Default::default()
        };
        addon.apply_microphone_refresh(Err("Enumeration unavailable".into()));
        assert_eq!(
            addon.microphone_refresh_error.as_deref(),
            Some("Enumeration unavailable")
        );
        assert_eq!(addon.selected_outputs, [Some(2), Some(4)]);
        addon.apply_microphone_refresh(Ok(Vec::new()));
        assert!(addon.microphone_refresh_error.is_none());
        assert_eq!(addon.message.as_deref(), Some("Existing audio error"));
        assert_eq!(
            addon.route_errors[0].as_ref().map(|f| f.short.as_str()),
            Some("Existing route error")
        );
        assert_eq!(addon.selected_outputs, [Some(2), Some(4)]);
        assert!(!addon.loaded);
    }

    #[test]
    fn gain_updates_the_stereo_peaks_and_latches_clipping() {
        let signal = Signal::default();
        signal.gain.store(2.0_f32.to_bits(), Relaxed);
        signal.metering.store(true, Relaxed);
        process(&[0.25_f32, -0.75], 2, &signal);
        assert_eq!(
            signal
                .peaks
                .each_ref()
                .map(|v| f32::from_bits(v.load(Relaxed))),
            [0.5, 1.5]
        );
        assert!(signal.clipped.load(Relaxed));
        process(&[0.0_f32; 2], 2, &signal);
        assert!(signal.clipped.load(Relaxed));
        assert!((db(gain(-18.0)) + 18.0).abs() < 0.001);
    }

    #[test]
    fn background_audio_still_forwards_without_metering() {
        let signal = Signal::default();
        signal.gain.store(2.0_f32.to_bits(), Relaxed);
        let bridge = Arc::new(Mutex::new(routing::Bridge::new(100, 100)));
        signal.routes.lock().unwrap()[1] = Some(bridge.clone());
        process(&[0.75_f32; 8], 1, &signal);
        assert_eq!(signal.peaks[0].load(Relaxed), 0);
        assert!(!signal.clipped.load(Relaxed));
        assert_eq!(bridge.lock().unwrap().next(), [1.0; 2]);
    }

    #[test]
    fn streams_open_on_the_one_current_entry_for_an_endpoint() {
        // Endpoint IDs stand in for devices: a reopened stream uses today's entry for the same
        // endpoint, never an old one and never a guess between duplicates.
        let same = |a: &(&str, u32), b: &(&str, u32)| a.0 == b.0;
        let listed = ("{mic}", 1);
        assert_eq!(
            fresh_match(&listed, [("{spk}", 2), ("{mic}", 2)].into_iter(), same),
            Some(("{mic}", 2))
        );
        assert_eq!(fresh_match(&listed, [("{spk}", 2)].into_iter(), same), None);
        assert_eq!(
            fresh_match(&listed, [("{mic}", 2), ("{mic}", 3)].into_iter(), same),
            None
        );
    }

    #[test]
    fn recognizes_standard_and_additional_vb_cable_endpoints() {
        assert!(virtual_cable("CABLE Input (VB-Audio Virtual Cable)"));
        assert!(virtual_cable("CABLE-A Output (VB-Audio Cable A)"));
        assert!(!virtual_cable("Headphones (USB Audio)"));
    }

    #[test]
    fn walkthrough_targets_render_at_both_widths_without_opening_audio() {
        for width in [610.0, 900.0] {
            let ctx = egui::Context::default();
            let mut addon = Aural::default();
            addon.set_capabilities(&[Capability::LiveAudio]);
            let mut prefs = BTreeMap::new();
            for step in addon.tutorial_steps() {
                for _ in 0..3 {
                    let mut output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(width, 720.0),
                            )),
                            ..Default::default()
                        },
                        |ui| {
                            tutorial::prepare(ui.ctx(), Some(&step.target), true);
                            addon.ui(ui, &mut prefs);
                        },
                    );
                    output.textures_delta.clear();
                }
                let anchor = tutorial::anchor(&ctx)
                    .unwrap_or_else(|| panic!("missing anchor for {} at {width}", step.title));
                assert!(
                    anchor.right() <= width + 1.0,
                    "{} overflows at {width}",
                    step.title
                );
                assert!(!addon.loaded && addon.input.is_none());
                assert!(addon.routes.iter().all(Option::is_none));
            }
        }
    }
    #[test]
    fn page_text_and_cards_fit_three_columns_and_stacked_layouts() {
        fn texts(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(t) => out.push((
                    t.galley.text().into(),
                    t.galley.rect.translate(t.pos.to_vec2()),
                )),
                egui::Shape::Vec(v) => {
                    for s in v {
                        texts(s, out);
                    }
                }
                _ => {}
            }
        }
        for width in [420.0, 824.0, 860.0, 899.0, 900.0, 1000.0, 1106.0] {
            let ctx = egui::Context::default();
            let mut addon = Aural::default();
            let mut prefs = BTreeMap::new();
            for _ in 0..3 {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 720.0),
                        )),
                        ..Default::default()
                    },
                    |ui| {
                        addon.ui(ui, &mut prefs);
                    },
                );
                output.textures_delta.clear();
                let mut painted = Vec::new();
                for shape in output.shapes {
                    texts(&shape.shape, &mut painted);
                }
                for (text, rect) in painted {
                    assert!(
                        rect.right() <= width + 1.0 && rect.left() >= -1.0,
                        "{text} overflows at {width}: {rect:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn device_text_is_kept_apart_and_never_doubles_punctuation() {
        let fault = Fault::new(MICROPHONE_LOST, "The device is no longer available.. ");
        assert_eq!(
            fault.short,
            "Microphone disconnected. Reconnect it, then enable it again."
        );
        assert_eq!(
            fault.detail.as_deref(),
            Some("The device is no longer available")
        );
        assert!(!fault.short.contains(".."));
        assert_eq!(Fault::new(OUTPUT_LOST, "  ").detail, None);
    }

    #[test]
    fn cable_is_found_from_the_devices_not_from_the_choice() {
        // Ordinary speakers chosen, no cable installed: still missing.
        assert_eq!(
            CableState::of(false, Some(false), true, false),
            CableState::Missing
        );
        // Cable installed, nothing chosen.
        assert_eq!(
            CableState::of(true, None, true, false),
            CableState::NoDestination
        );
        // Cable installed, ordinary speakers chosen.
        assert_eq!(
            CableState::of(true, Some(false), true, false),
            CableState::NotACable
        );
        // Cable chosen, microphone on or off, and running.
        assert_eq!(
            CableState::of(true, Some(true), true, false),
            CableState::Ready
        );
        assert_eq!(
            CableState::of(true, Some(true), false, false),
            CableState::ReadyInputOff
        );
        assert_eq!(
            CableState::of(true, Some(true), true, true),
            CableState::Running
        );
        // Nothing chosen and no cable.
        assert_eq!(
            CableState::of(false, None, false, false),
            CableState::Missing
        );
    }

    #[test]
    fn every_state_has_its_own_words() {
        let all = [
            CableState::Missing,
            CableState::NoDestination,
            CableState::NotACable,
            CableState::Ready,
            CableState::ReadyInputOff,
            CableState::Running,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.headline(), b.headline());
            }
            assert!(!a.hint().is_empty());
        }
        assert!(CableState::ReadyInputOff
            .headline()
            .contains("microphone is off"));
    }
}
