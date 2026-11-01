//! The plugin's own actions for the M131E dial screen (the official app's display modes):
//! clock, timer, CPU load, memory use and CPU temperature; plus a stopwatch, a counter, a command's output and a
//! value from MQTT.
//!
//! On a MasterHUB dial the screen is drawn here at its full 240x240 and sent straight to the
//! module; anywhere else (a key, another device) the image goes through OpenDeck's setImage.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::Cursor;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine;
use chrono::{Datelike, Local, Timelike};
use image::{imageops::FilterType, DynamicImage, Rgb, RgbImage};
use serde_json::{json, Value};

const SIZE: u32 = 240;
const PREVIEW: u32 = 144; // what OpenDeck gets for its UI or other devices
const WHITE: Rgb<u8> = Rgb([255, 255, 255]);
const GREY: Rgb<u8> = Rgb([0x78, 0x78, 0x7e]); // secondary text colour of the official app
const TIMER_GREEN: Rgb<u8> = Rgb([0x00, 0xc6, 0x96]); // running timer background, as in the app
const TIMER_DONE: Rgb<u8> = Rgb([0x20, 0x27, 0x22]);
const ALARM_RED: Rgb<u8> = Rgb([0xc0, 0x1a, 0x1a]);
/// A key held this long resets the stopwatch or the counter instead.
const LONG_PRESS: Duration = Duration::from_millis(600);
/// Text longer than this many characters moves along instead of shrinking, at this scale and this pace.
const SCROLL_FROM: u32 = 12;
const SCROLL_SCALE: u32 = 4;
pub const SCROLL_STEP: Duration = Duration::from_millis(150);

/// When the plugin started: moving text takes its place from it.
fn epoch() -> Instant {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Samples a graph keeps: one a tick (about a second), so the last 2 minutes.
const GRAPH_SAMPLES: usize = 120;
/// Longest the output action's command may run.
const OUTPUT_LIMIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Clock,
    AnalogClock,
    Timer,
    Cpu,
    Memory,
    CpuTemp,
    Gpu,
    GpuTemp,
    Stopwatch,
    Counter,
    Output,
    Mqtt,
}

/// The screens "next screen" presses go through.
const CYCLE: [Kind; 7] =
    [Kind::Clock, Kind::AnalogClock, Kind::Cpu, Kind::Memory, Kind::CpuTemp, Kind::Gpu, Kind::GpuTemp];

/// Opens an app, file or web address with the system's own opener, without a shell.
fn open(target: &str) {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(windows)]
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut cmd = std::process::Command::new("xdg-open");
    // started, not waited for: some openers wait for the app they open
    match cmd
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => crate::command::reap(child, format!("open {target}")),
        Err(e) => eprintln!("cannot open {target}: {e}"),
    }
}

impl Kind {
    /// The name in the action's uuid ("cpu"), as `from_uuid` reads it back.
    fn name(self) -> &'static str {
        match self {
            Kind::Clock => "clock",
            Kind::AnalogClock => "analogclock",
            Kind::Timer => "timer",
            Kind::Cpu => "cpu",
            Kind::Memory => "memory",
            Kind::CpuTemp => "cputemp",
            Kind::Gpu => "gpu",
            Kind::GpuTemp => "gputemp",
            Kind::Stopwatch => "stopwatch",
            Kind::Counter => "counter",
            Kind::Output => "output",
            Kind::Mqtt => "mqtt",
        }
    }

    fn from_uuid(uuid: &str) -> Option<Kind> {
        Some(match uuid.rsplit('.').next()? {
            "clock" => Kind::Clock,
            "analogclock" => Kind::AnalogClock,
            "timer" => Kind::Timer,
            "cpu" => Kind::Cpu,
            "memory" => Kind::Memory,
            "cputemp" => Kind::CpuTemp,
            "gpu" => Kind::Gpu,
            "gputemp" => Kind::GpuTemp,
            "stopwatch" => Kind::Stopwatch,
            "counter" => Kind::Counter,
            "output" => Kind::Output,
            "mqtt" => Kind::Mqtt,
            _ => return None,
        })
    }
}

pub enum Out {
    /// A message for OpenDeck.
    Ws(Value),
    /// A full-size JPEG for one encoder slot of the MasterHUB dial with this device id.
    Dial { device: String, position: u8, jpeg: Vec<u8> },
}

#[derive(Default)]
struct Timer {
    duration: u32, // seconds
    ends: Option<Instant>,
    done_until: Option<Instant>,
}

/// Time counted while running, paused in between.
#[derive(Default)]
struct Stopwatch {
    before: Duration,
    since: Option<Instant>,
}

impl Stopwatch {
    fn elapsed(&self, now: Instant) -> Duration {
        self.before + self.since.map_or(Duration::ZERO, |s| now.saturating_duration_since(s))
    }

    fn toggle(&mut self, now: Instant) {
        match self.since.take() {
            Some(s) => self.before += now.saturating_duration_since(s),
            None => self.since = Some(now),
        }
    }
}

/// The output action's last line, when its command runs next, and the run under way.
#[derive(Default)]
struct Output {
    text: String,
    next: Option<Instant>,
    running: Option<mpsc::Receiver<String>>,
}

struct Inst {
    kind: Kind,
    context: Value,
    device: String,
    encoder: bool,
    position: u8,
    /// The action's settings as OpenDeck last gave them.
    settings: Value,
    /// When a key went down (stopwatch, counter: a long press resets).
    pressed: Option<Instant>,
    timer: Timer,
    stopwatch: Stopwatch,
    count: i64,
    output: Output,
    /// The value's last samples, oldest first, when the action draws a graph.
    history: VecDeque<f32>,
    /// The value shown, as a number (for the graph and the alarm), as of the last tick.
    value: Option<f32>,
    last: String, // what was drawn last, to skip identical frames
    /// Its screen moves (long text): redrawn every `SCROLL_STEP`, not only every second.
    scrolling: bool,
    /// Pinned by the "Dial screen" setting: draws the whole dial, whatever its actions.
    pinned: bool,
}

/// Out::Dial position of a pinned screen: shown whatever the dial's current function.
pub const WHOLE_DIAL: u8 = u8::MAX;

#[derive(Default)]
pub struct Actions {
    insts: HashMap<String, Inst>,
    sensors: Option<Sensors>,
    /// The last message on each topic the MQTT value actions read (`topics`), from the MQTT connection.
    pub mqtt_values: HashMap<String, String>,
}

struct Sensors {
    system: sysinfo::System,
    components: sysinfo::Components,
    /// When the temperature sensors were last listed (None: never).
    listed: Option<Instant>,
}

impl Actions {
    /// Whether one of our actions draws this dial slot's screen (OpenDeck's own image is ignored then).
    pub fn owns_dial(&self, device: &str, position: u8, is_dial: impl Fn(&str) -> bool) -> bool {
        is_dial(device)
            && self.insts.values().any(|i| i.encoder && i.device == device && (i.pinned || i.position == position))
    }

    /// The MQTT topics the MQTT value actions read (exact topics: wildcards are left out).
    pub fn topics(&self) -> BTreeSet<String> {
        self.insts
            .values()
            .filter(|i| i.kind == Kind::Mqtt)
            .filter_map(|i| i.settings["topic"].as_str().map(str::trim))
            .filter(|t| !t.is_empty() && !t.contains(['+', '#']))
            .map(str::to_string)
            .collect()
    }

    /// Whether the "Dial screen" setting draws this dial.
    pub fn pinned(&self, device: &str) -> bool {
        self.insts.contains_key(&format!("pin/{device}"))
    }

    /// Makes each dial show its screen (an action name like "cpu"; None: the dial's own actions), as a graph or
    /// not, and drops pins of dials that are gone. Called about once a second.
    pub fn pin_dials(&mut self, dials: &[(String, Option<String>)], graph: bool) {
        let wanted: HashMap<String, Kind> =
            dials.iter().filter_map(|(d, s)| Some((format!("pin/{d}"), Kind::from_uuid(s.as_deref()?)?))).collect();
        self.insts.retain(|id, i| !i.pinned || wanted.get(id) == Some(&i.kind));
        for (id, &kind) in &wanted {
            let d = id.trim_start_matches("pin/").to_string();
            let i = self.insts.entry(id.clone()).or_insert_with(|| Inst {
                pinned: true,
                ..Inst::new(kind, Value::Null, d, true, WHOLE_DIAL, Value::Null)
            });
            if i.settings["graph"] != graph {
                i.settings = json!({ "graph": graph });
                i.history.clear();
            }
        }
    }

