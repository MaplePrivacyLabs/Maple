//! Popup menus: dropdowns, overflow menus, and context menus.
//!
//! Every popup goes through this module, so all of them behave the way
//! native menus do. A view that shows popups owns one [`Popup`] (one per
//! nesting level), wires each opener with [`Popup::trigger`], and describes
//! the open menu as a [`Menu`] on every render. The state stays in the view
//! and the rows are rebuilt each frame, so a switch turned on or a status
//! that arrives late shows without rebuilding anything.
//!
//! What every popup gets:
//!
//! - Its trigger toggles it. While the popup is open, a press on the
//!   trigger closes it and is consumed, so the release cannot reopen it.
//! - A press anywhere else closes it and still reaches what was pressed. A
//!   menu opened from inside it (a nested menu, a text field's own
//!   right-click menu) counts as inside.
//! - The open popup holds keyboard focus. Up and Down (`j`/`k` under
//!   Application Vim) move the highlight, Home and End (`g g`/`G`) jump,
//!   Enter or Space picks, Escape closes, and a typed letter jumps to the
//!   next row that starts with it. The pointer moves the same highlight. A
//!   text field inside the menu keeps its own keys.
//! - One popup per window. Whatever takes focus next (another popup, a
//!   dialog, a text field) closes the open one. When a popup that holds
//!   focus closes, focus returns to where it was before it opened, also
//!   when the popup closes because its owner stopped drawing it.
//! - The panel draws above everything and blocks the pointer from what
//!   lies beneath it. It opens on its preferred side of the trigger, flips
//!   to the other side when only that one has room, and stays inside the
//!   window, so it never covers its own trigger while there is room.
//! - Assistive technology sees a button that reports whether its popup is
//!   expanded, a named menu of named items, and the highlighted item as the
//!   focused one.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ClickEvent, Context, Div, Edges, ElementId,
    FocusHandle, GlobalElementId, HitboxBehavior, HitboxId, InspectorElementId, KeyDownEvent,
    LayoutId, MouseButton, MouseDownEvent, Pixels, Point, Position, Role, ScrollHandle,
    SharedString, Size, Stateful, Style, Subscription, Toggled, WeakFocusHandle, Window, canvas,
    div, point, prelude::*, px,
};

use super::icons::icon;
use super::{motion, theme, widgets};

/// Key context of an open popup menu. Its bindings live in the keymap
/// catalog; Application Vim adds its aliases under `ApplicationVim && Menu`.
pub(crate) const MENU_CONTEXT: &str = "Menu";
/// The menu's context under Application Vim. GPUI matches `A && B` within a
/// single context, so the panel carries both identifiers itself.
const MENU_VIM_CONTEXT: &str = "Menu ApplicationVim";

gpui::actions!(
    menu,
    [
        SelectPrevious,
        SelectNext,
        SelectFirst,
        SelectLast,
        Confirm,
        Cancel
    ]
);

/// Space kept between a trigger and the menu it opens.
const GAP: Pixels = px(4.);
/// Closest a menu comes to the window edge.
const WINDOW_MARGIN: Pixels = px(8.);

/// The popups that hold focus, or held it and have not handed it back yet,
/// across the app. Each entry keeps what other popups need to know about
/// it.
///
/// - Where it hands focus back. When a popup opens while another one holds
///   focus, its target is that popup. If that one closes first (another
///   menu's button was pressed, a shortcut opened a menu elsewhere), the
///   target passes on to the popup that pointed at it, so focus returns to
///   where the first popup began. A menu opened from inside a menu that
///   stays open returns to that menu.
/// - Where its panel is. A press on a menu opened from inside another (a
///   nested menu, a field's own right-click menu) is not a press outside
///   the outer menu, even where the inner one reaches past its edge.
#[derive(Default)]
struct Popups(Vec<Registered>);

struct Registered {
    focus: WeakFocusHandle,
    target: Option<WeakFocusHandle>,
    open: bool,
    hitbox: Rc<Cell<Option<HitboxId>>>,
}

impl gpui::Global for Popups {}

/// The registered popups, less those whose owners were dropped.
fn popups(cx: &mut App) -> &mut Vec<Registered> {
    let popups = &mut cx.default_global::<Popups>().0;
    popups.retain(|entry| entry.focus.upgrade().is_some());
    popups
}

/// Drop `focus`'s entry and return its target. A popup that would hand
/// focus back to it hands it on to that target instead.
fn unregister(popups: &mut Vec<Registered>, focus: &FocusHandle) -> Option<WeakFocusHandle> {
    let index = popups.iter().position(|entry| entry.focus == *focus)?;
    let target = popups.remove(index).target;
    for entry in popups.iter_mut() {
        if entry
            .target
            .as_ref()
            .is_some_and(|pointed| pointed == focus)
        {
            entry.target = target.clone();
        }
    }
    target
}

/// Whether the pointer is on the panel of a menu opened from inside
/// `outer`'s: a nested menu, or a text field's own right-click menu.
fn over_inner_menu(outer: &FocusHandle, window: &Window, cx: &App) -> bool {
    cx.try_global::<Popups>().is_some_and(|popups| {
        popups.0.iter().any(|entry| {
            entry.open
                && entry.focus != *outer
                && entry
                    .hitbox
                    .get()
                    .is_some_and(|hitbox| hitbox.is_hovered(window))
                && entry
                    .focus
                    .upgrade()
                    .is_some_and(|inner| outer.contains(&inner, window))
        })
    })
}

/// Bringing the highlighted row of a menu that just opened into view. GPUI
/// scrolls with the viewport a scroll container recorded in the previous
/// frame, which a panel does not have in its first frame, so the request
/// waits for the next frame. It counts frames, not renders: a list can
/// render its rows more than once in a frame.
#[derive(Clone, Copy, PartialEq)]
enum Reveal {
    Done,
    /// Opened with a highlight; not drawn yet.
    AfterLayout,
    /// Drawn once; the next frame brings the row into view.
    NextFrame,
    Now,
}

/// Which of a view's popups is open, and the plumbing they share.
///
/// `V` is the owning view. `get` finds this state inside it again, so the
/// listeners a popup installs (trigger presses, menu actions, the focus
/// watch) can reach it. At most one of a view's popups is open at a time;
/// a menu that opens from inside another menu uses a second `Popup`.
pub(crate) struct Popup<V: 'static, K: 'static> {
    get: fn(&mut V) -> &mut Popup<V, K>,
    open: Option<K>,
    /// The highlighted item, by id: rows that arrive or leave while the
    /// menu is open (a status that loads late) do not move it to another
    /// item.
    highlighted: Option<ElementId>,
    focus: FocusHandle,
    /// Focus moves in on the next render, once the panel is in the tree.
    focus_pending: bool,
    /// Closes the popup when focus leaves it. Held from when focus moves
    /// in until the popup hands it back.
    focus_out: Option<Subscription>,
    /// The panel's hitbox in the last frame (see [`Popups`]).
    hitbox: Rc<Cell<Option<HitboxId>>>,
    scroll: ScrollHandle,
    reveal: Rc<Cell<Reveal>>,
    /// This popup has an entry in [`Popups`].
    registered: bool,
}

