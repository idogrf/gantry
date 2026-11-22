//! The Appearance section: theme, status colors and fonts.

use app_core::session_tree::Tone;
use app_core::settings::{Settings, StatusFills, TRANSPARENT, ThemeChoice, fill_text};
use app_core::theme_colors::{Preset, ProjectColor, effective_fill, mix, tokens as preset_tokens, tone_colors};
use gpui::{Context, Div, FontWeight, IntoElement, Styled, Window, deferred, div, px, rgb, rgba};

use super::*;
use crate::theme::{RADIUS, RADIUS_SMALL, Theme, pin_icon};

/// A session status whose sidebar fill can be chosen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Status {
    NeedsInput,
    Working,
    Done,
    Idle,
    Unread,
}

impl Status {
    const ALL: [Status; 5] = [Status::NeedsInput, Status::Working, Status::Done, Status::Idle, Status::Unread];

    fn tone(self) -> Tone {
        match self {
            Status::NeedsInput => Tone::NeedsInput,
            Status::Working => Tone::Working,
            Status::Done => Tone::Done,
            Status::Idle => Tone::Idle,
            Status::Unread => Tone::Unread,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Status::NeedsInput => "Needs input",
            Status::Working => "Working",
            Status::Done => "Done",
            Status::Idle => "Idle",
            Status::Unread => "Unread",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Status::NeedsInput => "Claude is waiting for you.",
            Status::Working => "Claude is busy.",
            Status::Done => "Finished, and you have not looked yet.",
            Status::Idle => "Waiting quietly. A muted fill.",
            Status::Unread => "Marked unread by hand. Transparent leaves only the pin.",
        }
    }

    fn sample(self) -> &'static str {
        match self {
            Status::NeedsInput => "OPS work log triage",
            Status::Working => "Tab drag to reorder",
            Status::Done => "Right-click menus",
            Status::Idle => "Plan mode bridge hook",
            Status::Unread => "Sidebar width memory",
        }
    }

    fn fill(self, fills: &StatusFills) -> Option<u32> {
        match self {
            Status::NeedsInput => fills.needs_input,
            Status::Working => fills.working,
            Status::Done => fills.done,
            Status::Idle => fills.idle,
            Status::Unread => fills.unread,
        }
    }

    fn set(self, fills: &mut StatusFills, fill: Option<u32>) {
        match self {
            Status::NeedsInput => fills.needs_input = fill,
            Status::Working => fills.working = fill,
            Status::Done => fills.done = fill,
            Status::Idle => fills.idle = fill,
            Status::Unread => fills.unread = fill,
        }
    }

    /// The fill the preset draws when nothing is chosen (Unread's follows the Done fill in `fills`).
    fn default_fill(self, preset: Preset, fills: &StatusFills) -> u32 {
        let base = match self {
            Status::Unread => StatusFills { done: fills.done, ..StatusFills::default() },
            _ => StatusFills::default(),
        };
        tone_colors(preset, &base, self.tone()).fill.unwrap_or(preset_tokens(preset).surface)
    }

    /// The fills offered as swatches: Unread also has [`TRANSPARENT`].
    fn swatches(self) -> impl Iterator<Item = u32> {
        SUGGESTED_FILLS.into_iter().chain((self == Status::Unread).then_some(TRANSPARENT))
    }
}

const LAST_PROMPT_SIZE: Stepped = Stepped {
    name: "last-prompt-size",
    get: |s| s.last_prompt.size,
    set: |s, value| s.last_prompt.size = value,
    delta: 1.0,
    min: 11.0,
    max: 32.0,
    unit: "",
};

const INTERFACE_SIZE: Stepped = Stepped {
    name: "interface-size",
    get: |s| s.ui_font_size,
    set: |s, value| s.ui_font_size = value,
    delta: 1.0,
    min: 11.0,
    max: 20.0,
    unit: "",
};

