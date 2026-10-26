//! FineTune (macOS per-app volume, github.com/ronitsingh10/FineTune) through its finetune:// URLs
//! (guide/url-schemes.md). FineTune can't be asked for a volume, so levels are read from the settings it saves
//! (`saved_state`); step-volume always moves by 5% (no step parameter).

use crate::audio::{Op, State};

/// URLs that apply `op` to `apps` (bundle ids, already checked to be plain ids).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn urls(apps: &[String], op: Op) -> Vec<String> {
    let all = |f: &dyn Fn(&str) -> String| apps.iter().map(|a| f(a)).collect::<Vec<_>>().join("&");
    match op {
        Op::Set(level) => {
            // set-volumes takes linear gain in %, FineTune's own slider is gain = position² (VolumeMapping):
            // square it, so a fader at 50% shows 50% in FineTune, not 71%.
            let l = level.clamp(0.0, 1.0);
            let gain = (l * l * 100.0).round() as u32;
            vec![format!("finetune://set-volumes?{}", all(&|a| format!("app={a}&volume={gain}")))]
        }
        Op::Step(delta) => {
            // one 5% step per URL, one app per URL
            let direction = if delta > 0.0 { "up" } else { "down" };
            // at most 20 URLs in all: each is an `open` of its own
            let steps = ((delta.abs() / 0.05).round() as usize).clamp(1, (20 / apps.len().max(1)).max(1));
            apps.iter()
                .flat_map(|a| {
                    std::iter::repeat_n(format!("finetune://step-volume?app={a}&direction={direction}"), steps)
                })
                .collect()
        }
        Op::ToggleMute => vec![format!("finetune://toggle-mute?{}", all(&|a| format!("app={a}")))],
    }
}

/// An app's state in FineTune's settings.json: `appVolumes` holds the gain (FineTune's slider is gain = position²,
/// so the level shown is its square root; 100% when the app has none yet), `appMutes` the mute.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn saved_state(json: &str, app: &str) -> Option<State> {
    let settings: serde_json::Value = serde_json::from_str(json).ok()?;
    let gain = settings["appVolumes"][app].as_f64().unwrap_or(1.0).clamp(0.0, 1.0);
    Some(State { level: gain.sqrt() as f32, muted: settings["appMutes"][app].as_bool().unwrap_or(false) })
}

/// Opens a FineTune URL in the background (FineTune stays hidden), through Launch Services in the plugin's own
/// process: ~10 ms, where starting `open -g` for each change took 50–80 ms. `open -g` is the fallback.
#[cfg(target_os = "macos")]
pub fn open(url: &str) {
    let status = launch::open_in_background(url);
    if status != 0 {
        eprintln!("FineTune: Launch Services error {status} for {url}, trying open -g");
        if let Err(e) = std::process::Command::new("open").args(["-g", url]).status() {
            eprintln!("FineTune: cannot open {url}: {e}");
        }
    }
}

#[cfg(target_os = "macos")]
mod launch {
    use std::ffi::c_void;

    type CfRef = *const c_void;

    /// LSLaunchURLSpec (CoreServices, LSOpen.h).
    #[repr(C)]
    struct LaunchUrlSpec {
        app_url: CfRef,
        item_urls: CfRef,
        pass_thru_params: *const c_void,
        launch_flags: u32,
        async_ref_con: *mut c_void,
    }

    const UTF8: u32 = 0x0800_0100;
    /// kLSLaunchDefaults | kLSLaunchDontSwitch: the app handling the URL isn't brought to the front.
    const IN_BACKGROUND: u32 = 0x1 | 0x200;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFURLCreateWithBytes(alloc: CfRef, bytes: *const u8, len: isize, encoding: u32, base: CfRef) -> CfRef;
        fn CFArrayCreate(alloc: CfRef, values: *const CfRef, count: isize, callbacks: *const c_void) -> CfRef;
        fn CFRelease(cf: CfRef);
        static kCFTypeArrayCallBacks: c_void;
    }

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn LSOpenFromURLSpec(spec: *const LaunchUrlSpec, launched: *mut CfRef) -> i32;
    }

    /// 0 when the URL went to the app that handles it, else the OSStatus.
    pub fn open_in_background(url: &str) -> i32 {
        unsafe {
            let url = CFURLCreateWithBytes(std::ptr::null(), url.as_ptr(), url.len() as isize, UTF8, std::ptr::null());
            if url.is_null() {
                return -50; // paramErr
            }
            let items = CFArrayCreate(std::ptr::null(), &url, 1, &raw const kCFTypeArrayCallBacks);
            let spec = LaunchUrlSpec {
                app_url: std::ptr::null(),
                item_urls: items,
                pass_thru_params: std::ptr::null(),
                launch_flags: IN_BACKGROUND,
                async_ref_con: std::ptr::null_mut(),
            };
            let status = LSOpenFromURLSpec(&spec, std::ptr::null_mut());
            CFRelease(items);
            CFRelease(url);
            status
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_per_operation() {
        let apps = vec!["com.a".to_string(), "com.b".to_string()];
        // fader at 50% -> gain 25% = FineTune's own slider at 50%
        assert_eq!(urls(&apps, Op::Set(0.5)), ["finetune://set-volumes?app=com.a&volume=25&app=com.b&volume=25"]);
        assert!(urls(&apps, Op::Set(1.0))[0].ends_with("volume=100"));
        assert!(urls(&apps, Op::Set(0.0))[0].ends_with("volume=0"));
        // two 5% steps down, per app
        let steps = urls(&apps, Op::Step(-0.1));
        assert_eq!(steps.len(), 4);
        assert!(steps.iter().all(|u| u.ends_with("direction=down")));
        assert_eq!(urls(&apps, Op::ToggleMute), ["finetune://toggle-mute?app=com.a&app=com.b"]);
    }

    /// A URL for an app FineTune doesn't know reaches FineTune and changes nothing:
    /// `cargo test opens_in_background -- --ignored` (needs FineTune).
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn opens_in_background() {
        let started = std::time::Instant::now();
        assert_eq!(launch::open_in_background("finetune://step-volume?app=com.example.none&direction=up"), 0);
        println!("{:?}", started.elapsed());
    }

    #[test]
    fn reads_saved_state() {
        let json = r#"{"appVolumes": {"com.spotify.client": 0.5625}, "appMutes": {"com.spotify.client": true}}"#;
        assert_eq!(saved_state(json, "com.spotify.client"), Some(State { level: 0.75, muted: true }));
        assert_eq!(saved_state(json, "com.apple.Music"), Some(State { level: 1.0, muted: false })); // not set yet
        assert_eq!(saved_state("nope", "x"), None);
    }
}
