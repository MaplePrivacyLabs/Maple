//! What sits around the composer and the menus its controls open: a
//! conversation's frame and its strip (the integrations that are on, the
//! permission mode, the context bar and Compact), the permissions popover,
//! the integrations list, the context card, and `+`'s menu and attach
//! popover.

use std::time::Duration;

use gpui::{Context, Div, IntoElement, SharedString, Window, div, prelude::*, px, relative};
use maple_agent::agent::{AgentContextBreakdown, AgentPermissionMode};

use super::composer::{
    ComposerControl, ContextLevel, ContextPart, composer_shadow, mode_chip_label, mode_icon,
    mode_label, mode_note, vim_mark,
};
use super::{ChatPopup, ChatScreen, OpenSettingsSection, Section};
use crate::ui::i18n::tr;
use crate::ui::icons::{icon, spinner};
use crate::ui::morph_text::morph_text;
use crate::ui::motion::{self, HoverBg as _};
use crate::ui::popup::{Menu, MenuItem, Placement};
use crate::ui::theme;
use crate::ui::typography;
use crate::ui::widgets;

/// The frame's top strip, which carries the task's state.
const STRIP_HEIGHT: gpui::Pixels = px(38.);
/// The frame's padding around the box, its 1 px line included: the box
/// keeps the transcript's width while the frame reaches past it.
const FRAME_INSET: gpui::Pixels = px(5.);
/// Concentric with the box: its radius plus the inset.
const FRAME_RADIUS: gpui::Pixels = px(29.);
/// A strip button: smaller than the box's chips, as quiet as text.
const STRIP_BUTTON_RADIUS: gpui::Pixels = px(9.);
/// The context bar's track.
const METER_WIDTH: f32 = 56.;

/// The permission modes, in the order the segmented control lays them out.
const MODES: [AgentPermissionMode; 3] = [
    AgentPermissionMode::ReadOnly,
    AgentPermissionMode::Auto,
    AgentPermissionMode::FullAccess,
];

/// The narrowest the menus holding the permissions switch get.
const PERMISSIONS_MENU_WIDTH: gpui::Pixels = px(340.);

/// What a mode's note says, its last two words held together, so a note
/// that has to wrap never leaves a word alone on its last line.
fn mode_note_text(mode: AgentPermissionMode) -> SharedString {
    typography::keep_last_words_together(&tr(mode_note(mode)))
}

/// Wide enough that every mode's note sits on one line, in whatever
/// language, so picking a mode never re-wraps its note or resizes the
/// menu. The menu still never outgrows the window; only there does a note
/// wrap.
fn permissions_menu_width(window: &Window) -> gpui::Pixels {
    // The note's `text_xs`, in the app's body face.
    let font_size = gpui::rems(0.75).to_pixels(window.rem_size());
    let widest = MODES
        .iter()
        .map(|mode| {
            typography::single_line_width(
                window,
                &mode_note_text(*mode),
                crate::assets::FONT_BODY,
                font_size,
            )
        })
        .fold(px(0.), |widest, width| widest.max(width));
    // Each side: the panel's 1 px border and 4 px padding, then the note's
    // 10 px. And a pixel spare, rounded up, so layout rounding never wraps
    // the note.
    (widest + px(2. * (1. + 4. + 10.) + 1.))
        .ceil()
        .max(PERMISSIONS_MENU_WIDTH)
}

/// A page of the new task's `+` menu beyond its first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AddPage {
    Integrations,
}

/// A part's estimate in the host's breakdown of a task's context.
fn part_tokens(breakdown: &AgentContextBreakdown, part: ContextPart) -> u64 {
    match part {
        ContextPart::Instructions => breakdown.instructions,
        ContextPart::Summary => breakdown.summary,
        ContextPart::Messages => breakdown.messages,
        ContextPart::ToolResults => breakdown.tool_results,
        ContextPart::Images => breakdown.images,
    }
}

/// Each part's share of `total` tokens: the estimates set the proportions
/// and the meter's total sets the size, so the parts always add up to what
/// the bar shows.
fn scaled_parts(breakdown: &AgentContextBreakdown, total: u64) -> Vec<(ContextPart, u64)> {
    let estimated: u64 = ContextPart::ALL
        .iter()
        .map(|part| part_tokens(breakdown, *part))
        .sum();
    if estimated == 0 || total == 0 {
        return Vec::new();
    }
    ContextPart::ALL
        .into_iter()
        .map(|part| {
            let share = part_tokens(breakdown, part) as f64 / estimated as f64;
            (part, (share * total as f64).round() as u64)
        })
        .filter(|(_, tokens)| *tokens > 0)
        .collect()
}

/// A token count as the card writes it: `812`, `48.6k`, `131k`.
pub(super) fn format_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    let thousands = tokens as f64 / 1_000.;
    if thousands >= 100. {
        format!("{}k", thousands.round() as u64)
    } else {
        let text = format!("{thousands:.1}");
        format!("{}k", text.strip_suffix(".0").unwrap_or(&text))
    }
}

/// A share of the window as the bar writes it: whole percent, and `<1%`
/// for a sliver rather than a misleading zero.
pub(super) fn format_percent(fraction: f32) -> String {
    if fraction > 0. && fraction < 0.01 {
        "<1%".to_string()
    } else {
        format!("{}%", (fraction * 100.).round() as u32)
    }
}

/// How much the strip's controls say in words: a narrow window drops the
/// integrations' word, then the context bar's percentage, then the mode's
/// name. Their tooltips and accessible names still say it all.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum StripFit {
    #[default]
    Full,
    /// The integrations' number alone.
    Count,
    /// The context bar without its percentage as well.
    Bare,
    /// The mode's icon alone as well.
    Icons,
}

/// The strip's padding either side, and the gap between its two groups.
const STRIP_PADDING: gpui::Pixels = px(3.);
const STRIP_GAP: gpui::Pixels = px(8.);

impl StripFit {
    const ALL: [StripFit; 4] = [
        StripFit::Full,
        StripFit::Count,
        StripFit::Bare,
        StripFit::Icons,
    ];

