//! The Shortcuts section: every rebindable action with its keystroke, a recorder, and the "Replace?" and "Use anyway?"
//! cards.

use app_core::settings::Settings;
use app_core::shortcuts::{self, ACTIONS, Action, Check, Group, Platform};
use gpui::{Context, Div, FontWeight, Keystroke, Window, div, px, rgb, rgba};

use super::*;
use crate::hover_card::{self, HoverCard};
use crate::settings_store::SettingsStore;
use crate::theme::{RADIUS, RADIUS_SMALL, Theme};

const GROUPS: [Group; 6] = [Group::Tabs, Group::Panes, Group::Focus, Group::Overlays, Group::Terminal, Group::Editing];

/// A keystroke waiting for the user's yes: another action has it (`other` is that action), or programs in the
/// terminal use it (`other` is why).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Replace {
    id: &'static str,
    keystroke: String,
    other: &'static str,
}

/// What a keystroke pressed while recording means.
#[derive(Debug, PartialEq, Eq)]
enum Recorded {
    /// A bare modifier press: keep waiting for the full chord.
    Ignore,
    /// Esc.
    Cancel,
    /// Backspace: the action gets no shortcut.
    Unset,
    /// A keystroke to check.
    Choose(String),
}

/// The keystroke in settings syntax (`ctrl-alt-shift-cmd-key`).
fn keystroke_text(keystroke: &Keystroke) -> Option<String> {
    let modifiers = &keystroke.modifiers;
    let mut parts: Vec<&str> =
        [(modifiers.control, "ctrl"), (modifiers.alt, "alt"), (modifiers.shift, "shift"), (modifiers.platform, "cmd")]
            .into_iter()
            .filter_map(|(on, name)| on.then_some(name))
            .collect();
    if keystroke.key.is_empty() {
        return None;
    }
    parts.push(&keystroke.key);
    Some(parts.join("-"))
}

fn recorded(keystroke: &Keystroke) -> Recorded {
    let modifiers = &keystroke.modifiers;
    if matches!(keystroke.key.as_str(), "shift" | "control" | "alt" | "platform" | "function") {
        return Recorded::Ignore;
    }
    if !(modifiers.control || modifiers.alt || modifiers.shift || modifiers.platform) {
        return match keystroke.key.as_str() {
            "escape" => Recorded::Cancel,
            "backspace" => Recorded::Unset,
            // A plain key is checked too, so the user is told why it cannot be used.
            key if !key.is_empty() => Recorded::Choose(key.to_string()),
            _ => Recorded::Ignore,
        };
    }
    keystroke_text(keystroke).map_or(Recorded::Ignore, Recorded::Choose)
}

/// Store `value` for `action`: nothing when it is the default (only changes are written), `null` for no shortcut.
fn store_shortcut(settings: &mut Settings, action: &str, value: Option<String>, platform: Platform) {
    match value {
        Some(keystroke) if shortcuts::is_default(action, &keystroke, platform) => {
            settings.shortcuts.remove(action);
        }
        value => {
            settings.shortcuts.insert(action.to_string(), value);
        }
    }
}

/// What the key button of `action` shows.
fn shown(settings: &Settings, action: &Action, platform: Platform) -> String {
    shortcuts::label(shortcuts::current(action.id, &settings.shortcuts, platform).as_deref(), platform)
}

/// The `!` badge of a key taken from the terminal, in the Needs input colors.
fn warn_mark(theme: &Theme) -> Div {
    let colors = theme.tone(Tone::NeedsInput);
    div()
        .flex_none()
        .size(theme.ui(15.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgb(colors.fill.unwrap_or(colors.dot)))
        .text_color(rgb(colors.ink))
        .text_size(theme.ui(10.))
        .font_weight(FontWeight::BOLD)
        .child("!")
}

fn label_of(id: &str) -> &'static str {
    ACTIONS.iter().find(|action| action.id == id).map_or("", |action| action.label)
}

impl SettingsView {
    /// Recording ends when its key button loses focus (a click elsewhere, another tab).
    pub(super) fn end_recording_without_focus(&mut self, window: &Window) {
        let Some(id) = self.recording else { return };
        let index = ACTIONS.iter().position(|action| action.id == id).unwrap_or_default();
        if !self.stops.get(&("shortcut", index)).is_some_and(|handle| handle.is_focused(window)) {
            self.recording = None;
        }
    }

