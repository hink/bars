//! User configuration, persisted as TOML.

use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::hotkey::{Activation, Chord};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub hotkey: HotkeyConfig,
    pub audio: AudioConfig,
    pub model: ModelConfig,
    pub insertion: InsertionConfig,
    pub text: TextConfig,
    pub ui: UiConfig,
    /// Set once the first-run setup has been completed.
    pub onboarded: bool,
    /// The output polish model was offered: in setup, or once to people
    /// who had Bars before it.
    pub polish_offered: bool,
    /// Recommended models (speech or polish) already suggested to people
    /// using another one: each is suggested once.
    pub models_offered: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    /// Every shortcut that starts dictation (keys or mouse buttons). Older
    /// configs have the one `chord`.
    #[serde(alias = "chord", deserialize_with = "one_or_more")]
    pub chords: Vec<Chord>,
    /// How all of them work.
    pub activation: Activation,
    pub backend: HotkeyBackendKind,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        HotkeyConfig {
            chords: vec![Chord::default()],
            activation: Activation::default(),
            backend: HotkeyBackendKind::Auto,
        }
    }
}

impl HotkeyConfig {
    /// The shortcut to show when there's room for one.
    pub fn first(&self) -> Option<&Chord> {
        self.chords.first()
    }

    /// There is always at least one shortcut that can work: ones that
    /// can't (a config edited by hand) and repeats are dropped, and with
    /// none left the default comes back.
    pub fn keep_a_shortcut(&mut self) {
        let mut kept: Vec<Chord> = Vec::new();
        for chord in std::mem::take(&mut self.chords) {
            match chord.is_valid() {
                Err(e) => log::warn!("ignoring shortcut {chord}: {e}"),
                Ok(()) if !kept.contains(&chord) => kept.push(chord),
                Ok(()) => {}
            }
        }
        if kept.is_empty() {
            log::warn!("no usable shortcut; using {}", Chord::default());
            kept.push(Chord::default());
        }
        self.chords = kept;
    }
}

