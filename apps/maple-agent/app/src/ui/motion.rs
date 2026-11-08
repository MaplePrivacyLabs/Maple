//! Motion primitives. The rules and tokens are in `docs/motion.md`.
//!
//! Every animated frame re-renders the window, so motion here is short
//! and asks for frames only while something moves:
//!
//! - Looping effects (spinners, pulsing dots) run through [`ticker`]: a
//!   shared phase clock plus one coalesced repaint per view per interval,
//!   at a rate the eye still reads as continuous (`LOOP_FPS`).
//! - One-shot entrances ([`fade_in`], [`rise_in`], [`slide_in`]) wrap
//!   gpui's oneshot `with_animation`: they request frames until they end,
//!   then the window goes idle again. A new element id replays them.
//! - Reversible state changes keep a [`Tween`], which retargets from the
//!   value on screen: in view state for transitions a view owns (the
//!   sidebar, the composer), or in element state through [`animated`] and
//!   [`hover_background`] for render code that owns none.
//!
//! Everything settles at its end state under reduced motion. gpui cannot
//! transform a `div`, so movement is a paint offset ([`Shifted`]) that
//! leaves layout alone; only SVG icons scale or rotate. Press feedback
//! stays immediate.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gpui::{
    AnimationElement, AnimationExt, AnyElement, App, Bounds, Element, ElementId, EntityId,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, ParentElement, Pixels, Point,
    Styled, Window, px,
};

/// Frames per second a looping effect asks for. Twelve to fifteen steps a
/// second still read as motion at a fraction of the display rate.
pub const LOOP_FPS: u32 = 15;

/// Pointer hover: background and opacity only, the same both ways.
pub const HOVER: Duration = Duration::from_millis(120);
/// Menus, popovers, tooltips, notices, and content that replaces other
/// content (a Settings section, another task).
pub const REVEAL: Duration = Duration::from_millis(140);
/// A control changing state in place: switches, chevrons, icon swaps.
pub const TOGGLE: Duration = Duration::from_millis(150);
/// Content arriving: live transcript rows, cards, dialogs.
pub const ENTER: Duration = Duration::from_millis(180);
/// Panels changing size: the sidebar and the expanded composer.
pub const PANEL: Duration = Duration::from_millis(200);
/// How far arriving content travels before it settles.
pub const RISE: Pixels = px(6.);

/// Time for a box to grow or shrink by `distance`: about 500 px a second,
/// so one line of text takes the shortest time and a large jump [`PANEL`].
pub fn resize_duration(distance: Pixels) -> Duration {
    let millis = (distance.as_f32().abs() * 2.).clamp(120., PANEL.as_millis() as f32);
    Duration::from_millis(millis as u64)
}

/// Entrances and fades: a fast start and a long settle.
pub fn ease_out_quint(t: f32) -> f32 {
    1. - (1. - t).powi(5)
}

/// Movement and size: gentler than [`ease_out_quint`], so a moving edge
/// stays readable.
pub fn ease_out_cubic(t: f32) -> f32 {
    1. - (1. - t).powi(3)
}

/// Hover, which reverses often: the same curve both ways.
pub fn smoothstep(t: f32) -> f32 {
    t * t * (3. - 2. * t)
}

