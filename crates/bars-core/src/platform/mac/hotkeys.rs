//! Global shortcut via a Quartz event tap, on keys and on the mouse's
//! middle and side buttons (never left and right clicks). With
//! Accessibility the tap can drop events: Escape while recording (it
//! cancels the dictation instead of also closing a dialog in the app), the
//! key or button that finishes a shortcut, keys and buttons pressed while a
//! new shortcut is being recorded, and Fn/Globe when Fn alone is a shortcut
//! (so macOS doesn't also open the emoji picker or switch input source).
//! With only Input Monitoring it falls back to listening.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use core_foundation::base::TCFType;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{CFRunLoop, CFRunLoopTimer, CFRunLoopTimerRef, kCFRunLoopCommonModes};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};
use crossbeam_channel::Sender;
use parking_lot::Mutex;

use super::keycodes::{KVK_GLOBE, MODIFIERS, MOUSE_BASE, MacKeys};
use crate::hotkey::{
    self, Chord, ChordTracker, FilterInput, HeldKeys, HotkeyEvent, KeyChange, KeyFilter, KeyMap, Mods,
    STALE_KEY_AGE, STALE_KEY_SWEEP,
};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
    fn CGEventSourceFlagsState(state: i32) -> u64;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFAbsoluteTimeGetCurrent() -> f64;
}

/// kCGEventSourceStateHIDSystemState: the keyboard as the hardware reports it.
const HID_STATE: i32 = 1;

/// Marks events Bars posts itself (typing, the paste keystroke), which the
/// tap ignores. enigo tags its events with the same value.
pub const SYNTHETIC_MARKER: i64 = enigo::EVENT_MARKER as i64;

#[derive(Default)]
struct Shared {
    chords: Vec<Chord>,
    capturing: bool,
    recording: bool,
}

struct TapState {
    held: HeldKeys,
    tracker: ChordTracker,
    filter: KeyFilter,
}

struct Tap {
    state: Mutex<TapState>,
    shared: Arc<Mutex<Shared>>,
    events: Sender<HotkeyEvent>,
    /// The tap's port, for switching it back on from its callback.
    port: AtomicUsize,
    /// The tap can drop events (it's active, not listen-only).
    active: AtomicBool,
}

thread_local! {
    // The sweep timer fires on the tap's thread.
    static SWEPT: RefCell<Option<Arc<Tap>>> = const { RefCell::new(None) };
}

extern "C" fn on_sweep_timer(_timer: CFRunLoopTimerRef, _info: *mut c_void) {
    SWEPT.with(|t| {
        if let Some(tap) = t.borrow().as_ref() {
            tap.sweep();
        }
    });
}

pub struct MacHotkeys {
    shared: Arc<Mutex<Shared>>,
}

