//! Word layout (bidi-aware) and column measures for documents.

use app_core::bidi;

/// The share of the table's width each column takes. `widest` is each column's widest cell in characters (clamped to
/// `[3, 40]` for the weights), `longest` its longest word in characters; `char_px` is the measured average width of a
/// character and `pad` the cell padding and borders in px; `total` is the table's width in px (0 or less: not laid out
/// yet, so the shares are the weights alone). Every column gets at least its longest word plus `pad`, so a word never
/// breaks, and the rest of the width goes by the weights. When the minimums do not fit the columns share equally.
pub fn column_fractions(widest: &[usize], longest: &[usize], total: f32, char_px: f32, pad: f32) -> Vec<f32> {
    let weights: Vec<f32> = widest.iter().map(|w| (*w).clamp(3, 40) as f32).collect();
    let weight_sum: f32 = weights.iter().sum();
    if weights.is_empty() {
        return Vec::new();
    }
    let minimums: Vec<f32> =
        (0..weights.len()).map(|i| longest.get(i).copied().unwrap_or(0) as f32 * char_px + pad).collect();
    let minimum_sum: f32 = minimums.iter().sum();
    if total <= 0.0 {
        return weights.iter().map(|w| w / weight_sum).collect();
    }
    if minimum_sum > total {
        return vec![1.0 / weights.len() as f32; weights.len()];
    }
    let spare = total - minimum_sum;
    weights.iter().zip(&minimums).map(|(w, min)| (min + spare * w / weight_sum) / total).collect()
}

/// The length in characters of the longest run of non-space characters in `text`.
pub fn longest_word(text: &str) -> usize {
    text.split_whitespace().map(|word| word.chars().count()).max().unwrap_or(0)
}

/// One word of a paragraph: its characters in display order, each with its logical byte offset in the source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    /// The characters in display order (brackets inside right-to-left runs mirrored, as `bidi::display_line`) with
    /// their logical byte offsets. A trailing space belongs to the word before it.
    pub chars: Vec<(char, usize)>,
    /// The word holds a non-ASCII character, so it is drawn one element per character.
    pub split: bool,
}

/// A paragraph cut into words in the order a flex-wrap container takes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualText {
    /// The paragraph's first strong letter is right-to-left: the direction of the container that draws the words
    /// (`flex_row_reverse` when true).
    pub rtl: bool,
    /// The first logical word of every line comes first, so lines wrap in reading order. A line whose own base
    /// direction (its first strong letter) is right to left comes back reversed from display order, one whose base is
    /// left to right in display order; in a container of the other direction such a line wraps in reading order but
    /// is drawn with its words in the container's direction. A hard break (`\n`, with a `\r` before it dropped) is
    /// its own word.
    pub words: Vec<Word>,
}

/// Cut `text` into words in display order (see [`VisualText`]); offsets stay logical.
///
/// The work is unbounded in the paragraph's length: run it off the UI thread, once per file version.
pub fn visual_words(text: &str) -> VisualText {
    cut(text, true)
}

/// Cut `text` into words in logical order, with no reordering at all (for text shown with the right-to-left display
/// off): the words of every line in reading order, left to right, each character as typed. `rtl` is false.
pub fn logical_words(text: &str) -> VisualText {
    cut(text, false)
}

fn cut(text: &str, reorder: bool) -> VisualText {
    let rtl = reorder && bidi::first_strong_rtl(text).unwrap_or(false);
    let mut words = Vec::new();
    let mut base = 0;
    for (n, segment) in text.split('\n').enumerate() {
        if n > 0 {
            words.push(Word { chars: vec![('\n', base - 1)], split: false });
        }
        let line = segment.strip_suffix('\r').unwrap_or(segment);
        let mut line_words = line_words(line, base, reorder);
        if reorder && bidi::first_strong_rtl(line).unwrap_or(false) {
            line_words.reverse();
        }
        words.extend(line_words);
        base += segment.len() + 1;
    }
    VisualText { rtl, words }
}

