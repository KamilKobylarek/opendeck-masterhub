//! Per-app volume on every system, for the "App volume" action:
//! - macOS: FineTune's URL scheme (finetune.rs) — write-only; a step becomes an absolute level (appvolume.rs),
//! - Windows: Core Audio sessions (audio_windows.rs),
//! - Linux: PulseAudio / PipeWire streams through `pactl`.
//!
//! Every call blocks (a tool or COM), so it runs on the runner thread, never on the main loop.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    /// Level 0.0–1.0 (a fader's position).
    Set(f32),
    /// Change by this much (0.05 = +5%).
    Step(f32),
    ToggleMute,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct State {
    pub level: f32,
    pub muted: bool,
}

/// Whether the system steps by any amount (not FineTune, 5% per URL) and answers with the state (not FineTune).
pub const STEPS_ANY: bool = !cfg!(target_os = "macos");

/// Applies `op` to every audio stream of `apps` and returns the first one's state afterwards; None when no app
/// matched or the system doesn't report it.
pub fn run(apps: &[String], op: Op) -> Option<State> {
    imp::run(apps, op)
}

/// The current level of the first of `apps` that has audio, without changing anything.
pub fn read(apps: &[String]) -> Option<State> {
    #[cfg(target_os = "macos")]
    return imp::read(apps.first()?);
    #[cfg(target_os = "linux")]
    return imp::read(apps);
    // Windows: a step of 0 only reads
    #[cfg(windows)]
    return imp::run(apps, Op::Step(0.0));
}

/// An app name as the user typed it, for comparing with what the system reports ("Spotify.exe" → "spotify").
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn normalize(name: &str) -> String {
    let n = name.trim().to_ascii_lowercase();
    n.strip_suffix(".exe").unwrap_or(&n).to_string()
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{Op, State};

    pub fn run(apps: &[String], op: Op) -> Option<State> {
        for url in crate::finetune::urls(apps, op) {
            crate::finetune::open(&url);
        }
        None // FineTune can't be asked; App volume reads its saved settings a little later
    }

    /// An app's level and mute as FineTune saved them (~/Library/Application Support/FineTune/settings.json).
    pub fn read(app: &str) -> Option<State> {
        let path = std::path::PathBuf::from(std::env::var_os("HOME")?)
            .join("Library/Application Support/FineTune/settings.json");
        crate::finetune::saved_state(&std::fs::read_to_string(path).ok()?, app)
    }
}

#[cfg(windows)]
#[path = "audio_windows.rs"]
mod imp;

#[cfg(target_os = "linux")]
mod imp {
    use super::{parse_sink_inputs, Op, State};

    fn pactl(args: &[&str]) -> Option<String> {
        let out = crate::command::tool_output(
            std::process::Command::new("pactl").args(args),
            std::time::Duration::from_secs(5),
        );
        if out.is_none() {
            eprintln!("pactl {args:?} failed");
        }
        out
    }

    /// The first stream's level and mute, only read: setting it to itself would clip a level over 100% and the
    /// balance between channels.
    pub fn read(apps: &[String]) -> Option<State> {
        streams(apps).first().map(|&(_, level, muted)| State { level, muted })
    }

    fn streams(apps: &[String]) -> Vec<(u64, f32, bool)> {
        pactl(&["-f", "json", "list", "sink-inputs"]).map_or_else(Vec::new, |json| parse_sink_inputs(&json, apps))
    }

    pub fn run(apps: &[String], op: Op) -> Option<State> {
        let percent = |l: f32| format!("{}%", (l.clamp(0.0, 1.0) * 100.0).round() as u32);
        let found = streams(apps);
        for (index, level, _) in &found {
            let index = index.to_string();
            match op {
                Op::Set(l) => pactl(&["set-sink-input-volume", &index, &percent(l)]),
                Op::Step(d) => pactl(&["set-sink-input-volume", &index, &percent(level + d)]),
                Op::ToggleMute => pactl(&["set-sink-input-mute", &index, "toggle"]),
            };
        }
        if found.is_empty() {
            return None;
        }
        streams(apps).first().map(|&(_, level, muted)| State { level, muted })
    }
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod imp {
    use super::{Op, State};

    pub fn run(_apps: &[String], _op: Op) -> Option<State> {
        None
    }
}

/// `pactl -f json list sink-inputs` → (index, level 0–1, muted) of the streams whose process binary or
/// application name is one of `apps` (normalized).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_sink_inputs(json: &str, apps: &[String]) -> Vec<(u64, f32, bool)> {
    let apps: Vec<String> = apps.iter().map(|a| normalize(a)).collect();
    let Ok(list) = serde_json::from_str::<Vec<serde_json::Value>>(json) else { return Vec::new() };
    list.iter()
        .filter_map(|s| {
            let props = &s["properties"];
            let names = [props["application.process.binary"].as_str(), props["application.name"].as_str()];
            if !names.iter().flatten().any(|n| apps.contains(&normalize(n))) {
                return None;
            }
            // the first channel's "value_percent", e.g. "45%"
            let percent = s["volume"].as_object()?.values().next()?["value_percent"].as_str()?;
            let level = percent.trim_end_matches('%').trim().parse::<f32>().ok()? / 100.0;
            Some((s["index"].as_u64()?, level, s["mute"].as_bool().unwrap_or(false)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pactl_streams() {
        let json = r#"[
            {"index": 42, "mute": false, "volume": {"front-left": {"value": 29491, "value_percent": "45%", "db": "-20 dB"}},
             "properties": {"application.name": "Spotify", "application.process.binary": "spotify"}},
            {"index": 7, "mute": true, "volume": {"mono": {"value_percent": "100%"}},
             "properties": {"application.name": "Firefox", "application.process.binary": "firefox"}}
        ]"#;
        assert_eq!(parse_sink_inputs(json, &["Spotify.exe".into()]), [(42, 0.45, false)]);
        assert_eq!(parse_sink_inputs(json, &["firefox".into(), "spotify".into()]).len(), 2);
        assert!(parse_sink_inputs(json, &["discord".into()]).is_empty());
        assert!(parse_sink_inputs("not json", &["spotify".into()]).is_empty());
    }
}
