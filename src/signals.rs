//! Signal LEDs: a knob or fader LED that shows something unrelated to its control (a VPN, a build, a door…).
//! Each signal's command runs every `signal_every` seconds on a thread of its own; what it prints or how it
//! exits picks the colour (lights::signal_colour); one that runs over `TIME_LIMIT` is stopped and counts as
//! failed. Other programs (Home Assistant) set LEDs over MQTT instead (mqtt.rs).

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{command, lights, Shared, Up};

/// Longest a signal command may run.
const TIME_LIMIT: Duration = Duration::from_secs(10);

/// Runs the signal commands while the plugin manages the LEDs and reports each result as `Up::Signal`.
pub fn start(shared: Shared, up: Sender<Up>) {
    std::thread::spawn(move || {
        let mut last: HashMap<String, Instant> = HashMap::new();
        let running: Arc<Mutex<HashSet<String>>> = Arc::default();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let (signals, every) = {
                let cfg = shared.read().unwrap();
                if !cfg.lights.managed {
                    continue;
                }
                (cfg.lights.signals.clone(), Duration::from_secs(cfg.lights.signal_every as u64))
            };
            for (led, line) in signals.into_iter().filter(|(_, line)| !line.is_empty()) {
                if last.get(&led).is_some_and(|t| t.elapsed() < every) || !running.lock().unwrap().insert(led.clone()) {
                    continue;
                }
                last.insert(led.clone(), Instant::now());
                let (up, running) = (up.clone(), running.clone());
                std::thread::spawn(move || {
                    let rgb = command::output_within(&line, TIME_LIMIT)
                        .map_or([255, 0, 0], |(ok, out)| lights::signal_colour(ok, &out));
                    let _ = up.send(Up::Signal(led.clone(), rgb));
                    running.lock().unwrap().remove(&led);
                });
            }
        }
    });
}
