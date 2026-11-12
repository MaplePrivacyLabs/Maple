//! The empty-state title, in the display face, with a one-shot glint.
//!
//! The word settles, then a thin light rakes across it from the top: a
//! bright edge and a soft tail, so the dotted letters catch light without
//! filling in. It plays when the empty state appears, and again when New
//! Task is clicked. Reduced motion, and a window nobody is looking at,
//! paint the settled title and ask for no frames.

use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ContentMask, Element, ElementId, GlobalElementId, Hsla, IntoElement, LayoutId,
    Pixels, SharedString, Style, TextAlign, Window, point, px, size,
};

use super::theme;

/// How long the title takes to reach its resting ink.
const FADE: Duration = Duration::from_millis(64);
/// When the glint starts, just after the title has settled.
const START_MS: f32 = 72.;
/// How long the glint takes to cross the line.
const TRAVEL_MS: f32 = 280.;
/// How much the top of a letter leads its bottom, so the light rakes.
const RAKE_MS: f32 = 22.;
/// The bright edge follows the soft tail by this much.
const SPARK_LAG_MS: f32 = 14.;
const SPARK_MS: f32 = 96.;
const SPARK_PEAK: f32 = 0.24;
const GLOW_MS: f32 = 200.;
const GLOW_PEAK: f32 = 0.32;
/// Peak strength of the tail, under the bright edge.
const GLOW_ALPHA: f32 = 0.42;
/// Horizontal slices, so the rake is a few steps rather than one bar.
const SLICES: usize = 4;
/// Samples along the lit span. Enough to read as a falloff, not a stack of bars.
const BANDS: usize = 8;

/// The empty-state title. `play` changes when New Task is clicked, which
/// starts the glint again even if the title never left the screen.
pub fn empty_title(text: impl Into<SharedString>, play: u64) -> EmptyTitle {
    EmptyTitle {
        text: text.into(),
        play,
    }
}

pub struct EmptyTitle {
    text: SharedString,
    play: u64,
}

impl IntoElement for EmptyTitle {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

struct Drawn {
    play: u64,
    started: Option<Instant>,
}

impl Element for EmptyTitle {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::NamedInteger("empty-title".into(), self.play))
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let id = id.expect("empty title has an id");
        let play = self.play;
        let paused = cx.reduce_motion() || !window.is_window_active();
        let starting = self.text.clone();
        window.with_element_state::<Drawn, _>(id, |drawn, _| {
            let mut drawn = drawn.unwrap_or(Drawn {
                play,
                started: None,
            });
            if drawn.play != play {
                drawn.play = play;
                drawn.started = None;
            }
            drawn.started = settle(drawn.started, paused);
            if drawn.started.is_none() && !paused && !starting.is_empty() {
                drawn.started = Some(Instant::now());
            }
            ((), drawn)
        });
        let text = self.text.clone();
        // The measure closure runs after the parent text style has popped.
        let style = window.text_style();
        let rem = window.rem_size();
        let font_size = style.font_size.to_pixels(rem);
        let line_height = style.line_height_in_pixels(rem);
        let layout = window.request_measured_layout(
            Style::default(),
            move |known, available, window, _cx| {
                measure_title(
                    &text,
                    known.width,
                    available.width,
                    &style,
                    font_size,
                    line_height,
                    window,
                )
            },
        );
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let id = id.expect("empty title has an id");
        if self.text.is_empty() || bounds.size.width <= px(0.) {
            return;
        }
        let style = window.text_style();
        let rem = window.rem_size();
        let font_size = style.font_size.to_pixels(rem);
        let line_height = style.line_height_in_pixels(rem);
        let text = self.text.clone();
        let ink = style.color;
        let light = theme::is_light();
        let paused = cx.reduce_motion() || !window.is_window_active();
        let elapsed = window.with_element_state::<Drawn, _>(id, |drawn, _| {
            let mut drawn = drawn.unwrap_or(Drawn {
                play: self.play,
                started: None,
            });
            drawn.started = settle(drawn.started, paused);
            let elapsed = drawn.started.map(elapsed_ms);
            (elapsed, drawn)
        });
        let resting = paused || elapsed.is_none_or(sheen_finished);
        let alpha = if resting {
            1.
        } else {
            title_alpha(elapsed.unwrap_or(0.))
        };
        let content = content_width(window, &text, font_size, &style);
        let wrap = wrap_width(
            content,
            Some(bounds.size.width),
            gpui::AvailableSpace::Definite(bounds.size.width),
        );
        let lines = shape_lines(window, &text, font_size, &style, ink, alpha, wrap);
        let mut top = bounds.top();
        for line in &lines {
            let paragraph = Bounds::new(
                point(bounds.left(), top),
                size(bounds.size.width, line.size(line_height).height),
            );
            line.paint(
                paragraph.origin,
                line_height,
                TextAlign::Center,
                Some(paragraph),
                window,
                cx,
            )
            .ok();
            top += paragraph.size.height;
        }
        if !paused && let Some(elapsed) = elapsed.filter(|elapsed| !sheen_finished(*elapsed)) {
            paint_glint(
                window,
                cx,
                bounds,
                &lines,
                &Glint {
                    text: &text,
                    style: &style,
                    font_size,
                    line_height,
                    wrap,
                    elapsed_ms: elapsed,
                    peak: if light { 0.55 } else { 1. },
                    white: Hsla::from(gpui::rgb(0xf7f7f9)),
                },
            );
            window.request_animation_frame();
        }
    }
}

