//! A thick stepped slider for a model's thinking levels.
//!
//! Models offer different sets of levels (three for one, five for another),
//! so the stops are spread evenly across the track whatever their number.
//! What stays the same across models is each level's heat: a global score
//! from Off to Max that warms the fill, the glow and the label, so High
//! looks like High on every model even when it sits at a different stop.
//!
//! The thumb rides a spring: it follows the pointer while dragged, then
//! settles on the nearest stop with a little overshoot. When the stops
//! change (another model), the dots glide from where they were and the
//! thumb follows to its new stop. The element draws only while something
//! moves; at rest it asks for no frames. Reduced motion jumps instead.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    App, BorderStyle, Bounds, BoxShadow, Corners, CursorStyle, DispatchPhase, Element, ElementId,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Style, Window, linear_color_stop, linear_gradient, point,
    px, quad, size,
};

use super::theme;

/// Height of the track; the thumb sits inside its rounded ends.
const TRACK_HEIGHT: Pixels = px(36.);
/// The thumb at rest, a little larger than the track so it reads as a knob.
const THUMB: Pixels = px(40.);
/// The thumb while held.
const THUMB_PRESSED: Pixels = px(44.);
const DOT: Pixels = px(5.);
/// The recommended stop's ring.
const RING: Pixels = px(8.);
/// How long the dots take to glide to a new set of stops.
const MORPH: Duration = Duration::from_millis(320);
/// Spring stiffness and damping ratio of the thumb: brisk, with a small
/// overshoot that reads as the thumb clicking into its stop.
const STIFFNESS: f32 = 520.;
const DAMPING_RATIO: f32 = 0.72;

/// What the owner hears from the slider: the stop under a drag as it
/// changes (`None` when the drag ends), and the stop picked by a release.
type OnPreview = Rc<dyn Fn(&Option<usize>, &mut Window, &mut App)>;
type OnCommit = Rc<dyn Fn(&usize, &mut Window, &mut App)>;

/// A slider over `heats.len()` stops, each with its level's heat in
/// `[0, 1]`. Heats double as the stops' identities when they change.
pub struct EffortSlider {
    id: ElementId,
    state: Rc<RefCell<EffortSliderState>>,
    heats: Vec<f32>,
    selected: usize,
    recommended: Option<usize>,
    enabled: bool,
    on_preview: Option<OnPreview>,
    on_commit: Option<OnCommit>,
}

/// Build a slider. `selected` and `recommended` index into `heats`. The
/// owner keeps `state` across frames and across the slider being hidden,
/// so stops that changed meanwhile glide into place when it shows again.
pub fn effort_slider(
    id: impl Into<ElementId>,
    state: Rc<RefCell<EffortSliderState>>,
    heats: Vec<f32>,
    selected: usize,
) -> EffortSlider {
    EffortSlider {
        id: id.into(),
        state,
        selected: selected.min(heats.len().saturating_sub(1)),
        heats,
        recommended: None,
        enabled: true,
        on_preview: None,
        on_commit: None,
    }
}

impl EffortSlider {
    /// Mark the stop a reset returns to.
    pub fn recommended(mut self, stop: Option<usize>) -> Self {
        self.recommended = stop;
        self
    }

    /// A disabled slider draws its stops but takes no input.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn on_preview(
        mut self,
        f: impl Fn(&Option<usize>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_preview = Some(Rc::new(f));
        self
    }

