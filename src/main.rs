//! OpenDeck device plugin for the Cooler Master MasterHUB.
//!
//! Every MasterHUB module is its own USB HID device, so every module is registered with OpenDeck
//! as its own device. Protocol notes: docs/sdk-commands.md and docs/masterhub-app-analysis.md.

mod actions;
mod appvolume;
mod audio;
mod command;
mod debug;
mod device;
mod device_info;
mod finetune;
mod gpu;
mod lights;
mod location;
mod mqtt;
mod pictures;
mod placement;
mod runner;
mod secret;
mod signals;
#[cfg(target_os = "macos")]
mod usbhost;
mod web;

use device::*;
use pictures::*;
use web::*;

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Cursor, ErrorKind, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use base64::Engine;
use hidapi::{HidApi, HidDevice};
use image::{DynamicImage, RgbImage};
use lights::{Control, Lights};
use serde_json::{json, Value};
use tungstenite::Message;

const VID: u16 = 0x2516;
const NAMESPACE: &str = "mh"; // must match DeviceNamespace in manifest.json
const KEY_IMAGE_SIZE: u32 = 72; // one key's picture, JPEG
const SCREEN_SIZE: u32 = 240; // M131E dial screen, JPEG like the official app sends
const KEY_ROWS: u8 = 3;
const KEY_COLS: u8 = 5;
const FADERS: usize = 5;
const DEFAULT_REPORT_LEN: usize = 320; // M4315 image report; used when the HID descriptor can't be read
const DEFAULT_M0_REPORT_LEN: usize = 64; // knob/fader/base output report; used when the HID descriptor can't be read
const LIGHTS_REFRESH: Duration = Duration::from_secs(60); // the official app re-sends LED settings this often
const LONG_PRESS: Duration = Duration::from_millis(600); // the official app's dial long press
const MAX_FUNCTIONS: u8 = 4;
/// Whole-screen image of the 15xKey (Display_Picture_On_Screen, 02 35). ponytail: the size comes from
/// the SDK's 522 240-byte picture buffer (= 480 x 272 x 4); check it on the device.
const KEYPAD_SCREEN: (u32, u32) = (480, 272);
/// A wall-clock jump this big between two turns of the main loop means the computer slept.
const WAKE_GAP: Duration = Duration::from_secs(10);
/// How long the dial shows the function card after a press switched it.
const DIAL_CARD_TIME: Duration = Duration::from_millis(1000);
/// The "LED lighting" action (lights::lighting_action).
const LIGHTING_UUID: &str = "com.masterhub.opendeck.lighting";
/// Screens the "Dial screen" setting can pin on the dial (actions.rs names).
const DIAL_SCREENS: [(&str, &str); 7] = [
    ("clock", "Clock"),
    ("analogclock", "Analog clock"),
    ("cpu", "CPU load"),
    ("memory", "Memory use"),
    ("cputemp", "CPU temperature"),
    ("gpu", "GPU load"),
    ("gputemp", "GPU temperature"),
];
/// How long identify mode shows numbers and colours.
const IDENTIFY_TIME: Duration = Duration::from_secs(10);
/// Identify colours by OpenDeck position: red, green, blue, yellow, magenta.
const IDENTIFY_COLOURS: [lights::Rgb; 5] = [[255, 0, 0], [0, 255, 0], [0, 64, 255], [255, 200, 0], [255, 0, 255]];

/// Plugin settings (kept in OpenDeck's global plugin settings) plus what the settings page lists.
struct Config {
    /// Ticks sent per 1% of fader travel, per fader.
    fader_scale: [f64; FADERS],
    lights: Lights,
    /// Functions per knob / dial, switched by pressing it (1 = off).
    functions: u8,
    /// Image file shown on the whole 15xKey screen behind the keys (experimental).
    keypad_background: Option<String>,
    /// Knobs and faders currently registered, with what OpenDeck last drew for them.
    controls: Vec<Control>,
    /// What each connected module reports about itself (settings page).
    modules: Vec<device_info::Info>,
    /// Registered modules for the placement drawing: id -> (kind, short name, socket).
    placed: std::collections::BTreeMap<String, (Kind, String, Option<String>)>,
    /// The base's turn on the desk when the modules were last registered.
    base_turn: u16,
    /// Turn of the whole base set by hand (0, 90, 180, 270); None follows the base's tilt sensor, which some
    /// may report wrong.
    base_rotation: Option<u16>,
    /// Fast turns of knobs, rollers and the dial count more clicks.
    acceleration: bool,
    /// What the dial's screen shows whatever its actions ("cpu", "clock", …); None: its turn action.
    dial_screen: Option<String>,
    /// The placement view: the plugin's drawing, or the official app's pictures.
    placement_pictures: bool,
    /// Turn key and dial pictures against the module's rotation so they stand upright (the official app doesn't).
    upright: bool,
    /// Signal LEDs' current colours, by "<device>/<control>" (signals.rs).
    signal_colours: HashMap<String, lights::Rgb>,
    /// The brightness OpenDeck set, for LEDs sent one by one.
    led_brightness: u8,
    /// Identify mode is on: nothing but it draws on the LEDs (`push_*` wait).
    identifying: bool,
    /// What the last save didn't take (an invalid value), for the page to say once.
    notice: Vec<String>,
    /// OpenDeck's `-info` (its version, the system), for Home Assistant's plugin sensor.
    host: Value,
    /// Last position (0–100) of every fader by "<device>/<OpenDeck position>": absolute actions, and a fader's
    /// LED as a level meter when Home Assistant or a signal lets go of it.
    fader_values: HashMap<String, u8>,
    /// Home Assistant over MQTT (mqtt.rs): the connection, the lights it set and how the connection is.
    mqtt: mqtt::Settings,
    mqtt_lights: mqtt::Lights,
    mqtt_flashes: HashMap<String, mqtt::Flash>,
    mqtt_status: String,
}

impl Config {
    fn new() -> Self {
        Config {
            fader_scale: [1.0; FADERS],
            lights: Lights::default(),
            functions: 1,
            keypad_background: None,
            controls: Vec::new(),
            base_rotation: None,
            acceleration: false,
            dial_screen: None,
            placement_pictures: false,
            upright: false,
            modules: Vec::new(),
            placed: Default::default(),
            base_turn: 0,
            signal_colours: HashMap::new(),
            led_brightness: 100,
            identifying: false,
            notice: Vec::new(),
            host: Value::Null,
            fader_values: HashMap::new(),
            mqtt: mqtt::Settings::default(),
            mqtt_lights: HashMap::new(),
            mqtt_flashes: HashMap::new(),
            mqtt_status: String::new(),
        }
    }

    fn load(&mut self, v: &Value) {
        self.fader_scale = read_scales(v);
        self.lights = Lights::from_json(&v["lights"]);
        self.functions = v["functions"].as_u64().map_or(1, |f| f.clamp(1, MAX_FUNCTIONS as u64) as u8);
        self.keypad_background = v["keypadBackground"].as_str().filter(|p| !p.is_empty()).map(str::to_string);
        self.acceleration = v["acceleration"].as_bool().unwrap_or(false);
        self.placement_pictures = v["placementPictures"].as_bool().unwrap_or(false);
        self.dial_screen =
            v["dialScreen"].as_str().filter(|s| DIAL_SCREENS.iter().any(|(k, _)| k == s)).map(str::to_string);
        self.upright = v["upright"].as_bool().unwrap_or(false);
        self.base_rotation = v["baseRotation"].as_u64().map(|r| r as u16).filter(|r| [0, 90, 180, 270].contains(r));
        self.mqtt = mqtt::Settings::from_json(&v["mqtt"]);
    }

    fn to_json(&self) -> Value {
        json!({ "faderScale": self.fader_scale, "lights": self.lights.to_json(), "functions": self.functions, "keypadBackground": self.keypad_background, "baseRotation": self.base_rotation, "acceleration": self.acceleration, "dialScreen": self.dial_screen, "placementPictures": self.placement_pictures, "upright": self.upright, "mqtt": self.mqtt.to_json() })
    }

    fn forget(&mut self, id: &str) {
        self.controls.retain(|c| !c.key.starts_with(&format!("{id}/")));
        self.modules.retain(|m| m.id != id);
        self.placed.remove(id);
    }
}

/// Encoder slots in OpenDeck that hold an action, and the last screen drawn for each dial slot.
/// Keys are "<device id>/<slot>".
#[derive(Default)]
struct Slots {
    occupied: HashSet<String>,
    screens: HashMap<String, Vec<u8>>,
}

impl Slots {
    fn forget(&mut self, id: &str) {
        let prefix = format!("{id}/");
        self.occupied.retain(|k| !k.starts_with(&prefix));
        self.screens.retain(|k, _| !k.starts_with(&prefix));
    }
}

type Shared = Arc<RwLock<Config>>;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Keys,    // M4315: 15 keys with IPS screens
    Knobs,   // M003K: 3 knobs
    Faders,  // M005S: 5 faders
    Rollers, // M032LE: 2 rollers
    Dial,    // M131E: dial with 1.3" IPS screen
}

struct Model {
    pids: &'static [u16], // the official app accepts each module under two product ids
    kind: Kind,
    name: &'static str,
    rows: u8,
    columns: u8,
    encoders: u8,
    sw_mode_cmd: Option<u8>, // modules ignore commands until switched to software mode
}

