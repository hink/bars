//! UI-side mirror of the controller's state. The controller thread is the
//! source of truth; this entity renders its events and forwards edits back.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::*;
use bars_core::audio::{self, InputDevice};
use bars_core::config::Config;
use bars_core::controller::{Command, ControllerHandle, Event, NoticeKind, Phase};
use bars_core::engine::EngineStatus;
use bars_core::history::{Entry, Stats};
use bars_core::hotkey::Chord;
use bars_core::models;
use bars_core::platform::Permissions;
use bars_core::polish::PolishStatus;
use bars_core::update::{self, Release};

/// Mic level, kept separate so 60 Hz updates only re-render the overlay.
pub struct LevelMeter {
    pub level: f32,
    pub updated: Instant,
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub kind: NoticeKind,
    pub text: SharedString,
    pub at: Instant,
}

/// Emitted for window-level reactions (overlay visibility, etc).
#[derive(Clone, Debug)]
pub enum StateEvent {
    PhaseChanged(Phase),
    Notice,
    /// A paste landed nowhere; `AppState::paste_failed` has the text.
    PasteFailed,
    /// A correction was learned; `AppState::learned` has the pair.
    Learned,
    ThemeChanged,
    /// A newer release was found; `AppState::update` has it.
    UpdateFound,
    /// A check the user asked for found nothing, or installing failed.
    UpdateResult,
    /// The update is in place: Windows' installer takes over from here,
    /// macOS restarts into it, and Linux asks whether to restart now.
    UpdateInstalled,
    /// The user chose to restart into the installed update (once no
    /// dictation is under way).
    RestartToUpdate,
    /// macOS: audio needs a fresh start of Bars.
    Restart,
}

/// Where looking for a newer Bars stands.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateStatus {
    /// Not looked yet this session.
    Unknown,
    Checking,
    UpToDate,
    Available(Release),
    Installing(Release),
    /// Installing failed; it can be tried again.
    InstallFailed(Release, String),
    /// In place; it runs once Bars restarts.
    Installed(Release),
    Failed(String),
}

impl UpdateStatus {
    /// The newer release on offer: found, installing, to try again, or to
    /// restart into.
    pub fn offered(&self) -> Option<&Release> {
        match self {
            UpdateStatus::Available(r)
            | UpdateStatus::Installing(r)
            | UpdateStatus::InstallFailed(r, _)
            | UpdateStatus::Installed(r) => Some(r),
            _ => None,
        }
    }
}

pub struct AppState {
    pub controller: ControllerHandle,
    pub config: Config,
    pub phase: Phase,
    pub engine: EngineStatus,
    pub downloads: HashMap<String, (u64, u64)>,
    pub download_errors: HashMap<String, String>,
    pub installed: HashSet<&'static str>,
    pub history: Vec<Entry>,
    pub stats: Stats,
    pub permissions: Permissions,
    pub hotkeys_active: bool,
    pub hotkey_error: Option<String>,
    pub hotkey_backend: &'static str,
    pub desktop_shortcut: bool,
    pub desktop_trigger: Option<String>,
    pub capturing: bool,
    /// The shortcut a recording replaces; None adds one.
    pub capture_slot: Option<usize>,
    pub capture_error: Option<String>,
    pub notice: Option<Notice>,
    /// The text of the last paste that landed nowhere, to copy.
    pub paste_failed: Option<String>,
    /// macOS: the app whose Secure Keyboard Entry blocks the shortcut.
    pub secure_input: Option<String>,
    /// The last correction learned (from, to), offered to undo.
    pub learned: Option<(String, String)>,
    /// The selected model supports learning from corrections.
    pub learning_available: bool,
    pub polish: PolishStatus,
    pub devices: Vec<InputDevice>,
    /// The latest dictation's text and how many finished this session
    /// (kept even when history isn't saved).
    pub last_transcript: Option<String>,
    pub transcripts: u64,
    pub level: Entity<LevelMeter>,
    pub update: UpdateStatus,
    /// The recommended model being downloaded to switch to (see
    /// `model_offer`).
    pub model_switch: Option<String>,
    /// The newest version already announced, so a daily check doesn't
    /// announce it again.
    update_announced: Option<String>,
    /// When looking for an update last worked, and was last tried. By the
    /// clock: the time a computer sleeps counts too.
    update_checked: Option<SystemTime>,
    update_tried: Option<SystemTime>,
}

/// How often Bars looks for a newer version while it's running, and how
/// soon it tries again when it couldn't.
const UPDATE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const UPDATE_RETRY: Duration = Duration::from_secs(60 * 60);

impl EventEmitter<StateEvent> for AppState {}

