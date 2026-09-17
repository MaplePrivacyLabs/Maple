//! Markdown tables, laid out the way browsers lay out `table-layout:
//! auto`. Every column is measured once per document from the shaped text
//! of its cells: the widest line any cell needs to stay unwrapped (its
//! max-content width) and the widest run the line wrapper cannot break at
//! a word boundary (its min-content width). Each row is then a flex line
//! whose cells carry those numbers as flex basis, minimum width and
//! shrink factor, so every row resolves the same column widths without a
//! grid: a deficit is shared in proportion to how much each column can
//! give before it wraps, so short columns keep their content on one line
//! while prose columns absorb the loss and wrap; the table hugs its
//! content when there is room; and when the floors do not fit it scrolls
//! sideways under an overlay bar instead of breaking words apart. One
//! floor goes beyond the browser's: a column that wraps never gets
//! narrower than a readable measure, since a word per line helps nobody.
//!
//! Measuring needs the live text style, which exists only during layout,
//! so the table is a custom element: `request_layout` reads the cascaded
//! font, measures (or reuses the measurement cached on the block), builds
//! the row tree and lays it out; prepaint and paint delegate to it.

use std::sync::{Arc, Mutex};

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, FontWeight, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, ScrollHandle, SharedString, TextRun, TextStyle, Window, div,
    prelude::*, px,
};

use super::{ColumnAlign, TableCell, mono_ranges};
use crate::ui::rich_text::{self, Highlights, RenderCtx};
use crate::ui::{scrollbar, theme, typography};

/// Horizontal padding on each side of a cell.
const PAD_X: Pixels = px(10.);
/// Vertical padding above and below a cell's text.
const PAD_Y: Pixels = px(5.);
/// Hairline between columns, drawn on every cell but the first.
const RULE: Pixels = px(1.);
/// Added to every measured width: layout rounds sizes to device pixels,
/// and a column a fraction narrower than its text would split a word.
const SLACK: Pixels = px(1.);
/// The narrowest a column that has to wrap may get, in ems. Its widest
/// word is a floor a browser would accept, but a word per line is not
/// readable; below this measure the table scrolls sideways instead. A
/// column whose whole content is narrower simply never wraps.
const READABLE_FLOOR_EMS: f32 = 12.;

/// Widths one column needs, from the shaped text of its cells.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ColumnMetrics {
    /// The widest run no cell can break at a word boundary. The column
    /// never goes narrower, so no word is ever split.
    pub min: Pixels,
    /// The widest line any cell needs to stay unwrapped.
    pub max: Pixels,
}

/// What a measurement depends on; a change makes it stale.
#[derive(Clone, PartialEq)]
struct MetricsKey {
    family: SharedString,
    size: Pixels,
    weight: FontWeight,
}

/// Column metrics cached on a table block. A parse leaves it empty; the
/// first layout under a given face and size fills it on the UI thread,
/// and every frame after that reads it. A streamed table re-measures
/// once per revision, when its document is parsed again.
#[derive(Default)]
pub struct TableMetrics {
    cache: Mutex<Option<(MetricsKey, Arc<[ColumnMetrics]>)>>,
}

impl TableMetrics {
    fn columns(
        &self,
        rows: &[Arc<[TableCell]>],
        column_count: usize,
        style: &TextStyle,
        window: &Window,
    ) -> Arc<[ColumnMetrics]> {
        let key = MetricsKey {
            family: style.font_family.clone(),
            size: style.font_size.to_pixels(window.rem_size()),
            weight: style.font_weight,
        };
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((cached, columns)) = cache.as_ref()
            && *cached == key
        {
            return Arc::clone(columns);
        }
        let started = std::time::Instant::now();
        let columns: Arc<[ColumnMetrics]> =
            measure_columns(rows, column_count, style, window).into();
        log::debug!(
            "measured a {} x {} table in {:?}",
            rows.len(),
            column_count,
            started.elapsed()
        );
        *cache = Some((key, Arc::clone(&columns)));
        columns
    }
}

