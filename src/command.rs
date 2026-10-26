//! "Command": runs a command line with the control's value — on a MasterHUB fader `{value}` is its position
//! (0–100), on knobs, rollers and the dial `{ticks}` the clicks turned; a press runs its own command. E.g.
//! `ddcctl -d 1 -b {value}` for a monitor's brightness. Only numbers are put into the command, which comes from
//! the user's own action settings; it runs through the system shell.
//!
//! A press starts its command and doesn't wait (it may start an app that keeps running). Turns and fader moves
//! wait in `PENDING` per action, clicks adding up and a fader keeping its last position, and one thread per
//! action runs them one after another, each for at most `TIME_LIMIT`: a slow or hung command only holds up its
//! own action. A command over its limit is stopped with everything it started.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub const UUID: &str = "com.masterhub.opendeck.command";

/// Longest a turn's or fader's command may run.
const TIME_LIMIT: Duration = Duration::from_secs(10);

/// What a turn or fader move asks to run: the action's command, the fader's last position, the clicks so far.
struct Turn {
    command: String,
    value: Option<u8>,
    ticks: i64,
}

/// Turns waiting per action (context), and the actions whose thread is running.
type Waiting = (HashMap<String, Turn>, HashSet<String>);
static PENDING: Mutex<Option<Waiting>> = Mutex::new(None);

/// The command line for a turn: `{value}` the fader's position, `{ticks}` the clicks (numbers only).
fn turn_line(command: &str, value: Option<u8>, ticks: i64) -> String {
    match value {
        Some(v) => command.replace("{value}", &v.min(100).to_string()).replace("{ticks}", &ticks.to_string()),
        None => command.replace("{ticks}", &ticks.to_string()),
    }
}

/// Handles an event for the action; `fader` gives a MasterHUB fader's last position by device id and position.
pub fn handle(msg: &Value, fader: impl Fn(&str, u8) -> Option<u8>) -> Vec<Value> {
    let event = msg["event"].as_str().unwrap_or_default();
    let device = msg["device"].as_str().unwrap_or_default();
    let settings = &msg["payload"]["settings"];
    let template = |key: &str| settings[key].as_str().map(str::trim).filter(|c| !c.is_empty()).map(str::to_string);
    let alert = || vec![json!({ "event": "showAlert", "context": msg["context"] })];
    match event {
        "keyDown" | "dialDown" => match template("pressCommand") {
            Some(line) => spawn(&line),
            None => return alert(),
        },
        "dialRotate" => {
            let Some(command) = template("command") else { return alert() };
            let value = fader(device, msg["payload"]["coordinates"]["column"].as_u64().unwrap_or(0) as u8);
            queue(msg["context"].to_string(), command, value, msg["payload"]["ticks"].as_i64().unwrap_or(0));
        }
        _ => {}
    }
    Vec::new()
}

/// Adds a turn to its action's pending one and starts the action's thread when none runs.
fn queue(context: String, command: String, value: Option<u8>, ticks: i64) {
    let mut guard = PENDING.lock().unwrap();
    let (pending, running) = guard.get_or_insert_with(Default::default);
    let turn = pending.entry(context.clone()).or_insert(Turn { command: String::new(), value: None, ticks: 0 });
    turn.command = command;
    turn.value = value.or(turn.value);
    turn.ticks += ticks;
    if running.insert(context.clone()) {
        std::thread::spawn(move || loop {
            let turn = {
                let mut guard = PENDING.lock().unwrap();
                let (pending, running) = guard.get_or_insert_with(Default::default);
                match pending.remove(&context) {
                    Some(t) => t,
                    None => {
                        running.remove(&context);
                        return;
                    }
                }
            };
            let line = turn_line(&turn.command, turn.value, turn.ticks);
            if let Some((false, why)) = run_limited(&line, TIME_LIMIT) {
                eprintln!("command `{line}` failed: {why}");
            }
        });
    }
}

/// Starts a command line and doesn't wait for it; a thread reaps it and logs a failure.
pub fn spawn(line: &str) {
    match shell(line).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(child) => reap(child, line.to_string()),
        Err(e) => eprintln!("command `{line}`: {e}"),
    }
}

/// Waits for a started program on a thread of its own and logs when it fails.
pub fn reap(mut child: Child, what: String) {
    std::thread::spawn(move || match child.wait() {
        Ok(status) if !status.success() => eprintln!("`{what}` failed: {status}"),
        Err(e) => eprintln!("`{what}`: {e}"),
        Ok(_) => {}
    });
}

/// Runs a command line, its output unread, for at most `limit`: whether it succeeded, or why not. None when it
/// couldn't start.
fn run_limited(line: &str, limit: Duration) -> Option<(bool, String)> {
    let child = match shell(line).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("command `{line}`: {e}");
            return None;
        }
    };
    Some(finish(child, limit, None))
}

