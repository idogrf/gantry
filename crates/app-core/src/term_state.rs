//! Terminal emulator state for one pane, wrapping `alacritty_terminal`.

use std::sync::{Arc, Mutex};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{ClipboardType, Config, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb as VteRgb};

use crate::palette::{DARK, Palette};
use crate::snapshot::{CellStyle, CellView, CursorView, Rgb, ScreenSnapshot};

/// How far a Shift+arrow moves the moving end of a prompt selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// One character (Shift+arrow).
    Char,
    /// To a word's edge (Shift+Option+arrow).
    Word,
    /// To the line's start or end (Shift+Cmd+arrow).
    Line,
}

/// Keyboard-relevant terminal modes set by the running program.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyModes {
    /// DECCKM: arrow keys send `ESC O x` instead of `ESC [ x`.
    pub app_cursor: bool,
    /// Kitty keyboard protocol "disambiguate escape codes" flag.
    pub kitty_disambiguate: bool,
}

/// Mouse-wheel-relevant terminal modes set by the running program.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollModes {
    /// Alternate screen is active.
    pub alt_screen: bool,
    /// Wheel should become arrow keys on the alternate screen.
    pub alternate_scroll: bool,
    /// The program wants mouse events.
    pub mouse_report: bool,
    /// Mouse events use SGR encoding.
    pub sgr_mouse: bool,
    /// Report motion while a button is held (1002).
    pub mouse_drag: bool,
    /// Report all motion (1003).
    pub mouse_motion: bool,
    /// DECCKM application cursor keys.
    pub app_cursor: bool,
}

type ColorFormatter = Arc<dyn Fn(VteRgb) -> String + Send + Sync>;
type SizeFormatter = Arc<dyn Fn(WindowSize) -> String + Send + Sync>;

/// A reply the terminal owes the program, in the order it was requested.
enum Reply {
    Text(String),
    Color(usize, ColorFormatter),
    TextAreaSize(SizeFormatter),
}

#[derive(Clone)]
pub(crate) struct Listener {
    replies: Arc<Mutex<Vec<Reply>>>,
    title: Arc<Mutex<Option<String>>>,
    clipboard: Arc<Mutex<Vec<String>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let reply = match event {
            Event::PtyWrite(text) => Reply::Text(text),
            Event::ColorRequest(index, format) => Reply::Color(index, format),
            Event::TextAreaSizeRequest(format) => Reply::TextAreaSize(format),
            Event::Title(title) => {
                *self.title.lock().unwrap() = Some(title);
                return;
            }
            Event::ClipboardStore(ClipboardType::Clipboard, text) => {
                self.clipboard.lock().unwrap().push(text);
                return;
            }
            Event::ResetTitle => {
                *self.title.lock().unwrap() = None;
                return;
            }
            _ => return,
        };
        self.replies.lock().unwrap().push(reply);
    }
}

struct GridSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// One pane's terminal: parser, grid, scrollback, modes, and selection.
pub struct TermState {
    pub(crate) term: Term<Listener>,
    parser: Processor,
    replies: Arc<Mutex<Vec<Reply>>>,
    title: Arc<Mutex<Option<String>>>,
    clipboard: Arc<Mutex<Vec<String>>>,
    palette: Palette,
    cell_size: (u16, u16),
}

