//! App volume on Windows: the Core Audio sessions of the default output device whose process is one of the
//! apps. Every process with the app's file name counts, so a browser's helper processes are included. COM is
//! set up for each call on the calling thread (multithreaded; the plugin's threads use COM for nothing else).

use super::{normalize, Op, State};
use windows::core::{Interface, Result, PWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, ISimpleAudioVolume,
    MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

pub fn run(apps: &[String], op: Op) -> Option<State> {
    let apps: Vec<String> = apps.iter().map(|a| normalize(a)).collect();
    let _com = Com::init().map_err(|e| eprintln!("Windows audio: {e}")).ok()?;
    // declared after `_com`, so released before COM is
    let volumes = matching(&apps).map_err(|e| eprintln!("Windows audio sessions: {e}")).ok()?;
    let mut first = None;
    for volume in &volumes {
        match change(volume, op) {
            Ok(state) => {
                first.get_or_insert(state);
            }
            Err(e) => eprintln!("Windows audio: {e}"),
        }
    }
    first
}

/// COM on the calling thread while this lives.
struct Com;

impl Com {
    fn init() -> Result<Com> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Com)
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

/// The volume controls of the default output device's sessions that belong to `apps` (normalized names).
fn matching(apps: &[String]) -> Result<Vec<ISimpleAudioVolume>> {
    unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let manager: IAudioSessionManager2 =
            devices.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None)?;
        let sessions = manager.GetSessionEnumerator()?;
        let mut out = Vec::new();
        for i in 0..sessions.GetCount()? {
            let Ok(session) = sessions.GetSession(i) else { continue };
            let pid = session.cast::<IAudioSessionControl2>().and_then(|s| s.GetProcessId()).unwrap_or(0);
            if pid != 0 && exe_name(pid).is_some_and(|name| apps.contains(&name)) {
                out.extend(session.cast::<ISimpleAudioVolume>().ok());
            }
        }
        Ok(out)
    }
}

/// Applies `op` to one session and reads it back. A step of 0 only reads (that's how `audio::read` asks).
fn change(volume: &ISimpleAudioVolume, op: Op) -> Result<State> {
    unsafe {
        let (level, muted) = (volume.GetMasterVolume()?, volume.GetMute()?.as_bool());
        match op {
            Op::Set(to) => volume.SetMasterVolume(to.clamp(0.0, 1.0), std::ptr::null())?,
            Op::Step(by) if by != 0.0 => volume.SetMasterVolume((level + by).clamp(0.0, 1.0), std::ptr::null())?,
            Op::Step(_) => return Ok(State { level, muted }),
            Op::ToggleMute => volume.SetMute(!muted, std::ptr::null())?,
        }
        Ok(State { level: volume.GetMasterVolume()?, muted: volume.GetMute()?.as_bool() })
    }
}

/// A process's file name, normalized ("C:\…\Spotify.exe" → "spotify").
fn exe_name(pid: u32) -> Option<String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut path = [0u16; 1024];
        let mut len = path.len() as u32;
        let found = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut len);
        let _ = CloseHandle(process);
        found.ok()?;
        Some(normalize(String::from_utf16_lossy(&path[..len as usize]).rsplit('\\').next()?))
    }
}
