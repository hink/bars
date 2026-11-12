//! The settings window: a quiet sidebar and one page at a time.

mod dictionary;
mod general;
mod history;
mod home;
mod model;
mod setup;

use gpui_kit::assets::IconName as I;
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::component::{TITLE_BAR_HEIGHT, TitleBar};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use bars_core::controller::{Command, NoticeKind, Phase};
use bars_core::engine::EngineStatus;
use bars_core::models;

use crate::state::{AppState, Offer, StateEvent, UpdateStatus};
use crate::theme::{FONT, PaletteExt};
use crate::widgets::{ButtonKind, button, fmt_bytes, h_row, icon, icon_button, progress_bar, v_col};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Page {
    Home,
    General,
    Model,
    Dictionary,
    History,
    Setup,
}

impl Page {
    /// Dev helper: `BARS_PAGE=model bars` opens straight to a page.
    pub fn from_env() -> Option<Page> {
        let v = std::env::var("BARS_PAGE").ok()?;
        Some(match v.as_str() {
            "home" => Page::Home,
            "general" => Page::General,
            "model" => Page::Model,
            "dictionary" => Page::Dictionary,
            "history" => Page::History,
            "setup" => Page::Setup,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Page::Home => "Home",
            Page::General => "Shortcut & input",
            Page::Model => "Models",
            Page::Dictionary => "Dictionary",
            Page::History => "History",
            Page::Setup => "Setup",
        }
    }

    fn icon(self) -> I {
        match self {
            Page::Home => I::House,
            Page::General => I::Keyboard,
            Page::Model => I::AudioLines,
            Page::Dictionary => I::BookA,
            Page::History => I::ListClock,
            Page::Setup => I::ShieldCheck,
        }
    }
}

pub struct SettingsView {
    pub state: Entity<AppState>,
    pub page: Page,
    // Dictionary page inputs.
    pub dict_from: Entity<InputState>,
    pub dict_to: Entity<InputState>,
    pub snip_trigger: Entity<InputState>,
    pub snip_text: Entity<InputState>,
    // History page search.
    pub search: Entity<InputState>,
    // Setup page practice field.
    pub practice: Entity<InputState>,
    pub copied: Option<u64>,
    /// A History entry being corrected, and its editor.
    pub editing: Option<(u64, Entity<TextareaState>)>,
    /// The sidebar card being shown, when there are several (see
    /// `render_sidebar`); the first otherwise.
    notice: Option<Notice>,
    _subs: Vec<Subscription>,
}

/// How long a notice from the controller stays up in this window.
const NOTICE_FOR: std::time::Duration = std::time::Duration::from_secs(8);

/// A card above Setup in the sidebar. Only one shows at a time, the first
/// waiting in this order; arrows on it page through the others, and
/// answering one brings up the next.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Notice {
    Update,
    Offer(Offer),
}

