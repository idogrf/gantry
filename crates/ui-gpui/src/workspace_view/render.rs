use super::layout::pane_progress;
use super::{CardKind, HostUpdateCard, PANE_GAP, SIDEBAR_EDGE, WorkspaceView};

use app_core::host_update::ask_note;
use app_core::version::{Note, Version};
use gpui::{
    Context, CursorStyle, DragMoveEvent, FontWeight, HighlightStyle, IntoElement, MouseButton, MouseDownEvent, Render,
    StyledText, Window, canvas, div, prelude::*, px, rgb, rgba,
};

use app_core::menus::MenuAction;
use app_core::session_tree::Tone;

use crate::color_picker_view::ColorPickerEvent;
use crate::context_menu::{self};
use crate::dialog_view::dialog_overlay;
use crate::pane_drag::DraggedPane;
use crate::settings_store::SettingsStore;
use crate::theme::{FadeKind, RADIUS, RADIUS_SMALL, Theme, UI_FONT, fade_in, theme};
use crate::window_frame;

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let animate = cx.global::<SettingsStore>().settings().animate_working && window.is_window_active();
        let theme = theme(cx).clone();
        let viewport = window.viewport_size();
        // The pane drag is over once gpui has no drag.
        self.pane_dragging &= cx.has_active_drag();
        let fade = cx.global::<SettingsStore>().settings().animate_ui;
        // A split or close moves only while animations are on and the window is active; else it is at its end.
        if !(fade && window.is_window_active()) {
            self.cancel_pane_anim();
        }
        match self.pane_anim.as_ref().map(|anim| pane_progress(anim.started.elapsed()) >= 1.0) {
            Some(true) => self.cancel_pane_anim(),
            Some(false) => window.request_animation_frame(),
            None => {}
        }
        match self.panel_anim.as_ref().map(|anim| pane_progress(anim.started.elapsed()) >= 1.0) {
            Some(true) => self.panel_anim = None,
            Some(false) => window.request_animation_frame(),
            None => {}
        }
        // The panel is a fifth of the width, at most 290 px, never more than 45% of the tab.
        let content_width = f32::from(self.content_bounds.size.width);
        self.panel_full = if content_width > 0.0 { f32::from(theme.ui(290.)).min(content_width * 0.45) } else { 0.0 };
        let scrim = rgba((theme.tokens.ground << 8) | 0xcc);
        // The interface scales with `uiFontSize`; GPUI's rem is 16 px at the default of 14.
        window.set_rem_size(px(16.0 * theme.ui_scale));
        let sidebar = self.workspace.sidebar.clone();
        let body = div()
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .flex_row()
            .when(!sidebar.compact, |body| {
                body.child(
                    div()
                        .relative()
                        .flex_none()
                        .h_full()
                        .w(theme.ui(sidebar.width))
                        .child({
                            let view = cx.entity();
                            canvas(
                                move |bounds, _, cx| view.update(cx, |this, _| this.sidebar_bounds = bounds),
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full()
                        })
                        .child(self.sidebar.clone()),
                )
                .child(
                    // A hairline, grabbed through a wider invisible handle that overlaps the sidebar.
                    div().relative().flex_none().w(px(SIDEBAR_EDGE)).h_full().bg(rgb(theme.tokens.line)).child(
                        div()
                            .id("sidebar-edge")
                            .absolute()
                            .top_0()
                            .left(px(-5.))
                            .w(px(7.))
                            .h_full()
                            .cursor(CursorStyle::ResizeLeftRight)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.resizing_sidebar = true;
                                    cx.stop_propagation();
                                }),
                            ),
                    ),
                )
            })
            .child(div().flex_1().min_w_0().h_full().flex().flex_col().p(px(PANE_GAP / 2.)).child(
                if self.git_page.is_some() {
                    self.render_git_page(window, cx).into_any_element()
                } else {
                    self.render_active_tab(animate, cx).into_any_element()
                },
            ));
        let root = div()
            // Without the context no shortcut reaches the panes while a dialog is open.
            .when(self.dialog.is_none(), |root| root.key_context("Workspace"))
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_claude_tab))
            .on_action(cx.listener(Self::new_shell_tab))
            .on_action(cx.listener(Self::split_right_claude))
            .on_action(cx.listener(Self::split_right_shell))
            .on_action(cx.listener(Self::split_down_claude))
            .on_action(cx.listener(Self::split_down_shell))
            .on_action(cx.listener(Self::close_pane))
            .on_action(cx.listener(Self::focus_left))
            .on_action(cx.listener(Self::focus_right))
            .on_action(cx.listener(Self::focus_up))
            .on_action(cx.listener(Self::focus_down))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::history_back))
            .on_action(cx.listener(Self::history_forward))
            // The mouse side buttons, anywhere in the window (over a pane too: the panes never see them).
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                if let MouseButton::Navigate(direction) = event.button
                    && this.dialog.is_none()
                {
                    cx.stop_propagation();
                    this.history(if direction == gpui::NavigationDirection::Back { -1 } else { 1 }, window, cx);
                }
            }))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_git_panel))
            .on_action(cx.listener(Self::find_session))
            .on_action(cx.listener(Self::open_launcher))
            .on_action(cx.listener(Self::open_recents))
            .on_action(cx.listener(Self::open_settings))
            // Esc cancels a drag, and no pane or the sidebar sees it.
            .capture_key_down(cx.listener(|_, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && cx.has_active_drag() {
                    cx.stop_active_drag(window);
                    cx.stop_propagation();
                }
            }))
            .on_mouse_move(cx.listener(Self::drag_sidebar))
            // Only a pane drag reports here; a file dragged in from the OS is a drag too, but not this type.
            .on_drag_move(cx.listener(|this, _: &DragMoveEvent<DraggedPane>, _, cx| {
                if !std::mem::replace(&mut this.pane_dragging, true) {
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if std::mem::take(&mut this.resizing_sidebar) {
                        this.save(cx);
                    }
                }),
            )
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme.tokens.ground))
            .text_color(rgb(theme.tokens.text))
            .font_family(UI_FONT)
            .child(self.render_title_bar(window, cx))
            .child(body)
            .when_some(self.launcher.as_ref().map(|(view, _)| view.clone()), |root, view| {
                root.child(
                    div()
                        .id("launcher-overlay")
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .pt(theme.ui(72.))
                        .child(view),
                )
            })
            .when_some(self.recents.as_ref().map(|(view, _)| view.clone()), |root, view| {
                root.child(
                    div()
                        .id("recents-overlay")
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .pt(theme.ui(72.))
                        .child(view),
                )
            })
            .when_some(self.setup.as_ref().map(|(view, _)| view.clone()), |root, view| {
                root.child(
                    div()
                        .id("setup-overlay")
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(theme.ui(16.))
                        .when(!fade, |overlay| overlay.bg(scrim))
                        .when(fade, |overlay| {
                            overlay.child(fade_in(
                                div().absolute().top_0().left_0().size_full().bg(scrim),
                                "fade-setup-scrim",
                                FadeKind::Scrim,
                                true,
                            ))
                        })
                        .child(view),
                )
            })
            .when_some(self.ssh_wizard.as_ref().map(|(view, _)| view.clone()), |root, view| {
                root.child(
                    div()
                        .id("ssh-wizard-overlay")
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(theme.ui(16.))
                        .when(!fade, |overlay| overlay.bg(scrim))
                        .when(fade, |overlay| {
                            overlay.child(fade_in(
                                div().absolute().top_0().left_0().size_full().bg(scrim),
                                "fade-ssh-wizard-scrim",
                                FadeKind::Scrim,
                                true,
                            ))
                        })
                        .child(view),
                )
            })
            .when_some(self.askpass.as_ref().map(|(view, _)| view.clone()), |root, view| {
                root.child(
                    div()
                        .id("askpass-overlay")
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(theme.ui(16.))
                        .when(!fade, |overlay| overlay.bg(scrim))
                        .when(fade, |overlay| {
                            overlay.child(fade_in(
                                div().absolute().top_0().left_0().size_full().bg(scrim),
                                "fade-askpass-scrim",
                                FadeKind::Scrim,
                                true,
                            ))
                        })
                        .child(view),
                )
            })
            .when_some(self.color_picker.as_ref().map(|(view, _)| view.clone()), |root, view| {
                let dismiss = view.clone();
                root.child(
                    div()
                        .id("color-picker-overlay")
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p(theme.ui(16.))
                        .when(!fade, |overlay| overlay.bg(scrim))
                        .when(fade, |overlay| {
                            overlay.child(fade_in(
                                div().absolute().top_0().left_0().size_full().bg(scrim),
                                "fade-color-picker-scrim",
                                FadeKind::Scrim,
                                true,
                            ))
                        })
                        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                            dismiss.update(cx, |_, cx| cx.emit(ColorPickerEvent::Cancelled));
                        })
                        .child(div().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).child(view)),
                )
            })
            .when_some(self.update_card.as_ref().zip(self.update_notice()), |root, (focus, update)| {
                let note = match update.note {
                    Note::ClosesSessions(_) => {
                        let colors = theme.tone(Tone::NeedsInput);
                        div()
                            .p(theme.ui(10.))
                            .rounded(px(RADIUS_SMALL))
                            .text_xs()
                            .text_color(rgb(colors.ink))
                            .when_some(colors.fill, |note, fill| note.bg(rgb(fill)))
                    }
                    Note::KeepsSessions => div().text_xs().text_color(rgb(theme.tokens.muted)),
                }
                .child(update.note_text());
                let close = |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                    this.update_card = None;
                    this.focus_current(window, cx);
                    cx.notify();
                };
                let (close_on_key, close_outside) =
                    (cx.listener(move |this, _: &(), window, cx| close(this, window, cx)), {
                        cx.listener(move |this, _: &(), window, cx| close(this, window, cx))
                    });
                root.child(
                    div()
                        .id("update-card")
                        .occlude()
                        .absolute()
                        .top(theme.ui(46.))
                        .right(theme.ui(74.))
                        .w(theme.ui(320.))
                        .p(theme.ui(16.))
                        .flex()
                        .flex_col()
                        .gap(theme.ui(12.))
                        .bg(rgb(theme.tokens.raised))
                        .border_1()
                        .border_color(rgb(theme.tokens.line))
                        .rounded(px(RADIUS))
                        .track_focus(focus)
                        .on_key_down(move |event, window, cx| {
                            if event.keystroke.key == "escape" {
                                close_on_key(&(), window, cx);
                            }
                            cx.stop_propagation();
                        })
                        .on_mouse_down_out(move |_, window, cx| close_outside(&(), window, cx))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(theme.tokens.text))
                                .child(update.title()),
                        )
                        .child(div().text_xs().text_color(rgb(theme.tokens.muted)).child(update.detail_text()))
                        .child(note)
                        .when_some(self.update_error.clone(), |card, error| {
                            card.child(div().text_xs().text_color(rgb(theme.tokens.danger)).child(error))
                        })
                        .child(
                            div()
                                .id("restart-and-update")
                                .h(theme.ui(32.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(RADIUS_SMALL))
                                .bg(rgb(theme.tokens.accent))
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(theme.tokens.on_accent))
                                .cursor_pointer()
                                .child("Restart and update")
                                .on_click(cx.listener(|this, _, _, cx| this.restart_and_update(cx))),
                        ),
                )
            })
            .when_some(self.update_host_card.as_ref(), |root, card| {
                root.child(self.render_host_update_card(card, viewport, &theme, cx))
            })
            .when_some(self.release_card.as_ref().zip(self.online_notice()), |root, (focus, online)| {
                root.child(self.render_release_card(focus, &online, &theme, cx))
            })
            .when_some(self.pane_menu.as_ref(), |root, menu| {
                let pick = cx.listener(|this, action: &MenuAction, window, cx| {
                    this.pane_menu = None;
                    this.focus_current(window, cx);
                    this.run_menu_action(action.clone(), window, cx);
                    cx.notify();
                });
                let dismiss = cx.listener(|this, _: &(), window, cx| {
                    this.pane_menu = None;
                    this.focus_current(window, cx);
                    cx.notify();
                });
                root.child(context_menu::render(
                    "pane-menu",
                    menu,
                    &theme,
                    cx.global::<SettingsStore>().settings(),
                    move |action, window, cx| pick(action, window, cx),
                    move |window, cx| dismiss(&(), window, cx),
                ))
            })
            .when_some(self.tab_menu.as_ref(), |root, menu| {
                let pick = cx.listener(|this, action: &MenuAction, window, cx| {
                    this.tab_menu = None;
                    this.focus_current(window, cx);
                    this.run_menu_action(action.clone(), window, cx);
                    cx.notify();
                });
                let dismiss = cx.listener(|this, _: &(), window, cx| {
                    this.tab_menu = None;
                    this.focus_current(window, cx);
                    cx.notify();
                });
                root.child(context_menu::render(
                    "tab-menu",
                    menu,
                    &theme,
                    cx.global::<SettingsStore>().settings(),
                    move |action, window, cx| pick(action, window, cx),
                    move |window, cx| dismiss(&(), window, cx),
                ))
            })
            // Last, so nothing above the workspace draws over its scrim.
            .when_some(self.dialog.as_ref().map(|open| open.view.clone()), |root, view| {
                root.child(dialog_overlay(view, &theme, fade))
            });
        window_frame::frame(window, theme.tokens.line, root)
    }
}