    /// Handles an OpenDeck event addressed to one of our actions.
    pub fn handle(&mut self, msg: &Value, is_dial: impl Fn(&str) -> bool) -> Vec<Out> {
        let Some(id) = msg["context"]
            .as_str()
            .map(str::to_string)
            .or_else(|| msg["context"].as_object().map(|_| msg["context"].to_string()))
        else {
            return Vec::new();
        };
        match msg["event"].as_str() {
            Some("willAppear") => {
                let Some(action) = msg["action"].as_str().and_then(Kind::from_uuid) else { return Vec::new() };
                // a screen chosen by "next screen" presses replaces the action's own
                let kind = msg["payload"]["settings"]["screen"]
                    .as_str()
                    .and_then(Kind::from_uuid)
                    .filter(|k| CYCLE.contains(k) && CYCLE.contains(&action))
                    .unwrap_or(action);
                let settings = msg["payload"]["settings"].clone();
                let mut inst = Inst::new(
                    kind,
                    msg["context"].clone(),
                    msg["device"].as_str().unwrap_or_default().to_string(),
                    msg["payload"]["controller"] == "Encoder",
                    msg["payload"]["coordinates"]["column"].as_u64().unwrap_or(0) as u8,
                    settings,
                );
                inst.timer.duration = inst.settings["duration"].as_u64().map_or(60, |d| d.clamp(10, 5999) as u32);
                inst.count = number(&inst.settings["count"]).unwrap_or(0);
                self.insts.insert(id.clone(), inst);
            }
            Some("willDisappear") => {
                self.insts.remove(&id);
                return Vec::new();
            }
            Some("didReceiveSettings") => {
                let Some(i) = self.insts.get_mut(&id) else { return Vec::new() };
                i.settings = msg["payload"]["settings"].clone();
                i.count = number(&i.settings["count"]).unwrap_or(i.count);
                i.output.next = None; // a new command runs right away
            }
            Some(event @ ("keyDown" | "dialDown")) => {
                let Some(i) = self.insts.get_mut(&id) else { return Vec::new() };
                let now = Instant::now();
                match i.kind {
                    Kind::Timer => {
                        // Press starts the countdown; pressing again stops and resets it (like the app).
                        let t = &mut i.timer;
                        t.done_until = None;
                        t.ends = match t.ends {
                            Some(_) => None,
                            None => Some(now + Duration::from_secs(t.duration as u64)),
                        };
                    }
                    // a key's press counts on release: held long, it resets
                    Kind::Stopwatch | Kind::Counter if event == "keyDown" => {
                        i.pressed = Some(now);
                        return Vec::new();
                    }
                    Kind::Stopwatch => i.stopwatch.toggle(now),
                    Kind::Counter => return self.set_count(&id, 0, &is_dial), // the dial's press resets
                    Kind::Output => i.output.next = None,                     // runs on the next tick
                    _ => return self.press(&id, &msg["payload"]["settings"], &is_dial),
                }
            }
            Some("keyUp") => {
                let Some(i) = self.insts.get_mut(&id) else { return Vec::new() };
                let Some(down) = i.pressed.take() else { return Vec::new() };
                let long = down.elapsed() >= LONG_PRESS;
                match i.kind {
                    Kind::Stopwatch if long => i.stopwatch = Stopwatch::default(),
                    Kind::Stopwatch => i.stopwatch.toggle(Instant::now()),
                    Kind::Counter => {
                        let count = if long { 0 } else { i.count + step(&i.settings) };
                        return self.set_count(&id, count, &is_dial);
                    }
                    _ => return Vec::new(),
                }
            }
            Some("dialRotate") => {
                let Some(i) = self.insts.get_mut(&id) else { return Vec::new() };
                let ticks = msg["payload"]["ticks"].as_i64().unwrap_or(0);
                match i.kind {
                    Kind::Timer if i.timer.ends.is_none() => {
                        i.timer.duration = (i.timer.duration as i64 + ticks * 10).clamp(10, 5999) as u32;
                        i.timer.done_until = None;
                        let save = json!({ "event": "setSettings", "context": i.context, "payload": { "duration": i.timer.duration } });
                        let mut out = vec![Out::Ws(save)];
                        out.extend(self.draw(&id, &is_dial));
                        return out;
                    }
                    // turned while stopped: back to zero
                    Kind::Stopwatch if i.stopwatch.since.is_none() => i.stopwatch = Stopwatch::default(),
                    Kind::Counter => {
                        let count = i.count + ticks * step(&i.settings);
                        return self.set_count(&id, count, &is_dial);
                    }
                    _ => return Vec::new(),
                }
            }
            _ => return Vec::new(),
        }
        self.draw(&id, &is_dial)
    }

    /// Sets a counter, and every counter with the same label (one +1 key, one -1 key, the dial showing it), and
    /// keeps the number in their settings.
    fn set_count(&mut self, id: &str, count: i64, is_dial: &impl Fn(&str) -> bool) -> Vec<Out> {
        let group = |id: &str, i: &Inst| match i.settings["label"].as_str().map(str::trim).filter(|l| !l.is_empty()) {
            Some(label) => label.to_uppercase(),
            None => id.to_string(),
        };
        let Some(wanted) = self.insts.get(id).map(|i| group(id, i)) else { return Vec::new() };
        let ids: Vec<String> = self
            .insts
            .iter()
            .filter(|(k, i)| i.kind == Kind::Counter && !i.pinned && group(k, i) == wanted)
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::new();
        for k in ids {
            let i = self.insts.get_mut(&k).expect("listed above");
            i.count = count;
            if !i.settings.is_object() {
                i.settings = json!({});
            }
            i.settings["count"] = json!(count);
            out.push(Out::Ws(json!({ "event": "setSettings", "context": i.context, "payload": i.settings })));
            out.extend(self.draw(&k, is_dial));
        }
        out
    }

    /// A press on an info screen (clock, CPU, …), as set in its inspector: open an app, file or address, run a
    /// command, or go to the next screen on this key.
    fn press(&mut self, id: &str, settings: &Value, is_dial: &impl Fn(&str) -> bool) -> Vec<Out> {
        let target = settings["target"].as_str().unwrap_or_default().trim().to_string();
        match settings["press"].as_str().unwrap_or_default() {
            "open" if !target.is_empty() => open(&target),
            "command" if !target.is_empty() => crate::command::spawn(&target),
            "next" => {
                let Some(i) = self.insts.get_mut(id) else { return Vec::new() };
                let at = CYCLE.iter().position(|k| *k == i.kind).unwrap_or(0);
                i.kind = CYCLE[(at + 1) % CYCLE.len()];
                i.history.clear();
                // keep the choice: OpenDeck replaces all settings at once
                let mut keep = settings.clone();
                keep["screen"] = json!(i.kind.name());
                i.settings = keep.clone();
                let mut out = vec![Out::Ws(json!({ "event": "setSettings", "context": i.context, "payload": keep }))];
                out.extend(self.draw(id, is_dial));
                return out;
            }
            _ => {}
        }
        Vec::new()
    }

