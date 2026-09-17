//! One scrollbar for every scrolling surface: the transcript and sidebar
//! lists (`ListState`), the settings pane (`ScrollHandle`), and wide
//! markdown tables (`ScrollHandle`, sideways). A thin thumb rides the
//! far edge and widens under the pointer; dragging it scrolls, and the
//! drag keeps working after the pointer leaves the track because the
//! thumb captures the pointer. A press on the track pages, the way the
//! platform's own bars do. The wheel passes through to the content
//! underneath.
//!
//! The bar hides itself, like the platform's overlay bars: it appears
//! when the user scrolls, while the pointer is on its strip, and during
//! a drag, then fades after a second of quiet. Content that grows under
//! a list following its tail is not a scroll, so a streaming transcript
//! never flashes the bar.
//!
//! The bar is a plain element, not an entity: its state (the drag, the
//! last activity) lives in element state, so a surface adds a scrollbar
//! with one child and no bookkeeping of its own.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    App, BorderStyle, Bounds, DispatchPhase, Element, ElementId, GlobalElementId, Hitbox,
    HitboxBehavior, IntoElement, ListState, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollHandle, Style, Task, Window, div, point, prelude::*, px,
    quad, size,
};

use super::theme;

/// Width of the strip the bar occupies at the far edge.
pub const WIDTH: Pixels = px(12.);
const THUMB_WIDTH: Pixels = px(6.);
const THUMB_WIDTH_WIDE: Pixels = px(9.);
const THUMB_MIN_LENGTH: Pixels = px(25.);
const EDGE_INSET: Pixels = px(2.);
/// Quiet time before the bar starts to hide.
const IDLE: Duration = Duration::from_millis(1000);
/// How long the hide takes; skipped under reduce motion.
const FADE: Duration = Duration::from_millis(250);

/// What the bar drives: any surface with an offset along the bar's axis.
/// Offsets are distances from the start edge (top or left), never
/// negative, whatever the surface's own sign convention.
pub trait ScrollTarget: Clone + 'static {
    /// How far the content extends past the viewport.
    fn scroll_range(&self) -> Pixels;
    /// How far the viewport is scrolled from the start edge.
    fn scroll_top(&self) -> Pixels;
    fn scroll_to(&self, top: Pixels);
    /// The thumb was pressed; a list may freeze its content height so
    /// the drag stays stable while rows measure.
    fn drag_started(&self) {}
    fn drag_ended(&self) {}
}

impl ScrollTarget for ListState {
    fn scroll_range(&self) -> Pixels {
        self.max_offset_for_scrollbar().y.max(px(0.))
    }

    fn scroll_top(&self) -> Pixels {
        (-self.scroll_px_offset_for_scrollbar().y).max(px(0.))
    }

    fn scroll_to(&self, top: Pixels) {
        self.set_offset_from_scrollbar(point(px(0.), -top));
    }

    fn drag_started(&self) {
        self.scrollbar_drag_started();
    }

    fn drag_ended(&self) {
        self.scrollbar_drag_ended();
    }
}

impl ScrollTarget for ScrollHandle {
    fn scroll_range(&self) -> Pixels {
        self.max_offset().y.max(px(0.))
    }

    fn scroll_top(&self) -> Pixels {
        (-self.offset().y).max(px(0.))
    }

    fn scroll_to(&self, top: Pixels) {
        let x = self.offset().x;
        self.set_offset(point(x, -top));
    }
}

/// A `ScrollHandle` read along x, for a bar under content that scrolls
/// sideways.
#[derive(Clone)]
pub struct Sideways(pub ScrollHandle);

impl ScrollTarget for Sideways {
    fn scroll_range(&self) -> Pixels {
        self.0.max_offset().x.max(px(0.))
    }

    fn scroll_top(&self) -> Pixels {
        (-self.0.offset().x).max(px(0.))
    }

    fn scroll_to(&self, left: Pixels) {
        let y = self.0.offset().y;
        self.0.set_offset(point(-left, y));
    }
}

/// A scrollbar overlaid on the right edge of a `relative` container. It
/// paints nothing and blocks nothing while the content fits or the bar
/// is hidden.
pub fn scrollbar<T: ScrollTarget>(id: impl Into<ElementId>, target: T) -> impl IntoElement {
    div()
        .absolute()
        .top_0()
        .right_0()
        .bottom_0()
        .w(WIDTH)
        .flex()
        .flex_col()
        .child(Scrollbar {
            id: id.into(),
            target,
            axis: Axis::Vertical,
        })
}

