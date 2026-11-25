//! Markdown rows and colored code for the preview pane: one list item per `preview_doc::markdown::Row`, drawn in a
//! GitHub-like look from the theme's tokens, and the text runs (inline styles, syntax colors, selection) the rows and
//! the plain code files share. Everything here works on what is visible; the document was parsed once, off the UI
//! thread.

use std::ops::Range;
use std::rc::Rc;

use app_core::bidi::{display_line, is_rtl_letter};
use app_core::preview::{FileKind, kind_of};
use app_core::theme_colors::mix;
use gpui::{
    AnyElement, App, Context, CursorStyle, Div, Entity, Font, FontStyle, FontWeight, HitboxBehavior, Hsla,
    InteractiveText, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, SharedString,
    StrikethroughStyle, StyledText, TextAlign, TextLayout, TextRun, TextStyle, UnderlineStyle, canvas, div, prelude::*,
    px, relative, rgb, svg,
};
use preview_doc::highlight::{Span, Token};
use preview_doc::markdown::{
    AlertKind, Align, Block, BlockKind, ImageSrc, Inline, LinkTarget, Marker, MdDoc, RowKind, RtlText,
};
use preview_doc::words::{VisualText, Word, column_fractions};

use crate::assets::COPY_ICON;
use crate::preview_view::{BlockSelection, Content, PreviewView};
use crate::terminal_view::pane_font;
use crate::theme::{RADIUS_SMALL, RADIUS_TINY, Theme, UI_FONT, ansi_color, token_color};

/// What a row needs besides the document and the theme.
pub(crate) struct RowState {
    /// The terminal font family code is drawn in.
    pub code_font: SharedString,
    /// `window.rightToLeft`: text with right-to-left letters is drawn in display order, a character at a time (off:
    /// in logical order, a character at a time; never handed to the shaper either way).
    pub rtl: bool,
    /// The document is laid out right to left (`doc_rtl`): gutters, bars and table columns mirror, text aligns right.
    pub mirror: bool,
    /// The code block whose Copy button shows `Copied`.
    pub copied: Option<usize>,
    /// The measured average width of a body text character in px, for table columns.
    pub char_px: f32,
    /// For an image row, what its file's read made (`None` until it arrives).
    pub image: Option<Content>,
    /// The list's width in px (0 before it is laid out once).
    pub viewport: f32,
    /// The text selection (one block).
    pub selection: Option<BlockSelection>,
}

/// A press or a drag on text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gesture {
    /// The left button went down; `clicks` counts the quick presses in a row.
    Press { shift: bool, clicks: usize },
    /// The pointer moved with the left button held.
    Drag,
}

impl Gesture {
    /// The gesture of a mouse press.
    fn press(event: &MouseDownEvent) -> Self {
        Gesture::Press { shift: event.modifiers.shift, clicks: event.click_count }
    }
}

/// Reports a gesture with the logical byte offset it landed on, in the unit the text stands for.
pub(crate) type Hit = Rc<dyn Fn(Gesture, usize, &mut App)>;

/// How a piece of text takes part in selection.
pub(crate) struct Interact {
    /// Where presses and drags go.
    pub hit: Hit,
    /// The selected bytes of this text, painted.
    pub selected: Option<Range<usize>>,
}

/// The offset of a press or drag on block `block`.
fn block_hit(view: Entity<PreviewView>, block: usize) -> Hit {
    Rc::new(move |gesture, offset, app| view.update(app, |this, cx| this.md_gesture(block, offset, gesture, cx)))
}

/// `hit` for text that starts `base` bytes into the unit.
fn shifted(hit: &Hit, base: usize) -> Hit {
    let hit = hit.clone();
    Rc::new(move |gesture, at, app| hit(gesture, base + at, app))
}

/// The caret index of a position that may fall outside the text.
fn caret(found: Result<usize, usize>) -> usize {
    match found {
        Ok(at) | Err(at) => at,
    }
}

/// How far each visual row of a text line is painted from the element's left edge: `align` and the free space of the
/// row (`box_width` less the row's width). `wraps` are the x positions where the line wraps, `total` its width.
/// GPUI paints this shift but its `index_for_position` ignores it.
pub(crate) fn row_shifts(align: TextAlign, box_width: f32, wraps: &[f32], total: f32) -> Vec<f32> {
    let mut start = 0.0;
    wraps
        .iter()
        .copied()
        .chain([total])
        .map(|end| {
            let free = box_width - (end - start);
            start = end;
            match align {
                _ if total <= 0.0 => 0.0,
                TextAlign::Left => 0.0,
                TextAlign::Center => free / 2.0,
                TextAlign::Right => free,
            }
        })
        .collect()
}

/// The visual row at `y` below a line's top, for rows `line_height` high; above or below stays on the first or last.
pub(crate) fn row_at(y: f32, line_height: f32, rows: usize) -> usize {
    if y <= 0.0 || line_height <= 0.0 { 0 } else { ((y / line_height) as usize).min(rows.saturating_sub(1)) }
}

/// `position` moved to where GPUI's `index_for_position` expects it: left aligned, undoing the shift `align` painted
/// the row under it with.
fn unshifted(layout: &TextLayout, align: TextAlign, mut position: Point<Pixels>) -> Point<Pixels> {
    if align == TextAlign::Left {
        return position;
    }
    let (bounds, line_height) = (layout.bounds(), layout.line_height());
    let mut top = bounds.top();
    for line in layout.line_layouts() {
        let rows = line.wrap_boundaries.len() + 1;
        let height = line_height * rows as f32;
        if position.y <= top + height {
            let wraps: Vec<f32> = line
                .wrap_boundaries
                .iter()
                .map(|b| f32::from(line.unwrapped_layout.runs[b.run_ix].glyphs[b.glyph_ix].position.x))
                .collect();
            let shifts =
                row_shifts(align, f32::from(bounds.size.width), &wraps, f32::from(line.unwrapped_layout.width));
            let row = row_at(f32::from(position.y - top), f32::from(line_height), rows);
            position.x -= px(shifts[row]);
            return position;
        }
        top += height;
    }
    position
}

/// The byte index of the character at `position` in text painted with `align`.
fn index_at(layout: &TextLayout, align: TextAlign, position: Point<Pixels>) -> Result<usize, usize> {
    layout.index_for_position(unshifted(layout, align, position))
}

/// Turn a press or drag on `el` into `hit`, the offset read from `layout` (text painted with `align`) and mapped by
/// `map`.
fn layout_pointer<E: InteractiveElement>(
    el: E,
    hit: &Hit,
    layout: TextLayout,
    align: TextAlign,
    map: impl Fn(usize) -> usize + Clone + 'static,
) -> E {
    let (down, drag, down_layout, drag_layout, down_map) =
        (hit.clone(), hit.clone(), layout.clone(), layout, map.clone());
    el.on_mouse_down(MouseButton::Left, move |event, _, app| {
        down(Gesture::press(event), down_map(caret(index_at(&down_layout, align, event.position))), app)
    })
    .on_mouse_move(move |event: &MouseMoveEvent, _, app| {
        if event.pressed_button == Some(MouseButton::Left) {
            drag(Gesture::Drag, map(caret(index_at(&drag_layout, align, event.position))), app);
        }
    })
}

/// Turn a press or drag on `el` into `hit` at the fixed `offset` (a character drawn on its own).
fn offset_pointer<E: InteractiveElement>(el: E, hit: &Hit, offset: usize) -> E {
    let (down, drag) = (hit.clone(), hit.clone());
    el.on_mouse_down(MouseButton::Left, move |event, _, app| down(Gesture::press(event), offset, app)).on_mouse_move(
        move |event: &MouseMoveEvent, _, app| {
            if event.pressed_button == Some(MouseButton::Left) {
                drag(Gesture::Drag, offset, app);
            }
        },
    )
}

