//! Motion primitives that keep the frame rate low.
//!
//! gpui's own animation element asks for a frame on every display tick,
//! and each frame re-renders the whole window. Two patterns keep motion
//! cheap:
//!
//! - Looping effects (spinners, pulsing dots) run through [`ticker`]: a
//!   shared phase clock plus one coalesced repaint per view per interval,
//!   at a rate the eye still reads as continuous (`LOOP_FPS`). Motion that
//!   travels across the screen steps visibly at that rate and asks for
//!   more through [`Ticker::fps`].
//! - One-shot effects (a popup fading in) use [`fade_in`], which wraps
//!   gpui's oneshot `with_animation`: it requests frames only until the
//!   animation ends, then the window goes idle again.
//!
//! Nothing here animates hover or press: gpui has no style transitions,
//! and a stepped scale change on hover reads as flicker.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gpui::{
    AnimationElement, AnimationExt, AnyElement, App, Bounds, Element, ElementId, EntityId,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, Styled, Window,
};

/// Frames per second a looping effect asks for. Twelve to fifteen steps a
/// second still read as motion at a fraction of the display rate.
pub const LOOP_FPS: u32 = 15;

/// Duration of a one-shot reveal.
pub const REVEAL: Duration = Duration::from_millis(140);

/// Phase clock every looping effect reads, so they all stay in step and
/// their frame timers line up.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Position in `[0, 1)` within a cycle of `period`, from the shared clock.
pub fn phase(period: Duration) -> f32 {
    let period = period.as_millis().max(1);
    (epoch().elapsed().as_millis() % period) as f32 / period as f32
}

thread_local! {
    /// Views with a loop frame on its way. Any number of looping effects in
    /// one view cost one repaint per interval of the fastest of them.
    static FRAME_PENDING: RefCell<FrameSchedule<EntityId>> = RefCell::new(FrameSchedule::default());
}

/// Which key (a view) has a frame on its way, at what rate, and which
/// request owns it. A faster request takes the slot over; the timer of the
/// request it replaced finds a generation not its own and stays quiet, so a
/// view never runs two repaint chains, whatever order its loops ask in.
struct FrameSchedule<K> {
    pending: HashMap<K, (u32, u64)>,
    next: u64,
}

impl<K> Default for FrameSchedule<K> {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            next: 0,
        }
    }
}

impl<K: Hash + Eq> FrameSchedule<K> {
    /// The generation of a new frame request at `fps`, or `None` when one
    /// at least as fast is already on its way.
    fn request(&mut self, key: K, fps: u32) -> Option<u64> {
        if self
            .pending
            .get(&key)
            .is_some_and(|&(scheduled, _)| scheduled >= fps)
        {
            return None;
        }
        self.next += 1;
        self.pending.insert(key, (fps, self.next));
        Some(self.next)
    }

    /// A request's timer fired: whether it still owns the slot and should
    /// repaint. A replaced request's newer owner repaints instead, or
    /// already did, and that render re-armed every loop still running.
    fn fire(&mut self, key: &K, generation: u64) -> bool {
        let owns = self
            .pending
            .get(key)
            .is_some_and(|&(_, current)| current == generation);
        if owns {
            self.pending.remove(key);
        }
        owns
    }
}

/// Frames per second under thermal pressure: still reads as motion.
const THROTTLED_FPS: u32 = 6;

/// Whether looping effects should hold still: the user asked for reduced
/// motion, or the window is in the background where nobody sees them.
/// gpui's inactive-window frame cap does not cover timer-driven repaints,
/// so this gate is what keeps a backgrounded Maple idle.
pub fn loops_paused(window: &Window, cx: &App) -> bool {
    cx.reduce_motion() || !window.is_window_active()
}

/// Ask for one repaint of the view being rendered after one interval at
/// `fps`, unless a repaint at least that soon is already on its way. Does
/// nothing while loops are paused; the next activation renders the view
/// again, which re-arms the clock.
pub fn schedule_loop_frame(fps: u32, window: &mut Window, cx: &mut App) {
    if loops_paused(window, cx) {
        return;
    }
    let fps = match cx.thermal_state() {
        gpui::ThermalState::Serious | gpui::ThermalState::Critical => fps.min(THROTTLED_FPS),
        _ => fps,
    }
    .max(1);
    let view = window.current_view();
    let Some(generation) = FRAME_PENDING.with(|pending| pending.borrow_mut().request(view, fps))
    else {
        return;
    };
    let interval = Duration::from_secs(1) / fps;
    window
        .spawn(cx, async move |cx| {
            cx.background_executor().timer(interval).await;
            if FRAME_PENDING.with(|pending| pending.borrow_mut().fire(&view, generation)) {
                cx.update(|_, cx| cx.notify(view)).ok();
            }
        })
        .detach();
}