const SUGGESTED_FILLS: [u32; 12] = [
    0xc1e5cd, 0xb7e4dd, 0xcfe8b4, 0xe6efb0, 0xa8dcc0, 0xd5ecd9, 0xf7c9a3, 0xf6b8b8, 0xe9c6ef, 0xc9cdf6, 0xb9d5f6,
    0xf1e1a6,
];

const THEME_CARDS: [(ThemeChoice, &str, &str); 3] = [
    (ThemeChoice::Mist, "Mist", "Dark slate with a light-blue accent."),
    (ThemeChoice::MistLight, "Mist light", "The same look on a light ground."),
    (ThemeChoice::System, "Follow system", "Dark or light, as the desktop is."),
];

/// The ring colors offered besides the theme's accent, each readable against the sidebar.
fn selection_choices(preset: Preset) -> [u32; 4] {
    match preset {
        Preset::Mist => [0xffffff, 0xb79cff, 0xff7eb6, 0xffe066],
        Preset::MistLight => [0x1c2333, 0x6b3fd4, 0xd4206c, 0xb45309],
    }
}

/// "3 folders have a color you picked." ("1 folder has" in the singular).
fn picked_text(count: usize) -> String {
    if count == 1 {
        "1 folder has a color you picked.".into()
    } else {
        format!("{count} folders have a color you picked.")
    }
}

/// What to store for a picked `fill`: nothing when it is the theme's own.
fn fill_to_store(fill: u32, default: u32) -> Option<u32> {
    (fill != default).then_some(fill)
}

/// Paint `element` as a swatch of `fill`: the color it shows, or no fill and a dashed `outline` for [`TRANSPARENT`].
fn swatch<E: Styled>(element: E, fill: u32, outline: u32, theme: &Theme) -> E {
    if fill == TRANSPARENT {
        element.border_dashed().border_color(rgb(outline))
    } else {
        element.bg(rgb(effective_fill(fill, &theme.tokens)))
    }
}

impl SettingsView {
    fn open_fill_picker(&mut self, status: Status, cx: &mut Context<Self>) {
        self.popover = Some(Popover::Fill(status));
        cx.notify();
    }

    fn apply_fill(&mut self, status: Status, fill: u32, cx: &mut Context<Self>) {
        let default = status.default_fill(theme(cx).preset, &cx.global::<SettingsStore>().settings().status_fills);
        SettingsStore::update(cx, |settings| status.set(&mut settings.status_fills, fill_to_store(fill, default)));
    }

    pub(super) fn appearance(
        &mut self,
        theme: &Theme,
        settings: &Settings,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(theme.ui(26.))
            .child(self.theme_cards(theme, settings, window, cx))
            .child(self.status_colors(theme, settings, window, cx))
            .child(self.interface_group(theme, settings, window, cx))
            .child(self.panes_group(theme, settings, window, cx))
            .child(self.project_colors(theme, settings, window, cx))
    }

