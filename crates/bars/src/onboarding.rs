//! First-run assistant: walks through microphone, permissions, the speech
//! model and a first dictation. Platform differences come from
//! `bars_core::platform::requirements`, so the flow is the same everywhere.

use gpui_kit::assets::IconName as I;
use gpui_kit::component::TitleBar;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use bars_core::controller::Command;
use bars_core::models;
use bars_core::platform::requirements::{FixAction, ReqId, ReqStatus, Requirement};
use bars_core::polish;

use crate::state::{AppState, LevelMeter, StateEvent};
use crate::theme::{FONT, Palette, PaletteExt};
use crate::widgets::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Welcome,
    Microphone,
    Access,
    Model,
    Try,
    Done,
}

impl Step {
    /// Dev helper: `BARS_ONBOARDING=access bars` opens the assistant there.
    pub fn from_env() -> Option<Step> {
        Some(match std::env::var("BARS_ONBOARDING").ok()?.as_str() {
            "welcome" => Step::Welcome,
            "microphone" => Step::Microphone,
            "access" => Step::Access,
            "model" => Step::Model,
            "try" => Step::Try,
            "done" => Step::Done,
            _ => return None,
        })
    }
}

const STEPS: [Step; 6] = [Step::Welcome, Step::Microphone, Step::Access, Step::Model, Step::Try, Step::Done];

pub struct Onboarding {
    state: Entity<AppState>,
    step: Step,
    practice: Entity<InputState>,
    level: f32,
    heard_voice: bool,
    elevating: bool,
    transcripts_baseline: u64,
    try_result: Option<String>,
    _subs: Vec<Subscription>,
}

