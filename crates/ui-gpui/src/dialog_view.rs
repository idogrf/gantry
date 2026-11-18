//! The shared confirmation dialog (an overlay): draws one [`DialogSpec`], moves the focus between its choices and
//! reports the one chosen. What each answer does is up to the window that opened it.

use app_core::dialogs::{ChoiceStyle, DialogSpec, Layout, next_focus};
use app_core::session_tree::Tone;
use app_core::text_field::TextField;
use app_core::theme_colors::mix;
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyDownEvent, Render, Stateful, Window, div, prelude::*, px, rgb, rgba,
};

use crate::field;
use crate::theme::{FadeKind, RADIUS_LARGE, RADIUS_SMALL, Theme, fade_in, theme};

/// What the dialog tells the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DialogEvent {
    /// The user picked `spec.actions[index]` (Esc picks `spec.escape`). Sent once.
    Chosen(usize),
}

/// One dialog on screen.
pub(crate) struct DialogView {
    focus_handle: FocusHandle,
    spec: DialogSpec,
    focus: usize,
    answered: bool,
    /// The text field, when the spec has one.
    input: Option<TextField>,
}

impl EventEmitter<DialogEvent> for DialogView {}

impl Focusable for DialogView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl DialogView {
    /// The dialog for `spec`, focus on its initial-focus choice.
    pub(crate) fn new(spec: DialogSpec, cx: &mut Context<Self>) -> Self {
        let focus = spec.actions.iter().position(|action| action.initial_focus).unwrap_or(0);
        let input = spec.input.as_ref().map(|input| {
            let mut field = TextField::default();
            field.insert(&input.text);
            field.select_all();
            field
        });
        DialogView { focus_handle: cx.focus_handle(), spec, focus, answered: false, input }
    }

    /// The text field's text, when the dialog has one.
    pub(crate) fn input_text(&self) -> Option<String> {
        self.input.as_ref().map(|field| field.text().to_string())
    }

    /// Report `index` (once; later input is ignored).
    fn choose(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.answered {
            return;
        }
        self.answered = true;
        cx.emit(DialogEvent::Chosen(index));
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.answered {
            return;
        }
        let keystroke = &event.keystroke;
        let len = self.spec.actions.len();
        match (keystroke.key.as_str(), keystroke.modifiers.shift) {
            ("escape", _) => self.choose(self.spec.escape, cx),
            ("enter", _) => self.choose(self.focus, cx),
            ("tab", false) => self.focus = next_focus(self.focus, len, true),
            ("tab", true) => self.focus = next_focus(self.focus, len, false),
            // With a text field, the other keys (arrows included) edit it.
            _ if self.input.is_some() => {
                let Some(input) = &mut self.input else { return };
                if !field::edit(input, keystroke, cx) {
                    return;
                }
            }
            ("down", _) | ("right", _) => self.focus = next_focus(self.focus, len, true),
            ("up", _) | ("left", _) => self.focus = next_focus(self.focus, len, false),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }
}

/// A choice's frame: the focus ring (2 px, accent; the text color on an accent fill) around `fill`.
fn frame(id: usize, focused: bool, on_accent: bool, theme: &Theme) -> Stateful<Div> {
    div().id(("dialog-action", id)).cursor_pointer().rounded(px(RADIUS_SMALL)).border_2().border_color(rgb(
        match (focused, on_accent) {
            (false, _) => theme.tokens.surface,
            (true, true) => theme.tokens.text,
            (true, false) => theme.tokens.accent,
        },
    ))
}

fn label_color(style: ChoiceStyle, pills: bool, theme: &Theme) -> u32 {
    match style {
        ChoiceStyle::Danger => theme.tokens.danger,
        ChoiceStyle::Primary if pills => theme.tokens.on_accent,
        ChoiceStyle::Cancel => theme.tokens.muted,
        _ => theme.tokens.text,
    }
}

fn fill(style: ChoiceStyle, pills: bool, theme: &Theme) -> u32 {
    match style {
        ChoiceStyle::Primary if pills => theme.tokens.accent,
        ChoiceStyle::Primary => mix(theme.tokens.accent, theme.tokens.surface, 0.18),
        _ => theme.tokens.raised,
    }
}

impl Render for DialogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        let focused = self.focus_handle.is_focused(window);
        let input = self.input.as_ref().map(|input| {
            let press = field::on_press(cx, |this: &mut Self, offset, clicks, _, cx| {
                if let Some(input) = &mut this.input {
                    input.press(offset, clicks);
                }
                cx.notify();
            });
            let placeholder = self.spec.input.as_ref().map(|spec| spec.placeholder.clone()).unwrap_or_default();
            div()
                .h(theme.ui(32.))
                .px(theme.ui(10.))
                .flex()
                .items_center()
                .rounded(px(RADIUS_SMALL))
                .border_1()
                .border_color(rgb(theme.tokens.accent))
                .bg(rgb(theme.tokens.ground))
                .child(field::render(input, placeholder, focused, &theme, press).flex_1())
        });
        let pills = self.spec.layout == Layout::Pills;
        let mut choices = div().flex().gap(theme.ui(8.));
        choices = if pills { choices.flex_row().flex_wrap().justify_end() } else { choices.flex_col() };
        for (index, action) in self.spec.actions.iter().enumerate() {
            let focused = index == self.focus;
            let style = action.style;
            let click = cx.listener(move |this, _: &ClickEvent, _, cx| this.choose(index, cx));
            let label = div()
                .font_weight(if style == ChoiceStyle::Cancel { FontWeight::NORMAL } else { FontWeight::SEMIBOLD })
                .text_color(rgb(label_color(style, pills, &theme)))
                .child(action.label.clone());
            let item = if style == ChoiceStyle::Cancel && !pills {
                frame(index, focused, false, &theme).py(theme.ui(6.)).flex().justify_center().child(label)
            } else if pills {
                frame(index, focused, style == ChoiceStyle::Primary, &theme)
                    .px(theme.ui(14.))
                    .py(theme.ui(7.))
                    .bg(rgb(fill(style, pills, &theme)))
                    .child(label)
            } else {
                frame(index, focused, false, &theme)
                    .px(theme.ui(12.))
                    .py(theme.ui(9.))
                    .bg(rgb(fill(style, pills, &theme)))
                    .flex()
                    .flex_col()
                    .gap(theme.ui(2.))
                    .child(label)
                    .children(
                        action
                            .description
                            .clone()
                            .map(|text| div().text_xs().text_color(rgb(theme.tokens.muted)).child(text)),
                    )
            };
            choices = choices.child(item.on_click(click));
        }

