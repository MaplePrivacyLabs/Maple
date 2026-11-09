//! The model chip's panel: the models a task can use, and under them how
//! hard the selected model thinks, on a slider sized to the levels that
//! model offers. Maple's users switch models more often than effort, so
//! the models come first, and a pick closes the panel as the plain list
//! did.
//!
//! Changes of state move rather than swap: the level's name morphs letter
//! by letter into the next one, the brain gives a small pulse as the level
//! lands, and reset spins back. Arriving at the model's top level, the
//! brain sparks: a warm flash and a burst of rays.

use std::time::Duration;

use gpui::{Animation, AnimationExt as _, Transformation};

use super::thinking::thinking_label;
use super::*;
use crate::ui::effort_slider::{effort_slider, heat_color};
use crate::ui::i18n::tr;
use crate::ui::icons::{icon, spinner};
use crate::ui::morph_text::morph_text;

/// The brain's pulse as a level lands from the pointer.
const PULSE: Duration = Duration::from_millis(420);
/// Reset's spin back.
const SPIN: Duration = Duration::from_millis(560);
/// The brain's spark at the top stop.
const SPARK: Duration = Duration::from_millis(560);
/// The slider and the status line cross-fade this fast when one replaces
/// the other (levels arriving for a new model).
const BODY_IN: Duration = Duration::from_millis(200);

/// What the effort footer's body showed last, and its latest swap between
/// the slider and the status line.
#[derive(Clone, Copy, Default)]
pub(super) struct EffortBody {
    slider: Option<bool>,
    swaps: usize,
    at: Option<std::time::Instant>,
}

/// A level's place on one scale shared by every model, from no thinking
/// (0) to the most a model offers (1). The slider spreads a model's own
/// levels evenly; this keeps the colour of each level the same on all of
/// them.
pub(super) fn level_heat(level: ThinkingLevel) -> f32 {
    match level {
        ThinkingLevel::Off => 0.,
        ThinkingLevel::Minimal => 0.15,
        ThinkingLevel::Low => 0.3,
        ThinkingLevel::Medium => 0.5,
        ThinkingLevel::High => 0.7,
        ThinkingLevel::Xhigh => 0.85,
        ThinkingLevel::Max => 1.,
    }
}

/// The level a model runs at when nobody picked one: Medium, else the
/// nearest level it accepts above Medium, else below. The runtime clamps
/// the same way when no level is saved (`clamp_thinking_level`).
pub(super) fn recommended_level(levels: &[ThinkingLevel]) -> Option<ThinkingLevel> {
    levels
        .iter()
        .copied()
        .find(|level| *level >= ThinkingLevel::Medium)
        .or_else(|| levels.last().copied())
}

impl ChatScreen {
    /// The levels the slider offers, or `None` while they are unknown or
    /// the model does not think.
    fn effort_levels(&self) -> Option<&[ThinkingLevel]> {
        self.thinking
            .as_ref()
            .map(|state| state.levels.as_slice())
            .filter(|levels| !levels.is_empty() && *levels != [ThinkingLevel::Off])
    }

    /// The level the panel shows: the one under a drag, then one waiting
    /// to save, then the saved one.
    pub(super) fn effort_shown(&self) -> Option<ThinkingLevel> {
        let state = self.thinking.as_ref()?;
        let level = self
            .effort_preview
            .or_else(|| self.queued_thinking())
            .unwrap_or(state.current);
        Some(if state.levels.contains(&level) {
            level
        } else {
            state.current
        })
    }

    /// Forget a drag the panel never saw end and start from the saved
    /// level, with nothing left to replay.
    pub(super) fn reset_effort_input(&mut self) {
        self.effort_preview = None;
        self.effort_keyed = false;
        self.effort_pulse_at = None;
        self.effort_body.set(EffortBody::default());
        self.effort_slider.borrow_mut().release();
    }

    /// The brain's pulse for a level landing from the pointer.
    fn pulse_effort(&mut self) {
        self.effort_pulses += 1;
        self.effort_pulse_at = Some(std::time::Instant::now());
    }

    fn preview_effort(&mut self, stop: Option<usize>, cx: &mut Context<Self>) {
        let level = stop.and_then(|stop| self.effort_levels()?.get(stop).copied());
        if self.effort_preview != level {
            self.effort_preview = level;
            cx.notify();
        }
    }

