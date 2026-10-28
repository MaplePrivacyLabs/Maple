//! Motion primitives that keep the frame rate low.
//!
//! gpui's own animation element asks for a frame on every display tick,
//! and each frame re-renders the whole window. Two patterns keep motion
//! cheap:
//!
//! - Looping effects (spinners, pulsing dots) run through [`ticker`]: a
//!   shared phase clock plus one coalesced repaint per view per interval,
//!   at a rate the eye still reads as continuous (`LOOP_FPS`).
//! - One-shot effects (a popup fading in) use [`fade_in`], which wraps
//!   gpui's oneshot `with_animation`: it requests frames only until the
//!   animation ends, then the window goes idle again.
//!
//! Hover plates use a short, reversible color transition. Press feedback
//! remains immediate; no control scales or changes layout on hover.

use std::cell::RefCell;
use std::collections::HashSet;
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
    /// Views with a loop frame already scheduled. Any number of looping
    /// effects in one view cost one repaint per interval.
    static FRAME_PENDING: RefCell<HashSet<EntityId>> = RefCell::new(HashSet::new());
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

/// Ask for one repaint of the view being rendered after a loop interval,
/// unless one is already on its way. Does nothing while loops are paused;
/// the next activation renders the view again, which re-arms the clock.
pub fn schedule_loop_frame(window: &mut Window, cx: &mut App) {
    if loops_paused(window, cx) {
        return;
    }
    let view = window.current_view();
    let scheduled = FRAME_PENDING.with(|pending| !pending.borrow_mut().insert(view));
    if scheduled {
        return;
    }
    let fps = match cx.thermal_state() {
        gpui::ThermalState::Serious | gpui::ThermalState::Critical => THROTTLED_FPS,
        _ => LOOP_FPS,
    };
    let interval = Duration::from_secs(1) / fps;
    window
        .spawn(cx, async move |cx| {
            cx.background_executor().timer(interval).await;
            FRAME_PENDING.with(|pending| pending.borrow_mut().remove(&view));
            cx.update(|_, cx| cx.notify(view)).ok();
        })
        .detach();
}

/// A looping element rebuilt at `LOOP_FPS` from the shared clock. `build`
/// receives the phase in `[0, 1)` of a cycle of `period` and returns the
/// element for that instant. It runs at layout time, so keep it cheap.
pub fn ticker(
    id: impl Into<ElementId>,
    period: Duration,
    build: impl Fn(f32) -> AnyElement + 'static,
) -> Ticker {
    Ticker {
        id: id.into(),
        period,
        build: Box::new(build),
    }
}

pub struct Ticker {
    id: ElementId,
    period: Duration,
    build: Box<dyn Fn(f32) -> AnyElement>,
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
        schedule_loop_frame(window, cx);
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

/// A short, symmetric transition for hover plates. Press and focus styles
/// remain native GPUI refinements, applied after this background.
pub const HOVER: Duration = Duration::from_millis(120);

#[derive(Clone, Copy)]
struct HoverTransition {
    from: f32,
    target: f32,
    started: Instant,
}

impl HoverTransition {
    fn new(now: Instant) -> Self {
        Self {
            from: 0.,
            target: 0.,
            started: now,
        }
    }

    fn value(&self, now: Instant) -> f32 {
        let t = (now.saturating_duration_since(self.started).as_secs_f32() / HOVER.as_secs_f32())
            .min(1.);
        let eased = t * t * (3. - 2. * t);
        self.from + (self.target - self.from) * eased
    }

