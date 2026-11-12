//! The floating recording pill. On Wayland compositors with layer-shell
//! (KDE, wlroots, Hyprland, COSMIC...) it is an overlay surface that never
//! takes keyboard focus, so text lands in the app you were using.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use bars_core::controller::{Command, NoticeKind, Phase};

use crate::state::{AppState, LevelMeter, StateEvent};
use crate::theme::FONT;
use crate::widgets::{h_row, icon};

/// Surface size; the pill is drawn centred at its bottom and the input
/// region is restricted to the pill so the rest is click-through.
#[cfg(target_os = "linux")]
const SURFACE: Size<Pixels> = Size {
    width: px(440.),
    height: px(96.),
};
/// Windows/macOS: clicks outside the pill pass through too (window region
/// / mouse passthrough), but there's no compositor blur to confine, so hug
/// the pill.
#[cfg(not(target_os = "linux"))]
const SURFACE: Size<Pixels> = Size {
    width: px(440.),
    height: px(56.),
};
/// Gap between a popup overlay and the taskbar/Dock.
const SCREEN_MARGIN: f32 = 12.;
const PILL_H: f32 = 36.;
const BARS: usize = 13;
/// How long a notice stays on the pill.
const MESSAGE_FOR: Duration = Duration::from_millis(2600);
/// "Select a text box first" stays longer: it offers the text to copy.
pub const PASTE_FAILED_FOR: Duration = Duration::from_millis(6000);
/// Room for the message's button (Copy, Undo).
const ACTION_BUTTON_W: f32 = 64.;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Visual {
    Hidden,
    Listening,
    HandsFree,
    Thinking,
    Message,
}

pub struct Overlay {
    state: Entity<AppState>,
    levels: VecDeque<f32>,
    last_sample: Instant,
    visual: Visual,
    changed_at: Instant,
    from_width: f32,
    message: Option<(SharedString, NoticeKind, Instant)>,
    /// How long the current message stays.
    message_for: Duration,
    /// What the message's button does, and its label.
    action: Option<(SharedString, Command)>,
    input_region: Option<Bounds<Pixels>>,
    /// Moved to the active screen for the current appearance.
    #[cfg(not(target_os = "linux"))]
    placed: bool,
    #[cfg(not(target_os = "linux"))]
    native: crate::overlay_native::NativeOverlay,
    _subs: Vec<Subscription>,
}

/// The most a notice shows of itself in the pill.
const NOTICE_CHARS: usize = 64;

/// A notice as the pill shows it: whole when short, else its beginning
/// (what went wrong comes first; the settings window and the log have the
/// rest).
fn glanceable(text: &SharedString) -> SharedString {
    if text.chars().count() <= NOTICE_CHARS {
        return text.clone();
    }
    let start: String = text.chars().take(NOTICE_CHARS - 1).collect();
    format!("{}…", start.trim_end()).into()
}