    /// The widest fit whose groups are known to fit a strip `width` wide,
    /// or the next to try while one of them is unmeasured.
    pub(super) fn choose(
        width: Option<gpui::Pixels>,
        left: [Option<gpui::Pixels>; 4],
        right: [Option<gpui::Pixels>; 4],
    ) -> Self {
        let Some(width) = width else {
            return StripFit::Full;
        };
        let room = width - STRIP_PADDING * 2. - STRIP_GAP;
        Self::ALL
            .into_iter()
            .zip(left.into_iter().zip(right))
            .find(|(_, (left, right))| match (left, right) {
                (Some(left), Some(right)) => *left + *right <= room + px(0.5),
                _ => true,
            })
            .map_or(StripFit::Icons, |(fit, _)| fit)
    }
}

/// What a strip measurement records.
#[derive(Clone, Copy)]
enum StripPart {
    /// The strip itself.
    Width,
    /// The integrations and mode, at their natural width.
    Left,
    /// The context bar and Compact, at their natural width.
    Right,
}

/// Records one of the strip's widths at prepaint, for the fit drawn, and
/// asks for the next frame when that changes the fit to draw.
fn measure_strip(
    frame: &std::rc::Rc<std::cell::Cell<super::composer::ComposerFrame>>,
    fit: StripFit,
    part: StripPart,
) -> impl IntoElement {
    let frame = frame.clone();
    gpui::canvas(
        move |bounds, window, _| {
            let mut measured = frame.get();
            let width = Some(bounds.size.width);
            match part {
                StripPart::Width => measured.strip_width = width,
                StripPart::Left => measured.strip_left[fit as usize] = width,
                StripPart::Right => measured.strip_right[fit as usize] = width,
            }
            frame.set(measured);
            let next = StripFit::choose(
                measured.strip_width,
                measured.strip_left,
                measured.strip_right,
            );
            // A refresh is ignored while the window draws; the next frame
            // renders the strip again.
            if next != fit {
                window.request_animation_frame();
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0()
}

/// A strip button: a 26 px capsule of 12 px text that only fills under
/// the pointer or while its menu is open.
fn strip_button(id: &'static str, open: bool) -> motion::HoverBackground {
    div()
        .id(id)
        .debug_selector(|| id.to_string())
        .role(gpui::Role::Button)
        .flex_none()
        .h(px(26.))
        .px_2()
        .flex()
        .items_center()
        .gap_1p5()
        .rounded(STRIP_BUTTON_RADIUS)
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(gpui::rgb(theme::text_secondary()))
        .when(open, |el| el.bg(gpui::rgb(theme::bg_sidebar_pill())))
        .hover_bg(gpui::rgb(theme::bg_sidebar_pill()))
        .hover(|style| style.cursor_pointer())
        .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
}

impl ChatScreen {
    /// A conversation's composer in its frame: a tray 4 px around the box
    /// whose top strip carries the task's state. The frame's fill and line
    /// sit on a layer of their own, so the frame can fade in around a box
    /// that just landed from a new task.
    pub(super) fn render_composer_frame(
        &mut self,
        composer_box: gpui::Stateful<Div>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let (tray, strip) = self.frame_arrival(window, cx);
        let selected = self.application_vim_selected_control(window);
        let fills = self.composer_fills();
        div()
            .debug_selector(|| "composer-frame".to_string())
            .relative()
            .flex()
            .flex_col()
            .mx(-FRAME_INSET)
            .px(FRAME_INSET)
            .pb(FRAME_INSET)
            .rounded(FRAME_RADIUS)
            .when(fills, |frame| frame.flex_1().min_h_0())
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(FRAME_RADIUS)
                    .bg(gpui::rgb(theme::bg_composer_tray()))
                    .border_1()
                    .border_color(gpui::rgb(theme::composer_frame_border()))
                    .shadow(composer_shadow())
                    .opacity(tray),
            )
            .child(self.render_strip(selected, strip, window, cx))
            .child(composer_box)
    }

    /// For a box that just left a new task: how far the frame's tray and
    /// strip have faded in around it, the tray first, then the strip.
    fn frame_arrival(&mut self, window: &mut Window, cx: &mut Context<Self>) -> (f32, f32) {
        let Some(landed) = self.frame_arrived else {
            return (1., 1.);
        };
        let reduced = cx.reduce_motion();
        let now = motion::now();
        let after = |delay: u64, token: motion::Token| {
            let started = landed + Duration::from_millis(delay);
            if now < started {
                0.
            } else {
                motion::ease_out_cubic(motion::progress(started, token, now, reduced))
            }
        };
        let tray = after(80, motion::PANEL);
        let strip = after(140, motion::ENTER);
        if strip < 1. {
            window.request_animation_frame();
        } else {
            self.frame_arrived = None;
        }
        (tray, strip)
    }

    fn render_strip(
        &mut self,
        selected: Option<ComposerControl>,
        opacity: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        // The words the fits were measured with; new ones measure afresh.
        let words = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            self.integrations_count_label().hash(&mut hasher);
            tr(mode_label(self.permission_mode)).hash(&mut hasher);
            (
                self.context_shown(),
                self.compact_offered(),
                self.compacting,
            )
                .hash(&mut hasher);
            format_percent(self.context_used()).len().hash(&mut hasher);
            hasher.finish()
        };
        let mut measured = self.composer_frame.get();
        if measured.strip_words != words {
            measured.strip_words = words;
            measured.strip_left = [None; 4];
            measured.strip_right = [None; 4];
            self.composer_frame.set(measured);
        }
        let fit = StripFit::choose(
            measured.strip_width,
            measured.strip_left,
            measured.strip_right,
        );
        let compacting = self.compacting.then(|| {
            div()
                .debug_selector(|| "composer-compacting".to_string())
                .flex()
                .items_center()
                .gap_1p5()
                .h(px(26.))
                .px_2()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(spinner("composer-compacting", px(12.), theme::text_muted()))
                .child(tr("chat-compacting"))
        });
        let meter =
            self.render_context_bar(selected == Some(ComposerControl::Context), fit, window, cx);
        let compact = self.render_compact_button(selected == Some(ComposerControl::Compact), cx);
        div()
            .debug_selector(|| "composer-strip".to_string())
            .relative()
            .h(STRIP_HEIGHT)
            .px(STRIP_PADDING)
            .flex()
            .items_center()
            .justify_between()
            .gap(STRIP_GAP)
            .opacity(opacity)
            .child(measure_strip(&self.composer_frame, fit, StripPart::Width))
            .child(
                // The room the bar and Compact leave; the controls keep to it.
                div().flex().flex_1().min_w_0().overflow_hidden().child(
                    div()
                        .relative()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(2.))
                        .child(measure_strip(&self.composer_frame, fit, StripPart::Left))
                        .child(self.render_integrations_button(
                            selected == Some(ComposerControl::Integrations),
                            fit,
                            window,
                            cx,
                        ))
                        .child(self.render_mode_button(
                            selected == Some(ComposerControl::Mode),
                            fit,
                            window,
                            cx,
                        )),
                ),
            )
            .child(
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .flex_none()
                    .child(measure_strip(&self.composer_frame, fit, StripPart::Right))
                    .children(compacting)
                    .children(meter)
                    .children(compact),
            )
    }

    /// How many integrations are on, as the strip and the new task's token
    /// write it, which is also their accessible name.
    pub(super) fn integrations_count_label(&self) -> SharedString {
        self.messages.get(
            "chat-integrations-on",
            [("count", self.mcp_enabled_count.into())],
        )
    }

    /// The count, drawn: only the number morphs as one turns on or off; the
    /// word stays put, its plural easing in or out.
    pub(super) fn integrations_count_text(&self) -> crate::ui::morph_text::MorphText {
        morph_text(
            ("composer-integrations-count", 0usize),
            self.integrations_count_label(),
        )
    }

    /// What the integrations' tooltip says: the ones on, or how to get some.
    pub(super) fn integrations_tooltip(&self) -> SharedString {
        if self.mcp_enabled_count > 0 {
            self.messages.get(
                "chat-integrations-on-tooltip",
                [("names", self.integrations_on_names.to_string().into())],
            )
        } else if self.usable_integrations().next().is_some() {
            tr("chat-integrations-turn-on")
        } else {
            tr("chat-setup-integrations")
        }
    }

    /// The integrations set up for this task's use: on, or ready to turn
    /// on. One that still needs setting up is Settings' business.
    fn usable_integrations(
        &self,
    ) -> impl Iterator<Item = &maple_agent::agent::AgentSessionMcpServer> {
        self.session_mcp
            .iter()
            .filter(|server| server.available || server.enabled)
    }

    fn render_integrations_button(
        &self,
        selected: bool,
        fit: StripFit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let open = self.popup.is_open(&ChatPopup::Integrations);
        let any_on = self.mcp_enabled_count > 0;
        let ink = if any_on {
            theme::text_secondary()
        } else {
            theme::text_muted()
        };
        let tooltip = self.integrations_tooltip();
        let button = strip_button("mcp-menu", open)
            .aria_label(self.integrations_count_label())
            .text_color(gpui::rgb(ink))
            .when(!open, |el| el.tooltip(widgets::tooltip(tooltip, None)))
            .child(icon("puzzle", px(14.), ink))
            .child(match fit {
                StripFit::Full => self.integrations_count_text().color(gpui::rgb(ink)),
                StripFit::Count | StripFit::Bare | StripFit::Icons => morph_text(
                    ("composer-integrations-count", 0usize),
                    SharedString::from(self.mcp_enabled_count.to_string()),
                )
                .color(gpui::rgb(ink)),
            });
        vim_mark(
            self.with_menu(
                ChatPopup::Integrations,
                button,
                Placement::AboveStart,
                window,
                cx,
                |this, _, _| this.integrations_menu(),
            ),
            selected,
        )
    }

    fn render_mode_button(
        &self,
        selected: bool,
        fit: StripFit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let mode = self.permission_mode;
        let open = self.popup.is_open(&ChatPopup::Mode);
        // Full access runs everything without asking, so it stays warm.
        let ink = if mode == AgentPermissionMode::FullAccess {
            theme::accent_ink()
        } else {
            theme::text_secondary()
        };
        let index = MODES.iter().position(|each| *each == mode).unwrap_or(1);
        // Without its name, the mode's tooltip says it.
        let named = fit != StripFit::Icons;
        let tooltip = if named {
            tr("chat-mode")
        } else {
            tr(mode_label(mode))
        };
        let button = strip_button("permission-mode-menu", open)
            .aria_label(tr(mode_chip_label(mode)))
            .text_color(gpui::rgb(ink))
            .when(!open, |el| el.tooltip(widgets::tooltip(tooltip, None)))
            .child(motion::icon_cycle(
                "composer-mode-icon",
                index,
                &MODES.map(mode_icon),
                px(14.),
                ink,
            ))
            .when(named, |button| {
                button.child(div().whitespace_nowrap().child(morph_text(
                    ("composer-mode-label", 0usize),
                    tr(mode_label(mode)),
                )))
            })
            .child(icon("chevron-down", px(12.), theme::text_muted()));
        vim_mark(
            self.with_menu(
                ChatPopup::Mode,
                button,
                Placement::AboveStart,
                window,
                cx,
                |this, window, cx| this.permissions_menu(window, cx),
            ),
            selected,
        )
    }

    /// The context bar shows once the task has used some of its window
    /// and is not compacting.
    pub(super) fn context_shown(&self) -> bool {
        self.context_limit > 0 && self.ledger_context_tokens > 0 && !self.compacting
    }

    fn context_used(&self) -> f32 {
        if self.context_limit <= 0 {
            return 0.;
        }
        (self.ledger_context_tokens as f32 / self.context_limit as f32).clamp(0., 1.)
    }

    /// Compact sits beside the bar once the window nears its limit, while
    /// no turn is running.
    pub(super) fn compact_offered(&self) -> bool {
        self.context_shown()
            && ContextLevel::of(self.context_used()) != ContextLevel::Room
            && !self.is_run_active()
    }

    /// The bar is quiet grey while the window has room, amber from 75% and
    /// red from 90%. Its label is the percentage alone: the card it opens
    /// says what it measures.
    fn render_context_bar(
        &mut self,
        selected: bool,
        fit: StripFit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        if !self.context_shown() {
            return None;
        }
        let used = self.context_used();
        let level = ContextLevel::of(used);
        let (fill, moving) =
            self.context_fill
                .step(used, motion::PANEL, motion::now(), cx.reduce_motion());
        if moving {
            window.request_animation_frame();
        }
        let percent = format_percent(used);
        let open = self.popup.is_open(&ChatPopup::Context);
        let label = if level == ContextLevel::Room {
            self.messages
                .get("chat-context-used", [("percent", percent.clone().into())])
        } else {
            self.messages.get(
                "chat-context-near-limit",
                [("percent", percent.clone().into())],
            )
        };
        // Without its percentage, the bar's tooltip says it.
        let worded = matches!(fit, StripFit::Full | StripFit::Count);
        let button = strip_button("context-bar", open)
            .gap_2()
            .when(!worded && !open, |el| {
                el.tooltip(widgets::tooltip(label.clone(), None))
            })
            .aria_label(label)
            .child(
                div()
                    .flex_none()
                    .w(px(METER_WIDTH))
                    .h(px(6.))
                    .rounded(px(3.))
                    .bg(gpui::rgb(theme::meter_track()))
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .w(px(METER_WIDTH * fill))
                            .rounded(px(3.))
                            .bg(gpui::rgb(level.color())),
                    ),
            )
            .when(worded, |button| {
                button.child(
                    div()
                        .min_w(px(28.))
                        .flex()
                        .justify_end()
                        .text_color(gpui::rgb(if level == ContextLevel::Room {
                            theme::text_muted()
                        } else {
                            level.color()
                        }))
                        .child(percent),
                )
            });
        let popup = ChatPopup::Context;
        let menu = self.popup.is_open(&popup).then(|| {
            let menu = self.context_card(window, cx);
            self.popup.render(menu, Placement::AboveEnd, window, cx)
        });
        Some(vim_mark(
            div()
                .relative()
                .child(self.popup.trigger(popup, button, cx, |this, _, cx| {
                    if this.popup.is_open(&ChatPopup::Context) {
                        this.popup.close(cx);
                    } else {
                        this.open_context_card(cx);
                    }
                }))
                .children(menu),
            selected,
        ))
    }

    fn render_compact_button(&self, selected: bool, cx: &mut Context<Self>) -> Option<Div> {
        if !self.compact_offered() {
            return None;
        }
        let level = ContextLevel::of(self.context_used());
        let button = div()
            .id("compact-now")
            .debug_selector(|| "compact-now".to_string())
            .role(gpui::Role::Button)
            .aria_label(tr("chat-compact"))
            .h(px(26.))
            .px_2()
            .flex()
            .items_center()
            .rounded(STRIP_BUTTON_RADIUS)
            .text_xs()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(gpui::rgb(level.color()))
            .hover_bg(gpui::rgb(theme::bg_sidebar_pill()))
            .hover(|style| style.cursor_pointer())
            .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
            .tooltip(widgets::tooltip(tr("chat-slash-compact"), None))
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.compact_now(cx);
            }))
            .child(tr("chat-compact"));
        Some(vim_mark(div().child(button), selected))
    }

    pub(super) fn open_context_card(&mut self, cx: &mut Context<Self>) {
        self.context_card_opened = Some(motion::now());
        self.context_hover = None;
        self.popup.open(ChatPopup::Context, cx);
        self.refresh_context_usage(cx);
    }

    /// Ask the host what fills the task's context, for the open card. A
    /// host too old to say refuses, and the card shows the total alone.
    pub(super) fn refresh_context_breakdown(&self, cx: &mut Context<Self>) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        let backend = self.backend.clone();
        let user_id = self.user_id.clone();
        let requested = session_id.clone();
        self.call(
            async move { backend.context_breakdown(&user_id, &requested).await },
            cx,
            move |this, result, cx| {
                // The answer describes the task it was asked about; a switch
                // since then owns the card.
                if this.selected_session.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                let breakdown = result.ok().flatten();
                if this.context_breakdown != breakdown {
                    this.context_breakdown = breakdown;
                    cx.notify();
                }
            },
        );
    }

    /// The permission mode, stepped by Left and Right in its popover.
    fn step_permission_mode(&mut self, delta: i32, cx: &mut Context<Self>) {
        let index = MODES
            .iter()
            .position(|mode| *mode == self.permission_mode)
            .unwrap_or(1) as i32;
        let next = (index + delta.signum()).clamp(0, MODES.len() as i32 - 1) as usize;
        if MODES[next] != self.permission_mode {
            self.set_permission_mode(MODES[next], cx);
        }
    }

    /// The permission modes as one segmented control, a knob sliding
    /// behind the current one, and a line under it saying what it means.
    fn render_permissions_section(&self, cx: &mut Context<Self>) -> Div {
        let mode = self.permission_mode;
        let index = MODES.iter().position(|each| *each == mode).unwrap_or(1);
        let knob = motion::animated("composer-mode-knob", index as f32, motion::TOGGLE, |at| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(relative(at / MODES.len() as f32))
                .w(relative(1. / MODES.len() as f32))
                .p(px(1.))
                .child(
                    div()
                        .size_full()
                        .rounded(STRIP_BUTTON_RADIUS)
                        .bg(gpui::rgb(theme::bg_sidebar_pill())),
                )
                .into_any_element()
        });
        let segments = MODES.map(|each| {
            let current = each == mode;
            let ink = match (current, each) {
                (true, AgentPermissionMode::FullAccess) => theme::accent_ink(),
                (true, _) => theme::text_heading(),
                (false, _) => theme::text_muted(),
            };
            div()
                .id(SharedString::from(format!("mode-{}", each.as_str())))
                .debug_selector(move || format!("mode-{}", each.as_str()))
                .role(gpui::Role::RadioButton)
                .aria_label(tr(mode_label(each)))
                .aria_toggled(if current {
                    gpui::accesskit::Toggled::True
                } else {
                    gpui::accesskit::Toggled::False
                })
                .relative()
                .flex_1()
                .min_w_0()
                .h(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .gap_1p5()
                .rounded(STRIP_BUTTON_RADIUS)
                .text_xs()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(gpui::rgb(ink))
                .when(!current, |el| {
                    el.hover(|style| style.text_color(gpui::rgb(theme::text_secondary())))
                })
                .cursor_pointer()
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    if this.permission_mode != each {
                        this.set_permission_mode(each, cx);
                    }
                }))
                .child(icon(mode_icon(each), px(13.), ink))
                .child(div().truncate().child(tr(mode_label(each))))
        });
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_2p5()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(tr("chat-mode")),
            )
            .child(
                div()
                    .mx_1p5()
                    .p(px(2.))
                    .rounded(theme::RADIUS_MD)
                    .bg(gpui::rgb(theme::bg_app()))
                    .border_1()
                    .border_color(gpui::rgb(theme::border_subtle()))
                    .child(
                        div()
                            .id("mode-segments")
                            .relative()
                            .flex()
                            .role(gpui::Role::RadioGroup)
                            .aria_label(tr("chat-mode"))
                            .child(knob)
                            .children(segments),
                    ),
            )
            .child(
                div()
                    .px_2p5()
                    .pt_1p5()
                    .pb_1()
                    .min_h(px(26.))
                    // The face and size `permissions_menu_width` measures.
                    .font_family(crate::assets::FONT_BODY)
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(
                        div()
                            .debug_selector(|| "mode-note".to_string())
                            .child(gpui::text!(mode_note_text(mode))),
                    ),
            )
    }

    /// The permissions popover from a conversation's strip, or from a new
    /// task's mode token. Left and Right step the mode.
    pub(super) fn permissions_menu(&self, window: &Window, cx: &mut Context<Self>) -> Menu<Self> {
        Menu::new("mode-menu", permissions_menu_width(window))
            .label(tr("chat-mode"))
            .application_vim(self.application_vim_enabled)
            .on_adjust(
                |this: &mut Self, delta: i32, _: &mut Window, cx: &mut Context<Self>| {
                    this.step_permission_mode(delta, cx)
                },
            )
            .child(self.render_permissions_section(cx))
    }

    /// One integration's row: its name and a switch. The description is
    /// the row's tooltip, so a dozen integrations still fit in view.
    fn integration_item(
        &self,
        server: &maple_agent::agent::AgentSessionMcpServer,
        arrive: impl FnOnce(gpui::Stateful<Div>) -> gpui::Stateful<Div> + 'static,
    ) -> MenuItem<Self> {
        let name = server.name.clone();
        let kind = server.kind;
        let enabled = server.enabled;
        let description = if server.kind == maple_agent::agent::AgentSessionIntegrationKind::Mcp
            && server.name == "cua-driver"
        {
            tr("settings-integration-cua-description")
        } else {
            crate::ui::settings::saved_description(&server.display_name, &server.description)
        };
        MenuItem::new(
            SharedString::from(format!("mcp-{kind:?}-{name}")),
            server.display_name.clone(),
            move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                this.toggle_session_mcp(name.clone(), kind, !enabled, cx);
            },
        )
        .switch(enabled)
        .style(move |row| {
            let row = arrive(row);
            if description.is_empty() {
                row
            } else {
                row.tooltip(widgets::tooltip(description, None))
            }
        })
    }

    fn manage_integrations_item(&self) -> MenuItem<Self> {
        MenuItem::new(
            "mcp-manage",
            tr("chat-manage-integrations"),
            |_: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                cx.emit(OpenSettingsSection(Section::Integrations));
            },
        )
        .style(|row| row.text_color(gpui::rgb(theme::accent_ink())))
    }

    /// The task's integrations, each with a switch, in one compact list
    /// that keeps Manage in view under it however long it runs.
    pub(super) fn integrations_menu(&self) -> Menu<Self> {
        let mut menu = Menu::new("integrations-menu", px(320.))
            .label(tr("chat-integrations"))
            .max_height(px(440.))
            .application_vim(self.application_vim_enabled)
            .header(tr("chat-integrations-for-task"));
        let mut any = false;
        for server in self.usable_integrations() {
            any = true;
            menu = menu.item(self.integration_item(server, |row| row));
        }
        if !any {
            menu = menu.child(
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(tr("chat-no-integrations")),
            );
        }
        menu.pinned(self.manage_integrations_item())
    }

    /// A new task's `+`: permissions on top, then integrations and attach.
    /// It opens upward, so Attach, the most common pick, sits nearest the
    /// button. Integrations turns the menu to a page of their switches;
    /// the menu eases to the page's height and the page's rows come in top
    /// to bottom.
    pub(super) fn add_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> Menu<Self> {
        let turned = self.add_page_turned;
        // Rows keep arriving for a moment after the page turns.
        if turned.is_some_and(|turned| {
            motion::now().saturating_duration_since(turned)
                < Duration::from_millis(40 + 30 * 16) + motion::ENTER.duration
        }) {
            window.request_animation_frame();
        }
        let arrive = move |row: gpui::Stateful<Div>, order: usize| {
            let Some(turned) = turned else {
                return row;
            };
            let started = turned + Duration::from_millis(40 + 30 * order as u64);
            let now = motion::now();
            let shown = if now < started {
                0.
            } else {
                motion::ease_out_quint(motion::progress(started, motion::ENTER, now, false))
            };
            row.opacity(shown)
        };
        let menu = Menu::new("add-menu", permissions_menu_width(window))
            .label(tr("chat-add"))
            .max_height(px(460.))
            .ease_height()
            .application_vim(self.application_vim_enabled);
        match self.add_page {
            Some(AddPage::Integrations) => {
                let mut menu = menu
                    .on_adjust(
                        |this: &mut Self, delta: i32, _: &mut Window, cx: &mut Context<Self>| {
                            if delta < 0 {
                                this.turn_add_page(None, cx);
                            }
                        },
                    )
                    .item(
                        MenuItem::new(
                            "add-back",
                            tr("chat-integrations"),
                            |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                                this.turn_add_page(None, cx);
                            },
                        )
                        .icon("chevron-left")
                        .keep_open()
                        .style(|row| row.font_weight(gpui::FontWeight::MEDIUM)),
                    )
                    .separator();
                let mut any = false;
                for (index, server) in self.usable_integrations().enumerate() {
                    any = true;
                    menu =
                        menu.item(self.integration_item(server, move |row| arrive(row, index + 1)));
                }
                if !any {
                    menu = menu.child(
                        div()
                            .px_3()
                            .py_2()
                            .text_sm()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(tr("chat-no-integrations")),
                    );
                }
                menu.pinned(self.manage_integrations_item())
            }
            None => {
                let usable = self.usable_integrations().next().is_some();
                let integrations = if usable {
                    let summary = if self.mcp_enabled_count == 0 {
                        tr("chat-integrations-summary-none")
                    } else {
                        self.messages.get(
                            "chat-integrations-summary-on",
                            [("count", self.mcp_enabled_count.into())],
                        )
                    };
                    MenuItem::new(
                        "add-integrations",
                        tr("chat-integrations"),
                        |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                            this.turn_add_page(Some(AddPage::Integrations), cx);
                            this.refresh_session_mcp(cx);
                        },
                    )
                    .icon("puzzle")
                    .keep_open()
                    .trailing(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_xs()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(summary)
                            .child(icon(
                                "chevron-right",
                                widgets::ROW_ICON,
                                theme::text_muted(),
                            )),
                    )
                } else {
                    MenuItem::new(
                        "add-integrations",
                        tr("chat-integrations"),
                        |_: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                            cx.emit(OpenSettingsSection(Section::Integrations));
                        },
                    )
                    .icon("puzzle")
                    .trailing(widgets::navigate_mark(theme::text_muted()))
                    .style(|row| row.tooltip(widgets::tooltip(tr("chat-setup-integrations"), None)))
                };
                menu.on_adjust(
                    |this: &mut Self, delta: i32, _: &mut Window, cx: &mut Context<Self>| {
                        let on_integrations = this
                            .popup
                            .highlighted()
                            .is_some_and(|id| *id == gpui::ElementId::from("add-integrations"));
                        if on_integrations && delta > 0 {
                            this.turn_add_page(Some(AddPage::Integrations), cx);
                            this.refresh_session_mcp(cx);
                        } else {
                            this.step_permission_mode(delta, cx);
                        }
                    },
                )
                .child(self.render_permissions_section(cx))
                .separator()
                .item(integrations.style(move |row| arrive(row, 1)))
                .item(
                    MenuItem::new(
                        "add-attach",
                        tr("chat-attach"),
                        |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                            this.pick_attachments(cx);
                        },
                    )
                    .icon("paperclip")
                    .style(move |row| {
                        arrive(row, 2).tooltip(widgets::tooltip(tr("chat-attach-drop-hint"), None))
                    }),
                )
            }
        }
    }

    /// Turn the `+` menu to `page`, from where it is.
    fn turn_add_page(&mut self, page: Option<AddPage>, cx: &mut Context<Self>) {
        if self.add_page == page {
            return;
        }
        self.add_page = page;
        self.add_page_turned = (!cx.reduce_motion()).then(motion::now);
        // Back on the first page the Integrations row stays highlighted,
        // so Right goes straight back in.
        let highlight = match page {
            Some(AddPage::Integrations) => gpui::ElementId::from("add-back"),
            None => gpui::ElementId::from("add-integrations"),
        };
        self.popup.highlight(Some(highlight), cx);
        cx.notify();
    }

    /// A conversation's `+`: one well to click or drop files on, and how
    /// to paste an image.
    pub(super) fn attach_menu(&self, cx: &mut Context<Self>) -> Menu<Self> {
        let hovered = self
            .popup
            .highlighted()
            .is_some_and(|id| *id == gpui::ElementId::from("attach-well"));
        let tile = motion::animated(
            "attach-well-tile",
            if hovered { 1. } else { 0. },
            motion::REVEAL,
            |lift| {
                div()
                    .size(px(40.))
                    .mb_3()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::RADIUS_MD)
                    .bg(motion::mix(
                        gpui::rgb(theme::bg_sidebar_pill()).into(),
                        gpui::rgb(theme::bg_sidebar_row_selected()).into(),
                        lift,
                    ))
                    .group_drag_over::<gpui::ExternalPaths>("attach-well", |style| {
                        style.bg(gpui::rgb(theme::accent_container()))
                    })
                    .child(
                        icon("paperclip", px(18.), theme::text_secondary()).with_transformation(
                            gpui::Transformation::rotate(gpui::radians(-0.21 * lift)),
                        ),
                    )
                    .into_any_element()
            },
        );
        let dropped = cx.listener(|this, paths: &gpui::ExternalPaths, _window, cx| {
            this.popup.close(cx);
            this.add_attachment_paths(paths.paths().to_vec(), cx);
        });
        let well = MenuItem::new(
            "attach-well",
            tr("chat-attach"),
            |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                this.pick_attachments(cx);
            },
        )
        .content(
            div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(tile)
                .child(
                    div()
                        .relative()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(gpui::rgb(theme::text_primary()))
                        .child(
                            div()
                                .group_drag_over::<gpui::ExternalPaths>("attach-well", |style| {
                                    style.opacity(0.)
                                })
                                .child(tr("chat-attach-choose")),
                        )
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .justify_center()
                                .opacity(0.)
                                .text_color(gpui::rgb(theme::accent_ink()))
                                .group_drag_over::<gpui::ExternalPaths>("attach-well", |style| {
                                    style.opacity(1.)
                                })
                                .child(tr("chat-attach-drop")),
                        ),
                )
                .child(
                    div()
                        .mt(px(3.))
                        .text_xs()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .group_drag_over::<gpui::ExternalPaths>("attach-well", |style| {
                            style.opacity(0.)
                        })
                        .child(tr("chat-attach-or-drop")),
                ),
        )
        .style(move |row| {
            row.group("attach-well")
                .h(px(132.))
                .justify_center()
                .rounded(theme::RADIUS_MD)
                .bg(gpui::rgb(theme::bg_composer_tray()))
                .border_1()
                .border_color(gpui::rgb(theme::border_subtle()))
                .hover(|style| style.border_color(gpui::rgb(theme::border_strong())))
                .can_drop(|dragged, _window, _cx| {
                    dragged.downcast_ref::<gpui::ExternalPaths>().is_some()
                })
                .drag_over::<gpui::ExternalPaths>(|style, _paths, _window, _cx| {
                    style
                        .bg(gpui::rgb(theme::accent_container()))
                        .border_color(gpui::rgb(theme::accent_hover()))
                })
                .on_drop(dropped)
        });
        Menu::new("attach-menu", px(300.))
            .label(tr("chat-attach"))
            .application_vim(self.application_vim_enabled)
            .item(well)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_1p5()
                    .pt_2p5()
                    .pb_1()
                    .text_xs()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(tr("chat-attach-paste"))
                    .child(widgets::keycap(if cfg!(target_os = "macos") {
                        "⌘V"
                    } else {
                        "Ctrl+V"
                    })),
            )
    }

    /// The context card: how full the window is, what fills it, in the
    /// order the window holds it, and where the task compacts itself.
    pub(super) fn context_card(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Menu<Self> {
        Menu::new("context-card", px(300.))
            .label(tr("chat-context-title"))
            .application_vim(self.application_vim_enabled)
            .child(self.render_context_card(window, cx))
    }

    fn render_context_card(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let window_tokens = self.context_limit.max(0) as u64;
        let total = self.ledger_context_tokens.max(0) as u64;
        let used = self.context_used();
        let level = ContextLevel::of(used);
        let parts = self
            .context_breakdown
            .map(|breakdown| scaled_parts(&breakdown, total))
            .unwrap_or_default();
        let compact_at = self
            .context_breakdown
            .and_then(|breakdown| breakdown.compact_at)
            .filter(|at| *at > 0 && *at < window_tokens);
        let busy = self.is_run_active() || self.compacting;
        let past = compact_at.is_some_and(|at| total > at);
        // The parts grow in, left to right, as the card opens.
        let opened = self.context_card_opened.unwrap_or_else(motion::now);
        let reduced = cx.reduce_motion();
        let now = motion::now();
        let mut growing = false;
        let share = move |tokens: u64| -> f32 {
            if window_tokens == 0 {
                0.
            } else {
                tokens as f32 / window_tokens as f32
            }
        };
        let hover = self.context_hover;
        let segments = if parts.is_empty() {
            vec![(ContextPart::Messages, total)]
        } else {
            parts.clone()
        }
        .into_iter()
        .enumerate()
        .map(|(order, (part, tokens))| {
            let started = opened + Duration::from_millis(40 + 35 * order as u64);
            let grown = if now < started {
                0.
            } else {
                motion::ease_out_quint(motion::progress(
                    started,
                    motion::ENTER.scaled(1.45),
                    now,
                    reduced,
                ))
            };
            growing |= grown < 1.;
            let dimmed = hover.is_some_and(|hovered| hovered != part);
            motion::animated(
                ("context-segment", order),
                if dimmed { 0.32 } else { 1. },
                motion::HOVER,
                move |strength| {
                    div()
                        .h_full()
                        .flex_shrink(1.)
                        .min_w(px(3.))
                        .w(relative(share(tokens) * grown))
                        .bg(gpui::rgb(part.color()))
                        .opacity(strength)
                        .into_any_element()
                },
            )
            .into_any_element()
        })
        .collect::<Vec<_>>();
        if growing {
            window.request_animation_frame();
        }
        let tick = compact_at.map(|at| {
            div()
                .absolute()
                .top(px(-5.))
                .left(relative(share(at)))
                .ml(px(-3.))
                .w(px(6.))
                .h(px(20.))
                .rounded(px(3.))
                .bg(gpui::rgb(theme::bg_elevated()))
                .flex()
                .justify_center()
                .py(px(2.))
                .child(
                    div()
                        .w(px(2.))
                        .h_full()
                        .rounded(px(1.))
                        .bg(gpui::rgb(theme::context_tick())),
                )
        });
        let legend = parts
            .iter()
            .copied()
            .enumerate()
            .map(|(order, (part, tokens))| {
                let dimmed = hover.is_some_and(|hovered| hovered != part);
                div()
                    .id(("context-legend", order))
                    .debug_selector(move || format!("context-legend-{part:?}"))
                    .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                        let next = if *hovered {
                            Some(part)
                        } else if this.context_hover == Some(part) {
                            None
                        } else {
                            this.context_hover
                        };
                        if next != this.context_hover {
                            this.context_hover = next;
                            cx.notify();
                        }
                    }))
                    .child(motion::animated(
                        ("context-legend-row", order),
                        if dimmed { 0.32 } else { 1. },
                        motion::HOVER,
                        move |strength| {
                            div()
                                .h(px(24.))
                                .flex()
                                .items_center()
                                .gap_2p5()
                                .opacity(strength)
                                .child(
                                    div()
                                        .flex_none()
                                        .size(px(8.))
                                        .rounded(px(2.5))
                                        .bg(gpui::rgb(part.color())),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .text_xs()
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(gpui::rgb(theme::text_secondary()))
                                        .child(gpui::text!(
                                            id = ("context-legend-label", order),
                                            tr(part.label())
                                        )),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(gpui::rgb(theme::text_muted()))
                                        .child(gpui::text!(
                                            id = ("context-legend-tokens", order),
                                            format_tokens(tokens)
                                        )),
                                )
                                .into_any_element()
                        },
                    ))
            });
        // Where the task compacts by itself, once the host has said; an
        // older host does not, and the card claims nothing.
        let compact_note = self.context_breakdown.map(|_| match compact_at {
            Some(_) if past => tr("chat-context-compacts-next"),
            Some(at) => self.messages.get(
                "chat-context-compacts-at",
                [("percent", format_percent(share(at)).into())],
            ),
            None => tr("chat-context-compacts-off"),
        });
        div()
            .debug_selector(|| "context-card".to_string())
            .flex()
            .flex_col()
            .px_3()
            .pt_2p5()
            .pb_2()
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(22.))
                            .line_height(px(26.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(if level == ContextLevel::Room {
                                theme::text_heading()
                            } else {
                                level.color()
                            }))
                            .child(gpui::text!(format_percent(used))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(gpui::text!(self.messages.get(
                                "chat-context-of",
                                [
                                    ("used", format_tokens(total).into()),
                                    ("limit", format_tokens(window_tokens).into()),
                                ],
                            ))),
                    ),
            )
            .child(
                div()
                    .id("context-card-bar")
                    .relative()
                    .mt_3p5()
                    .mb_3()
                    .child(
                        div()
                            .h(px(10.))
                            .flex()
                            .gap(px(2.))
                            .rounded(px(5.))
                            .overflow_hidden()
                            .children(segments)
                            .child(
                                div()
                                    .h_full()
                                    .flex_1()
                                    .min_w_0()
                                    .bg(gpui::rgb(theme::context_free())),
                            ),
                    )
                    .children(tick),
            )
            .child(div().flex().flex_col().gap(px(2.)).children(legend))
            .child(
                div()
                    .mt_2p5()
                    .pt_2p5()
                    .border_t_1()
                    .border_color(gpui::rgb(theme::border_subtle()))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .when(compact_note.is_some(), |note| {
                                note.debug_selector(|| "context-compact-note".to_string())
                            })
                            .children(compact_note.map(|note| gpui::text!(note))),
                    )
                    .child(
                        widgets::secondary_button("context-compact-now")
                            .debug_selector(|| "context-compact-now".to_string())
                            .flex_none()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .when(level != ContextLevel::Room, |button| {
                                button
                                    .bg(gpui::rgb(theme::accent_container()))
                                    .text_color(gpui::rgb(theme::accent_ink()))
                            })
                            .when(busy, |button| {
                                button
                                    .opacity(0.5)
                                    .tooltip(widgets::tooltip(tr("chat-compact-after-turn"), None))
                            })
                            .when(!busy, |button| {
                                button.on_click(cx.listener(|this, _event, _window, cx| {
                                    this.popup.close(cx);
                                    this.compact_now(cx);
                                }))
                            })
                            .child(gpui::text!(tr("chat-compact"))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_read_short() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(812), "812");
        assert_eq!(format_tokens(1_000), "1k");
        assert_eq!(format_tokens(48_640), "48.6k");
        assert_eq!(format_tokens(99_960), "100k");
        assert_eq!(format_tokens(131_072), "131k");
    }

    #[test]
    fn a_sliver_of_the_window_is_not_zero() {
        assert_eq!(format_percent(0.), "0%");
        assert_eq!(format_percent(0.004), "<1%");
        assert_eq!(format_percent(0.486), "49%");
        assert_eq!(format_percent(1.), "100%");
    }

    /// The bar's colours change at 75% and 90%, where Compact is offered
    /// and where the window is nearly full.
    #[test]
    fn the_context_level_turns_at_75_and_90_percent() {
        assert_eq!(ContextLevel::of(0.), ContextLevel::Room);
        assert_eq!(ContextLevel::of(0.749), ContextLevel::Room);
        assert_eq!(ContextLevel::of(0.75), ContextLevel::Near);
        assert_eq!(ContextLevel::of(0.899), ContextLevel::Near);
        assert_eq!(ContextLevel::of(0.9), ContextLevel::Full);
    }

    /// The strip keeps the widest fit that fits, trying an unmeasured one
    /// before giving up words.
    #[test]
    fn the_strip_gives_up_words_only_as_it_must() {
        let width = Some(px(300.) + STRIP_PADDING * 2. + STRIP_GAP);
        let known = |px_each: [f32; 4]| px_each.map(|each| Some(px(each)));
        assert_eq!(StripFit::choose(None, [None; 4], [None; 4]), StripFit::Full);
        assert_eq!(
            StripFit::choose(width, [None; 4], [None; 4]),
            StripFit::Full
        );
        let right = known([120., 120., 80., 80.]);
        assert_eq!(
            StripFit::choose(width, known([180., 150., 150., 100.]), right),
            StripFit::Full
        );
        assert_eq!(
            StripFit::choose(width, [Some(px(200.)), None, None, None], right),
            StripFit::Count
        );
        assert_eq!(
            StripFit::choose(width, known([260., 230., 240., 100.]), right),
            StripFit::Icons
        );
        assert_eq!(
            StripFit::choose(width, known([260., 230., 210., 100.]), right),
            StripFit::Bare
        );
        assert_eq!(
            StripFit::choose(width, known([400., 400., 400., 400.]), right),
            StripFit::Icons,
            "the last fit when nothing fits"
        );
    }

    /// The card's parts come from estimates, but they always add up to
    /// the total the bar shows, in the window's order, without empty ones.
    #[test]
    fn the_breakdown_scales_to_the_measured_total() {
        let breakdown = AgentContextBreakdown {
            instructions: 9_000,
            summary: 0,
            messages: 21_000,
            tool_results: 12_000,
            images: 3_000,
            compact_at: Some(114_688),
        };
        let parts = scaled_parts(&breakdown, 50_000);
        assert_eq!(
            parts.iter().map(|(part, _)| *part).collect::<Vec<_>>(),
            [
                ContextPart::Instructions,
                ContextPart::Messages,
                ContextPart::ToolResults,
                ContextPart::Images,
            ]
        );
        let sum: u64 = parts.iter().map(|(_, tokens)| tokens).sum();
        assert!(sum.abs_diff(50_000) <= parts.len() as u64, "{sum}");
        assert_eq!(parts[0].1, 10_000);
        assert!(scaled_parts(&AgentContextBreakdown::default(), 50_000).is_empty());
        assert!(scaled_parts(&breakdown, 0).is_empty());
    }
}
