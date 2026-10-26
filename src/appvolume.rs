//! "App volume": the volume of chosen apps (audio.rs per system). A MasterHUB fader sets it to its own position,
//! knobs, rollers and the dial step it, a press toggles mute. The action's title shows the level; on MasterHUB
//! the dial's screen and the knob and fader LEDs show it too (macOS reads it from FineTune's saved settings).
//!
//! A roller or knob sends dozens of clicks a second and each system call takes a while, so changes wait in
//! `PENDING`, where clicks add up, and one runner job applies whatever piled up. Where the system can't step by
//! any amount (FineTune: 5% per URL) the step becomes one absolute set from the level last known.

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::audio::{self, Op};
use crate::Up;

pub const UUID: &str = "com.masterhub.opendeck.appvolume";

/// Most apps one action drives.
const MAX_APPS: usize = 10;

/// The apps in the action's settings: bundle ids on macOS ("com.spotify.client"), process names elsewhere
/// ("spotify", "Discord.exe"). Anything but a plain name is dropped, so settings can't add URL parameters
/// (macOS) or odd strings to the system tools.
pub fn apps(settings: &Value) -> Vec<String> {
    // a bundle id always has a dot: keeps words of a pasted `osascript -e 'id of app "…"'` out
    let any_name = !cfg!(target_os = "macos");
    let valid = |id: &&str| {
        !id.is_empty()
            && id.len() <= 155
            && (any_name || id.contains('.'))
            && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    let list = settings["apps"].as_str().unwrap_or_default();
    list.split([',', ' ', '\n']).map(str::trim).filter(valid).take(MAX_APPS).map(str::to_string).collect()
}

/// The operation for one OpenDeck event: `fader` is the fader's own position when the action sits on one.
fn op(event: &str, ticks: i64, fader: Option<u8>, step_percent: f32) -> Option<Op> {
    match (event, fader) {
        ("dialRotate", Some(v)) => Some(Op::Set(v.min(100) as f32 / 100.0)),
        ("dialRotate", None) if ticks != 0 => Some(Op::Step(ticks.clamp(-20, 20) as f32 * step_percent / 100.0)),
        ("keyDown" | "dialDown", _) => Some(Op::ToggleMute),
        _ => None,
    }
}

/// A change waiting for the runner: clicks add up, a fader's position replaces the last one.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Pending {
    Set(f32),
    Step(f32),
}

impl Pending {
    fn merge(self, next: Pending) -> Pending {
        match (self, next) {
            (Pending::Step(a), Pending::Step(b)) => Pending::Step(a + b),
            (_, n) => n,
        }
    }
}

/// Changes waiting per action (context), and the level last known per action (level, muted, when).
static PENDING: Mutex<Option<HashMap<String, Pending>>> = Mutex::new(None);
static KNOWN: Mutex<Option<HashMap<String, (audio::State, Instant)>>> = Mutex::new(None);

fn known(key: &str) -> Option<audio::State> {
    KNOWN.lock().unwrap().as_ref()?.get(key).filter(|(_, at)| at.elapsed() < Duration::from_secs(3)).map(|(s, _)| *s)
}

fn remember(key: &str, state: audio::State) {
    KNOWN.lock().unwrap().get_or_insert_with(HashMap::new).insert(key.to_string(), (state, Instant::now()));
}