impl Overlay {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let level = state.read(cx).level.clone();
        let subs = vec![
            cx.observe(&level, |this, level, cx| this.on_level(level, cx)),
            cx.subscribe(&state, |this, state, ev: &StateEvent, cx| match ev {
                StateEvent::PhaseChanged(_) => {
                    this.sync(state.read(cx).phase);
                    cx.notify();
                }
                StateEvent::Notice => {
                    if let Some(n) = state.read(cx).notice.clone() {
                        this.show_message(glanceable(&n.text), n.kind, MESSAGE_FOR, None);
                        cx.notify();
                    }
                }
                StateEvent::PasteFailed => {
                    if let Some(text) = state.read(cx).paste_failed.clone() {
                        let copy = Some(("Copy".into(), Command::CopyText(text)));
                        this.show_message("Select a text box first".into(), NoticeKind::Warning, PASTE_FAILED_FOR, copy);
                        cx.notify();
                    }
                }
                StateEvent::Learned => {
                    if let Some((from, to)) = state.read(cx).learned.clone() {
                        let label = format!("Learned “{to}”");
                        let undo = Some(("Undo".into(), Command::UndoLearned { from, to }));
                        this.show_message(label.into(), NoticeKind::Info, PASTE_FAILED_FOR, undo);
                        cx.notify();
                    }
                }
                // Updates are announced by notification, not the pill.
                StateEvent::ThemeChanged
                | StateEvent::UpdateFound
                | StateEvent::UpdateResult
                | StateEvent::UpdateInstalled
                | StateEvent::RestartToUpdate
                | StateEvent::Restart => {}
            }),
        ];
        let mut this = Overlay {
            state: state.clone(),
            levels: VecDeque::from(vec![0.0; BARS]),
            last_sample: Instant::now(),
            visual: Visual::Hidden,
            changed_at: Instant::now(),
            from_width: 0.0,
            message: None,
            message_for: MESSAGE_FOR,
            action: None,
            input_region: None,
            #[cfg(not(target_os = "linux"))]
            placed: false,
            #[cfg(not(target_os = "linux"))]
            native: Default::default(),
            _subs: subs,
        };
        // A notice may be what caused this overlay to open.
        if let Some(n) = state.read(cx).notice.clone()
            && n.at.elapsed() < Duration::from_millis(500)
        {
            this.message = Some((glanceable(&n.text), n.kind, n.at));
        }
        this.sync(state.read(cx).phase);
        this
    }

    fn show_message(&mut self, text: SharedString, kind: NoticeKind, lasts: Duration, action: Option<(SharedString, Command)>) {
        self.message = Some((text, kind, Instant::now()));
        self.message_for = lasts;
        self.action = action;
        self.set_visual(Visual::Message);
    }

    fn set_visual(&mut self, v: Visual) {
        if self.visual != v {
            self.from_width = self.current_width();
            self.visual = v;
            self.changed_at = Instant::now();
        }
    }

    fn sync(&mut self, phase: Phase) {
        let v = match phase {
            Phase::Idle => {
                if self.message.as_ref().is_some_and(|m| m.2.elapsed() < self.message_for) {
                    Visual::Message
                } else {
                    Visual::Hidden
                }
            }
            Phase::Recording { locked: false } => Visual::Listening,
            Phase::Recording { locked: true } => Visual::HandsFree,
            Phase::Transcribing => Visual::Thinking,
        };
        if v != Visual::Message {
            self.message = None;
            self.action = None;
        }
        if matches!(v, Visual::Listening) && self.visual == Visual::Hidden {
            self.levels.iter_mut().for_each(|l| *l = 0.0);
        }
        self.set_visual(v);
    }

    fn on_level(&mut self, level: Entity<LevelMeter>, cx: &mut Context<Self>) {
        let l = level.read(cx).level;
        // Scroll the waveform at ~30 Hz regardless of callback rate.
        if self.last_sample.elapsed() >= Duration::from_millis(33) {
            self.last_sample = Instant::now();
            self.levels.pop_front();
            self.levels.push_back(l);
        } else if let Some(last) = self.levels.back_mut() {
            *last = last.max(l);
        }
        if matches!(self.visual, Visual::Listening | Visual::HandsFree) {
            cx.notify();
        }
    }

    fn target_width(&self) -> f32 {
        match self.visual {
            Visual::Hidden => 44.,
            Visual::Listening => 132.,
            Visual::HandsFree => 196.,
            Visual::Thinking => 92.,
            Visual::Message => {
                let len = self.message.as_ref().map_or(0, |m| m.0.len());
                let button = if self.action.is_some() { ACTION_BUTTON_W } else { 0. };
                (len as f32 * 7.0 + 56. + button).clamp(160., 420.)
            }
        }
    }

    fn progress(&self) -> f32 {
        let t = self.changed_at.elapsed().as_secs_f32() / 0.22;
        // easeOutCubic
        1.0 - (1.0 - t.min(1.0)).powi(3)
    }

    fn current_width(&self) -> f32 {
        let to = self.target_width();
        if self.from_width == 0.0 {
            return to;
        }
        self.from_width + (to - self.from_width) * self.progress()
    }

    fn render_bars(&self, color: Option<Hsla>, t: f32) -> impl IntoElement {
        let thinking = self.visual == Visual::Thinking;
        h_row().gap(px(3.)).h(px(22.)).children((0..BARS).map(move |i| {
            let h = if thinking {
                // A soft travelling wave while the model works.
                let phase = t * 7.0 - i as f32 * 0.55;
                4.0 + (phase.sin() * 0.5 + 0.5).powf(2.0) * 9.0
            } else {
                let l = self.levels[i];
                // Center bars a little taller: reads as a voice, not a meter.
                let centre = 1.0 - ((i as f32 - (BARS as f32 - 1.0) / 2.0).abs() / BARS as f32);
                3.0 + (l * (0.55 + 0.45 * centre)).min(1.0) * 19.0
            };
            let alpha = if thinking {
                0.35 + 0.5 * ((t * 7.0 - i as f32 * 0.55).sin() * 0.5 + 0.5)
            } else {
                1.0
            };
            div().w(px(3.)).h(px(h)).rounded_full().bg(bar_fill(color, alpha))
        }))
    }
}