        div()
            .key_context("Dialog")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .w(theme.ui(340.))
            .max_w_full()
            .flex()
            .flex_col()
            .gap(theme.ui(12.))
            .p(theme.ui(22.))
            .bg(rgb(theme.tokens.surface))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .rounded(px(RADIUS_LARGE))
            .shadow_lg()
            .text_sm()
            .text_color(rgb(theme.tokens.text))
            .child(div().text_size(theme.ui(16.)).font_weight(FontWeight::SEMIBOLD).child(self.spec.title.clone()))
            .when(!self.spec.chips.is_empty(), |card| {
                card.child(div().flex().flex_row().flex_wrap().gap(theme.ui(6.)).children(self.spec.chips.iter().map(
                    |chip| {
                        div()
                            .px(theme.ui(8.))
                            .py(theme.ui(2.))
                            .rounded(px(RADIUS_SMALL))
                            .bg(rgb(theme.tokens.raised))
                            .text_xs()
                            .text_color(rgb(theme.tokens.muted))
                            .child(chip.clone())
                    },
                )))
            })
            .children(
                self.spec
                    .warning
                    .clone()
                    .map(|text| div().text_xs().text_color(rgb(theme.tone(Tone::NeedsInput).dot)).child(text)),
            )
            .children(
                self.spec.detail.clone().map(|text| div().text_xs().text_color(rgb(theme.tokens.muted)).child(text)),
            )
            .children(input)
            .child(choices)
    }
}

/// `view` centred over the window on a scrim: the card fades in with a rise, the scrim fades in, and a click outside
/// the card does nothing (a stray click must not cancel a quit).
pub(crate) fn dialog_overlay(view: Entity<DialogView>, theme: &Theme, animate: bool) -> AnyElement {
    let scrim = rgba((theme.tokens.ground << 8) | 0xcc);
    div()
        .id("dialog-overlay")
        .occlude()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(theme.ui(16.))
        .when(!animate, |overlay| overlay.bg(scrim))
        .when(animate, |overlay| {
            overlay.child(fade_in(
                div().absolute().top_0().left_0().size_full().bg(scrim),
                "fade-dialog-scrim",
                FadeKind::Scrim,
                true,
            ))
        })
        .child(fade_in(div().child(view), "fade-dialog-card", FadeKind::Card, animate))
        .into_any_element()
}
