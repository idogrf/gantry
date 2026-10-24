use super::{OpenSettings, SIDEBAR_EDGE, WorkspaceView};
use std::time::Duration;

use app_core::release::{self, CheckStatus, NotesBlock, Online, Release};
use app_core::version::{self, Update, Version};
use daemon::paths::{Paths, current_profile};
use gpui::{
    Context, Decorations, Div, FocusHandle, FontWeight, IntoElement, MouseButton, MouseDownEvent, Task, Window, div,
    img, prelude::*, px, rgb,
};

use app_core::session_tree::{Tone, display_path, leaf_tone, status_tone, tone_word};

use crate::assets::LOGO;
use crate::hover_card::{self, HoverCard};
use crate::release_check::{self, CheckRow};
use crate::settings_store::SettingsStore;
use crate::theme::{
    RADIUS, RADIUS_SMALL, Theme, container_icon, dir_badge, pin_icon, ssh_icon, sweeping, theme, working_sweep,
};
use crate::update_watch::{self};
use crate::window_frame;

/// Height of the title bar.
pub(crate) const TITLE_BAR_HEIGHT: f32 = 36.0;

impl WorkspaceView {
    /// A version was found at the start path.
    #[cfg(target_os = "linux")]
    pub(crate) fn installed_version(&mut self, version: Version, cx: &mut Context<Self>) {
        self.installed = Some(version);
        cx.notify();
    }

    /// The update notice, if anything is older than the newest version known.
    pub(super) fn update_notice(&self) -> Option<Update> {
        let running = Version::parse(protocol::build::VERSION)?;
        // Only this machine's sessions end with a restart: remote hosts keep running.
        let live = self.workspace.leaves.values().filter(|leaf| leaf.host.is_local() && leaf.pane.is_some()).count();
        version::update(running, self.daemon_version, self.installed, live)
    }

    /// The release notice: a published release newer than this window and the installed binary. The install notice
    /// wins when both exist.
    pub(super) fn online_notice(&self) -> Option<Online> {
        if !(self.check_updates || self.manual_found) || self.update_notice().is_some() {
            return None;
        }
        release::online(Version::parse(protocol::build::VERSION)?, self.installed, &self.releases)
    }

