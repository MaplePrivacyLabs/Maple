//! A thick stepped slider for a model's thinking levels, with a liquid feel.
//!
//! Models offer different sets of levels (three for one, five for another),
//! so the stops are spread evenly across the track whatever their number.
//! What stays the same across models is each level's heat: a global score
//! from Off to Max that warms the fill, the glow and the label, so High
//! looks like High on every model even when it sits at a different stop.
//!
//! The motion is a few springs:
//!
//! - The thumb follows the pointer closely while held, drawn toward a stop
//!   as it passes near one, and on release settles on the nearest stop
//!   with a jelly overshoot. It stretches along its motion and squashes
//!   across it, so a fast sweep reads as a drop of something thick.
//! - The fill's edge is its own, softer spring chasing the thumb. When the
//!   thumb runs ahead, a thinning neck of fill stretches between them. The
//!   fill never passes the thumb: no orange shows beyond the white.
//! - Dots swell as the fill flows over them, a ripple marks each stop a
//!   drag crosses, and when a level lands (a release, a click, reset) a
//!   stronger ripple spreads and the thumb blooms in the level's colour.
//!   A step from the keyboard moves the thumb but plays none of these.
//! - When its panel opens the thumb settles in from slightly large, with
//!   the fill already at it: the panel opens often, to switch models, so
//!   the opening stays quiet. When the stops change (another model), the
//!   dots glide to their places.
//! - At the model's top stop the thumb keeps a warm halo, which rises in
//!   when the thumb arrives there and fades, quicker, when it leaves. The
//!   owner hears of the arrival ([`EffortSliderState::topped`]) to play
//!   its own part.
//!
//! The element asks for frames only while something moves. Reduced motion
//! drops the springs and effects and jumps instead.

use std::cell::RefCell;
use std::f32::consts::PI;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    App, BorderStyle, Bounds, BoxShadow, Corners, CursorStyle, DispatchPhase, Element, ElementId,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Style, Window, linear_color_stop, linear_gradient, point,
    px, quad, size,
};

use super::theme;

/// Height of the track; the thumb sits inside its rounded ends. Kept low
/// so the slider sits under the model list without pulling focus from it.
pub(crate) const TRACK_HEIGHT: Pixels = px(24.);
/// The thumb at rest, a little larger than the track so it reads as a knob.
const THUMB: Pixels = px(27.);
/// The thumb while held.
const THUMB_PRESSED: Pixels = px(30.);
/// The slider's height: room for the held thumb.
pub(crate) const SLIDER_HEIGHT: Pixels = THUMB_PRESSED;
const DOT: Pixels = px(4.);
/// How long the dots take to glide to a new set of stops.
const MORPH: Duration = Duration::from_millis(320);
const RIPPLE: Duration = Duration::from_millis(560);
const DOT_SWELL: Duration = Duration::from_millis(340);
const BLOOM: Duration = Duration::from_millis(460);

/// Spring tunings, as stiffness and damping ratio.
#[derive(Clone, Copy)]
struct Tuning(f32, f32);

/// Settling on a stop: brisk, with a jelly overshoot.
const SNAP: Tuning = Tuning(520., 0.6);
/// Under the pointer: close enough to feel direct, soft enough to be smooth.
const FOLLOW: Tuning = Tuning(2200., 0.86);
/// The fill's edge: trails a thumb that runs ahead, without overshoot, and
/// never passes it.
const POUR: Tuning = Tuning(220., 0.9);
/// The thumb lifting under the pointer.
const LIFT: Tuning = Tuning(600., 0.7);
/// The thumb settling as the panel opens: from a little large down to
/// its size, without a visible bounce, never so small that the track
/// shows around it.
const POP: Tuning = Tuning(420., 0.75);
/// Where that settle starts.
const POP_FROM: f32 = 1.05;
/// The thumb counts as arrived at the top stop this close to the end.
const ARRIVED: f32 = 0.985;
/// The halo the thumb keeps at the top stop takes this long to come up,
/// and this long to go when the thumb leaves: an exit quieter than the
/// entrance.
const HOT_IN: Duration = Duration::from_millis(300);
const HOT_OUT: Duration = Duration::from_millis(160);

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
    enabled: bool,
    on_preview: Option<OnPreview>,
    on_commit: Option<OnCommit>,
}

