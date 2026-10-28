//! The modules' side: a thread per module and one for the base, their input reports turned into OpenDeck
//! events, and every report sent to them (through `allowed`, which refuses the dangerous commands).

use super::*;

/// Module ids whose thread was told to stop and hasn't ended yet: a module isn't opened again before its old
/// thread let go of it (both would read its input and write their pictures into each other).
pub(crate) static STOPPING: Mutex<Option<HashSet<String>>> = Mutex::new(None);

pub(crate) fn stopping(id: &str) -> bool {
    STOPPING.lock().unwrap().as_ref().is_some_and(|s| s.contains(id))
}

/// Tells a module's thread to stop, and remembers that until it has (`Ended`).
pub(crate) fn stop(id: &str, h: &Handle) {
    STOPPING.lock().unwrap().get_or_insert_with(HashSet::new).insert(id.to_string());
    let _ = h.tx.send(Down::Stop);
}

/// Dropped when a module's thread ends, however it ends.
pub(crate) struct Ended(String);

impl Drop for Ended {
    fn drop(&mut self) {
        if let Some(s) = STOPPING.lock().unwrap().as_mut() {
            s.remove(&self.0);
        }
    }
}

/// Owns one module's HID handle: applies commands from OpenDeck and turns input reports into events.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_module(
    device: HidDevice,
    model: &'static Model,
    id: String,
    name: String,
    rot: u16,
    path: Option<String>,
    down_rx: Receiver<Down>,
    up_tx: Sender<Up>,
    shared: Shared,
) {
    let _ended = Ended(id.clone());
    let device = &device;
    let debug = std::env::var_os("MASTERHUB_DEBUG").is_some();
    let kind = model.kind;
    // Setup that waits on the module, here rather than in the main loop: a module that doesn't answer holds up
    // only itself. Feature reads from a 15xKey or dial still in hardware mode can hang it until it's replugged,
    // so its info is only read once software mode is confirmed.
    let read_info = match model.sw_mode_cmd {
        Some(cmd) if !software_mode(device, cmd) => {
            eprintln!("{id} did not confirm software mode: close the official MasterHUB app and replug the module");
            if reset_stuck(&id, path.as_deref()) {
                // it goes away and comes back as a new device, which the scan sets up afresh
                let _ = up_tx.send(Up::Gone(id));
                return;
            }
            false
        }
        _ => true,
    };
    if kind == Kind::Rollers {
        wake_rollers(&id, path.as_deref());
    }
    let mut state = State::default();
    if kind == Kind::Faders {
        // Feature report 0x33 (Get_Slider_Value): positions at [2..7], so the first move isn't lost.
        // 32 bytes like the SDK reads every feature report (Windows checks the length).
        let mut raw = [0u8; 0x20];
        raw[0] = 0x33;
        match device.get_feature_report(&mut raw) {
            Ok(n) if n >= 7 && raw[0] == 0x33 => {
                if debug {
                    eprintln!("{id}: start positions {:?}", &raw[2..7]);
                }
                state.faders = Some(raw[2..7].to_vec());
                // the LEDs showing positions and the actions know them from the start, by OpenDeck position
                {
                    let mut cfg = shared.write().unwrap();
                    for (i, v) in raw[2..7].iter().enumerate() {
                        let position = encoder_index(kind, rot, FADERS as u8, i as u8);
                        cfg.fader_values.insert(format!("{id}/{position}"), (*v).min(100));
                    }
                }
                let _ = up_tx.send(Up::LightsChanged(false));
            }
            // without them the first report sets where the faders are
            Ok(n) => eprintln!("{id}: unexpected fader positions reply {:02x?}", &raw[..n.min(8)]),
            Err(e) => eprintln!("{id}: cannot read fader positions: {e}"),
        }
    }
    let default_len = if matches!(kind, Kind::Keys | Kind::Dial) { DEFAULT_REPORT_LEN } else { DEFAULT_M0_REPORT_LEN };
    let report_len = report_len(device, default_len, &id, debug);
    let ids = match kind {
        Kind::Keys => &device_info::M4315,
        Kind::Dial => &device_info::M131E,
        _ => &device_info::M0,
    };
    let info = if read_info {
        device_info::read(device, ids, &id, &name)
    } else {
        device_info::Info { id: id.clone(), name: name.clone(), ..Default::default() }
    };
    let _ = up_tx.send(Up::Info(info));

    let mut key_images: HashMap<u8, (Vec<u8>, Vec<u8>)> = HashMap::new(); // key -> (normal, pressed)
    let mut background: Option<Arc<KeypadBackground>> = None;
    // Animated background: the keys' images at window size for compositing, and where the animation is.
    let mut key_pixels: HashMap<u8, RgbImage> = HashMap::new();
    let (mut frame, mut next_frame) = (0usize, Instant::now());
    let (mut frames_sent, mut frames_since) = (0u32, Instant::now()); // MASTERHUB_DEBUG: the rate reached
    let mut screen: Option<Vec<u8>> = None; // the dial's current picture, to put back after identify mode
                                            // Identify mode: until then the screens show numbers; what OpenDeck sends meanwhile is kept for later.
    let mut identify_until: Option<Instant> = None;
    // OpenDeck's brightness 0 (e.g. its device sleep): screens black, what OpenDeck sends kept for later
    let mut dark = false;
    // With a keypad background, a cleared key shows the background again: redraw it, then the keys.
    let redraw = |background: &Option<Arc<KeypadBackground>>,
                  key_images: &HashMap<u8, (Vec<u8>, Vec<u8>)>|
     -> hidapi::HidResult<()> {
        match background {
            Some(bg) => write_image(device, 0x35, 0, &bg.first, report_len)?,
            None => send_feature(device, &[0x03, 0x36, 0, 0, 0])?,
        }
        key_images.iter().try_for_each(|(k, (jpeg, _))| write_image(device, 0x36, *k, jpeg, report_len))
    };
    // the screen as OpenDeck last set it, after identify mode or a dark screen
    let restore = |background: &Option<Arc<KeypadBackground>>,
                   key_images: &HashMap<u8, (Vec<u8>, Vec<u8>)>,
                   screen: &Option<Vec<u8>>|
     -> hidapi::HidResult<()> {
        match (kind, screen) {
            (Kind::Keys, _) => redraw(background, key_images),
            (Kind::Dial, Some(jpeg)) => write_image(device, 0x45, 0, jpeg, report_len),
            (Kind::Dial, None) => send_feature(device, &[0x03, 0x46, 0, 0, 0]),
            _ => Ok(()),
        }
    };
    let black = || match kind {
        Kind::Keys => send_feature(device, &[0x03, 0x36, 0, 0, 0]),
        Kind::Dial => send_feature(device, &[0x03, 0x46, 0, 0, 0]),
        _ => Ok(()),
    };
    let mut buf = [0u8; 512];
    loop {
        if identify_until.is_some_and(|t| Instant::now() >= t) {
            identify_until = None;
            let result = if dark { black() } else { restore(&background, &key_images, &screen) };
            if let Err(e) = result {
                eprintln!("{id}: write failed: {e}");
            }
        }
        if let Some(bg) = background.as_ref().filter(|b| !b.frames.is_empty() && identify_until.is_none() && !dark) {
            if Instant::now() >= next_frame {
                let (img, delay) = &bg.frames[frame % bg.frames.len()];
                let result = to_jpeg(&DynamicImage::from(panel_frame(img, &key_pixels)))
                    .map_err(|e| hidapi::HidError::HidApiError { message: e.to_string() })
                    .and_then(|jpeg| write_image(device, 0x35, 0, &jpeg, report_len));
                if let Err(e) = result {
                    eprintln!("{id}: write failed: {e}");
                }
                // on schedule rather than from now, so the time a frame takes to send doesn't slow the animation
                (frame, next_frame) = (frame + 1, (next_frame + *delay).max(Instant::now()));
                frames_sent += 1;
                if debug && frames_since.elapsed() >= Duration::from_secs(5) {
                    eprintln!(
                        "{id}: background {:.1} frames a second",
                        frames_sent as f64 / frames_since.elapsed().as_secs_f64()
                    );
                    (frames_sent, frames_since) = (0, Instant::now());
                }
            }
        }
        let waiting: Vec<Down> = down_rx.try_iter().collect();
        // an LED's colour or brightness that a later one in the queue replaces isn't sent (effects send ~25 a
        // second; on a slow module they'd pile up and delay its input)
        let replaced = |i: usize| match &waiting[i] {
            Down::LedColour { index, save: false, .. } => waiting[i + 1..]
                .iter()
                .any(|d| matches!(d, Down::LedColour { index: j, save: false, .. } if j == index)),
            Down::LedBrightness { index, .. } => {
                waiting[i + 1..].iter().any(|d| matches!(d, Down::LedBrightness { index: j, .. } if j == index))
            }
            _ => false,
        };
        let keep: Vec<bool> = (0..waiting.len()).map(|i| !replaced(i)).collect();
        for (cmd, keep) in waiting.into_iter().zip(keep) {
            if !keep {
                continue;
            }
            let identifying = identify_until.is_some() || dark; // screen writes held back
            let result = match cmd {
                Down::KeyImage { position, jpeg, pressed } => {
                    let (jpeg, pressed) = match upright(&shared, rot) {
                        true => (turn_jpeg(&jpeg, rot), turn_jpeg(&pressed, rot)),
                        false => (jpeg, pressed),
                    };
                    let r = if identifying { Ok(()) } else { write_image(device, 0x36, position, &jpeg, report_len) };
                    if let Ok(img) = image::load_from_memory(&jpeg) {
                        key_pixels.insert(
                            position,
                            img.resize_exact(KEY_WINDOW.0, KEY_WINDOW.1, image::imageops::FilterType::Triangle)
                                .to_rgb8(),
                        );
                    }
                    key_images.insert(position, (jpeg, pressed));
                    r
                }
                Down::ScreenImage(jpeg) => {
                    let jpeg = if upright(&shared, rot) { turn_jpeg(&jpeg, rot) } else { jpeg };
                    let r = if identifying { Ok(()) } else { write_image(device, 0x45, 0, &jpeg, report_len) };
                    screen = Some(jpeg);
                    r
                }
                Down::ScreenColour(rgb) => {
                    screen = None;
                    if identifying {
                        Ok(())
                    } else {
                        send_feature(device, &[0x03, 0x46, rgb[0], rgb[1], rgb[2]])
                    } // dial, Display_Color_On_Screen
                }
                Down::Identify => {
                    identify_until = Some(Instant::now() + IDENTIFY_TIME);
                    match kind {
                        // each key shows its OpenDeck position, the dial the rotation the plugin assumes
                        Kind::Keys => (0..KEY_ROWS * KEY_COLS).try_for_each(|k| {
                            write_image(
                                device,
                                0x36,
                                k,
                                &actions::identify_jpeg(&key_to_display(k, rot).to_string(), rot, 72),
                                report_len,
                            )
                        }),
                        Kind::Dial => write_image(
                            device,
                            0x45,
                            0,
                            &actions::identify_jpeg(&format!("{rot}°"), rot, 240),
                            report_len,
                        ),
                        _ => Ok(()),
                    }
                }
                // during identify mode only remembered: its end redraws the keys
                Down::Background(bg) => {
                    background = bg;
                    (frame, next_frame) = (0, Instant::now());
                    if identifying {
                        Ok(())
                    } else {
                        redraw(&background, &key_images)
                    }
                }
                Down::KeyColor { position: Some(p), rgb } => {
                    key_images.remove(&p);
                    key_pixels.remove(&p);
                    match background {
                        _ if identifying => Ok(()),
                        Some(_) => redraw(&background, &key_images),
                        None => send_feature(device, &[0x03, 0x37, p, rgb[0], rgb[1], rgb[2]]),
                    }
                }
                Down::KeyColor { position: None, rgb } => {
                    key_images.clear();
                    key_pixels.clear();
                    match background {
                        _ if identifying => Ok(()),
                        Some(_) => redraw(&background, &key_images),
                        None => send_feature(device, &[0x03, 0x36, rgb[0], rgb[1], rgb[2]]),
                    }
                }
                Down::Brightness(b) => match (kind, led_module(kind)) {
                    // never 0: on macOS that times out and leaves the screen module dead. A black screen at the
                    // lowest brightness instead, and the pictures back when the brightness is.
                    (Kind::Keys | Kind::Dial, _) => {
                        let cmd = if kind == Kind::Keys { 0x34 } else { 0x44 };
                        let was_dark = std::mem::replace(&mut dark, b == 0);
                        send_feature(device, &[0x03, cmd, b.clamp(1, 100)]).and_then(|()| {
                            match (was_dark, dark, identify_until.is_some()) {
                                (false, true, false) => black(),
                                (true, false, false) => {
                                    (frame, next_frame) = (0, Instant::now());
                                    restore(&background, &key_images, &screen)
                                }
                                _ => Ok(()),
                            }
                        })
                    }
                    (_, Some(m)) => write_m0(device, report_len, 0x2e, &[m, 0xff, b.min(100)]),
                    _ => Ok(()),
                },
                Down::LedMode { mode, speed, save } => match led_module(kind) {
                    Some(m) => write_led_mode(device, report_len, m, 0xff, mode, speed, save),
                    None => Ok(()),
                },
                Down::LedBrightness { index, value } => match led_module(kind) {
                    Some(m) => write_m0(device, report_len, 0x2e, &[m, index, value.min(100)]),
                    None => Ok(()),
                },
                Down::LedColour { index, rgb, save } => match led_module(kind) {
                    Some(m) => write_m0(device, report_len, 0x2d, &[m, index, rgb[0], rgb[1], rgb[2], save as u8]),
                    None => Ok(()),
                },
                Down::Stop => return,
                Down::Shutdown => {
                    if let Some(cmd) = model.sw_mode_cmd {
                        let _ = send_feature(device, &[0x03, cmd, 0x00]);
                    }
                    return;
                }
            };
            if let Err(e) = result {
                eprintln!("{id}: write failed: {e}");
            }
        }

        // input waits at most until the next background frame is due
        let animating = background.as_ref().is_some_and(|b| b.frames.len() > 1) && identify_until.is_none() && !dark;
        let wait =
            if animating { next_frame.saturating_duration_since(Instant::now()).as_millis().clamp(1, 20) } else { 20 };
        match device.read_timeout(&mut buf, wait as i32) {
            Ok(0) => {}
            Ok(n) => {
                if debug {
                    eprintln!("{id} <- {:02x?}", &buf[..n]);
                }
                // the settings number faders as OpenDeck does; parse works in the module's own order
                let shown = shared.read().unwrap().fader_scale;
                let scales: [f64; FADERS] =
                    std::array::from_fn(|i| shown[encoder_index(Kind::Faders, rot, FADERS as u8, i as u8) as usize]);
                let mut seen = Vec::new();
                for (event, mut payload) in parse(kind, &buf[..n], &mut state, &scales) {
                    let key = payload["position"].as_u64().map(|p| p as u8);
                    orient(kind, rot, model.encoders, &mut payload);
                    seen.push(json!({ "event": event, "module": key, "opendeck": payload["position"], "ticks": payload["ticks"] }));
                    payload["device"] = json!(id);
                    if up_tx.send(Up::Send(json!({ "event": event, "payload": payload }))).is_err() {
                        return; // the plugin is going away
                    }
                    // The official app's press feedback; after the event so actions don't wait for it.
                    if let (Kind::Keys, Some(k), None, false) = (kind, key, identify_until, dark) {
                        if let Some((normal, pressed)) = key_images.get(&k) {
                            let jpeg = if event == "keyDown" { pressed } else { normal };
                            if let Err(e) = write_image(device, 0x36, k, jpeg, report_len) {
                                eprintln!("{id}: write failed: {e}");
                            }
                        }
                    }
                }
                debug::record(&id, &format!("{kind:?}"), &buf[..n], seen);
            }
            Err(e) => {
                eprintln!("{id}: disconnected ({e})");
                let _ = up_tx.send(Up::Gone(id));
                return;
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) pressed: Vec<u8>,
    pub(crate) faders: Option<Vec<u8>>,
    pub(crate) fader_rest: [f64; FADERS], // fractional ticks carried over, so small moves with scale < 1 add up
}

/// Turns one raw input packet into OpenDeck events (event name, payload without "device"),
/// with positions in the module's own order.
///
/// Packet layout (the SDK's Check_Rcv_Data): [0x01, channel tag, len lo, len hi, payload..],
/// payload: [event id, count, one byte per control..].
pub(crate) fn parse(kind: Kind, raw: &[u8], state: &mut State, scales: &[f64; FADERS]) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    // Events come on channel 0 only (what the SDK's Check_IO_Status reads); other channels carry
    // heartbeats and replies (docs/sdk-commands.md).
    if raw.len() < 6 || raw[0] != 0x01 || raw[1] != event_channel(kind) {
        return out;
    }
    let len = (u16::from_le_bytes([raw[2], raw[3]]) as usize).min(raw.len() - 4);
    let payload = &raw[4..4 + len];
    if payload.len() < 2 {
        return out;
    }
    let (event_id, values) = (payload[0], &payload[2..(2 + payload[1] as usize).min(payload.len())]);
    // no more values than the module has controls: a malformed packet can't name a control that isn't there
    let controls = match kind {
        Kind::Keys => (KEY_ROWS * KEY_COLS) as usize,
        Kind::Knobs => 3,
        Kind::Faders => FADERS,
        Kind::Rollers => 2,
        Kind::Dial => 1,
    };
    let values = &values[..values.len().min(controls)];

    let mut presses = |down: &'static str, up: &'static str, values: &[u8], out: &mut Vec<_>| {
        // grow only: a shorter packet mustn't forget a key that's held (its release would be lost)
        if state.pressed.len() < values.len() {
            state.pressed.resize(values.len(), 0);
        }
        for (i, (&now, was)) in values.iter().zip(state.pressed.iter_mut()).enumerate() {
            if now != *was {
                out.push((if now == 1 { down } else { up }, json!({ "position": i })));
                *was = now;
            }
        }
    };
    let rotations = |values: &[u8], cw: u8, out: &mut Vec<_>| {
        for (i, &v) in values.iter().enumerate() {
            if v == 0 || v == 1 {
                let ticks = if v == cw { 1 } else { -1 };
                out.push(("encoderChange", json!({ "position": i, "ticks": ticks })));
            }
        }
    };

    match (kind, event_id) {
        (Kind::Keys, 0x02) => presses("keyDown", "keyUp", values, &mut out),
        (Kind::Knobs, 0x00) => rotations(values, 1, &mut out), // 1 = clockwise
        (Kind::Knobs, 0x03) => presses("encoderDown", "encoderUp", values, &mut out),
        (Kind::Rollers, 0x01) => rotations(values, 0, &mut out), // 0 = up; index 0 is the right wheel
        (Kind::Dial, 0x00 | 0x03) => rotations(&values[..values.len().min(1)], 0, &mut out), // 0 = clockwise here
        (Kind::Dial, 0x04) => presses("encoderDown", "encoderUp", &values[..values.len().min(1)], &mut out),
        // ponytail: OpenDeck has no fader control, so a fader is a dial turned by the position
        // delta (0-100) times the per-fader scale. Faders are only moved by hand, so no drift.
        (Kind::Faders, _) => {
            // Potentiometers jitter: snap the ends (0–2 → 0, 98+ → 100) and only take a change of 2 or more
            // from the last value taken, so a resting fader sends nothing and a slow move still adds up.
            let snap = |v: u8| match v {
                0..=2 => 0,
                98.. => 100,
                v => v,
            };
            let values: Vec<u8> = values.iter().map(|&v| snap(v)).collect();
            if let Some(prev) = &mut state.faders {
                for (i, (&now, was)) in values.iter().zip(prev.iter_mut()).enumerate().take(FADERS) {
                    let was_value = *was;
                    if now.abs_diff(was_value) >= 2 || (now != was_value && (now == 0 || now == 100)) {
                        *was = now;
                        let was = was_value;
                        let exact = (now as f64 - was as f64) * scales[i] + state.fader_rest[i];
                        let ticks = exact.trunc();
                        state.fader_rest[i] = exact - ticks;
                        if ticks != 0.0 {
                            // "value": the position itself, kept by the plugin for absolute actions (FineTune) and not sent on
                            out.push(("encoderChange", json!({ "position": i, "ticks": ticks as i16, "value": now })));
                        }
                    }
                }
            }
            // the first report only sets where the faders are (when the start positions couldn't be read)
            state.faders.get_or_insert(values);
        }
        _ => eprintln!("{kind:?}: unknown input {:02x?} (run with MASTERHUB_DEBUG=1 and report)", payload),
    }
    out
}

/// Tag of input channel 0 (events) per SDK: M0 modules '*' (0x2a), M4315 '4' (0x34), M131E 'D' (0x44).
pub(crate) fn event_channel(kind: Kind) -> u8 {
    match kind {
        Kind::Keys => 0x34,
        Kind::Dial => 0x44,
        Kind::Knobs | Kind::Faders | Kind::Rollers => 0x2a,
    }
}

/// Maps a module-order position to where the control sits as the user sees the base. Like the
/// official app (which only rotates its drawing), turning directions and images are left alone.
pub(crate) fn orient(kind: Kind, rot: u16, encoders: u8, payload: &mut Value) {
    let Some(pos) = payload["position"].as_u64() else { return };
    let pos = pos as u8;
    match kind {
        Kind::Keys => payload["position"] = json!(key_to_display(pos, rot)),
        Kind::Dial => {}
        Kind::Knobs | Kind::Faders | Kind::Rollers => {
            payload["position"] = json!(encoder_index(kind, rot, encoders, pos))
        }
    }
}

/// Module order <-> display order of encoders (the mapping is its own inverse): upside down (180°)
/// or turned left (270°) the first control ends up last; rollers report the right wheel first.
pub(crate) fn encoder_index(kind: Kind, rot: u16, encoders: u8, pos: u8) -> u8 {
    if matches!(rot, 180 | 270) != (kind == Kind::Rollers) {
        encoders - 1 - pos
    } else {
        pos
    }
}

/// LED "module" byte of the M0 LED commands (docs/sdk-commands.md): 0 base, 1 faders, 2 knobs.
pub(crate) fn led_module(kind: Kind) -> Option<u8> {
    match kind {
        Kind::Faders => Some(1),
        Kind::Knobs => Some(2),
        _ => None,
    }
}

/// Key index on the module (row-major, 5x3) -> position in OpenDeck's grid after rotating the module
/// clockwise; at 90°/270° the grid is registered as 3 columns x 5 rows.
pub(crate) fn key_to_display(k: u8, rot: u16) -> u8 {
    let (r, c) = (k / KEY_COLS, k % KEY_COLS);
    match rot {
        90 => c * KEY_ROWS + (KEY_ROWS - 1 - r),
        180 => (KEY_ROWS - 1 - r) * KEY_COLS + (KEY_COLS - 1 - c),
        270 => (KEY_COLS - 1 - c) * KEY_ROWS + r,
        _ => k,
    }
}

pub(crate) fn display_to_key(p: u8, rot: u16) -> Option<u8> {
    (0..KEY_ROWS * KEY_COLS).find(|&k| key_to_display(k, rot) == p)
}

/// Size in bytes (report id included) of an output report, from a HID report descriptor.
pub(crate) fn output_report_len(desc: &[u8], report_id: u8) -> Option<usize> {
    let (mut size, mut count, mut id, mut bits) = (0u32, 0u32, 0u8, 0u32);
    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xfe {
            i += 3 + *desc.get(i + 1)? as usize; // long item
            continue;
        }
        let n = [0, 1, 2, 4][(prefix & 3) as usize];
        let data = desc.get(i + 1..i + 1 + n)?.iter().rev().fold(0u32, |acc, &b| (acc << 8) | b as u32);
        match prefix & 0xfc {
            0x74 => size = data,                             // Report Size
            0x94 => count = data,                            // Report Count
            0x84 => id = data as u8,                         // Report ID
            0x90 if id == report_id => bits += size * count, // Output
            _ => {}
        }
        i += 1 + n;
    }
    (bits > 0).then(|| (bits as usize).div_ceil(8) + 1)
}

