//! Text that morphs from one value to the next, after torph's TextMorph:
//! letters the two values share glide from their old places to their new
//! ones, letters that leave fade out where they stood, new letters rise in
//! where they land, and the width and colour ease between the two. "High"
//! to "Extra high" keeps its "igh"; "Low" to "Off" simply crossfades.
//!
//! One shaping per change and one tiny shaped run per letter per frame,
//! only while a morph runs; at rest it paints a single line and asks for no
//! frames. Reduced motion swaps the text at once.
//!
//! Text that reports work in progress can also carry a shine: a band of a
//! second colour that crosses the words on the shared loop clock.

use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, Hsla, IntoElement, LayoutId, Pixels,
    SharedString, Style, TextAlign, Window, point, px, size,
};

/// How long a morph takes; torph's default.
const DURATION: Duration = Duration::from_millis(400);
/// How far a letter rises as it fades in, or drifts as it fades out.
const RISE: Pixels = px(3.);

pub struct MorphText {
    id: ElementId,
    text: SharedString,
    color: Option<Hsla>,
    instant: bool,
    shine: Option<(Duration, Hsla)>,
    shine_band: ShineBand,
}

/// How wide the shine's band is and how long it rests between passes.
#[derive(Clone, Copy)]
struct ShineBand {
    /// The band's half-width, as a fraction of the line.
    reach: f32,
    /// The share of each cycle the band spends off the line.
    rest: f32,
}

impl Default for ShineBand {
    fn default() -> Self {
        Self {
            reach: 0.3,
            rest: 0.,
        }
    }
}

/// Morphing text in the surrounding text style; `id` keeps its state
/// across frames, so it must be stable while the text changes.
pub fn morph_text(id: impl Into<ElementId>, text: impl Into<SharedString>) -> MorphText {
    MorphText {
        id: id.into(),
        text: text.into(),
        color: None,
        instant: false,
        shine: None,
        shine_band: ShineBand::default(),
    }
}

impl MorphText {
    /// Paint in `color` rather than the inherited text colour; a change of
    /// colour eases along with the letters.
    pub fn color(mut self, color: impl Into<Hsla>) -> Self {
        self.color = Some(color.into());
        self
    }

    /// Swap to a new value at once instead of morphing, for text that
    /// changes faster than a morph can read, such as a label tracking a
    /// drag.
    pub fn instant(mut self, instant: bool) -> Self {
        self.instant = instant;
        self
    }

    /// Sweep a band of `color` across the words once every `cycle`. It
    /// holds still when loops do: under reduced motion, and in a window
    /// that is not active.
    pub fn shine(mut self, cycle: Duration, color: impl Into<Hsla>) -> Self {
        self.shine = Some((cycle, color.into()));
        self
    }

    /// Shape the shine: a band `reach` of the line wide on each side of
    /// its centre, crossing in the first `1 - rest` of each cycle and
    /// resting off the line for the remainder. The defaults (0.3, no
    /// rest) are the transcript's thinking line.
    pub fn shine_band(mut self, reach: f32, rest: f32) -> Self {
        self.shine_band = ShineBand {
            reach: reach.max(0.01),
            rest: rest.clamp(0., 0.95),
        };
        self
    }
}

/// How much of the shine a letter `at` of the way along the line takes
/// when the cycle is `phase` of the way through.
fn shine_level(at: f32, phase: f32, band: ShineBand) -> f32 {
    let pass = phase / (1. - band.rest);
    if pass >= 1. {
        return 0.;
    }
    // The band enters and leaves past the ends of the line, so it is
    // off the line at both ends of its pass and a rest adds no snap.
    let head = -band.reach + pass * (1. + 2. * band.reach);
    crate::ui::motion::smoothstep((1. - (at - head).abs() / band.reach).max(0.))
}

/// One letter and where it sits from the line's start.
#[derive(Clone)]
struct Letter {
    text: SharedString,
    x: Pixels,
}

struct Morph {
    started: Instant,
    /// Where each current letter starts: `Some` glides from there, `None`
    /// fades in at its place.
    from: Vec<Option<Pixels>>,
    leaving: Vec<Letter>,
    from_width: Pixels,
    from_color: Hsla,
}

#[derive(Default)]
struct State {
    text: SharedString,
    letters: Vec<Letter>,
    width: Pixels,
    color: Option<Hsla>,
    morph: Option<Morph>,
}

/// torph's `cubic-bezier(0.19, 1, 0.22, 1)` is an exponential ease out.
fn ease(t: f32) -> f32 {
    if t >= 1. {
        1.
    } else {
        1. - 2f32.powf(-10. * t.max(0.))
    }
}

fn mix(from: Hsla, to: Hsla, amount: f32) -> Hsla {
    let from = from.to_rgb();
    let to = to.to_rgb();
    gpui::Rgba {
        r: from.r + (to.r - from.r) * amount,
        g: from.g + (to.g - from.g) * amount,
        b: from.b + (to.b - from.b) * amount,
        a: from.a + (to.a - from.a) * amount,
    }
    .into()
}