impl TermState {
    /// Create an empty terminal of the given size with kitty keyboard support enabled.
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_config(cols, rows, Config { kitty_keyboard: true, ..Config::default() })
    }

    /// Like `new`, keeping `lines` lines of scrollback.
    pub fn with_scrollback(cols: u16, rows: u16, lines: usize) -> Self {
        Self::with_config(cols, rows, Config { kitty_keyboard: true, scrolling_history: lines, ..Config::default() })
    }

    fn with_config(cols: u16, rows: u16, config: Config) -> Self {
        let replies = Arc::new(Mutex::new(Vec::new()));
        let title = Arc::new(Mutex::new(None));
        let clipboard = Arc::new(Mutex::new(Vec::new()));
        let listener = Listener { replies: replies.clone(), title: title.clone(), clipboard: clipboard.clone() };
        let size = GridSize { cols: cols as usize, rows: rows as usize };
        TermState {
            term: Term::new(config, &size, listener),
            parser: Processor::new(),
            replies,
            title,
            clipboard,
            palette: DARK,
            cell_size: (0, 0),
        }
    }

    /// Use `palette` from now on, including for color query answers.
    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// Feed program output into the terminal.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Take the bytes the terminal wants written back to the program (replies to queries).
    pub fn take_pty_writes(&mut self) -> Vec<u8> {
        let replies = std::mem::take(&mut *self.replies.lock().unwrap());
        let mut bytes = Vec::new();
        for reply in replies {
            let text = match reply {
                Reply::Text(text) => text,
                Reply::Color(index, format) => {
                    let color = self.term.colors()[index].unwrap_or_else(|| {
                        let rgb = self.default_color(index);
                        VteRgb { r: rgb.r, g: rgb.g, b: rgb.b }
                    });
                    format(color)
                }
                Reply::TextAreaSize(format) => format(WindowSize {
                    num_lines: self.term.screen_lines() as u16,
                    num_cols: self.term.columns() as u16,
                    cell_width: self.cell_size.0,
                    cell_height: self.cell_size.1,
                }),
            };
            bytes.extend_from_slice(text.as_bytes());
        }
        bytes
    }

    /// Palette color for an OSC color index: 0–255 indexed, 256 foreground, 257 background, 258 cursor.
    fn default_color(&self, index: usize) -> Rgb {
        match index {
            0..=255 => self.palette.indexed(index as u8),
            256 => self.palette.foreground,
            257 => self.palette.background,
            258 => self.palette.cursor,
            _ => self.palette.foreground,
        }
    }

    /// Take the texts programs asked to copy to the clipboard (OSC 52), oldest first.
    pub fn take_clipboard_stores(&mut self) -> Vec<String> {
        std::mem::take(&mut *self.clipboard.lock().unwrap())
    }

    /// Window title set by the program (OSC 0/2), if any.
    pub fn title(&self) -> Option<String> {
        self.title.lock().unwrap().clone()
    }

    /// Change the grid size.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(GridSize { cols: cols as usize, rows: rows as usize });
    }

    /// Record the cell size in pixels, used to answer text-area size queries.
    pub fn set_cell_size(&mut self, width: u16, height: u16) {
        self.cell_size = (width, height);
    }

    /// Modes that affect key encoding.
    pub fn key_modes(&self) -> KeyModes {
        let mode = self.term.mode();
        KeyModes {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            kitty_disambiguate: mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
        }
    }

    /// Modes that affect mouse-wheel handling.
    pub fn scroll_modes(&self) -> ScrollModes {
        let mode = self.term.mode();
        ScrollModes {
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
            mouse_report: mode.intersects(TermMode::MOUSE_MODE),
            sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
            mouse_drag: mode.contains(TermMode::MOUSE_DRAG),
            mouse_motion: mode.contains(TermMode::MOUSE_MOTION),
            app_cursor: mode.contains(TermMode::APP_CURSOR),
        }
    }

    /// Whether the program enabled bracketed paste.
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Scroll the viewport; positive moves towards older output.
    pub fn scroll_viewport(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    /// Lines of scrollback above the screen and how many of them the viewport is scrolled up by.
    pub fn scroll_extent(&self) -> (usize, usize) {
        let grid = self.term.grid();
        (grid.history_size(), grid.display_offset())
    }

    /// Scroll the viewport to `offset` lines above the live output.
    pub fn scroll_to_offset(&mut self, offset: usize) {
        let (_, current) = self.scroll_extent();
        self.scroll_viewport(offset as i32 - current as i32);
    }

    /// Whether the program is on the alternate screen, which has no scrollback.
    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Jump the viewport back to live output.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Begin a selection at a viewport cell.
    pub fn start_selection(&mut self, col: u16, row: u16, right_half: bool) {
        let point = self.viewport_point(col, row);
        self.term.selection = Some(Selection::new(SelectionType::Simple, point, side(right_half)));
    }

    /// Extend the current selection to a viewport cell.
    pub fn update_selection(&mut self, col: u16, row: u16, right_half: bool) {
        let point = self.viewport_point(col, row);
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side(right_half));
        }
    }

    /// Remove any selection.
    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// Selected text in logical order, or `None` when nothing is selected.
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|text| !text.is_empty())
    }

    /// Text of a viewport row, one char per cell (wide-char spacers become spaces), trailing spaces trimmed.
    pub fn row_text(&self, row: u16) -> String {
        let line = Line(row as i32 - self.term.grid().display_offset() as i32);
        let grid_row = &self.term.grid()[line];
        let text: String = (0..self.term.columns())
            .map(|col| {
                let cell = &grid_row[Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) { ' ' } else { cell.c }
            })
            .collect();
        text.trim_end().to_string()
    }

    /// Viewport `(col, row)` of the cursor, if it is visible on screen.
    pub fn cursor_cell(&self) -> Option<(usize, usize)> {
        let point = self.term.grid().cursor.point;
        let row = point.line.0 + self.term.grid().display_offset() as i32;
        (0..self.term.screen_lines() as i32).contains(&row).then_some((point.column.0, row as usize))
    }

    /// How many characters the cursor must move to reach viewport cell `(col, row)`, negative for left. Only at a
    /// prompt (main screen, not scrolled back) and on the cursor's line, soft-wrapped rows included; a cell past the
    /// line's text means its end. `None` when it cannot or need not move.
    pub fn cursor_moves_to(&self, col: u16, row: u16) -> Option<i32> {
        let cursor = self.prompt_cursor()?;
        let steps = self.steps(cursor, (row as usize, self.clamp_to_text(row as usize, col as usize)))?;
        (steps != 0).then_some(steps)
    }

    /// A selection on the prompt's line, as an edit: the cursor moves this many characters to the selection's end,
    /// then this many Backspaces delete it. `None` without a selection there.
    pub fn prompt_selection_edit(&self) -> Option<(i32, usize)> {
        let cursor = self.prompt_cursor()?;
        let range = self.term.selection.as_ref()?.to_range(&self.term)?;
        let (start, end) = (range.start, range.end);
        if start.line.0 < 0 {
            return None;
        }
        let start = (start.line.0 as usize, self.clamp_to_text(start.line.0 as usize, start.column.0));
        let end = (end.line.0 as usize, self.clamp_to_text(end.line.0 as usize, end.column.0 + 1));
        let (moves, length) = (self.steps(cursor, end)?, self.steps(start, end)?);
        (length > 0).then_some((moves, length as usize))
    }

    /// The prompt line's cells (`(row, col)`): from the start of its first (soft-wrapped) row to the end of its text.
    pub fn prompt_line(&self) -> Option<((usize, usize), (usize, usize))> {
        let (row, _) = self.prompt_cursor()?;
        let cols = self.term.columns();
        let wraps = |row: usize| self.term.grid()[Line(row as i32)][Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        let first = (0..row).rev().take_while(|&above| wraps(above)).last().unwrap_or(row);
        let last = (row..self.term.screen_lines() - 1).find(|&below| !wraps(below)).unwrap_or(row);
        Some(((first, 0), (last, self.clamp_to_text(last, cols))))
    }

    /// The cell one `reach` from `from` (`(row, col)`) on the prompt line, forward or back, kept within it.
    pub fn prompt_step(&self, from: (usize, usize), reach: Reach, forward: bool) -> (usize, usize) {
        let Some((start, end)) = self.prompt_line() else { return from };
        let cols = self.term.columns();
        let (start, end) = (start.0 * cols + start.1, end.0 * cols + end.1);
        let cell = |index: usize| &self.term.grid()[Line((index / cols) as i32)][Column(index % cols)];
        let word = |index: usize| cell(index).c.is_alphanumeric() || cell(index).c == '_';
        let step = |index: usize| {
            if forward { (index < end).then_some(index + 1) } else { (index > start).then(|| index - 1) }
        };
        let next = |index: usize| {
            let mut index = step(index)?;
            while index < end && cell(index).flags.contains(Flags::WIDE_CHAR_SPACER) {
                index = step(index)?;
            }
            Some(index)
        };
        // The character a move crosses: the one at the index going forward, the one before it going back.
        let crossed = |index: usize| if forward { index } else { index - 1 };
        let at_edge = |index: usize| if forward { index >= end } else { index <= start };
        let mut index = (from.0 * cols + from.1).clamp(start, end);
        match reach {
            Reach::Char => index = next(index).unwrap_or(index),
            Reach::Line => index = if forward { end } else { start },
            Reach::Word => {
                while !at_edge(index) && !word(crossed(index)) {
                    index = next(index).unwrap_or(index);
                }
                while !at_edge(index) && word(crossed(index)) {
                    index = next(index).unwrap_or(index);
                }
            }
        }
        (index / cols, index % cols)
    }

    /// Select the cells between `a` and `b` (`(row, col)`, either order, the later one excluded); none when equal.
    pub fn select_cells(&mut self, a: (usize, usize), b: (usize, usize)) {
        if a == b {
            self.clear_selection();
            return;
        }
        let ((first_row, first_col), last) = if a < b { (a, b) } else { (b, a) };
        let cols = self.term.columns();
        let last = last.0 * cols + last.1 - 1;
        self.start_selection(first_col as u16, first_row as u16, false);
        self.update_selection((last % cols) as u16, (last / cols) as u16, true);
    }

    /// The cursor's viewport cell (`(row, col)`), when it sits at a prompt: main screen, not scrolled back.
    pub fn prompt_cursor(&self) -> Option<(usize, usize)> {
        if self.alt_screen() || self.term.grid().display_offset() != 0 {
            return None;
        }
        self.cursor_cell().map(|(col, row)| (row, col))
    }

    /// `col` on viewport `row`, no further than the end of its text (or the cursor, on the cursor's row).
    fn clamp_to_text(&self, row: usize, col: usize) -> usize {
        let end = self.row_text(row as u16).chars().count();
        let end = match self.cursor_cell() {
            Some((cursor_col, cursor_row)) if cursor_row == row => end.max(cursor_col),
            _ => end,
        };
        col.min(end)
    }

    /// Characters from viewport cell `from` to `to` (`(row, col)`), negative going left, when every row between
    /// them wraps into the next (one line of input); wide characters count once.
    fn steps(&self, from: (usize, usize), to: (usize, usize)) -> Option<i32> {
        let (grid, cols) = (self.term.grid(), self.term.columns());
        let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
        let cell = |index: usize| &grid[Line((index / cols) as i32)][Column(index % cols)];
        if (lo.0..hi.0).any(|row| !cell(row * cols + cols - 1).flags.contains(Flags::WRAPLINE)) {
            return None;
        }
        let steps = (lo.0 * cols + lo.1..hi.0 * cols + hi.1)
            .filter(|&index| !cell(index).flags.contains(Flags::WIDE_CHAR_SPACER))
            .count() as i32;
        Some(if from <= to { steps } else { -steps })
    }

    /// Resolve the visible screen into plain data for rendering.
    pub fn snapshot(&self) -> ScreenSnapshot {
        let content = self.term.renderable_content();
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let offset = content.display_offset as i32;
        let blank = CellView {
            ch: ' ',
            combining: None,
            style: CellStyle {
                fg: self.palette.foreground,
                bg: self.palette.background,
                bold: false,
                italic: false,
                underline: false,
                strikethrough: false,
            },
            wide: false,
            spacer: false,
        };
        let mut grid = vec![vec![blank; cols]; rows];
        let selection = content.selection;
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            let col = indexed.point.column.0;
            if row < 0 || row as usize >= rows || col >= cols {
                continue;
            }
            let selected = selection.is_some_and(|range| range.contains(indexed.point));
            grid[row as usize][col] = self.cell_view(indexed.cell, content.colors, selected);
        }
        let cursor = (content.mode.contains(TermMode::SHOW_CURSOR) && content.cursor.shape != CursorShape::Hidden)
            .then(|| {
                let row = content.cursor.point.line.0 + offset;
                let col = content.cursor.point.column.0;
                (row >= 0 && (row as usize) < rows && col < cols).then(|| {
                    let cell = &grid[row as usize][col];
                    CursorView { col, row: row as usize, wide: cell.wide, ch: cell.ch, style: cell.style }
                })
            })
            .flatten();
        ScreenSnapshot {
            cols,
            rows: grid,
            cursor,
            background: self.palette.background,
            cursor_color: self.palette.cursor,
        }
    }

    fn viewport_point(&self, col: u16, row: u16) -> Point {
        let offset = self.term.grid().display_offset() as i32;
        Point::new(Line(row as i32 - offset), Column(col as usize))
    }

    fn cell_view(&self, cell: &Cell, colors: &Colors, selected: bool) -> CellView {
        let flags = cell.flags;
        let mut fg = self.resolve(cell.fg, colors);
        let mut bg = self.resolve(cell.bg, colors);
        if flags.contains(Flags::DIM) {
            fg = dim(fg);
        }
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        if selected {
            bg = self.palette.selection;
        }
        CellView {
            ch: cell.c,
            combining: cell.zerowidth().map(|marks| marks.to_vec()),
            style: CellStyle {
                fg,
                bg,
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                underline: flags.intersects(Flags::ALL_UNDERLINES),
                strikethrough: flags.contains(Flags::STRIKEOUT),
            },
            wide: flags.contains(Flags::WIDE_CHAR),
            spacer: flags.contains(Flags::WIDE_CHAR_SPACER),
        }
    }

    fn resolve(&self, color: Color, colors: &Colors) -> Rgb {
        match color {
            Color::Spec(rgb) => Rgb { r: rgb.r, g: rgb.g, b: rgb.b },
            Color::Indexed(index) => colors[index as usize]
                .map(|rgb| Rgb { r: rgb.r, g: rgb.g, b: rgb.b })
                .unwrap_or_else(|| self.palette.indexed(index)),
            Color::Named(name) => colors[name as usize]
                .map(|rgb| Rgb { r: rgb.r, g: rgb.g, b: rgb.b })
                .unwrap_or_else(|| self.named(name)),
        }
    }

    fn named(&self, name: NamedColor) -> Rgb {
        let index = name as usize;
        match name {
            NamedColor::Foreground | NamedColor::BrightForeground => self.palette.foreground,
            NamedColor::DimForeground => dim(self.palette.foreground),
            NamedColor::Background => self.palette.background,
            NamedColor::Cursor => self.palette.cursor,
            _ if index < 16 => self.palette.ansi[index],
            // DimBlack..=DimWhite follow Cursor (258) in declaration order.
            _ => dim(self.palette.ansi[index - NamedColor::DimBlack as usize]),
        }
    }
}

