//! The files a turn changed, listed in a card under its reply.
//!
//! A turn's edits and writes each say what they did on their own step.
//! Once the turn is over, the card adds them up by file, so what changed is
//! one glance away without opening the steps. A file's row opens the
//! changes the turn made to it.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{Context, Div, SharedString, div, prelude::*, px};
use maple_agent::agent::AgentTimelineItem;

use super::ChatScreen;
use super::step::{self, StepState};
use super::transcript::diff_rows;
use super::turn::TurnRow;
use crate::ui::i18n::MessageCache;
use crate::ui::motion::{self, HoverBg as _};
use crate::ui::theme;

/// The +/- lines of an edit or a write, as its row derives them.
type DiffLines = Vec<(char, SharedString)>;

/// One file a turn changed.
struct FileChange {
    /// The path as the tool call gave it.
    path: SharedString,
    name: SharedString,
    /// The folders before the name, with their trailing slash.
    folder: SharedString,
    /// "+6" and "−2".
    counts: (SharedString, SharedString),
    /// The +/- lines of every step that changed the file, in order.
    lines: DiffLines,
}

/// What a turn changed, file by file in the order it first touched them.
pub(super) struct TurnChanges {
    files: Vec<FileChange>,
    counts: (SharedString, SharedString),
}

/// The card a reply row carries.
pub(super) struct ReplyChanges<'a> {
    pub(super) changes: Rc<TurnChanges>,
    /// The id of the turn's lead row, which the card's state is kept under.
    pub(super) lead: &'a str,
    /// The files whose changes show.
    pub(super) open: Option<&'a BTreeSet<usize>>,
}

/// The path an edit or a write call names.
fn changed_path(item: &AgentTimelineItem) -> Option<&str> {
    let input = item.input.as_ref()?.as_object()?;
    if !input.contains_key("edits") && !input.contains_key("content") {
        return None;
    }
    input.get("path")?.as_str()
}

fn counts(added: usize, removed: usize) -> (SharedString, SharedString) {
    (
        format!("+{added}").into(),
        format!("\u{2212}{removed}").into(),
    )
}

impl TurnChanges {
    /// Add up the edits and writes among a turn's `steps` that went
    /// through, each with the +/- lines its row derived. `None` when the
    /// turn changed nothing.
    pub(super) fn of<'a>(
        steps: impl Iterator<Item = (&'a AgentTimelineItem, Arc<DiffLines>)>,
    ) -> Option<Self> {
        struct Sum<'a> {
            path: &'a str,
            added: usize,
            removed: usize,
            lines: DiffLines,
        }
        let mut sums: Vec<Sum> = Vec::new();
        for (item, lines) in steps {
            // A call that failed or was refused changed nothing.
            if StepState::of(item.status.as_deref()) != StepState::Done {
                continue;
            }
            let Some(path) = changed_path(item) else {
                continue;
            };
            let index = sums
                .iter()
                .position(|sum| sum.path == path)
                .unwrap_or_else(|| {
                    sums.push(Sum {
                        path,
                        added: 0,
                        removed: 0,
                        lines: Vec::new(),
                    });
                    sums.len() - 1
                });
            let sum = &mut sums[index];
            // Counted from the whole edit; the lines shown stop at a cap.
            let (added, removed) = super::transcript::diff_counts(item);
            sum.added += added;
            sum.removed += removed;
            // All but the line that names the file, which the row does.
            sum.lines.extend(
                lines
                    .iter()
                    .filter(|(sign, _)| matches!(sign, '+' | '-'))
                    .cloned(),
            );
        }
        if sums.is_empty() {
            return None;
        }
        let total = sums.iter().fold((0, 0), |(added, removed), sum| {
            (added + sum.added, removed + sum.removed)
        });
        Some(Self {
            counts: counts(total.0, total.1),
            files: sums
                .into_iter()
                .map(|sum| {
                    let name = sum.path.rfind(['/', '\\']).map_or(0, |at| at + 1);
                    FileChange {
                        path: sum.path.to_owned().into(),
                        name: sum.path[name..].to_owned().into(),
                        folder: sum.path[..name].to_owned().into(),
                        counts: counts(sum.added, sum.removed),
                        lines: sum.lines,
                    }
                })
                .collect(),
        })
    }

    /// Each file's path with its "+n" and "−n".
    #[cfg(test)]
    pub(super) fn files(&self) -> Vec<(&str, &str, &str)> {
        self.files
            .iter()
            .map(|file| {
                (
                    file.path.as_ref(),
                    file.counts.0.as_ref(),
                    file.counts.1.as_ref(),
                )
            })
            .collect()
    }
}