impl SettingsView {
    pub fn new(state: Entity<AppState>, page: Page, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mk = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let search = mk("Search history", window, cx);
        window.set_window_title("Bars");
        let subs = vec![
            cx.observe(&state, |_, _, cx| cx.notify()),
            cx.observe(&search, |_, _, cx| cx.notify()),
            // Follow the desktop's light/dark preference live.
            cx.observe_window_appearance(window, |this, window, cx| {
                this.sync_theme(window, cx);
            }),
            // A notice shows here for a while (see `render_notice`): draw
            // again once its time is up.
            cx.subscribe(&state, |_, _, ev: &StateEvent, cx| {
                if let StateEvent::Notice = ev {
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(NOTICE_FOR + std::time::Duration::from_millis(50)).await;
                        let _ = this.update(cx, |_, cx| cx.notify());
                    })
                    .detach();
                }
            }),
        ];
        // A shortcut being recorded when the window closes would otherwise
        // be taken from the next keys pressed anywhere.
        cx.on_release(|this, cx| this.end_capture(cx)).detach();
        SettingsView {
            dict_from: mk("What you say, e.g. “example dot x y z”", window, cx),
            dict_to: mk("What to write, e.g. example.xyz", window, cx),
            snip_trigger: mk("Spoken cue, e.g. “my address”", window, cx),
            snip_text: mk("Text to insert", window, cx),
            practice: mk("Click here, then hold your shortcut and speak…", window, cx),
            search,
            state,
            page,
            copied: None,
            editing: None,
            notice: None,
            _subs: subs,
        }
    }

    pub fn sync_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        crate::theme::follow_system(window, cx);
    }

    /// Stops recording a shortcut, if one is being recorded.
    fn end_capture(&mut self, cx: &mut App) {
        self.state.update(cx, |s, cx| {
            if s.capturing {
                s.stop_capture(cx);
            }
        });
    }

    /// What the controller last had to say, for a while after it said it:
    /// the pill doesn't show while this window has focus, and can't fit a
    /// long message anyway.
    fn render_notice(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let p = cx.palette();
        let notice = self.state.read(cx).notice.clone().filter(|n| n.at.elapsed() < NOTICE_FOR)?;
        let tint = match notice.kind {
            NoticeKind::Error => p.danger,
            NoticeKind::Warning => p.warning,
            NoticeKind::Info => p.accent,
        };
        Some(
            div().absolute().bottom(px(20.)).left_0().w_full().px(px(40.)).flex().justify_center().child(
                h_row()
                    .max_w(px(560.))
                    .gap(px(10.))
                    .items_start()
                    .px(px(14.))
                    .py(px(10.))
                    .rounded(px(10.))
                    .bg(p.surface_2)
                    .border_1()
                    .border_color(p.border_strong)
                    .shadow_md()
                    .child(div().mt(px(6.)).size(px(7.)).flex_none().rounded_full().bg(tint))
                    .child(div().min_w_0().text_size(px(13.)).text_color(p.text).child(notice.text)),
            ),
        )
    }

    pub fn go(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.page == Page::General && page != Page::General {
            self.end_capture(cx);
        }
        self.page = page;
        if page == Page::General {
            self.state.update(cx, |s, cx| s.refresh_devices(cx));
        }
        cx.notify();
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let st = self.state.read(cx);
        let needs_setup = !st.setup_complete();
        let status = status_line(st);
        let update = st.update.offered().map(|r| (r.clone(), st.update.clone()));
        let mut notices = Vec::new();
        if update.is_some() {
            notices.push(Notice::Update);
        }
        for offer in [Offer::Speech, Offer::Polish] {
            if st.offer(offer).is_some() {
                notices.push(Notice::Offer(offer));
            }
        }
        // The one picked with the arrows while it's still there, else the
        // first.
        let shown = self.notice.and_then(|n| notices.iter().position(|m| *m == n)).unwrap_or(0);
        let pages = [Page::Home, Page::General, Page::Model, Page::Dictionary, Page::History];
        let item = |page: Page, attention: bool, cx: &mut Context<Self>| {
            let active = self.page == page;
            div()
                .id(SharedString::from(page.label()))
                .h(px(34.))
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(10.))
                .rounded(px(8.))
                .cursor_pointer()
                .text_size(px(13.5))
                .font_weight(FontWeight::MEDIUM)
                .when(active, |el| el.bg(p.hover).text_color(p.text))
                .when(!active, |el| el.text_color(p.text_muted).hover(|s| s.bg(p.hover).text_color(p.text)))
                .on_click(cx.listener(move |this, _, _, cx| this.go(page, cx)))
                .child(icon(page.icon(), if active { p.accent_text } else { p.text_faint }, 16.))
                .child(div().flex_1().child(page.label()))
                .when(attention, |el| el.child(div().size(px(7.)).rounded_full().bg(p.warning)))
        };

        v_col()
            .w(px(232.))
            .h_full()
            .flex_none()
            .bg(p.sidebar)
            .border_r_1()
            .border_color(p.border)
            .px(px(12.))
            // Under the title bar (the window's controls), which overlays it.
            .pt(TITLE_BAR_HEIGHT)
            .pb(px(14.))
            .child(
                h_row()
                    .px(px(2.))
                    .pt(px(14.))
                    .pb(px(24.))
                    .child(crate::brand::wordmark(204., p.dark)),
            )
            .child(
                v_col()
                    .gap(px(2.))
                    .children(pages.into_iter().map(|page| item(page, false, cx).into_any_element())),
            )
            .child(div().flex_1())
            .when_some(notices.get(shown).copied(), |el, notice| {
                let card = match (notice, update) {
                    (Notice::Update, Some((release, status))) => self.update_banner(release, status, cx).into_any_element(),
                    (Notice::Offer(offer), _) => self.model_banner(offer, cx).into_any_element(),
                    (Notice::Update, None) => return el,
                };
                el.child(card).children(self.pager(&notices, shown, cx))
            })
            .child(item(Page::Setup, needs_setup, cx))
            .child(
                h_row()
                    .mt(px(12.))
                    .px(px(10.))
                    .gap(px(8.))
                    .child(div().size(px(7.)).rounded_full().bg(match status.1 {
                        Status::Good => p.success,
                        Status::Busy => p.accent,
                        Status::Warn => p.warning,
                        Status::Live => p.recording,
                    }))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(p.text_muted).child(status.0))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(p.text_faint)
                            .child(concat!("v", env!("CARGO_PKG_VERSION"))),
                    ),
            )
    }
}