impl<V: 'static, K: Clone + PartialEq + 'static> Popup<V, K> {
    pub(crate) fn new(get: fn(&mut V) -> &mut Self, cx: &mut App) -> Self {
        Self {
            get,
            open: None,
            highlighted: None,
            focus: cx.focus_handle(),
            focus_pending: false,
            focus_out: None,
            hitbox: Rc::default(),
            scroll: ScrollHandle::new(),
            reveal: Rc::new(Cell::new(Reveal::Done)),
            registered: false,
        }
    }

    pub(crate) fn is_open(&self, key: &K) -> bool {
        self.open.as_ref() == Some(key)
    }

    pub(crate) fn open_key(&self) -> Option<&K> {
        self.open.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn highlighted(&self) -> Option<&ElementId> {
        self.highlighted.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Open `key`'s popup, closing whichever of this view's popups was open.
    /// Nothing is highlighted until the pointer or a key picks a row.
    pub(crate) fn open(&mut self, key: K, cx: &mut Context<V>) {
        self.open_with(key, None, cx);
    }

    /// Open with `item` highlighted and scrolled into view. A menu opened
    /// from the keyboard starts on the current choice, the way a native
    /// popup button does.
    pub(crate) fn open_highlighted(
        &mut self,
        key: K,
        item: impl Into<ElementId>,
        cx: &mut Context<V>,
    ) {
        self.open_with(key, Some(item.into()), cx);
    }

    fn open_with(&mut self, key: K, item: Option<ElementId>, cx: &mut Context<V>) {
        if self.open.as_ref() != Some(&key) {
            self.scroll.set_offset(point(px(0.), px(0.)));
        }
        self.open = Some(key);
        self.reveal.set(if item.is_some() {
            Reveal::AfterLayout
        } else {
            Reveal::Done
        });
        self.highlighted = item;
        self.focus_pending = true;
        cx.notify();
    }

    /// Open `key` if it is closed and close it if it is open. Returns
    /// whether it is open now.
    pub(crate) fn toggle(&mut self, key: K, cx: &mut Context<V>) -> bool {
        if self.is_open(&key) {
            self.close(cx);
            false
        } else {
            self.open(key, cx);
            true
        }
    }

    /// Close whichever popup is open. Returns whether one was. If it held
    /// focus, focus goes back on the owner's next render (see
    /// [`Self::sync_focus`]).
    pub(crate) fn close(&mut self, cx: &mut Context<V>) -> bool {
        self.focus_pending = false;
        self.highlighted = None;
        if self.open.take().is_none() {
            return false;
        }
        if self.registered
            && let Some(entry) = popups(cx)
                .iter_mut()
                .find(|entry| entry.focus == self.focus)
        {
            entry.open = false;
        }
        cx.notify();
        true
    }

    /// Highlight `item`, or nothing.
    pub(crate) fn highlight(&mut self, item: Option<ElementId>, cx: &mut Context<V>) {
        if self.highlighted != item {
            self.highlighted = item;
            cx.notify();
        }
    }

    /// Take focus back into the open popup on the owner's next render, for
    /// when a text field inside it goes away.
    pub(crate) fn refocus(&mut self, cx: &mut Context<V>) {
        if self.open.is_some() {
            self.focus_pending = true;
            cx.notify();
        }
    }

    /// Move focus into a popup that just opened, and back out of one that
    /// closed while holding it. Call this first in the owning view's
    /// render, so that focus requested later in the same render wins, and
    /// for an outer popup before a popup nested in it.
    pub(crate) fn sync_focus(&mut self, window: &mut Window, cx: &mut Context<V>) {
        if self.open.is_none() {
            if self.registered {
                // The last frame still has the panel, so this tells whether
                // the popup held focus when it closed. If focus moved on,
                // it stays.
                let held = self.focus.contains_focused(window, cx);
                self.settle(held, window, cx);
            }
            return;
        }
        if !std::mem::take(&mut self.focus_pending) {
            return;
        }
        let reopened = self.focus.contains_focused(window, cx);
        let focused = window.focused(cx);
        let registered = self.registered;
        let popups = popups(cx);
        let entry = registered
            .then(|| popups.iter_mut().find(|entry| entry.focus == self.focus))
            .flatten();
        match entry {
            // Moving to another of this view's menus, reopened from inside
            // itself, or taking focus back from a field in it: the target
            // stays.
            Some(entry) if entry.open || reopened => entry.open = true,
            _ => {
                unregister(popups, &self.focus);
                // Focus in a popup that already closed passes straight
                // through to that popup's target.
                let target = focused.and_then(|focused| {
                    popups
                        .iter()
                        .find(|entry| !entry.open && entry.focus == focused)
                        .map_or_else(|| Some(focused.downgrade()), |entry| entry.target.clone())
                });
                popups.push(Registered {
                    focus: self.focus.downgrade(),
                    target,
                    open: true,
                    hitbox: self.hitbox.clone(),
                });
            }
        }
        self.registered = true;
        window.focus(&self.focus, cx);
        let get = self.get;
        self.focus_out = Some(
            cx.on_focus_out(&self.focus, window, move |_, event, window, cx| {
                // Something else took focus (another popup, a dialog, a
                // field), the window went inactive, or the owner stopped
                // drawing the menu. The popup closes the way a native menu
                // does. Focus events arrive while the frame draws, when a
                // notify is dropped, so close once the frame is done.
                cx.defer_in(window, move |view, window, cx| {
                    // Focus that did not move on (it is still on what lost
                    // it, or on nothing) goes back now: the owner may never
                    // draw again, as when the sidebar was just hidden.
                    let stayed = window
                        .focused(cx)
                        .is_none_or(|focused| event.blurred == focused);
                    let popup = get(view);
                    popup.close(cx);
                    popup.settle(stayed, window, cx);
                });
            }),
        );
    }

    /// Drop this popup's entry in [`Popups`] and, if it still held focus,
    /// hand focus back to where it was before the popup opened.
    fn settle(&mut self, held: bool, window: &mut Window, cx: &mut App) {
        if !std::mem::take(&mut self.registered) {
            return;
        }
        self.focus_out = None;
        let target = unregister(popups(cx), &self.focus);
        if held {
            match target.and_then(|target| target.upgrade()) {
                Some(target) => window.focus(&target, cx),
                None => window.blur(),
            }
        }
    }

    /// Wire `button` as the opener of `key`'s popup.
    ///
    /// `toggle` runs on click. While the popup is open, a left press on the
    /// button closes it in the capture phase and is consumed. Without that,
    /// the popup's outside-press handler would close it and the click would
    /// open it again. The press is taken only while the popup is open, so a
    /// closed trigger stays an ordinary button: it shows its pressed state,
    /// and any other open popup still sees the press and closes. Commands
    /// and keys that open a menu call [`Self::open`] instead.
    pub(crate) fn trigger(
        &self,
        key: K,
        button: Stateful<Div>,
        cx: &mut Context<V>,
        toggle: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Stateful<Div> {
        let open = self.is_open(&key);
        let get = self.get;
        button
            .aria_expanded(open)
            .when(open, |button| {
                button.capture_any_mouse_down(cx.listener(
                    move |view, event: &MouseDownEvent, _, cx| {
                        if event.button == MouseButton::Left {
                            cx.stop_propagation();
                            get(view).close(cx);
                        }
                    },
                ))
            })
            .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                toggle(view, window, cx);
            }))
    }