fn measure_title(
    text: &SharedString,
    known: Option<Pixels>,
    available: gpui::AvailableSpace,
    style: &gpui::TextStyle,
    font_size: Pixels,
    line_height: Pixels,
    window: &mut Window,
) -> gpui::Size<Pixels> {
    if text.is_empty() {
        return size(px(0.), line_height);
    }
    let content = content_width(window, text, font_size, style);
    let wrap = wrap_width(content, known, available);
    let lines = shape_lines(window, text, font_size, style, style.color, 1., wrap);
    size(wrap.unwrap_or(content), block_height(&lines, line_height))
}

/// Wrap when the pane is narrower than the unwrapped title. `None` means
/// the line fits. A known width (the laid-out box) wins over indefinite space.
pub(crate) fn wrap_width(
    content: Pixels,
    known: Option<Pixels>,
    available: gpui::AvailableSpace,
) -> Option<Pixels> {
    let limit = known.or(match available {
        gpui::AvailableSpace::Definite(width) => Some(width),
        gpui::AvailableSpace::MinContent | gpui::AvailableSpace::MaxContent => None,
    });
    limit.filter(|limit| *limit > px(0.) && *limit < content)
}

fn content_width(
    window: &mut Window,
    text: &SharedString,
    font_size: Pixels,
    style: &gpui::TextStyle,
) -> Pixels {
    let len = text.len();
    window
        .text_system()
        .shape_line(text.clone(), font_size, &[style.to_run(len)], None)
        .width
}

fn shape_lines(
    window: &mut Window,
    text: &SharedString,
    font_size: Pixels,
    style: &gpui::TextStyle,
    color: Hsla,
    alpha: f32,
    wrap: Option<Pixels>,
) -> Vec<gpui::WrappedLine> {
    if alpha <= 0. || text.is_empty() {
        return Vec::new();
    }
    let mut run = style.to_run(text.len());
    run.color = color.opacity(alpha);
    window
        .text_system()
        .shape_text(text.clone(), font_size, &[run], wrap, None)
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn visual_rows(line: &gpui::WrappedLine) -> usize {
    line.wrap_boundaries().len() + 1
}

fn block_height(lines: &[gpui::WrappedLine], line_height: Pixels) -> Pixels {
    let height = lines
        .iter()
        .fold(px(0.), |total, line| total + line.size(line_height).height);
    if height == px(0.) {
        line_height
    } else {
        height
    }
}

struct Glint<'a> {
    text: &'a SharedString,
    style: &'a gpui::TextStyle,
    font_size: Pixels,
    line_height: Pixels,
    wrap: Option<Pixels>,
    elapsed_ms: f32,
    peak: f32,
    white: Hsla,
}

fn paint_glint(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    lines: &[gpui::WrappedLine],
    glint: &Glint<'_>,
) {
    let mut top = bounds.top();
    for (index, line) in lines.iter().enumerate() {
        let paragraph = Bounds::new(
            point(bounds.left(), top),
            size(bounds.size.width, line.size(glint.line_height).height),
        );
        for row in 0..visual_rows(line) {
            let row_box = Bounds::new(
                point(bounds.left(), top + glint.line_height * row as f32),
                size(bounds.size.width, glint.line_height),
            );
            paint_row_glint(window, cx, paragraph, row_box, index, glint);
        }
        top += paragraph.size.height;
    }
}