impl SettingsView {
    /// Above Setup while a newer Bars is on offer: what it is, and a button
    /// to update (or download, where Bars can't install it itself), or to
    /// restart into it once it's installed.
    fn update_banner(
        &self,
        release: bars_core::update::Release,
        status: UpdateStatus,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = cx.palette();
        let version = &release.version;
        let (detail, action) = match status {
            UpdateStatus::Installing(_) => (format!("Installing ver. {version}…"), None),
            UpdateStatus::InstallFailed(..) => ("Couldn't install".to_string(), Some("Try again")),
            UpdateStatus::Installed(_) => (format!("ver. {version}"), Some("Restart")),
            _ => (format!("ver. {version}"), Some(crate::shell::update_action(&release))),
        };
        let title = if matches!(status, UpdateStatus::Installed(_)) { "Update installed" } else { "New version available" };
        let state = self.state.clone();
        v_col()
            .mb(px(8.))
            .px(px(12.))
            .py(px(10.))
            .gap(px(8.))
            .rounded(px(10.))
            .bg(p.accent_soft)
            .child(
                h_row()
                    .gap(px(8.))
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(p.text)
                    .child(icon(I::Sparkles, p.accent_text, 15.))
                    .child(title),
            )
            .child(
                h_row()
                    .gap(px(8.))
                    .child(div().flex_1().text_size(px(12.)).text_color(p.text_muted).child(detail))
                    .when_some(action, |el, label| {
                        el.child(button("sidebar-update", label, ButtonKind::Primary, p).h(px(26.)).on_click(
                            move |_, _, cx| state.update(cx, |s, cx| s.install_update(cx)),
                        ))
                    }),
            )
    }
}

