//! Markdown parsing for agent messages: pulldown-cmark events resolved into
//! block-level structures that callers cache per message. Rendering is in
//! [`super::rich_text`], which turns blocks into interactive elements
//! (clickable links, drag selection, copyable code blocks).
//!
//! Parsing and element building are separate steps: [`parse`] produces a
//! [`Document`] of resolved blocks that callers cache per message, and
//! [`render`] turns it into elements each frame. Parsing is the expensive
//! part; rendering from blocks is a handful of allocations.

use std::sync::Arc;

use gpui::{Div, ElementId, SharedString, div, prelude::*, px};
use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use super::rich_text::{self, Highlights, Links, RenderCtx};
use super::theme;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct InlineStyle {
    bold: bool,
    italic: bool,
    strikethrough: bool,
    code: bool,
    link: bool,
}

/// Inline style spans of a text block. Stored as flags, not resolved
/// colors, so a cached document re-renders when the theme changes.
pub type InlineStyles = Arc<[(std::ops::Range<usize>, InlineStyle)]>;

impl InlineStyle {
    fn is_plain(self) -> bool {
        self == Self::default()
    }

    fn highlight(self) -> gpui::HighlightStyle {
        let mut style = gpui::HighlightStyle {
            color: None,
            font_weight: None,
            font_style: None,
            background_color: None,
            underline: None,
            strikethrough: None,
            fade_out: None,
        };
        if self.bold {
            style.font_weight = Some(super::typography::emphasis_weight());
        }
        if self.italic {
            style.font_style = Some(gpui::FontStyle::Italic);
        }
        if self.strikethrough {
            style.strikethrough = Some(gpui::StrikethroughStyle {
                thickness: px(1.0),
                color: None,
            });
        }
        if self.code {
            style.background_color = Some(theme::overlay_hover());
            style.color = Some(gpui::rgb(theme::code_text()).into());
        }
        if self.link {
            style.color = Some(gpui::rgb(theme::link()).into());
            style.underline = Some(gpui::UnderlineStyle {
                thickness: px(1.0),
                color: Some(gpui::rgb(theme::link()).into()),
                wavy: false,
            });
        }
        style
    }
}

#[derive(Default)]
struct Paragraph {
    text: String,
    spans: Vec<(std::ops::Range<usize>, InlineStyle)>,
    /// Link ranges with their destinations, byte ranges into `text`.
    links: Vec<(std::ops::Range<usize>, String)>,
    /// Length of the list marker ("• " or "1. ") at the start of `text`.
    /// A paragraph that holds only its marker counts as empty.
    marker_len: usize,
}

impl Paragraph {
    fn push_marker(&mut self, marker: &str) {
        self.push(marker, InlineStyle::default());
        self.marker_len = self.text.len();
    }

    fn push(&mut self, chunk: &str, style: InlineStyle) {
        let start = self.text.len();
        self.text.push_str(chunk);
        let end = self.text.len();
        if start != end {
            self.spans.push((start..end, style));
        }
    }

    /// Record that `start..end` belongs to the link at `url`, merging with
    /// the previous range when it continues the same link.
    fn extend_link(&mut self, url: &str, start: usize, end: usize) {
        if let Some((range, existing)) = self.links.last_mut()
            && existing == url
            && range.end == start
        {
            range.end = end;
            return;
        }
        self.links.push((start..end, url.to_string()));
    }
    fn is_empty(&self) -> bool {
        self.text[self.marker_len..].trim().is_empty()
    }

    fn take(&mut self) -> Paragraph {
        std::mem::take(self)
    }
}

/// Horizontal alignment of one table column, from the delimiter row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColumnAlign {
    Left,
    Center,
    Right,
}

/// One table cell: resolved text with inline styles and links, like a
/// text block but without block-level chrome.
#[derive(Clone)]
pub struct TableCell {
    pub text: SharedString,
    pub styles: InlineStyles,
    pub links: Links,
}