    /// The open menu, placed and wired to this popup. Build it only while
    /// the popup is open.
    pub(crate) fn render(
        &self,
        menu: Menu<V>,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<V>,
    ) -> AnyElement {
        let get = self.get;
        let Menu {
            id,
            width,
            max_height,
            label,
            entries,
            priority,
            application_vim,
        } = menu;

        // What the keyboard needs about each item: its id, its action,
        // whether it can be picked, the letter it starts with, and its
        // child index for scrolling it into view.
        let mut picks: Vec<Pick<V>> = Vec::new();
        let mut children: Vec<AnyElement> = Vec::with_capacity(entries.len());
        for entry in entries {
            match entry {
                Entry::Item(item) => {
                    let highlighted = self.highlighted.as_ref() == Some(&item.id);
                    picks.push(Pick {
                        id: item.id.clone(),
                        on_select: item.on_select.clone(),
                        enabled: item.enabled,
                        keep_open: item.keep_open,
                        initial: initial(&item.label),
                        child: children.len(),
                    });
                    children.push(item_row(item, highlighted, get, cx));
                }
                Entry::Header(text) => children.push(
                    div()
                        .px_3()
                        .pt_1()
                        .pb_1()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(text)
                        .into_any_element(),
                ),
                Entry::Separator => children.push(
                    div()
                        .my_1()
                        .h(px(1.))
                        .bg(gpui::rgb(theme::border()))
                        .into_any_element(),
                ),
                Entry::Element(element) => children.push(element),
            }
        }
        match self.reveal.get() {
            Reveal::Done | Reveal::NextFrame => {}
            Reveal::AfterLayout => {
                self.reveal.set(Reveal::NextFrame);
                let reveal = self.reveal.clone();
                let view = window.current_view();
                window.on_next_frame(move |_, cx| {
                    if reveal.get() == Reveal::NextFrame {
                        reveal.set(Reveal::Now);
                        cx.notify(view);
                    }
                });
            }
            Reveal::Now => {
                self.reveal.set(Reveal::Done);
                if let Some(pick) = self
                    .highlighted
                    .as_ref()
                    .and_then(|id| picks.iter().find(|pick| &pick.id == id))
                {
                    self.scroll.scroll_to_item(pick.child);
                }
            }
        }
        let picks = Rc::new(picks);
        // Application Vim's keys drive the menu only while the menu itself
        // has focus: `g g` must not hold back a "g" typed into a field
        // inside it.
        let vim = application_vim && self.focus.is_focused(window);

        let focus = self.focus.clone();
        let panel = widgets::popup_panel(id.clone(), width)
            .debug_selector(|| id.to_string())
            .key_context(if vim { MENU_VIM_CONTEXT } else { MENU_CONTEXT })
            .track_focus(&self.focus)
            .when_some(label, |panel, label| panel.aria_label(label))
            .when_some(max_height, |panel, height| {
                panel
                    .max_h(height)
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
            })
            .on_mouse_down_out(cx.listener(move |view, _: &MouseDownEvent, window, cx| {
                if !over_inner_menu(&focus, window, cx) {
                    get(view).close(cx);
                }
            }))
            .on_action(self.navigation::<SelectNext>(&picks, Step::By(1), cx))
            .on_action(self.navigation::<SelectPrevious>(&picks, Step::By(-1), cx))
            .on_action(self.navigation::<SelectFirst>(&picks, Step::First, cx))
            .on_action(self.navigation::<SelectLast>(&picks, Step::Last, cx))
            .on_action({
                let picks = picks.clone();
                let focus = self.focus.clone();
                cx.listener(move |view, _: &Confirm, window, cx| {
                    // Enter in a field inside the menu belongs to the field.
                    if !focus.is_focused(window) {
                        cx.propagate();
                        return;
                    }
                    let Some(row) = get(view).highlighted_row(&picks) else {
                        return;
                    };
                    pick(view, get, &picks, row, window, cx);
                })
            })
            .on_action({
                let focus = self.focus.clone();
                cx.listener(move |view, _: &Cancel, window, cx| {
                    // Escape in a field inside the menu goes to the menu's
                    // owner first, which ends the field's edit or closes
                    // the menu.
                    if !focus.is_focused(window) {
                        cx.propagate();
                        return;
                    }
                    get(view).close(cx);
                })
            })
            .on_key_down({
                let picks = picks.clone();
                let focus = self.focus.clone();
                cx.listener(move |view, event: &KeyDownEvent, window, cx| {
                    type_ahead(view, get, &picks, &focus, event, window, cx);
                })
            })
            .children(children);

        // A sibling after the panel, covering it, records the panel's hitbox
        // for an outer menu to test. The panel itself may scroll, so the
        // recorder sits on a box that does not.
        let hitbox = self.hitbox.clone();
        let panel = div().relative().child(panel).child(
            canvas(
                |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                move |_, recorded, _, _| hitbox.set(Some(recorded.id)),
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        );
        Float {
            panel: Some(motion::fade_in(panel, "popup-reveal").into_any_element()),
            placement,
            priority,
        }
        .into_any_element()
    }

    /// The highlighted item's row among `picks`, if it is still there.
    fn highlighted_row(&self, picks: &[Pick<V>]) -> Option<usize> {
        let id = self.highlighted.as_ref()?;
        picks.iter().position(|pick| &pick.id == id)
    }

    /// A handler that moves the highlight. Arrow keys in a field inside the
    /// menu stay with the field.
    fn navigation<A: gpui::Action>(
        &self,
        picks: &Rc<Vec<Pick<V>>>,
        by: Step,
        cx: &mut Context<V>,
    ) -> impl Fn(&A, &mut Window, &mut App) + 'static {
        let get = self.get;
        let picks = picks.clone();
        let focus = self.focus.clone();
        cx.listener(move |view, _: &A, window, cx| {
            if !focus.is_focused(window) {
                cx.propagate();
                return;
            }
            step(get(view), &picks, by, cx);
        })
    }
}

/// Where a menu opens. Beside a trigger, the menu element must be a child
/// of a `relative` box around the trigger: the menu takes that box as the
/// trigger's bounds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Placement {
    /// Below the trigger, sharing its left edge.
    BelowStart,
    /// Below the trigger, sharing its right edge.
    BelowEnd,
    /// Above the trigger, sharing its left edge.
    AboveStart,
    /// At a point in window coordinates, for a right-click.
    At(Point<Pixels>),
}

