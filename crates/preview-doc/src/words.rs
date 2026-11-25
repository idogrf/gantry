//! Word layout (bidi-aware) and column measures for documents.

use app_core::bidi;

/// The share of the table's width each column takes: its widest cell in characters, clamped to `[3, 40]`, normalized
/// to sum to 1.0.
pub fn column_fractions(widths: &[usize]) -> Vec<f32> {
    let clamped: Vec<usize> = widths.iter().map(|w| (*w).clamp(3, 40)).collect();
    let total: usize = clamped.iter().sum();
    clamped.iter().map(|w| *w as f32 / total as f32).collect()
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
    let rtl = bidi::first_strong_rtl(text).unwrap_or(false);
    let mut words = Vec::new();
    let mut base = 0;
    for (n, segment) in text.split('\n').enumerate() {
        if n > 0 {
            words.push(Word { chars: vec![('\n', base - 1)], split: false });
        }
        let line = segment.strip_suffix('\r').unwrap_or(segment);
        let mut line_words = line_words(line, base);
        if bidi::first_strong_rtl(line).unwrap_or(false) {
            line_words.reverse();
        }
        words.extend(line_words);
        base += segment.len() + 1;
    }
    VisualText { rtl, words }
}

/// The words of one line (no `\n`) in left-to-right display order; `base` is the line's offset in the text.
fn line_words(line: &str, base: usize) -> Vec<Word> {
    let logical: Vec<(usize, char)> = line.char_indices().collect();
    let display = bidi::display_line(line);
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

    #[test]
    fn column_fractions_clamp_and_normalize() {
        let f = column_fractions(&[1, 100, 10]);
        let want = [3.0 / 53.0, 40.0 / 53.0, 10.0 / 53.0];
        assert_eq!(f.len(), 3);
        for (a, b) in f.iter().zip(want) {
            assert!((a - b).abs() < 1e-6, "{f:?}");
        }
        assert!(column_fractions(&[]).is_empty());
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
}