/// The kind of a character for double-click words: word, space or other.
fn char_class(c: char) -> u8 {
    if c.is_alphanumeric() || c == '_' {
        0
    } else if c.is_whitespace() {
        1
    } else {
        2
    }
}

/// The word at byte `at` of `text`: the run of characters of the same kind around it.
pub(crate) fn word_bounds(text: &str, at: usize) -> Range<usize> {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    let Some(pivot) = text[at..].chars().next().or_else(|| text[..at].chars().next_back()) else { return 0..0 };
    let pivot_at = if at < text.len() { at } else { at - pivot.len_utf8() };
    let kind = char_class(pivot);
    let before: usize = text[..pivot_at].chars().rev().take_while(|c| char_class(*c) == kind).map(char::len_utf8).sum();
    let after: usize = text[pivot_at..].chars().take_while(|c| char_class(*c) == kind).map(char::len_utf8).sum();
    pivot_at - before..pivot_at + after
}

/// The selection a press makes: a caret, a word (two clicks), the whole block (three), or, with Shift in the same
/// block, the old selection stretched to the new place.
pub(crate) fn press_selection(
    previous: Option<BlockSelection>,
    block: usize,
    offset: usize,
    shift: bool,
    clicks: usize,
    text: &str,
) -> BlockSelection {
    match clicks {
        0 | 1 => {
            let anchor = previous.filter(|p| shift && p.block == block).map_or(offset, |p| p.anchor);
            BlockSelection { block, anchor, head: offset }
        }
        2 => {
            let word = word_bounds(text, offset);
            BlockSelection { block, anchor: word.start, head: word.end }
        }
        _ => BlockSelection { block, anchor: 0, head: text.len() },
    }
}

/// What a press selects, and so the step a drag extends it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unit {
    /// One click: characters.
    Char,
    /// Two clicks: words.
    Word,
    /// Three clicks: the whole block.
    Block,
}

impl Unit {
    /// The unit of a press that is the `clicks`-th in a row.
    pub(crate) fn of(clicks: usize) -> Self {
        match clicks {
            0 | 1 => Unit::Char,
            2 => Unit::Word,
            _ => Unit::Block,
        }
    }
}

/// The selection a drag to `offset` makes after a press of `unit` that selected `origin` (a caret for characters): by
/// characters it runs from the press to the pointer, by words from the pressed word to the word under the pointer (the
/// pressed word stays selected), by blocks it stays the whole block.
pub(crate) fn drag_selection(
    unit: Unit,
    origin: &Range<usize>,
    block: usize,
    offset: usize,
    text: &str,
) -> BlockSelection {
    let (anchor, head) = match unit {
        Unit::Char => (origin.start, offset),
        Unit::Block => (0, text.len()),
        Unit::Word => {
            let word = word_bounds(text, offset);
            if offset >= origin.end {
                (origin.start, word.end.max(origin.end))
            } else if offset < origin.start {
                (origin.end, word.start.min(origin.start))
            } else {
                (origin.start, origin.end)
            }
        }
    };
    BlockSelection { block, anchor, head }
}

/// An empty strip `width` wide that reports a press or drag on it as `offset` (the margin beside a line of text).
pub(crate) fn pad(width: gpui::Pixels, hit: &Hit, offset: usize) -> Div {
    offset_pointer(div().flex_none().w(width), hit, offset)
}

/// The part of `range` that falls in a line of `len` bytes starting `base` bytes into its block, relative to the line.
fn clip_range(range: &Range<usize>, base: usize, len: usize) -> Option<Range<usize>> {
    let (start, end) = (range.start.max(base), range.end.min(base + len));
    (start < end).then(|| start - base..end - base)
}

/// The logical offset of the character a word of `chars` shows at byte `index` of its text.
fn word_offset(chars: &[(char, usize)], index: usize) -> usize {
    let mut at = 0;
    for (c, offset) in chars {
        at += c.len_utf8();
        if index < at {
            return *offset;
        }
    }
    chars.last().map_or(0, |(_, offset)| *offset)
}

/// How text reaches the screen: the shaper never gets right-to-left letters, whatever the setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// No right-to-left letter: ordinary wrapped styled text.
    Shaped,
    /// `window.rightToLeft` on: words and characters in display order, one element per non-ASCII character.
    Display,
    /// `window.rightToLeft` off: words and characters in logical order, one element per non-ASCII character, as the
    /// terminal draws them.
    Logical,
}

/// The route of a block or line of text: `setting` is `window.rightToLeft`, `has_rtl` whether it holds a
/// right-to-left letter. It is the same for paragraphs, headings, table cells and code lines.
pub(crate) fn text_route(setting: bool, has_rtl: bool) -> Route {
    match (has_rtl, setting) {
        (false, _) => Route::Shaped,
        (true, true) => Route::Display,
        (true, false) => Route::Logical,
    }
}

/// The characters of `text` with their logical byte offsets in the order `route` draws them; `None` for `Shaped`.
fn char_order(text: &str, route: Route) -> Option<Vec<(usize, char)>> {
    let logical = || text.char_indices().collect();
    match route {
        Route::Shaped => None,
        Route::Display => Some(display_line(text).map_or_else(logical, |order| {
            let offsets: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
            order.into_iter().map(|(ix, ch)| (offsets[ix], ch)).collect()
        })),
        Route::Logical => Some(logical()),
    }
}

/// Whether the document is laid out right to left: `window.rightToLeft` is on and the document's own direction
/// (`doc_rtl`, from its first letter) differs from the flip toggle's state.
pub(crate) fn doc_rtl(setting: bool, doc_rtl: bool, flipped: bool) -> bool {
    setting && (doc_rtl != flipped)
}

/// A table's column fractions and alignments in mirrored order: the columns reversed, `Left` and `Right` swapped. The
/// two slices are per column; a table with fewer alignments than columns is mirrored as it is.
pub(crate) fn mirror_table(fractions: &[f32], align: &[Align]) -> (Vec<f32>, Vec<Align>) {
    let swap = |a: &Align| match a {
        Align::Left => Align::Right,
        Align::Right => Align::Left,
        other => *other,
    };
    (fractions.iter().rev().copied().collect(), align.iter().rev().map(swap).collect())
}

/// Body text size in base px.
const BODY: f32 = 16.0;
/// Line height of body text.
const BODY_LINE: f32 = 1.5;
/// Line height of headings.
const HEADING_LINE: f32 = 1.25;
/// The code font size in base px (85% of the body).
const CODE: f32 = 13.6;
/// The widest the text column grows, in base px.
const MAX_WIDTH: f32 = 980.0;
/// The list indent and marker gutter, in base px.
const INDENT: f32 = 32.0;

fn hsla(color: u32) -> Hsla {
    rgb(color).into()
}

/// The measured average width in px of a character of body text, with a 10% margin for bold and wide letters: what a
/// table column needs per character of its longest word.
pub(crate) fn char_px(window: &gpui::Window, theme: &Theme) -> f32 {
    let system = window.text_system();
    let id = system.resolve_font(&gpui::font(UI_FONT));
    let size = theme.ui(BODY);
    let (sum, count) = ('a'..='z').chain('A'..='Z').chain('0'..='9').fold((0.0, 0u32), |(sum, count), ch| match system
        .advance(id, size, ch)
    {
        Ok(advance) => (sum + f32::from(advance.width), count + 1),
        Err(_) => (sum, count),
    });
    if count == 0 { f32::from(size) * 0.55 * 1.1 } else { sum / count as f32 * 1.1 }
}