impl Placement {
    /// The top-left corner of a panel of `size` in a window of `window`
    /// size. The panel goes on its preferred side of the trigger (or of the
    /// point) when it fits there, on the other side when only that one has
    /// room, and otherwise on the roomier side, pushed inside the window.
    fn origin(
        self,
        trigger: Bounds<Pixels>,
        size: Size<Pixels>,
        window: Size<Pixels>,
        margin: Pixels,
    ) -> Point<Pixels> {
        let (anchor, prefer_below, prefer_start, gap) = match self {
            Placement::BelowStart => (trigger, true, true, GAP),
            Placement::BelowEnd => (trigger, true, false, GAP),
            Placement::AboveStart => (trigger, false, true, GAP),
            Placement::At(position) => (Bounds::new(position, Size::default()), true, true, px(0.)),
        };
        let room_below = window.height - margin - anchor.bottom() - gap;
        let room_above = anchor.top() - gap - margin;
        let below = first_side(prefer_below, size.height, room_below, room_above);
        let y = if below {
            anchor.bottom() + gap
        } else {
            anchor.top() - gap - size.height
        };
        // Sharing the start edge, the panel reaches right; sharing the end
        // edge, it reaches left.
        let room_right = window.width - margin - anchor.left();
        let room_left = anchor.right() - margin;
        let x = if first_side(prefer_start, size.width, room_right, room_left) {
            anchor.left()
        } else {
            anchor.right() - size.width
        };
        // A panel with room on neither side stays inside the window, over
        // its trigger if it must.
        let x = x.min(window.width - margin - size.width).max(margin);
        let y = y.min(window.height - margin - size.height).max(margin);
        // Whole pixels, rounding away from the trigger to keep the gap.
        let y = if below { y.ceil() } else { y.floor() };
        point(x.round(), y)
    }
}

/// Whether a panel `length` long goes on the first of two sides, which
/// leave `first` and `second` room: the preferred side when it fits there,
/// the other when only that one fits, and the roomier one when neither
/// does.
fn first_side(prefer_first: bool, length: Pixels, first: Pixels, second: Pixels) -> bool {
    let (preferred, other) = if prefer_first {
        (first, second)
    } else {
        (second, first)
    };
    let stays = length <= preferred || length > other && preferred >= other;
    stays == prefer_first
}

/// Draws a menu panel above everything, where [`Placement::origin`] puts
/// it. Beside a trigger, this element covers the `relative` box around the
/// trigger and takes that box's bounds. The panel is measured before it is
/// placed, so it lands on the right side in its first frame.
struct Float {
    panel: Option<AnyElement>,
    placement: Placement,
    priority: usize,
}

impl IntoElement for Float {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Float {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style {
            position: Position::Absolute,
            ..Style::default()
        };
        if !matches!(self.placement, Placement::At(_)) {
            style.inset = Edges::all(px(0.).into());
        }
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(mut panel) = self.panel.take() else {
            return;
        };
        let size = panel.layout_as_root(AvailableSpace::min_size(), window, cx);
        let margin = WINDOW_MARGIN + window.client_inset().unwrap_or_default();
        let origin = self
            .placement
            .origin(bounds, size, window.viewport_size(), margin);
        window.defer_draw(panel, origin, self.priority, None);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }
}

/// What picking a row does, with the owning view in hand.
type OnSelect<V> = Rc<dyn Fn(&mut V, &mut Window, &mut Context<V>)>;
/// A caller's adjustment to one row.
type RowStyle = Box<dyn FnOnce(Stateful<Div>) -> Stateful<Div>>;

/// The rows of a popup menu, rebuilt every render from the owner's state.
pub(crate) struct Menu<V: 'static> {
    id: ElementId,
    width: Pixels,
    max_height: Option<Pixels>,
    label: Option<SharedString>,
    entries: Vec<Entry<V>>,
    priority: usize,
    application_vim: bool,
}

enum Entry<V: 'static> {
    Item(MenuItem<V>),
    Header(SharedString),
    Separator,
    Element(AnyElement),
}

impl<V: 'static> Menu<V> {
    pub(crate) fn new(id: impl Into<ElementId>, width: Pixels) -> Self {
        Self {
            id: id.into(),
            width,
            max_height: None,
            label: None,
            entries: Vec::new(),
            priority: 1,
            application_vim: false,
        }
    }

    /// Take Application Vim's `j`/`k`/`g g`/`G` while it is on.
    pub(crate) fn application_vim(mut self, enabled: bool) -> Self {
        self.application_vim = enabled;
        self
    }

    /// Scroll past this height.
    pub(crate) fn max_height(mut self, height: Pixels) -> Self {
        self.max_height = Some(height);
        self
    }

    /// The menu's name for assistive technology.
    pub(crate) fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub(crate) fn item(mut self, item: MenuItem<V>) -> Self {
        self.entries.push(Entry::Item(item));
        self
    }

    pub(crate) fn items(mut self, items: impl IntoIterator<Item = MenuItem<V>>) -> Self {
        self.entries.extend(items.into_iter().map(Entry::Item));
        self
    }

    /// A caption above a group of items.
    pub(crate) fn header(mut self, text: impl Into<SharedString>) -> Self {
        self.entries.push(Entry::Header(text.into()));
        self
    }

    pub(crate) fn separator(mut self) -> Self {
        self.entries.push(Entry::Separator);
        self
    }

    /// Content that is not an item: the keyboard skips it.
    pub(crate) fn child(mut self, element: impl IntoElement) -> Self {
        self.entries
            .push(Entry::Element(element.into_any_element()));
        self
    }

    /// Draw above menus of a lower level. A menu opened from inside another
    /// passes 2.
    pub(crate) fn level(mut self, level: usize) -> Self {
        self.priority = level;
        self
    }
}

/// One pickable row.
pub(crate) struct MenuItem<V: 'static> {
    id: ElementId,
    label: SharedString,
    icon: Option<&'static str>,
    note: Option<SharedString>,
    /// Replaces the label and note; the label still names the row for
    /// assistive technology and type-ahead.
    content: Option<AnyElement>,
    /// Elide a long label at its start, keeping the end (a path's folder).
    truncate_start: bool,
    /// `Some` for a row that shows a state: a check for the current choice
    /// (a radio item) or a switch (a checkbox item).
    state: Option<ItemState>,
    enabled: bool,
    keep_open: bool,
    trailing: Option<AnyElement>,
    style: Option<RowStyle>,
    on_select: OnSelect<V>,
}

#[derive(Clone, Copy)]
enum ItemState {
    Current(bool),
    Switch(bool),
}

impl<V: 'static> MenuItem<V> {
    pub(crate) fn new(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        on_select: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            note: None,
            content: None,
            truncate_start: false,
            state: None,
            enabled: true,
            keep_open: false,
            trailing: None,
            style: None,
            on_select: Rc::new(on_select),
        }
    }

    pub(crate) fn icon(mut self, name: &'static str) -> Self {
        self.icon = Some(name);
        self
    }

    /// A second, smaller line under the label.
    pub(crate) fn note(mut self, note: impl Into<SharedString>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// Show `content` in place of the label and note.
    pub(crate) fn content(mut self, content: impl IntoElement) -> Self {
        self.content = Some(content.into_any_element());
        self
    }

    /// Elide a long label at its start, so a path keeps its last folder.
    pub(crate) fn truncate_start(mut self) -> Self {
        self.truncate_start = true;
        self
    }

    /// One choice of several; `current` shows the check mark.
    pub(crate) fn current(mut self, current: bool) -> Self {
        self.state = Some(ItemState::Current(current));
        self
    }

    /// An on/off row with a switch. Flipping it keeps the menu open.
    pub(crate) fn switch(mut self, on: bool) -> Self {
        self.state = Some(ItemState::Switch(on));
        self.keep_open = true;
        self
    }

    /// A disabled row is dimmed and cannot be picked or highlighted.
    pub(crate) fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// A control of the row's own at its end, such as a "…" button. A
    /// press on it is the control's alone.
    pub(crate) fn trailing(mut self, element: impl IntoElement) -> Self {
        self.trailing = Some(element.into_any_element());
        self
    }

    /// Adjust the row, for a font preview or a hover group.
    pub(crate) fn style(
        mut self,
        style: impl FnOnce(Stateful<Div>) -> Stateful<Div> + 'static,
    ) -> Self {
        self.style = Some(Box::new(style));
        self
    }
}