fn paint_row_glint(
    window: &mut Window,
    cx: &mut App,
    paragraph: Bounds<Pixels>,
    row_box: Bounds<Pixels>,
    index: usize,
    glint: &Glint<'_>,
) {
    let width = row_box.size.width;
    let slice_h = row_box.size.height / SLICES as f32;
    let band_w = (width * 0.08).max(px(8.));
    for slice in 0..SLICES {
        let y = (slice as f32 + 0.5) / SLICES as f32;
        let lead = x_at(glint.elapsed_ms, y, SPARK_LAG_MS);
        let tail = x_at(glint.elapsed_ms, y, GLOW_MS);
        let top = row_box.top() + slice_h * slice as f32;
        for band in 0..BANDS {
            let x = tail + (lead - tail) * (band as f32 + 0.5) / BANDS as f32;
            if !(-0.08..=1.08).contains(&x) {
                continue;
            }
            let alpha = sheen_overlay(x.clamp(0., 1.), y, glint.elapsed_ms) * glint.peak;
            if alpha < 0.04 {
                continue;
            }
            let center = row_box.left() + width * x;
            let clip = Bounds::new(
                point(center - band_w * 0.5, top),
                size(band_w, slice_h + px(1.)),
            );
            let shaped = shape_lines(
                window,
                glint.text,
                glint.font_size,
                glint.style,
                glint.white,
                alpha,
                glint.wrap,
            );
            let Some(line) = shaped.get(index) else {
                continue;
            };
            window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                line.paint(
                    paragraph.origin,
                    glint.line_height,
                    TextAlign::Center,
                    Some(paragraph),
                    window,
                    cx,
                )
                .ok();
            });
        }
    }
}

/// Where, across the line, a mark `lag_ms` after the glint's arrival sits.
fn x_at(elapsed_ms: f32, y: f32, lag_ms: f32) -> f32 {
    (elapsed_ms - START_MS - y * RAKE_MS - lag_ms) / TRAVEL_MS
}

fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    1. - (1. - t).powi(3)
}

/// Resting ink, `0` at the start and `1` once the title has settled.
pub(crate) fn title_alpha(elapsed_ms: f32) -> f32 {
    ease_out(elapsed_ms / FADE.as_millis() as f32)
}

/// White overlay on a point of the title. `x` and `y` run `0..=1` across
/// the line; `y` is 0 at the top. `0` outside the glint.
pub(crate) fn sheen_overlay(x: f32, y: f32, elapsed_ms: f32) -> f32 {
    let arrival = START_MS + x.clamp(0., 1.) * TRAVEL_MS + y.clamp(0., 1.) * RAKE_MS;
    let glow = envelope((elapsed_ms - arrival) / GLOW_MS, GLOW_PEAK) * GLOW_ALPHA;
    let spark = envelope((elapsed_ms - arrival - SPARK_LAG_MS) / SPARK_MS, SPARK_PEAK);
    (glow + spark).clamp(0., 1.)
}

fn envelope(t: f32, peak_at: f32) -> f32 {
    if !(0.0..1.0).contains(&t) || peak_at <= 0. || peak_at >= 1. {
        return 0.;
    }
    if t <= peak_at {
        ease_out(t / peak_at)
    } else {
        let u = (t - peak_at) / (1. - peak_at);
        (1. - u).powi(3)
    }
}

fn sheen_finished(elapsed_ms: f32) -> bool {
    let end = START_MS + TRAVEL_MS + RAKE_MS + GLOW_MS;
    elapsed_ms >= end
}

fn elapsed_ms(started: Instant) -> f32 {
    started.elapsed().as_secs_f32() * 1000.
}

