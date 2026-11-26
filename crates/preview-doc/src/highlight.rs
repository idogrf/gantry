//! Syntax highlighting with syntect, mapped to eight tokens the theme colors.

use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

use app_core::preview::MAX_LINE_CHARS;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// The largest text that is highlighted (a file, or a document's code blocks together); above it the text is plain.
pub const MAX_HIGHLIGHT_BYTES: usize = 1024 * 1024;

/// What a piece of code is, as far as its color goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Token {
    /// Anything not listed below.
    Plain,
    /// A comment.
    Comment,
    /// A keyword or storage modifier.
    Keyword,
    /// A string literal.
    String,
    /// A number, or a language constant.
    Number,
    /// A function name.
    Function,
    /// A type or class name.
    Type,
    /// A markup tag or heading.
    Tag,
}

/// A colored part of one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// Byte range inside the line, the line ending excluded.
    pub range: Range<usize>,
    /// What the range is; never `Plain`, those ranges are not stored.
    pub token: Token,
}

/// The colored spans of every line of a text.
#[derive(Debug)]
pub struct Highlights {
    lines: Vec<Vec<Span>>,
}

impl Highlights {
    /// The spans of line `index`, in order; empty past the last line.
    pub fn line(&self, index: usize) -> &[Span] {
        self.lines.get(index).map_or(&[], Vec::as_slice)
    }

    /// The number of lines, counted as `app_core::preview::TextDoc` counts them.
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
}

/// The token a scope name stands for, by prefix; `Plain` when none matches.
pub fn token_for_scope(scope: &str) -> Token {
    const PREFIXES: [(&str, Token); 17] = [
        ("comment", Token::Comment),
        ("string", Token::String),
        ("constant.numeric", Token::Number),
        ("constant.language", Token::Number),
        ("constant.character", Token::Number),
        ("keyword", Token::Keyword),
        ("storage.modifier", Token::Keyword),
        // `fn`, `def` and the like: keywords, though they sit under `storage.type`.
        ("storage.type.function", Token::Keyword),
        ("entity.name.function", Token::Function),
        ("support.function", Token::Function),
        ("variable.function", Token::Function),
        ("entity.name.type", Token::Type),
        ("entity.name.class", Token::Type),
        ("support.type", Token::Type),
        ("support.class", Token::Type),
        ("storage.type", Token::Type),
        ("entity.name.tag", Token::Tag),
    ];
    if scope.starts_with("markup.heading") {
        return Token::Tag;
    }
    PREFIXES.iter().find(|(prefix, _)| scope.starts_with(prefix)).map_or(Token::Plain, |(_, token)| *token)
}

/// Highlight a file's text: the syntax comes from the extension, then the file name, then the first line.
/// `None` for plain text, an unknown syntax, text over the caps or a parse error.
pub fn highlight_file(path: &Path, text: &str) -> Option<Highlights> {
    if text.len() > MAX_HIGHLIGHT_BYTES {
        return None;
    }
    let set = syntaxes();
    let name = path.file_name()?.to_str()?;
    let syntax = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| set.find_syntax_by_extension(e))
        .or_else(|| set.find_syntax_by_extension(name))
        .or_else(|| set.find_syntax_by_first_line(text.lines().next().unwrap_or("")))?;
    highlight(syntax, text)
}

