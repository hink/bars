//! Windows: mutes each other app's audio session on every output, the way
//! the volume mixer does. Bars' own sessions are left alone.

use std::ffi::c_void;

use windows::Win32::Media::Audio::{
    AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};
use windows::core::{Interface, PWSTR};

use crate::platform::mute::{Backend, Muted};

pub struct Sessions {
    devices: IMMDeviceEnumerator,
    /// Sessions seen since the last unmute (muted or not, they're left be).
    seen: Vec<String>,
    muted: Vec<ISimpleAudioVolume>,
}

struct Session {
    /// This session, while it lasts.
    instance: String,
    /// Its app, the same from one run to the next.
    app: String,
    pid: u32,
    volume: ISimpleAudioVolume,
}

impl Sessions {
    /// On the thread that will use it.
    pub fn new() -> Option<Sessions> {
        // SAFETY: plain COM setup on this thread (S_FALSE, already set up,
        // is fine too).
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let devices = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| log::warn!("no audio sessions to mute: {e}"))
                .ok()?;
            Some(Sessions { devices, seen: Vec::new(), muted: Vec::new() })
        }
    }

    /// Every live audio session, on every output.
    fn sessions(&self) -> windows::core::Result<Vec<Session>> {
        let mut out = Vec::new();
        // SAFETY: COM calls on interfaces this thread created.
        unsafe {
            let outputs = self.devices.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            for i in 0..outputs.GetCount()? {
                let Ok(manager) = outputs.Item(i).and_then(|d| d.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)) else {
                    continue;
                };
                let Ok(list) = manager.GetSessionEnumerator() else { continue };
                for j in 0..list.GetCount().unwrap_or(0) {
                    let Ok(control) = list.GetSession(j) else { continue };
                    if control.GetState().is_ok_and(|s| s == AudioSessionStateExpired) {
                        continue;
                    }
                    let (Ok(control), Ok(volume)) = (control.cast::<IAudioSessionControl2>(), control.cast::<ISimpleAudioVolume>()) else {
                        continue;
                    };
                    out.push(Session {
                        instance: take_string(control.GetSessionInstanceIdentifier()),
                        app: take_string(control.GetSessionIdentifier()),
                        pid: control.GetProcessId().unwrap_or(0),
                        volume,
                    });
                }
            }
        }
        Ok(out)
    }
}

impl Backend for Sessions {
    fn mute(&mut self) -> Vec<Muted> {
        let sessions = match self.sessions() {
            Ok(s) => s,
            Err(e) => {
                log::warn!("couldn't list audio sessions: {e}");
                return Vec::new();
            }
        };
        let ours = std::process::id();
        let mut new = Vec::new();
        for s in sessions {
            if s.pid == ours || (!s.instance.is_empty() && self.seen.contains(&s.instance)) {
                continue;
            }
            self.seen.push(s.instance);
            // SAFETY: COM calls on a session's volume control.
            let muted = unsafe { s.volume.GetMute().is_ok_and(|m| !m.as_bool()) && s.volume.SetMute(true, std::ptr::null()).is_ok() };
            if muted {
                new.push(Muted { id: s.app, volume: None });
                self.muted.push(s.volume);
            }
        }
        new
    }

    fn unmute(&mut self) {
        for volume in self.muted.drain(..) {
            // SAFETY: as above; a session that ended just fails.
            unsafe {
                if volume.GetMute().is_ok_and(|m| m.as_bool()) {
                    let _ = volume.SetMute(false, std::ptr::null());
                }
            }
        }
        self.seen.clear();
    }

    /// The mixer remembers an app's mute while the app isn't running, and
    /// there's no session to unmute until it is: such an app is handed
    /// back, to be tried again once it has started.
    fn recover(&mut self, muted: &[Muted]) -> Vec<Muted> {
        let Ok(sessions) = self.sessions() else { return muted.to_vec() };
        let mut waiting = Vec::new();
        for app in muted {
            let mut running = false;
            for s in sessions.iter().filter(|s| s.app == app.id) {
                running = true;
                // SAFETY: as above.
                unsafe {
                    if s.volume.GetMute().is_ok_and(|on| on.as_bool()) {
                        let _ = s.volume.SetMute(false, std::ptr::null());
                    }
                }
            }
            if !running {
                waiting.push(app.clone());
            }
        }
        waiting
    }
}

/// A string COM handed over, freed.
fn take_string(s: windows::core::Result<PWSTR>) -> String {
    match s {
        Ok(p) if !p.is_null() => {
            // SAFETY: a valid, NUL-terminated string from COM, freed once.
            unsafe {
                let text = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const c_void));
                text
            }
        }
        _ => String::new(),
    }
}
