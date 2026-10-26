//! Home Assistant over MQTT: every knob and fader LED and the base's LED is an MQTT light (RGB, brightness,
//! effects, flash) that HA finds by itself (MQTT discovery), one HA device per MasterHUB module. Turning a light
//! on lights its LED with that colour, over whatever the LED shows; turning it off gives the LED back to its
//! function. Effects are drawn by the plugin (`frame`), so every LED has its own; a flash blinks the LED for a
//! moment (a notification) and leaves the light as it was.
//!
//! The plugin only connects out to the broker (rumqttc, TLS through rustls) and never listens: nothing on the
//! computer is opened to the network. With TLS and a CA file only a broker with a certificate from that CA is
//! trusted; without a CA file, the system's certificates. A broker going away is retried by rumqttc; HA shows the
//! lights unavailable through the will message.

use std::collections::{BTreeSet, HashMap};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rumqttc::{Client, ConnectReturnCode, Event, LastWill, MqttOptions, Packet, QoS, TlsConfiguration, Transport};
use serde_json::{json, Value};

use crate::lights::{esc, Rgb};
use crate::secret::{self, Store};
use crate::{Kind, Shared, Up, MODELS};

/// The connection as the settings page shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub tls: bool,
    /// PEM file of the CA that signed the broker's certificate; None trusts the system's certificates.
    pub ca_file: Option<String>,
    pub username: String,
    /// Where the password is kept (secret.rs: the keychain, or a user-only file), None without one. The password
    /// itself is never in the settings.
    pub password_store: Option<Store>,
    /// Counts password changes (not saved): a new password makes these settings differ, so the session reconnects.
    pub password_changes: u32,
    /// A password still in OpenDeck's settings from an earlier version, to move into the keychain.
    pub legacy_password: Option<String>,
    /// First part of the plugin's topics: `<prefix>/<module>/<control>/set`.
    pub prefix: String,
    /// Home Assistant's discovery prefix.
    pub discovery: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: false,
            host: String::new(),
            port: 1883,
            tls: false,
            ca_file: None,
            username: String::new(),
            password_store: None,
            password_changes: 0,
            legacy_password: None,
            prefix: "masterhub".into(),
            discovery: "homeassistant".into(),
        }
    }
}

/// What the settings form did with the password.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Password {
    Keep,
    Stored(Store),
    Failed,
    Cleared,
}

/// Deals with the form's password, before the settings are locked: a new one goes into the keychain (which may ask
/// the user), no user name forgets it, an empty field keeps it. The page never gets the password back.
pub fn password_from_form(params: &[(String, String)]) -> Password {
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).unwrap_or_default();
    if get("mqtt_user").trim().is_empty() {
        secret::delete();
        return Password::Cleared;
    }
    match get("mqtt_pass") {
        "" => Password::Keep,
        new => secret::save(new).map_or(Password::Failed, Password::Stored),
    }
}

/// A topic part the user picks: letters, digits, `_`, `-` and `/` between parts, never MQTT's wildcards.
fn topic_part(s: &str) -> Option<String> {
    let s = s.trim().trim_matches('/');
    let ok = !s.is_empty()
        && s.len() <= 64
        && !s.contains("//")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/'));
    ok.then(|| s.to_string())
}

/// A host name or IPv4 address, nothing else (no scheme, port, IPv6 or spaces: rumqttc joins host and port with
/// a colon).
fn host(s: &str) -> Option<String> {
    let s = s.trim();
    let ok = !s.is_empty() && s.len() <= 253 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
    ok.then(|| s.to_string())
}

impl Settings {
    pub fn from_json(v: &Value) -> Self {
        let d = Settings::default();
        let text = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        Settings {
            enabled: v["enabled"].as_bool().unwrap_or(false),
            host: host(&text("host")).unwrap_or_default(),
            port: v["port"].as_u64().filter(|p| (1..=65535).contains(p)).map_or(d.port, |p| p as u16),
            tls: v["tls"].as_bool().unwrap_or(false),
            ca_file: v["caFile"].as_str().map(str::trim).filter(|f| !f.is_empty()).map(str::to_string),
            username: text("username"),
            password_store: v["passwordStore"].as_str().and_then(Store::from_name),
            password_changes: 0,
            legacy_password: v["password"].as_str().filter(|p| !p.is_empty()).map(str::to_string),
            prefix: topic_part(&text("prefix")).unwrap_or(d.prefix),
            discovery: topic_part(&text("discovery")).unwrap_or(d.discovery),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({ "enabled": self.enabled, "host": self.host, "port": self.port, "tls": self.tls, "caFile": self.ca_file,
                "username": self.username, "passwordStore": self.password_store.map(Store::name), "prefix": self.prefix, "discovery": self.discovery })
    }

    /// Applies the MQTT part of the settings form and says what it didn't take (a value it keeps the old one
    /// for). The password itself was dealt with before (`password_from_form`, outside the settings' lock: the
    /// keychain may ask the user); `password` says what came of it.
    pub fn apply_form(&mut self, params: &[(String, String)], password: Password) -> Vec<String> {
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).unwrap_or_default();
        let mut notes = Vec::new();
        self.enabled = !get("mqtt_on").is_empty();
        match get("mqtt_host").trim() {
            "" => self.host.clear(),
            h => match host(h) {
                Some(h) => self.host = h,
                None => notes.push(format!(
                    "MQTT broker host “{h}” not taken: a host name or IPv4 address only (the port has its own field)."
                )),
            },
        }
        match get("mqtt_port").trim().parse::<u16>() {
            Ok(p) if p > 0 => self.port = p,
            _ => notes.push("MQTT port not taken: 1–65535.".into()),
        }
        self.tls = !get("mqtt_tls").is_empty();
        self.ca_file = Some(get("mqtt_ca").trim().to_string()).filter(|f| !f.is_empty());
        self.username = get("mqtt_user").trim().to_string();
        match password {
            Password::Keep => {}
            Password::Stored(store) => {
                self.password_store = Some(store);
                self.password_changes += 1;
                self.legacy_password = None;
            }
            Password::Failed => notes
                .push("MQTT password not stored: neither the keychain nor the plugin's data folder took it.".into()),
            Password::Cleared => {
                self.password_store = None;
                self.password_changes += 1;
                self.legacy_password = None;
            }
        }
        let prefix = topic_part(get("mqtt_prefix"));
        let discovery = topic_part(get("mqtt_discovery"));
        match (prefix, discovery) {
            (Some(p), Some(d)) if p == d || p.starts_with(&format!("{d}/")) || d.starts_with(&format!("{p}/")) => {
                notes.push(
                    "MQTT prefixes not taken: the topic prefix and Home Assistant's discovery prefix must differ."
                        .into(),
                );
            }
            (p, d) => {
                match p {
                    Some(p) => self.prefix = p,
                    None => notes.push("MQTT topic prefix not taken: letters, digits, _ - and / only.".into()),
                }
                match d {
                    Some(d) => self.discovery = d,
                    None => {
                        notes.push("Home Assistant discovery prefix not taken: letters, digits, _ - and / only.".into())
                    }
                }
            }
        }
        notes
    }

