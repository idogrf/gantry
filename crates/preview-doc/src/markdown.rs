//! Markdown documents for the preview pane: parsed once into blocks (the unit of selection and copy) and rows (the
//! unit of a virtualized list), with links and images classified. No drawing code.

use crate::highlight::{Highlights, MAX_HIGHLIGHT_BYTES, highlight_code};
use crate::words::{VisualText, logical_words, longest_word, visual_words};
use app_core::bidi::is_rtl_letter;
use comrak::nodes::{AlertType, ListType, Node, NodeList, NodeTable, NodeValue, TableAlignment};
use comrak::{Anchorizer, Arena, Options, parse_document};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

/// Markdown above this many bytes is shown as plain text by the caller.
pub const MAX_MARKDOWN_BYTES: usize = 2 * 1024 * 1024;
/// Code lines per list row of a code block.
pub const CODE_ROWS: usize = 64;

/// The text styles of one run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlineStyle {
    /// Strong emphasis.
    pub bold: bool,
    /// Emphasis.
    pub italic: bool,
    /// Strikethrough.
    pub strike: bool,
    /// Code (also math).
    pub code: bool,
}

/// One run of a block's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inline {
    /// Byte range in the block's text; the runs of a block cover its whole text in order.
    pub range: Range<usize>,
    /// The run's styles.
    pub style: InlineStyle,
    /// Index into `MdDoc::links` when the run is (part of) a link.
    pub link: Option<usize>,
}

/// What a block is.
#[derive(Debug)]
pub enum BlockKind {
    /// A paragraph, list item paragraph, footnote paragraph or HTML block.
    Paragraph,
    /// A heading.
    Heading {
        /// 1 to 6.
        level: u8,
    },
    /// A code block (also math).
    Code {
        /// The first word of the info string.
        lang: Option<String>,
        /// The colors, when the language is known and the budget allowed.
        highlights: Option<Highlights>,
    },
    /// A table cell.
    Cell {
        /// Whether the cell is in the header row.
        header: bool,
    },
}

/// The unit of selection and copy.
#[derive(Debug)]
pub struct Block {
    /// The text; a hard break is `\n`, a soft break a space.
    pub text: String,
    /// The runs of `text`.
    pub inlines: Vec<Inline>,
    /// What the block is.
    pub kind: BlockKind,
    /// The words and run lookup of a text block with a right-to-left letter (never a code block), made once here so
    /// drawing never splits words.
    pub rtl: Option<RtlText>,
    /// The byte offset where each line of a code block starts (empty for any other block).
    pub line_starts: Vec<usize>,
}

/// What a text block with a right-to-left letter needs to be drawn a word at a time.
#[derive(Debug)]
pub struct RtlText {
    /// The words in display order, for the right-to-left display.
    pub visual: VisualText,
    /// The words as typed, for the display switched off.
    pub logical: VisualText,
    /// The index into the block's `inlines` of each byte of its text.
    inline_at: Vec<u32>,
}

impl RtlText {
    fn new(text: &str, inlines: &[Inline]) -> Self {
        let mut inline_at = vec![u32::MAX; text.len()];
        for (ix, inline) in inlines.iter().enumerate() {
            for slot in inline_at.get_mut(inline.range.clone()).unwrap_or_default() {
                *slot = ix as u32;
            }
        }
        Self { visual: visual_words(text), logical: logical_words(text), inline_at }
    }

    /// The index of the inline run that holds byte `offset` of the block's text.
    pub fn inline_of(&self, offset: usize) -> Option<usize> {
        self.inline_at.get(offset).filter(|ix| **ix != u32::MAX).map(|ix| *ix as usize)
    }
}

/// A list item's marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    /// A bullet.
    Bullet,
    /// A number of an ordered list.
    Number(u64),
    /// A task; `true` when checked.
    Task(bool),
}

/// The kind of a GitHub alert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertKind {
    /// `[!NOTE]`.
    Note,
    /// `[!TIP]`.
    Tip,
    /// `[!IMPORTANT]`.
    Important,
    /// `[!WARNING]`.
    Warning,
    /// `[!CAUTION]`.
    Caution,
}

impl AlertKind {
    /// The title drawn above the alert.
    pub fn title(self) -> &'static str {
        match self {
            AlertKind::Note => "Note",
            AlertKind::Tip => "Tip",
            AlertKind::Important => "Important",
            AlertKind::Warning => "Warning",
            AlertKind::Caution => "Caution",
        }
    }
}

/// A table column's alignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    /// Left (the default).
    Left,
    /// Centered.
    Center,
    /// Right.
    Right,
}

/// What a list row shows.
#[derive(Debug)]
pub enum RowKind {
    /// A block of text (paragraph, heading, footnote paragraph, HTML).
    Text {
        /// Index into `MdDoc::blocks`.
        block: usize,
    },
    /// A list item's first paragraph.
    Item {
        /// Index into `MdDoc::blocks`.
        block: usize,
        /// The item's marker.
        marker: Marker,
    },
    /// Up to `CODE_ROWS` lines of a code block.
    Code {
        /// Index into `MdDoc::blocks`.
        block: usize,
        /// The lines of the block this row shows.
        lines: Range<usize>,
    },
    /// One table row.
    TableRow {
        /// Index into `MdDoc::tables`.
        table: usize,
        /// The `Cell` blocks, one per column.
        cells: Vec<usize>,
        /// Whether it is the header row.
        header: bool,
    },
    /// An image pulled out of a paragraph.
    Image {
        /// Index into `MdDoc::images`.
        image: usize,
    },
    /// A thematic break (also before the footnotes).
    Rule,
    /// The title line of an alert.
    AlertTitle(AlertKind),
}