fn side(right_half: bool) -> Side {
    if right_half { Side::Right } else { Side::Left }
}

fn dim(color: Rgb) -> Rgb {
    let scale = |v: u8| (v as u16 * 2 / 3) as u8;
    Rgb { r: scale(color.r), g: scale(color.g), b: scale(color.b) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::DARK;
    use crate::snapshot::Rgb;

    #[test]
    fn a_click_on_the_prompt_line_moves_the_cursor_there() {
        let mut t = TermState::new(10, 3);
        t.feed(b"hello");
        assert_eq!(t.cursor_moves_to(1, 0), Some(-4));
        assert_eq!(t.cursor_moves_to(8, 0), None, "past the text is the end, where the cursor already is");
        t.feed(b"\x1b[3D");
        assert_eq!(t.cursor_moves_to(9, 0), Some(3), "clamped to the end of the text");
    }

    #[test]
    fn a_click_follows_soft_wraps_but_not_other_lines() {
        let mut t = TermState::new(10, 3);
        t.feed(b"abcdefghijkl");
        assert_eq!(t.cursor_moves_to(3, 0), Some(-9), "from the wrapped row back to the first");
        let mut t = TermState::new(10, 3);
        t.feed(b"ab\r\ncd");
        assert_eq!(t.cursor_moves_to(0, 0), None, "a previous line is not the prompt");
    }

    #[test]
    fn a_selection_on_the_prompt_line_becomes_moves_and_backspaces() {
        let mut t = TermState::new(20, 3);
        t.feed(b"$ hello world");
        t.start_selection(8, 0, false);
        t.update_selection(12, 0, true);
        assert_eq!(t.prompt_selection_edit(), Some((0, 5)), "\"world\" ends at the cursor");
        t.start_selection(2, 0, false);
        t.update_selection(6, 0, true);
        assert_eq!(t.prompt_selection_edit(), Some((-6, 5)), "\"hello\": back to its end, then delete it");
        t.start_selection(10, 0, false);
        t.update_selection(19, 0, true);
        assert_eq!(t.prompt_selection_edit(), Some((0, 3)), "blank space past the text is not deleted");
    }

    #[test]
    fn a_selection_off_the_prompt_line_is_not_an_edit() {
        let mut t = TermState::new(20, 3);
        t.feed(b"old output\r\n$ ls");
        t.start_selection(0, 0, false);
        t.update_selection(2, 0, true);
        assert_eq!(t.prompt_selection_edit(), None);
    }

    #[test]
    fn prompt_steps_move_by_character_word_and_line() {
        let mut t = TermState::new(20, 3);
        t.feed(b"$ hello big_world");
        let cursor = t.prompt_cursor().unwrap();
        assert_eq!(cursor, (0, 17));
        assert_eq!(t.prompt_step(cursor, Reach::Char, false), (0, 16));
        assert_eq!(t.prompt_step(cursor, Reach::Char, true), (0, 17), "the end of the text is as far as it goes");
        assert_eq!(t.prompt_step(cursor, Reach::Word, false), (0, 8), "big_world is one word");
        assert_eq!(t.prompt_step((0, 8), Reach::Word, false), (0, 2));
        assert_eq!(t.prompt_step((0, 2), Reach::Word, true), (0, 7));
        assert_eq!(t.prompt_step(cursor, Reach::Line, false), (0, 0));
    }

    #[test]
    fn select_cells_then_delete_takes_exactly_those_characters() {
        let mut t = TermState::new(20, 3);
        t.feed(b"$ hello world");
        let cursor = t.prompt_cursor().unwrap();
        let word = t.prompt_step(cursor, Reach::Word, false);
        t.select_cells(cursor, word);
        assert_eq!(t.selection_text().as_deref(), Some("world"));
        assert_eq!(t.prompt_selection_edit(), Some((0, 5)));
        t.select_cells(cursor, cursor);
        assert_eq!(t.selection_text(), None);
    }

    #[test]
    fn the_prompt_line_spans_its_soft_wraps() {
        let mut t = TermState::new(10, 3);
        t.feed(b"x\r\n$ abcdefghij");
        assert_eq!(t.prompt_line(), Some(((1, 0), (2, 2))));
    }

    #[test]
    fn no_click_to_move_on_the_alternate_screen() {
        let mut t = TermState::new(10, 3);
        t.feed(b"\x1b[?1049hhello");
        assert_eq!(t.cursor_moves_to(0, 0), None);
    }

    #[test]
    fn plain_text_lands_in_grid_and_moves_cursor() {
        let mut t = TermState::new(10, 3);
        t.feed(b"hi");
        let s = t.snapshot();
        assert_eq!((s.rows[0][0].ch, s.rows[0][1].ch), ('h', 'i'));
        let c = s.cursor.unwrap();
        assert_eq!((c.col, c.row), (2, 0));
    }

    #[test]
    fn sgr_colors_resolve() {
        let mut t = TermState::new(10, 3);
        t.feed(b"\x1b[31mA\x1b[38;2;1;2;3mB\x1b[0mC\x1b[38;5;196mD\x1b[38;5;244mE");
        let row = &t.snapshot().rows[0];
        assert_eq!(row[0].style.fg, DARK.ansi[1]);
        assert_eq!(row[1].style.fg, Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(row[2].style.fg, DARK.foreground);
        assert_eq!(row[3].style.fg, Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(row[4].style.fg, Rgb { r: 128, g: 128, b: 128 });
    }

    #[test]
    fn inverse_swaps_fg_and_bg() {
        let mut t = TermState::new(10, 3);
        t.feed(b"\x1b[7mZ");
        let style = t.snapshot().rows[0][0].style;
        assert_eq!((style.fg, style.bg), (DARK.background, DARK.foreground));
    }

    #[test]
    fn wide_char_marks_following_spacer() {
        let mut t = TermState::new(10, 3);
        t.feed("漢a".as_bytes());
        let row = &t.snapshot().rows[0];
        assert!(row[0].wide);
        assert!(row[1].spacer);
        assert_eq!(row[2].ch, 'a');
    }

    #[test]
    fn cursor_position_query_is_answered_once() {
        let mut t = TermState::new(10, 5);
        t.feed(b"\x1b[2;3H\x1b[6n");
        assert_eq!(t.take_pty_writes(), b"\x1b[2;3R".to_vec());
        assert!(t.take_pty_writes().is_empty());
    }

    #[test]
    fn color_queries_are_answered_from_the_palette() {
        let mut t = TermState::new(10, 3);
        t.feed(b"\x1b[6n\x1b]11;?\x07\x1b]10;?\x1b\\");
        let replies = String::from_utf8(t.take_pty_writes()).unwrap();
        assert_eq!(replies, "\x1b[1;1R\x1b]11;rgb:1414/1818/2121\x07\x1b]10;rgb:d8d8/dddd/e8e8\x1b\\");
    }

    #[test]
    fn set_palette_changes_query_answers() {
        let mut t = TermState::new(10, 3);
        t.set_palette(crate::palette::LIGHT);
        t.feed(b"\x1b]11;?\x07");
        assert_eq!(String::from_utf8(t.take_pty_writes()).unwrap(), "\x1b]11;rgb:f8f8/f9f9/fcfc\x07");
    }

    #[test]
    fn text_area_pixel_size_query_is_answered() {
        let mut t = TermState::new(10, 3);
        t.set_cell_size(8, 18);
        t.feed(b"\x1b[14t");
        assert_eq!(t.take_pty_writes(), b"\x1b[4;54;80t".to_vec());
    }

    #[test]
    fn kitty_keyboard_query_and_push_pop() {
        let mut t = TermState::new(10, 3);
        t.feed(b"\x1b[?u");
        assert_eq!(t.take_pty_writes(), b"\x1b[?0u".to_vec());
        assert!(!t.key_modes().kitty_disambiguate);
        t.feed(b"\x1b[>1u");
        assert!(t.key_modes().kitty_disambiguate);
        t.feed(b"\x1b[<u");
        assert!(!t.key_modes().kitty_disambiguate);
    }

    #[test]
    fn modes_are_reported() {
        let mut t = TermState::new(10, 3);
        t.feed(b"\x1b[?1h\x1b[?2004h\x1b[?1049h\x1b[?1000h\x1b[?1006h");
        assert!(t.key_modes().app_cursor);
        assert!(t.bracketed_paste());
        let s = t.scroll_modes();
        assert!(s.alt_screen && s.mouse_report && s.sgr_mouse && s.app_cursor);
    }

    #[test]
    fn selection_copies_logical_text() {
        let mut t = TermState::new(20, 3);
        t.feed(b"hello world");
        t.start_selection(0, 0, false);
        t.update_selection(4, 0, true);
        assert_eq!(t.selection_text().as_deref(), Some("hello"));
        assert_eq!(t.snapshot().rows[0][0].style.bg, DARK.selection);
        t.clear_selection();
        assert_eq!(t.selection_text(), None);
    }

    #[test]
    fn resize_changes_dimensions() {
        let mut t = TermState::new(10, 3);
        t.resize(20, 5);
        let s = t.snapshot();
        assert_eq!((s.cols, s.rows.len()), (20, 5));
    }

    #[test]
    fn viewport_scrolls_through_history() {
        let mut t = TermState::new(10, 5);
        for i in 0..30 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(t.row_text(0), "line26");
        t.scroll_viewport(5);
        assert_eq!(t.row_text(0), "line21");
        t.scroll_to_bottom();
        assert_eq!(t.row_text(0), "line26");
    }

    #[test]
    fn scroll_extent_follows_the_viewport() {
        let mut t = TermState::new(10, 5);
        assert_eq!(t.scroll_extent(), (0, 0));
        for i in 0..30 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(t.scroll_extent(), (26, 0));
        t.scroll_to_offset(10);
        assert_eq!(t.scroll_extent(), (26, 10));
        assert_eq!(t.row_text(0), "line16");
        t.scroll_to_offset(0);
        assert_eq!(t.scroll_extent(), (26, 0));
        assert!(!t.alt_screen());
        t.feed(b"\x1b[?1049h");
        assert!(t.alt_screen());
    }

    #[test]
    fn osc_title_is_tracked() {
        let mut t = TermState::new(10, 3);
        assert_eq!(t.title(), None);
        t.feed(b"\x1b]2;build: ok\x07");
        assert_eq!(t.title().as_deref(), Some("build: ok"));
        t.feed(b"\x1b]0;second\x1b\\");
        assert_eq!(t.title().as_deref(), Some("second"));
    }

    #[test]
    fn osc52_copy_is_collected_and_paste_is_ignored() {
        let mut t = TermState::new(10, 3);
        assert!(t.take_clipboard_stores().is_empty());
        t.feed(b"\x1b]52;c;aGVsbG8=\x07");
        t.feed(b"\x1b]52;p;aWdub3JlZA==\x07");
        t.feed(b"\x1b]52;c;?\x07");
        assert_eq!(t.take_clipboard_stores(), vec!["hello".to_string()]);
        assert!(t.take_clipboard_stores().is_empty());
        assert!(t.take_pty_writes().is_empty());
    }

    #[test]
    fn history_is_bounded() {
        let mut t = TermState::new(10, 5);
        for i in 0..20_000 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        t.scroll_viewport(1_000_000);
        assert_eq!(t.row_text(0), "line9996");
    }

    #[test]
    fn with_scrollback_keeps_that_many_lines() {
        let mut t = TermState::with_scrollback(10, 5, 1_000);
        for i in 0..3_000 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        t.scroll_viewport(1_000_000);
        assert_eq!(t.row_text(0), "line1996");
    }
}