/// A scrollbar overlaid on the bottom edge of a `relative` container
/// whose content scrolls sideways.
pub fn horizontal_scrollbar(id: impl Into<ElementId>, handle: ScrollHandle) -> impl IntoElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .bottom_0()
        .h(WIDTH)
        .flex()
        .child(Scrollbar {
            id: id.into(),
            target: Sideways(handle),
            axis: Axis::Horizontal,
        })
}

pub struct Scrollbar<T> {
    id: ElementId,
    target: T,
    axis: Axis,
}

/// Which way the bar runs: down the right edge or along the bottom edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Axis {
    Vertical,
    Horizontal,
}

impl Axis {
    /// The coordinate that moves along the bar.
    fn along(self, point: Point<Pixels>) -> Pixels {
        match self {
            Axis::Vertical => point.y,
            Axis::Horizontal => point.x,
        }
    }

    /// Where the track starts, in window coordinates.
    fn start(self, bounds: Bounds<Pixels>) -> Pixels {
        match self {
            Axis::Vertical => bounds.top(),
            Axis::Horizontal => bounds.left(),
        }
    }

    /// The track length.
    fn track(self, bounds: Bounds<Pixels>) -> Pixels {
        match self {
            Axis::Vertical => bounds.size.height,
            Axis::Horizontal => bounds.size.width,
        }
    }

    /// The thumb's rectangle: `start` and `length` along the bar,
    /// `width` across it, inset from the far edge.
    fn thumb_bounds(
        self,
        bounds: Bounds<Pixels>,
        start: Pixels,
        length: Pixels,
        width: Pixels,
    ) -> Bounds<Pixels> {
        match self {
            Axis::Vertical => Bounds::new(
                point(bounds.right() - EDGE_INSET - width, bounds.top() + start),
                size(width, length),
            ),
            Axis::Horizontal => Bounds::new(
                point(bounds.left() + start, bounds.bottom() - EDGE_INSET - width),
                size(length, width),
            ),
        }
    }
}

/// Thumb placement for one frame, in track-local pixels along the bar.
#[derive(Clone, Copy, Debug)]
struct Geometry {
    /// Track length; also the distance one track press pages by.
    track: Pixels,
    thumb: Pixels,
    thumb_top: Pixels,
    /// How far the thumb top can travel.
    scrollable: Pixels,
    /// How far the content can scroll.
    range: Pixels,
}

fn geometry<T: ScrollTarget>(track: Pixels, target: &T) -> Option<Geometry> {
    let range = target.scroll_range();
    if range <= px(1.) || track <= px(0.) {
        return None;
    }
    let ratio = f32::from(track) / f32::from(track + range);
    let thumb = (track * ratio).max(THUMB_MIN_LENGTH).min(track);
    let scrollable = (track - thumb).max(px(0.));
    let progress = (f32::from(target.scroll_top()) / f32::from(range)).clamp(0., 1.);
    Some(Geometry {
        track,
        thumb,
        thumb_top: scrollable * progress,
        scrollable,
        range,
    })
}

/// The content offset that puts the thumb top at `thumb_top`.
fn offset_for_thumb_top(geometry: &Geometry, thumb_top: Pixels) -> Pixels {
    if geometry.scrollable <= px(0.) {
        return px(0.);
    }
    let progress = (f32::from(thumb_top) / f32::from(geometry.scrollable)).clamp(0., 1.);
    geometry.range * progress
}

/// Whether a change in `(top, range)` between two frames was the user
/// scrolling, as opposed to content arriving. A list following its tail
/// moves its top and its range together and stays at the end; that is
/// growth, not a scroll. A scroll moves the top while the range holds.
fn user_scrolled(last: (Pixels, Pixels), now: (Pixels, Pixels)) -> bool {
    let (last_top, last_range) = last;
    let (top, range) = now;
    if top == last_top {
        return false;
    }
    let grew = range != last_range;
    let at_end = (top - range).abs() < px(1.);
    !(grew && at_end)
}

#[derive(Default)]
struct State {
    drag: Option<Drag>,
    /// Pointer over the bar as of the last move, so widening repaints
    /// once per change rather than per move.
    hovered: bool,
    /// Scroll top and range as of the last frame.
    last: Option<(Pixels, Pixels)>,
    /// When the bar was last used: a scroll, a hover, a drag.
    active_at: Option<Instant>,
    /// The wake-up that hides the bar after `IDLE`; one at a time.
    hide_timer: Option<Task<()>>,
}

#[derive(Clone, Copy)]
struct Drag {
    /// Pointer position along the bar when the thumb was pressed.
    pointer: Pixels,
    thumb_top: Pixels,
}

/// How visible the bar is this frame.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Visibility {
    Hidden,
    /// Mid-fade; the next frame is already requested.
    Fading(f32),
    Shown,
}

