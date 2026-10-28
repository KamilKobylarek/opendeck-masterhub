//! The settings page: a small HTTP server on localhost behind a random secret, its form and the live input page.

use super::*;

/// The placement section's live parts (the page asks for them every second): the base's turn and where it comes
/// from, the drawing, the official app's pictures, the table rows and whether those pictures were found.
pub(crate) fn placement_parts(cfg: &Config) -> Value {
    let shown: Vec<placement::Shown> = cfg
        .placed
        .values()
        .map(|(kind, name, socket)| placement::Shown { kind: *kind, label: name, socket: socket.as_deref() })
        .collect();
    let rows: String = cfg
        .placed
        .iter()
        .map(|(id, (kind, name, socket))| {
            let turn = socket
                .as_deref()
                .and_then(|s| placement::place(*kind, s))
                .map(|p| format!("{}°", (p.rotation + cfg.base_turn) % 360));
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                lights::esc(name),
                lights::esc(id),
                socket.as_deref().map_or("not found (not on the base?)".to_string(), lights::esc),
                turn.unwrap_or_else(|| "—".into())
            )
        })
        .collect();
    let source = if cfg.base_rotation.is_some() { "set by hand" } else { "from the base's sensor" };
    json!({
        "turn": format!("{}°, {source}", cfg.base_turn),
        "drawing": placement::svg(&shown, cfg.base_turn),
        "pictures": placement::pictures(&shown, cfg.base_turn),
        "rows": rows,
    })
}

/// The settings page (template filled in by serve_settings).
pub(crate) const SETTINGS_PAGE: &str = include_str!("settings.html");

/// Largest picture the settings page takes for the 15xKey background.
pub(crate) const MAX_UPLOAD: usize = 32 << 20;

/// Sends the file picked in the settings page's file dialog to the plugin, then shows the page again.
pub(crate) const UPLOAD_SCRIPT: &str = "<script>
kbgpick.addEventListener('click', () => kbgfile.click());
kbgfile.addEventListener('change', async () => {
  const file = kbgfile.files[0];
  if (!file) return;
  kbgstatus.textContent = 'Uploading…';
  try {
    const r = await fetch('background', { method: 'POST', body: file });
    const text = await r.text();
    if (r.ok) { kbg.value = text; kbgstatus.textContent = 'In use now (the rest of the form isn\\'t saved yet).'; }
    else kbgstatus.textContent = 'Not used: ' + text;
  } catch (e) {
    kbgstatus.textContent = 'Not used: the upload failed (a picture over 32 MB is refused).';
  }
  kbgfile.value = '';
});
</script>";

/// Keeps an uploaded picture as the keypad background file and returns its path. It must decode as a picture;
/// the extension comes from its content, never from the browser.
pub(crate) fn save_upload(bytes: &[u8]) -> Result<String, String> {
    save_upload_in(&data_dir(), bytes)
}

pub(crate) fn save_upload_in(dir: &std::path::Path, bytes: &[u8]) -> Result<String, String> {
    let format = image::guess_format(bytes).map_err(|_| "not a picture".to_string())?;
    let ext = format.extensions_str().first().ok_or("unknown picture format")?;
    image::load_from_memory(bytes).map_err(|e| format!("can't read it: {e}"))?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // one background at a time: drop the previous one, whatever its format
    for old in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
        if old.file_name().to_string_lossy().starts_with("keypad-background.") {
            let _ = std::fs::remove_file(old.path());
        }
    }
    let file = dir.join(format!("keypad-background.{ext}"));
    std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
    Ok(file.to_string_lossy().into_owned())
}

/// OpenDeck only tells the plugin to show its settings, so serve a tiny form on localhost and let OpenDeck open
/// it in the browser. Saving stores the values as global plugin settings.
///
/// Any web page in the browser and any local program can send requests to localhost, so every route sits behind
/// a random secret in the path. OpenDeck gets a one-time address (`Settings::open_url`, used once, for two
/// minutes), which sends the browser on to the secret one: the secret never appears on a command line, where
/// other users of the computer could read it (the browser is started with the address it opens).
pub(crate) struct Settings {
    pub(crate) address: std::net::SocketAddr,
    /// One-time addresses handed to OpenDeck, with when.
    pub(crate) launches: Arc<Mutex<Vec<(String, Instant)>>>,
}