/// Measure every column of `rows` under `style`: the header row in the
/// emphasis weight, the others as they are, code spans in the monospace
/// face, so the numbers match what the cells shape when they render.
fn measure_columns(
    rows: &[Arc<[TableCell]>],
    column_count: usize,
    style: &TextStyle,
    window: &Window,
) -> Vec<ColumnMetrics> {
    let mut columns = vec![ColumnMetrics::default(); column_count];
    let font_size = style.font_size.to_pixels(window.rem_size());
    let mut header_style = style.clone();
    header_style.font_weight = typography::emphasis_weight();
    let text_system = window.text_system();
    for (row_ix, row) in rows.iter().enumerate() {
        let base = if row_ix == 0 { &header_style } else { style };
        for (column, cell) in columns.iter_mut().zip(row.iter()) {
            if cell.text.is_empty() {
                continue;
            }
            let runs = cell_runs(base, cell);
            let Ok(lines) = text_system.shape_text(cell.text.clone(), font_size, &runs, None, None)
            else {
                continue;
            };
            for line in lines.iter() {
                let layout = &line.unwrapped_layout;
                let text: &str = &line.text;
                let glyphs = layout
                    .runs
                    .iter()
                    .flat_map(|run| run.glyphs.iter())
                    .map(|glyph| {
                        let ch = text
                            .get(glyph.index..)
                            .and_then(|rest| rest.chars().next())
                            .unwrap_or('\0');
                        (ch, glyph.position.x)
                    });
                column.max = column.max.max(layout.width);
                column.min = column.min.max(longest_segment(glyphs, layout.width));
            }
        }
    }
    columns
}

/// Text runs for a cell, derived from its highlights the way `StyledText`
/// derives them: styled spans over the base style, code spans in the
/// monospace face.
fn cell_runs(base: &TextStyle, cell: &TableCell) -> Vec<TextRun> {
    let len = cell.text.len();
    let mut runs = Vec::with_capacity(cell.styles.len() * 2 + 1);
    let mut ix = 0;
    for (range, style) in cell.styles.iter() {
        if ix < range.start {
            runs.push(base.to_run(range.start - ix));
        }
        let mut run = base
            .clone()
            .highlight(style.highlight())
            .to_run(range.len());
        if style.code {
            run.font.family = SharedString::new_static(crate::assets::FONT_MONO);
        }
        runs.push(run);
        ix = range.end;
    }
    if ix < len {
        runs.push(base.to_run(len - ix));
    }
    runs
}

/// The widest stretch of a shaped line that wrapping keeps together, from
/// the glyphs' characters and x positions and the line's width. gpui
/// breaks before a word that follows a space and before any character
/// that is not part of a word, never inside a run of word characters
/// unless nothing else fits; a segment therefore runs from one such
/// break opportunity to the next and includes the space that trails it.
fn longest_segment(glyphs: impl IntoIterator<Item = (char, Pixels)>, width: Pixels) -> Pixels {
    let mut longest = px(0.);
    let mut segment_start = px(0.);
    let mut seen_ink = false;
    let mut prev = '\0';
    for (ch, x) in glyphs {
        let opportunity = if is_word_char(ch) {
            prev == ' ' && seen_ink
        } else {
            ch != ' ' && seen_ink
        };
        if opportunity {
            longest = longest.max(x - segment_start);
            segment_start = x;
        }
        if ch != ' ' {
            seen_ink = true;
        }
        prev = ch;
    }
    longest.max(width - segment_start)
}

/// The characters gpui's line wrapper keeps together in a word (its
/// `LineWrapper::is_word_char` at the pinned revision, which is not
/// public). A character missing here becomes a break opportunity to us
/// but not to gpui, which could let a word split, so keep the list at
/// least as wide as gpui's.
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        // Latin-1 Supplement, Latin Extended-A and -B.
        || matches!(c, '\u{00C0}'..='\u{024F}')
        // Cyrillic.
        || matches!(c, '\u{0400}'..='\u{04FF}')
        // Latin Extended Additional and combining diacritical marks.
        || matches!(c, '\u{1E00}'..='\u{1EFF}')
        || matches!(c, '\u{0300}'..='\u{036F}')
        // Bengali.
        || matches!(c, '\u{0980}'..='\u{09FF}')
        // Punctuation that stays attached to a word.
        || matches!(
            c,
            '-' | '_'
                | '.'
                | '\''
                | '’'
                | '‘'
                | '$'
                | '%'
                | '@'
                | '#'
                | '^'
                | '~'
                | ','
                | '='
                | ':'
                | ';'
                | '⋯'
        )
        // Non-breaking glue.
        || matches!(c, '\u{202F}' | '\u{00A0}' | '\u{2011}')
}