impl AppState {
    pub fn new(controller: ControllerHandle, config: Config, cx: &mut Context<Self>) -> Self {
        AppState {
            controller,
            config,
            phase: Phase::Idle,
            engine: EngineStatus::Loading(String::new()),
            downloads: HashMap::new(),
            download_errors: HashMap::new(),
            installed: models::installed().into_iter().collect(),
            history: Vec::new(),
            stats: Stats::default(),
            permissions: Permissions::default(),
            hotkeys_active: false,
            hotkey_error: None,
            hotkey_backend: "none",
            desktop_shortcut: false,
            desktop_trigger: None,
            capturing: false,
            capture_slot: None,
            capture_error: None,
            notice: None,
            paste_failed: None,
            secure_input: None,
            learned: None,
            learning_available: false,
            polish: PolishStatus::Off,
            // Filled in off the UI thread (see refresh_devices).
            devices: Vec::new(),
            last_transcript: None,
            transcripts: 0,
            level: cx.new(|_| LevelMeter {
                level: 0.0,
                updated: Instant::now(),
            }),
            update: UpdateStatus::Unknown,
            model_switch: None,
            update_announced: None,
            update_checked: None,
            update_tried: None,
        }
    }

    /// Looks for a newer release in the background. `asked`: the user asked,
    /// so the outcome is always reported; otherwise only a version not yet
    /// announced is.
    pub fn check_for_updates(&mut self, asked: bool, cx: &mut Context<Self>) {
        // Installed: this copy is still the old version, so a look would offer
        // the update again.
        if matches!(self.update, UpdateStatus::Checking | UpdateStatus::Installing(_) | UpdateStatus::Installed(_)) {
            return;
        }
        self.update_tried = Some(SystemTime::now());
        self.update = UpdateStatus::Checking;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async { update::check() }).await;
            let _ = this.update(cx, |s, cx| {
                if result.is_ok() {
                    s.update_checked = Some(SystemTime::now());
                }
                match result {
                    Ok(Some(release)) => {
                        log::info!("Bars {} is available", release.version);
                        let new = s.update_announced.as_deref() != Some(release.version.as_str());
                        s.update_announced = Some(release.version.clone());
                        s.update = UpdateStatus::Available(release);
                        if asked || new {
                            cx.emit(StateEvent::UpdateFound);
                        }
                    }
                    Ok(None) => {
                        s.update = UpdateStatus::UpToDate;
                        if asked {
                            cx.emit(StateEvent::UpdateResult);
                        }
                    }
                    Err(e) => {
                        log::warn!("update check failed: {e:#}");
                        s.update = UpdateStatus::Failed("Couldn't check for updates.".into());
                        if asked {
                            cx.emit(StateEvent::UpdateResult);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Looks for an update if 24 hours have passed since the last look (an
    /// hour, if it failed), or there hasn't been one yet.
    pub fn check_for_updates_if_due(&mut self, cx: &mut Context<Self>) {
        let now = SystemTime::now();
        // A time in the future (the clock was set back) counts as long ago.
        let waited = |since: Option<SystemTime>, wait: Duration| since.and_then(|t| now.duration_since(t).ok()).is_none_or(|d| d >= wait);
        if waited(self.update_checked, UPDATE_EVERY) && waited(self.update_tried, UPDATE_RETRY) {
            self.check_for_updates(false, cx);
        }
    }

    /// Installs the available update, restarts into an installed one, or
    /// opens the release page where this copy can't update itself.
    pub fn install_update(&mut self, cx: &mut Context<Self>) {
        let release = match &self.update {
            UpdateStatus::Available(r) | UpdateStatus::InstallFailed(r, _) => r.clone(),
            UpdateStatus::Installed(_) => {
                cx.emit(StateEvent::RestartToUpdate);
                return;
            }
            _ => return,
        };
        if !release.installable() {
            self.send(Command::OpenUrl(update::RELEASE_PAGE.into()));
            return;
        }
        self.update = UpdateStatus::Installing(release.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            // The installer's blocking HTTP client needs a thread of its own.
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let _ = tx.send_blocking(update::install());
            });
            let result = rx.recv().await.unwrap_or_else(|_| Err(anyhow::anyhow!("the installer stopped")));
            let _ = this.update(cx, |s, cx| {
                match result {
                    // The newest release, which may be newer than the one
                    // offered.
                    Ok(version) => {
                        let mut installed = release;
                        installed.version = version;
                        s.update = UpdateStatus::Installed(installed);
                        cx.emit(StateEvent::UpdateInstalled);
                    }
                    Err(e) => {
                        log::error!("installing Bars {} failed: {e:#}", release.version);
                        let why = format!("Couldn't install ver. {}.", release.version);
                        s.update = UpdateStatus::InstallFailed(release, why);
                        cx.emit(StateEvent::UpdateResult);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn send(&self, cmd: Command) {
        self.controller.send(cmd);
    }

    /// Edit the config locally and push it to the controller (which saves it).
    pub fn edit_config(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Config)) {
        let theme_before = self.config.ui.theme;
        f(&mut self.config);
        // As the controller does: a shortcut always stays.
        self.config.hotkey.keep_a_shortcut();
        self.send(Command::SetConfig(Box::new(self.config.clone())));
        if self.config.ui.theme != theme_before {
            cx.emit(StateEvent::ThemeChanged);
        }
        cx.notify();
    }

    /// Microphone choices for a picker: the system default (naming the
    /// device it currently is), then every input device.
    pub fn mic_options(&self) -> Vec<(Option<String>, String)> {
        let default = match self.devices.iter().find(|d| d.is_default) {
            Some(d) => format!("System default ({})", d.name),
            None => "System default".to_string(),
        };
        std::iter::once((None, default))
            .chain(self.devices.iter().map(|d| (Some(d.id.clone()), d.name.clone())))
            .collect()
    }

    /// Re-reads the microphones in the background: on macOS, device queries
    /// can block for seconds while a device reconfigures (e.g. a Bluetooth
    /// headset switching modes), which must never stall the UI thread.
    pub fn refresh_devices(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let devices = cx.background_executor().spawn(async { audio::list_input_devices() }).await;
            let _ = this.update(cx, |s, cx| {
                if s.devices != devices {
                    s.devices = devices;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Records a shortcut: in place of the one at `slot`, or a new one.
    pub fn start_capture(&mut self, slot: Option<usize>, cx: &mut Context<Self>) {
        self.capturing = true;
        self.capture_slot = slot;
        self.capture_error = None;
        self.send(Command::CaptureHotkey(true));
        cx.notify();
    }

    pub fn stop_capture(&mut self, cx: &mut Context<Self>) {
        self.capturing = false;
        self.send(Command::CaptureHotkey(false));
        cx.notify();
    }

    fn on_captured(&mut self, chord: Chord, cx: &mut Context<Self>) {
        self.capturing = false;
        let slot = self.capture_slot.take();
        let taken = self.config.hotkey.chords.iter().enumerate().any(|(i, c)| *c == chord && Some(i) != slot);
        let problem = match chord.is_valid() {
            Err(e) if chord.parts().is_empty() => Some(e.to_string()),
            Err(e) => Some(format!("{e} (pressed {})", chord.label())),
            Ok(()) if taken => Some(format!("{} is already a shortcut", chord.label())),
            Ok(()) => None,
        };
        if let Some(e) = problem {
            self.capture_error = Some(e);
            cx.notify();
            return;
        }
        self.capture_error = None;
        self.edit_config(cx, |c| match slot.filter(|i| *i < c.hotkey.chords.len()) {
            Some(i) => c.hotkey.chords[i] = chord,
            None => c.hotkey.chords.push(chord),
        });
    }

    /// Removes a shortcut, keeping at least one.
    pub fn remove_shortcut(&mut self, i: usize, cx: &mut Context<Self>) {
        if self.config.hotkey.chords.len() > 1 && i < self.config.hotkey.chords.len() {
            self.edit_config(cx, |c| {
                c.hotkey.chords.remove(i);
            });
        }
    }

    pub fn apply(&mut self, event: Event, cx: &mut Context<Self>) {
        if !matches!(event, Event::Level(_) | Event::History { .. } | Event::Config(_)) {
            log::debug!("ui event: {event:?}");
        }
        match event {
            Event::Level(l) => {
                self.level.update(cx, |m, cx| {
                    m.level = l;
                    m.updated = Instant::now();
                    cx.notify();
                });
                return;
            }
            Event::Phase(p) => {
                self.phase = p;
                cx.emit(StateEvent::PhaseChanged(p));
            }
            Event::Engine(s) => self.engine = s,
            Event::Download { id, done, total } => {
                self.download_errors.remove(&id);
                self.downloads.insert(id, (done, total));
            }
            Event::DownloadFinished { id, error } => {
                self.downloads.remove(&id);
                self.installed = models::installed().into_iter().collect();
                if self.model_switch.as_deref() == Some(id.as_str()) {
                    if self.installed.contains(id.as_str()) {
                        self.model_switch = None;
                        let id = id.clone();
                        self.edit_config(cx, |c| c.model.id = id);
                    } else if error.as_deref().is_some_and(|e| e.contains("cancelled")) {
                        self.model_switch = None;
                    }
                }
                if let Some(e) = error {
                    if !e.contains("cancelled") {
                        self.download_errors.insert(id, e);
                    }
                }
            }
            Event::Captured(chord) => self.on_captured(chord, cx),
            Event::Transcript(text) => {
                self.last_transcript = Some(text);
                self.transcripts += 1;
            }
            Event::Dictated(entry) => self.history.push(entry),
            Event::History { entries, stats } => {
                // An empty list with stats is a stats-only refresh.
                if !entries.is_empty() || stats.dictations == 0 {
                    self.history = entries;
                }
                self.stats = stats;
            }
            Event::Permissions(p) => self.permissions = p,
            Event::Hotkeys { active, error, backend, desktop_managed, trigger } => {
                self.hotkeys_active = active;
                self.hotkey_error = error;
                self.hotkey_backend = backend;
                self.desktop_shortcut = desktop_managed;
                self.desktop_trigger = trigger;
            }
            Event::Notice { kind, text } => {
                self.notice = Some(Notice {
                    kind,
                    text: text.into(),
                    at: Instant::now(),
                });
                cx.emit(StateEvent::Notice);
            }
            Event::PasteFailed(text) => {
                self.paste_failed = Some(text);
                cx.emit(StateEvent::PasteFailed);
            }
            Event::SecureInput(app) => self.secure_input = app,
            Event::Learned { from, to } => {
                self.learned = Some((from, to));
                cx.emit(StateEvent::Learned);
            }
            Event::LearningAvailable(on) => self.learning_available = on,
            Event::Polish(s) => self.polish = s,
            Event::Config(cfg) => self.config = *cfg,
            Event::Restart => cx.emit(StateEvent::Restart),
        }
        cx.notify();
    }

    /// Output polish is on but its model isn't here: people who had Bars
    /// before it are asked to download it.
    pub fn polish_needs_download(&self) -> bool {
        self.config.onboarded
            && self.config.text.polish
            && bars_core::polish::supported()
            && !self.installed.contains(models::POLISH.id)
    }

    pub fn active_model(&self) -> Option<&'static models::ModelInfo> {
        models::find(&self.config.model.id)
    }

    /// The recommended speech model, for someone who uses another one and
    /// hasn't been offered it yet (see `answer_model_offer`); also while it
    /// downloads to switch to it.
    pub fn model_offer(&self) -> Option<&'static models::ModelInfo> {
        let m = models::find(models::DEFAULT_MODEL)?;
        let switching = self.model_switch.as_deref() == Some(m.id);
        let new = self.config.onboarded
            && self.config.model.id != m.id
            && !self.config.models_offered.iter().any(|id| id == m.id);
        (switching || new).then_some(m)
    }

    /// Switches to the recommended model (once it's downloaded) or not;
    /// either way it isn't offered again.
    pub fn answer_model_offer(&mut self, switch: bool, cx: &mut Context<Self>) {
        let id = models::DEFAULT_MODEL;
        let installed = self.installed.contains(id);
        self.edit_config(cx, |c| {
            if !c.models_offered.iter().any(|o| o == id) {
                c.models_offered.push(id.to_string());
            }
            if switch && installed {
                c.model.id = id.to_string();
            }
        });
        self.model_switch = (switch && !installed).then(|| id.to_string());
        if self.model_switch.is_some() {
            self.download_errors.remove(id);
            self.send(Command::DownloadModel(id.to_string()));
        }
    }

    /// Everything needed before the first dictation works.
    pub fn setup_complete(&self) -> bool {
        self.installed.contains(self.config.model.id.as_str())
            && self.hotkeys_active
            && self.permissions.items.iter().all(|r| r.ok())
    }

    /// Whether the controller has reported permissions yet.
    pub fn permissions_known(&self) -> bool {
        !self.permissions.items.is_empty()
    }

    /// Keycaps for the (first) shortcut as it will actually trigger.
    pub fn shortcut_parts(&self) -> Vec<String> {
        self.shortcuts().into_iter().next().unwrap_or_else(|| vec!["Not assigned".into()])
    }

    /// Keycaps for every shortcut; the desktop's one when it manages it.
    pub fn shortcuts(&self) -> Vec<Vec<String>> {
        if self.desktop_shortcut {
            vec![match &self.desktop_trigger {
                Some(t) => t.split('+').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect(),
                None => vec!["Not assigned".into()],
            }]
        } else {
            self.config.hotkey.chords.iter().map(Chord::labels).collect()
        }
    }
}

/// Starts the task that drains controller events into the state entity.
pub fn pump(state: Entity<AppState>, rx: async_channel::Receiver<Event>, cx: &mut App) {
    cx.spawn(async move |cx| {
        while let Ok(ev) = rx.recv().await {
            // Coalesce bursts of level updates: only the newest matters.
            let mut ev = ev;
            if matches!(ev, Event::Level(_)) {
                while let Ok(next) = rx.try_recv() {
                    if matches!(next, Event::Level(_)) {
                        ev = next;
                    } else {
                        let s = state.clone();
                        cx.update(|cx| s.update(cx, |s, cx| s.apply(ev, cx)));
                        ev = next;
                        break;
                    }
                }
            }
            let s = state.clone();
            cx.update(|cx| s.update(cx, |s, cx| s.apply(ev, cx)));
        }
    })
    .detach();
}