/// The text size of a heading in base px.
fn heading_size(level: u8) -> f32 {
    match level {
        1 => 32.0,
        2 => 24.0,
        3 => 20.0,
        4 => 16.0,
        5 => 14.0,
        _ => 13.6,
    }
}

/// The color of an alert's bar and title.
fn alert_color(kind: AlertKind, theme: &Theme) -> u32 {
    let index = match kind {
        AlertKind::Note => 4,
        AlertKind::Tip => 2,
        AlertKind::Important => 5,
        AlertKind::Warning => 3,
        AlertKind::Caution => 1,
    };
    ansi_color(index, theme)
}

/// Whether a run is a link that goes somewhere.
fn live_link(doc: &MdDoc, inline: &Inline) -> Option<usize> {
    inline.link.filter(|link| doc.links.get(*link).is_some_and(|target| !matches!(target, LinkTarget::Inert(_))))
}

/// The run of one inline (its length is the caller's).
fn inline_run(inline: &Inline, doc: &MdDoc, base: &TextStyle, code: &Font, theme: &Theme) -> TextRun {
    let style = inline.style;
    let mut font = if style.code { code.clone() } else { base.font() };
    font.weight = if style.bold { FontWeight::BOLD } else { base.font_weight };
    if style.italic {
        font.style = FontStyle::Italic;
    }
    let link = live_link(doc, inline).is_some();
    TextRun {
        len: inline.range.len(),
        font,
        color: if link { hsla(theme.tokens.accent) } else { base.color },
        background_color: style.code.then(|| hsla(theme.tokens.raised)),
        underline: link.then(|| UnderlineStyle {
            thickness: px(1.),
            color: Some(hsla(theme.tokens.accent)),
            wavy: false,
        }),
        strikethrough: style.strike.then(|| StrikethroughStyle { thickness: px(1.), color: None }),
    }
}

/// The background of selected text.
fn selection_background(theme: &Theme) -> Hsla {
    hsla(mix(theme.tokens.accent, theme.tokens.terminal, 0.30))
}

/// Split `runs` at the ends of `range` and give the part inside the selection background.
fn paint_selection(runs: Vec<TextRun>, range: Range<usize>, background: Hsla) -> Vec<TextRun> {
    let mut out = Vec::with_capacity(runs.len() + 2);
    let mut at = 0;
    for run in runs {
        let end = at + run.len;
        let (from, to) = (range.start.max(at), range.end.min(end));
        if from >= to {
            out.push(run);
        } else {
            if from > at {
                out.push(TextRun { len: from - at, ..run.clone() });
            }
            out.push(TextRun { len: to - from, background_color: Some(background), ..run.clone() });
            if to < end {
                out.push(TextRun { len: end - to, ..run });
            }
        }
        at = end;
    }
    out
}

/// The runs of a line of code of `len` bytes: the token colors from `spans`, the rest plain.
pub(crate) fn token_runs(
    len: usize,
    spans: impl IntoIterator<Item = (Range<usize>, Token)>,
    font: &Font,
    theme: &Theme,
) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let mut push = |len: usize, token: Token| {
        if len > 0 {
            runs.push(TextRun {
                len,
                font: font.clone(),
                color: hsla(token_color(token, theme)),
                background_color: None,
                underline: None,
                strikethrough: None,
            });
        }
    };
    let mut at = 0;
    for (range, token) in spans {
        let (start, end) = (range.start.max(at), range.end.min(len));
        if start >= end {
            continue;
        }
        push(start - at, Token::Plain);
        push(end - start, token);
        at = end;
    }
    push(len - at, Token::Plain);
    runs
}

/// The spans of a displayed line: a line with tabs shows them as four spaces, so the raw offsets move.
pub(crate) fn display_spans(raw: &str, spans: &[Span]) -> Vec<(Range<usize>, Token)> {
    let mut map = vec![0; raw.len() + 1];
    let mut shown = 0;
    for (at, c) in raw.char_indices() {
        for slot in &mut map[at..at + c.len_utf8()] {
            *slot = shown;
        }
        shown += if c == '\t' { 4 } else { c.len_utf8() };
    }
    map[raw.len()] = shown;
    spans
        .iter()
        .map(|span| (map[span.range.start.min(raw.len())]..map[span.range.end.min(raw.len())], span.token))
        .collect()
}

/// The token colored at logical byte `at` of a line (`Plain` between spans).
pub(crate) fn token_at(spans: &[(Range<usize>, Token)], at: usize) -> Token {
    spans.iter().find(|(range, _)| range.contains(&at)).map_or(Token::Plain, |(_, token)| *token)
}

/// The byte range of lines `lines` of a code text of `len` bytes whose lines start at `starts`.
fn code_range(len: usize, starts: &[usize], lines: &Range<usize>) -> Range<usize> {
    let start = starts.get(lines.start).copied().unwrap_or(len);
    let end = starts.get(lines.end).map_or(len, |next| next - 1);
    start..end.max(start)
}

/// The runs of a block's text, one per inline (a code block: one per syntax span), with the selection painted.
pub(crate) fn text_runs(
    block: &Block,
    doc: &MdDoc,
    base: &TextStyle,
    code: &Font,
    theme: &Theme,
    selected: Option<Range<usize>>,
) -> Vec<TextRun> {
    let runs = match &block.kind {
        BlockKind::Code { highlights, .. } => {
            let lines = 0..block.text.split('\n').count();
            let mut runs = Vec::new();
            for (n, line) in block.text.split('\n').enumerate() {
                if n > 0 {
                    runs.extend(token_runs(1, [], code, theme));
                }
                let spans = highlights.as_ref().map_or(&[][..], |h| h.line(lines.start + n));
                runs.extend(token_runs(line.len(), spans.iter().map(|s| (s.range.clone(), s.token)), code, theme));
            }
            runs
        }
        _ => block.inlines.iter().map(|inline| inline_run(inline, doc, base, code, theme)).collect(),
    };
    match selected {
        Some(range) if range.start < range.end => paint_selection(runs, range, selection_background(theme)),
        _ => runs,
    }
}

/// How a block of text is set.
#[derive(Clone, Copy)]
struct Look {
    /// Size in base px.
    size: f32,
    /// Line height factor.
    line: f32,
    weight: FontWeight,
    color: u32,
}

/// The base text style of a look.
fn base_style(look: Look, theme: &Theme) -> TextStyle {
    TextStyle {
        color: hsla(look.color),
        font_family: UI_FONT.into(),
        font_size: theme.ui(look.size).into(),
        font_weight: look.weight,
        ..Default::default()
    }
}

/// Open the link `target` (a click on it).
fn link_opener(view: Entity<PreviewView>, target: LinkTarget) -> impl Fn(&mut gpui::App) + 'static {
    // A drag that ends inside the link selected text; it is not a click.
    move |app| {
        view.update(app, |this, cx| {
            if !this.md_selecting() {
                this.open_link(target.clone(), cx);
            }
        })
    }
}