/// What every cell of one column tells the flex row, border-box.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ColumnWidths {
    /// The floor: the column's widest unbreakable run plus chrome.
    min: Pixels,
    /// The flex basis: the column's widest line plus chrome.
    max: Pixels,
    /// Flex shrink factor. Taffy scales it by the cell's inner basis, so
    /// `(max - min) / inner max` hands out a deficit in proportion to how
    /// much each column can give before a word has to wrap, the way the
    /// automatic table layout of a browser does: a column of short
    /// numbers stays on one line while a prose column absorbs the loss.
    shrink: f32,
}

/// Per-column flex parameters from measured `columns`, with a floor of
/// `readable` for any column that would otherwise wrap tighter.
fn column_widths(columns: &[ColumnMetrics], readable: Pixels) -> Vec<ColumnWidths> {
    let mut widths: Vec<ColumnWidths> = columns
        .iter()
        .enumerate()
        .map(|(col_ix, metrics)| {
            let chrome = PAD_X * 2. + if col_ix > 0 { RULE } else { px(0.) };
            let inner_max = (metrics.max.ceil() + SLACK).max(metrics.min.ceil() + SLACK);
            let inner_min = (metrics.min.ceil() + SLACK).max(readable.min(inner_max));
            let min = inner_min + chrome;
            let max = inner_max + chrome;
            ColumnWidths {
                min,
                max,
                shrink: f32::from(max - min) / f32::from(inner_max),
            }
        })
        .collect();
    // Flexbox scales the deficit down when the shrink factors sum to less
    // than one; the ratios are all that matter, so lift them to a sum of
    // one and let every column shrink all the way to its floor.
    let sum: f32 = widths.iter().map(|column| column.shrink).sum();
    if sum > 0. && sum < 1. {
        for column in &mut widths {
            column.shrink /= sum;
        }
    }
    widths
}

/// A markdown table as an element. Each cell is its own selectable
/// paragraph at `base_offset` plus its index, and the tree exposes rows
/// and cells to assistive technology.
pub struct TableElement {
    id: ElementId,
    rows: Arc<[Arc<[TableCell]>]>,
    alignments: Arc<[ColumnAlign]>,
    metrics: Arc<TableMetrics>,
    /// Selection ordinal of the first cell.
    base_offset: usize,
    ctx: RenderCtx,
}

impl TableElement {
    pub(super) fn new(
        rows: Arc<[Arc<[TableCell]>]>,
        alignments: Arc<[ColumnAlign]>,
        metrics: Arc<TableMetrics>,
        base_offset: usize,
        ctx: &RenderCtx,
    ) -> Self {
        let id = ElementId::NamedInteger(
            SharedString::from(format!("{}-table", ctx.id_name())),
            base_offset as u64,
        );
        Self {
            id,
            rows,
            alignments,
            metrics,
            base_offset,
            ctx: ctx.clone(),
        }
    }

    fn column_count(&self) -> usize {
        self.alignments
            .len()
            .max(self.rows.iter().map(|row| row.len()).max().unwrap_or(0))
    }

