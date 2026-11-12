//! Platform-independent hotkey model: chords, the events backends emit, and
//! the gesture recognizer that turns raw chord presses into dictation intents
//! (push-to-talk, double-tap hands-free, toggle). Any number of shortcuts
//! start dictation, a shortcut can be a key on its own (F9, Pause), and it
//! can be a mouse button: backends report buttons as keys of their own,
//! named `Mouse3` (middle), `Mouse4` (back), `Mouse5` (forward) and so on.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
    pub struct Mods: u8 {
        const CTRL = 1;
        const ALT = 1 << 1;
        const SHIFT = 1 << 2;
        const SUPER = 1 << 3;
        /// The Fn (Globe) key. Only Macs report it; elsewhere keyboards
        /// handle it in firmware (a few Linux laptops send it too).
        const FN = 1 << 4;
    }
}

impl Mods {
    pub fn labels(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.contains(Mods::FN) {
            out.push("Fn");
        }
        if self.contains(Mods::CTRL) {
            out.push("Ctrl");
        }
        if self.contains(Mods::ALT) {
            out.push(ALT_LABEL);
        }
        if self.contains(Mods::SHIFT) {
            out.push("Shift");
        }
        if self.contains(Mods::SUPER) {
            out.push(SUPER_LABEL);
        }
        out
    }
}

#[cfg(target_os = "macos")]
const ALT_LABEL: &str = "Option";
#[cfg(not(target_os = "macos"))]
const ALT_LABEL: &str = "Alt";

#[cfg(target_os = "macos")]
const SUPER_LABEL: &str = "Cmd";
#[cfg(target_os = "windows")]
const SUPER_LABEL: &str = "Win";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const SUPER_LABEL: &str = "Super";

/// A non-modifier key, named with a platform-neutral canonical name
/// (`"Space"`, `"A"`, `"F9"`, `"Grave"`, `"Mouse4"`, ...). Platform
/// backends map these names to native key codes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key(pub String);

impl Key {
    /// A mouse button as a key.
    pub fn mouse(button: u8) -> Key {
        Key(format!("Mouse{button}"))
    }

    /// The mouse button this is, numbered as most systems do: 1 left,
    /// 2 right, 3 middle, 4 back, 5 forward, then extra buttons.
    pub fn mouse_button(&self) -> Option<u8> {
        self.0.strip_prefix("Mouse")?.parse().ok().filter(|b| *b >= 1)
    }

    /// A key that types or edits text: letters, digits, punctuation,
    /// Space, Enter, Tab, Backspace and Delete. On its own as a shortcut it
    /// would be taken from every app.
    pub fn types_text(&self) -> bool {
        self.0.chars().count() == 1
            || matches!(
                self.0.as_str(),
                "Space"
                    | "Enter"
                    | "Tab"
                    | "Backspace"
                    | "Delete"
                    | "Grave"
                    | "Minus"
                    | "Equal"
                    | "Leftbracket"
                    | "Rightbracket"
                    | "Backslash"
                    | "Semicolon"
                    | "Apostrophe"
                    | "Comma"
                    | "Period"
                    | "Slash"
            )
    }

    /// One side's modifier key (`Rightalt`, `Leftctrl`), which a shortcut
    /// uses as a key of its own: Right Option alone leaves the left one
    /// to work as usual. Says which modifier it is.
    pub fn modifier(&self) -> Option<Mods> {
        let name = self.0.strip_prefix("Left").or_else(|| self.0.strip_prefix("Right"))?;
        SIDED_NAMES.iter().find(|(_, n)| *n == name).map(|(m, _)| *m)
    }

    /// For keycaps: "Space", "Mouse 4" (the numbers gamers know: 3 is
    /// the middle button, 4 and 5 back and forward), "Right Option".
    pub fn label(&self) -> String {
        if let Some(b) = self.mouse_button() {
            return format!("Mouse {b}");
        }
        match self.modifier() {
            Some(m) => {
                let side = if self.0.starts_with("Left") { "Left" } else { "Right" };
                format!("{side} {}", m.labels()[0])
            }
            None => self.0.clone(),
        }
    }
}

/// The modifier a word names ("ctrl", "Option", "cmd"), either side.
fn modifier_word(word: &str) -> Option<Mods> {
    match word.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(Mods::CTRL),
        "alt" | "option" => Some(Mods::ALT),
        "shift" => Some(Mods::SHIFT),
        "super" | "meta" | "win" | "cmd" | "command" | "logo" => Some(Mods::SUPER),
        "fn" | "globe" => Some(Mods::FN),
        _ => None,
    }
}

/// How one side's modifier keys are named, after "Left" or "Right".
const SIDED_NAMES: [(Mods, &str); 4] =
    [(Mods::CTRL, "ctrl"), (Mods::ALT, "alt"), (Mods::SHIFT, "shift"), (Mods::SUPER, "super")];

/// The key name for one side's modifier ("RightOption", "right cmd",
/// "Leftctrl"): `Rightalt`, `Rightsuper`, `Leftctrl`.
fn sided_modifier_name(part: &str) -> Option<String> {
    let lower = part.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    let (side, word) = match lower.strip_prefix("right") {
        Some(word) => ("Right", word),
        None => ("Left", lower.strip_prefix("left")?),
    };
    let m = modifier_word(word)?;
    let (_, name) = SIDED_NAMES.iter().find(|(s, _)| *s == m)?;
    Some(format!("{side}{name}"))
}

/// A hotkey: modifiers plus a single key (or mouse button), a key on its
/// own (one that doesn't type, like F9), or modifiers alone. A chord
/// without a key (e.g. Ctrl+Super, or a modifier on its own) is a
/// "modifier-only" chord, which is the most comfortable push-to-talk
/// binding because it never types anything. A modifier on its own can
/// name its side, as a key: [`Key::modifier`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub mods: Mods,
    pub key: Option<Key>,
}

impl Default for Chord {
    fn default() -> Self {
        Chord {
            mods: Mods::CTRL | Mods::SUPER,
            key: None,
        }
    }
}

