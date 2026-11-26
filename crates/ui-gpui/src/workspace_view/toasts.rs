//! The in-app pop-up stack (bottom right): feeding it from alerts, its one timer, drawing and clicks.
//! The queue itself is `app_core::toasts`; the decisions here are small pure helpers.

use std::time::{Duration, Instant};

use app_core::attention::{Alert, AlertKind, describe, file_sent_heading};
use app_core::preview::file_name;
use app_core::session_tree::{Tone, session_title};
use app_core::split_tree::NodeId;
use app_core::toasts::{FADE_OUT_MS, Toast};
use app_core::workspace::DropFile;
use gpui::{
    Animation, AnimationExt, AnyElement, App, AppContext, Context, FontWeight, InteractiveElement, IntoElement,
    ParentElement, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px, rgb, svg,
};

use super::WorkspaceView;
use crate::assets::{CLOSE_ICON, FILE_ICON};
use crate::pane_drag::DraggedFile;
use crate::preview_view::bidi_label;
use crate::settings_store::SettingsStore;
use crate::theme::{FadeKind, RADIUS, RADIUS_SMALL, Theme, fade_in};

/// What a click on a pop-up does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Click {
    /// Open (or focus) the session's preview on the newest file.
    Preview,
    /// Focus the session's pane.
    GoToSession,
}

/// The line naming the newest file (the last of `names`) and counting the rest.
fn file_line(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_string(),
        [.., newest] => format!("{newest} and {} more", names.len() - 1),
    }
}

/// Whether the Go to session button shows: only while the Claude pane is not on screen.
fn shows_go_to(visible: bool) -> bool {
    !visible
}

/// A click on the card's body: a file pop-up previews, a status pop-up focuses the pane.
fn body_click(kind: &AlertKind) -> Click {
    if matches!(kind, AlertKind::FileSent { .. }) { Click::Preview } else { Click::GoToSession }
}

/// Whether a following preview of the session on screen hides this alert: only a file alert is hidden by it.
fn preview_in_view(kind: &AlertKind, following_in_view: impl FnOnce() -> bool) -> bool {
    matches!(kind, AlertKind::FileSent { .. }) && following_in_view()
}

/// How long the timer sleeps for `next_wake`: never zero (no spin), none when nothing counts down.
fn wake_delay(next_wake: Option<u64>) -> Option<Duration> {
    next_wake.map(|ms| Duration::from_millis(ms.max(1)))
}

impl WorkspaceView {
    /// Whether a following preview of `alert`'s session is on screen (the only thing that hides a File sent alert).
    pub(super) fn alert_preview_in_view(&self, alert: &Alert) -> bool {
        preview_in_view(&alert.kind, || self.workspace.following_preview_in_view(&alert.pane.host, &alert.session))
    }

    /// Milliseconds since the stack's clock started.
    fn now_ms(&self) -> u64 {
        u64::try_from(Instant::now().saturating_duration_since(self.toast_epoch).as_millis()).unwrap_or(u64::MAX)
    }

    /// Shows `alert` as a pop-up on the pane that raised it.
    pub(super) fn push_toast(&mut self, leaf: NodeId, alert: Alert, window: &Window, cx: &mut Context<Self>) {
        let now = self.now_ms();
        self.toasts.push(leaf, alert.pane.host, alert.session, alert.kind, now, window.is_window_active());
        self.reschedule_toasts(window, cx);
        cx.notify();
    }