    /// The row tree for measured `columns`, scrolling through `scroll`;
    /// `readable` is the narrowest a wrapping column may get.
    fn build(
        &self,
        columns: &[ColumnMetrics],
        readable: Pixels,
        scroll: ScrollHandle,
    ) -> AnyElement {
        let border = gpui::rgb(theme::border_subtle());
        let widths = column_widths(columns, readable);
        let sum_min = widths.iter().fold(px(0.), |sum, column| sum + column.min);
        let sum_max = widths.iter().fold(px(0.), |sum, column| sum + column.max);

        // Wider than its columns need only when they have to wrap;
        // narrower than their floors never: then it scrolls.
        let mut grid = div()
            .id("grid")
            .debug_selector(|| "md-table".into())
            .role(gpui::Role::Table)
            .aria_row_count(self.rows.len())
            .aria_column_count(columns.len())
            .flex()
            .flex_col()
            .w(sum_max)
            .max_w_full()
            .min_w(sum_min);
        let mut ordinal = self.base_offset;
        for (row_ix, row) in self.rows.iter().enumerate() {
            let header = row_ix == 0;
            let mut line = div().flex().w_full();
            if header {
                line = line.bg(gpui::rgb(theme::bg_code_block()));
            } else if row_ix % 2 == 0 {
                line = line.bg(theme::table_stripe());
            }
            if row_ix + 1 < self.rows.len() {
                line = line.border_b_1().border_color(border);
            }
            for (col_ix, column) in widths.iter().enumerate() {
                let mut cell_div = div()
                    .id(ElementId::NamedInteger("cell".into(), ordinal as u64))
                    .debug_selector(move || format!("md-cell-{row_ix}-{col_ix}"))
                    .role(if header {
                        gpui::Role::ColumnHeader
                    } else {
                        gpui::Role::Cell
                    })
                    .aria_row_index(row_ix + 1)
                    .aria_column_index(col_ix + 1)
                    .flex_basis(column.max)
                    .flex_shrink(column.shrink)
                    .min_w(column.min)
                    .px(PAD_X)
                    .py(PAD_Y);
                if col_ix > 0 {
                    cell_div = cell_div.border_l_1().border_color(border);
                }
                match self.alignments.get(col_ix) {
                    Some(ColumnAlign::Center) => cell_div = cell_div.text_center(),
                    Some(ColumnAlign::Right) => cell_div = cell_div.text_right(),
                    _ => {}
                }
                if let Some(cell) = row.get(col_ix) {
                    // Resolve the palette now, like text blocks: cached
                    // documents must not keep stale colors.
                    let highlights: Highlights = cell
                        .styles
                        .iter()
                        .map(|(range, style)| (range.clone(), style.highlight()))
                        .collect();
                    cell_div = cell_div.child(rich_text::paragraph(
                        cell.text.clone(),
                        rich_text::Inline {
                            highlights,
                            links: cell.links.clone(),
                            mono: mono_ranges(&cell.styles),
                        },
                        None,
                        header.then_some(typography::emphasis_weight()),
                        self.ctx.for_block(ordinal),
                        &self.ctx,
                    ));
                    ordinal += 1;
                }
                line = line.child(cell_div);
            }
            grid = grid.child(line);
        }

        // The frame hugs the grid up to the full width; the grid scrolls
        // inside it when its floors do not fit, and the bar rides the
        // frame's bottom edge so it stays put while the rows move.
        let scroller = div()
            .id("scroll")
            .w_full()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&scroll)
            .child(grid);
        let frame = div()
            .relative()
            .min_w_0()
            .max_w_full()
            .rounded(theme::RADIUS_SM)
            .border_1()
            .border_color(border)
            .overflow_hidden()
            .child(scroller)
            .child(scrollbar::horizontal_scrollbar("bar", scroll));
        div().flex().w_full().my_1().child(frame).into_any_element()
    }
}

impl Element for TableElement {
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
        let id = id.expect("the table element has an id");
        // The scroll offset survives across frames with the element.
        let scroll = window.with_element_state::<ScrollHandle, _>(id, |state, _window| {
            let handle = state.unwrap_or_else(ScrollHandle::new);
            (handle.clone(), handle)
        });
        let style = window.text_style();
        let columns = self
            .metrics
            .columns(&self.rows, self.column_count(), &style, window);
        let readable = style.font_size.to_pixels(window.rem_size()) * READABLE_FLOOR_EMS;
        let mut tree = self.build(&columns, readable, scroll);
        let layout_id = tree.request_layout(window, cx);
        (layout_id, tree)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        tree: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        tree.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        tree: &mut AnyElement,
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        tree.paint(window, cx);
    }
}