/// Output report 0x02 with an image command (0x36 key, 0x45 dial screen): the JPEG goes out in
/// chunks of `report_len - 8` bytes behind [0x02, cmd, chunk u16, length u16, index, last].
pub(crate) fn write_image(
    device: &HidDevice,
    cmd: u8,
    index: u8,
    jpeg: &[u8],
    report_len: usize,
) -> hidapi::HidResult<()> {
    allowed(&[0x02, cmd])?;
    let chunk_max = report_len - 8;
    let send = || {
        for (i, chunk) in jpeg.chunks(chunk_max).enumerate() {
            let mut report = vec![0u8; report_len];
            report[0] = 0x02;
            report[1] = cmd;
            report[2..4].copy_from_slice(&(i as u16).to_le_bytes());
            report[4..6].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
            report[6] = index;
            report[7] = ((i + 1) * chunk_max >= jpeg.len()) as u8;
            report[8..8 + chunk.len()].copy_from_slice(chunk);
            device.write(&report)?;
        }
        Ok(())
    };
    // right after it switched to software mode a module can refuse a write for a moment ("not responding",
    // seen 2026-10-26): the whole picture once more, else a key would stay blank until its next picture
    send().or_else(|_| {
        std::thread::sleep(Duration::from_millis(100));
        send()
    })
}

