//! App-level coordination: owns the settings and overlay windows, keeps the
//! tray icon in sync, and routes tray/D-Bus commands.

use std::time::Duration;

use gpui_kit::*;
use bars_core::config::OverlayMode;
use bars_core::controller::{Command, NoticeKind, Phase};
use bars_core::models;
use bars_core::update::Release;

use crate::brand::TrayState;
use crate::onboarding::{self, Step};
use crate::overlay::{self, Overlay};
use crate::settings::{self, Page};
use crate::state::{AppState, StateEvent, UpdateStatus};
use crate::theme;

#[derive(Clone, Debug)]
pub enum ShellCommand {
    Toggle,
    /// Sent by `bars --cancel`.
    Cancel,
    PasteLast,
    ShowSettings(Option<Page>),
    /// Open the setup assistant (at the first unfinished step).
    ShowOnboarding,
    /// Install the update found, or look for one.
    Update,
    Quit,
}

/// How the app was started, which decides how to ask for missing setup.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Launch {
    /// Started by the user: open a window.
    Interactive,
    /// Started at login: stay quiet, use a notification if needed.
    Background,
}

const SETUP_NOTIFICATION: &str = "bars-setup";
const UPDATE_NOTIFICATION: &str = "bars-update";
const POLISH_NOTIFICATION: &str = "bars-polish";
const NOTICE_NOTIFICATION: &str = "bars-notice";
/// How long after starting to look for a newer Bars (when allowed), and how
/// often to see whether it's time to look again (every 24 hours by the
/// clock, which keeps going while the computer sleeps; timers don't).
const UPDATE_FIRST: Duration = Duration::from_secs(60);
const UPDATE_TICK: Duration = Duration::from_secs(10 * 60);

/// Lets any view ask the shell for something (open a window, etc).
pub struct ShellSender(pub async_channel::Sender<ShellCommand>);

impl Global for ShellSender {}

pub fn send(cx: &App, cmd: ShellCommand) {
    if let Some(s) = cx.try_global::<ShellSender>() {
        let _ = s.0.try_send(cmd);
    }
}

pub struct Shell {
    state: Entity<AppState>,
    settings: Option<WindowHandle<gpui_kit::base::Root>>,
    onboarding: Option<WindowHandle<gpui_kit::base::Root>>,
    overlay: Option<WindowHandle<Overlay>>,
    overlay_close: Option<Task<()>>,
    overlay_open: Option<Task<()>>,
    tray: Option<crate::tray::TrayHandle>,
    /// macOS: the Dock icon is showing (while settings or setup is open).
    in_dock: bool,
    restarting: bool,
    _subs: Vec<Subscription>,
}

