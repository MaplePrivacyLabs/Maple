//! Per-item caches the transcript renders from. Markdown parses off the
//! UI thread: on a miss the row keeps its previous document (or, for a
//! short cold source, parses inline) while a background parse runs, and
//! chunks that land mid-parse coalesce into one follow-up parse, so the
//! parse rate is bounded by parse latency rather than a timer. Display
//! strings are derived once per item revision. Interior mutability
//! because the list render callback only has shared access to the screen.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{AsyncApp, SharedString, Task, WeakEntity};
use maple_agent::agent::{AgentTimelineItem, ExternalAgentActivity};

use super::ChatScreen;
use super::transcript::{
    diff_lines_for, external_agent_activity, maple_display_text, tool_input_line,
    tool_output_markdown,
};
use crate::ui::markdown;

/// Which text of a timeline item a parsed document belongs to.
#[derive(Clone, Copy)]
pub(super) enum MarkdownKind {
    Body = 0,
    ToolOutput = 1,
}

/// A cold source up to this long parses on the UI thread: that takes
/// microseconds and spares the row a frame of raw text.
pub(super) const INLINE_PARSE_LIMIT: usize = 4096;

/// Gap between selection ordinal bases of two items.
pub(super) const ORDINAL_SPACING: u64 = 4096;

/// Entries beyond this count are dropped wholesale; they are rebuilt on
/// demand.
const MAX_ENTRIES: usize = 4096;

/// Cached document with the item revision and text length it belongs to.
struct MarkdownEntry {
    revision: u64,
    len: usize,
    document: Arc<markdown::Document>,
    /// The raw text standing in for a parse still running.
    provisional: bool,
}

/// A background parse in flight for one key.
struct PendingParse {
    /// Dropping the task cancels the parse.
    _task: Task<()>,
    revision: u64,
    len: usize,
}

#[derive(Default)]
pub(super) struct MarkdownCache {
    /// Where finished parses go and how to spawn them; set by `attach`.
    /// Without it every parse is inline.
    link: RefCell<Option<(WeakEntity<ChatScreen>, AsyncApp)>>,
    entries: [RefCell<HashMap<String, MarkdownEntry>>; 2],
    pending: [RefCell<HashMap<String, PendingParse>>; 2],
    /// Base ordinal per item key, so paragraphs get stable selection keys.
    ordinals: RefCell<HashMap<String, u64>>,
    /// The base the next unseen key gets; bases only grow, so this is
    /// the maximum without scanning the map on every miss.
    next_base: std::cell::Cell<u64>,
}

impl MarkdownCache {
    /// Enable background parses: `chat` receives them through
    /// `ChatScreen::markdown_parsed`.
    pub(super) fn attach(&self, chat: WeakEntity<ChatScreen>, cx: AsyncApp) {
        *self.link.borrow_mut() = Some((chat, cx));
    }

    /// Parsed document for `source`. A cache hit returns at once. On a
    /// miss, a short cold source parses inline; anything else parses in
    /// the background while the previous document (or the raw text) is
    /// returned. A source that moves on during a parse gets the next
    /// parse when this one lands, through the re-render it triggers.
    pub(super) fn get(
        &self,
        id: &str,
        kind: MarkdownKind,
        revision: u64,
        source: &str,
    ) -> Arc<markdown::Document> {
        let slot = kind as usize;
        let len = source.len();
        let previous = {
            let entries = self.entries[slot].borrow();
            match entries.get(id) {
                Some(entry)
                    if entry.revision == revision && entry.len == len && !entry.provisional =>
                {
                    return Arc::clone(&entry.document);
                }
                Some(entry) => Some(Arc::clone(&entry.document)),
                None => None,
            }
        };
        if let Some(previous) = &previous
            && self.pending[slot].borrow().contains_key(id)
        {
            // Let it land; if the source moved on, the re-render that
            // installs it comes back here and starts the next parse.
            return Arc::clone(previous);
        }
        let can_spawn = self.link.borrow().is_some();
        if !can_spawn || (previous.is_none() && len <= INLINE_PARSE_LIMIT) {
            let document = Arc::new(markdown::parse(source));
            self.insert(
                slot,
                id,
                MarkdownEntry {
                    revision,
                    len,
                    document: Arc::clone(&document),
                    provisional: false,
                },
            );
            return document;
        }
        let task = self.spawn_parse(kind, id, revision, source);
        self.pending[slot].borrow_mut().insert(
            id.to_string(),
            PendingParse {
                _task: task,
                revision,
                len,
            },
        );
        match previous {
            Some(document) => document,
            None => {
                let document = Arc::new(markdown::Document::plain(source));
                self.insert(
                    slot,
                    id,
                    MarkdownEntry {
                        revision,
                        len,
                        document: Arc::clone(&document),
                        provisional: true,
                    },
                );
                document
            }
        }
    }

