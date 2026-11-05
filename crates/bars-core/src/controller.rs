//! The dictation controller: a single thread that owns all state and turns
//! hotkey gestures, UI commands and engine results into actions. The UI only
//! sends [`Command`]s and renders [`Event`]s, which keeps it thin and makes
//! the whole pipeline usable headless (CLI, tests, other front-ends).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Local;
use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};

use crate::audio::{Audio, Cue, Stopped};
use crate::config::{Config, InsertionConfig, Replacement, Suggestion};
use crate::engine::{Engine, EngineEvent, EngineStatus, Transcript};
use crate::history::{self, History, Stats};
use crate::hotkey::{Activation, Chord, HotkeyEvent, Intent, Mods, Recognizer};
use crate::learn::{self, Heard, Pieces, Tracker, Verdict};
use crate::models;
use crate::paths;
use crate::platform::focus::{self, Focus};
use crate::platform::mute::{self, Muter};
use crate::platform::{self, Hotkeys, Injector, Inserted, Permissions};
use crate::polish::{self, Part, PolishEvent, PolishStatus, Polisher};
use crate::segment::{self, Segmenter};
use crate::text::{self, TextProcessor};

/// Requests from the UI (or tray, D-Bus, CLI).
#[derive(Clone, Debug)]
pub enum Command {
    /// Start hands-free dictation, or finish the current one.
    Toggle,
    Finish,
    Cancel,
    SetConfig(Box<Config>),
    /// Start/stop recording the next key chord for the shortcut picker.
    CaptureHotkey(bool),
    /// Open the desktop's shortcut editor (portal backend).
    ConfigureShortcut,
    /// Open the microphone just to show input levels (onboarding).
    MicTest(bool),
    /// Open a system settings page or link.
    OpenUrl(String),
    /// macOS: let Bars read the clipboard, to restore it after pasting.
    AllowClipboardReads,
    /// Downloads a model (speech or polish); one at a time, the rest wait.
    DownloadModel(String),
    CancelDownload(String),
    DeleteModel(String),
    RefreshPermissions,
    /// Cheap re-check while the setup assistant waits for the user to grant
    /// something in system settings; acts only if anything changed.
    PollPermissions,
    /// Ask for missing permissions (all, or one where the OS asks per item).
    FixPermissions(Option<platform::requirements::ReqId>),
    /// Re-insert the most recent transcript.
    PasteLast,
    CopyText(String),
    /// The user corrected a dictation on the History page.
    EditEntry { id: u64, text: String },
    DeleteEntry(u64),
    /// Take back a learned dictionary entry (and don't learn it again).
    UndoLearned { from: String, to: String },
    AcceptSuggestion { from: String, to: String },
    DismissSuggestion { from: String, to: String },
    ClearHistory,
    Shutdown,
}

/// Completions from helper threads back to the controller.
enum Internal {
    DownloadFinished { id: String, error: Option<String> },
    PermissionsFixed(Result<(), String>),
    /// A dictation was edited in place after it went in.
    Corrected { request: u64, inserted: String, edited: String },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Recording { locked: bool },
    Transcribing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warning,
    Error,
}

/// State changes for the UI.
#[derive(Clone, Debug)]
pub enum Event {
    Phase(Phase),
    /// Microphone level 0..1 while recording.
    Level(f32),
    Engine(EngineStatus),
    Download { id: String, done: u64, total: u64 },
    DownloadFinished { id: String, error: Option<String> },
    Captured(Chord),
    /// A dictation finished (always sent; carries only the text).
    Transcript(String),
    /// A dictation was added to the history (only while history is saved).
    Dictated(history::Entry),
    History { entries: Vec<history::Entry>, stats: Stats },
    Permissions(Permissions),
    Hotkeys {
        active: bool,
        error: Option<String>,
        /// "direct", "desktop" or "none".
        backend: &'static str,
        /// The desktop owns the binding; `trigger` describes it.
        desktop_managed: bool,
        trigger: Option<String>,
    },
    Notice { kind: NoticeKind, text: String },
    /// A paste landed nowhere (no text field had focus); carries the text
    /// so it can be copied.
    PasteFailed(String),
    /// macOS: the app whose Secure Keyboard Entry keeps the shortcut from
    /// working, or None once it's off.
    SecureInput(Option<String>),
    /// A correction became a dictionary entry.
    Learned { from: String, to: String },
    /// Whether learning from corrections works with the selected model.
    LearningAvailable(bool),
    Polish(PolishStatus),
    Config(Box<Config>),
    /// macOS: audio stopped working until Bars restarts; restart once idle.
    Restart,
}

pub type EventSender = async_channel::Sender<Event>;

#[derive(Clone)]
pub struct ControllerHandle {
    tx: Sender<Command>,
    /// Closes when the controller has stopped.
    done: Receiver<()>,
}

impl ControllerHandle {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    /// Stops the controller, giving it a moment to finish (turning other
    /// audio back on, say) before the app quits.
    pub fn shutdown(&self) {
        self.send(Command::Shutdown);
        let _ = self.done.recv_timeout(Duration::from_secs(1));
    }
}

pub fn spawn(config: Config, ui: EventSender) -> ControllerHandle {
    let (tx, rx) = unbounded();
    let (done_tx, done) = bounded::<()>(0);
    let handle = ControllerHandle { tx: tx.clone(), done };
    thread::Builder::new()
        .name("bars-controller".into())
        .spawn(move || {
            Controller::new(config, ui).run(rx);
            drop(done_tx);
        })
        .expect("spawn controller");
    handle
}

struct Pending {
    /// Audio length, for stats.
    audio_secs: f32,
    /// The field, being read since the dictation started.
    focus: Option<Receiver<Focus>>,
    /// The field went to the polisher instead, to read ahead.
    prepared: bool,
    /// Heard, and being polished.
    transcript: Option<Transcript>,
}

/// The dictation being recorded.
struct Recording {
    /// Its engine request, once it's heard.
    id: u64,
    focus: Option<Receiver<Focus>>,
    prepared: bool,
    /// Cuts a long dictation into pieces as it's recorded.
    segmenter: Option<Segmenter>,
    /// Samples of the recording looked at so far.
    seen: usize,
}

/// A long dictation, transcribed (and polished) piece by piece while it
/// was being recorded (see `segment`).
#[derive(Default)]
struct Parts {
    /// Each piece sent to the engine, as it comes back.
    pieces: Vec<Piece>,
    /// Pieces passed on (to the polisher, or just counted), in order.
    passed: usize,
    /// The recording ended (all pieces are sent), and when.
    finished: Option<Instant>,
    /// The pieces go through the polisher (decided at the first).
    polished: bool,
}

enum Piece {
    Waiting,
    Heard(Transcript),
    /// The engine failed on it: it adds nothing.
    Lost,
}

/// Where insertion finds what had focus.
enum Field {
    /// Being read since the dictation started.
    Reading(Option<Receiver<Focus>>),
    /// Already read (polish needed it first).
    Read(Option<Focus>),
    /// Read it now ("Paste last").
    Now,
}

enum InsertJob {
    Text {
        text: String,
        cfg: InsertionConfig,
        /// Dictionary words, which keep their capital in smart spacing.
        names: Vec<String>,
        field: Field,
        /// Watch the field for corrections to learn from (the engine
        /// request the text came from).
        watch: Option<u64>,
    },
    Copy(String),
    /// Dictation started: note which app/window should receive the text.
    RememberTarget,
}

