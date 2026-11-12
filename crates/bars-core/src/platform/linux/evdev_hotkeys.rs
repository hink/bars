//! Global hotkeys by reading keyboards directly (evdev), and mice while a
//! shortcut is a mouse button (or one is being recorded).
//!
//! Works on every compositor (KDE, GNOME, wlroots, X11) and supports
//! modifier-only chords such as Ctrl+Super. Keys and buttons are observed,
//! so other apps keep receiving them (a mouse button shortcut too). Requires
//! read access to `/dev/input/event*` (see `permissions`).
//!
//! The one exception: during a hands-free dictation the keyboards are
//! held (grabbed) and every key is passed on through a virtual keyboard,
//! except Escape, which cancels the dictation instead of also reaching the
//! app. Keyboards are only taken and let go while no key is down on them,
//! so no key can end up stuck.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use evdev::{Device, EventSummary, KeyCode};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use parking_lot::Mutex;

use super::keycodes::{key_from_name, modifier_of, name_from_key};
use super::VIRTUAL_KEYBOARD_NAME;
use super::inject::VirtualKeyboard;
use crate::hotkey::{
    self, Chord, ChordTracker, FilterInput, HotkeyEvent, Key, KeyChange, KeyFilter, KeyMap, Mods, STALE_KEY_SWEEP,
};

#[derive(Default)]
struct Shared {
    chords: Vec<Chord>,
    capturing: bool,
    recording: bool,
    /// A hands-free dictation: the shortcut is up, so keyboards can be held.
    hands_free: bool,
    held_mods: Mods,
    stop: bool,
}

/// A virtual keyboard needs this long before the compositor listens to it.
const FORWARD_SETTLE: Duration = Duration::from_millis(500);
/// Keyboards still held this long after a dictation (a key that looks
/// down) are let go anyway, releasing that key in apps first.
const RELEASE_GRACE: Duration = Duration::from_secs(2);

/// Handle to the listener thread.
pub struct EvdevHotkeys {
    shared: Arc<Mutex<Shared>>,
}

impl EvdevHotkeys {
    pub fn spawn(chords: Vec<Chord>, events: Sender<HotkeyEvent>) -> anyhow::Result<Self> {
        // Fail fast (so the caller can fall back) if no keyboard is readable.
        let initial = open_devices(&HashMap::new(), false);
        if initial.is_empty() {
            anyhow::bail!("no readable keyboards in /dev/input");
        }
        let shared = Arc::new(Mutex::new(Shared {
            chords,
            ..Default::default()
        }));
        let s = shared.clone();
        thread::Builder::new()
            .name("bars-hotkeys".into())
            .spawn(move || Listener::new(s, events, initial).run())?;
        Ok(EvdevHotkeys { shared })
    }

    pub fn set_chords(&self, chords: Vec<Chord>) {
        self.shared.lock().chords = chords;
    }

    pub fn set_capture(&self, on: bool) {
        self.shared.lock().capturing = on;
    }

    pub fn set_recording(&self, on: bool, hands_free: bool) {
        let mut s = self.shared.lock();
        s.recording = on;
        s.hands_free = on && hands_free;
    }

    pub fn held_mods(&self) -> Mods {
        self.shared.lock().held_mods
    }

    pub fn mods_probe(&self) -> Arc<dyn Fn() -> Mods + Send + Sync> {
        let shared = self.shared.clone();
        Arc::new(move || shared.lock().held_mods)
    }
}

impl Drop for EvdevHotkeys {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
    }
}

fn is_keyboard(dev: &Device) -> bool {
    if dev.name() == Some(VIRTUAL_KEYBOARD_NAME) {
        return false;
    }
    dev.supported_keys().is_some_and(|keys| {
        keys.contains(KeyCode::KEY_LEFTCTRL)
            || keys.contains(KeyCode::KEY_LEFTMETA)
            || (keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_Z))
    })
}

