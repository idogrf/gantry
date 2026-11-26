//! The Terminal section: font, scrollback, text direction and shell.

use app_core::settings::Settings;
use app_core::theme_colors::terminal_palette;
use gpui::{App, Context, Div, Window, div, px, rgb};

use super::sessions::{SHELL_ARGS, SHELL_PROGRAM, join_args};
use super::*;
use crate::settings_store::shortcut_label;
use crate::terminal_view::FONT_FAMILY;
use crate::theme::{RADIUS, Theme};

const TERMINAL_SIZE: Stepped = Stepped {
    name: "terminal-size",
    get: |s| s.terminal_font.size,
    set: |s, value| s.terminal_font.size = value,
    delta: 1.0,
    min: 8.0,
    max: 40.0,
    unit: "",
};
const LINE_HEIGHT: Stepped = Stepped {
    name: "line-height",
    get: |s| s.terminal_font.line_height,
    set: |s, value| s.terminal_font.line_height = value,
    delta: 0.1,
    min: 1.0,
    max: 2.0,
    unit: "",
};
/// Scrollback lines are changed in thousands.
const SCROLLBACK: Stepped = Stepped {
    name: "scrollback",
    get: |s| s.window.scrollback as f32,
    set: |s, value| s.window.scrollback = value as u32,
    delta: 1000.0,
    min: 1000.0,
    max: 100_000.0,
    unit: "lines",
};

/// The families whose `m` and `i` advance the same: `(family, advance of m, advance of i)` in, sorted names out.
fn monospace_families(measured: impl IntoIterator<Item = (String, f32, f32)>) -> Vec<String> {
    let mut names: Vec<String> =
        measured.into_iter().filter(|(_, m, i)| (m - i).abs() < 0.01).map(|(name, _, _)| name).collect();
    names.sort();
    names.dedup();
    names
}

pub(super) fn installed_monospace(cx: &App) -> Vec<String> {
    let text_system = cx.text_system();
    let measured = text_system.all_font_names().into_iter().filter_map(|name| {
        let id = text_system.resolve_font(&gpui::font(name.clone()));
        let m = text_system.advance(id, px(14.), 'm').ok()?.width;
        let i = text_system.advance(id, px(14.), 'i').ok()?.width;
        Some((name, f32::from(m), f32::from(i)))
    });
    monospace_families(measured)
}

impl SettingsView {
    pub(super) fn terminal(
        &mut self,
        theme: &Theme,
        settings: &Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let font = self.font_group(theme, settings, window, cx);
        let text = self.text_group(theme, settings, window, cx);
        let shell = self.shell_group(theme, settings, window, cx);
        div().flex().flex_col().gap(theme.ui(26.)).child(font).child(text).child(shell)
    }

    fn font_group(&mut self, theme: &Theme, settings: &Settings, window: &Window, cx: &mut Context<Self>) -> Div {
        let font = &settings.terminal_font;
        let label = match &font.family {
            Some(family) => family.clone(),
            None => format!("{FONT_FAMILY} (default)"),
        };
        let mut options: Vec<(String, bool, Handler)> = vec![(
            format!("{FONT_FAMILY} (default)"),
            font.family.is_none(),
            change(|settings| settings.terminal_font.family = None),
        )];
        options.extend(self.families.iter().filter(|family| *family != FONT_FAMILY).map(|family| {
            let name = family.clone();
            (
                family.clone(),
                font.family.as_deref() == Some(family.as_str()),
                change(move |settings| settings.terminal_font.family = Some(name.clone())),
            )
        }));
        let family_control = self.dropdown("font-family", 0, Popover::Font, label, theme.ui(210.), options, window, cx);
        let family_reset = self.reset_slot(
            "reset-font-family",
            0,
            settings,
            |s| s.terminal_font.family.clone(),
            |s, v| s.terminal_font.family = v,
            window,
            cx,
        );
        let size_control = self.stepper(&TERMINAL_SIZE, settings, window, cx);
        let size_reset = self.reset_slot(
            "reset-terminal-size",
            0,
            settings,
            |s| s.terminal_font.size,
            |s, v| s.terminal_font.size = v,
            window,
            cx,
        );
        let height_control = self.stepper(&LINE_HEIGHT, settings, window, cx);
        let height_reset = self.reset_slot(
            "reset-line-height",
            0,
            settings,
            |s| s.terminal_font.line_height,
            |s, v| s.terminal_font.line_height = v,
            window,
            cx,
        );

        let palette = terminal_palette(theme.preset);
        let ok = color(palette.ansi[2]);
        let line = |parts: Vec<(String, Option<u32>)>| {
            div().flex().flex_row().whitespace_nowrap().children(
                parts
                    .into_iter()
                    .map(move |(text, ink)| div().when_some(ink, |part, ink| part.text_color(rgb(ink))).child(text)),
            )
        };
        let family = font.family.clone().unwrap_or_else(|| FONT_FAMILY.to_string());
        let sample = div()
            .w_full()
            .px(theme.ui(16.))
            .py(theme.ui(14.))
            .rounded(px(RADIUS))
            .bg(rgb(theme.tokens.terminal))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .overflow_hidden()
            .font_family(family)
            .text_size(px(font.size))
            .line_height(px(font.size * font.line_height))
            .text_color(rgb(color(palette.foreground)))
            .child(line(vec![
                ("~/rust_terminal".into(), Some(theme.tokens.accent)),
                (" $ cargo test -p app-core settings".into(), None),
            ]))
            .child(line(vec![("running 12 tests".into(), None)]))
            .child(line(vec![("test settings::missing_keys_use_defaults ... ".into(), None), ("ok".into(), Some(ok))]))
            .child(line(vec![
                ("test settings::low_contrast_fill_is_refused ... ".into(), None),
                ("ok".into(), Some(ok)),
            ]))
            .child(line(vec![
                ("test result: ".into(), None),
                ("ok".into(), Some(ok)),
                (". 12 passed; 0 failed".into(), None),
            ]));
        let size_hint = format!(
            "{} and {} also change this.",
            shortcut_label(settings, "zoomIn"),
            shortcut_label(settings, "zoomOut")
        );
        group(
            theme,
            "Font",
            None,
            vec![
                row(theme, "Terminal font", "Monospace fonts installed on this machine.", family_control, family_reset),
                row(theme, "Size", size_hint, size_control, size_reset),
                row(theme, "Line height", "Space between terminal lines.", height_control, height_reset),
                stacked_row(theme, Some(("Preview", "How terminal text looks with these settings.")), sample),
            ],
        )
    }

