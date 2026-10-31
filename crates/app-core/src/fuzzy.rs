//! Fuzzy matching (nucleo) shared by the sidebar filter, the launcher and the recent sessions project chooser.

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// A compiled query: each typed word matches unbroken (anywhere, case ignored) when some candidate contains them all;
/// only when none does may its letters spread out ("jnkns" finds "Jenkins").
pub struct Fuzzy {
    pattern: Pattern,
    matcher: Matcher,
    buf: Vec<char>,
}

impl Fuzzy {
    /// A matcher for `query` over `texts` (every candidate the caller will score); `None` when the query is blank
    /// (everything matches).
    pub fn new<S: AsRef<str>>(query: &str, texts: impl IntoIterator<Item = S>) -> Option<Fuzzy> {
        if query.trim().is_empty() {
            return None;
        }
        let mut fuzzy = Fuzzy {
            pattern: Pattern::new(query, CaseMatching::Ignore, Normalization::Smart, AtomKind::Substring),
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            buf: Vec::new(),
        };
        if !texts.into_iter().any(|text| fuzzy.score(text.as_ref()).is_some()) {
            fuzzy.pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        }
        Some(fuzzy)
    }

    /// Score of `text` (higher is better), or `None` if it does not match.
    pub fn score(&mut self, text: &str) -> Option<u32> {
        self.pattern.score(Utf32Str::new(text, &mut self.buf), &mut self.matcher)
    }

    /// The character indices of `text` the query matched (sorted, each once), or `None` if it does not match.
    pub fn indices(&mut self, text: &str) -> Option<Vec<u32>> {
        let mut found = Vec::new();
        self.pattern.indices(Utf32Str::new(text, &mut self.buf), &mut self.matcher, &mut found)?;
        found.sort_unstable();
        found.dedup();
        Some(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_queries_match_everything_and_others_filter() {
        assert!(Fuzzy::new("  ", [""]).is_none());
        let mut fuzzy = Fuzzy::new("jnkns", ["Fix Jenkins lib", "Bump k8s chart"]).unwrap();
        assert!(fuzzy.score("Fix Jenkins lib").is_some());
        assert!(fuzzy.score("Bump k8s chart").is_none());
        let paths = ["~/Private/Repos/rust_terminal", "~/r/u/s/t"];
        let mut exact = Fuzzy::new("rust", paths).unwrap();
        assert!(exact.score(paths[0]).unwrap() > exact.score(paths[1]).unwrap_or(0));
    }

    #[test]
    fn indices_mark_the_matched_chars() {
        let mut fuzzy = Fuzzy::new("se", ["serving"]).unwrap();
        assert_eq!(fuzzy.indices("serving"), Some(vec![0, 1]));
        assert_eq!(fuzzy.indices("docs"), None);
    }

    #[test]
    fn case_is_ignored() {
        assert!(Fuzzy::new("Font", ["font fallback"]).unwrap().score("font fallback").is_some());
        assert!(Fuzzy::new("font", ["Font Fallback"]).unwrap().score("Font Fallback").is_some());
    }

    #[test]
    fn typed_words_stay_whole_while_some_text_contains_them() {
        let texts = ["System sounds", "Selected tab cyan boundary"];
        let mut fuzzy = Fuzzy::new("sound", texts).unwrap();
        assert!(fuzzy.score(texts[0]).is_some());
        assert!(fuzzy.score(texts[1]).is_none(), "s…ound is not the word sound");
        // Without any text holding the word, the letters may spread out again.
        let mut fallback = Fuzzy::new("sound", [texts[1]]).unwrap();
        assert!(fallback.score(texts[1]).is_some());
    }
}