/// A block of text as an element: wrapped styled text, or, for text with a right-to-left letter, words in display order
/// (the display on) or logical order (off), a character at a time; the shaper never gets such text.
fn text_block(
    doc: &MdDoc,
    block_ix: usize,
    look: Look,
    align: TextAlign,
    state: &RowState,
    theme: &Theme,
    cx: &mut Context<PreviewView>,
) -> Div {
    let frame = div()
        .w_full()
        .text_size(theme.ui(look.size))
        .line_height(relative(look.line))
        .font_family(UI_FONT)
        .font_weight(look.weight)
        .text_color(rgb(look.color));
    let frame = match align {
        TextAlign::Left => frame,
        TextAlign::Center => frame.text_center(),
        TextAlign::Right => frame.text_right(),
    };
    let Some(block) = doc.blocks.get(block_ix) else { return frame };
    let base = base_style(look, theme);
    let code = pane_font(Some(&state.code_font));
    let selected = state.selection.filter(|s| s.block == block_ix).map(|s| s.range());
    let hit = block_hit(cx.entity(), block_ix);
    if let Some(rtl) = &block.rtl {
        let words = match text_route(state.rtl, true) {
            Route::Display => &rtl.visual,
            _ => &rtl.logical,
        };
        let interact = Interact { hit, selected };
        return frame.child(rtl_text(doc, block_ix, rtl, words, &base, &code, theme, &interact, state.mirror, cx));
    }
    let runs = text_runs(block, doc, &base, &code, theme, selected);
    let styled = StyledText::new(SharedString::from(block.text.clone())).with_runs(runs);
    let frame = layout_pointer(frame, &hit, styled.layout().clone(), align, |at| at);
    let links: Vec<(Range<usize>, LinkTarget)> = block
        .inlines
        .iter()
        .filter_map(|inline| Some((inline.range.clone(), doc.links[live_link(doc, inline)?].clone())))
        .collect();
    if links.is_empty() {
        return frame.child(styled);
    }
    if align != TextAlign::Left {
        // GPUI's link hit-test ignores the alignment shift, so the click is resolved here.
        let layout = styled.layout().clone();
        let openers: Vec<_> =
            links.into_iter().map(|(range, target)| (range, link_opener(cx.entity(), target))).collect();
        let (hover_layout, hover_ranges): (_, Vec<Range<usize>>) =
            (layout.clone(), openers.iter().map(|(range, _)| range.clone()).collect());
        // A hitbox over the text for the pointer cursor, read after the text laid out.
        let cursor = canvas(
            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
            move |_, hitbox, window, _| {
                if hitbox.is_hovered(window)
                    && let Ok(at) = index_at(&hover_layout, align, window.mouse_position())
                    && hover_ranges.iter().any(|range| range.contains(&at))
                {
                    window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
                }
            },
        )
        .absolute()
        .size_full();
        return frame.child(div().id(("md-text", block_ix)).relative().w_full().child(styled).child(cursor).on_click(
            move |event, _, app| {
                if let Ok(at) = index_at(&layout, align, event.position())
                    && let Some((_, open)) = openers.iter().find(|(range, _)| range.contains(&at))
                {
                    open(app);
                }
            },
        ));
    }
    let ranges = links.iter().map(|(range, _)| range.clone()).collect();
    let openers: Vec<_> = links.into_iter().map(|(_, target)| link_opener(cx.entity(), target)).collect();
    frame.child(InteractiveText::new(("md-text", block_ix), styled).on_click(ranges, move |which, _, app| {
        if let Some(open) = openers.get(which) {
            open(app);
        }
    }))
}

/// The `words` of a paragraph with a right-to-left letter in a wrapping row (reversed when the paragraph reads right to
/// left). Each word with a non-ASCII letter, or a link, is drawn a character at a time: the shaper never gets the text.
#[allow(clippy::too_many_arguments)] // The block's whole context.
fn rtl_text(
    doc: &MdDoc,
    block_ix: usize,
    rtl: &RtlText,
    words: &VisualText,
    base: &TextStyle,
    code: &Font,
    theme: &Theme,
    interact: &Interact,
    mirror: bool,
    cx: &mut Context<PreviewView>,
) -> Div {
    let Some(block) = doc.blocks.get(block_ix) else { return div() };
    let background = selection_background(theme);
    let inline_runs: Vec<(TextRun, Option<usize>)> = block
        .inlines
        .iter()
        .map(|inline| (inline_run(inline, doc, base, code, theme), live_link(doc, inline)))
        .collect();
    let run_at = |offset: usize, len: usize| -> (TextRun, Option<usize>) {
        let (mut run, link) = match rtl.inline_of(offset).and_then(|ix| inline_runs.get(ix)) {
            Some((run, link)) => (run.clone(), *link),
            None => (TextRun { font: base.font(), color: base.color, ..Default::default() }, None),
        };
        run.len = len;
        if interact.selected.as_ref().is_some_and(|range| range.contains(&offset)) {
            run.background_color = Some(background);
        }
        (run, link)
    };
    let mut flow = div().w_full().flex().flex_wrap();
    if words.rtl {
        flow = flow.flex_row_reverse();
    }
    if packs_end(words.rtl, mirror) {
        flow = flow.justify_end();
    }
    let mut counter = 0;
    for word in &words.words {
        if word.chars.first().is_some_and(|(c, _)| *c == '\n') {
            flow = flow.child(div().w_full().h_0());
            continue;
        }
        let linked = word.chars.iter().any(|(_, offset)| run_at(*offset, 1).1.is_some());
        flow = flow.child(rtl_word(doc, word, linked, &run_at, (block_ix, &mut counter), &interact.hit, cx));
    }
    flow
}

/// Whether a wrapping row of words needs `justify_end` to sit on the side the document reads from: a reversed row packs
/// right by itself and a plain one left, while a mirrored document wants the right and a flipped one the left (next to
/// its list bullets).
fn packs_end(reversed: bool, mirror: bool) -> bool {
    reversed != mirror
}

/// One word of `rtl_text`.
fn rtl_word(
    doc: &MdDoc,
    word: &Word,
    per_char: bool,
    run_at: &dyn Fn(usize, usize) -> (TextRun, Option<usize>),
    (block_ix, counter): (usize, &mut usize),
    hit: &Hit,
    cx: &mut Context<PreviewView>,
) -> Div {
    let el = div().flex().flex_none();
    if !word.split && !per_char {
        let text: String = word.chars.iter().map(|(c, _)| *c).collect();
        let mut runs: Vec<TextRun> = Vec::new();
        for (c, offset) in &word.chars {
            let (run, _) = run_at(*offset, c.len_utf8());
            match runs.last_mut() {
                Some(last) if same_look(last, &run) => last.len += run.len,
                _ => runs.push(run),
            }
        }
        let styled = StyledText::new(SharedString::from(text)).with_runs(runs);
        let chars: Rc<[(char, usize)]> = word.chars.clone().into();
        let el = layout_pointer(el, hit, styled.layout().clone(), TextAlign::Left, move |at| word_offset(&chars, at));
        return el.child(styled);
    }
    el.children(word.chars.iter().map(|(c, offset)| {
        let (run, link) = run_at(*offset, c.len_utf8());
        let text = StyledText::new(SharedString::from(c.to_string())).with_runs(vec![run]);
        let cell = offset_pointer(div().child(text), hit, *offset);
        match link.and_then(|link| doc.links.get(link)) {
            Some(target) => {
                *counter += 1;
                let open = link_opener(cx.entity(), target.clone());
                cell.id(("md-link", (block_ix << 20) | *counter))
                    .cursor_pointer()
                    .on_click(move |_, _, app| open(app))
                    .into_any_element()
            }
            None => cell.into_any_element(),
        }
    }))
}

/// Whether two runs look the same (only their lengths differ).
fn same_look(a: &TextRun, b: &TextRun) -> bool {
    TextRun { len: 0, ..a.clone() } == TextRun { len: 0, ..b.clone() }
}