/// How long a sound cue takes to be heard: its tones, and the output's
/// latency. Where muting silences Bars' sounds too, it waits this long.
const CUE_HEARD: Duration = Duration::from_millis(400);
/// How long insertion waits for the focused field to be read.
const FOCUS_TIMEOUT: Duration = Duration::from_millis(400);
/// While recording, how often to look for a piece of a long dictation to
/// transcribe (see `segment`).
const SPLIT_EVERY: Duration = Duration::from_millis(300);
/// After a dictation goes in, its field is re-read this often, for this
/// long, to see corrections.
const WATCH_EVERY: Duration = Duration::from_secs(1);
const WATCH_FOR: Duration = Duration::from_secs(60);
/// Characters read on each side of the cursor while watching: in effect
/// the whole field (a terminal's whole screen).
const WATCH_CHARS: usize = 20_000;
/// A correction counts once the text stayed the same for this many reads
/// (the user stopped typing).
const SETTLED_READS: u32 = 3;
/// Consecutive reads without the dictation (a terminal program redrawing)
/// before watching stops.
const MISSES_ALLOWED: u32 = 5;
/// Recent dictations whose model confidences are kept for learning.
const HEARD_KEPT: usize = 32;
/// How often macOS is asked whether another app holds secure input.
const SECURE_INPUT_POLL: Duration = Duration::from_secs(3);
/// Secure input held this long (not just a password typed in passing) is
/// worth a notice, once per app.
const SECURE_INPUT_NOTICE: Duration = Duration::from_secs(6);

struct Controller {
    cfg: Config,
    ui: EventSender,
    audio: Audio,
    engine: Engine,
    engine_rx: Receiver<EngineEvent>,
    polisher: Polisher,
    polish_rx: Receiver<PolishEvent>,
    muter: Muter,
    /// Other audio is muted (or about to be) for the recording.
    muting: bool,
    polish_status: PolishStatus,
    hotkeys: Hotkeys,
    hotkey_rx: Receiver<HotkeyEvent>,
    hotkey_tx: Sender<HotkeyEvent>,
    inserter: Sender<InsertJob>,
    recognizer: Recognizer,
    phase: Phase,
    recording_since: Option<Instant>,
    start_cue_at: Option<Instant>,
    next_request: u64,
    pending: HashMap<u64, Pending>,
    history: History,
    text: TextProcessor,
    /// The model downloading, and how to stop it.
    downloading: Option<(String, Arc<AtomicBool>)>,
    /// Models waiting to download after it.
    download_queue: VecDeque<String>,
    recording: Option<Recording>,
    /// When the recording was last looked at for a piece to transcribe.
    split_checked: Instant,
    /// Long dictations in pieces, by dictation.
    parts: HashMap<u64, Parts>,
    /// Engine requests for pieces: the dictation and which piece.
    part_of: HashMap<u64, (u64, usize)>,
    capturing: bool,
    mic_testing: bool,
    bound_trigger: Option<String>,
    internal_tx: Sender<Internal>,
    internal_rx: Receiver<Internal>,
    /// The most recent dictation, for "Paste last" (never saved).
    last_text: Option<String>,
    /// What the UI was last told about permissions.
    last_requirements: Vec<platform::requirements::Requirement>,
    /// macOS: while the shortcut backend is down, whether access was granted
    /// at the last check, and when that was.
    hotkey_access: bool,
    hotkey_access_checked: Instant,
    /// macOS: the app holding secure input, since when, and which apps the
    /// user was already told about.
    secure_input: Option<String>,
    secure_input_since: Instant,
    secure_input_checked: Instant,
    secure_input_told: HashSet<String>,
    /// The loaded model's vocabulary (Parakeet only: learning needs it).
    pieces: Option<Arc<Pieces>>,
    /// Model confidences for recent dictations, by engine request.
    heard: VecDeque<(u64, Vec<Heard>)>,
    /// Engine requests of recent History entries.
    entry_requests: VecDeque<(u64, u64)>,
    /// Bumped when a dictation starts: watchers of earlier ones stop.
    watch_generation: Arc<AtomicU64>,
    /// Corrections already judged, per engine request: one dictation
    /// edited in stages is reported more than once.
    judged: VecDeque<(u64, String, String)>,
}

impl Controller {
    fn new(cfg: Config, ui: EventSender) -> Self {
        let level_ui = ui.clone();
        let audio = Audio::spawn(Arc::new(move |level| {
            let _ = level_ui.try_send(Event::Level(level));
        }));
        let (engine_tx, engine_rx) = unbounded();
        let engine = Engine::spawn(engine_tx);
        let (polish_tx, polish_rx) = unbounded();
        let polisher = Polisher::spawn(polish_tx);
        let (hotkey_tx, hotkey_rx) = unbounded();
        let hotkeys = Hotkeys::start(cfg.hotkey.backend, cfg.hotkey.chords.clone(), hotkey_tx.clone());
        let (internal_tx, internal_rx) = unbounded();
        let watch_generation = Arc::new(AtomicU64::new(0));
        let inserter = spawn_inserter(ui.clone(), &hotkeys, internal_tx.clone(), watch_generation.clone());
        Controller {
            internal_tx,
            internal_rx,
            recognizer: Recognizer::new(cfg.hotkey.activation),
            text: TextProcessor::new(&cfg.text),
            history: History::open(paths::history_file()),
            cfg,
            ui,
            audio,
            engine,
            engine_rx,
            polisher,
            muter: Muter::spawn(),
            muting: false,
            polish_rx,
            polish_status: PolishStatus::Off,
            hotkeys,
            hotkey_rx,
            hotkey_tx,
            inserter,
            phase: Phase::Idle,
            recording_since: None,
            start_cue_at: None,
            next_request: 1,
            pending: HashMap::new(),
            downloading: None,
            download_queue: VecDeque::new(),
            recording: None,
            split_checked: Instant::now(),
            parts: HashMap::new(),
            part_of: HashMap::new(),
            capturing: false,
            mic_testing: false,
            bound_trigger: None,
            last_requirements: Vec::new(),
            last_text: None,
            hotkey_access: platform::hotkey_access_granted(),
            hotkey_access_checked: Instant::now(),
            secure_input: None,
            secure_input_since: Instant::now(),
            secure_input_checked: Instant::now(),
            secure_input_told: HashSet::new(),
            pieces: None,
            heard: VecDeque::new(),
            entry_requests: VecDeque::new(),
            watch_generation,
            judged: VecDeque::new(),
        }
    }

    fn emit(&self, e: Event) {
        let _ = self.ui.try_send(e);
    }

    fn notice(&self, kind: NoticeKind, text: impl Into<String>) {
        self.emit(Event::Notice { kind, text: text.into() });
    }

    fn set_phase(&mut self, phase: Phase) {
        if self.phase != phase {
            self.phase = phase;
            self.sync_hotkeys_recording();
            self.emit(Event::Phase(phase));
        }
    }

    /// Tells the shortcut backend whether (and how) a dictation records.
    fn sync_hotkeys_recording(&self) {
        let recording = matches!(self.phase, Phase::Recording { .. });
        self.hotkeys.set_recording(recording, self.phase == (Phase::Recording { locked: true }));
    }