    /// Runs one timer to the stack's next change; none while nothing counts down.
    pub(super) fn reschedule_toasts(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.toast_timer = wake_delay(self.toasts.next_wake(self.now_ms())).map(|delay| {
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(delay).await;
                let _ = this.update_in(cx, |view, window, cx| {
                    let fade = if cx.global::<SettingsStore>().settings().animate_ui { FADE_OUT_MS } else { 0 };
                    let now = view.now_ms();
                    if view.toasts.tick(now, fade) {
                        cx.notify();
                    }
                    view.reschedule_toasts(window, cx);
                });
            })
        });
    }

    /// The window became active or inactive: the pop-ups' time runs only while it is active.
    pub(super) fn toasts_running(&mut self, active: bool, window: &Window, cx: &mut Context<Self>) {
        let now = self.now_ms();
        self.toasts.set_running(active, now);
        self.reschedule_toasts(window, cx);
        cx.notify();
    }

    /// A pop-up was clicked: close it and do what `click` says.
    fn click_toast(&mut self, id: u64, click: Click, window: &mut Window, cx: &mut Context<Self>) {
        let Some(toast) = self.toasts.shown().iter().find(|toast| toast.id == id).cloned() else { return };
        self.toasts.dismiss(id);
        self.reschedule_toasts(window, cx);
        match (click, &toast.kind) {
            (Click::Preview, AlertKind::FileSent { files }) => {
                if let Some(newest) = files.last() {
                    self.preview_file_for(toast.leaf, &newest.id, window, cx);
                }
            }
            _ => self.focus_leaf(toast.leaf, window, cx),
        }
        cx.notify();
    }

    /// The pop-up stack, bottom right: the newest card nearest the corner.
    pub(super) fn render_toasts(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let shown = self.toasts.shown();
        if shown.is_empty() {
            return None;
        }
        let animate = cx.global::<SettingsStore>().settings().animate_ui;
        Some(
            div()
                .absolute()
                .right(theme.ui(16.))
                .bottom(theme.ui(16.))
                .w(theme.ui(340.))
                .flex()
                .flex_col()
                .gap(theme.ui(8.))
                .children(shown.iter().map(|toast| self.render_toast(toast, animate, theme, cx)))
                .into_any_element(),
        )
    }

    fn render_toast(&self, toast: &Toast, animate: bool, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let rtl = cx.global::<SettingsStore>().settings().window.right_to_left;
        let sidebar = self.sidebar.read(cx);
        let info = sidebar.sessions().get(&toast.host, &toast.session);
        let title = info.map(|info| session_title(info).to_string()).unwrap_or_default();
        let project = info.map(|info| {
            let gantry = self.workspace.leaf_project(toast.leaf).map(String::from);
            let root = sidebar.project_of(&toast.host, &info.cwd, gantry.as_deref());
            sidebar.project_label(&root).unwrap_or_else(|| file_name(&root))
        });
        let place = match project {
            Some(project) if !project.is_empty() => format!("{project} › {title}"),
            _ => title.clone(),
        };
        let visible = self.workspace.is_visible(toast.leaf);
        let (heading, lines): (String, Vec<AnyElement>) = match &toast.kind {
            AlertKind::FileSent { files } => {
                let names: Vec<String> = files.iter().map(|file| file_name(&file.path)).collect();
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                (
                    file_sent_heading(files.len()),
                    [
                        (!place.is_empty()).then(|| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme.tokens.muted))
                                .child(bidi_label(&place, rtl))
                                .into_any_element()
                        }),
                        Some(
                            div()
                                .flex()
                                .items_center()
                                .gap(theme.ui(6.))
                                .text_sm()
                                .child(div().min_w_0().truncate().child(bidi_label(&file_line(&names), rtl)))
                                .into_any_element(),
                        ),
                    ]
                    .into_iter()
                    .flatten()
                    .collect(),
                )
            }
            kind => {
                let (summary, body) = describe(kind, &title, info.and_then(|info| info.last_prompt.as_deref()));
                let summary = summary.split(" · ").next().unwrap_or_default().to_string();
                let tone = if matches!(kind, AlertKind::NeedsInput { .. }) { Tone::NeedsInput } else { Tone::Done };
                let mut lines = Vec::new();
                if !place.is_empty() {
                    lines.push(
                        div()
                            .text_xs()
                            .text_color(rgb(theme.tokens.muted))
                            .child(bidi_label(&place, rtl))
                            .into_any_element(),
                    );
                }
                if !body.is_empty() {
                    lines.push(
                        div()
                            .text_sm()
                            .text_color(rgb(theme.tokens.muted))
                            .child(bidi_label(&body, rtl))
                            .into_any_element(),
                    );
                }
                let dot = div().flex_none().size(theme.ui(8.)).rounded_full().bg(rgb(theme.tone(tone).dot));
                return self.toast_card(toast, dot.into_any_element(), summary, lines, visible, animate, theme, cx);
            }
        };
        let icon = svg().path(FILE_ICON).flex_none().size(theme.ui(14.)).text_color(rgb(theme.tokens.accent));
        self.toast_card(toast, icon.into_any_element(), heading, lines, visible, animate, theme, cx)
    }

    #[allow(clippy::too_many_arguments)]
    fn toast_card(
        &self,
        toast: &Toast,
        mark: AnyElement,
        heading: String,
        lines: Vec<AnyElement>,
        visible: bool,
        animate: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = toast.id;
        let is_file = matches!(toast.kind, AlertKind::FileSent { .. });
        let dragged = match &toast.kind {
            AlertKind::FileSent { files } => files.last().map(|file| DraggedFile {
                file: DropFile { host: toast.host.clone(), session: Some(toast.session.clone()), file: file.clone() },
                name: file_name(&file.path),
            }),
            _ => None,
        };
        let button = |name: &'static str, label: &'static str, primary: bool, click: Click| {
            div()
                .id((name, id))
                .h(theme.ui(24.))
                .px(theme.ui(10.))
                .flex()
                .items_center()
                .rounded(px(RADIUS_SMALL))
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .when(primary, |b| b.bg(rgb(theme.tokens.accent)).text_color(rgb(theme.tokens.on_accent)))
                .when(!primary, |b| {
                    b.border_1().border_color(rgb(theme.tokens.line)).text_color(rgb(theme.tokens.text))
                })
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.click_toast(id, click, window, cx);
                }))
        };
        let card = div()
            .id(("toast-card", id))
            .occlude()
            .w_full()
            .flex()
            .flex_col()
            .gap(theme.ui(6.))
            .p(theme.ui(12.))
            .rounded(px(RADIUS))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .bg(rgb(theme.tokens.raised))
            .text_color(rgb(theme.tokens.text))
            .cursor_pointer()
            // A File sent card drags its newest file (a copy: the card stays).
            .when_some(dragged, |card, dragged| {
                card.on_drag(dragged, |dragged: &DraggedFile, _offset, _window, cx: &mut App| {
                    cx.new(|_| dragged.clone())
                })
            })
            .on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
                let now = this.now_ms();
                this.toasts.set_hovered(id, *hovered, now);
                this.reschedule_toasts(window, cx);
            }))
            .on_click(cx.listener({
                let click = body_click(&toast.kind);
                move |this, _, window, cx| this.click_toast(id, click, window, cx)
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.ui(8.))
                    .child(mark)
                    .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(heading))
                    .child(
                        div()
                            .id(("toast-close", id))
                            .flex_none()
                            .size(theme.ui(18.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(RADIUS_SMALL))
                            .cursor_pointer()
                            .child(svg().path(CLOSE_ICON).size(theme.ui(10.)).text_color(rgb(theme.tokens.muted)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.toasts.dismiss(id);
                                this.reschedule_toasts(window, cx);
                                cx.notify();
                            })),
                    ),
            )
            .children(lines)
            .child(
                div()
                    .flex()
                    .gap(theme.ui(6.))
                    .mt(theme.ui(2.))
                    .when(is_file, |row| row.child(button("toast-preview", "Preview", true, Click::Preview)))
                    .when(!is_file || shows_go_to(visible), |row| {
                        row.child(button("toast-go", "Go to session", !is_file, Click::GoToSession))
                    }),
            );
        if toast.leaving {
            card.with_animation(
                ("toast-out", id),
                Animation::new(Duration::from_millis(FADE_OUT_MS)),
                |card, progress| card.opacity(1. - progress),
            )
            .into_any_element()
        } else {
            fade_in(card, ("toast", id), FadeKind::Card, animate)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::message::SentFile;

    #[test]
    fn file_line_names_the_newest_and_counts_the_rest() {
        assert_eq!(file_line(&["a.md"]), "a.md");
        assert_eq!(file_line(&["a.md", "b.png", "c.txt"]), "c.txt and 2 more");
    }

    #[test]
    fn go_to_session_shows_only_off_screen() {
        assert!(shows_go_to(false));
        assert!(!shows_go_to(true));
    }

    #[test]
    fn a_click_targets_the_preview_for_files_and_the_pane_for_status() {
        let file = AlertKind::FileSent {
            files: vec![SentFile { id: "a".into(), path: "/r/a.md".into(), sent_ms: 0, caption: None, backlog: false }],
        };
        assert_eq!(body_click(&file), Click::Preview);
        assert_eq!(body_click(&AlertKind::Finished), Click::GoToSession);
        assert_eq!(body_click(&AlertKind::NeedsInput { reason: None }), Click::GoToSession);
    }

    #[test]
    fn only_a_file_alert_is_hidden_by_a_preview_in_view() {
        let file = AlertKind::FileSent { files: Vec::new() };
        assert!(preview_in_view(&file, || true));
        assert!(!preview_in_view(&file, || false));
        // A status alert never asks.
        assert!(!preview_in_view(&AlertKind::Finished, || panic!("not asked")));
    }

    #[test]
    fn the_timer_sleeps_to_the_next_change_and_never_spins() {
        assert_eq!(wake_delay(None), None);
        assert_eq!(wake_delay(Some(6000)), Some(Duration::from_millis(6000)));
        assert_eq!(wake_delay(Some(0)), Some(Duration::from_millis(1)));
    }
}
