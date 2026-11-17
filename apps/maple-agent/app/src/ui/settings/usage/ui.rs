//! Building blocks the Usage page's sections share: headings, legends,
//! segmented controls, tables and loading and empty states.

use super::*;
use crate::ui::motion::HoverBg as _;

pub(super) fn card_heading(title: SharedString, description: SharedString) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_0p5()
        .child(subsection_title(&title))
        .child(
            div()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(description),
        )
}

/// A share column: a bar scaled to the largest row, and the percent of
/// the total.
pub(super) fn share_cell(value: f64, peak: f64, total: f64, color: u32) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .justify_end()
        .gap_2()
        .child(chart::share_bar(
            if peak > 0.0 { value / peak } else { 0.0 },
            color,
        ))
        .child(
            div()
                .w(px(32.))
                .text_right()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(format!("{}%", percent(value, total))),
        )
        .into_any_element()
}

pub(super) fn legend_item(color: u32, label: SharedString) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1p5()
        .text_xs()
        .text_color(gpui::rgb(theme::text_secondary()))
        .child(chart::swatch(color, px(9.)))
        .child(label)
}

/// A labelled row of pills where one is chosen.
pub(super) fn segmented(
    id: &'static str,
    label: SharedString,
    pills: Vec<gpui::AnyElement>,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(label.clone()),
        )
        .child(
            div()
                .id(id)
                .role(gpui::Role::TabList)
                .aria_label(label)
                .flex()
                .items_center()
                .gap_0p5()
                .p_0p5()
                .rounded_full()
                .bg(gpui::rgb(theme::bg_input()))
                .border_1()
                .border_color(gpui::rgb(theme::border_subtle()))
                .children(pills),
        )
}

/// One choice in [`segmented`]: a tab stop that picks on a click, Space or
/// Enter, like the Appearance theme pills.
pub(super) fn segment_pill(
    id: SharedString,
    label: SharedString,
    active: bool,
    tab_index: isize,
    select: impl Fn(&mut SettingsScreen, &mut Context<SettingsScreen>) + Copy + 'static,
    cx: &mut Context<SettingsScreen>,
) -> crate::ui::motion::HoverBackground {
    let selector = id.to_string();
    div()
        .id(id)
        .debug_selector(move || selector.clone())
        .role(gpui::Role::Tab)
        .aria_label(label.clone())
        .aria_selected(active)
        .tab_index(tab_index)
        .px_2p5()
        .py_0p5()
        .rounded_full()
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .bg(gpui::rgb(if active {
            theme::bg_default_button()
        } else {
            theme::bg_input()
        }))
        .text_color(gpui::rgb(if active {
            theme::text_on_default_button()
        } else {
            theme::text_secondary()
        }))
        .when(!active, |pill| {
            pill.hover(|style| {
                style
                    .text_color(gpui::rgb(theme::text_primary()))
                    .cursor_pointer()
            })
        })
        // The selected pill keeps its fill under the pointer.
        .hover_bg(gpui::rgb(if active {
            theme::bg_default_button()
        } else {
            theme::bg_sidebar_row_hover()
        }))
        .focus_visible(widgets::focus_ring)
        .on_click(cx.listener(move |this, _event, _window, cx| select(this, cx)))
        .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
            if super::super::activates(event) {
                cx.stop_propagation();
                select(this, cx);
            }
        }))
        .child(label)
}

pub(super) fn loading_block(id: &'static str, text: SharedString) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .gap_2()
        .h(px(120.))
        .text_sm()
        .text_color(gpui::rgb(theme::text_muted()))
        .child(spinner(id, px(14.), theme::text_muted()))
        .child(text)
}

pub(super) fn empty_note(text: SharedString) -> Div {
    div()
        .text_sm()
        .text_color(gpui::rgb(theme::text_muted()))
        .child(text)
}

#[derive(Clone, Copy)]
pub(super) enum Column {
    Name,
    Time,
    Source,
    Number,
    Price,
    Share,
}

pub(super) fn table_row(cells: impl IntoIterator<Item = (Column, gpui::AnyElement)>) -> Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .text_sm()
        .children(cells.into_iter().map(|(column, cell)| {
            let slot = div().child(cell);
            match column {
                Column::Name => slot.flex_1().min_w(px(140.)),
                Column::Time => slot.w(px(132.)).flex_none(),
                Column::Source => slot.w(px(136.)).flex_none(),
                // Figures and their headers align right; a header longer than
                // its column ("Entrada en caché") wraps instead of running
                // into the next one.
                Column::Number => slot
                    .w(px(84.))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .text_right(),
                Column::Price => slot
                    .w(px(104.))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .text_right(),
                Column::Share => slot
                    .w(px(108.))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .text_right(),
            }
        }))
}

pub(super) fn header_cell(label: SharedString) -> gpui::AnyElement {
    div()
        // May shrink to its column, so a long label wraps.
        .min_w(px(0.))
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(gpui::rgb(theme::text_muted()))
        .child(label)
        .into_any_element()
}

pub(super) fn number_cell(text: String) -> gpui::AnyElement {
    div()
        .text_right()
        .whitespace_nowrap()
        .text_color(gpui::rgb(theme::text_primary()))
        .child(text)
        .into_any_element()
}