/// One row of the document's list.
#[derive(Debug)]
pub struct Row {
    /// What the row shows.
    pub kind: RowKind,
    /// Blockquote depth (an alert counts).
    pub quote: u8,
    /// The alert the row is inside, if any.
    pub alert: Option<AlertKind>,
    /// List depth; paragraphs and code inside list items keep their item's.
    pub indent: u8,
    /// Space above the row in base px.
    pub space_above: f32,
}

/// Where a link goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkTarget {
    /// An `http` or `https` address, opened in the browser.
    Web(String),
    /// A file path, absolute and lexically normalized, opened as a linked tab.
    File(PathBuf),
    /// A heading or footnote anchor of this document.
    Anchor(String),
    /// Any other scheme (and anything unusable): drawn as plain text, never opened.
    Inert(String),
}

/// Where an image comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageSrc {
    /// A scheme-less path, absolute and lexically normalized.
    File(PathBuf),
    /// An `http` or `https` address, never fetched.
    Web(String),
    /// Anything else.
    Bad,
}

/// An image of the document.
#[derive(Debug)]
pub struct DocImage {
    /// Where it comes from.
    pub src: ImageSrc,
    /// Its alt text.
    pub alt: String,
    /// Index into `MdDoc::links` when the image is inside a link.
    pub link: Option<usize>,
}

/// A table's layout.
#[derive(Debug)]
pub struct Table {
    /// Alignment per column.
    pub align: Vec<Align>,
    /// Each column's widest cell, in characters.
    pub widest: Vec<usize>,
    /// Each column's longest word, in characters.
    pub longest: Vec<usize>,
}

/// A parsed markdown document.
#[derive(Debug)]
pub struct MdDoc {
    /// The blocks, in document order.
    pub blocks: Vec<Block>,
    /// The rows of the list.
    pub rows: Vec<Row>,
    /// The link targets the runs point at.
    pub links: Vec<LinkTarget>,
    /// The images.
    pub images: Vec<DocImage>,
    /// The tables.
    pub tables: Vec<Table>,
    /// Whether the document reads right to left (its first alphabetic character outside code).
    pub rtl: bool,
    source: String,
    anchors: Vec<(String, usize)>,
}

impl MdDoc {
    /// The markdown text this was parsed from (a leading BOM dropped).
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The row a heading or footnote anchor leads to.
    pub fn anchor_row(&self, anchor: &str) -> Option<usize> {
        let find = |name: &str| self.anchors.iter().find(|(a, _)| a == name).map(|(_, row)| *row);
        find(anchor).or_else(|| find(&anchor.to_lowercase()))
    }

    /// The text of `range` of a block, clamped to the text; its ends move out to character boundaries.
    pub fn copy(&self, block: usize, range: Range<usize>) -> String {
        let Some(block) = self.blocks.get(block) else {
            return String::new();
        };
        let text = &block.text;
        let mut start = range.start.min(text.len());
        let mut end = range.end.min(text.len());
        while !text.is_char_boundary(start) {
            start -= 1;
        }
        while !text.is_char_boundary(end) {
            end += 1;
        }
        text.get(start..end).unwrap_or_default().to_string()
    }
}

/// Parse markdown. `folder` is where relative links and images start. Runs on the caller's thread and highlights
/// code blocks, so call it off the UI thread.
pub fn parse(text: &str, folder: &Path) -> MdDoc {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.footnotes = true;
    options.extension.alerts = true;
    options.extension.math_dollars = true;
    options.extension.math_code = true;
    let arena = Arena::new();
    let root = parse_document(&arena, text, &options);

    let mut b = Builder {
        folder,
        doc: MdDoc {
            blocks: Vec::new(),
            rows: Vec::new(),
            links: Vec::new(),
            images: Vec::new(),
            tables: Vec::new(),
            rtl: false,
            source: text.to_string(),
            anchors: Vec::new(),
        },
        anchorizer: Anchorizer::new(),
        highlighted: 0,
        depth: 0,
        footnotes: Vec::new(),
    };
    for child in root.children() {
        b.block(child, Ctx::default());
    }
    let footnotes = std::mem::take(&mut b.footnotes);
    if !footnotes.is_empty() {
        b.push_row(RowKind::Rule, Ctx::default(), 16.0);
        for node in footnotes {
            b.footnote(node);
        }
    }
    let mut doc = b.doc;
    doc.rtl = first_letter_is_rtl(&doc);
    doc
}

/// The first alphabetic character outside code decides the direction.
fn first_letter_is_rtl(doc: &MdDoc) -> bool {
    for block in &doc.blocks {
        if matches!(block.kind, BlockKind::Code { .. }) {
            continue;
        }
        for run in block.inlines.iter().filter(|i| !i.style.code) {
            if let Some(ch) = block.text[run.range.clone()].chars().find(|c| c.is_alphabetic()) {
                return is_rtl_letter(ch);
            }
        }
    }
    false
}

/// The deepest nesting of blocks or inlines that is walked; deeper content is flattened to its text.
const MAX_DEPTH: usize = 100;

/// Where a block sits: quote depth, alert, list depth.
#[derive(Clone, Copy, Default)]
struct Ctx {
    quote: u8,
    alert: Option<AlertKind>,
    /// Indent of the rows here.
    indent: u8,
    /// Lists entered so far.
    depth: u8,
}

/// The text and runs being collected for one block, and the images pulled out of it.
#[derive(Default)]
struct Collect {
    text: String,
    inlines: Vec<Inline>,
    parts: Vec<Part>,
}

enum Part {
    Text(String, Vec<Inline>),
    Image(usize),
}

impl Collect {
    fn push(&mut self, s: &str, style: InlineStyle, link: Option<usize>) {
        if s.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(s);
        match self.inlines.last_mut() {
            Some(last) if last.style == style && last.link == link => {
                last.range.end = self.text.len();
                return;
            }
            _ => {}
        }
        self.inlines.push(Inline { range: start..self.text.len(), style, link });
    }