/// Mouse buttons beyond left and right, which shortcuts can use.
const EXTRA_BUTTONS: [KeyCode; 6] =
    [KeyCode::BTN_MIDDLE, KeyCode::BTN_SIDE, KeyCode::BTN_EXTRA, KeyCode::BTN_FORWARD, KeyCode::BTN_BACK, KeyCode::BTN_TASK];
/// Mouse buttons are evdev's codes from BTN_LEFT: button 1 is left.
const BTN_FIRST: u32 = 0x110;
const BTN_LAST: u32 = 0x11f;

fn is_mouse(dev: &Device) -> bool {
    dev.name() != Some(VIRTUAL_KEYBOARD_NAME)
        && dev.supported_keys().is_some_and(|keys| EXTRA_BUTTONS.iter().any(|b| keys.contains(*b)))
}

/// Keys and mouse buttons count; a touchpad's touches, a tablet's pen and
/// joystick buttons don't.
fn is_tracked(code: KeyCode) -> bool {
    let c = code.code() as u32;
    c < 0x100 || (BTN_FIRST..=BTN_LAST).contains(&c) || c >= 0x160
}

#[repr(C)]
struct InputMask {
    kind: u32,
    codes_size: u32,
    codes_ptr: u64,
}
nix::ioctl_write_ptr!(eviocsmask, b'E', 0x93, InputMask);

/// Asks the kernel not to pass this device's movement on to us: a mouse
/// would wake Bars hundreds of times a second while it moves, and only its
/// buttons matter here. Other programs still get everything.
fn ignore_motion(dev: &Device) {
    const EV_REL: u32 = 0x02;
    const EV_ABS: u32 = 0x03;
    let none = [0u8; 8];
    for kind in [EV_REL, EV_ABS] {
        let mask = InputMask { kind, codes_size: none.len() as u32, codes_ptr: none.as_ptr() as u64 };
        if let Err(e) = unsafe { eviocsmask(dev.as_fd().as_raw_fd(), &mask) } {
            log::debug!("hotkeys: can't mask movement on {}: {e}", dev.name().unwrap_or("?"));
        }
    }
}

/// Opens readable keyboards not open yet, and with `mice` mice too.
fn open_devices(existing: &HashMap<PathBuf, Device>, mice: bool) -> HashMap<PathBuf, Device> {
    let mut out = HashMap::new();
    let Ok(dir) = std::fs::read_dir("/dev/input") else {
        return out;
    };
    for entry in dir.flatten() {
        let path = entry.path();
        let is_event = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("event"));
        if !is_event || existing.contains_key(&path) {
            continue;
        }
        if let Ok(dev) = Device::open(&path)
            && (is_keyboard(&dev) || (mice && is_mouse(&dev))) && dev.set_nonblocking(true).is_ok()
        {
            log::debug!("hotkeys: watching {} ({})", path.display(), dev.name().unwrap_or("?"));
            ignore_motion(&dev);
            out.insert(path, dev);
        }
    }
    out
}

/// Only plain keyboards are held during dictation: a device that also
/// moves a pointer would freeze it.
fn holdable(dev: &Device) -> bool {
    dev.supported_relative_axes().is_none() && dev.supported_absolute_axes().is_none()
}

/// Can we read at least one keyboard?
pub fn can_read_keyboards() -> bool {
    !open_devices(&HashMap::new(), false).is_empty()
}

/// Whether mice can be read, for mouse-button shortcuts: false when a mouse
/// with buttons beyond left and right is there but can't be opened (an
/// access rule from before mouse shortcuts), None when there's no such
/// mouse. What a device has is read from sysfs, which anyone can.
pub fn mouse_access() -> Option<bool> {
    let mut unreadable = false;
    for entry in std::fs::read_dir("/dev/input").ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("event") {
            continue;
        }
        match Device::open(entry.path()) {
            Ok(dev) if is_mouse(&dev) => return Some(true),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => unreadable |= has_extra_buttons(&name),
            Err(_) => {}
        }
    }
    unreadable.then_some(false)
}