impl MacHotkeys {
    pub fn spawn(chords: Vec<Chord>, events: Sender<HotkeyEvent>) -> anyhow::Result<Self> {
        // Checked first: creating a tap without access pops a system prompt
        // before the setup assistant has explained anything.
        if !super::permissions::control() && !super::permissions::input_monitoring() {
            log::info!("macOS access: {}", super::permissions::summary());
            anyhow::bail!("{} permission is needed", super::permissions::accessibility_pane());
        }
        let shared = Arc::new(Mutex::new(Shared {
            chords,
            ..Default::default()
        }));
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(), String>>(1);
        let tap = Arc::new(Tap {
            state: Mutex::new(TapState {
                held: HeldKeys::default(),
                tracker: ChordTracker::default(),
                filter: KeyFilter::default(),
            }),
            shared: shared.clone(),
            events,
            port: AtomicUsize::new(0),
            active: AtomicBool::new(false),
        });
        thread::Builder::new()
            .name("bars-hotkeys".into())
            .spawn(move || {
                let create = |options| {
                    let t = tap.clone();
                    CGEventTap::new(
                        CGEventTapLocation::Session,
                        CGEventTapPlacement::HeadInsertEventTap,
                        options,
                        vec![
                            CGEventType::KeyDown,
                            CGEventType::KeyUp,
                            CGEventType::FlagsChanged,
                            // Middle and side buttons; left and right clicks
                            // never reach the tap.
                            CGEventType::OtherMouseDown,
                            CGEventType::OtherMouseUp,
                        ],
                        move |_proxy, etype, event| t.on_event(etype, event),
                    )
                };
                let (cg_tap, active) = match create(CGEventTapOptions::Default) {
                    Ok(t) => (Ok(t), true),
                    // Input Monitoring alone only allows listening.
                    Err(()) => (create(CGEventTapOptions::ListenOnly), false),
                };
                let cg_tap = match cg_tap {
                    Ok(t) => t,
                    Err(()) => {
                        let _ = ready_tx.send(Err("could not create the keyboard event tap".into()));
                        return;
                    }
                };
                if !active {
                    log::info!("keyboard tap is listen-only: Escape and the shortcut's key also reach apps");
                }
                super::permissions::set_full_listener(active);
                log::info!("macOS access: {}", super::permissions::summary());
                tap.active.store(active, Ordering::Release);
                tap.port
                    .store(cg_tap.mach_port().as_concrete_TypeRef() as usize, Ordering::Release);
                let Ok(source) = cg_tap.mach_port().create_runloop_source(0) else {
                    let _ = ready_tx.send(Err("could not attach the event tap".into()));
                    return;
                };
                let run_loop = CFRunLoop::get_current();
                run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });
                SWEPT.with(|t| *t.borrow_mut() = Some(tap.clone()));
                let every = STALE_KEY_SWEEP.as_secs_f64();
                let first = unsafe { CFAbsoluteTimeGetCurrent() } + every;
                let timer = CFRunLoopTimer::new(first, every, 0, 0, on_sweep_timer, std::ptr::null_mut());
                run_loop.add_timer(&timer, unsafe { kCFRunLoopCommonModes });
                cg_tap.enable();
                let _ = ready_tx.send(Ok(()));
                CFRunLoop::run_current();
            })?;
        ready_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("hotkey thread exited"))?
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(MacHotkeys { shared })
    }

    pub fn set_chords(&self, chords: Vec<Chord>) {
        self.shared.lock().chords = chords;
    }

    pub fn set_capture(&self, on: bool) {
        self.shared.lock().capturing = on;
    }

    pub fn set_recording(&self, on: bool) {
        self.shared.lock().recording = on;
    }

    /// Modifiers held right now, as the keyboard reports them.
    pub fn mods_probe(&self) -> Arc<dyn Fn() -> Mods + Send + Sync> {
        Arc::new(|| {
            let flags = CGEventFlags::from_bits_retain(unsafe { CGEventSourceFlagsState(HID_STATE) });
            [Mods::CTRL, Mods::ALT, Mods::SHIFT, Mods::SUPER]
                .into_iter()
                .filter(|m| flags.contains(flag_for(*m)))
                .fold(Mods::empty(), |a, m| a | m)
        })
    }
}

