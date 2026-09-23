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
//! - A press anywhere else closes it and still reaches what was pressed.
//! - The open popup holds keyboard focus. Up and Down (`j`/`k` under
//!   Application Vim) move the highlight, Home and End (`g g`/`G`) jump,
//!   Enter or Space picks, Escape closes, and a typed letter jumps to the
//!   next row that starts with it. The pointer moves the same highlight.
//! - One popup per window. Whatever takes focus next (another popup, a
//!   dialog, a text field) closes the open one. When a popup that holds
//!   focus closes, focus returns to where it was before it opened.
//! - The panel draws above everything, blocks the pointer from what lies
//!   beneath it, and stays inside the window.
//! - Assistive technology sees a button that reports whether its popup is
//!   expanded, a menu of menu items, and the highlighted item as the
//!   focused one.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    Anchor, AnyElement, App, ClickEvent, Context, Div, ElementId, FocusHandle, HitboxBehavior,
    HitboxId, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, Role, ScrollHandle,
    SharedString, Stateful, Subscription, Toggled, Window, anchored, canvas, deferred, div, point,
    prelude::*, px,
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

/// Which of a view's popups is open, and the plumbing they share.
///
/// `V` is the owning view. `get` finds this state inside it again, so the
/// listeners a popup installs (trigger presses, menu actions, the focus
/// watch) can reach it. At most one of a view's popups is open at a time;
/// a menu that opens from inside another menu uses a second `Popup` (see
/// [`Menu::nested`]).
pub(crate) struct Popup<V: 'static, K: 'static> {
    get: fn(&mut V) -> &mut Popup<V, K>,
    open: Option<K>,
    highlighted: Option<usize>,
    focus: FocusHandle,
    /// Where focus was when the popup opened. It goes back there when the
    /// popup closes while it holds focus.
    return_focus: Option<FocusHandle>,
    /// Focus moves in on the next render, once the panel is in the tree.
    focus_pending: bool,
    /// Closes the popup when focus leaves it. Held only while it is open.
    focus_out: Option<Subscription>,
    /// The panel's hitbox in the last frame. A parent menu ignores presses
    /// on it, since this panel may reach outside the parent's bounds.
    hitbox: Rc<Cell<Option<HitboxId>>>,
    scroll: ScrollHandle,
    /// Scroll the highlighted row into view on the next render.
    reveal: Cell<bool>,
}

impl<V: 'static, K: Clone + PartialEq + 'static> Popup<V, K> {
    pub(crate) fn new(get: fn(&mut V) -> &mut Self, cx: &mut App) -> Self {
        Self {
            get,
            open: None,
            highlighted: None,
            focus: cx.focus_handle(),
            return_focus: None,
            focus_pending: false,
            focus_out: None,
            hitbox: Rc::default(),
            scroll: ScrollHandle::new(),
            reveal: Cell::new(false),
        }
    }

    pub(crate) fn is_open(&self, key: &K) -> bool {
        self.open.as_ref() == Some(key)
    }

    pub(crate) fn open_key(&self) -> Option<&K> {
        self.open.as_ref()
    }

    /// The highlighted row, counting only the menu's items.
    #[cfg(test)]
    pub(crate) fn highlighted(&self) -> Option<usize> {
        self.highlighted
    }

    #[cfg(test)]
    pub(crate) fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Open `key`'s popup, closing whichever of this view's popups was open.
    /// Nothing is highlighted until the pointer or a key picks a row.
    pub(crate) fn open(&mut self, key: K, cx: &mut Context<V>) {
        self.open_at_row(key, None, cx);
    }