/// From sysfs: the device has mouse buttons beyond left and right.
fn has_extra_buttons(event: &str) -> bool {
    let Ok(caps) = std::fs::read_to_string(format!("/sys/class/input/{event}/device/capabilities/key")) else {
        return false;
    };
    // Hex words, most significant first.
    let words: Vec<u64> = caps.split_whitespace().rev().filter_map(|w| u64::from_str_radix(w, 16).ok()).collect();
    EXTRA_BUTTONS.iter().any(|b| {
        let n = b.code() as usize;
        words.get(n / 64).is_some_and(|w| ((w >> (n % 64)) & 1) == 1)
    })
}

struct EvdevKeys;

impl KeyMap for EvdevKeys {
    fn modifier_of(&self, code: u32) -> Option<Mods> {
        modifier_of(KeyCode::new(code as u16))
    }
    fn name_of(&self, code: u32) -> Option<String> {
        if let Some(button) = self.mouse_button(code) {
            return Some(Key::mouse(button).0);
        }
        name_from_key(KeyCode::new(code as u16))
    }
    fn code_of(&self, name: &str) -> Option<u32> {
        if let Some(button) = Key(hotkey::canonical_key_name(name)).mouse_button() {
            return Some(BTN_FIRST + button as u32 - 1).filter(|c| *c <= BTN_LAST);
        }
        key_from_name(name).map(|k| k.code() as u32)
    }
    fn is_escape(&self, code: u32) -> bool {
        code == KeyCode::KEY_ESC.code() as u32
    }
    fn mouse_button(&self, code: u32) -> Option<u8> {
        (BTN_FIRST..=BTN_LAST).contains(&code).then(|| (code - BTN_FIRST + 1) as u8)
    }
}

struct Listener {
    shared: Arc<Mutex<Shared>>,
    events: Sender<HotkeyEvent>,
    devices: HashMap<PathBuf, Device>,
    pressed: HashMap<PathBuf, HashSet<KeyCode>>,
    tracker: ChordTracker,
    /// Mice are open (a shortcut is a mouse button, or one is being
    /// recorded).
    mice: bool,
    last_scan: Instant,
    last_sweep: Instant,
    filter: KeyFilter,
    /// Passes on the keys of held keyboards during hands-free dictation.
    forward: Option<VirtualKeyboard>,
    forward_failed: bool,
    held: HashSet<PathBuf>,
    /// Keyboards another program holds (keyd, a VM), not retried until the
    /// next dictation.
    unholdable: HashSet<PathBuf>,
    release_due: Option<Instant>,
}

impl Listener {
    fn new(shared: Arc<Mutex<Shared>>, events: Sender<HotkeyEvent>, devices: HashMap<PathBuf, Device>) -> Self {
        Listener {
            shared,
            events,
            devices,
            pressed: HashMap::new(),
            tracker: ChordTracker::default(),
            mice: false,
            last_scan: Instant::now(),
            last_sweep: Instant::now(),
            filter: KeyFilter::default(),
            forward: None,
            forward_failed: false,
            held: HashSet::new(),
            unholdable: HashSet::new(),
            release_due: None,
        }
    }

    fn run(mut self) {
        loop {
            if self.shared.lock().stop {
                self.release_all();
                return;
            }
            let mice = {
                let s = self.shared.lock();
                hotkey::wants_mouse(&s.chords, s.capturing)
            };
            if mice != self.mice || self.last_scan.elapsed() > Duration::from_secs(3) {
                if self.mice && !mice {
                    self.close_mice();
                }
                self.mice = mice;
                // Pick up hot-plugged keyboards (and permission changes).
                let new = open_devices(&self.devices, mice);
                self.devices.extend(new);
                self.last_scan = Instant::now();
            }
            if self.last_sweep.elapsed() >= STALE_KEY_SWEEP {
                self.sweep_stale();
            }
            self.update_holds();

            let paths: Vec<PathBuf> = self.devices.keys().cloned().collect();
            // Notice the end of a dictation quickly while keyboards are held.
            let timeout: u16 = if self.held.is_empty() { 500 } else { 50 };
            let ready: Vec<PathBuf> = {
                // SAFETY: the fds stay alive for the duration of poll()
                // because `self.devices` is not modified until after.
                let mut fds: Vec<PollFd> = paths
                    .iter()
                    .map(|p| {
                        let fd = self.devices[p].as_fd().as_raw_fd();
                        PollFd::new(unsafe { BorrowedFd::borrow_raw(fd) }, PollFlags::POLLIN)
                    })
                    .collect();
                match poll(&mut fds, PollTimeout::from(timeout)) {
                    Ok(0) => Vec::new(),
                    Ok(_) => fds
                        .iter()
                        .zip(&paths)
                        .filter(|(fd, _)| fd.revents().is_some_and(|r| !r.is_empty()))
                        .map(|(_, p)| p.clone())
                        .collect(),
                    Err(nix::errno::Errno::EINTR) => Vec::new(),
                    Err(e) => {
                        log::error!("hotkeys: poll failed: {e}");
                        thread::sleep(Duration::from_millis(500));
                        Vec::new()
                    }
                }
            };

            for path in ready {
                self.drain(&path);
            }
        }
    }