/// Build a slider. `selected` indexes into `heats`. The
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
        enabled: true,
        on_preview: None,
        on_commit: None,
    }
}

impl EffortSlider {
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

/// `position` drawn toward the nearest stop when it passes close by, so a
/// drag feels each stop as a soft notch. Away from the stops it is left
/// alone, and the pull fades to nothing at the edge of a notch.
fn notched(position: f32, count: usize) -> f32 {
    if count < 2 {
        return position;
    }
    let stop = stop_position(nearest_stop(position, count), count);
    let offset = position - stop;
    let zone = 0.32 / (count - 1) as f32;
    if offset.abs() >= zone {
        return position;
    }
    let pull = (1. - offset.abs() / zone).powi(2) * 0.55;
    position - offset * pull
}

/// The heat along the track at `position`, blended between the two stops
/// around it so colour flows rather than steps.
fn heat_at(heats: &[f32], position: f32) -> f32 {
    match heats.len() {
        0 => 0.,
        1 => heats[0],
        count => {
            let scaled = position.clamp(0., 1.) * (count - 1) as f32;
            let low = (scaled.floor() as usize).min(count - 2);
            let amount = scaled - low as f32;
            heats[low] + (heats[low + 1] - heats[low]) * amount
        }
    }
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

/// 0 at both ends of `[0, 1]`, 1 in the middle: a swell that returns.
fn swell(t: f32) -> f32 {
    (PI * t.clamp(0., 1.)).sin()
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    t * t * (3. - 2. * t)
}

fn elapsed(since: Instant, now: Instant, over: Duration) -> f32 {
    now.saturating_duration_since(since).as_secs_f32() / over.as_secs_f32()
}

#[derive(Clone, Copy, Default)]
struct Spring {
    value: f32,
    velocity: f32,
}

impl Spring {
    /// Advance toward `target` over `dt` seconds. Returns whether it still
    /// moves; close enough, it settles exactly on the target.
    fn step(&mut self, target: f32, Tuning(stiffness, ratio): Tuning, dt: f32) -> bool {
        // Semi-implicit Euler in a few substeps stays stable at any frame
        // rate the display gives us.
        let damping = 2. * stiffness.sqrt() * ratio;
        let steps = 4;
        let h = dt / steps as f32;
        for _ in 0..steps {
            let force = stiffness * (target - self.value) - damping * self.velocity;
            self.velocity += force * h;
            self.value += self.velocity * h;
        }
        if (target - self.value).abs() < 0.0005 && self.velocity.abs() < 0.005 {
            self.snap(target);
            false
        } else {
            true
        }
    }

    fn snap(&mut self, target: f32) {
        self.value = target;
        self.velocity = 0.;
    }
}

/// The dots of the previous stops while they glide to new ones.
struct Morph {
    from: Vec<(f32, f32)>,
    started: Instant,
}

/// A ring spreading from a stop.
struct Ripple {
    position: f32,
    strength: f32,
    started: Instant,
}

/// The slider's motion and pointer, kept by its owner.
#[derive(Default)]
pub struct EffortSliderState {
    /// The thumb's position along the track, in `[0, 1]` give or take an
    /// overshoot.
    thumb: Spring,
    /// The fill's leading edge, chasing the thumb.
    fill: Spring,
    /// How far the thumb has lifted: a little under the pointer, fully
    /// while held.
    lift: Spring,
    /// The thumb's entrance scale.
    pop: Spring,
    /// The pointer along the track while it hovers or drags.
    hover: Option<f32>,
    drag: Option<f32>,
    /// The stop last reported to `on_preview`.
    preview: Option<usize>,
    /// The stop last drawn as selected, to notice a level landing.
    selected: Option<usize>,
    heats: Vec<f32>,
    morph: Option<Morph>,
    ripples: Vec<Ripple>,
    swells: Vec<Option<Instant>>,
    bloom: Option<Instant>,
    last_frame: Option<Instant>,
    started: bool,
    /// Set while the selected stop is below the top, so arriving there
    /// starts the top's moment once; an opening at the top never does.
    armed: bool,
    /// When the thumb last arrived at the top stop, and how many times it
    /// has, so the owner can key its part of the moment.
    topped: Option<Instant>,
    topped_count: usize,
    /// Whether the selected stop was the top one last frame, and when the
    /// thumb last left it, for the halo's fade.
    was_top: bool,
    left_top: Option<Instant>,
    /// Set by a fresh showing: put the thumb and the fill straight at the
    /// selected stop on the next frame.
    settle: bool,
}

impl EffortSliderState {
    /// Start a fresh showing: forget a drag or hover the slider never saw
    /// end (its popup closed under the pointer), and settle the thumb in
    /// with the fill already at it.
    pub fn release(&mut self) {
        self.drag = None;
        self.hover = None;
        self.preview = None;
        self.pop.snap(POP_FROM);
        self.settle = true;
        self.ripples.clear();
        self.bloom = None;
        self.armed = false;
        self.topped = None;
        self.was_top = false;
        self.left_top = None;
    }

    /// The thumb's last arrival at the top stop since this showing began:
    /// how many arrivals there have been, and when this one was.
    pub fn topped(&self) -> Option<(usize, Instant)> {
        self.topped.map(|at| (self.topped_count, at))
    }

    /// Watch for the thumb arriving at the top stop (`top`: the selected
    /// stop is the last). Arriving from below starts the top's moment,
    /// once, unless `quiet` (reduced motion, or a step from the keyboard):
    /// then the halo shows without it. Leaving the top starts the halo's
    /// fade.
    fn note_top(&mut self, top: bool, now: Instant, quiet: bool) {
        if !top {
            self.armed = true;
            if self.was_top && !quiet {
                self.left_top = Some(now);
            }
        } else {
            self.left_top = None;
            if self.armed && (quiet || self.thumb.value >= ARRIVED) {
                self.armed = false;
                if !quiet {
                    self.topped = Some(now);
                    self.topped_count += 1;
                }
            }
        }
        self.was_top = top;
    }

    /// The warm halo at the top stop, from 0 to 1: full when it was there
    /// already, rising after an arrival, fading after the thumb leaves,
    /// none while the thumb is on its way.
    fn hot(&self, top: bool, now: Instant) -> f32 {
        if !top || self.armed {
            return self
                .left_top
                .map_or(0., |at| 1. - ease_out_cubic(elapsed(at, now, HOT_OUT)));
        }
        self.topped
            .map_or(1., |at| ease_out_cubic(elapsed(at, now, HOT_IN)))
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
        let lift = if held {
            1.
        } else if self.hover.is_some() {
            0.35
        } else {
            0.
        };
        if !self.started {
            self.thumb.snap(target);
            // Shown without a release (no panel opening): no settle from
            // nothing, which would bare the fill around a tiny thumb.
            if self.pop.value < 1. {
                self.pop.snap(1.);
            }
            self.started = true;
        }
        if std::mem::take(&mut self.settle) {
            self.thumb.snap(target);
            self.fill.snap(target);
        }
        if reduce_motion {
            self.thumb.snap(target);
            self.fill.snap(target);
            self.lift.snap(lift);
            self.pop.snap(1.);
            self.morph = None;
            self.ripples.clear();
            self.swells.iter_mut().for_each(|swell| *swell = None);
            self.bloom = None;
            return false;
        }
        let mut moving = self
            .thumb
            .step(target, if held { FOLLOW } else { SNAP }, dt);
        let before = self.fill.value;
        moving |= self.fill.step(self.thumb.value, POUR, dt);
        // The fill only ever trails: a thumb running back takes the fill's
        // edge with it rather than leaving orange beyond the white.
        if self.fill.value > self.thumb.value {
            self.fill.value = self.thumb.value;
            self.fill.velocity = self.fill.velocity.min(self.thumb.velocity);
        }
        // A dot swells as the fill's edge flows over it.
        let count = self.swells.len();
        let after = self.fill.value;
        for (index, swell) in self.swells.iter_mut().enumerate() {
            let at = stop_position(index, count);
            if (before - at) * (after - at) < 0. {
                *swell = Some(now);
            }
        }
        moving |= self.lift.step(lift, LIFT, dt);
        moving |= self.pop.step(1., POP, dt);
        self.ripples
            .retain(|ripple| elapsed(ripple.started, now, RIPPLE) < 1.);
        for swell in &mut self.swells {
            if swell.is_some_and(|at| elapsed(at, now, DOT_SWELL) >= 1.) {
                *swell = None;
            }
        }
        if self.bloom.is_some_and(|at| elapsed(at, now, BLOOM) >= 1.) {
            self.bloom = None;
        }
        if self
            .morph
            .as_ref()
            .is_some_and(|morph| elapsed(morph.started, now, MORPH) >= 1.)
        {
            self.morph = None;
        }
        moving
            || !self.ripples.is_empty()
            || self.swells.iter().any(Option::is_some)
            || self.bloom.is_some()
            || self.morph.is_some()
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
            size: size(gpui::relative(1.).into(), SLIDER_HEIGHT.into()),
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
        let reduce_motion = cx.reduce_motion();
        let moving = {
            let mut state = self.state.borrow_mut();
            if state.heats != self.heats {
                // Another model: the old dots glide to the new stops.
                if state.started && !state.heats.is_empty() && !reduce_motion {
                    let from = state
                        .heats
                        .iter()
                        .enumerate()
                        .map(|(index, heat)| (*heat, stop_position(index, state.heats.len())))
                        .collect();
                    state.morph = Some(Morph { from, started: now });
                }
                state.heats = self.heats.clone();
                state.swells = vec![None; count];
                state.preview = None;
                state.selected = Some(self.selected);
                // Switching models onto the top stop is not arriving there.
                state.armed = false;
            } else if state.selected != Some(self.selected) {
                // A level landed without a drag: reset, or a save from
                // elsewhere. A drag's stops ripple as it crosses them and its
                // release blooms where it lets go; a key step plays nothing.
                if state.selected.is_some()
                    && !reduce_motion
                    && state.drag.is_none()
                    && !window.last_input_was_keyboard()
                {
                    state.ripples.push(Ripple {
                        position: stop_position(self.selected, count),
                        strength: 1.,
                        started: now,
                    });
                    state.bloom = Some(now);
                }
                state.selected = Some(self.selected);
            }
            if !self.enabled {
                state.drag = None;
            }
            let target = state.drag.map_or_else(
                || stop_position(self.selected, count),
                |drag| notched(drag, count),
            );
            let moving = state.step(target, now, reduce_motion);
            state.note_top(
                self.enabled && count > 1 && self.selected + 1 == count,
                now,
                reduce_motion || window.last_input_was_keyboard(),
            );
            moving
                || state.topped.is_some_and(|at| elapsed(at, now, HOT_IN) < 1.)
                || state
                    .left_top
                    .is_some_and(|at| elapsed(at, now, HOT_OUT) < 1.)
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
        cx: &mut App,
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
        let now = Instant::now();
        let light = theme::is_light();
        let goo = self.enabled && !cx.reduce_motion();

        let snapshot = {
            let state = state.borrow();
            Snapshot {
                thumb: state.thumb,
                fill: state.fill.value,
                lift: state.lift.value.clamp(0., 1.),
                pop: state.pop.value,
                hover: state.hover,
                dragging: state.drag.is_some(),
                morph: state.morph.as_ref().map(|morph| {
                    (
                        morph.from.clone(),
                        ease_out_cubic(elapsed(morph.started, now, MORPH)),
                    )
                }),
                ripples: state
                    .ripples
                    .iter()
                    .map(|ripple| {
                        (
                            ripple.position,
                            ripple.strength,
                            elapsed(ripple.started, now, RIPPLE),
                        )
                    })
                    .collect(),
                swells: state
                    .swells
                    .iter()
                    .map(|at| at.map_or(0., |at| swell(elapsed(at, now, DOT_SWELL))))
                    .collect(),
                bloom: state.bloom.map_or(0., |at| swell(elapsed(at, now, BLOOM))),
                hot: state.hot(self.enabled && count > 1 && self.selected + 1 == count, now),
            }
        };
        let thumb_x = x_at(snapshot.thumb.value);
        // The thumb stretches along its motion and squashes across it, but
        // never thinner than the track, so the fill cannot show around it.
        let speed = if goo {
            (snapshot.thumb.velocity * f32::from(travel)).abs()
        } else {
            0.
        };
        let stretch = (speed / 1400.).min(0.42);
        let diameter = (THUMB + (THUMB_PRESSED - THUMB) * snapshot.lift) * snapshot.pop;
        let thumb = Bounds::centered_at(
            point(thumb_x, track.center().y),
            size(
                diameter * (1. + stretch),
                (diameter * (1. - 0.32 * stretch)).max(diameter.min(TRACK_HEIGHT + px(2.))),
            ),
        );
        let fill_x = if goo {
            x_at(snapshot.fill).min(thumb_x)
        } else {
            thumb_x
        };
        let near = fill_x;
        let far = thumb_x;
        // Colour flows with the thumb, blended between the stops around it.
        let heat = heat_at(&self.heats, snapshot.thumb.value);

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

        // Fill: a full-height body up to the fill's edge, and between it and
        // the thumb a neck that thins as they part.
        // Its rounded end stays under the thumb, even while the thumb pops
        // in smaller than the track.
        let body_end = (near + radius).min(thumb.right() - px(1.));
        let body = Bounds::new(
            track.origin,
            size((body_end - track.left()).max(TRACK_HEIGHT), TRACK_HEIGHT),
        );
        // The fill keeps at least the lowest heat's coral, so even Off shows
        // a sliver of it under the thumb.
        const FILL_FLOOR: f32 = 0.15;
        let fill_end = if self.enabled {
            heat_color(heat.max(FILL_FLOOR))
        } else {
            gpui::rgb(theme::text_faint()).into()
        };
        // Off has no colour of its own, so the thumb's coloured glow fades
        // out as the thumb nears Off instead of glowing coral there.
        let glow = (heat / FILL_FLOOR).min(1.);
        // The fill starts at the coral of the lowest heat and warms toward
        // the thumb, the same way in both themes.
        let fill_start = if self.enabled {
            gpui::rgb(theme::send_top()).into()
        } else {
            fill_end
        };
        if self.enabled && heat > 0. {
            window.paint_drop_shadows(
                body,
                full,
                &[BoxShadow {
                    color: fill_end.opacity(0.1 + 0.18 * heat + 0.08 * snapshot.hot),
                    offset: point(px(0.), px(1.)),
                    blur_radius: px(6. + 6. * heat + 3. * snapshot.hot),
                    spread_radius: px(0.),
                    inset: false,
                }],
            );
        }
        window.paint_quad(quad(
            body,
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
        let gap = far - near;
        if goo && gap > px(0.5) {
            let reach = (travel * 0.28).max(px(1.));
            let neck = TRACK_HEIGHT * (1. - 0.6 * smoothstep(gap / reach));
            window.paint_quad(quad(
                Bounds::new(point(near, track.center().y - neck / 2.), size(gap, neck)),
                Corners::all(neck / 2.),
                fill_end,
                px(0.),
                gpui::transparent_black(),
                BorderStyle::Solid,
            ));
        }
        // A highlight along the body's top edge, as on the send button.
        window.paint_inset_shadows(
            body,
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
            && let Some(hover) = snapshot.hover
            && !snapshot.dragging
        {
            let stop = nearest_stop(hover, count);
            if stop != self.selected {
                let center = point(x_at(stop_position(stop, count)), track.center().y);
                let halo = TRACK_HEIGHT * 0.72;
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

        // Dots, swelling as the fill flows over them and gliding from the
        // previous stops while a morph runs.
        let dot = |window: &mut Window, position: f32, alpha: f32, grow: f32| {
            let x = x_at(position);
            let filled = x <= far;
            let color: Hsla = if filled {
                gpui::rgb(theme::on_accent()).into()
            } else {
                gpui::rgb(theme::text_muted()).into()
            };
            let color = color.opacity(alpha * if filled { 0.75 + 0.25 * grow } else { 0.6 });
            let center = point(x, track.center().y);
            let dot = DOT * (1. + 1.1 * grow);
            window.paint_quad(quad(
                Bounds::centered_at(center, size(dot, dot)),
                Corners::all(dot / 2.),
                color,
                px(0.),
                gpui::transparent_black(),
                BorderStyle::Solid,
            ));
        };
        for (index, stop_heat) in self.heats.iter().enumerate() {
            let target = stop_position(index, count);
            let grow = snapshot.swells.get(index).copied().unwrap_or(0.);
            match &snapshot.morph {
                Some((from, progress)) => match from.iter().find(|(old, _)| old == stop_heat) {
                    Some((_, start)) => dot(window, start + (target - start) * progress, 1., grow),
                    None => dot(window, target, *progress, grow),
                },
                None => dot(window, target, 1., grow),
            }
        }
        if let Some((from, progress)) = &snapshot.morph {
            for (stop_heat, start) in from {
                if !self.heats.contains(stop_heat) {
                    dot(window, *start, 1. - progress, 0.);
                }
            }
        }

        // Ripples: rings spreading from the stops a drag crossed and the
        // levels that landed.
        for (position, strength, t) in &snapshot.ripples {
            let x = x_at(*position);
            let eased = ease_out_cubic(*t);
            let diameter = DOT + (TRACK_HEIGHT * 1.2 - DOT) * eased;
            let color: Hsla = if x <= far {
                gpui::rgb(theme::on_accent()).into()
            } else {
                fill_end
            };
            window.paint_quad(quad(
                Bounds::centered_at(point(x, track.center().y), size(diameter, diameter)),
                Corners::all(diameter / 2.),
                gpui::transparent_black(),
                px(1.6) - px(1.2) * eased,
                color.opacity(0.45 * strength * (1. - t).powi(2)),
                BorderStyle::Solid,
            ));
        }

        // Thumb: a white knob that lifts under the pointer.
        let round = Corners::all(thumb.size.height / 2.);
        let mut shadows = vec![
            BoxShadow {
                color: gpui::hsla(0., 0., 0., if light { 0.18 } else { 0.35 }),
                offset: point(px(0.), px(1.)),
                blur_radius: px(2.),
                spread_radius: px(0.),
                inset: false,
            },
            BoxShadow {
                color: gpui::hsla(
                    0.,
                    0.,
                    0.,
                    (if light { 0.12 } else { 0.30 }) + 0.1 * snapshot.lift,
                ),
                offset: point(px(0.), px(2.) + px(2.) * snapshot.lift),
                blur_radius: px(6.) + px(5.) * snapshot.lift,
                spread_radius: px(0.),
                inset: false,
            },
        ];
        if self.enabled && snapshot.lift > 0. && glow > 0. {
            // A soft glow in the level's colour rather than an outline.
            shadows.push(BoxShadow {
                color: fill_end.opacity(0.2 * snapshot.lift * glow),
                offset: point(px(0.), px(0.)),
                blur_radius: px(8.) * snapshot.lift,
                spread_radius: px(1.) * snapshot.lift,
                inset: false,
            });
        }
        if snapshot.bloom > 0. && glow > 0. {
            shadows.push(BoxShadow {
                color: fill_end.opacity(0.25 * snapshot.bloom * glow),
                offset: point(px(0.), px(0.)),
                blur_radius: px(9.) * snapshot.bloom,
                spread_radius: px(3.) * snapshot.bloom,
                inset: false,
            });
        }
        if snapshot.hot > 0. {
            // At the top stop the thumb keeps a warm halo.
            shadows.push(BoxShadow {
                color: fill_end.opacity(0.25 * snapshot.hot),
                offset: point(px(0.), px(0.)),
                blur_radius: px(10.) * snapshot.hot,
                spread_radius: px(1.) * snapshot.hot,
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
        // A hand that opens over the thumb and closes on it.
        let over_thumb = hitbox.is_hovered(window)
            && (window.mouse_position().x - thumb_x).abs() <= thumb.size.width / 2.;
        window.set_cursor_style(
            if snapshot.dragging {
                CursorStyle::ClosedHand
            } else if over_thumb {
                CursorStyle::OpenHand
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
                        if changed && state.preview.is_some() && goo {
                            // A soft ripple for each stop the drag crosses.
                            state.ripples.push(Ripple {
                                position: stop_position(stop, count),
                                strength: 0.5,
                                started: Instant::now(),
                            });
                        }
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
                let stop = nearest_stop(position, count);
                {
                    let mut state = state.borrow_mut();
                    state.preview = None;
                    // The level lands where the pointer lets go.
                    if goo {
                        let now = Instant::now();
                        state.ripples.push(Ripple {
                            position: stop_position(stop, count),
                            strength: 1.,
                            started: now,
                        });
                        state.bloom = Some(now);
                    }
                }
                window.release_pointer();
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

/// What one frame draws, read from the state in one borrow.
struct Snapshot {
    thumb: Spring,
    fill: f32,
    lift: f32,
    pop: f32,
    hover: Option<f32>,
    dragging: bool,
    morph: Option<(Vec<(f32, f32)>, f32)>,
    ripples: Vec<(f32, f32, f32)>,
    swells: Vec<f32>,
    bloom: f32,
    /// The top stop's halo, from 0 to 1.
    hot: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(state: &mut EffortSliderState, target: f32, frames: usize) -> bool {
        let mut now = state.last_frame.unwrap_or_else(Instant::now);
        let mut moving = true;
        for _ in 0..frames {
            now += Duration::from_millis(16);
            moving = state.step(target, now, false);
            if !moving {
                break;
            }
        }
        moving
    }

    fn started() -> EffortSliderState {
        EffortSliderState {
            started: true,
            last_frame: Some(Instant::now()),
            pop: Spring {
                value: 1.,
                velocity: 0.,
            },
            ..EffortSliderState::default()
        }
    }

    #[test]
    fn stops_spread_evenly_whatever_their_number() {
        assert_eq!(stop_position(0, 3), 0.);
        assert_eq!(stop_position(1, 3), 0.5);
        assert_eq!(stop_position(2, 3), 1.);
        assert_eq!(stop_position(3, 5), 0.75);
        // A single stop sits in the middle rather than at an end.
        assert_eq!(stop_position(0, 1), 0.5);
    }

    /// The top's moment waits for the thumb to reach the end of the track,
    /// plays once per arrival, and never for an opening already at the top.
    #[test]
    fn the_top_moment_plays_on_arrival_only() {
        let now = Instant::now();
        let count = |state: &EffortSliderState| state.topped().map(|(count, _)| count);

        // Opened at the top: no moment, the halo is simply on.
        let mut state = started();
        state.release();
        state.thumb.snap(1.);
        state.note_top(true, now, false);
        assert_eq!(count(&state), None);
        assert_eq!(state.hot(true, now), 1.);

        // Down a stop and back: nothing until the thumb reaches the end.
        state.note_top(false, now, false);
        state.thumb.snap(0.9);
        state.note_top(true, now, false);
        assert_eq!(count(&state), None);
        assert_eq!(state.hot(true, now), 0.);
        state.thumb.snap(1.);
        state.note_top(true, now, false);
        assert_eq!(count(&state), Some(1));
        // Staying there does not play it again.
        state.note_top(true, now + Duration::from_millis(100), false);
        assert_eq!(count(&state), Some(1));
        // Leaving and arriving again does.
        state.note_top(false, now, false);
        state.note_top(true, now, false);
        assert_eq!(count(&state), Some(2));

        // Leaving, the halo fades out over its exit; a key step drops it at
        // once.
        let later = now + HOT_IN;
        state.note_top(false, later, false);
        assert_eq!(state.hot(false, later), 1.);
        assert_eq!(state.hot(false, later + HOT_OUT), 0.);
        state.note_top(true, later, true);
        state.note_top(false, later, true);
        assert_eq!(state.hot(false, later), 0.);

        // Reduced motion: no moment, the halo at once.
        let mut still = started();
        still.note_top(false, now, true);
        still.note_top(true, now, true);
        assert_eq!(count(&still), None);
        assert_eq!(still.hot(true, now), 1.);
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

    /// Near a stop a drag is drawn toward it, never past it; between stops
    /// it is left where the pointer is.
    #[test]
    fn a_drag_is_drawn_into_the_stops_it_passes() {
        let pulled = notched(0.45, 3);
        assert!(pulled > 0.45 && pulled < 0.5, "{pulled}");
        assert_eq!(notched(0.25, 3), 0.25);
        assert_eq!(notched(0.5, 3), 0.5);
        assert_eq!(notched(0.3, 1), 0.3);
    }

    #[test]
    fn colour_flows_between_the_stops_heats() {
        let heats = [0.3, 0.7, 1.];
        assert_eq!(heat_at(&heats, 0.), 0.3);
        assert!((heat_at(&heats, 0.25) - 0.5).abs() < 1e-6);
        assert_eq!(heat_at(&heats, 1.), 1.);
    }

    #[test]
    fn the_thumb_springs_to_its_stop_and_comes_to_rest() {
        let mut state = started();
        let mut overshot = false;
        let mut now = state.last_frame.unwrap();
        for _ in 0..240 {
            now += Duration::from_millis(16);
            state.step(1., now, false);
            overshot |= state.thumb.value > 1.;
        }
        assert!(!run(&mut state, 1., 1), "everything settles");
        assert_eq!(state.thumb.value, 1.);
        assert_eq!(state.fill.value, 1.);
        assert!(overshot, "a small overshoot reads as the thumb clicking in");
    }

    /// The fill trails a moving thumb, which is what draws the neck.
    #[test]
    fn the_fill_trails_the_thumb_and_catches_up() {
        let mut state = started();
        let mut now = state.last_frame.unwrap();
        now += Duration::from_millis(16);
        state.step(1., now, false);
        assert!(state.fill.value < state.thumb.value);
        run(&mut state, 1., 400);
        assert_eq!(state.fill.value, state.thumb.value);
    }

    /// No orange runs past the white: through the opening, a sweep
    /// back and the settle's overshoot, the fill's edge stays at or behind
    /// the thumb.
    #[test]
    fn the_fill_never_passes_the_thumb() {
        let mut state = started();
        run(&mut state, 1., 400);
        state.release();
        let mut now = state.last_frame.unwrap();
        for target in [1., 0., 0.75, 0.25] {
            for _ in 0..120 {
                now += Duration::from_millis(16);
                state.step(target, now, false);
                assert!(
                    state.fill.value <= state.thumb.value,
                    "fill {} passed thumb {} toward {target}",
                    state.fill.value,
                    state.thumb.value
                );
                // Nor around it: the settling thumb stays taller than the
                // track, whose height the fill's body fills.
                assert!(
                    THUMB * state.pop.value > TRACK_HEIGHT,
                    "thumb at {} of its size shows the fill around it",
                    state.pop.value
                );
            }
        }
    }

    #[test]
    fn reduced_motion_jumps_to_the_stop() {
        let mut state = started();
        assert!(!state.step(0.75, Instant::now(), true));
        assert_eq!(state.thumb.value, 0.75);
        assert_eq!(state.fill.value, 0.75);
    }
}