    /// Handle a keystroke while a shortcut is being recorded; false (and not consumed) when none is.
    pub(super) fn record(&mut self, keystroke: &Keystroke, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.end_recording_without_focus(window);
        let Some(id) = self.recording else { return false };
        match recorded(keystroke) {
            Recorded::Ignore => {}
            Recorded::Cancel => self.recording = None,
            Recorded::Unset => {
                self.recording = None;
                SettingsStore::update(cx, |settings| store_shortcut(settings, id, None, Platform::current()));
            }
            Recorded::Choose(text) => self.choose(id, text, cx),
        }
        if self.recording.is_none() {
            self.focus_handle.focus(window, cx);
        }
        cx.notify();
        true
    }

    fn choose(&mut self, id: &'static str, text: String, cx: &mut Context<Self>) {
        let platform = Platform::current();
        let current = shortcuts::bindings(&cx.global::<SettingsStore>().settings().shortcuts, platform);
        match shortcuts::check(&text, id, &current) {
            Check::Ok => {
                self.recording = None;
                SettingsStore::update(cx, |settings| store_shortcut(settings, id, Some(text), platform));
            }
            Check::Reserved(reason) => {
                self.recording = None;
                self.pending_take = Some(Replace { id, keystroke: text, other: reason });
            }
            Check::Conflict(other) => {
                self.recording = None;
                self.pending_replace = Some(Replace { id, keystroke: text, other });
            }
            Check::Invalid => {
                self.shortcut_message = Some((id, "Not a usable shortcut. Press other keys, or Esc.".into()))
            }
        }
    }