/// macOS: reads FineTune's saved level 0.7 s after the last change of an action (FineTune saves 0.5 s after it)
/// on a thread of its own, so a turn never waits for it; a newer change postpones the read.
type Readback = (String, Vec<String>, Value, Option<(String, u8)>, Sender<Up>);
fn read_back_later(job: Readback) {
    static QUEUE: OnceLock<Mutex<Sender<Readback>>> = OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Readback>();
        std::thread::spawn(move || {
            let mut due: HashMap<String, (Instant, Readback)> = HashMap::new();
            loop {
                let wait = due
                    .values()
                    .map(|(t, _)| t.saturating_duration_since(Instant::now()))
                    .min()
                    .unwrap_or(Duration::from_secs(3600));
                match rx.recv_timeout(wait) {
                    Ok(job) => {
                        due.insert(job.0.clone(), (Instant::now() + Duration::from_millis(700), job));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let now = Instant::now();
                        let ready: Vec<String> =
                            due.iter().filter(|(_, (t, _))| *t <= now).map(|(k, _)| k.clone()).collect();
                        for k in ready {
                            let (_, (key, apps, context, dial, up)) = due.remove(&k).unwrap();
                            if let Some(state) = audio::read(&apps) {
                                remember(&key, state);
                                publish(&up, &context, dial, state);
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        Mutex::new(tx)
    });
    let _ = queue.lock().unwrap().send(job);
}

/// Applies `op` and shows the result: straight away where the system answers, else the level just set (known
/// from the operation) at once and FineTune's saved level shortly after.
fn apply(key: &str, apps: Vec<String>, op: Op, context: Value, at: Option<(String, u8)>, up: Sender<Up>) {
    match audio::run(&apps, op) {
        Some(state) => {
            remember(key, state);
            publish(&up, &context, at.clone(), state);
        }
        None => {
            if let Op::Set(level) = op {
                let state = audio::State { level, muted: known(key).is_some_and(|s| s.muted) };
                remember(key, state);
                publish(&up, &context, at.clone(), state);
            }
            read_back_later((key.to_string(), apps, context, at, up));
        }
    }
}

/// Shows a state: the action's title (any device) and, for its place on MasterHUB, `Up::Level` (the dial's
/// screen, a knob's or fader's LED).
fn publish(up: &Sender<Up>, context: &Value, at: Option<(String, u8)>, state: audio::State) {
    let percent = (state.level * 100.0).round().clamp(0.0, 100.0) as u8;
    let title = if state.muted { "MUTE".to_string() } else { format!("{percent}%") };
    let _ = up.send(Up::Send(json!({ "event": "setTitle", "context": context, "payload": { "title": title } })));
    if let Some((device, position)) = at {
        let _ = up.send(Up::Level(device, position, percent, state.muted));
    }
}

/// Handles an event for the action. `fader` gives a MasterHUB fader's last position by OpenDeck device id and
/// position. Returns messages for OpenDeck; the work itself runs on the runner thread, which reports the level
/// back through `up`.
pub fn handle(msg: &Value, fader: impl Fn(&str, u8) -> Option<u8>, up: &Sender<Up>) -> Vec<Value> {
    let alert = || vec![json!({ "event": "showAlert", "context": msg["context"] })];
    let event = msg["event"].as_str().unwrap_or_default();
    let settings = &msg["payload"]["settings"];
    let device = msg["device"].as_str().unwrap_or_default().to_string();
    let position = msg["payload"]["coordinates"]["column"].as_u64().unwrap_or(0) as u8;
    let value = fader(&device, position);
    // the level shows at its place on MasterHUB (dial screen, knob/fader LED) only for an encoder: on the dial's key
    // (a keypad position 0) it would draw over the dial's screen
    let at = (msg["payload"]["controller"] == "Encoder").then_some((device, position));
    if event == "willAppear" {
        // show the level straight away
        let (apps, context, up) = (apps(settings), msg["context"].clone(), up.clone());
        let key = context.to_string();
        crate::runner::submit(None, move || {
            if let Some(state) = audio::read(&apps) {
                remember(&key, state);
                publish(&up, &context, at, state);
            }
        });
        return Vec::new();
    }
    let step = settings["step"]
        .as_f64()
        .or_else(|| settings["step"].as_str()?.parse().ok())
        .map_or(5.0, |s| s.clamp(1.0, 25.0)) as f32;
    let Some(op) = op(event, msg["payload"]["ticks"].as_i64().unwrap_or(0), value, step) else { return Vec::new() };
    let apps = apps(settings);
    if apps.is_empty() {
        return alert();
    }
    let context = msg["context"].clone();
    let up = up.clone();
    let key = context.to_string();
    let change = match op {
        Op::ToggleMute => {
            // every press counts
            crate::runner::submit(None, move || apply(&key, apps, Op::ToggleMute, context, at, up));
            return Vec::new();
        }
        Op::Set(l) => Pending::Set(l),
        Op::Step(d) => Pending::Step(d),
    };
    {
        let mut pending = PENDING.lock().unwrap();
        let pending = pending.get_or_insert_with(HashMap::new);
        let merged = pending.get(&key).map_or(change, |p| p.merge(change));
        pending.insert(key.clone(), merged);
    }
    // one job per action in the queue applies whatever piled up by the time it runs
    crate::runner::submit(Some(format!("volume/{key}")), move || {
        let Some(change) = PENDING.lock().unwrap().as_mut().and_then(|p| p.remove(&key)) else { return };
        let op = match change {
            Pending::Set(l) => Op::Set(l),
            // FineTune can only step 5% per URL: set the level from the one last known instead
            Pending::Step(d) if !audio::STEPS_ANY => match known(&key).or_else(|| audio::read(&apps)) {
                Some(state) => Op::Set((state.level + d).clamp(0.0, 1.0)),
                None => Op::Step(d),
            },
            Pending::Step(d) => Op::Step(d),
        };
        apply(&key, apps, op, context, at, up);
    });
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apps_are_checked() {
        let s = json!({ "apps": "com.spotify.client, Discord.exe\nbad&volume=400 x?y=1 my_app" });
        if cfg!(target_os = "macos") {
            assert_eq!(apps(&s), ["com.spotify.client", "Discord.exe"]); // bundle ids have a dot
            assert_eq!(apps(&json!({ "apps": "osascript -e 'id of app \"Spotify\"'" })), Vec::<String>::new());
        } else {
            assert_eq!(apps(&s), ["com.spotify.client", "Discord.exe", "my_app"]);
        }
        assert!(apps(&json!({})).is_empty());
    }

    #[test]
    fn clicks_add_up() {
        assert_eq!(Pending::Step(0.25).merge(Pending::Step(0.5)), Pending::Step(0.75));
        assert_eq!(Pending::Step(0.05).merge(Pending::Set(0.4)), Pending::Set(0.4));
        assert_eq!(Pending::Set(0.4).merge(Pending::Set(0.6)), Pending::Set(0.6));
    }

    #[test]
    fn operations() {
        assert_eq!(op("dialRotate", 3, Some(42), 5.0), Some(Op::Set(0.42)));
        assert_eq!(op("dialRotate", -2, None, 10.0), Some(Op::Step(-0.2)));
        assert_eq!(op("dialRotate", 0, None, 5.0), None);
        assert_eq!(op("keyDown", 0, None, 5.0), Some(Op::ToggleMute));
        assert_eq!(op("dialUp", 0, None, 5.0), None);
    }
}