#[cfg(test)]
thread_local! {
    static TEST_NOW: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// The clock transitions read. Tests pin it with [`set_test_now`], since
/// the test executor's clock does not advance `Instant`.
pub fn now() -> Instant {
    #[cfg(test)]
    if let Some(now) = TEST_NOW.with(std::cell::Cell::get) {
        return now;
    }
    Instant::now()
}

/// Pin [`now`] on this thread; `None` returns it to the real clock.
#[cfg(test)]
pub fn set_test_now(now: Option<Instant>) {
    TEST_NOW.with(|cell| cell.set(now));
}

/// One reversible value. Retargeting starts from the value on screen, so
/// a second click never jumps back to an endpoint or restarts.
#[derive(Clone, Copy, Debug)]
pub struct Tween {
    from: f32,
    to: f32,
    started: Instant,
    duration: Duration,
    easing: fn(f32) -> f32,
}

impl Tween {
    /// At rest at `value`.
    pub fn new(value: f32) -> Self {
        Self {
            from: value,
            to: value,
            started: epoch(),
            duration: Duration::ZERO,
            easing: ease_out_cubic,
        }
    }

    /// Where the value is heading.
    pub fn target(&self) -> f32 {
        self.to
    }

    /// The value to draw at `now`.
    pub fn value(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= self.duration {
            return self.to;
        }
        let t = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        self.from + (self.to - self.from) * (self.easing)(t)
    }

    /// Whether the value still changes after `now`.
    pub fn is_moving(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) < self.duration
    }

    /// Head for `to` over `duration` from wherever the value is now.
    /// Reduced motion settles at once.
    pub fn set(
        &mut self,
        to: f32,
        duration: Duration,
        easing: fn(f32) -> f32,
        now: Instant,
        reduced: bool,
    ) {
        if reduced || duration.is_zero() {
            *self = Self::new(to);
        } else if to != self.to {
            *self = Self {
                from: self.value(now),
                to,
                started: now,
                duration,
                easing,
            };
        }
    }

    /// [`Tween::set`], then the value to draw now and whether another
    /// frame is needed. Reduced motion also settles a transition already
    /// under way.
    pub fn step(
        &mut self,
        to: f32,
        duration: Duration,
        easing: fn(f32) -> f32,
        now: Instant,
        reduced: bool,
    ) -> (f32, bool) {
        self.set(to, duration, easing, now, reduced);
        if reduced {
            *self = Self::new(self.to);
        }
        (self.value(now), self.is_moving(now))
    }
}

/// How far an entrance that began at `started` is through `duration`, in
/// `[0, 1]`. For entrances timed by view state rather than element state,
/// such as transcript rows that can scroll away and back.
pub fn progress(started: Instant, duration: Duration, now: Instant, reduced: bool) -> f32 {
    if reduced || duration.is_zero() {
        return 1.;
    }
    (now.saturating_duration_since(started).as_secs_f32() / duration.as_secs_f32()).min(1.)
}

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
        gpui::Animation::new(REVEAL).with_easing(ease_out_quint),
        |element, delta| element.opacity(delta),
    )
}

/// Fade in and settle up from [`RISE`] below, for panels that open above
/// their anchor. Same one-shot cost profile as [`fade_in`].
pub fn rise_in<E>(element: E, id: impl Into<ElementId>) -> AnimationElement<Shifted<E>>
where
    E: IntoElement + Styled + 'static,
{
    slide_in(element, id, REVEAL, gpui::point(px(0.), RISE))
}

/// Fade in over `duration` while moving from `from`, an offset from the
/// resting position, to rest. Layout and hit testing use the resting
/// position throughout.
pub fn slide_in<E>(
    element: E,
    id: impl Into<ElementId>,
    duration: Duration,
    from: Point<Pixels>,
) -> AnimationElement<Shifted<E>>
where
    E: IntoElement + Styled + 'static,
{
    shifted(element).with_animation(
        id,
        gpui::Animation::new(duration).with_easing(ease_out_quint),
        move |element, delta| {
            let remaining = 1. - delta;
            element
                .opacity(delta)
                .offset(gpui::point(from.x * remaining, from.y * remaining))
        },
    )
}

/// Draw content `progress` of the way through its entrance (see
/// [`progress`]): faded and [`RISE`] low at 0, at rest at 1.
pub fn arrive<E>(element: E, progress: f32) -> Shifted<E>
where
    E: IntoElement + Styled + 'static,
{
    let eased = ease_out_quint(progress.clamp(0., 1.));
    shifted(element)
        .opacity(eased)
        .offset(gpui::point(px(0.), RISE * (1. - eased)))
}

/// Paint an element away from where layout put it, hitbox included,
/// without moving anything else: gpui's stand-in for a CSS translate.
pub fn shifted<E: IntoElement>(element: E) -> Shifted<E> {
    Shifted {
        element: Some(element),
        offset: Point::default(),
    }
}

pub struct Shifted<E> {
    element: Option<E>,
    offset: Point<Pixels>,
}

impl<E> Shifted<E> {
    /// Rounded to whole pixels, so shifted text stays crisp.
    pub fn offset(mut self, offset: Point<Pixels>) -> Self {
        self.offset = gpui::point(offset.x.round(), offset.y.round());
        self
    }
}