    /// The first check 30 seconds after start, then one a day.
    pub(super) fn start_release_timer(&self, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(release_check::FIRST_DELAY).await;
            loop {
                if this.update(cx, |view, cx| view.check_releases(false, cx)).is_err() {
                    return;
                }
                cx.background_executor().timer(release_check::INTERVAL).await;
            }
        })
    }

    /// Asks GitHub for the release list. The daily check does nothing while the setting is off; Check now (`manual`)
    /// always asks, and ignores a press while one is running.
    pub(super) fn check_releases(&mut self, manual: bool, cx: &mut Context<Self>) {
        if !manual && !self.check_updates {
            return;
        }
        if manual {
            if cx.global::<CheckRow>().status == CheckStatus::Checking {
                return;
            }
            self.set_check_row(CheckStatus::Checking, cx);
        }
        let (url, etag) = (release_check::endpoint(), self.release_etag.clone());
        cx.spawn(async move |this, cx| {
            let fetched =
                cx.background_executor().spawn(async move { release_check::fetch(&url, etag.as_deref()) }).await;
            let _ = this.update(cx, |view, cx| view.release_fetched(fetched, manual, cx));
        })
        .detach();
    }

    /// An answer arrived. The daily one is dropped when the check was turned off meanwhile; only a manual one reaches
    /// the Check now row, and its find shows the notice even while the setting is off.
    fn release_fetched(&mut self, fetched: release_check::Fetched, manual: bool, cx: &mut Context<Self>) {
        if !manual && !self.check_updates {
            return;
        }
        let failed = fetched == release_check::Fetched::Failed;
        release_check::apply_result(&mut self.releases, &mut self.release_etag, fetched);
        if manual && let Some(running) = Version::parse(protocol::build::VERSION) {
            self.manual_found |= !failed;
            let status = if failed { CheckStatus::Failed } else { release::check_status(running, &self.releases) };
            self.set_check_row(status, cx);
        }
        cx.notify();
    }

    /// Publishes what the Check now row shows.
    pub(super) fn set_check_row(&mut self, status: CheckStatus, cx: &mut Context<Self>) {
        let url = match status {
            CheckStatus::Available(_) => {
                self.releases.first().map(|release| release.url.clone()).filter(|url| !url.is_empty())
            }
            _ => None,
        };
        let row = CheckRow { status, url };
        if *cx.global::<CheckRow>() != row {
            cx.set_global(row);
        }
    }

    /// Restart and update: Stop all when the daemon is older, start the new GUI, quit.
    pub(super) fn restart_and_update(&mut self, cx: &mut Context<Self>) {
        let (Some(update), Some(path)) = (self.update_notice(), self.start_path.clone()) else { return };
        // Nothing is stopped unless the file there really is the new version (not half written, not older).
        if !update_watch::probe(&path, update_watch::PROBE_TIMEOUT).is_some_and(|version| version >= update.target) {
            self.update_error =
                Some(format!("Could not start {}: it does not report version {}", path.display(), update.target));
            cx.notify();
            return;
        }
        if update.restart_daemon {
            self.local_client().stop_all_and_wait(Duration::from_secs(2));
            // The new GUI must not connect to the dying daemon: wait until it has let go of its lock.
            let lock = Paths::for_profile(&current_profile()).lock_file;
            if !update_watch::wait_for_lock_free(&lock, Duration::from_secs(5)) {
                tracing::warn!("the daemon was still running 5 s after Stop all; starting the new window anyway");
            }
        }
        match update_watch::restart_command(&path, update.restart_daemon).spawn() {
            Ok(_) => cx.quit(),
            Err(err) => {
                self.update_error = Some(format!("Could not start {}: {err}", path.display()));
                cx.notify();
            }
        }
    }

    pub(super) fn render_title_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let client_decorations = matches!(window.window_decorations(), Decorations::Client { .. });
        let theme = theme(cx).clone();
        let waiting = self.sidebar.read(cx).needs_input().len();
        let pill_colors = theme.tone(Tone::NeedsInput);
        let sidebar = &self.workspace.sidebar;
        let spacer = div().id("drag-area").flex_1().min_w(theme.ui(40.)).h_full();
        div()
            .id("title-bar")
            .h(theme.ui(TITLE_BAR_HEIGHT))
            .w_full()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(theme.ui(6.))
            .bg(rgb(theme.tokens.surface))
            .border_b_1()
            .border_color(rgb(theme.tokens.line))
            .text_sm()
            .map(|bar| {
                // The brand keeps the sidebar's width; the focused session starts where the panes do.
                let brand = div()
                    .id("brand")
                    .flex_none()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap(theme.ui(8.))
                    .font_weight(FontWeight::BOLD)
                    .child(img(LOGO).flex_none().size(theme.ui(22.)))
                    .child("Gantry");
                if sidebar.visible {
                    let brand = brand.w(theme.ui(sidebar.width) + px(SIDEBAR_EDGE)).pl(theme.ui(18.));
                    bar.child(if client_decorations { moves_window(brand) } else { brand })
                } else {
                    bar.pl_2().child(if client_decorations { moves_window(brand) } else { brand })
                }
            })
            .child(self.render_title_session(window, cx))
            .child(if client_decorations { moves_window(spacer) } else { spacer })
            .when_some(self.update_notice(), |bar, update| {
                bar.child(
                    div()
                        .id("update-pill")
                        .flex_none()
                        .h(theme.ui(24.))
                        .px(theme.ui(10.))
                        .flex()
                        .items_center()
                        .rounded_full()
                        .border_1()
                        .border_color(rgb(theme.tokens.accent))
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme.tokens.accent))
                        .cursor_pointer()
                        .child(update.pill())
                        .on_click(cx.listener(|this, _, window, cx| {
                            let focus = cx.focus_handle();
                            window.focus(&focus, cx);
                            this.update_card = Some(focus);
                            this.update_error = None;
                            cx.notify();
                        })),
                )
            })
            .when_some(self.online_notice(), |bar, online| {
                bar.child(
                    div()
                        .id("release-pill")
                        .flex_none()
                        .h(theme.ui(24.))
                        .px(theme.ui(10.))
                        .flex()
                        .items_center()
                        .rounded_full()
                        .border_1()
                        .border_color(rgb(theme.tokens.accent))
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme.tokens.accent))
                        .cursor_pointer()
                        .child(online.pill())
                        .on_click(cx.listener(|this, _, window, cx| {
                            let focus = cx.focus_handle();
                            window.focus(&focus, cx);
                            this.release_card = Some(focus);
                            cx.notify();
                        })),
                )
            })
            .when(waiting > 0, |bar| {
                bar.child(
                    div()
                        .id("need-input")
                        .flex_none()
                        .h(theme.ui(24.))
                        .px(theme.ui(10.))
                        .flex()
                        .items_center()
                        .rounded_full()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(pill_colors.ink))
                        .when_some(pill_colors.fill, |pill, fill| pill.bg(rgb(fill)))
                        .cursor_pointer()
                        .child(format!("{} {}", waiting, if waiting == 1 { "needs input" } else { "need input" }))
                        .on_click(cx.listener(|this, _, window, cx| {
                            let next = this.sidebar.read(cx).next_needing_input(this.workspace.focused());
                            if let Some(leaf) = next {
                                this.focus_leaf(leaf, window, cx);
                            }
                        })),
                )
            })
            .child(round_button(
                "open-settings",
                "⚙",
                &theme,
                cx.listener(|this, _, window, cx| this.open_settings(&OpenSettings, window, cx)),
            ))
            .when(client_decorations, |bar| {
                bar.child(window_frame::window_controls(
                    window,
                    &theme,
                    cx.listener(|this, _, window, cx| {
                        if this.request_quit(window, cx) {
                            cx.quit();
                        }
                    }),
                ))
            })
    }

    /// The release notes card under the release pill: the notes of every release newer than this window.
    pub(super) fn render_release_card(
        &self,
        focus: &FocusHandle,
        online: &Online,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let close = |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            this.release_card = None;
            this.focus_current(window, cx);
            cx.notify();
        };
        let (close_on_key, close_outside) = (cx.listener(move |this, _: &(), window, cx| close(this, window, cx)), {
            cx.listener(move |this, _: &(), window, cx| close(this, window, cx))
        });
        let notes = online.newer.iter().fold(div().flex().flex_col().gap(theme.ui(14.)), |notes, release| {
            notes.child(release_section(theme, release))
        });
        let url = online.newer.first().map(|release| release.url.clone()).filter(|url| !url.is_empty());
        let running = Version::parse(protocol::build::VERSION).map(|version| version.to_string()).unwrap_or_default();
        div()
            .id("release-card")
            .occlude()
            .absolute()
            .top(theme.ui(46.))
            .right(theme.ui(74.))
            .w(theme.ui(360.))
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
                    .child(format!("Gantry {} is available", online.latest)),
            )
            .child(div().text_xs().text_color(rgb(theme.tokens.muted)).child(format!("This window runs {running}.")))
            .child(div().id("release-notes").max_h(theme.ui(380.)).overflow_y_scroll().child(notes))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme.tokens.muted))
                    .child("Download it from the release page and install as before."),
            )
            .when_some(url, |card, url| {
                card.child(
                    div()
                        .id("open-release-page")
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
                        .child("Open release page")
                        .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url))),
                )
            })
    }

    /// The middle of the title bar: `Settings`, or the project and the focused session's chip.
    fn render_title_session(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let theme = theme(cx).clone();
        let middle = div().min_w_0().flex_shrink(1.).flex().flex_row().items_center().gap(theme.ui(8.));
        let Some(tab) = self.workspace.active_tab() else { return middle };
        if tab.settings {
            // With the sidebar hidden its row is gone, so the bar closes Settings.
            return middle.font_weight(FontWeight::SEMIBOLD).child("Settings").when(
                !self.workspace.sidebar.visible,
                |middle| {
                    middle.child(
                        div()
                            .id("close-settings")
                            .flex_none()
                            .size(theme.ui(20.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .text_xs()
                            .font_weight(FontWeight::NORMAL)
                            .hover(|style| style.bg(rgb(theme.tokens.raised)))
                            .cursor_pointer()
                            .child("×")
                            .on_click(cx.listener(|this, _, window, cx| this.close_settings(window, cx))),
                    )
                },
            );
        }
        let index = self.workspace.active;
        let leaf = tab.focused;
        let animate = cx.global::<SettingsStore>().settings().animate_working && window.is_window_active();
        let tab_name = tab.name.clone().filter(|_| tab.root.leaves().len() > 1);
        let title = tab_name
            .or_else(|| self.workspace.leaf_name(leaf).map(String::from))
            .or_else(|| self.sidebar.read(cx).leaf_title(leaf))
            .unwrap_or_else(|| self.tab_title(index, cx));
        let status = self.workspace.leaf_status(leaf, self.sidebar.read(cx).sessions()).cloned();
        let unseen = self.unseen_of(leaf);
        let container = self.workspace.leaves.get(&leaf).is_some_and(|leaf| leaf.launch.container.is_some());
        let remote = self.workspace.leaves.get(&leaf).and_then(|leaf| leaf.host.remote_name().map(String::from));
        let dir = self.leaf_dir_badge(leaf, cx);
        let cwd = self.leaf_cwd(leaf, cx);
        let offline = self.leaf_offline(leaf);
        let tone = leaf_tone(status.as_ref(), unseen, offline);
        let colors = tone.map(|tone| theme.tone(tone));
        let pin = (!offline && status_tone(status.as_ref(), unseen) == Tone::Unread)
            .then(|| pin_icon(colors.map_or(theme.tokens.text, |colors| colors.dot), &theme));
        let ink = colors.map_or(theme.tokens.text, |colors| colors.ink);
        let sweep = tone.is_some_and(|tone| sweeping(tone, animate, true));
        let card = HoverCard {
            title: title.clone(),
            status: tone.map(|tone| tone_word(tone).to_string()),
            dir: dir.clone(),
            container,
            host: remote.clone(),
            folder: cwd.as_deref().map(|cwd| display_path(cwd, std::env::home_dir().as_deref())),
        };
        let sidebar = self.sidebar.read(cx);
        let host = self.workspace.leaves.get(&leaf).map(|leaf| leaf.host.clone()).unwrap_or_default();
        let project = cwd.as_deref().map(|cwd| {
            let root = sidebar.project_of(&host, cwd);
            let label = sidebar.project_label(&root).unwrap_or_else(|| {
                root.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned())
            });
            (label, theme.project(&root).unwrap_or(theme.tokens.muted))
        });
        middle
            .children(project.map(|(label, color)| {
                div()
                    .min_w_0()
                    .max_w(theme.ui(240.))
                    .flex()
                    .gap(theme.ui(8.))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(color))
                            .child(label),
                    )
                    .child(div().flex_none().text_color(rgb(theme.tokens.muted)).child("›"))
            }))
            .child(
                div()
                    .id("title-chip")
                    .tooltip(hover_card::tooltip(card))
                    .relative()
                    .h(theme.ui(26.))
                    .px(theme.ui(10.))
                    .min_w_0()
                    .flex_shrink(1.)
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(px(RADIUS_SMALL))
                    .text_color(rgb(ink))
                    .font_weight(FontWeight::SEMIBOLD)
                    .when_some(colors.and_then(|colors| colors.fill), |chip, fill| chip.bg(rgb(fill)))
                    .when(sweep, |chip| chip.child(working_sweep("sweep-title", px(RADIUS_SMALL), &theme)))
                    .children(pin)
                    .child(div().min_w_0().truncate().child(title))
                    .when(container, |chip| chip.child(container_icon(ink, &theme)))
                    .when(remote.is_some(), |chip| chip.child(ssh_icon(ink, &theme)))
                    .children(dir.map(|badge| dir_badge(&badge, &theme)))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.open_tab_menu(index, event.position, window, cx);
                            // Keep focus on the menu: the workspace's tracked focus must not take it back.
                            window.prevent_default();
                            cx.stop_propagation();
                        }),
                    ),
            )
            .children(self.git_chip_of(leaf, "git-chip-title", &theme, cx))
    }
}