    /// Close the text before an image.
    fn cut(&mut self) {
        let text = std::mem::take(&mut self.text);
        let inlines = std::mem::take(&mut self.inlines);
        self.parts.push(Part::Text(text, inlines));
    }

    fn finish(mut self) -> Vec<Part> {
        self.cut();
        self.parts
    }
}

/// Cut surrounding white space off a text, keeping its runs in step; `None` when nothing is left.
fn trimmed(text: String, inlines: Vec<Inline>) -> Option<(String, Vec<Inline>)> {
    let lead = text.len() - text.trim_start().len();
    let body = text.trim();
    if body.is_empty() {
        return None;
    }
    let end = lead + body.len();
    let runs = inlines
        .into_iter()
        .filter_map(|i| {
            let (s, e) = (i.range.start.max(lead), i.range.end.min(end));
            (s < e).then(|| Inline { range: s - lead..e - lead, ..i })
        })
        .collect();
    Some((body.to_string(), runs))
}

struct Builder<'a, 'n> {
    folder: &'a Path,
    doc: MdDoc,
    anchorizer: Anchorizer,
    /// Bytes of code highlighted so far.
    highlighted: usize,
    /// Containers and inline levels being walked; past `MAX_DEPTH` the rest shows as plain text.
    depth: usize,
    footnotes: Vec<Node<'n>>,
}