impl<E: Styled> Styled for Shifted<E> {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        self.element
            .as_mut()
            .expect("a shifted element is styled before layout")
            .style()
    }
}

impl<E: IntoElement + 'static> IntoElement for Shifted<E> {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl<E: IntoElement + 'static> Element for Shifted<E> {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
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
        let mut element = self
            .element
            .take()
            .expect("a shifted element is laid out once")
            .into_any_element();
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
        window.with_element_offset(self.offset, |window| element.prepaint(window, cx));
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

/// Paint an element starting at window y `from_y` and easing (ease-out
/// cubic, by `progress`) to where layout put it: a layout-free move from
/// where something was drawn last frame to its new place. The offset is
/// worked out from the element's own bounds as it prepaints, so the first
/// frame is already in the right place.
pub fn glide_from(element: impl IntoElement, from_y: Pixels, progress: f32) -> GlideFrom {
    GlideFrom {
        element: Some(element.into_any_element()),
        from_y,
        progress,
    }
}

pub struct GlideFrom {
    element: Option<AnyElement>,
    from_y: Pixels,
    progress: f32,
}

impl IntoElement for GlideFrom {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for GlideFrom {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
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
        let mut element = self
            .element
            .take()
            .expect("a gliding element is laid out once");
        (element.request_layout(window, cx), element)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        element: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        let remaining = 1. - ease_out_cubic(self.progress.clamp(0., 1.));
        let offset = gpui::point(
            px(0.),
            ((self.from_y - bounds.origin.y) * remaining).round(),
        );
        window.with_element_offset(offset, |window| element.prepaint(window, cx));
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

/// An element rebuilt each frame from a value that eases toward `target`
/// and retargets from wherever it is. The value lives in element state
/// under `id`, so render code needs no view state. It starts at `target`,
/// so nothing animates on mount. `build` runs at layout time; keep it
/// cheap.
pub fn animated(
    id: impl Into<ElementId>,
    target: f32,
    duration: Duration,
    easing: fn(f32) -> f32,
    build: impl Fn(f32) -> AnyElement + 'static,
) -> Animated {
    Animated {
        id: id.into(),
        target,
        duration,
        easing,
        build: Box::new(build),
    }
}

pub struct Animated {
    id: ElementId,
    target: f32,
    duration: Duration,
    easing: fn(f32) -> f32,
    build: Box<dyn Fn(f32) -> AnyElement>,
}

// Keyed apart from other element state that may share the id.
struct AnimatedValue(Tween);

impl IntoElement for Animated {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Animated {
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
        id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, AnyElement) {
        let now = now();
        let reduced = cx.reduce_motion();
        let (value, moving) = window.with_element_state(
            id.expect("animated elements have an id"),
            |state: Option<AnimatedValue>, _| {
                let mut state = state.unwrap_or_else(|| AnimatedValue(Tween::new(self.target)));
                let frame = state
                    .0
                    .step(self.target, self.duration, self.easing, now, reduced);
                (frame, state)
            },
        );
        if moving {
            window.request_animation_frame();
        }
        let mut element = (self.build)(value);
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

/// Two icons sharing one `size` slot: the one `second` selects scales up
/// and fades in while the other scales down and out, over [`TOGGLE`].
/// Use it where an icon names the state its control toggles (expand and
/// shrink, send and stop).
pub fn icon_swap(
    id: impl Into<ElementId>,
    second: bool,
    icons: [&'static str; 2],
    size: Pixels,
    color: u32,
) -> gpui::Div {
    let id = std::sync::Arc::new(id.into());
    let layer =
        |index: usize| {
            let name = icons[index];
            let shown = (index == 1) == second;
            animated(
                ElementId::NamedChild(
                    id.clone(),
                    if index == 0 { "first" } else { "second" }.into(),
                ),
                if shown { 1. } else { 0. },
                TOGGLE,
                ease_out_cubic,
                move |shown| {
                    let scale = 0.6 + 0.4 * shown;
                    gpui::div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .opacity(shown)
                        .child(super::icons::icon(name, size, color).with_transformation(
                            gpui::Transformation::scale(gpui::size(scale, scale)),
                        ))
                        .into_any_element()
                },
            )
        };
    gpui::div()
        .relative()
        .flex_none()
        .size(size)
        .child(layer(0))
        .child(layer(1))
}

/// A disclosure chevron that points right when closed and turns down when
/// `open`, over [`TOGGLE`].
pub fn chevron(id: impl Into<ElementId>, open: bool, size: Pixels, color: u32) -> Animated {
    animated(
        id,
        if open { 1. } else { 0. },
        TOGGLE,
        ease_out_cubic,
        move |turn| {
            super::icons::icon("chevron-right", size, color)
                .with_transformation(gpui::Transformation::rotate(gpui::radians(
                    turn * std::f32::consts::FRAC_PI_2,
                )))
                .into_any_element()
        },
    )
}

/// A sine pulse in `[low, high]` offset by `shift` cycles, for dots that
/// breathe out of step with each other.
pub fn pulse(phase: f32, shift: f32, low: f32, high: f32) -> f32 {
    let wave = ((phase + shift) * std::f32::consts::TAU).sin() * 0.5 + 0.5;
    low + (high - low) * wave
}

// Background and visibility on one control are separate values with
// separate targets under the same element id.
struct HoverPlate(Tween);
struct Reveal(Tween);

/// The color `amount` of the way from `from` to `to`. Premultiplied RGB
/// avoids a dark flash when fading from transparent.
pub fn mix(from: gpui::Hsla, to: gpui::Hsla, amount: f32) -> gpui::Hsla {
    if amount <= 0. {
        return from;
    }
    if amount >= 1. {
        return to;
    }
    let from = gpui::Rgba::from(from);
    let to = gpui::Rgba::from(to);
    let alpha = from.a + (to.a - from.a) * amount;
    let channel = |a: f32, b: f32| {
        if alpha == 0. {
            0.
        } else {
            (a * from.a * (1. - amount) + b * to.a * amount) / alpha
        }
    };
    gpui::Rgba {
        r: channel(from.r, to.r),
        g: channel(from.g, to.g),
        b: channel(from.b, to.b),
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
        revealed: None,
        instant: false,
        #[cfg(test)]
        observed: None,
        #[cfg(test)]
        observed_opacity: None,
        #[cfg(test)]
        test_now: None,
    }
}

pub struct HoverBackground {
    element: gpui::Stateful<gpui::Div>,
    rest: gpui::Hsla,
    hover: gpui::Hsla,
    highlighted: Option<bool>,
    revealed: Option<bool>,
    instant: bool,
    #[cfg(test)]
    observed: Option<std::rc::Rc<RefCell<gpui::Fill>>>,
    #[cfg(test)]
    observed_opacity: Option<std::rc::Rc<std::cell::Cell<f32>>>,
    #[cfg(test)]
    test_now: Option<Instant>,
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

    /// Fade visibility in place, keeping the control's hitbox and layout.
    /// Native focus styles can still reveal a keyboard target immediately.
    pub fn revealed(mut self, visible: bool) -> Self {
        self.revealed = Some(visible);
        self
    }

    /// Settle at once instead of easing, for a highlight the keyboard
    /// moved: arrow keys inside a menu must not trail behind.
    pub fn instant(mut self, instant: bool) -> Self {
        self.instant = instant;
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
        let now = now();
        #[cfg(test)]
        let now = self.test_now.unwrap_or(now);
        let reduced = cx.reduce_motion() || self.instant;
        let (amount, animating) = window.with_element_state(
            id.expect("hover controls have an id"),
            |state: Option<HoverPlate>, _| {
                let mut state = state.unwrap_or_else(|| HoverPlate(Tween::new(0.)));
                let frame = state.0.step(
                    if hovered { 1. } else { 0. },
                    HOVER,
                    smoothstep,
                    now,
                    reduced,
                );
                (frame, state)
            },
        );
        self.element.style().background = Some(mix(self.rest, self.hover, amount).into());
        let reveal_animating = if let Some(visible) = self.revealed {
            let target = if visible { 1. } else { 0. };
            let (opacity, animating) = window.with_element_state(
                id.expect("revealed controls have an id"),
                |state: Option<Reveal>, _| {
                    // An initially visible control should not fade in on mount.
                    let mut state = state.unwrap_or_else(|| Reveal(Tween::new(target)));
                    let frame = state.0.step(target, HOVER, smoothstep, now, reduced);
                    (frame, state)
                },
            );
            self.element.style().opacity = Some(opacity);
            animating
        } else {
            false
        };
        if animating || reveal_animating {
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
        #[cfg(test)]
        if let Some(observed) = &self.observed_opacity {
            use gpui::InteractiveElement;
            observed.set(
                self.element
                    .interactivity()
                    .compute_style(id, hitbox.as_ref(), window, cx)
                    .opacity
                    .unwrap_or(1.),
            );
        }
    }
}

// ---- Content swaps --------------------------------------------------------

/// Content that replaces other content in one place, such as the window's
/// screen or a Settings section: whenever `key` changes, the new content
/// fades in over [`REVEAL`]. Its first appearance does not fade, so a
/// first paint is never held back. `id` must be stable across frames.
///
/// The content itself always paints at full opacity; a `cover` in the
/// color behind it, painted over its bounds, fades out instead. gpui
/// replays a cached view's last paint, opacity included, so fading an
/// ancestor of one (the sidebar) would leave it stuck at its first, nearly
/// transparent frame.
pub fn swap_in<E>(
    element: E,
    id: impl Into<ElementId>,
    key: impl Into<ElementId>,
    cover: impl Into<gpui::Hsla>,
) -> Swap<E>
where
    E: IntoElement + 'static,
{
    Swap {
        id: id.into(),
        key: key.into(),
        cover: cover.into(),
        element: Some(element),
    }
}

pub struct Swap<E> {
    id: ElementId,
    key: ElementId,
    cover: gpui::Hsla,
    element: Option<E>,
}

// The key last drawn, and when content under a new key arrived.
struct Swapped {
    key: ElementId,
    started: Option<Instant>,
}

impl<E: IntoElement + 'static> IntoElement for Swap<E> {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl<E: IntoElement + 'static> Element for Swap<E> {
    // The content, and how much of it shows through the cover.
    type RequestLayoutState = (AnyElement, f32);
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
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, (AnyElement, f32)) {
        let now = now();
        let reduced = cx.reduce_motion();
        let key = &self.key;
        let shown = window.with_element_state(
            id.expect("swaps have an id"),
            |state: Option<Swapped>, _| {
                let mut state = state.unwrap_or_else(|| Swapped {
                    key: key.clone(),
                    started: None,
                });
                if &state.key != key {
                    state.key = key.clone();
                    state.started = Some(now);
                }
                let shown = state.started.map_or(1., |started| {
                    ease_out_quint(progress(started, REVEAL, now, reduced))
                });
                if shown >= 1. {
                    state.started = None;
                }
                (shown, state)
            },
        );
        if shown < 1. {
            window.request_animation_frame();
        }
        let mut element = self
            .element
            .take()
            .expect("a swap is laid out once")
            .into_any_element();
        (element.request_layout(window, cx), (element, shown))
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        (element, _): &mut (AnyElement, f32),
        window: &mut Window,
        cx: &mut App,
    ) {
        element.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (element, shown): &mut (AnyElement, f32),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        element.paint(window, cx);
        if *shown < 1. {
            let mut cover = self.cover;
            cover.a *= 1. - *shown;
            window.paint_quad(gpui::fill(bounds, cover));
        }
    }
}

#[cfg(test)]
mod hover_tests {
    use super::*;
    fn hover(tween: &mut Tween, hovered: bool, now: Instant, reduced: bool) -> (f32, bool) {
        tween.step(
            if hovered { 1. } else { 0. },
            HOVER,
            smoothstep,
            now,
            reduced,
        )
    }

    #[test]
    fn hover_reverses_from_the_displayed_value_and_settles() {
        let now = Instant::now();
        let mut transition = Tween::new(0.);
        assert_eq!(hover(&mut transition, false, now, false), (0., false));
        assert_eq!(hover(&mut transition, true, now, false), (0., true));
        let halfway = now + HOVER / 2;
        assert_eq!(hover(&mut transition, true, halfway, false), (0.5, true));
        assert_eq!(hover(&mut transition, false, halfway, false), (0.5, true));
        assert_eq!(
            hover(&mut transition, false, halfway + HOVER, false),
            (0., false)
        );
        assert_eq!(
            hover(&mut transition, false, halfway + HOVER * 2, false),
            (0., false)
        );
    }
    #[test]
    fn reduced_motion_settles_an_interrupted_hover_immediately() {
        let now = Instant::now();
        let mut transition = Tween::new(0.);
        hover(&mut transition, true, now, false);
        assert_eq!(
            hover(&mut transition, true, now + HOVER / 4, true),
            (1., false)
        );
        assert_eq!(
            hover(&mut transition, false, now + HOVER / 2, true),
            (0., false)
        );
    }

    #[test]
    fn hover_eases_symmetrically_without_darkening_transparent_colors() {
        let now = Instant::now();
        let mut transition = Tween::new(0.);
        hover(&mut transition, true, now, false);
        let early = transition.value(now + HOVER / 4);
        let late = transition.value(now + HOVER * 3 / 4);
        assert!(early < 0.25 && late > 0.75);
        assert!((early + late - 1.).abs() <= 2. * f32::EPSILON);
        let white = gpui::hsla(0., 0., 1., 1.);
        let middle = gpui::Rgba::from(mix(gpui::transparent_black(), white, 0.5));
        assert_eq!(middle.r, 1.);
        assert_eq!(middle.g, 1.);
        assert_eq!(middle.b, 1.);
        assert_eq!(middle.a, 0.5);
    }

    #[gpui::test]
    fn reveal_fades_reverses_and_preserves_focus_and_geometry(cx: &mut gpui::TestAppContext) {
        use gpui::{div, prelude::*, px};
        use std::{cell::Cell, rc::Rc};
        struct Host {
            visible: bool,
            now: Instant,
            focus: gpui::FocusHandle,
            observed: Rc<Cell<f32>>,
        }
        impl gpui::Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                let mut button = super::super::widgets::secondary_button("reveal-test")
                    .debug_selector(|| "reveal-test".into())
                    .track_focus(&self.focus)
                    .focus_visible(|style| style.opacity(1.))
                    .highlighted(false)
                    .revealed(self.visible)
                    .child("Action");
                button.observed_opacity = Some(self.observed.clone());
                button.test_now = Some(self.now);
                div().size_full().p(px(20.)).child(button)
            }
        }
        let observed = Rc::new(Cell::new(0.));
        cx.update(|cx| cx.set_reduce_motion(false));
        let (host, cx) = cx.add_window_view(|_, cx| Host {
            visible: true,
            now: Instant::now(),
            focus: cx.focus_handle(),
            observed: observed.clone(),
        });
        cx.run_until_parked();
        let bounds = cx.debug_bounds("reveal-test").unwrap();
        assert_eq!(
            observed.get(),
            1.,
            "visible controls do not fade in on mount"
        );
        let set_visible = |visible, cx: &mut gpui::VisualTestContext| {
            host.update(cx, |host, cx| {
                host.visible = visible;
                cx.notify();
            });
            cx.run_until_parked();
        };
        let frame_after = |duration, cx: &mut gpui::VisualTestContext| {
            host.update(cx, |host, cx| {
                host.now += duration;
                cx.notify();
            });
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.run_until_parked();
        };
        set_visible(false, cx);
        assert!(observed.get() > 0., "hide must not snap off");
        frame_after(HOVER / 4, cx);
        assert!(observed.get() > 0. && observed.get() < 1.);
        set_visible(true, cx);
        assert!(
            observed.get() > 0. && observed.get() < 1.,
            "reverse in place"
        );
        frame_after(HOVER, cx);
        assert_eq!(observed.get(), 1.);
        set_visible(false, cx);
        frame_after(HOVER, cx);
        assert_eq!(observed.get(), 0.);
        set_visible(true, cx);
        assert!(observed.get() < 1., "show must not snap on");
        frame_after(HOVER, cx);
        assert_eq!(observed.get(), 1.);
        assert_eq!(cx.debug_bounds("reveal-test").unwrap(), bounds);
        assert_eq!(cx.update(|window, cx| window.simulate_next_frame(cx)), 0);

        cx.update(|_, cx| cx.set_reduce_motion(true));
        set_visible(false, cx);
        assert_eq!(observed.get(), 0.);
        set_visible(true, cx);
        assert_eq!(observed.get(), 1.);
        set_visible(false, cx);
        let focus = host.update(cx, |host, _| host.focus.clone());
        cx.simulate_keystrokes("tab");
        cx.update(|window, cx| focus.focus(window, cx));
        cx.run_until_parked();
        assert_eq!(observed.get(), 1., "keyboard focus reveals immediately");
        assert_eq!(cx.update(|window, cx| window.simulate_next_frame(cx)), 0);
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

#[cfg(test)]
mod primitive_tests {
    use super::*;
    use gpui::{VisualTestContext, div, prelude::*};
    use std::{cell::Cell, rc::Rc};

    struct Host {
        open: bool,
        shift: Pixels,
        seen: Rc<Cell<f32>>,
    }

    impl gpui::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            let seen = self.seen.clone();
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(animated(
                    "value",
                    if self.open { 1. } else { 0. },
                    PANEL,
                    ease_out_cubic,
                    move |value| {
                        seen.set(value);
                        div().size(px(10.)).into_any_element()
                    },
                ))
                .child(
                    shifted(
                        div()
                            .debug_selector(|| "shifted".into())
                            .w(px(40.))
                            .h(px(20.)),
                    )
                    .offset(gpui::point(px(0.), self.shift)),
                )
                .child(
                    div()
                        .debug_selector(|| "below".into())
                        .w(px(40.))
                        .h(px(20.)),
                )
        }
    }

    fn host(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::Entity<Host>, &mut VisualTestContext, Rc<Cell<f32>>) {
        let seen = Rc::new(Cell::new(-1.));
        cx.update(|cx| cx.set_reduce_motion(false));
        let (host, cx) = cx.add_window_view(|_, _| Host {
            open: false,
            shift: px(0.),
            seen: seen.clone(),
        });
        cx.run_until_parked();
        (host, cx, seen)
    }

    #[gpui::test]
    fn animated_values_start_at_rest_reverse_from_the_screen_and_settle(
        cx: &mut gpui::TestAppContext,
    ) {
        let start = Instant::now();
        set_test_now(Some(start));
        let (host, cx, seen) = host(cx);
        assert_eq!(seen.get(), 0., "nothing animates on mount");
        assert_eq!(cx.update(|window, cx| window.simulate_next_frame(cx)), 0);

        let draw = |at: Instant, open: bool, cx: &mut VisualTestContext| {
            set_test_now(Some(at));
            host.update(cx, |host, cx| {
                host.open = open;
                cx.notify();
            });
            cx.run_until_parked();
        };
        draw(start, true, cx);
        assert_eq!(seen.get(), 0.);
        draw(start + PANEL / 2, true, cx);
        let halfway = seen.get();
        assert!(halfway > 0.5 && halfway < 1., "eases out: {halfway}");
        draw(start + PANEL / 2, false, cx);
        assert_eq!(
            seen.get(),
            halfway,
            "reversing starts from the value on screen"
        );
        draw(start + PANEL / 2 + PANEL / 4, false, cx);
        assert!(seen.get() > 0. && seen.get() < halfway);
        draw(start + PANEL * 2, false, cx);
        assert_eq!(seen.get(), 0.);
        // The frame requested while it moved still runs once.
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        cx.run_until_parked();
        assert_eq!(seen.get(), 0.);
        assert_eq!(
            cx.update(|window, cx| window.simulate_next_frame(cx)),
            0,
            "a settled value asks for no frames"
        );

        cx.update(|_, cx| cx.set_reduce_motion(true));
        draw(start + PANEL * 2, true, cx);
        assert_eq!(seen.get(), 1., "reduced motion settles at once");
        assert_eq!(cx.update(|window, cx| window.simulate_next_frame(cx)), 0);
        set_test_now(None);
    }

    #[gpui::test]
    fn shifted_content_moves_without_moving_its_neighbors(cx: &mut gpui::TestAppContext) {
        let (host, cx, _) = host(cx);
        let shifted = cx.debug_bounds("shifted").unwrap();
        let below = cx.debug_bounds("below").unwrap();
        host.update(cx, |host, cx| {
            host.shift = px(5.6);
            cx.notify();
        });
        cx.run_until_parked();
        let moved = cx.debug_bounds("shifted").unwrap();
        assert_eq!(moved.origin.y, shifted.origin.y + px(6.), "whole pixels");
        assert_eq!(moved.size, shifted.size);
        assert_eq!(cx.debug_bounds("below").unwrap(), below);
    }
}