    fn theme_cards(&mut self, theme: &Theme, settings: &Settings, window: &Window, cx: &mut Context<Self>) -> Div {
        let cards = THEME_CARDS.iter().enumerate().map(|(index, (choice, name, hint))| {
            let choice = *choice;
            let selected = settings.theme == choice;
            let preview =
                preset_tokens(if choice == ThemeChoice::MistLight { Preset::MistLight } else { Preset::Mist });
            let preset = if choice == ThemeChoice::MistLight { Preset::MistLight } else { Preset::Mist };
            let bar = |status: Status| {
                div().h(theme.ui(8.)).rounded(px(4.)).bg(rgb(status.default_fill(preset, &StatusFills::default())))
            };
            let light = preset_tokens(Preset::MistLight);
            let miniature = div()
                .h(theme.ui(74.))
                .flex()
                .gap(theme.ui(6.))
                .p(theme.ui(8.))
                .rounded(px(RADIUS_SMALL))
                .bg(rgb(preview.ground))
                .border_1()
                .border_color(rgb(preview.line))
                .child(
                    div()
                        .w(theme.ui(48.))
                        .flex()
                        .flex_col()
                        .gap(theme.ui(4.))
                        .p(theme.ui(5.))
                        .rounded(px(6.))
                        .bg(rgb(preview.surface))
                        .child(bar(Status::NeedsInput))
                        .child(bar(Status::Working))
                        .child(bar(Status::Done)),
                )
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_end()
                        .p(theme.ui(5.))
                        .rounded(px(6.))
                        .bg(rgb(preview.terminal))
                        .child(div().w(theme.ui(18.)).h(theme.ui(6.)).rounded(px(3.)).bg(rgb(preview.accent))),
                )
                .when(choice == ThemeChoice::System, |mini| {
                    mini.child(
                        div().flex_1().rounded(px(6.)).bg(rgb(light.terminal)).border_1().border_color(rgb(light.line)),
                    )
                });
            let radio = if selected { theme.tokens.accent } else { theme.tokens.muted };
            self.pressable("theme", index, None, window, cx, change(move |settings| settings.theme = choice))
                .w(theme.ui(210.))
                .p(theme.ui(10.))
                .flex()
                .flex_col()
                .gap(theme.ui(10.))
                .rounded(px(RADIUS))
                .bg(rgb(theme.tokens.raised))
                .border_2()
                .when(selected, |card| card.border_color(rgb(theme.tokens.accent)))
                .child(miniature)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.ui(8.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(
                            div()
                                .size(theme.ui(14.))
                                .rounded_full()
                                .border_2()
                                .border_color(rgb(radio))
                                .when(selected, |dot| dot.bg(rgb(theme.tokens.accent))),
                        )
                        .child(*name),
                )
                .child(div().mt(theme.ui(-6.)).child(muted(theme, *hint)))
        });
        group(
            theme,
            "Theme",
            None,
            vec![stacked_row(theme, None, div().flex().flex_wrap().gap(theme.ui(14.)).children(cards))],
        )
    }

    fn status_colors(&mut self, theme: &Theme, settings: &Settings, window: &Window, cx: &mut Context<Self>) -> Div {
        let mut rows: Vec<Div> = Status::ALL
            .map(|status| {
                let index = status as usize;
                let chosen = status.fill(&settings.status_fills);
                let colors = theme.tone(status.tone());
                let shown = chosen.unwrap_or_else(|| status.default_fill(theme.preset, &settings.status_fills));
                let open = self.popover == Some(Popover::Fill(status));
                let sample = div()
                    .w(theme.ui(250.))
                    .flex_none()
                    .h(theme.ui(34.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(px(RADIUS))
                    .bg(rgb(colors.fill.unwrap_or(theme.tokens.surface)))
                    .text_color(rgb(colors.ink))
                    .map(|sample| match status {
                        Status::Unread => sample.child(pin_icon(colors.dot, theme)),
                        _ => sample.child(div().flex_none().size(theme.ui(8.)).rounded_full().bg(rgb(colors.dot))),
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when(status == Status::NeedsInput, |el| el.font_weight(FontWeight::SEMIBOLD))
                            .child(status.sample()),
                    )
                    .child(
                        div().flex_none().text_xs().text_color(rgba((colors.ink << 8) | 0xcc)).child(status.label()),
                    );
                let open_picker = handler(move |this, _, cx| this.open_fill_picker(status, cx));
                let picker_button = self
                    .pressable(
                        "status-color",
                        index,
                        Some(if open { theme.tokens.accent } else { theme.tokens.line }),
                        window,
                        cx,
                        open_picker,
                    )
                    .h(theme.ui(34.))
                    .pl(theme.ui(6.))
                    .pr(theme.ui(12.))
                    .flex()
                    .items_center()
                    .gap(theme.ui(8.))
                    .rounded(px(RADIUS_SMALL))
                    .bg(rgb(theme.tokens.raised))
                    .child(
                        swatch(div().border_1().border_color(rgb(theme.tokens.line)), shown, theme.tokens.muted, theme)
                            .size(theme.ui(22.))
                            .rounded(px(6.)),
                    )
                    .child(fill_text(shown));
                let picker = open.then(|| self.fill_picker(status, theme, window, cx));
                let reset = self.reset_button(
                    "status-reset",
                    index,
                    chosen.is_some(),
                    change(move |settings| status.set(&mut settings.status_fills, None)),
                    window,
                    cx,
                );
                let control = div()
                    .flex()
                    .items_center()
                    .gap(theme.ui(10.))
                    .child(sample)
                    .child(div().relative().flex_none().child(picker_button).children(picker));
                row(theme, status.label(), status.description(), control, reset)
            })
            .into_iter()
            .collect();
        let animate = settings.animate_working;
        let toggle = self.switch(
            "animate-working",
            0,
            animate,
            window,
            cx,
            change(move |settings| settings.animate_working = !animate),
        );
        let reset = self.reset_slot(
            "reset-animate-working",
            0,
            settings,
            |s| s.animate_working,
            |s, v| s.animate_working = v,
            window,
            cx,
        );
        rows.push(row(
            theme,
            "Animate working sessions",
            "A slow glow crosses Working rows, tabs and pane headers. Paused while Gantry is not focused.",
            toggle,
            reset,
        ));
        group(theme, "Status colors", Some("Pick a fill for each status. Text and dot colors follow it."), rows)
    }

    fn interface_group(&mut self, theme: &Theme, settings: &Settings, window: &Window, cx: &mut Context<Self>) -> Div {
        let control = self.stepper(&INTERFACE_SIZE, settings, window, cx);
        let reset = self.reset_slot(
            "reset-interface-size",
            0,
            settings,
            |s| s.ui_font_size,
            |s, v| s.ui_font_size = v,
            window,
            cx,
        );
        let swatches =
            [None].into_iter().chain(selection_choices(theme.preset).map(Some)).enumerate().map(|(index, choice)| {
                let shown = choice.unwrap_or(theme.tokens.accent);
                let pick = change(move |settings| settings.selection = choice);
                self.pressable(
                    "selection-swatch",
                    index,
                    (settings.selection == choice).then_some(theme.tokens.text),
                    window,
                    cx,
                    pick,
                )
                .size(theme.ui(28.))
                .rounded(px(RADIUS_SMALL))
                .bg(rgb(shown))
                .border_2()
            });
        let swatches = div().flex().gap(theme.ui(8.)).children(swatches.collect::<Vec<_>>());
        let selection_reset =
            self.reset_slot("reset-selection", 0, settings, |s| s.selection, |s, v| s.selection = v, window, cx);
        let animate = settings.animate_ui;
        let animate_toggle =
            self.switch("animate-ui", 0, animate, window, cx, change(move |settings| settings.animate_ui = !animate));
        let animate_reset =
            self.reset_slot("reset-animate-ui", 0, settings, |s| s.animate_ui, |s, v| s.animate_ui = v, window, cx);
        group(
            theme,
            "Interface",
            None,
            vec![
                row(theme, "Interface size", "Sidebar, tabs and menus.", control, reset),
                row(
                    theme,
                    "Animate panes and pop-ups",
                    "Splits slide in, and menus, overlays and cards fade in. Off shows every change at once.",
                    animate_toggle,
                    animate_reset,
                ),
                row(
                    theme,
                    "Selection ring",
                    "The ring around the selected row. The first swatch is the theme's accent.",
                    swatches,
                    selection_reset,
                ),
            ],
        )
    }

    fn panes_group(&mut self, theme: &Theme, settings: &Settings, window: &Window, cx: &mut Context<Self>) -> Div {
        let shown = settings.last_prompt.enabled;
        let prompt_switch = self.switch(
            "last-prompt",
            0,
            shown,
            window,
            cx,
            change(move |settings| settings.last_prompt.enabled = !shown),
        );
        let prompt_reset = self.reset_slot(
            "reset-last-prompt",
            0,
            settings,
            |s| s.last_prompt.enabled,
            |s, v| s.last_prompt.enabled = v,
            window,
            cx,
        );

        let size_control = self.stepper(&LAST_PROMPT_SIZE, settings, window, cx);
        let size_reset = self.reset_slot(
            "reset-last-prompt-size",
            0,
            settings,
            |s| s.last_prompt.size,
            |s, v| s.last_prompt.size = v,
            window,
            cx,
        );
        let swatches = [None]
            .into_iter()
            .chain(ProjectColor::ALL.map(|color| Some(color.value(theme.preset))))
            .enumerate()
            .map(|(index, choice)| {
                let shown = mix(choice.unwrap_or(theme.tokens.accent), theme.tokens.surface, 0.5);
                let pick = change(move |settings| settings.last_prompt.color = choice);
                self.pressable(
                    "last-prompt-swatch",
                    index,
                    (settings.last_prompt.color == choice).then_some(theme.tokens.text),
                    window,
                    cx,
                    pick,
                )
                .size(theme.ui(28.))
                .rounded(px(RADIUS_SMALL))
                .bg(rgb(shown))
                .border_2()
            })
            .collect::<Vec<_>>();
        let swatches = div().flex().gap(theme.ui(8.)).children(swatches);
        let color_reset = self.reset_slot(
            "reset-last-prompt-color",
            0,
            settings,
            |s| s.last_prompt.color,
            |s, v| s.last_prompt.color = v,
            window,
            cx,
        );
        let claude_bar = settings.scrollbar.claude;
        let claude_bar_switch = self.switch(
            "claude-scrollbar",
            0,
            claude_bar,
            window,
            cx,
            change(move |settings| settings.scrollbar.claude = !claude_bar),
        );
        let claude_bar_reset = self.reset_slot(
            "reset-claude-scrollbar",
            0,
            settings,
            |s| s.scrollbar.claude,
            |s, v| s.scrollbar.claude = v,
            window,
            cx,
        );
        let claude_bar_label = row_label(
            "Scrollbar in Claude panes",
            div()
                .flex()
                .flex_col()
                .gap(theme.ui(4.))
                .child(muted(theme, "Terminal panes always have a scrollbar. This adds one to Claude panes."))
                .child(muted(
                    theme,
                    "Note: Claude's fullscreen mode doesn't allow external scrollbars, so this only works in classic mode. To use it, run /tui default in Claude.",
                )),
        );
        group(
            theme,
            "Panes",
            None,
            vec![
                row(
                    theme,
                    "Last prompt",
                    "Show your last prompt in a card above each Claude pane.",
                    prompt_switch,
                    prompt_reset,
                ),
                row(theme, "Prompt text size", "Two lines at most; the card grows with it.", size_control, size_reset),
                row(
                    theme,
                    "Prompt background",
                    "A tint of this color. The first swatch is the theme's accent.",
                    swatches,
                    color_reset,
                ),
                row_shell(theme, claude_bar_label, claude_bar_switch, claude_bar_reset),
            ],
        )
    }

    fn project_colors(&mut self, theme: &Theme, settings: &Settings, window: &Window, cx: &mut Context<Self>) -> Div {
        let enabled = settings.project_colors.enabled;
        let switch = self.switch(
            "project-colors",
            0,
            enabled,
            window,
            cx,
            change(move |settings| settings.project_colors.enabled = !enabled),
        );
        let switch_reset = self.reset_slot(
            "reset-project-colors",
            0,
            settings,
            |s| s.project_colors.enabled,
            |s, v| s.project_colors.enabled = v,
            window,
            cx,
        );
        let strip = |preset: Preset| {
            div()
                .flex()
                .gap(theme.ui(4.))
                .p(theme.ui(6.))
                .rounded(px(RADIUS_SMALL))
                .bg(rgb(preset_tokens(preset).ground))
                .border_1()
                .border_color(rgb(theme.tokens.line))
                .children(ProjectColor::ALL.map(|color| {
                    div().flex_none().size(theme.ui(18.)).rounded(px(RADIUS_SMALL - 4.)).bg(rgb(color.value(preset)))
                }))
        };
        let palette = div()
            .flex()
            .flex_wrap()
            .justify_end()
            .gap(theme.ui(8.))
            .child(strip(Preset::Mist))
            .child(strip(Preset::MistLight));
        let picked = settings.project_colors.folders.len();
        let mut rows = vec![
            row(
                theme,
                "Project colors",
                "Each project gets a color in the sidebar, on its tabs and in recent sessions. Change one from the project's right-click menu.",
                switch,
                switch_reset,
            ),
            row(theme, "Palette", "Mist, then Mist light.", palette, empty_slot(theme)),
        ];
        if picked > 0 {
            let forget = self.button(
                "forget-colors",
                0,
                "Forget all",
                window,
                cx,
                change(|settings| settings.project_colors.folders.clear()),
            );
            rows.push(row(theme, "Picked colors", picked_text(picked), forget, empty_slot(theme)));
        }
        group(theme, "Project colors", None, rows)
    }

    fn fill_picker(
        &mut self,
        status: Status,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let fills = cx.global::<SettingsStore>().settings().status_fills;
        let current = status.fill(&fills).unwrap_or_else(|| status.default_fill(theme.preset, &fills));
        let swatches = status
            .swatches()
            .enumerate()
            .map(|(index, fill)| {
                let pick = handler(move |this, _, cx| {
                    this.apply_fill(status, fill, cx);
                    cx.notify();
                });
                let outline = if fill == current { theme.tokens.text } else { theme.tokens.muted };
                swatch(self.pressable("swatch", index, None, window, cx, pick), fill, outline, theme)
                    .h(theme.ui(30.))
                    .rounded(px(RADIUS_SMALL))
                    .border_2()
                    .when(fill == current, |swatch| swatch.border_color(rgb(theme.tokens.text)))
            })
            .collect::<Vec<_>>();
        let custom = handler(move |this, window, cx| {
            this.close_popover(window, cx);
            cx.emit(SettingsEvent::CustomColor(status.tone()));
        });
        let custom = self.button("custom-fill", 0, "Custom color…", window, cx, custom);
        deferred(
            div()
                .occlude()
                .absolute()
                .top(theme.ui(40.))
                .right_0()
                .w(theme.ui(272.))
                .p(theme.ui(14.))
                .flex()
                .flex_col()
                .gap(theme.ui(12.))
                .rounded(px(RADIUS))
                .bg(rgb(theme.tokens.raised))
                .border_1()
                .border_color(rgb(theme.tokens.line))
                .shadow_lg()
                .child(div().grid().grid_cols(6).gap(theme.ui(8.)).children(swatches))
                .child(custom),
        )
        .with_priority(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picked_colors_text_is_singular_for_one_folder() {
        assert_eq!(picked_text(1), "1 folder has a color you picked.");
        assert_eq!(picked_text(3), "3 folders have a color you picked.");
    }

    #[test]
    fn the_themes_own_fill_is_stored_as_nothing() {
        assert_eq!(fill_to_store(0xc1e5cd, 0xc1e5cd), None);
        assert_eq!(fill_to_store(0xb7e4dd, 0xc1e5cd), Some(0xb7e4dd));
        assert_eq!(
            Status::Done.default_fill(Preset::Mist, &StatusFills::default()),
            tone_colors(Preset::Mist, &StatusFills::default(), Tone::Done).fill.unwrap()
        );
    }

    #[test]
    fn only_unread_offers_transparent() {
        assert!(Status::Unread.swatches().any(|fill| fill == TRANSPARENT));
        assert!(!Status::Done.swatches().any(|fill| fill == TRANSPARENT));
    }

    #[test]
    fn unread_default_follows_a_custom_done_fill() {
        let fills = StatusFills { done: Some(0xffe08a), ..StatusFills::default() };
        assert_eq!(
            Status::Unread.default_fill(Preset::Mist, &fills),
            tone_colors(Preset::Mist, &fills, Tone::Unread).fill.unwrap()
        );
    }
}