impl WorkspaceView {
    /// The card of an SSH host's update: the question before a restart that ends sessions, or why the update cannot
    /// start or failed. Opens at the click on the pill, kept inside the window.
    fn render_host_update_card(
        &self,
        card: &HostUpdateCard,
        viewport: gpui::Size<gpui::Pixels>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let name = card.host.name().to_string();
        let width = theme.ui(330.);
        // An estimate of the card's height, enough to keep it above the window's bottom edge.
        let height = theme.ui(230.);
        let left = card.position.x.min(viewport.width - width - theme.ui(8.)).max(theme.ui(8.));
        let top = card.position.y.min(viewport.height - height - theme.ui(8.)).max(theme.ui(8.));
        let close = |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            this.update_host_card = None;
            this.focus_current(window, cx);
            cx.notify();
        };
        let (close_on_key, close_outside) = (cx.listener(move |this, _: &(), window, cx| close(this, window, cx)), {
            cx.listener(move |this, _: &(), window, cx| close(this, window, cx))
        });
        let bold = HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            color: Some(rgb(theme.tokens.accent).into()),
            ..HighlightStyle::default()
        };
        let (title, body, buttons) = match &card.kind {
            CardKind::Ask { running } => {
                let remote = self.hosts.get(&card.host).and_then(|conn| conn.daemon_version);
                let remote = remote.map(|version| version.to_string()).unwrap_or_default();
                let local = Version::parse(protocol::build::VERSION).map(|v| v.to_string()).unwrap_or_default();
                let lead = format!("{name} runs ");
                let middle = format!(". This Gantry is {local}");
                let text = format!("{lead}{remote}{middle}. It still works, but misses the daemon fixes since then.");
                let first = lead.len()..lead.len() + remote.len();
                let second_start = first.end + middle.len() - local.len();
                let second = second_start..second_start + local.len();
                let colors = theme.tone(Tone::NeedsInput);
                let body = div()
                    .flex()
                    .flex_col()
                    .gap(theme.ui(12.))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme.tokens.muted))
                            .child(StyledText::new(text).with_highlights([(first, bold), (second, bold)])),
                    )
                    .child(
                        div()
                            .p(theme.ui(10.))
                            .rounded(px(RADIUS_SMALL))
                            .text_xs()
                            .text_color(rgb(colors.ink))
                            .when_some(colors.fill, |note, fill| note.bg(rgb(fill)))
                            .child(ask_note(&name, *running)),
                    );
                (format!("Update Gantry on {name}"), body, true)
            }
            CardKind::Blocked(reason) => {
                (format!("Cannot update {name}"), reason_text(reason, theme.tokens.muted), false)
            }
            CardKind::Failed(reason) => {
                (format!("Update failed on {name}"), reason_text(reason, theme.tokens.danger), true)
            }
        };
        let (host, position) = (card.host.clone(), card.position);
        let go_label = if matches!(card.kind, CardKind::Ask { .. }) { "Update and restart" } else { "Try again" };
        let go = buttons.then(|| {
            div()
                .id("update-host-go")
                .h(theme.ui(32.))
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(RADIUS_SMALL))
                .bg(rgb(theme.tokens.accent))
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme.tokens.on_accent))
                .cursor_pointer()
                .child(go_label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.update_host_card = None;
                    this.focus_current(window, cx);
                    this.retry_host_update(&host, position, window, cx);
                    cx.notify();
                }))
        });
        let later = div()
            .id("update-host-later")
            .h(theme.ui(32.))
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(RADIUS_SMALL))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(theme.tokens.text))
            .cursor_pointer()
            .child("Later")
            .on_click(cx.listener(|this, _, window, cx| {
                this.update_host_card = None;
                this.focus_current(window, cx);
                cx.notify();
            }));
        div()
            .id("update-host-card")
            .occlude()
            .absolute()
            .left(left)
            .top(top)
            .w(width)
            .p(theme.ui(16.))
            .flex()
            .flex_col()
            .gap(theme.ui(12.))
            .bg(rgb(theme.tokens.raised))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .rounded(px(RADIUS))
            .track_focus(&card.focus)
            .on_key_down(move |event, window, cx| {
                if event.keystroke.key == "escape" {
                    close_on_key(&(), window, cx);
                }
                cx.stop_propagation();
            })
            .on_mouse_down_out(move |_, window, cx| close_outside(&(), window, cx))
            .child(div().text_sm().font_weight(FontWeight::BOLD).text_color(rgb(theme.tokens.text)).child(title))
            .child(body)
            .child(div().flex().gap(theme.ui(8.)).children(go).child(later))
    }
}

/// A card's reason line in `color`.
fn reason_text(reason: &str, color: u32) -> gpui::Div {
    div().text_xs().text_color(rgb(color)).child(reason.to_string())
}