    fn drain(&mut self, path: &Path) {
        let mut keys: Vec<(KeyCode, i32)> = Vec::new();
        let mut gone = false;
        if let Some(dev) = self.devices.get_mut(path) {
            match dev.fetch_events() {
                Ok(events) => {
                    for ev in events {
                        if let EventSummary::Key(_, code, value) = ev.destructure()
                            && is_tracked(code)
                        {
                            keys.push((code, value));
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    log::debug!("hotkeys: {} went away: {e}", path.display());
                    gone = true;
                }
            }
        }
        if gone {
            self.devices.remove(path);
            let pressed = self.pressed.remove(path).unwrap_or_default();
            // While it was held, its keys went through the virtual
            // keyboard: let go of them there, or they'd stay pressed for
            // as long as Bars runs.
            if self.held.remove(path)
                && let Some(kb) = self.forward.as_mut()
            {
                for key in pressed {
                    if !self.filter.is_dropped(key.code() as u32) {
                        let _ = kb.emit_key(key, 0);
                    }
                }
            }
            self.evaluate(None);
            return;
        }
        for (code, value) in keys {
            let set = self.pressed.entry(path.to_path_buf()).or_default();
            let completed = match value {
                1 => {
                    set.insert(code);
                    self.evaluate(Some(code))
                }
                0 => {
                    set.remove(&code);
                    self.evaluate(None)
                }
                _ => false, // autorepeat
            };
            if self.held.contains(path) {
                self.pass_on(code, value, completed);
            }
        }
    }

    /// Sends a key from a held keyboard on to apps, unless it's Escape
    /// cancelling the dictation or the key that finished the shortcut.
    fn pass_on(&mut self, code: KeyCode, value: i32, completed: bool) {
        let (chords, recording) = {
            let s = self.shared.lock();
            (s.chords.clone(), s.recording)
        };
        let input = FilterInput {
            chords: &chords,
            completed_chord: completed,
            recording,
            capturing: false,
        };
        let change = match value {
            0 => KeyChange::Release,
            1 => KeyChange::Press,
            _ => KeyChange::Repeat,
        };
        // Compositors repeat held keys themselves.
        if self.filter.should_drop(&EvdevKeys, code.code() as u32, change, &input) || change == KeyChange::Repeat {
            return;
        }
        let failed = self.forward.as_mut().is_none_or(|kb| kb.emit_key(code, value).is_err());
        if failed {
            log::warn!("hotkeys: couldn't pass a key on; letting go of the keyboards");
            self.release_all();
        }
    }

    /// Holds keyboards while a dictation records and lets go after, each
    /// only while none of its keys is down.
    fn update_holds(&mut self) {
        let (recording, hands_free) = {
            let s = self.shared.lock();
            (s.recording, s.hands_free)
        };
        if !recording {
            self.unholdable.clear();
        }
        // Made when a dictation starts, so it's ready by hands-free mode.
        if recording && self.forward.is_none() && !self.forward_failed {
            match VirtualKeyboard::new() {
                Ok(kb) => self.forward = Some(kb),
                Err(e) => {
                    log::info!("can't hold keyboards while dictating ({e:#}); Escape also reaches the app");
                    self.forward_failed = true;
                }
            }
        }
        let ready = self.forward.as_ref().is_some_and(|kb| kb.age() >= FORWARD_SETTLE);
        let paths: Vec<PathBuf> = self.devices.keys().cloned().collect();
        for path in paths {
            let busy = self.pressed.get(&path).is_some_and(|s| !s.is_empty());
            let held = self.held.contains(&path);
            let Some(dev) = self.devices.get_mut(&path) else { continue };
            if hands_free && ready && !held && !busy && !self.unholdable.contains(&path) && holdable(dev) {
                match dev.grab() {
                    Ok(()) => {
                        log::debug!("hotkeys: holding {}", path.display());
                        self.held.insert(path);
                    }
                    // Another program (keyd, a VM) holds it already.
                    Err(e) => {
                        log::debug!("hotkeys: can't hold {}: {e}", path.display());
                        self.unholdable.insert(path);
                    }
                }
            } else if !recording && held && !busy {
                let _ = dev.ungrab();
                self.held.remove(&path);
            }
        }
        if recording || self.held.is_empty() {
            self.release_due = None;
        } else if *self.release_due.get_or_insert_with(|| Instant::now() + RELEASE_GRACE) <= Instant::now() {
            self.release_all();
        }
    }

    /// Lets go of every held keyboard, releasing in apps any key passed on
    /// as pressed.
    fn release_all(&mut self) {
        for path in std::mem::take(&mut self.held) {
            if let Some(kb) = self.forward.as_mut() {
                for key in self.pressed.get(&path).into_iter().flatten() {
                    if !self.filter.is_dropped(key.code() as u32) {
                        let _ = kb.emit_key(*key, 0);
                    }
                }
            }
            if let Some(dev) = self.devices.get_mut(&path) {
                let _ = dev.ungrab();
            }
        }
        self.filter.reset();
        self.release_due = None;
    }

    /// Stops reading mice (no shortcut is a mouse button any more).
    fn close_mice(&mut self) {
        let mice: Vec<PathBuf> = self.devices.iter().filter(|(_, d)| !is_keyboard(d)).map(|(p, _)| p.clone()).collect();
        for path in mice {
            self.devices.remove(&path);
            self.pressed.remove(&path);
        }
        self.evaluate(None);
    }

    /// Forgets keys the kernel says are up (a release we never read).
    fn sweep_stale(&mut self) {
        self.last_sweep = Instant::now();
        let mut changed = false;
        for (path, set) in self.pressed.iter_mut() {
            if set.is_empty() || self.held.contains(path) {
                continue;
            }
            let Some(Ok(state)) = self.devices.get(path).map(|d| d.get_key_state()) else { continue };
            let before = set.len();
            set.retain(|k| state.contains(*k));
            if set.len() != before {
                log::debug!("hotkeys: dropped {} stuck key(s) on {}", before - set.len(), path.display());
                changed = true;
            }
        }
        if changed {
            self.evaluate(None);
        }
    }

    /// `newly_pressed` is the key that just went down, if any. Returns
    /// whether that press completed the shortcut.
    fn evaluate(&mut self, newly_pressed: Option<KeyCode>) -> bool {
        // Merge keys held on every keyboard.
        let pressed: HashSet<u32> = self.pressed.values().flatten().map(|k| k.code() as u32).collect();
        let (chords, capturing) = {
            let s = self.shared.lock();
            (s.chords.clone(), s.capturing)
        };
        let tracked = self.tracker.update(&EvdevKeys, &pressed, newly_pressed.map(|k| k.code() as u32), &chords, capturing);
        let completed = tracked.completed;
        let mut s = self.shared.lock();
        s.held_mods = tracked.mods;
        for ev in tracked.events {
            if matches!(ev, HotkeyEvent::Captured(_)) {
                s.capturing = false;
            }
            let _ = self.events.send(ev);
        }
        completed
    }
}