    /// The MQTT part of the settings page; `status` is the connection as the MQTT thread last saw it.
    pub fn form_html(&self, status: &str) -> String {
        let checked = |on: bool| if on { " checked" } else { "" };
        format!(
            "<h2>Home Assistant (MQTT)</h2>\
             <p class=lead>Every knob, fader and base LED shows up in Home Assistant as a light (MQTT discovery) \
             with colour, brightness, effects and flash: on lights it over whatever it shows, off gives it back to \
             its function. The plugin only \
             connects to your broker; nothing is opened on this computer. Needs “Manage the LEDs from the plugin” \
             and the Static effect.</p>\
             <p class=hint>{}</p>\
             <label>Connect to the broker<input type=checkbox name=mqtt_on{}></label>\
             <label>Broker host<input name=mqtt_host value=\"{}\" placeholder=\"homeassistant.local\"></label>\
             <label>Port (1883, TLS usually 8883)<input name=mqtt_port type=number min=1 max=65535 value={}></label>\
             <label>TLS (encrypted, the broker's certificate checked)<input type=checkbox name=mqtt_tls{}></label>\
             <label>CA certificate file (PEM; empty: the system's certificates)<input name=mqtt_ca value=\"{}\" placeholder=\"/path/to/ca.crt\"></label>\
             <label>User name<input name=mqtt_user value=\"{}\" autocomplete=off></label>\
             <label>Password ({})<input name=mqtt_pass type=password value=\"\" autocomplete=new-password></label>\
             <label>Topic prefix<input name=mqtt_prefix value=\"{}\"></label>\
             <label>Home Assistant discovery prefix<input name=mqtt_discovery value=\"{}\"></label>",
            esc(status),
            checked(self.enabled),
            esc(&self.host),
            self.port,
            checked(self.tls),
            esc(self.ca_file.as_deref().unwrap_or_default()),
            esc(&self.username),
            match self.password_store {
                None => "none set",
                Some(Store::Keychain) => "in the system's keychain; empty keeps it",
                Some(Store::File) => "in a user-only file (no keychain here); empty keeps it",
            },
            esc(&self.prefix),
            esc(&self.discovery)
        )
    }
}

/// The effects Home Assistant offers, by index in `Light::effect`.
pub const EFFECTS: [&str; 6] = ["Solid", "Pulse", "Slow pulse", "Blink", "Fast blink", "Rainbow"];
/// A flash: (effect, how long), for HA's `flash: short` and `flash: long`.
const FLASH_SHORT: (u8, Duration) = (4, Duration::from_secs(2));
const FLASH_LONG: (u8, Duration) = (3, Duration::from_secs(10));

/// A light set from Home Assistant: colour, HA's brightness (0–255) and effect (index in `EFFECTS`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Light {
    pub rgb: Rgb,
    pub brightness: u8,
    pub effect: u8,
}

impl Light {
    /// The LED's brightness (0–100).
    pub fn led_brightness(&self) -> u8 {
        ((self.brightness as u16 * 100 + 127) / 255) as u8
    }

    /// Whether the plugin has to draw it over time.
    pub fn animated(&self) -> bool {
        self.effect != 0
    }
}

/// What an LED shows `t` into its effect: colour and LED brightness (0–100).
pub fn frame(light: &Light, t: Duration) -> (Rgb, u8) {
    let full = light.led_brightness();
    let phase = |period: f32| (t.as_secs_f32() % period) / period; // 0..1
    let wave = |period: f32| 0.5 - 0.5 * (phase(period) * std::f32::consts::TAU).cos(); // 0..1, starting dark
    let scaled = |k: f32| (full as f32 * k).round() as u8;
    match light.effect {
        1 => (light.rgb, scaled(0.08 + 0.92 * wave(2.0))),
        2 => (light.rgb, scaled(0.08 + 0.92 * wave(5.0))),
        3 => (light.rgb, if phase(1.0) < 0.5 { full } else { 0 }),
        4 => (light.rgb, if phase(0.25) < 0.5 { full } else { 0 }),
        5 => (hue(phase(6.0)), full),
        _ => (light.rgb, full),
    }
}

/// A fully saturated colour around the colour wheel (0..1).
fn hue(h: f32) -> Rgb {
    let x = (h.fract() * 6.0) as u8;
    let f = ((h.fract() * 6.0).fract() * 255.0) as u8;
    match x {
        0 => [255, f, 0],
        1 => [255 - f, 255, 0],
        2 => [0, 255, f],
        3 => [0, 255 - f, 255],
        4 => [f, 0, 255],
        _ => [255, 0, 255 - f],
    }
}