impl Render for Overlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let animating = self.changed_at.elapsed() < Duration::from_millis(260);
        // Expire messages.
        if self.visual == Visual::Message
            && self.message.as_ref().is_none_or(|m| m.2.elapsed() > self.message_for)
        {
            self.message = None;
            let phase = self.state.read(cx).phase;
            self.sync(phase);
        }
        let t = self.changed_at.elapsed().as_secs_f32();
        if animating || matches!(self.visual, Visual::Thinking | Visual::Message | Visual::HandsFree) {
            window.request_animation_frame();
        }

        let visible = self.visual != Visual::Hidden;
        let width = self.current_width();
        let appear = if visible { self.progress() } else { 1.0 - self.progress() };

        // Restrict input to the pill.
        let pill_bounds = Bounds {
            origin: point(
                (SURFACE.width - px(width)) / 2.0,
                SURFACE.height - px(PILL_H) - px(10.),
            ),
            size: size(px(width), px(PILL_H)),
        };
        let region = visible.then_some(pill_bounds);
        if region != self.input_region {
            self.input_region = region;
            match region {
                // Also shapes the compositor blur behind the pill.
                Some(b) => window.set_input_region(Some(&capsule_rects(b))),
                None => window.set_input_region(Some(&[])),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            if visible && !self.placed {
                self.placed = true;
                self.native.place_on_active_screen(window, SURFACE, SCREEN_MARGIN, cx);
            } else if !visible && !animating {
                self.placed = false;
            }
            // The window region also clips drawing (Windows), so keep the
            // pill's area while it fades out.
            let shown = (visible || animating).then_some(pill_bounds);
            self.native.update(window, shown, appear, cx);
        }

        let white: Hsla = rgb(0xffffff).into();
        let red: Hsla = rgb(0xff5a4e).into();
        let state = self.state.clone();

        let content: AnyElement = match self.visual {
            Visual::Hidden => div().into_any_element(),
            Visual::Listening => self.render_bars(None, t).into_any_element(),
            Visual::Thinking => self.render_bars(None, t).into_any_element(),
            Visual::HandsFree => {
                let pulse = 0.55 + 0.45 * ((t * 3.2).sin() * 0.5 + 0.5);
                let mut dot = red;
                dot.a = pulse;
                let s1 = state.clone();
                let s2 = state.clone();
                h_row()
                    .w_full()
                    .justify_between()
                    .px(px(6.))
                    .child(
                        div()
                            .id("cancel")
                            .size(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .cursor_pointer()
                            .hover(|s| s.bg(hsla(0., 0., 1., 0.12)))
                            .on_click(move |_, _, cx| s1.read(cx).send(Command::Cancel))
                            .child(icon(gpui_kit::assets::IconName::X, hsla(0., 0., 1., 0.7), 13.)),
                    )
                    .child(
                        h_row()
                            .gap(px(8.))
                            .child(div().size(px(7.)).rounded_full().bg(dot))
                            .child(self.render_bars(None, t)),
                    )
                    .child(
                        div()
                            .id("stop")
                            .size(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(red)
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.85))
                            .on_click(move |_, _, cx| s2.read(cx).send(Command::Finish))
                            .child(div().size(px(8.)).rounded(px(2.)).bg(white)),
                    )
                    .into_any_element()
            }
            Visual::Message => {
                let (text, kind) = self
                    .message
                    .as_ref()
                    .map(|m| (m.0.clone(), m.1.clone()))
                    .unwrap_or_default_kind();
                let tint: Hsla = match kind {
                    NoticeKind::Error => rgb(0xff8a8a).into(),
                    NoticeKind::Warning => rgb(0xffcf7a).into(),
                    NoticeKind::Info => hsla(0., 0., 1., 0.92),
                };
                let label = div()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tint)
                    .whitespace_nowrap()
                    .child(text);
                match self.action.clone() {
                    None => div().px(px(16.)).child(label).into_any_element(),
                    Some((button_label, command)) => {
                        let s = state.clone();
                        h_row()
                            .pl(px(16.))
                            .pr(px(6.))
                            .gap(px(10.))
                            .child(label)
                            .child(
                                div()
                                    .id("message-action")
                                    .h(px(24.))
                                    .px(px(12.))
                                    .flex()
                                    .items_center()
                                    .rounded_full()
                                    .bg(hsla(0., 0., 1., 0.14))
                                    .cursor_pointer()
                                    .hover(|el| el.bg(hsla(0., 0., 1., 0.22)))
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(white)
                                    .child(button_label)
                                    .on_click(move |_, _, cx| s.read(cx).send(command.clone())),
                            )
                            .into_any_element()
                    }
                }
            }
        };

        let pill = div()
            .id("pill")
            .w(px(width))
            .h(px(PILL_H))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            // Frosted glass: translucent fill over the compositor's blur,
            // a faint top highlight, and a hairline edge.
            .bg(linear_gradient(
                180.,
                linear_color_stop(hsla(240. / 360., 0.10, 0.16, 0.62), 0.),
                linear_color_stop(hsla(240. / 360., 0.10, 0.07, 0.58), 1.),
            ))
            .border_1()
            .border_color(hsla(0., 0., 1., 0.14))
            .shadow_md()
            .opacity(appear)
            .font_family(FONT)
            .when(self.visual == Visual::Listening || self.visual == Visual::Thinking, |el| {
                // Clicking a push-to-talk pill locks it into hands-free mode.
                let s = state.clone();
                el.cursor_pointer().on_click(move |_, _, cx| {
                    if s.read(cx).phase == (Phase::Recording { locked: false }) {
                        s.read(cx).send(Command::Toggle);
                    }
                })
            })
            .child(content);

        div()
            .size_full()
            .flex()
            .flex_col()
            .justify_end()
            .items_center()
            .pb(px(10.))
            .when(visible || animating, |el| el.child(pill))
    }
}