    pub(super) fn shortcuts(
        &mut self,
        theme: &Theme,
        settings: &Settings,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let platform = Platform::current();
        let filter = self.shortcut_filter.text().trim().to_lowercase();

        let mut groups: Vec<Div> = Vec::new();
        for shortcut_group in GROUPS {
            let rows: Vec<Div> = ACTIONS
                .iter()
                .enumerate()
                .filter(|(_, action)| action.group == shortcut_group)
                .filter(|(_, action)| {
                    filter.is_empty()
                        || action.label.to_lowercase().contains(&filter)
                        || shown(settings, action, platform).to_lowercase().contains(&filter)
                })
                .map(|(index, action)| self.shortcut_row(index, action, theme, settings, platform, window, cx))
                .collect();
            if !rows.is_empty() {
                groups.push(group(theme, shortcut_group.title(), None, rows));
            }
        }
        let body = if groups.is_empty() {
            muted(theme, "No shortcut matches.")
        } else {
            div().flex().flex_col().gap(theme.ui(26.)).children(groups)
        };

        let focused = self.filter_focus.is_focused(window);
        let filter_box = div()
            .id("shortcut-filter")
            .track_focus(&self.filter_focus)
            .tab_stop(true)
            .w(theme.ui(260.))
            .max_w_full()
            .h(theme.ui(34.))
            .px(theme.ui(12.))
            .flex()
            .items_center()
            .rounded(px(RADIUS_SMALL))
            .bg(rgb(theme.tokens.raised))
            .border_1()
            .border_color(rgb(if focused { theme.tokens.accent } else { theme.tokens.line }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if field::edit(&mut this.shortcut_filter, &event.keystroke, cx) {
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| this.filter_focus.focus(window, cx)))
            .child(field::render(
                &self.shortcut_filter,
                "Filter shortcuts",
                focused,
                theme,
                field::on_press(cx, |this, offset, clicks, _, cx| {
                    this.shortcut_filter.press(offset, clicks);
                    cx.notify();
                }),
            ));

        div().relative().flex().flex_col().gap(theme.ui(20.)).child(filter_box).child(body)
    }

    #[allow(clippy::too_many_arguments)]
    fn shortcut_row(
        &mut self,
        index: usize,
        action: &'static Action,
        theme: &Theme,
        settings: &Settings,
        platform: Platform,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = action.id;
        let recording = self.recording == Some(id);
        let unset = shortcuts::current(id, &settings.shortcuts, platform).is_none();
        let changed = settings.shortcuts.contains_key(id);

        let record = handler(move |this, _, cx| {
            this.recording = Some(id);
            this.shortcut_message = None;
            this.pending_replace = None;
            this.pending_take = None;
            cx.notify();
        });
        // A key taken from the terminal is marked: amber outline and `!`.
        let taken = !recording
            && shortcuts::current(id, &settings.shortcuts, platform)
                .is_some_and(|keystroke| shortcuts::takes_from_terminal(&keystroke));
        let warn = theme.tone(Tone::NeedsInput).dot;
        let key_text = if recording { "Press keys…".to_string() } else { shown(settings, action, platform) };
        let key_ink = if recording {
            theme.tokens.accent
        } else if unset {
            theme.tokens.muted
        } else {
            theme.tokens.text
        };
        let key = self
            .pressable(
                "shortcut",
                index,
                Some(if recording {
                    theme.tokens.accent
                } else if taken {
                    warn
                } else {
                    theme.tokens.line
                }),
                window,
                cx,
                record,
            )
            .h(theme.ui(28.))
            .min_w(theme.ui(120.))
            .px(theme.ui(10.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(RADIUS_SMALL))
            .bg(if recording {
                rgba((theme.tokens.accent << 8) | 0x33)
            } else {
                rgba((theme.tokens.raised << 8) | 0xff)
            })
            .text_size(theme.ui(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(key_ink))
            .whitespace_nowrap()
            .gap(theme.ui(6.))
            .when(taken, |key| {
                let card = HoverCard {
                    title: "Taken from the terminal".into(),
                    status: Some("Programs there no longer get it.".into()),
                    dir: None,
                    container: false,
                    host: None,
                    folder: None,
                };
                key.tooltip(hover_card::tooltip(card)).child(warn_mark(theme))
            })
            .child(key_text);
        let reset = self.reset_button(
            "shortcut-reset",
            index,
            changed,
            change(move |settings| {
                settings.shortcuts.remove(id);
            }),
            window,
            cx,
        );

        let row = row_shell(theme, div().font_weight(FontWeight::MEDIUM).child(action.label), key, reset);

        let message = self.shortcut_message.as_ref().filter(|(message_id, _)| *message_id == id).map(|(_, text)| {
            let colors = theme.tone(Tone::NeedsInput);
            div()
                .mx(theme.ui(16.))
                .mb(theme.ui(12.))
                .px(theme.ui(10.))
                .py(theme.ui(6.))
                .rounded(px(RADIUS_SMALL))
                .bg(rgb(colors.fill.unwrap_or(theme.tokens.raised)))
                .text_color(rgb(colors.ink))
                .text_sm()
                .child(text.clone())
        });
        let replace = self.pending_replace.clone().filter(|pending| pending.id == id).map(|pending| {
            let shown = shortcuts::label(Some(&pending.keystroke), platform);
            let other = label_of(pending.other);
            let cancel = handler(|this, _, cx| {
                this.pending_replace = None;
                cx.notify();
            });
            let confirm = handler(move |this, _, cx| {
                let Some(pending) = this.pending_replace.take() else { return };
                SettingsStore::update(cx, |settings| {
                    store_shortcut(settings, pending.other, None, platform);
                    store_shortcut(settings, pending.id, Some(pending.keystroke.clone()), platform);
                });
                cx.notify();
            });
            let cancel = self.button("shortcut-cancel", index, "Cancel", window, cx, cancel);
            let confirm = self
                .pressable("shortcut-replace", index, None, window, cx, confirm)
                .h(theme.ui(34.))
                .px(theme.ui(14.))
                .flex()
                .items_center()
                .rounded(px(RADIUS_SMALL))
                .bg(rgb(theme.tokens.accent))
                .text_color(rgb(theme.tokens.on_accent))
                .font_weight(FontWeight::SEMIBOLD)
                .text_sm()
                .child("Replace");
            div()
                .mx(theme.ui(16.))
                .mb(theme.ui(12.))
                .p(theme.ui(14.))
                .flex()
                .flex_col()
                .gap(theme.ui(10.))
                .rounded(px(RADIUS))
                .bg(rgb(theme.tokens.raised))
                .border_1()
                .border_color(rgb(theme.tokens.line))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("{shown} is used by “{other}”.")))
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(theme.tokens.muted))
                        .child(format!("Replace it? “{other}” will have no shortcut until you set one.")),
                )
                .child(div().flex().flex_wrap().justify_end().gap(theme.ui(8.)).child(cancel).child(confirm))
        });
        let take = self.pending_take.clone().filter(|pending| pending.id == id).map(|pending| {
            let shown = shortcuts::label(Some(&pending.keystroke), platform);
            let cancel = handler(|this, _, cx| {
                this.pending_take = None;
                cx.notify();
            });
            let confirm = handler(move |this, _, cx| {
                let Some(pending) = this.pending_take.take() else { return };
                let current = shortcuts::bindings(&cx.global::<SettingsStore>().settings().shortcuts, platform);
                match shortcuts::conflict(&pending.keystroke, pending.id, &current) {
                    Some(other) => this.pending_replace = Some(Replace { other, ..pending }),
                    None => SettingsStore::update(cx, |settings| {
                        store_shortcut(settings, pending.id, Some(pending.keystroke.clone()), platform)
                    }),
                }
                cx.notify();
            });
            let cancel = self.button("shortcut-take-cancel", index, "Cancel", window, cx, cancel);
            let confirm = self
                .pressable("shortcut-take", index, None, window, cx, confirm)
                .h(theme.ui(34.))
                .px(theme.ui(14.))
                .flex()
                .items_center()
                .rounded(px(RADIUS_SMALL))
                .bg(rgb(theme.tokens.accent))
                .text_color(rgb(theme.tokens.on_accent))
                .font_weight(FontWeight::SEMIBOLD)
                .text_sm()
                .child("Use anyway");
            div()
                .mx(theme.ui(16.))
                .mb(theme.ui(12.))
                .p(theme.ui(14.))
                .flex()
                .flex_col()
                .gap(theme.ui(10.))
                .rounded(px(RADIUS))
                .bg(rgb(theme.tokens.raised))
                .border_1()
                .border_color(rgb(warn))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.ui(8.))
                        .child(warn_mark(theme))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("{shown} {}.", pending.other))),
                )
                .child(div().text_sm().text_color(rgb(theme.tokens.muted)).child(format!(
                    "Use it anyway? Programs in the terminal (your shell, Claude, vim…) will no longer receive {shown}."
                )))
                .child(div().flex().flex_wrap().justify_end().gap(theme.ui(8.)).child(cancel).child(confirm))
        });
        div().flex().flex_col().child(row).children(message).children(replace).children(take)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn key(key: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke { modifiers, key: key.into(), key_char: None }
    }

    fn ctrl_shift() -> Modifiers {
        Modifiers { control: true, shift: true, ..Default::default() }
    }

    #[test]
    fn a_pressed_chord_becomes_settings_syntax() {
        assert_eq!(keystroke_text(&key("d", ctrl_shift())).as_deref(), Some("ctrl-shift-d"));
        let all = Modifiers { control: true, alt: true, shift: true, platform: true, ..Default::default() };
        assert_eq!(keystroke_text(&key("pageup", all)).as_deref(), Some("ctrl-alt-shift-cmd-pageup"));
    }

    #[test]
    fn plain_keys_are_checked_except_escape_and_backspace() {
        let none = Modifiers::default();
        assert_eq!(recorded(&key("escape", none)), Recorded::Cancel);
        assert_eq!(recorded(&key("backspace", none)), Recorded::Unset);
        assert_eq!(recorded(&key("enter", none)), Recorded::Choose("enter".into()));
        assert_eq!(recorded(&key("a", none)), Recorded::Choose("a".into()));
        assert_eq!(recorded(&key("shift", Modifiers { shift: true, ..Default::default() })), Recorded::Ignore);
        assert_eq!(recorded(&key("control", Modifiers { control: true, ..Default::default() })), Recorded::Ignore);
        assert_eq!(recorded(&key("d", ctrl_shift())), Recorded::Choose("ctrl-shift-d".into()));
        assert_eq!(recorded(&key("escape", ctrl_shift())), Recorded::Choose("ctrl-shift-escape".into()));
    }

    #[test]
    fn storing_the_default_removes_the_override_and_unset_writes_null() {
        let mut settings = Settings::default();
        store_shortcut(&mut settings, "newShellTab", Some("ctrl-alt-shift-t".into()), Platform::Linux);
        assert!(settings.shortcuts.is_empty(), "it is the default");
        store_shortcut(&mut settings, "toggleSidebar", Some("ctrl-alt-s".into()), Platform::Linux);
        assert_eq!(settings.shortcuts["toggleSidebar"].as_deref(), Some("ctrl-alt-s"));
        store_shortcut(&mut settings, "toggleSidebar", Some("ctrl-shift-b".into()), Platform::Linux);
        assert!(settings.shortcuts.is_empty(), "back to the default");
        store_shortcut(&mut settings, "closePane", None, Platform::Linux);
        assert_eq!(settings.shortcuts["closePane"], None);
    }

    #[test]
    fn the_key_button_shows_the_label() {
        let settings = Settings::default();
        let by_id = |id: &str| ACTIONS.iter().find(|action| action.id == id).unwrap();
        assert_eq!(shown(&settings, by_id("newClaudeTab"), Platform::Linux), "Ctrl+Shift+T");
        let mut settings = Settings::default();
        settings.shortcuts.insert("closePane".into(), None);
        assert_eq!(shown(&settings, by_id("closePane"), Platform::Linux), "Not set");
    }
}