/// A knob or fader LED as Home Assistant sees it.
#[derive(Clone, Debug, PartialEq)]
struct Led {
    module: String,
    control: u8,
    name: String,
    device: String,
    /// The module's firmware versions and serial number, as it reports them (device_info).
    firmware: Option<String>,
    serial: Option<String>,
}

impl Led {
    fn key(&self) -> String {
        format!("{}/{}", self.module, self.control)
    }
}

/// The base's LED, by the same "<module>/<control>" key as the others.
pub const BASE_LED: &str = "mh-base/0";

/// The LEDs of the knob and fader modules registered now, and the base's.
fn leds(cfg: &crate::Config) -> Vec<Led> {
    let about = |id: &str| {
        let info = cfg.modules.iter().find(|m| m.id == id);
        let firmware = info.map(|i| i.firmware.join(" / ")).filter(|f| !f.is_empty());
        (firmware, info.and_then(|i| i.serial.clone()))
    };
    let mut leds = Vec::new();
    if cfg.modules.iter().any(|m| m.id == crate::BASE_ID) {
        let (firmware, serial) = about(crate::BASE_ID);
        leds.push(Led {
            module: crate::BASE_ID.into(),
            control: 0,
            name: "Base".into(),
            device: "MasterHUB Base".into(),
            firmware,
            serial,
        });
    }
    for (module, (kind, short, _)) in &cfg.placed {
        let what = match kind {
            Kind::Knobs => "Knob",
            Kind::Faders => "Fader",
            _ => continue,
        };
        let encoders = MODELS.iter().find(|m| m.kind == *kind).map_or(0, |m| m.encoders);
        let (firmware, serial) = about(module);
        for control in 0..encoders {
            let (firmware, serial) = (firmware.clone(), serial.clone());
            leds.push(Led {
                module: module.clone(),
                control,
                name: format!("{what} {}", control + 1),
                device: format!("MasterHUB {short}"),
                firmware,
                serial,
            });
        }
    }
    leds
}

/// Home Assistant's discovery message for one LED: (topic, retained payload).
fn discovery(s: &Settings, led: &Led) -> (String, String) {
    let base = format!("{}/{}/{}", s.prefix, led.module, led.control);
    let config = json!({
        "name": led.name,
        "unique_id": format!("{}_{}_led", led.module, led.control),
        "schema": "json",
        "supported_color_modes": ["rgb"],
        "brightness": true,
        "effect": true,
        "effect_list": EFFECTS,
        "flash": true,
        "flash_time_short": FLASH_SHORT.1.as_secs(),
        "flash_time_long": FLASH_LONG.1.as_secs(),
        "command_topic": format!("{base}/set"),
        "state_topic": format!("{base}/state"),
        "availability": [{ "topic": format!("{}/status", s.prefix) }, { "topic": format!("{}/{}/availability", s.prefix, led.module) }],
        "availability_mode": "all",
        "device": device(led),
        "origin": { "name": "opendeck-masterhub", "sw_version": env!("CARGO_PKG_VERSION") },
    });
    (format!("{}/light/{}/led{}/config", s.discovery, led.module, led.control), config.to_string())
}

/// A module as a Home Assistant device: its model, firmware and serial number, connected through the plugin.
fn device(led: &Led) -> Value {
    let mut device = json!({ "identifiers": [led.module], "name": led.device, "manufacturer": "Cooler Master", "model": led.device, "via_device": PLUGIN_DEVICE });
    if let Some(f) = &led.firmware {
        device["sw_version"] = json!(f);
    }
    if let Some(s) = &led.serial {
        device["serial_number"] = json!(s);
    }
    device
}

/// The plugin as a Home Assistant device, the modules connected through it.
const PLUGIN_DEVICE: &str = "opendeck-masterhub";

/// The plugin's diagnostic sensor: its version as the state; OpenDeck's version, the system and every module
/// (model, serial number, firmware) as attributes. (topic, retained payload).
fn plugin_discovery(s: &Settings) -> (String, String) {
    let state = format!("{}/plugin/state", s.prefix);
    let config = json!({
        "name": "Plugin version",
        "unique_id": "opendeck_masterhub_plugin_version",
        "state_topic": state,
        "value_template": "{{ value_json.version }}",
        "json_attributes_topic": state,
        "entity_category": "diagnostic",
        "icon": "mdi:puzzle",
        "availability_topic": format!("{}/status", s.prefix),
        "device": { "identifiers": [PLUGIN_DEVICE], "name": "OpenDeck MasterHUB plugin", "manufacturer": "opendeck-masterhub",
                    "model": "OpenDeck plugin", "sw_version": env!("CARGO_PKG_VERSION") },
        "origin": { "name": "opendeck-masterhub", "sw_version": env!("CARGO_PKG_VERSION") },
    });
    (format!("{}/sensor/{PLUGIN_DEVICE}/version/config", s.discovery), config.to_string())
}

/// The software the plugin runs with, for the plugin's sensor: its version, OpenDeck's, the system, the modules.
fn software(cfg: &crate::Config) -> Value {
    let app = &cfg.host["application"];
    let modules: Vec<Value> =
        cfg.modules.iter().map(|m| json!({ "name": m.name, "serial": m.serial, "firmware": m.firmware })).collect();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "opendeck": app["version"],
        "system": format!("{} {}", std::env::consts::OS, app["platformVersion"].as_str().unwrap_or_default()).trim().to_string(),
        "modules": modules,
    })
}

/// The state Home Assistant shows for a light.
fn state(light: Option<&Light>) -> String {
    match light {
        Some(l) => json!({ "state": "ON", "color_mode": "rgb", "brightness": l.brightness, "color": { "r": l.rgb[0], "g": l.rgb[1], "b": l.rgb[2] },
                           "effect": EFFECTS[l.effect as usize] })
        .to_string(),
        None => json!({ "state": "OFF" }).to_string(),
    }
}