/// `None` color = the brand gradient (magenta to blue, like the icon's bars).
fn bar_fill(color: Option<Hsla>, alpha: f32) -> Background {
    match color {
        Some(mut c) => {
            c.a *= alpha;
            c.into()
        }
        None => {
            let mut top: Hsla = rgb(crate::brand::GRADIENT_TOP).into();
            let mut bottom: Hsla = rgb(crate::brand::GRADIENT_BOTTOM).into();
            top.a = alpha;
            bottom.a = alpha;
            linear_gradient(180., linear_color_stop(top, 0.), linear_color_stop(bottom, 1.))
        }
    }
}

/// Approximates a capsule with horizontal strips, so regions (input, blur)
/// follow the rounded ends instead of the bounding box.
fn capsule_rects(b: Bounds<Pixels>) -> Vec<Bounds<Pixels>> {
    let h = f32::from(b.size.height);
    let w = f32::from(b.size.width);
    let r = h / 2.0;
    let strip: f32 = 2.0;
    let mut rects = Vec::new();
    let mut y = 0.0;
    while y < h {
        let sh = strip.min(h - y);
        let dy = ((y + sh / 2.0) - r).abs();
        let inset = (r - (r * r - dy * dy).max(0.0).sqrt()).ceil();
        rects.push(Bounds {
            origin: point(b.origin.x + px(inset), b.origin.y + px(y)),
            size: size(px((w - 2.0 * inset).max(0.0)), px(sh)),
        });
        y += sh;
    }
    rects
}