    fn update(&mut self, hovered: bool, now: Instant, reduced: bool) -> (f32, bool) {
        let target = if hovered { 1. } else { 0. };
        if reduced {
            self.from = target;
            self.target = target;
        } else if self.target != target {
            self.from = self.value(now);
            self.target = target;
            self.started = now;
        }
        let value = self.value(now);
        (value, value != self.target)
    }
}

// Premultiplied RGB avoids a dark flash when fading from transparent.
fn hover_color(rest: gpui::Hsla, hover: gpui::Hsla, amount: f32) -> gpui::Hsla {
    if amount == 0. {
        return rest;
    }
    if amount == 1. {
        return hover;
    }
    let rest = gpui::Rgba::from(rest);
    let hover = gpui::Rgba::from(hover);
    let alpha = rest.a + (hover.a - rest.a) * amount;
    let channel = |a: f32, b: f32| {
        if alpha == 0. {
            0.
        } else {
            (a * rest.a * (1. - amount) + b * hover.a * amount) / alpha
        }
    };
    gpui::Rgba {
        r: channel(rest.r, hover.r),
        g: channel(rest.g, hover.g),
        b: channel(rest.b, hover.b),
        a: alpha,
    }
    .into()
}

/// Animate a control's background without changing its layout, identity,
/// handlers, tooltip, or accessibility node. Keep native `.hover()` for
/// non-background styles and native `.active()` for immediate press feedback.
pub fn hover_background(
    element: gpui::Stateful<gpui::Div>,
    rest: impl Into<gpui::Hsla>,
    hover: impl Into<gpui::Hsla>,
) -> HoverBackground {
    HoverBackground {
        element,
        rest: rest.into(),
        hover: hover.into(),
        highlighted: None,
        #[cfg(test)]
        observed: None,
    }
}

pub struct HoverBackground {
    element: gpui::Stateful<gpui::Div>,
    rest: gpui::Hsla,
    hover: gpui::Hsla,
    highlighted: Option<bool>,
    #[cfg(test)]
    observed: Option<std::rc::Rc<RefCell<gpui::Fill>>>,
}

impl HoverBackground {
    /// Override the resting background, including selected or open states.
    pub fn bg(mut self, color: impl Into<gpui::Hsla>) -> Self {
        self.rest = color.into();
        self.element.style().background = Some(self.rest.into());
        self
    }

    /// Menus share one highlight between pointer and keyboard navigation.
    /// Use that existing state instead of maintaining a second hover target.
    pub fn highlighted(mut self, highlighted: bool) -> Self {
        self.highlighted = Some(highlighted);
        self
    }
}

impl Styled for HoverBackground {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        self.element.style()
    }
}
impl gpui::InteractiveElement for HoverBackground {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        gpui::InteractiveElement::interactivity(&mut self.element)
    }
}
impl gpui::StatefulInteractiveElement for HoverBackground {}
impl gpui::ParentElement for HoverBackground {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        gpui::ParentElement::extend(&mut self.element, elements);
    }
}
impl IntoElement for HoverBackground {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for HoverBackground {
    type RequestLayoutState = <gpui::Stateful<gpui::Div> as Element>::RequestLayoutState;
    type PrepaintState = <gpui::Stateful<gpui::Div> as Element>::PrepaintState;
    fn id(&self) -> Option<ElementId> {
        self.element.id()
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        self.element.source_location()
    }
    fn a11y_role(&self) -> Option<gpui::accesskit::Role> {
        self.element.a11y_role()
    }
    fn write_a11y_info(&self, node: &mut gpui::accesskit::Node) {
        self.element.write_a11y_info(node);
    }
    fn a11y_synthetic_children(
        &mut self,
        state: &mut Self::PrepaintState,
        builder: &mut gpui::A11ySubtreeBuilder,
    ) {
        self.element.a11y_synthetic_children(state, builder);
    }
    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.element.request_layout(id, inspector, window, cx)
    }
    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.element
            .prepaint(id, inspector, bounds, layout, window, cx)
    }
    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let hovered = self.highlighted.unwrap_or_else(|| {
            hitbox
                .as_ref()
                .is_some_and(|hitbox| hitbox.is_hovered(window))
                && !cx.has_active_drag()
        });
        let now = Instant::now();
        let (amount, animating) = window.with_element_state(
            id.expect("hover controls have an id"),
            |state: Option<HoverTransition>, _| {
                let mut state = state.unwrap_or_else(|| HoverTransition::new(now));
                let frame = state.update(hovered, now, cx.reduce_motion());
                (frame, state)
            },
        );
        self.element.style().background = Some(hover_color(self.rest, self.hover, amount).into());
        if animating {
            window.request_animation_frame();
        }
        if self.highlighted.is_none()
            && let Some(hitbox) = hitbox.clone()
        {
            let view = window.current_view();
            window.on_mouse_event(move |_: &gpui::MouseMoveEvent, phase, window, cx| {
                if phase == gpui::DispatchPhase::Capture && hitbox.is_hovered(window) != hovered {
                    cx.notify(view);
                }
            });
        }
        self.element
            .paint(id, inspector, bounds, layout, hitbox, window, cx);
        #[cfg(test)]
        if let Some(observed) = &self.observed {
            use gpui::InteractiveElement;
            *observed.borrow_mut() = self
                .element
                .interactivity()
                .compute_style(id, hitbox.as_ref(), window, cx)
                .background
                .unwrap_or_default();
        }
    }
}