impl Onboarding {
    pub fn new(state: Entity<AppState>, step: Step, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let level = state.read(cx).level.clone();
        let practice = cx.new(|cx| InputState::new(window, cx).placeholder("Your words will appear here…"));
        window.set_window_title("Welcome to Bars");
        let subs = vec![
            cx.observe(&state, |this, state, cx| {
                // A finished dictation during "Try it" is the success signal.
                if this.step == Step::Try && this.try_result.is_none() {
                    let st = state.read(cx);
                    if st.transcripts > this.transcripts_baseline {
                        this.try_result = st.last_transcript.clone();
                    }
                }
                cx.notify();
            }),
            cx.subscribe(&state, |this, _, ev: &StateEvent, cx| {
                if let StateEvent::Notice = ev {
                    // Polkit cancelled/failed: stop showing the spinner.
                    this.elevating = false;
                    cx.notify();
                }
            }),
            cx.observe(&level, |this, level: Entity<LevelMeter>, cx| {
                let l = level.read(cx).level;
                this.level = l;
                if l > 0.32 {
                    this.heard_voice = true;
                }
                if this.step == Step::Microphone {
                    cx.notify();
                }
            }),
            cx.observe_window_appearance(window, |_, window, cx| {
                crate::theme::follow_system(window, cx);
            }),
        ];
        // The window can be closed on any step: the microphone test ends
        // with it, as does a shortcut being recorded.
        cx.on_release(|this, cx| {
            this.state.update(cx, |s, cx| {
                s.send(Command::MicTest(false));
                if s.capturing {
                    s.stop_capture(cx);
                }
            });
        })
        .detach();
        // Permissions are often granted in system settings, outside Bars:
        // re-check while a step is waiting on one.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
                let Ok(waiting) = this.update(cx, |this, cx| {
                    let step = matches!(this.step, Step::Microphone | Step::Access);
                    let pending = this.state.read(cx).permissions.items.iter().any(|r| !r.ok());
                    step && pending
                }) else {
                    break;
                };
                if waiting {
                    let _ = this.update(cx, |this, cx| this.state.read(cx).send(Command::PollPermissions));
                }
            }
        })
        .detach();
        let mut this = Onboarding {
            state,
            step: Step::Welcome,
            practice,
            level: 0.0,
            heard_voice: false,
            elevating: false,
            transcripts_baseline: 0,
            try_result: None,
            _subs: subs,
        };
        this.go(step, window, cx);
        this
    }

    fn requirements(&self, cx: &App) -> Vec<Requirement> {
        self.state
            .read(cx)
            .permissions
            .items
            .iter()
            .filter(|r| r.id != ReqId::Microphone)
            .cloned()
            .collect()
    }

    fn go(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        let leaving = self.step;
        if leaving == Step::Microphone && step != Step::Microphone {
            self.state.read(cx).send(Command::MicTest(false));
        }
        // Platforms with nothing to grant skip the access step.
        let step = if step == Step::Access && self.requirements(cx).is_empty() {
            if leaving > Step::Access { Step::Microphone } else { Step::Model }
        } else {
            step
        };
        self.step = step;
        match step {
            Step::Microphone => {
                self.heard_voice = false;
                self.state.update(cx, |s, cx| s.refresh_devices(cx));
                self.state.read(cx).send(Command::MicTest(true));
            }
            Step::Access => self.state.read(cx).send(Command::RefreshPermissions),
            Step::Model => {
                // Start the downloads right away; it's the slowest step.
                // The polish model comes after the speech model.
                let st = self.state.read(cx);
                let id = st.config.model.id.clone();
                if !st.installed.contains(id.as_str()) && !st.downloads.contains_key(&id) {
                    st.send(Command::DownloadModel(id));
                }
                let polish = models::polish_model(&st.config.text.polish_model).id;
                if st.config.text.polish
                    && polish::supported()
                    && !st.installed.contains(polish)
                    && !st.downloads.contains_key(polish)
                {
                    st.send(Command::DownloadModel(polish.to_string()));
                }
            }
            Step::Try => {
                self.transcripts_baseline = self.state.read(cx).transcripts;
                self.try_result = None;
                self.practice.update(cx, |s, cx| s.focus(window, cx));
            }
            _ => {}
        }
        cx.notify();
    }

    fn next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let i = STEPS.iter().position(|s| *s == self.step).unwrap_or(0);
        if let Some(n) = STEPS.get(i + 1) {
            self.go(*n, window, cx);
        }
    }

    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let i = STEPS.iter().position(|s| *s == self.step).unwrap_or(0);
        if i > 0 {
            self.go(STEPS[i - 1], window, cx);
        }
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            s.edit_config(cx, |c| {
                c.onboarded = true;
                // The model step offered output polish, and the
                // recommended models.
                c.polish_offered = true;
                c.models_offered.push(bars_core::models::DEFAULT_MODEL.to_string());
                c.models_offered.push(bars_core::models::DEFAULT_POLISH.to_string());
            })
        });
        self.state.read(cx).send(Command::MicTest(false));
        window.remove_window();
    }

    fn can_continue(&self, cx: &App) -> bool {
        let st = self.state.read(cx);
        match self.step {
            Step::Access => self.requirements(cx).iter().all(Requirement::ok),
            Step::Model => {
                st.installed.contains(st.config.model.id.as_str()) || st.downloads.contains_key(&st.config.model.id)
            }
            _ => true,
        }
    }

    // ---- steps -----------------------------------------------------------

    fn welcome(&self, p: Palette) -> AnyElement {
        v_col()
            .items_center()
            .gap(px(18.))
            .pt(px(16.))
            .child(div().pb(px(6.)).child(crate::brand::wordmark(340., p.dark)))
            .child(title("Talk instead of typing", p))
            .child(lead(
                "Hold a shortcut, speak, let go: your words appear in whatever app you're using. Everything runs on this computer.",
                p,
            ))
            .child(
                h_row()
                    .gap(px(24.))
                    .mt(px(12.))
                    .child(feature(I::Zap, "Fast", "Text in a blink", p))
                    .child(feature(I::ShieldCheck, "Private", "Nothing leaves your PC", p))
                    .child(feature(I::Keyboard, "Everywhere", "Every app, every field", p)),
            )
            .into_any_element()
    }

    fn microphone(&self, p: Palette, cx: &mut Context<Self>) -> AnyElement {
        let st = self.state.read(cx);
        let mic = st.permissions.items.iter().find(|r| r.id == ReqId::Microphone).cloned();
        let missing = mic.as_ref().is_some_and(|m| m.status == ReqStatus::Missing);
        // A microphone is there, but the OS isn't letting Bars use it.
        let blocked = missing && mic.as_ref().is_some_and(|m| m.granted_note.is_some());
        // Pick another microphone right here; the test switches to it at once.
        let device: AnyElement = if st.devices.is_empty() {
            div()
                .text_size(px(14.))
                .font_weight(FontWeight::MEDIUM)
                .child("Microphone")
                .into_any_element()
        } else {
            let state = self.state.clone();
            let this = cx.entity().downgrade();
            picker("ob-mic", st.mic_options(), st.config.audio.device.clone(), p, move |v, cx| {
                state.update(cx, |s, cx| s.edit_config(cx, |c| c.audio.device = v));
                let _ = this.update(cx, |o, cx| {
                    o.heard_voice = false;
                    cx.notify();
                });
            })
            .into_any_element()
        };
        v_col()
            .gap(px(14.))
            .child(step_icon(I::Mic, p))
            .child(title("Check your microphone", p))
            .child(lead("Say a few words. The bars should move as you talk.", p))
            .child(
                v_col()
                    .mt(px(10.))
                    .p(px(20.))
                    .gap(px(14.))
                    .rounded(px(14.))
                    .bg(p.surface)
                    .border_1()
                    .border_color(p.border)
                    .child(
                        h_row()
                            .justify_between()
                            .child(
                                h_row()
                                    .gap(px(10.))
                                    .child(icon(I::Mic, p.text_muted, 16.))
                                    .child(device),
                            )
                            .child(if blocked {
                                badge("Needs permission", p.warning).into_any_element()
                            } else if missing {
                                badge("No microphone found", p.danger).into_any_element()
                            } else if self.heard_voice {
                                badge("Sounds good", p.success).into_any_element()
                            } else {
                                badge("Listening…", p.accent_text).into_any_element()
                            }),
                    )
                    .child(level_meter(self.level, p)),
            )
            .when(missing, |el| {
                el.child(hint(
                    if blocked {
                        "Allow Bars to use your microphone. This page updates as soon as you do."
                    } else {
                        "Plug in a microphone or headset, then go back and try again."
                    },
                    p,
                ))
            })
            .when_some(
                mic.filter(|_| missing).map(|m| m.actions),
                |el, actions| el.child(self.actions_row(ReqId::Microphone, &actions, p, cx)),
            )
            .into_any_element()
    }

    fn access(&self, p: Palette, cx: &mut Context<Self>) -> AnyElement {
        let reqs = self.requirements(cx);
        let all_ok = reqs.iter().all(Requirement::ok);
        let mut col = v_col()
            .gap(px(14.))
            .child(step_icon(I::ShieldCheck, p))
            .child(title(if all_ok { "You're all set" } else { "Allow Bars to listen for your shortcut" }, p))
            .child(lead(
                "Bars needs a little help from your system to work in every app. You can change this later in Setup.",
                p,
            ));
        for req in reqs {
            col = col.child(self.requirement_card(&req, p, cx));
        }
        col.into_any_element()
    }

    fn requirement_card(&self, req: &Requirement, p: Palette, cx: &mut Context<Self>) -> AnyElement {
        let granted = req.status == ReqStatus::Granted;
        v_col()
            .mt(px(6.))
            .p(px(20.))
            .gap(px(12.))
            .rounded(px(14.))
            .bg(p.surface)
            .border_1()
            .border_color(if granted { p.success } else { p.border })
            .child(
                h_row()
                    .justify_between()
                    .gap(px(12.))
                    .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(req.title.clone()))
                    .child(match req.status {
                        ReqStatus::Granted => badge("Allowed", p.success).into_any_element(),
                        ReqStatus::Missing => badge("Needs permission", p.warning).into_any_element(),
                        ReqStatus::Unknown => badge("Check settings", p.text_muted).into_any_element(),
                    }),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .line_height(px(19.))
                    .text_color(p.text_muted)
                    .child(if granted {
                        req.granted_note.clone().unwrap_or_else(|| req.why.clone())
                    } else {
                        req.why.clone()
                    }),
            )
            .when(!granted, |el| el.child(self.actions_row(req.id, &req.actions, p, cx)))
            .into_any_element()
    }

    fn actions_row(&self, id: ReqId, actions: &[FixAction], p: Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut primary = h_row().gap(px(10.)).flex_wrap();
        let mut extra = v_col().gap(px(10.));
        for (i, action) in actions.iter().enumerate() {
            match action.clone() {
                FixAction::Elevate => {
                    primary = primary.child(if self.elevating {
                        button(("elevate", i), "Waiting for your password…", ButtonKind::Secondary, p)
                            .into_any_element()
                    } else {
                        button(("elevate", i), "Allow access", ButtonKind::Primary, p)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // Linux waits on a password prompt; elsewhere
                                // the OS dialog answers asynchronously.
                                this.elevating = cfg!(target_os = "linux");
                                this.state.read(cx).send(Command::FixPermissions(Some(id)));
                                cx.notify();
                            }))
                            .into_any_element()
                    });
                }
                FixAction::OpenSettings { label, url } => {
                    primary = primary.child(
                        button(("settings", i), label, ButtonKind::Primary, p)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.state.read(cx).send(Command::OpenUrl(url.clone()))
                            })),
                    );
                    primary = primary.child(recheck_button(i, p, cx));
                }
                FixAction::Command(cmd) => {
                    extra = extra.child(command_box(i, cmd, p, cx));
                    primary = primary.child(recheck_button(i, p, cx));
                }
                FixAction::UseAlternative { label, detail } => {
                    extra = extra.child(
                        v_col()
                            .gap(px(4.))
                            .pt(px(4.))
                            .child(
                                div()
                                    .id(("alt", i))
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(p.accent_text)
                                    .cursor_pointer()
                                    .hover(|s| s.underline())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.state.update(cx, |s, cx| {
                                            s.edit_config(cx, |c| {
                                                c.hotkey.backend = bars_core::config::HotkeyBackendKind::Portal;
                                                // Desktops can't bind bare modifiers.
                                                if let Some(first) = c.hotkey.chords.first_mut() {
                                                    if let Some(m) = first.key.as_ref().and_then(bars_core::hotkey::Key::modifier) {
                                                        first.mods |= m;
                                                        first.key = None;
                                                    }
                                                    if first.key.is_none() {
                                                        first.key = Some(bars_core::hotkey::Key("Space".into()));
                                                    }
                                                }
                                            })
                                        });
                                    }))
                                    .child(label),
                            )
                            .child(div().text_size(px(12.)).text_color(p.text_faint).child(detail)),
                    );
                }
            }
        }
        v_col().gap(px(12.)).child(primary).child(extra).into_any_element()
    }

    fn model(&self, p: Palette, cx: &mut Context<Self>) -> AnyElement {
        let st = self.state.read(cx);
        let m = st.active_model().unwrap_or(&models::CATALOG[0]);
        let installed = st.installed.contains(m.id);
        let progress = st.downloads.get(m.id).copied();
        let error = st.download_errors.get(m.id).cloned();
        let small = models::find("canary-180m-flash");
        let polish_card = self.polish_card(p, cx);
        let st = self.state.read(cx);
        let polish_on = st.config.text.polish && polish_card.is_some();
        let polish_id = models::polish_model(&st.config.text.polish_model).id;
        let polish_downloading = polish_on && st.downloads.contains_key(polish_id);
        let ready = installed && (!polish_on || st.installed.contains(polish_id));
        v_col()
            .gap(px(14.))
            .child(step_icon(I::AudioLines, p))
            .child(title(
                match (ready, polish_on) {
                    (true, true) => "Models ready",
                    (true, false) => "Speech model ready",
                    (false, true) => "Getting the models",
                    (false, false) => "Getting the speech model",
                },
                p,
            ))
            .child(lead(
                if polish_on {
                    "Bars transcribes with a model that runs entirely on this computer, and a second one polishes English dictations. It's a one-time download."
                } else {
                    "Bars transcribes with a model that runs entirely on this computer. It's a one-time download."
                },
                p,
            ))
            .child(
                v_col()
                    .mt(px(10.))
                    .p(px(20.))
                    .gap(px(12.))
                    .rounded(px(14.))
                    .bg(p.surface)
                    .border_1()
                    .border_color(if installed { p.success } else { p.border })
                    .child(
                        h_row()
                            .justify_between()
                            .child(
                                v_col()
                                    .gap(px(3.))
                                    .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(m.name))
                                    .child(
                                        div()
                                            .text_size(px(12.5))
                                            .text_color(p.text_muted)
                                            .child(format!("{} · {}", m.languages, fmt_bytes(m.size_bytes()))),
                                    ),
                            )
                            .child(if installed {
                                badge("Ready", p.success).into_any_element()
                            } else if progress.is_some() {
                                badge("Downloading", p.accent_text).into_any_element()
                            } else {
                                button("retry-dl", "Download", ButtonKind::Primary, p)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let id = this.state.read(cx).config.model.id.clone();
                                        this.state.read(cx).send(Command::DownloadModel(id));
                                    }))
                                    .into_any_element()
                            }),
                    )
                    .when_some(progress, |el, (done, total)| {
                        el.child(progress_bar(if total > 0 { done as f32 / total as f32 } else { 0.0 }, p))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(p.text_faint)
                                    .child(format!("{} of {}", fmt_bytes(done), fmt_bytes(total))),
                            )
                    })
                    .when_some(error, |el, e| {
                        el.child(div().text_size(px(12.5)).text_color(p.danger).child(format!("Download failed: {e}")))
                    }),
            )
            .children(polish_card)
            .when(!installed && small.is_some_and(|s| s.id != m.id), |el| {
                let small = small.expect("checked");
                el.child(
                    div()
                        .id("smaller")
                        .text_size(px(13.))
                        .text_color(p.accent_text)
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.state.update(cx, |s, cx| {
                                s.send(Command::CancelDownload(s.config.model.id.clone()));
                                s.edit_config(cx, |c| c.model.id = small.id.to_string());
                                s.send(Command::DownloadModel(small.id.to_string()));
                            })
                        }))
                        .child(format!(
                            "Low on space? Use {} instead ({}, English/German/Spanish/French)",
                            small.name,
                            fmt_bytes(small.size_bytes())
                        )),
                )
            })
            .when((!installed && progress.is_some()) || polish_downloading, |el| {
                el.child(hint("You can continue; the download finishes in the background.", p))
            })
            .into_any_element()
    }

    /// Output polish and its model, with a switch to go without. None where
    /// this processor can't run it.
    fn polish_card(&self, p: Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !polish::supported() {
            return None;
        }
        let st = self.state.read(cx);
        let m = models::polish_model(&st.config.text.polish_model);
        let id = m.id;
        let on = st.config.text.polish;
        let installed = st.installed.contains(m.id);
        let progress = st.downloads.get(m.id).copied().filter(|_| on);
        let error = st.download_errors.get(m.id).cloned().filter(|_| on);
        let state = self.state.clone();
        Some(
            v_col()
                .p(px(20.))
                .gap(px(12.))
                .rounded(px(14.))
                .bg(p.surface)
                .border_1()
                .border_color(if on && installed { p.success } else { p.border })
                .child(
                    h_row()
                        .justify_between()
                        .gap(px(16.))
                        .child(
                            v_col()
                                .flex_1()
                                .min_w_0()
                                .gap(px(3.))
                                .child(
                                    h_row()
                                        .gap(px(10.))
                                        .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child("Output polish"))
                                        .when(on && installed, |el| el.child(badge("Ready", p.success))),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.5))
                                        .line_height(px(18.))
                                        .text_color(p.text_muted)
                                        .child("Cleans up English dictations: drops fillers and false starts, and applies spoken corrections."),
                                )
                                .child(div().text_size(px(12.)).text_color(p.text_faint).child(format!(
                                    "Uses {} by {} · {}",
                                    m.name,
                                    models::polish_maker(m),
                                    fmt_bytes(m.size_bytes())
                                ))),
                        )
                        .child(small_switch("ob-polish", on, move |v, _, cx| {
                            let v = *v;
                            state.update(cx, |s, cx| {
                                s.edit_config(cx, |c| c.text.polish = v);
                                if v {
                                    if !s.installed.contains(id) {
                                        s.send(Command::DownloadModel(id.to_string()));
                                    }
                                } else {
                                    s.send(Command::CancelDownload(id.to_string()));
                                }
                            })
                        })),
                )
                .when_some(progress, |el, (done, total)| {
                    el.child(progress_bar(if total > 0 { done as f32 / total as f32 } else { 0.0 }, p)).child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.text_faint)
                            .child(if done == 0 {
                                format!("Waiting for the speech model · {}", fmt_bytes(total))
                            } else {
                                format!("{} of {}", fmt_bytes(done), fmt_bytes(total))
                            }),
                    )
                })
                .when_some(error, |el, e| {
                    el.child(
                        h_row()
                            .gap(px(10.))
                            .child(div().flex_1().text_size(px(12.5)).text_color(p.danger).child(format!("Download failed: {e}")))
                            .child(button("retry-polish", "Try again", ButtonKind::Secondary, p).on_click(cx.listener(
                                move |this, _, _, cx| this.state.read(cx).send(Command::DownloadModel(id.to_string())),
                            ))),
                    )
                })
                .into_any_element(),
        )
    }

    fn try_it(&self, p: Palette, cx: &mut Context<Self>) -> AnyElement {
        let st = self.state.read(cx);
        let parts = st.shortcut_parts();
        let ready = st.setup_complete();
        v_col()
            .gap(px(14.))
            .child(step_icon(I::Sparkles, p))
            .child(title("Try your first dictation", p))
            .child(lead("Hold the shortcut, say something like “Hello, this is my first dictation”, then let go.", p))
            .child(h_row().mt(px(6.)).justify_center().child(keycaps(parts, p, true)))
            .child(div().mt(px(8.)).child(Input::new(&self.practice)))
            .child(match &self.try_result {
                Some(text) => h_row()
                    .gap(px(10.))
                    .p(px(14.))
                    .rounded(px(12.))
                    .bg(p.accent_soft)
                    .child(icon(I::CircleCheck, p.success, 18.))
                    .child(
                        div()
                            .text_size(px(13.5))
                            .child(format!("Nice! Bars heard: “{}”", text.trim())),
                    )
                    .into_any_element(),
                None if !ready => hint(&crate::wording::needs_for_dictation(), p).into_any_element(),
                None => hint("Tip: double-tap the shortcut to keep talking hands-free, tap again to finish.", p)
                    .into_any_element(),
            })
            .into_any_element()
    }

    fn done(&self, p: Palette, cx: &mut Context<Self>) -> AnyElement {
        let autostart = self.state.read(cx).config.ui.launch_at_login;
        let state = self.state.clone();
        v_col()
            .items_center()
            .gap(px(16.))
            .pt(px(20.))
            .child(
                div()
                    .size(px(72.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(p.accent_soft)
                    .child(icon(I::Check, p.accent_text, 34.)),
            )
            .child(title("Bars is ready", p))
            .child(lead(
                if cfg!(target_os = "macos") {
                    "It lives in the menu bar. Click the icon any time for settings, history and your dictionary."
                } else {
                    "It lives in your system tray. Click the icon any time for settings, history and your dictionary."
                },
                p,
            ))
            .child(
                h_row()
                    .mt(px(10.))
                    .gap(px(12.))
                    .p(px(16.))
                    .rounded(px(12.))
                    .bg(p.surface)
                    .border_1()
                    .border_color(p.border)
                    .child(div().text_size(px(14.)).child("Start Bars when I log in"))
                    .child(small_switch("ob-autostart", autostart, move |v, _, cx| {
                        let v = *v;
                        state.update(cx, |s, cx| s.edit_config(cx, |c| c.ui.launch_at_login = v))
                    })),
            )
            .into_any_element()
    }
}

impl Render for Onboarding {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        // Permission results end the "waiting for password" state.
        if self.elevating && self.requirements(cx).iter().all(Requirement::ok) {
            self.elevating = false;
        }
        let _ = window;
        let body = match self.step {
            Step::Welcome => self.welcome(p),
            Step::Microphone => self.microphone(p, cx),
            Step::Access => self.access(p, cx),
            Step::Model => self.model(p, cx),
            Step::Try => self.try_it(p, cx),
            Step::Done => self.done(p, cx),
        };
        let index = STEPS.iter().position(|s| *s == self.step).unwrap_or(0);
        let can_continue = self.can_continue(cx);
        let (primary_label, primary_kind) = match self.step {
            Step::Welcome => ("Get started", ButtonKind::Primary),
            Step::Try if self.try_result.is_none() => ("Skip", ButtonKind::Secondary),
            Step::Done => ("Finish", ButtonKind::Primary),
            _ => ("Continue", ButtonKind::Primary),
        };

        v_col()
            .size_full()
            .bg(p.bg)
            .font_family(FONT)
            .text_color(p.text)
            .child(TitleBar::new().bg(p.bg).border_color(p.bg))
            .child(
                // Step dots.
                h_row().justify_center().gap(px(8.)).pt(px(6.)).children(STEPS.iter().enumerate().map(|(i, _)| {
                    div()
                        .h(px(6.))
                        .w(px(if i == index { 22. } else { 6. }))
                        .rounded_full()
                        .bg(if i <= index { p.accent } else { p.border_strong })
                })),
            )
            .child(
                div()
                    .id("onboarding-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(div().max_w(px(560.)).mx_auto().px(px(32.)).py(px(28.)).child(body)),
            )
            .child(
                h_row()
                    .justify_between()
                    .px(px(28.))
                    .py(px(18.))
                    .border_t_1()
                    .border_color(p.border)
                    .child(if index > 0 && self.step != Step::Done {
                        button("back", "Back", ButtonKind::Ghost, p)
                            .on_click(cx.listener(|this, _, window, cx| this.back(window, cx)))
                            .into_any_element()
                    } else {
                        div().into_any_element()
                    })
                    .child(
                        button("next", primary_label, primary_kind, p)
                            .px(px(18.))
                            .when(!can_continue, |el| el.opacity(0.45))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if !this.can_continue(cx) {
                                    return;
                                }
                                if this.step == Step::Done {
                                    this.finish(window, cx);
                                } else {
                                    this.next(window, cx);
                                }
                            })),
                    ),
            )
    }
}