impl<'n> Builder<'_, 'n> {
    fn push_row(&mut self, kind: RowKind, ctx: Ctx, space_above: f32) -> usize {
        let space_above = if self.doc.rows.is_empty() { 0.0 } else { space_above };
        self.doc.rows.push(Row { kind, quote: ctx.quote, alert: ctx.alert, indent: ctx.indent, space_above });
        self.doc.rows.len() - 1
    }

    /// The gap above a block that follows another one.
    fn gap(&self) -> f32 {
        match self.doc.rows.last() {
            Some(Row { kind: RowKind::AlertTitle(_), .. }) => 4.0,
            _ => 16.0,
        }
    }

    fn push_block(&mut self, text: String, inlines: Vec<Inline>, kind: BlockKind) -> usize {
        let rtl = (!matches!(kind, BlockKind::Code { .. }) && text.chars().any(is_rtl_letter))
            .then(|| RtlText::new(&text, &inlines));
        let line_starts = if matches!(kind, BlockKind::Code { .. }) {
            std::iter::once(0).chain(text.match_indices('\n').map(|(at, _)| at + 1)).collect()
        } else {
            Vec::new()
        };
        self.doc.blocks.push(Block { text, inlines, kind, rtl, line_starts });
        self.doc.blocks.len() - 1
    }

    fn block(&mut self, node: Node<'n>, ctx: Ctx) {
        if self.depth >= MAX_DEPTH {
            let mut c = Collect::default();
            c.push(plain_text(node).trim(), InlineStyle::default(), None);
            if !c.text.is_empty() {
                let block = self.push_block(c.text, c.inlines, BlockKind::Paragraph);
                let gap = self.gap();
                self.push_row(RowKind::Text { block }, ctx, gap);
            }
            return;
        }
        self.depth += 1;
        self.block_at(node, ctx);
        self.depth -= 1;
    }

    fn block_at(&mut self, node: Node<'n>, ctx: Ctx) {
        let data = node.data();
        match &data.value {
            NodeValue::Paragraph => self.paragraph(node, ctx, None),
            NodeValue::Heading(h) => {
                let mut c = Collect::default();
                self.inline_children(node, &mut c, InlineStyle::default(), None, false);
                let slug = self.anchorizer.anchorize(&c.text);
                let gap = if self.gap() > 4.0 { 24.0 } else { 4.0 };
                let block = self.push_block(c.text, c.inlines, BlockKind::Heading { level: h.level.clamp(1, 6) });
                let row = self.push_row(RowKind::Text { block }, ctx, gap);
                self.doc.anchors.push((slug, row));
            }
            NodeValue::ThematicBreak => {
                let gap = self.gap();
                self.push_row(RowKind::Rule, ctx, gap);
            }
            NodeValue::BlockQuote | NodeValue::MultilineBlockQuote(_) => {
                let inner = Ctx { quote: ctx.quote.saturating_add(1), ..ctx };
                self.children(node, inner);
            }
            NodeValue::Alert(alert) => {
                let kind = match alert.alert_type {
                    AlertType::Note => AlertKind::Note,
                    AlertType::Tip => AlertKind::Tip,
                    AlertType::Important => AlertKind::Important,
                    AlertType::Warning => AlertKind::Warning,
                    AlertType::Caution => AlertKind::Caution,
                };
                let inner = Ctx { quote: ctx.quote.saturating_add(1), alert: Some(kind), ..ctx };
                let gap = self.gap();
                self.push_row(RowKind::AlertTitle(kind), inner, gap);
                self.children(node, inner);
            }
            NodeValue::List(list) => self.list(node, list, ctx),
            NodeValue::CodeBlock(code) => {
                let math = code.info.split_whitespace().next() == Some("math");
                let lang = if math { None } else { code.info.split_whitespace().next().map(str::to_string) };
                let text = code.literal.strip_suffix('\n').unwrap_or(&code.literal).to_string();
                self.code(text, lang, ctx);
            }
            NodeValue::HtmlBlock(html) => {
                let text = html.literal.trim_end_matches(['\n', '\r']).to_string();
                let mut c = Collect::default();
                c.push(&text, InlineStyle::default(), None);
                let block = self.push_block(c.text, c.inlines, BlockKind::Paragraph);
                let gap = self.gap();
                self.push_row(RowKind::Text { block }, ctx, gap);
            }
            NodeValue::Table(table) => self.table(node, table, ctx),
            NodeValue::FootnoteDefinition(_) => self.footnotes.push(node),
            _ => self.children(node, ctx),
        }
    }

    fn children(&mut self, node: Node<'n>, ctx: Ctx) {
        for child in node.children() {
            self.block(child, ctx);
        }
    }

    fn code(&mut self, text: String, lang: Option<String>, ctx: Ctx) {
        let highlights = match &lang {
            Some(l) if self.highlighted + text.len() <= MAX_HIGHLIGHT_BYTES => {
                let h = highlight_code(l, &text);
                if h.is_some() {
                    self.highlighted += text.len();
                }
                h
            }
            _ => None,
        };
        let lines = text.split('\n').count();
        let block = self.push_block(text, Vec::new(), BlockKind::Code { lang, highlights });
        let mut gap = self.gap();
        for from in (0..lines).step_by(CODE_ROWS) {
            let kind = RowKind::Code { block, lines: from..(from + CODE_ROWS).min(lines) };
            self.push_row(kind, ctx, gap);
            gap = 0.0;
        }
    }

    /// The marker of an item whose first block is not a paragraph (or is an image): an empty text block.
    fn empty_item(&mut self, marker: Marker, gap: f32, ctx: Ctx) {
        let block = self.push_block(String::new(), Vec::new(), BlockKind::Paragraph);
        self.push_row(RowKind::Item { block, marker }, ctx, gap);
    }

    /// A paragraph: its text, then each image in order with the text after it. `marker` makes the first row an item.
    fn paragraph(&mut self, node: Node<'n>, ctx: Ctx, mut marker: Option<(Marker, f32)>) {
        let mut c = Collect::default();
        self.inline_children(node, &mut c, InlineStyle::default(), None, true);
        let mut after_text = false;
        for part in c.finish() {
            match part {
                Part::Text(text, inlines) => {
                    let Some((text, inlines)) = trimmed(text, inlines) else { continue };
                    let block = self.push_block(text, inlines, BlockKind::Paragraph);
                    if let Some((marker, gap)) = marker.take() {
                        self.push_row(RowKind::Item { block, marker }, ctx, gap);
                    } else {
                        let gap = if after_text { 8.0 } else { self.gap() };
                        self.push_row(RowKind::Text { block }, ctx, gap);
                    }
                    after_text = true;
                }
                Part::Image(image) => {
                    if let Some((marker, gap)) = marker.take() {
                        self.empty_item(marker, gap, ctx);
                        after_text = true;
                    }
                    let gap = if after_text { 8.0 } else { self.gap() };
                    self.push_row(RowKind::Image { image }, ctx, gap);
                    after_text = true;
                }
            }
        }
        if let Some((marker, gap)) = marker {
            self.empty_item(marker, gap, ctx);
        }
    }

    fn list(&mut self, node: Node<'n>, list: &NodeList, ctx: Ctx) {
        let inner = Ctx { indent: ctx.depth, depth: ctx.depth.saturating_add(1), ..ctx };
        for (i, item) in node.children().enumerate() {
            let task = match item.data().value {
                NodeValue::TaskItem(t) => Some(t.symbol.is_some()),
                _ => None,
            };
            let marker = match (task, list.list_type) {
                (Some(done), _) => Marker::Task(done),
                (None, ListType::Ordered) => Marker::Number(list.start as u64 + i as u64),
                (None, ListType::Bullet) => Marker::Bullet,
            };
            // A list's first item sits below the text before it, or right under its parent item.
            let gap = if i > 0 || ctx.depth > 0 { 4.0 } else { self.gap() };
            self.item(item, marker, gap, inner);
        }
    }

    fn item(&mut self, node: Node<'n>, marker: Marker, gap: f32, ctx: Ctx) {
        let mut pending = Some((marker, gap));
        // What follows the marker line sits in the text column, one step in from the marker.
        let text_column = Ctx { indent: ctx.indent.saturating_add(1), ..ctx };
        for child in node.children() {
            let is_paragraph = matches!(child.data().value, NodeValue::Paragraph);
            if is_paragraph {
                let at = if pending.is_some() { ctx } else { text_column };
                self.paragraph(child, at, pending.take());
                continue;
            }
            if let Some((marker, gap)) = pending.take() {
                self.empty_item(marker, gap, ctx);
            }
            self.block(child, text_column);
        }
        if let Some((marker, gap)) = pending {
            self.empty_item(marker, gap, ctx);
        }
    }

    fn table(&mut self, node: Node<'n>, table: &NodeTable, ctx: Ctx) {
        let index = self.doc.tables.len();
        let align = table
            .alignments
            .iter()
            .map(|a| match a {
                TableAlignment::Center => Align::Center,
                TableAlignment::Right => Align::Right,
                _ => Align::Left,
            })
            .collect();
        self.doc.tables.push(Table { align, widest: Vec::new(), longest: Vec::new() });
        let mut widths: Vec<usize> = Vec::new();
        let mut longest: Vec<usize> = Vec::new();
        let mut gap = self.gap();
        for row in node.children() {
            let header = matches!(row.data().value, NodeValue::TableRow(true));
            let mut cells = Vec::new();
            for (col, cell) in row.children().enumerate() {
                let mut c = Collect::default();
                self.inline_children(cell, &mut c, InlineStyle::default(), None, false);
                let width = c.text.lines().map(|l| l.chars().count()).max().unwrap_or(0);
                if col >= widths.len() {
                    widths.resize(col + 1, 0);
                }
                widths[col] = widths[col].max(width);
                longest.resize(widths.len(), 0);
                longest[col] = longest[col].max(longest_word(&c.text));
                cells.push(self.push_block(c.text, c.inlines, BlockKind::Cell { header }));
            }
            self.push_row(RowKind::TableRow { table: index, cells, header }, ctx, gap);
            gap = 0.0;
        }
        widths.resize(widths.len().max(table.alignments.len()), 0);
        longest.resize(widths.len(), 0);
        self.doc.tables[index].widest = widths;
        self.doc.tables[index].longest = longest;
    }

    fn footnote(&mut self, node: Node<'n>) {
        let name = match &node.data().value {
            NodeValue::FootnoteDefinition(def) => def.name.clone(),
            _ => return,
        };
        let first = self.doc.rows.len();
        self.children(node, Ctx::default());
        if self.doc.rows.len() > first {
            self.doc.anchors.push((format!("fn-{name}"), first));
        }
    }

    /// Collect the inline children of `node`. With `pull`, an image ends the text and becomes its own part;
    /// without, it shows as its alt text.
    fn inline_children(
        &mut self,
        node: Node<'n>,
        c: &mut Collect,
        style: InlineStyle,
        link: Option<usize>,
        pull: bool,
    ) {
        if self.depth >= MAX_DEPTH {
            c.push(&plain_text(node), style, link);
            return;
        }
        self.depth += 1;
        self.inline_run(node, c, style, link, pull);
        self.depth -= 1;
    }

    fn inline_run(&mut self, node: Node<'n>, c: &mut Collect, style: InlineStyle, link: Option<usize>, pull: bool) {
        for child in node.children() {
            let data = child.data();
            match &data.value {
                NodeValue::Text(t) => c.push(t, style, link),
                NodeValue::SoftBreak => c.push(" ", style, link),
                NodeValue::LineBreak => c.push("\n", style, link),
                NodeValue::Code(code) => c.push(&code.literal, InlineStyle { code: true, ..style }, link),
                NodeValue::Math(math) => c.push(&math.literal, InlineStyle { code: true, ..style }, link),
                NodeValue::HtmlInline(html) | NodeValue::Raw(html) => c.push(html, style, link),
                NodeValue::Emph => self.inline_children(child, c, InlineStyle { italic: true, ..style }, link, pull),
                NodeValue::Strong => self.inline_children(child, c, InlineStyle { bold: true, ..style }, link, pull),
                NodeValue::Strikethrough => {
                    self.inline_children(child, c, InlineStyle { strike: true, ..style }, link, pull)
                }
                NodeValue::Link(l) => {
                    self.doc.links.push(classify_link(&l.url, self.folder));
                    let index = self.doc.links.len() - 1;
                    self.inline_children(child, c, style, Some(index), pull);
                }
                NodeValue::Image(l) => {
                    let alt = plain_text(child);
                    if pull {
                        let src = classify_image(&l.url, self.folder);
                        self.doc.images.push(DocImage { src, alt, link });
                        c.cut();
                        c.parts.push(Part::Image(self.doc.images.len() - 1));
                    } else {
                        c.push(&alt, style, link);
                    }
                }
                NodeValue::FootnoteReference(r) => {
                    self.doc.links.push(LinkTarget::Anchor(format!("fn-{}", r.name)));
                    let index = self.doc.links.len() - 1;
                    c.push(&format!("[{}]", r.ix), style, Some(index));
                }
                NodeValue::TaskItem(t) => c.push(if t.symbol.is_some() { "[x] " } else { "[ ] " }, style, link),
                _ => self.inline_children(child, c, style, link, pull),
            }
        }
    }
}