    fn commit_effort(&mut self, stop: usize, cx: &mut Context<Self>) {
        self.effort_preview = None;
        if let Some(level) = self
            .effort_levels()
            .and_then(|levels| levels.get(stop).copied())
        {
            self.pick_thinking(level, cx);
        }
        self.effort_keyed = false;
        self.pulse_effort();
        cx.notify();
    }

    /// Left and Right in the panel: one stop down or up.
    pub(super) fn step_effort(&mut self, step: i32, cx: &mut Context<Self>) {
        let (Some(levels), Some(shown)) = (self.effort_levels(), self.effort_shown()) else {
            return;
        };
        let Some(index) = levels.iter().position(|level| *level == shown) else {
            return;
        };
        let next = index as i64 + i64::from(step);
        let Some(level) = usize::try_from(next)
            .ok()
            .and_then(|next| levels.get(next).copied())
        else {
            return;
        };
        // A key step moves the thumb but plays no effects (the slider
        // checks the same), and its name swaps at once.
        self.effort_keyed = true;
        self.pick_thinking(level, cx);
    }

    fn reset_effort(&mut self, cx: &mut Context<Self>) {
        if let Some(level) = self.effort_levels().and_then(recommended_level) {
            self.effort_reset_spins += 1;
            self.effort_reset_at = Some(std::time::Instant::now());
            self.effort_keyed = false;
            self.pulse_effort();
            self.pick_thinking(level, cx);
        }
    }

    /// Open the model panel from the keyboard (`/model`, the composer
    /// row): on the selected model, as a native popup button opens on its
    /// choice, with the slider starting from the saved level.
    pub(super) fn open_model_panel(&mut self, cx: &mut Context<Self>) {
        match self.selected_model.clone() {
            Some(model) => {
                self.popup
                    .open_highlighted(ChatPopup::Model, format!("model-{model}"), cx)
            }
            None => self.popup.open(ChatPopup::Model, cx),
        }
        self.reset_effort_input();
    }

