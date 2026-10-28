//! The plugin's own actions for the M131E dial screen (the official app's display modes):
//! clock, timer, CPU load, memory use and CPU temperature.
//!
//! On a MasterHUB dial the screen is drawn here at its full 240x240 and sent straight to the
//! module; anywhere else (a key, another device) the image goes through OpenDeck's setImage.

use std::collections::HashMap;
use std::io::Cursor;
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

struct Inst {
    kind: Kind,
    context: Value,
    device: String,
    encoder: bool,
    position: u8,
    timer: Timer,
    last: String, // what was drawn last, to skip identical frames
    /// Pinned by the "Dial screen" setting: draws the whole dial, whatever its actions.
    pinned: bool,
}

/// Out::Dial position of a pinned screen: shown whatever the dial's current function.
pub const WHOLE_DIAL: u8 = u8::MAX;

#[derive(Default)]
pub struct Actions {
    insts: HashMap<String, Inst>,
    sensors: Option<Sensors>,
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

    /// Whether the "Dial screen" setting draws this dial.
    pub fn pinned(&self, device: &str) -> bool {
        self.insts.contains_key(&format!("pin/{device}"))
    }

    /// Makes each dial show its screen (an action name like "cpu"; None: the dial's own actions), and drops pins
    /// of dials that are gone. Called about once a second.
    pub fn pin_dials(&mut self, dials: &[(String, Option<String>)]) {
        let wanted: HashMap<String, Kind> =
            dials.iter().filter_map(|(d, s)| Some((format!("pin/{d}"), Kind::from_uuid(s.as_deref()?)?))).collect();
        self.insts.retain(|id, i| !i.pinned || wanted.get(id) == Some(&i.kind));
        for (id, &kind) in &wanted {
            let d = id.trim_start_matches("pin/").to_string();
            self.insts.entry(id.clone()).or_insert_with(|| Inst {
                kind,
                context: Value::Null,
                device: d,
                encoder: true,
                position: WHOLE_DIAL,
                timer: Timer::default(),
                last: String::new(),
                pinned: true,
            });
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
                let duration = msg["payload"]["settings"]["duration"].as_u64().map_or(60, |d| d.clamp(10, 5999) as u32);
                let inst = Inst {
                    kind,
                    context: msg["context"].clone(),
                    device: msg["device"].as_str().unwrap_or_default().to_string(),
                    encoder: msg["payload"]["controller"] == "Encoder",
                    position: msg["payload"]["coordinates"]["column"].as_u64().unwrap_or(0) as u8,
                    timer: Timer { duration, ..Default::default() },
                    last: String::new(),
                    pinned: false,
                };
                self.insts.insert(id.clone(), inst);
            }
            Some("willDisappear") => {
                self.insts.remove(&id);
                return Vec::new();
            }
            Some("keyDown" | "dialDown") if self.insts.get(&id).is_some_and(|i| i.kind != Kind::Timer) => {
                return self.press(&id, &msg["payload"]["settings"], &is_dial);
            }
            Some("keyDown" | "dialDown") => {
                if let Some(i) = self.insts.get_mut(&id).filter(|i| i.kind == Kind::Timer) {
                    // Press starts the countdown; pressing again stops and resets it (like the app).
                    let t = &mut i.timer;
                    t.done_until = None;
                    t.ends = match t.ends {
                        Some(_) => None,
                        None => Some(Instant::now() + Duration::from_secs(t.duration as u64)),
                    };
                }
            }
            Some("dialRotate") => {
                if let Some(i) = self.insts.get_mut(&id).filter(|i| i.kind == Kind::Timer && i.timer.ends.is_none()) {
                    let ticks = msg["payload"]["ticks"].as_i64().unwrap_or(0);
                    i.timer.duration = (i.timer.duration as i64 + ticks * 10).clamp(10, 5999) as u32;
                    i.timer.done_until = None;
                    let save = json!({ "event": "setSettings", "context": i.context, "payload": { "duration": i.timer.duration } });
                    let mut out = vec![Out::Ws(save)];
                    out.extend(self.draw(&id, &is_dial));
                    return out;
                }
            }
            _ => return Vec::new(),
        }
        self.draw(&id, &is_dial)
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
                // keep the choice: OpenDeck replaces all settings at once
                let mut keep = settings.clone();
                keep["screen"] = json!(i.kind.name());
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
        let ids: Vec<String> = self.insts.keys().cloned().collect();
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
}

impl Screen {
    fn of(inst: &Inst, r: Option<Readings>, now: Instant) -> Screen {
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
    text(&mut img, label, 52, 3, accent);
    text(&mut img, value, 90, 8, value_colour);
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

/// Draws `s` centred horizontally, top at `y`, each font cell `scale` px, as round dots.
fn text(img: &mut RgbImage, s: &str, y: u32, scale: u32, colour: Rgb<u8>) {
    let chars: Vec<&str> = s.chars().filter_map(glyph).collect();
    let width = (chars.len() as u32 * 6).saturating_sub(1) * scale;
    let x0 = (SIZE.saturating_sub(width)) / 2;
    let r = scale as f32 * 0.45;
    for (i, g) in chars.iter().enumerate() {
        for (row, bits) in g.split(' ').enumerate() {
            for (col, bit) in bits.chars().enumerate() {
                if bit != '1' {
                    continue;
                }
                let cx = (x0 + (i as u32 * 6 + col as u32) * scale) as f32 + scale as f32 / 2.0;
                let cy = (y + row as u32 * scale) as f32 + scale as f32 / 2.0;
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
        a.pin_dials(&pin(Some("cpu")));
        assert!(a.pinned("D") && a.owns_dial("D", 3, |_| true)); // every slot of the dial
        let outs = a.tick(|_| true);
        assert!(outs.iter().any(|o| matches!(o, Out::Dial { position: WHOLE_DIAL, .. })));
        assert!(!outs.iter().any(|o| matches!(o, Out::Ws(_)))); // nothing for OpenDeck
        a.pin_dials(&pin(Some("clock"))); // another screen replaces it
        assert_eq!(a.insts["pin/D"].kind, Kind::Clock);
        a.pin_dials(&[]); // dial gone
        assert!(!a.pinned("D"));
        a.pin_dials(&pin(Some("clock")));
        a.pin_dials(&pin(None));
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
        for s in ["0123456789:.%-° ", "CPU RAM TEMP TIMER DONE", "MON TUE WED THU FRI SAT SUN"] {
            assert!(s.chars().all(|c| glyph(c).is_some()), "{s}");
        }
        assert!(glyph('A').unwrap().split(' ').all(|row| row.len() == 5));
        let gauge = Screen::Gauge { label: "CPU", value: "100%".into(), fill: Some(100), colour: [1, 2, 3] }.render();
        assert_eq!(gauge.dimensions(), (SIZE, SIZE));
    }
}
