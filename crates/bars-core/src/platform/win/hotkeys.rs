//! Global shortcut via a low-level keyboard hook (WH_KEYBOARD_LL), and a
//! low-level mouse hook (WH_MOUSE_LL) while a shortcut is a mouse button or
//! one is being recorded. Keys and buttons are observed, and a few
//! swallowed: Escape while recording (it cancels the dictation instead of
//! also reaching the app), the key or button that finishes a shortcut, and
//! keys and buttons pressed while a new shortcut is being recorded.
//! Modifiers, and left and right clicks, always pass. No permission is
//! needed.

use std::cell::RefCell;
use std::sync::Arc;
use std::thread;

use crossbeam_channel::Sender;
use parking_lot::Mutex;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, PostThreadMessageW, SetTimer, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
    WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1,
};

use super::keycodes::{VK_MBUTTON, VK_UNASSIGNED, VK_XBUTTON1, VK_XBUTTON2, WinKeys};
use crate::hotkey::{
    self, Chord, ChordTracker, FilterInput, HeldKeys, HotkeyEvent, KeyChange, KeyFilter, Mods, STALE_KEY_AGE,
    STALE_KEY_SWEEP,
};

/// Asks the hook thread to install or remove the mouse hook to match
/// what's wanted.
const WM_SYNC_MOUSE: u32 = WM_APP + 1;

/// Marks the input Bars sends itself (the paste keystroke, typed text, the
/// key that keeps menus shut), which the hooks ignore. enigo tags its
/// events with the same value. Other programs' injected input counts as
/// the keyboard's: a remapped key or a macro can be a shortcut too.
const OWN_INPUT: usize = enigo::EVENT_MARKER as usize;

#[derive(Default)]
struct Shared {
    chords: Vec<Chord>,
    capturing: bool,
    recording: bool,
}

struct HookState {
    shared: Arc<Mutex<Shared>>,
    events: Sender<HotkeyEvent>,
    held: HeldKeys,
    tracker: ChordTracker,
    filter: KeyFilter,
}

thread_local! {
    // The hook procedures run on the thread that installed them.
    static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
    /// The mouse hook, while it's installed.
    static MOUSE_HOOK: RefCell<Option<HHOOK>> = const { RefCell::new(None) };
}

pub struct WinHotkeys {
    shared: Arc<Mutex<Shared>>,
    /// The hook thread, which installs and removes the mouse hook.
    thread: u32,
}

impl WinHotkeys {
    pub fn spawn(chords: Vec<Chord>, events: Sender<HotkeyEvent>) -> anyhow::Result<Self> {
        let shared = Arc::new(Mutex::new(Shared {
            chords,
            ..Default::default()
        }));
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<u32, String>>(1);
        let s = shared.clone();
        thread::Builder::new()
            .name("bars-hotkeys".into())
            .spawn(move || {
                STATE.with(|st| {
                    *st.borrow_mut() = Some(HookState {
                        shared: s,
                        events,
                        held: HeldKeys::default(),
                        tracker: ChordTracker::default(),
                        filter: KeyFilter::default(),
                    })
                });
                let hook: HHOOK = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) } {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("keyboard hook: {e}")));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));
                let _ = hook;
                sync_mouse_hook();
                // Checks for keys whose release was missed.
                unsafe { SetTimer(None, 0, STALE_KEY_SWEEP.as_millis() as u32, None) };
                // Low-level hooks are delivered through this thread's queue.
                let mut msg = MSG::default();
                unsafe {
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        if msg.message == WM_TIMER {
                            sweep();
                            continue;
                        }
                        if msg.message == WM_SYNC_MOUSE {
                            sync_mouse_hook();
                            continue;
                        }
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            })?;
        let thread = ready_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("hotkey thread exited"))?
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(WinHotkeys { shared, thread })
    }

    pub fn set_chords(&self, chords: Vec<Chord>) {
        self.shared.lock().chords = chords;
        self.sync_mouse();
    }

    pub fn set_capture(&self, on: bool) {
        self.shared.lock().capturing = on;
        self.sync_mouse();
    }

    /// The mouse hook sees every move of the mouse, so it's only there
    /// while a shortcut is a mouse button (or one is being recorded).
    fn sync_mouse(&self) {
        let _ = unsafe { PostThreadMessageW(self.thread, WM_SYNC_MOUSE, WPARAM(0), LPARAM(0)) };
    }

    pub fn set_recording(&self, on: bool) {
        self.shared.lock().recording = on;
    }

    /// Modifiers held right now, as Windows reports them.
    pub fn mods_probe(&self) -> Arc<dyn Fn() -> Mods + Send + Sync> {
        Arc::new(|| {
            [
                (0x10, Mods::SHIFT),
                (0x11, Mods::CTRL),
                (0x12, Mods::ALT),
                (0x5B, Mods::SUPER),
                (0x5C, Mods::SUPER),
            ]
            .into_iter()
            .filter(|(vk, _)| key_is_down(*vk))
            .fold(Mods::empty(), |a, (_, m)| a | m)
        })
    }
}