    /// Called about once a second: advances timers and redraws whatever changed.
    pub fn tick(&mut self, is_dial: impl Fn(&str) -> bool) -> Vec<Out> {
        let needs_sensors = self
            .insts
            .values()
            .any(|i| matches!(i.kind, Kind::Cpu | Kind::Memory | Kind::CpuTemp | Kind::Gpu | Kind::GpuTemp));
        let needs_temperature = self.insts.values().any(|i| i.kind == Kind::CpuTemp);
        if needs_sensors && self.sensors.is_none() {
            let mut system = sysinfo::System::new();
            system.refresh_cpu_usage(); // CPU load needs two samples; the next tick has them
            system.refresh_memory();
            self.sensors = Some(Sensors { system, components: sysinfo::Components::new(), listed: None });
        } else if !needs_sensors {
            self.sensors = None;
        }
        if let Some(s) = &mut self.sensors {
            s.system.refresh_cpu_usage();
            s.system.refresh_memory();
            // temperatures only for a temperature screen; where there are none (Windows without admin rights),
            // asked again only every minute, since each try is a slow WMI query on the plugin's loop
            if needs_temperature {
                if !s.components.is_empty() {
                    s.components.refresh(false);
                } else if s.listed.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                    s.components = sysinfo::Components::new_with_refreshed_list();
                    s.listed = Some(Instant::now());
                }
            }
        }
        let now = Instant::now();
        for i in self.insts.values_mut().filter(|i| i.kind == Kind::Mqtt) {
            let topic = i.settings["topic"].as_str().unwrap_or_default().trim();
            i.output.text = self.mqtt_values.get(topic).map_or(String::new(), |m| {
                let unit = i.settings["unit"].as_str().unwrap_or_default();
                mqtt_text(m, i.settings["field"].as_str().unwrap_or_default().trim()) + unit
            });
        }
        for i in self.insts.values_mut().filter(|i| i.kind == Kind::Output) {
            let o = &mut i.output;
            match o.running.as_ref().map(|rx| rx.try_recv()) {
                Some(Ok(text)) => (o.text, o.running) = (text, None),
                Some(Err(mpsc::TryRecvError::Disconnected)) => o.running = None,
                _ => {}
            }
            if o.running.is_some() || o.next.is_some_and(|n| n > now) {
                continue;
            }
            let every = number(&i.settings["interval"]).unwrap_or(5).clamp(1, 3600) as u64;
            o.next = Some(now + Duration::from_secs(every));
            let line = i.settings["command"].as_str().unwrap_or_default().trim().to_string();
            if line.is_empty() {
                o.text = String::new();
                continue;
            }
            // on a thread of its own: a slow command holds up only this action
            let (tx, rx) = mpsc::channel();
            o.running = Some(rx);
            std::thread::spawn(move || {
                let text = match crate::command::output_within(&line, OUTPUT_LIMIT) {
                    Some((true, out)) => {
                        out.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default().to_string()
                    }
                    Some((false, why)) => {
                        eprintln!("output `{line}`: failed: {}", why.lines().next().unwrap_or_default());
                        "ERROR".into()
                    }
                    None => "ERROR".into(),
                };
                let _ = tx.send(text);
            });
        }
        let r = self.sensors.as_ref().map(read_sensors).unwrap_or_default();
        for i in self.insts.values_mut() {
            i.value = match i.kind {
                Kind::Cpu => r.cpu,
                Kind::Memory => r.memory,
                Kind::CpuTemp => r.cpu_temp,
                Kind::Gpu => r.gpu,
                Kind::GpuTemp => r.gpu_temp,
                Kind::Mqtt => {
                    let topic = i.settings["topic"].as_str().unwrap_or_default().trim();
                    let field = i.settings["field"].as_str().unwrap_or_default().trim();
                    self.mqtt_values.get(topic).and_then(|m| mqtt_text(m, field).trim().parse().ok())
                }
                Kind::Output => i.output.text.trim().parse().ok(),
                _ => None,
            }
            .filter(|v: &f32| v.is_finite());
            if let (Some(v), true) = (i.value, i.settings["graph"] == true) {
                if i.history.len() == GRAPH_SAMPLES {
                    i.history.pop_front();
                }
                i.history.push_back(v);
            }
        }
        let ids: Vec<String> = self.insts.keys().cloned().collect();
        ids.iter().flat_map(|id| self.draw(id, &is_dial)).collect()
    }

    /// Called every `SCROLL_STEP`: moves long text along.
    pub fn scroll(&mut self, is_dial: impl Fn(&str) -> bool) -> Vec<Out> {
        let ids: Vec<String> = self.insts.iter().filter(|(_, i)| i.scrolling).map(|(k, _)| k.clone()).collect();
        ids.iter().flat_map(|id| self.draw(id, &is_dial)).collect()
    }

    fn draw(&mut self, id: &str, is_dial: &impl Fn(&str) -> bool) -> Vec<Out> {
        let readings = self.sensors.as_ref().map(read_sensors);
        let Some(inst) = self.insts.get_mut(id) else { return Vec::new() };
        let now = Instant::now();
        let t = &mut inst.timer;
        if let Some(end) = t.ends.filter(|e| *e <= now) {
            t.ends = None;
            t.done_until = Some(end + Duration::from_secs(2)); // show "done" for 2 s
        }
        if t.done_until.is_some_and(|d| d <= now) {
            t.done_until = None;
        }
        let screen = Screen::of(inst, readings, now);
        inst.scrolling = matches!(screen, Screen::Value { scroll: Some(_), .. });
        let key = format!("{screen:?}");
        if key == inst.last {
            return Vec::new();
        }
        inst.last = key;
        let img = screen.render();
        let mut out = Vec::new();
        if inst.encoder && is_dial(&inst.device) {
            out.push(Out::Dial { device: inst.device.clone(), position: inst.position, jpeg: jpeg(&img) });
        }
        if !inst.pinned {
            out.push(Out::Ws(
                json!({ "event": "setImage", "context": inst.context, "payload": { "image": preview_uri(&img) } }),
            ));
        }
        out
    }
}

impl Inst {
    fn new(kind: Kind, context: Value, device: String, encoder: bool, position: u8, settings: Value) -> Inst {
        Inst {
            kind,
            context,
            device,
            encoder,
            position,
            settings,
            pressed: None,
            timer: Timer::default(),
            stopwatch: Stopwatch::default(),
            count: 0,
            output: Output::default(),
            history: VecDeque::new(),
            value: None,
            last: String::new(),
            scrolling: false,
            pinned: false,
        }
    }
}

/// A whole number from settings, written by the inspector as a string or kept by the plugin as a number.
fn number(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.trim().parse().ok())
}

/// What an MQTT message shows: the value at `field` ("temperature", "sensor.temp" for a nested one) of a JSON
/// message, or with no field the message's first line. Empty when the field isn't there.
fn mqtt_text(message: &str, field: &str) -> String {
    if field.is_empty() {
        return message.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default().to_string();
    }
    let Ok(json) = serde_json::from_str::<Value>(message) else { return String::new() };
    match field.split('.').try_fold(&json, |v, k| v.get(k)) {
        Some(Value::String(s)) => s.clone(),
        Some(v @ (Value::Number(_) | Value::Bool(_))) => v.to_string(),
        _ => String::new(),
    }
}

/// How much a counter's press or click adds (its "step" setting, 1 when unset; may be negative).
fn step(settings: &Value) -> i64 {
    number(&settings["step"]).unwrap_or(1)
}

/// Text for the dot font: upper case, characters it hasn't left out, at most `max` of them.
fn displayable(s: &str, max: usize) -> String {
    s.to_uppercase().chars().filter(|c| glyph(*c).is_some()).take(max).collect()
}

/// A number from settings, typed with a dot or a comma.
fn decimal(v: &Value) -> Option<f32> {
    v.as_f64().map(|f| f as f32).or_else(|| v.as_str()?.trim().replace(',', ".").parse().ok())
}

/// Whether the value is past one of the action's alarm limits ("alarmAbove", "alarmBelow").
fn alarm(inst: &Inst) -> bool {
    let Some(v) = inst.value else { return false };
    decimal(&inst.settings["alarmAbove"]).is_some_and(|a| v > a)
        || decimal(&inst.settings["alarmBelow"]).is_some_and(|b| v < b)
}

#[derive(Debug, Clone, Copy, Default)]
struct Readings {
    cpu: Option<f32>,
    memory: Option<f32>,
    cpu_temp: Option<f32>,
    gpu: Option<f32>,
    gpu_temp: Option<f32>,
}

fn read_sensors(s: &Sensors) -> Readings {
    let total = s.system.total_memory();
    // ponytail: the hottest CPU-ish sensor; many Windows PCs expose none without admin rights,
    // then the screen shows "--". GPU readings come from crate::gpu (per-vendor tools).
    let cpu_temp = s
        .components
        .iter()
        .filter(|c| {
            ["cpu", "package", "tdie", "tctl", "core", "soc"].iter().any(|k| c.label().to_lowercase().contains(k))
        })
        .filter_map(|c| c.temperature())
        .fold(None, |m: Option<f32>, t| Some(m.map_or(t, |m| m.max(t))));
    let (gpu, gpu_temp) = crate::gpu::reading();
    Readings {
        cpu: Some(s.system.global_cpu_usage()),
        memory: (total > 0).then(|| s.system.used_memory() as f32 * 100.0 / total as f32),
        cpu_temp,
        gpu,
        gpu_temp,
    }
}

/// Everything a frame depends on, so identical frames aren't drawn twice.
#[derive(Debug, PartialEq)]
enum Screen {
    Clock {
        hm: String,
        date: String,
    },
    /// Hands' angles in degrees, clockwise from 12.
    Analog {
        hour: u32,
        minute: u32,
        second: u32,
    },
    Timer {
        text: String,
        running: bool,
        done: bool,
    },
    Gauge {
        label: &'static str,
        value: String,
        fill: Option<u8>,
        colour: [u8; 3],
    },
    Stopwatch {
        text: String,
        running: bool,
    },
    /// A label, the value and its last samples (0–100 of the graph's height, oldest first).
    Graph {
        label: String,
        value: String,
        points: Vec<u8>,
        colour: [u8; 3],
    },
    /// A label over a value as large as it fits, or at the font scale `size` chosen in the action: the counter,
    /// the output action; one too long moving left, `scroll` px along.
    Value {
        label: String,
        value: String,
        scroll: Option<u32>,
        size: Option<u32>,
    },
    /// A counter with a goal: "17/50" and a bar, green once reached.
    Goal {
        label: String,
        value: String,
        fill: u8,
        reached: bool,
    },
    /// A value past its alarm limit, every other second: the screen on red.
    Alarm(Box<Screen>),
}

impl Screen {
    fn of(inst: &Inst, r: Option<Readings>, now: Instant) -> Screen {
        let screen = Screen::with_graph(inst, Screen::of_value(inst, r, now));
        let second = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        if alarm(inst) && second.is_multiple_of(2) {
            return Screen::Alarm(Box::new(screen));
        }
        screen
    }