// ---- small pieces ---------------------------------------------------------

fn title(text: &str, p: Palette) -> impl IntoElement {
    div()
        .text_size(px(26.))
        .font_weight(FontWeight::BOLD)
        .text_color(p.text)
        .child(text.to_string())
}

fn lead(text: &str, p: Palette) -> impl IntoElement {
    div()
        .text_size(px(14.5))
        .line_height(px(22.))
        .text_color(p.text_muted)
        .child(text.to_string())
}

fn hint(text: &str, p: Palette) -> Div {
    div().text_size(px(12.5)).line_height(px(18.)).text_color(p.text_faint).child(text.to_string())
}

fn step_icon(name: I, p: Palette) -> impl IntoElement {
    div()
        .size(px(48.))
        .rounded(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .bg(p.accent_soft)
        .child(icon(name, p.accent_text, 24.))
}

fn feature(name: I, title_text: &str, sub: &str, p: Palette) -> impl IntoElement {
    v_col()
        .items_center()
        .gap(px(6.))
        .w(px(140.))
        .child(icon(name, p.accent_text, 22.))
        .child(div().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).child(title_text.to_string()))
        .child(div().text_size(px(12.)).text_color(p.text_muted).child(sub.to_string()))
}

fn level_meter(level: f32, p: Palette) -> impl IntoElement {
    let segments = 28;
    let lit = (level.clamp(0.0, 1.0) * segments as f32).round() as usize;
    h_row().gap(px(3.)).h(px(18.)).children((0..segments).map(move |i| {
        let on = i < lit;
        let mut c = if i as f32 > segments as f32 * 0.85 { p.warning } else { p.accent };
        if !on {
            c = p.border_strong;
        }
        div().flex_1().h_full().rounded(px(2.)).bg(c)
    }))
}