impl Shell {
    pub fn new(state: Entity<AppState>, commands: async_channel::Receiver<ShellCommand>, tray_tx: async_channel::Sender<ShellCommand>, cx: &mut Context<Self>) -> Self {
        let weak = cx.weak_entity();
        let subs = vec![
            cx.subscribe(&state, |this, _, ev: &StateEvent, cx| this.on_state(ev, cx)),
            // The window may close in the middle of updating the shell.
            cx.on_window_closed(move |cx, _| {
                let weak = weak.clone();
                cx.defer(move |cx| {
                    let _ = weak.update(cx, |this, cx| this.sync_dock(cx));
                });
            }),
        ];

        // Route tray / D-Bus commands into the app.
        cx.spawn(async move |this, cx| {
            while let Ok(cmd) = commands.recv().await {
                if this.update(cx, |this, cx| this.on_command(cmd, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let tray = crate::tray::TrayHandle::spawn(tray_tx);

        // Notification buttons ("Finish setup", "Update") come back here.
        let weak = cx.weak_entity();
        cx.on_system_notification_response(move |response, cx| {
            let _ = weak.update(cx, |this, cx| match response.tag.as_ref() {
                SETUP_NOTIFICATION => this.show_onboarding(cx),
                UPDATE_NOTIFICATION if response.action_id.is_some() => {
                    this.state.update(cx, |s, cx| s.install_update(cx))
                }
                UPDATE_NOTIFICATION => this.show_settings(Some(Page::General), cx),
                POLISH_NOTIFICATION if response.action_id.is_some() => {
                    let st = this.state.read(cx);
                    st.send(Command::DownloadModel(models::polish_model(&st.config.text.polish_model).id.to_string()))
                }
                POLISH_NOTIFICATION => this.show_settings(Some(Page::Home), cx),
                _ => {}
            });
        });

        Shell {
            state,
            settings: None,
            onboarding: None,
            overlay: None,
            overlay_close: None,
            overlay_open: None,
            tray,
            in_dock: false,
            restarting: false,
            _subs: subs,
        }
    }

    fn on_state(&mut self, ev: &StateEvent, cx: &mut Context<Self>) {
        let overlay_enabled = self.state.read(cx).config.ui.overlay != OverlayMode::Never;
        match ev {
            StateEvent::PhaseChanged(phase) => {
                if let Some(t) = &self.tray {
                    t.set_state(match phase {
                        Phase::Idle => TrayState::Idle,
                        Phase::Recording { .. } => TrayState::Recording,
                        Phase::Transcribing => TrayState::Busy,
                    });
                }
                match phase {
                    Phase::Recording { locked: false } if overlay_enabled => self.show_overlay_soon(cx),
                    Phase::Recording { locked: true } | Phase::Transcribing if overlay_enabled => {
                        self.show_overlay(cx)
                    }
                    // The pill hides itself; keep the surface briefly so the
                    // next dictation shows instantly.
                    Phase::Idle => {
                        self.overlay_open = None;
                        // Linux overlays are click-through when hidden; elsewhere
                        // close soon so the transparent window can't block clicks.
                        let keep = if cfg!(target_os = "linux") { 20_000 } else { 3_200 };
                        self.schedule_overlay_close(Duration::from_millis(keep), cx)
                    }
                    _ => {}
                }
            }
            StateEvent::Notice => {
                if self.settings_window_active(cx) {
                    // The settings window shows it.
                } else if overlay_enabled {
                    self.show_overlay(cx);
                    if self.state.read(cx).phase == Phase::Idle {
                        self.schedule_overlay_close(Duration::from_millis(3200), cx);
                    }
                } else if let Some(n) = self.state.read(cx).notice.clone().filter(|n| n.kind != NoticeKind::Info) {
                    // The pill is turned off, but a problem still has to
                    // reach the user.
                    cx.show_system_notification(SystemNotification {
                        tag: NOTICE_NOTIFICATION.into(),
                        title: "Bars".into(),
                        body: n.text,
                        actions: Vec::new(),
                    });
                }
            }
            // Stay up longer: they offer to copy the text, or to undo.
            StateEvent::PasteFailed | StateEvent::Learned => {
                if overlay_enabled {
                    self.show_overlay(cx);
                    if self.state.read(cx).phase == Phase::Idle {
                        self.schedule_overlay_close(crate::overlay::PASTE_FAILED_FOR + Duration::from_millis(600), cx);
                    }
                }
            }
            StateEvent::ThemeChanged => self.apply_theme(cx),
            StateEvent::UpdateFound => self.notify_update(cx),
            StateEvent::UpdateResult => {
                if !self.settings_window_active(cx) {
                    self.notify_update(cx);
                }
            }
            // Windows: the installer takes over. macOS: the old app keeps
            // running from a bundle that's been replaced, so it restarts.
            // Both wait for a dictation under way to finish. Linux keeps
            // running the old binary until asked.
            StateEvent::UpdateInstalled if cfg!(any(target_os = "windows", target_os = "macos")) => {
                self.restart_when_idle(Duration::ZERO, cx)
            }
            StateEvent::UpdateInstalled => self.ask_to_restart(cx),
            StateEvent::RestartToUpdate => self.restart_when_idle(Duration::ZERO, cx),
            StateEvent::Restart => self.restart_when_idle(Duration::from_secs(3), cx),
        }
        if let Some(t) = &mut self.tray {
            let st = self.state.read(cx);
            t.set_update(update_label(&st.update));
        }
        let needs = {
            let st = self.state.read(cx);
            st.permissions_known() && !st.setup_complete()
        };
        if let Some(t) = &mut self.tray {
            t.set_needs_setup(needs);
        }
    }

    /// Restarts Bars once no dictation is under way, and not before `after`
    /// (a notice saying so has been up a while).
    fn restart_when_idle(&mut self, after: Duration, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.restarting, true) {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(after).await;
            loop {
                match this.update(cx, |this, cx| this.state.read(cx).phase == Phase::Idle) {
                    Ok(true) => break,
                    Ok(false) => cx.background_executor().timer(Duration::from_millis(500)).await,
                    Err(_) => return,
                }
            }
            let _ = this.update(cx, |this, cx| {
                // Windows: the installer closes Bars and starts the new
                // version. This process ends inside the call.
                #[cfg(target_os = "windows")]
                if let Err(e) = bars_core::update::run_installer() {
                    log::error!("could not run the installer: {e:#}");
                    this.restarting = false;
                    this.state.update(cx, |s, cx| s.installer_failed(cx));
                    return;
                }
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                if let Err(e) = bars_core::update::relaunch_after_exit() {
                    log::error!("could not restart Bars: {e:#}");
                    this.restarting = false;
                    cx.show_system_notification(SystemNotification {
                        tag: UPDATE_NOTIFICATION.into(),
                        title: "Bars couldn't restart".into(),
                        body: "Quit Bars and open it again.".into(),
                        actions: Vec::new(),
                    });
                    return;
                }
                this.on_command(ShellCommand::Quit, cx);
            });
        })
        .detach();
    }

    /// Called once at startup: greet new users, and make sure anyone with
    /// missing setup is guided to it in a way that fits how we launched.
    pub fn startup(&mut self, launch: Launch, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            // Wait for the controller's first permission/model report.
            for _ in 0..50 {
                let known = this.update(cx, |this, cx| this.state.read(cx).permissions_known()).unwrap_or(true);
                if known {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(60)).await;
            }
            let _ = this.update(cx, |this, cx| {
                let st = this.state.read(cx);
                let first_run = !st.config.onboarded;
                let complete = st.setup_complete();
                let forced = Step::from_env().is_some();
                match (launch, first_run || !complete || forced) {
                    (Launch::Interactive, true) => this.show_onboarding(cx),
                    (Launch::Interactive, false) => {
                        let page = Page::from_env().unwrap_or(Page::Home);
                        this.show_settings(Some(page), cx)
                    }
                    (Launch::Background, true) => this.notify_setup(cx),
                    (Launch::Background, false) => {}
                }
                this.offer_polish(launch, cx);
            });
        })
        .detach();

        // Look for a newer Bars soon after starting, then every 24 hours
        // while running, when allowed.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(UPDATE_FIRST).await;
            loop {
                let alive = this.update(cx, |this, cx| {
                    if this.state.read(cx).config.ui.check_updates {
                        this.state.update(cx, |s, cx| s.check_for_updates_if_due(cx));
                    }
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(UPDATE_TICK).await;
            }
        })
        .detach();
    }

    /// Asks whether to restart into the update just installed: in the
    /// settings window when it's open, otherwise in a notification.
    fn ask_to_restart(&mut self, cx: &mut Context<Self>) {
        let Some(version) = self.state.read(cx).update.offered().map(|r| r.version.clone()) else {
            return;
        };
        let title = format!("Bars ver. {version} is installed");
        let body = "Restart Bars to start using it.";
        let asked = self.settings.and_then(|h| {
            h.update(cx, |_, window, cx| {
                window.activate_window();
                window.prompt(PromptLevel::Info, &title, Some(body), &["Restart", "Later"], cx)
            })
            .ok()
        });
        match asked {
            Some(answer) => {
                let state = self.state.clone();
                cx.spawn(async move |_, cx| {
                    if answer.await == Ok(0) {
                        state.update(cx, |s, cx| s.install_update(cx));
                    }
                })
                .detach();
            }
            None => cx.show_system_notification(SystemNotification {
                tag: UPDATE_NOTIFICATION.into(),
                title: title.into(),
                body: body.into(),
                actions: vec![SystemNotificationAction {
                    id: "restart".into(),
                    label: "Restart".into(),
                }],
            }),
        }
    }

    /// Tells the user what looking for an update found.
    fn notify_update(&mut self, cx: &mut Context<Self>) {
        let current = env!("CARGO_PKG_VERSION");
        let (title, body, action): (String, String, Option<&str>) = match &self.state.read(cx).update {
            UpdateStatus::Available(r) => (
                "New version available".into(),
                match &r.notes {
                    Some(notes) => format!("ver. {}: {notes}", r.version),
                    None => format!("ver. {} is out. You have ver. {current}.", r.version),
                },
                Some(update_action(r)),
            ),
            UpdateStatus::UpToDate => ("Bars is up to date".into(), format!("ver. {current} is the latest version."), None),
            UpdateStatus::InstallFailed(_, why) => ("Bars couldn't update".into(), why.clone(), Some("Try again")),
            UpdateStatus::Failed(why) => ("Bars couldn't update".into(), why.clone(), None),
            _ => return,
        };
        cx.show_system_notification(SystemNotification {
            tag: UPDATE_NOTIFICATION.into(),
            title: title.into(),
            body: body.into(),
            actions: action
                .map(|label| SystemNotificationAction {
                    id: "update".into(),
                    label: label.into(),
                })
                .into_iter()
                .collect(),
        });
    }

    /// People who had Bars before output polish are asked once to download
    /// its model: in a notification when Bars started at login, otherwise
    /// on the Home page that just opened (which keeps asking until they
    /// answer).
    fn offer_polish(&mut self, launch: Launch, cx: &mut Context<Self>) {
        let st = self.state.read(cx);
        if st.config.polish_offered || !st.polish_needs_download() {
            return;
        }
        if launch == Launch::Background {
            cx.show_system_notification(SystemNotification {
                tag: POLISH_NOTIFICATION.into(),
                title: "New: Output polish".into(),
                body: format!(
                    "Bars can now clean up English dictations: fillers, false starts and spoken corrections. It needs a one-time download ({}).",
                    crate::widgets::fmt_bytes(models::polish_model(&st.config.text.polish_model).size_bytes())
                )
                .into(),
                actions: vec![SystemNotificationAction {
                    id: "download".into(),
                    label: "Download".into(),
                }],
            });
        }
        self.state.update(cx, |s, cx| s.edit_config(cx, |c| c.polish_offered = true));
    }

    fn notify_setup(&mut self, cx: &mut Context<Self>) {
        cx.show_system_notification(SystemNotification {
            tag: SETUP_NOTIFICATION.into(),
            title: "Bars needs a moment of your time".into(),
            body: "Finish setup to start dictating with your shortcut.".into(),
            actions: vec![SystemNotificationAction {
                id: "setup".into(),
                label: "Finish setup".into(),
            }],
        });
    }

    pub fn show_onboarding(&mut self, cx: &mut Context<Self>) {
        bring_to_front(cx);
        if let Some(h) = self.onboarding
            && h.update(cx, |_, window, _| window.activate_window()).is_ok()
        {
            return;
        }
        let step = self.first_unfinished_step(cx);
        match onboarding::open(self.state.clone(), step, cx) {
            Ok(h) => self.onboarding = Some(h),
            Err(e) => log::error!("could not open setup assistant: {e:#}"),
        }
        self.sync_dock(cx);
    }

    /// macOS: Bars stays out of the Dock (it lives in the menu bar), except
    /// while the settings or setup window is open, so it can be switched
    /// back to like any app. Elsewhere those windows already get a taskbar
    /// button and the recording pill never does.
    fn sync_dock(&mut self, cx: &mut Context<Self>) {
        let alive = |h: Option<WindowHandle<gpui_kit::base::Root>>, cx: &mut Context<Self>| {
            h.is_some_and(|h| h.update(cx, |_, _, _| ()).is_ok())
        };
        let want = alive(self.settings, cx) || alive(self.onboarding, cx);
        if want == self.in_dock {
            return;
        }
        self.in_dock = want;
        #[cfg(target_os = "macos")]
        if want {
            cx.set_activation_policy(ActivationPolicy::Regular);
            // Without this the menu bar may not appear until reactivation.
            cx.activate(true);
        } else {
            cx.set_activation_policy(ActivationPolicy::Accessory);
        }
    }

    fn first_unfinished_step(&self, cx: &App) -> Step {
        if let Some(step) = Step::from_env() {
            return step;
        }
        let st = self.state.read(cx);
        if !st.config.onboarded {
            return Step::Welcome;
        }
        if st.permissions.items.iter().any(|r| !r.ok()) || !st.hotkeys_active {
            return Step::Access;
        }
        if !st.installed.contains(st.config.model.id.as_str()) {
            return Step::Model;
        }
        Step::Welcome
    }

    fn settings_window_active(&self, cx: &mut Context<Self>) -> bool {
        self.settings
            .and_then(|h| h.update(cx, |_, window, _| window.is_window_active()).ok())
            .unwrap_or(false)
    }

    pub fn apply_theme(&mut self, cx: &mut Context<Self>) {
        let pref = self.state.read(cx).config.ui.theme;
        theme::set_pref(pref, cx);
        let dark = theme::wants_dark(pref, cx);
        theme::apply(dark, cx);
        cx.refresh_windows();
    }

    /// Push-to-talk chords are often the start of another shortcut
    /// (e.g. Ctrl+Super+Left); wait a beat so those never flash the pill.
    fn show_overlay_soon(&mut self, cx: &mut Context<Self>) {
        self.overlay_close = None;
        if self.overlay_alive(cx) {
            return;
        }
        self.overlay_open = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(110)).await;
            let _ = this.update(cx, |this, cx| {
                if matches!(this.state.read(cx).phase, Phase::Recording { .. }) {
                    this.show_overlay(cx);
                }
            });
        }));
    }

    fn overlay_alive(&self, cx: &mut Context<Self>) -> bool {
        self.overlay.is_some_and(|h| h.update(cx, |_, _, _| ()).is_ok())
    }

    fn show_overlay(&mut self, cx: &mut Context<Self>) {
        self.overlay_close = None;
        if !self.overlay_alive(cx) {
            self.overlay = overlay::open(self.state.clone(), cx);
        }
    }

    fn schedule_overlay_close(&mut self, after: Duration, cx: &mut Context<Self>) {
        let Some(handle) = self.overlay else { return };
        self.overlay_close = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(after).await;
            let _ = this.update(cx, |this, cx| {
                if this.state.read(cx).phase == Phase::Idle {
                    let _ = handle.update(cx, |_, window, _| window.remove_window());
                    this.overlay = None;
                }
            });
        }));
    }

    pub fn show_settings(&mut self, page: Option<Page>, cx: &mut Context<Self>) {
        bring_to_front(cx);
        if let Some(h) = self.settings {
            let alive = h
                .update(cx, |root, window, cx| {
                    window.activate_window();
                    if let Some(page) = page
                        && let Ok(view) = root.view().clone().downcast::<settings::SettingsView>()
                    {
                        view.update(cx, |v, cx| v.go(page, cx));
                    }
                })
                .is_ok();
            if alive {
                return;
            }
        }
        match settings::open(self.state.clone(), page.unwrap_or(Page::Home), cx) {
            Ok(h) => self.settings = Some(h),
            Err(e) => log::error!("could not open settings: {e:#}"),
        }
        self.sync_dock(cx);
    }

    fn on_command(&mut self, cmd: ShellCommand, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        match cmd {
            ShellCommand::Toggle => state.send(Command::Toggle),
            ShellCommand::Cancel => state.send(Command::Cancel),
            ShellCommand::PasteLast => state.send(Command::PasteLast),
            ShellCommand::ShowSettings(page) => self.show_settings(page, cx),
            ShellCommand::ShowOnboarding => self.show_onboarding(cx),
            ShellCommand::Update => self.state.update(cx, |s, cx| match s.update {
                UpdateStatus::Available(_) | UpdateStatus::InstallFailed(..) | UpdateStatus::Installed(_) => {
                    s.install_update(cx)
                }
                _ => s.check_for_updates(true, cx),
            }),
            ShellCommand::Quit => {
                state.controller.shutdown();
                cx.quit();
            }
        }
    }
}

/// What updating to `release` does here: installs it, or opens the release
/// page.
pub fn update_action(release: &Release) -> &'static str {
    if release.installable() { "Update" } else { "Download" }
}

/// The tray's update item: "Update to ver. 0.4.0…" once one is found, and
/// a restart once it's installed.
pub fn update_label(status: &UpdateStatus) -> String {
    match status {
        UpdateStatus::Installed(_) => "Restart to Finish Updating".into(),
        _ => match status.offered() {
            Some(r) if r.installable() => format!("Update to ver. {}…", r.version),
            Some(r) => format!("Download ver. {}…", r.version),
            None => "Check for Updates…".into(),
        },
    }
}

/// macOS: Bars has no Dock icon, so the app isn't active when the tray or a
/// second launch asks for a window; activate it so the window comes forward.
fn bring_to_front(cx: &mut App) {
    #[cfg(target_os = "macos")]
    cx.activate(true);
    #[cfg(not(target_os = "macos"))]
    let _ = cx;
}