    /// The screen as a graph, when the action's settings ask for one and its value is a number.
    fn with_graph(inst: &Inst, screen: Screen) -> Screen {
        if inst.settings["graph"] != true {
            return screen;
        }
        match screen {
            // percent and degrees: 0–100 always, so a graph reads the same from one minute to the next
            Screen::Gauge { label, value, colour, .. } => Screen::Graph {
                label: label.into(),
                value,
                points: inst.history.iter().map(|v| v.clamp(0.0, 100.0) as u8).collect(),
                colour,
            },
            // anything else: from the lowest to the highest sample shown
            Screen::Value { label, value, .. } if inst.kind == Kind::Mqtt => {
                let (lo, hi) = inst.history.iter().fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
                let points = inst
                    .history
                    .iter()
                    .map(|v| if hi > lo { ((v - lo) / (hi - lo) * 100.0) as u8 } else { 50 })
                    .collect();
                let value = value.chars().take(18).collect();
                Screen::Graph { label, value, points, colour: [0x2e, 0xc4, 0xff] }
            }
            screen => screen,
        }
    }

    fn of_value(inst: &Inst, r: Option<Readings>, now: Instant) -> Screen {
        let r = r.unwrap_or_default();
        let percent = |v: Option<f32>| v.map_or("--".to_string(), |v| format!("{}%", v.round() as u32));
        match inst.kind {
            Kind::Clock => {
                let t = Local::now();
                const DAYS: [&str; 7] = ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];
                Screen::Clock {
                    hm: format!("{:02}:{:02}", t.hour(), t.minute()),
                    date: format!(
                        "{:02}.{:02} {}",
                        t.day(),
                        t.month(),
                        DAYS[t.weekday().num_days_from_monday() as usize]
                    ),
                }
            }
            Kind::AnalogClock => {
                let t = Local::now();
                Screen::Analog {
                    hour: (t.hour() % 12) * 30 + t.minute() / 2,
                    minute: t.minute() * 6,
                    second: t.second() * 6,
                }
            }
            Kind::Timer => {
                let t = &inst.timer;
                let left =
                    t.ends.map_or(t.duration as u64, |e| e.saturating_duration_since(now).as_secs_f64().ceil() as u64);
                Screen::Timer {
                    text: format!("{:02}:{:02}", left / 60, left % 60),
                    running: t.ends.is_some(),
                    done: t.done_until.is_some(),
                }
            }
            Kind::Stopwatch => {
                let s = inst.stopwatch.elapsed(now).as_secs();
                Screen::Stopwatch {
                    text: match s / 3600 {
                        0 => format!("{:02}:{:02}", s / 60, s % 60),
                        h => format!("{h}:{:02}:{:02}", s / 60 % 60, s % 60),
                    },
                    running: inst.stopwatch.since.is_some(),
                }
            }
            Kind::Counter if number(&inst.settings["goal"]).is_some_and(|g| g > 0) => {
                let goal = number(&inst.settings["goal"]).unwrap_or(1);
                Screen::Goal {
                    label: displayable(inst.settings["label"].as_str().unwrap_or_default(), 12),
                    value: format!("{}/{goal}", inst.count),
                    fill: (inst.count.clamp(0, goal) * 100 / goal) as u8,
                    reached: inst.count >= goal,
                }
            }
            Kind::Counter | Kind::Output | Kind::Mqtt => {
                let value = match inst.kind {
                    Kind::Counter => inst.count.to_string(),
                    _ => displayable(&inst.output.text, 64),
                };
                let value = if value.is_empty() { "--".into() } else { value };
                // too long to read at once (auto: over SCROLL_FROM characters; a size: wider than the screen):
                // it moves by one column of dots every SCROLL_STEP
                let size = number(&inst.settings["size"]).filter(|s| (2..=12).contains(s)).map(|s| s as u32);
                let n = value.chars().count() as u32;
                let scale = size.unwrap_or(SCROLL_SCALE);
                let too_long = match size {
                    None => n > SCROLL_FROM,
                    Some(s) => (n * 6).saturating_sub(1) * s > 220,
                };
                let scroll = too_long.then(|| {
                    let period = (n + 3) * 6 * scale;
                    let steps = now.saturating_duration_since(epoch()).as_millis() / SCROLL_STEP.as_millis();
                    (steps as u32).wrapping_mul(scale) % period
                });
                Screen::Value {
                    label: displayable(inst.settings["label"].as_str().unwrap_or_default(), 12),
                    value,
                    scroll,
                    size,
                }
            }
            Kind::Cpu => Screen::Gauge {
                label: "CPU",
                value: percent(r.cpu),
                fill: r.cpu.map(|v| v as u8),
                colour: [0x2e, 0xc4, 0xff],
            },
            Kind::Memory => Screen::Gauge {
                label: "RAM",
                value: percent(r.memory),
                fill: r.memory.map(|v| v as u8),
                colour: [0xb4, 0x6e, 0xff],
            },
            Kind::CpuTemp => Screen::Gauge {
                label: "CPU TEMP",
                value: r.cpu_temp.map_or("--".to_string(), |t| format!("{}°", t.round() as i32)),
                fill: r.cpu_temp.map(|t| t.clamp(0.0, 100.0) as u8),
                colour: [0xff, 0x8a, 0x3d],
            },
            Kind::Gpu => Screen::Gauge {
                label: "GPU",
                value: percent(r.gpu),
                fill: r.gpu.map(|v| v as u8),
                colour: [0x4c, 0xd9, 0x64],
            },
            Kind::GpuTemp => Screen::Gauge {
                label: "GPU TEMP",
                value: r.gpu_temp.map_or("--".to_string(), |t| format!("{}°", t.round() as i32)),
                fill: r.gpu_temp.map(|t| t.clamp(0.0, 100.0) as u8),
                colour: [0xff, 0x5c, 0x5c],
            },
        }
    }

    fn render(&self) -> RgbImage {
        match self {
            Screen::Clock { hm, date } => {
                let mut img = RgbImage::new(SIZE, SIZE);
                text(&mut img, hm, 72, 7, WHITE);
                text(&mut img, date, 146, 3, GREY);
                img
            }
            Screen::Analog { hour, minute, second } => {
                let mut img = RgbImage::new(SIZE, SIZE);
                let c = SIZE as f32 / 2.0;
                let at = |deg: u32, r: f32| {
                    let a = (deg as f32).to_radians();
                    (c + r * a.sin(), c - r * a.cos())
                };
                for h in 0..12 {
                    let (outer, inner) = (at(h * 30, 108.0), at(h * 30, if h % 3 == 0 { 88.0 } else { 96.0 }));
                    line(&mut img, inner, outer, if h % 3 == 0 { 6.0 } else { 3.0 }, GREY);
                }
                line(&mut img, (c, c), at(*hour, 58.0), 9.0, WHITE);
                line(&mut img, (c, c), at(*minute, 88.0), 6.0, WHITE);
                line(&mut img, at(*second + 180, 18.0), at(*second, 96.0), 2.5, TIMER_GREEN);
                line(&mut img, (c, c), (c, c), 10.0, TIMER_GREEN); // centre cap
                img
            }
            Screen::Timer { text: t, running, done } => {
                let bg = if *done {
                    TIMER_DONE
                } else if *running {
                    TIMER_GREEN
                } else {
                    Rgb([0, 0, 0])
                };
                let mut img = RgbImage::from_pixel(SIZE, SIZE, bg);
                if *done {
                    text(&mut img, "DONE", 96, 7, WHITE);
                } else {
                    text(&mut img, "TIMER", 56, 3, if *running { WHITE } else { GREY });
                    text(&mut img, t, 96, 7, WHITE);
                }
                img
            }
            Screen::Gauge { label, value, fill, colour } => gauge(label, value, *fill, Rgb(*colour), WHITE),
            Screen::Stopwatch { text: t, running } => {
                let mut img = RgbImage::from_pixel(SIZE, SIZE, if *running { TIMER_GREEN } else { Rgb([0, 0, 0]) });
                text(&mut img, "STOPWATCH", 56, 3, if *running { WHITE } else { GREY });
                let scale = if t.len() > 5 { 5 } else { 7 };
                text(&mut img, t, 120 - scale * 7 / 2 + 10, scale, WHITE);
                img
            }
            Screen::Goal { label, value, fill, reached } => {
                let colour = if *reached { TIMER_GREEN } else { Rgb([0x2e, 0xc4, 0xff]) };
                gauge(label, value, Some(*fill), colour, if *reached { TIMER_GREEN } else { WHITE })
            }
            Screen::Alarm(screen) => {
                let mut img = screen.render();
                for p in img.pixels_mut().filter(|p| p.0.iter().all(|&c| c < 24)) {
                    *p = ALARM_RED;
                }
                img
            }
            Screen::Graph { label, value, points, colour } => {
                // everything inside the dial's round screen: the plot's corners 113 px from the centre
                let accent = Rgb(*colour);
                let mut img = RgbImage::new(SIZE, SIZE);
                text(&mut img, label, 38, fit(label, 170, 3), accent);
                text(&mut img, value, 66, fit(value, 200, 6), WHITE);
                // newest on the right, a column per pixel; the area under the line in a darker shade
                let (x0, w, bottom, h) = (40u32, 160u32, 200u32, 80u32);
                let dim = Rgb(colour.map(|c| c / 3));
                for x in 0..w {
                    img.put_pixel(x0 + x, bottom, Rgb([0x30, 0x30, 0x34]));
                    let back = (w - 1 - x) as usize * (GRAPH_SAMPLES - 1) / (w - 1) as usize;
                    let Some(&p) = points.len().checked_sub(back + 1).and_then(|i| points.get(i)) else { continue };
                    let top = bottom - 1 - p.min(100) as u32 * (h - 1) / 100;
                    for y in top..bottom {
                        img.put_pixel(x0 + x, y, if y < top + 3 { accent } else { dim });
                    }
                }
                img
            }
            Screen::Value { label, value, scroll, size } => {
                let mut img = RgbImage::new(SIZE, SIZE);
                let below: u32 = if label.is_empty() { 0 } else { 14 };
                text(&mut img, label, 52, fit(label, 190, 3), GREY);
                let scale = match (size, scroll) {
                    (Some(s), _) => *s,
                    (None, Some(_)) => SCROLL_SCALE,
                    (None, None) => fit(value, 220, 10),
                };
                let y = (120 + below).saturating_sub(scale * 7 / 2);
                match scroll {
                    // the text, three spaces, the text again: it comes round without a jump
                    Some(off) => {
                        let period = (value.chars().count() as i64 + 3) * 6 * scale as i64;
                        for start in [-(*off as i64), period - *off as i64] {
                            text_at(&mut img, value, start, y, scale, WHITE);
                        }
                    }
                    None => text(&mut img, value, y, scale, WHITE),
                }
                img
            }
        }
    }
}