/// Runs a command line for at most `limit`: whether it succeeded, and what it printed (at most 64 KB of it). A
/// command over its limit is stopped with everything it started and counts as failed; so does one that ends but
/// leaves something running that holds its output. None when it couldn't start.
pub fn output_within(line: &str, limit: Duration) -> Option<(bool, String)> {
    let mut child = match shell(line).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("command `{line}`: {e}");
            return None;
        }
    };
    // read on a thread, so a program that keeps the pipe open can't hold this one up
    let (tx, rx) = mpsc::channel();
    let mut stdout = child.stdout.take()?;
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stdout).take(64 << 10).read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    Some(finish(child, limit, Some(rx)))
}

/// Waits for a started command and its output until `limit`; stops it (and what it started) past that.
fn finish(mut child: Child, limit: Duration, output: Option<mpsc::Receiver<Vec<u8>>>) -> (bool, String) {
    let started = Instant::now();
    let left = |started: Instant| limit.saturating_sub(started.elapsed());
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < limit => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                kill_tree(&mut child);
                return (false, format!("timed out after {} s", limit.as_secs()));
            }
        }
    };
    let text = match output {
        None => String::new(),
        Some(rx) => match rx.recv_timeout(left(started).max(Duration::from_millis(200))) {
            Ok(buf) => String::from_utf8_lossy(&buf).into_owned(),
            Err(_) => {
                kill_tree(&mut child); // something it started still holds the output
                return (false, "its output never ended".into());
            }
        },
    };
    (status.success(), text)
}

/// Stops a command and everything it started: its process group (Unix), its process tree (Windows).
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    let _ = Command::new("kill").args(["-KILL", "--", &format!("-{}", child.id())]).status();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .creation_flags(0x0800_0000)
            .stdout(std::process::Stdio::null()) // its "SUCCESS: ..." lines would land in the plugin's log
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The system shell for a command line, in a process group of its own (Unix) so `kill_tree` reaches everything it
/// starts.
fn shell(line: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.args(["/C", line]).creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        c
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new("sh");
        c.args(["-c", line]).process_group(0);
        c
    }
}

/// Runs a program (no shell) for at most `limit` and returns what it printed, or None when it failed, couldn't
/// start or ran too long (the system tools the plugin asks: ioreg, PowerShell, nvidia-smi…).
pub fn tool_output(cmd: &mut Command, limit: Duration) -> Option<String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let (tx, rx) = mpsc::channel();
    let mut stdout = child.stdout.take()?;
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stdout).take(16 << 20).read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let (ok, text) = finish(child, limit, Some(rx));
    ok.then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_lines() {
        assert_eq!(turn_line("ddcctl -d 1 -b {value}", Some(142), 3), "ddcctl -d 1 -b 100");
        assert_eq!(turn_line("volume {ticks}", None, -2), "volume -2");
        assert_eq!(turn_line("x {value} {ticks}", None, 1), "x {value} 1"); // no fader: {value} stays
    }

    #[cfg(unix)]
    #[test]
    fn commands_get_a_time_limit() {
        assert_eq!(output_within("echo '#ff8800'", Duration::from_secs(5)), Some((true, "#ff8800\n".into())));
        assert_eq!(output_within("echo no; exit 3", Duration::from_secs(5)).map(|r| r.0), Some(false));
        let started = Instant::now();
        let (ok, why) = output_within("sleep 30", Duration::from_millis(300)).unwrap();
        assert!(!ok && why.starts_with("timed out") && started.elapsed() < Duration::from_secs(5));
        // what it started is stopped too, and one that leaves something holding its output doesn't hang
        let started = Instant::now();
        let (ok, why) = output_within("sleep 30 & echo started", Duration::from_millis(500)).unwrap();
        assert!(!ok && why.contains("output") && started.elapsed() < Duration::from_secs(5), "{why}");
        assert_eq!(run_limited("true", Duration::from_secs(5)), Some((true, String::new())));
        assert_eq!(tool_output(Command::new("echo").arg("hi"), Duration::from_secs(5)).as_deref(), Some("hi\n"));
        assert_eq!(tool_output(Command::new("sleep").arg("30"), Duration::from_millis(200)), None);
    }

    #[cfg(unix)]
    #[test]
    fn turns_add_up_per_action() {
        let dir = std::env::temp_dir().join(format!("mh-turns-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let command = format!("sleep 0.3; echo {{ticks}} >> '{}'", log.display());
        queue("ctx".into(), command.clone(), None, 1);
        std::thread::sleep(Duration::from_millis(100)); // the first turn is running now
        for _ in 0..4 {
            queue("ctx".into(), command.clone(), None, 1);
        }
        std::thread::sleep(Duration::from_millis(1200));
        let runs = std::fs::read_to_string(&log).unwrap();
        // the first turn ran at once, the four that came meanwhile as one, after it
        assert_eq!(runs.lines().collect::<Vec<_>>(), ["1", "4"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