impl Visibility {
    fn opacity(self) -> f32 {
        match self {
            Visibility::Hidden => 0.,
            Visibility::Fading(opacity) => opacity,
            Visibility::Shown => 1.,
        }
    }
}

/// Visibility from the time since the last activity.
fn visibility(since_active: Option<Duration>, reduce_motion: bool) -> Visibility {
    let Some(elapsed) = since_active else {
        return Visibility::Hidden;
    };
    if elapsed <= IDLE {
        return Visibility::Shown;
    }
    if reduce_motion {
        return Visibility::Hidden;
    }
    let fading = elapsed - IDLE;
    if fading >= FADE {
        Visibility::Hidden
    } else {
        Visibility::Fading(1. - fading.as_secs_f32() / FADE.as_secs_f32())
    }
}

pub struct Prepaint {
    hitbox: Hitbox,
    state: Rc<RefCell<State>>,
    geometry: Option<Geometry>,
    visibility: Visibility,
}

impl<T: ScrollTarget> IntoElement for Scrollbar<T> {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl<T: ScrollTarget> Element for Scrollbar<T> {
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
            flex_grow: 1.,
            size: match self.axis {
                Axis::Vertical => size(WIDTH.into(), gpui::Length::Auto),
                Axis::Horizontal => size(gpui::Length::Auto, WIDTH.into()),
            },
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let geometry = geometry(self.axis.track(bounds), &self.target);
        let id = id.expect("the scrollbar element has an id");
        let state = window.with_element_state::<Rc<RefCell<State>>, _>(id, |state, _window| {
            let state = state.unwrap_or_default();
            (state.clone(), state)
        });
        let now = Instant::now();
        let visibility = {
            let mut state = state.borrow_mut();
            let current = (self.target.scroll_top(), self.target.scroll_range());
            if let Some(last) = state.last
                && geometry.is_some()
                && user_scrolled(last, current)
            {
                state.active_at = Some(now);
            }
            state.last = Some(current);
            if state.drag.is_some() || state.hovered {
                state.active_at = Some(now);
            }
            visibility(
                state.active_at.map(|at| now.duration_since(at)),
                cx.reduce_motion(),
            )
        };
        // Swallow presses only while there is a visible bar to press; the
        // wheel always reaches the content underneath, and a hidden bar
        // still notices the pointer so hovering its strip reveals it.
        let behavior = if geometry.is_some() && visibility != Visibility::Hidden {
            HitboxBehavior::BlockMouseExceptScroll
        } else {
            HitboxBehavior::Normal
        };
        let hitbox = window.insert_hitbox(bounds, behavior);
        Prepaint {
            hitbox,
            state,
            geometry,
            visibility,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let state = prepaint.state.clone();
        let Some(geometry) = prepaint.geometry else {
            // The content shrank to fit mid-drag: let the target go.
            if state.borrow_mut().drag.take().is_some() {
                self.target.drag_ended();
                window.release_pointer();
            }
            return;
        };
        let axis = self.axis;
        let view = window.current_view();
        let hitbox = prepaint.hitbox.clone();
        let visibility = prepaint.visibility;
        let dragging = state.borrow().drag.is_some();
        let wide = dragging || hitbox.is_hovered(window);
        let width = if wide { THUMB_WIDTH_WIDE } else { THUMB_WIDTH };
        if visibility != Visibility::Hidden {
            let thumb_bounds = axis.thumb_bounds(bounds, geometry.thumb_top, geometry.thumb, width);
            let mut color = if wide {
                theme::scrollbar_thumb_active()
            } else {
                theme::scrollbar_thumb()
            };
            color.a *= visibility.opacity();
            window.paint_quad(quad(
                thumb_bounds,
                width / 2.,
                color,
                px(0.),
                gpui::transparent_black(),
                BorderStyle::Solid,
            ));
        }
        match visibility {
            // The fade draws itself frame by frame, only while fading.
            Visibility::Fading(_) => window.request_animation_frame(),
            // Shown and quiet: one wake-up starts the hide.
            Visibility::Shown if !dragging => {
                let mut guard = state.borrow_mut();
                if guard.hide_timer.is_none() {
                    let state = state.clone();
                    let wake_at = guard.active_at.unwrap_or_else(Instant::now) + IDLE;
                    guard.hide_timer = Some(window.spawn(cx, async move |cx| {
                        let wait = wake_at.saturating_duration_since(Instant::now());
                        cx.background_executor().timer(wait).await;
                        state.borrow_mut().hide_timer = None;
                        // The next frame re-arms if activity moved the deadline.
                        cx.update(|_window, cx| cx.notify(view)).ok();
                    }));
                }
            }
            _ => {}
        }

        let target = self.target.clone();
        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let state = state.clone();
            let target = target.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || visibility == Visibility::Hidden
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                let local = axis.along(event.position) - axis.start(bounds);
                let on_thumb =
                    local >= geometry.thumb_top && local <= geometry.thumb_top + geometry.thumb;
                if on_thumb {
                    state.borrow_mut().drag = Some(Drag {
                        pointer: axis.along(event.position),
                        thumb_top: geometry.thumb_top,
                    });
                    target.drag_started();
                    window.capture_pointer(hitbox.id);
                } else if local < geometry.thumb_top {
                    target.scroll_to((target.scroll_top() - geometry.track).max(px(0.)));
                } else {
                    target.scroll_to((target.scroll_top() + geometry.track).min(geometry.range));
                }
                state.borrow_mut().active_at = Some(Instant::now());
                cx.stop_propagation();
                cx.notify(view);
            }
        });
        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let state = state.clone();
            let target = target.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let drag = state.borrow().drag;
                if let Some(drag) = drag {
                    let thumb_top = drag.thumb_top + (axis.along(event.position) - drag.pointer);
                    target.scroll_to(offset_for_thumb_top(&geometry, thumb_top));
                    state.borrow_mut().active_at = Some(Instant::now());
                    cx.notify(view);
                    return;
                }
                let hovered = hitbox.is_hovered(window);
                let mut state = state.borrow_mut();
                if state.hovered != hovered {
                    state.hovered = hovered;
                    if hovered {
                        state.active_at = Some(Instant::now());
                    }
                    cx.notify(view);
                }
            }
        });
        window.on_mouse_event({
            let state = state.clone();
            move |event: &MouseUpEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                    return;
                }
                if state.borrow_mut().drag.take().is_some() {
                    target.drag_ended();
                    window.release_pointer();
                    cx.notify(view);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scroll_moves_the_top_while_the_range_holds() {
        assert!(user_scrolled((px(100.), px(1000.)), (px(140.), px(1000.))));
        assert!(user_scrolled((px(1000.), px(1000.)), (px(900.), px(1000.))));
    }

    #[test]
    fn content_growing_under_a_tailing_list_is_not_a_scroll() {
        // At the end before and after: the list followed its tail.
        assert!(!user_scrolled(
            (px(1000.), px(1000.)),
            (px(1060.), px(1060.))
        ));
        // Growth while scrolled up leaves the top alone.
        assert!(!user_scrolled((px(100.), px(1000.)), (px(100.), px(1060.))));
        // Nothing changed.
        assert!(!user_scrolled((px(100.), px(1000.)), (px(100.), px(1000.))));
    }

    #[test]
    fn visibility_shows_then_fades_then_hides() {
        assert_eq!(visibility(None, false), Visibility::Hidden);
        assert_eq!(visibility(Some(Duration::ZERO), false), Visibility::Shown);
        assert_eq!(visibility(Some(IDLE), false), Visibility::Shown);
        let Visibility::Fading(opacity) = visibility(Some(IDLE + FADE / 2), false) else {
            panic!("mid-fade");
        };
        assert!(opacity > 0.4 && opacity < 0.6);
        assert_eq!(visibility(Some(IDLE + FADE), false), Visibility::Hidden);
        // Reduce motion: no fade at all.
        assert_eq!(visibility(Some(IDLE + FADE / 2), true), Visibility::Hidden);
    }

    #[test]
    fn a_horizontal_thumb_lies_along_the_bottom_edge() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(200.), WIDTH));
        let thumb = Axis::Horizontal.thumb_bounds(bounds, px(30.), px(50.), THUMB_WIDTH);
        assert_eq!(thumb.left(), px(40.));
        assert_eq!(thumb.size.width, px(50.));
        assert_eq!(thumb.size.height, THUMB_WIDTH);
        assert_eq!(thumb.bottom(), bounds.bottom() - EDGE_INSET);
        assert_eq!(Axis::Horizontal.track(bounds), px(200.));
        assert_eq!(Axis::Horizontal.along(point(px(7.), px(9.))), px(7.));
    }

    #[test]
    fn a_vertical_thumb_lies_along_the_right_edge() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(WIDTH, px(300.)));
        let thumb = Axis::Vertical.thumb_bounds(bounds, px(30.), px(50.), THUMB_WIDTH);
        assert_eq!(thumb.top(), px(50.));
        assert_eq!(thumb.size.height, px(50.));
        assert_eq!(thumb.size.width, THUMB_WIDTH);
        assert_eq!(thumb.right(), bounds.right() - EDGE_INSET);
        assert_eq!(Axis::Vertical.track(bounds), px(300.));
    }
}