    fn run(mut self, rx: Receiver<Command>) {
        self.emit(Event::Config(Box::new(self.cfg.clone())));
        self.emit_permissions();
        self.emit_hotkey_status();
        self.emit_history();
        self.engine.select(&self.cfg.model.id, self.cfg.model.threads, self.cfg.model.unload_after_mins);
        self.polisher.configure(self.cfg.text.polish, &self.cfg.text.polish_model, self.cfg.model.unload_after_mins);
        if self.cfg.audio.keep_warm {
            self.audio.set_warm(self.cfg.audio.device.clone(), true);
        }
        if reads_focus(&self.cfg) {
            platform::prepare_focus_reading();
        }

        loop {
            let deadline = [
                self.recognizer.deadline(),
                self.start_cue_at,
                self.hotkey_access_check_at(),
                self.secure_input_check_at(),
                self.split_check_at(),
            ]
                .into_iter()
                .flatten()
                .min();
            let timeout = deadline
                .map(|d| d.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_secs(3600));
            select! {
                recv(rx) -> cmd => match cmd {
                    Ok(Command::Shutdown) | Err(_) => {
                        self.audio.cancel();
                        self.unmute_others();
                        return;
                    }
                    Ok(cmd) => self.on_command(cmd),
                },
                recv(self.hotkey_rx) -> ev => {
                    if let Ok(ev) = ev {
                        self.on_hotkey(ev);
                    }
                }
                recv(self.engine_rx) -> ev => {
                    if let Ok(ev) = ev {
                        self.on_engine(ev);
                    }
                }
                recv(self.polish_rx) -> ev => {
                    if let Ok(ev) = ev {
                        self.on_polish(ev);
                    }
                }
                recv(self.internal_rx) -> ev => {
                    if let Ok(ev) = ev {
                        self.on_internal(ev);
                    }
                }
                default(timeout) => {}
            }
            let now = Instant::now();
            if let Some(intent) = self.recognizer.on_timer(now) {
                self.on_intent(intent, true);
            }
            if self.start_cue_at.is_some_and(|t| now >= t) {
                self.start_cue_at = None;
                if matches!(self.phase, Phase::Recording { .. }) {
                    if self.cfg.audio.sounds {
                        self.audio.cue(Cue::Start, self.cfg.audio.sound_volume);
                    }
                    self.mute_others(self.cfg.audio.sounds);
                }
            }
            if self.hotkey_access_check_at().is_some_and(|t| now >= t) {
                self.check_hotkey_access(now);
            }
            if self.split_check_at().is_some_and(|t| now >= t) {
                self.split_recording(now);
            }
            if self.secure_input_check_at().is_some_and(|t| now >= t) {
                self.check_secure_input(now);
            }
        }
    }

    /// macOS: when to next ask whether another app holds secure input.
    fn secure_input_check_at(&self) -> Option<Instant> {
        (cfg!(target_os = "macos") && self.hotkeys.is_active())
            .then(|| self.secure_input_checked + SECURE_INPUT_POLL)
    }

    /// Secure Keyboard Entry (Terminal, password managers, password fields)
    /// hides every key from Bars, so the shortcut silently stops working.
    /// Name the app responsible.
    fn check_secure_input(&mut self, now: Instant) {
        self.secure_input_checked = now;
        let owner = platform::secure_input_owner();
        if owner != self.secure_input {
            if let Some(app) = &owner {
                log::info!("{app} turned on secure keyboard entry; the shortcut can't be seen");
            }
            self.secure_input = owner.clone();
            self.secure_input_since = now;
            self.emit(Event::SecureInput(owner));
        } else if let Some(app) = &owner {
            if now.saturating_duration_since(self.secure_input_since) >= SECURE_INPUT_NOTICE
                && self.secure_input_told.insert(app.clone())
            {
                self.notice(NoticeKind::Warning, format!("Shortcut paused: {app} has secure input on"));
            }
        }
    }

    /// macOS: when to next check whether shortcut access has been granted,
    /// while the backend is down (it can't start without it).
    fn hotkey_access_check_at(&self) -> Option<Instant> {
        (cfg!(target_os = "macos") && !self.hotkeys.is_active())
            .then(|| self.hotkey_access_checked + Duration::from_secs(2))
    }

    /// Starts the shortcut backend once access appears, wherever it was
    /// granted (System Settings, not only the setup assistant).
    fn check_hotkey_access(&mut self, now: Instant) {
        self.hotkey_access_checked = now;
        let granted = platform::hotkey_access_granted();
        if granted && !self.hotkey_access {
            log::info!("shortcut access granted; starting the shortcut listener");
            self.refresh_permissions();
        }
        self.hotkey_access = granted;
    }

    fn permissions_context(&self) -> platform::requirements::Context {
        platform::requirements::Context {
            hotkey_backend: self.hotkeys.backend_name(),
            prefers_portal: self.cfg.hotkey.backend == crate::config::HotkeyBackendKind::Portal,
            uses_mouse: uses_mouse(&self.cfg),
        }
    }

    fn emit_permissions(&mut self) {
        let p = platform::check_permissions(&self.permissions_context());
        self.last_requirements = p.items.clone();
        self.emit(Event::Permissions(p));
    }

    fn poll_permissions(&mut self) {
        let p = platform::check_permissions(&self.permissions_context());
        let statuses = |items: &[platform::requirements::Requirement]| {
            items.iter().map(|r| (r.id, r.status)).collect::<Vec<_>>()
        };
        if statuses(&p.items) != statuses(&self.last_requirements) {
            use platform::requirements::ReqId;
            let mic = |items: &[platform::requirements::Requirement]| {
                items.iter().find(|r| r.id == ReqId::Microphone).map(|r| r.status)
            };
            // A stream opened before access was granted stays silent
            // (macOS), so reopen the live microphone test.
            if mic(&p.items) != mic(&self.last_requirements) && self.mic_testing && self.phase == Phase::Idle {
                let device = self.cfg.audio.device.clone();
                self.audio.cancel();
                self.audio.set_warm(device.clone(), false);
                self.audio.start(device, self.cfg.audio.keep_warm);
            }
            // Something was granted (or revoked): restart what depends on it.
            self.refresh_permissions();
        } else if p.items != self.last_requirements {
            self.last_requirements = p.items.clone();
            self.emit(Event::Permissions(p));
        }
    }

    fn emit_hotkey_status(&self) {
        self.emit(Event::Hotkeys {
            active: self.hotkeys.is_active(),
            error: self.hotkeys.error().map(str::to_string),
            backend: self.hotkeys.backend_name(),
            desktop_managed: self.hotkeys.desktop_managed(),
            trigger: self.bound_trigger.clone(),
        });
    }

    fn emit_history(&self) {
        self.emit(Event::History {
            entries: self.history.entries().to_vec(),
            stats: self.history.stats(),
        });
    }

    fn on_hotkey(&mut self, ev: HotkeyEvent) {
        if let HotkeyEvent::Captured(chord) = ev {
            self.capturing = false;
            self.emit(Event::Captured(chord));
            return;
        }
        if let HotkeyEvent::Bound(trigger) = ev {
            self.bound_trigger = (!trigger.is_empty()).then_some(trigger);
            self.emit_hotkey_status();
            return;
        }
        if self.capturing {
            return;
        }
        if let Some(intent) = self.recognizer.on_event(&ev, Instant::now()) {
            self.on_intent(intent, false);
        }
    }

    fn on_intent(&mut self, intent: Intent, from_timer: bool) {
        match intent {
            Intent::Start => self.start_recording(),
            Intent::Lock => {
                if let Phase::Recording { .. } = self.phase {
                    self.start_cue_at = None;
                    self.set_phase(Phase::Recording { locked: true });
                    let sounds = self.cfg.audio.sounds;
                    if sounds && mute::MUTES_OWN_SOUNDS {
                        self.unmute_others();
                    }
                    if sounds {
                        self.audio.cue(Cue::Lock, self.cfg.audio.sound_volume);
                    }
                    if !self.muting {
                        self.mute_others(sounds);
                    }
                }
            }
            Intent::Finish => self.finish_recording(),
            Intent::Cancel => {
                let was_tap = from_timer;
                self.cancel_recording(!was_tap);
                if was_tap && self.cfg.hotkey.activation == Activation::HoldOrDoubleTap {
                    let shortcut = self.cfg.hotkey.first().map_or_else(|| "the shortcut".into(), Chord::label);
                    self.notice(NoticeKind::Info, format!("Hold {shortcut} to talk · double-tap for hands-free"));
                }
            }
        }
    }

