//! Prepare clipboard text for sending to a terminal program.

/// Encode pasted text: line endings become `CR`, `ESC` is removed (so pasted text can never
/// inject control sequences or end bracketed paste early), and bracketed-paste markers are
/// added when the program enabled that mode.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let cleaned: String = text.replace("\r\n", "\r").replace('\n', "\r").chars().filter(|&c| c != '\x1b').collect();
    if bracketed { [b"\x1b[200~".as_slice(), cleaned.as_bytes(), b"\x1b[201~"].concat() } else { cleaned.into_bytes() }
}

/// Paths as shell words, space-separated with a trailing space, as Terminal.app pastes dropped files: anything outside a
/// safe set gets a backslash, so both a shell and Claude's prompt read each path back whole.
pub fn quote_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> String {
    paths
        .into_iter()
        .map(|path| {
            path.chars().fold(String::new(), |mut word, c| {
                if !(c.is_alphanumeric() || "/._-+,:@%=^".contains(c)) {
                    word.push('\\');
                }
                word.push(c);
                word
            })
        })
        .map(|word| word + " ")
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newlines_become_carriage_returns() {
        assert_eq!(encode_paste("a\r\nb\nc", false), b"a\rb\rc".to_vec());
    }

    #[test]
    fn bracketed_paste_wraps_text() {
        assert_eq!(encode_paste("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
    }

    #[test]
    fn escape_cannot_break_out_of_bracketed_paste() {
        let pasted = encode_paste("x\x1b[201~rm -rf ~\n", true);
        assert_eq!(pasted, b"\x1b[200~x[201~rm -rf ~\r\x1b[201~".to_vec());
    }

    #[test]
    fn paths_are_escaped_and_space_separated() {
        assert_eq!(
            quote_paths(["/tmp/a.png", "/Users/me/My Shot (2).png"]),
            "/tmp/a.png /Users/me/My\\ Shot\\ \\(2\\).png "
        );
        assert_eq!(quote_paths(["/x/it's;$y"]), "/x/it\\'s\\;\\$y ");
        assert_eq!(quote_paths(["/דוח/צילום.pdf"]), "/דוח/צילום.pdf ");
        assert_eq!(quote_paths([]), "");
    }
}