impl Chord {
    pub fn is_valid(&self) -> Result<(), &'static str> {
        // Any button but the two every click needs, with or without modifiers.
        if let Some(button) = self.mouse_button() {
            return if button >= 3 { Ok(()) } else { Err("Left and right click can't be shortcuts") };
        }
        if self.mods.is_empty() {
            // A key on its own, as long as apps can do without it.
            return match &self.key {
                None => Err("That key can't be a shortcut"),
                Some(k) if k.0 == "Escape" => Err("Escape cancels a dictation, so on its own it can't be a shortcut"),
                Some(k) if k.types_text() => Err("A key that types or edits text needs a modifier (Ctrl, Alt, Shift or Super)"),
                Some(_) => Ok(()),
            };
        }
        if self.lone_modifier().is_none() && self.key.as_ref().is_some_and(|k| k.modifier().is_some()) {
            return Err("A left or right modifier can only be a shortcut on its own");
        }
        Ok(())
    }

    /// The modifier this shortcut is, when it's one on its own (either
    /// side, or one side's), Fn aside.
    pub fn lone_modifier(&self) -> Option<Mods> {
        match &self.key {
            None => (self.mods.bits().count_ones() == 1 && self.mods != Mods::FN).then_some(self.mods),
            Some(k) if self.mods.is_empty() => k.modifier(),
            Some(_) => None,
        }
    }

    /// The shortcut is the Fn (Globe) key on its own.
    pub fn is_fn_only(&self) -> bool {
        self.mods == Mods::FN && self.key.is_none()
    }

    /// The mouse button this shortcut uses, if any.
    pub fn mouse_button(&self) -> Option<u8> {
        self.key.as_ref().and_then(Key::mouse_button)
    }

    /// The canonical names of its parts, e.g. `["Ctrl", "Super", "Space"]`.
    pub fn parts(&self) -> Vec<String> {
        let mut parts: Vec<String> = self.mods.labels().into_iter().map(String::from).collect();
        if let Some(key) = &self.key {
            parts.push(key.0.clone());
        }
        parts
    }

    /// Labels for rendering keycaps, e.g. `["Ctrl", "Mouse 4"]`.
    pub fn labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = self.mods.labels().into_iter().map(String::from).collect();
        if let Some(key) = &self.key {
            labels.push(key.label());
        }
        labels
    }

    /// For sentences: "Ctrl + Mouse 4".
    pub fn label(&self) -> String {
        self.labels().join(" + ")
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.parts().join("+"))
    }
}

impl FromStr for Chord {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut mods = Mods::empty();
        let mut key = None;
        for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
            if let Some(m) = modifier_word(part) {
                mods |= m;
                continue;
            }
            if key.is_some() {
                return Err(format!("more than one non-modifier key in '{s}'"));
            }
            let name = sided_modifier_name(part).unwrap_or_else(|| canonical_key_name(part));
            key = Some(Key(name));
        }
        Ok(Chord { mods, key })
    }
}

/// Normalizes user-typed key names so `"space"`, `"SPACE"` and `"Space"` match.
pub fn canonical_key_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.len() == 1 {
        return lower.to_ascii_uppercase();
    }
    if let Some(n) = lower.strip_prefix('f')
        && n.parse::<u8>().is_ok()
    {
        return format!("F{n}");
    }
    let mut chars = lower.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

