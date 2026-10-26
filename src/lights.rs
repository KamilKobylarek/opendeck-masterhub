//! RGB LEDs of the knob, fader and base modules.
//!
//! A control's colour, most specific first: the colour picked for the action's current image (each
//! state of an action has its own image, so this is "a colour per state", e.g. muted = red), the
//! colour picked for the control, the dominant colour of the action's image, off.
//! The modes are the firmware's own effects, as in the official app (docs/masterhub-app-analysis.md).

use std::collections::BTreeMap;

use image::DynamicImage;
use serde_json::{json, Value};

pub const MODES: [&str; 10] =
    ["Off", "Static", "Breathing", "Strobe", "Spring", "Cycling", "Random", "Wave", "Water", "Rainbow"];

pub type Rgb = [u8; 3];

#[derive(Clone, Debug, PartialEq)]
pub struct Lights {
    /// Off until the user turns it on: the modules keep the lighting their firmware has.
    pub managed: bool,
    pub mode: u8,
    pub speed: u8,
    /// None leaves the base LEDs as they are.
    pub base: Option<Rgb>,
    /// "<device id>/<position>" -> colour
    pub controls: BTreeMap<String, Rgb>,
    /// image hash -> colour
    pub icons: BTreeMap<String, Rgb>,
    /// Each fader's LED brightness follows the fader's position, like a level meter.
    pub faders_level: bool,
    /// Knob and fader LEDs show App volume's level (brightness) and mute (red).
    pub levels: bool,
    /// Signal LEDs, by "<device id>/<control>": the command whose result lights the LED.
    pub signals: BTreeMap<String, String>,
    /// Seconds between two runs of a signal command.
    pub signal_every: u32,
}

impl Default for Lights {
    fn default() -> Self {
        Lights {
            managed: false,
            mode: 1,
            speed: 50,
            base: None,
            controls: BTreeMap::new(),
            icons: BTreeMap::new(),
            faders_level: false,
            levels: true,
            signals: BTreeMap::new(),
            signal_every: 5,
        }
    }
}

/// A knob or fader as the settings page shows it, with what OpenDeck last drew for it.
#[derive(Clone, Debug, Default)]
pub struct Control {
    pub key: String, // "<device id>/<position>"
    pub label: String,
    pub image: Option<String>, // data URI from OpenDeck
    pub hash: Option<String>,
    pub auto: Option<Rgb>,
    /// App volume's level (0–100) and mute, when that action sits here.
    pub level: Option<(u8, bool)>,
}

/// The colour of a muted App volume.
pub const MUTED: Rgb = [255, 0, 0];

/// A fader showing its position with no colour of its own (no action, nothing picked): black wouldn't show it.
pub const LEVEL_METER: Rgb = [255, 255, 255];