impl State {
    fn progress(&self, now: Instant) -> Option<f32> {
        let morph = self.morph.as_ref()?;
        Some(now.saturating_duration_since(morph.started).as_secs_f32() / DURATION.as_secs_f32())
    }

    /// Where letter `index` is drawn at eased progress `eased`.
    fn letter_x(&self, index: usize, eased: f32) -> Pixels {
        let to = self.letters[index].x;
        match self.morph.as_ref().and_then(|morph| morph.from[index]) {
            Some(from) => from + (to - from) * eased,
            None => to,
        }
    }

    fn visual_width(&self, eased: Option<f32>) -> Pixels {
        match (&self.morph, eased) {
            (Some(morph), Some(eased)) => {
                morph.from_width + (self.width - morph.from_width) * eased
            }
            _ => self.width,
        }
    }

    fn visual_color(&self, eased: Option<f32>, fallback: Hsla) -> Hsla {
        let color = self.color.unwrap_or(fallback);
        match (&self.morph, eased) {
            (Some(morph), Some(eased)) => mix(morph.from_color, color, eased),
            _ => color,
        }
    }
}

/// Longest common subsequence of two letter lists, as paired indices,
/// walked forwards so ties go to the earliest match.
fn common_letters(a: &[Letter], b: &[Letter]) -> Vec<(usize, usize)> {
    let (m, n) = (a.len(), b.len());
    let mut table = vec![vec![0u16; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            table[i][j] = if a[i].text == b[j].text {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < m && j < n {
        if a[i].text == b[j].text {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

impl IntoElement for MorphText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for MorphText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
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
        let id = id.expect("morphing text has an id");
        let style = window.text_style();
        let rem = window.rem_size();
        let font_size = style.font_size.to_pixels(rem);
        let line_height = style.line_height_in_pixels(rem);
        let reduce_motion = cx.reduce_motion() || self.instant;
        let text = self.text.clone();
        let color = self.color.unwrap_or(style.color);
        let width = window.with_element_state::<State, _>(id, |state, window| {
            let mut state = state.unwrap_or_default();
            let now = Instant::now();
            if state.text != text || state.letters.is_empty() && !text.is_empty() {
                let shaped = window.text_system().shape_line(
                    text.clone(),
                    font_size,
                    &[style.to_run(text.len())],
                    None,
                );
                let letters: Vec<Letter> = text
                    .char_indices()
                    .map(|(index, letter)| Letter {
                        text: SharedString::from(letter.to_string()),
                        x: shaped.x_for_index(index),
                    })
                    .collect();
                let first = state.letters.is_empty() && state.text.is_empty();
                state.morph = if first || reduce_motion {
                    None
                } else {
                    // Start every letter from where it is drawn now, so a
                    // morph that interrupts another carries on smoothly.
                    let eased = state.progress(now).map(ease);
                    let at = eased.unwrap_or(1.);
                    let drawn: Vec<Letter> = (0..state.letters.len())
                        .map(|index| Letter {
                            text: state.letters[index].text.clone(),
                            x: state.letter_x(index, at),
                        })
                        .collect();
                    let pairs = common_letters(&drawn, &letters);
                    let mut from = vec![None; letters.len()];
                    for (old, new) in &pairs {
                        from[*new] = Some(drawn[*old].x);
                    }
                    let leaving = drawn
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| !pairs.iter().any(|(old, _)| old == index))
                        .map(|(_, letter)| letter.clone())
                        .collect();
                    Some(Morph {
                        started: now,
                        from,
                        leaving,
                        from_width: state.visual_width(eased),
                        from_color: state.visual_color(eased, color),
                    })
                };
                state.text = text.clone();
                state.letters = letters;
                state.width = shaped.width;
            } else if state.progress(now).is_some_and(|progress| progress >= 1.) {
                state.morph = None;
            }
            state.color = Some(color);
            let width = state.visual_width(state.progress(now).map(ease));
            (width, state)
        });
        let layout = Style {
            size: size(width.into(), line_height.into()),
            flex_shrink: 0.,
            ..Style::default()
        };
        (window.request_layout(layout, [], cx), ())
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
        let id = id.expect("morphing text has an id");
        let style = window.text_style();
        let rem = window.rem_size();
        let font_size = style.font_size.to_pixels(rem);
        let line_height = style.line_height_in_pixels(rem);
        let fallback = style.color;
        // Where the band is this frame, while loops run.
        let shine = self
            .shine
            .filter(|_| !crate::ui::motion::loops_paused(window, cx))
            .map(|(cycle, color)| (crate::ui::motion::phase(cycle), color));
        let band = self.shine_band;
        let animating = window.with_element_state::<State, _>(id, |state, window| {
            let state = state.unwrap_or_default();
            let progress = state.progress(Instant::now());
            let eased = progress.map(ease);
            let color = state.visual_color(eased, fallback);
            // A letter `x` along the line takes the shine where the band is.
            let tint = |x: Pixels| match shine {
                Some((phase, shine)) if state.width > px(0.) => {
                    mix(color, shine, shine_level(x / state.width, phase, band))
                }
                _ => color,
            };
            let draw = |window: &mut Window,
                        cx: &mut App,
                        text: &SharedString,
                        runs: &[gpui::TextRun],
                        x,
                        y| {
                window
                    .text_system()
                    .shape_line(text.clone(), font_size, runs, None)
                    .paint(
                        point(bounds.left() + x, bounds.top() + y),
                        line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    )
                    .ok();
            };
            let paint =
                |window: &mut Window, cx: &mut App, text: &SharedString, x, y, alpha: f32| {
                    if alpha <= 0. {
                        return;
                    }
                    let mut run = style.to_run(text.len());
                    run.color = tint(x).opacity(alpha);
                    draw(window, cx, text, &[run], x, y);
                };
            match (progress, &state.morph) {
                (Some(t), Some(morph)) if t < 1. => {
                    let eased = ease(t);
                    for (index, letter) in state.letters.iter().enumerate() {
                        let x = state.letter_x(index, eased);
                        if morph.from[index].is_some() {
                            paint(window, cx, &letter.text, x, px(0.), 1.);
                        } else {
                            // Arrivals start as the leavers fade, so the
                            // text is never empty, then rise in.
                            let alpha = ((t - 0.1) / 0.45).clamp(0., 1.);
                            paint(window, cx, &letter.text, x, RISE * (1. - alpha), alpha);
                        }
                    }
                    for letter in &morph.leaving {
                        let alpha = 1. - (t / 0.25).clamp(0., 1.);
                        paint(
                            window,
                            cx,
                            &letter.text,
                            letter.x,
                            -RISE * (1. - alpha),
                            alpha,
                        );
                    }
                    (true, state)
                }
                _ if shine.is_some() => {
                    // The resting line's one layout, each glyph painted in
                    // its own tint. A text run per colour would split the
                    // shaping wherever the colour changes, so letters lose
                    // their kerning, and shift, as the band passes.
                    let layout = window.text_system().layout_line(
                        &state.text,
                        font_size,
                        &[style.to_run(state.text.len())],
                        None,
                    );
                    let padding_top = (line_height - layout.ascent - layout.descent) / 2.;
                    let baseline = point(bounds.left(), bounds.top() + padding_top + layout.ascent);
                    for run in &layout.runs {
                        for glyph in &run.glyphs {
                            let origin = baseline + glyph.position;
                            if glyph.is_emoji {
                                window
                                    .paint_emoji(origin, run.font_id, glyph.id, font_size)
                                    .ok();
                            } else {
                                window
                                    .paint_glyph(
                                        origin,
                                        run.font_id,
                                        glyph.id,
                                        font_size,
                                        tint(glyph.position.x),
                                    )
                                    .ok();
                            }
                        }
                    }
                    (false, state)
                }
                _ => {
                    paint(window, cx, &state.text, px(0.), px(0.), 1.);
                    (false, state)
                }
            }
        });
        if animating {
            window.request_animation_frame();
        } else if shine.is_some() {
            crate::ui::motion::schedule_loop_frame(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn letters(text: &str) -> Vec<Letter> {
        text.chars()
            .map(|letter| Letter {
                text: SharedString::from(letter.to_string()),
                x: px(0.),
            })
            .collect()
    }

    #[test]
    fn shared_letters_pair_up_in_order() {
        // "igh" carries over; the capital H of "High" is not the "h" of "high".
        let pairs = common_letters(&letters("High"), &letters("Extra high"));
        let shared: String = pairs
            .iter()
            .map(|(old, _)| "High".chars().nth(*old).unwrap())
            .collect();
        assert_eq!(shared, "igh");
        assert!(
            pairs
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0 && pair[0].1 < pair[1].1)
        );
        assert!(common_letters(&letters("Low"), &letters("Off")).is_empty());
    }

    #[test]
    fn a_resting_band_leaves_the_line_and_returns_without_a_snap() {
        let band = ShineBand {
            reach: 0.55,
            rest: 0.3,
        };
        // Off the whole line from the end of the pass through the rest,
        // and at the start of the next pass, so the loop point is dark.
        for at in [0., 0.5, 1.] {
            assert_eq!(shine_level(at, 0., band), 0.);
            assert_eq!(shine_level(at, 0.7, band), 0.);
            assert_eq!(shine_level(at, 0.85, band), 0.);
            assert!(shine_level(at, 0.6999, band) < 0.01);
        }
        // Halfway through the pass the band sits on the middle of the line.
        assert!((shine_level(0.5, 0.35, band) - 1.).abs() < 1e-4);
        // Without a rest the thinking line's band never pauses.
        assert!(shine_level(1., 0.95, ShineBand::default()) > 0.);
    }
}