fn key_is_down(vk: u32) -> bool {
    (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        // Ignore our own synthetic input (paste chords, typed text).
        let own = (kb.flags.0 & LLKHF_INJECTED.0) != 0 && kb.dwExtraInfo == OWN_INPUT;
        if !own {
            let msg = wparam.0 as u32;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            if (down || up) && on_key(kb.vkCode, down) {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let msg = wparam.0 as u32;
        // Moves and clicks pass at once; only middle and side buttons count.
        let down = matches!(msg, WM_MBUTTONDOWN | WM_XBUTTONDOWN);
        if down || matches!(msg, WM_MBUTTONUP | WM_XBUTTONUP) {
            let ms = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            let vk = match msg {
                WM_MBUTTONDOWN | WM_MBUTTONUP => VK_MBUTTON,
                _ if (ms.mouseData >> 16) as u16 == XBUTTON1 => VK_XBUTTON1,
                _ => VK_XBUTTON2,
            };
            let own = (ms.flags & LLMHF_INJECTED) != 0 && ms.dwExtraInfo == OWN_INPUT;
            if !own && on_key(vk, down) {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Installs or removes the mouse hook (on the hook thread).
fn sync_mouse_hook() {
    let wanted = STATE.with(|st| {
        st.borrow().as_ref().is_some_and(|s| {
            let sh = s.shared.lock();
            hotkey::wants_mouse(&sh.chords, sh.capturing)
        })
    });
    MOUSE_HOOK.with(|h| {
        let mut hook = h.borrow_mut();
        match (wanted, *hook) {
            (true, None) => match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0) } {
                Ok(new) => *hook = Some(new),
                Err(e) => log::warn!("mouse hook: {e}"),
            },
            (false, Some(old)) => {
                let _ = unsafe { UnhookWindowsHookEx(old) };
                *hook = None;
            }
            _ => {}
        }
    });
}

/// Returns whether to swallow the key (or mouse button).
fn on_key(vk: u32, down: bool) -> bool {
    STATE.with(|st| {
        let mut guard = st.borrow_mut();
        let Some(state) = guard.as_mut() else { return false };
        let (chords, capturing, recording) = {
            let s = state.shared.lock();
            (s.chords.clone(), s.capturing, s.recording)
        };
        // A button no shortcut uses passes untouched (unless its press was
        // swallowed, before the shortcuts changed).
        if !hotkey::counts(&WinKeys, vk, &chords, capturing) && !state.held.contains(vk) {
            return false;
        }
        // Autorepeat sends repeated key-downs; only the first counts.
        let change = if !down {
            state.held.release(vk);
            KeyChange::Release
        } else if state.held.press(vk) {
            KeyChange::Press
        } else {
            KeyChange::Repeat
        };
        let completed = match change {
            KeyChange::Repeat => false,
            KeyChange::Press => {
                // Before the shortcut can fire: a key that has looked held
                // a while may have been let go unseen (e.g. on the secure
                // desktop of a UAC prompt).
                let HookState { held, filter, .. } = &mut *state;
                held.drop_stale(STALE_KEY_AGE, |k| filter.is_dropped(k) || key_is_down(k));
                state.track(Some(vk), &chords, capturing)
            }
            KeyChange::Release => state.track(None, &chords, capturing),
        };
        let input = FilterInput {
            chords: &chords,
            completed_chord: completed,
            recording,
            capturing,
        };
        state.filter.should_drop(&WinKeys, vk, change, &input)
    })
}

/// Every few seconds: forget keys Windows says are up, so a shortcut whose
/// release was missed still ends.
fn sweep() {
    STATE.with(|st| {
        let mut guard = st.borrow_mut();
        let Some(state) = guard.as_mut() else { return };
        let HookState { held, filter, .. } = &mut *state;
        if held.drop_stale(STALE_KEY_AGE, |k| filter.is_dropped(k) || key_is_down(k)).is_empty() {
            return;
        }
        let (chords, capturing) = {
            let s = state.shared.lock();
            (s.chords.clone(), s.capturing)
        };
        state.track(None, &chords, capturing);
    });
}

impl HookState {
    /// Runs the tracker; returns whether this press completed a shortcut.
    fn track(&mut self, newly: Option<u32>, chords: &[Chord], capturing: bool) -> bool {
        let tracked = self.tracker.update(&WinKeys, &self.held.keys, newly, chords, capturing);
        if tracked.events.iter().any(|e| matches!(e, HotkeyEvent::Captured(_))) {
            self.shared.lock().capturing = false;
            // Recording ended here, not through `set_capture`.
            sync_mouse_hook_later();
        }
        let started = tracked.completed || tracked.events.contains(&HotkeyEvent::Down);
        if started && tracked.mods.intersects(Mods::SUPER | Mods::ALT) {
            suppress_menus();
        }
        for ev in tracked.events {
            let _ = self.events.send(ev);
        }
        tracked.completed
    }
}

/// From inside a hook procedure: the mouse hook can't be changed there.
fn sync_mouse_hook_later() {
    let _ = unsafe { PostThreadMessageW(GetCurrentThreadId(), WM_SYNC_MOUSE, WPARAM(0), LPARAM(0)) };
}

/// Releasing the Windows key after using it alone opens the Start menu,
/// and Alt the focused app's menu bar. A tap of an unassigned key in
/// between tells them it was a combination.
fn suppress_menus() {
    // Not from inside the hook callback: do it off-thread.
    thread::spawn(|| {
        let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(VK_UNASSIGNED),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: OWN_INPUT,
                },
            },
        };
        let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
        unsafe {
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    });
}