/// Watches the base's orientation (it only changes when someone turns the base, so poll slowly)
/// and applies LED commands to it.
pub(crate) fn run_base(device: HidDevice, mut horizontal: bool, rx: Receiver<Down>, up_tx: Sender<Up>) {
    let device = &device;
    let report_len = report_len(device, DEFAULT_M0_REPORT_LEN, "base", std::env::var_os("MASTERHUB_DEBUG").is_some());
    let _ = up_tx.send(Up::Info(device_info::read(device, &device_info::M0, BASE_ID, "MasterHUB Base")));
    let mut last_ask = Instant::now();
    // a new orientation read once, waiting for a second reading to count (a flickering sensor otherwise sets
    // every module up again each time)
    let mut candidate: Option<bool> = None;
    loop {
        let mut next = match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(msg) => Some(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        // everything waiting, not one command per turn: a colour is three commands, an effect ~50 a second
        while let Some(msg) = next.take() {
            let result = match msg {
                Down::LedMode { mode, speed, save } => write_led_mode(device, report_len, 0, 0, mode, speed, save),
                Down::LedColour { rgb, save, .. } => {
                    write_m0(device, report_len, 0x2d, &[0, 0, rgb[0], rgb[1], rgb[2], save as u8])
                }
                Down::Brightness(b) | Down::LedBrightness { value: b, .. } => {
                    write_m0(device, report_len, 0x2e, &[0, 0, b.min(100)])
                }
                Down::Stop => return,
                _ => Ok(()),
            };
            if let Err(e) = result {
                eprintln!("base: write failed: {e}");
            }
            next = rx.try_recv().ok();
        }
        // what the base sends by itself, plus asking every 2 s
        let asked = if last_ask.elapsed() >= Duration::from_secs(2) {
            last_ask = Instant::now();
            asked_orientation(device)
        } else {
            None
        };
        match read_orientation(device, Duration::from_millis(20)).map(|sent| sent.or(asked)) {
            Ok(Some(h)) if h == horizontal => candidate = None,
            Ok(Some(h)) if candidate == Some(h) => {
                candidate = None;
                horizontal = h;
                let _ = up_tx.send(Up::Orientation(h));
            }
            Ok(Some(h)) => candidate = Some(h),
            Ok(None) => {}
            Err(e) => {
                eprintln!("base: disconnected ({e})");
                let _ = up_tx.send(Up::BaseGone);
                return;
            }
        }
    }
}

/// The base's orientation asked for with feature report 0x35 (Get_Device_Direction): byte 2 is 1 when it lies
/// horizontal, like its input report. Answered on demand (checked on the user's unit, 2026-10-25: `35 00 01`
/// with the base flat), so it doesn't depend on catching the report the base sends by itself.
pub(crate) fn asked_orientation(device: &HidDevice) -> Option<bool> {
    let mut raw = [0u8; 0x20];
    raw[0] = 0x35;
    let n = device.get_feature_report(&mut raw).ok()?;
    (n >= 3 && raw[0] == 0x35).then_some(raw[2] == 1)
}

/// Reads input reports until the base reports its orientation (Ok(None) on timeout).
pub(crate) fn read_orientation(device: &HidDevice, timeout: Duration) -> hidapi::HidResult<Option<bool>> {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let n = device.read_timeout(&mut buf, left.as_millis().max(1) as i32)?;
        if n == 0 {
            break;
        }
        let h = parse_orientation(&buf[..n]);
        debug::record(
            BASE_ID,
            "Base",
            &buf[..n],
            h.map(|h| json!({ "event": "orientation", "horizontal": h })).into_iter().collect(),
        );
        if let Some(h) = h {
            return Ok(Some(h));
        }
    }
    Ok(None)
}