impl IntoElement for TableElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::markdown;
    use gpui::{Context, Render, TestAppContext, VisualTestContext, size};

    fn glyphs(text: &str, advance: f32) -> Vec<(char, Pixels)> {
        text.chars()
            .enumerate()
            .map(|(ix, ch)| (ch, px(ix as f32 * advance)))
            .collect()
    }

    fn segment(text: &str) -> Pixels {
        longest_segment(glyphs(text, 10.), px(text.chars().count() as f32 * 10.))
    }

    #[test]
    fn a_segment_runs_from_one_break_opportunity_to_the_next() {
        assert_eq!(
            segment("ab cd"),
            px(30.),
            "the trailing space belongs to the word"
        );
        assert_eq!(segment("permanently bricked"), px(120.));
        assert_eq!(segment("word"), px(40.));
        assert_eq!(segment(""), px(0.));
    }

    #[test]
    fn punctuation_that_is_not_part_of_a_word_is_a_break_opportunity() {
        // Before `/`: "src" and "/main.rs"; `.` stays inside the word.
        assert_eq!(segment("src/main.rs"), px(80.));
        // A hash of word characters never breaks.
        assert_eq!(segment("5d2f3b62d733"), px(120.));
        // Trailing punctuation stays with its word.
        assert_eq!(segment("done, then"), px(60.));
    }

    #[test]
    fn leading_spaces_join_the_first_segment() {
        assert_eq!(segment("   ab cd"), px(60.));
    }

    #[test]
    fn shrink_factors_share_a_deficit_by_slack() {
        let widths = column_widths(
            &[
                // No slack: never shrinks.
                ColumnMetrics {
                    min: px(30.),
                    max: px(30.),
                },
                // 100 px of slack over a 110 px inner basis.
                ColumnMetrics {
                    min: px(10.),
                    max: px(110.),
                },
                // 400 px of slack over a 410 px inner basis.
                ColumnMetrics {
                    min: px(10.),
                    max: px(410.),
                },
            ],
            px(0.),
        );
        assert_eq!(widths[0].shrink, 0.);
        let scaled: Vec<f32> = widths
            .iter()
            .map(|column| column.shrink * f32::from(column.max - column.min))
            .collect();
        // Taffy multiplies by the inner basis; the products are the slacks.
        let inner = |column: &ColumnWidths| f32::from(column.max - column.min);
        assert!((widths[1].shrink * (inner(&widths[1]) + 11.) - 100.).abs() < 0.01);
        assert!((widths[2].shrink * (inner(&widths[2]) + 11.) - 400.).abs() < 0.01);
        assert!(scaled[2] > scaled[1]);
        // Floors and bases carry padding, plus the rule on later columns.
        assert_eq!(widths[0].min, px(30. + 1. + 20.));
        assert_eq!(widths[1].min, px(10. + 1. + 21.));
        assert_eq!(widths[1].max, px(110. + 1. + 21.));
    }

    #[test]
    fn a_wrapping_column_keeps_a_readable_measure() {
        let widths = column_widths(
            &[
                // Narrower than the measure: rigid, wraps never.
                ColumnMetrics {
                    min: px(40.),
                    max: px(90.),
                },
                // Prose: floored at the measure, not at its widest word.
                ColumnMetrics {
                    min: px(60.),
                    max: px(600.),
                },
                // An unbreakable run wider than the measure keeps its own floor.
                ColumnMetrics {
                    min: px(300.),
                    max: px(300.),
                },
            ],
            px(160.),
        );
        assert_eq!(widths[0].min, widths[0].max);
        assert_eq!(widths[0].shrink, 0.);
        assert_eq!(widths[1].min, px(160. + 21.));
        assert_eq!(widths[2].min, px(300. + 1. + 21.));
    }

    #[test]
    fn shrink_factors_are_lifted_to_a_sum_of_one() {
        let widths = column_widths(
            &[
                ColumnMetrics {
                    min: px(90.),
                    max: px(100.),
                },
                ColumnMetrics {
                    min: px(95.),
                    max: px(100.),
                },
            ],
            px(0.),
        );
        let sum: f32 = widths.iter().map(|column| column.shrink).sum();
        assert!((sum - 1.).abs() < 0.001, "sum {sum}");
        // Ratios survive the lift: 10 px of slack against 5 px.
        let ratio = widths[0].shrink * 101. / (widths[1].shrink * 101.);
        assert!((ratio - 2.).abs() < 0.01, "ratio {ratio}");
    }

    struct TableView {
        source: &'static str,
        width: Pixels,
    }

    impl Render for TableView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let document = markdown::parse(self.source);
            div()
                .w(self.width)
                .text_size(px(14.))
                .child(markdown::render(&document))
        }
    }

    /// The test text system advances every character by 0.6 em, so at
    /// 14 px a character is 8.4 px wide.
    const CH: f32 = 8.4;
    /// `markdown::render` pads the right edge of every message.
    const CONTAINER_PAD: Pixels = px(24.);
    /// The frame draws a border on both sides of the grid.
    const FRAME: Pixels = px(2.);

    fn render_at(source: &'static str, width: f32, cx: &mut TestAppContext) -> VisualTestContext {
        let window = cx.open_window(size(px(width), px(600.)), |_, _| TableView {
            source,
            width: px(width),
        });
        cx.run_until_parked();
        VisualTestContext::from_window(window.into(), cx)
    }

    fn bounds(cx: &mut VisualTestContext, selector: String) -> Bounds<Pixels> {
        let selector: &'static str = Box::leak(selector.into_boxed_str());
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} was not rendered"))
    }

    fn cell(cx: &mut VisualTestContext, row: usize, col: usize) -> Bounds<Pixels> {
        bounds(cx, format!("md-cell-{row}-{col}"))
    }

    /// Border-box width of a first column whose widest content is `chars`
    /// characters long.
    fn first_column(chars: usize) -> Pixels {
        (px(chars as f32 * CH)).ceil() + SLACK + PAD_X * 2.
    }

    /// The same for a later column, which also draws the rule on its left.
    fn later_column(chars: usize) -> Pixels {
        first_column(chars) + RULE
    }

    /// Border-box floor of a later prose column at the test font size.
    fn readable_later_column() -> Pixels {
        px(14. * READABLE_FLOOR_EMS) + PAD_X * 2. + RULE
    }

    const ISSUES: &str = "| # | Issue |\n\
        |---|-------|\n\
        | 863 | Chat permanently bricked after image upload on Kimi models |\n\
        | 780 | Desktop: using Maple chat disrupts the local API proxy |";

    #[gpui::test]
    fn columns_take_their_content_width_and_the_table_hugs_them(cx: &mut TestAppContext) {
        let mut cx = render_at(ISSUES, 800., cx);
        let number = cell(&mut cx, 1, 0);
        let issue = cell(&mut cx, 1, 1);
        assert_eq!(
            number.size.width,
            first_column(3),
            "the number column fits `863`"
        );
        assert_eq!(
            issue.size.width,
            later_column("Chat permanently bricked after image upload on Kimi models".len())
        );
        let table = bounds(&mut cx, "md-table".into());
        assert_eq!(table.size.width, number.size.width + issue.size.width);
        assert!(table.size.width < px(800.), "the table hugs its columns");
        // Every row shares the widths.
        assert_eq!(cell(&mut cx, 0, 0).size.width, number.size.width);
        assert_eq!(cell(&mut cx, 2, 1).size.width, issue.size.width);
    }

    #[gpui::test]
    fn the_prose_column_absorbs_the_deficit(cx: &mut TestAppContext) {
        let mut cx = render_at(ISSUES, 300., cx);
        let number = cell(&mut cx, 1, 0);
        let issue = cell(&mut cx, 1, 1);
        assert_eq!(
            number.size.width,
            first_column(3),
            "`863` stays on one line"
        );
        let table = bounds(&mut cx, "md-table".into());
        assert_eq!(table.size.width, px(300.) - CONTAINER_PAD - FRAME);
        assert_eq!(issue.size.width, table.size.width - number.size.width);
        for row in 0..3 {
            for col in 0..2 {
                assert!(cell(&mut cx, row, col).right() <= px(300.) - CONTAINER_PAD);
            }
        }
        // Cells stretch to their row, so compare with the one-line header.
        assert!(
            cell(&mut cx, 1, 1).size.height > cell(&mut cx, 0, 1).size.height,
            "the long issue wrapped"
        );
    }

    #[gpui::test]
    fn short_columns_are_rigid_and_prose_keeps_its_measure(cx: &mut TestAppContext) {
        let source = "| a | b |\n|---|---|\n| ab cd | x/y |\n| ok | this prose column is long enough to take every pixel the row can spare |";
        // Room for "ab cd" and the prose floor, but not for more.
        let mut wide = render_at(source, 300., cx);
        let table = bounds(&mut wide, "md-table".into());
        assert_eq!(table.size.width, px(300.) - CONTAINER_PAD - FRAME);
        // "ab cd" is narrower than the readable measure, so it never wraps.
        assert_eq!(cell(&mut wide, 1, 0).size.width, first_column(5));
        assert_eq!(
            cell(&mut wide, 1, 1).size.width,
            table.size.width - first_column(5),
            "the prose column takes what the short column leaves"
        );
        assert!(cell(&mut wide, 1, 1).size.width > readable_later_column());

        // Less room than the floors need: the prose column stops at its
        // measure instead of its widest word, and the table scrolls.
        let mut narrow = render_at(source, 160., cx);
        assert_eq!(cell(&mut narrow, 1, 0).size.width, first_column(5));
        assert_eq!(cell(&mut narrow, 1, 1).size.width, readable_later_column());
        let table = bounds(&mut narrow, "md-table".into());
        assert_eq!(table.size.width, first_column(5) + readable_later_column());
        assert!(table.size.width > px(160.) - CONTAINER_PAD - FRAME);
    }

    #[gpui::test]
    fn an_unbreakable_run_makes_the_table_scroll(cx: &mut TestAppContext) {
        let source = "| hash |\n|---|\n| 0123456789abcdef0123456789abcdef0123456789 |";
        let mut cx = render_at(source, 200., cx);
        let hash = cell(&mut cx, 1, 0);
        assert_eq!(hash.size.width, first_column(42));
        let table = bounds(&mut cx, "md-table".into());
        assert_eq!(table.size.width, hash.size.width);
        assert!(table.size.width > px(200.));
    }
}