/// End a running glint when motion is paused. A finished or unstarted clock
/// stays as it is, so leaving pause does not replay it.
fn settle(started: Option<Instant>, paused: bool) -> Option<Instant> {
    if paused && started.is_some_and(|started| !sheen_finished(elapsed_ms(started))) {
        let end = (START_MS + TRAVEL_MS + RAKE_MS + GLOW_MS) as u64 + 1;
        Some(Instant::now() - Duration::from_millis(end))
    } else {
        started
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{InteractiveElement, ParentElement, Styled};

    #[test]
    fn the_title_settles_before_the_glint_and_the_glint_leaves() {
        assert_eq!(title_alpha(0.), 0.);
        assert!(title_alpha(64.) > 0.99);
        assert_eq!(sheen_overlay(0.5, 0.5, 0.), 0.);
        assert_eq!(sheen_overlay(0.5, 0.5, 40.), 0.);
        assert_eq!(sheen_overlay(0., 0., 800.), 0.);
        assert_eq!(sheen_overlay(1., 1., 800.), 0.);
    }

    #[test]
    fn the_glint_is_bright_at_its_head_and_quiet_behind_it() {
        let y = 0.5;
        let arrival = START_MS + 0.4 * TRAVEL_MS + y * RAKE_MS;
        let head = arrival + SPARK_LAG_MS + SPARK_PEAK * SPARK_MS;
        let at_head = sheen_overlay(0.4, y, head);
        let behind = sheen_overlay(0.05, y, head);
        let ahead = sheen_overlay(0.9, y, head);
        assert!(at_head > 0.7, "head {at_head}");
        assert!(behind < 0.2, "tail {behind}");
        assert_eq!(ahead, 0., "ahead of the light should still be dark");
    }

    #[test]
    fn a_narrow_pane_wraps_the_long_title_and_a_wide_one_does_not() {
        // German "Arbeite an allem, was du möchtest…" is the long title.
        // A 320px pane wraps it. The English line fits a 720px hero.
        assert_eq!(
            wrap_width(px(640.), None, gpui::AvailableSpace::Definite(px(320.))),
            Some(px(320.))
        );
        assert_eq!(
            wrap_width(px(292.), None, gpui::AvailableSpace::Definite(px(720.))),
            None
        );
        assert_eq!(
            wrap_width(px(640.), Some(px(280.)), gpui::AvailableSpace::MaxContent),
            Some(px(280.))
        );
    }

    #[test]
    fn the_top_of_a_letter_catches_the_light_before_the_bottom() {
        let x = 0.5;
        let arrival_top = START_MS + x * TRAVEL_MS;
        let caught = arrival_top + SPARK_LAG_MS + SPARK_PEAK * SPARK_MS;
        let top = sheen_overlay(x, 0., caught);
        let bottom = sheen_overlay(x, 1., caught);
        assert!(top > bottom, "top {top} bottom {bottom}");
        assert!(top > 0.7, "top {top}");
    }

    struct TitleProbe {
        text: SharedString,
        width: f32,
    }

    impl gpui::Render for TitleProbe {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div()
                .w(px(self.width))
                .font_family(crate::assets::FONT_DISPLAY)
                .text_size(px(36.))
                .line_height(px(48.))
                .child(
                    gpui::div()
                        .w_full()
                        .debug_selector(|| "empty-title-bounds".to_string())
                        .child(empty_title(self.text.clone(), 1)),
                )
        }
    }

    #[gpui::test]
    fn a_narrow_german_title_keeps_every_wrapped_row(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.text_system()
                .add_fonts(
                    crate::assets::FONTS
                        .iter()
                        .map(|font| std::borrow::Cow::Borrowed(*font))
                        .collect(),
                )
                .unwrap();
        });
        let (probe, cx) = cx.add_window_view(|_, _| TitleProbe {
            text: "Arbeite an allem, was du möchtest…".into(),
            width: 320.,
        });
        cx.simulate_resize(gpui::size(px(900.), px(700.)));
        let narrow = cx.debug_bounds("empty-title-bounds").expect("title");
        assert!(
            narrow.size.height >= px(96.),
            "styled parent did not wrap the title, got {:?}",
            narrow.size
        );
        probe.update(cx, |probe, cx| {
            probe.text = "Work on anything.".into();
            probe.width = 720.;
            cx.notify();
        });
        let wide = cx.debug_bounds("empty-title-bounds").expect("title");
        assert_eq!(wide.size.height, px(48.), "wide title {:?}", wide.size);
    }

    #[test]
    fn a_running_glint_settles_when_motion_is_paused() {
        let running = std::time::Instant::now();
        let settled = settle(Some(running), true).expect("clock");
        assert!(sheen_finished(elapsed_ms(settled)));
        assert!(!sheen_finished(elapsed_ms(
            settle(Some(running), false).expect("clock")
        )));
        assert!(settle(None, true).is_none());
    }
}