    fn start_recording(&mut self) {
        if matches!(self.phase, Phase::Recording { .. }) {
            return;
        }
        self.mic_testing = false;
        self.audio.start(self.cfg.audio.device.clone(), self.cfg.audio.keep_warm);
        // Stop watching earlier dictations for corrections.
        self.watch_generation.fetch_add(1, Ordering::Relaxed);
        let _ = self.inserter.send(InsertJob::RememberTarget);
        // Read what's around the cursor while the user talks.
        let ins = &self.cfg.insertion;
        let read_field = ins.smart_spacing || self.learning_active() || self.polishing();
        let read_focus = read_field || ins.split_for_agents;
        let mut focus = read_focus.then(|| focus::start_capture(read_field.then_some(focus::CONTEXT_CHARS)));
        let id = self.next_request;
        self.next_request += 1;
        // Polish reads where the dictation goes while the user talks.
        let prepared = self.polishing() && focus.is_some();
        if let Some(focus) = focus.take_if(|_| prepared) {
            self.polisher.prepare(id, focus);
        }
        self.recording = Some(Recording { id, focus, prepared, segmenter: None, seen: 0 });
        self.split_checked = Instant::now();
        self.recording_since = Some(Instant::now());
        log::info!("dictation started");
        // Delay the start sound slightly: chords that turn out to be part of
        // another shortcut get cancelled within this window silently.
        self.start_cue_at = Some(Instant::now() + Duration::from_millis(160));
        self.set_phase(Phase::Recording { locked: false });
    }

    fn cancel_recording(&mut self, audible: bool) {
        if !matches!(self.phase, Phase::Recording { .. }) {
            return;
        }
        self.audio.cancel();
        self.drop_recording();
        self.unmute_others();
        let long_enough = self
            .recording_since
            .is_some_and(|t| t.elapsed() > Duration::from_millis(400));
        if audible && long_enough && self.cfg.audio.sounds && self.start_cue_at.is_none() {
            self.audio.cue(Cue::Cancel, self.cfg.audio.sound_volume);
        }
        self.start_cue_at = None;
        self.recording_since = None;
        self.recognizer.reset();
        self.set_phase(if self.pending.is_empty() { Phase::Idle } else { Phase::Transcribing });
    }

    fn finish_recording(&mut self) {
        if !matches!(self.phase, Phase::Recording { .. }) {
            return;
        }
        self.recognizer.reset();
        self.start_cue_at = None;
        self.recording_since = None;
        self.unmute_others();
        let Stopped { recording: rec, stuck } = self.audio.stop();
        if stuck {
            self.replace_audio();
        } else if self.cfg.audio.sounds {
            self.audio.cue(Cue::Stop, self.cfg.audio.sound_volume);
        }
        let secs = rec.duration().as_secs_f32();
        let loudness = rec.loudest_window_rms();
        log::info!("recorded {secs:.2}s (peak {:.3}, loudest rms {loudness:.4})", rec.peak);
        if secs < 0.25 {
            self.drop_recording();
            self.set_phase(if self.pending.is_empty() { Phase::Idle } else { Phase::Transcribing });
            return;
        }
        if loudness < 0.003 {
            self.drop_recording();
            if !stuck {
                self.notice(NoticeKind::Warning, "Didn't hear anything — check your microphone");
            }
            self.set_phase(if self.pending.is_empty() { Phase::Idle } else { Phase::Transcribing });
            return;
        }
        let Recording { id, focus, prepared, segmenter, .. } = self.recording.take().unwrap_or_else(|| {
            self.next_request += 1;
            Recording { id: self.next_request - 1, focus: None, prepared: false, segmenter: None, seen: 0 }
        });
        self.pending.insert(id, Pending { audio_secs: secs, focus, prepared, transcript: None });
        if self.parts.contains_key(&id) {
            // A long dictation: only the rest is left to transcribe.
            let segmenter = segmenter.unwrap_or_else(|| Segmenter::new(rec.rate));
            self.send_piece(id, segmenter.last(&rec.samples), rec.rate);
            if let Some(parts) = self.parts.get_mut(&id) {
                parts.finished = Some(Instant::now());
            }
        } else {
            self.engine.transcribe(id, rec.samples, rec.rate, self.cfg.model.language.clone());
        }
        self.set_phase(Phase::Transcribing);
    }

    /// The audio thread is stuck in the audio system. macOS: CoreAudio's
    /// hang is in this process's state of the device, so a new thread would
    /// hang as well; Bars restarts instead.
    fn replace_audio(&mut self) {
        log::error!("the audio thread stopped answering");
        if cfg!(target_os = "macos") {
            self.notice(NoticeKind::Error, "The audio device stopped responding. Restarting Bars…");
            self.emit(Event::Restart);
        } else {
            self.audio = self.audio.respawn();
            if self.cfg.audio.keep_warm {
                self.audio.set_warm(self.cfg.audio.device.clone(), true);
            }
            self.notice(NoticeKind::Error, "The audio device stopped responding. Try again");
        }
    }

    /// Mutes other audio while recording, if that's on: once the sound
    /// just played (if `cue`) has been heard.
    fn mute_others(&mut self, cue: bool) {
        if self.cfg.audio.mute_others {
            self.muter.mute(if cue && mute::MUTES_OWN_SOUNDS { CUE_HEARD } else { Duration::ZERO });
            self.muting = true;
        }
    }

    /// Turns other audio back on (before a sound plays).
    fn unmute_others(&mut self) {
        if std::mem::take(&mut self.muting) {
            self.muter.restore();
        }
    }

    /// The recording ends without a dictation.
    fn drop_recording(&mut self) {
        if let Some(r) = self.recording.take() {
            let polishing = self.parts.remove(&r.id).is_some_and(|p| p.polished);
            if r.prepared || polishing {
                self.polisher.forget(r.id);
            }
        }
    }

    /// While recording, when to next look for a piece to transcribe.
    fn split_check_at(&self) -> Option<Instant> {
        (self.recording.is_some() && matches!(self.phase, Phase::Recording { .. })).then(|| self.split_checked + SPLIT_EVERY)
    }

    /// Hands any finished piece of a long dictation to the engine, so it's
    /// transcribed while the user goes on talking.
    fn split_recording(&mut self, now: Instant) {
        self.split_checked = now;
        let Some(rec) = self.recording.as_mut() else { return };
        let (new, rate) = self.audio.peek(rec.seen);
        rec.seen += new.len();
        let segmenter = rec.segmenter.get_or_insert_with(|| Segmenter::new(rate));
        let mut cut = Vec::new();
        let mut next = segmenter.push(&new);
        while let Some(piece) = next {
            cut.push(piece);
            next = segmenter.push(&[]);
        }
        let id = rec.id;
        for piece in cut {
            self.send_piece(id, piece, rate);
        }
    }

    fn send_piece(&mut self, id: u64, piece: segment::Piece, rate: u32) {
        let request = self.next_request;
        self.next_request += 1;
        let parts = self.parts.entry(id).or_default();
        self.part_of.insert(request, (id, parts.pieces.len()));
        parts.pieces.push(Piece::Waiting);
        log::info!("transcribing piece {} of a long dictation", parts.pieces.len());
        self.engine.transcribe_piece(request, piece, rate, self.cfg.model.language.clone());
    }