    /// The panel's footer, under the models: the selected model's level,
    /// reset, and the slider.
    pub(super) fn effort_footer(&self, cx: &mut Context<Self>) -> Div {
        let levels = self.effort_levels();
        let shown = self.effort_shown();
        let heat = match (levels, shown) {
            (Some(_), Some(level)) => level_heat(level),
            _ => 0.,
        };
        let recommended = levels.and_then(recommended_level);
        let resettable = levels.is_some_and(|levels| levels.len() > 1)
            && recommended.is_some()
            && shown != recommended;

        let title: SharedString = match (levels, shown) {
            (Some(_), Some(level)) => thinking_label(level),
            (None, Some(_)) => tr("chat-thinking-off"),
            _ => tr("chat-effort"),
        };
        let title_color: gpui::Hsla = if levels.is_some() {
            heat_color(heat)
        } else {
            gpui::rgb(theme::text_secondary()).into()
        };
        // The brain pulses for a pick or reset from the pointer, once: not
        // for each stop a drag crosses, a key step, or a panel reopening.
        let pulse = self
            .effort_pulse_at
            .filter(|at| at.elapsed() < PULSE)
            .map(|_| self.effort_pulses);
        // Arriving at the top level, while the brain's spark runs.
        let spark = self
            .effort_slider
            .borrow()
            .topped()
            .filter(|(_, at)| at.elapsed() < SPARK);
        let header = div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                // What this footer sets, warming with the level and giving
                // a small pulse as one lands.
                div()
                    .size_8()
                    .flex_none()
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    // At the top stop, a warm disc flashes behind the brain.
                    .when_some(spark, |brain, (count, _)| {
                        brain.child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(div().rounded_full().bg(title_color).with_animation(
                                    ("effort-brain-flash", count),
                                    Animation::new(SPARK).with_easing(gpui::ease_out_quint()),
                                    |disc, delta| {
                                        disc.size(px(14. + 16. * delta))
                                            .opacity(0.32 * (1. - delta))
                                    },
                                )),
                        )
                    })
                    .child({
                        let brain =
                            icon("brain", px(18.), theme::text_muted()).text_color(title_color);
                        match pulse {
                            Some(count) => brain
                                .with_animation(
                                    ("effort-brain", count),
                                    Animation::new(PULSE).with_easing(gpui::ease_out_quint()),
                                    |brain, delta| {
                                        let scale =
                                            1. + 0.16 * (std::f32::consts::PI * delta).sin();
                                        brain.with_transformation(Transformation::scale(
                                            gpui::size(scale, scale),
                                        ))
                                    },
                                )
                                .into_any_element(),
                            None => brain.into_any_element(),
                        }
                    })
                    // ...and throws a burst of spark lines.
                    .when_some(spark, |brain, (count, _)| {
                        brain.child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    icon("effort-burst", px(32.), theme::accent())
                                        .text_color(title_color)
                                        .with_animation(
                                            ("effort-brain-burst", count),
                                            Animation::new(SPARK)
                                                .with_easing(gpui::ease_out_quint()),
                                            |burst, delta| {
                                                let scale = 0.65 + 0.6 * delta;
                                                burst
                                                    .with_transformation(Transformation::scale(
                                                        gpui::size(scale, scale),
                                                    ))
                                                    .opacity(
                                                        (delta / 0.15).min(1.)
                                                            * (1. - delta).powf(1.5),
                                                    )
                                            },
                                        ),
                                ),
                        )
                    }),
            )
            .child(
                // The level's name morphs into the next one.
                div()
                    .debug_selector(|| "effort-title".to_string())
                    .flex_1()
                    .min_w_0()
                    .text_size(px(14.))
                    .line_height(px(20.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .whitespace_nowrap()
                    // A drag swaps the name as it crosses stops, since a morph
                    // per stop would garble a fast sweep; so does a key step.
                    .child(
                        morph_text("effort-title-text", title.clone())
                            .color(title_color)
                            .instant(self.effort_preview.is_some() || self.effort_keyed),
                    ),
            )
            .child({
                // Reset spins back as it acts, and rests dimmed at the
                // recommended level.
                let reset = icon(
                    "rotate-ccw",
                    px(16.),
                    if resettable {
                        theme::text_secondary()
                    } else {
                        theme::text_faint()
                    },
                );
                // Only while the spin runs: a reopened panel must not
                // replay an earlier reset.
                let spinning = self.effort_reset_at.is_some_and(|at| at.elapsed() < SPIN);
                let reset = if spinning {
                    reset
                        .with_animation(
                            ("effort-reset-spin", self.effort_reset_spins),
                            Animation::new(SPIN).with_easing(gpui::ease_out_quint()),
                            |reset, delta| {
                                // One turn back, counterclockwise like the icon.
                                reset.with_transformation(Transformation::rotate(gpui::radians(
                                    -std::f32::consts::TAU * delta,
                                )))
                            },
                        )
                        .into_any_element()
                } else {
                    reset.into_any_element()
                };
                div()
                    .id("effort-reset")
                    .debug_selector(|| "effort-reset".to_string())
                    .role(gpui::Role::Button)
                    .aria_label(tr("chat-effort-reset"))
                    .size_8()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::RADIUS_SM)
                    .when(resettable, |button| {
                        button
                            .hover(|style| {
                                style
                                    .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
                                    .cursor_pointer()
                            })
                            .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
                            .tooltip(widgets::tooltip(tr("chat-effort-reset"), None))
                            .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.reset_effort(cx);
                            }))
                    })
                    .child(reset)
            });

        let labels = || {
            div()
                .flex()
                .justify_between()
                .px_1()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(gpui::text!(tr("chat-effort-faster")))
                .child(gpui::text!(tr("chat-effort-deeper")))
        };
        let slider_shown = levels.is_some();
        let body = match levels {
            Some(levels) => {
                let selected = shown
                    .and_then(|shown| levels.iter().position(|level| *level == shown))
                    .unwrap_or(0);
                let stop = |level: Option<ThinkingLevel>| {
                    level.and_then(|level| levels.iter().position(|each| *each == level))
                };
                let slider = div()
                    .id("effort-slider")
                    .debug_selector(|| "effort-slider".to_string())
                    .role(gpui::Role::Slider)
                    .aria_label(tr("chat-effort"))
                    .aria_value(title.clone())
                    .aria_numeric_value(selected as f64)
                    .aria_min_numeric_value(0.)
                    .aria_max_numeric_value(levels.len().saturating_sub(1) as f64)
                    .w_full()
                    .child(
                        effort_slider(
                            "effort-track",
                            self.effort_slider.clone(),
                            levels.iter().copied().map(level_heat).collect(),
                            selected,
                        )
                        .recommended(stop(recommended))
                        .enabled(levels.len() > 1)
                        .on_preview(cx.listener(|this, stop: &Option<usize>, _, cx| {
                            this.preview_effort(*stop, cx);
                        }))
                        .on_commit(cx.listener(
                            |this, stop: &usize, _, cx| {
                                this.commit_effort(*stop, cx);
                            },
                        )),
                    );
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(slider)
                    .when(levels.len() > 1, |body| body.child(labels()))
            }
            // The status line holds the slider's place, so the panel keeps
            // its height when levels arrive and the slider replaces it.
            None => div()
                .relative()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(div().h(px(44.)))
                .child(labels().invisible())
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .child(self.effort_status(cx).w_full()),
                ),
        };
        // When one replaces the other (not on opening), the incoming body
        // rises into place as it fades in.
        let swap = {
            let mut seen = self.effort_body.get();
            if seen.slider != Some(slider_shown) {
                if seen.slider.is_some() {
                    seen.swaps += 1;
                    seen.at = Some(std::time::Instant::now());
                }
                seen.slider = Some(slider_shown);
                self.effort_body.set(seen);
            }
            seen.at
                .filter(|at| at.elapsed() < BODY_IN)
                .map(|_| seen.swaps)
        };
        let body = match swap {
            Some(count) => body
                .with_animation(
                    ("effort-body", count),
                    Animation::new(BODY_IN).with_easing(gpui::ease_out_quint()),
                    |body, delta| {
                        body.opacity(delta)
                            .mt(px(4. * (1. - delta)))
                            .mb(px(-4. * (1. - delta)))
                    },
                )
                .into_any_element(),
            None => body.into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .gap_2()
            .mt_1()
            .pt_2()
            .px_2()
            .pb_1()
            .border_t_1()
            .border_color(gpui::rgb(theme::border()))
            .child(header)
            .child(body)
    }

    /// The track's place while there is no slider: loading, saving, a
    /// model that does not think, or levels that failed to load.
    fn effort_status(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let (text, retry) = if self.selected_model.is_none() {
            (tr("chat-thinking-choose-model"), false)
        } else if self.thinking.is_some() {
            (tr("chat-thinking-unsupported"), false)
        } else if self.thinking_loading {
            (tr("chat-thinking-loading"), false)
        } else if self.thinking_saving {
            (tr("chat-thinking-saving"), false)
        } else {
            (tr("chat-thinking-unavailable"), true)
        };
        div()
            .id("effort-status")
            .debug_selector(|| "effort-status".to_string())
            .h(px(44.))
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .px_4()
            .rounded_full()
            .bg(gpui::rgb(theme::bg_sidebar_pill()))
            .text_xs()
            .text_color(gpui::rgb(theme::text_muted()))
            .when(self.thinking_loading || self.thinking_saving, |row| {
                row.child(spinner("effort-loading", px(12.), theme::text_muted()))
            })
            .child(div().min_w_0().truncate().child(gpui::text!(text)))
            .when(retry, |row| {
                row.child(
                    div()
                        .id("retry-thinking")
                        .debug_selector(|| "retry-thinking".to_string())
                        .role(gpui::Role::Button)
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(gpui::rgb(theme::accent()))
                        .hover(|style| style.underline().cursor_pointer())
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.refresh_thinking(cx);
                            cx.notify();
                        }))
                        .child(gpui::text!(tr("chat-thinking-retry"))),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ThinkingLevel::{High, Low, Max, Medium, Minimal, Off, Xhigh};

    /// The levels each Maple Work model gets from the catalog's reasoning
    /// controls (see the runtime's `reasoning_catalog_shapes_define_levels_
    /// and_clamp_the_default`), and the level the runtime falls back to.
    #[test]
    fn the_recommended_level_is_the_runtimes_fallback_for_each_shape() {
        for (levels, recommended) in [
            // GLM 5.3, GLM-5.3 Flash, Kimi K3, Kimi K3.1 Pro.
            (vec![Low, High, Max], High),
            // DeepSeek V4.1 Flash and its uncensored build.
            (vec![Off, Low, High, Xhigh, Max], High),
            // Gemma 4 and other models without effort tiers.
            (vec![Off, Minimal, Low, Medium, High], Medium),
            // A model whose levels all sit below Medium.
            (vec![Off, Minimal, Low], Low),
            (vec![Off], Off),
        ] {
            assert_eq!(recommended_level(&levels), Some(recommended), "{levels:?}");
        }
        assert_eq!(recommended_level(&[]), None);
    }
}