/// Each finished turn's changes, by the id of its lead row. An entry is
/// dropped when one of the turn's steps changes, and built again the next
/// time its card draws.
#[derive(Default)]
pub(super) struct ChangesCache(RefCell<HashMap<String, Option<Rc<TurnChanges>>>>);

impl ChangesCache {
    pub(super) fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    pub(super) fn forget(&self, lead: &str) {
        self.0.borrow_mut().remove(lead);
    }

    fn get(
        &self,
        lead: &str,
        build: impl FnOnce() -> Option<TurnChanges>,
    ) -> Option<Rc<TurnChanges>> {
        if let Some(changes) = self.0.borrow().get(lead) {
            return changes.clone();
        }
        let changes = build().map(Rc::new);
        self.0.borrow_mut().insert(lead.to_owned(), changes.clone());
        changes
    }
}

/// The card under a turn's reply. `reply` is the id of the row it sits on.
pub(super) fn render_changes(
    card: &ReplyChanges,
    reply: &str,
    chat: &gpui::WeakEntity<ChatScreen>,
    messages: &MessageCache,
) -> Div {
    const ROW: gpui::Pixels = px(34.);
    let rule = gpui::rgb(theme::border_subtle());
    let changes = &card.changes;
    let mut body = div()
        .debug_selector(|| "turn-changes".to_string())
        .mt_2()
        .w_full()
        .max_w(px(620.))
        .rounded(theme::RADIUS_MD)
        .bg(gpui::rgb(theme::bg_tool_card()))
        .border_1()
        .border_color(rule)
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(
            div()
                .h(ROW)
                .px_3()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .text_xs()
                .text_color(gpui::rgb(theme::text_muted()))
                .child(gpui::text!(messages.get(
                    "chat-turn-files-changed",
                    [("count", changes.files.len().into())],
                )))
                .child(step::changed(&changes.counts)),
        );
    for (index, file) in changes.files.iter().enumerate() {
        let shown = card.open.is_some_and(|open| open.contains(&index));
        let (chat, reply, lead) = (chat.clone(), reply.to_owned(), card.lead.to_owned());
        body = body.child(
            div()
                .id(SharedString::from(format!(
                    "turn-change-{}-{index}",
                    card.lead
                )))
                .debug_selector(|| "turn-change".to_string())
                .role(gpui::Role::DisclosureTriangle)
                .aria_label(file.path.clone())
                .aria_expanded(shown)
                .h(ROW)
                .px_3()
                .flex()
                .items_center()
                .gap_2p5()
                .border_t_1()
                .border_color(rule)
                .font_family(crate::assets::FONT_MONO)
                .text_xs()
                .cursor_pointer()
                .child(motion::chevron(
                    SharedString::from(format!("turn-change-chevron-{}-{index}", card.lead)),
                    shown,
                    px(12.),
                    theme::text_muted(),
                ))
                .child(
                    div()
                        .flex_none()
                        .text_color(gpui::rgb(theme::text_primary()))
                        .child(file.name.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(gpui::rgb(theme::text_muted()))
                        .child(file.folder.clone()),
                )
                .child(step::changed(&file.counts))
                .hover_bg(theme::overlay_hover())
                .on_click(move |_event, _window, cx: &mut gpui::App| {
                    chat.update(cx, |chat, cx| chat.toggle_change(&reply, &lead, index, cx))
                        .ok();
                }),
        );
        if shown {
            body = body.child(
                div()
                    .debug_selector(|| "turn-change-diff".to_string())
                    .py_1()
                    .border_t_1()
                    .border_color(rule)
                    .bg(gpui::rgb(theme::bg_code_block()))
                    .child(diff_rows(&file.lines)),
            );
        }
    }
    body
}

impl ChatScreen {
    /// The changes to list under the row at `reply`, with the lead of
    /// their turn: the row is the reply that ends a turn, the turn is
    /// over, and it changed files.
    pub(super) fn changes_under(&self, reply: usize) -> Option<(usize, Rc<TurnChanges>)> {
        let lead = self.turns.reply_of(reply)?;
        if self.turn_runs(lead) {
            return None;
        }
        let lead_id = &self.timeline.get(lead)?.id;
        let changes = self.turn_changes.get(lead_id, || {
            TurnChanges::of(
                self.timeline[lead..reply]
                    .iter()
                    .enumerate()
                    .filter(|(offset, _)| self.turns.row(lead + offset) != TurnRow::Plain)
                    .map(|(_, item)| {
                        let revision = self
                            .timeline_index
                            .get(&item.id)
                            .map_or(0, |(_, revision)| *revision);
                        (item, self.derived.get(item, revision).diff_lines.clone())
                    }),
            )
        })?;
        Some((lead, changes))
    }

    /// Show or fold one file's changes in the card under the row `reply`.
    pub(super) fn toggle_change(
        &mut self,
        reply: &str,
        lead: &str,
        file: usize,
        cx: &mut Context<Self>,
    ) {
        let open = self.open_changes.entry(lead.to_owned()).or_default();
        if !open.remove(&file) {
            open.insert(file);
        }
        if let Some(&(index, _)) = self.timeline_index.get(reply) {
            self.list_state.remeasure_items(index..index + 1);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(status: &str, input: serde_json::Value) -> AgentTimelineItem {
        AgentTimelineItem {
            label: None,
            persisted: None,
            live: None,
            truncated: None,
            revision: 0,
            id: String::new(),
            item_type: "tool".into(),
            role: None,
            title: None,
            text: None,
            status: Some(status.into()),
            input: Some(input),
            output: None,
            created_ms: 0,
            merge: "replace".into(),
        }
    }

    fn changes(calls: &[AgentTimelineItem]) -> Option<TurnChanges> {
        TurnChanges::of(calls.iter().map(|call| {
            (
                call,
                Arc::new(super::super::transcript::diff_lines_for(call)),
            )
        }))
    }

    #[test]
    fn a_turns_changes_add_up_by_file() {
        let edit = |status: &str, path: &str, old: &str, new: &str| {
            call(
                status,
                serde_json::json!({ "path": path, "edits": [{ "oldText": old, "newText": new }] }),
            )
        };
        let turn = [
            edit("completed", "src/net/retry.rs", "a", "b\nc"),
            call("completed", serde_json::json!({ "command": "cargo test" })),
            call(
                "completed",
                serde_json::json!({ "path": "notes.md", "content": "one\ntwo\nthree" }),
            ),
            // A second edit of the same file joins the first.
            edit("completed", "src/net/retry.rs", "d\ne", "f"),
            // An edit that failed changed nothing.
            edit("failed", "src/lib.rs", "x", "y"),
        ];
        let changes = changes(&turn).expect("the turn changed files");
        assert_eq!(
            changes.files(),
            [
                ("src/net/retry.rs", "+3", "\u{2212}3"),
                ("notes.md", "+3", "\u{2212}0"),
            ]
        );
        assert_eq!(changes.counts.0.as_ref(), "+6");
        assert_eq!(changes.counts.1.as_ref(), "\u{2212}3");
        assert_eq!(changes.files[0].name.as_ref(), "retry.rs");
        assert_eq!(changes.files[0].folder.as_ref(), "src/net/");
        assert_eq!(changes.files[1].folder.as_ref(), "");
    }

    #[test]
    fn a_turn_that_only_ran_commands_changed_nothing() {
        let turn = [call(
            "completed",
            serde_json::json!({ "command": "cargo test" }),
        )];
        assert!(changes(&turn).is_none());
    }
}