/// Identify mode: `label` under an arrow pointing up, turned against the module's rotation `rot` (clockwise),
/// so on the hardware both read upright exactly when the plugin's idea of the rotation is right. `size` 72 for
/// a key, 240 for the dial.
pub fn identify_jpeg(label: &str, rot: u16, size: u32) -> Vec<u8> {
    let mut img = RgbImage::new(SIZE, SIZE);
    // arrow: a filled triangle at the top
    for y in 20..80u32 {
        let half = (y - 20) * 45 / 60;
        for x in SIZE / 2 - half..=SIZE / 2 + half {
            img.put_pixel(x, y, TIMER_GREEN);
        }
    }
    big_text(&mut img, label, 110);
    let img = match rot {
        90 => image::imageops::rotate270(&img),
        180 => image::imageops::rotate180(&img),
        270 => image::imageops::rotate90(&img),
        _ => img,
    };
    jpeg(&image::imageops::resize(&img, size, size, FilterType::Triangle))
}

/// App volume's level on the dial: VOLUME and the percent like the CPU screens, all red with MUTE when muted.
pub fn volume_jpeg(percent: u8, muted: bool) -> Vec<u8> {
    const RED: Rgb<u8> = Rgb([0xff, 0x3b, 0x30]);
    let value = format!("{percent}%");
    jpeg(&match muted {
        true => gauge("MUTE", &value, Some(percent), RED, RED),
        false => gauge("VOLUME", &value, Some(percent), Rgb([0x2e, 0xc4, 0xff]), WHITE),
    })
}

/// A label in the accent colour, a big value and a bar under it filled to `fill` percent.
fn gauge(label: &str, value: &str, fill: Option<u8>, accent: Rgb<u8>, value_colour: Rgb<u8>) -> RgbImage {
    let mut img = RgbImage::new(SIZE, SIZE);
    text(&mut img, label, 52, fit(label, 190, 3), accent);
    text(&mut img, value, 90, fit(value, 200, 8), value_colour);
    let (x0, y0, w, h) = (40u32, 170u32, 160u32, 10u32);
    let filled = fill.map_or(0, |f| w * f.min(100) as u32 / 100);
    for x in x0..x0 + w {
        for y in y0..y0 + h {
            img.put_pixel(x, y, if x < x0 + filled { accent } else { Rgb([0x30, 0x30, 0x34]) });
        }
    }
    img
}

/// The dial's card after switching function by a press: the function's number, before its own screen.
pub fn card_jpeg(label: &str) -> Vec<u8> {
    let mut img = RgbImage::new(SIZE, SIZE);
    big_text(&mut img, label, 57);
    jpeg(&img)
}

/// The largest font scale, up to `max` and at least 2, at which `s` fits `width` px.
fn fit(s: &str, width: u32, max: u32) -> u32 {
    let cells = (s.chars().count() as u32 * 6).saturating_sub(1).max(1);
    (width / cells).clamp(2, max)
}

/// `label` as large as fits the 240 px width, top at `y`.
fn big_text(img: &mut RgbImage, label: &str, y: u32) {
    let cells = (label.chars().count() as u32 * 6).saturating_sub(1).max(1);
    text(img, label, y, (220 / cells).min(18), WHITE);
}

fn jpeg(img: &RgbImage) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut Cursor::new(&mut out), 90)
        .encode_image(img)
        .expect("JPEG of a valid image");
    out
}

fn preview_uri(img: &RgbImage) -> String {
    let mut png = Vec::new();
    DynamicImage::from(img.clone())
        .resize_exact(PREVIEW, PREVIEW, FilterType::Triangle)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("PNG of a valid image");
    format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png))
}

/// A 5x7 dot-matrix font (the official app uses the "Zen Dots" display font); rows top to bottom.
fn glyph(c: char) -> Option<&'static str> {
    Some(match c {
        '0' => "01110 10001 10011 10101 11001 10001 01110",
        '1' => "00100 01100 00100 00100 00100 00100 01110",
        '2' => "01110 10001 00001 00010 00100 01000 11111",
        '3' => "11111 00010 00100 00010 00001 10001 01110",
        '4' => "00010 00110 01010 10010 11111 00010 00010",
        '5' => "11111 10000 11110 00001 00001 10001 01110",
        '6' => "00110 01000 10000 11110 10001 10001 01110",
        '7' => "11111 00001 00010 00100 01000 01000 01000",
        '8' => "01110 10001 10001 01110 10001 10001 01110",
        '9' => "01110 10001 10001 01111 00001 00010 01100",
        ':' => "00000 01100 01100 00000 01100 01100 00000",
        '.' => "00000 00000 00000 00000 00000 01100 01100",
        '%' => "11000 11001 00010 00100 01000 10011 00011",
        '-' => "00000 00000 00000 11111 00000 00000 00000",
        '°' => "01100 10010 10010 01100 00000 00000 00000",
        '/' => "00001 00010 00010 00100 01000 01000 10000",
        '+' => "00000 00100 00100 11111 00100 00100 00000",
        '=' => "00000 00000 11111 00000 11111 00000 00000",
        '!' => "00100 00100 00100 00100 00100 00000 00100",
        '?' => "01110 10001 00001 00010 00100 00000 00100",
        ',' => "00000 00000 00000 00000 01100 00100 01000",
        '\'' => "00100 00100 01000 00000 00000 00000 00000",
        '(' => "00010 00100 01000 01000 01000 00100 00010",
        ')' => "01000 00100 00010 00010 00010 00100 01000",
        '#' => "01010 01010 11111 01010 11111 01010 01010",
        ' ' => "00000 00000 00000 00000 00000 00000 00000",
        'A' => "01110 10001 10001 11111 10001 10001 10001",
        'B' => "11110 10001 10001 11110 10001 10001 11110",
        'C' => "01110 10001 10000 10000 10000 10001 01110",
        'D' => "11100 10010 10001 10001 10001 10010 11100",
        'E' => "11111 10000 10000 11110 10000 10000 11111",
        'F' => "11111 10000 10000 11110 10000 10000 10000",
        'G' => "01110 10001 10000 10111 10001 10001 01111",
        'H' => "10001 10001 10001 11111 10001 10001 10001",
        'I' => "01110 00100 00100 00100 00100 00100 01110",
        'J' => "00111 00010 00010 00010 00010 10010 01100",
        'K' => "10001 10010 10100 11000 10100 10010 10001",
        'L' => "10000 10000 10000 10000 10000 10000 11111",
        'M' => "10001 11011 10101 10101 10001 10001 10001",
        'N' => "10001 10001 11001 10101 10011 10001 10001",
        'O' => "01110 10001 10001 10001 10001 10001 01110",
        'P' => "11110 10001 10001 11110 10000 10000 10000",
        'Q' => "01110 10001 10001 10001 10101 10010 01101",
        'R' => "11110 10001 10001 11110 10100 10010 10001",
        'S' => "01111 10000 10000 01110 00001 00001 11110",
        'T' => "11111 00100 00100 00100 00100 00100 00100",
        'U' => "10001 10001 10001 10001 10001 10001 01110",
        'V' => "10001 10001 10001 10001 10001 01010 00100",
        'W' => "10001 10001 10001 10101 10101 10101 01010",
        'X' => "10001 10001 01010 00100 01010 10001 10001",
        'Y' => "10001 10001 10001 01010 00100 00100 00100",
        'Z' => "11111 00001 00010 00100 01000 10000 11111",
        // Polish letters: a mark above starts two rows higher (`lifted`), an ogonek hangs two rows lower
        'Ą' => "01110 10001 10001 11111 10001 10001 10001 00010 00011",
        'Ę' => "11111 10000 10000 11110 10000 10000 11111 00010 00011",
        'Ł' => "10000 10000 10100 11000 10000 10000 11111",
        'Ć' => "00010 00100 01110 10001 10000 10000 10000 10001 01110",
        'Ń' => "00010 00100 10001 10001 11001 10101 10011 10001 10001",
        'Ó' => "00010 00100 01110 10001 10001 10001 10001 10001 01110",
        'Ś' => "00010 00100 01111 10000 10000 01110 00001 00001 11110",
        'Ź' => "00010 00100 11111 00001 00010 00100 01000 10000 11111",
        'Ż' => "00100 00000 11111 00001 00010 00100 01000 10000 11111",
        _ => return None,
    })
}

