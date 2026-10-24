//! Translate key presses into the bytes a terminal program expects.

use crate::term_state::KeyModes;

/// A key press, independent of any GUI toolkit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    /// Key name as reported by the GUI (`"enter"`, `"up"`, `"a"`, …). On non-Latin layouts this is the ASCII equivalent.
    pub key: String,
    /// The character the key would type, if any (e.g. `"ב"` on a Hebrew layout).
    pub key_char: Option<String>,
    /// Control held.
    pub ctrl: bool,
    /// Alt / Option held.
    pub alt: bool,
    /// Shift held.
    pub shift: bool,
    /// Super / Cmd held.
    pub platform: bool,
}

/// Encode a key press for the running program.
///
/// Returns `None` for keys that should arrive through the platform text-input path instead
/// (plain or shifted printable characters) and for Super/Cmd combinations, which are reserved
/// for application shortcuts.
pub fn encode_key(input: &KeyInput, modes: KeyModes) -> Option<Vec<u8>> {
    if input.platform {
        return None;
    }
    let m = modifier_param(input);
    let has_mods = m > 1;
    let kitty = modes.kitty_disambiguate;
    let bytes = match input.key.as_str() {
        "enter" if kitty && has_mods => csi_u(13, m),
        "enter" if input.shift || input.alt => b"\x1b\r".to_vec(),
        "enter" => b"\r".to_vec(),
        "tab" if kitty && has_mods => csi_u(9, m),
        "tab" if input.shift => b"\x1b[Z".to_vec(),
        "tab" => b"\t".to_vec(),
        "backspace" if kitty && has_mods => csi_u(127, m),
        "backspace" if input.ctrl => vec![0x08],
        "backspace" if input.alt => b"\x1b\x7f".to_vec(),
        "backspace" => vec![0x7f],
        "escape" if kitty => csi_u(27, m),
        "escape" => vec![0x1b],
        "up" => cursor_key('A', m, modes.app_cursor),
        "down" => cursor_key('B', m, modes.app_cursor),
        "right" => cursor_key('C', m, modes.app_cursor),
        "left" => cursor_key('D', m, modes.app_cursor),
        "home" => cursor_key('H', m, modes.app_cursor),
        "end" => cursor_key('F', m, modes.app_cursor),
        "insert" => tilde(2, m),
        "delete" => tilde(3, m),
        "pageup" => tilde(5, m),
        "pagedown" => tilde(6, m),
        "f1" => ss3_function('P', m),
        "f2" => ss3_function('Q', m),
        "f3" => ss3_function('R', m),
        "f4" => ss3_function('S', m),
        "f5" => tilde(15, m),
        "f6" => tilde(17, m),
        "f7" => tilde(18, m),
        "f8" => tilde(19, m),
        "f9" => tilde(20, m),
        "f10" => tilde(21, m),
        "f11" => tilde(23, m),
        "f12" => tilde(24, m),
        "space" => return encode_char(' ', input, kitty, m),
        name => {
            let mut chars = name.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            return encode_char(ch, input, kitty, m);
        }
    };
    Some(bytes)
}

fn encode_char(ch: char, input: &KeyInput, kitty: bool, m: u8) -> Option<Vec<u8>> {
    if !input.ctrl && !input.alt {
        return None;
    }
    let ch = ch.to_ascii_lowercase();
    if kitty {
        return Some(csi_u(ch as u32, m));
    }
    if input.ctrl {
        let code = match ch {
            'a'..='z' => ch as u8 & 0x1f,
            ' ' | '@' | '2' => 0x00,
            '[' => 0x1b,
            '\\' => 0x1c,
            ']' => 0x1d,
            '/' => 0x1f,
            _ => return None,
        };
        let mut bytes = Vec::with_capacity(2);
        if input.alt {
            bytes.push(0x1b);
        }
        bytes.push(code);
        return Some(bytes);
    }
    let text = input.key_char.clone().unwrap_or_else(|| ch.to_string());
    Some([b"\x1b".as_slice(), text.as_bytes()].concat())
}

/// xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl.
fn modifier_param(input: &KeyInput) -> u8 {
    1 + input.shift as u8 + 2 * input.alt as u8 + 4 * input.ctrl as u8
}

fn csi_u(code: u32, m: u8) -> Vec<u8> {
    if m > 1 { format!("\x1b[{code};{m}u") } else { format!("\x1b[{code}u") }.into_bytes()
}

/// The readline/Emacs bytes shells and Claude read for a Text editing action (`id` in `shortcuts::ACTIONS`), like
/// iTerm's "Natural Text Editing". `None` for the selection actions and anything else.
pub fn edit_bytes(id: &str) -> Option<&'static [u8]> {
    Some(match id {
        "lineStart" => b"\x01",
        "lineEnd" => b"\x05",
        "wordLeft" => b"\x1bb",
        "wordRight" => b"\x1bf",
        "deleteToLineStart" => b"\x15",
        "deleteToLineEnd" => b"\x0b",
        "deleteWordRight" => b"\x1bd",
        "undoEdit" => b"\x1f",
        _ => return None,
    })
}

/// `count` presses of → (← for a negative count).
pub fn arrow_presses(count: i32, app_cursor: bool) -> Vec<u8> {
    cursor_key(if count > 0 { 'C' } else { 'D' }, 1, app_cursor).repeat(count.unsigned_abs() as usize)
}

fn cursor_key(letter: char, m: u8, app_cursor: bool) -> Vec<u8> {
    if m > 1 {
        format!("\x1b[1;{m}{letter}")
    } else if app_cursor {
        format!("\x1bO{letter}")
    } else {
        format!("\x1b[{letter}")
    }
    .into_bytes()
}

fn tilde(number: u8, m: u8) -> Vec<u8> {
    if m > 1 { format!("\x1b[{number};{m}~") } else { format!("\x1b[{number}~") }.into_bytes()
}