    pub fn on_commit(mut self, f: impl Fn(&usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_commit = Some(Rc::new(f));
        self
    }
}

/// Where stop `index` of `count` sits along the track, in `[0, 1]`.
pub fn stop_position(index: usize, count: usize) -> f32 {
    if count <= 1 {
        0.5
    } else {
        index as f32 / (count - 1) as f32
    }
}

/// The stop nearest to `position` along the track.
pub fn nearest_stop(position: f32, count: usize) -> usize {
    if count <= 1 {
        return 0;
    }
    let last = (count - 1) as f32;
    (position.clamp(0., 1.) * last).round() as usize
}

/// The colour of `heat`: Maple's coral warming to its deepest shade at
/// full heat. Zero heat (thinking off) has no colour of its own.
pub fn heat_color(heat: f32) -> Hsla {
    if heat <= 0. {
        return gpui::rgb(theme::text_muted()).into();
    }
    mix(theme::send_top(), theme::send_bottom(), heat)
}

fn mix(from: u32, to: u32, amount: f32) -> Hsla {
    let from = gpui::rgb(from);
    let to = gpui::rgb(to);
    let amount = amount.clamp(0., 1.);
    gpui::Rgba {
        r: from.r + (to.r - from.r) * amount,
        g: from.g + (to.g - from.g) * amount,
        b: from.b + (to.b - from.b) * amount,
        a: 1.,
    }
    .into()
}

fn ease_out_cubic(t: f32) -> f32 {
    1. - (1. - t.clamp(0., 1.)).powi(3)
}

/// The dots of the previous stops while they glide to new ones.
struct Morph {
    from: Vec<(f32, f32)>,
    started: Instant,
}

/// The slider's motion and pointer, kept by its owner.
#[derive(Default)]
pub struct EffortSliderState {
    /// The thumb's position in `[0, 1]` and its velocity per second.
    position: f32,
    velocity: f32,
    /// How far the thumb has grown toward its held size.
    press: f32,
    /// The pointer along the track while it hovers or drags.
    hover: Option<f32>,
    drag: Option<f32>,
    /// The stop last reported to `on_preview`.
    preview: Option<usize>,
    heats: Vec<f32>,
    morph: Option<Morph>,
    last_frame: Option<Instant>,
    started: bool,
}

impl EffortSliderState {
    /// Forget a drag or hover the slider never saw end, as when its popup
    /// closed under the pointer.
    pub fn release(&mut self) {
        self.drag = None;
        self.hover = None;
        self.preview = None;
    }

