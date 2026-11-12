//! A turn's steps, drawn as a thread under the turn's line: each thinking
//! row and tool call is a node on a rail, with a one-line header that opens
//! its details under it.

use gpui::{AnyElement, Div, Pixels, SharedString, Stateful, div, prelude::*, px};
use maple_agent::agent::AgentTimelineItem;

use crate::ui::icons::icon;
use crate::ui::theme;

/// Width of the column the nodes and the rail sit in. The mark on the
/// turn's line is centred in the same column.
pub(super) const NODE: Pixels = px(22.);

/// From a step's top to the centre of its node, where the rail meets it.
const NODE_CENTRE: Pixels = px(16.);

/// How a step reads at a glance. Only the turn's line animates: a step is
/// dim until it has run and inked once it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StepState {
    Running,
    Done,
    Failed,
    Stopped,
}

impl StepState {
    pub(super) fn of(status: Option<&str>) -> Self {
        match status {
            Some("completed") => Self::Done,
            Some("failed" | "error") => Self::Failed,
            Some("cancelled") => Self::Stopped,
            _ => Self::Running,
        }
    }

    /// Colour of the step's title.
    pub(super) fn ink(self) -> u32 {
        match self {
            Self::Done | Self::Failed => theme::text_primary(),
            Self::Running | Self::Stopped => theme::text_muted(),
        }
    }
}

/// Whether the rail runs on from a step's node to the step drawn right
/// above it, and to the one right below.
pub(super) type Rail = (bool, bool);

/// The icon on a tool call's node, by what the tool does.
pub(super) fn tool_icon(item: &AgentTimelineItem) -> &'static str {
    let name = item
        .label
        .as_ref()
        .filter(|label| label.kind == "tool")
        .and_then(|label| label.name.as_deref());
    match name {
        Some("bash" | "powershell" | "shell") => "terminal",
        Some("read" | "read_file") => "file-text",
        Some("list_files" | "glob") => "folder",
        Some("grep") => "search",
        Some(
            "edit"
            | "write"
            | "write_file"
            | "text_editor"
            | "str_replace_editor"
            | "str_replace_based_edit_tool",
        ) => "pencil",
        Some("web_search") => "globe",
        Some("load_skill") => "puzzle",
        Some(
            "agent_start" | "agent_send" | "agent_status" | "agent_cancel" | "list_agent_providers",
        ) => "users",
        _ => "wrench",
    }
}

/// One step: its node on the rail, then `header` and, when it is open,
/// `details` under it.
pub(super) fn step(
    icon_name: &'static str,
    state: StepState,
    rail: Rail,
    header: impl IntoElement,
    details: Option<AnyElement>,
) -> Div {
    let line = gpui::rgb(theme::border());
    let glyph = match state {
        StepState::Done => theme::text_secondary(),
        StepState::Failed => theme::status_error(),
        StepState::Running | StepState::Stopped => theme::text_muted(),
    };
    let segment = || div().absolute().left(px(10.)).w(px(1.)).bg(line);
    div()
        .flex()
        .gap_3()
        .child(
            // The column is as tall as the step, so the rail reaches the
            // next step's without a gap.
            div()
                .relative()
                .flex_none()
                .w(NODE)
                .pt(px(5.))
                .when(rail.0, |column| {
                    column.child(segment().top_0().h(NODE_CENTRE))
                })
                .when(rail.1, |column| {
                    column.child(segment().top(NODE_CENTRE).bottom_0())
                })
                .child(
                    // Filled with the transcript's colour, so the rail
                    // stops at the node's edge.
                    div()
                        .debug_selector(|| "step-node".to_string())
                        .relative()
                        .size(NODE)
                        .rounded_full()
                        .bg(gpui::rgb(theme::bg_app()))
                        .border_1()
                        .border_color(line)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icon(icon_name, px(12.), glyph).opacity(
                            if state == StepState::Running {
                                0.45
                            } else {
                                1.
                            },
                        )),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .py_1()
                .flex()
                .flex_col()
                .items_start()
                .child(header)
                .children(details.map(|details| div().w_full().pt_1().pb_1().child(details))),
        )
}

/// The line of a step that opens its details: as wide as what it says,
/// with a hover plate that reaches a little past it on both sides.
pub(super) fn step_header(id: SharedString) -> Stateful<Div> {
    div()
        .id(id)
        .max_w_full()
        .min_h(px(24.))
        .ml(px(-8.))
        .px_2()
        .flex()
        .items_center()
        .gap_2p5()
        .rounded(theme::RADIUS_SM)
        .text_sm()
}

/// What a tool call acts on, beside its title: a command, a path, a query.
pub(super) fn chip(text: SharedString) -> Div {
    div()
        .debug_selector(|| "step-chip".to_string())
        .min_w_0()
        .max_w(px(440.))
        .px_2()
        .py(px(2.))
        .rounded(theme::RADIUS_SM)
        .bg(theme::overlay_hover())
        .font_family(crate::assets::FONT_MONO)
        .font_weight(gpui::FontWeight::NORMAL)
        .text_xs()
        .text_color(gpui::rgb(theme::text_secondary()))
        .truncate()
        .child(gpui::text!(text))
}

/// The lines an edit adds and removes.
pub(super) fn changed((added, removed): &(SharedString, SharedString)) -> Div {
    div()
        .debug_selector(|| "step-changed".to_string())
        .flex_none()
        .flex()
        .gap_1p5()
        .font_family(crate::assets::FONT_MONO)
        .font_weight(gpui::FontWeight::NORMAL)
        .text_xs()
        .child(
            div()
                .text_color(gpui::rgb(theme::status_success()))
                .child(gpui::text!(added.clone())),
        )
        .child(
            div()
                .text_color(gpui::rgb(theme::status_error()))
                .child(gpui::text!(removed.clone())),
        )
}