impl Settings {
    /// A one-time address for OpenDeck to open the page with.
    pub(crate) fn open_url(&self) -> String {
        let launch = secret();
        let mut launches = self.launches.lock().unwrap();
        launches.retain(|(_, at)| at.elapsed() < LAUNCH_TIME);
        launches.push((launch.clone(), Instant::now()));
        format!("http://{}/open/{launch}", self.address)
    }
}

/// How long a one-time address works.
pub(crate) const LAUNCH_TIME: Duration = Duration::from_secs(120);
/// Requests handled at once; more wait in the system's queue.
pub(crate) const MAX_REQUESTS: usize = 16;

pub(crate) fn settings_server(shared: Shared, up_tx: Sender<Up>, uuid: String) -> (Settings, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("cannot open settings port");
    let token = Arc::new(secret());
    let address = listener.local_addr().unwrap();
    let launches: Arc<Mutex<Vec<(String, Instant)>>> = Arc::default();
    let page = format!("http://{address}/{token}/");
    let accepted = launches.clone();
    std::thread::spawn(move || {
        let busy = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for stream in listener.incoming().flatten() {
            use std::sync::atomic::Ordering;
            // a thread per request, so a slow client holds up only itself
            if busy.fetch_add(1, Ordering::SeqCst) >= MAX_REQUESTS {
                busy.fetch_sub(1, Ordering::SeqCst);
                continue; // dropped: too many at once
            }
            let (shared, up_tx, uuid, token, launches, busy) =
                (shared.clone(), up_tx.clone(), uuid.clone(), token.clone(), accepted.clone(), busy.clone());
            std::thread::spawn(move || {
                if let Err(e) = serve_settings(stream, &shared, &up_tx, &uuid, &token, &launches) {
                    eprintln!("settings page: {e}");
                }
                busy.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (Settings { address, launches }, page)
}

/// 128 random bits from the system's secure source, as hex.
pub(crate) fn secret() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("no secure random numbers");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compares a secret in constant time, so the time an answer takes tells nothing about it.
pub(crate) fn same_secret(given: &str, secret: &str) -> bool {
    given.len() == secret.len() && given.bytes().zip(secret.bytes()).fold(0, |d, (a, b)| d | (a ^ b)) == 0
}

/// Largest settings form accepted (POST).
pub(crate) const MAX_FORM: usize = 64 << 10;

/// Fills `{{NAME}}` placeholders in one pass: a value that itself contains `{{…}}` (a serial number, a command) is
/// never filled in again.
pub(crate) fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() * 2);
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}").map(|end| (&after[..end], end)) {
            Some((name, end)) if values.iter().any(|(n, _)| *n == name) => {
                out.push_str(values.iter().find(|(n, _)| *n == name).map_or("", |(_, v)| v));
                rest = &after[end + 2..];
            }
            _ => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out + rest
}

/// An HTML page with its inline scripts allowed by a fresh nonce and nothing else (Content-Security-Policy), so a
/// value that slipped into the page unescaped can't run script.
pub(crate) fn respond_html(stream: &mut TcpStream, page: &str) -> std::io::Result<()> {
    let nonce = &secret()[..22];
    let body = page.replace("<script>", &format!("<script nonce=\"{nonce}\">"));
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         X-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'nonce-{nonce}'; style-src 'unsafe-inline'; \
         img-src 'self' data:; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

pub(crate) fn serve_settings(
    mut stream: TcpStream,
    shared: &Shared,
    up_tx: &Sender<Up>,
    uuid: &str,
    token: &str,
    launches: &Mutex<Vec<(String, Instant)>>,
) -> std::io::Result<()> {
    // A local client could send slowly or endlessly: bound the whole request (not just each read) and the reply.
    let deadline = Instant::now() + Duration::from_secs(10);
    let remaining = |stream: &TcpStream| {
        stream.set_read_timeout(Some(deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1))))
    };
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let (request_line, body) = {
        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        remaining(&stream)?;
        std::io::Read::take(&mut reader, 8192).read_line(&mut line)?;
        let mut length = 0usize;
        for _ in 0..100 {
            let mut header = String::new();
            remaining(&stream)?;
            std::io::Read::take(&mut reader, 8192).read_line(&mut header)?;
            let header = header.trim_end().to_ascii_lowercase();
            if header.is_empty() {
                break;
            }
            if let Some(v) = header.strip_prefix("content-length:") {
                length = v.trim().parse().unwrap_or(0);
            }
        }
        // a body is only read behind the settings secret: the background upload up to MAX_UPLOAD, forms up to
        // MAX_FORM
        let mut body = None;
        let secret_given = line
            .split_whitespace()
            .nth(1)
            .and_then(|t| t.split(['/', '?']).nth(1))
            .is_some_and(|s| same_secret(s, token));
        let limit = if line.starts_with("POST ") && line.contains("/background ") { MAX_UPLOAD } else { MAX_FORM };
        if line.starts_with("POST ") && secret_given && length <= limit {
            let mut b = Vec::with_capacity(length);
            let mut part = std::io::Read::take(&mut reader, length as u64);
            loop {
                remaining(&stream)?;
                let mut chunk = [0u8; 16 << 10];
                let n = std::io::Read::read(&mut part, &mut chunk)?;
                if n == 0 {
                    break;
                }
                b.extend_from_slice(&chunk[..n]);
            }
            body = (b.len() == length).then_some(b);
        }
        (line, body)
    };
    let query = |q: &str| -> Vec<(String, String)> {
        q.split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (lights::url_decode(k), lights::url_decode(v)))
            .collect()
    };
    let respond = |stream: &mut TcpStream, kind: &str, body: &str| {
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
    };
    let status = |stream: &mut TcpStream, status: &str| {
        write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    };
    let error = |stream: &mut TcpStream, e: &str| {
        write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{e}", e.len())
    };
    let post = request_line.starts_with("POST ");
    let target = request_line.split_whitespace().nth(1).unwrap_or_default();

    // The one-time address OpenDeck opens: on to the page, once.
    if let Some(launch) = target.strip_prefix("/open/") {
        let mut launches = launches.lock().unwrap();
        let found = launches.iter().position(|(l, at)| same_secret(launch, l) && at.elapsed() < LAUNCH_TIME);
        return match found {
            Some(i) => {
                launches.remove(i);
                write!(stream, "HTTP/1.1 303 See Other\r\nLocation: /{token}/\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            }
            None => {
                let page = "<!doctype html><meta charset=utf-8><title>MasterHUB</title><p>This address works once: open the settings again from OpenDeck.";
                write!(stream, "HTTP/1.1 410 Gone\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len())
            }
        };
    }

    // "/<secret><path>"
    let rest = target.strip_prefix('/').unwrap_or_default();
    let secret_given = rest.split(['/', '?']).next().unwrap_or_default();
    let path = &rest[secret_given.len()..];
    let path = match path {
        _ if !same_secret(secret_given, token) => return status(&mut stream, "404 Not Found"),
        p if p.starts_with('/') => p,
        // the pages use relative links, which need the trailing slash
        _ => {
            return write!(
            stream,
            "HTTP/1.1 301 Moved Permanently\r\nLocation: /{token}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        }
    };
    // after a form, back to the page (so reloading it doesn't send the form again)
    let back = |stream: &mut TcpStream, to: &str| {
        write!(stream, "HTTP/1.1 303 See Other\r\nLocation: ./{to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    };

    // The 15xKey background picked with the browser's file dialog (it can't tell us the file's path).
    if path == "/background" {
        if !post {
            return status(&mut stream, "405 Method Not Allowed");
        }
        let saved = body
            .ok_or_else(|| format!("too big (more than {} MB) or cut short", MAX_UPLOAD >> 20))
            .and_then(|b| save_upload(&b));
        return match saved {
            Ok(file) => {
                let settings = {
                    let mut cfg = shared.write().unwrap();
                    cfg.keypad_background = Some(file.clone());
                    cfg.to_json()
                };
                let _ = up_tx.send(Up::BackgroundChanged);
                let _ =
                    up_tx.send(Up::Send(json!({ "event": "setGlobalSettings", "context": uuid, "payload": settings })));
                respond(&mut stream, "text/plain", &file)
            }
            Err(e) => error(&mut stream, &e),
        };
    }

    // The placement section, polled by the settings page so it follows modules moved on the base.
    if path == "/placement" {
        let parts = placement_parts(&shared.read().unwrap()).to_string(); // the lock goes before the reply is sent
        return respond(&mut stream, "application/json", &parts);
    }

    // Live input view (checking real hardware).
    if path == "/debug" {
        return respond_html(&mut stream, debug::PAGE);
    }
    if let Some(after) = path.strip_prefix("/debug/events?after=") {
        return respond(&mut stream, "application/json", &debug::since(after.parse().unwrap_or(0)).to_string());
    }

    // The settings form, sent with POST so the values (signal commands) stay out of the browser's history.
    if post && path == "/save" {
        let Some(body) = body else { return error(&mut stream, "the form was too big or cut short: nothing saved") };
        let params = query(&String::from_utf8_lossy(&body));
        if !params.iter().any(|(k, v)| k == "form" && v == "settings") {
            return error(&mut stream, "not the settings form: nothing saved");
        }
        // before the settings are locked: the keychain may ask the user
        let password = mqtt::password_from_form(&params);
        let (settings, store) = {
            let mut cfg = shared.write().unwrap();
            let mut values = cfg.fader_scale;
            for (k, v) in &params {
                if let (Some(i), Ok(v)) = (k.strip_prefix('f').and_then(|i| i.parse::<usize>().ok()), v.parse::<f64>())
                {
                    if i < FADERS {
                        values[i] = v;
                    }
                }
            }
            cfg.fader_scale = read_scales(&json!({ "faderScale": values })); // validates the range
            cfg.acceleration = params.iter().any(|(k, _)| k == "accel");
            cfg.placement_pictures = params.iter().any(|(k, v)| k == "placeview" && v == "pictures");
            cfg.dial_screen = params
                .iter()
                .find(|(k, _)| k == "dialscreen")
                .map(|(_, v)| v.clone())
                .filter(|v| DIAL_SCREENS.iter().any(|(k, _)| k == v));
            let functions = params
                .iter()
                .find(|(k, _)| k == "functions")
                .and_then(|(_, v)| v.parse::<u8>().ok())
                .map(|f| f.clamp(1, MAX_FUNCTIONS));
            if let Some(f) = functions.filter(|f| *f != cfg.functions) {
                cfg.functions = f;
                let _ = up_tx.send(Up::LayoutChanged);
            }
            let background =
                params.iter().find(|(k, _)| k == "kbg").map(|(_, v)| v.trim().to_string()).filter(|v| !v.is_empty());
            if background != cfg.keypad_background {
                cfg.keypad_background = background;
                let _ = up_tx.send(Up::BackgroundChanged);
            }
            let upright = params.iter().any(|(k, _)| k == "upright");
            if upright != cfg.upright {
                cfg.upright = upright;
                let _ = up_tx.send(Up::Reorient); // registering again makes OpenDeck send every picture anew
            }
            let base_rotation = params
                .iter()
                .find(|(k, _)| k == "baserot")
                .and_then(|(_, v)| v.parse::<u16>().ok())
                .filter(|r| [0, 90, 180, 270].contains(r));
            if base_rotation != cfg.base_rotation {
                cfg.base_rotation = base_rotation;
                let _ = up_tx.send(Up::Reorient);
            }
            let controls = cfg.controls.clone();
            let leds = signal_leds(&cfg);
            cfg.lights.apply_form(&controls, &leds, &params);
            cfg.notice = cfg.mqtt.apply_form(&params, password);
            (cfg.to_json(), cfg.lights.managed && params.iter().any(|(k, _)| k == "store"))
        };
        let _ = up_tx.send(Up::Send(json!({ "event": "setGlobalSettings", "context": uuid, "payload": settings })));
        let _ = up_tx.send(Up::LightsChanged(store));
        return back(&mut stream, if store { "?stored" } else { "?saved" });
    }
    if post && path == "/identify" {
        let _ = up_tx.send(Up::Identify);
        return back(&mut stream, "?identify");
    }
    let mut saved = match path {
        "/?saved" => "<p class=ok>Saved.</p>".to_string(),
        "/?stored" => "<p class=ok>Saved and stored in the modules.</p>".to_string(),
        "/?identify" => "<p class=ok>Identify mode is on for 10 seconds.</p>".to_string(),
        _ => String::new(),
    };
    // what saving didn't take, said once
    for note in std::mem::take(&mut shared.write().unwrap().notice) {
        saved += &format!("<p class=warn>{}</p>", lights::esc(&note));
    }

    // the page is built under the lock and sent after it
    let page = {
        let cfg = shared.read().unwrap();
        let checked = |on: bool| if on { " checked" } else { "" };
        let faders: String = cfg
            .fader_scale
            .iter()
            .enumerate()
            .map(|(i, v)| format!("<label>Fader {}<input name=f{i} type=number step=0.01 min=0.01 max=100 value={v} required></label>", i + 1))
            .collect();
        let detected = cfg.modules.iter().find_map(|m| m.led_modes.first().copied());
        let live = placement_parts(&cfg);
        let base = format!(
            "<label>Base rotation<select name=baserot>{}</select></label>\
             <label>Turn key and dial pictures upright when a module is turned (the official app doesn't)<input type=checkbox name=upright{}></label>",
            [None, Some(0), Some(90), Some(180), Some(270)]
                .iter()
                .map(|r| format!(
                    "<option value={}{}>{}</option>",
                    r.map_or("auto".into(), |r| r.to_string()),
                    if *r == cfg.base_rotation { " selected" } else { "" },
                    r.map_or("Auto (sensor)".into(), |r| format!("{r}°"))
                ))
                .collect::<String>(),
            checked(cfg.upright)
        );
        let knobs = format!(
            "<label>Functions per knob and the dial (1 = off; changing it registers them again)<input name=functions type=number min=1 max={MAX_FUNCTIONS} value={}></label>\
             <label>Acceleration: fast turns count up to 3× the clicks<input type=checkbox name=accel{}></label>\
             <label>The dial's screen shows (unless App volume on it picks one)<select name=dialscreen>{}</select></label>",
            cfg.functions,
            checked(cfg.acceleration),
            std::iter::once(("", "What its turn action shows"))
                .chain(DIAL_SCREENS.iter().copied())
                .map(|(k, name)| format!("<option value={k}{}>{name}</option>", if cfg.dial_screen.as_deref().unwrap_or("") == k { " selected" } else { "" }))
                .collect::<String>()
        );
        let keypad = format!(
            "<label>Picture file<input name=kbg id=kbg value=\"{}\" placeholder=\"empty: no background\"></label>\
             <p><input type=file id=kbgfile accept=\"image/*\" hidden><button type=button id=kbgpick>Choose file…</button> \
             <span id=kbgstatus class=hint>Copied to the plugin's data folder and used right away; clear the path and save to remove it.</span></p>",
            lights::esc(cfg.keypad_background.as_deref().unwrap_or_default())
        ) + UPLOAD_SCRIPT;
        let modules = device_info::html(&cfg.modules).replacen("<h2>Modules</h2>", "", 1);
        let lighting = cfg.lights.form_html(&cfg.controls, detected, &signal_leds(&cfg));
        let mqtt = cfg.mqtt.form_html(&cfg.mqtt_status);
        let size = format!("{}×{}", KEYPAD_SCREEN.0, KEYPAD_SCREEN.1);
        let text = |k: &str| live[k].as_str().unwrap_or_default().to_string();
        let (turn, drawing, pictures, rows) = (text("turn"), text("drawing"), text("pictures"), text("rows"));
        fill(
            SETTINGS_PAGE,
            &[
                ("BASE_TURN", &turn),
                ("VIEW_DRAWING", checked(!cfg.placement_pictures)),
                ("VIEW_PICTURES", checked(cfg.placement_pictures)),
                ("PLACEMENT_SVG", &drawing),
                ("PLACEMENT_PICTURES", &pictures),
                ("PLACEMENT_ROWS", &rows),
                ("MODULES", &modules),
                ("BASE", &base),
                ("KNOBS", &knobs),
                ("FADERS", &faders),
                ("LIGHTING", &lighting),
                ("MQTT", &mqtt),
                ("KEYPAD_SIZE", &size),
                ("KEYPAD", &keypad),
                ("SAVED", &saved),
            ],
        )
    };
    respond_html(&mut stream, &page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploaded_background_is_checked_and_replaces_the_old_one() {
        let dir = std::env::temp_dir().join(format!("mh-upload-{}", std::process::id()));
        let mut png = Vec::new();
        DynamicImage::from(RgbImage::new(8, 8)).write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        assert!(save_upload_in(&dir, b"<html>not a picture</html>").is_err());
        let gif = {
            let mut g = Vec::new();
            DynamicImage::from(RgbImage::new(8, 8))
                .write_to(&mut Cursor::new(&mut g), image::ImageFormat::Gif)
                .unwrap();
            g
        };
        let first = save_upload_in(&dir, &gif).unwrap();
        assert!(first.ends_with("keypad-background.gif"));
        let second = save_upload_in(&dir, &png).unwrap();
        assert!(second.ends_with("keypad-background.png"));
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1); // the GIF is gone
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The settings page with a realistic set-up, to look at it and for the documentation's screenshots
    /// (docs/guide.md): prints its address and serves it for 10 minutes.
    /// `cargo test docs_preview -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn docs_preview() {
        let icon = |name: &str| {
            let svg = std::fs::read(format!(
                "{}/pl.kobylarek.masterhub.sdPlugin/icons/{name}.svg",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap();
            format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg))
        };
        let mut cfg = Config::new();
        // the two arrangements of the unit the plugin was made on, both filling the base: knobs and the dial
        // (default) or the faders (MASTERHUB_PREVIEW=faders)
        cfg.base_turn = 90;
        let faders = std::env::var("MASTERHUB_PREVIEW").is_ok_and(|v| v == "faders");
        let modules: &[(&str, Kind, &str, &str, &str)] = if faders {
            &[
                ("mh-01fe-MHKD15AA431251600773", Kind::Keys, "15xKey", "23", "MasterHUB 15xKey"),
                ("mh-0204-MHRR02AA411251600950", Kind::Rollers, "2xRoller", "33", "MasterHUB 2xRoller"),
                ("mh-0200-MHFD05AA421251601501", Kind::Faders, "5xFader", "113", "MasterHUB 5xFader"),
            ]
        } else {
            &[
                ("mh-01fe-MHKD15AA431251600773", Kind::Keys, "15xKey", "23", "MasterHUB 15xKey"),
                ("mh-0204-MHRR02AA411251600950", Kind::Rollers, "2xRoller", "33", "MasterHUB 2xRoller"),
                ("mh-0206-MHDD01AA221252300708", Kind::Dial, "Dial", "134", "MasterHUB Dial"),
                ("mh-0202-MHKB03AA221252301133", Kind::Knobs, "3xKnob", "143", "MasterHUB 3xKnob"),
            ]
        };
        for &(id, kind, short, socket, name) in modules {
            cfg.placed.insert(id.into(), (kind, short.into(), Some(socket.into())));
            cfg.modules.push(device_info::Info {
                id: id.into(),
                name: name.into(),
                firmware: vec!["V1.0.6".into()],
                serial: Some(id[8..].into()),
                device_info: Some("01 02".into()),
                backlight: (kind == Kind::Dial).then_some(80),
                led_modes: match kind {
                    Kind::Knobs => vec![1; 3],
                    Kind::Faders => vec![1; 5],
                    _ => Vec::new(),
                },
                led_brightness: match kind {
                    Kind::Knobs => vec![100; 3],
                    Kind::Faders => vec![100; 5],
                    _ => Vec::new(),
                },
            });
        }
        cfg.modules.push(device_info::Info {
            id: BASE_ID.into(),
            name: "MasterHUB Base".into(),
            firmware: vec!["V1.0.3".into()],
            serial: Some("MHBS01AA461251601185".into()),
            led_modes: vec![1],
            led_brightness: vec![100],
            ..Default::default()
        });
        let knobs = "mh-0202-MHKB03AA221252301133";
        for (control, image) in
            if faders { Vec::new() } else { vec![(0, Some(icon("appvolume"))), (1, Some(icon("lighting"))), (2, None)] }
        {
            let hash = image.as_deref().map(lights::hash);
            cfg.controls.push(Control {
                key: format!("{knobs}/{control}"),
                label: format!("3xKnob — knob {}", control + 1),
                image,
                hash,
                auto: Some([255, 255, 255]),
                level: None,
            });
        }
        let fader_module = "mh-0200-MHFD05AA421251601501";
        for control in if faders { 0..5 } else { 0..0 } {
            let image = (control == 0).then(|| icon("appvolume"));
            let hash = image.as_deref().map(lights::hash);
            cfg.controls.push(Control {
                key: format!("{fader_module}/{control}"),
                label: format!("5xFader — fader {}", control + 1),
                image,
                hash,
                auto: Some([255, 255, 255]),
                level: (control == 0).then_some((40, false)),
            });
        }
        cfg.lights.managed = true;
        cfg.lights.faders_level = true;
        cfg.lights.levels = true;
        cfg.lights.base = Some([0x6c, 0x4c, 0xf5]);
        cfg.lights.controls.insert(format!("{knobs}/1"), [255, 200, 0]);
        cfg.lights.signals.insert(format!("{knobs}/2"), "curl -sf http://nas.local:5000".into());
        cfg.mqtt = mqtt::Settings {
            enabled: true,
            host: "homeassistant.local".into(),
            port: 8883,
            tls: true,
            ca_file: Some("/Users/me/certs/home-ca.crt".into()),
            username: "masterhub".into(),
            password_store: Some(secret::Store::Keychain),
            ..Default::default()
        };
        cfg.mqtt_status = "Connected to homeassistant.local:8883 (TLS).".into();
        cfg.keypad_background =
            Some("/Users/me/Library/Application Support/opendeck-masterhub/keypad-background.gif".into());
        cfg.functions = 2;
        cfg.acceleration = true;
        cfg.dial_screen = Some("cpu".into());
        // a few input packets for the live input view
        for (device, kind, raw, events) in [
            (
                "mh-01fe-MHKD15AA431251600773",
                "Keys",
                vec![0x01, 0x34, 0x02, 0x00, 0x01, 0x07],
                vec![json!({ "event": "keyDown", "module": 7, "opendeck": 11 })],
            ),
            (
                "mh-0202-MHKB03AA221252301133",
                "Knobs",
                vec![0x01, 0x2a, 0x03, 0x00, 0x05, 0x02, 0x01],
                vec![json!({ "event": "encoderChange", "module": 2, "opendeck": 2, "ticks": 1 })],
            ),
            (
                "mh-0200-MHFD05AA421251601501",
                "Faders",
                vec![0x01, 0x2a, 0x07, 0x00, 0x02, 0x05, 40, 0, 100, 25, 75],
                vec![json!({ "event": "encoderChange", "module": 0, "opendeck": 0, "ticks": -10, "value": 40 })],
            ),
            (
                "mh-0204-MHRR02AA411251600950",
                "Rollers",
                vec![0x01, 0x2a, 0x03, 0x00, 0x07, 0x00, 0xfd],
                vec![json!({ "event": "encoderChange", "module": 0, "opendeck": 1, "ticks": -3 })],
            ),
            (
                "mh-0206-MHDD01AA221252300708",
                "Dial",
                vec![0x01, 0x44, 0x02, 0x00, 0x03, 0x01],
                vec![json!({ "event": "encoderDown", "module": 0, "opendeck": 0 })],
            ),
        ] {
            if cfg.placed.contains_key(device) {
                debug::record(device, kind, &raw, events);
            }
        }
        let (up_tx, _up_rx) = mpsc::channel();
        println!("{}", settings_server(Arc::new(RwLock::new(cfg)), up_tx, "uuid".into()).1);
        std::thread::sleep(Duration::from_secs(600));
    }

    #[test]
    fn upload_route_needs_the_secret_and_a_picture() {
        use std::io::Read;
        let (up_tx, _up_rx) = mpsc::channel();
        let url = settings_server(Arc::new(RwLock::new(Config::new())), up_tx, "uuid".into()).1;
        let (host, token) = url.trim_start_matches("http://").trim_end_matches('/').split_once('/').unwrap();
        let post = |path: &str, body: &[u8]| {
            let mut s = TcpStream::connect(host).unwrap();
            write!(s, "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
            s.write_all(body).unwrap();
            let mut reply = String::new();
            s.read_to_string(&mut reply).unwrap();
            reply
        };
        assert!(post("/background", b"").starts_with("HTTP/1.1 404")); // no body: unread without the secret, the socket may be reset
        let reply = post(&format!("/{token}/background"), b"not a picture");
        assert!(reply.starts_with("HTTP/1.1 400") && reply.ends_with("not a picture"), "{reply}");
    }

    /// One request to the settings server; returns the whole reply.
    fn request(host: &str, head: &str, body: &str) -> String {
        use std::io::Read;
        let mut s = TcpStream::connect(host).unwrap();
        write!(s, "{head} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut reply = String::new();
        s.read_to_string(&mut reply).unwrap();
        reply
    }

    #[test]
    fn only_the_secret_opens_the_settings() {
        let (up_tx, _up_rx) = mpsc::channel();
        let (launcher, settings) = settings_server(Arc::new(RwLock::new(Config::new())), up_tx, "uuid".into());
        let (host, path) = settings.trim_start_matches("http://").split_once('/').unwrap();
        let secret = path.trim_end_matches('/');
        // OpenDeck's one-time address leads to the page once, then never again
        let open = launcher.open_url();
        let open = open.trim_start_matches(&format!("http://{host}"));
        let first = request(host, &format!("GET {open}"), "");
        assert!(first.starts_with("HTTP/1.1 303") && first.contains(&format!("Location: /{secret}/")), "{first}");
        assert!(request(host, &format!("GET {open}"), "").starts_with("HTTP/1.1 410"));
        assert!(request(host, "GET /open/0123456789abcdef0123456789abcdef", "").starts_with("HTTP/1.1 410"));
        // the page allows only its own scripts
        let page = request(host, &format!("GET /{secret}/"), "");
        assert!(
            page.contains("Content-Security-Policy: default-src 'none'; script-src 'nonce-")
                && page.contains("<script nonce=\"")
        );
        assert!(request(host, &format!("GET /{secret}/"), "").contains("<form"));
        assert!(request(host, &format!("GET /{secret}"), "").starts_with("HTTP/1.1 301"));
        let wrong = format!("{}{}", &secret[..31], if secret.ends_with('0') { '1' } else { '0' });
        assert!(request(host, &format!("GET /{wrong}/"), "").starts_with("HTTP/1.1 404"));
        assert!(request(host, &format!("GET /{}/", &secret[..31]), "").starts_with("HTTP/1.1 404"));
        assert!(request(host, "GET /", "").starts_with("HTTP/1.1 404"));
        // the local signal address of an earlier version is gone
        assert!(request(host, "GET /signal?led=K/0&colour=red", "").starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn settings_form_is_posted() {
        let shared = Arc::new(RwLock::new(Config::new()));
        let (up_tx, up_rx) = mpsc::channel();
        let settings = settings_server(shared.clone(), up_tx, "uuid".into()).1;
        let (host, path) = settings.trim_start_matches("http://").split_once('/').unwrap();
        // anything but the whole settings form saves nothing
        assert!(request(host, &format!("POST /{path}save"), "accel=on").starts_with("HTTP/1.1 400"));
        assert!(!shared.read().unwrap().acceleration);
        let reply = request(host, &format!("POST /{path}save"), "form=settings&accel=on&levels=on");
        assert!(reply.starts_with("HTTP/1.1 303") && reply.contains("Location: ./?saved"), "{reply}");
        assert!(shared.read().unwrap().acceleration);
        assert!(up_rx.try_iter().any(|u| matches!(u, Up::LightsChanged(false))));
        assert!(request(host, &format!("GET /{path}?saved"), "").contains("<p class=ok>Saved.</p>"));
        // a GET no longer saves
        request(host, &format!("GET /{path}save?accel="), "");
        assert!(shared.read().unwrap().acceleration);
    }

    #[test]
    fn settings_secret_is_random() {
        let (a, b) = (secret(), secret());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(
            same_secret(&a, &a.clone()) && !same_secret(&a, &b) && !same_secret(&a, &a[..31]) && !same_secret("", &a)
        );
    }
}