fn recheck_button(i: usize, p: Palette, cx: &mut Context<Onboarding>) -> impl IntoElement {
    button(("recheck", i), "Check again", ButtonKind::Ghost, p)
        .on_click(cx.listener(|this, _, _, cx| this.state.read(cx).send(Command::RefreshPermissions)))
}

fn command_box(i: usize, cmd: String, p: Palette, cx: &mut Context<Onboarding>) -> impl IntoElement {
    v_col()
        .gap(px(8.))
        .child(
            div()
                .text_size(px(12.5))
                .text_color(p.text_muted)
                .child("Run this once in a terminal, then press Check again:"),
        )
        .child(
            h_row()
                .gap(px(8.))
                .p(px(10.))
                .rounded(px(8.))
                .bg(p.surface_2)
                .border_1()
                .border_color(p.border)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family("monospace")
                        .text_size(px(11.5))
                        .child(cmd.lines().next().unwrap_or_default().to_string() + " …"),
                )
                .child(
                    button(("copy", i), "Copy", ButtonKind::Secondary, p).on_click(cx.listener(move |this, _, _, cx| {
                        this.state.read(cx).send(Command::CopyText(cmd.clone()))
                    })),
                ),
        )
}

/// Opens the assistant window.
pub fn open(state: Entity<AppState>, step: Step, cx: &mut App) -> anyhow::Result<WindowHandle<gpui_kit::base::Root>> {
    let bounds = Bounds::centered(None, size(px(720.), px(640.)), cx);
    let opts = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(560.), px(520.))),
        window_decorations: Some(WindowDecorations::Client),
        app_id: Some(bars_core::paths::APP_ID.into()),
        icon: crate::brand::window_icon(),
        focus: true,
        show: true,
        ..TitleBar::window_options()
    };
    let (handle, _) = gpui_kit::open_window(opts, cx, |window, cx| {
        crate::theme::follow_system(window, cx);
        cx.new(|cx| Onboarding::new(state, step, window, cx))
    })?;
    handle
        .downcast::<gpui_kit::base::Root>()
        .ok_or_else(|| anyhow::anyhow!("unexpected root type"))
}