#[cfg(test)]
mod hover_tests {
    use super::*;
    #[test]
    fn hover_reverses_from_the_displayed_value_and_settles() {
        let now = Instant::now();
        let mut transition = HoverTransition::new(now);
        assert_eq!(transition.update(false, now, false), (0., false));
        assert_eq!(transition.update(true, now, false), (0., true));
        let halfway = now + HOVER / 2;
        assert_eq!(transition.update(true, halfway, false), (0.5, true));
        assert_eq!(transition.update(false, halfway, false), (0.5, true));
        assert_eq!(
            transition.update(false, halfway + HOVER, false),
            (0., false)
        );
        assert_eq!(
            transition.update(false, halfway + HOVER * 2, false),
            (0., false)
        );
    }
    #[test]
    fn reduced_motion_settles_an_interrupted_hover_immediately() {
        let now = Instant::now();
        let mut transition = HoverTransition::new(now);
        transition.update(true, now, false);
        assert_eq!(transition.update(true, now + HOVER / 4, true), (1., false));
        assert_eq!(transition.update(false, now + HOVER / 2, true), (0., false));
    }

    #[test]
    fn hover_eases_symmetrically_without_darkening_transparent_colors() {
        let now = Instant::now();
        let mut transition = HoverTransition::new(now);
        transition.update(true, now, false);
        let early = transition.value(now + HOVER / 4);
        let late = transition.value(now + HOVER * 3 / 4);
        assert!(early < 0.25 && late > 0.75);
        assert!((early + late - 1.).abs() <= 2. * f32::EPSILON);
        let white = gpui::hsla(0., 0., 1., 1.);
        let middle = gpui::Rgba::from(hover_color(gpui::transparent_black(), white, 0.5));
        assert_eq!(middle.r, 1.);
        assert_eq!(middle.g, 1.);
        assert_eq!(middle.b, 1.);
        assert_eq!(middle.a, 0.5);
    }