/// Highlight a code block: `lang` is its info string, of which the first word names the language.
/// `None` for an unknown or empty language, text over the caps or a parse error.
pub fn highlight_code(lang: &str, text: &str) -> Option<Highlights> {
    if text.len() > MAX_HIGHLIGHT_BYTES {
        return None;
    }
    let token = lang.split_whitespace().next()?;
    highlight(syntaxes().find_syntax_by_token(token)?, text)
}

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn highlight(syntax: &SyntaxReference, text: &str) -> Option<Highlights> {
    if syntax.name == "Plain Text" {
        return None;
    }
    let set = syntaxes();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut lines = Vec::new();
    for piece in text.split_inclusive('\n') {
        let body = piece.strip_suffix('\n').unwrap_or(piece);
        let body = body.strip_suffix('\r').unwrap_or(body);
        if body.len() > MAX_LINE_CHARS {
            return None;
        }
        // syntect's newline syntaxes expect every line to end in `\n`.
        let line = format!("{body}\n");
        let ops = state.parse_line(&line, set).ok()?;
        let mut spans: Vec<Span> = Vec::new();
        let mut from = 0;
        let mut token = token_of(&stack);
        let mut push = |from: usize, to: usize, token: Token| {
            let to = to.min(body.len());
            if token == Token::Plain || from >= to {
                return;
            }
            match spans.last_mut() {
                Some(last) if last.token == token && last.range.end == from => last.range.end = to,
                _ => spans.push(Span { range: from..to, token }),
            }
        };
        for (at, op) in ops {
            push(from, at, token);
            from = at;
            stack.apply(&op).ok()?;
            token = token_of(&stack);
        }
        push(from, body.len(), token);
        lines.push(spans);
    }
    Some(Highlights { lines })
}

/// The innermost scope of the stack that maps to a token.
fn token_of(stack: &ScopeStack) -> Token {
    stack
        .as_slice()
        .iter()
        .rev()
        .map(|scope: &Scope| token_for_scope(&scope.build_string()))
        .find(|token| *token != Token::Plain)
        .unwrap_or(Token::Plain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn scopes_map_to_tokens() {
        assert_eq!(token_for_scope("comment.line.double-slash.rust"), Token::Comment);
        assert_eq!(token_for_scope("string.quoted.double"), Token::String);
        assert_eq!(token_for_scope("constant.numeric.integer"), Token::Number);
        assert_eq!(token_for_scope("storage.type.rust"), Token::Type);
        assert_eq!(token_for_scope("keyword.control"), Token::Keyword);
        assert_eq!(token_for_scope("entity.name.function"), Token::Function);
        assert_eq!(token_for_scope("meta.block"), Token::Plain);
    }

    #[test]
    fn rust_is_colored_by_extension() {
        let h = highlight_file(Path::new("a.rs"), "fn main() { // hi\n}\n").unwrap();
        assert!(h.line(0).iter().any(|s| s.token == Token::Keyword && s.range == (0..2)));
        assert!(h.line(0).iter().any(|s| s.token == Token::Comment && s.range.end == 17));
        assert_eq!(h.line_count(), 2);
        assert!(h.line(9).is_empty());
    }

    #[test]
    fn crlf_lines_line_up_with_text_doc() {
        let h = highlight_file(Path::new("a.rs"), "a\r\nb").unwrap();
        assert_eq!(h.line_count(), 2);
        for i in 0..2 {
            assert!(h.line(i).iter().all(|s| s.range.end <= 1));
        }
    }

    #[test]
    fn unknown_or_plain_text_is_not_highlighted() {
        assert!(highlight_file(Path::new("a.xyz"), "x").is_none());
        assert!(highlight_file(Path::new("a.txt"), "x").is_none());
        assert!(highlight_code("nosuchlang", "x").is_none());
        assert!(highlight_code("", "x").is_none());
    }

    #[test]
    fn code_blocks_find_their_language() {
        for lang in ["rust", "rs", "py", "sh", "json", "rust ignore"] {
            assert!(highlight_code(lang, "x").is_some(), "{lang}");
        }
    }

    #[test]
    fn file_names_and_shebangs_find_a_syntax() {
        assert!(highlight_file(Path::new("Makefile"), "all:\n\techo 1\n").is_some());
        assert!(highlight_file(Path::new("run"), "#!/bin/bash\necho 1\n").is_some());
    }

    #[test]
    fn caps_turn_highlighting_off() {
        let big = "let x = 1;\n".repeat(MAX_HIGHLIGHT_BYTES / 11 + 1);
        assert!(big.len() > MAX_HIGHLIGHT_BYTES);
        assert!(highlight_file(Path::new("a.rs"), &big).is_none());
        assert!(highlight_file(Path::new("a.rs"), &"a".repeat(10_001)).is_none());
        assert!(highlight_code("rust", &"a".repeat(10_001)).is_none());
    }
}