/// A round-capped line of `width` px from `a` to `b`.
fn line(img: &mut RgbImage, a: (f32, f32), b: (f32, f32), width: f32, colour: Rgb<u8>) {
    let r = width / 2.0;
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = (dx * dx + dy * dy).max(1e-6);
    let (x0, x1) = ((a.0.min(b.0) - r).floor().max(0.0) as u32, ((a.0.max(b.0) + r).ceil() as u32).min(SIZE - 1));
    let (y0, y1) = ((a.1.min(b.1) - r).floor().max(0.0) as u32, ((a.1.max(b.1) + r).ceil() as u32).min(SIZE - 1));
    for x in x0..=x1 {
        for y in y0..=y1 {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            // distance from the pixel to the segment
            let t = (((px - a.0) * dx + (py - a.1) * dy) / len2).clamp(0.0, 1.0);
            let (ex, ey) = (px - (a.0 + t * dx), py - (a.1 + t * dy));
            if ex * ex + ey * ey <= r * r {
                img.put_pixel(x, y, colour);
            }
        }
    }
}

/// Whether a letter's mark is above it: its glyph starts two rows above the others.
fn lifted(c: char) -> bool {
    matches!(c, 'Ć' | 'Ń' | 'Ó' | 'Ś' | 'Ź' | 'Ż')
}

/// Draws `s` centred horizontally, top at `y`, each font cell `scale` px, as round dots.
fn text(img: &mut RgbImage, s: &str, y: u32, scale: u32, colour: Rgb<u8>) {
    let n = s.chars().filter(|c| glyph(*c).is_some()).count() as u32;
    let width = (n * 6).saturating_sub(1) * scale;
    text_at(img, s, (SIZE.saturating_sub(width) / 2) as i64, y, scale, colour);
}