    /// A piece came back from the engine (None: it failed). Pieces are
    /// passed on in order; once the recording ended and all are back, the
    /// dictation is put together.
    fn on_piece(&mut self, id: u64, index: usize, heard: Option<Transcript>) {
        let Some(parts) = self.parts.get_mut(&id) else { return };
        if let Some(slot) = parts.pieces.get_mut(index) {
            *slot = heard.map_or(Piece::Lost, Piece::Heard);
        }
        while let Some(piece) = self.parts.get(&id).and_then(|p| p.pieces.get(p.passed)) {
            if matches!(piece, Piece::Waiting) {
                break;
            }
            let text = match piece {
                Piece::Heard(t) => t.text.clone(),
                _ => String::new(),
            };
            self.pass_piece(id, text);
        }
        let Some(parts) = self.parts.get(&id) else { return };
        if parts.finished.is_none() || parts.passed < parts.pieces.len() {
            return;
        }
        let Some(parts) = self.parts.remove(&id) else { return };
        if parts.pieces.iter().any(|p| matches!(p, Piece::Lost)) {
            self.notice(NoticeKind::Warning, "Part of the dictation couldn't be transcribed");
        }
        let t = put_together(id, parts.pieces, parts.finished);
        if parts.polished {
            // The polisher has the last piece; it finishes the dictation.
            if let Some(p) = self.pending.get_mut(&id) {
                p.transcript = Some(t);
            }
        } else if let Some(p) = self.pending.remove(&id) {
            self.deliver(t, p.audio_secs, None, Field::Reading(p.focus));
        }
    }

    /// Passes the next piece of a long dictation on: to the polisher when
    /// the dictation is being polished (decided at its first piece).
    fn pass_piece(&mut self, id: u64, text: String) {
        let first = self.parts.get(&id).is_some_and(|p| p.passed == 0);
        let prepared = match &self.recording {
            Some(r) if r.id == id => r.prepared,
            _ => self.pending.get(&id).is_some_and(|p| p.prepared),
        };
        let polishing = self.polishing();
        let polish = polishing && !text.trim().is_empty() && self.is_english(&text);
        let Some(parts) = self.parts.get_mut(&id) else { return };
        if first {
            parts.polished = polishing || prepared;
        }
        let index = parts.passed;
        parts.passed += 1;
        if !parts.polished {
            return;
        }
        let last = parts.finished.is_some() && parts.passed == parts.pieces.len();
        // The field (when not read ahead) goes with the first piece.
        let focus = if first && !prepared {
            match self.recording.as_mut() {
                Some(r) if r.id == id => r.focus.take(),
                _ => self.pending.get_mut(&id).and_then(|p| p.focus.take()),
            }
        } else {
            None
        };
        log::debug!("polishing piece {} of a long dictation", index + 1);
        self.polisher.polish(polish::Job {
            request: id,
            text,
            polish,
            focus,
            prepared: first && prepared,
            part: if last { Part::Last } else { Part::Next },
        });
    }