impl Tap {
    fn on_event(&self, etype: CGEventType, event: &CGEvent) -> CallbackResult {
        // macOS switches taps off when a callback is slow, and while a
        // password field has secure input (System Settings asks for one
        // when toggling permissions). Without this the shortcut stays dead
        // until restart.
        if matches!(etype, CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput) {
            log::info!("keyboard event tap was disabled by macOS ({etype:?}); re-enabling");
            let mut st = self.state.lock();
            // Keys may have been let go while the tap wasn't listening.
            // Those the keyboard says are up are forgotten, and the tracker
            // is told: a shortcut that was held ends, and the dictation
            // with it, rather than recording on with no release to come.
            // Keys still down stay held; their releases arrive as usual.
            let released = st.held.drop_stale(Duration::ZERO, key_is_down);
            let TapState { held, filter, .. } = &mut *st;
            filter.keep_only(&held.keys);
            if !released.is_empty() {
                let (chords, capturing) = {
                    let sh = self.shared.lock();
                    (sh.chords.clone(), sh.capturing)
                };
                self.track(&mut st, None, &chords, capturing);
            }
            let p = self.port.load(Ordering::Acquire);
            if p != 0 {
                unsafe { CGEventTapEnable(p as CFMachPortRef, true) };
            }
            return CallbackResult::Keep;
        }
        // Our own typing and pasting.
        if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == SYNTHETIC_MARKER {
            return CallbackResult::Keep;
        }
        let (chords, capturing, recording) = {
            let sh = self.shared.lock();
            (sh.chords.clone(), sh.capturing, sh.recording)
        };
        let active = self.active.load(Ordering::Acquire);
        let mouse = matches!(etype, CGEventType::OtherMouseDown | CGEventType::OtherMouseUp);
        let code = if mouse {
            MOUSE_BASE + event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) as u32
        } else {
            event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u32
        };
        // Newer keyboards send the Globe key as a key of its own, next to
        // Fn's flag change.
        if !mouse && code == KVK_GLOBE {
            let drop = active && chords.iter().any(Chord::is_fn_only);
            return if drop { CallbackResult::Drop } else { CallbackResult::Keep };
        }
        let flags = event.get_flags();
        let mut st = self.state.lock();
        // A button no shortcut uses passes untouched (unless its press was
        // dropped, before the shortcuts changed).
        if mouse && !hotkey::counts(&MacKeys, code, &chords, capturing) && !st.held.contains(code) {
            return CallbackResult::Keep;
        }
        let (change, newly) = match etype {
            CGEventType::OtherMouseDown => (KeyChange::Press, st.held.press(code).then_some(code)),
            CGEventType::OtherMouseUp => {
                st.held.release(code);
                (KeyChange::Release, None)
            }
            CGEventType::KeyDown if event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0 => {
                (KeyChange::Repeat, None)
            }
            CGEventType::KeyDown => (KeyChange::Press, st.held.press(code).then_some(code)),
            CGEventType::KeyUp => {
                st.held.release(code);
                (KeyChange::Release, None)
            }
            // Caps Lock also reports through here, but only when it
            // toggles: tracked as a key it would look held while it's on,
            // blocking the shortcut and shortcut recording.
            CGEventType::FlagsChanged if MacKeys.modifier_of(code).is_none() => return CallbackResult::Keep,
            CGEventType::FlagsChanged => {
                // A modifier changed; which way is told by the flags.
                let was_down = st.held.contains(code);
                let down = modifier_down(code, flags, was_down);
                if down {
                    st.held.press(code);
                } else {
                    st.held.release(code);
                }
                sync_modifiers(&mut st.held, flags);
                let change = if down { KeyChange::Press } else { KeyChange::Release };
                (change, (down && !was_down).then_some(code))
            }
            _ => return CallbackResult::Keep,
        };
        if newly.is_some() {
            // Before the shortcut can fire: a key that has looked held a
            // while may have been let go unseen.
            let TapState { held, filter, .. } = &mut *st;
            held.drop_stale(STALE_KEY_AGE, |k| filter.is_dropped(k) || key_is_down(k));
        }
        let completed = change != KeyChange::Repeat && self.track(&mut st, newly, &chords, capturing);
        let input = FilterInput {
            chords: &chords,
            completed_chord: completed,
            recording,
            capturing,
        };
        if active && st.filter.should_drop(&MacKeys, code, change, &input) {
            CallbackResult::Drop
        } else {
            CallbackResult::Keep
        }
    }

    /// Runs the tracker; returns whether this press completed a shortcut.
    fn track(&self, st: &mut TapState, newly: Option<u32>, chords: &[Chord], capturing: bool) -> bool {
        let TapState { held, tracker, .. } = st;
        let tracked = tracker.update(&MacKeys, &held.keys, newly, chords, capturing);
        if tracked.events.iter().any(|e| matches!(e, HotkeyEvent::Captured(_))) {
            self.shared.lock().capturing = false;
        }
        for ev in tracked.events {
            let _ = self.events.send(ev);
        }
        tracked.completed
    }

    /// Every few seconds: forget keys the keyboard says are up, so a
    /// shortcut whose release was missed still ends.
    fn sweep(&self) {
        let mut st = self.state.lock();
        let TapState { held, filter, .. } = &mut *st;
        if held.drop_stale(STALE_KEY_AGE, |k| filter.is_dropped(k) || key_is_down(k)).is_empty() {
            return;
        }
        log::debug!("hotkeys: dropped keys macOS reports as released");
        let (chords, capturing) = {
            let sh = self.shared.lock();
            (sh.chords.clone(), sh.capturing)
        };
        self.track(&mut st, None, &chords, capturing);
    }
}