/// The Base (M0064) only has LEDs and no inputs, so it isn't registered with OpenDeck; it is only
/// read for its orientation (vertical or horizontal, the engraving in its corner).
const BASE_PIDS: &[u16] = &[0x01f6, 0x9999];
/// A horizontal base turns everything on it by 90° clockwise. The official app rotates its view by 270°, but
/// the other way round: on the user's unit the 15xKey at port 23 (270° in the app's table) stands upright with
/// the base flat (2026-10-25).
const HORIZONTAL_ROT: u16 = 90;
const BASE_ID: &str = "mh-base"; // settings page only; the base isn't an OpenDeck device

const MODELS: &[Model] = &[
    Model {
        pids: &[0x01fe, 0x9997],
        kind: Kind::Keys,
        name: "MasterHUB 15xKey",
        rows: KEY_ROWS,
        columns: KEY_COLS,
        encoders: 0,
        sw_mode_cmd: Some(0x3a),
    },
    Model {
        pids: &[0x0202, 0x9996],
        kind: Kind::Knobs,
        name: "MasterHUB 3xKnob",
        rows: 0,
        columns: 0,
        encoders: 3,
        sw_mode_cmd: None,
    },
    Model {
        pids: &[0x0200, 0x9991],
        kind: Kind::Faders,
        name: "MasterHUB 5xFader",
        rows: 0,
        columns: 0,
        encoders: 5,
        sw_mode_cmd: None,
    },
    Model {
        pids: &[0x0204, 0x9995],
        kind: Kind::Rollers,
        name: "MasterHUB 2xRoller",
        rows: 0,
        columns: 0,
        encoders: 2,
        sw_mode_cmd: None,
    },
    Model {
        pids: &[0x0206, 0x9994],
        kind: Kind::Dial,
        name: "MasterHUB Dial",
        rows: 0,
        columns: 0,
        encoders: 1,
        sw_mode_cmd: Some(0x49),
    },
];

/// Messages sent from module threads to the main (WebSocket) thread.
enum Up {
    Send(Value),
    Gone(String),
    Orientation(bool), // true = horizontal
    BaseGone,
    /// true: also store the LEDs in the modules' flash ("Save and store in the modules").
    LightsChanged(bool),
    LayoutChanged,
    BackgroundChanged,
    Info(device_info::Info),
    /// The base rotation setting changed: register every module again.
    Reorient,
    /// "Identify modules" on the settings page.
    Identify,
    /// App volume's level at a place: device, slot, level 0–100, muted. A dial's screen shows it, a knob's or
    /// fader's LED.
    Level(String, u8, u8, bool),
    /// A signal command's result for its LED ("<device>/<control>").
    Signal(String, lights::Rgb),
    /// Draw an LED ("<device>/<control>") again: Home Assistant set or released it (mqtt.rs).
    Repaint(String),
    /// Store the global settings in OpenDeck again (they changed outside the settings page).
    SaveSettings,
}

/// Commands sent from the main thread to a module thread.
enum Down {
    KeyImage {
        position: u8,
        jpeg: Vec<u8>,
        pressed: Vec<u8>,
    },
    KeyColor {
        position: Option<u8>,
        rgb: [u8; 3],
    },
    ScreenImage(Vec<u8>),
    ScreenColour(lights::Rgb),
    Background(Option<Arc<KeypadBackground>>),
    Brightness(u8),
    /// `save`: also store in the module's flash (kept without the PC).
    LedMode {
        mode: u8,
        speed: u8,
        save: bool,
    },
    LedColour {
        index: u8,
        rgb: lights::Rgb,
        save: bool,
    },
    /// One LED's brightness (0–100), e.g. a fader's LED following its position.
    LedBrightness {
        index: u8,
        value: u8,
    },
    /// Identify mode: show positions and rotation on the screens for `IDENTIFY_TIME`.
    Identify,
    Stop,
    /// OpenDeck is going away: hand the screens back to the firmware (Set_Sw_Mode 0) and stop.
    Shutdown,
}

/// ponytail: one base assumed; with several bases, all modules follow the first base found.
#[derive(Default)]
struct Base {
    open: bool,
    horizontal: bool,
    tx: Option<Sender<Down>>, // LED commands for the base
}

/// What the main thread keeps per registered module.
struct Handle {
    tx: Sender<Down>,
    kind: Kind,
    rot: u16,
    encoders: u8, // physical knobs / faders / ...
    /// Press-to-switch (official app): knobs and the dial get `functions` OpenDeck slots each,
    /// slot = function * encoders + control, so with 1 function the slots are the controls.
    functions: u8,
    active: Vec<u8>, // current function per control
    pressed_at: Vec<Option<Instant>>,
}

