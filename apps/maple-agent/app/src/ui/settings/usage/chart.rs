//! The Usage page's charts: bars and segments are plain elements, so gpui
//! gives them hit testing, hover and tooltips.

use std::rc::Rc;

use gpui::{
    Anchor, AnyView, App, Div, Hsla, Pixels, Point, SharedString, Window, anchored, div,
    prelude::*, px, relative,
};

use super::model::NiceScale;
use crate::ui::theme;

/// Width of the value axis beside a plot.
const AXIS_WIDTH: Pixels = px(44.);
const AXIS_TEXT: Pixels = px(10.5);

/// A chart tooltip: a title, an optional headline value, colored rows and
/// trailing notes.
pub(super) struct Tip {
    pub(super) title: SharedString,
    pub(super) headline: Option<SharedString>,
    pub(super) rows: Vec<(u32, SharedString, SharedString)>,
    pub(super) notes: Vec<SharedString>,
}

struct ChartTooltip {
    tip: Tip,
    pointer: Point<Pixels>,
}

impl Render for ChartTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let tip = &self.tip;
        let panel = div()
            .debug_selector(|| "usage-tooltip".into())
            .flex()
            .flex_col()
            .gap_1()
            .min_w(px(168.))
            .max_w(px(300.))
            .px_3()
            .py_2()
            .rounded(theme::RADIUS_SM)
            .bg(gpui::rgb(theme::bg_elevated()))
            .border_1()
            .border_color(gpui::rgb(theme::border()))
            .shadow_md()
            .text_xs()
            .text_color(gpui::rgb(theme::text_primary()))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(tip.title.clone()),
                    )
                    .children(tip.headline.clone().map(|headline| {
                        div()
                            .text_color(gpui::rgb(theme::text_secondary()))
                            .child(headline)
                    })),
            )
            .children(tip.rows.iter().map(|(color, label, value)| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(swatch(*color, px(8.)))
                    .child(
                        div()
                            .flex_1()
                            .text_color(gpui::rgb(theme::text_secondary()))
                            .child(label.clone()),
                    )
                    .child(div().child(value.clone()))
            }))
            .children(tip.notes.iter().map(|note| {
                div()
                    .pt_1()
                    .text_color(gpui::rgb(theme::text_muted()))
                    .child(note.clone())
            }));
        // Anchored at the pointer like the app's other tooltips: centered
        // above it, flipping below when there is no room.
        div().child(
            anchored()
                .position(self.pointer)
                .anchor(Anchor::BottomCenter)
                .child(div().pt(px(20.)).pb(px(16.)).child(panel)),
        )
    }
}

/// A tooltip built when it shows, so nothing is formatted for marks the
/// pointer never visits.
pub(super) fn tooltip(
    build: impl Fn() -> Tip + 'static,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |window, cx| {
        let tip = build();
        let pointer = window.mouse_position();
        cx.new(|_| ChartTooltip { tip, pointer }).into()
    }
}

pub(super) fn swatch(color: u32, size: Pixels) -> Div {
    div()
        .flex_none()
        .size(size)
        .rounded(px(3.))
        .bg(gpui::rgb(color))
}

fn axis_label(text: SharedString) -> Div {
    div()
        .text_size(AXIS_TEXT)
        .line_height(px(14.))
        .whitespace_nowrap()
        .text_color(gpui::rgb(theme::text_muted()))
        .child(text)
}

fn alpha(color: u32, a: f32) -> Hsla {
    let mut color: Hsla = gpui::rgb(color).into();
    color.a = a;
    color
}

/// The value axis's labels and the plot's horizontal grid lines.
fn value_grid(scale: &NiceScale, labels: &[SharedString]) -> (Div, Vec<Div>) {
    let max = scale.max.max(1.0);
    let axis = div()
        .absolute()
        .left_0()
        .top_0()
        .bottom_0()
        .w(AXIS_WIDTH)
        .children(scale.ticks.iter().zip(labels).map(|(tick, label)| {
            div()
                .absolute()
                .right(px(8.))
                .top(relative((1.0 - tick / max) as f32))
                .mt(px(-7.))
                .child(axis_label(label.clone()))
        }));
    let lines = scale
        .ticks
        .iter()
        .map(|tick| {
            div()
                .absolute()
                .left_0()
                .right_0()
                .h(px(1.))
                .top(relative((1.0 - tick / max) as f32))
                .bg(gpui::rgb(if *tick == 0.0 {
                    super::palette::axis()
                } else {
                    super::palette::grid()
                }))
        })
        .collect();
    (axis, lines)
}

/// One column of a stacked bar chart.
pub(super) struct Column {
    /// `(color, value)` from the bottom of the stack up.
    pub(super) segments: Vec<(u32, f64)>,
    pub(super) total: f64,
    pub(super) tick: SharedString,
}

pub(super) struct Bars {
    pub(super) id: &'static str,
    pub(super) label: SharedString,
    pub(super) columns: Vec<Column>,
    pub(super) scale: NiceScale,
    pub(super) axis: Vec<SharedString>,
    pub(super) height: Pixels,
    pub(super) max_bar_width: Pixels,
    /// Label every n-th column, always including the last.
    pub(super) label_every: usize,
    /// A horizontal limit line and its label.
    pub(super) reference: Option<(f64, SharedString)>,
    /// A vertical marker before a column, and its label.
    pub(super) marker: Option<(usize, SharedString)>,
    /// A bracket under a run of columns, its color and its label.
    pub(super) span: Option<(usize, usize, u32, SharedString)>,
}

