//! Drawing a `TextField` and applying editing keys and mouse presses to it.

use app_core::text_field::TextField;
use gpui::{
    App, ClipboardItem, Context, DispatchPhase, Div, Keystroke, MouseButton, MouseDownEvent, SharedString, Window,
    canvas, div, prelude::*, px, rgb, rgba,
};

use crate::theme::Theme;

/// What a field does with a left press: the byte offset under the pointer and the click count.
pub(crate) type Press = Box<dyn Fn(usize, usize, &mut Window, &mut App)>;

/// A `Press` that runs `handler` on the view `cx` belongs to.
pub(crate) fn on_press<V: 'static>(
    cx: &mut Context<V>,
    handler: impl Fn(&mut V, usize, usize, &mut Window, &mut Context<V>) + 'static,
) -> Press {
    let entity = cx.entity().downgrade();
    let handler = std::rc::Rc::new(handler);
    Box::new(move |offset, clicks, window, cx| {
        let handler = handler.clone();
        entity.update(cx, |this, cx| handler(this, offset, clicks, window, cx)).ok();
    })
}

/// Apply an editing key to `field`: typing, Backspace, Delete, arrows, Home/End, word movement and deletion (Ctrl, Alt on
/// macOS), Shift to select, and Ctrl+A/C/X/V (Cmd on macOS). False for anything else.
pub(crate) fn edit(field: &mut TextField, keystroke: &Keystroke, cx: &mut App) -> bool {
    if command(keystroke) && !keystroke.modifiers.shift {
        match keystroke.key.as_str() {
            "c" | "x" => {
                if let Some(text) = field.selected_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                    if keystroke.key == "x" {
                        field.backspace();
                    }
                }
                return true;
            }
            "v" => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    field.insert(&text);
                }
                return true;
            }
            _ => {}
        }
    }
    edit_keys(field, keystroke)
}

/// How a key reaches the field of a masked prompt (a password): `None` for Copy and Cut, which would put the hidden
/// text on the clipboard; word movement and word deletion become plain moves and deletes (the bullets have no words);
/// anything else as it is.
pub(crate) fn masked_key(keystroke: &Keystroke) -> Option<Keystroke> {
    if command(keystroke) && !keystroke.modifiers.shift && matches!(keystroke.key.as_str(), "c" | "x") {
        return None;
    }
    let words = ["backspace", "delete", "left", "right", "home", "end"];
    if word(keystroke) && words.contains(&keystroke.key.as_str()) {
        let mut plain = keystroke.clone();
        plain.modifiers.control = false;
        plain.modifiers.alt = false;
        plain.modifiers.platform = false;
        return Some(plain);
    }
    Some(keystroke.clone())
}

/// `edit` for a masked prompt: see [`masked_key`]. A key that is ignored still counts as handled.
pub(crate) fn edit_masked(field: &mut TextField, keystroke: &Keystroke, cx: &mut App) -> bool {
    match masked_key(keystroke) {
        Some(keystroke) => edit(field, &keystroke, cx),
        None => true,
    }
}

/// Ctrl (Cmd on macOS) held alone, or with Shift.
fn command(keystroke: &Keystroke) -> bool {
    let m = &keystroke.modifiers;
    if cfg!(target_os = "macos") { m.platform && !m.control && !m.alt } else { m.control && !m.platform && !m.alt }
}

/// Ctrl (Alt on macOS) held alone, or with Shift: the word modifier.
fn word(keystroke: &Keystroke) -> bool {
    let m = &keystroke.modifiers;
    if cfg!(target_os = "macos") { m.alt && !m.control && !m.platform } else { command(keystroke) }
}

fn edit_keys(field: &mut TextField, keystroke: &Keystroke) -> bool {
    let m = &keystroke.modifiers;
    if m.function {
        return false;
    }
    let plain = !(m.control || m.alt || m.platform);
    let (word, select) = (word(keystroke), m.shift);
    match keystroke.key.as_str() {
        "a" if command(keystroke) && !select => field.select_all(),
        "backspace" if plain && !select => field.backspace(),
        "backspace" if word && !select => field.delete_word_back(),
        "delete" if plain && !select => field.delete(),
        "delete" if word && !select => field.delete_word_forward(),
        "left" if plain => field.left(select),
        "left" if word => field.word_left(select),
        "right" if plain => field.right(select),
        "right" if word => field.word_right(select),
        "home" if plain || word => field.home(select),
        "end" if plain || word => field.end(select),
        _ if plain => match &keystroke.key_char {
            Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => field.insert(text),
            _ => return false,
        },
        _ => return false,
    }
    true
}

/// The field's text with a caret at the cursor when `focused` and the selection highlighted; `placeholder` while empty.
/// `press` runs on a left press inside the field.
pub(crate) fn render(
    field: &TextField,
    placeholder: impl Into<SharedString>,
    focused: bool,
    theme: &Theme,
    press: Press,
) -> Div {
    let caret = || div().flex_none().w(px(1.)).h(theme.ui(14.)).bg(rgb(theme.tokens.accent));
    let row = div()
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .child(press_area(field.text(), press));
    if field.text().is_empty() {
        return row
            .when(focused, |row| row.child(caret()))
            .child(div().text_color(rgb(theme.tokens.muted)).child(placeholder.into()));
    }
    let text = field.text();
    let range = field.selection().unwrap_or(field.cursor()..field.cursor());
    let (start, end) = (range.start, range.end);
    row.child(text[..start].to_string())
        .when(focused && field.cursor() == start, |row| row.child(caret()))
        .when(start < end, |row| {
            row.child(div().bg(rgba((theme.tokens.accent << 8) | 0x55)).child(text[start..end].to_string()))
        })
        .when(focused && start < end && field.cursor() == end, |row| row.child(caret()))
        .child(text[end..].to_string())
}