impl Lights {
    pub fn from_json(v: &Value) -> Self {
        let map = |v: &Value| -> BTreeMap<String, Rgb> {
            v.as_object()
                .map(|m| m.iter().filter_map(|(k, c)| Some((k.clone(), parse_hex(c.as_str()?)?))).collect())
                .unwrap_or_default()
        };
        let d = Lights::default();
        Lights {
            managed: v["managed"].as_bool().unwrap_or(false),
            mode: v["mode"].as_u64().filter(|m| *m < MODES.len() as u64).map_or(d.mode, |m| m as u8),
            speed: v["speed"].as_u64().map_or(d.speed, |s| s.min(100) as u8),
            base: v["base"].as_str().and_then(parse_hex),
            controls: map(&v["controls"]),
            icons: map(&v["icons"]),
            faders_level: v["fadersLevel"].as_bool().unwrap_or(false),
            levels: v["levels"].as_bool().unwrap_or(d.levels),
            signals: v["signals"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, c)| {
                            Some((k.clone(), c.as_str().filter(|c| !c.trim().is_empty())?.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            signal_every: v["signalEvery"].as_u64().map_or(d.signal_every, |s| s.clamp(1, 3600) as u32),
        }
    }

    pub fn to_json(&self) -> Value {
        let map = |m: &BTreeMap<String, Rgb>| {
            m.iter().map(|(k, c)| (k.clone(), json!(to_hex(*c)))).collect::<serde_json::Map<_, _>>()
        };
        json!({ "managed": self.managed, "mode": self.mode, "speed": self.speed, "base": self.base.map(to_hex), "controls": map(&self.controls), "icons": map(&self.icons), "fadersLevel": self.faders_level, "levels": self.levels, "signals": self.signals, "signalEvery": self.signal_every })
    }

    pub fn colour(&self, c: &Control) -> Rgb {
        c.hash
            .as_ref()
            .and_then(|h| self.icons.get(h))
            .or_else(|| self.controls.get(&c.key))
            .copied()
            .or(c.auto)
            .unwrap_or([0, 0, 0])
    }

    /// What one knob or fader LED shows, most important first: its signal, App volume's mute (red) and level
    /// (brightness), the control's colour. Returns the colour and the brightness to send: OpenDeck's brightness,
    /// scaled by the level; a fader showing its position, that position (`position`, when known; None leaves it).
    pub fn led(
        &self,
        c: Option<&Control>,
        signal: Option<Rgb>,
        brightness: u8,
        fader: bool,
        position: Option<u8>,
    ) -> (Rgb, Option<u8>) {
        if let Some(rgb) = signal {
            return (rgb, Some(brightness));
        }
        let colour = c.map_or([0, 0, 0], |c| self.colour(c));
        match c.and_then(|c| c.level).filter(|_| self.levels) {
            Some((_, true)) => (MUTED, Some(brightness)),
            // never quite dark: the knob still shows where it is
            Some((level, false)) => (colour, Some((level.min(100) as u16 * brightness as u16 / 100).max(3) as u8)),
            None if fader && self.faders_level => (if colour == [0, 0, 0] { LEVEL_METER } else { colour }, position),
            None => (colour, Some(brightness)),
        }
    }

    /// Applies the lighting part of the settings form (checkboxes are only sent when ticked). `signal_leds` are the
    /// LEDs listed for signals: "<device id>/<control>" and a label.
    pub fn apply_form(&mut self, controls: &[Control], signal_leds: &[(String, String)], params: &[(String, String)]) {
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        self.managed = get("lights_on").is_some();
        self.faders_level = get("faders_level").is_some();
        self.levels = get("levels").is_some();
        if let Some(s) = get("signal_every").and_then(|s| s.parse::<u32>().ok()) {
            self.signal_every = s.clamp(1, 3600);
        }
        // only what was on the page (`ls:`/`cs:` markers): a module that appeared after it was loaded keeps its
        // settings, rather than losing them as if they had been unticked
        for (key, _) in signal_leds.iter().filter(|(key, _)| get(&format!("ls:{key}")).is_some()) {
            let command = get(&format!("sc:{key}")).unwrap_or_default().trim().to_string();
            match get(&format!("sm:{key}")) {
                Some("cmd") if !command.is_empty() => self.signals.insert(key.clone(), command),
                _ => self.signals.remove(key),
            };
        }
        if let Some(m) = get("mode").and_then(|m| m.parse::<u8>().ok()).filter(|m| (*m as usize) < MODES.len()) {
            self.mode = m;
        }
        if let Some(s) = get("speed").and_then(|s| s.parse::<u8>().ok()) {
            self.speed = s.min(100);
        }
        self.base = get("base_on").and(get("base")).and_then(parse_hex);
        for c in controls.iter().filter(|c| get(&format!("cs:{}", c.key)).is_some()) {
            match get(&format!("c_on:{}", c.key)).and(get(&format!("c:{}", c.key))).and_then(parse_hex) {
                Some(rgb) => self.controls.insert(c.key.clone(), rgb),
                None => self.controls.remove(&c.key),
            };
            if let Some(h) = &c.hash {
                match get(&format!("i_on:{h}")).and(get(&format!("i:{h}"))).and_then(parse_hex) {
                    Some(rgb) => self.icons.insert(h.clone(), rgb),
                    None => self.icons.remove(h),
                };
            }
        }
    }

    /// The lighting part of the settings form. `detected` is the effect read from a module, shown
    /// while the plugin doesn't manage the lighting.
    pub fn form_html(&self, controls: &[Control], detected: Option<u8>, signal_leds: &[(String, String)]) -> String {
        let mode = if self.managed {
            self.mode
        } else {
            detected.filter(|m| (*m as usize) < MODES.len()).unwrap_or(self.mode)
        };
        let modes: String = MODES
            .iter()
            .enumerate()
            .map(|(i, m)| {
                format!("<option value={i}{}>{m}</option>", if i == mode as usize { " selected" } else { "" })
            })
            .collect();
        let picker = |on: &str, name: &str, value: Option<Rgb>, fallback: Rgb| {
            format!(
                "<input type=checkbox name=\"{}\"{}><input type=color name=\"{}\" value=\"{}\">",
                esc(on),
                if value.is_some() { " checked" } else { "" },
                esc(name),
                to_hex(value.unwrap_or(fallback))
            )
        };
        let rows: String = controls
            .iter()
            .map(|c| {
                let shown = self.colour(c);
                let icon = c.image.as_ref().map_or(String::new(), |src| format!("<img src=\"{}\" alt=\"\">", esc(src)));
                let state = c.hash.as_ref().map_or("<td>—</td>".to_string(), |h| {
                    format!("<td>{}</td>", picker(&format!("i_on:{h}"), &format!("i:{h}"), self.icons.get(h).copied(), shown))
                });
                format!(
                    "<tr><td>{}<input type=hidden name=\"cs:{}\" value=1></td><td>{icon}</td><td><span class=sw style=\"background:{}\"></span></td><td>{}</td>{state}</tr>",
                    esc(&c.label),
                    esc(&c.key),
                    to_hex(shown),
                    picker(&format!("c_on:{}", c.key), &format!("c:{}", c.key), self.controls.get(&c.key).copied(), shown)
                )
            })
            .collect();
        let signal_rows: String = signal_leds
            .iter()
            .map(|(key, label)| {
                let now = self.signals.get(key);
                let option = |value: &str, name: &str, on: bool| format!("<option value={value}{}>{name}</option>", if on { " selected" } else { "" });
                format!(
                    "<tr><td>{}<input type=hidden name=\"ls:{}\" value=1></td><td><select name=\"sm:{}\">{}{}</select></td><td><input name=\"sc:{}\" value=\"{}\" placeholder=\"curl -sf http://192.168.1.10\"></td></tr>",
                    esc(label),
                    esc(key),
                    esc(key),
                    option("", "Its function", now.is_none()),
                    option("cmd", "Command", now.is_some()),
                    esc(key),
                    esc(now.map_or("", String::as_str))
                )
            })
            .collect();
        let signals = if signal_leds.is_empty() {
            String::new()
        } else {
            format!(
                "<h3>Signal LEDs</h3>\
                 <p>An LED can signal something unrelated to its knob or fader, which keeps working as before. \
                 <b>Command</b>: runs every few seconds; it prints a colour (<code>#ff8800</code>, <code>red</code>, <code>off</code>…) \
                 or else lights green when it succeeds and red when it fails; one running over 10 s is stopped. \
                 Other programs and Home Assistant set LEDs over MQTT (below).</p>\
                 <label>Run signal commands every (seconds)<input name=signal_every type=number min=1 max=3600 value={}></label>\
                 <div class=scroll><table><tr><th>LED</th><th>Shows</th><th>Command</th></tr>{signal_rows}</table></div>",
                self.signal_every
            )
        };
        let table = if controls.is_empty() {
            "<p>No knob or fader module connected.</p>".to_string()
        } else {
            format!(
                "<table><tr><th>Control</th><th>Image</th><th>Now</th><th>Control colour</th><th>Colour for this image (state)</th></tr>{rows}</table>"
            )
        };
        format!(
            "<h2>Lighting</h2><label>Manage the LEDs from the plugin<input type=checkbox name=lights_on{}></label>\
             <p>Off: the knob, fader and base LEDs keep what their firmware shows (the effect below is read from a module).</p>\
             <label>Effect<select name=mode>{modes}</select></label>\
             <label>Speed (0–100)<input name=speed type=number min=0 max=100 value={}></label>\
             <label>Base colour {}</label>\
             <label>Fader LEDs show the fader's position (brightness, like a level meter)<input type=checkbox name=faders_level{}></label>\
             <label>Knob and fader LEDs show App volume: brightness = its level, red when muted<input type=checkbox name=levels{}></label>\
             <p>Unticked colours fall back to the next rule: image (state) colour → control colour → \
             the action image's own colour → off. Colours apply in the Static, Breathing, Strobe and Spring effects.</p>{table}\
             <p><button name=store value=1>Save and store in the modules</button> Also writes the effect and colours to the \
             modules' memory, so they stay when OpenDeck isn't running. Brightness and speed can't be stored. \
             Only when needed: the memory wears with every write.</p>{signals}",
            if self.managed { " checked" } else { "" },
            self.speed,
            picker("base_on", "base", self.base, [255, 255, 255]),
            if self.faders_level { " checked" } else { "" },
            if self.levels { " checked" } else { "" }
        )
    }
}

/// The "LED lighting" action: what one press or turn does to the effect. `how` is "toggle" (off ↔ `effect`),
/// "next" (cycle the effects 1–9, a turn by its ticks) or "set" (`effect`).
pub fn lighting_action(l: &mut Lights, how: &str, effect: u8, ticks: i64) {
    let effect = if (1..MODES.len() as u8).contains(&effect) { effect } else { 1 };
    l.mode = match how {
        "next" => {
            let n = MODES.len() as i64 - 1; // effects 1..=9, Off left out
            let step = if ticks == 0 { 1 } else { ticks };
            ((l.mode.max(1) as i64 - 1 + step).rem_euclid(n) + 1) as u8
        }
        "set" => effect,
        _ if l.mode == 0 => effect,
        _ => 0,
    };
}

/// Dominant colour of an action image, brightened to full scale, so a red "muted" icon gives a red
/// LED and a white icon a white one. None for an (almost) black image.
pub fn image_colour(img: &DynamicImage) -> Option<Rgb> {
    let (mut sum, mut weight) = ([0f64; 3], 0f64);
    for p in img.thumbnail(32, 32).to_rgb8().pixels() {
        let [r, g, b] = p.0;
        let (max, min) = (r.max(g).max(b) as f64, r.min(g).min(b) as f64);
        if max < 48.0 {
            continue; // background
        }
        let w = (max - min + 8.0) * max; // colourful and bright pixels count most
        for (s, c) in sum.iter_mut().zip([r, g, b]) {
            *s += c as f64 * w;
        }
        weight += w;
    }
    if weight == 0.0 {
        return None;
    }
    let avg = sum.map(|s| s / weight);
    let peak = avg.iter().cloned().fold(1.0, f64::max);
    Some(avg.map(|c| (c * 255.0 / peak).round() as u8))
}

/// Stable short hash (FNV-1a) to recognise the same image again, also after a rebuild.
pub fn hash(s: &str) -> String {
    let h = s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{h:016x}")
}

/// A colour as a signal gives it: `#ff8800`, a few names, or `off`.
pub fn parse_colour(s: &str) -> Option<Rgb> {
    let s = s.trim().to_ascii_lowercase();
    let named = match s.as_str() {
        "off" | "black" => [0, 0, 0],
        "red" => [255, 0, 0],
        "green" => [0, 255, 0],
        "blue" => [0, 64, 255],
        "yellow" => [255, 200, 0],
        "orange" => [255, 80, 0],
        "purple" => [160, 0, 255],
        "cyan" => [0, 255, 255],
        "white" => [255, 255, 255],
        // only with its #: output like "123456" (a count) or "decade" is a word, not a colour
        _ => return parse_hex(&s),
    };
    Some(named)
}

/// A signal command's result: the colour it prints first, else green when it succeeded and red when it failed.
pub fn signal_colour(ok: bool, output: &str) -> Rgb {
    output.split_whitespace().next().and_then(parse_colour).unwrap_or(if ok { [0, 255, 0] } else { [255, 0, 0] })
}

pub fn parse_hex(s: &str) -> Option<Rgb> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

pub fn to_hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

/// Decodes one application/x-www-form-urlencoded component.
pub fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => match s
                .get(i + 1..i + 3)
                .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                Some(v) => {
                    out.push(v);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lighting_action_steps() {
        let mut l = Lights { mode: 2, ..Default::default() };
        lighting_action(&mut l, "toggle", 5, 0);
        assert_eq!(l.mode, 0);
        lighting_action(&mut l, "toggle", 5, 0);
        assert_eq!(l.mode, 5);
        lighting_action(&mut l, "next", 0, 0);
        assert_eq!(l.mode, 6);
        lighting_action(&mut l, "next", 0, 4); // 6 + 4 wraps past 9 to 1
        assert_eq!(l.mode, 1);
        lighting_action(&mut l, "next", 0, -1);
        assert_eq!(l.mode, 9);
        lighting_action(&mut l, "set", 3, 0);
        assert_eq!(l.mode, 3);
        lighting_action(&mut l, "set", 0, 0); // Off isn't an effect to set: static
        assert_eq!(l.mode, 1);
    }
    use image::{Rgb as Px, RgbImage};

    #[test]
    fn picks_image_colour() {
        let mut img = RgbImage::new(72, 72); // black background
        for x in 20..50 {
            for y in 20..50 {
                img.put_pixel(x, y, Px([200, 20, 20]));
            }
        }
        let c = image_colour(&img.into()).unwrap();
        assert_eq!(c[0], 255);
        assert!(c[1] < 40 && c[2] < 40);
        assert_eq!(image_colour(&RgbImage::from_pixel(8, 8, Px([230, 230, 230])).into()), Some([255, 255, 255]));
        assert_eq!(image_colour(&RgbImage::new(8, 8).into()), None);
    }

    #[test]
    fn colour_rules_and_form() {
        let c = Control {
            key: "mh-0202-A/1".into(),
            hash: Some("h1".into()),
            auto: Some([0, 0, 255]),
            ..Default::default()
        };
        let mut l = Lights::default();
        assert_eq!(l.colour(&c), [0, 0, 255]); // image colour
        let form =
            |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        l.apply_form(
            std::slice::from_ref(&c),
            &[],
            &form(&[
                ("lights_on", "on"),
                ("mode", "2"),
                ("cs:mh-0202-A/1", "1"),
                ("c_on:mh-0202-A/1", "on"),
                ("c:mh-0202-A/1", "#00ff00"),
                ("i:h1", "#ff0000"),
            ]),
        );
        assert_eq!((l.managed, l.mode, l.colour(&c)), (true, 2, [0, 255, 0])); // control colour wins; image colour unticked
        l.apply_form(
            std::slice::from_ref(&c),
            &[],
            &form(&[("cs:mh-0202-A/1", "1"), ("c:mh-0202-A/1", "#00ff00"), ("i_on:h1", "on"), ("i:h1", "#ff0000")]),
        );
        // a control that wasn't on the page keeps its colours
        l.apply_form(std::slice::from_ref(&c), &[], &form(&[]));
        assert_eq!(l.colour(&c), [255, 0, 0]);
        assert_eq!(l.colour(&c), [255, 0, 0]); // state colour wins; control colour unticked
        assert_eq!(Lights::from_json(&l.to_json()), l);
        assert!(l.form_html(&[c], None, &[]).contains("i_on:h1"));
        // not managed: the form shows the effect read from the device
        assert!(Lights::default().form_html(&[], Some(9), &[]).contains("<option value=9 selected>Rainbow"));
    }

    #[test]
    fn signals_and_levels() {
        let form =
            |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        let leds = [
            ("K/0".to_string(), "knob 1".to_string()),
            ("K/1".to_string(), "knob 2".to_string()),
            ("K/2".to_string(), "knob 3".to_string()),
        ];
        let mut l = Lights::default();
        l.signals.insert("other/0".into(), "x".into()); // a module not connected now: kept
        l.signals.insert("K/1".into(), "old".into());
        l.apply_form(
            &[],
            &leds,
            &form(&[
                ("levels", "on"),
                ("signal_every", "9"),
                ("ls:K/0", "1"),
                ("ls:K/1", "1"),
                ("ls:K/2", "1"),
                ("sm:K/0", "cmd"),
                ("sc:K/0", " vpn-up "),
                ("sm:K/1", ""),
                ("sm:K/2", "cmd"),
            ]),
        );
        assert_eq!(l.signals.get("K/0").map(String::as_str), Some("vpn-up"));
        assert!(!l.signals.contains_key("K/1")); // back to its function
        assert!(!l.signals.contains_key("K/2")); // a command without a command line
        assert!(l.signals.contains_key("other/0"));
        assert_eq!((l.levels, l.signal_every), (true, 9));
        assert_eq!(Lights::from_json(&l.to_json()), l);
        assert!(l.form_html(&[], None, &leds).contains("<option value=cmd selected>"));

        // what an LED shows: signal, mute, level, colour
        let mut c = Control { auto: Some([0, 0, 255]), ..Default::default() };
        assert_eq!(l.led(Some(&c), None, 80, false, None), ([0, 0, 255], Some(80)));
        c.level = Some((50, false));
        assert_eq!(l.led(Some(&c), None, 80, false, None), ([0, 0, 255], Some(40)));
        c.level = Some((0, false));
        assert_eq!(l.led(Some(&c), None, 80, false, None).1, Some(3)); // never quite dark
        c.level = Some((50, true));
        assert_eq!(l.led(Some(&c), None, 80, false, None), (MUTED, Some(80)));
        assert_eq!(l.led(Some(&c), Some([1, 2, 3]), 80, false, None), ([1, 2, 3], Some(80)));
        l.levels = false;
        assert_eq!(l.led(Some(&c), None, 80, false, None), ([0, 0, 255], Some(80)));
        l.faders_level = true;
        assert_eq!(l.led(Some(&c), None, 80, true, None).1, None); // the fader's position sets it
        c.level = None;
        assert_eq!(l.led(Some(&c), None, 80, true, Some(35)).1, Some(35)); // back to it, e.g. after Home Assistant
                                                                           // a fader without an action still shows its position
        assert_eq!(l.led(None, None, 80, true, Some(35)), (LEVEL_METER, Some(35)));
    }

    #[test]
    fn signal_results() {
        assert_eq!(signal_colour(true, ""), [0, 255, 0]);
        assert_eq!(signal_colour(false, "error: down\n"), [255, 0, 0]);
        assert_eq!(signal_colour(false, "#ff8800\n"), [255, 136, 0]);
        assert_eq!(signal_colour(true, "Orange"), [255, 80, 0]);
        assert_eq!(signal_colour(true, "off"), [0, 0, 0]);
        assert_eq!(parse_colour("#00ff00"), Some([0, 255, 0]));
        assert_eq!(parse_colour("123456"), None);
        assert_eq!(signal_colour(true, "123456 files"), [0, 255, 0]); // a number, so success: green
        assert_eq!(parse_colour("none"), None);
    }

    #[test]
    fn decodes_form_values() {
        assert_eq!(url_decode("c%3Amh-0202-A%2F1"), "c:mh-0202-A/1");
        assert_eq!(url_decode("%23ff0000"), "#ff0000");
        assert_eq!(url_decode("a+b%2"), "a b%2");
        assert_eq!(url_decode("%+1"), "% 1"); // a sign is no hex digit: no byte 0x01, the + a space as usual
        assert_eq!(parse_hex("#0a0B0c"), Some([10, 11, 12]));
        assert_eq!(parse_hex("red"), None);
        assert_eq!(parse_hex("#+12345"), None);
    }
}