/// A list of shortcuts, or (older configs) a single one; never none.
fn one_or_more<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Chord>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMore {
        One(Chord),
        More(Vec<Chord>),
    }
    let mut hotkey = HotkeyConfig {
        chords: match OneOrMore::deserialize(d)? {
            OneOrMore::One(c) => vec![c],
            OneOrMore::More(v) => v,
        },
        ..Default::default()
    };
    hotkey.keep_a_shortcut();
    Ok(hotkey.chords)
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HotkeyBackendKind {
    /// Pick the best available backend for this system.
    #[default]
    Auto,
    /// Read keyboards directly (Linux evdev). Supports modifier-only chords
    /// and works on every compositor, but needs input-device access.
    Direct,
    /// XDG GlobalShortcuts portal. Sandbox-friendly; the desktop owns the binding.
    Portal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Input device id; `None` follows the system default.
    pub device: Option<String>,
    /// Keep the microphone open between dictations for zero start latency
    /// (also enables a short pre-roll so the first syllable is never clipped).
    pub keep_warm: bool,
    pub sounds: bool,
    pub sound_volume: f32,
    /// Mute other audio while dictating (see `platform::mute`).
    pub mute_others: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        AudioConfig {
            device: None,
            keep_warm: false,
            sounds: true,
            sound_volume: 0.35,
            mute_others: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub id: String,
    /// BCP-47 language hint; `None` = auto-detect where supported.
    pub language: Option<String>,
    /// Free the model from memory after this many idle minutes (0 = never).
    pub unload_after_mins: u32,
}

impl Default for ModelConfig {
    fn default() -> Self {
        ModelConfig {
            id: crate::models::DEFAULT_MODEL.to_string(),
            language: None,
            unload_after_mins: 0,
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InsertMethod {
    /// Paste through the clipboard when available, otherwise type.
    #[default]
    Auto,
    /// Put text on the clipboard and send the paste shortcut. Instant, and
    /// handles every character, but briefly replaces the clipboard.
    Paste,
    /// Type the text through a virtual keyboard.
    Type,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PasteKeys {
    /// Shift+Insert, with the text placed on both the clipboard and the
    /// primary selection. Works in GUI apps *and* terminals (Linux).
    ShiftInsert,
    /// Ctrl+V (Cmd+V on macOS).
    CtrlV,
    CtrlShiftV,
}

impl Default for PasteKeys {
    fn default() -> Self {
        if cfg!(target_os = "linux") {
            PasteKeys::ShiftInsert
        } else {
            PasteKeys::CtrlV
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InsertionConfig {
    pub method: InsertMethod,
    pub paste_keys: PasteKeys,
    /// Put the previous clipboard contents back after pasting.
    pub restore_clipboard: bool,
    /// Append a space so consecutive dictations flow together (with smart
    /// spacing: only at the end of a line).
    pub trailing_space: bool,
    /// Read the text around the cursor (accessibility) to fit spacing,
    /// capitalization and the final full stop into the sentence.
    pub smart_spacing: bool,
    /// Paste long text into a terminal coding agent (Claude Code, Codex,
    /// Droid, pi, OpenCode) in pieces, so it shows as text instead of
    /// "[Pasted text]".
    pub split_for_agents: bool,
}

impl Default for InsertionConfig {
    fn default() -> Self {
        InsertionConfig {
            method: InsertMethod::Auto,
            paste_keys: PasteKeys::default(),
            restore_clipboard: true,
            trailing_space: true,
            smart_spacing: true,
            split_for_agents: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Replacement {
    /// What the model tends to hear ("chat gee pee tee").
    pub from: String,
    /// What should be written ("ChatGPT").
    pub to: String,
    /// Learned from a correction rather than added by hand.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub learned: bool,
}

/// A correction seen too few times (or too unclear) to learn outright.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub from: String,
    pub to: String,
    pub seen: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snippet {
    /// Spoken cue ("my email address").
    pub trigger: String,
    /// Inserted text.
    pub expansion: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextConfig {
    pub remove_fillers: bool,
    /// Turn "new line" / "new paragraph" into line breaks.
    pub voice_commands: bool,
    pub dictionary: Vec<Replacement>,
    pub snippets: Vec<Snippet>,
    /// Learn dictionary words from corrections made after dictating
    /// (Parakeet models only).
    pub learn: bool,
    /// Polish English dictations with a small local model (see `polish`):
    /// false starts and spoken corrections resolved.
    pub polish: bool,
    /// The polish model's id (see `models::POLISH_MODELS`). Empty in a
    /// config from before there was a choice (see `settle_polish_model`).
    #[serde(default)]
    pub polish_model: String,
    pub suggestions: Vec<Suggestion>,
    /// Corrections undone or dismissed: never suggested again.
    pub ignored: Vec<Replacement>,
}

impl Default for TextConfig {
    fn default() -> Self {
        TextConfig {
            remove_fillers: true,
            voice_commands: true,
            dictionary: Vec::new(),
            snippets: Vec::new(),
            learn: true,
            polish: true,
            polish_model: crate::models::DEFAULT_POLISH.to_string(),
            suggestions: Vec::new(),
            ignored: Vec::new(),
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OverlayMode {
    /// Show the pill only while recording/transcribing.
    #[default]
    WhileActive,
    /// Keep a tiny idle pill on screen at all times.
    Always,
    Never,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub overlay: OverlayMode,
    pub theme: ThemeMode,
    pub launch_at_login: bool,
    pub save_history: bool,
    /// Look for a newer Bars once a day (see `update`).
    pub check_updates: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            overlay: OverlayMode::WhileActive,
            theme: ThemeMode::System,
            launch_at_login: true,
            save_history: true,
            check_updates: true,
        }
    }
}

impl Config {
    pub fn load_or_default(path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(mut cfg) => {
                    cfg.settle_polish_model(crate::models::SPOKENLY.is_installed());
                    cfg
                }
                Err(e) => {
                    log::error!("config at {} is invalid ({e}); using defaults", path.display());
                    let _ = std::fs::rename(path, path.with_extension("toml.bad"));
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    /// A config from before there was a choice of polish model names none.
    /// It keeps Spokenly Cleanup 1 where that's downloaded (the default is
    /// offered once instead), and starts on the default otherwise.
    fn settle_polish_model(&mut self, spokenly_installed: bool) {
        if self.text.polish_model.is_empty() {
            let id = if spokenly_installed { crate::models::SPOKENLY.id } else { crate::models::DEFAULT_POLISH };
            self.text.polish_model = id.to_string();
        }
    }

    /// Atomic save (write to a temp file, then rename).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut cfg = Config::default();
        cfg.text.dictionary.push(Replacement {
            from: "example".into(),
            to: "Example".into(),
            learned: true,
        });
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back, cfg);
        assert!(s.contains(&format!("chords = [\"{}\"]", Chord::default())));
    }

    #[test]
    fn older_configs_keep_spokenly_if_they_have_it() {
        use crate::models::{DEFAULT_POLISH, SPOKENLY};
        assert_eq!(Config::default().text.polish_model, DEFAULT_POLISH);
        // Written before there was a choice: no polish model.
        let old = "[text]\npolish = true\n";
        for (installed, expected) in [(true, SPOKENLY.id), (false, DEFAULT_POLISH)] {
            let mut cfg: Config = toml::from_str(old).unwrap();
            assert_eq!(cfg.text.polish_model, "");
            cfg.settle_polish_model(installed);
            assert_eq!(cfg.text.polish_model, expected);
        }
        // A choice made stays.
        let mut cfg: Config = toml::from_str("[text]\npolish_model = \"spokenly-cleanup-1\"\n").unwrap();
        cfg.settle_polish_model(false);
        assert_eq!(cfg.text.polish_model, SPOKENLY.id);
    }

    #[test]
    fn several_shortcuts_and_mouse_buttons_roundtrip() {
        let cfg: Config = toml::from_str("[hotkey]\nchords = [\"Ctrl+Option\", \"Mouse4\"]\n").unwrap();
        assert_eq!(cfg.hotkey.chords.len(), 2);
        assert_eq!(cfg.hotkey.chords[1].mouse_button(), Some(4));
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back.hotkey, cfg.hotkey);
    }

    #[test]
    fn always_keeps_a_shortcut() {
        let chords = |toml: &str| toml::from_str::<Config>(&format!("[hotkey]\n{toml}\n")).unwrap().hotkey.chords;
        assert_eq!(chords("chords = []"), vec![Chord::default()]);
        // Ones that can't work (left click, a typing key alone) don't count.
        assert_eq!(chords("chords = [\"Mouse1\", \"K\"]"), vec![Chord::default()]);
        assert_eq!(
            chords("chords = [\"Ctrl+Shift+K\", \"Mouse1\", \"Ctrl+Shift+K\"]"),
            vec!["Ctrl+Shift+K".parse::<Chord>().unwrap()]
        );
        let mut cfg = HotkeyConfig { chords: Vec::new(), ..Default::default() };
        cfg.keep_a_shortcut();
        assert_eq!(cfg.chords, vec![Chord::default()]);
    }

    #[test]
    fn partial_file_fills_defaults() {
        let cfg: Config = toml::from_str("[hotkey]\nchord = \"Alt+Space\"\n").unwrap();
        assert_eq!(cfg.hotkey.chords, vec!["Alt+Space".parse::<Chord>().unwrap()]);
        assert_eq!(cfg.audio, AudioConfig::default());
    }
}