impl Handle {
    fn slot(&self, control: u8) -> u8 {
        self.active[control as usize] * self.encoders + control
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let port = arg("-port").expect("missing -port");
    let uuid = arg("-pluginUUID").expect("missing -pluginUUID");
    let register_event = arg("-registerEvent").expect("missing -registerEvent");

    let stream =
        TcpStream::connect(("127.0.0.1", port.parse::<u16>().expect("bad -port"))).expect("cannot reach OpenDeck");
    let (mut ws, _) =
        tungstenite::client(format!("ws://127.0.0.1:{port}"), stream).expect("WebSocket handshake failed");
    ws.get_ref().set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    post(&mut ws, &json!({ "event": register_event, "uuid": uuid }));
    post(&mut ws, &json!({ "event": "getGlobalSettings", "context": uuid }));

    let mut api = HidApi::new().expect("cannot initialise hidapi");
    let (up_tx, up_rx) = mpsc::channel::<Up>();
    let shared: Shared = Arc::new(RwLock::new(Config::new()));
    shared.write().unwrap().host = arg("-info").and_then(|i| serde_json::from_str(&i).ok()).unwrap_or_default();
    let (settings, _) = settings_server(shared.clone(), up_tx.clone(), uuid.clone());
    signals::start(shared.clone(), up_tx.clone());
    mqtt::start(shared.clone(), up_tx.clone());
    let mut modules: HashMap<String, Handle> = HashMap::new();
    let mut base = Base::default();
    let mut last_scan = ago(Duration::from_secs(60));
    let mut last_lights = Instant::now();
    let mut actions = actions::Actions::default();
    let mut last_tick = Instant::now();
    let mut slots = Slots::default();
    let mut last_turns: HashMap<String, Instant> = HashMap::new(); // for acceleration
    let mut identify_until: Option<Instant> = None;
    // Sleep: Instant stops while the computer sleeps (macOS, Linux), the wall clock doesn't.
    let mut last_wall = std::time::SystemTime::now();
    let mut last_awake = awake_now();
    let mut wake_at: Option<Instant> = None;
    // Dials showing their function card, until when.
    let mut dial_cards: HashMap<String, Instant> = HashMap::new();
    // Dial slots whose screen App volume draws ("<device>/<slot>"), and the screen App volume picked per dial.
    let mut volume_dials: HashSet<String> = HashSet::new();
    let mut action_screens: HashMap<String, String> = HashMap::new();
    // Dials with an action on their key: their press goes there.
    let mut dial_keys: HashSet<String> = HashSet::new();
    // Home Assistant's effects: their common start, and what was last drawn on each LED (animate).
    let epoch = Instant::now();
    let mut last_frame = Instant::now();
    let mut frames_sent: HashMap<String, ((lights::Rgb, u8), Instant)> = HashMap::new();

    loop {
        if CLOSED.load(std::sync::atomic::Ordering::Relaxed) {
            return shutdown(&modules);
        }
        // Hotplug: modules are swapped by hand, so rescan every 2 s.
        if last_scan.elapsed() > Duration::from_secs(2) {
            last_scan = Instant::now();
            scan(&mut api, &mut modules, &mut base, &up_tx, &shared);
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = Instant::now();
            let dials: Vec<String> =
                modules.iter().filter(|(_, h)| h.kind == Kind::Dial).map(|(id, _)| id.clone()).collect();
            let was_pinned: Vec<bool> = dials.iter().map(|d| actions.pinned(d)).collect();
            let plugin_screen = shared.read().unwrap().dial_screen.clone();
            let screens: Vec<(String, Option<String>)> =
                dials.iter().map(|d| (d.clone(), action_screens.get(d).cloned().or(plugin_screen.clone()))).collect();
            actions.pin_dials(&screens);
            // unpinned: the dial shows its own function again
            for (d, _) in dials.iter().zip(was_pinned).filter(|(d, was)| *was && !actions.pinned(d)) {
                if let Some(h) = modules.get(d) {
                    show_dial(d, h, &slots);
                }
            }
            let outs = actions.tick(|d| is_dial(&modules, d));
            deliver(outs, &mut ws, &modules, &mut slots);
        }
        // After sleep the modules may be back in hardware mode with their pictures gone: register them again
        // 2 s after waking, like the official app.
        // The awake clock stops while the computer sleeps, the wall clock doesn't: a wall-clock jump the loop didn't
        // spend awake is sleep, not a clock change or a slow turn of the loop
        let wall = std::time::SystemTime::now();
        let awake = awake_now();
        let spent = awake.saturating_sub(last_awake);
        last_awake = awake;
        if wall.duration_since(last_wall).is_ok_and(|gap| gap > spent + WAKE_GAP) {
            eprintln!("woke from sleep: setting the modules up again");
            wake_at = Some(Instant::now() + Duration::from_secs(2));
        }
        last_wall = wall;
        if wake_at.is_some_and(|t| Instant::now() >= t) {
            wake_at = None;
            let _ = up_tx.send(Up::Reorient);
        }
        dial_cards.retain(|id, until| {
            let showing = Instant::now() < *until;
            if let (false, Some(h)) = (showing, modules.get(id)) {
                show_dial(id, h, &slots);
            }
            showing
        });
        if identify_until.is_some_and(|t| Instant::now() >= t) {
            identify_until = None;
            shared.write().unwrap().identifying = false;
            end_identify(&modules, &base, &shared.read().unwrap());
        }
        if last_frame.elapsed() >= Duration::from_millis(40) && identify_until.is_none() {
            last_frame = Instant::now();
            animate(&modules, &base, &shared, epoch, &mut frames_sent);
        }
        if last_lights.elapsed() > LIGHTS_REFRESH && identify_until.is_none() {
            last_lights = Instant::now();
            push_lights(&modules, &base, &shared.read().unwrap(), false);
        }

        loop {
            match up_rx.try_recv() {
                Ok(Up::Send(mut msg)) => {
                    if let Some(v) = msg["payload"].as_object_mut().and_then(|p| p.remove("value")) {
                        let (device, position) = (
                            msg["payload"]["device"].as_str().unwrap_or_default(),
                            msg["payload"]["position"].as_u64().unwrap_or(0) as u8,
                        );
                        let value = v.as_u64().unwrap_or(0).min(100) as u8;
                        shared.write().unwrap().fader_values.insert(format!("{device}/{position}"), value);
                        let cfg = shared.read().unwrap();
                        let led = format!("{device}/{position}");
                        // a signal, Home Assistant or identify mode own the LED meanwhile
                        let taken = cfg.lights.signals.contains_key(&led)
                            || mqtt::shown(&cfg, &led, Instant::now()).is_some()
                            || cfg.identifying;
                        if let (true, true, false, Some(h)) =
                            (cfg.lights.managed, cfg.lights.faders_level, taken, modules.get(device))
                        {
                            let _ = h.tx.send(Down::LedBrightness {
                                index: encoder_index(h.kind, h.rot, h.encoders, position),
                                value,
                            });
                        }
                    }
                    // the dial's press goes to its key when an action sits there
                    if msg["payload"]["device"].as_str().is_some_and(|d| dial_keys.contains(d)) {
                        match msg["event"].as_str() {
                            Some("encoderDown") => msg["event"] = json!("keyDown"),
                            Some("encoderUp") => msg["event"] = json!("keyUp"),
                            _ => {}
                        }
                    }
                    if msg["event"] == "encoderChange" && shared.read().unwrap().acceleration {
                        accelerate(&mut msg, &modules, &mut last_turns);
                    }
                    let (msgs, switched) = route_input(msg, &mut modules, &slots);
                    for m in msgs {
                        post(&mut ws, &m);
                    }
                    if let Some((id, h)) = switched.as_deref().and_then(|id| modules.get_key_value(id)) {
                        match h.kind {
                            // like the app: a card with the function first, then its screen (unless pinned)
                            Kind::Dial if actions.pinned(id) => {}
                            Kind::Dial => {
                                let _ =
                                    h.tx.send(Down::ScreenImage(actions::card_jpeg(&format!("F{}", h.active[0] + 1))));
                                dial_cards.insert(id.clone(), Instant::now() + DIAL_CARD_TIME);
                            }
                            _ => push_module_lights(id, h, &shared.read().unwrap(), false),
                        }
                    }
                }
                Ok(Up::Gone(id)) => {
                    modules.remove(&id);
                    slots.forget(&id);
                    // what was kept for the device: OpenDeck sends it again when it comes back
                    dial_keys.remove(&id);
                    action_screens.remove(&id);
                    dial_cards.remove(&id);
                    volume_dials.retain(|k| !k.starts_with(&format!("{id}/")));
                    shared.write().unwrap().forget(&id);
                    post(&mut ws, &json!({ "event": "deregisterDevice", "payload": id }));
                }
                Ok(msg @ (Up::Orientation(_) | Up::Reorient)) => {
                    if let Up::Orientation(horizontal) = msg {
                        let changed = horizontal != base.horizontal;
                        base.horizontal = horizontal;
                        if !changed || shared.read().unwrap().base_rotation.is_some() {
                            continue; // nothing moves, or the rotation is set by hand
                        }
                    }
                    // Every rotation (and the key grid shape) changes: register all modules again.
                    {
                        let mut cfg = shared.write().unwrap();
                        cfg.controls.clear();
                        cfg.modules.retain(|m| m.id == BASE_ID);
                    }
                    slots = Slots::default();
                    // what was kept per device: OpenDeck sends it again for the new registrations
                    dial_keys.clear();
                    action_screens.clear();
                    dial_cards.clear();
                    volume_dials.clear();
                    frames_sent.clear();
                    for (id, h) in modules.drain() {
                        stop(&id, &h);
                        post(&mut ws, &json!({ "event": "deregisterDevice", "payload": id }));
                    }
                    last_scan = ago(Duration::from_millis(1500)); // let the threads let go first
                }
                Ok(Up::Level(device, slot, percent, muted)) => match modules.get(&device) {
                    Some(h) if h.kind == Kind::Dial => {
                        let jpeg = actions::volume_jpeg(percent, muted);
                        slots.screens.insert(format!("{device}/{slot}"), jpeg.clone());
                        if h.slot(0) == slot && !dial_cards.contains_key(&device) && !actions.pinned(&device) {
                            let _ = h.tx.send(Down::ScreenImage(jpeg));
                        }
                    }
                    Some(h) if led_module(h.kind).is_some() => {
                        let mut cfg = shared.write().unwrap();
                        if let Some(c) = cfg.controls.iter_mut().find(|c| c.key == format!("{device}/{slot}")) {
                            c.level = Some((percent, muted));
                        }
                        let control = slot % h.encoders;
                        if h.slot(control) == slot && identify_until.is_none() {
                            push_control_light(&device, h, &cfg, control, false);
                        }
                    }
                    _ => {}
                },
                Ok(msg @ (Up::Signal(..) | Up::Repaint(_))) => {
                    let mut cfg = shared.write().unwrap();
                    let (led, changed) = match msg {
                        Up::Signal(led, rgb) => {
                            let changed = cfg.signal_colours.insert(led.clone(), rgb) != Some(rgb);
                            (led, changed)
                        }
                        Up::Repaint(led) if led == mqtt::BASE_LED => {
                            if identify_until.is_none() {
                                push_base(&base, &cfg, false, true);
                            }
                            continue;
                        }
                        Up::Repaint(led) => (led, true),
                        _ => unreachable!(),
                    };
                    let place = led.rsplit_once('/').and_then(|(id, c)| Some((id, c.parse::<u8>().ok()?)));
                    if let (true, Some((id, control)), None) = (changed, place, identify_until) {
                        if let Some(h) = modules.get(id).filter(|h| control < h.encoders) {
                            push_control_light(id, h, &cfg, control, false);
                        }
                    }
                }
                Ok(Up::Identify) => {
                    identify_until = Some(Instant::now() + IDENTIFY_TIME);
                    shared.write().unwrap().identifying = true;
                    identify(&modules, &base);
                }
                Ok(Up::SaveSettings) => {
                    let settings = shared.read().unwrap().to_json();
                    post(&mut ws, &json!({ "event": "setGlobalSettings", "context": uuid, "payload": settings }));
                }
                Ok(Up::BaseGone) => {
                    base.open = false;
                    base.tx = None;
                    shared.write().unwrap().forget(BASE_ID);
                }
                Ok(Up::LightsChanged(save)) => {
                    let cfg = shared.read().unwrap();
                    push_lights(&modules, &base, &cfg, save);
                    if save {
                        push_lights(&modules, &base, &cfg, false); // signals and App volume's level again
                    }
                }
                Ok(Up::BackgroundChanged) => {
                    let bg = keypad_background(&shared.read().unwrap(), &up_tx);
                    for h in modules.values().filter(|h| h.kind == Kind::Keys) {
                        let _ = h.tx.send(Down::Background(bg.clone()));
                    }
                }
                Ok(Up::Info(info)) => {
                    let mut cfg = shared.write().unwrap();
                    cfg.modules.retain(|m| m.id != info.id);
                    cfg.modules.push(info);
                    cfg.modules.sort_by(|a, b| a.name.cmp(&b.name));
                }
                Ok(Up::LayoutChanged) => {
                    // The number of functions changes the knobs' and dial's slots: register them again.
                    let ids: Vec<String> = modules
                        .iter()
                        .filter(|(_, h)| matches!(h.kind, Kind::Knobs | Kind::Dial))
                        .map(|(id, _)| id.clone())
                        .collect();
                    for id in ids {
                        if let Some(h) = modules.remove(&id) {
                            stop(&id, &h);
                        }
                        shared.write().unwrap().forget(&id);
                        slots.forget(&id);
                        dial_keys.remove(&id);
                        action_screens.remove(&id);
                        dial_cards.remove(&id);
                        volume_dials.retain(|k| !k.starts_with(&format!("{id}/")));
                        post(&mut ws, &json!({ "event": "deregisterDevice", "payload": id }));
                    }
                    last_scan = ago(Duration::from_millis(1500));
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }

        match ws.read() {
            Ok(Message::Text(text)) => {
                if let Ok(msg) = serde_json::from_str::<Value>(text.as_str()) {
                    match msg["event"].as_str() {
                        Some("didReceiveGlobalSettings") => {
                            load_settings(&shared, &msg["payload"]["settings"], &up_tx);
                            push_lights(&modules, &base, &shared.read().unwrap(), false);
                        }
                        Some("showSettingsInterface") => {
                            post(&mut ws, &json!({ "event": "openUrl", "payload": { "url": settings.open_url() } }))
                        }
                        Some(event @ ("keyDown" | "dialDown" | "dialRotate")) if msg["action"] == LIGHTING_UUID => {
                            let settings = &msg["payload"]["settings"];
                            let effect = settings["effect"].as_str().and_then(|e| e.parse().ok()).unwrap_or(1);
                            let ticks =
                                if event == "dialRotate" { msg["payload"]["ticks"].as_i64().unwrap_or(0) } else { 0 };
                            let how = settings["do"].as_str().unwrap_or("toggle");
                            if event == "dialRotate" && how != "next" {
                                continue; // only cycling has a direction
                            }
                            let mut cfg = shared.write().unwrap();
                            lights::lighting_action(&mut cfg.lights, how, effect, ticks);
                            cfg.lights.managed = true; // the action is the plugin managing the LEDs
                            let title = lights::MODES[cfg.lights.mode as usize];
                            post(
                                &mut ws,
                                &json!({ "event": "setGlobalSettings", "context": uuid, "payload": cfg.to_json() }),
                            );
                            post(
                                &mut ws,
                                &json!({ "event": "setTitle", "context": msg["context"], "payload": { "title": title } }),
                            );
                            push_lights(&modules, &base, &cfg, false);
                        }
                        Some(_) if msg["action"] == command::UUID => {
                            let fader =
                                |d: &str, p: u8| shared.read().unwrap().fader_values.get(&format!("{d}/{p}")).copied();
                            for m in command::handle(&msg, fader) {
                                post(&mut ws, &m);
                            }
                        }
                        Some(event) if msg["action"] == appvolume::UUID => {
                            // on a dial the action draws the screen of its slot (its level), not OpenDeck
                            let slot = format!(
                                "{}/{}",
                                msg["device"].as_str().unwrap_or_default(),
                                msg["payload"]["coordinates"]["column"]
                            );
                            match event {
                                "willAppear"
                                    if msg["payload"]["controller"] == "Encoder"
                                        && is_dial(&modules, msg["device"].as_str().unwrap_or_default()) =>
                                {
                                    volume_dials.insert(slot);
                                }
                                "willDisappear" => {
                                    volume_dials.remove(&slot);
                                    // the LED no longer shows a level
                                    let mut cfg = shared.write().unwrap();
                                    if let Some(c) =
                                        cfg.controls.iter_mut().find(|c| c.key == slot && c.level.is_some())
                                    {
                                        c.level = None;
                                        let device = msg["device"].as_str().unwrap_or_default();
                                        if let Some(h) = modules.get(device) {
                                            push_module_lights(device, h, &cfg, false);
                                        }
                                    }
                                }
                                _ => {}
                            }
                            if let Some(device) = msg["device"]
                                .as_str()
                                .filter(|d| is_dial(&modules, d) && msg["payload"]["controller"] == "Encoder")
                            {
                                match msg["payload"]["settings"]["dialScreen"]
                                    .as_str()
                                    .filter(|s| DIAL_SCREENS.iter().any(|(k, _)| k == s))
                                {
                                    Some(screen) if event != "willDisappear" => {
                                        action_screens.insert(device.to_string(), screen.to_string());
                                    }
                                    _ => {
                                        action_screens.remove(device);
                                    }
                                }
                            }
                            let fader =
                                |d: &str, p: u8| shared.read().unwrap().fader_values.get(&format!("{d}/{p}")).copied();
                            for m in appvolume::handle(&msg, fader, &up_tx) {
                                post(&mut ws, &m);
                            }
                        }
                        // Events for our own (dial screen) actions.
                        _ if msg["action"].is_string() => {
                            let outs = actions.handle(&msg, |d| is_dial(&modules, d));
                            deliver(outs, &mut ws, &modules, &mut slots);
                        }
                        // an image (or none) for a dial's key: whether an action sits there
                        Some("setImage")
                            if msg["controller"] == "Keypad"
                                && is_dial(&modules, msg["device"].as_str().unwrap_or_default()) =>
                        {
                            let device = msg["device"].as_str().unwrap_or_default().to_string();
                            if msg["image"].is_string() {
                                dial_keys.insert(device);
                            } else {
                                dial_keys.remove(&device);
                            }
                        }
                        // OpenDeck's own image for a dial slot whose screen one of our actions draws:
                        // only note that the slot is in use.
                        Some("setImage")
                            if msg["device"].as_str().is_some_and(|d| {
                                actions
                                    .owns_dial(d, msg["position"].as_u64().unwrap_or(0) as u8, |d| is_dial(&modules, d))
                                    || (msg["controller"] == "Encoder"
                                        && volume_dials.contains(&format!("{d}/{}", msg["position"])))
                            }) =>
                        {
                            slots.occupied.insert(format!(
                                "{}/{}",
                                msg["device"].as_str().unwrap_or_default(),
                                msg["position"]
                            ));
                        }
                        _ => handle_opendeck(&msg, &modules, &base, &shared, &mut slots),
                    }
                }
            }
            Ok(Message::Close(_)) => return shutdown(&modules),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => {
                eprintln!("WebSocket closed: {e}");
                return shutdown(&modules);
            }
        }
    }
}

type Ws = tungstenite::WebSocket<TcpStream>;

/// OpenDeck's connection is gone (a send failed): the loop shuts down cleanly instead of panicking, so the modules
/// still get handed back to their firmware.
static CLOSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Sends a message to OpenDeck; a failure marks the connection gone (`CLOSED`).
fn post(ws: &mut Ws, msg: &Value) {
    if let Err(e) = ws.send(Message::text(msg.to_string())) {
        if !CLOSED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!("cannot reach OpenDeck: {e}");
        }
    }
}

/// `d` ago, or now when that's before the clock's start (on Windows `Instant` counts from boot, so subtracting can
/// panic in the first minute after power-on).
/// A clock that stands still while the computer sleeps: Instant on macOS and Linux; on Windows, where Instant
/// keeps counting through sleep, the unbiased interrupt time.
fn awake_now() -> Duration {
    #[cfg(windows)]
    {
        let mut t = 0u64; // 100 ns units
        let _ = unsafe { windows::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime(&mut t) };
        Duration::from_nanos(t.saturating_mul(100))
    }
    #[cfg(not(windows))]
    {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        START.get_or_init(Instant::now).elapsed()
    }
}

fn ago(d: Duration) -> Instant {
    Instant::now().checked_sub(d).unwrap_or_else(Instant::now)
}

/// OpenDeck closed the connection: give the keys and the dial back to their firmware before exiting.
/// ponytail: only on a clean close; when the process is killed outright nothing can be sent.
fn shutdown(modules: &HashMap<String, Handle>) {
    for h in modules.values() {
        let _ = h.tx.send(Down::Shutdown);
    }
    std::thread::sleep(Duration::from_millis(300));
}

/// Loads the global settings from OpenDeck. They come a moment after the plugin starts, when the modules may
/// already be registered: what's set up at registration (the keypad background, functions per knob, the base's
/// turn by hand, upright pictures) is set up again when it changed.
fn load_settings(shared: &Shared, settings: &Value, up_tx: &Sender<Up>) {
    let mut cfg = shared.write().unwrap();
    let before = (cfg.keypad_background.clone(), cfg.functions, cfg.base_rotation, cfg.upright);
    cfg.load(settings);
    if before.0 != cfg.keypad_background {
        let _ = up_tx.send(Up::BackgroundChanged);
    }
    if (before.2, before.3) != (cfg.base_rotation, cfg.upright) {
        let _ = up_tx.send(Up::Reorient); // registers every module again, the knobs' functions too
    } else if before.1 != cfg.functions {
        let _ = up_tx.send(Up::LayoutChanged);
    }
}

/// The knob and fader LEDs that can be signals: "<device id>/<control>" and the control's label.
fn signal_leds(cfg: &Config) -> Vec<(String, String)> {
    let mut leds = Vec::new();
    for (id, (kind, ..)) in &cfg.placed {
        let encoders =
            MODELS.iter().find(|m| m.kind == *kind && led_module(m.kind).is_some()).map_or(0, |m| m.encoders);
        for control in 0..encoders {
            let led = format!("{id}/{control}");
            let label = cfg
                .controls
                .iter()
                .find(|c| c.key == led)
                .map_or(led.clone(), |c| c.label.split(", function").next().unwrap_or_default().to_string());
            leds.push((led, label));
        }
    }
    leds
}

/// Where the plugin keeps files the user gives it (an uploaded keypad background): outside the plugin
/// folder, which a reinstall replaces.
fn data_dir() -> std::path::PathBuf {
    let var = |name: &str| std::env::var_os(name).map(std::path::PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = var("HOME").map(|h| h.join("Library/Application Support"));
    #[cfg(windows)]
    let base = var("APPDATA");
    #[cfg(not(any(target_os = "macos", windows)))]
    let base = var("XDG_DATA_HOME").or_else(|| var("HOME").map(|h| h.join(".local/share")));
    // MASTERHUB_DATA: another folder (tests, or keeping the plugin's files elsewhere)
    if let Some(dir) = var("MASTERHUB_DATA") {
        return dir;
    }
    base.unwrap_or_else(std::env::temp_dir).join("opendeck-masterhub")
}

fn is_dial(modules: &HashMap<String, Handle>, device: &str) -> bool {
    modules.get(device).is_some_and(|h| h.kind == Kind::Dial)
}

/// Sends what our actions drew: to OpenDeck, or to a dial slot (shown when it's the current function).
fn deliver(outs: Vec<actions::Out>, ws: &mut Ws, modules: &HashMap<String, Handle>, slots: &mut Slots) {
    for out in outs {
        match out {
            actions::Out::Ws(msg) => post(ws, &msg),
            // a pinned screen ("Dial screen" setting) covers the dial whatever its function
            actions::Out::Dial { device, position: actions::WHOLE_DIAL, jpeg } => {
                if let Some(h) = modules.get(&device) {
                    let _ = h.tx.send(Down::ScreenImage(jpeg));
                }
            }
            actions::Out::Dial { device, position, jpeg } => {
                let Some(h) = modules.get(&device) else { continue }; // the dial went meanwhile
                if h.slot(0) == position {
                    let _ = h.tx.send(Down::ScreenImage(jpeg.clone()));
                }
                let key = format!("{device}/{position}");
                slots.occupied.insert(key.clone());
                slots.screens.insert(key, jpeg);
            }
        }
    }
}

/// Shows the dial's current function: its last screen, or black.
fn show_dial(id: &str, h: &Handle, slots: &Slots) {
    let cmd = slots
        .screens
        .get(&format!("{id}/{}", h.slot(0)))
        .cloned()
        .map_or(Down::ScreenColour([0, 0, 0]), Down::ScreenImage);
    let _ = h.tx.send(cmd);
}

/// Acceleration: a click within 40 ms of the last one on the same knob, roller or dial counts 3 times, within
/// 80 ms twice. Faders report positions, not clicks, so they're left alone.
fn accelerate(msg: &mut Value, modules: &HashMap<String, Handle>, last: &mut HashMap<String, Instant>) {
    let p = &msg["payload"];
    let device = p["device"].as_str().unwrap_or_default();
    if !modules.get(device).is_some_and(|h| matches!(h.kind, Kind::Knobs | Kind::Rollers | Kind::Dial)) {
        return;
    }
    let key = format!("{device}/{}", p["position"]);
    let now = Instant::now();
    let factor = match last.insert(key, now).map(|t| now - t) {
        Some(d) if d < Duration::from_millis(40) => 3,
        Some(d) if d < Duration::from_millis(80) => 2,
        _ => 1,
    };
    if let Some(ticks) = p["ticks"].as_i64() {
        msg["payload"]["ticks"] = json!(ticks * factor);
    }
}

/// Press-to-switch for knobs and the dial (official app): input arrives per physical control and is
/// sent to the slot of its current function. With two or more functions in use a short press
/// switches to the next one and a long press (>= 600 ms) presses the current function; otherwise a
/// press passes straight through. Returns the messages for OpenDeck and the device that switched.
fn route_input(msg: Value, modules: &mut HashMap<String, Handle>, slots: &Slots) -> (Vec<Value>, Option<String>) {
    let event = msg["event"].as_str().unwrap_or_default().to_string();
    let id = msg["payload"]["device"].as_str().unwrap_or_default().to_string();
    let Some(h) = modules.get_mut(&id).filter(|h| matches!(h.kind, Kind::Knobs | Kind::Dial)) else {
        return (vec![msg], None);
    };
    let Some(control) = msg["payload"]["position"].as_u64().map(|p| p as u8).filter(|c| *c < h.encoders) else {
        return (vec![msg], None);
    };
    let at = |slot: u8, event: &str| {
        let mut m = msg.clone();
        m["event"] = json!(event);
        m["payload"]["position"] = json!(slot);
        m
    };
    // the dial's key (1×1 keypad, position 0) takes its press when an action sits there, whatever the function
    if h.kind == Kind::Dial && matches!(event.as_str(), "keyDown" | "keyUp") {
        return (vec![at(0, &event)], None);
    }
    let used: Vec<u8> =
        (0..h.functions).filter(|f| slots.occupied.contains(&format!("{id}/{}", f * h.encoders + control))).collect();
    let c = control as usize;
    // the current function's action was removed: go to one that has an action, so the control isn't dead
    let snapped = match used.first() {
        Some(&first) if !used.contains(&h.active[c]) => {
            h.active[c] = first;
            Some(id.clone())
        }
        _ => None,
    };
    let slot = h.slot(control);
    let (out, switched) = match event.as_str() {
        "encoderDown" if used.len() >= 2 => {
            h.pressed_at[c] = Some(Instant::now());
            (vec![], None)
        }
        "encoderUp" => match h.pressed_at[c].take() {
            Some(t) if t.elapsed() >= LONG_PRESS => (vec![at(slot, "encoderDown"), at(slot, "encoderUp")], None),
            Some(_) => {
                let current = h.active[c];
                let next = used.iter().find(|f| **f > current).or(used.first()).copied().unwrap_or(current);
                h.active[c] = next;
                (vec![], (next != current).then_some(id))
            }
            None => (vec![at(slot, "encoderUp")], None),
        },
        _ => (vec![at(slot, &event)], None),
    };
    (out, switched.or(snapped))
}

#[allow(clippy::too_many_arguments)]
fn scan(api: &mut HidApi, modules: &mut HashMap<String, Handle>, base: &mut Base, up_tx: &Sender<Up>, shared: &Shared) {
    if let Err(e) = api.refresh_devices() {
        eprintln!("HID enumeration failed: {e}");
        return;
    }
    let debug = std::env::var_os("MASTERHUB_DEBUG").is_some();
    // Windows lists every HID collection of a module as its own device path, and only the
    // vendor-defined one (usage page >= 0xff00) accepts the custom reports, so try it first.
    // ponytail: falls back to the first interface (hidapi open(vid, pid), as in the reference SDK)
    // when no vendor page is reported; check MASTERHUB_DEBUG output if a module stays silent.
    let mut infos: Vec<_> = api.device_list().filter(|i| i.vendor_id() == VID).collect();
    infos.sort_by_key(|i| i.usage_page() < 0xff00);
    let mut locations = None; // the USB port lookup runs a system tool, so only when a module is new

    // The base first, so modules register with the right rotation straight away.
    if !base.open {
        if let Some(device) =
            infos.iter().find(|i| BASE_PIDS.contains(&i.product_id())).and_then(|i| i.open_device(api).ok())
        {
            base.open = true;
            // asked for (feature 0x35) first: the base only sends its orientation by itself at power-up and
            // when it's turned, so a plugin started later would otherwise never learn it
            let asked = asked_orientation(&device)
                .or_else(|| read_orientation(&device, Duration::from_millis(500)).ok().flatten());
            let horizontal = asked.unwrap_or(base.horizontal);
            if debug {
                eprintln!("base: horizontal = {horizontal}");
            }
            if modules.is_empty() {
                base.horizontal = horizontal;
            } else {
                // modules registered before the base was found: a different turn registers them again
                let _ = up_tx.send(Up::Orientation(horizontal));
            }
            let (tx, rx) = mpsc::channel();
            base.tx = Some(tx);
            let up_tx = up_tx.clone();
            std::thread::spawn(move || run_base(device, horizontal, rx, up_tx));
        }
    }

    for info in infos {
        let Some(model) = MODELS.iter().find(|m| m.pids.contains(&info.product_id())) else { continue };
        let serial = info.serial_number().unwrap_or("");
        // the serial as the device gives it, only its safe characters (the id goes into pages, topics, names)
        let safe: String =
            serial.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(64).collect();
        let prefix = format!("{NAMESPACE}-{:04x}-", model.pids[0]);
        if !safe.is_empty() && modules.contains_key(&format!("{prefix}{safe}")) {
            continue;
        }
        let locs = locations.get_or_insert_with(location::usb_locations);
        let port = location::port_key(locs, model.pids, serial);
        // without a serial, the socket keeps two modules of a kind apart (and a module's interfaces together)
        let id = match (safe.is_empty(), &port) {
            (false, _) => format!("{prefix}{safe}"),
            (true, Some(p)) => format!("{prefix}socket{p}"),
            (true, None) => format!("{prefix}0"),
        };
        if modules.contains_key(&id) || stopping(&id) {
            continue; // registered, or its old thread still holds the device (registered again next scan)
        }
        if debug {
            eprintln!(
                "found {id}: interface {} usage page {:#06x} path {:?} port {port:?}",
                info.interface_number(),
                info.usage_page(),
                info.path()
            );
        }
        match info.open_device(api) {
            Ok(d) => register(
                d,
                model,
                id,
                port,
                Some(info.path().to_string_lossy().into_owned()),
                modules,
                base,
                up_tx,
                shared,
            ),
            Err(e) => eprintln!("cannot open {id}: {e}"),
        }
    }
}

/// Registers one opened module with OpenDeck and starts its thread.
#[allow(clippy::too_many_arguments)]
fn register(
    device: HidDevice,
    model: &'static Model,
    id: String,
    port: Option<String>,
    path: Option<String>,
    modules: &mut HashMap<String, Handle>,
    base: &Base,
    up_tx: &Sender<Up>,
    shared: &Shared,
) {
    let base_turn = shared.read().unwrap().base_rotation.unwrap_or(if base.horizontal { HORIZONTAL_ROT } else { 0 });
    // a socket not on the base (or not read): turned like the base, its own turn unknown
    let rot = port
        .as_deref()
        .and_then(|p| placement::place(model.kind, p))
        .map_or(base_turn % 360, |p| (p.rotation + base_turn) % 360);
    {
        let mut cfg = shared.write().unwrap();
        cfg.base_turn = base_turn;
        cfg.placed
            .insert(id.clone(), (model.kind, model.name.trim_start_matches("MasterHUB ").to_string(), port.clone()));
    }
    let name = match &port {
        Some(p) => format!("{} · port {p} · {rot}°", model.name),
        None => model.name.to_string(),
    };
    // the dial gets a key next to its encoder: an action put on it takes the dial's press
    let (rows, columns) = match (model.kind, rot % 180 == 90) {
        (Kind::Dial, _) => (1, 1),
        (_, true) => (model.columns, model.rows),
        (_, false) => (model.rows, model.columns),
    };
    let functions = if matches!(model.kind, Kind::Knobs | Kind::Dial) { shared.read().unwrap().functions } else { 1 };
    up_tx
        .send(Up::Send(json!({ "event": "registerDevice", "payload": {
            "id": id, "name": name, "rows": rows, "columns": columns, "encoders": model.encoders * functions, "type": 0,
            "layoutVersion": 1, "layout": placement::layout(model.kind, rot, functions),
        }})))
        .unwrap();

    let (down_tx, down_rx) = mpsc::channel();
    let n = model.encoders as usize;
    let handle = Handle {
        tx: down_tx,
        kind: model.kind,
        rot,
        encoders: model.encoders,
        functions,
        active: vec![0; n],
        pressed_at: vec![None; n],
    };
    if led_module(model.kind).is_some() {
        let mut cfg = shared.write().unwrap();
        let what = if model.kind == Kind::Knobs { "knob" } else { "fader" };
        for slot in 0..model.encoders * functions {
            let (control, function) = (slot % model.encoders + 1, slot / model.encoders + 1);
            let label = match functions {
                1 => format!("{name} — {what} {control}"),
                _ => format!("{name} — {what} {control}, function {function}"),
            };
            cfg.controls.push(Control { key: format!("{id}/{slot}"), label, ..Default::default() });
        }
        push_module_lights(&id, &handle, &cfg, false);
    }
    if model.kind == Kind::Keys {
        if let Some(bg) = keypad_background(&shared.read().unwrap(), up_tx) {
            let _ = handle.tx.send(Down::Background(Some(bg)));
        }
    }
    modules.insert(id.clone(), handle);
    let up_tx = up_tx.clone();
    let shared = shared.clone();
    std::thread::spawn(move || run_module(device, model, id, name, rot, path, down_rx, up_tx, shared));
}

/// Sends the effect and every control colour of one knob or fader module.
fn push_module_lights(id: &str, h: &Handle, cfg: &Config, save: bool) {
    if led_module(h.kind).is_none() || !cfg.lights.managed || cfg.identifying {
        return; // not managed: the LEDs keep what the firmware shows
    }
    let _ = h.tx.send(Down::LedMode { mode: cfg.lights.mode, speed: cfg.lights.speed, save });
    for control in 0..h.encoders {
        push_control_light(id, h, cfg, control, save);
    }
}

/// One knob or fader LED (`control` as shown): its signal, else the state of the control's current function.
fn push_control_light(id: &str, h: &Handle, cfg: &Config, control: u8, save: bool) {
    if led_module(h.kind).is_none() || !cfg.lights.managed || cfg.identifying {
        return;
    }
    let led = format!("{id}/{control}");
    // what's stored in the modules is the lighting itself, never Home Assistant's light, a passing signal or App
    // volume's mute
    let from_ha = mqtt::shown(cfg, &led, Instant::now()).map(|(light, _)| light).filter(|_| !save);
    let signal = cfg
        .lights
        .signals
        .contains_key(&led)
        .then(|| cfg.signal_colours.get(&led).copied())
        .flatten()
        .filter(|_| !save);
    let key = format!("{id}/{}", h.slot(control));
    let control_now = cfg.controls.iter().find(|c| c.key == key);
    let stored = control_now.filter(|_| save).map(|c| Control { level: None, ..c.clone() });
    let (rgb, brightness) = match from_ha {
        Some(light) => (light.rgb, Some(light.led_brightness())),
        None => cfg.lights.led(
            stored.as_ref().or(control_now),
            signal,
            cfg.led_brightness,
            h.kind == Kind::Faders,
            cfg.fader_values.get(&led).copied(),
        ),
    };
    let index = encoder_index(h.kind, h.rot, h.encoders, control);
    let _ = h.tx.send(Down::LedColour { index, rgb, save });
    if let Some(value) = brightness {
        let _ = h.tx.send(Down::LedBrightness { index, value });
    }
}

/// Identify mode: the screens show positions (see `actions::identify_jpeg`), every knob and fader LED the
/// colour of its OpenDeck position (`IDENTIFY_COLOURS`), the base white.
fn identify(modules: &HashMap<String, Handle>, base: &Base) {
    for h in modules.values() {
        let _ = h.tx.send(Down::Identify);
        if led_module(h.kind).is_some() {
            let _ = h.tx.send(Down::LedMode { mode: 1, speed: 0, save: false });
            for p in 0..h.encoders {
                let rgb = IDENTIFY_COLOURS[p as usize % IDENTIFY_COLOURS.len()];
                let _ =
                    h.tx.send(Down::LedColour { index: encoder_index(h.kind, h.rot, h.encoders, p), rgb, save: false });
            }
        }
    }
    if let Some(tx) = &base.tx {
        let _ = tx.send(Down::LedMode { mode: 1, speed: 0, save: false });
        let _ = tx.send(Down::LedColour { index: 0, rgb: [255, 255, 255], save: false });
    }
}

/// Puts the LEDs back after identify mode (the modules restore their own screens): the plugin's lights when
/// it manages them, otherwise the effect read from the module at connect (colour cycling if unknown).
fn end_identify(modules: &HashMap<String, Handle>, base: &Base, cfg: &Config) {
    if cfg.lights.managed {
        return push_lights(modules, base, cfg, false);
    }
    let mode =
        |id: &str| cfg.modules.iter().find(|m| m.id == id).and_then(|m| m.led_modes.first().copied()).unwrap_or(5);
    for (id, h) in modules.iter().filter(|(_, h)| led_module(h.kind).is_some()) {
        let _ = h.tx.send(Down::LedMode { mode: mode(id), speed: cfg.lights.speed, save: false });
    }
    if let Some(tx) = &base.tx {
        let _ = tx.send(Down::LedMode { mode: mode(BASE_ID), speed: cfg.lights.speed, save: false });
    }
}

/// (Re)sends all LED settings: on start, on change and every minute like the official app.
fn push_lights(modules: &HashMap<String, Handle>, base: &Base, cfg: &Config, save: bool) {
    for (id, h) in modules {
        push_module_lights(id, h, cfg, save);
    }
    push_base(base, cfg, save, false);
}

/// The base's LED: Home Assistant's light over the base colour; with neither the base keeps its own effect,
/// put back (`restore`) when Home Assistant lets go of it.
fn push_base(base: &Base, cfg: &Config, save: bool, restore: bool) {
    let (Some(tx), true, false) = (&base.tx, cfg.lights.managed, cfg.identifying) else { return };
    let from_ha = mqtt::shown(cfg, mqtt::BASE_LED, Instant::now()).map(|(light, _)| light).filter(|_| !save);
    let (rgb, brightness) = match (from_ha, cfg.lights.base) {
        (Some(light), _) => (light.rgb, light.led_brightness()),
        (None, Some(rgb)) => (rgb, cfg.led_brightness),
        (None, None) => {
            if restore {
                let mode = cfg
                    .modules
                    .iter()
                    .find(|m| m.id == BASE_ID)
                    .and_then(|m| m.led_modes.first().copied())
                    .unwrap_or(5);
                let _ = tx.send(Down::LedMode { mode, speed: cfg.lights.speed, save: false });
                let _ = tx.send(Down::LedBrightness { index: 0, value: cfg.led_brightness });
            }
            return;
        }
    };
    let mode = if from_ha.is_some() { 1 } else { cfg.lights.mode }; // HA's colour needs the static effect
    let _ = tx.send(Down::LedMode { mode, speed: cfg.lights.speed, save });
    let _ = tx.send(Down::LedColour { index: 0, rgb, save });
    if !save {
        let _ = tx.send(Down::LedBrightness { index: 0, value: brightness });
    }
}

/// Draws Home Assistant's effects and flashes (mqtt::frame) on their LEDs, about 25 times a second; an LED is
/// written when its picture changes and at least every second. Ended flashes give the LED back.
fn animate(
    modules: &HashMap<String, Handle>,
    base: &Base,
    shared: &Shared,
    epoch: Instant,
    sent: &mut HashMap<String, ((lights::Rgb, u8), Instant)>,
) {
    let now = Instant::now();
    let ended: Vec<String> =
        shared.read().unwrap().mqtt_flashes.iter().filter(|(_, f)| f.until <= now).map(|(k, _)| k.clone()).collect();
    if !ended.is_empty() {
        let mut cfg = shared.write().unwrap();
        for led in &ended {
            cfg.mqtt_flashes.remove(led);
            sent.remove(led);
        }
        drop(cfg);
        let cfg = shared.read().unwrap();
        for led in &ended {
            match led.rsplit_once('/').and_then(|(id, c)| Some((id, c.parse::<u8>().ok()?))) {
                _ if led == mqtt::BASE_LED => push_base(base, &cfg, false, true),
                Some((id, control)) => {
                    if let Some(h) = modules.get(id).filter(|h| control < h.encoders) {
                        push_control_light(id, h, &cfg, control, false);
                    }
                }
                None => {}
            }
        }
    }
    let cfg = shared.read().unwrap();
    if !cfg.lights.managed {
        return;
    }
    let animated: Vec<&String> =
        cfg.mqtt_flashes.keys().chain(cfg.mqtt_lights.iter().filter(|(_, l)| l.animated()).map(|(k, _)| k)).collect();
    sent.retain(|led, _| animated.contains(&led));
    for led in animated {
        let Some((light, from)) = mqtt::shown(&cfg, led, epoch) else { continue };
        let picture = mqtt::frame(&light, now - from);
        if sent.get(led).is_some_and(|(p, at)| *p == picture && at.elapsed() < Duration::from_secs(1)) {
            continue;
        }
        // once a second everything again, in case something else wrote the LED meanwhile
        let refresh = sent.get(led).is_none_or(|(_, at)| at.elapsed() >= Duration::from_secs(1));
        let colour_changed = refresh || sent.get(led).is_some_and(|((rgb, _), _)| *rgb != picture.0);
        let (tx, index) = match led.rsplit_once('/').and_then(|(id, c)| Some((id, c.parse::<u8>().ok()?))) {
            _ if led == mqtt::BASE_LED => match &base.tx {
                Some(tx) => (tx, 0),
                None => continue,
            },
            Some((id, control)) => {
                match modules.get(id).filter(|h| control < h.encoders && led_module(h.kind).is_some()) {
                    Some(h) => (&h.tx, encoder_index(h.kind, h.rot, h.encoders, control)),
                    None => continue,
                }
            }
            None => continue,
        };
        let brightness_changed = refresh || sent.get(led).is_some_and(|((_, b), _)| *b != picture.1);
        if colour_changed {
            let _ = tx.send(Down::LedColour { index, rgb: picture.0, save: false });
        }
        if brightness_changed {
            let _ = tx.send(Down::LedBrightness { index, value: picture.1 });
        }
        sent.insert(led.clone(), (picture, Instant::now()));
    }
}

/// Fader scales from the global settings; anything missing or out of range falls back to 1.
fn read_scales(settings: &Value) -> [f64; FADERS] {
    std::array::from_fn(|i| settings["faderScale"][i].as_f64().filter(|v| (0.01..=100.0).contains(v)).unwrap_or(1.0))
}

fn handle_opendeck(msg: &Value, modules: &HashMap<String, Handle>, base: &Base, shared: &Shared, slots: &mut Slots) {
    let Some((id, h)) = msg["device"].as_str().and_then(|d| modules.get_key_value(d)) else { return };
    let image = msg["image"].as_str();
    if msg["event"] == "setImage" {
        // Which encoder slots hold an action (press-to-switch only cycles through those).
        match (msg["controller"].as_str(), msg["position"].as_u64()) {
            (None, _) => slots.forget(id),
            (Some("Encoder"), Some(p)) if image.is_some() => {
                slots.occupied.insert(format!("{id}/{p}"));
            }
            (Some("Encoder"), Some(p)) => {
                slots.occupied.remove(&format!("{id}/{p}"));
                slots.screens.remove(&format!("{id}/{p}"));
            }
            _ => {}
        }
    }
    let cmd = match (msg["event"].as_str(), msg["controller"].as_str(), h.kind) {
        // Knobs and faders have no screen: the action image only picks the LED colour.
        (Some("setImage"), controller, Kind::Knobs | Kind::Faders) => {
            let mut cfg = shared.write().unwrap();
            let key = msg["position"].as_u64().map(|p| format!("{id}/{p}"));
            for c in cfg.controls.iter_mut().filter(|c| controller.is_none() || Some(&c.key) == key.as_ref()) {
                c.image = image.filter(|_| controller.is_some()).map(str::to_string);
                c.hash = c.image.as_deref().map(lights::hash);
                c.auto = c
                    .image
                    .as_deref()
                    .and_then(|uri| decode_data_uri(uri).ok())
                    .and_then(|img| lights::image_colour(&img));
            }
            push_module_lights(id, h, &cfg, false);
            return;
        }
        (Some("setImage"), None, Kind::Keys) => Down::KeyColor { position: None, rgb: [0, 0, 0] },
        (Some("setImage"), None, Kind::Dial) => Down::ScreenColour([0, 0, 0]), // slots forgotten above
        (Some("setImage"), Some("Keypad"), Kind::Keys) => {
            let Some(position) = display_to_key(msg["position"].as_u64().unwrap_or(0) as u8, h.rot) else { return };
            match image.map(to_key_jpegs) {
                Some(Ok((jpeg, pressed))) => Down::KeyImage { position, jpeg, pressed },
                Some(Err(e)) => {
                    eprintln!("bad image for key {position}: {e}");
                    return;
                }
                None => Down::KeyColor { position: Some(position), rgb: [0, 0, 0] },
            }
        }
        (Some("setImage"), Some("Encoder"), Kind::Dial) => {
            let position = msg["position"].as_u64().unwrap_or(0) as u8;
            let jpeg = match image.map(to_screen_jpeg) {
                Some(Ok(jpeg)) => Some(jpeg),
                Some(Err(e)) => {
                    eprintln!("bad image for the dial screen: {e}");
                    return;
                }
                None => None,
            };
            if let Some(jpeg) = &jpeg {
                slots.screens.insert(format!("{id}/{position}"), jpeg.clone());
            }
            if position != h.slot(0) {
                return; // not the current function: shown when the dial switches to it
            }
            jpeg.map_or(Down::ScreenColour([0, 0, 0]), Down::ScreenImage)
        }
        (Some("setBrightness"), _, kind) => {
            let b = msg["brightness"].as_u64().unwrap_or(100) as u8;
            // LED brightness only once the plugin manages the lighting; screens always follow OpenDeck.
            let managed = {
                let mut cfg = shared.write().unwrap();
                cfg.led_brightness = b.min(100);
                cfg.lights.managed
            };
            if let (Some(tx), true) = (&base.tx, managed) {
                let _ = tx.send(Down::Brightness(b)); // the base has no OpenDeck device of its own
            }
            if led_module(kind).is_some() && !managed {
                return;
            }
            Down::Brightness(b)
        }
        _ => return,
    };
    let _ = h.tx.send(cmd);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_arriving_late_set_the_modules_up_again() {
        let shared: Shared = Arc::new(RwLock::new(Config::new()));
        let (up_tx, up_rx) = mpsc::channel();
        let settings = json!({ "keypadBackground": "/x/bg.gif", "functions": 2 });
        load_settings(&shared, &settings, &up_tx);
        let ups: Vec<Up> = up_rx.try_iter().collect();
        assert!(matches!(ups[..], [Up::BackgroundChanged, Up::LayoutChanged]));
        load_settings(&shared, &settings, &up_tx); // the same again: nothing to redo
        assert!(up_rx.try_recv().is_err());
        load_settings(&shared, &json!({ "keypadBackground": "/x/bg.gif", "functions": 3, "upright": true }), &up_tx);
        assert!(matches!(up_rx.try_iter().collect::<Vec<_>>()[..], [Up::Reorient]));
    }

    #[test]
    fn home_assistant_effects_and_flashes() {
        let (tx, rx) = mpsc::channel();
        let modules = HashMap::from([("K".to_string(), knobs(tx, 0, 1))]);
        let mut cfg = Config::new();
        cfg.lights.managed = true;
        cfg.mqtt_lights.insert("K/1".into(), mqtt::Light { rgb: [255, 0, 0], brightness: 255, effect: 3 }); // blink
        let shared: Shared = Arc::new(RwLock::new(cfg));
        let (base, epoch, mut sent) = (Base::default(), Instant::now(), HashMap::new());
        animate(&modules, &base, &shared, epoch, &mut sent);
        let out: Vec<_> = rx.try_iter().collect();
        assert!(matches!(
            out[..],
            [Down::LedColour { index: 1, rgb: [255, 0, 0], .. }, Down::LedBrightness { index: 1, value: 100 }]
        ));
        animate(&modules, &base, &shared, epoch, &mut sent);
        assert!(rx.try_iter().next().is_none()); // the same picture isn't sent again

        // an ended flash is dropped and the LED shows its own colour again (no action: off)
        let light = mqtt::Light { rgb: [0, 0, 255], brightness: 255, effect: 4 };
        let past = Instant::now() - Duration::from_secs(3);
        shared
            .write()
            .unwrap()
            .mqtt_flashes
            .insert("K/0".into(), mqtt::Flash { light, from: past, until: past + Duration::from_secs(2) });
        animate(&modules, &base, &shared, epoch, &mut sent);
        assert!(shared.read().unwrap().mqtt_flashes.is_empty());
        assert!(rx.try_iter().any(|d| matches!(d, Down::LedColour { index: 0, rgb: [0, 0, 0], .. })));
    }

    #[test]
    fn knob_led_shows_signal_then_level() {
        let (tx, rx) = mpsc::channel();
        let h = knobs(tx, 0, 1);
        let mut cfg = Config::new();
        cfg.lights.managed = true;
        cfg.led_brightness = 80;
        cfg.controls = vec![Control {
            key: "K/1".into(),
            auto: Some([0, 0, 255]),
            level: Some((50, false)),
            ..Default::default()
        }];
        push_control_light("K", &h, &cfg, 1, false);
        let sent: Vec<_> = rx.try_iter().collect();
        assert!(matches!(
            sent[..],
            [Down::LedColour { index: 1, rgb: [0, 0, 255], .. }, Down::LedBrightness { index: 1, value: 40 }]
        ));
        cfg.lights.signals.insert("K/1".into(), "true".into());
        cfg.signal_colours.insert("K/1".into(), [0, 255, 0]);
        push_control_light("K", &h, &cfg, 1, false);
        let sent: Vec<_> = rx.try_iter().collect();
        assert!(matches!(sent[..], [Down::LedColour { rgb: [0, 255, 0], .. }, Down::LedBrightness { value: 80, .. }]));
    }

    #[test]
    fn fast_turns_accelerate() {
        let (tx, _rx) = mpsc::channel();
        let modules = HashMap::from([("K".to_string(), knobs(tx, 0, 1))]);
        let mut last = HashMap::new();
        let turn = || json!({ "event": "encoderChange", "payload": { "device": "K", "position": 0, "ticks": 1 } });
        let mut first = turn();
        accelerate(&mut first, &modules, &mut last);
        assert_eq!(first["payload"]["ticks"], 1); // nothing before it
        let mut fast = turn();
        accelerate(&mut fast, &modules, &mut last);
        assert_eq!(fast["payload"]["ticks"], 3); // right after
        let mut other = json!({ "event": "encoderChange", "payload": { "device": "F", "position": 0, "ticks": 1 } });
        accelerate(&mut other, &modules, &mut last);
        assert_eq!(other["payload"]["ticks"], 1); // unknown device / faders untouched
    }

    #[test]
    fn lights_follow_images_and_settings() {
        let (tx, rx) = mpsc::channel();
        let mut modules = HashMap::new();
        modules.insert("mh-0202-K".to_string(), knobs(tx, 180, 1));
        let mut cfg = Config::new();
        cfg.lights.managed = true;
        cfg.controls = (0..3).map(|p| Control { key: format!("mh-0202-K/{p}"), ..Default::default() }).collect();
        let shared: Shared = Arc::new(RwLock::new(cfg));
        let base = Base::default();

        // OpenDeck draws a red image on the first knob as shown; the module is upside down -> LED 2.
        let mut red = RgbImage::new(16, 16);
        red.pixels_mut().for_each(|p| *p = image::Rgb([220, 0, 0]));
        let mut png = Vec::new();
        DynamicImage::from(red).write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let uri = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png));
        let mut slots = Slots::default();
        handle_opendeck(
            &json!({ "event": "setImage", "device": "mh-0202-K", "controller": "Encoder", "position": 0, "image": uri }),
            &modules,
            &base,
            &shared,
            &mut slots,
        );
        assert!(slots.occupied.contains("mh-0202-K/0"));

        let sent: Vec<_> = rx.try_iter().collect();
        assert!(matches!(sent[0], Down::LedMode { mode: 1, speed: 50, save: false }));
        assert!(sent.iter().any(|d| matches!(d, Down::LedColour { index: 2, rgb: [255, 0, 0], .. })));
        assert!(sent.iter().any(|d| matches!(d, Down::LedColour { index: 0, rgb: [0, 0, 0], .. }))); // no action -> off

        // A colour picked for that image (state) wins over the image's own colour.
        let hash = lights::hash(&uri);
        shared.write().unwrap().lights.icons.insert(hash, [0, 0, 255]);
        push_lights(&modules, &base, &shared.read().unwrap(), false);
        assert!(rx.try_iter().any(|d| matches!(d, Down::LedColour { index: 2, rgb: [0, 0, 255], .. })));

        // "Save and store in the modules" sets the flash flag on the effect and every colour.
        rx.try_iter().for_each(drop);
        push_lights(&modules, &base, &shared.read().unwrap(), true);
        let sent: Vec<_> = rx.try_iter().collect();
        assert!(
            !sent.is_empty()
                && sent.iter().all(|d| matches!(
                    d,
                    Down::LedMode { save: true, .. } | Down::LedColour { save: true, .. } | Down::LedBrightness { .. }
                ))
        );
    }

    fn knobs(tx: Sender<Down>, rot: u16, functions: u8) -> Handle {
        Handle { tx, kind: Kind::Knobs, rot, encoders: 3, functions, active: vec![0; 3], pressed_at: vec![None; 3] }
    }

    #[test]
    fn press_switches_functions() {
        let (tx, _rx) = mpsc::channel();
        let mut modules = HashMap::from([("K".to_string(), knobs(tx, 0, 3))]);
        let mut slots = Slots::default();
        let ev = |event: &str, position: u8| json!({ "event": event, "payload": { "device": "K", "position": position, "ticks": 1 } });
        let pos = |msgs: &[Value]| {
            msgs.iter()
                .map(|m| (m["event"].as_str().unwrap().to_string(), m["payload"]["position"].as_u64().unwrap()))
                .collect::<Vec<_>>()
        };

        // Knob 2 (control 1) has one function: everything passes through to slot 1.
        slots.occupied.insert("K/1".into());
        let (out, _) = route_input(ev("encoderDown", 1), &mut modules, &slots);
        assert_eq!(pos(&out), [("encoderDown".into(), 1)]);
        let (out, _) = route_input(ev("encoderUp", 1), &mut modules, &slots);
        assert_eq!(pos(&out), [("encoderUp".into(), 1)]);

        // Functions 1 and 3 in use (slots 1 and 7): a short press switches 1 -> 3, turning goes there.
        slots.occupied.insert("K/7".into());
        assert!(route_input(ev("encoderDown", 1), &mut modules, &slots).0.is_empty());
        let (out, switched) = route_input(ev("encoderUp", 1), &mut modules, &slots);
        assert!(out.is_empty());
        assert_eq!(switched.as_deref(), Some("K"));
        assert_eq!(pos(&route_input(ev("encoderChange", 1), &mut modules, &slots).0), [("encoderChange".into(), 7)]);

        // A long press presses the current function instead of switching.
        route_input(ev("encoderDown", 1), &mut modules, &slots);
        modules.get_mut("K").unwrap().pressed_at[1] = Some(Instant::now() - LONG_PRESS);
        let (out, switched) = route_input(ev("encoderUp", 1), &mut modules, &slots);
        assert_eq!(pos(&out), [("encoderDown".into(), 7), ("encoderUp".into(), 7)]);
        assert!(switched.is_none());

        // Next short press wraps around to function 1; other devices are untouched.
        route_input(ev("encoderDown", 1), &mut modules, &slots);
        route_input(ev("encoderUp", 1), &mut modules, &slots);
        assert_eq!(modules["K"].active[1], 0);
        let other = json!({ "event": "keyDown", "payload": { "device": "X", "position": 4 } });
        assert_eq!(route_input(other.clone(), &mut modules, &slots).0, vec![other]);
    }

    #[test]
    fn controls_leave_an_emptied_function() {
        let (tx, _rx) = mpsc::channel();
        let mut modules = HashMap::from([("K".to_string(), knobs(tx.clone(), 0, 2))]);
        let mut slots = Slots::default();
        let ev = |event: &str, position: u8| json!({ "event": event, "payload": { "device": "K", "position": position, "ticks": 1 } });
        // knob 2 on function 2 (slot 4), then that function's action is removed
        slots.occupied.extend(["K/1".to_string(), "K/4".to_string()]);
        modules.get_mut("K").unwrap().active[1] = 1;
        slots.occupied.remove("K/4");
        let (out, switched) = route_input(ev("encoderChange", 1), &mut modules, &slots);
        assert_eq!(out[0]["payload"]["position"], 1); // back on function 1
        assert_eq!(switched.as_deref(), Some("K"));

        // the dial's key takes its press at position 0, whatever function the dial is on
        let dial =
            Handle { tx, kind: Kind::Dial, rot: 0, encoders: 1, functions: 2, active: vec![1], pressed_at: vec![None] };
        let mut modules = HashMap::from([("D".to_string(), dial)]);
        slots.occupied.extend(["D/0".to_string(), "D/1".to_string()]);
        let key = json!({ "event": "keyDown", "payload": { "device": "D", "position": 0 } });
        let (out, _) = route_input(key, &mut modules, &slots);
        assert_eq!((out[0]["event"].as_str(), out[0]["payload"]["position"].as_u64()), (Some("keyDown"), Some(0)));
    }

    #[test]
    fn leaves_firmware_lighting_alone_until_managed() {
        let (tx, rx) = mpsc::channel();
        let h = knobs(tx, 0, 1);
        let mut cfg = Config::new();
        cfg.controls = vec![Control { key: "K/0".into(), auto: Some([255, 0, 0]), ..Default::default() }];
        push_module_lights("K", &h, &cfg, false);
        assert!(rx.try_iter().next().is_none());
        cfg.lights.managed = true;
        push_module_lights("K", &h, &cfg, false);
        assert!(rx.try_iter().any(|d| matches!(d, Down::LedColour { index: 0, rgb: [255, 0, 0], .. })));
    }
}