struct Pick<V: 'static> {
    id: ElementId,
    on_select: OnSelect<V>,
    enabled: bool,
    keep_open: bool,
    initial: Option<char>,
    child: usize,
}

/// The lowercase letter `text` starts with, for type-ahead.
fn initial(text: &str) -> Option<char> {
    text.chars().next()?.to_lowercase().next()
}

fn item_row<V: 'static, K: Clone + PartialEq + 'static>(
    item: MenuItem<V>,
    highlighted: bool,
    get: fn(&mut V) -> &mut Popup<V, K>,
    cx: &mut Context<V>,
) -> AnyElement {
    let MenuItem {
        id,
        label,
        icon: icon_name,
        note,
        content,
        truncate_start,
        state,
        enabled,
        keep_open,
        trailing,
        style,
        on_select,
    } = item;
    let role = match state {
        None => Role::MenuItem,
        Some(ItemState::Current(_)) => Role::MenuItemRadio,
        Some(ItemState::Switch(_)) => Role::MenuItemCheckBox,
    };
    let text = if enabled {
        theme::text_primary()
    } else {
        theme::text_muted()
    };
    let hovered_id = id.clone();
    let row_element = div()
        .debug_selector(|| id.to_string())
        .id(id)
        .role(role)
        .aria_label(label.clone())
        .when_some(state, |row, state| {
            let on = match state {
                ItemState::Current(on) | ItemState::Switch(on) => on,
            };
            row.aria_toggled(if on { Toggled::True } else { Toggled::False })
        })
        .when(highlighted, |row| {
            row.aria_active_descendant()
                .bg(gpui::rgb(theme::bg_sidebar_row_hover()))
        })
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1p5()
        .rounded(theme::RADIUS_SM)
        .text_sm()
        .text_color(gpui::rgb(text))
        // The pointer highlights the row under it, and nothing over a
        // disabled row, the way a native menu does.
        .on_hover(cx.listener(move |view, hovered: &bool, _, cx| {
            let popup = get(view);
            if *hovered {
                popup.highlight(enabled.then(|| hovered_id.clone()), cx);
            } else if popup.highlighted.as_ref() == Some(&hovered_id) {
                popup.highlight(None, cx);
            }
        }))
        .when(enabled, |row_element| {
            row_element
                .cursor_pointer()
                .active(|style| style.bg(gpui::rgb(theme::bg_sidebar_row_selected())))
                .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    if !keep_open {
                        get(view).close(cx);
                    }
                    on_select(view, window, cx);
                }))
        })
        .children(icon_name.map(|name| icon(name, widgets::ROW_ICON, theme::text_secondary())))
        .map(|row| match content {
            Some(content) => row.child(div().flex_1().min_w_0().child(content)),
            None => row.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .min_w_0()
                            .line_clamp(1)
                            .map(|label| {
                                if truncate_start {
                                    label.text_ellipsis_start()
                                } else {
                                    label.text_ellipsis()
                                }
                            })
                            .child(label),
                    )
                    .children(note.map(|note| {
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(theme::text_muted()))
                            .child(note)
                    })),
            ),
        })
        .children(trailing.map(|trailing| {
            // The row's own control, such as a "…" button. A press on it is
            // the control's alone: the row neither shows as pressed nor
            // takes the click. Stopping only the control's click is not
            // enough, since GPUI then leaves the row holding a press whose
            // release the control consumed, and a second release in the
            // same frame (input faster than frames) clicks the row.
            div()
                .flex_none()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(trailing)
        }))
        .children(state.map(|state| {
            match state {
                ItemState::Current(current) => div()
                    .flex_none()
                    .size(widgets::ROW_ICON)
                    .when(current, |mark| {
                        mark.child(icon("check", widgets::ROW_ICON, theme::accent()))
                    })
                    .into_any_element(),
                ItemState::Switch(on) => widgets::switch_track(on).into_any_element(),
            }
        }));
    match style {
        Some(style) => style(row_element).into_any_element(),
        None => row_element.into_any_element(),
    }
}

#[derive(Clone, Copy)]
enum Step {
    By(isize),
    First,
    Last,
}

/// Move the highlight over the enabled items, wrapping at both ends. With
/// nothing highlighted, Down starts at the top and Up at the bottom.
fn step<V: 'static, K: Clone + PartialEq + 'static>(
    popup: &mut Popup<V, K>,
    picks: &[Pick<V>],
    step: Step,
    cx: &mut Context<V>,
) {
    let rows = picks.len() as isize;
    if rows == 0 {
        return;
    }
    let (mut at, delta) = match step {
        Step::First => (-1, 1),
        Step::Last => (rows, -1),
        Step::By(delta) => match popup.highlighted_row(picks) {
            Some(row) => (row as isize, delta.signum()),
            None if delta < 0 => (rows, -1),
            None => (-1, 1),
        },
    };
    for _ in 0..rows {
        at = (at + delta).rem_euclid(rows);
        let candidate = &picks[at as usize];
        if candidate.enabled {
            popup.scroll.scroll_to_item(candidate.child);
            popup.highlight(Some(candidate.id.clone()), cx);
            return;
        }
    }
}

/// Run item `row`'s action. The menu closes first, so an action that opens
/// another of the view's popups keeps it open.
fn pick<V: 'static, K: Clone + PartialEq + 'static>(
    view: &mut V,
    get: fn(&mut V) -> &mut Popup<V, K>,
    picks: &[Pick<V>],
    row: usize,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    let Some(item) = picks.get(row).filter(|item| item.enabled) else {
        return;
    };
    if !item.keep_open {
        get(view).close(cx);
    }
    (item.on_select)(view, window, cx);
}