/// One parsed block. Text blocks keep inline styles as flags; colors are
/// resolved at render time so cached documents follow theme changes.
#[derive(Clone)]
pub enum Block {
    Text {
        text: SharedString,
        styles: InlineStyles,
        /// Clickable link ranges with destinations, byte ranges into `text`.
        links: Links,
        heading: Option<HeadingLevel>,
        in_quote: bool,
        list_depth: usize,
    },
    Code {
        code: SharedString,
        /// Upper-case language tag from the fence info string, or "CODE",
        /// shown in the block header.
        label: SharedString,
        in_quote: bool,
        list_depth: usize,
    },
    Table {
        /// Rows of cells; the first row is the header.
        rows: Arc<[Arc<[TableCell]>]>,
        /// Per-column alignment from the delimiter row.
        alignments: Arc<[ColumnAlign]>,
        /// Per-column width weight from the longest cell text, computed
        /// once here rather than on every frame.
        column_weights: Arc<[usize]>,
        in_quote: bool,
        list_depth: usize,
    },
    Rule,
}

impl Block {
    /// Selection ordinals this block consumes. Every block takes one
    /// slot, except a table, whose cells are each their own selectable
    /// paragraph.
    pub fn ordinal_count(&self) -> u64 {
        match self {
            Block::Table { rows, .. } => {
                rows.iter().map(|row| row.len() as u64).sum::<u64>().max(1)
            }
            _ => 1,
        }
    }
}

/// A parsed markdown message, ready to render any number of times.
#[derive(Clone, Default)]
pub struct Document {
    pub blocks: Vec<Block>,
}

impl Document {
    /// The source as one unstyled paragraph: what a row shows while its
    /// real parse runs in the background.
    pub fn plain(source: &str) -> Document {
        Document {
            blocks: vec![Block::Text {
                text: SharedString::new(source),
                styles: Arc::from([]),
                links: Arc::from([]),
                heading: None,
                in_quote: false,
                list_depth: 0,
            }],
        }
    }

    /// Visit every selectable paragraph with the ordinal offset that
    /// [`render_with`] assigns it, so select-all can register the text
    /// of paragraphs that never rendered.
    pub fn for_each_selectable(&self, mut visit: impl FnMut(u64, &SharedString)) {
        let mut offset = 0u64;
        for block in &self.blocks {
            match block {
                Block::Text { text, .. } => visit(offset, text),
                Block::Table { rows, .. } => {
                    for (cell_ix, cell) in rows.iter().flat_map(|row| row.iter()).enumerate() {
                        visit(offset + cell_ix as u64, &cell.text);
                    }
                }
                _ => {}
            }
            offset += block.ordinal_count();
        }
    }
}

/// Turn bare URLs in the finished paragraph into links. Runs at flush
/// time, on the whole text, because pulldown-cmark splits text events
/// at characters like `_`, so a per-event scan would truncate URLs.
/// URLs already inside a link or a code span stay as they are.
fn linkify_bare_urls(paragraph: &mut Paragraph) {
    for range in find_bare_urls(&paragraph.text) {
        let overlaps = |other: &std::ops::Range<usize>| -> bool {
            other.start < range.end && range.start < other.end
        };
        if paragraph
            .links
            .iter()
            .any(|(existing, _)| overlaps(existing))
        {
            continue;
        }
        if paragraph
            .spans
            .iter()
            .any(|(span, style)| overlaps(span) && (style.code || style.link))
        {
            continue;
        }
        let url = paragraph.text[range.clone()].to_string();
        paragraph.links.push((range.clone(), url));
        // Overlay the link flag: split every overlapping span so the
        // part inside the URL keeps its other flags and gains `link`.
        let mut spans: Vec<(std::ops::Range<usize>, InlineStyle)> =
            Vec::with_capacity(paragraph.spans.len() + 2);
        for (span, style) in paragraph.spans.drain(..) {
            if !overlaps(&span) {
                spans.push((span, style));
                continue;
            }
            if span.start < range.start {
                spans.push((span.start..range.start, style));
            }
            let mut linked = style;
            linked.link = true;
            spans.push((span.start.max(range.start)..span.end.min(range.end), linked));
            if span.end > range.end {
                spans.push((range.end..span.end, style));
            }
        }
        paragraph.spans = spans;
    }
}

/// Merge a paragraph's styled spans and links into the shared form text
/// blocks and table cells store. Only styled spans become highlights;
/// unstyled ranges inherit the ambient text style resolved at paint
/// time. Spans keep style flags, not colors: the theme is read when the
/// block is rendered.
fn resolve_inline(mut paragraph: Paragraph) -> (SharedString, InlineStyles, Links) {
    linkify_bare_urls(&mut paragraph);
    let mut styles: Vec<(std::ops::Range<usize>, InlineStyle)> = Vec::new();
    for (range, style) in paragraph.spans {
        if style.is_plain() {
            continue;
        }
        if let Some((last_range, last_style)) = styles.last_mut()
            && last_range.end == range.start
            && *last_style == style
        {
            last_range.end = range.end;
            continue;
        }
        styles.push((range, style));
    }
    let links: Links = paragraph
        .links
        .into_iter()
        .filter(|(range, _)| !range.is_empty())
        .collect();
    (SharedString::new(paragraph.text), Arc::from(styles), links)
}