    /// Advance the motion to `now` toward `target`. Returns whether
    /// anything still moves.
    fn step(&mut self, target: f32, now: Instant, reduce_motion: bool) -> bool {
        let dt = self
            .last_frame
            .map_or(0., |last| now.saturating_duration_since(last).as_secs_f32())
            .min(1. / 30.);
        self.last_frame = Some(now);
        let held = self.drag.is_some();
        if reduce_motion || held || !self.started {
            self.position = target;
            self.velocity = 0.;
            self.started = true;
        } else {
            // Semi-implicit Euler in a few substeps stays stable at any
            // frame rate the display gives us.
            let damping = 2. * STIFFNESS.sqrt() * DAMPING_RATIO;
            let steps = 4;
            let h = dt / steps as f32;
            for _ in 0..steps {
                let force = STIFFNESS * (target - self.position) - damping * self.velocity;
                self.velocity += force * h;
                self.position += self.velocity * h;
            }
            if (target - self.position).abs() < 0.0005 && self.velocity.abs() < 0.005 {
                self.position = target;
                self.velocity = 0.;
            }
        }
        let press_target = if held { 1. } else { 0. };
        if reduce_motion {
            self.press = press_target;
        } else {
            let rate = 1. - (-18. * dt).exp();
            self.press += (press_target - self.press) * rate;
            if (press_target - self.press).abs() < 0.01 {
                self.press = press_target;
            }
        }
        let morphing = self
            .morph
            .as_ref()
            .is_some_and(|morph| now.saturating_duration_since(morph.started) < MORPH);
        if !morphing {
            self.morph = None;
        }
        self.position != target || self.velocity != 0. || self.press != press_target || morphing
    }
}

pub struct Prepaint {
    hitbox: Hitbox,
    track: Bounds<Pixels>,
}

impl IntoElement for EffortSlider {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EffortSlider {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, ()) {
        let style = Style {
            size: size(gpui::relative(1.).into(), THUMB_PRESSED.into()),
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let track = Bounds::new(
            point(bounds.left(), bounds.center().y - TRACK_HEIGHT / 2.),
            size(bounds.size.width, TRACK_HEIGHT),
        );
        let count = self.heats.len();
        let now = Instant::now();
        let moving = {
            let mut state = self.state.borrow_mut();
            if state.heats != self.heats {
                // Another model: the old dots glide to the new stops.
                if state.started && !state.heats.is_empty() && !cx.reduce_motion() {
                    let from = state
                        .heats
                        .iter()
                        .enumerate()
                        .map(|(index, heat)| (*heat, stop_position(index, state.heats.len())))
                        .collect();
                    state.morph = Some(Morph { from, started: now });
                }
                state.heats = self.heats.clone();
                state.preview = None;
            }
            if !self.enabled {
                state.drag = None;
            }
            let target = state
                .drag
                .unwrap_or_else(|| stop_position(self.selected, count));
            state.step(target, now, cx.reduce_motion())
        };
        if moving {
            window.request_animation_frame();
        }
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        Prepaint { hitbox, track }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let track = prepaint.track;
        let state = self.state.clone();
        let hitbox = prepaint.hitbox.clone();
        let count = self.heats.len();
        let radius = TRACK_HEIGHT / 2.;
        // The thumb's centre travels between the centres of the two ends.
        let travel = (track.size.width - TRACK_HEIGHT).max(px(0.));
        let x_at = move |position: f32| track.left() + radius + travel * position;
        let position_at = move |x: Pixels| {
            if travel <= px(0.) {
                0.5
            } else {
                ((x - track.left() - radius) / travel).clamp(0., 1.)
            }
        };

        let (position, press, hover, morph) = {
            let state = state.borrow();
            let morph = state.morph.as_ref().map(|morph| {
                let progress =
                    ease_out_cubic(morph.started.elapsed().as_secs_f32() / MORPH.as_secs_f32());
                (morph.from.clone(), progress)
            });
            (state.position, state.press, state.hover, morph)
        };
        let heat = self.heats.get(self.selected).copied().unwrap_or(0.);
        // While dragging, the colour follows the stop under the pointer.
        let live_heat = self
            .heats
            .get(nearest_stop(position, count))
            .copied()
            .unwrap_or(heat);
        let light = theme::is_light();

        // Track: a recessed groove.
        let full = Corners::all(radius);
        window.paint_quad(quad(
            track,
            full,
            gpui::rgb(theme::bg_sidebar_pill()),
            px(0.),
            gpui::transparent_black(),
            BorderStyle::Solid,
        ));
        window.paint_inset_shadows(
            track,
            full,
            &[BoxShadow {
                color: gpui::hsla(0., 0., 0., if light { 0.10 } else { 0.28 }),
                offset: point(px(0.), px(1.)),
                blur_radius: px(3.),
                spread_radius: px(0.),
                inset: true,
            }],
        );

        // Fill: from the left end to under the thumb, warming with heat,
        // with a glow that grows with it.
        let thumb_x = x_at(position);
        let fill = Bounds::new(
            track.origin,
            size(
                (thumb_x - track.left() + radius).max(TRACK_HEIGHT),
                TRACK_HEIGHT,
            ),
        );
        let fill_end = if self.enabled {
            heat_color(live_heat.max(0.15))
        } else {
            gpui::rgb(theme::text_faint()).into()
        };
        // The fill starts at the coral of the lowest heat and warms toward
        // the thumb, the same way in both themes.
        let fill_start = if self.enabled {
            gpui::rgb(theme::send_top()).into()
        } else {
            fill_end
        };
        if self.enabled && live_heat > 0. {
            window.paint_drop_shadows(
                fill,
                full,
                &[BoxShadow {
                    color: fill_end.opacity(0.18 + 0.32 * live_heat),
                    offset: point(px(0.), px(2.)),
                    blur_radius: px(10. + 10. * live_heat),
                    spread_radius: px(0.),
                    inset: false,
                }],
            );
        }
        window.paint_quad(quad(
            fill,
            full,
            linear_gradient(
                90.,
                linear_color_stop(fill_start, 0.),
                linear_color_stop(fill_end, 1.),
            ),
            px(0.),
            gpui::transparent_black(),
            BorderStyle::Solid,
        ));
        // A highlight along the fill's top edge, as on the send button.
        window.paint_inset_shadows(
            fill,
            full,
            &[BoxShadow {
                color: gpui::hsla(0., 0., 1., 0.22),
                offset: point(px(0.), px(1.)),
                blur_radius: px(1.),
                spread_radius: px(0.),
                inset: true,
            }],
        );

        // Hover: a soft halo on the stop a click would pick.
        if self.enabled
            && let Some(hover) = hover
            && state.borrow().drag.is_none()
        {
            let stop = nearest_stop(hover, count);
            if stop != self.selected {
                let center = point(x_at(stop_position(stop, count)), track.center().y);
                let halo = TRACK_HEIGHT - px(10.);
                window.paint_quad(quad(
                    Bounds::centered_at(center, size(halo, halo)),
                    Corners::all(halo / 2.),
                    if light {
                        gpui::hsla(0., 0., 0., 0.06)
                    } else {
                        gpui::hsla(0., 0., 1., 0.07)
                    },
                    px(0.),
                    gpui::transparent_black(),
                    BorderStyle::Solid,
                ));
            }
        }

        // Dots, gliding from the previous stops while a morph runs.
        let dot = |window: &mut Window, position: f32, alpha: f32, ring: bool| {
            let x = x_at(position);
            let filled = x <= thumb_x;
            let color: Hsla = if filled {
                gpui::rgb(theme::on_accent()).into()
            } else {
                gpui::rgb(theme::text_muted()).into()
            };
            let color = color.opacity(alpha * if filled { 0.75 } else { 0.6 });
            let center = point(x, track.center().y);
            if ring {
                window.paint_quad(quad(
                    Bounds::centered_at(center, size(RING, RING)),
                    Corners::all(RING / 2.),
                    gpui::transparent_black(),
                    px(1.5),
                    color,
                    BorderStyle::Solid,
                ));
            } else {
                window.paint_quad(quad(
                    Bounds::centered_at(center, size(DOT, DOT)),
                    Corners::all(DOT / 2.),
                    color,
                    px(0.),
                    gpui::transparent_black(),
                    BorderStyle::Solid,
                ));
            }
        };
        for (index, heat) in self.heats.iter().enumerate() {
            let target = stop_position(index, count);
            let ring = self.recommended == Some(index);
            match &morph {
                Some((from, progress)) => match from.iter().find(|(old, _)| old == heat) {
                    Some((_, start)) => dot(window, start + (target - start) * progress, 1., ring),
                    None => dot(window, target, *progress, ring),
                },
                None => dot(window, target, 1., ring),
            }
        }
        if let Some((from, progress)) = &morph {
            for (heat, start) in from {
                if !self.heats.contains(heat) {
                    dot(window, *start, 1. - progress, false);
                }
            }
        }

        // Thumb: a white knob that lifts while held.
        let diameter = THUMB + (THUMB_PRESSED - THUMB) * press;
        let thumb = Bounds::centered_at(point(thumb_x, track.center().y), size(diameter, diameter));
        let round = Corners::all(diameter / 2.);
        let mut shadows = vec![
            BoxShadow {
                color: gpui::hsla(0., 0., 0., if light { 0.18 } else { 0.35 }),
                offset: point(px(0.), px(1.)),
                blur_radius: px(2.),
                spread_radius: px(0.),
                inset: false,
            },
            BoxShadow {
                color: gpui::hsla(0., 0., 0., (if light { 0.12 } else { 0.30 }) + 0.1 * press),
                offset: point(px(0.), px(3.) + px(3.) * press),
                blur_radius: px(10.) + px(8.) * press,
                spread_radius: px(0.),
                inset: false,
            },
        ];
        if self.enabled && press > 0. {
            shadows.push(BoxShadow {
                color: fill_end.opacity(0.35 * press),
                offset: point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(5.) * press,
                inset: false,
            });
        }
        window.paint_drop_shadows(thumb, round, &shadows);
        window.paint_quad(quad(
            thumb,
            round,
            gpui::rgb(if self.enabled {
                theme::on_accent()
            } else {
                theme::text_muted()
            }),
            px(0.),
            gpui::transparent_black(),
            BorderStyle::Solid,
        ));

        if !self.enabled || count == 0 {
            return;
        }
        let dragging = state.borrow().drag.is_some();
        window.set_cursor_style(
            if dragging {
                CursorStyle::ClosedHand
            } else {
                CursorStyle::PointingHand
            },
            &hitbox,
        );

        let view = window.current_view();
        let on_preview = self.on_preview.clone();
        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let state = state.clone();
            let on_preview = on_preview.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                let position = position_at(event.position.x);
                let stop = nearest_stop(position, count);
                {
                    let mut state = state.borrow_mut();
                    state.drag = Some(position);
                    state.preview = Some(stop);
                }
                window.capture_pointer(hitbox.id);
                cx.stop_propagation();
                if let Some(on_preview) = &on_preview {
                    on_preview(&Some(stop), window, cx);
                }
                cx.notify(view);
            }
        });
        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let state = state.clone();
            let on_preview = on_preview.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let position = position_at(event.position.x);
                let changed_stop = {
                    let mut state = state.borrow_mut();
                    if state.drag.is_some() {
                        state.drag = Some(position);
                        let stop = nearest_stop(position, count);
                        let changed = state.preview != Some(stop);
                        state.preview = Some(stop);
                        changed.then_some(stop)
                    } else {
                        let hover = hitbox.is_hovered(window).then_some(position);
                        let moved = hover.map(|hover| nearest_stop(hover, count))
                            != state.hover.map(|hover| nearest_stop(hover, count));
                        state.hover = hover;
                        if !moved {
                            return;
                        }
                        None
                    }
                };
                if let (Some(stop), Some(on_preview)) = (changed_stop, &on_preview) {
                    on_preview(&Some(stop), window, cx);
                }
                cx.notify(view);
            }
        });
        let on_commit = self.on_commit.clone();
        window.on_mouse_event({
            let state = state.clone();
            move |event: &MouseUpEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                    return;
                }
                let Some(position) = state.borrow_mut().drag.take() else {
                    return;
                };
                state.borrow_mut().preview = None;
                window.release_pointer();
                let stop = nearest_stop(position, count);
                if let Some(on_commit) = &on_commit {
                    on_commit(&stop, window, cx);
                }
                if let Some(on_preview) = &on_preview {
                    on_preview(&None, window, cx);
                }
                cx.notify(view);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_spread_evenly_whatever_their_number() {
        assert_eq!(stop_position(0, 3), 0.);
        assert_eq!(stop_position(1, 3), 0.5);
        assert_eq!(stop_position(2, 3), 1.);
        assert_eq!(stop_position(3, 5), 0.75);
        // A single stop sits in the middle rather than at an end.
        assert_eq!(stop_position(0, 1), 0.5);
    }

    #[test]
    fn a_position_picks_the_nearest_stop_and_clamps_past_the_ends() {
        assert_eq!(nearest_stop(0.24, 3), 0);
        assert_eq!(nearest_stop(0.26, 3), 1);
        assert_eq!(nearest_stop(0.74, 3), 1);
        assert_eq!(nearest_stop(0.76, 3), 2);
        assert_eq!(nearest_stop(-1., 5), 0);
        assert_eq!(nearest_stop(2., 5), 4);
        assert_eq!(nearest_stop(0.9, 1), 0);
    }

    #[test]
    fn the_thumb_springs_to_its_stop_and_comes_to_rest() {
        let mut state = EffortSliderState {
            started: true,
            ..EffortSliderState::default()
        };
        let start = Instant::now();
        state.last_frame = Some(start);
        let mut now = start;
        let mut overshot = false;
        let mut moving = true;
        for _ in 0..240 {
            now += Duration::from_millis(16);
            moving = state.step(1., now, false);
            overshot |= state.position > 1.;
            if !moving {
                break;
            }
        }
        assert!(!moving, "the spring settles");
        assert_eq!(state.position, 1.);
        assert!(overshot, "a small overshoot reads as the thumb clicking in");
    }

    #[test]
    fn reduced_motion_jumps_to_the_stop() {
        let mut state = EffortSliderState {
            started: true,
            ..EffortSliderState::default()
        };
        assert!(!state.step(0.75, Instant::now(), true));
        assert_eq!(state.position, 0.75);
    }
}