/// The base's input report: its first data byte is 1 when the base lies horizontal (official app:
/// replyM00x64IoStatus). It comes on channel 1 (0x2b); which channel the app reads isn't visible in the SDK,
/// and the base sends nothing else, so channel 0 counts too.
pub(crate) fn parse_orientation(raw: &[u8]) -> Option<bool> {
    (raw.len() >= 5 && raw[0] == 0x01 && matches!(raw[1], 0x2a | 0x2b) && u16::from_le_bytes([raw[2], raw[3]]) >= 1)
        .then(|| raw[4] == 1)
}

/// Output report length (report id included) from the device's HID descriptor, as the SDK asks hidapi.
pub(crate) fn report_len(device: &HidDevice, default: usize, id: &str, debug: bool) -> usize {
    let mut desc = [0u8; 4096];
    let len = device.get_report_descriptor(&mut desc).ok().and_then(|n| output_report_len(&desc[..n], 0x02));
    if debug || len.is_none() {
        eprintln!("{id}: output report length {len:?} (default {default})");
    }
    // a descriptor that makes no sense (a device posing as a module) can't break the writes
    len.unwrap_or(default).clamp(9, 1024)
}

/// Commands that write firmware, serial numbers or factory data, reset or recalibrate a module (the SDK's
/// commands, docs/sdk-commands.md). The plugin never sends them; this stops a
/// future bug from doing so — e.g. moving a 15xKey command to the dial by +0x10 turns 03 3c into factory reset.
pub(crate) const FORBIDDEN_OUTPUT: &[u8] = &[0x2a, 0x2b, 0x2f, 0x31, 0x32, 0x33, 0x34, 0x38, 0x44, 0x47, 0x49, 0x4a];
pub(crate) const FORBIDDEN_FEATURE: &[u8] = &[0x35, 0x39, 0x3c, 0x3d, 0x3e, 0x45, 0x48, 0x4b, 0x4c, 0x4d];