/// A line of code as an element: wrapped styled text with its token colors, or, for a line with right-to-left letters,
/// characters one element each (display order with the display on, logical order off) colored by their logical
/// offsets. `interact` gets presses and drags (as byte offsets of `line`) and says what to paint selected.
pub(crate) fn code_line(
    line: &str,
    spans: &[(Range<usize>, Token)],
    font: &Font,
    rtl: bool,
    wrap: bool,
    theme: &Theme,
    interact: &Interact,
) -> AnyElement {
    let background = selection_background(theme);
    if let Some(order) = char_order(line, text_route(rtl, line.chars().any(is_rtl_letter))) {
        let chars = div().flex().min_w_0().when(wrap, |d| d.flex_wrap()).children(order.into_iter().map(|(at, ch)| {
            let color = token_color(token_at(spans, at), theme);
            let shown = interact.selected.as_ref().is_some_and(|range| range.contains(&at));
            let cell = div().text_color(rgb(color)).when(shown, |d| d.bg(background)).child(ch.to_string());
            offset_pointer(cell, &interact.hit, at)
        }));
        // The space after the characters is a target of its own (a sibling, so it never overrides a character).
        let rest = offset_pointer(div().flex_1().min_w(px(4.)), &interact.hit, line.len());
        return div().w_full().flex().child(chars).child(rest).into_any_element();
    }
    let mut runs = token_runs(line.len(), spans.iter().cloned(), font, theme);
    if let Some(range) = interact.selected.clone() {
        runs = paint_selection(runs, range, background);
    }
    let styled = StyledText::new(SharedString::from(line.to_string())).with_runs(runs);
    let layout = styled.layout().clone();
    layout_pointer(div().w_full().child(styled), &interact.hit, layout, TextAlign::Left, |at| at).into_any_element()
}

/// The list item at `ix` of the document's list.
pub(crate) fn row(
    doc: &MdDoc,
    ix: usize,
    state: &RowState,
    theme: &Theme,
    cx: &mut Context<PreviewView>,
) -> AnyElement {
    let Some(row) = doc.rows.get(ix) else { return div().into_any_element() };
    let quoted = row.quote > 0 && row.alert.is_none();
    let color = if quoted { theme.tokens.muted } else { theme.tokens.text };
    let body = Look { size: BODY, line: BODY_LINE, weight: FontWeight::NORMAL, color };
    let content: AnyElement = match &row.kind {
        RowKind::Text { block } => text_row(doc, *block, body, state, theme, cx).into_any_element(),
        RowKind::Item { block, marker } => div()
            .w_full()
            .flex()
            .when(state.mirror, |d| d.flex_row_reverse())
            .items_start()
            .child(marker_gutter(*marker, color, state.mirror, theme))
            .child(div().flex_1().min_w_0().child(text_block(
                doc,
                *block,
                body,
                text_align(state.mirror),
                state,
                theme,
                cx,
            )))
            .into_any_element(),
        RowKind::Code { block, lines } => code_row(doc, ix, *block, lines, state, theme, cx),
        RowKind::TableRow { table, cells, header } => table_row(doc, ix, *table, cells, *header, state, theme, cx),
        RowKind::Image { image } => image_row(doc, ix, *image, state, theme, cx),
        RowKind::Rule => div().w_full().h(theme.ui(4.)).bg(rgb(theme.tokens.line)).into_any_element(),
        RowKind::AlertTitle(kind) => div()
            .when(state.mirror, |d| d.text_right())
            .text_size(theme.ui(BODY))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(alert_color(*kind, theme)))
            .child(kind.title())
            .into_any_element(),
    };
    framed(content, doc, ix, state.mirror, theme)
}

/// The size an image is drawn at: its natural size, shrunk (aspect kept) to fit `width`, never enlarged.
pub(crate) fn image_box(natural: (f32, f32), width: f32) -> (f32, f32) {
    if natural.0 <= width || natural.0 <= 0.0 {
        return natural;
    }
    (width.max(0.0), natural.1 * width.max(0.0) / natural.0)
}

/// The width in px left for the content of row `ix` in a list `viewport` px wide: the text column's width less its
/// padding, the list indent and the quote bars; unbounded before the list was laid out.
fn room(doc: &MdDoc, ix: usize, viewport: f32, theme: &Theme) -> f32 {
    let Some(row) = doc.rows.get(ix).filter(|_| viewport > 0.0) else { return f32::MAX };
    let ui = |px: f32| f32::from(theme.ui(px));
    viewport.min(ui(MAX_WIDTH)) - ui(32.) - ui(INDENT * f32::from(row.indent)) - ui(20.) * f32::from(row.quote)
}

/// One line of muted or linked text for an image row. Right-to-left text is never handed to the shaper as one run.
fn image_label(text: &str, color: u32, underline: bool, state: &RowState) -> AnyElement {
    let line = div().text_color(rgb(color)).when(underline, |d| d.underline());
    match char_order(text, text_route(state.rtl, text.chars().any(is_rtl_letter))) {
        Some(order) => {
            line.flex().children(order.into_iter().map(|(_, ch)| div().child(ch.to_string()))).into_any_element()
        }
        None => line.whitespace_nowrap().overflow_hidden().child(text.to_string()).into_any_element(),
    }
}

/// An image row: the picture at its natural size shrunk to the row, or a line saying why there is none.
fn image_row(
    doc: &MdDoc,
    ix: usize,
    image: usize,
    state: &RowState,
    theme: &Theme,
    cx: &mut Context<PreviewView>,
) -> AnyElement {
    let Some(item) = doc.images.get(image) else { return div().into_any_element() };
    let muted = theme.tokens.muted;
    let alt = if item.alt.is_empty() { "Image" } else { item.alt.as_str() };
    let alt_line = || image_label(&format!("[{alt}]"), muted, false, state);
    let note = |text: &str| image_label(text, muted, false, state);
    let live = item.link.and_then(|link| doc.links.get(link)).filter(|target| !matches!(target, LinkTarget::Inert(_)));
    let mut open = live.cloned();
    let body = match &item.src {
        ImageSrc::File(path) if !matches!(kind_of(path), FileKind::Image(_)) => note("Not an image"),
        ImageSrc::File(_) => match &state.image {
            Some(Content::Image { .. }) if room(doc, ix, state.viewport, theme) <= 0.0 => alt_line(),
            Some(Content::Image { image, natural }) => {
                let (width, height) = image_box(*natural, room(doc, ix, state.viewport, theme));
                gpui::img(image.clone())
                    .w(px(width))
                    .h(px(height))
                    .object_fit(gpui::ObjectFit::Contain)
                    .into_any_element()
            }
            Some(Content::Missing) => note("Image not found"),
            Some(Content::TooLarge(_)) => note("Too large to preview"),
            Some(Content::Failed(reason)) => note(reason),
            _ => alt_line(),
        },
        ImageSrc::Web(url) => {
            // A surrounding link wins over the image's own address.
            open = open.or_else(|| Some(LinkTarget::Web(url.clone())));
            let text = if item.alt.is_empty() { url.as_str() } else { alt };
            image_label(text, theme.tokens.accent, true, state)
        }
        ImageSrc::Bad => alt_line(),
    };
    let el = div().w_full().text_size(theme.ui(BODY)).when(state.mirror, |d| d.flex().justify_end()).child(body);
    match open {
        Some(target) => {
            let opener = link_opener(cx.entity(), target);
            el.id(("md-image", ix)).cursor_pointer().on_click(move |_, _, app| opener(app)).into_any_element()
        }
        None => el.into_any_element(),
    }
}