    fn on_engine(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::Status(s) => self.emit(Event::Engine(s)),
            EngineEvent::Pieces(pieces) => {
                self.pieces = pieces;
                self.emit(Event::LearningAvailable(self.pieces.is_some()));
            }
            EngineEvent::Failed { request, error } => {
                if let Some((id, index)) = self.part_of.remove(&request) {
                    log::warn!("a piece of a long dictation couldn't be transcribed: {error}");
                    self.on_piece(id, index, None);
                    return;
                }
                if self.pending.remove(&request).is_some_and(|p| p.prepared) {
                    self.polisher.forget(request);
                }
                self.notice(NoticeKind::Error, format!("Transcription failed: {error}"));
                self.settle_phase();
            }
            EngineEvent::Done(t) => {
                if let Some((id, index)) = self.part_of.remove(&t.request) {
                    self.on_piece(id, index, Some(t));
                    return;
                }
                if !self.pending.contains_key(&t.request) {
                    return;
                }
                let polishing = self.polishing();
                // Every dictation goes through the polisher while it's on, so
                // they come out in order (and it has the fields it read
                // ahead with); only English ones are polished.
                if polishing || self.pending.get(&t.request).is_some_and(|p| p.prepared) {
                    let polish = polishing && !t.text.trim().is_empty() && self.is_english(&t.text);
                    let Some(p) = self.pending.get_mut(&t.request) else { return };
                    self.polisher.polish(polish::Job {
                        request: t.request,
                        text: t.text.clone(),
                        polish,
                        focus: p.focus.take(),
                        prepared: p.prepared,
                        part: Part::Whole,
                    });
                    p.transcript = Some(t);
                    return;
                }
                let Some(p) = self.pending.remove(&t.request) else { return };
                self.deliver(t, p.audio_secs, None, Field::Reading(p.focus));
            }
        }
    }

    fn on_polish(&mut self, ev: PolishEvent) {
        match ev {
            PolishEvent::Status(s) => {
                self.polish_status = s.clone();
                self.emit(Event::Polish(s));
            }
            PolishEvent::Done { request, polished, focus } => {
                let Some(p) = self.pending.remove(&request) else { return };
                let Some(t) = p.transcript else { return };
                self.deliver(t, p.audio_secs, polished, Field::Read(focus));
            }
        }
    }

    /// Dictations go through polish: it's on, and its model is loaded or
    /// can be.
    fn polishing(&self) -> bool {
        self.cfg.text.polish
            && matches!(self.polish_status, PolishStatus::Ready | PolishStatus::Loading | PolishStatus::Unloaded)
    }

    /// Whether a dictation is in English: the language set for the speech
    /// model, the only one it knows, or else what the words look like.
    fn is_english(&self, text: &str) -> bool {
        if let Some(lang) = &self.cfg.model.language {
            return lang == "en";
        }
        match models::find(&self.cfg.model.id) {
            // Canary hears English unless told otherwise (engine.rs).
            Some(m) if m.family == models::Family::Canary || !m.multilingual => true,
            _ => polish::looks_english(text),
        }
    }

    /// The end of a dictation: clean-up, insertion and history. `polished`
    /// is the polish model's version of what was heard, if any.
    fn deliver(&mut self, t: Transcript, audio_secs: f32, polished: Option<String>, field: Field) {
        let cleaned = self.text.process(polished.as_deref().unwrap_or(&t.text));
        if cleaned.is_empty() {
            self.notice(NoticeKind::Info, "No speech recognized");
            self.settle_phase();
            return;
        }
        if let Some(heard) = t.heard.clone() {
            self.heard.push_back((t.request, heard));
            if self.heard.len() > HEARD_KEPT {
                self.heard.pop_front();
            }
        }
        let _ = self.inserter.send(InsertJob::Text {
            text: cleaned.clone(),
            cfg: self.cfg.insertion.clone(),
            names: self.dictionary_words(),
            field,
            watch: self.learning_active().then_some(t.request),
        });
        // What was said stays out of the logs (which can end up in the
        // system journal), unless debugging.
        log::info!(
            "transcribed {audio_secs:.1}s in {} ms ({} words{})",
            t.infer_ms,
            text::word_count(&cleaned),
            if polished.is_some() { ", polished" } else { "" }
        );
        log::debug!("transcript: {cleaned:?}");
        // Kept in memory only, for "Paste last".
        self.last_text = Some(cleaned.clone());
        self.emit(Event::Transcript(cleaned.clone()));
        // History off: nothing is recorded, on disk or in the history list
        // (which a later rewrite would save).
        if self.cfg.ui.save_history {
            let entry = history::Entry {
                id: self.history.next_id(),
                at: Local::now(),
                words: text::word_count(&cleaned),
                raw: (t.text != cleaned).then(|| t.text.clone()),
                text: cleaned,
                model: t.model,
                audio_secs,
                infer_ms: t.infer_ms,
            };
            self.entry_requests.push_back((entry.id, t.request));
            if self.entry_requests.len() > HEARD_KEPT {
                self.entry_requests.pop_front();
            }
            self.history.append(entry.clone());
            self.emit(Event::Dictated(entry));
            self.emit(Event::History {
                entries: Vec::new(),
                stats: self.history.stats(),
            });
        }
        self.settle_phase();
    }

    /// What dictionary entries write, which smart spacing never lowercases.
    fn dictionary_words(&self) -> Vec<String> {
        self.cfg.text.dictionary.iter().map(|r| r.to.clone()).collect()
    }

    fn settle_phase(&mut self) {
        if matches!(self.phase, Phase::Recording { .. }) {
            return;
        }
        self.set_phase(if self.pending.is_empty() { Phase::Idle } else { Phase::Transcribing });
    }

    fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Toggle => match self.phase {
                Phase::Recording { .. } => self.finish_recording(),
                _ => {
                    self.start_recording();
                    self.recognizer.lock_external();
                    self.on_intent(Intent::Lock, false);
                }
            },
            Command::Finish => self.finish_recording(),
            Command::Cancel => self.cancel_recording(true),
            Command::SetConfig(cfg) => self.apply_config(*cfg),
            Command::CaptureHotkey(on) => {
                self.capturing = on;
                self.hotkeys.set_capture(on);
                if on && !self.hotkeys.is_active() {
                    #[cfg(target_os = "macos")]
                    let text = format!("Allow Bars under {} to record a shortcut", platform::mac::permissions::accessibility_pane());
                    #[cfg(not(target_os = "macos"))]
                    let text = "Keyboard access is needed to record a shortcut".to_string();
                    self.notice(NoticeKind::Warning, text);
                }
            }
            Command::ConfigureShortcut => self.hotkeys.configure(),
            Command::MicTest(on) => {
                if on && self.phase == Phase::Idle {
                    self.audio.start(self.cfg.audio.device.clone(), self.cfg.audio.keep_warm);
                    self.mic_testing = true;
                } else if !on && self.mic_testing {
                    self.mic_testing = false;
                    if self.phase == Phase::Idle {
                        self.audio.cancel();
                    }
                }
            }
            Command::OpenUrl(url) => {
                if let Err(e) = platform::requirements::open_url(&url) {
                    self.notice(NoticeKind::Error, format!("Couldn't open settings: {e:#}"));
                }
            }
            Command::AllowClipboardReads => {
                // macOS may ask (and wait for an answer) on this thread.
                let ui = self.ui.clone();
                thread::spawn(move || {
                    if let Err(e) = platform::request_clipboard_access() {
                        let _ = ui.try_send(Event::Notice {
                            kind: NoticeKind::Error,
                            text: format!("Couldn't open settings: {e:#}"),
                        });
                    }
                });
            }
            Command::DownloadModel(id) => self.download(id),
            Command::CancelDownload(id) => {
                if let Some((current, cancel)) = &self.downloading
                    && *current == id
                {
                    cancel.store(true, Ordering::Relaxed);
                }
                if let Some(i) = self.download_queue.iter().position(|q| *q == id) {
                    self.download_queue.remove(i);
                    self.emit(Event::DownloadFinished { id, error: Some("cancelled".into()) });
                }
            }
            Command::DeleteModel(id) => {
                if let Some(m) = models::find_any(&id) {
                    if let Err(e) = m.delete() {
                        self.notice(NoticeKind::Error, format!("Couldn't delete {}: {e}", m.name));
                    }
                    if id == self.cfg.model.id {
                        self.engine.unload();
                        self.emit(Event::Engine(EngineStatus::Missing(id.clone())));
                    }
                    if id == models::polish_model(&self.cfg.text.polish_model).id {
                        self.polisher.recheck();
                    }
                    self.emit(Event::DownloadFinished { id, error: None });
                }
            }
            Command::RefreshPermissions => self.refresh_permissions(),
            Command::PollPermissions => self.poll_permissions(),
            Command::FixPermissions(only) => {
                // The polkit prompt can stay open a while; don't block on it.
                let tx = self.internal_tx.clone();
                thread::spawn(move || {
                    let r = platform::fix_permissions(only).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Internal::PermissionsFixed(r));
                });
            }
            Command::PasteLast => {
                let last = self
                    .last_text
                    .clone()
                    .or_else(|| self.history.entries().last().map(|e| e.text.clone()));
                if let Some(last) = last {
                    let _ = self.inserter.send(InsertJob::Text {
                        text: last,
                        cfg: self.cfg.insertion.clone(),
                        names: self.dictionary_words(),
                        field: Field::Now,
                        watch: None,
                    });
                }
            }
            Command::CopyText(text) => {
                let _ = self.inserter.send(InsertJob::Copy(text));
            }
            Command::EditEntry { id, text } => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    return;
                }
                if let Some(old) = self.history.update(id, &text) {
                    let request = self.entry_requests.iter().find(|(e, _)| *e == id).map(|(_, r)| *r);
                    self.learn_from(request, &old, &text);
                }
                self.emit_history();
            }
            Command::DeleteEntry(id) => {
                self.history.delete(id);
                self.emit_history();
            }
            Command::UndoLearned { from, to } => {
                let before = self.cfg.text.dictionary.len();
                self.cfg.text.dictionary.retain(|r| !(r.learned && r.from == from && r.to == to));
                if self.cfg.text.dictionary.len() != before {
                    self.ignore_pair(from, to);
                    self.save_learning();
                }
            }
            Command::AcceptSuggestion { from, to } => {
                self.add_learned(from, to);
                self.save_learning();
            }
            Command::DismissSuggestion { from, to } => {
                self.cfg.text.suggestions.retain(|s| !(s.from == from && s.to == to));
                self.ignore_pair(from, to);
                self.save_learning();
            }
            Command::ClearHistory => {
                self.history.clear();
                self.emit_history();
            }
            Command::Shutdown => {}
        }
    }

    fn refresh_permissions(&mut self) {
        // Prefer direct access once it's granted (it supports more chords).
        let want_direct = self.hotkeys.backend_name() != "direct"
            && matches!(self.cfg.hotkey.backend, crate::config::HotkeyBackendKind::Auto | crate::config::HotkeyBackendKind::Direct);
        if !self.hotkeys.is_active() || want_direct {
            let next = Hotkeys::start(
                self.cfg.hotkey.backend,
                self.cfg.hotkey.chords.clone(),
                self.hotkey_tx.clone(),
            );
            if next.is_active() || !self.hotkeys.is_active() {
                self.hotkeys = next;
                self.sync_hotkeys_recording();
                self.bound_trigger = None;
            }
        }
        self.inserter = spawn_inserter(self.ui.clone(), &self.hotkeys, self.internal_tx.clone(), self.watch_generation.clone());
        self.emit_permissions();
        self.emit_hotkey_status();
    }

    fn apply_config(&mut self, mut new: Config) {
        // Whatever the change, a shortcut stays.
        new.hotkey.keep_a_shortcut();
        let old = std::mem::replace(&mut self.cfg, new);
        if self.cfg.hotkey.backend != old.hotkey.backend {
            self.hotkeys =
                Hotkeys::start(self.cfg.hotkey.backend, self.cfg.hotkey.chords.clone(), self.hotkey_tx.clone());
            self.sync_hotkeys_recording();
            self.inserter = spawn_inserter(self.ui.clone(), &self.hotkeys, self.internal_tx.clone(), self.watch_generation.clone());
            self.bound_trigger = None;
            self.emit_hotkey_status();
            self.emit_permissions();
        } else if self.cfg.hotkey.chords != old.hotkey.chords {
            let all: Vec<String> = self.cfg.hotkey.chords.iter().map(Chord::to_string).collect();
            log::info!("shortcuts set to {}", all.join(", "));
            self.hotkeys.set_chords(self.cfg.hotkey.chords.clone());
            // Linux: a mouse-button shortcut needs the mice readable.
            if uses_mouse(&self.cfg) != uses_mouse(&old) {
                self.emit_permissions();
            }
        }
        let cfg = &self.cfg;
        if cfg.hotkey.activation != old.hotkey.activation {
            self.recognizer = Recognizer::new(cfg.hotkey.activation);
        }
        if cfg.model != old.model {
            if cfg.model.id != old.model.id {
                // Known again once the new model has loaded.
                self.pieces = None;
                self.emit(Event::LearningAvailable(false));
            }
            self.engine.select(&cfg.model.id, cfg.model.threads, cfg.model.unload_after_mins);
        }
        if cfg.audio.device != old.audio.device && self.mic_testing && self.phase == Phase::Idle {
            // The microphone test (setup assistant) is listening: move it to
            // the newly picked device right away.
            self.audio.cancel();
            self.audio.start(cfg.audio.device.clone(), cfg.audio.keep_warm);
        } else if cfg.audio.keep_warm != old.audio.keep_warm || cfg.audio.device != old.audio.device {
            self.audio.set_warm(cfg.audio.device.clone(), cfg.audio.keep_warm);
        }
        if cfg.text != old.text {
            self.text = TextProcessor::new(&cfg.text);
        }
        if cfg.text.polish != old.text.polish
            || cfg.text.polish_model != old.text.polish_model
            || cfg.model.unload_after_mins != old.model.unload_after_mins
        {
            self.polisher.configure(cfg.text.polish, &cfg.text.polish_model, cfg.model.unload_after_mins);
        }
        if reads_focus(cfg) && !reads_focus(&old) {
            platform::prepare_focus_reading();
        }
        if cfg.audio.mute_others && !old.audio.mute_others {
            self.muter.prepare();
        }
        if let Err(e) = self.cfg.save(&paths::config_file()) {
            self.notice(NoticeKind::Error, format!("Couldn't save settings: {e:#}"));
        }
    }

    fn download(&mut self, id: String) {
        let Some(info) = models::find_any(&id) else { return };
        if self.downloading.as_ref().is_some_and(|(d, _)| *d == id) || self.download_queue.contains(&id) {
            return;
        }
        if self.downloading.is_some() {
            // One at a time: this one is next.
            self.emit(Event::Download { id: id.clone(), done: 0, total: info.size_bytes() });
            self.download_queue.push_back(id);
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.downloading = Some((id, cancel.clone()));
        let ui = self.ui.clone();
        let done = self.internal_tx.clone();
        thread::Builder::new()
            .name("bars-download".into())
            .spawn(move || {
                let mut last = Instant::now() - Duration::from_secs(1);
                let result = models::download(info, cancel, &mut |done, total| {
                    if last.elapsed() > Duration::from_millis(100) || done == total {
                        last = Instant::now();
                        let _ = ui.try_send(Event::Download {
                            id: info.id.to_string(),
                            done,
                            total,
                        });
                    }
                });
                let _ = done.send(Internal::DownloadFinished {
                    id: info.id.to_string(),
                    error: result.err().map(|e| format!("{e:#}")),
                });
            })
            .expect("spawn download thread");
    }

    fn on_internal(&mut self, ev: Internal) {
        match ev {
            Internal::DownloadFinished { id, error } => {
                self.downloading = None;
                if error.is_none() && id == self.cfg.model.id {
                    let m = &self.cfg.model;
                    self.engine.select(&m.id, m.threads, m.unload_after_mins);
                }
                if error.is_none() && id == models::polish_model(&self.cfg.text.polish_model).id {
                    self.polisher.recheck();
                }
                self.emit(Event::DownloadFinished { id, error });
                if let Some(next) = self.download_queue.pop_front() {
                    self.download(next);
                }
            }
            Internal::PermissionsFixed(Ok(())) => {
                self.refresh_permissions();
                // Elsewhere the OS prompts are still open; polling notices
                // when the user answers.
                #[cfg(target_os = "linux")]
                self.notice(NoticeKind::Info, "Keyboard access granted");
            }
            Internal::PermissionsFixed(Err(e)) => self.notice(NoticeKind::Error, e),
            Internal::Corrected { request, inserted, edited } => self.learn_from(Some(request), &inserted, &edited),
        }
    }

    /// Learning from corrections is on, and the selected model supports it
    /// (Parakeet: its confidences tell mishearings from rewordings).
    fn learning_active(&self) -> bool {
        self.cfg.text.learn
            && self.pieces.is_some()
            && models::find(&self.cfg.model.id).is_some_and(|m| m.family == models::Family::Parakeet)
    }

    /// Judges each small change between `before` and `after` (what was
    /// dictated, and the user's correction of it). Clear fixes of
    /// mishearings join the dictionary; plausible ones are suggested and
    /// learned the second time they're seen. Nothing of the text is logged.
    fn learn_from(&mut self, request: Option<u64>, before: &str, after: &str) {
        if !self.learning_active() {
            return;
        }
        let Some(pieces) = self.pieces.clone() else { return };
        let Some(edits) = learn::find_edits(before, after) else {
            log::debug!("learning: the dictation was rewritten, not corrected");
            return;
        };
        let heard = request.and_then(|r| self.heard.iter().find(|(id, _)| *id == r)).map(|(_, h)| h.clone());
        let mut changed = false;
        for edit in edits {
            if let Some(r) = request {
                let key = (r, edit.from.clone(), edit.to.clone());
                if self.judged.contains(&key) {
                    continue;
                }
                self.judged.push_back(key);
                if self.judged.len() > 256 {
                    self.judged.pop_front();
                }
            }
            let confidence = heard.as_ref().and_then(|h| learn::confidence_of(&edit, h));
            let known: Vec<Replacement> =
                self.cfg.text.dictionary.iter().chain(&self.cfg.text.ignored).cloned().collect();
            let verdict = learn::judge(&edit, confidence, &pieces, &known);
            log::info!("learning: a correction ({verdict:?}, model confidence {confidence:?})");
            match verdict {
                Verdict::Learn => {
                    self.add_learned(edit.from, edit.to);
                    changed = true;
                }
                Verdict::Suggest => {
                    let suggestions = &mut self.cfg.text.suggestions;
                    let seen = match suggestions.iter_mut().find(|s| s.from.eq_ignore_ascii_case(&edit.from) && s.to == edit.to) {
                        Some(s) => {
                            s.seen += 1;
                            s.seen
                        }
                        None => {
                            suggestions.push(Suggestion { from: edit.from.clone(), to: edit.to.clone(), seen: 1 });
                            1
                        }
                    };
                    if seen >= learn::PROMOTE_AFTER {
                        self.add_learned(edit.from, edit.to);
                    }
                    changed = true;
                }
                Verdict::Ignore(_) => {}
            }
        }
        if changed {
            self.save_learning();
        }
    }

    fn add_learned(&mut self, from: String, to: String) {
        let text = &mut self.cfg.text;
        text.suggestions.retain(|s| !(s.from.eq_ignore_ascii_case(&from) && s.to == to));
        if text.dictionary.iter().any(|r| r.from.eq_ignore_ascii_case(&from) && r.to == to) {
            return;
        }
        text.dictionary.push(Replacement { from: from.clone(), to: to.clone(), learned: true });
        self.emit(Event::Learned { from, to });
    }

    fn ignore_pair(&mut self, from: String, to: String) {
        let ignored = &mut self.cfg.text.ignored;
        if !ignored.iter().any(|r| r.from == from && r.to == to) {
            ignored.push(Replacement { from, to, learned: false });
        }
    }

    /// Applies and saves learning's changes to the dictionary.
    fn save_learning(&mut self) {
        self.text = TextProcessor::new(&self.cfg.text);
        if let Err(e) = self.cfg.save(&paths::config_file()) {
            self.notice(NoticeKind::Error, format!("Couldn't save settings: {e:#}"));
        }
        self.emit(Event::Config(Box::new(self.cfg.clone())));
    }
}