pub(crate) fn allowed(report: &[u8]) -> hidapi::HidResult<()> {
    let forbidden = match report {
        [0x02, cmd, ..] => FORBIDDEN_OUTPUT.contains(cmd),
        [0x03, cmd, ..] => FORBIDDEN_FEATURE.contains(cmd),
        _ => false,
    };
    if forbidden {
        return Err(hidapi::HidError::HidApiError {
            message: format!("refused to send dangerous command {:02x?}", &report[..2]),
        });
    }
    Ok(())
}

/// An M0 (knob, fader, base) command: output report 0x02, [0x02, cmd, args..] zero-padded.
pub(crate) fn write_m0(device: &HidDevice, report_len: usize, cmd: u8, args: &[u8]) -> hidapi::HidResult<()> {
    allowed(&[0x02, cmd])?;
    let mut report = vec![0u8; report_len];
    report[0] = 0x02;
    report[1] = cmd;
    report[2..2 + args.len()].copy_from_slice(args);
    device.write(&report)?;
    // M0 modules poll their OUT endpoint every 3 ms (the official app paces LEDs ~2 ms apart).
    std::thread::sleep(Duration::from_millis(3));
    Ok(())
}

/// LED effect (0x2c, stored in the module's flash when `save`) and effect speed (0x30, which has no flash flag).
pub(crate) fn write_led_mode(
    device: &HidDevice,
    report_len: usize,
    module: u8,
    index: u8,
    mode: u8,
    speed: u8,
    save: bool,
) -> hidapi::HidResult<()> {
    write_m0(device, report_len, 0x2c, &[module, index, mode, save as u8])?;
    write_m0(device, report_len, 0x30, &[speed.min(100)])
}