/// Byte ranges an inline code span covers, for the monospace face.
fn mono_ranges(styles: &InlineStyles) -> rich_text::Mono {
    styles
        .iter()
        .filter(|(_, style)| style.code)
        .map(|(range, _)| range.clone())
        .collect()
}

fn text_block(
    paragraph: Paragraph,
    heading: Option<HeadingLevel>,
    in_quote: bool,
    list_depth: usize,
) -> Block {
    let (text, styles, links) = resolve_inline(paragraph);
    Block::Text {
        text,
        styles,
        links,
        heading,
        in_quote,
        list_depth,
    }
}

/// Build elements from a parsed document (no selection context).
pub fn render(document: &Document) -> Div {
    render_with(document, &RenderCtx::default())
}

/// Build elements from a parsed document, making text blocks selectable
/// when the context carries a selection entity.
pub fn render_with(document: &Document, ctx: &RenderCtx) -> Div {
    let mut container = div().flex().flex_col().gap_2().w_full().pr_6();
    // One shared name per message; each code block adds its index.
    let id_name = ctx.id_name();
    // Blocks consume ordinal slots (see `Block::ordinal_count`); the
    // running offset keeps table cells from colliding with later blocks.
    let mut ordinal_offset: usize = 0;
    for (index, block) in document.blocks.iter().enumerate() {
        let block_offset = ordinal_offset;
        ordinal_offset += block.ordinal_count() as usize;
        container = match block {
            Block::Text {
                text,
                styles,
                links,
                heading,
                in_quote,
                list_depth,
            } => {
                // Resolve the palette and heading metrics now; documents
                // are cached across theme and chat-size switches.
                let highlights: Highlights = styles
                    .iter()
                    .map(|(range, style)| (range.clone(), style.highlight()))
                    .collect();
                let text_size = heading.map(super::typography::heading_size);
                let weight = heading.map(|_| super::typography::emphasis_weight());
                let mut paragraph = rich_text::paragraph(
                    text.clone(),
                    rich_text::Inline {
                        highlights,
                        links: links.clone(),
                        mono: mono_ranges(styles),
                    },
                    text_size,
                    weight,
                    ctx.for_block(block_offset),
                    ctx,
                );
                if heading.is_some() {
                    paragraph = paragraph.text_color(gpui::rgb(theme::text_heading()));
                }
                container.child(wrap_inline(paragraph, *in_quote, *list_depth))
            }
            Block::Code {
                code,
                label,
                in_quote,
                list_depth,
            } => container.child(wrap_inline(
                rich_text::code_block(
                    code.clone(),
                    label.clone(),
                    id_name.clone(),
                    index as u64,
                    ctx.view,
                ),
                *in_quote,
                *list_depth,
            )),
            Block::Table {
                rows,
                alignments,
                column_weights,
                in_quote,
                list_depth,
            } => container.child(wrap_inline(
                table_element(rows, alignments, column_weights, block_offset, ctx),
                *in_quote,
                *list_depth,
            )),
            Block::Rule => container.child(
                div()
                    .h(px(1.))
                    .w_full()
                    .my_1()
                    .bg(gpui::rgb(theme::border_subtle())),
            ),
        };
    }
    container
}