impl SettingsView {
    /// Under the sidebar card, when there's more than one: arrows, and
    /// which one this is ("1 of 2").
    fn pager(&self, notices: &[Notice], shown: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let n = notices.len();
        if n < 2 {
            return None;
        }
        let p = cx.palette();
        let (prev, next) = (notices[(shown + n - 1) % n], notices[(shown + 1) % n]);
        Some(
            h_row()
                .justify_center()
                .gap(px(4.))
                .mt(px(-2.))
                .mb(px(8.))
                .child(
                    icon_button("notice-prev", I::ChevronLeft, p.text_muted, p)
                        .size(px(22.))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.notice = Some(prev);
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(p.text_muted)
                        .child(format!("{} of {n}", shown + 1)),
                )
                .child(
                    icon_button("notice-next", I::ChevronRight, p.text_muted, p)
                        .size(px(22.))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.notice = Some(next);
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    /// Above Setup, once, for someone using another model of a kind when
    /// Bars recommends a new one: what it is, and a button to switch to it
    /// (after its download).
    fn model_banner(&self, offer: Offer, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let st = self.state.read(cx);
        let m = offer.model();
        let switching = st.switching.contains(&offer);
        let progress = st.downloads.get(m.id).copied();
        let failed = switching && st.download_errors.contains_key(m.id);
        let better = match offer {
            Offer::Speech => format!("Fewer mistakes than {}", st.active_model().map_or("your model", |c| c.name)),
            Offer::Polish => format!(
                "Better polish than {}: it catches more spoken corrections",
                models::polish_model(&st.config.text.polish_model).name
            ),
        };
        let detail = match progress {
            Some((done, total)) => format!("Downloading… {}%", (done * 100).checked_div(total).unwrap_or(0)),
            None if failed => "The download failed.".to_string(),
            None if switching => "Waiting to download…".to_string(),
            None if st.installed.contains(m.id) => format!("{better}, and it's already downloaded."),
            None => format!("{better}. Switching downloads {}.", fmt_bytes(m.size_bytes())),
        };
        let state = self.state.clone();
        let answer = move |switch: bool| {
            let state = state.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| state.update(cx, |s, cx| s.answer_offer(offer, switch, cx))
        };
        let buttons = match (switching, failed) {
            (true, false) => {
                let state = self.state.clone();
                let id = m.id.to_string();
                h_row().child(button("model-offer-cancel", "Cancel", ButtonKind::Ghost, p).h(px(26.)).on_click(
                    move |_, _, cx| state.read(cx).send(Command::CancelDownload(id.clone())),
                ))
            }
            _ => h_row()
                .gap(px(6.))
                .child(button("model-offer-no", "Not now", ButtonKind::Ghost, p).h(px(26.)).on_click(answer(false)))
                .child(
                    button("model-offer-yes", if failed { "Try again" } else { "Switch" }, ButtonKind::Primary, p)
                        .h(px(26.))
                        .on_click(answer(true)),
                ),
        };
        v_col()
            .mb(px(8.))
            .px(px(12.))
            .py(px(10.))
            .gap(px(8.))
            .rounded(px(10.))
            .bg(p.accent_soft)
            .child(
                h_row()
                    .gap(px(8.))
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(p.text)
                    .child(icon(I::Sparkles, p.accent_text, 15.))
                    .child(format!("New: {}", m.name)),
            )
            .child(div().text_size(px(12.)).line_height(px(17.)).text_color(p.text_muted).child(detail))
            .when_some(progress, |el, (done, total)| {
                el.child(progress_bar(if total > 0 { done as f32 / total as f32 } else { 0.0 }, p))
            })
            .child(buttons.justify_end())
    }
}

#[derive(Clone, Copy)]
enum Status {
    Good,
    Busy,
    Warn,
    Live,
}

fn status_line(st: &AppState) -> (String, Status) {
    match st.phase {
        Phase::Recording { .. } => return ("Listening…".into(), Status::Live),
        Phase::Transcribing => return ("Transcribing…".into(), Status::Busy),
        Phase::Idle => {}
    }
    if let Some((id, (done, total))) = st.downloads.iter().next() {
        let name = bars_core::models::find(id).map_or("model", |m| m.name);
        let pct = if *total > 0 { done * 100 / total } else { 0 };
        return (format!("Downloading {name} · {pct}%"), Status::Busy);
    }
    match &st.engine {
        EngineStatus::Loading(_) => ("Loading model…".into(), Status::Busy),
        EngineStatus::Ready { .. } | EngineStatus::Unloaded(_) if st.setup_complete() => {
            ("Ready".into(), Status::Good)
        }
        EngineStatus::Ready { .. } | EngineStatus::Unloaded(_) => ("Finish setup".into(), Status::Warn),
        EngineStatus::Missing(_) => ("Model not downloaded".into(), Status::Warn),
        EngineStatus::Failed { .. } => ("Model failed to load".into(), Status::Warn),
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let page: AnyElement = match self.page {
            Page::Home => self.render_home(window, cx).into_any_element(),
            Page::General => self.render_general(window, cx).into_any_element(),
            Page::Model => self.render_model(window, cx).into_any_element(),
            Page::Dictionary => self.render_dictionary(window, cx).into_any_element(),
            Page::History => self.render_history(window, cx).into_any_element(),
            Page::Setup => self.render_setup(window, cx).into_any_element(),
        };
        // Full bleed: the sidebar and the page reach the top of the window,
        // and the title bar lies over them, transparent, for dragging and
        // the window controls.
        div()
            .size_full()
            .relative()
            .bg(p.bg)
            .font_family(FONT)
            .text_color(p.text)
            .child(
                h_row()
                    .size_full()
                    .items_start()
                    .child(self.render_sidebar(cx))
                    .child(
                        // The page scrolls below the title bar's strip.
                        div().flex_1().min_w_0().h_full().relative().pt(TITLE_BAR_HEIGHT).children(self.render_notice(cx)).child(
                            div()
                                .id("page-scroll")
                                .size_full()
                                .overflow_y_scroll()
                                .child(
                                    div()
                                        .w_full()
                                        .max_w(px(780.))
                                        .mx_auto()
                                        .px(px(40.))
                                        .pt(px(28.))
                                        .pb(px(48.))
                                        .child(page),
                                ),
                        ),
                    ),
            )
            .child(
                div().absolute().top_0().left_0().w_full().child(
                    TitleBar::new()
                        .w_full()
                        .bg(transparent_black())
                        .border_color(transparent_black()),
                ),
            )
    }
}

pub fn open(state: Entity<AppState>, page: Page, cx: &mut App) -> anyhow::Result<WindowHandle<gpui_kit::base::Root>> {
    let bounds = Bounds::centered(None, size(px(980.), px(680.)), cx);
    let opts = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(760.), px(520.))),
        window_decorations: Some(WindowDecorations::Client),
        app_id: Some(bars_core::paths::APP_ID.into()),
        icon: crate::brand::window_icon(),
        focus: true,
        show: true,
        ..TitleBar::window_options()
    };
    let (handle, _) = gpui_kit::open_window(opts, cx, |window, cx| {
        cx.new(|cx| {
            let mut view = SettingsView::new(state, page, window, cx);
            view.sync_theme(window, cx);
            view
        })
    })?;
    handle
        .downcast::<gpui_kit::base::Root>()
        .ok_or_else(|| anyhow::anyhow!("unexpected root type"))
}