/// Draws `s` from `x0` (may be off the left edge), top at `y`; what falls outside the picture is left out.
fn text_at(img: &mut RgbImage, s: &str, x0: i64, y: u32, scale: u32, colour: Rgb<u8>) {
    let chars: Vec<(char, &str)> = s.chars().filter_map(|c| Some((c, glyph(c)?))).collect();
    let r = scale as f32 * 0.45;
    for (i, (c, g)) in chars.iter().enumerate() {
        let lift = if lifted(*c) { 2.0 } else { 0.0 };
        for (row, bits) in g.split(' ').enumerate() {
            for (col, bit) in bits.chars().enumerate() {
                if bit != '1' {
                    continue;
                }
                let cx = (x0 + (i as i64 * 6 + col as i64) * scale as i64) as f32 + scale as f32 / 2.0;
                if cx + r < 0.0 || cx - r >= SIZE as f32 {
                    continue;
                }
                let cy = y as f32 + (row as f32 - lift) * scale as f32 + scale as f32 / 2.0;
                let (xa, xb) = ((cx - r).floor().max(0.0) as u32, ((cx + r).ceil() as u32).min(SIZE - 1));
                let (ya, yb) = ((cy - r).floor().max(0.0) as u32, ((cy + r).ceil() as u32).min(SIZE - 1));
                for px in xa..=xb {
                    for py in ya..=yb {
                        let (dx, dy) = (px as f32 + 0.5 - cx, py as f32 + 0.5 - cy);
                        if dx * dx + dy * dy <= r * r {
                            img.put_pixel(px, py, colour);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn appear(action: &str, device: &str, controller: &str) -> Value {
        json!({ "event": "willAppear", "action": format!("pl.kobylarek.masterhub.{action}"), "context": format!("{device}.P.{controller}.0.0"),
                "device": device, "payload": { "controller": controller, "coordinates": { "column": 0, "row": 0 }, "settings": {} } })
    }

    #[test]
    fn draws_to_dial_or_opendeck() {
        let is_dial = |d: &str| d == "mh-0206-D";
        let mut a = Actions::default();
        let out = a.handle(&appear("clock", "mh-0206-D", "Encoder"), is_dial);
        assert!(
            matches!(&out[0], Out::Dial { device, position: 0, jpeg } if device == "mh-0206-D" && jpeg[..2] == [0xff, 0xd8])
        );
        assert!(matches!(&out[1], Out::Ws(v) if v["event"] == "setImage"));
        assert!(a.owns_dial("mh-0206-D", 0, is_dial));
        assert!(a.tick(is_dial).is_empty() || Local::now().second() == 0); // same minute -> nothing to redraw

        // On a key of another device only OpenDeck gets the image.
        let out = a.handle(&appear("cpu", "sd-XYZ", "Keypad"), is_dial);
        assert_eq!(out.len(), 1);
        a.handle(&json!({ "event": "willDisappear", "context": "mh-0206-D.P.Encoder.0.0" }), is_dial);
        assert!(!a.owns_dial("mh-0206-D", 0, is_dial));
    }

    #[test]
    fn next_screen_presses() {
        let mut a = Actions::default();
        a.handle(&appear("cpu", "x", "Keypad"), |_| false);
        let press = json!({ "event": "keyDown", "context": "x.P.Keypad.0.0", "device": "x", "payload": { "settings": { "press": "next" } } });
        let outs = a.handle(&press, |_| false);
        assert_eq!(a.insts.values().next().unwrap().kind, Kind::Memory);
        assert!(outs
            .iter()
            .any(|o| matches!(o, Out::Ws(m) if m["event"] == "setSettings" && m["payload"]["screen"] == "memory")));
        // the choice survives the action appearing again
        let mut again = appear("cpu", "x", "Keypad");
        again["payload"]["settings"] = json!({ "screen": "memory" });
        let mut b = Actions::default();
        b.handle(&again, |_| false);
        assert_eq!(b.insts.values().next().unwrap().kind, Kind::Memory);
        for k in CYCLE {
            assert_eq!(Kind::from_uuid(k.name()), Some(k));
        }
    }

    #[test]
    fn pinned_dial_screens() {
        let mut a = Actions::default();
        let pin = |s: Option<&str>| vec![("D".to_string(), s.map(str::to_string))];
        a.pin_dials(&pin(Some("cpu")), false);
        assert!(a.pinned("D") && a.owns_dial("D", 3, |_| true)); // every slot of the dial
        let outs = a.tick(|_| true);
        assert!(outs.iter().any(|o| matches!(o, Out::Dial { position: WHOLE_DIAL, .. })));
        assert!(!outs.iter().any(|o| matches!(o, Out::Ws(_)))); // nothing for OpenDeck
        a.pin_dials(&pin(Some("clock")), false); // another screen replaces it
        a.pin_dials(&pin(Some("clock")), true); // as a graph: kept in its settings
        assert_eq!(a.insts["pin/D"].settings["graph"], true);
        assert_eq!(a.insts["pin/D"].kind, Kind::Clock);
        a.pin_dials(&[], false); // dial gone
        assert!(!a.pinned("D"));
        a.pin_dials(&pin(Some("clock")), false);
        a.pin_dials(&pin(None), false);
        assert!(!a.pinned("D"));
    }

    #[test]
    fn timer_counts_down() {
        let is_dial = |_: &str| false;
        let mut a = Actions::default();
        a.handle(&appear("timer", "mh-0206-D", "Encoder"), is_dial);
        let ctx = "mh-0206-D.P.Encoder.0.0";
        // turning sets the duration (10 s per tick) and stores it
        let out = a.handle(&json!({ "event": "dialRotate", "context": ctx, "payload": { "ticks": -3 } }), is_dial);
        assert!(matches!(&out[0], Out::Ws(v) if v["event"] == "setSettings" && v["payload"]["duration"] == 30));
        a.handle(&json!({ "event": "dialDown", "context": ctx }), is_dial);
        let inst = &a.insts[ctx];
        assert!(matches!(Screen::of(inst, None, Instant::now()), Screen::Timer { running: true, .. }));
        assert!(
            matches!(Screen::of(inst, None, Instant::now() + Duration::from_secs(31)), Screen::Timer { ref text, .. } if text == "00:00")
        );
        a.handle(&json!({ "event": "dialDown", "context": ctx }), is_dial); // press again: stop and reset
        assert!(
            matches!(Screen::of(&a.insts[ctx], None, Instant::now()), Screen::Timer { ref text, running: false, .. } if text == "00:30")
        );
    }

    #[test]
    fn stopwatch_pauses_and_resets() {
        let mut a = Actions::default();
        a.handle(&appear("stopwatch", "x", "Keypad"), |_| false);
        let ctx = "x.P.Keypad.0.0";
        let key = |a: &mut Actions, event: &str| a.handle(&json!({ "event": event, "context": ctx }), |_| false);
        key(&mut a, "keyDown");
        assert!(a.insts[ctx].stopwatch.since.is_none()); // counts on release
        key(&mut a, "keyUp");
        assert!(a.insts[ctx].stopwatch.since.is_some());
        let later = Instant::now() + Duration::from_secs(3725);
        assert!(
            matches!(Screen::of(&a.insts[ctx], None, later), Screen::Stopwatch { ref text, running: true } if text == "1:02:05")
        );
        key(&mut a, "keyDown");
        key(&mut a, "keyUp"); // paused, time kept
        assert!(a.insts[ctx].stopwatch.since.is_none());
        a.insts.get_mut(ctx).unwrap().stopwatch.before = Duration::from_secs(42);
        a.insts.get_mut(ctx).unwrap().pressed = Some(Instant::now() - LONG_PRESS);
        key(&mut a, "keyUp"); // held: reset
        assert_eq!(a.insts[ctx].stopwatch.elapsed(Instant::now()), Duration::ZERO);
    }

    #[test]
    fn counters_with_one_label_count_together() {
        let mut a = Actions::default();
        let mut plus = appear("counter", "x", "Keypad");
        plus["payload"]["settings"] = json!({ "label": "deaths", "count": 4 });
        a.handle(&plus, |_| false);
        let mut minus = appear("counter", "y", "Keypad");
        minus["payload"]["settings"] = json!({ "label": "Deaths", "step": "-1" });
        a.handle(&minus, |_| false);
        a.handle(&appear("counter", "D", "Encoder"), |_| false); // no label: on its own
        let ev = |event: &str, ctx: &str| json!({ "event": event, "context": ctx, "payload": { "ticks": 3 } });
        a.handle(&ev("keyDown", "x.P.Keypad.0.0"), |_| false);
        let out = a.handle(&ev("keyUp", "x.P.Keypad.0.0"), |_| false);
        assert_eq!((a.insts["x.P.Keypad.0.0"].count, a.insts["y.P.Keypad.0.0"].count), (5, 5));
        assert!(out.iter().any(|o| matches!(o, Out::Ws(m) if m["event"] == "setSettings" && m["payload"]["count"] == 5 && m["payload"]["step"] == "-1")));
        a.handle(&ev("keyDown", "y.P.Keypad.0.0"), |_| false);
        a.handle(&ev("keyUp", "y.P.Keypad.0.0"), |_| false);
        assert_eq!(a.insts["x.P.Keypad.0.0"].count, 4);
        a.handle(&ev("dialRotate", "D.P.Encoder.0.0"), |_| false);
        assert_eq!((a.insts["D.P.Encoder.0.0"].count, a.insts["x.P.Keypad.0.0"].count), (3, 4));
        a.handle(&ev("dialDown", "D.P.Encoder.0.0"), |_| false); // the dial's press resets
        assert_eq!(a.insts["D.P.Encoder.0.0"].count, 0);
        assert_eq!(
            Screen::of(&a.insts["x.P.Keypad.0.0"], None, Instant::now()),
            Screen::Value { label: "DEATHS".into(), value: "4".into(), scroll: None, size: None }
        );
    }

    #[test]
    fn mqtt_values() {
        assert_eq!(mqtt_text("21.5\n", ""), "21.5");
        assert_eq!(mqtt_text(r#"{"sensor":{"temp":21.5},"on":true}"#, "sensor.temp"), "21.5");
        assert_eq!(mqtt_text(r#"{"on":true}"#, "on"), "true");
        assert_eq!(mqtt_text(r#"{"on":true}"#, "off"), "");
        assert_eq!(mqtt_text("not json", "on"), "");
        let mut a = Actions::default();
        let mut room = appear("mqtt", "x", "Keypad");
        room["payload"]["settings"] = json!({ "topic": " home/room ", "field": "temp", "label": "room", "unit": "°" });
        a.handle(&room, |_| false);
        let mut any = appear("mqtt", "y", "Keypad");
        any["payload"]["settings"] = json!({ "topic": "home/+/temp" });
        a.handle(&any, |_| false);
        assert_eq!(a.topics(), BTreeSet::from(["home/room".to_string()])); // wildcards left out
        a.mqtt_values.insert("home/room".into(), r#"{"temp":21.5}"#.into());
        a.tick(|_| false);
        assert_eq!(
            Screen::of(&a.insts["x.P.Keypad.0.0"], None, Instant::now()),
            Screen::Value { label: "ROOM".into(), value: "21.5°".into(), scroll: None, size: None }
        );
    }

    #[test]
    fn graphs_keep_the_last_samples() {
        let mut a = Actions::default();
        let mut temp = appear("mqtt", "x", "Keypad");
        temp["payload"]["settings"] = json!({ "topic": "t", "graph": true });
        a.handle(&temp, |_| false);
        a.handle(&appear("mqtt", "y", "Keypad"), |_| false); // no graph: no samples kept
        for v in 0..GRAPH_SAMPLES + 5 {
            a.mqtt_values.insert("t".into(), format!("{}", 10 + v % 3));
            a.tick(|_| false);
        }
        let inst = &a.insts["x.P.Keypad.0.0"];
        assert_eq!(inst.history.len(), GRAPH_SAMPLES);
        assert!(a.insts["y.P.Keypad.0.0"].history.is_empty());
        let Screen::Graph { points, value, .. } = Screen::of(inst, None, Instant::now()) else { panic!() };
        assert_eq!((points.iter().min(), points.iter().max(), value.as_str()), (Some(&0), Some(&100), "11"));
        // a sensor's graph is 0–100 whatever its samples
        let mut cpu = Inst::new(Kind::Cpu, Value::Null, "d".into(), false, 0, json!({ "graph": true }));
        cpu.history.extend([20.0, 40.0, 140.0]);
        let r = Readings { cpu: Some(40.0), ..Default::default() };
        assert!(
            matches!(Screen::of(&cpu, Some(r), Instant::now()), Screen::Graph { ref points, .. } if points == &[20, 40, 100])
        );
        assert_eq!(Screen::of(&cpu, Some(r), Instant::now()).render().dimensions(), (SIZE, SIZE));
    }

    #[test]
    fn goals_and_alarms() {
        let mut a = Actions::default();
        let mut subs = appear("counter", "x", "Keypad");
        subs["payload"]["settings"] = json!({ "label": "subs", "goal": "50", "count": 17 });
        a.handle(&subs, |_| false);
        let ctx = "x.P.Keypad.0.0";
        assert_eq!(
            Screen::of(&a.insts[ctx], None, Instant::now()),
            Screen::Goal { label: "SUBS".into(), value: "17/50".into(), fill: 34, reached: false }
        );
        a.set_count(ctx, 51, &|_: &str| false);
        assert!(matches!(
            Screen::of(&a.insts[ctx], None, Instant::now()),
            Screen::Goal { fill: 100, reached: true, .. }
        ));

        let mut cpu = Inst::new(Kind::Cpu, Value::Null, "d".into(), false, 0, json!({ "alarmAbove": "90" }));
        assert!(!alarm(&cpu)); // no value yet
        cpu.value = Some(95.0);
        assert!(alarm(&cpu));
        cpu.settings = json!({ "alarmBelow": "10,5" });
        assert!(!alarm(&cpu));
        cpu.value = Some(10.0);
        assert!(alarm(&cpu));
        // the alarm look: the black background red, the text as it was
        let img = Screen::Alarm(Box::new(Screen::Value {
            label: String::new(),
            value: "1".into(),
            scroll: None,
            size: None,
        }))
        .render();
        assert_eq!(*img.get_pixel(0, 0), ALARM_RED);
        assert!(img.pixels().any(|p| *p == WHITE));
    }

    #[test]
    fn long_text_moves_along() {
        let mut a = Actions::default();
        let mut song = appear("mqtt", "x", "Keypad");
        song["payload"]["settings"] = json!({ "topic": "song" });
        a.handle(&song, |_| false);
        a.mqtt_values.insert("song".into(), "Daft Punk - One More Time".into());
        a.tick(|_| false);
        let ctx = "x.P.Keypad.0.0";
        let at = |a: &Actions, t: Instant| Screen::of(&a.insts[ctx], None, t);
        let now = Instant::now();
        assert!(
            matches!(at(&a, now), Screen::Value { scroll: Some(_), ref value, .. } if value == "DAFT PUNK - ONE MORE TIME")
        );
        assert_ne!(at(&a, now), at(&a, now + SCROLL_STEP)); // moved
        assert!(a.insts[ctx].scrolling);
        let period = (25 + 3) * 6 * SCROLL_SCALE;
        let cycle = SCROLL_STEP * period / SCROLL_SCALE;
        assert_eq!(at(&a, now), at(&a, now + cycle)); // and comes round
                                                      // short text stays put
        a.mqtt_values.insert("song".into(), "42".into());
        a.tick(|_| false);
        assert!(!a.insts[ctx].scrolling);
        assert_eq!(a.scroll(|_| false).len(), 0);
    }

    #[test]
    fn text_size() {
        let mut a = Actions::default();
        let mut big = appear("counter", "x", "Keypad");
        big["payload"]["settings"] = json!({ "size": "10", "count": 1234 });
        a.handle(&big, |_| false);
        // 4 digits at 10: 230 px, wider than the screen, so it moves along at that size
        assert!(matches!(
            Screen::of(&a.insts["x.P.Keypad.0.0"], None, Instant::now()),
            Screen::Value { size: Some(10), scroll: Some(_), .. }
        ));
        a.set_count("x.P.Keypad.0.0", 7, &|_: &str| false);
        assert!(matches!(
            Screen::of(&a.insts["x.P.Keypad.0.0"], None, Instant::now()),
            Screen::Value { size: Some(10), scroll: None, .. }
        ));
        // small: 13 characters fit at 2, so they stay put
        let mut small = appear("output", "y", "Keypad");
        small["payload"]["settings"] = json!({ "size": 2 });
        a.handle(&small, |_| false);
        a.insts.get_mut("y.P.Keypad.0.0").unwrap().output.text = "ZAŻÓŁĆ 42/100".into();
        assert!(matches!(
            Screen::of(&a.insts["y.P.Keypad.0.0"], None, Instant::now()),
            Screen::Value { size: Some(2), scroll: None, .. }
        ));
    }

    #[test]
    fn output_shows_the_first_line() {
        let mut a = Actions::default();
        let mut out = appear("output", "x", "Keypad");
        out["payload"]["settings"] = json!({ "command": "echo; echo Zażółć 42/100", "label": "viewers" });
        a.handle(&out, |_| false);
        let ctx = "x.P.Keypad.0.0";
        let deadline = Instant::now() + Duration::from_secs(5);
        while a.insts[ctx].output.text.is_empty() && Instant::now() < deadline {
            a.tick(|_| false);
            std::thread::sleep(Duration::from_millis(50));
        }
        // 13 characters: long enough to move along
        assert!(matches!(Screen::of(&a.insts[ctx], None, Instant::now()),
            Screen::Value { ref label, ref value, scroll: Some(_), .. } if label == "VIEWERS" && value == "ZAŻÓŁĆ 42/100"));
    }

    /// `cargo test dump_screens -- --ignored` writes sample frames to target/screens/ to look at.
    #[test]
    #[ignore]
    fn dump_screens() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/screens");
        std::fs::create_dir_all(&dir).unwrap();
        let screens = [
            ("clock", Screen::Clock { hm: "21:37".into(), date: "03.10 FRI".into() }),
            ("analog", Screen::Analog { hour: 10 * 30 + 4, minute: 8 * 6, second: 37 * 6 }),
        ];
        for (name, s) in screens {
            s.render().save(dir.join(format!("{name}.png"))).unwrap();
        }
        for (name, muted) in [("volume", false), ("volume-muted", true)] {
            image::load_from_memory(&volume_jpeg(75, muted)).unwrap().save(dir.join(format!("{name}.png"))).unwrap();
        }
        let screens = [
            ("timer", Screen::Timer { text: "01:30".into(), running: false, done: false }),
            ("timer-running", Screen::Timer { text: "00:42".into(), running: true, done: false }),
            ("timer-done", Screen::Timer { text: "00:00".into(), running: false, done: true }),
            ("stopwatch", Screen::Stopwatch { text: "12:47".into(), running: true }),
            ("counter", Screen::Value { label: "DEATHS".into(), value: "17".into(), scroll: None, size: None }),
            ("output", Screen::Value { label: "VIEWERS".into(), value: "1284".into(), scroll: None, size: None }),
            ("mqtt", Screen::Value { label: "LIVING ROOM".into(), value: "21.5°".into(), scroll: None, size: None }),
            ("polish", Screen::Value { label: "ZAŻÓŁĆ".into(), value: "GĘŚLĄ JAŹŃ".into(), scroll: None, size: None }),
            (
                "scroll",
                Screen::Value {
                    label: "NOW PLAYING".into(),
                    value: "DAFT PUNK - ONE MORE TIME".into(),
                    scroll: Some(60),
                    size: None,
                },
            ),
            ("goal", Screen::Goal { label: "SUBS".into(), value: "17/50".into(), fill: 34, reached: false }),
            (
                "alarm",
                Screen::Alarm(Box::new(Screen::Gauge {
                    label: "CPU TEMP",
                    value: "92°".into(),
                    fill: Some(92),
                    colour: [0xff, 0x8a, 0x3d],
                })),
            ),
            (
                "mqtt-graph",
                Screen::Graph {
                    label: "LIVING ROOM".into(),
                    value: "21.5°".into(),
                    points: (0..90).map(|i| (50.0 + 40.0 * (i as f32 / 14.0).sin()) as u8).collect(),
                    colour: [0x2e, 0xc4, 0xff],
                },
            ),
            (
                "cpu-graph",
                Screen::Graph {
                    label: "CPU".into(),
                    value: "37%".into(),
                    points: (0..120)
                        .map(|i| (25.0 + 18.0 * (i as f32 / 9.0).sin() + 6.0 * (i as f32 / 2.3).sin()) as u8)
                        .collect(),
                    colour: [0x2e, 0xc4, 0xff],
                },
            ),
            ("cpu", Screen::Gauge { label: "CPU", value: "37%".into(), fill: Some(37), colour: [0x2e, 0xc4, 0xff] }),
            (
                "cputemp",
                Screen::Gauge { label: "CPU TEMP", value: "52°".into(), fill: Some(52), colour: [0xff, 0x8a, 0x3d] },
            ),
            ("memory", Screen::Gauge { label: "RAM", value: "64%".into(), fill: Some(64), colour: [0xb4, 0x6e, 0xff] }),
            ("gpu", Screen::Gauge { label: "GPU", value: "23%".into(), fill: Some(23), colour: [0x4c, 0xd9, 0x64] }),
            (
                "gputemp",
                Screen::Gauge { label: "GPU TEMP", value: "47°".into(), fill: Some(47), colour: [0xff, 0x5c, 0x5c] },
            ),
        ];
        for (name, s) in screens {
            s.render().save(dir.join(format!("{name}.png"))).unwrap();
        }
        // and what the sensors say on this machine
        let mut a = Actions::default();
        a.handle(&appear("cpu", "x", "Keypad"), |_| false);
        a.tick(|_| false);
        std::thread::sleep(Duration::from_millis(500));
        a.tick(|_| false);
        println!("{:?}", a.sensors.as_ref().map(read_sensors));
    }

    #[test]
    #[ignore]
    fn print_sensor_labels() {
        for c in sysinfo::Components::new_with_refreshed_list().iter() {
            println!("{} = {:?}", c.label(), c.temperature());
        }
    }

    #[test]
    fn font_covers_all_screens() {
        for s in ["0123456789:.%-° ", "CPU RAM TEMP TIMER DONE", "MON TUE WED THU FRI SAT SUN", "ĄĆĘŁŃÓŚŹŻ"] {
            assert!(s.chars().all(|c| glyph(c).is_some()), "{s}");
        }
        assert!(glyph('A').unwrap().split(' ').all(|row| row.len() == 5));
        let gauge = Screen::Gauge { label: "CPU", value: "100%".into(), fill: Some(100), colour: [1, 2, 3] }.render();
        assert_eq!(gauge.dimensions(), (SIZE, SIZE));
    }
}