    fn text_group(&mut self, theme: &Theme, settings: &Settings, window: &Window, cx: &mut Context<Self>) -> Div {
        let scrollback_control = self.stepper(&SCROLLBACK, settings, window, cx);
        let scrollback_reset = self.reset_slot(
            "reset-scrollback",
            0,
            settings,
            |s| s.window.scrollback,
            |s, v| s.window.scrollback = v,
            window,
            cx,
        );
        let rtl = settings.window.right_to_left;
        let rtl_control = self.switch(
            "right-to-left",
            0,
            rtl,
            window,
            cx,
            change(move |settings| settings.window.right_to_left = !rtl),
        );
        let rtl_reset = self.reset_slot(
            "reset-right-to-left",
            0,
            settings,
            |s| s.window.right_to_left,
            |s, v| s.window.right_to_left = v,
            window,
            cx,
        );
        group(
            theme,
            "Text",
            None,
            vec![
                row(
                    theme,
                    "Scrollback",
                    "For new panes. Open panes keep theirs until restarted.",
                    scrollback_control,
                    scrollback_reset,
                ),
                row(
                    theme,
                    "Right-to-left display",
                    "Shows Hebrew and Arabic in reading order. Off: characters stay in the order the program wrote them. Also lays out right-to-left documents in the preview pane.",
                    rtl_control,
                    rtl_reset,
                ),
            ],
        )
    }

    fn shell_group(&mut self, theme: &Theme, settings: &Settings, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let program = self
            .text_field((SHELL_PROGRAM, 0), &settings.shell.program, "$SHELL", true, theme, window, cx)
            .w(theme.ui(260.));
        let program_reset = self.reset_slot(
            "reset-shell-program",
            0,
            settings,
            |s| s.shell.program.clone(),
            |s, v| s.shell.program = v,
            window,
            cx,
        );
        let args = self
            .text_field((SHELL_ARGS, 0), &join_args(&settings.shell.args), "none", true, theme, window, cx)
            .w(theme.ui(260.));
        let args_reset = self.reset_slot(
            "reset-shell-args",
            0,
            settings,
            |s| s.shell.args.clone(),
            |s, v| s.shell.args = v,
            window,
            cx,
        );
        group(
            theme,
            "Shell",
            None,
            vec![
                row(theme, "Program", "Empty: your login shell ($SHELL).", program, program_reset),
                row(theme, "Arguments", "Split at spaces. Quote a word that has spaces in it.", args, args_reset),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_steps_by_a_thousand_within_its_range() {
        let step = |value: f32, sign: f32| step(value, sign * SCROLLBACK.delta, SCROLLBACK.min, SCROLLBACK.max);
        assert_eq!(step(10_000., 1.), 11_000.);
        assert_eq!(step(1_000., -1.), 1_000.);
        assert_eq!(step(100_000., 1.), 100_000.);
        let mut settings = Settings::default();
        (SCROLLBACK.set)(&mut settings, 25_000.);
        assert_eq!((SCROLLBACK.get)(&settings), 25_000.);
        assert_eq!(settings.window.scrollback, 25_000);
    }

    #[test]
    fn only_fixed_width_families_are_listed_once_and_sorted() {
        let measured = [
            ("Noto Sans Mono".to_string(), 8.4, 8.4),
            ("Figtree".to_string(), 12.0, 4.0),
            ("Adwaita Mono".to_string(), 8.4, 8.4),
            ("Noto Sans Mono".to_string(), 8.4, 8.4),
        ];
        assert_eq!(monospace_families(measured), ["Adwaita Mono", "Noto Sans Mono"]);
    }
}