/// The words of one line (no `\n`) in left-to-right display order (`reorder`) or as typed; `base` is the line's
/// offset in the text.
fn line_words(line: &str, base: usize, reorder: bool) -> Vec<Word> {
    let logical: Vec<(usize, char)> = line.char_indices().collect();
    let display = if reorder { bidi::display_line(line) } else { None };
    let mut pos: Vec<usize> = (0..logical.len()).collect();
    let mut shown: Vec<char> = logical.iter().map(|(_, c)| *c).collect();
    if let Some(order) = &display {
        for (p, (idx, ch)) in order.iter().enumerate() {
            pos[*idx] = p;
            shown[*idx] = *ch;
        }
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut open = false;
    for (i, (_, c)) in logical.iter().enumerate() {
        if !open {
            groups.push(Vec::new());
        }
        groups.last_mut().expect("a group is open").push(i);
        open = *c != ' ';
    }
    let mut words: Vec<(usize, Word)> = groups
        .into_iter()
        .map(|mut g| {
            g.sort_by_key(|i| pos[*i]);
            let first = pos[g[0]];
            let chars: Vec<(char, usize)> = g.iter().map(|i| (shown[*i], base + logical[*i].0)).collect();
            let split = chars.iter().any(|(c, _)| !c.is_ascii());
            (first, Word { chars, split })
        })
        .collect();
    words.sort_by_key(|(first, _)| *first);
    words.into_iter().map(|(_, w)| w).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(f: &[f32]) -> f32 {
        f.iter().sum()
    }

    #[test]
    fn columns_share_the_width_by_their_widest_cell_before_layout() {
        let f = column_fractions(&[1, 100, 10], &[1, 1, 1], 0.0, 8.0, 26.0);
        let want = [3.0 / 53.0, 40.0 / 53.0, 10.0 / 53.0];
        assert_eq!(f.len(), 3);
        for (a, b) in f.iter().zip(want) {
            assert!((a - b).abs() < 1e-6, "{f:?}");
        }
        assert!(column_fractions(&[], &[], 500.0, 8.0, 26.0).is_empty());
    }

    #[test]
    fn a_column_never_gets_less_than_its_longest_word() {
        // Column 0 has a 20 character word but few characters overall; 8 px per character, 26 px padding.
        let f = column_fractions(&[20, 100, 100], &[20, 3, 3], 600.0, 8.0, 26.0);
        assert!((sum(&f) - 1.0).abs() < 1e-5);
        assert!(f[0] * 600.0 >= 20.0 * 8.0 + 26.0 - 1e-3, "{f:?}");
        assert!(f[1] * 600.0 >= 3.0 * 8.0 + 26.0 - 1e-3, "{f:?}");
        assert!((f[1] - f[2]).abs() < 1e-6);
    }

    #[test]
    fn columns_share_equally_when_the_minimums_do_not_fit() {
        let f = column_fractions(&[30, 5, 5], &[30, 10, 10], 300.0, 8.0, 26.0);
        for share in &f {
            assert!((share - 1.0 / 3.0).abs() < 1e-6, "{f:?}");
        }
    }

    fn text_of(w: &Word) -> String {
        w.chars.iter().map(|(c, _)| *c).collect()
    }

    #[test]
    fn plain_ascii_is_one_word_per_space() {
        let v = visual_words("ab cd");
        assert!(!v.rtl);
        let words: Vec<String> = v.words.iter().map(text_of).collect();
        assert_eq!(words, ["ab ", "cd"]);
        assert!(v.words.iter().all(|w| !w.split));
    }

    #[test]
    fn a_hebrew_paragraph_flows_right_to_left() {
        let v = visual_words("שלום עולם");
        assert!(v.rtl);
        assert_eq!(v.words.len(), 2);
        // The word's letters and its trailing space (offset 8), which the word before it owns.
        let mut offsets: Vec<usize> = v.words[0].chars.iter().map(|(_, o)| *o).collect();
        offsets.sort();
        assert_eq!(offsets, [0, 2, 4, 6, 8]);
        assert!(v.words[0].split);
        // The first logical word shows its letters in display order: the last letter leftmost.
        assert_eq!(v.words[0].chars.last().unwrap().0, 'ש');
    }

    #[test]
    fn english_inside_hebrew_keeps_its_order() {
        let text = "שלום Claude Code";
        let v = visual_words(text);
        assert!(v.rtl);
        let claude = v.words.iter().find(|w| text_of(w).starts_with("Claude")).unwrap();
        assert_eq!(text_of(claude), "Claude ");
        for (c, o) in &claude.chars {
            assert_eq!(text[*o..].chars().next(), Some(*c));
        }
        let code = v.words.iter().find(|w| text_of(w) == "Code").unwrap();
        assert!(!code.split);
        assert!(code.chars.windows(2).all(|p| p[0].1 < p[1].1));
        // Flex-reverse order: the first logical word comes first.
        assert!(v.words[0].chars.iter().any(|(_, o)| *o == 0));
        assert!(v.words[2].chars.iter().any(|(c, _)| *c == 'C'));
    }

    #[test]
    fn brackets_are_mirrored_in_rtl_runs() {
        let v = visual_words("(שלום)");
        let shown: String = v.words.iter().flat_map(|w| w.chars.iter().map(|(c, _)| *c)).collect();
        assert_eq!(shown, "(םולש)");
        // The logical `(` (offset 0) is drawn at the right as `)`; the logical `)` at the left as `(`.
        assert_eq!(v.words[0].chars.last(), Some(&(')', 0)));
        assert_eq!(v.words[0].chars[0].0, '(');
    }

    #[test]
    fn hard_breaks_are_their_own_word() {
        let v = visual_words("ab\ncd");
        let words: Vec<String> = v.words.iter().map(text_of).collect();
        assert_eq!(words, ["ab", "\n", "cd"]);
        assert_eq!(v.words[1].chars, [('\n', 2)]);
        assert_eq!(v.words[2].chars, [('c', 3), ('d', 4)]);
    }

    #[test]
    fn each_line_follows_its_own_direction() {
        let text = "hello\nשלום עולם";
        let v = visual_words(text);
        assert!(!v.rtl);
        let first_offsets: Vec<usize> =
            v.words.iter().map(|w| w.chars.iter().map(|(_, o)| *o).min().unwrap()).collect();
        // hello, break, then the Hebrew line's first logical word (offset 6) before its second (15).
        assert_eq!(first_offsets, [0, 5, 6, 15]);

        let v = visual_words("שלום\nhello world");
        assert!(v.rtl);
        let words: Vec<String> = v.words.iter().map(text_of).collect();
        assert_eq!(words[1..], ["\n", "hello ", "world"]);
    }

    #[test]
    fn digits_and_latin_inside_rtl_keep_their_order() {
        let v = visual_words("גרסה 2.5");
        assert!(v.rtl);
        let num = v.words.iter().find(|w| text_of(w) == "2.5").unwrap();
        assert!(num.chars.windows(2).all(|p| p[0].1 < p[1].1));
    }

    #[test]
    fn crlf_is_a_break() {
        let v = visual_words("ab\r\ncd");
        let words: Vec<String> = v.words.iter().map(text_of).collect();
        assert_eq!(words, ["ab", "\n", "cd"]);
        assert_eq!(v.words[1].chars, [('\n', 3)]);
        assert_eq!(v.words[2].chars, [('c', 4), ('d', 5)]);
    }

    #[test]
    fn logical_words_keep_the_text_order() {
        let v = logical_words("שלום עולם abc");
        assert!(!v.rtl);
        let shown: Vec<String> = v.words.iter().map(text_of).collect();
        assert_eq!(shown, ["שלום ", "עולם ", "abc"]);
        assert!(v.words[0].split && !v.words[2].split);
        let offsets: Vec<usize> = v.words[0].chars.iter().map(|(_, o)| *o).collect();
        assert_eq!(offsets, [0, 2, 4, 6, 8]);
    }

    #[test]
    fn logical_words_break_lines_like_visual_ones() {
        let v = logical_words("ab\r\nשל");
        let shown: Vec<String> = v.words.iter().map(text_of).collect();
        assert_eq!(shown, ["ab", "\n", "של"]);
        assert_eq!(v.words[1].chars, [('\n', 3)]);
    }

    #[test]
    fn the_longest_word_is_counted_in_characters() {
        assert_eq!(longest_word("ab cdef g"), 4);
        assert_eq!(longest_word("שלום\nab"), 4);
        assert_eq!(longest_word(""), 0);
    }
}