/// Title-bar behaviour for an empty stretch of the title bar (client-side decorations): drag moves the window,
/// double-click maximizes, right-click opens the window menu.
fn moves_window(area: gpui::Stateful<gpui::Div>) -> gpui::Stateful<gpui::Div> {
    area.on_mouse_down(MouseButton::Left, |event, window, _| {
        if event.click_count >= 2 {
            window.zoom_window();
        } else {
            window.start_window_move();
        }
    })
    .on_mouse_down(MouseButton::Right, |event, window, _| window.show_window_menu(event.position))
}

/// A small round icon button in the title bar.
pub(crate) fn round_button(
    id: &'static str,
    label: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex_none()
        .size(theme.ui(30.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_color(rgb(theme.tokens.muted))
        .hover(|style| style.text_color(rgb(theme.tokens.text)).bg(rgb(theme.tokens.raised)))
        .cursor_pointer()
        .child(label)
        .on_click(on_click)
}

/// One release's heading and notes for the release card.
fn release_section(theme: &Theme, release: &Release) -> Div {
    let heading = if release.date.is_empty() {
        format!("v{}", release.version)
    } else {
        format!("v{} · {}", release.version, release.date)
    };
    let text = rgb(theme.tokens.text);
    release::notes_blocks(&release.notes).into_iter().fold(
        div()
            .flex()
            .flex_col()
            .gap(theme.ui(3.))
            .child(div().text_sm().font_weight(FontWeight::BOLD).text_color(rgb(theme.tokens.accent)).child(heading)),
        |section, block| {
            section.child(match block {
                NotesBlock::Heading(text_line) => {
                    div().pt(theme.ui(4.)).text_xs().font_weight(FontWeight::BOLD).text_color(text).child(text_line)
                }
                NotesBlock::Bullet(item) => div()
                    .flex()
                    .gap(theme.ui(6.))
                    .text_xs()
                    .text_color(text)
                    .child(div().flex_none().child("•"))
                    .child(div().flex_1().min_w_0().child(item)),
                NotesBlock::Line(line) => div().text_xs().text_color(text).child(line),
                NotesBlock::Gap => div().h(theme.ui(4.)),
            })
        },
    )
}