/// An invisible layer over the text that turns a left press into a byte offset (measured with the inherited text style).
fn press_area(text: &str, press: Press) -> impl IntoElement {
    let text = text.to_string();
    canvas(
        |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            let style = window.text_style();
            let size = style.font_size.to_pixels(window.rem_size());
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !bounds.contains(&event.position)
                {
                    return;
                }
                let line =
                    window.text_system().shape_line(text.clone().into(), size, &[style.to_run(text.len())], None);
                press(line.closest_index_for_x(event.position.x - bounds.left()), event.click_count, window, cx);
            });
        },
    )
    .absolute()
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn key(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke { modifiers, key: key.into(), key_char: key_char.map(Into::into) }
    }

    fn ctrl() -> Modifiers {
        if cfg!(target_os = "macos") {
            Modifiers { alt: true, ..Default::default() }
        } else {
            Modifiers { control: true, ..Default::default() }
        }
    }

    fn command() -> Modifiers {
        if cfg!(target_os = "macos") {
            Modifiers { platform: true, ..Default::default() }
        } else {
            Modifiers { control: true, ..Default::default() }
        }
    }

    #[test]
    fn typing_and_editing_keys_change_the_field() {
        let mut field = TextField::default();
        let plain = Modifiers::default();
        let shift = Modifiers { shift: true, ..Default::default() };
        for keystroke in [
            key("a", Some("a"), plain),
            key("b", Some("B"), shift),
            key("ש", Some("ש"), plain),
            key("space", Some(" "), plain),
        ] {
            assert!(edit_keys(&mut field, &keystroke));
        }
        assert_eq!(field.text(), "aBש ");
        for name in ["backspace", "left", "left", "delete", "home", "end"] {
            assert!(edit_keys(&mut field, &key(name, None, plain)), "{name}");
        }
        assert_eq!(field.text(), "aש");
    }

    #[test]
    fn word_keys_delete_and_move_by_word() {
        let mut field = TextField::default();
        field.insert("one two three");
        assert!(edit_keys(&mut field, &key("backspace", None, ctrl())));
        assert_eq!(field.text(), "one two ");
        assert!(edit_keys(&mut field, &key("left", None, ctrl())));
        assert!(edit_keys(&mut field, &key("delete", None, ctrl())));
        assert_eq!(field.text(), "one  ");
        let shift_word = Modifiers { shift: true, ..ctrl() };
        assert!(edit_keys(&mut field, &key("left", None, shift_word)));
        assert_eq!(field.selected_text(), Some("one "));
        let shift = Modifiers { shift: true, ..Default::default() };
        assert!(edit_keys(&mut field, &key("end", None, shift)));
        assert_eq!(field.selected_text(), Some(" "));
    }

    #[test]
    fn select_all_replaces_on_typing() {
        let mut field = TextField::default();
        field.insert("abc");
        assert!(edit_keys(&mut field, &key("a", Some("a"), command())));
        assert_eq!(field.selected_text(), Some("abc"));
        assert!(edit_keys(&mut field, &key("z", Some("z"), Modifiers::default())));
        assert_eq!(field.text(), "z");
    }

    #[test]
    fn chords_and_non_editing_keys_are_left_alone() {
        let mut field = TextField::default();
        let alt = Modifiers { alt: true, ..Default::default() };
        assert!(!edit_keys(&mut field, &key("o", Some("o"), command())));
        assert!(!edit_keys(&mut field, &key("1", Some("1"), alt)));
        for name in ["enter", "escape", "up", "down", "tab"] {
            assert!(!edit_keys(&mut field, &key(name, None, Modifiers::default())), "{name}");
        }
        assert_eq!(field.text(), "");
    }

    fn parsed(text: &str) -> Keystroke {
        Keystroke::parse(text).unwrap()
    }

    #[test]
    fn a_masked_field_never_copies_or_cuts_and_words_become_plain_moves() {
        let modifier = if cfg!(target_os = "macos") { "cmd" } else { "ctrl" };
        assert_eq!(masked_key(&parsed(&format!("{modifier}-c"))), None);
        assert_eq!(masked_key(&parsed(&format!("{modifier}-x"))), None);
        assert!(masked_key(&parsed(&format!("{modifier}-v"))).is_some(), "paste is fine");
        let word = if cfg!(target_os = "macos") { "alt" } else { "ctrl" };
        let moved = masked_key(&parsed(&format!("{word}-backspace"))).unwrap();
        let m = moved.modifiers;
        assert_eq!(moved.key, "backspace");
        assert!(!m.control && !m.alt && !m.platform, "a plain Backspace, not a word delete");
        assert_eq!(masked_key(&parsed("a")), Some(parsed("a")));
    }
}
