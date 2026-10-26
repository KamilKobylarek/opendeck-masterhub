//! GPU load and temperature for the GPU dial actions. There is no vendor-neutral API, so each system uses the
//! simplest source that needs no admin rights:
//! - macOS: `ioreg` (the accelerator's "Device Utilization %"; no temperature without the SMC),
//! - NVIDIA on Windows and Linux: `nvidia-smi`,
//! - Linux AMD / Intel: sysfs (`gpu_busy_percent`, the card's hwmon temperature),
//! - Windows otherwise: LibreHardwareMonitor's web server (Options → Remote Web Server, port 8085) when running.
//!
//! Read on a background thread every 2 s while someone asks, so a slow tool never stalls the plugin.

use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// (load %, temperature °C)
pub type Gpu = (Option<f32>, Option<f32>);

struct State {
    reading: Gpu,
    asked: Instant,
}

/// The latest reading; starts the reader on first use. Stops reading 10 s after the last call.
pub fn reading() -> Gpu {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    let state = STATE.get_or_init(|| {
        std::thread::spawn(|| loop {
            let wanted = STATE.get().is_some_and(|s| s.lock().unwrap().asked.elapsed() < Duration::from_secs(10));
            if wanted {
                let r = read();
                STATE.get().unwrap().lock().unwrap().reading = r;
            }
            std::thread::sleep(Duration::from_secs(2));
        });
        Mutex::new(State { reading: (None, None), asked: Instant::now() })
    });
    let mut s = state.lock().unwrap();
    s.asked = Instant::now();
    s.reading
}

fn read() -> Gpu {
    if cfg!(target_os = "macos") {
        return (run("ioreg", &["-r", "-d", "1", "-c", "IOAccelerator"]).and_then(|t| parse_ioreg(&t)), None);
    }
    if let Some(r) =
        run("nvidia-smi", &["--query-gpu=utilization.gpu,temperature.gpu", "--format=csv,noheader,nounits"])
            .and_then(|t| parse_nvidia(&t))
    {
        return r;
    }
    if cfg!(target_os = "linux") {
        return read_sysfs();
    }
    lhm().map_or((None, None), |v| parse_lhm(&v))
}

/// Output of a command run without a console window, or None when it's missing, fails or takes over 5 s.
fn run(program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    // nvidia-smi can hang when the driver is in trouble: a limit, so the screen shows "--" instead of a stale value
    crate::command::tool_output(&mut cmd, std::time::Duration::from_secs(5))
}

/// `"Device Utilization %"=37` in ioreg's PerformanceStatistics (first GPU).
fn parse_ioreg(text: &str) -> Option<f32> {
    let rest = &text[text.find("\"Device Utilization %\"=")? + 23..];
    rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// `37, 55` per GPU; the first one counts.
fn parse_nvidia(text: &str) -> Option<Gpu> {
    let mut parts = text.lines().next()?.split(',').map(|p| p.trim().parse::<f32>().ok());
    let load = parts.next()??;
    Some((Some(load), parts.next().flatten()))
}

fn read_sysfs() -> Gpu {
    let read = |p: &std::path::Path| std::fs::read_to_string(p).ok().and_then(|s| s.trim().parse::<f32>().ok());
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else { return (None, None) };
    for card in cards.flatten() {
        let device = card.path().join("device");
        let Some(load) = read(&device.join("gpu_busy_percent")) else { continue };
        let temp = std::fs::read_dir(device.join("hwmon"))
            .ok()
            .and_then(|mut h| h.find_map(|e| e.ok().and_then(|e| read(&e.path().join("temp1_input")))))
            .map(|millis| millis / 1000.0);
        return (Some(load), temp);
    }
    (None, None)
}

/// LibreHardwareMonitor's sensor tree from its web server, or None when it isn't running.
fn lhm() -> Option<Value> {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 8085));
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream.write_all(b"GET /data.json HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").ok()?;
    let mut response = String::new();
    stream.take(4 << 20).read_to_string(&mut response).ok()?;
    let (head, body) = response.split_once("\r\n\r\n")?;
    // only LibreHardwareMonitor's answer, not whatever else listens on that port
    head.lines().next().filter(|status| status.split_whitespace().nth(1) == Some("200"))?;
    serde_json::from_str(body).ok()
}

/// "GPU Core" under the first GPU's "Load" and "Temperatures" groups ("37.0 %", "55,0 °C").
fn parse_lhm(tree: &Value) -> Gpu {
    fn find<'a>(node: &'a Value, path: &[&str]) -> Option<&'a Value> {
        let children = node["Children"].as_array()?;
        match path {
            [] => Some(node),
            [first, rest @ ..] => children
                .iter()
                .filter(|c| c["Text"].as_str().is_some_and(|t| t.contains(first)))
                .find_map(|c| find(c, rest)),
        }
    }
    let number = |v: &Value| v["Value"].as_str()?.split_whitespace().next()?.replace(',', ".").parse::<f32>().ok();
    // the hardware node whose sensors include a GPU Core load is the GPU
    let gpus = || tree.pointer("/Children/0/Children").and_then(Value::as_array).into_iter().flatten();
    for hw in gpus() {
        let load = find(hw, &["Load"])
            .and_then(|g| g["Children"].as_array()?.iter().find(|s| s["Text"] == "GPU Core"))
            .and_then(number);
        if load.is_some() {
            let temp = find(hw, &["Temperatures"])
                .and_then(|g| g["Children"].as_array()?.iter().find(|s| s["Text"] == "GPU Core"))
                .and_then(number);
            return (load, temp);
        }
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_every_source() {
        let ioreg = "+-o AGXAcceleratorG13X  <class AGXAcceleratorG13X>\n  \"PerformanceStatistics\" = {\"In use system memory\"=1,\"Device Utilization %\"=37,\"Renderer Utilization %\"=30}";
        assert_eq!(parse_ioreg(ioreg), Some(37.0));
        assert_eq!(parse_ioreg("nothing"), None);

        assert_eq!(parse_nvidia("12, 55\n40, 60\n"), Some((Some(12.0), Some(55.0))));
        assert_eq!(parse_nvidia("[N/A], 50\n"), None);

        let tree = json!({ "Text": "Sensor", "Children": [{ "Text": "DESKTOP", "Children": [
            { "Text": "Intel Core i5", "Children": [{ "Text": "Load", "Children": [{ "Text": "CPU Total", "Value": "5,0 %" }] }] },
            { "Text": "AMD Radeon RX 7800", "Children": [
                { "Text": "Temperatures", "Children": [{ "Text": "GPU Core", "Value": "48,0 °C" }] },
                { "Text": "Load", "Children": [{ "Text": "GPU Core", "Value": "23.0 %" }] }
            ] }
        ] }] });
        assert_eq!(parse_lhm(&tree), (Some(23.0), Some(48.0)));
        assert_eq!(parse_lhm(&json!({})), (None, None));
    }

    /// Real reading on this machine: `cargo test gpu_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn gpu_live() {
        println!("{:?}", read());
    }
}