/// A paragraph, heading or footnote text; headings are sized by level.
fn text_row(
    doc: &MdDoc,
    block: usize,
    body: Look,
    state: &RowState,
    theme: &Theme,
    cx: &mut Context<PreviewView>,
) -> Div {
    let Some(BlockKind::Heading { level }) = doc.blocks.get(block).map(|b| &b.kind) else {
        return text_block(doc, block, body, text_align(state.mirror), state, theme, cx);
    };
    let size = heading_size(*level);
    let color = if *level == 6 { theme.tokens.muted } else { body.color };
    let look = Look { size, line: HEADING_LINE, weight: FontWeight::SEMIBOLD, color };
    let text = text_block(doc, block, look, text_align(state.mirror), state, theme, cx);
    if *level <= 2 { text.pb(theme.ui(size * 0.3)).border_b_1().border_color(rgb(theme.tokens.line)) } else { text }
}

/// The alignment of body text: right in a mirrored document.
fn text_align(mirror: bool) -> TextAlign {
    if mirror { TextAlign::Right } else { TextAlign::Left }
}

/// The marker column of a list item.
fn marker_gutter(marker: Marker, color: u32, mirror: bool, theme: &Theme) -> Div {
    let gutter = div()
        .flex_none()
        .w(theme.ui(INDENT))
        .h(theme.ui(BODY * BODY_LINE))
        .when(mirror, |g| g.pl(theme.ui(8.)).justify_start())
        .when(!mirror, |g| g.pr(theme.ui(8.)).justify_end())
        .flex()
        .items_center()
        .text_size(theme.ui(BODY))
        .text_color(rgb(color));
    match marker {
        Marker::Bullet => gutter.child("•"),
        Marker::Number(n) => gutter.child(format!("{n}.")),
        Marker::Task(done) => gutter.child(
            div()
                .size(theme.ui(14.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(RADIUS_TINY))
                .border_1()
                .border_color(rgb(if done { theme.tokens.accent } else { theme.tokens.line }))
                .when(done, |box_| box_.bg(rgb(theme.tokens.accent)).text_color(rgb(theme.tokens.on_accent)))
                .text_size(theme.ui(10.))
                .child(if done { "✓" } else { "" }),
        ),
    }
}

/// Lines `lines` of a code block on the raised surface, with the Copy button on the block's first row.
fn code_row(
    doc: &MdDoc,
    ix: usize,
    block_ix: usize,
    lines: &Range<usize>,
    state: &RowState,
    theme: &Theme,
    cx: &mut Context<PreviewView>,
) -> AnyElement {
    let Some(block) = doc.blocks.get(block_ix) else { return div().into_any_element() };
    let first = lines.start == 0;
    let last = !matches!(
        doc.rows.get(ix + 1).map(|r| &r.kind),
        Some(RowKind::Code { block, .. }) if *block == block_ix
    );
    let font = pane_font(Some(&state.code_font));
    let radius = px(RADIUS_SMALL);
    let starts = &block.line_starts;
    let range = code_range(block.text.len(), starts, lines);
    let text = &block.text[range.clone()];
    let line_height = theme.ui(CODE * 1.45);
    let highlights = match &block.kind {
        BlockKind::Code { highlights, .. } => highlights.as_ref(),
        _ => None,
    };
    let selection = state.selection.filter(|s| s.block == block_ix).map(|s| s.range());
    let hit = block_hit(cx.entity(), block_ix);
    let body = text.split('\n').enumerate().map(|(n, line)| {
        let spans: Vec<(Range<usize>, Token)> = highlights
            .map(|h| h.line(lines.start + n).iter().map(|s| (s.range.clone(), s.token)).collect())
            .unwrap_or_default();
        let base = starts.get(lines.start + n).copied().unwrap_or(range.start);
        let interact = Interact {
            hit: shifted(&hit, base),
            selected: selection.as_ref().and_then(|range| clip_range(range, base, line.len())),
        };
        div().w_full().min_h(line_height).child(code_line(line, &spans, &font, state.rtl, true, theme, &interact))
    });
    let copied = state.copied == Some(block_ix);
    div()
        .relative()
        .w_full()
        .px(theme.ui(16.))
        .when(first, |d| d.pt(theme.ui(16.)).rounded_t(radius))
        .when(last, |d| d.pb(theme.ui(16.)).rounded_b(radius))
        .bg(rgb(theme.tokens.raised))
        .text_size(theme.ui(CODE))
        .line_height(line_height)
        .font_family(state.code_font.clone())
        .text_color(rgb(theme.tokens.text))
        .children(body)
        .when(first, |d| {
            d.child(
                div()
                    .id(("md-copy", block_ix))
                    .absolute()
                    .top(theme.ui(8.))
                    .right(theme.ui(8.))
                    .px(theme.ui(8.))
                    .py(theme.ui(3.))
                    .flex()
                    .items_center()
                    .gap(theme.ui(4.))
                    .rounded(px(RADIUS_SMALL))
                    .bg(rgb(theme.tokens.surface))
                    .border_1()
                    .border_color(rgb(theme.tokens.line))
                    .font_family(UI_FONT)
                    .text_size(theme.ui(12.))
                    .text_color(rgb(theme.tokens.muted))
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgb(theme.tokens.text)))
                    .on_click(cx.listener(move |this, _, _, cx| this.copy_block(block_ix, cx)))
                    .child(svg().path(COPY_ICON).size(theme.ui(12.)).text_color(rgb(theme.tokens.muted)))
                    .child(if copied { "Copied" } else { "Copy" }),
            )
        })
        .into_any_element()
}

/// One table row: cells side by side, `fractions` of the width each. A row with fewer cells than the table has
/// columns gets empty cells.
#[allow(clippy::too_many_arguments)] // The row's whole context.
fn table_row(
    doc: &MdDoc,
    ix: usize,
    table_ix: usize,
    cells: &[usize],
    header: bool,
    state: &RowState,
    theme: &Theme,
    cx: &mut Context<PreviewView>,
) -> AnyElement {
    let Some(table) = doc.tables.get(table_ix) else { return div().into_any_element() };
    let columns = table.widest.len().max(cells.len()).max(1);
    let first = !matches!(
        ix.checked_sub(1).and_then(|p| doc.rows.get(p)).map(|r| &r.kind),
        Some(RowKind::TableRow { table, .. }) if *table == table_ix
    );
    let weight = if header { FontWeight::SEMIBOLD } else { FontWeight::NORMAL };
    let look = Look { size: BODY, line: BODY_LINE, weight, color: theme.tokens.text };
    // Mirrored: the columns show in reverse order, each with its own cell.
    let width = room(doc, ix, state.viewport, theme);
    let pad = f32::from(theme.ui(26.)) + 1.0;
    let fractions =
        column_fractions(&table.widest, &table.longest, if width > 1e6 { 0.0 } else { width }, state.char_px, pad);
    let (fractions, align) =
        if state.mirror { mirror_table(&fractions, &table.align) } else { (fractions, table.align.clone()) };
    div()
        .w_full()
        .flex()
        .when(header, |row| row.bg(rgb(theme.tokens.raised)))
        .when(first, |row| row.border_t_1().border_color(rgb(theme.tokens.line)))
        .children((0..columns).map(|col| {
            let fraction = fractions.get(col).copied().unwrap_or(1.0 / columns as f32);
            let cell = div()
                .flex_none()
                .w(relative(fraction))
                .min_w_0()
                .px(theme.ui(13.))
                .py(theme.ui(6.))
                .border_b_1()
                .border_r_1()
                .when(col == 0, |cell| cell.border_l_1())
                .border_color(rgb(theme.tokens.line));
            let text_align = match align.get(col) {
                Some(Align::Center) => TextAlign::Center,
                Some(Align::Right) => TextAlign::Right,
                _ => TextAlign::Left,
            };
            let source = if state.mirror { Some(columns - 1 - col) } else { Some(col) };
            match source.and_then(|source| cells.get(source)) {
                Some(block) => cell.child(text_block(doc, *block, look, text_align, state, theme, cx)),
                None => cell,
            }
        }))
        .into_any_element()
}