trait MessageExt {
    fn unwrap_or_default_kind(self) -> (SharedString, NoticeKind);
}

impl MessageExt for Option<(SharedString, NoticeKind)> {
    fn unwrap_or_default_kind(self) -> (SharedString, NoticeKind) {
        self.unwrap_or_else(|| (SharedString::default(), NoticeKind::Info))
    }
}

/// Creates the overlay window. Prefers a layer-shell surface; falls back to
/// an unfocused popup where layer-shell isn't available (e.g. GNOME).
pub fn open(state: Entity<AppState>, cx: &mut App) -> Option<WindowHandle<Overlay>> {
    #[cfg(target_os = "linux")]
    {
        let bounds = Bounds {
            origin: point(px(0.), px(0.)),
            size: SURFACE,
        };
        let opts = WindowOptions {
            titlebar: None,
            kind: WindowKind::LayerShell(layer_shell::LayerShellOptions {
                namespace: "bars-overlay".into(),
                layer: layer_shell::Layer::Overlay,
                anchor: layer_shell::Anchor::BOTTOM,
                margin: Some((px(0.), px(0.), px(28.), px(0.))),
                keyboard_interactivity: layer_shell::KeyboardInteractivity::None,
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // Blurred = transparent + KDE blur (where supported), confined
            // to the pill by the input region.
            window_background: WindowBackgroundAppearance::Blurred,
            focus: false,
            show: true,
            is_movable: false,
            is_resizable: false,
            app_id: Some(bars_core::paths::APP_ID.into()),
            ..Default::default()
        };
        let s = state.clone();
        match cx.open_window(opts, |_, cx| cx.new(|cx| Overlay::new(s, cx))) {
            Ok(h) => return Some(h),
            Err(e) => log::info!("layer-shell overlay unavailable ({e}); using a popup"),
        }
    }
    // Bottom centre of the primary display's work area (above the taskbar
    // or Dock); on Windows/macOS the overlay then follows the pointer's
    // screen each time it appears.
    let placed = cx
        .primary_display()
        .map(|d| {
            let area = d.visible_bounds();
            Bounds {
                origin: point(
                    area.origin.x + (area.size.width - SURFACE.width) / 2.0,
                    area.origin.y + area.size.height - SURFACE.height - px(SCREEN_MARGIN),
                ),
                size: SURFACE,
            }
        })
        .unwrap_or_else(|| Bounds::centered(None, SURFACE, cx));
    let opts = WindowOptions {
        titlebar: None,
        kind: WindowKind::PopUp,
        window_bounds: Some(WindowBounds::Windowed(placed)),
        // macOS gets its blur from a separate backdrop (see overlay_native).
        window_background: WindowBackgroundAppearance::Transparent,
        focus: false,
        show: true,
        is_movable: false,
        is_resizable: false,
        app_id: Some(bars_core::paths::APP_ID.into()),
        ..Default::default()
    };
    cx.open_window(opts, |_, cx| cx.new(|cx| Overlay::new(state, cx))).ok()
}