/// A letter typed into the menu highlights the next item that starts with
/// it. Other typing stops here too: it must not reach a text field under
/// the menu. Only while the menu itself has focus, so a field inside the
/// menu still takes its typing.
fn type_ahead<V: 'static, K: Clone + PartialEq + 'static>(
    view: &mut V,
    get: fn(&mut V) -> &mut Popup<V, K>,
    picks: &[Pick<V>],
    focus: &FocusHandle,
    event: &KeyDownEvent,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    if !focus.is_focused(window) {
        return;
    }
    let keystroke = &event.keystroke;
    if !keystroke.modifiers.is_subset_of(&gpui::Modifiers::shift()) {
        return;
    }
    let Some(typed) = keystroke.key_char.as_deref() else {
        return;
    };
    cx.stop_propagation();
    let typed = initial(typed);
    let popup = get(view);
    let rows = picks.len();
    let start = popup.highlighted_row(picks).map_or(0, |row| row + 1);
    let found = (0..rows)
        .map(|offset| (start + offset) % rows)
        .find(|&row| picks[row].enabled && typed.is_some() && picks[row].initial == typed);
    if let Some(row) = found {
        popup.scroll.scroll_to_item(picks[row].child);
        popup.highlight(Some(picks[row].id.clone()), cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, size};

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Which {
        A,
        B,
        /// A long menu on a trigger at the bottom of the window.
        Low,
    }

    /// A second view with a popup of its own, for one-popup-per-window.
    struct Other {
        popup: Popup<Other, ()>,
    }

    impl Render for Other {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.popup.sync_focus(window, cx);
            let trigger = self.popup.trigger(
                (),
                div()
                    .id("other-trigger")
                    .debug_selector(|| "other-trigger".into())
                    .w(px(80.))
                    .h(px(20.)),
                cx,
                |this, _, cx| {
                    this.popup.toggle((), cx);
                },
            );
            let menu = self.popup.is_open(&()).then(|| {
                self.popup.render(
                    Menu::new("other-menu", px(120.)).item(MenuItem::new(
                        "other-one",
                        "Other",
                        |_: &mut Other, _, _| {},
                    )),
                    Placement::BelowStart,
                    window,
                    cx,
                )
            });
            div().relative().child(trigger).children(menu)
        }
    }

    struct Host {
        popup: Popup<Host, Which>,
        nested: Popup<Host, ()>,
        other: Entity<Other>,
        /// Whether the host draws the other view at all.
        show_other: bool,
        picked: Vec<&'static str>,
        outside_clicks: usize,
        field: FocusHandle,
        second_enabled: bool,
        /// A row that arrives above the others while the menu is open.
        lead: bool,
        vim: bool,
        /// Build the long menu twice per render, the way a list can render
        /// its rows more than once in a frame.
        render_twice: bool,
    }

    impl Host {
        fn menu_a(&self, cx: &mut Context<Host>) -> Menu<Host> {
            let pick = |name: &'static str| {
                move |this: &mut Host, _: &mut Window, _: &mut Context<Host>| this.picked.push(name)
            };
            // A row with a control of its own, like a project's "…".
            let more = self.nested.trigger(
                (),
                div()
                    .id("a-three-more")
                    .debug_selector(|| "a-three-more".into())
                    .size(px(16.)),
                cx,
                |this, _, cx| {
                    this.nested.toggle((), cx);
                },
            );
            let mut menu = Menu::new("menu-a", px(160.)).application_vim(self.vim);
            if self.lead {
                menu = menu.item(MenuItem::new("a-lead", "Lead", pick("lead")));
            }
            menu.item(MenuItem::new("a-one", "One", pick("one")))
                .item(MenuItem::new("a-two", "Two", pick("two")).enabled(self.second_enabled))
                .separator()
                .item(MenuItem::new("a-three", "Three", pick("three")).trailing(more))
                .item(
                    MenuItem::new(
                        "a-sub",
                        "Submenu",
                        |this: &mut Host, _: &mut Window, cx: &mut Context<Host>| {
                            this.nested.open((), cx);
                        },
                    )
                    .switch(false),
                )
        }

        fn menu_low(&self) -> Menu<Host> {
            Menu::new("menu-low", px(160.))
                .max_height(px(200.))
                .items((0..30).map(|row| {
                    MenuItem::new(
                        SharedString::from(format!("low-{row}")),
                        format!("Row {row}"),
                        |_: &mut Host, _: &mut Window, _: &mut Context<Host>| {},
                    )
                }))
        }
    }

    impl Render for Host {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.popup.sync_focus(window, cx);
            self.nested.sync_focus(window, cx);
            let trigger = |popup: &Popup<Host, Which>,
                           which: Which,
                           id: &'static str,
                           cx: &mut Context<Host>| {
                popup.trigger(
                    which,
                    div()
                        .id(id)
                        .debug_selector(move || id.into())
                        .w(px(80.))
                        .h(px(20.))
                        // A plain button grows while pressed, so the tests
                        // can see the pressed state.
                        .active(|style| style.w(px(90.))),
                    cx,
                    move |this, _, cx| {
                        this.popup.toggle(which, cx);
                    },
                )
            };
            let menu_a = self.popup.is_open(&Which::A).then(|| {
                let mut menu = self.menu_a(cx);
                if self.nested.is_open(&()) {
                    menu = menu.child(
                        div().relative().child(
                            self.nested.render(
                                Menu::new("menu-nested", px(200.))
                                    .level(2)
                                    .item(MenuItem::new(
                                        "nested-one",
                                        "Nested",
                                        |this: &mut Host, _: &mut Window, _: &mut Context<Host>| {
                                            this.picked.push("nested")
                                        },
                                    )),
                                Placement::BelowEnd,
                                window,
                                cx,
                            ),
                        ),
                    );
                }
                self.popup.render(menu, Placement::BelowStart, window, cx)
            });
            let menu_b = self.popup.is_open(&Which::B).then(|| {
                self.popup.render(
                    Menu::new("menu-b", px(160.)).item(MenuItem::new(
                        "b-one",
                        "Bee",
                        |this: &mut Host, _: &mut Window, _: &mut Context<Host>| {
                            this.picked.push("bee")
                        },
                    )),
                    Placement::BelowStart,
                    window,
                    cx,
                )
            });
            let menu_low = self.popup.is_open(&Which::Low).then(|| {
                if self.render_twice {
                    self.popup
                        .render(self.menu_low(), Placement::BelowStart, window, cx);
                }
                self.popup
                    .render(self.menu_low(), Placement::BelowStart, window, cx)
            });
            div()
                .relative()
                .size_full()
                .p_4()
                .flex()
                .gap_16()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .relative()
                                .child(trigger(&self.popup, Which::A, "a-trigger", cx))
                                .children(menu_a),
                        )
                        .child(
                            div()
                                .relative()
                                .child(trigger(&self.popup, Which::B, "b-trigger", cx))
                                .children(menu_b),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .id("outside")
                                .debug_selector(|| "outside".into())
                                .w(px(80.))
                                .h(px(20.))
                                .on_click(cx.listener(|this, _, _, _| this.outside_clicks += 1)),
                        )
                        .child(
                            div()
                                .id("field")
                                .debug_selector(|| "field".into())
                                .track_focus(&self.field)
                                .w(px(80.))
                                .h(px(20.)),
                        )
                        .when(self.show_other, |column| column.child(self.other.clone())),
                )
                .child(
                    // A trigger at the bottom of the window, whose menu has
                    // no room below it.
                    div().absolute().bottom(px(10.)).right(px(40.)).child(
                        div()
                            .relative()
                            .child(trigger(&self.popup, Which::Low, "low-trigger", cx))
                            .children(menu_low),
                    ),
                )
        }
    }

    fn setup(cx: &mut TestAppContext) -> (Entity<Host>, &mut VisualTestContext) {
        cx.update(crate::desktop::register_key_bindings);
        let (host, cx) = cx.add_window_view(|_, cx| {
            let other = cx.new(|cx| Other {
                popup: Popup::new(|this| &mut this.popup, cx),
            });
            Host {
                popup: Popup::new(|this| &mut this.popup, cx),
                nested: Popup::new(|this| &mut this.nested, cx),
                other,
                show_other: true,
                picked: Vec::new(),
                outside_clicks: 0,
                field: cx.focus_handle(),
                second_enabled: true,
                lead: false,
                vim: false,
                render_twice: false,
            }
        });
        cx.simulate_resize(size(px(800.), px(600.)));
        // Focus events carry no path while the window is inactive.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        (host, cx)
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is not on screen"));
        cx.simulate_click(bounds.center(), Modifiers::default());
    }

    fn open(host: &Entity<Host>, cx: &mut VisualTestContext) -> Option<Which> {
        host.update(cx, |host, _| host.popup.open_key().copied())
    }

    fn highlighted(host: &Entity<Host>, cx: &mut VisualTestContext) -> Option<ElementId> {
        host.update(cx, |host, _| host.popup.highlighted().cloned())
    }

    fn focus_field(host: &Entity<Host>, cx: &mut VisualTestContext) -> FocusHandle {
        let field = host.update(cx, |host, _| host.field.clone());
        cx.update(|window, cx| window.focus(&field, cx));
        field
    }

    fn focused(cx: &mut VisualTestContext) -> Option<FocusHandle> {
        cx.update(|window, cx| window.focused(cx))
    }

    #[gpui::test]
    fn a_second_press_on_the_trigger_closes_the_menu(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        click(cx, "a-trigger");
        assert_eq!(open(&host, cx), Some(Which::A));
        click(cx, "a-trigger");
        assert_eq!(open(&host, cx), None, "the release must not reopen it");
        click(cx, "a-trigger");
        assert_eq!(open(&host, cx), Some(Which::A), "and it opens again");
    }

    #[gpui::test]
    fn a_press_outside_closes_the_menu_and_still_lands(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        click(cx, "a-trigger");
        click(cx, "outside");
        assert_eq!(open(&host, cx), None);
        assert_eq!(host.update(cx, |host, _| host.outside_clicks), 1);
    }

    #[gpui::test]
    fn another_trigger_switches_menus(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        // B's menu opens below B, clear of A's trigger above it.
        click(cx, "b-trigger");
        click(cx, "a-trigger");
        assert_eq!(open(&host, cx), Some(Which::A));
    }

    /// Near the bottom of the window a menu opens above its trigger, so the
    /// trigger stays uncovered and a second press on it closes the menu
    /// instead of picking whatever row would cover it.
    #[gpui::test]
    fn a_menu_without_room_below_opens_above_its_trigger(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        click(cx, "low-trigger");
        let menu = cx.debug_bounds("menu-low").expect("menu");
        let trigger = cx.debug_bounds("low-trigger").expect("trigger");
        assert!(
            menu.bottom() <= trigger.top(),
            "the menu {menu:?} sits above the trigger {trigger:?}"
        );
        click(cx, "low-trigger");
        assert_eq!(open(&host, cx), None);
    }

    #[gpui::test]
    fn a_menu_opened_on_a_row_shows_that_row(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        host.update(cx, |host, cx| {
            host.render_twice = true;
            host.popup.open_highlighted(Which::Low, "low-29", cx)
        });
        cx.run_until_parked();
        // The row comes into view in the panel's second frame.
        cx.update(|window, cx| window.simulate_next_frame(cx));
        cx.run_until_parked();
        let menu = cx.debug_bounds("menu-low").expect("menu");
        let row = cx.debug_bounds("low-29").expect("row");
        assert!(
            menu.top() <= row.top() && row.bottom() <= menu.bottom(),
            "the row {row:?} is scrolled into the menu {menu:?}"
        );
    }

    #[gpui::test]
    fn placement_flips_to_the_side_with_room_and_stays_inside(_: &mut TestAppContext) {
        let window = size(px(800.), px(600.));
        let panel = size(px(200.), px(300.));
        let trigger = |x: f32, y: f32| Bounds::new(point(px(x), px(y)), size(px(80.), px(20.)));
        let origin =
            |placement: Placement, trigger| placement.origin(trigger, panel, window, WINDOW_MARGIN);
        // Room below: below, 4 px down, left edges shared.
        assert_eq!(
            origin(Placement::BelowStart, trigger(100., 100.)),
            point(px(100.), px(124.))
        );
        // No room below but room above: above.
        assert_eq!(
            origin(Placement::BelowStart, trigger(100., 500.)),
            point(px(100.), px(196.))
        );
        // No room above but room below: below.
        assert_eq!(
            origin(Placement::AboveStart, trigger(100., 50.)),
            point(px(100.), px(74.))
        );
        // Sharing the right edge would cross the left side: share the left.
        assert_eq!(
            origin(Placement::BelowEnd, trigger(20., 100.)),
            point(px(20.), px(124.))
        );
        // Room on neither side: the roomier side, inside the window.
        assert_eq!(
            Placement::BelowStart.origin(
                trigger(100., 250.),
                size(px(200.), px(560.)),
                window,
                WINDOW_MARGIN
            ),
            point(px(100.), px(32.))
        );
        // A right-click near the bottom right corner opens up and left.
        assert_eq!(
            origin(Placement::At(point(px(790.), px(590.))), trigger(0., 0.)),
            point(px(590.), px(290.))
        );
    }

    #[gpui::test]
    fn a_deactivated_window_closes_the_menu_and_keeps_the_keyboard(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        let field = focus_field(&host, cx);
        click(cx, "a-trigger");
        cx.deactivate_window();
        cx.run_until_parked();
        assert_eq!(open(&host, cx), None);
        assert_eq!(focused(cx), Some(field));
    }

    /// A menu whose owner stops drawing it (a panel that was hidden) still
    /// hands focus back, though the owner never renders again.
    #[gpui::test]
    fn a_menu_whose_owner_goes_away_hands_focus_back(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        let field = focus_field(&host, cx);
        let other = host.update(cx, |host, _| host.other.clone());
        click(cx, "other-trigger");
        assert!(other.update(cx, |other, _| other.popup.is_open(&())));
        host.update(cx, |host, cx| {
            host.show_other = false;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!other.update(cx, |other, _| other.popup.is_open(&())));
        assert_eq!(focused(cx), Some(field));
    }

    #[gpui::test]
    fn one_popup_per_window_across_views(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        let other = host.update(cx, |host, _| host.other.clone());
        click(cx, "a-trigger");
        click(cx, "other-trigger");
        assert!(other.update(cx, |other, _| other.popup.is_open(&())));
        assert_eq!(open(&host, cx), None, "the pointer closed the first menu");

        // Opened without the pointer, the second popup still closes the
        // first: it takes the focus away.
        other.update(cx, |other, cx| {
            other.popup.close(cx);
        });
        host.update(cx, |host, cx| host.popup.open(Which::A, cx));
        cx.run_until_parked();
        other.update(cx, |other, cx| other.popup.open((), cx));
        cx.run_until_parked();
        assert_eq!(open(&host, cx), None);
        assert!(other.update(cx, |other, _| other.popup.is_open(&())));
    }

    /// A menu that replaces another view's menu, by a press on its button or
    /// from the keyboard, hands focus back to where the first menu began,
    /// not to the menu that closed.
    #[gpui::test]
    fn a_menu_that_replaces_another_returns_focus_where_it_began(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        let other = host.update(cx, |host, _| host.other.clone());
        for from_the_keyboard in [false, true] {
            let field = focus_field(&host, cx);
            click(cx, "a-trigger");
            if from_the_keyboard {
                other.update(cx, |other, cx| other.popup.open((), cx));
                cx.run_until_parked();
            } else {
                click(cx, "other-trigger");
            }
            assert_eq!(open(&host, cx), None);
            cx.simulate_keystrokes("escape");
            assert!(!other.update(cx, |other, _| other.popup.is_open(&())));
            assert_eq!(
                focused(cx),
                Some(field),
                "from the keyboard: {from_the_keyboard}"
            );
        }
    }

    #[gpui::test]
    fn the_keyboard_walks_picks_and_returns_focus(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        let field = focus_field(&host, cx);
        host.update(cx, |host, cx| host.popup.open(Which::A, cx));
        cx.run_until_parked();
        let menu_focus = host.update(cx, |host, _| host.popup.focus_handle().clone());
        assert_eq!(focused(cx), Some(menu_focus));

        cx.simulate_keystrokes("down");
        assert_eq!(highlighted(&host, cx), Some("a-one".into()));
        cx.simulate_keystrokes("up");
        assert_eq!(
            highlighted(&host, cx),
            Some("a-sub".into()),
            "wraps to the last item"
        );
        cx.simulate_keystrokes("home");
        assert_eq!(highlighted(&host, cx), Some("a-one".into()));
        cx.simulate_keystrokes("end");
        assert_eq!(highlighted(&host, cx), Some("a-sub".into()));
        cx.simulate_keystrokes("home down space");
        assert_eq!(host.update(cx, |host, _| host.picked.clone()), vec!["two"]);
        assert_eq!(open(&host, cx), None);
        assert_eq!(focused(cx), Some(field), "focus goes back where it was");
    }

    #[gpui::test]
    fn application_vim_walks_the_menu(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        host.update(cx, |host, cx| {
            host.vim = true;
            host.popup.open(Which::A, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("j j");
        assert_eq!(highlighted(&host, cx), Some("a-two".into()));
        cx.simulate_keystrokes("k");
        assert_eq!(highlighted(&host, cx), Some("a-one".into()));
        cx.simulate_keystrokes("G");
        assert_eq!(highlighted(&host, cx), Some("a-sub".into()));
        cx.simulate_keystrokes("g g");
        assert_eq!(highlighted(&host, cx), Some("a-one".into()));
        cx.simulate_keystrokes("enter");
        assert_eq!(host.update(cx, |host, _| host.picked.clone()), vec!["one"]);
    }

    /// Rows that arrive while the menu is open (a status that loads late)
    /// do not move the highlight to another item.
    #[gpui::test]
    fn the_highlight_stays_on_its_item_when_rows_arrive(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        host.update(cx, |host, cx| host.popup.open(Which::A, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("down down");
        assert_eq!(highlighted(&host, cx), Some("a-two".into()));
        host.update(cx, |host, cx| {
            host.lead = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        assert_eq!(host.update(cx, |host, _| host.picked.clone()), vec!["two"]);
    }

    #[gpui::test]
    fn escape_closes_and_returns_focus(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        let field = focus_field(&host, cx);
        click(cx, "a-trigger");
        cx.simulate_keystrokes("escape");
        assert_eq!(open(&host, cx), None);
        assert_eq!(focused(cx), Some(field));
    }

    #[gpui::test]
    fn the_keyboard_skips_disabled_items(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        host.update(cx, |host, cx| {
            host.second_enabled = false;
            host.popup.open(Which::A, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("down down");
        assert_eq!(highlighted(&host, cx), Some("a-three".into()));
        click(cx, "a-two");
        assert!(host.update(cx, |host, _| host.picked.is_empty()));
    }

    #[gpui::test]
    fn the_pointer_moves_the_highlight(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        host.update(cx, |host, _| host.second_enabled = false);
        click(cx, "a-trigger");
        let row = cx.debug_bounds("a-three").expect("row");
        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        assert_eq!(highlighted(&host, cx), Some("a-three".into()));
        // Nothing is highlighted over a disabled row.
        let disabled = cx.debug_bounds("a-two").expect("row");
        cx.simulate_mouse_move(disabled.center(), None, Modifiers::default());
        assert_eq!(highlighted(&host, cx), None);
        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        cx.simulate_keystrokes("enter");
        assert_eq!(
            host.update(cx, |host, _| host.picked.clone()),
            vec!["three"]
        );
    }

    #[gpui::test]
    fn a_letter_jumps_to_the_item_that_starts_with_it(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        click(cx, "a-trigger");
        cx.simulate_keystrokes("t");
        assert_eq!(highlighted(&host, cx), Some("a-two".into()));
        cx.simulate_keystrokes("t");
        assert_eq!(highlighted(&host, cx), Some("a-three".into()));
    }

    #[gpui::test]
    fn the_menu_blocks_the_pointer_from_what_lies_beneath(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        click(cx, "a-trigger");
        let menu = cx.debug_bounds("menu-a").expect("menu");
        let beneath = cx.debug_bounds("b-trigger").expect("b trigger");
        assert!(
            menu.contains(&beneath.center()),
            "the fixture puts B's trigger under A's menu"
        );
        cx.simulate_click(beneath.center(), Modifiers::default());
        assert_ne!(
            open(&host, cx),
            Some(Which::B),
            "the press went to the menu"
        );
    }

    #[gpui::test]
    fn a_press_on_a_nested_menu_keeps_its_parent_open(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        click(cx, "a-trigger");
        click(cx, "a-sub");
        assert!(host.update(cx, |host, _| host.nested.is_open(&())));
        let parent = cx.debug_bounds("menu-a").expect("parent");
        let nested = cx.debug_bounds("nested-one").expect("nested row");
        assert!(
            !parent.contains(&nested.center()),
            "the fixture puts the nested row outside its parent"
        );
        click(cx, "nested-one");
        assert_eq!(
            host.update(cx, |host, _| host.picked.clone()),
            vec!["nested"]
        );
        assert_eq!(open(&host, cx), Some(Which::A));

        // A press on the parent closes the nested menu.
        click(cx, "a-sub");
        click(cx, "a-one");
        assert!(!host.update(cx, |host, _| host.nested.is_open(&())));
    }

    /// A press on a row's own control opens that control's menu and never
    /// picks the row, even when each event arrives twice before the next
    /// frame (an automation driver delivers input that way).
    #[gpui::test]
    fn a_press_on_a_rows_control_never_picks_the_row(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        click(cx, "a-trigger");
        let position = cx.debug_bounds("a-three-more").expect("control").center();
        cx.update(|window, cx| {
            let down = MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            };
            let up = gpui::MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
            };
            for event in [
                gpui::PlatformInput::MouseDown(down.clone()),
                gpui::PlatformInput::MouseDown(down),
                gpui::PlatformInput::MouseUp(up.clone()),
                gpui::PlatformInput::MouseUp(up),
            ] {
                window.dispatch_event(event, cx);
            }
        });
        cx.run_until_parked();
        assert!(host.update(cx, |host, _| host.nested.is_open(&())));
        assert!(host.update(cx, |host, _| host.picked.is_empty()));
        assert_eq!(open(&host, cx), Some(Which::A));
    }

    #[gpui::test]
    fn a_closed_trigger_shows_its_pressed_state(cx: &mut TestAppContext) {
        let (_host, cx) = setup(cx);
        let bounds = cx.debug_bounds("a-trigger").expect("trigger");
        cx.simulate_mouse_down(bounds.center(), MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        let pressed = cx.debug_bounds("a-trigger").expect("trigger");
        assert_eq!(pressed.size.width, px(90.));
        cx.simulate_mouse_up(bounds.center(), MouseButton::Left, Modifiers::default());
    }
}