impl Serialize for Chord {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Chord {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Raw events produced by a hotkey backend.
#[derive(Clone, Debug, PartialEq)]
pub enum HotkeyEvent {
    /// The chord became fully pressed.
    Down,
    /// The chord was released.
    Up,
    /// While the chord was held, another key was pressed: the user is most
    /// likely performing an unrelated shortcut (e.g. Ctrl+Super+Left).
    Interrupted,
    /// Escape was pressed (used to cancel a recording).
    Escape,
    /// The backend finished capturing a new chord for the settings UI.
    Captured(Chord),
    /// The desktop bound our shortcut (portal backend); human-readable trigger.
    Bound(String),
}

/// How a platform names its key codes. Implemented by each hotkey backend
/// (evdev codes, Windows virtual keys, macOS key codes), which give mouse
/// buttons codes of their own.
pub trait KeyMap {
    /// The modifier a key code represents, if any.
    fn modifier_of(&self, code: u32) -> Option<Mods>;
    /// Canonical name ("Space", "A", "F9", "Mouse4") for a non-modifier
    /// key, or for one side's modifier ("Rightalt", see [`Key::modifier`]).
    fn name_of(&self, code: u32) -> Option<String>;
    /// Key code for a canonical name.
    fn code_of(&self, name: &str) -> Option<u32>;
    fn is_escape(&self, code: u32) -> bool;
    /// The mouse button a code stands for (see [`Key::mouse_button`]).
    fn mouse_button(&self, code: u32) -> Option<u8> {
        let _ = code;
        None
    }
}

/// Whether a key counts for shortcuts: every key, but only the mouse
/// buttons a shortcut uses (any but left and right while one is being
/// recorded), so clicking never gets in a shortcut's way.
pub fn counts(map: &dyn KeyMap, code: u32, chords: &[Chord], capturing: bool) -> bool {
    match map.mouse_button(code) {
        None => true,
        Some(button) if capturing => button >= 3,
        Some(button) => chords.iter().any(|c| c.mouse_button() == Some(button)),
    }
}

/// Some shortcut is a mouse button, or one is being recorded: backends
/// listen to the mouse.
pub fn wants_mouse(chords: &[Chord], capturing: bool) -> bool {
    capturing || chords.iter().any(|c| c.mouse_button().is_some())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ChordState {
    Inactive,
    /// The shortcut at this index is held.
    Active(usize),
    /// Another key was pressed while the shortcut at this index was held.
    Interrupted(usize),
}

/// What one [`ChordTracker::update`] found.
#[derive(Debug, Default)]
pub struct Tracked {
    pub events: Vec<HotkeyEvent>,
    /// Modifiers held.
    pub mods: Mods,
    /// The key just pressed completed a shortcut (backends that can drop
    /// input keep it from the app).
    pub completed: bool,
}

/// Turns the set of currently pressed keys into chord [`HotkeyEvent`]s, and
/// records new chords for the shortcut picker. Shared by every OS backend.
///
/// Every shortcut does the same, so one held shortcut turning into another
/// (Ctrl+Super, then Space with Ctrl+Super+Space also a shortcut) carries
/// on as the same press.
#[derive(Debug)]
pub struct ChordTracker {
    state: ChordState,
    capture_keys: Vec<u32>,
}

impl Default for ChordTracker {
    fn default() -> Self {
        ChordTracker {
            state: ChordState::Inactive,
            capture_keys: Vec::new(),
        }
    }
}

impl ChordTracker {
    /// Feed the full pressed set after each key change. `newly_pressed` is
    /// the key that just went down (None for releases).
    pub fn update(
        &mut self,
        map: &dyn KeyMap,
        pressed: &HashSet<u32>,
        newly_pressed: Option<u32>,
        chords: &[Chord],
        capturing: bool,
    ) -> Tracked {
        let keys: HashSet<u32> = pressed.iter().copied().filter(|k| counts(map, *k, chords, capturing)).collect();
        let newly = newly_pressed.filter(|k| keys.contains(k));
        let mods = keys
            .iter()
            .filter_map(|k| map.modifier_of(*k))
            .fold(Mods::empty(), |a, m| a | m);
        let mut out = Tracked { mods, ..Default::default() };

        if capturing {
            self.state = ChordState::Inactive;
            if let Some(k) = newly {
                if !self.capture_keys.contains(&k) {
                    self.capture_keys.push(k);
                }
            } else if keys.is_empty() && !self.capture_keys.is_empty() {
                let keys = std::mem::take(&mut self.capture_keys);
                let cmods = keys
                    .iter()
                    .filter_map(|k| map.modifier_of(*k))
                    .fold(Mods::empty(), |a, m| a | m);
                let pressed = keys.iter().rev().find(|k| map.modifier_of(**k).is_none());
                let key = pressed.and_then(|k| map.name_of(*k)).map(Key);
                // A modifier on its own is recorded with its side.
                let sided = match keys.as_slice() {
                    [k] => map.name_of(*k).map(Key).filter(|k| k.modifier().is_some()),
                    _ => None,
                };
                let chord = match sided {
                    Some(k) => Chord { mods: Mods::empty(), key: Some(k) },
                    // A key with no name here (a numpad or media key, say)
                    // can't be a shortcut. Nothing is recorded, rather
                    // than the modifiers held with it on their own.
                    None if pressed.is_some() && key.is_none() => Chord { mods: Mods::empty(), key: None },
                    None => Chord { mods: cmods, key },
                };
                out.events.push(HotkeyEvent::Captured(chord));
            }
            return out;
        }
        self.capture_keys.clear();

        if newly.is_some_and(|k| map.is_escape(k)) {
            out.events.push(HotkeyEvent::Escape);
        }
        let others: Vec<u32> = keys.iter().copied().filter(|k| map.modifier_of(*k).is_none()).collect();
        // A shortcut whose key this platform doesn't have never matches.
        let key_of = |c: &Chord| c.key.as_ref().map(|k| map.code_of(&k.0).ok_or(()));
        let exact = |c: &Chord| match key_of(c) {
            None => mods == c.mods && others.is_empty(),
            // One side's modifier, with nothing else held.
            Some(Ok(k)) if map.modifier_of(k).is_some() => keys.len() == 1 && keys.contains(&k),
            Some(Ok(k)) => mods == c.mods && others.len() == 1 && others[0] == k,
            Some(Err(())) => false,
        };
        let held = |i: usize| {
            chords.get(i).is_some_and(|c| {
                mods.contains(c.mods)
                    && match key_of(c) {
                        None => true,
                        Some(Ok(k)) => keys.contains(&k),
                        Some(Err(())) => false,
                    }
            })
        };
        let matching = chords.iter().position(exact);
        self.state = match self.state {
            ChordState::Inactive => match matching {
                Some(i) => {
                    out.events.push(HotkeyEvent::Down);
                    out.completed = newly.is_some();
                    ChordState::Active(i)
                }
                None => ChordState::Inactive,
            },
            ChordState::Active(i) => {
                if chords.get(i).is_some_and(exact) {
                    ChordState::Active(i)
                } else if let Some(j) = matching {
                    // Held into another shortcut: the same press goes on.
                    out.completed = newly.is_some();
                    ChordState::Active(j)
                } else if held(i) {
                    out.events.push(HotkeyEvent::Interrupted);
                    ChordState::Interrupted(i)
                } else {
                    out.events.push(HotkeyEvent::Up);
                    ChordState::Inactive
                }
            }
            ChordState::Interrupted(i) if held(i) => ChordState::Interrupted(i),
            ChordState::Interrupted(_) => {
                out.events.push(HotkeyEvent::Up);
                ChordState::Inactive
            }
        };
        out
    }

    /// Whether a shortcut is currently held.
    pub fn active(&self) -> bool {
        matches!(self.state, ChordState::Active(_))
    }
}

/// What [`KeyFilter`] needs to know about the moment a key changed.
#[derive(Clone, Copy, Debug, Default)]
pub struct FilterInput<'a> {
    pub chords: &'a [Chord],
    /// This very press completed a shortcut ([`Tracked::completed`]).
    pub completed_chord: bool,
    /// A dictation is recording (Escape cancels it).
    pub recording: bool,
    /// The settings window is recording a new shortcut.
    pub capturing: bool,
}

/// One change of a key, as a backend sees it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KeyChange {
    Press,
    /// Autorepeat while held.
    Repeat,
    Release,
}

/// Decides which key events a backend that can drop input (macOS event
/// tap, Windows keyboard and mouse hooks, Linux while it holds the
/// keyboards) keeps from the focused app: Escape cancels only the
/// dictation, the key or mouse button that finishes a shortcut does
/// nothing else, and recording a shortcut doesn't also trigger it in the
/// app. Mouse buttons no shortcut uses always pass.
///
/// Modifiers always pass, since a dropped one would stay held in other
/// apps. Fn is the exception when it is a whole shortcut, so the system's
/// Globe-key action (emoji picker, input switch) doesn't fire too.
#[derive(Debug, Default)]
pub struct KeyFilter {
    /// Keys whose press was dropped: their repeats and release go too.
    dropped: HashSet<u32>,
}

impl KeyFilter {
    /// Whether to drop this event. Repeats and the release follow the
    /// press; every new press is decided afresh.
    pub fn should_drop(&mut self, map: &dyn KeyMap, code: u32, change: KeyChange, input: &FilterInput) -> bool {
        match change {
            KeyChange::Release => return self.dropped.remove(&code),
            KeyChange::Repeat => return self.dropped.contains(&code),
            KeyChange::Press => {
                self.dropped.remove(&code);
            }
        }
        if !counts(map, code, input.chords, input.capturing) {
            return false;
        }
        let drop = match map.modifier_of(code) {
            Some(m) => m == Mods::FN && input.chords.iter().any(Chord::is_fn_only),
            None => input.capturing || (input.recording && map.is_escape(code)) || input.completed_chord,
        };
        if drop {
            self.dropped.insert(code);
        }
        drop
    }

    /// Whether this key's press was dropped (so its release must be too).
    pub fn is_dropped(&self, code: u32) -> bool {
        self.dropped.contains(&code)
    }

    /// Forget dropped presses (after the backend lost track of keys).
    pub fn reset(&mut self) {
        self.dropped.clear();
    }

    /// Forget the dropped presses of keys that aren't in `held` any more:
    /// their releases went by unseen.
    pub fn keep_only(&mut self, held: &HashSet<u32>) {
        self.dropped.retain(|code| held.contains(code));
    }
}

/// Keys that have looked held at least this long are checked against the
/// OS's own key state before the shortcut can fire.
pub const STALE_KEY_AGE: Duration = Duration::from_millis(2500);
/// How often every held key is checked, so a shortcut whose release was
/// missed still ends.
pub const STALE_KEY_SWEEP: Duration = Duration::from_secs(5);

/// The keys a backend believes are held, and since when. A release the
/// backend never saw (secure input, a tap macOS switched off, a lost
/// event) would otherwise keep a key "held" and block the shortcut.
#[derive(Debug, Default)]
pub struct HeldKeys {
    pub keys: HashSet<u32>,
    since: HashMap<u32, Instant>,
}

impl HeldKeys {
    /// Returns whether the key was newly pressed (false for autorepeat).
    pub fn press(&mut self, code: u32) -> bool {
        let new = self.keys.insert(code);
        if new {
            self.since.insert(code, Instant::now());
        }
        new
    }