/// One transcript from the pieces of a long dictation: their words in
/// order, and as the time it took, what was left after the recording ended.
fn put_together(id: u64, pieces: Vec<Piece>, finished: Option<Instant>) -> Transcript {
    let heard: Vec<Transcript> = pieces
        .into_iter()
        .filter_map(|p| match p {
            Piece::Heard(t) => Some(t),
            _ => None,
        })
        .collect();
    let text = heard.iter().map(|t| t.text.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ");
    // Learning needs every piece's word confidences, or none.
    let words = heard.iter().map(|t| t.heard.clone()).collect::<Option<Vec<_>>>().map(|w| w.concat());
    Transcript {
        request: id,
        text,
        audio_secs: heard.iter().map(|t| t.audio_secs).sum(),
        infer_ms: finished.map_or(0, |f| f.elapsed().as_millis() as u64),
        model: heard.first().map(|t| t.model.clone()).unwrap_or_default(),
        heard: words,
    }
}

/// Some shortcut is a mouse button.
fn uses_mouse(c: &Config) -> bool {
    c.hotkey.chords.iter().any(|c| c.mouse_button().is_some())
}

/// Whether anything reads the focused app or field.
fn reads_focus(c: &Config) -> bool {
    c.insertion.smart_spacing || c.insertion.split_for_agents || c.text.polish
}

fn spawn_inserter(
    ui: EventSender,
    hotkeys: &Hotkeys,
    internal: Sender<Internal>,
    watch_generation: Arc<AtomicU64>,
) -> Sender<InsertJob> {
    let (tx, rx) = unbounded::<InsertJob>();
    let held = hotkeys.mods_probe();
    thread::Builder::new()
        .name("bars-insert".into())
        .spawn(move || {
            let mut injector = Injector::new();
            for job in rx {
                match job {
                    InsertJob::Text { text, cfg, names, field, watch } => {
                        let wants = cfg.smart_spacing || cfg.split_for_agents;
                        let focus = match field {
                            Field::Now => wants
                                .then(|| focus::capture(cfg.smart_spacing.then_some(focus::CONTEXT_CHARS), FOCUS_TIMEOUT)),
                            Field::Reading(rx) => rx.and_then(|rx| rx.recv_timeout(FOCUS_TIMEOUT).ok()),
                            Field::Read(focus) => focus,
                        };
                        let focus = focus.unwrap_or_default();
                        let inserted =
                            insert_text(&mut injector, &ui, held.as_deref(), &text, &cfg, &names, &focus);
                        if let (Some(request), Some(inserted), true) = (watch, inserted, focus.field.is_some()) {
                            let generation = watch_generation.load(Ordering::Relaxed);
                            let watch = Watch {
                                request,
                                tracker: Tracker::new(&inserted),
                                inserted,
                                pid: focus.pid,
                            };
                            let (internal, gen_now) = (internal.clone(), watch_generation.clone());
                            thread::spawn(move || watch.run(generation, &gen_now, &internal));
                        }
                    }
                    InsertJob::RememberTarget => injector.remember_target(),
                    InsertJob::Copy(text) => {
                        if let Err(e) = injector.copy(&text) {
                            let _ = ui.try_send(Event::Notice {
                                kind: NoticeKind::Error,
                                text: format!("Couldn't copy: {e:#}"),
                            });
                        } else {
                            let _ = ui.try_send(Event::Notice {
                                kind: NoticeKind::Info,
                                text: "Copied".into(),
                            });
                        }
                    }
                }
            }
        })
        .expect("spawn inserter");
    tx
}

/// Follows a dictation in its field for a minute, to see whether the user
/// corrects it. A correction is reported once the text stops changing for
/// a few seconds, and again if it changes after. Stops when another
/// dictation starts, focus moves to another app, or the dictation is gone
/// (a chat message was sent).
struct Watch {
    request: u64,
    inserted: String,
    tracker: Tracker,
    pid: Option<u32>,
}

impl Watch {
    fn run(mut self, generation: u64, current: &AtomicU64, internal: &Sender<Internal>) {
        let inserted = self.inserted.trim().to_string();
        let deadline = Instant::now() + WATCH_FOR;
        let (mut last, mut reported) = (None::<String>, inserted.clone());
        let (mut steady, mut misses) = (0u32, 0u32);
        let report = |edited: &str| {
            let _ = internal.send(Internal::Corrected {
                request: self.request,
                inserted: inserted.clone(),
                edited: edited.to_string(),
            });
        };
        while Instant::now() < deadline {
            thread::sleep(WATCH_EVERY);
            if current.load(Ordering::Relaxed) != generation {
                break;
            }
            let now = focus::capture(Some(WATCH_CHARS), FOCUS_TIMEOUT);
            if now.pid != self.pid {
                break;
            }
            let found = now.field.and_then(|f| self.tracker.locate(&format!("{}{}{}", f.before, f.selected, f.after)));
            let Some(found) = found else {
                misses += 1;
                if misses >= MISSES_ALLOWED {
                    break;
                }
                continue;
            };
            misses = 0;
            if last.as_deref() == Some(found.as_str()) {
                steady += 1;
            } else {
                (last, steady) = (Some(found), 1);
            }
            let settled = last.as_ref().filter(|l| steady >= SETTLED_READS && **l != reported);
            if let Some(edited) = settled {
                report(edited);
                reported = edited.clone();
            }
        }
        if let Some(edited) = last.filter(|l| *l != reported) {
            report(&edited);
        }
    }
}

/// Fits `text` to the focused field, splits it for a terminal agent, and
/// inserts it.
/// Returns the text as it went in, unless it didn't.
fn insert_text(
    injector: &mut Injector,
    ui: &EventSender,
    held: Option<&(dyn Fn() -> Mods + Send + Sync)>,
    text: &str,
    cfg: &InsertionConfig,
    names: &[String],
    focus: &Focus,
) -> Option<String> {
    log::debug!("inserting into {}", focus.summary());
    // A terminal's cursor position can't be trusted: no context.
    let text = match focus.field.as_ref().filter(|_| cfg.smart_spacing && !focus.in_terminal()) {
        Some(field) => text::fit_to_field(text, field, cfg.trailing_space, names),
        None => text::finalize(text, cfg.trailing_space),
    };
    if text.is_empty() {
        return None;
    }
    // Typed text never collapses into "[Pasted text]".
    let agent = focus
        .agent
        .filter(|_| cfg.split_for_agents && cfg.method != crate::config::InsertMethod::Type);
    let pieces = match agent {
        Some(agent) => text::split_for_agent(&text, agent),
        None => vec![text.clone()],
    };
    if injector.waits_for_released_keys(cfg) {
        // Wait for the user to let go of the hotkey modifiers, otherwise
        // our paste chord would combine with them.
        match held {
            Some(held) => {
                let deadline = Instant::now() + Duration::from_millis(1500);
                while held() != Mods::empty() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(15));
                }
            }
            // Backend can't see physical keys (portal): give fingers a
            // moment to leave the modifiers.
            None => thread::sleep(Duration::from_millis(150)),
        }
    }
    let mut on_unread = || {
        let _ = ui.try_send(Event::PasteFailed(text.trim().to_string()));
    };
    match injector.insert(&pieces, cfg, &mut on_unread) {
        Ok(how) => {
            log::info!(
                "inserted {} characters in {} piece(s) ({:?}, {how:?})",
                text.chars().count(),
                pieces.len(),
                cfg.method
            );
            if how == Inserted::NotRead {
                log::info!("no app read the paste: no text field had focus");
                return None;
            }
            if platform::clipboard_restore_skipped() {
                let _ = ui.try_send(Event::Notice {
                    kind: NoticeKind::Info,
                    text: "The clipboard keeps your dictation: macOS asks before Bars reads it. See Restore clipboard in settings".into(),
                });
            }
            Some(text)
        }
        Err(e) => {
            log::warn!("insert failed: {e:#}");
            let _ = ui.try_send(Event::Notice {
                kind: NoticeKind::Warning,
                text: format!("{e:#}"),
            });
            None
        }
    }
}