    /// Open with `row` highlighted. A menu opened from the keyboard starts
    /// on the current choice, the way a native popup button does.
    pub(crate) fn open_at_row(&mut self, key: K, row: Option<usize>, cx: &mut Context<V>) {
        if self.open.as_ref() != Some(&key) {
            self.scroll.set_offset(point(px(0.), px(0.)));
        }
        self.open = Some(key);
        self.highlighted = row;
        self.reveal.set(row.is_some());
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
    /// focus, focus goes back on the next render.
    pub(crate) fn close(&mut self, cx: &mut Context<V>) -> bool {
        self.focus_out = None;
        self.focus_pending = false;
        self.highlighted = None;
        if self.open.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    /// Highlight `row`, or nothing.
    pub(crate) fn highlight(&mut self, row: Option<usize>, cx: &mut Context<V>) {
        if self.highlighted != row {
            self.highlighted = row;
            cx.notify();
        }
    }

    /// Move focus into a popup that just opened, and back out of one that
    /// closed while holding it. Call this first in the owning view's render
    /// so that focus requested later in the same render wins.
    pub(crate) fn sync_focus(&mut self, window: &mut Window, cx: &mut Context<V>) {
        if self.open.is_some() {
            if !std::mem::take(&mut self.focus_pending) {
                return;
            }
            // Reopening from inside itself keeps the original return target.
            if !self.focus.contains_focused(window, cx) {
                self.return_focus = window.focused(cx);
            }
            window.focus(&self.focus, cx);
            let get = self.get;
            self.focus_out =
                Some(
                    cx.on_focus_out(&self.focus, window, move |view, _, window, cx| {
                        // Something else took focus: another popup, a dialog, a
                        // field. Focus stays where it went. If it did not move,
                        // the window went inactive, and the popup closes the way
                        // a native menu does, handing focus back as usual.
                        let moved_on = !get(view).focus.is_focused(window);
                        // Focus events arrive while the frame draws, when a
                        // notify is dropped; close once the frame is done.
                        cx.defer_in(window, move |view, _, cx| {
                            let popup = get(view);
                            if moved_on {
                                popup.return_focus = None;
                            }
                            popup.close(cx);
                        });
                    }),
                );
        } else if self.focus.contains_focused(window, cx) {
            // The last frame still has the panel, so this tells whether the
            // popup held focus when it closed.
            match self.return_focus.take() {
                Some(handle) => window.focus(&handle, cx),
                None => window.blur(),
            }
        } else {
            self.return_focus = None;
        }
    }

    /// Wire `button` as the opener of `key`'s popup.
    ///
    /// `toggle` runs on click, and on Enter or Space when the button has
    /// focus. While the popup is open, a left press on the button closes it
    /// in the capture phase and is consumed. Without that, the popup's
    /// outside-press handler would close it and the click would open it
    /// again. The press is taken only while the popup is open, so a closed
    /// trigger stays an ordinary button: it shows its pressed state, and any
    /// other open popup still sees the press and closes.
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
        cx: &mut Context<V>,
    ) -> AnyElement {
        let get = self.get;
        let Menu {
            id,
            width,
            max_height,
            label,
            entries,
            nested,
            priority,
            application_vim,
        } = menu;

        // What the keyboard needs about each item: its action, whether it
        // can be picked, the letter it starts with, and its child index for
        // scrolling it into view.
        let mut picks: Vec<Pick<V>> = Vec::new();
        let mut children: Vec<AnyElement> = Vec::with_capacity(entries.len());
        for entry in entries {
            match entry {
                Entry::Item(item) => {
                    let row = picks.len();
                    picks.push(Pick {
                        on_select: item.on_select.clone(),
                        enabled: item.enabled,
                        keep_open: item.keep_open,
                        initial: item
                            .label
                            .chars()
                            .next()
                            .map(|c| c.to_lowercase().collect()),
                        child: children.len(),
                    });
                    children.push(item_row(item, row, self.highlighted == Some(row), get, cx));
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
        if self.reveal.take()
            && let Some(item) = self.highlighted.and_then(|row| picks.get(row))
        {
            self.scroll.scroll_to_item(item.child);
        }
        let picks = Rc::new(picks);

        let panel = widgets::popup_panel(id.clone(), width)
            .debug_selector(|| id.to_string())
            .key_context(if application_vim {
                MENU_VIM_CONTEXT
            } else {
                MENU_CONTEXT
            })
            .track_focus(&self.focus)
            .when_some(label, |panel, label| panel.aria_label(label))
            .when_some(max_height, |panel, height| {
                panel
                    .max_h(height)
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
            })
            .on_mouse_down_out(cx.listener(move |view, _: &MouseDownEvent, window, cx| {
                // A menu opened from this one may reach past its edge.
                let on_nested = nested
                    .as_ref()
                    .and_then(|hitbox| hitbox.get())
                    .is_some_and(|hitbox| hitbox.is_hovered(window));
                if !on_nested {
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
                    let Some(row) = get(view).highlighted else {
                        return;
                    };
                    pick(view, get, &picks, row, window, cx);
                })
            })
            .on_action(cx.listener(move |view, _: &Cancel, _, cx| {
                get(view).close(cx);
            }))
            .on_key_down({
                let picks = picks.clone();
                let focus = self.focus.clone();
                cx.listener(move |view, event: &KeyDownEvent, window, cx| {
                    type_ahead(view, get, &picks, &focus, event, window, cx);
                })
            })
            .children(children);

        // A sibling after the panel, covering it, records the panel's hitbox
        // for a parent menu to test. The panel itself may scroll, so the
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
        let panel = motion::fade_in(panel, "popup-reveal").into_any_element();
        place(panel, placement, priority)
    }
}

impl<V: 'static, K: Clone + PartialEq + 'static> Popup<V, K> {
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

/// Where a menu opens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Placement {
    /// Below the trigger, sharing its left edge. The menu element must be
    /// a child of a `relative` box around the trigger.
    BelowStart,
    /// Below the trigger, sharing its right edge.
    BelowEnd,
    /// Above the trigger, sharing its left edge.
    AboveStart,
    /// At a point in window coordinates, for a right-click.
    At(Point<Pixels>),
}

/// Put the panel at `placement`, drawn above everything and kept inside
/// the window.
fn place(panel: AnyElement, placement: Placement, priority: usize) -> AnyElement {
    let floating = anchored().snap_to_window_with_margin(WINDOW_MARGIN);
    // Everything but a right-click hangs off a zero-size box on the
    // trigger's edge, which gives the anchored element its origin.
    let (edge, floating) = match placement {
        Placement::At(position) => {
            return deferred(floating.position(position).child(panel))
                .with_priority(priority)
                .into_any_element();
        }
        Placement::BelowStart => (
            div().absolute().top_full().left_0(),
            floating.anchor(Anchor::TopLeft).offset(point(px(0.), GAP)),
        ),
        Placement::BelowEnd => (
            div().absolute().top_full().right_0(),
            floating.anchor(Anchor::TopRight).offset(point(px(0.), GAP)),
        ),
        Placement::AboveStart => (
            div().absolute().top_0().left_0(),
            floating
                .anchor(Anchor::BottomLeft)
                .offset(point(px(0.), -GAP)),
        ),
    };
    edge.child(deferred(floating.child(panel)).with_priority(priority))
        .into_any_element()
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
    nested: Option<Rc<Cell<Option<HitboxId>>>>,
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
            nested: None,
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

    /// `child` opens from inside this menu. A press on it is not a press
    /// outside this menu, even where it reaches past this menu's edge, and
    /// it draws above this menu.
    pub(crate) fn nested<K2: Clone + PartialEq + 'static>(mut self, child: &Popup<V, K2>) -> Self {
        self.nested = Some(child.hitbox.clone());
        self
    }

    /// Draw above menus of a lower level. A nested menu passes 2.
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

    /// A disabled row is dimmed and cannot be picked.
    pub(crate) fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Content at the row's end, after the label.
    pub(crate) fn trailing(mut self, element: impl IntoElement) -> Self {
        self.trailing = Some(element.into_any_element());
        self
    }

    /// Adjust the row, for a font preview or an ellipsis at the start.
    pub(crate) fn style(
        mut self,
        style: impl FnOnce(Stateful<Div>) -> Stateful<Div> + 'static,
    ) -> Self {
        self.style = Some(Box::new(style));
        self
    }
}

struct Pick<V: 'static> {
    on_select: OnSelect<V>,
    enabled: bool,
    keep_open: bool,
    initial: Option<String>,
    child: usize,
}

fn item_row<V: 'static, K: Clone + PartialEq + 'static>(
    item: MenuItem<V>,
    row: usize,
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
    let row_element = div()
        .debug_selector(|| id.to_string())
        .id(id)
        .role(role)
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
        .on_hover(cx.listener(move |view, hovered: &bool, _, cx| {
            let popup = get(view);
            if *hovered {
                popup.highlight(Some(row), cx);
            } else if popup.highlighted == Some(row) {
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
            Some(content) => row
                .aria_label(label)
                .child(div().flex_1().min_w_0().child(content)),
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
        .children(trailing)
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
        Step::By(delta) => match popup.highlighted {
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
            popup.highlight(Some(at as usize), cx);
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
    let typed = typed.to_lowercase();
    let popup = get(view);
    let rows = picks.len();
    let start = popup.highlighted.map_or(0, |row| row + 1);
    let found = (0..rows)
        .map(|offset| (start + offset) % rows)
        .find(|&row| picks[row].enabled && picks[row].initial.as_deref() == Some(typed.as_str()));
    if let Some(row) = found {
        popup.scroll.scroll_to_item(picks[row].child);
        popup.highlight(Some(row), cx);
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
        picked: Vec<&'static str>,
        outside_clicks: usize,
        field: FocusHandle,
        second_enabled: bool,
    }

    impl Host {
        fn menu_a(&self) -> Menu<Host> {
            let pick = |name: &'static str| {
                move |this: &mut Host, _: &mut Window, _: &mut Context<Host>| this.picked.push(name)
            };
            Menu::new("menu-a", px(160.))
                .item(MenuItem::new("a-one", "One", pick("one")))
                .item(MenuItem::new("a-two", "Two", pick("two")).enabled(self.second_enabled))
                .separator()
                .item(MenuItem::new("a-three", "Three", pick("three")))
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
                .nested(&self.nested)
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
                let mut menu = self.menu_a();
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
                                cx,
                            ),
                        ),
                    );
                }
                self.popup.render(menu, Placement::BelowStart, cx)
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
                    cx,
                )
            });
            div()
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
                        .child(self.other.clone()),
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
                picked: Vec::new(),
                outside_clicks: 0,
                field: cx.focus_handle(),
                second_enabled: true,
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

    #[gpui::test]
    fn a_deactivated_window_closes_the_menu_and_keeps_the_keyboard(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        let field = host.update(cx, |host, _| host.field.clone());
        cx.update(|window, cx| window.focus(&field, cx));
        click(cx, "a-trigger");
        cx.deactivate_window();
        cx.run_until_parked();
        assert_eq!(open(&host, cx), None);
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(field));
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

    #[gpui::test]
    fn the_keyboard_walks_picks_and_returns_focus(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        let field = host.update(cx, |host, _| host.field.clone());
        cx.update(|window, cx| window.focus(&field, cx));
        host.update(cx, |host, cx| host.popup.open(Which::A, cx));
        cx.run_until_parked();
        let menu_focus = host.update(cx, |host, _| host.popup.focus_handle().clone());
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(menu_focus));

        cx.simulate_keystrokes("down");
        assert_eq!(host.update(cx, |host, _| host.popup.highlighted()), Some(0));
        cx.simulate_keystrokes("up");
        assert_eq!(
            host.update(cx, |host, _| host.popup.highlighted()),
            Some(3),
            "wraps to the last item"
        );
        cx.simulate_keystrokes("home down enter");
        assert_eq!(host.update(cx, |host, _| host.picked.clone()), vec!["two"]);
        assert_eq!(open(&host, cx), None);
        assert_eq!(
            cx.update(|window, cx| window.focused(cx)),
            Some(field),
            "focus goes back where it was"
        );
    }

    #[gpui::test]
    fn escape_closes_and_returns_focus(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        let field = host.update(cx, |host, _| host.field.clone());
        cx.update(|window, cx| window.focus(&field, cx));
        click(cx, "a-trigger");
        cx.simulate_keystrokes("escape");
        assert_eq!(open(&host, cx), None);
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(field));
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
        assert_eq!(host.update(cx, |host, _| host.popup.highlighted()), Some(2));
        click(cx, "a-two");
        assert!(host.update(cx, |host, _| host.picked.is_empty()));
    }

    #[gpui::test]
    fn the_pointer_moves_the_highlight(cx: &mut TestAppContext) {
        let (host, cx) = setup(cx);
        click(cx, "a-trigger");
        let row = cx.debug_bounds("a-three").expect("row");
        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        assert_eq!(host.update(cx, |host, _| host.popup.highlighted()), Some(2));
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
        assert_eq!(host.update(cx, |host, _| host.popup.highlighted()), Some(1));
        cx.simulate_keystrokes("t");
        assert_eq!(host.update(cx, |host, _| host.popup.highlighted()), Some(2));
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