/// Byte ranges of bare `http(s)://` URLs in `text`. A URL runs to the
/// next whitespace or angle bracket; trailing punctuation that ends the
/// sentence is trimmed, and a closing paren only stays while the URL
/// holds an unmatched open paren (Wikipedia-style paths).
fn find_bare_urls(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find("http") {
        let start = from + found;
        let after = &text[start..];
        let scheme_len = if after.starts_with("https://") {
            8
        } else if after.starts_with("http://") {
            7
        } else {
            from = start + 4;
            continue;
        };
        // Not a URL when it continues a word ("xhttps://...").
        if text[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric())
        {
            from = start + scheme_len;
            continue;
        }
        let len = after
            .find(|c: char| c.is_whitespace() || c == '<' || c == '>')
            .unwrap_or(after.len());
        let mut end = start + len;
        loop {
            let url = &text[start..end];
            let Some(last) = url.chars().next_back() else {
                break;
            };
            let trim = match last {
                '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '*' | '`' => true,
                ')' | ']' | '}' => {
                    let open = match last {
                        ')' => '(',
                        ']' => '[',
                        _ => '{',
                    };
                    url.matches(open).count() < url.matches(last).count()
                }
                _ => false,
            };
            if !trim {
                break;
            }
            end -= last.len_utf8();
        }
        if end > start + scheme_len {
            ranges.push(start..end);
            from = end;
        } else {
            from = start + scheme_len;
        }
    }
    ranges
}

/// Parse markdown into resolved blocks.
pub fn parse(source: &str) -> Document {
    let mut options = Options::empty();
    // Open link destinations, parallel to the `link` entries pushed onto
    // `inline_flags`.
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(source, options);

    let mut blocks: Vec<Block> = Vec::new();
    let mut paragraph = Paragraph::default();
    let mut inline_flags: Vec<InlineStyle> = Vec::new();
    let mut link_stack: Vec<String> = Vec::new();
    let mut list_counters: Vec<Option<u64>> = Vec::new();
    let mut code_block_text: Option<String> = None;
    let mut code_block_language: Option<String> = None;
    let mut in_quote = false;
    // The table being parsed, if any. Cells collect into `paragraph`
    // like ordinary inline text and are taken at each cell end.
    struct TableBuilder {
        rows: Vec<Arc<[TableCell]>>,
        row: Vec<TableCell>,
        alignments: Vec<ColumnAlign>,
    }
    let mut table: Option<TableBuilder> = None;

    let current_style = |flags: &[InlineStyle]| -> InlineStyle {
        let mut style = InlineStyle::default();
        for flag in flags {
            style.bold |= flag.bold;
            style.italic |= flag.italic;
            style.strikethrough |= flag.strikethrough;
            style.code |= flag.code;
            style.link |= flag.link;
        }
        style
    };

    // Emit the pending paragraph before a block boundary.
    macro_rules! flush_paragraph {
        () => {
            let taken = paragraph.take();
            if !taken.is_empty() {
                blocks.push(text_block(taken, None, in_quote, list_counters.len()));
            }
        };
    }

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { .. } => {
                    flush_paragraph!();
                }
                Tag::BlockQuote(_) => {
                    flush_paragraph!();
                    in_quote = true;
                }
                Tag::CodeBlock(kind) => {
                    flush_paragraph!();
                    code_block_text = Some(String::new());
                    code_block_language = match kind {
                        pulldown_cmark::CodeBlockKind::Fenced(info) => {
                            let tag = info.split_whitespace().next().unwrap_or("");
                            (!tag.is_empty()).then(|| tag.to_string())
                        }
                        pulldown_cmark::CodeBlockKind::Indented => None,
                    };
                }
                Tag::List(start) => {
                    flush_paragraph!();
                    list_counters.push(start);
                }
                Tag::Item => {
                    // A new list item starts a fresh paragraph even when the
                    // previous one has not been closed (tight lists), so
                    // nested items never merge into their parent's line.
                    flush_paragraph!();
                    if let Some(counter) = list_counters.last_mut() {
                        match counter {
                            Some(number) => {
                                paragraph.push_marker(&format!("{number}. "));
                                *number += 1;
                            }
                            None => paragraph.push_marker("• "),
                        }
                    }
                }
                Tag::Emphasis => inline_flags.push(InlineStyle {
                    italic: true,
                    ..Default::default()
                }),
                Tag::Strong => inline_flags.push(InlineStyle {
                    bold: true,
                    ..Default::default()
                }),
                Tag::Strikethrough => inline_flags.push(InlineStyle {
                    strikethrough: true,
                    ..Default::default()
                }),
                Tag::Link { dest_url, .. } => {
                    inline_flags.push(InlineStyle {
                        link: true,
                        ..Default::default()
                    });
                    link_stack.push(dest_url.to_string());
                }
                Tag::Table(column_alignments) => {
                    flush_paragraph!();
                    let alignments = column_alignments
                        .iter()
                        .map(|alignment| match alignment {
                            Alignment::Center => ColumnAlign::Center,
                            Alignment::Right => ColumnAlign::Right,
                            Alignment::None | Alignment::Left => ColumnAlign::Left,
                        })
                        .collect();
                    table = Some(TableBuilder {
                        rows: Vec::new(),
                        row: Vec::new(),
                        alignments,
                    });
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    flush_paragraph!();
                }
                TagEnd::Heading(level) => {
                    let taken = paragraph.take();
                    if !taken.is_empty() {
                        blocks.push(text_block(
                            taken,
                            Some(level),
                            in_quote,
                            list_counters.len(),
                        ));
                    }
                }
                TagEnd::BlockQuote(_) => {
                    in_quote = false;
                }
                TagEnd::CodeBlock => {
                    if let Some(code) = code_block_text.take() {
                        let language = code_block_language.take();
                        let label = language.as_deref().unwrap_or("code").to_uppercase();
                        blocks.push(Block::Code {
                            code: SharedString::new(code.trim_end()),
                            label: SharedString::new(label),
                            in_quote,
                            list_depth: list_counters.len(),
                        });
                    }
                }
                TagEnd::List(_) => {
                    list_counters.pop();
                }
                TagEnd::Item | TagEnd::HtmlBlock => {
                    flush_paragraph!();
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    inline_flags.pop();
                }
                TagEnd::Link => {
                    inline_flags.pop();
                    link_stack.pop();
                }
                TagEnd::TableCell => {
                    if let Some(builder) = table.as_mut() {
                        // Keep empty cells: they hold their column open.
                        let (text, styles, links) = resolve_inline(paragraph.take());
                        builder.row.push(TableCell {
                            text,
                            styles,
                            links,
                        });
                    }
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let Some(builder) = table.as_mut() {
                        builder
                            .rows
                            .push(Arc::from(std::mem::take(&mut builder.row)));
                    }
                }
                TagEnd::Table => {
                    if let Some(builder) = table.take()
                        && !builder.rows.is_empty()
                    {
                        blocks.push(Block::Table {
                            column_weights: table_weights(&builder.rows),
                            rows: Arc::from(builder.rows),
                            alignments: Arc::from(builder.alignments),
                            in_quote,
                            list_depth: list_counters.len(),
                        });
                    }
                }
                _ => {}
            },
            Event::Text(chunk) => {
                if let Some(code) = code_block_text.as_mut() {
                    code.push_str(&chunk);
                } else {
                    let start = paragraph.text.len();
                    paragraph.push(&chunk, current_style(&inline_flags));
                    if let Some(url) = link_stack.last().cloned() {
                        let end = paragraph.text.len();
                        paragraph.extend_link(&url, start, end);
                    }
                }
            }
            Event::Code(chunk) => {
                let start = paragraph.text.len();
                let mut style = current_style(&inline_flags);
                style.code = true;
                paragraph.push(&chunk, style);
                if let Some(url) = link_stack.last().cloned() {
                    let end = paragraph.text.len();
                    paragraph.extend_link(&url, start, end);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if code_block_text.is_none() {
                    paragraph.push("\n", InlineStyle::default());
                }
            }
            Event::Rule => {
                flush_paragraph!();
                blocks.push(Block::Rule);
            }
            Event::InlineHtml(html) => {
                let start = paragraph.text.len();
                paragraph.push(&html, current_style(&inline_flags));
                if let Some(url) = link_stack.last().cloned() {
                    let end = paragraph.text.len();
                    paragraph.extend_link(&url, start, end);
                }
            }
            Event::Html(html) => {
                // Block HTML is shown as its source, like inline HTML.
                if let Some(code) = code_block_text.as_mut() {
                    code.push_str(&html);
                } else {
                    paragraph.push(&html, current_style(&inline_flags));
                }
            }
            _ => {}
        }
    }

    // Flush any trailing content outside a closing tag (defensive).
    if !paragraph.is_empty() && code_block_text.is_none() {
        let taken = paragraph.take();
        blocks.push(text_block(taken, None, false, 0));
    }
    Document { blocks }
}

/// Longest cell text a column's width weight counts; keeps one huge
/// cell from starving the other columns.
const TABLE_WEIGHT_CAP: usize = 60;

/// Per-column width weights: the longest cell text of each column, so a
/// short column does not take the same space as a prose column.
fn table_weights(rows: &[Arc<[TableCell]>]) -> Arc<[usize]> {
    let mut weights: Vec<usize> = Vec::new();
    for row in rows {
        for (col_ix, cell) in row.iter().enumerate() {
            if weights.len() <= col_ix {
                weights.push(1);
            }
            let len = cell.text.chars().count().clamp(1, TABLE_WEIGHT_CAP);
            weights[col_ix] = weights[col_ix].max(len);
        }
    }
    Arc::from(weights)
}

/// Build a table: bordered rows of flex cells sized by `weights`. Each
/// cell is its own selectable paragraph at `base_offset` plus its cell
/// index, and the tree exposes rows and cells to assistive technology.
fn table_element(
    rows: &Arc<[Arc<[TableCell]>]>,
    alignments: &[ColumnAlign],
    weights: &[usize],
    base_offset: usize,
    ctx: &RenderCtx,
) -> gpui::Stateful<Div> {
    let total: usize = weights.iter().sum::<usize>().max(1);
    let columns = weights.len();

    let border = gpui::rgb(theme::border_subtle());
    let mut table = div()
        .id(ElementId::NamedInteger("table".into(), base_offset as u64))
        .role(gpui::Role::Table)
        .aria_row_count(rows.len())
        .aria_column_count(columns)
        .w_full()
        .my_1()
        .rounded(theme::RADIUS_SM)
        .border_1()
        .border_color(border)
        .overflow_hidden()
        .flex()
        .flex_col();
    let mut ordinal = base_offset;
    for (row_ix, row) in rows.iter().enumerate() {
        let header = row_ix == 0;
        let mut line = div().flex().w_full();
        if header {
            line = line.bg(gpui::rgb(theme::bg_code_block()));
        }
        if row_ix + 1 < rows.len() {
            line = line.border_b_1().border_color(border);
        }
        for (col_ix, cell) in row.iter().enumerate() {
            // Resolve the palette now, like text blocks: cached
            // documents must not keep stale colors.
            let highlights: Highlights = cell
                .styles
                .iter()
                .map(|(range, style)| (range.clone(), style.highlight()))
                .collect();
            let weight = weights.get(col_ix).copied().unwrap_or(1);
            let mut cell_div = div()
                .id(ElementId::NamedInteger("cell".into(), ordinal as u64))
                .role(if header {
                    gpui::Role::ColumnHeader
                } else {
                    gpui::Role::Cell
                })
                .aria_row_index(row_ix + 1)
                .aria_column_index(col_ix + 1)
                .flex_grow(1.)
                .flex_basis(gpui::relative(weight as f32 / total as f32))
                .min_w(px(0.))
                .px_2()
                .py_1()
                .overflow_hidden();
            match alignments.get(col_ix) {
                Some(ColumnAlign::Center) => cell_div = cell_div.text_center(),
                Some(ColumnAlign::Right) => cell_div = cell_div.text_right(),
                _ => {}
            }
            if col_ix > 0 {
                cell_div = cell_div.border_l_1().border_color(border);
            }
            line = line.child(cell_div.child(rich_text::paragraph(
                cell.text.clone(),
                rich_text::Inline {
                    highlights,
                    links: cell.links.clone(),
                    mono: mono_ranges(&cell.styles),
                },
                None,
                header.then_some(gpui::FontWeight::SEMIBOLD),
                ctx.for_block(ordinal),
                ctx,
            )));
            ordinal += 1;
        }
        table = table.child(line);
    }
    table
}

/// Apply list indentation and blockquote chrome to an inner block.
fn wrap_inline(element: impl IntoElement, in_quote: bool, list_depth: usize) -> Div {
    let mut outer = div().w_full();
    if list_depth > 1 {
        outer = outer.pl(px(16. * (list_depth - 1) as f32));
    }
    if in_quote {
        outer = outer
            .border_l_2()
            .border_color(gpui::rgb(theme::border()))
            .pl_3();
    }
    outer.child(element)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_ranges_carry_destinations() {
        let document = parse("see [the docs](https://example.com/a) now");
        let Block::Text { text, links, .. } = &document.blocks[0] else {
            panic!("expected a text block");
        };
        assert_eq!(text.as_ref(), "see the docs now");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "https://example.com/a");
        assert_eq!(&text[links[0].0.clone()], "the docs");
    }

    #[test]
    fn adjacent_links_stay_separate() {
        let document = parse("[a](https://x.test) and [b](https://y.test)");
        let Block::Text { links, .. } = &document.blocks[0] else {
            panic!("expected a text block");
        };
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].1, "https://x.test");
        assert_eq!(links[1].1, "https://y.test");
    }

    #[test]
    fn fenced_code_keeps_language() {
        let document = parse("```rust\nfn main() {}\n```");
        let Block::Code { code, label, .. } = &document.blocks[0] else {
            panic!("expected a code block");
        };
        assert_eq!(code.as_ref(), "fn main() {}");
        assert_eq!(label.as_ref(), "RUST");
    }

    #[test]
    fn code_block_label_is_upper_case() {
        let document = parse("```sh\nls\n```\n\n    plain\n");
        let labels: Vec<&str> = document
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Code { label, .. } => Some(label.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["SH", "CODE"]);
    }

    #[test]
    fn list_item_with_leading_code_block_has_no_orphan_marker() {
        // Unindented lines end the item, so the fence is empty; there must
        // still be no marker-only paragraph.
        let document = parse("- ```sh\nls\n```");
        assert!(
            !document
                .blocks
                .iter()
                .any(|block| matches!(block, Block::Text { text, .. } if text.trim() == "•"))
        );

        let document = parse("- ```sh\n  ls\n  ```\n- second");
        let Block::Code {
            code, list_depth, ..
        } = &document.blocks[0]
        else {
            panic!("expected a code block first, got a marker paragraph");
        };
        assert_eq!(code.as_ref(), "ls");
        assert_eq!(*list_depth, 1);
        let Block::Text { text, .. } = &document.blocks[1] else {
            panic!("expected the second item");
        };
        assert_eq!(text.as_ref(), "• second");
        assert_eq!(document.blocks.len(), 2);
    }

    #[test]
    fn list_item_with_leading_heading_has_no_orphan_marker() {
        let document = parse("- # Title\n  body");
        assert!(
            !document
                .blocks
                .iter()
                .any(|block| matches!(block, Block::Text { text, .. } if text.trim() == "•"))
        );
    }

    #[test]
    fn bare_urls_become_links() {
        let document = parse("It's live: https://github.com/a/b. Next step");
        let Block::Text {
            text,
            links,
            styles,
            ..
        } = &document.blocks[0]
        else {
            panic!("expected a text block");
        };
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "https://github.com/a/b");
        assert_eq!(&text[links[0].0.clone()], "https://github.com/a/b");
        assert!(
            styles
                .iter()
                .any(|(range, style)| style.link && *range == links[0].0),
            "the URL range carries the link style"
        );
    }

    #[test]
    fn bare_url_in_bold_text_keeps_both_styles() {
        let document = parse("go to **https://x.test** now");
        let Block::Text { links, styles, .. } = &document.blocks[0] else {
            panic!("expected a text block");
        };
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "https://x.test");
        assert!(
            styles
                .iter()
                .any(|(range, style)| style.link && style.bold && *range == links[0].0)
        );
    }

    #[test]
    fn bare_url_trims_punctuation_but_keeps_balanced_parens() {
        let document =
            parse("see https://en.wikipedia.org/wiki/Rust_(language) (or https://x.test).");
        let Block::Text { links, .. } = &document.blocks[0] else {
            panic!("expected a text block");
        };
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].1, "https://en.wikipedia.org/wiki/Rust_(language)");
        assert_eq!(links[1].1, "https://x.test");
    }

    #[test]
    fn urls_in_markdown_links_and_code_spans_are_untouched() {
        let document = parse("[docs](https://a.test) and `https://b.test` and xhttps://c.test");
        let Block::Text { links, .. } = &document.blocks[0] else {
            panic!("expected a text block");
        };
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "https://a.test");
    }

    #[test]
    fn table_parses_rows_alignments_and_cell_styles() {
        let document = parse(
            "| Name | Age |\n\
             | :--- | ---: |\n\
             | **Alice** | 30 |\n\
             | [Bob](https://b.test) | 25 |",
        );
        let Block::Table {
            rows, alignments, ..
        } = &document.blocks[0]
        else {
            panic!("expected a table block");
        };
        assert_eq!(rows.len(), 3, "header plus two body rows");
        assert_eq!(rows[0].len(), 2);
        assert_eq!(rows[0][0].text.as_ref(), "Name");
        assert_eq!(rows[2][1].text.as_ref(), "25");
        assert_eq!(
            alignments.as_ref(),
            &[ColumnAlign::Left, ColumnAlign::Right]
        );
        assert!(
            rows[1][0].styles.iter().any(|(_, style)| style.bold),
            "bold survives into the cell styles"
        );
        assert_eq!(rows[2][0].links.len(), 1);
        assert_eq!(rows[2][0].links[0].1, "https://b.test");
    }

    #[test]
    fn empty_table_cells_keep_their_column() {
        let document = parse("| a | b |\n| --- | --- |\n| 1 | |");
        let Block::Table { rows, .. } = &document.blocks[0] else {
            panic!("expected a table block");
        };
        assert_eq!(rows[1].len(), 2);
        assert_eq!(rows[1][1].text.as_ref(), "");
    }

    #[test]
    fn table_cells_take_selection_ordinals_before_later_blocks() {
        let document = parse("| a | b |\n| --- | --- |\n| c | d |\n\nafter");
        let mut seen: Vec<(u64, String)> = Vec::new();
        document.for_each_selectable(|offset, text| seen.push((offset, text.to_string())));
        let expected: Vec<(u64, String)> = [(0, "a"), (1, "b"), (2, "c"), (3, "d"), (4, "after")]
            .into_iter()
            .map(|(offset, text)| (offset, text.to_string()))
            .collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn block_html_is_kept_as_text() {
        let document = parse("<div>\nhi\n</div>\n\nafter");
        let Block::Text { text, .. } = &document.blocks[0] else {
            panic!("expected html text");
        };
        assert!(text.contains("<div>"), "got {text:?}");
    }

    #[test]
    fn indented_code_has_no_language() {
        let document = parse("text\n\n    indented code\n");
        assert!(
            document.blocks.iter().any(
                |block| matches!(block, Block::Code { label, .. } if label.as_ref() == "CODE")
            )
        );
    }

    #[test]
    fn headings_keep_their_level_and_strong_is_not_bold() {
        let document = parse("# Title\n\n**loud**");
        let Block::Text { heading, .. } = &document.blocks[0] else {
            panic!("expected a heading block");
        };
        assert_eq!(*heading, Some(HeadingLevel::H1));
        let Block::Text {
            styles, heading, ..
        } = &document.blocks[1]
        else {
            panic!("expected a body block");
        };
        assert_eq!(*heading, None);
        assert!(styles.iter().any(|(_, style)| style.bold));
        let highlight = styles
            .iter()
            .find(|(_, style)| style.bold)
            .unwrap()
            .1
            .highlight();
        assert_eq!(
            highlight.font_weight,
            Some(crate::ui::typography::emphasis_weight())
        );
        assert_ne!(highlight.font_weight, Some(gpui::FontWeight::BOLD));
    }

    /// Serializes tests that flip the process-global theme, and pairs with
    /// `ThemeRestore` so the flip never leaks into a test that reads the
    /// palette while this one runs or after it panics.
    static THEME_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// Restores the theme when the test ends, panics included.
    struct ThemeRestore;
    impl Drop for ThemeRestore {
        fn drop(&mut self) {
            theme::set_preference(theme::Preference::System);
            theme::resolve(gpui::WindowAppearance::Dark);
        }
    }

    #[test]
    fn code_span_color_follows_theme_switch() {
        let _guard = THEME_LOCK.lock();
        let _restore = ThemeRestore;
        // A document parsed under one palette must re-resolve its inline
        // colors under the other: parse caches blocks across theme flips.
        let document = parse("token at `~/.mutiny/token` end");
        let Block::Text { styles, .. } = &document.blocks[0] else {
            panic!("expected a text block");
        };
        assert!(
            styles.iter().any(|(_, style)| style.code),
            "expected a code span in the parsed styles"
        );

        let code_color = || {
            styles
                .iter()
                .filter(|(_, style)| style.code)
                .map(|(_, style)| style.highlight().color)
                .next()
                .flatten()
        };

        theme::set_preference(theme::Preference::Light);
        theme::resolve(gpui::WindowAppearance::Dark);
        let light = code_color();
        let expected_light = Some(gpui::rgb(theme::code_text()).into());

        theme::set_preference(theme::Preference::System);
        theme::resolve(gpui::WindowAppearance::Dark);
        let dark = code_color();
        let expected_dark = Some(gpui::rgb(theme::code_text()).into());

        assert_eq!(light, expected_light);
        assert_eq!(dark, expected_dark);
        assert_ne!(light, dark, "inline code color must change with the theme");
    }
}