/// A looping element rebuilt at `LOOP_FPS` (or [`Ticker::fps`]) from the
/// shared clock. `build` receives the phase in `[0, 1)` of a cycle of
/// `period` and returns the element for that instant. It runs at layout
/// time, so keep it cheap.
pub fn ticker(
    id: impl Into<ElementId>,
    period: Duration,
    build: impl Fn(f32) -> AnyElement + 'static,
) -> Ticker {
    Ticker {
        id: id.into(),
        period,
        fps: LOOP_FPS,
        build: Box::new(build),
    }
}

pub struct Ticker {
    id: ElementId,
    period: Duration,
    fps: u32,
    build: Box<dyn Fn(f32) -> AnyElement>,
}

impl Ticker {
    /// Repaint at `fps` instead of `LOOP_FPS`, for motion that travels.
    /// Each frame renders the whole view, so every other loop in that view
    /// redraws at this rate too while this one runs; ask only for what the
    /// effect needs.
    pub fn fps(mut self, fps: u32) -> Self {
        self.fps = fps;
        self
    }
}

impl IntoElement for Ticker {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Ticker {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, AnyElement) {
        // Paused loops draw their rest frame and never ask for another.
        let phase = if loops_paused(window, cx) {
            0.
        } else {
            phase(self.period)
        };
        let mut element = (self.build)(phase);
        schedule_loop_frame(self.fps, window, cx);
        (element.request_layout(window, cx), element)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        element: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        element: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        element.paint(window, cx);
    }
}

/// Fade an element in over [`REVEAL`] the first time it appears. The
/// animation is one-shot: gpui stops requesting frames when it ends. `id`
/// must be stable across frames and unique among animations on screen.
pub fn fade_in<E>(element: E, id: impl Into<ElementId>) -> AnimationElement<E>
where
    E: IntoElement + Styled + 'static,
{
    element.with_animation(
        id,
        gpui::Animation::new(REVEAL).with_easing(gpui::ease_out_quint()),
        |element, delta| element.opacity(delta),
    )
}

/// Fade in and slide up a few pixels, for panels that open above their
/// anchor. Same one-shot cost profile as [`fade_in`].
pub fn rise_in<E>(element: E, id: impl Into<ElementId>) -> AnimationElement<E>
where
    E: IntoElement + Styled + 'static,
{
    element.with_animation(
        id,
        gpui::Animation::new(REVEAL).with_easing(gpui::ease_out_quint()),
        |element, delta| {
            element
                .opacity(delta)
                .mt(gpui::px(6. * (1. - delta)))
                .mb(gpui::px(-6. * (1. - delta)))
        },
    )
}

/// A sine pulse in `[low, high]` offset by `shift` cycles, for dots that
/// breathe out of step with each other.
pub fn pulse(phase: f32, shift: f32, low: f32, high: f32) -> f32 {
    let wave = ((phase + shift) * std::f32::consts::TAU).sin() * 0.5 + 0.5;
    low + (high - low) * wave
}

#[cfg(test)]
mod tests {
    use super::FrameSchedule;

    #[test]
    fn a_faster_loop_takes_over_and_the_replaced_timer_stays_quiet() {
        let mut schedule = FrameSchedule::default();
        let slow = schedule.request("view", 15).expect("nothing pending");
        let fast = schedule.request("view", 30).expect("faster replaces");
        assert_eq!(schedule.request("view", 15), None, "a slower loop waits");
        assert!(schedule.fire(&"view", fast));
        assert!(!schedule.fire(&"view", slow));
    }

    /// The trace stops while a replaced 15 fps timer is still out: the
    /// next 15 fps request must not let that timer start a second chain.
    #[test]
    fn a_replaced_timer_never_claims_a_newer_request_at_its_rate() {
        let mut schedule = FrameSchedule::default();
        let stale = schedule.request("view", 15).unwrap();
        let fast = schedule.request("view", 30).unwrap();
        assert!(schedule.fire(&"view", fast));
        let next = schedule.request("view", 15).unwrap();
        assert!(!schedule.fire(&"view", stale));
        assert!(schedule.fire(&"view", next));
    }

    #[test]
    fn views_schedule_independently() {
        let mut schedule = FrameSchedule::default();
        let a = schedule.request("a", 15).unwrap();
        let b = schedule.request("b", 15).unwrap();
        assert!(schedule.fire(&"a", a));
        assert!(schedule.fire(&"b", b));
    }
}