/// Stacked bars with a value axis, hover and a tooltip per column.
pub(super) fn bars(chart: Bars, tip: Rc<dyn Fn(usize) -> Tip>) -> gpui::Stateful<Div> {
    let Bars {
        id,
        label,
        columns,
        scale,
        axis,
        height,
        max_bar_width,
        label_every,
        reference,
        marker,
        span,
    } = chart;
    let count = columns.len().max(1);
    let max = scale.max.max(1.0);
    let (axis, lines) = value_grid(&scale, &axis);
    let every = label_every.max(1);
    let last = count - 1;
    let ticks: Vec<SharedString> = columns.iter().map(|column| column.tick.clone()).collect();
    let top_room = if marker.is_some() { 18. } else { 6. };
    let mut plot = div()
        .absolute()
        .left(AXIS_WIDTH)
        .right_0()
        .top_0()
        .bottom_0()
        .children(lines);
    if let Some((value, text)) = reference {
        plot = plot.child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(relative((1.0 - (value / max).min(1.0)) as f32))
                .h(px(1.))
                .bg(alpha(theme::text_primary(), 0.55))
                .child(
                    div()
                        .absolute()
                        .right_0()
                        .bottom(px(3.))
                        .child(axis_label(text)),
                ),
        );
    }
    if let Some((index, text)) = marker {
        plot = plot.child(
            div()
                .absolute()
                .top(px(-14.))
                .bottom_0()
                .left(relative(index as f32 / count as f32))
                .w(px(1.))
                .bg(gpui::rgb(theme::text_muted()))
                .child(
                    // The label sits on the side of the line with room for
                    // it, so a marker near the right edge is not cut off.
                    div()
                        .absolute()
                        .top(px(-2.))
                        .map(|label| {
                            if index * 2 > count {
                                label.right(px(4.))
                            } else {
                                label.left(px(4.))
                            }
                        })
                        .child(axis_label(text)),
                ),
        );
    }
    plot = plot.child(div().absolute().inset_0().flex().items_end().children(
        columns.into_iter().enumerate().map(|(index, column)| {
            let tip = Rc::clone(&tip);
            let share = (column.total / max).clamp(0.0, 1.0) as f32;
            let total = column.total;
            let stack = div()
                .w(relative(0.72))
                .max_w(max_bar_width)
                .min_w(px(2.))
                .h(relative(share))
                .flex()
                .flex_col_reverse()
                .rounded_t(px(4.))
                .overflow_hidden()
                .children(
                    column
                        .segments
                        .into_iter()
                        .filter(|(_, value)| *value > 0.0)
                        .enumerate()
                        .map(move |(segment, (color, value))| {
                            div()
                                .flex_none()
                                .w_full()
                                .h(relative((value / total) as f32))
                                .bg(gpui::rgb(color))
                                .when(segment > 0, |segment| {
                                    segment
                                        .border_b_1()
                                        .border_color(gpui::rgb(theme::bg_elevated()))
                                })
                        }),
                );
            div()
                .id(SharedString::from(format!("{id}-{index}")))
                .debug_selector(move || format!("{id}-{index}"))
                .flex_1()
                .h_full()
                .flex()
                .flex_col()
                .justify_end()
                .items_center()
                .rounded(px(4.))
                .hover(|style| style.bg(theme::overlay_hover()))
                .tooltip(tooltip(move || tip(index)))
                .child(stack)
        }),
    ));
    let tick_row =
        div()
            .flex()
            .pl(AXIS_WIDTH)
            .pt_1()
            .h(px(20.))
            .children(ticks.into_iter().enumerate().map(|(index, tick)| {
                let shown = index % every == last % every;
                // A label wider than its column spills evenly to both sides.
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .justify_center()
                    .when(shown, |cell| cell.child(axis_label(tick).flex_none()))
            }));
    let mut figure = div()
        .id(id)
        .role(gpui::Role::Figure)
        .aria_label(label)
        .flex()
        .flex_col()
        .w_full()
        .pt(px(top_room))
        .child(div().relative().w_full().h(height).child(axis).child(plot))
        .child(tick_row);
    if let Some((from, to, color, text)) = span {
        figure = figure.child(
            div()
                .relative()
                .ml(AXIS_WIDTH)
                .h(px(30.))
                .child(
                    div()
                        .absolute()
                        .top(px(6.))
                        .left(relative(from as f32 / count as f32))
                        .w(relative((to + 1 - from) as f32 / count as f32))
                        .h(px(3.))
                        .rounded_full()
                        .bg(gpui::rgb(color)),
                )
                .child({
                    // Hang the label off whichever end keeps it in the chart.
                    let label = div().absolute().top(px(13.)).child(axis_label(text));
                    if from * 2 > count {
                        label.right(relative(1.0 - (to + 1) as f32 / count as f32))
                    } else {
                        label.left(relative(from as f32 / count as f32))
                    }
                }),
        );
    }
    figure
}

/// A thin horizontal share bar for table rows.
pub(super) fn share_bar(fraction: f64, color: u32) -> Div {
    div()
        .w(px(64.))
        .h(px(6.))
        .rounded_full()
        .bg(gpui::rgb(theme::bg_sidebar_pill()))
        .child(
            div()
                .h_full()
                .rounded_full()
                .w(relative(fraction.clamp(0.01, 1.0) as f32))
                .bg(gpui::rgb(color)),
        )
}
