use gpui_kit::assets::IconName as I;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use bars_core::config::Config;
use bars_core::controller::Command;
use bars_core::engine::EngineStatus;
use bars_core::models::{self, ModelInfo};
use bars_core::polish::{self, PolishStatus};

use super::SettingsView;
use crate::theme::{Palette, PaletteExt};
use crate::widgets::*;

const POLISH_HINT: &str = "Writes what you said the way you'd have typed it: fillers, repeats and false starts go, corrections like “no wait” are applied. Questions and instructions stay as you said them. English dictations only.";

const LANGUAGES: &[(&str, &str)] = &[
    ("en", "English"),
    ("de", "German"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("it", "Italian"),
    ("pt", "Portuguese"),
    ("nl", "Dutch"),
    ("pl", "Polish"),
    ("sv", "Swedish"),
    ("da", "Danish"),
    ("fi", "Finnish"),
    ("cs", "Czech"),
    ("uk", "Ukrainian"),
    ("ru", "Russian"),
];

impl SettingsView {
    pub fn render_model(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let st = self.state.read(cx);
        let active_id = st.config.model.id.clone();
        let multilingual = st.active_model().is_some_and(|m| m.multilingual);
        let language = st.config.model.language.clone();
        let engine = st.engine.clone();

        let cards: Vec<AnyElement> = models::CATALOG
            .iter()
            .map(|m| self.model_card(m, Slot::Speech, m.id == active_id, &engine, p, cx).into_any_element())
            .collect();

        let lang_options: Vec<(Option<String>, String)> = std::iter::once((None, "Auto-detect".to_string()))
            .chain(LANGUAGES.iter().map(|(c, n)| (Some(c.to_string()), n.to_string())))
            .collect();
        let state = self.state.clone();
        let lang_picker = picker(
            "language",
            lang_options,
            language,
            p,
            move |v, cx| state.update(cx, |s, cx| s.edit_config(cx, |c| c.model.language = v)),
        );

        let polish = self.polish_section(&engine, p, cx);
        v_col()
            .child(page_header(
                "Models",
                Some("Everything runs on this computer. Audio and text never leave it."),
                p,
            ))
            .child(section("Speech", p).mb(px(28.)).child(v_col().gap(px(12.)).children(cards)))
            .when(multilingual, |el| {
                el.child(section("Language", p).child(rows(
                    p,
                    vec![row(
                        "Spoken language",
                        Some("Auto-detect works well; pin a language if you only speak one."),
                        lang_picker,
                        p,
                    )
                    .into_any_element()],
                )))
            })
            .child(polish)
    }

    /// Output polish: the switch, then the models that can do it, as cards
    /// like the speech models'.
    fn polish_section(&self, engine: &EngineStatus, p: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let st = self.state.read(cx);
        let on = st.config.text.polish;
        let id = models::polish_model(&st.config.text.polish_model).id;
        let supported = polish::supported();
        let state = self.state.clone();

        let switch = small_switch("polish", on, move |v, _, cx| {
            let v = *v;
            state.update(cx, |s, cx| {
                s.edit_config(cx, |c| c.text.polish = v);
                // Turning it on fetches the model it needs.
                if v && supported && !s.installed.contains(id) {
                    s.send(Command::DownloadModel(id.to_string()));
                }
            })
        });
        let models: AnyElement = if supported {
            let cards: Vec<AnyElement> = models::POLISH_MODELS
                .iter()
                .map(|m| self.model_card(m, Slot::Polish, m.id == id, engine, p, cx).into_any_element())
                .collect();
            v_col().gap(px(12.)).children(cards).into_any_element()
        } else {
            div()
                .px(px(4.))
                .text_size(px(12.5))
                .text_color(p.text_muted)
                .child("This processor can't run it: it needs AVX2 (Intel since 2013, AMD since 2015).")
                .into_any_element()
        };

        section("Output polish", p)
            .child(rows(p, vec![row("Polish English dictations", Some(POLISH_HINT), switch, p).into_any_element()]))
            .child(div().pt(px(4.)).child(models))
    }

    /// A model's card: `active` is the one its slot uses (the speech
    /// model, or the polish model picked).
    fn model_card(
        &self,
        m: &'static ModelInfo,
        slot: Slot,
        active: bool,
        engine: &EngineStatus,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let st = self.state.read(cx);
        let installed = st.installed.contains(m.id);
        let progress = st.downloads.get(m.id).copied();
        let error = st.download_errors.get(m.id).cloned();
        let id = m.id;
        // A polish model picked while polish is off isn't in use.
        let in_use = active && (slot == Slot::Speech || st.config.text.polish);
        // Like a model in use, it can't be deleted (Windows can't delete a
        // file in use).
        let deletable = installed && !in_use;
        let (loading, failure) = match slot {
            Slot::Speech => match engine {
                EngineStatus::Loading(_) => (true, None),
                EngineStatus::Failed { error, .. } => (false, Some(error.clone())),
                _ => (false, None),
            },
            Slot::Polish => match &st.polish {
                PolishStatus::Loading => (true, None),
                PolishStatus::Failed(e) => (false, Some(e.clone())),
                _ => (false, None),
            },
        };
        let failure = failure.filter(|_| in_use);

        let status: AnyElement = if in_use {
            if loading {
                badge("Loading…", p.accent).into_any_element()
            } else if failure.is_some() {
                badge("Failed to load", p.danger).into_any_element()
            } else if installed {
                badge("In use", p.success).into_any_element()
            } else {
                badge("Not downloaded", p.warning).into_any_element()
            }
        } else if installed {
            badge("Downloaded", p.text_muted).into_any_element()
        } else {
            div().into_any_element()
        };

        let actions: AnyElement = if let Some((done, total)) = progress {
            h_row()
                .gap(px(10.))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(p.text_muted)
                        .child(format!("{} / {}", fmt_bytes(done), fmt_bytes(total))),
                )
                .child(
                    button(SharedString::from(format!("cancel-{id}")), "Cancel", ButtonKind::Ghost, p).on_click(
                        cx.listener(move |this, _, _, cx| this.state.read(cx).send(Command::CancelDownload(id.to_string()))),
                    ),
                )
                .into_any_element()
        } else if !installed {
            button(SharedString::from(format!("dl-{id}")), format!("Download · {}", fmt_bytes(m.size_bytes())), ButtonKind::Primary, p)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.state.update(cx, |s, cx| {
                        s.send(Command::DownloadModel(id.to_string()));
                        if !s.installed.contains(slot.current(&s.config)) {
                            // First model: make it the active one.
                            s.edit_config(cx, |c| slot.pick(c, id));
                        }
                    });
                }))
                .into_any_element()
        } else {
            h_row()
                .gap(px(6.))
                .when(!active, |el| {
                    el.child(
                        button(SharedString::from(format!("use-{id}")), "Use", ButtonKind::Secondary, p).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.state.update(cx, |s, cx| s.edit_config(cx, |c| slot.pick(c, id)))
                            }),
                        ),
                    )
                })
                .when(deletable, |el| {
                    el.child(
                        icon_button(SharedString::from(format!("rm-{id}")), I::Trash, p.text_muted, p).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.state.read(cx).send(Command::DeleteModel(id.to_string()))
                            }),
                        ),
                    )
                })
                .into_any_element()
        };

        v_col()
            .p(px(18.))
            .gap(px(12.))
            .rounded(px(12.))
            .bg(p.surface)
            .border_1()
            .border_color(if active { p.accent } else { p.border })
            .child(
                h_row()
                    .justify_between()
                    .gap(px(16.))
                    .child(
                        // Shrinks and wraps before the buttons beside it
                        // would be pushed out of the card.
                        v_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(4.))
                            .child(
                                h_row()
                                    .flex_wrap()
                                    .gap(px(10.))
                                    .gap_y(px(4.))
                                    .child(
                                        div()
                                            .text_size(px(15.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(m.name),
                                    )
                                    .when(m.id == slot.recommended(), |el| el.child(badge("Recommended", p.accent_text)))
                                    .child(status),
                            )
                            .child(div().text_size(px(13.)).text_color(p.text_muted).child(m.tagline)),
                    )
                    .child(div().flex_none().child(actions)),
            )
            .when_some(progress, |el, (done, total)| {
                el.child(progress_bar(if total > 0 { done as f32 / total as f32 } else { 0.0 }, p))
            })
            .child(
                h_row()
                    .flex_wrap()
                    .gap(px(24.))
                    .gap_y(px(10.))
                    .child(spec("Accuracy", meter(m.accuracy, p).into_any_element(), p))
                    .child(spec("Speed", meter(m.speed, p).into_any_element(), p))
                    .child(spec(
                        "Languages",
                        div().text_size(px(12.5)).text_color(p.text).child(m.languages).into_any_element(),
                        p,
                    ))
                    .child(spec(
                        "Size",
                        div().text_size(px(12.5)).text_color(p.text).child(fmt_bytes(m.size_bytes())).into_any_element(),
                        p,
                    ))
                    .when(slot == Slot::Polish, |el| {
                        el.child(spec(
                            "By",
                            div().text_size(px(12.5)).text_color(p.text).child(models::polish_maker(m)).into_any_element(),
                            p,
                        ))
                        .child(spec(
                            "License",
                            div().text_size(px(12.5)).text_color(p.text).child("Apache-2.0").into_any_element(),
                            p,
                        ))
                    }),
            )
            .when_some(error, |el, e| {
                el.child(div().text_size(px(12.5)).text_color(p.danger).child(format!("Download failed: {e}")))
            })
            .when_some(failure, |el, e| el.child(div().text_size(px(12.5)).text_color(p.danger).child(e)))
    }
}

/// Which model a card is for: the speech model, or the polish model.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Slot {
    Speech,
    Polish,
}

impl Slot {
    /// The model this slot uses now.
    fn current(self, c: &Config) -> &str {
        match self {
            Slot::Speech => &c.model.id,
            Slot::Polish => models::polish_model(&c.text.polish_model).id,
        }
    }

    /// Makes `id` the model this slot uses.
    fn pick(self, c: &mut Config, id: &str) {
        match self {
            Slot::Speech => c.model.id = id.to_string(),
            Slot::Polish => c.text.polish_model = id.to_string(),
        }
    }

    fn recommended(self) -> &'static str {
        match self {
            Slot::Speech => models::DEFAULT_MODEL,
            Slot::Polish => models::DEFAULT_POLISH,
        }
    }
}

fn spec(label: &str, value: AnyElement, p: Palette) -> impl IntoElement {
    v_col()
        .gap(px(6.))
        .child(div().text_size(px(11.5)).text_color(p.text_faint).child(label.to_string()))
        .child(div().h(px(16.)).flex().items_center().child(value))
}