    fn spawn_parse(&self, kind: MarkdownKind, id: &str, revision: u64, source: &str) -> Task<()> {
        let id = id.to_string();
        let source = source.to_string();
        let len = source.len();
        let (chat, cx) = self
            .link
            .borrow()
            .clone()
            .expect("background parses need an attached screen");
        cx.spawn(async move |cx| {
            let document = cx
                .background_executor()
                .spawn(async move { markdown::parse(&source) })
                .await;
            chat.update(cx, |chat, cx| {
                chat.markdown_parsed(kind, &id, revision, len, document, cx);
            })
            .ok();
        })
    }

    /// A parse landed, from the background or from the warm-up.
    pub(super) fn install(
        &self,
        kind: MarkdownKind,
        id: &str,
        revision: u64,
        len: usize,
        document: markdown::Document,
    ) {
        let slot = kind as usize;
        self.pending[slot].borrow_mut().remove(id);
        self.insert(
            slot,
            id,
            MarkdownEntry {
                revision,
                len,
                document: Arc::new(document),
                provisional: false,
            },
        );
    }

    /// Whether `id` already holds a final document for this revision and
    /// length, so a warm-up can skip it.
    pub(super) fn is_current(
        &self,
        id: &str,
        kind: MarkdownKind,
        revision: u64,
        len: usize,
    ) -> bool {
        let slot = kind as usize;
        self.entries[slot].borrow().get(id).is_some_and(|entry| {
            entry.revision == revision && entry.len == len && !entry.provisional
        }) || self.pending[slot]
            .borrow()
            .get(id)
            .is_some_and(|parse| parse.revision == revision && parse.len == len)
    }

    fn insert(&self, slot: usize, id: &str, entry: MarkdownEntry) {
        let mut entries = self.entries[slot].borrow_mut();
        if entries.len() > MAX_ENTRIES {
            entries.clear();
        }
        entries.insert(id.to_string(), entry);
    }

    /// Selection ordinal base for an item key. Bases are spaced far apart
    /// so `base + block index` never collides across messages.
    pub(super) fn ordinal_for(&self, key: &str) -> u64 {
        let mut ordinals = self.ordinals.borrow_mut();
        if let Some(base) = ordinals.get(key) {
            return *base;
        }
        let base = self.next_base.get() + ORDINAL_SPACING;
        self.next_base.set(base);
        ordinals.insert(key.to_string(), base);
        base
    }

    /// Forget every document; parses in flight are cancelled.
    pub(super) fn clear(&self) {
        for entries in &self.entries {
            entries.borrow_mut().clear();
        }
        for pending in &self.pending {
            pending.borrow_mut().clear();
        }
        self.ordinals.borrow_mut().clear();
        self.next_base.set(0);
    }
}

/// Strings a tool or text card shows, derived once per item revision
/// instead of on every frame.
#[derive(Default)]
pub(super) struct ItemDerived {
    /// Display text of thinking rows and user bubbles.
    pub(super) text: SharedString,
    /// Readable tool output for the expanded card.
    pub(super) output_text: Option<SharedString>,
    /// `input: {json}` for the expanded card.
    pub(super) input_line: Option<SharedString>,
    /// +/- lines of an edit or write tool, capped at `MAX_DIFF_LINES`.
    pub(super) diff_lines: Arc<Vec<(char, SharedString)>>,
    /// What an external agent (Codex) did, for the row of the tool call
    /// that ran it.
    pub(super) external_agent: Option<Arc<ExternalAgentActivity>>,
}

pub(super) const MAX_DIFF_LINES: usize = 200;

impl ItemDerived {
    fn build(item: &AgentTimelineItem) -> Self {
        let output_text = tool_output_markdown(item).map(SharedString::from);
        Self {
            text: SharedString::from(
                maple_display_text(item.text.as_deref().unwrap_or("")).into_owned(),
            ),
            output_text,
            input_line: tool_input_line(item).map(SharedString::from),
            diff_lines: Arc::new(diff_lines_for(item)),
            external_agent: external_agent_activity(item).map(Arc::new),
        }
    }
}

/// Lazily built `ItemDerived` per item id, validated by item revision.
#[derive(Default)]
pub(super) struct DerivedCache {
    entries: RefCell<HashMap<String, (u64, Arc<ItemDerived>)>>,
}

impl DerivedCache {
    pub(super) fn get(&self, item: &AgentTimelineItem, revision: u64) -> Arc<ItemDerived> {
        let mut entries = self.entries.borrow_mut();
        if let Some((cached, derived)) = entries.get(&item.id)
            && *cached == revision
        {
            return Arc::clone(derived);
        }
        if entries.len() > MAX_ENTRIES {
            entries.clear();
        }
        let derived = Arc::new(ItemDerived::build(item));
        entries.insert(item.id.clone(), (revision, Arc::clone(&derived)));
        derived
    }

    pub(super) fn clear(&self) {
        self.entries.borrow_mut().clear();
    }
}