/// Whether the keyboard (or mouse) reports `code` as held. Modifiers by
/// their flag: Fn's has no reliable per-key state.
fn key_is_down(code: u32) -> bool {
    if let Some(button) = MacKeys.mouse_button(code) {
        return unsafe { CGEventSourceButtonState(HID_STATE, button as u32 - 1) };
    }
    match MacKeys.modifier_of(code) {
        Some(m) => CGEventFlags::from_bits_retain(unsafe { CGEventSourceFlagsState(HID_STATE) }).contains(flag_for(m)),
        None => unsafe { CGEventSourceKeyState(HID_STATE, code as u16) },
    }
}

/// The event flag for a kind of modifier (either side).
fn flag_for(m: Mods) -> CGEventFlags {
    if m == Mods::FN {
        // Also set while arrow/function keys are held, but Fn is only ever
        // added from its own key code, so that can't fake a press.
        CGEventFlags::CGEventFlagSecondaryFn
    } else if m == Mods::SUPER {
        CGEventFlags::CGEventFlagCommand
    } else if m == Mods::SHIFT {
        CGEventFlags::CGEventFlagShift
    } else if m == Mods::ALT {
        CGEventFlags::CGEventFlagAlternate
    } else {
        CGEventFlags::CGEventFlagControl
    }
}

/// Whether the modifier `code` is down after a flags-changed event for it.
///
/// Keyboard events carry per-key bits (left/right Control, Shift, Option,
/// Command), which say exactly. Synthetic events may only carry the
/// either-side flag; then a change while it's set means this key went down,
/// unless it already was.
fn modifier_down(code: u32, flags: CGEventFlags, was_down: bool) -> bool {
    // (this key's bit, both sides' bits), from IOLLEvent.h (NX_DEVICE*KEYMASK).
    let (key_bit, kind_bits): (u64, u64) = match code {
        0x3B => (0x0001, 0x2001), // left Control
        0x3E => (0x2000, 0x2001), // right Control
        0x38 => (0x0002, 0x0006), // left Shift
        0x3C => (0x0004, 0x0006), // right Shift
        0x3A => (0x0020, 0x0060), // left Option
        0x3D => (0x0040, 0x0060), // right Option
        0x37 => (0x0008, 0x0018), // left Command
        0x36 => (0x0010, 0x0018), // right Command
        _ => (0, 0),
    };
    let bits = flags.bits();
    if bits & kind_bits != 0 {
        return bits & key_bit != 0;
    }
    let kind = MODIFIERS.iter().find(|(c, _)| *c == code).map(|(_, m)| *m);
    kind.is_some_and(|m| flags.contains(flag_for(m))) && !was_down
}

/// Drop modifier keys the flags say are no longer held (e.g. a release that
/// happened while the tap wasn't listening).
fn sync_modifiers(held: &mut HeldKeys, flags: CGEventFlags) {
    for (code, m) in MODIFIERS {
        if !flags.contains(flag_for(*m)) {
            held.release(*code);
        }
    }
}
