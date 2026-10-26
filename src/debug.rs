//! Live input view (/debug on the settings page): every input packet a module sends, with what the
//! plugin made of it, so reading real hardware can be checked control by control.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::{json, Value};

pub const PAGE: &str = include_str!("debug.html");

/// ponytail: fixed ring of the last packets, recorded even with the page closed (one lock per packet).
const KEEP: usize = 500;

/// (last seq, packets, last base packet).
static LOG: Mutex<(u64, VecDeque<Value>, Vec<u8>)> = Mutex::new((0, VecDeque::new(), Vec::new()));

/// Records one input packet: `raw` as read, `events` = what the plugin sends to OpenDeck for it.
pub fn record(device: &str, kind: &str, raw: &[u8], events: Vec<Value>) {
    let hex: Vec<String> = raw.iter().map(|b| format!("{b:02x}")).collect();
    let mut log = LOG.lock().unwrap();
    // The base is polled for its orientation twice a second: keep only packets that differ.
    if kind == "Base" {
        if log.2 == raw {
            return;
        }
        log.2 = raw.to_vec();
    }
    log.0 += 1;
    let entry = json!({
        "seq": log.0, "time": chrono::Local::now().format("%H:%M:%S%.3f").to_string(),
        "device": device, "kind": kind, "raw": hex.join(" "), "events": events,
    });
    log.1.push_back(entry);
    if log.1.len() > KEEP {
        log.1.pop_front();
    }
}

/// Packets newer than `after` (the page passes the last `seq` it saw).
pub fn since(after: u64) -> Value {
    let log = LOG.lock().unwrap();
    json!(log.1.iter().filter(|e| e["seq"].as_u64() > Some(after)).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_packets_and_skips_repeated_base_polls() {
        let start = LOG.lock().unwrap().0;
        record("mh-0202-T", "Knobs", &[1, 0x2a], vec![]);
        record("mh-0202-T", "Knobs", &[1, 0x2a], vec![]); // same turn twice is two events
        record("base", "Base", &[1, 0x2b, 9], vec![]);
        record("base", "Base", &[1, 0x2b, 9], vec![]);
        let new = since(start);
        assert_eq!(new.as_array().unwrap().iter().filter(|e| e["kind"] == "Knobs").count(), 2);
        assert_eq!(new.as_array().unwrap().iter().filter(|e| e["kind"] == "Base").count(), 1);
    }
}