fn ss3_function(letter: char, m: u8) -> Vec<u8> {
    if m > 1 { format!("\x1b[1;{m}{letter}") } else { format!("\x1bO{letter}") }.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY: KeyModes = KeyModes { app_cursor: false, kitty_disambiguate: false };
    const APP_CURSOR: KeyModes = KeyModes { app_cursor: true, kitty_disambiguate: false };
    const KITTY: KeyModes = KeyModes { app_cursor: false, kitty_disambiguate: true };

    fn key(name: &str, mods: &str) -> KeyInput {
        KeyInput {
            key: name.into(),
            key_char: (name.chars().count() == 1).then(|| name.to_string()),
            ctrl: mods.contains('c'),
            alt: mods.contains('a'),
            shift: mods.contains('s'),
            platform: mods.contains('p'),
        }
    }

    fn enc(name: &str, mods: &str, modes: KeyModes) -> Option<Vec<u8>> {
        encode_key(&key(name, mods), modes)
    }

    #[test]
    fn enter_variants() {
        assert_eq!(enc("enter", "", LEGACY), Some(b"\r".to_vec()));
        assert_eq!(enc("enter", "s", LEGACY), Some(b"\x1b\r".to_vec()));
        assert_eq!(enc("enter", "a", LEGACY), Some(b"\x1b\r".to_vec()));
        assert_eq!(enc("enter", "", KITTY), Some(b"\r".to_vec()));
        assert_eq!(enc("enter", "s", KITTY), Some(b"\x1b[13;2u".to_vec()));
    }

    #[test]
    fn tab_backspace_escape() {
        assert_eq!(enc("tab", "", LEGACY), Some(b"\t".to_vec()));
        assert_eq!(enc("tab", "s", LEGACY), Some(b"\x1b[Z".to_vec()));
        assert_eq!(enc("tab", "s", KITTY), Some(b"\x1b[9;2u".to_vec()));
        assert_eq!(enc("backspace", "", LEGACY), Some(vec![0x7f]));
        assert_eq!(enc("backspace", "c", LEGACY), Some(vec![0x08]));
        assert_eq!(enc("backspace", "a", LEGACY), Some(b"\x1b\x7f".to_vec()));
        assert_eq!(enc("backspace", "c", KITTY), Some(b"\x1b[127;5u".to_vec()));
        assert_eq!(enc("escape", "", LEGACY), Some(vec![0x1b]));
        assert_eq!(enc("escape", "", KITTY), Some(b"\x1b[27u".to_vec()));
    }

    #[test]
    fn arrows_and_navigation() {
        assert_eq!(enc("up", "", LEGACY), Some(b"\x1b[A".to_vec()));
        assert_eq!(enc("up", "", APP_CURSOR), Some(b"\x1bOA".to_vec()));
        assert_eq!(enc("left", "c", LEGACY), Some(b"\x1b[1;5D".to_vec()));
        assert_eq!(enc("right", "s", APP_CURSOR), Some(b"\x1b[1;2C".to_vec()));
        assert_eq!(enc("home", "", LEGACY), Some(b"\x1b[H".to_vec()));
        assert_eq!(enc("end", "", APP_CURSOR), Some(b"\x1bOF".to_vec()));
        assert_eq!(enc("pageup", "", LEGACY), Some(b"\x1b[5~".to_vec()));
        assert_eq!(enc("delete", "c", LEGACY), Some(b"\x1b[3;5~".to_vec()));
        assert_eq!(enc("f1", "", LEGACY), Some(b"\x1bOP".to_vec()));
        assert_eq!(enc("f5", "", LEGACY), Some(b"\x1b[15~".to_vec()));
        assert_eq!(enc("f12", "s", LEGACY), Some(b"\x1b[24;2~".to_vec()));
    }

    #[test]
    fn editing_actions_become_readline_bytes() {
        assert_eq!(edit_bytes("lineStart"), Some(&b"\x01"[..]));
        assert_eq!(edit_bytes("wordRight"), Some(&b"\x1bf"[..]));
        assert_eq!(edit_bytes("undoEdit"), Some(&b"\x1f"[..]));
        assert_eq!(edit_bytes("selectLine"), None, "selection is drawn here, not sent");
        let editing = crate::shortcuts::ACTIONS.iter().filter(|a| a.group == crate::shortcuts::Group::Editing);
        assert!(editing.clone().all(|a| edit_bytes(a.id).is_some() || a.id.starts_with("select")));
    }

    #[test]
    fn arrow_presses_repeat_the_cursor_key() {
        assert_eq!(arrow_presses(2, false), b"\x1b[C\x1b[C".to_vec());
        assert_eq!(arrow_presses(-1, true), b"\x1bOD".to_vec());
    }

    #[test]
    fn control_and_alt_characters() {
        assert_eq!(enc("c", "c", LEGACY), Some(vec![0x03]));
        assert_eq!(enc("r", "c", LEGACY), Some(vec![0x12]));
        assert_eq!(enc("space", "c", LEGACY), Some(vec![0x00]));
        assert_eq!(enc("[", "c", LEGACY), Some(vec![0x1b]));
        assert_eq!(enc("x", "a", LEGACY), Some(b"\x1bx".to_vec()));
        assert_eq!(enc("c", "ca", LEGACY), Some(vec![0x1b, 0x03]));
        assert_eq!(enc("c", "c", KITTY), Some(b"\x1b[99;5u".to_vec()));
        assert_eq!(enc("x", "a", KITTY), Some(b"\x1b[120;3u".to_vec()));
    }

    #[test]
    fn ctrl_on_hebrew_layout_uses_ascii_key() {
        let input = KeyInput {
            key: "c".into(),
            key_char: Some("ב".into()),
            ctrl: true,
            alt: false,
            shift: false,
            platform: false,
        };
        assert_eq!(encode_key(&input, LEGACY), Some(vec![0x03]));
    }

    #[test]
    fn text_and_platform_keys_are_not_encoded() {
        assert_eq!(enc("a", "", LEGACY), None);
        assert_eq!(enc("a", "s", LEGACY), None);
        assert_eq!(enc("space", "", LEGACY), None);
        assert_eq!(enc("c", "p", LEGACY), None);
        assert_eq!(enc("enter", "p", LEGACY), None);
    }
}