/// macOS: the 2xRoller sends nothing until it gets the HID request SET_IDLE, which macOS (unlike Linux and
/// Windows) doesn't send it (usbhost.rs). Every time it registers, since a replug or sleep resets it.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub(crate) fn wake_rollers(id: &str, hid_path: Option<&str>) {
    #[cfg(target_os = "macos")]
    match hid_path.and_then(usbhost::registry_id).map(usbhost::set_idle) {
        Some(Ok(())) => {}
        Some(Err(e)) => eprintln!("{id}: SET_IDLE failed, the rollers may stay silent: {e}"),
        None => eprintln!("{id}: no IORegistry id in the HID path, the rollers may stay silent"),
    }
}

/// macOS: a 15xKey or dial that ignores software mode may have stuck firmware — re-enumerate it as if it were
/// replugged (usbhost.rs), at most once a minute per module; it then shows up again and is set up afresh.
/// True when it was re-enumerated (it disappears now).
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub(crate) fn reset_stuck(id: &str, hid_path: Option<&str>) -> bool {
    #[cfg(target_os = "macos")]
    {
        static LAST: std::sync::Mutex<Option<HashMap<String, Instant>>> = std::sync::Mutex::new(None);
        let Some(registry_id) = hid_path.and_then(usbhost::registry_id) else { return false };
        let mut last = LAST.lock().unwrap();
        let last = last.get_or_insert_with(HashMap::new);
        if last.get(id).is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return false;
        }
        last.insert(id.to_string(), Instant::now());
        match usbhost::reenumerate(registry_id) {
            Ok(()) => {
                eprintln!("{id}: re-enumerated the module (like replugging it)");
                return true;
            }
            Err(e) => eprintln!("{id}: cannot re-enumerate the module: {e}"),
        }
    }
    false
}