    pub fn release(&mut self, code: u32) {
        self.keys.remove(&code);
        self.since.remove(&code);
    }

    pub fn contains(&self, code: u32) -> bool {
        self.keys.contains(&code)
    }

    pub fn clear(&mut self) {
        self.keys.clear();
        self.since.clear();
    }

    /// Drops keys held at least `min_age` that `is_down` reports as up.
    /// Returns the dropped keys.
    pub fn drop_stale(&mut self, min_age: Duration, is_down: impl Fn(u32) -> bool) -> Vec<u32> {
        let now = Instant::now();
        let stale: Vec<u32> = self
            .since
            .iter()
            .filter(|(code, at)| now.saturating_duration_since(**at) >= min_age && !is_down(**code))
            .map(|(code, _)| *code)
            .collect();
        for code in &stale {
            self.release(*code);
        }
        stale
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Activation {
    /// Hold to talk; double-tap to lock into hands-free mode.
    #[default]
    HoldOrDoubleTap,
    /// Hold to talk only.
    Hold,
    /// Press once to start, again to stop.
    Toggle,
}

/// What the recognizer wants the controller to do.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Intent {
    /// Start capturing audio (shows the overlay).
    Start,
    /// The recording is now locked in hands-free mode.
    Lock,
    /// Stop and transcribe.
    Finish,
    /// Stop and discard.
    Cancel,
}

/// Holds shorter than this are taps, not push-to-talk.
pub const TAP_MAX: Duration = Duration::from_millis(280);
/// A second press within this window after a tap locks hands-free mode.
pub const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);
/// An interruption later than this is ignored rather than cancelling.
pub const INTERRUPT_GRACE: Duration = Duration::from_millis(600);

#[derive(Copy, Clone, Debug, PartialEq)]
enum Phase {
    Idle,
    /// Chord held, audio capturing.
    Holding { since: Instant },
    /// A short tap happened; waiting to see if a second press follows.
    TapPending { until: Instant },
    /// Second press of a double tap is held; release locks hands-free.
    SecondPress,
    /// Hands-free (or toggle) recording; the next press stops it.
    Locked,
    /// Stop requested on press; finish fires on release so modifiers are
    /// up before text gets inserted.
    Stopping,
}

/// Converts raw [`HotkeyEvent`]s into [`Intent`]s. Pure logic, so it is easy
/// to test and shared by every platform backend.
#[derive(Debug)]
pub struct Recognizer {
    pub activation: Activation,
    phase: Phase,
}

impl Recognizer {
    pub fn new(activation: Activation) -> Self {
        Recognizer {
            activation,
            phase: Phase::Idle,
        }
    }

    pub fn is_idle(&self) -> bool {
        self.phase == Phase::Idle
    }

    /// Forget any in-flight gesture (e.g. after an external stop/cancel).
    pub fn reset(&mut self) {
        self.phase = Phase::Idle;
    }

    /// Mark that the controller started a hands-free recording by other means
    /// (tray menu, D-Bus); the next chord press stops it.
    pub fn lock_external(&mut self) {
        self.phase = Phase::Locked;
    }

