//! Display order for right-to-left text (UAX #9), one row at a time.
//!
//! The grid keeps text in logical order; this module only decides where each cell is drawn.

use std::ops::Range;

use unicode_bidi::{BidiClass, BidiInfo, bidi_class};

use crate::snapshot::{CellView, ScreenSnapshot};

/// Where each cell of one row is drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowOrder {
    to_logical: Vec<usize>,
    to_visual: Vec<usize>,
    rtl: Vec<bool>,
}

impl RowOrder {
    /// The logical column drawn at visual column `visual`.
    pub fn logical(&self, visual: usize) -> usize {
        self.to_logical.get(visual).copied().unwrap_or(visual)
    }

    /// The visual column where logical column `logical` is drawn.
    pub fn visual(&self, logical: usize) -> usize {
        self.to_visual.get(logical).copied().unwrap_or(logical)
    }

    /// Whether the cell at logical column `logical` is in a right-to-left run.
    pub fn is_rtl(&self, logical: usize) -> bool {
        self.rtl.get(logical).copied().unwrap_or(false)
    }
}

/// Display order of a row, or `None` when it has no right-to-left letters (it is drawn as stored).
///
/// The row splits into segments at frame characters (box drawing and blocks), which stay in place. Each segment is
/// one paragraph whose direction comes from its first strong character.
pub fn row_order(row: &[CellView]) -> Option<RowOrder> {
    if !row.iter().any(|cell| is_rtl_letter(cell.ch)) {
        return None;
    }
    let mut to_logical: Vec<usize> = (0..row.len()).collect();
    let mut rtl = vec![false; row.len()];
    let mut start = 0;
    while start < row.len() {
        if is_frame(row[start].ch) {
            start += 1;
            continue;
        }
        let end = (start..row.len()).find(|&col| is_frame(row[col].ch)).unwrap_or(row.len());
        order_segment(row, start..end, &mut to_logical, &mut rtl);
        start = end;
    }
    let mut to_visual = vec![0; row.len()];
    for (visual, &logical) in to_logical.iter().enumerate() {
        to_visual[logical] = visual;
    }
    Some(RowOrder { to_logical, to_visual, rtl })
}

/// Display order of one line of text outside the terminal grid (the diff view): for each position drawn from left to
/// right, the logical character index and the character to draw (brackets inside right-to-left runs are mirrored).
/// `None` when the line has no right-to-left letters.
pub fn display_line(text: &str) -> Option<Vec<(usize, char)>> {
    use crate::snapshot::{CellStyle, Rgb};
    let style = CellStyle {
        fg: Rgb::hex(0),
        bg: Rgb::hex(0),
        bold: false,
        italic: false,
        underline: false,
        strikethrough: false,
    };
    let cells: Vec<CellView> =
        text.chars().map(|ch| CellView { ch, combining: None, style, wide: false, spacer: false }).collect();
    let order = row_order(&cells)?;
    Some(
        (0..cells.len())
            .map(|visual| {
                let logical = order.logical(visual);
                let ch = if order.is_rtl(logical) { mirrored(cells[logical].ch) } else { cells[logical].ch };
                (logical, ch)
            })
            .collect(),
    )
}

/// The cells of `row` in display order; brackets inside right-to-left runs are mirrored.
pub fn reorder_row(row: &[CellView], order: &RowOrder) -> Vec<CellView> {
    (0..row.len())
        .map(|visual| {
            let logical = order.logical(visual);
            let mut cell = row[logical].clone();
            if order.is_rtl(logical) {
                cell.ch = mirrored(cell.ch);
            }
            cell
        })
        .collect()
}

/// Reorder the cells of one segment. A wide character and its spacer are one unit, character first.
fn order_segment(row: &[CellView], cols: Range<usize>, to_logical: &mut [usize], rtl: &mut [bool]) {
    let units: Vec<usize> = cols.clone().filter(|&col| !row[col].spacer).collect();
    if !units.iter().any(|&col| is_rtl_letter(row[col].ch)) {
        return;
    }
    // One char per unit; paragraph separators would split the paragraph, so they count as blanks.
    let text: String =
        units.iter().map(|&col| if bidi_class(row[col].ch) == BidiClass::B { ' ' } else { row[col].ch }).collect();
    let info = BidiInfo::new(&text, None);
    let paragraph = &info.paragraphs[0];
    let levels = info.reordered_levels_per_char(paragraph, paragraph.range.clone());
    let mut visual = cols.start;
    for unit in BidiInfo::reorder_visual(&levels) {
        let col = units[unit];
        let width = if col + 1 < cols.end && row[col + 1].spacer { 2 } else { 1 };
        for offset in 0..width {
            to_logical[visual] = col + offset;
            rtl[col + offset] = levels[unit].is_rtl();
            visual += 1;
        }
    }
}

