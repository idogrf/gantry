//! What a sidebar row, tab or pane header leaves out (status and how long, config dir, dev container, folder),
//! shown after GPUI's tooltip delay (0.5 s).

use app_core::host_state::HostDot;
use app_core::session_tree::Tone;
use app_core::workspace::DirBadge;
use gpui::{
    AnyView, App, Context, FontWeight, IntoElement, Pixels, Render, TextRun, Window, black, div, font, prelude::*, px,
    rgb,
};

use crate::context_menu;
use crate::settings_store::SettingsStore;
use crate::theme::{FadeKind, RADIUS_SMALL, UI_FONT, container_icon, dir_badge, fade_in, ssh_icon, theme};

/// The mark before a title: tones without a fill need one (the fill already says the rest), and a row marked unread
/// carries a pin so it is told apart from Done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lead {
    /// No mark.
    None,
    /// A hollow ring (Closed).
    Ring,
    /// `↗` (running outside Gantry).
    Arrow,
    /// A pin (marked unread by hand).
    Pin,
}

/// The lead mark of `tone`.
pub(crate) fn lead(tone: Tone) -> Lead {
    match tone {
        Tone::Closed => Lead::Ring,
        Tone::Elsewhere => Lead::Arrow,
        Tone::Unread => Lead::Pin,
        _ => Lead::None,
    }
}

/// The card's content.
#[derive(Clone, Default)]
pub(crate) struct HoverCard {
    /// The row's title.
    pub title: String,
    /// Status and how long ("Working · 3m"), if any.
    pub status: Option<String>,
    /// Its config dir, when badges are shown.
    pub dir: Option<DirBadge>,
    /// Runs in a dev container.
    pub container: bool,
    /// The SSH host it runs on; `None` for this machine.
    pub host: Option<String>,
    /// Its folder, `~`-shortened.
    pub folder: Option<String>,
    /// The sent files line ("Files: 2 new, 3 in all"), under the status.
    pub files: Option<String>,
}

/// The width of `text` on one line.
pub(crate) fn line_width(window: &Window, text: &str, size: Pixels, weight: FontWeight) -> Pixels {
    let mut font = font(UI_FONT);
    font.weight = weight;
    let run =
        TextRun { len: text.len(), font, color: black(), background_color: None, underline: None, strikethrough: None };
    window.text_system().shape_line(text.replace('\n', " ").into(), size, &[run], None).width
}

impl HoverCard {
    /// The widest line, with the card's padding and border.
    fn natural_width(&self, window: &Window, ui: impl Fn(f32) -> Pixels) -> Pixels {
        let rem = window.rem_size();
        let (small, extra_small) = (rem * 0.875, rem * 0.75);
        let mut widest = line_width(window, &self.title, small, FontWeight::SEMIBOLD);
        for (text, extra) in self.measured_lines() {
            let size = if extra { extra_small } else { small };
            widest = widest.max(line_width(window, &text, size, FontWeight::NORMAL));
        }
        widest + ui(24.) + px(2.)
    }

    /// The lines that set the card's width besides the title, and whether each is drawn extra small.
    fn measured_lines(&self) -> Vec<(String, bool)> {
        self.status
            .iter()
            .cloned()
            .chain(self.files.iter().cloned())
            .chain(self.dir.iter().map(|badge| format!("Config: {}", badge.name)))
            .map(|text| (text, false))
            .chain(self.folder.iter().cloned().map(|text| (text, true)))
            .collect()
    }
}

impl Render for HoverCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        // A tooltip is measured without a width to wrap at, so text wider than the maximum would wrap when drawn but
        // keep the height of one line: such a card gets the maximum as its definite width.
        let max = theme.ui(320.);
        let wide = self.natural_width(window, |size| theme.ui(size)) > max;
        let card = div()
            .max_w(max)
            .when(wide, |card| card.w(max))
            .px(theme.ui(12.))
            .py(theme.ui(10.))
            .flex()
            .flex_col()
            .gap(theme.ui(4.))
            .rounded(px(RADIUS_SMALL))
            .bg(rgb(theme.tokens.raised))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .shadow_lg()
            .font_family(UI_FONT)
            .text_sm()
            .text_color(rgb(theme.tokens.text))
            .child(div().font_weight(FontWeight::SEMIBOLD).child(self.title.clone()))
            .children(self.status.clone().map(|status| div().child(status)))
            .children(self.files.clone().map(|files| div().child(files)))
            .children(self.dir.as_ref().map(|badge| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(dir_badge(badge, &theme))
                    .child(format!("Config: {}", badge.name))
            }))
            .when(self.container, |card| {
                card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(container_icon(theme.tokens.text, &theme))
                        .child("Dev container"),
                )
            })
            .children(self.host.clone().map(|host| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(ssh_icon(theme.tokens.text, &theme))
                    .child(format!("Host: {host}"))
            }))
            .children(
                self.folder.clone().map(|folder| div().text_xs().text_color(rgb(theme.tokens.muted)).child(folder)),
            );
        fade_in(card, "fade-hover-card", FadeKind::Card, cx.global::<SettingsStore>().settings().animate_ui)
    }
}

/// What the collapsed SSH hosts line leaves out: one line per host with its state.
#[derive(Clone)]
pub(crate) struct HostsCard {
    /// Each host's dot, name and state ("connected", "not connected").
    pub hosts: Vec<(HostDot, String, String)>,
}

impl Render for HostsCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        let card = div()
            .max_w(theme.ui(320.))
            .px(theme.ui(12.))
            .py(theme.ui(10.))
            .flex()
            .flex_col()
            .gap(theme.ui(4.))
            .rounded(px(RADIUS_SMALL))
            .bg(rgb(theme.tokens.raised))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .shadow_lg()
            .font_family(UI_FONT)
            .text_sm()
            .text_color(rgb(theme.tokens.text))
            .children(self.hosts.iter().map(|(dot, name, state)| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .size(px(7.))
                            .rounded_full()
                            .bg(rgb(context_menu::host_dot_color(&theme, *dot))),
                    )
                    .child(div().truncate().child(format!("{name} · {state}")))
            }));
        fade_in(card, "fade-hosts-card", FadeKind::Card, cx.global::<SettingsStore>().settings().animate_ui)
    }
}

/// A `.tooltip(…)` builder showing `card`.
///
/// While a menu is open it shows nothing: GPUI's tooltips ignore occlusion, so the card would be drawn over the menu.
pub(crate) fn tooltip<T: Render + Clone>(card: T) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_, cx| {
        if context_menu::any_open() { cx.new(|_| NoCard).into() } else { cx.new(|_| card.clone()).into() }
    }
}

/// What a tooltip shows while a menu is open.
struct NoCard;

impl Render for NoCard {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tones_without_a_fill_and_unread_keep_a_lead_mark() {
        for tone in [Tone::NeedsInput, Tone::Working, Tone::Done, Tone::Idle] {
            assert_eq!(lead(tone), Lead::None, "{tone:?}");
        }
        assert_eq!(lead(Tone::Closed), Lead::Ring);
        assert_eq!(lead(Tone::Elsewhere), Lead::Arrow);
        assert_eq!(lead(Tone::Unread), Lead::Pin);
    }

    #[test]
    fn a_files_line_widens_the_card() {
        let plain = HoverCard { title: "a".into(), ..HoverCard::default() };
        let long = HoverCard { files: Some("Files: 2 new, 3 in all".into()), ..plain.clone() };
        assert!(long.measured_lines().iter().any(|(text, _)| text == "Files: 2 new, 3 in all"));
        assert!(plain.measured_lines().iter().all(|(text, _)| !text.starts_with("Files")));
    }
}