/// What a command from Home Assistant asks for.
#[derive(Debug, PartialEq)]
enum Command {
    /// The light afterwards; None turns it off.
    Set(Option<Light>),
    /// Blink this light for a moment, then show what was there before.
    Flash(Light, Duration),
}

/// A command (HA's JSON schema). Missing colour, brightness or effect keep what the light had (white, full
/// brightness, solid at first); anything malformed is ignored.
fn command(payload: &[u8], now: Option<&Light>) -> Option<Command> {
    let v: Value = serde_json::from_slice(payload).ok()?;
    match v["state"].as_str()? {
        "OFF" => Some(Command::Set(None)),
        "ON" => {
            let mut light = now.copied().unwrap_or(Light { rgb: [255, 255, 255], brightness: 255, effect: 0 });
            if let Some(c) = v["color"].as_object() {
                let part = |k: &str| c.get(k).and_then(Value::as_u64).map(|x| x.min(255) as u8);
                light.rgb = [part("r")?, part("g")?, part("b")?];
            }
            if let Some(b) = v["brightness"].as_u64() {
                light.brightness = b.min(255) as u8;
            }
            if let Some(e) = v["effect"].as_str() {
                light.effect = EFFECTS.iter().position(|n| *n == e)? as u8;
            }
            let flash = match v["flash"].as_str() {
                Some("short") => Some(FLASH_SHORT),
                Some("long") => Some(FLASH_LONG),
                _ => None,
            };
            Some(match flash {
                Some((effect, time)) => Command::Flash(Light { effect, ..light }, time),
                None => Command::Set(Some(light)),
            })
        }
        _ => None,
    }
}

/// The rustls configuration: only the CA in `ca_file` when given, else the system's certificates.
fn tls_config(ca_file: Option<&str>) -> Result<Arc<rustls::ClientConfig>, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::CertificateDer;
    let mut roots = rustls::RootCertStore::empty();
    match ca_file {
        Some(file) => {
            for cert in CertificateDer::pem_file_iter(file).map_err(|e| format!("CA file {file}: {e}"))? {
                roots
                    .add(cert.map_err(|e| format!("CA file {file}: {e}"))?)
                    .map_err(|e| format!("CA file {file}: {e}"))?;
            }
        }
        None => {
            roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        }
    }
    if roots.is_empty() {
        return Err("no CA certificate to check the broker with".into());
    }
    Ok(Arc::new(rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth()))
}