/// The plain text under a node (an image's alt text).
fn plain_text(node: Node<'_>) -> String {
    let mut out = String::new();
    for d in node.descendants() {
        match &d.data().value {
            NodeValue::Text(t) => out.push_str(t),
            NodeValue::Code(c) => out.push_str(&c.literal),
            NodeValue::CodeBlock(c) => {
                out.push_str(&c.literal);
                out.push(' ');
            }
            NodeValue::HtmlBlock(h) => {
                out.push_str(&h.literal);
                out.push(' ');
            }
            NodeValue::SoftBreak | NodeValue::LineBreak => out.push(' '),
            _ => {}
        }
    }
    out
}

/// What a url is before any decision: a scheme, or a path with its query and fragment cut off.
enum Split<'u> {
    Scheme(&'u str),
    Path { path: &'u str, fragment: Option<&'u str> },
}

fn has_scheme(url: &str) -> Option<&str> {
    let colon = url.find(':')?;
    let scheme = &url[..colon];
    let mut chars = scheme.chars();
    (chars.next()?.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')))
        .then_some(scheme)
}

fn split_url(url: &str) -> Split<'_> {
    let url = url.trim();
    if let Some(scheme) = has_scheme(url) {
        return Split::Scheme(scheme);
    }
    let (rest, fragment) = match url.split_once('#') {
        Some((rest, fragment)) => (rest, Some(fragment)),
        None => (url, None),
    };
    let path = rest.split_once('?').map_or(rest, |(path, _)| path);
    Split::Path { path, fragment }
}

fn is_web(scheme: &str) -> bool {
    scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
}