/// Whether `ch` is a letter written right to left (Hebrew, Arabic and the like).
pub fn is_rtl_letter(ch: char) -> bool {
    matches!(bidi_class(ch), BidiClass::R | BidiClass::AL)
}

/// Box drawing and block elements: Claude's frames and logo.
fn is_frame(ch: char) -> bool {
    matches!(ch, '\u{2500}'..='\u{259F}')
}

/// The mirror image of a paired bracket (UAX #9 rule L4), for the pairs a terminal meets in practice.
fn mirrored(ch: char) -> char {
    match ch {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        '«' => '»',
        '»' => '«',
        '‹' => '›',
        '›' => '‹',
        other => other,
    }
}

/// Rearrange `snapshot` into display order and return each row's order, for mapping mouse positions back.
///
/// The cursor moves with its cell and shows that cell's displayed (possibly mirrored) character. With
/// `right_to_left` off nothing moves: every row stays in logical order (no [`RowOrder`], so column maps are the
/// identity).
pub fn to_display(snapshot: &mut ScreenSnapshot, right_to_left: bool) -> Vec<Option<RowOrder>> {
    if !right_to_left {
        return vec![None; snapshot.rows.len()];
    }
    let orders: Vec<Option<RowOrder>> = snapshot.rows.iter().map(|row| row_order(row)).collect();
    for (row, order) in snapshot.rows.iter_mut().zip(&orders) {
        if let Some(order) = order {
            *row = reorder_row(row, order);
        }
    }
    if let Some(cursor) = snapshot.cursor.as_mut()
        && let Some(Some(order)) = orders.get(cursor.row)
    {
        cursor.col = order.visual(cursor.col);
        cursor.ch = snapshot.rows[cursor.row][cursor.col].ch;
    }
    orders
}