/// The row's content with its quote bars, list indent and the space above it. A bar that the row above shares runs
/// through the gap.
fn framed(content: AnyElement, doc: &MdDoc, ix: usize, mirror: bool, theme: &Theme) -> AnyElement {
    let row = &doc.rows[ix];
    let previous = ix.checked_sub(1).and_then(|p| doc.rows.get(p));
    let shared = previous.map_or(0, |p| p.quote.min(row.quote));
    let gap = theme.ui(row.space_above);
    let bar = row.alert.map_or(theme.tokens.line, |kind| alert_color(kind, theme));
    let indent = theme.ui(INDENT * f32::from(row.indent));
    let mut el = div().w_full().when(mirror, |d| d.pr(indent)).when(!mirror, |d| d.pl(indent)).child(content);
    for level in (1..=row.quote).rev() {
        el = div()
            .w_full()
            .border_color(rgb(bar))
            .when(mirror, |d| d.border_r(theme.ui(4.)).pr(theme.ui(16.)))
            .when(!mirror, |d| d.border_l(theme.ui(4.)).pl(theme.ui(16.)))
            .when(level == shared, |d| d.pt(gap))
            .child(el);
    }
    div()
        .w_full()
        .flex()
        .justify_center()
        .when(ix == 0, |d| d.pt(theme.ui(16.)))
        .when(ix + 1 == doc.rows.len(), |d| d.pb(theme.ui(16.)))
        .child(div().w_full().max_w(theme.ui(MAX_WIDTH)).px(theme.ui(16.)).when(shared == 0, |d| d.pt(gap)).child(el))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_core::settings::{ProjectColors, StatusFills};
    use app_core::theme_colors::{Preset, tokens};
    use gpui::{FontStyle, FontWeight, font, rgb};
    use preview_doc::markdown::{BlockKind, parse};
    use std::path::Path;

    fn theme() -> Theme {
        Theme {
            preset: Preset::Mist,
            tokens: tokens(Preset::Mist),
            fills: StatusFills::default(),
            selection: None,
            ui_scale: 1.0,
            project_colors: ProjectColors::default(),
        }
    }

    fn base(theme: &Theme) -> TextStyle {
        TextStyle { color: rgb(theme.tokens.text).into(), font_family: "Figtree".into(), ..Default::default() }
    }

    fn runs_of(text: &str, selected: Option<Range<usize>>) -> (Vec<TextRun>, usize) {
        let theme = theme();
        let doc = parse(text, Path::new("/"));
        let block = &doc.blocks[0];
        (text_runs(block, &doc, &base(&theme), &font("Mono"), &theme, selected), block.text.len())
    }

    fn starts(runs: &[TextRun]) -> Vec<usize> {
        runs.iter()
            .scan(0, |at, run| {
                let start = *at;
                *at += run.len;
                Some(start)
            })
            .collect()
    }

    #[test]
    fn runs_cover_the_block_and_style_inlines() {
        let theme = theme();
        let (runs, len) = runs_of("a **b** `c` [d](https://x)", None);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), len);
        // "a " | b | " " | c | " " | d
        assert_eq!(runs.len(), 6);
        assert_eq!(runs[1].font.weight, FontWeight::BOLD);
        assert_eq!(runs[3].font.family, "Mono");
        assert_eq!(runs[3].background_color, Some(rgb(theme.tokens.raised).into()));
        assert_eq!(runs[5].color, rgb(theme.tokens.accent).into());
        assert!(runs[5].underline.is_some());
        assert!(runs[0].underline.is_none());
        let (italic, _) = runs_of("*i* ~~s~~", None);
        assert_eq!(italic[0].font.style, FontStyle::Italic);
        assert!(italic[2].strikethrough.is_some());
    }

    #[test]
    fn inert_links_look_like_text() {
        let theme = theme();
        let (runs, len) = runs_of("[x](javascript:y)", None);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), len);
        assert_eq!(runs[0].color, rgb(theme.tokens.text).into());
        assert!(runs[0].underline.is_none());
    }

    #[test]
    fn code_runs_follow_highlights() {
        let theme = theme();
        let doc = parse("```rust\nfn main() {}\n```", Path::new("/"));
        let block = &doc.blocks[0];
        assert!(matches!(block.kind, BlockKind::Code { highlights: Some(_), .. }));
        let runs = text_runs(block, &doc, &base(&theme), &font("Mono"), &theme, None);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), block.text.len());
        assert_eq!(runs[0].len, 2, "fn");
        assert_eq!(runs[0].color, rgb(crate::theme::token_color(Token::Keyword, &theme)).into());
        assert_eq!(runs[0].font.family, "Mono");
    }

    #[test]
    fn selection_paints_its_range() {
        let theme = theme();
        let (runs, len) = runs_of("abcdef", Some(2..4));
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), len);
        assert_eq!(starts(&runs), [0, 2, 4]);
        let selection: gpui::Hsla =
            rgb(app_core::theme_colors::mix(theme.tokens.accent, theme.tokens.terminal, 0.30)).into();
        assert_eq!(runs[1].background_color, Some(selection));
        assert_eq!(runs[0].background_color, None);
        assert_eq!(runs[2].background_color, None);
    }

    #[test]
    fn token_runs_fill_the_gaps_and_clip_to_the_line() {
        let theme = theme();
        let runs = token_runs(10, [(2..4, Token::Keyword), (6..99, Token::String)], &font("Mono"), &theme);
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), [2, 2, 2, 4]);
        assert_eq!(runs[0].color, rgb(theme.tokens.text).into());
        assert_eq!(runs[3].color, rgb(crate::theme::token_color(Token::String, &theme)).into());
        assert!(token_runs(0, [], &font("Mono"), &theme).is_empty());
    }

    #[test]
    fn tabs_move_the_spans_of_a_displayed_line() {
        let spans = [Span { range: 1..3, token: Token::Keyword }, Span { range: 4..5, token: Token::String }];
        let shown = display_spans("\tfn x", &spans);
        assert_eq!(shown, [(4..6, Token::Keyword), (7..8, Token::String)]);
        assert_eq!(token_at(&shown, 5), Token::Keyword);
        assert_eq!(token_at(&shown, 6), Token::Plain);
    }

    #[test]
    fn direction_follows_setting_document_and_flip() {
        assert!(!doc_rtl(false, true, false));
        assert!(doc_rtl(true, true, false));
        assert!(!doc_rtl(true, true, true));
        assert!(doc_rtl(true, false, true));
        assert!(!doc_rtl(false, false, true));
    }

    #[test]
    fn mirrored_tables_reverse_columns_and_swap_alignment() {
        let (fractions, align) = mirror_table(&[0.2, 0.8], &[Align::Left, Align::Center]);
        assert_eq!(fractions, [0.8, 0.2]);
        assert_eq!(align, [Align::Center, Align::Right]);
        let (_, align) = mirror_table(&[0.5, 0.5], &[Align::Right, Align::Left]);
        assert_eq!(align, [Align::Right, Align::Left]);
    }

    #[test]
    fn aligned_rows_shift_by_the_free_space_of_each_row() {
        use gpui::TextAlign::{Center, Left, Right};
        // One row of 60 in a box of 100.
        assert_eq!(row_shifts(Left, 100.0, &[], 60.0), [0.0]);
        assert_eq!(row_shifts(Right, 100.0, &[], 60.0), [40.0]);
        assert_eq!(row_shifts(Center, 100.0, &[], 60.0), [20.0]);
        // Wrapped at x = 90 and x = 150 of a 200 wide line: rows of 90, 60 and 50.
        assert_eq!(row_shifts(Right, 100.0, &[90.0, 150.0], 200.0), [10.0, 40.0, 50.0]);
        assert_eq!(row_shifts(Center, 100.0, &[90.0, 150.0], 200.0), [5.0, 20.0, 25.0]);
        // An empty line is never shifted.
        assert_eq!(row_shifts(Right, 100.0, &[], 0.0), [0.0]);
    }

    #[test]
    fn the_row_under_a_pointer_is_found_by_height() {
        assert_eq!(row_at(0.0, 20.0, 3), 0);
        assert_eq!(row_at(39.9, 20.0, 3), 1);
        assert_eq!(row_at(41.0, 20.0, 3), 2);
        assert_eq!(row_at(500.0, 20.0, 3), 2, "below the last row stays on it");
        assert_eq!(row_at(-5.0, 20.0, 3), 0);
    }

    #[test]
    fn code_rows_cut_the_text_by_lines() {
        let text = "a\nbb\nccc\ndddd";
        let starts = [0, 2, 5, 9];
        let cut = |lines: Range<usize>| &text[code_range(text.len(), &starts, &lines)];
        assert_eq!(cut(1..3), "bb\nccc");
        assert_eq!(cut(0..1), "a");
        assert_eq!(cut(3..9), "dddd");
        assert!(cut(9..12).is_empty());
    }

    #[test]
    fn a_press_picks_a_caret_a_word_or_the_block() {
        let text = "hello big world";
        let at = |prev, offset, shift, clicks| press_selection(prev, 2, offset, shift, clicks, text);
        assert_eq!(at(None, 7, false, 1), BlockSelection { block: 2, anchor: 7, head: 7 });
        assert_eq!(at(None, 7, false, 2), BlockSelection { block: 2, anchor: 6, head: 9 });
        assert_eq!(at(None, 7, false, 3), BlockSelection { block: 2, anchor: 0, head: 15 });
        let before = Some(BlockSelection { block: 2, anchor: 1, head: 3 });
        assert_eq!(at(before, 9, true, 1), BlockSelection { block: 2, anchor: 1, head: 9 });
        // Shift on another block starts over.
        let other = Some(BlockSelection { block: 5, anchor: 1, head: 3 });
        assert_eq!(at(other, 9, true, 1), BlockSelection { block: 2, anchor: 9, head: 9 });
    }

    #[test]
    fn a_drag_extends_a_press_by_its_unit() {
        let text = "hello big world";
        let word = 6..9;
        let drag = |unit, origin: Range<usize>, offset| {
            let s = drag_selection(unit, &origin, 0, offset, text);
            (s.anchor, s.head)
        };
        // Jitter inside the pressed word keeps it.
        assert_eq!(drag(Unit::Word, word.clone(), 7), (6, 9));
        assert_eq!(drag(Unit::Word, word.clone(), 6), (6, 9));
        // Onto the next word, by words; back before the word, from its end.
        assert_eq!(drag(Unit::Word, word.clone(), 12), (6, 15));
        assert_eq!(drag(Unit::Word, word.clone(), 2), (9, 0));
        // A block stays whole; characters follow the pointer.
        assert_eq!(drag(Unit::Block, 0..15, 4), (0, 15));
        assert_eq!(drag(Unit::Char, 3..3, 8), (3, 8));
        assert_eq!(Unit::of(1), Unit::Char);
        assert_eq!(Unit::of(2), Unit::Word);
        assert_eq!(Unit::of(5), Unit::Block);
    }

    #[test]
    fn words_are_runs_of_one_kind_of_character() {
        assert_eq!(word_bounds("ab  cd", 1), 0..2);
        assert_eq!(word_bounds("ab  cd", 3), 2..4);
        assert_eq!(word_bounds("ab, cd", 2), 2..3);
        assert_eq!(word_bounds("שלום עולם", 3), 0..8);
        assert_eq!(word_bounds("ab", 2), 0..2);
        assert_eq!(word_bounds("", 0), 0..0);
    }

    #[test]
    fn a_range_is_cut_to_a_line_of_a_block() {
        assert_eq!(clip_range(&(2..12), 5, 4), Some(0..4));
        assert_eq!(clip_range(&(6..8), 5, 4), Some(1..3));
        assert_eq!(clip_range(&(0..5), 5, 4), None);
        assert_eq!(clip_range(&(9..12), 5, 4), None);
    }

    #[test]
    fn a_displayed_byte_of_a_word_maps_to_its_character() {
        let chars = [('ש', 10), ('l', 12), ('x', 13)];
        assert_eq!(word_offset(&chars, 0), 10);
        assert_eq!(word_offset(&chars, 1), 10);
        assert_eq!(word_offset(&chars, 2), 12);
        assert_eq!(word_offset(&chars, 3), 13);
        assert_eq!(word_offset(&chars, 99), 13);
    }

    #[test]
    fn right_to_left_text_never_reaches_the_shaper() {
        // setting x has-RTL-letter; the same rule serves paragraphs, headings, table cells and code lines.
        assert_eq!(text_route(true, false), Route::Shaped);
        assert_eq!(text_route(false, false), Route::Shaped);
        assert_eq!(text_route(true, true), Route::Display);
        assert_eq!(text_route(false, true), Route::Logical);
    }

    #[test]
    fn every_block_kind_with_a_hebrew_letter_is_routed_per_character() {
        let source = "# שלום\n\nשלום עולם\n\n| ת |\n|--|\n| ש |\n\n```\nשלום\n```\n\nplain";
        let doc = parse(source, Path::new("/"));
        for setting in [true, false] {
            let routes: Vec<Route> = doc
                .blocks
                .iter()
                .map(|b| text_route(setting, b.rtl.is_some() || b.text.chars().any(is_rtl_letter)))
                .collect();
            let want = if setting { Route::Display } else { Route::Logical };
            // heading, paragraph, two cells, code line: never Shaped; the plain paragraph stays Shaped.
            assert_eq!(routes, [want, want, want, want, want, Route::Shaped], "setting {setting}");
        }
    }

    #[test]
    fn characters_are_ordered_by_the_route() {
        assert_eq!(char_order("abc", Route::Shaped), None);
        let logical = char_order("אב c", Route::Logical).unwrap();
        assert_eq!(logical, [(0, 'א'), (2, 'ב'), (4, ' '), (5, 'c')]);
        let display = char_order("אב c", Route::Display).unwrap();
        assert_eq!(display.len(), 4);
        // The display order differs from the logical one for a right-to-left base line.
        assert_ne!(display.iter().map(|(at, _)| *at).collect::<Vec<_>>(), [0, 2, 4, 5]);
    }

    #[test]
    fn rows_of_words_pack_toward_the_side_the_document_reads_from() {
        assert!(!packs_end(false, false), "left to right text in a left to right document");
        assert!(!packs_end(true, true), "right to left text in a mirrored document");
        assert!(packs_end(false, true), "English in a mirrored document packs right");
        assert!(packs_end(true, false), "Hebrew in a flipped document packs left, by the bullet");
    }

    #[test]
    fn headings_shrink_by_level() {
        let sizes: Vec<f32> = (1..=6).map(heading_size).collect();
        assert_eq!(sizes, [32.0, 24.0, 20.0, 16.0, 14.0, 13.6]);
    }

    #[test]
    fn image_fits_the_row_and_never_grows() {
        assert_eq!(image_box((2000., 1000.), 500.), (500., 250.));
        assert_eq!(image_box((100., 50.), 500.), (100., 50.));
        assert_eq!(image_box((100., 50.), 0.), (0., 0.));
    }
}