fn options(s: &Settings) -> Result<MqttOptions, String> {
    let mut id = [0u8; 4];
    getrandom::getrandom(&mut id).map_err(|e| e.to_string())?;
    let client_id = format!("opendeck-masterhub-{}", id.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let mut o = MqttOptions::new(client_id, s.host.clone(), s.port);
    o.set_keep_alive(Duration::from_secs(30));
    o.set_clean_session(true);
    // room for a large retained message on a topic it reads; a bigger one makes rumqttc drop the connection
    o.set_max_packet_size(1 << 20, 64 << 10);
    o.set_last_will(LastWill::new(format!("{}/status", s.prefix), "offline", QoS::AtLeastOnce, true));
    if !s.username.is_empty() {
        // read here, on the MQTT thread: the keychain may ask the user
        let password = match s.password_store {
            Some(store) => secret::load(store)
                .ok_or("the MQTT password couldn't be read from where it was kept: enter it again")?,
            None => String::new(),
        };
        o.set_credentials(s.username.clone(), password);
    }
    if s.tls {
        o.set_transport(Transport::Tls(TlsConfiguration::Rustls(tls_config(s.ca_file.as_deref())?)));
    }
    Ok(o)
}

/// Keeps the connection while MQTT is on, follows the settings and the modules plugged in, and turns Home
/// Assistant's commands into `Config::mqtt_lights` and `Up::Repaint`.
pub fn start(shared: Shared, up: Sender<Up>) {
    std::thread::spawn(move || {
        let status = |text: String| shared.write().unwrap().mqtt_status = text;
        loop {
            // a password an earlier version kept in OpenDeck's settings: into the keychain, out of the settings
            let legacy = shared.read().unwrap().mqtt.legacy_password.clone();
            if let Some(password) = legacy {
                let store = secret::save(&password);
                let mut cfg = shared.write().unwrap();
                if store.is_some() {
                    cfg.mqtt.password_store = store;
                    cfg.mqtt.password_changes += 1;
                }
                cfg.mqtt.legacy_password = None;
                drop(cfg);
                let _ = up.send(Up::SaveSettings);
            }
            let settings = shared.read().unwrap().mqtt.clone();
            if !settings.enabled || settings.host.is_empty() {
                status(if settings.enabled { "No broker host set.".into() } else { "Off.".into() });
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            let options = match options(&settings) {
                Ok(o) => o,
                Err(e) => {
                    status(format!("Not connected: {e}"));
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            status(format!("Connecting to {}:{}…", settings.host, settings.port));
            session(&shared, &up, &settings, options, status);
            // the session ended (settings changed or MQTT turned off): the LEDs are their own again
            let gone: Vec<String> = {
                let mut cfg = shared.write().unwrap();
                let flashes: Vec<String> = cfg.mqtt_flashes.drain().map(|(k, _)| k).collect();
                cfg.mqtt_lights.drain().map(|(k, _)| k).chain(flashes).collect()
            };
            for led in gone {
                let _ = up.send(Up::Repaint(led));
            }
        }
    });
}

/// One connection with the given settings, until they change.
fn session(shared: &Shared, up: &Sender<Up>, s: &Settings, options: MqttOptions, status: impl Fn(String)) {
    let (client, mut connection) = Client::new(options, 256);
    // rumqttc makes progress (and reconnects) only while its connection is polled: on a thread of its own, which
    // ends when the client is dropped
    let (events_tx, events) = mpsc::channel();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = stop.clone();
    let reader = std::thread::spawn(move || {
        use std::sync::atomic::Ordering;
        for event in connection.iter() {
            let failed = event.is_err();
            if events_tx.send(event).is_err() || stopping.load(Ordering::Relaxed) {
                return;
            }
            if failed {
                // then rumqttc connects again; waiting in small steps, so the session's end isn't held up
                for _ in 0..50 {
                    if stopping.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });
    let topic = |led: &Led, what: &str| format!("{}/{}/{}/{what}", s.prefix, led.module, led.control);
    // false when it didn't fit in rumqttc's queue: what needs it is tried again
    let send = |topic: String, payload: String, retain: bool| match client.try_publish(
        topic,
        QoS::AtLeastOnce,
        retain,
        payload,
    ) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("MQTT: {e}");
            false
        }
    };
    let availability = format!("{}/+/availability", s.prefix);
    let mut announced: Vec<Led> = Vec::new();
    let mut published_software = Value::Null;
    // modules the broker remembers as online from an earlier run (retained availability)
    let mut remembered: BTreeSet<String> = BTreeSet::new();
    let mut connected = false;
    let mut last_check = Instant::now();
    loop {
        match events.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(Event::Incoming(Packet::ConnAck(ack)))) if ack.code == ConnectReturnCode::Success => {
                connected = true;
                announced.clear(); // a new session: announce everything again
                published_software = Value::Null;
                status(format!("Connected to {}:{}{}.", s.host, s.port, if s.tls { " (TLS)" } else { "" }));
                let _ = client.try_subscribe(format!("{}/+/+/set", s.prefix), QoS::AtLeastOnce);
                let _ = client.try_subscribe(format!("{}/status", s.discovery), QoS::AtLeastOnce);
                let _ = client.try_subscribe(availability.clone(), QoS::AtLeastOnce);
                send(format!("{}/status", s.prefix), "online".into(), true);
            }
            Ok(Ok(Event::Incoming(Packet::ConnAck(ack)))) => {
                status(format!("The broker refused the connection: {:?}.", ack.code))
            }
            Ok(Ok(Event::Incoming(Packet::SubAck(ack))))
                if ack.return_codes.iter().any(|c| matches!(c, rumqttc::SubscribeReasonCode::Failure)) =>
            {
                status(format!("Connected to {}:{}, but the broker refused a subscription (its access rules): Home Assistant's commands won't arrive.", s.host, s.port));
            }
            Ok(Ok(Event::Incoming(Packet::Publish(p)))) => 'publish: {
                if p.topic == format!("{}/status", s.discovery) {
                    if &p.payload[..] == b"online" {
                        announced.clear(); // Home Assistant restarted: announce again
                        published_software = Value::Null;
                    }
                    break 'publish;
                }
                if let Some(module) =
                    p.topic.strip_prefix(&format!("{}/", s.prefix)).and_then(|t| t.strip_suffix("/availability"))
                {
                    if &p.payload[..] == b"online" && !module.contains('/') {
                        remembered.insert(module.to_string());
                    }
                    break 'publish;
                }
                let Some(led) = announced.iter().find(|l| p.topic == topic(l, "set")) else { break 'publish };
                let key = led.key();
                let mut cfg = shared.write().unwrap();
                match command(&p.payload, cfg.mqtt_lights.get(&key)) {
                    Some(Command::Set(light)) => {
                        match light {
                            Some(l) => cfg.mqtt_lights.insert(key.clone(), l),
                            None => cfg.mqtt_lights.remove(&key),
                        };
                        cfg.mqtt_flashes.remove(&key);
                        send(topic(led, "state"), state(light.as_ref()), true);
                    }
                    Some(Command::Flash(light, time)) => {
                        let now = Instant::now();
                        cfg.mqtt_flashes.insert(key.clone(), Flash { light, from: now, until: now + time });
                        send(topic(led, "state"), state(cfg.mqtt_lights.get(&key)), true);
                        // unchanged
                    }
                    None => break 'publish,
                }
                let _ = up.send(Up::Repaint(key));
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                connected = false;
                status(format!("Not connected ({e}); trying again…"));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if last_check.elapsed() < Duration::from_secs(1) {
            continue;
        }
        last_check = Instant::now();
        let (now, lights, current, software) = {
            let cfg = shared.read().unwrap();
            (leds(&cfg), cfg.mqtt_lights.clone(), cfg.mqtt.clone(), software(&cfg))
        };
        if current != *s {
            if connected {
                send(format!("{}/status", s.prefix), "offline".into(), true);
            }
            break;
        }
        if !connected {
            continue;
        }
        if software != published_software {
            let (t, config) = plugin_discovery(s);
            if send(t, config, true) && send(format!("{}/plugin/state", s.prefix), software.to_string(), true) {
                published_software = software;
            }
        }
        // modules unplugged meanwhile, or while the plugin wasn't running, aren't available
        let modules = |leds: &[Led]| leds.iter().map(|l| l.module.clone()).collect::<BTreeSet<_>>();
        let present = modules(&now);
        let gone: Vec<String> =
            modules(&announced).union(&remembered).filter(|m| !present.contains(*m)).cloned().collect();
        for module in gone {
            if send(format!("{}/{module}/availability", s.prefix), "offline".into(), true) {
                remembered.remove(&module);
            }
        }
        if now == announced {
            continue;
        }
        // new LEDs (and the device details they carry): announced; tried again next second if it didn't all fit
        let mut all_sent = true;
        for led in now.iter().filter(|l| !announced.contains(l)) {
            let (t, config) = discovery(s, led);
            all_sent &= send(t, config, true);
            all_sent &= send(topic(led, "state"), state(lights.get(&led.key())), true);
        }
        for module in &present {
            all_sent &= send(format!("{}/{module}/availability", s.prefix), "online".into(), true);
        }
        if all_sent {
            announced = now;
        }
    }
    // always a clean goodbye, then wait for rumqttc to finish: a connection it was still making must not end
    // uncleanly later, or the broker would publish the will ("offline") over the next session's "online"
    let _ = client.disconnect();
    drop(client);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(events);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Lights set from Home Assistant, by "<module>/<control>".
pub type Lights = HashMap<String, Light>;

/// A flash going on: the blinking light, from when (its phase) and until when.
#[derive(Clone, Copy, Debug)]
pub struct Flash {
    pub light: Light,
    pub from: Instant,
    pub until: Instant,
}

/// What Home Assistant shows on an LED now, if anything: a flash over the light; for an effect, also since when
/// it runs (its phase).
pub fn shown(cfg: &crate::Config, led: &str, epoch: Instant) -> Option<(Light, Instant)> {
    match cfg.mqtt_flashes.get(led) {
        Some(f) => Some((f.light, f.from)),
        None => cfg.mqtt_lights.get(led).map(|l| (*l, epoch)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn settings_from_the_form() {
        let mut s = Settings::default();
        let stored = Password::Stored(Store::Keychain);
        s.apply_form(
            &form(&[
                ("mqtt_on", "on"),
                ("mqtt_host", " ha.local "),
                ("mqtt_port", "8883"),
                ("mqtt_tls", "on"),
                ("mqtt_user", "deck"),
                ("mqtt_prefix", "desk/hub"),
            ]),
            stored,
        );
        assert_eq!(
            (s.enabled, s.host.as_str(), s.port, s.tls, s.password_store, s.prefix.as_str()),
            (true, "ha.local", 8883, true, Some(Store::Keychain), "desk/hub")
        );
        // the settings say where the password is, never what it is
        let json = s.to_json();
        assert_eq!(json["passwordStore"], "keychain");
        assert!(json.get("password").is_none());
        let back = Settings::from_json(&json);
        assert_eq!((back.password_store, back.host.as_str()), (Some(Store::Keychain), "ha.local"));
        // a new password counts as a change (the session reconnects); none keeps it; cleared forgets where
        let before = s.clone();
        s.apply_form(
            &form(&[
                ("mqtt_on", "on"),
                ("mqtt_host", "ha.local"),
                ("mqtt_port", "8883"),
                ("mqtt_tls", "on"),
                ("mqtt_user", "deck"),
                ("mqtt_prefix", "desk/hub"),
            ]),
            stored,
        );
        assert_ne!(s, before);
        let before = s.clone();
        s.apply_form(
            &form(&[
                ("mqtt_on", "on"),
                ("mqtt_host", "ha.local"),
                ("mqtt_port", "8883"),
                ("mqtt_tls", "on"),
                ("mqtt_user", "deck"),
                ("mqtt_prefix", "desk/hub"),
            ]),
            Password::Keep,
        );
        assert_eq!(s, before);
        s.apply_form(
            &form(&[
                ("mqtt_host", "ha.local"),
                ("mqtt_port", "8883"),
                ("mqtt_prefix", "desk/hub"),
                ("mqtt_discovery", "homeassistant"),
            ]),
            Password::Cleared,
        );
        assert!(s.password_store.is_none() && !s.enabled);
        assert!(s.form_html("").contains("none set"));
        // a password an earlier version kept in the settings is picked up, to be moved
        assert_eq!(Settings::from_json(&json!({ "password": "old" })).legacy_password.as_deref(), Some("old"));
        // wildcards, odd hosts, host:port and markup are refused, with a word why, the old values kept
        let notes = s.apply_form(
            &form(&[
                ("mqtt_host", "x\"><script>"),
                ("mqtt_port", "8883"),
                ("mqtt_prefix", "a/#"),
                ("mqtt_discovery", "+"),
            ]),
            Password::Keep,
        );
        assert_eq!(
            (s.host.as_str(), s.prefix.as_str(), s.discovery.as_str()),
            ("ha.local", "desk/hub", "homeassistant")
        );
        assert_eq!(notes.len(), 3);
        assert_eq!(
            s.apply_form(
                &form(&[
                    ("mqtt_host", "ha.local:1883"),
                    ("mqtt_port", "8883"),
                    ("mqtt_prefix", "desk/hub"),
                    ("mqtt_discovery", "homeassistant")
                ]),
                Password::Keep
            )
            .len(),
            1
        );
        // the topic prefix may not be Home Assistant's own
        let notes = s.apply_form(
            &form(&[
                ("mqtt_host", "ha.local"),
                ("mqtt_port", "8883"),
                ("mqtt_prefix", "homeassistant"),
                ("mqtt_discovery", "homeassistant"),
            ]),
            Password::Keep,
        );
        assert_eq!((notes.len(), s.prefix.as_str()), (1, "desk/hub"));
    }

    #[test]
    fn home_assistant_messages() {
        let s = Settings::default();
        let led = Led {
            module: "mh-0202-MHKB03X".into(),
            control: 2,
            name: "Knob 3".into(),
            device: "MasterHUB 3xKnob".into(),
            firmware: Some("V1.0.6".into()),
            serial: Some("MHKB03X".into()),
        };
        let (topic, config) = discovery(&s, &led);
        assert_eq!(topic, "homeassistant/light/mh-0202-MHKB03X/led2/config");
        let config: Value = serde_json::from_str(&config).unwrap();
        assert_eq!(config["command_topic"], "masterhub/mh-0202-MHKB03X/2/set");
        assert_eq!(config["unique_id"], "mh-0202-MHKB03X_2_led");
        assert_eq!(config["device"]["model"], "MasterHUB 3xKnob");
        // software: the module's firmware and serial on its device, connected through the plugin's device
        assert_eq!(
            (config["device"]["sw_version"].as_str(), config["device"]["serial_number"].as_str()),
            (Some("V1.0.6"), Some("MHKB03X"))
        );
        assert_eq!(config["device"]["via_device"], PLUGIN_DEVICE);
        let (topic, plugin) = plugin_discovery(&s);
        assert_eq!(topic, "homeassistant/sensor/opendeck-masterhub/version/config");
        let plugin: Value = serde_json::from_str(&plugin).unwrap();
        assert_eq!(
            (plugin["entity_category"].as_str(), plugin["device"]["identifiers"][0].as_str()),
            (Some("diagnostic"), Some(PLUGIN_DEVICE))
        );
        let mut cfg = crate::Config::new();
        cfg.host = json!({ "application": { "version": "7.1.0", "platformVersion": "27.0.1" } });
        cfg.modules.push(crate::device_info::Info {
            id: "mh-base".into(),
            name: "MasterHUB Base".into(),
            serial: Some("MHBS01".into()),
            firmware: vec!["V1.0.3".into()],
            ..Default::default()
        });
        let sw = software(&cfg);
        assert_eq!((sw["version"].as_str(), sw["opendeck"].as_str()), (Some(env!("CARGO_PKG_VERSION")), Some("7.1.0")));
        assert_eq!(sw["modules"][0]["firmware"][0], "V1.0.3");

        assert_eq!(config["effect_list"][1], "Pulse");
        assert_eq!(config["flash"], true);
        let set = |payload: &[u8], now: Option<&Light>| match command(payload, now) {
            Some(Command::Set(Some(l))) => l,
            other => panic!("{other:?}"),
        };
        let on = set(br#"{"state":"ON","color":{"r":255,"g":136,"b":0},"brightness":128}"#, None);
        assert_eq!((on.rgb, on.brightness, on.led_brightness(), on.effect), ([255, 136, 0], 128, 50, 0));
        // brightness alone keeps the colour; ON alone starts white; an effect by its name
        assert_eq!(set(br#"{"state":"ON","brightness":255}"#, Some(&on)).rgb, [255, 136, 0]);
        assert_eq!(set(br#"{"state":"ON"}"#, None), Light { rgb: [255, 255, 255], brightness: 255, effect: 0 });
        let pulsing = set(br#"{"state":"ON","effect":"Pulse"}"#, Some(&on));
        assert_eq!((pulsing.effect, pulsing.rgb), (1, [255, 136, 0]));
        assert_eq!(set(br#"{"state":"ON","color":{"r":0,"g":0,"b":255}}"#, Some(&pulsing)).effect, 1); // kept
        assert_eq!(command(br#"{"state":"ON","effect":"Disco"}"#, None), None);
        assert_eq!(command(br#"{"state":"OFF"}"#, Some(&on)), Some(Command::Set(None)));
        assert_eq!(
            command(br#"{"state":"ON","flash":"short","color":{"r":255,"g":0,"b":0}}"#, None),
            Some(Command::Flash(Light { rgb: [255, 0, 0], brightness: 255, effect: 4 }, Duration::from_secs(2)))
        );
        assert_eq!(command(b"rm -rf", None), None);
        assert_eq!(command(br#"{"state":"ON","color":{"r":"x"}}"#, None), None);
        let st: Value = serde_json::from_str(&state(Some(&pulsing))).unwrap();
        assert_eq!(
            (st["state"].as_str(), st["color"]["g"].as_u64(), st["effect"].as_str()),
            (Some("ON"), Some(136), Some("Pulse"))
        );
        assert_eq!(state(None), r#"{"state":"OFF"}"#);
    }

    #[test]
    fn effects_over_time() {
        let at = |effect: u8, ms: u64| {
            frame(&Light { rgb: [10, 20, 30], brightness: 255, effect }, Duration::from_millis(ms))
        };
        assert_eq!(at(0, 1234), ([10, 20, 30], 100));
        // pulse: dim at the start of its 2 s, full in the middle, never quite dark
        assert_eq!(at(1, 0).1, 8);
        assert_eq!(at(1, 1000).1, 100);
        assert!(at(1, 500).1 > 8 && at(1, 500).1 < 100);
        assert_eq!(at(2, 2500).1, 100);
        // blink: on, then off; fast blink four times a second
        assert_eq!((at(3, 100).1, at(3, 600).1), (100, 0));
        assert_eq!((at(4, 100).1, at(4, 200).1, at(4, 300).1), (100, 0, 100));
        // rainbow: around the wheel in 6 s at full brightness
        assert_eq!(at(5, 0), ([255, 0, 0], 100));
        assert_eq!(at(5, 2000).0, [0, 255, 0]);
        assert_eq!(at(5, 4000).0, [0, 0, 255]);
    }

    /// Against a real broker (Mosquitto with a user and TLS, see docs): MQTT_TEST_DIR holds ca.crt and
    /// other-ca.crt, the broker listens on 127.0.0.1:18883 (plain) and 18884 (TLS) in container `mh-mosq`.
    /// `MQTT_TEST_DIR=… cargo test broker_round_trip -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn broker_round_trip() {
        let dir = std::path::PathBuf::from(std::env::var("MQTT_TEST_DIR").expect("MQTT_TEST_DIR"));
        // the password in a user-only file of a throwaway data folder, not the user's keychain
        let data = std::env::temp_dir().join(format!("mh-mqtt-test-{}", std::process::id()));
        std::env::set_var("MASTERHUB_DATA", &data);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("mqtt-password"), "testpass").unwrap();
        let sh = |cmd: &str| {
            String::from_utf8_lossy(&std::process::Command::new("sh").args(["-c", cmd]).output().unwrap().stdout)
                .into_owned()
        };
        let sub =
            |topic: &str| sh(&format!("docker exec mh-mosq mosquitto_sub -u deck -P testpass -t '{topic}' -C 1 -W 5"));
        let run = |settings: Settings| {
            let mut cfg = crate::Config::new();
            cfg.placed.insert("mh-0202-T".into(), (Kind::Knobs, "3xKnob".into(), Some("143".into())));
            cfg.mqtt = settings;
            let shared: Shared = Arc::new(std::sync::RwLock::new(cfg));
            let (up_tx, up_rx) = mpsc::channel();
            start(shared.clone(), up_tx);
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(8)
                && !shared.read().unwrap().mqtt_status.starts_with("Connected")
            {
                std::thread::sleep(Duration::from_millis(100));
            }
            (shared, up_rx)
        };
        let settings = Settings {
            enabled: true,
            host: "localhost".into(),
            port: 18884,
            tls: true,
            ca_file: Some(dir.join("ca.crt").display().to_string()),
            username: "deck".into(),
            password_store: Some(Store::File),
            prefix: "mhtest".into(),
            ..Settings::default()
        };

        // TLS with the right CA: discovery, availability, a command and its state
        let (shared, up_rx) = run(settings.clone());
        println!("status: {}", shared.read().unwrap().mqtt_status);
        assert!(shared.read().unwrap().mqtt_status.starts_with("Connected to localhost:18884 (TLS)"));
        std::thread::sleep(Duration::from_secs(2)); // the LEDs are announced within a second
        let config: Value = serde_json::from_str(&sub("homeassistant/light/mh-0202-T/led0/config")).unwrap();
        assert_eq!(config["command_topic"], "mhtest/mh-0202-T/0/set");
        assert_eq!(sub("mhtest/status").trim(), "online");
        assert_eq!(sub("mhtest/mh-0202-T/availability").trim(), "online");
        // the plugin's sensor: its version, retained
        let software: Value = serde_json::from_str(&sub("mhtest/plugin/state")).unwrap();
        assert_eq!(software["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
        // -R: the next state, not the retained one already there
        let state = std::thread::spawn(move || {
            sh("docker exec mh-mosq mosquitto_sub -u deck -P testpass -t mhtest/mh-0202-T/0/state -R -C 1 -W 5")
        });
        std::thread::sleep(Duration::from_millis(500));
        sh(
            r#"docker exec mh-mosq mosquitto_pub -u deck -P testpass -t mhtest/mh-0202-T/0/set -m '{"state":"ON","color":{"r":255,"g":0,"b":0},"brightness":128}'"#,
        );
        assert!(matches!(up_rx.recv_timeout(Duration::from_secs(5)), Ok(Up::Repaint(led)) if led == "mh-0202-T/0"));
        assert_eq!(
            shared.read().unwrap().mqtt_lights.get("mh-0202-T/0"),
            Some(&Light { rgb: [255, 0, 0], brightness: 128, effect: 0 })
        );
        let state: Value = serde_json::from_str(&state.join().unwrap()).unwrap();
        assert_eq!((state["state"].as_str(), state["brightness"].as_u64()), (Some("ON"), Some(128)));
        // retained: Home Assistant coming up later still reads it
        let kept: Value = serde_json::from_str(&sub("mhtest/mh-0202-T/0/state")).unwrap();
        assert_eq!(kept["state"].as_str(), Some("ON"));
        // an effect, then a flash over it (the light stays as it was)
        sh(
            r#"docker exec mh-mosq mosquitto_pub -u deck -P testpass -t mhtest/mh-0202-T/0/set -m '{"state":"ON","effect":"Pulse"}'"#,
        );
        assert!(up_rx.recv_timeout(Duration::from_secs(5)).is_ok());
        assert_eq!(shared.read().unwrap().mqtt_lights["mh-0202-T/0"].effect, 1);
        sh(
            r#"docker exec mh-mosq mosquitto_pub -u deck -P testpass -t mhtest/mh-0202-T/0/set -m '{"state":"ON","flash":"long"}'"#,
        );
        assert!(up_rx.recv_timeout(Duration::from_secs(5)).is_ok());
        assert_eq!(shared.read().unwrap().mqtt_flashes["mh-0202-T/0"].light.effect, 3);
        assert_eq!(shared.read().unwrap().mqtt_lights["mh-0202-T/0"].effect, 1);
        // a command for an LED that doesn't exist is ignored
        sh(r#"docker exec mh-mosq mosquitto_pub -u deck -P testpass -t mhtest/mh-0202-T/7/set -m '{"state":"ON"}'"#);
        assert!(up_rx.recv_timeout(Duration::from_secs(1)).is_err());
        // turned off in the settings: says offline, gives the LED back
        shared.write().unwrap().mqtt.enabled = false;
        assert!(matches!(up_rx.recv_timeout(Duration::from_secs(5)), Ok(Up::Repaint(_))));
        assert!(shared.read().unwrap().mqtt_lights.is_empty());
        assert_eq!(sub("mhtest/status").trim(), "offline");

        // a broker whose certificate another CA signed is refused
        let (shared, _) = run(Settings {
            ca_file: Some(dir.join("other-ca.crt").display().to_string()),
            prefix: "mhtest2".into(),
            ..settings.clone()
        });
        let status = shared.read().unwrap().mqtt_status.clone();
        println!("other CA: {status}");
        assert!(!status.starts_with("Connected") && status.contains("ertificate"), "{status}");
        shared.write().unwrap().mqtt.enabled = false;

        // a wrong password is refused by the broker
        std::fs::write(data.join("mqtt-password"), "nope").unwrap();
        let (shared, _) = run(Settings { prefix: "mhtest3".into(), ..settings });
        std::thread::sleep(Duration::from_secs(1));
        let status = shared.read().unwrap().mqtt_status.clone();
        println!("wrong password: {status}");
        assert!(!status.starts_with("Connected"), "{status}");
        shared.write().unwrap().mqtt.enabled = false;
    }

    #[test]
    fn tls_trusts_only_the_given_ca() {
        assert!(tls_config(None).is_ok()); // the system's certificates
        assert!(tls_config(Some("/nonexistent/ca.pem")).is_err());
        let empty = std::env::temp_dir().join(format!("mh-empty-ca-{}.pem", std::process::id()));
        std::fs::write(&empty, "not a certificate").unwrap();
        assert!(tls_config(Some(empty.to_str().unwrap())).is_err());
        std::fs::remove_file(empty).unwrap();
    }
}