/// The logical column under displayed column `col` of viewport row `row`, and whether the pointer is on that cell's
/// logical right half (the halves swap inside right-to-left runs).
pub fn logical_hit(orders: &[Option<RowOrder>], col: usize, row: usize, right_half: bool) -> (usize, bool) {
    match orders.get(row) {
        Some(Some(order)) => {
            let logical = order.logical(col);
            (logical, right_half != order.is_rtl(logical))
        }
        _ => (col, right_half),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{CellStyle, Rgb};

    fn style() -> CellStyle {
        CellStyle {
            fg: Rgb::hex(0xffffff),
            bg: Rgb::hex(0),
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
        }
    }

    /// `text` padded with blanks to `cols` cells; `漢` becomes a wide character plus its spacer.
    fn row(text: &str, cols: usize) -> Vec<CellView> {
        let mut cells = Vec::new();
        for ch in text.chars() {
            let wide = ch == '漢';
            cells.push(CellView { ch, combining: None, style: style(), wide, spacer: false });
            if wide {
                cells.push(CellView { ch: ' ', combining: None, style: style(), wide: false, spacer: true });
            }
        }
        cells.resize(cols, CellView { ch: ' ', combining: None, style: style(), wide: false, spacer: false });
        cells
    }

    /// The row as displayed; spacers show as `_`.
    fn shown(text: &str, cols: usize) -> String {
        let cells = row(text, cols);
        let order = row_order(&cells).expect("row has RTL text");
        reorder_row(&cells, &order).iter().map(|cell| if cell.spacer { '_' } else { cell.ch }).collect()
    }

    #[test]
    fn display_line_reorders_a_text_line() {
        let shown = |text: &str| display_line(text).map(|line| line.into_iter().map(|(_, ch)| ch).collect::<String>());
        assert_eq!(shown("abc (d)"), None);
        assert_eq!(shown("שלום world").as_deref(), Some("world םולש"));
        assert_eq!(
            display_line("a שלום").map(|line| line.into_iter().map(|(logical, _)| logical).collect::<Vec<_>>()),
            Some(vec![0, 1, 5, 4, 3, 2])
        );
        assert_eq!(shown("(שלום)").as_deref(), Some("(םולש)"));
    }

    #[test]
    fn rows_without_rtl_letters_are_left_alone() {
        assert_eq!(row_order(&row("hello │ world 2.5", 20)), None);
    }

    #[test]
    fn a_hebrew_line_is_right_aligned_and_english_keeps_its_order() {
        assert_eq!(shown("שלום world", 14), "    world םולש");
        assert_eq!(shown("⏺ אני אוהב Rust מאוד", 24), "    דואמ Rust בהוא ינא ⏺");
    }

    #[test]
    fn leading_blanks_become_the_right_margin() {
        assert_eq!(shown("  שלום", 8), "  םולש  ");
    }

    #[test]
    fn an_english_line_keeps_a_hebrew_word_in_place() {
        assert_eq!(shown("say שלום now", 12), "say םולש now");
    }

    #[test]
    fn frame_characters_stay_and_split_the_row() {
        assert_eq!(shown("│שלום  │abc│", 12), "│  םולש│abc│");
    }

    #[test]
    fn digits_stay_left_to_right_inside_hebrew() {
        assert_eq!(shown("גרסה 2.5", 8), "2.5 הסרג");
    }

    #[test]
    fn brackets_are_mirrored_inside_rtl_runs() {
        assert_eq!(shown("(שלום)", 6), "(םולש)");
    }

    #[test]
    fn a_wide_character_keeps_its_spacer_after_it() {
        assert_eq!(shown("שלום 漢x", 9), " 漢_x םולש");
    }

    #[test]
    fn combining_marks_move_with_their_letter() {
        let mut cells = row("שלום", 4);
        cells[0].combining = Some(vec!['\u{05B8}']);
        let order = row_order(&cells).unwrap();
        let shown = reorder_row(&cells, &order);
        assert_eq!((shown[3].ch, shown[3].combining.clone()), ('ש', Some(vec!['\u{05B8}'])));
    }

    #[test]
    fn the_column_map_goes_both_ways() {
        let order = row_order(&row("שלום world", 14)).unwrap();
        assert_eq!((order.logical(0), order.logical(4), order.logical(13)), (13, 5, 0));
        assert_eq!((order.visual(0), order.visual(5), order.visual(10)), (13, 4, 3));
        assert!(order.is_rtl(0));
        assert!(!order.is_rtl(5));
    }

    use crate::term_state::TermState;

    fn displayed(snapshot: &ScreenSnapshot, row: usize) -> String {
        snapshot.rows[row].iter().map(|cell| cell.ch).collect()
    }

    #[test]
    fn a_snapshot_is_shown_in_display_order() {
        let mut term = TermState::new(12, 2);
        term.feed("hello\r\nשלום עולם".as_bytes());
        let mut snapshot = term.snapshot();
        let orders = to_display(&mut snapshot, true);
        assert_eq!(orders.len(), 2);
        assert!(orders[0].is_none());
        assert_eq!(displayed(&snapshot, 0), "hello       ");
        assert_eq!(displayed(&snapshot, 1), "   םלוע םולש");
    }

    #[test]
    fn the_cursor_follows_its_cell_into_display_order() {
        let mut term = TermState::new(8, 1);
        term.feed("שלום".as_bytes());
        let mut snapshot = term.snapshot();
        to_display(&mut snapshot, true);
        let cursor = snapshot.cursor.expect("cursor is visible");
        assert_eq!((cursor.col, cursor.ch), (3, ' '));

        let mut term = TermState::new(8, 1);
        term.feed("(שלום)\r".as_bytes());
        let mut snapshot = term.snapshot();
        to_display(&mut snapshot, true);
        let cursor = snapshot.cursor.unwrap();
        // The cursor is on logical column 0, '(' — displayed at the right edge, mirrored.
        assert_eq!((cursor.col, cursor.ch), (7, ')'));
    }

    #[test]
    fn dragging_across_a_displayed_hebrew_word_copies_it() {
        let mut term = TermState::new(12, 1);
        term.feed("שלום עולם".as_bytes());
        let mut snapshot = term.snapshot();
        let orders = to_display(&mut snapshot, true);
        // "םולש" is displayed in columns 8..=11: press on the right half of 11, release on the left half of 8.
        let (col, right_half) = logical_hit(&orders, 11, 0, true);
        term.start_selection(col as u16, 0, right_half);
        let (col, right_half) = logical_hit(&orders, 8, 0, false);
        term.update_selection(col as u16, 0, right_half);
        assert_eq!(term.selection_text().as_deref(), Some("שלום"));
    }

    #[test]
    fn hits_outside_reordered_rows_pass_through() {
        let orders = vec![None];
        assert_eq!(logical_hit(&orders, 5, 0, true), (5, true));
        assert_eq!(logical_hit(&orders, 5, 3, false), (5, false));
    }

    #[test]
    fn with_right_to_left_off_text_stays_in_logical_order() {
        let mut term = TermState::new(12, 2);
        term.feed("(שלום)\r\nשלום עולם".as_bytes());
        let mut snapshot = term.snapshot();
        let before = snapshot.rows.clone();
        let orders = to_display(&mut snapshot, false);
        assert_eq!(orders.len(), 2);
        assert!(orders.iter().all(Option::is_none));
        assert_eq!(snapshot.rows, before);
        assert_eq!(displayed(&snapshot, 1), "שלום עולם   ");
        assert_eq!(logical_hit(&orders, 5, 1, true), (5, true), "the column map is the identity");
        assert_eq!(
            (snapshot.cursor.as_ref().unwrap().col, snapshot.cursor.as_ref().unwrap().ch),
            (9, ' '),
            "the cursor stays on its logical column"
        );
    }
}