/// A scheme-less path joined to `folder` and normalized lexically (never canonicalized: the folder may be on another
/// host). `None` for an empty path, a network path (`//host`) or a NUL.
fn local_path(path: &str, folder: &Path) -> Option<PathBuf> {
    if path.is_empty() || path.starts_with("//") {
        return None;
    }
    let path = percent_decode(path);
    // A scheme that was only spelled with `%XX` (or leading white space) is still a scheme.
    if path.contains('\0') || has_scheme(path.trim_start()).is_some() {
        return None;
    }
    let joined = folder.join(&path);
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// Decode `%XX`; the text stays as it was when the result is not UTF-8.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() && hex(bytes[i + 1]).is_some() && hex(bytes[i + 2]).is_some() {
            out.push((hex(bytes[i + 1]).unwrap_or(0) * 16 + hex(bytes[i + 2]).unwrap_or(0)) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// Decide what a link does; the url is untrusted.
///
/// Only `http` and `https` are web links, any other scheme is inert, a path without a scheme is a file (relative to
/// `folder`, `%XX` decoded, query and fragment dropped) and `#x` (or a query with only a fragment) an anchor.
pub fn classify_link(url: &str, folder: &Path) -> LinkTarget {
    match split_url(url) {
        Split::Scheme(scheme) if is_web(scheme) => LinkTarget::Web(url.trim().to_string()),
        Split::Scheme(_) => LinkTarget::Inert(url.to_string()),
        Split::Path { path: "", fragment: Some(f) } if !f.is_empty() => LinkTarget::Anchor(percent_decode(f)),
        Split::Path { path, .. } => match local_path(path, folder) {
            Some(path) => LinkTarget::File(path),
            None => LinkTarget::Inert(url.to_string()),
        },
    }
}

/// Decide where an image comes from; the url is untrusted. A scheme-less path is a file, `http(s)` a web image that
/// is never fetched, anything else bad.
pub fn classify_image(url: &str, folder: &Path) -> ImageSrc {
    match split_url(url) {
        Split::Scheme(scheme) if is_web(scheme) => ImageSrc::Web(url.trim().to_string()),
        Split::Scheme(_) => ImageSrc::Bad,
        Split::Path { path, .. } => local_path(path, folder).map_or(ImageSrc::Bad, ImageSrc::File),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> MdDoc {
        parse(text, Path::new("/d/docs"))
    }

    fn block_of(row: &Row) -> usize {
        match row.kind {
            RowKind::Text { block } | RowKind::Item { block, .. } | RowKind::Code { block, .. } => block,
            _ => panic!("no block: {row:?}"),
        }
    }

    fn run<'a>(d: &'a MdDoc, block: usize, word: &str) -> &'a Inline {
        let b = &d.blocks[block];
        b.inlines.iter().find(|i| &b.text[i.range.clone()] == word).unwrap_or_else(|| panic!("{word}: {b:?}"))
    }

    #[test]
    fn headings_paragraphs_and_inline_styles() {
        let d = doc("# Title\n\nSome **bold** and `code` and ~~gone~~");
        assert_eq!(d.rows.len(), 2);
        assert!(matches!(d.blocks[block_of(&d.rows[0])].kind, BlockKind::Heading { level: 1 }));
        let p = block_of(&d.rows[1]);
        assert!(matches!(d.blocks[p].kind, BlockKind::Paragraph));
        assert_eq!(d.blocks[p].text, "Some bold and code and gone");
        assert!(run(&d, p, "bold").style.bold);
        assert!(run(&d, p, "code").style.code);
        assert!(run(&d, p, "gone").style.strike);
        assert_eq!(run(&d, p, "Some ").style, InlineStyle::default());
        let covered: usize = d.blocks[p].inlines.iter().map(|i| i.range.len()).sum();
        assert_eq!(covered, d.blocks[p].text.len());
    }

    #[test]
    fn lists_and_tasks() {
        let d = doc("- a\n  - b\n1. c\n- [x] d");
        let items: Vec<(Marker, u8)> = d
            .rows
            .iter()
            .map(|r| match r.kind {
                RowKind::Item { marker, .. } => (marker, r.indent),
                _ => panic!("{r:?}"),
            })
            .collect();
        assert_eq!(items, [(Marker::Bullet, 0), (Marker::Bullet, 1), (Marker::Number(1), 0), (Marker::Task(true), 0)]);
        assert_eq!(d.blocks[block_of(&d.rows[3])].text, "d");
    }

    #[test]
    fn item_continuations_sit_in_the_text_column() {
        let d = doc("1. Install:\n\n   ```sh\n   make\n   ```\n\n   More.\n\n- ```\n  code\n  ```\n- - nested");
        let rows: Vec<(&str, u8)> = d
            .rows
            .iter()
            .map(|r| match r.kind {
                RowKind::Item { .. } => ("item", r.indent),
                RowKind::Code { .. } => ("code", r.indent),
                RowKind::Text { .. } => ("text", r.indent),
                _ => ("other", r.indent),
            })
            .collect();
        assert_eq!(
            rows,
            [("item", 0), ("code", 1), ("text", 1), ("item", 0), ("code", 1), ("item", 0), ("item", 1)],
            "{rows:?}"
        );
    }

    #[test]
    fn a_long_code_block_is_split_into_rows() {
        let d = doc(&format!("```\n{}```", "line\n".repeat(130)));
        assert_eq!(d.blocks.len(), 1);
        let lines: Vec<Range<usize>> = d
            .rows
            .iter()
            .map(|r| match &r.kind {
                RowKind::Code { lines, .. } => lines.clone(),
                _ => panic!("{r:?}"),
            })
            .collect();
        assert_eq!(lines, [0..64, 64..128, 128..130]);
        assert_eq!(d.rows[1].space_above, 0.0);
        assert_eq!(d.rows[2].space_above, 0.0);
    }

    fn highlights_of(d: &MdDoc, block: usize) -> bool {
        match &d.blocks[block].kind {
            BlockKind::Code { highlights, .. } => highlights.is_some(),
            k => panic!("{k:?}"),
        }
    }

    #[test]
    fn unknown_languages_stay_plain() {
        let d = doc("```nosuch\nx\n```\n\n```\ny\n```\n\n```rust\nfn main() {}\n```");
        assert!(!highlights_of(&d, 0));
        assert!(!highlights_of(&d, 1));
        assert!(highlights_of(&d, 2));
        assert!(matches!(&d.blocks[2].kind, BlockKind::Code { lang: Some(l), .. } if l == "rust"));
    }

    #[test]
    fn the_highlight_budget_is_shared() {
        let body = "fn main() { let x = 1; }\n".repeat(600 * 1024 / 25);
        let d = doc(&format!("```rust\n{body}```\n\n```rust\n{body}```"));
        assert!(highlights_of(&d, 0));
        assert!(!highlights_of(&d, 1));
    }

    #[test]
    fn tables_split_into_rows_with_column_measures() {
        let d = doc("| a | bb | c |\n|:--|:--:|--:|\n| 1 | 22 | 333 |\n| 4 | 55 | 666 |");
        assert_eq!(d.rows.len(), 3);
        let headers: Vec<bool> = d
            .rows
            .iter()
            .map(|r| match &r.kind {
                RowKind::TableRow { cells, header, .. } => {
                    assert_eq!(cells.len(), 3);
                    *header
                }
                _ => panic!("{r:?}"),
            })
            .collect();
        assert_eq!(headers, [true, false, false]);
        assert_eq!(d.tables.len(), 1);
        assert_eq!(d.tables[0].align, [Align::Left, Align::Center, Align::Right]);
        assert_eq!(d.tables[0].widest, [1, 2, 3]);
        assert_eq!(d.tables[0].longest, [1, 2, 3]);
        assert!(matches!(d.blocks[0].kind, BlockKind::Cell { header: true }));
        assert!(matches!(d.blocks[3].kind, BlockKind::Cell { header: false }));
        assert_eq!(d.rows[1].space_above, 0.0);
    }

    #[test]
    fn table_columns_record_the_widest_cell_and_the_longest_word() {
        let d = doc("| name | note |\n|--|--|\n| ab | one two three |\n| abcdefgh | x |");
        assert_eq!(d.tables[0].widest, [8, 13]);
        assert_eq!(d.tables[0].longest, [8, 5]);
    }

    #[test]
    fn right_to_left_blocks_carry_their_words_and_runs_from_the_parse() {
        let d = doc("# שלום\n\nשלום **עולם** abc\n\nplain text\n\n| ת |\n|--|\n| שלום |");
        let rtl: Vec<bool> = d.blocks.iter().map(|b| b.rtl.is_some()).collect();
        // heading, paragraph, plain paragraph, header cell, body cell
        assert_eq!(rtl, [true, true, false, true, true]);
        let block = &d.blocks[1];
        let words = block.rtl.as_ref().unwrap();
        assert!(words.visual.rtl);
        assert!(!words.logical.rtl);
        // "שלום " is 9 bytes, then the bold run "עולם" (8 bytes), then " abc".
        assert_eq!(block.inlines.len(), 3);
        assert_eq!(words.inline_of(0), Some(0));
        assert_eq!(words.inline_of(9), Some(1));
        assert_eq!(words.inline_of(17), Some(2));
        assert_eq!(words.inline_of(999), None);
    }

    #[test]
    fn code_blocks_know_where_their_lines_start() {
        let d = doc("```\na\nbb\n\nשלום\n```");
        let block = &d.blocks[0];
        assert!(matches!(block.kind, BlockKind::Code { .. }));
        assert_eq!(block.line_starts, [0, 2, 5, 6]);
        assert!(d.blocks.iter().all(|b| b.rtl.is_none()), "code lines are drawn per line, not as words");
    }

    #[test]
    fn alerts_quotes_and_footnotes() {
        let d = doc("> [!WARNING]\n> careful\n\n> plain quote\n\nA note[^1].\n\n[^1]: The footnote.");
        assert!(matches!(d.rows[0].kind, RowKind::AlertTitle(AlertKind::Warning)));
        assert_eq!(d.rows[0].quote, 1);
        assert_eq!(d.rows[1].alert, Some(AlertKind::Warning));
        assert_eq!(d.rows[1].quote, 1);
        assert_eq!(d.rows[2].alert, None);
        assert_eq!(d.rows[2].quote, 1);
        assert_eq!(AlertKind::Warning.title(), "Warning");
        let p = block_of(&d.rows[3]);
        assert_eq!(d.blocks[p].text, "A note[1].");
        let link = run(&d, p, "[1]").link.expect("linked");
        assert_eq!(d.links[link], LinkTarget::Anchor("fn-1".into()));
        assert!(matches!(d.rows[4].kind, RowKind::Rule));
        let def = d.anchor_row("fn-1").expect("anchor");
        assert_eq!(def, 5);
        assert_eq!(d.blocks[block_of(&d.rows[def])].text, "The footnote.");
    }

    #[test]
    fn anchors_follow_github_slugs() {
        let d = doc("## Getting Started\n\ntext\n\n## Getting Started");
        assert_eq!(d.anchor_row("getting-started"), Some(0));
        assert_eq!(d.anchor_row("getting-started-1"), Some(2));
        assert_eq!(d.anchor_row("nope"), None);
    }

    #[test]
    fn math_and_html_show_as_text() {
        let d = doc("$x^2$\n\n<b>hi</b>\n\n$$\ny\n$$\n\n```math\nz\n```");
        let p = block_of(&d.rows[0]);
        assert_eq!(d.blocks[p].text, "x^2");
        assert!(d.blocks[p].inlines.iter().all(|i| i.style.code));
        let h = block_of(&d.rows[1]);
        assert!(matches!(d.blocks[h].kind, BlockKind::Paragraph));
        assert_eq!(d.blocks[h].text, "<b>hi</b>");
        let math = d.blocks.last().unwrap();
        assert!(matches!(&math.kind, BlockKind::Code { lang: None, .. }), "{math:?}");
        assert_eq!(math.text, "z");
    }

    #[test]
    fn images_become_rows_in_order() {
        let d = doc("a ![one](x.png) b ![two](../y.png)");
        let kinds: Vec<&str> = d
            .rows
            .iter()
            .map(|r| match r.kind {
                RowKind::Text { .. } => "text",
                RowKind::Image { .. } => "image",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["text", "image", "text", "image"]);
        assert_eq!(d.blocks[block_of(&d.rows[0])].text, "a");
        assert_eq!(d.blocks[block_of(&d.rows[2])].text, "b");
        assert!(matches!(d.rows[1].kind, RowKind::Image { image: 0 }));
        assert_eq!(d.images[0].src, ImageSrc::File("/d/docs/x.png".into()));
        assert_eq!(d.images[0].alt, "one");
        assert_eq!(d.images[1].src, ImageSrc::File("/d/y.png".into()));
    }

    #[test]
    fn a_linked_image_keeps_its_link() {
        let d = doc("[![b](x.svg)](https://x.io)");
        assert_eq!(d.rows.len(), 1);
        let link = d.images[0].link.expect("link");
        assert_eq!(d.links[link], LinkTarget::Web("https://x.io".into()));
    }

    #[test]
    fn links_are_classified() {
        let f = Path::new("/d/docs");
        assert_eq!(classify_link("https://x.io", f), LinkTarget::Web("https://x.io".into()));
        assert_eq!(classify_link("#part", f), LinkTarget::Anchor("part".into()));
        assert_eq!(classify_link("other.md#part", f), LinkTarget::File("/d/docs/other.md".into()));
        assert_eq!(classify_link("../a%20b.md", f), LinkTarget::File("/d/a b.md".into()));
        assert_eq!(classify_link("./sub/../c.md?x=1", f), LinkTarget::File("/d/docs/c.md".into()));
        assert_eq!(classify_link("/etc/hosts", f), LinkTarget::File("/etc/hosts".into()));
        for bad in ["javascript:alert(1)", "JavaScript:x", "file:///etc/passwd", "data:text/html,x", "mailto:a@b"] {
            assert!(matches!(classify_link(bad, f), LinkTarget::Inert(_)), "{bad}");
        }
    }

    #[test]
    fn web_and_odd_images_are_classified() {
        let f = Path::new("/d/docs");
        assert_eq!(classify_image("https://x/b.svg", f), ImageSrc::Web("https://x/b.svg".into()));
        assert_eq!(classify_image("data:image/png;base64,AAAA", f), ImageSrc::Bad);
        assert_eq!(classify_image("file:///x.png", f), ImageSrc::Bad);
        assert_eq!(classify_image("a/b.png", f), ImageSrc::File("/d/docs/a/b.png".into()));
    }

    #[test]
    fn direction_is_the_first_letter() {
        assert!(doc("# שלום\n\nhello").rtl);
        assert!(!doc("hello\n\nשלום").rtl);
        assert!(!doc("```\nשלום\n```\nhello").rtl);
        assert!(!doc("`שלום` hello").rtl);
        assert!(!doc("123 !").rtl);
    }

    #[test]
    fn a_leading_bom_is_dropped() {
        let d = doc("\u{feff}# T");
        assert_eq!(d.source(), "# T");
    }

    #[test]
    fn copy_stays_inside_the_block() {
        let d = doc("héllo");
        assert_eq!(d.copy(0, 2..100), "éllo");
        assert_eq!(d.copy(0, 0..2), "hé");
        let reversed = Range { start: 3, end: 2 };
        assert_eq!(d.copy(0, reversed), "");
        assert_eq!(d.copy(9, 0..1), "");
    }

    #[test]
    fn a_large_document_parses() {
        let mut text = String::new();
        let mut n = 0;
        while text.len() < 1_500_000 {
            text.push_str(&format!("## Section {n}\n\nSome *text* for section {n} with `code`.\n\n- one\n- two\n\n"));
            n += 1;
        }
        let d = doc(&text);
        assert!(d.rows.len() > 10_000, "{}", d.rows.len());
    }

    /// Run `parse` on a thread with a small stack, as a hostile file would.
    fn parse_small_stack(text: String) -> MdDoc {
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || doc(&text))
            .expect("spawn")
            .join()
            .expect("parse overflowed or panicked")
    }

    #[test]
    fn deep_quotes_do_not_overflow() {
        let d = parse_small_stack(format!("{}deep", "> ".repeat(10_000)));
        assert!(!d.rows.is_empty());
    }

    #[test]
    fn deep_lists_do_not_overflow() {
        let text: String = (0..10_000).map(|i| format!("{}- x\n", " ".repeat(i.min(2000)))).collect();
        assert!(!parse_small_stack(text).rows.is_empty());
        let nested = format!("{}deep", "- ".repeat(10_000));
        assert!(!parse_small_stack(nested).rows.is_empty());
    }

    #[test]
    fn deep_emphasis_does_not_overflow() {
        let text = format!("{}x{}", "*a _b ".repeat(10_000), "_ a*".repeat(10_000));
        assert!(!parse_small_stack(text).rows.is_empty());
        let links = format!("{}x{}", "[".repeat(10_000), "](u)".repeat(10_000));
        assert!(!parse_small_stack(links).rows.is_empty());
    }

    #[test]
    fn nesting_past_the_cap_keeps_its_text() {
        let d = doc(&format!("{}deep words", "> ".repeat(300)));
        assert!(d.blocks.iter().any(|b| b.text.contains("deep words")));
    }

    #[test]
    fn a_two_mebibyte_document_parses_on_a_small_stack() {
        let text = "Some *text* with `code` and a [link](x.md).\n\n".repeat(MAX_MARKDOWN_BYTES / 46);
        assert!(parse_small_stack(text).rows.len() > 10_000);
    }

    #[test]
    fn encoded_schemes_stay_inert() {
        let f = Path::new("/d/docs");
        for bad in
            ["%6Aavascript:alert(1)", "  javascript:x", " %6aavascript:x", "java%73cript:x", "%66ile:///etc/passwd"]
        {
            assert!(matches!(classify_link(bad, f), LinkTarget::Inert(_)), "{bad}");
        }
        assert_eq!(classify_link("a%3Ab.md", f), LinkTarget::Inert("a%3Ab.md".into()));
    }

    #[test]
    fn an_image_first_in_an_item_has_the_small_gap() {
        let d = doc("para\n\n- ![a](x.png)");
        assert!(matches!(d.rows[1].kind, RowKind::Item { .. }));
        assert_eq!(d.rows[2].space_above, 8.0);
    }
}