    #[gpui::test]
    fn shared_button_preserves_hover_click_and_pressed_feedback(cx: &mut gpui::TestAppContext) {
        use gpui::{Modifiers, MouseButton, div, prelude::*, px};
        use std::rc::Rc;
        struct Host {
            observed: Rc<RefCell<gpui::Fill>>,
            clicks: Rc<std::cell::Cell<usize>>,
            hovered: Rc<std::cell::Cell<bool>>,
        }
        impl gpui::Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                let clicks = self.clicks.clone();
                let hovered = self.hovered.clone();
                let mut button = super::super::widgets::secondary_button("hover-test")
                    .debug_selector(|| "hover-test".into())
                    .on_hover(move |value, _, _| hovered.set(*value))
                    .on_click(move |_, _, _| clicks.set(clicks.get() + 1))
                    .child("Action");
                button.observed = Some(self.observed.clone());
                div().size_full().child(button)
            }
        }
        let observed = Rc::new(RefCell::new(gpui::Fill::default()));
        let clicks = Rc::new(std::cell::Cell::new(0));
        let hovered = Rc::new(std::cell::Cell::new(false));
        cx.update(|cx| cx.set_reduce_motion(true));
        let (_, cx) = cx.add_window_view(|_, _| Host {
            observed: observed.clone(),
            clicks: clicks.clone(),
            hovered: hovered.clone(),
        });
        cx.run_until_parked();
        let bounds = cx.debug_bounds("hover-test").unwrap();
        cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
        assert!(hovered.get());
        assert_eq!(
            *observed.borrow(),
            gpui::Fill::from(gpui::rgb(super::super::theme::bg_sidebar_row_selected()))
        );
        cx.simulate_mouse_down(bounds.center(), MouseButton::Left, Modifiers::default());
        assert_eq!(
            *observed.borrow(),
            gpui::Fill::from(gpui::rgb(super::super::theme::border()))
        );
        cx.simulate_mouse_up(bounds.center(), MouseButton::Left, Modifiers::default());
        assert_eq!(clicks.get(), 1);
        cx.simulate_mouse_move(
            gpui::point(bounds.right() + px(20.), bounds.bottom() + px(20.)),
            None,
            Modifiers::default(),
        );
        assert!(!hovered.get());
        assert_eq!(
            *observed.borrow(),
            gpui::Fill::from(gpui::rgb(super::super::theme::bg_sidebar_pill()))
        );
        assert_eq!(cx.update(|window, cx| window.simulate_next_frame(cx)), 0);
    }

    #[gpui::test]
    fn cached_child_hover_animates_reverses_and_goes_idle(cx: &mut gpui::TestAppContext) {
        use gpui::{Modifiers, div, prelude::*};
        use std::{cell::Cell, rc::Rc};
        struct Control {
            observed: Rc<RefCell<gpui::Fill>>,
            renders: Rc<Cell<usize>>,
        }
        impl gpui::Render for Control {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                self.renders.set(self.renders.get() + 1);
                let mut button = super::super::widgets::secondary_button("cached-hover")
                    .debug_selector(|| "cached-hover".into())
                    .child("Action");
                button.observed = Some(self.observed.clone());
                div().size_full().child(button)
            }
        }
        struct Host {
            control: gpui::Entity<Control>,
        }
        impl gpui::Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                div().size_full().child(
                    self.control.clone().cached(
                        gpui::StyleRefinement::default()
                            .w(gpui::px(200.))
                            .h(gpui::px(80.)),
                    ),
                )
            }
        }
        let observed = Rc::new(RefCell::new(gpui::Fill::default()));
        let child_renders = Rc::new(Cell::new(0));
        cx.update(|cx| cx.set_reduce_motion(false));
        let (_, cx) = cx.add_window_view(|_, cx| Host {
            control: cx.new(|_| Control {
                observed: observed.clone(),
                renders: child_renders.clone(),
            }),
        });
        cx.run_until_parked();
        let rest = gpui::Fill::from(gpui::rgb(super::super::theme::bg_sidebar_pill()));
        let hover = gpui::Fill::from(gpui::rgb(super::super::theme::bg_sidebar_row_selected()));
        let bounds = cx.debug_bounds("cached-hover").unwrap();
        let outside = gpui::point(
            bounds.right() + gpui::px(20.),
            bounds.bottom() + gpui::px(20.),
        );
        cx.simulate_mouse_move(outside, None, Modifiers::default());
        let child_count = child_renders.get();
        cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
        assert!(cx.update(|window, cx| window.simulate_next_frame(cx)) > 0);
        cx.run_until_parked();
        // The primitive uses real monotonic time; the test executor's clock
        // does not advance Instant. One bounded wait samples an in-flight fade.
        std::thread::sleep(HOVER / 4);
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        cx.run_until_parked();
        assert_ne!(*observed.borrow(), rest);
        cx.simulate_mouse_move(outside, None, Modifiers::default());
        assert_ne!(*observed.borrow(), rest, "leaving must not snap to rest");
        assert!(cx.update(|window, cx| window.simulate_next_frame(cx)) > 0);
        cx.run_until_parked();
        std::thread::sleep(HOVER + Duration::from_millis(20));
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        cx.run_until_parked();
        assert_eq!(*observed.borrow(), rest);
        assert_eq!(cx.update(|window, cx| window.simulate_next_frame(cx)), 0);
        cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
        std::thread::sleep(HOVER + Duration::from_millis(20));
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        cx.run_until_parked();
        assert_eq!(*observed.borrow(), hover);
        assert_eq!(cx.update(|window, cx| window.simulate_next_frame(cx)), 0);
        assert!(child_renders.get() > child_count);
    }
}