    /// When the recognizer is waiting on a timer, the instant it expires.
    pub fn deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::TapPending { until } => Some(until),
            _ => None,
        }
    }

    pub fn on_timer(&mut self, now: Instant) -> Option<Intent> {
        match self.phase {
            Phase::TapPending { until } if now >= until => {
                self.phase = Phase::Idle;
                Some(Intent::Cancel)
            }
            _ => None,
        }
    }

    pub fn on_event(&mut self, event: &HotkeyEvent, now: Instant) -> Option<Intent> {
        match (self.activation, self.phase, event) {
            (_, _, HotkeyEvent::Captured(_) | HotkeyEvent::Bound(_)) => None,

            // Escape cancels anything in flight.
            (_, Phase::Idle, HotkeyEvent::Escape) => None,
            (_, _, HotkeyEvent::Escape) => {
                self.phase = Phase::Idle;
                Some(Intent::Cancel)
            }

            // ---- Toggle mode ------------------------------------------------
            (Activation::Toggle, Phase::Idle, HotkeyEvent::Down) => {
                self.phase = Phase::Holding { since: now };
                Some(Intent::Start)
            }
            (Activation::Toggle, Phase::Holding { .. }, HotkeyEvent::Up) => {
                self.phase = Phase::Locked;
                Some(Intent::Lock)
            }
            (Activation::Toggle, Phase::Holding { since }, HotkeyEvent::Interrupted) => {
                self.interrupt(since, now)
            }

            // ---- Hold / HoldOrDoubleTap --------------------------------------
            (_, Phase::Idle, HotkeyEvent::Down) => {
                self.phase = Phase::Holding { since: now };
                Some(Intent::Start)
            }
            (_, Phase::Holding { since }, HotkeyEvent::Up) => {
                let held = now.saturating_duration_since(since);
                if held >= TAP_MAX {
                    self.phase = Phase::Idle;
                    Some(Intent::Finish)
                } else if self.activation == Activation::HoldOrDoubleTap {
                    self.phase = Phase::TapPending {
                        until: now + DOUBLE_TAP_WINDOW,
                    };
                    None
                } else {
                    self.phase = Phase::Idle;
                    Some(Intent::Cancel)
                }
            }
            (_, Phase::Holding { since }, HotkeyEvent::Interrupted) => self.interrupt(since, now),
            (_, Phase::TapPending { .. }, HotkeyEvent::Down) => {
                self.phase = Phase::SecondPress;
                None
            }
            (_, Phase::SecondPress, HotkeyEvent::Up) => {
                self.phase = Phase::Locked;
                Some(Intent::Lock)
            }
            (_, Phase::SecondPress, HotkeyEvent::Interrupted) => {
                self.phase = Phase::Idle;
                Some(Intent::Cancel)
            }

            // ---- Locked (hands-free / toggle) ------------------------------
            (_, Phase::Locked, HotkeyEvent::Down) => {
                self.phase = Phase::Stopping;
                None
            }
            (_, Phase::Stopping, HotkeyEvent::Up) => {
                self.phase = Phase::Idle;
                Some(Intent::Finish)
            }
            (_, Phase::Stopping, HotkeyEvent::Interrupted) => {
                // The user hit some other shortcut while hands-free; keep recording.
                self.phase = Phase::Locked;
                None
            }

            _ => None,
        }
    }

    fn interrupt(&mut self, since: Instant, now: Instant) -> Option<Intent> {
        if now.saturating_duration_since(since) < INTERRUPT_GRACE {
            self.phase = Phase::Idle;
            Some(Intent::Cancel)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn chord_roundtrip() {
        let c: Chord = "ctrl+super".parse().unwrap();
        assert_eq!(c.mods, Mods::CTRL | Mods::SUPER);
        assert!(c.key.is_none());
        let c: Chord = "Alt+Shift+space".parse().unwrap();
        assert_eq!(c.key, Some(Key("Space".into())));
        let back: Chord = c.to_string().parse().unwrap();
        assert_eq!(back, c);
        assert!("Space".parse::<Chord>().unwrap().is_valid().is_err());
        assert!("Ctrl+F9".parse::<Chord>().unwrap().is_valid().is_ok());
    }

    #[test]
    fn a_modifier_on_its_own() {
        // Either side, or one side's.
        for (text, m) in [("Ctrl", Mods::CTRL), ("Shift", Mods::SHIFT), ("Rightalt", Mods::ALT), ("Leftsuper", Mods::SUPER)] {
            let c: Chord = text.parse().unwrap();
            assert!(c.is_valid().is_ok(), "{text}");
            assert_eq!(c.lone_modifier(), Some(m), "{text}");
            assert_eq!(c.to_string().parse::<Chord>().unwrap(), c);
        }
        assert_eq!("Fn".parse::<Chord>().unwrap().lone_modifier(), None);
        assert_eq!("Ctrl+Super".parse::<Chord>().unwrap().lone_modifier(), None);
        // However it's written.
        for text in ["RightOption", "right option", "Right_Alt", "rightalt"] {
            assert_eq!(text.parse::<Chord>().unwrap().key, Some(Key("Rightalt".into())), "{text}");
        }
        assert_eq!("RightCmd".parse::<Chord>().unwrap().key, Some(Key("Rightsuper".into())));
        assert_eq!("Rightalt".parse::<Chord>().unwrap().label(), format!("Right {ALT_LABEL}"));
        assert_eq!("leftctrl".parse::<Chord>().unwrap().label(), "Left Ctrl");
        // Keys that only start like one.
        assert_eq!("Left".parse::<Chord>().unwrap().key, Some(Key("Left".into())));
        assert_eq!("Rightbracket".parse::<Chord>().unwrap().key.unwrap().modifier(), None);
        // A side only on its own.
        assert!("Ctrl+Rightalt".parse::<Chord>().unwrap().is_valid().is_err());
    }

    #[test]
    fn a_key_on_its_own_unless_it_types() {
        for ok in ["F9", "f13", "Pause", "Capslock", "Insert", "Scrolllock", "Home"] {
            let c: Chord = ok.parse().unwrap();
            assert!(c.mods.is_empty() && c.is_valid().is_ok(), "{ok}");
            assert_eq!(c.to_string().parse::<Chord>().unwrap(), c);
        }
        for typing in ["A", "7", "Space", "Enter", "Tab", "Backspace", "Delete", "Comma", "Grave", "Escape"] {
            assert!(typing.parse::<Chord>().unwrap().is_valid().is_err(), "{typing}");
        }
        // A key no backend could name, with nothing else.
        assert!(Chord { mods: Mods::empty(), key: None }.is_valid().is_err());
        assert!("Shift+A".parse::<Chord>().unwrap().is_valid().is_ok());
    }

    #[test]
    fn fn_key_alone_is_a_valid_shortcut() {
        let c: Chord = "Fn".parse().unwrap();
        assert_eq!(c.mods, Mods::FN);
        assert!(c.is_valid().is_ok());
        assert_eq!(c.to_string(), "Fn");
        let c: Chord = "globe+ctrl".parse().unwrap();
        assert_eq!(c.to_string().parse::<Chord>().unwrap(), c);
    }

    #[test]
    fn push_to_talk() {
        let t = Instant::now();
        let mut r = Recognizer::new(Activation::HoldOrDoubleTap);
        assert_eq!(r.on_event(&HotkeyEvent::Down, t), Some(Intent::Start));
        assert_eq!(r.on_event(&HotkeyEvent::Up, at(t, 1500)), Some(Intent::Finish));
        assert!(r.is_idle());
    }

    #[test]
    fn single_tap_cancels_after_window() {
        let t = Instant::now();
        let mut r = Recognizer::new(Activation::HoldOrDoubleTap);
        r.on_event(&HotkeyEvent::Down, t);
        assert_eq!(r.on_event(&HotkeyEvent::Up, at(t, 100)), None);
        assert!(r.deadline().is_some());
        assert_eq!(r.on_timer(at(t, 600)), Some(Intent::Cancel));
        assert!(r.is_idle());
    }

    #[test]
    fn double_tap_locks_then_tap_finishes() {
        let t = Instant::now();
        let mut r = Recognizer::new(Activation::HoldOrDoubleTap);
        assert_eq!(r.on_event(&HotkeyEvent::Down, t), Some(Intent::Start));
        r.on_event(&HotkeyEvent::Up, at(t, 100));
        r.on_event(&HotkeyEvent::Down, at(t, 250));
        assert_eq!(r.on_event(&HotkeyEvent::Up, at(t, 330)), Some(Intent::Lock));
        assert_eq!(r.on_event(&HotkeyEvent::Down, at(t, 9000)), None);
        assert_eq!(r.on_event(&HotkeyEvent::Up, at(t, 9100)), Some(Intent::Finish));
    }

    #[test]
    fn interrupted_shortcut_cancels() {
        let t = Instant::now();
        let mut r = Recognizer::new(Activation::HoldOrDoubleTap);
        r.on_event(&HotkeyEvent::Down, t);
        assert_eq!(
            r.on_event(&HotkeyEvent::Interrupted, at(t, 120)),
            Some(Intent::Cancel)
        );
        // A late interruption keeps recording.
        r.on_event(&HotkeyEvent::Down, at(t, 2000));
        assert_eq!(r.on_event(&HotkeyEvent::Interrupted, at(t, 4000)), None);
        assert_eq!(r.on_event(&HotkeyEvent::Up, at(t, 5000)), Some(Intent::Finish));
    }

    #[test]
    fn toggle_mode() {
        let t = Instant::now();
        let mut r = Recognizer::new(Activation::Toggle);
        assert_eq!(r.on_event(&HotkeyEvent::Down, t), Some(Intent::Start));
        assert_eq!(r.on_event(&HotkeyEvent::Up, at(t, 50)), Some(Intent::Lock));
        assert_eq!(r.on_event(&HotkeyEvent::Down, at(t, 3000)), None);
        assert_eq!(r.on_event(&HotkeyEvent::Up, at(t, 3050)), Some(Intent::Finish));
    }

    struct TestMap;
    impl KeyMap for TestMap {
        fn modifier_of(&self, code: u32) -> Option<Mods> {
            match code {
                1 => Some(Mods::CTRL),
                2 => Some(Mods::SUPER),
                3 => Some(Mods::SHIFT),
                // Right and left Alt.
                4 | 5 => Some(Mods::ALT),
                _ => None,
            }
        }
        fn name_of(&self, code: u32) -> Option<String> {
            match code {
                1 => Some("Leftctrl".into()),
                4 => Some("Rightalt".into()),
                5 => Some("Leftalt".into()),
                10 => Some("Space".into()),
                11 => Some("Left".into()),
                // Mouse buttons 1-9.
                201..=209 => Some(Key::mouse((code - 200) as u8).0),
                _ => None,
            }
        }
        fn code_of(&self, name: &str) -> Option<u32> {
            match name {
                "Leftctrl" => Some(1),
                "Rightalt" => Some(4),
                "Leftalt" => Some(5),
                "Space" => Some(10),
                "Left" => Some(11),
                _ => Key(name.into()).mouse_button().map(|b| 200 + b as u32),
            }
        }
        fn is_escape(&self, code: u32) -> bool {
            code == 99
        }
        fn mouse_button(&self, code: u32) -> Option<u8> {
            (201..=209).contains(&code).then(|| (code - 200) as u8)
        }
    }

    const LEFT_CLICK: u32 = 201;
    const MIDDLE: u32 = 203;
    const BACK: u32 = 204;

    fn chords(list: &[&str]) -> Vec<Chord> {
        list.iter().map(|c| c.parse().unwrap()).collect()
    }

    fn feed_all(t: &mut ChordTracker, keys: &mut HashSet<u32>, code: u32, down: bool, chords: &[Chord], capturing: bool) -> Tracked {
        if down {
            keys.insert(code);
        } else {
            keys.remove(&code);
        }
        t.update(&TestMap, keys, down.then_some(code), chords, capturing)
    }

    #[test]
    fn any_of_several_shortcuts_starts() {
        let all = chords(&["Ctrl+Super", "Shift+Space", "Mouse4"]);
        let (mut t, mut k) = (ChordTracker::default(), HashSet::new());
        feed_all(&mut t, &mut k, 3, true, &all, false);
        assert_eq!(feed_all(&mut t, &mut k, 10, true, &all, false).events, vec![HotkeyEvent::Down]);
        assert_eq!(feed_all(&mut t, &mut k, 10, false, &all, false).events, vec![HotkeyEvent::Up]);
        assert!(feed_all(&mut t, &mut k, 3, false, &all, false).events.is_empty());
        let down = feed_all(&mut t, &mut k, BACK, true, &all, false);
        assert_eq!(down.events, vec![HotkeyEvent::Down]);
        assert!(down.completed);
        assert_eq!(feed_all(&mut t, &mut k, BACK, false, &all, false).events, vec![HotkeyEvent::Up]);
    }

    #[test]
    fn holding_one_shortcut_into_another_goes_on() {
        let all = chords(&["Ctrl+Super", "Ctrl+Super+Space"]);
        let (mut t, mut k) = (ChordTracker::default(), HashSet::new());
        feed_all(&mut t, &mut k, 1, true, &all, false);
        assert_eq!(feed_all(&mut t, &mut k, 2, true, &all, false).events, vec![HotkeyEvent::Down]);
        // Space makes it the longer shortcut: no new events, and Space is
        // kept from the app.
        let space = feed_all(&mut t, &mut k, 10, true, &all, false);
        assert!(space.events.is_empty() && space.completed);
        assert!(feed_all(&mut t, &mut k, 10, false, &all, false).events.is_empty());
        assert_eq!(feed_all(&mut t, &mut k, 2, false, &all, false).events, vec![HotkeyEvent::Up]);
        // A key no shortcut has still interrupts.
        feed_all(&mut t, &mut k, 2, true, &all, false);
        assert_eq!(feed_all(&mut t, &mut k, 11, true, &all, false).events, vec![HotkeyEvent::Interrupted]);
    }

    #[test]
    fn clicks_never_get_in_the_way() {
        let all = chords(&["Ctrl+Super"]);
        let (mut t, mut k) = (ChordTracker::default(), HashSet::new());
        // Holding the left button (dragging) doesn't block the shortcut.
        feed_all(&mut t, &mut k, LEFT_CLICK, true, &all, false);
        feed_all(&mut t, &mut k, 1, true, &all, false);
        assert_eq!(feed_all(&mut t, &mut k, 2, true, &all, false).events, vec![HotkeyEvent::Down]);
        // Nor does a button no shortcut uses interrupt it.
        assert!(feed_all(&mut t, &mut k, MIDDLE, true, &all, false).events.is_empty());
        let mut f = KeyFilter::default();
        let input = FilterInput { chords: &all, completed_chord: true, ..Default::default() };
        assert!(!f.should_drop(&TestMap, MIDDLE, KeyChange::Press, &input));
    }

    #[test]
    fn records_a_mouse_button_but_never_a_click() {
        let all = chords(&["Ctrl+Super"]);
        let (mut t, mut k) = (ChordTracker::default(), HashSet::new());
        // The click that started recording, then Ctrl + the back button.
        assert!(feed_all(&mut t, &mut k, LEFT_CLICK, true, &all, true).events.is_empty());
        assert!(feed_all(&mut t, &mut k, LEFT_CLICK, false, &all, true).events.is_empty());
        feed_all(&mut t, &mut k, 1, true, &all, true);
        feed_all(&mut t, &mut k, BACK, true, &all, true);
        feed_all(&mut t, &mut k, BACK, false, &all, true);
        let done = feed_all(&mut t, &mut k, 1, false, &all, true);
        assert_eq!(done.events, vec![HotkeyEvent::Captured("Ctrl+Mouse4".parse().unwrap())]);
        // While recording, the button is kept from the app; clicks never are.
        let mut f = KeyFilter::default();
        let cap = FilterInput { capturing: true, ..Default::default() };
        assert!(f.should_drop(&TestMap, BACK, KeyChange::Press, &cap));
        assert!(!f.should_drop(&TestMap, LEFT_CLICK, KeyChange::Press, &cap));
    }

    #[test]
    fn mouse_shortcuts() {
        let back: Chord = "Mouse4".parse().unwrap();
        assert_eq!(back.mouse_button(), Some(4));
        assert!(back.is_valid().is_ok());
        assert!("Ctrl+Mouse3".parse::<Chord>().unwrap().is_valid().is_ok());
        assert!("Mouse1".parse::<Chord>().unwrap().is_valid().is_err());
        assert!("Ctrl+Mouse2".parse::<Chord>().unwrap().is_valid().is_err());
        assert_eq!(back.to_string(), "Mouse4");
        assert_eq!("ctrl+mouse5".parse::<Chord>().unwrap().label(), "Ctrl + Mouse 5");
        assert_eq!(Key::mouse(3).label(), "Mouse 3");
        assert!(wants_mouse(&[back], false) && wants_mouse(&[], true) && !wants_mouse(&chords(&["Ctrl+Super"]), false));
    }

    fn feed(t: &mut ChordTracker, keys: &mut std::collections::HashSet<u32>, code: u32, down: bool, chord: &Chord, capturing: bool) -> Vec<HotkeyEvent> {
        if down {
            keys.insert(code);
        } else {
            keys.remove(&code);
        }
        t.update(&TestMap, keys, down.then_some(code), std::slice::from_ref(chord), capturing).events
    }

    #[test]
    fn tracker_modifier_only_chord() {
        let chord: Chord = "Ctrl+Super".parse().unwrap();
        let (mut t, mut k) = (ChordTracker::default(), Default::default());
        assert!(feed(&mut t, &mut k, 1, true, &chord, false).is_empty());
        assert_eq!(feed(&mut t, &mut k, 2, true, &chord, false), vec![HotkeyEvent::Down]);
        assert_eq!(feed(&mut t, &mut k, 2, false, &chord, false), vec![HotkeyEvent::Up]);
        assert!(feed(&mut t, &mut k, 1, false, &chord, false).is_empty());
    }

    #[test]
    fn tracker_interruption_then_release() {
        let chord: Chord = "Ctrl+Super".parse().unwrap();
        let (mut t, mut k) = (ChordTracker::default(), Default::default());
        feed(&mut t, &mut k, 1, true, &chord, false);
        feed(&mut t, &mut k, 2, true, &chord, false);
        // Ctrl+Super+Left: another shortcut.
        assert_eq!(feed(&mut t, &mut k, 11, true, &chord, false), vec![HotkeyEvent::Interrupted]);
        assert!(feed(&mut t, &mut k, 11, false, &chord, false).is_empty());
        assert_eq!(feed(&mut t, &mut k, 1, false, &chord, false), vec![HotkeyEvent::Up]);
    }

    #[test]
    fn tracker_key_chord_and_capture() {
        let chord: Chord = "Ctrl+Space".parse().unwrap();
        let (mut t, mut k) = (ChordTracker::default(), Default::default());
        feed(&mut t, &mut k, 1, true, &chord, false);
        assert_eq!(feed(&mut t, &mut k, 10, true, &chord, false), vec![HotkeyEvent::Down]);
        assert_eq!(feed(&mut t, &mut k, 10, false, &chord, false), vec![HotkeyEvent::Up]);
        feed(&mut t, &mut k, 1, false, &chord, false);
        // Capture Shift+Super+Space.
        feed(&mut t, &mut k, 3, true, &chord, true);
        feed(&mut t, &mut k, 2, true, &chord, true);
        feed(&mut t, &mut k, 10, true, &chord, true);
        feed(&mut t, &mut k, 10, false, &chord, true);
        feed(&mut t, &mut k, 2, false, &chord, true);
        let ev = feed(&mut t, &mut k, 3, false, &chord, true);
        assert_eq!(ev, vec![HotkeyEvent::Captured("Shift+Super+Space".parse().unwrap())]);
    }

    #[test]
    fn a_key_without_a_name_records_nothing() {
        let chord: Chord = "Ctrl+Space".parse().unwrap();
        let (mut t, mut k) = (ChordTracker::default(), Default::default());
        // Ctrl with a key the map has no name for (50).
        feed(&mut t, &mut k, 1, true, &chord, true);
        feed(&mut t, &mut k, 50, true, &chord, true);
        feed(&mut t, &mut k, 50, false, &chord, true);
        let ev = feed(&mut t, &mut k, 1, false, &chord, true);
        // Not Ctrl on its own, which would be a valid shortcut.
        let [HotkeyEvent::Captured(captured)] = ev.as_slice() else { panic!("{ev:?}") };
        assert!(captured.parts().is_empty() && captured.is_valid().is_err(), "{captured:?}");
    }

    #[test]
    fn tracker_key_on_its_own() {
        let chord: Chord = "Left".parse().unwrap();
        let (mut t, mut k) = (ChordTracker::default(), Default::default());
        assert_eq!(feed(&mut t, &mut k, 11, true, &chord, false), vec![HotkeyEvent::Down]);
        assert_eq!(feed(&mut t, &mut k, 11, false, &chord, false), vec![HotkeyEvent::Up]);
        // With a modifier it's another shortcut, left to the app.
        feed(&mut t, &mut k, 3, true, &chord, false);
        assert!(feed(&mut t, &mut k, 11, true, &chord, false).is_empty());
        feed(&mut t, &mut k, 11, false, &chord, false);
        feed(&mut t, &mut k, 3, false, &chord, false);
        // A modifier pressed while it's held interrupts it.
        feed(&mut t, &mut k, 11, true, &chord, false);
        assert_eq!(feed(&mut t, &mut k, 1, true, &chord, false), vec![HotkeyEvent::Interrupted]);
        feed(&mut t, &mut k, 1, false, &chord, false);
        assert_eq!(feed(&mut t, &mut k, 11, false, &chord, false), vec![HotkeyEvent::Up]);
        // Recorded on its own, and kept from the app.
        feed(&mut t, &mut k, 11, true, &chord, true);
        assert_eq!(feed(&mut t, &mut k, 11, false, &chord, true), vec![HotkeyEvent::Captured(chord.clone())]);
        let mut f = KeyFilter::default();
        let done = FilterInput { chords: std::slice::from_ref(&chord), completed_chord: true, ..Default::default() };
        assert!(f.should_drop(&TestMap, 11, KeyChange::Press, &done));
        assert!(f.should_drop(&TestMap, 11, KeyChange::Repeat, &FilterInput::default()));
    }

    #[test]
    fn tracker_one_sides_modifier() {
        const RIGHT_ALT: u32 = 4;
        const LEFT_ALT: u32 = 5;
        let chord: Chord = "Rightalt".parse().unwrap();
        let (mut t, mut k) = (ChordTracker::default(), Default::default());
        assert_eq!(feed(&mut t, &mut k, RIGHT_ALT, true, &chord, false), vec![HotkeyEvent::Down]);
        assert_eq!(feed(&mut t, &mut k, RIGHT_ALT, false, &chord, false), vec![HotkeyEvent::Up]);
        // The other side, or in another shortcut, it's left alone.
        assert!(feed(&mut t, &mut k, LEFT_ALT, true, &chord, false).is_empty());
        assert!(feed(&mut t, &mut k, LEFT_ALT, false, &chord, false).is_empty());
        feed(&mut t, &mut k, 1, true, &chord, false);
        assert!(feed(&mut t, &mut k, RIGHT_ALT, true, &chord, false).is_empty());
        feed(&mut t, &mut k, RIGHT_ALT, false, &chord, false);
        feed(&mut t, &mut k, 1, false, &chord, false);
        // Another key while it's held interrupts it.
        feed(&mut t, &mut k, RIGHT_ALT, true, &chord, false);
        assert_eq!(feed(&mut t, &mut k, 11, true, &chord, false), vec![HotkeyEvent::Interrupted]);
        feed(&mut t, &mut k, 11, false, &chord, false);
        assert_eq!(feed(&mut t, &mut k, RIGHT_ALT, false, &chord, false), vec![HotkeyEvent::Up]);
        // Either side's.
        let either: Chord = "Alt".parse().unwrap();
        assert_eq!(feed(&mut t, &mut k, LEFT_ALT, true, &either, false), vec![HotkeyEvent::Down]);
        assert_eq!(feed(&mut t, &mut k, LEFT_ALT, false, &either, false), vec![HotkeyEvent::Up]);
        // Recorded with its side; two modifiers aren't.
        feed(&mut t, &mut k, RIGHT_ALT, true, &chord, true);
        assert_eq!(feed(&mut t, &mut k, RIGHT_ALT, false, &chord, true), vec![HotkeyEvent::Captured(chord.clone())]);
        feed(&mut t, &mut k, 1, true, &chord, true);
        feed(&mut t, &mut k, RIGHT_ALT, true, &chord, true);
        feed(&mut t, &mut k, RIGHT_ALT, false, &chord, true);
        let both = feed(&mut t, &mut k, 1, false, &chord, true);
        assert_eq!(both, vec![HotkeyEvent::Captured("Ctrl+Alt".parse().unwrap())]);
        // Modifiers still reach the app.
        let mut f = KeyFilter::default();
        let done = FilterInput { chords: std::slice::from_ref(&chord), completed_chord: true, ..Default::default() };
        assert!(!f.should_drop(&TestMap, RIGHT_ALT, KeyChange::Press, &done));
    }

    #[test]
    fn filter_drops_escape_only_while_recording() {
        let chord: Chord = "Ctrl+Super".parse().unwrap();
        let mut f = KeyFilter::default();
        let idle = FilterInput { chords: std::slice::from_ref(&chord), ..Default::default() };
        assert!(!f.should_drop(&TestMap, 99, KeyChange::Press, &idle));
        assert!(!f.should_drop(&TestMap, 99, KeyChange::Release, &idle));
        let rec = FilterInput { chords: std::slice::from_ref(&chord), recording: true, ..Default::default() };
        assert!(f.should_drop(&TestMap, 99, KeyChange::Press, &rec));
        // Its autorepeat and release go too, even once recording stopped.
        assert!(f.should_drop(&TestMap, 99, KeyChange::Repeat, &idle));
        assert!(f.should_drop(&TestMap, 99, KeyChange::Release, &idle));
        assert!(!f.should_drop(&TestMap, 99, KeyChange::Release, &idle));
        // A press is decided afresh even if a release was missed.
        assert!(f.should_drop(&TestMap, 99, KeyChange::Press, &rec));
        assert!(!f.should_drop(&TestMap, 99, KeyChange::Press, &idle));
    }

    #[test]
    fn filter_drops_the_key_completing_the_chord_but_never_modifiers() {
        let chord: Chord = "Ctrl+Space".parse().unwrap();
        let mut f = KeyFilter::default();
        let done = FilterInput { chords: std::slice::from_ref(&chord), completed_chord: true, ..Default::default() };
        assert!(!f.should_drop(&TestMap, 1, KeyChange::Press, &done));
        assert!(f.should_drop(&TestMap, 10, KeyChange::Press, &done));
        assert!(f.should_drop(&TestMap, 10, KeyChange::Release, &FilterInput::default()));
        assert!(!f.should_drop(&TestMap, 1, KeyChange::Release, &FilterInput::default()));
        // Recording a new shortcut keeps its keys from the app too.
        let cap = FilterInput { capturing: true, ..Default::default() };
        assert!(f.should_drop(&TestMap, 11, KeyChange::Press, &cap));
        assert!(!f.should_drop(&TestMap, 3, KeyChange::Press, &cap));
    }

    #[test]
    fn filter_drops_fn_only_when_it_is_the_whole_shortcut() {
        struct FnMap;
        impl KeyMap for FnMap {
            fn modifier_of(&self, code: u32) -> Option<Mods> {
                match code {
                    7 => Some(Mods::FN),
                    1 => Some(Mods::CTRL),
                    _ => None,
                }
            }
            fn name_of(&self, _: u32) -> Option<String> {
                None
            }
            fn code_of(&self, _: &str) -> Option<u32> {
                None
            }
            fn is_escape(&self, _: u32) -> bool {
                false
            }
        }
        let mut f = KeyFilter::default();
        let fn_only: Chord = "Fn".parse().unwrap();
        let fn_ctrl: Chord = "Fn+Ctrl".parse().unwrap();
        let press = KeyChange::Press;
        assert!(f.should_drop(&FnMap, 7, press, &FilterInput { chords: std::slice::from_ref(&fn_only), ..Default::default() }));
        assert!(f.should_drop(&FnMap, 7, KeyChange::Release, &FilterInput::default()));
        assert!(!f.should_drop(&FnMap, 7, press, &FilterInput { chords: std::slice::from_ref(&fn_ctrl), ..Default::default() }));
        assert!(!f.should_drop(&FnMap, 1, press, &FilterInput { chords: std::slice::from_ref(&fn_only), ..Default::default() }));
    }

    #[test]
    fn held_keys_drop_only_old_keys_the_os_says_are_up() {
        let mut h = HeldKeys::default();
        assert!(h.press(1));
        assert!(!h.press(1));
        h.press(2);
        // Too young to check.
        assert!(h.drop_stale(Duration::from_secs(60), |_| false).is_empty());
        // Old enough: 2 is up, 1 is really held.
        assert_eq!(h.drop_stale(Duration::ZERO, |c| c == 1), vec![2]);
        assert!(h.contains(1) && !h.contains(2));
    }

    #[test]
    fn escape_cancels() {
        let t = Instant::now();
        let mut r = Recognizer::new(Activation::HoldOrDoubleTap);
        r.on_event(&HotkeyEvent::Down, t);
        r.on_event(&HotkeyEvent::Up, at(t, 50));
        r.on_event(&HotkeyEvent::Down, at(t, 100));
        r.on_event(&HotkeyEvent::Up, at(t, 150));
        assert_eq!(r.on_event(&HotkeyEvent::Escape, at(t, 2000)), Some(Intent::Cancel));
    }
}