/// Switches a 15xKey or dial to software mode (03 3a / 03 49) and reads feature report 0x03 back: the module
/// only applies a feature command once it's read back, and an empty reply means it ignored it. Retries after
/// 600 ms and 2 s more: a module another app (or OpenDeck closing a moment ago) has just let go of takes a while.
pub(crate) fn software_mode(device: &HidDevice, cmd: u8) -> bool {
    for wait in [0, 600, 2000] {
        std::thread::sleep(Duration::from_millis(wait));
        if send_feature(device, &[0x03, cmd, 0x01]).is_ok() {
            let mut back = [0u8; 0x20];
            back[0] = 0x03;
            // Windows reports an empty reply as an error
            if matches!(device.get_feature_report(&mut back), Ok(n) if n > 1) {
                return true;
            }
        }
    }
    false
}

pub(crate) fn send_feature(device: &HidDevice, data: &[u8]) -> hidapi::HidResult<()> {
    allowed(data)?;
    let mut report = [0u8; 0x20];
    report[..data.len()].copy_from_slice(data);
    device.send_feature_report(&report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_commands_are_refused() {
        for bad in [[0x03, 0x4c], [0x03, 0x3d], [0x03, 0x45], [0x02, 0x2a], [0x02, 0x2f], [0x02, 0x47], [0x02, 0x4a]] {
            assert!(allowed(&bad).is_err(), "{bad:02x?}");
        }
        // everything the plugin sends
        for ok in [
            [0x03, 0x3a],
            [0x03, 0x49],
            [0x03, 0x34],
            [0x03, 0x44],
            [0x03, 0x36],
            [0x03, 0x37],
            [0x03, 0x46],
            [0x02, 0x2c],
            [0x02, 0x2d],
            [0x02, 0x2e],
            [0x02, 0x30],
            [0x02, 0x35],
            [0x02, 0x36],
            [0x02, 0x45],
        ] {
            assert!(allowed(&ok).is_ok(), "{ok:02x?}");
        }
    }

    #[test]
    fn malformed_packets_stay_in_bounds() {
        let scales = [1.0; FADERS];
        // a knob packet naming 6 knobs: only the module's 3 are read
        let mut state = State::default();
        let events = parse(Kind::Knobs, &packet(&[0x00, 6, 1, 1, 1, 1, 1, 1]), &mut state, &scales);
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|(_, p)| p["position"].as_u64().unwrap() < 3));
        // a key held, then a shorter packet: the key isn't forgotten, its release still comes
        let mut state = State::default();
        let keys = |v: &[u8]| packet_on(0x34, &[&[0x02, v.len() as u8][..], v].concat());
        assert_eq!(parse(Kind::Keys, &keys(&[0, 0, 0, 0, 1]), &mut state, &scales).len(), 1);
        assert!(parse(Kind::Keys, &keys(&[0, 0]), &mut state, &scales).is_empty());
        let up = parse(Kind::Keys, &keys(&[0, 0, 0, 0, 0]), &mut state, &scales);
        assert_eq!((up[0].0, up[0].1["position"].as_u64()), ("keyUp", Some(4)));
    }

    fn packet(payload: &[u8]) -> Vec<u8> {
        packet_on(0x2a, payload)
    }

    fn packet_on(channel: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x01, channel, payload.len() as u8, 0];
        p.extend_from_slice(payload);
        p.resize(64, 0);
        p
    }

    #[test]
    fn parses_inputs() {
        let one = [1.0; FADERS];
        let mut s = State::default();
        let ev = parse(
            Kind::Keys,
            &packet_on(0x34, &[0x02, 0x0f, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            &mut s,
            &one,
        );
        assert_eq!(ev, vec![("keyDown", json!({ "position": 1 }))]);
        let ev = parse(
            Kind::Keys,
            &packet_on(0x34, &[0x02, 0x0f, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            &mut s,
            &one,
        );
        assert_eq!(ev, vec![("keyUp", json!({ "position": 1 }))]);

        let ev = parse(Kind::Knobs, &packet(&[0x00, 0x03, 0x01, 0xff, 0x00]), &mut State::default(), &one);
        assert_eq!(
            ev,
            vec![
                ("encoderChange", json!({ "position": 0, "ticks": 1 })),
                ("encoderChange", json!({ "position": 2, "ticks": -1 })),
            ]
        );

        let ev = parse(Kind::Rollers, &packet(&[0x01, 0x02, 0x00, 0xff]), &mut State::default(), &one);
        // the same bytes on another channel (heartbeat / replies) are not events
        assert!(
            parse(Kind::Rollers, &packet_on(0x2b, &[0x01, 0x02, 0x00, 0xff]), &mut State::default(), &one).is_empty()
        );
        assert!(parse(Kind::Dial, &packet(&[0x03, 0x01, 0x00]), &mut State::default(), &one).is_empty());
        assert_eq!(ev, vec![("encoderChange", json!({ "position": 0, "ticks": 1 }))]);

        // Dial: event 3 with 0 = clockwise, event 4 = press
        let mut s = State::default();
        let ev = parse(Kind::Dial, &packet_on(0x44, &[0x03, 0x01, 0x00]), &mut s, &one);
        assert_eq!(ev, vec![("encoderChange", json!({ "position": 0, "ticks": 1 }))]);
        let ev = parse(Kind::Dial, &packet_on(0x44, &[0x04, 0x01, 0x01]), &mut s, &one);
        assert_eq!(ev, vec![("encoderDown", json!({ "position": 0 }))]);

        let mut s = State::default();
        assert!(parse(Kind::Faders, &packet(&[0x02, 0x05, 50, 0, 100, 25, 75]), &mut s, &one).is_empty());
        let ev = parse(Kind::Faders, &packet(&[0x02, 0x05, 40, 0, 100, 25, 75]), &mut s, &one);
        assert_eq!(ev, vec![("encoderChange", json!({ "position": 0, "ticks": -10, "value": 40 }))]);
    }

    #[test]
    fn fader_jitter_is_filtered() {
        let one = [1.0; FADERS];
        let mut s = State::default();
        parse(Kind::Faders, &packet(&[0x02, 0x05, 50, 1, 99, 0, 0]), &mut s, &one);
        // ±1 around a resting fader: nothing
        assert!(parse(Kind::Faders, &packet(&[0x02, 0x05, 51, 1, 99, 0, 0]), &mut s, &one).is_empty());
        assert!(parse(Kind::Faders, &packet(&[0x02, 0x05, 49, 2, 98, 0, 0]), &mut s, &one).is_empty());
        // a slow move adds up: 52 is 2 away from the 50 last taken
        let ev = parse(Kind::Faders, &packet(&[0x02, 0x05, 52, 1, 99, 0, 0]), &mut s, &one);
        assert_eq!(ev, vec![("encoderChange", json!({ "position": 0, "ticks": 2, "value": 52 }))]);
        // the ends snap: 1 → 0 and 99 → 100 counted as at the end already
        assert!(parse(Kind::Faders, &packet(&[0x02, 0x05, 52, 2, 98, 0, 0]), &mut s, &one).is_empty());
    }

    #[test]
    fn scales_faders_and_carries_remainder() {
        let scales = [0.25, 3.0, 1.0, 1.0, 1.0];
        let mut s = State::default();
        parse(Kind::Faders, &packet(&[0x02, 0x05, 50, 50, 0, 0, 0]), &mut s, &scales);
        // fader 1: -10 * 0.25 = -2.5 -> -2 now, -0.5 carried; fader 2: +2 * 3 = +6
        let ev = parse(Kind::Faders, &packet(&[0x02, 0x05, 40, 52, 0, 0, 0]), &mut s, &scales);
        assert_eq!(
            ev,
            vec![
                ("encoderChange", json!({ "position": 0, "ticks": -2, "value": 40 })),
                ("encoderChange", json!({ "position": 1, "ticks": 6, "value": 52 })),
            ]
        );
        // fader 1: -2 * 0.25 - 0.5 carried = -1
        let ev = parse(Kind::Faders, &packet(&[0x02, 0x05, 38, 52, 0, 0, 0]), &mut s, &scales);
        assert_eq!(ev, vec![("encoderChange", json!({ "position": 0, "ticks": -1, "value": 38 }))]);

        assert_eq!(read_scales(&json!({ "faderScale": [0.2, 0, "x", 500] })), [0.2, 1.0, 1.0, 1.0, 1.0]);
        assert_eq!(read_scales(&json!({})), [1.0; FADERS]);
    }

    #[test]
    fn orients_by_placement() {
        let oriented = |kind, rot, n, payload: Value| {
            let mut p = payload;
            orient(kind, rot, n, &mut p);
            p
        };
        // rollers: right wheel first -> shown second; at 180° the right wheel is on the left
        assert_eq!(
            oriented(Kind::Rollers, 0, 2, json!({ "position": 0, "ticks": 1 })),
            json!({ "position": 1, "ticks": 1 })
        );
        assert_eq!(
            oriented(Kind::Rollers, 180, 2, json!({ "position": 0, "ticks": 1 })),
            json!({ "position": 0, "ticks": 1 })
        );
        // faders at 270°: reversed order; directions are never touched (official app does the same)
        assert_eq!(
            oriented(Kind::Faders, 270, 5, json!({ "position": 1, "ticks": 4 })),
            json!({ "position": 3, "ticks": 4 })
        );
        assert_eq!(
            oriented(Kind::Knobs, 180, 3, json!({ "position": 0, "ticks": 1 })),
            json!({ "position": 2, "ticks": 1 })
        );
        assert_eq!(oriented(Kind::Knobs, 90, 3, json!({ "position": 0 })), json!({ "position": 0 }));

        // keys: corners move as expected and the mapping is a bijection both ways
        assert_eq!(key_to_display(0, 180), 14);
        assert_eq!(key_to_display(0, 90), 2); // top-left goes to the top-right of a 3-wide grid
        assert_eq!(key_to_display(4, 270), 0); // top-right goes to the top-left
        for rot in [0, 90, 180, 270] {
            for k in 0..15 {
                assert_eq!(display_to_key(key_to_display(k, rot), rot), Some(k));
            }
        }
    }

    #[test]
    fn reads_base_orientation() {
        assert_eq!(parse_orientation(&[0x01, 0x2b, 0x01, 0x00, 0x01]), Some(true));
        assert_eq!(parse_orientation(&[0x01, 0x2b, 0x01, 0x00, 0x00]), Some(false));
        assert_eq!(parse_orientation(&[0x01, 0x2a, 0x01, 0x00, 0x01]), Some(true)); // channel 0 too
        assert_eq!(parse_orientation(&[0x01, 0x2c, 0x01, 0x00, 0x01]), None); // other channels
        assert_eq!(parse_orientation(&[0x01, 0x2b]), None);
    }

    #[test]
    fn reads_output_report_length() {
        // Usage page 0xff00; report id 1: 63 bytes in; report id 2: 319 bytes out (= 320 with id)
        let desc = [
            0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01, //
            0x85, 0x01, 0x75, 0x08, 0x95, 0x3f, 0x09, 0x01, 0x81, 0x02, //
            0x85, 0x02, 0x75, 0x08, 0x96, 0x3f, 0x01, 0x09, 0x01, 0x91, 0x02, //
            0xc0,
        ];
        assert_eq!(output_report_len(&desc, 0x02), Some(320));
        assert_eq!(output_report_len(&desc, 0x03), None);
    }
}
