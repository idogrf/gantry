use super::{CommitView, GitMetrics, GitPage, PanelAnim, ToggleGitPanel, WorkspaceView, unix_now};
use std::path::Path;
use std::time::{Duration, Instant};

use app_core::split_tree::NodeId;
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, Div, FontWeight, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Task, Window, div, prelude::*, px, rgb, uniform_list,
};
use protocol::host::HostId;
use protocol::message::SessionStatus;

use app_core::session_tree::Tone;

use crate::git_chip;
use crate::git_diff_view::{self, DiffState};
use crate::git_panel::{self, PanelTab};
use crate::git_scrollbar::{self, Bar};
use crate::git_state::RepoView;
use crate::settings_store::SettingsStore;
use crate::theme::{RADIUS_SMALL, Theme, theme};

impl WorkspaceView {
    /// Once a second: decide which git repos are on screen and ask the daemons for those that are due. The tick only
    /// looks at the clock; git runs when `GitRepos` says a repo is due (5 s while busy, a minute otherwise).
    pub(super) fn start_git_timer(window: &mut Window, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update_in(cx, |view, window, cx| view.git_tick(window, cx)).is_err() {
                    return;
                }
            }
        })
    }

    /// The repos the open tab's panes sit in, with whether a Claude session in them is busy.
    fn git_wanted(&self, cx: &Context<Self>) -> Vec<(HostId, String, bool)> {
        let Some(tab) = self.workspace.active_tab().filter(|tab| !tab.settings) else { return Vec::new() };
        let sessions = self.sidebar.read(cx).sessions();
        tab.root
            .leaves()
            .into_iter()
            .filter_map(|id| {
                let (host, folder) = self.leaf_repo(id)?;
                self.client(&host)?;
                let busy = matches!(self.workspace.leaf_status(id, sessions), Some(SessionStatus::Busy));
                // The panel shows the focused pane's repo, which is polled at the fast rate while it is open.
                let in_panel = self.git_panel_open && id == tab.focused;
                Some((host, folder, busy || in_panel))
            })
            .chain(self.git_page.as_ref().map(|page| (page.host.clone(), page.folder.clone(), true)))
            .collect()
    }

    /// The host and folder whose git repo `leaf` sits in (a container pane: its host folder).
    fn leaf_repo(&self, leaf: NodeId) -> Option<(HostId, String)> {
        let entry = self.workspace.leaves.get(&leaf)?;
        let folder = entry.launch.container.as_ref().unwrap_or(&entry.launch.cwd);
        Some((entry.host.clone(), folder.to_string_lossy().into_owned()))
    }

    /// The git chip for `leaf`'s repo, once it is known to be one.
    pub(super) fn git_chip_of(
        &self,
        leaf: NodeId,
        id: impl Into<gpui::ElementId>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<Div>> {
        let (host, folder) = self.leaf_repo(leaf)?;
        let chip = git_chip::chip(id, self.git.view(&host, &folder), unix_now(), theme)?;
        Some(
            chip.cursor_pointer()
                .on_click(cx.listener(|this, _, window, cx| this.toggle_git_panel(&ToggleGitPanel, window, cx))),
        )
    }

    fn git_tick(&mut self, window: &Window, cx: &mut Context<Self>) {
        let wanted = self.git_wanted(cx);
        self.git.set_wanted(wanted);
        for (host, folder, known_head, request) in self.git.due(Instant::now(), window.is_window_active()) {
            if let Some(client) = self.client(&host) {
                client.git_status(request, &folder, known_head.as_deref());
            }
        }
    }

    /// Open or close the git panel; either way every repo is looked at again at once.
    pub(super) fn toggle_git_panel(&mut self, _: &ToggleGitPanel, window: &mut Window, cx: &mut Context<Self>) {
        self.git_panel_open = !self.git_panel_open;
        self.git.refresh_now();
        self.cancel_pane_anim();
        let animate = cx.global::<SettingsStore>().settings().animate_ui && window.is_window_active();
        self.panel_anim = animate.then(|| PanelAnim { started: Instant::now(), opening: self.git_panel_open });
        cx.notify();
    }

    /// Open the git page on the focused pane's repo, optionally with a file's diff.
    fn open_git_page(&mut self, pick: Option<(String, Option<String>)>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((host, folder)) = self.workspace.focused().and_then(|leaf| self.leaf_repo(leaf)) else { return };
        let same = self.git_page.as_ref().is_some_and(|page| page.host == host && page.folder == folder);
        if !same {
            let label = Path::new(&folder)
                .file_name()
                .map_or_else(|| folder.clone(), |name| name.to_string_lossy().into_owned());
            self.git_page = Some(GitPage {
                host,
                folder,
                label,
                picked: None,
                request: 0,
                state: DiffState::Nothing,
                counts: None,
                selection: None,
                commit: None,
            });
        }
        self.git.refresh_now();
        self.sync_git_row(cx);
        self.git_focus.focus(window, cx);
        if let Some((path, old_path)) = pick {
            self.pick_git_file(path, old_path, false, cx);
        }
        cx.notify();
    }

    /// Open the git page on the focused pane's repo showing commit `hash`: its message and changed files.
    fn open_git_commit(&mut self, hash: String, window: &mut Window, cx: &mut Context<Self>) {
        self.open_git_page(None, window, cx);
        let request = self.git.new_request();
        let Some(page) = self.git_page.as_mut() else { return };
        page.picked = None;
        page.state = DiffState::Nothing;
        page.selection = None;
        page.commit = Some(CommitView { hash: hash.clone(), request, detail: None, failed: None });
        let (host, folder) = (page.host.clone(), page.folder.clone());
        if let Some(client) = self.client(&host) {
            client.git_commit(request, &folder, &hash);
        }
        cx.notify();
    }

    /// Leave the commit shown on the git page for the working changes.
    fn leave_git_commit(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.git_page.as_mut() else { return };
        page.commit = None;
        page.picked = None;
        page.state = DiffState::Nothing;
        page.selection = None;
        cx.notify();
    }

    pub(super) fn close_git_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.git_page.take().is_some() {
            self.sync_git_row(cx);
            self.focus_current(window, cx);
            cx.notify();
        }
    }

    fn sync_git_row(&self, cx: &mut Context<Self>) {
        let label = self.git_page.as_ref().map(|page| page.label.clone());
        self.sidebar.update(cx, |sidebar, cx| sidebar.set_git(label, cx));
    }

    /// The added and removed counts git lists for `path` in `host`'s `folder`.
    fn git_file_counts(&self, host: &HostId, folder: &str, path: &str) -> Option<(Option<u32>, Option<u32>)> {
        let Some(RepoView::Repo(state)) = self.git.view(host, folder) else { return None };
        state.files.iter().find(|file| file.path == path).map(|file| (file.added, file.removed))
    }

    /// Show `path`'s diff: the answer replaces what is drawn once it arrives.
    pub(super) fn pick_git_file(&mut self, path: String, old_path: Option<String>, full: bool, cx: &mut Context<Self>) {
        let Some(page) = self.git_page.as_mut() else { return };
        page.picked = Some((path, old_path));
        page.state = DiffState::Loading;
        page.selection = None;
        self.git_scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        self.request_git_diff(full, cx);
        cx.notify();
    }

    /// Ask for the picked file's diff again, because its counts changed (or `force`); what is drawn stays until the
    /// answer arrives.
    pub(super) fn refresh_git_diff(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(page) = self.git_page.as_ref().filter(|page| page.commit.is_none()) else { return };
        let Some((path, _)) = page.picked.clone() else { return };
        let counts = self.git_file_counts(&page.host, &page.folder, &path);
        if force || (counts.is_some() && counts != page.counts) {
            self.request_git_diff(false, cx);
        }
    }

    fn request_git_diff(&mut self, full: bool, _cx: &mut Context<Self>) {
        let request = self.git.new_request();
        let Some(page) = self.git_page.as_mut() else { return };
        let Some((path, old_path)) = page.picked.clone() else { return };
        page.request = request;
        let (host, folder) = (page.host.clone(), page.folder.clone());
        let commit = page.commit.as_ref().map(|commit| commit.hash.clone());
        let counts = if commit.is_some() { None } else { self.git_file_counts(&host, &folder, &path) };
        if let Some(page) = self.git_page.as_mut() {
            page.counts = counts;
        }
        if let Some(client) = self.client(&host) {
            client.git_diff(request, &folder, &path, old_path.as_deref(), full, commit.as_deref());
        }
    }

    /// Select diff rows for copying: `index` alone, or from where the selection started with `extend`.
    fn select_git_rows(&mut self, index: usize, extend: bool, cx: &mut Context<Self>) {
        let Some(page) = self.git_page.as_mut() else { return };
        page.selection = match page.selection {
            Some((anchor, _)) if extend => Some((anchor, index)),
            _ => Some((index, index)),
        };
        cx.notify();
    }

    /// Put the selected diff rows on the clipboard, without their `+` and `-`.
    fn copy_git_selection(&self, cx: &mut Context<Self>) {
        let Some(page) = &self.git_page else { return };
        let (DiffState::Rows { diff, .. }, Some((a, b))) = (&page.state, page.selection) else { return };
        let text = git_diff_view::copy_text(&diff.rows, a.min(b)..a.max(b) + 1);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// The diff page's font (the terminal's, a little smaller) and the size of one cell of it.
    fn git_metrics(&self, window: &Window, cx: &App) -> GitMetrics {
        let font = cx.global::<SettingsStore>().settings().terminal_font.clone();
        let family = crate::terminal_view::installed_family(
            font.family.as_deref(),
            crate::terminal_view::installed_families(cx),
        )
        .to_string();
        let size = (font.size * 0.92).max(8.0);
        let face = crate::terminal_view::pane_font(Some(&family));
        let text_system = window.text_system();
        let id = text_system.resolve_font(&face);
        let cell_width = text_system.advance(id, px(size), 'm').map_or(size * 0.6, |advance| f32::from(advance.width));
        GitMetrics { family: family.into(), size, row_height: (size * font.line_height).round(), cell_width }
    }

    /// The visible rows of the panel's file list.
    fn render_panel_file_rows(
        &self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::Stateful<Div>> {
        let theme = theme(cx).clone();
        let Some((host, folder)) = self.workspace.focused().and_then(|leaf| self.leaf_repo(leaf)) else {
            return Vec::new();
        };
        let Some(RepoView::Repo(state)) = self.git.view(&host, &folder) else { return Vec::new() };
        range
            .filter_map(|index| state.files.get(index).map(|file| (index, file)))
            .map(|(index, file)| {
                let (path, old_path) = (file.path.clone(), file.old_path.clone());
                git_panel::file_row(index, file, false, &theme).on_click(cx.listener(move |this, _, window, cx| {
                    this.open_git_page(Some((path.clone(), old_path.clone())), window, cx)
                }))
            })
            .collect()
    }

    /// The files the git page lists, and how many more there are: the commit's, else the working changes'.
    fn page_files<'a>(&'a self, page: &'a GitPage) -> (&'a [protocol::message::GitFile], u32) {
        match (&page.commit, self.git.view(&page.host, &page.folder)) {
            (Some(commit), _) => commit.detail.as_ref().map_or((&[][..], 0), |detail| (&detail.files[..], detail.more)),
            (None, Some(RepoView::Repo(state))) => (&state.files[..], state.more),
            _ => (&[][..], 0),
        }
    }

    /// The visible rows of the git page's file list.
    fn render_page_file_rows(&self, range: std::ops::Range<usize>, cx: &mut Context<Self>) -> Vec<gpui::Stateful<Div>> {
        let theme = theme(cx).clone();
        let Some(page) = &self.git_page else { return Vec::new() };
        let files = self.page_files(page).0;
        range
            .filter_map(|index| files.get(index).map(|file| (index, file)))
            .map(|(index, file)| {
                let picked = page.picked.as_ref().is_some_and(|(path, _)| *path == file.path);
                let (path, old_path) = (file.path.clone(), file.old_path.clone());
                git_panel::file_row(index, file, picked, &theme).on_click(
                    cx.listener(move |this, _, _, cx| this.pick_git_file(path.clone(), old_path.clone(), false, cx)),
                )
            })
            .collect()
    }

    /// The git page: the repo's changed files on the left, the picked file's diff on the right.
    pub(super) fn render_git_page(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let theme = theme(cx).clone();
        let base = div().id("git-page").track_focus(&self.git_focus).size_full().flex().flex_row();
        let Some(page) = &self.git_page else { return base };
        let (listed, more) = self.page_files(page);
        let files = listed.len();
        let list = div()
            .flex_none()
            .w(theme.ui(280.))
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(rgb(theme.tokens.line))
            .children(page.commit.as_ref().map(|commit| {
                git_panel::commit_head(commit.detail.as_ref(), commit.failed.as_deref(), unix_now(), &theme).child(
                    div()
                        .id("git-leave-commit")
                        .cursor_pointer()
                        .text_xs()
                        .text_color(rgb(theme.tokens.accent))
                        .child("← Working changes")
                        .on_click(cx.listener(|this, _, _, cx| this.leave_git_commit(cx))),
                )
            }))
            .child(git_panel::heading(
                if page.commit.is_some() { "Files in this commit" } else { "Changed files" },
                Some((files + more as usize).to_string()),
                &theme,
            ))
            .child(
                uniform_list(
                    "git-page-files",
                    files,
                    cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                        this.render_page_file_rows(range, cx)
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
            .when(more > 0, |list| list.child(git_panel::message(format!("+{more} more files"), &theme)));
        let title = page.picked.as_ref().map_or_else(
            || {
                page.commit
                    .as_ref()
                    .and_then(|c| c.detail.as_ref())
                    .map_or_else(String::new, |d| git_panel::subject(&d.message))
            },
            |(path, _)| path.clone(),
        );
        let header = div()
            .flex_none()
            .h(theme.ui(34.))
            .px(theme.ui(12.))
            .flex()
            .items_center()
            .gap(theme.ui(8.))
            .border_b_1()
            .border_color(rgb(theme.tokens.line))
            .text_sm()
            .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(
                git_panel::close_button("git-page-close", &theme)
                    .on_click(cx.listener(|this, _, window, cx| this.close_git_page(window, cx))),
            );
        let body = match git_diff_view::placeholder_text(&page.state) {
            Some(text) => {
                let too_large = matches!(page.state, DiffState::TooLarge(_));
                div().flex_1().min_h_0().flex().flex_col().child(git_panel::message(text, &theme)).when(
                    too_large,
                    |column| {
                        column.child(
                            div().px(theme.ui(12.)).child(
                                div()
                                    .id("git-show-anyway")
                                    .w(theme.ui(120.))
                                    .h(theme.ui(28.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(RADIUS_SMALL))
                                    .bg(rgb(theme.tokens.raised))
                                    .text_sm()
                                    .cursor_pointer()
                                    .child("Show anyway")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some((path, old_path)) =
                                            this.git_page.as_ref().and_then(|page| page.picked.clone())
                                        {
                                            this.pick_git_file(path, old_path, true, cx);
                                        }
                                    })),
                            ),
                        )
                    },
                )
            }
            None => {
                let DiffState::Rows { diff, truncated, .. } = &page.state else { return base };
                let longest =
                    diff.rows.iter().enumerate().max_by_key(|(_, row)| row.text.len()).map(|(index, _)| index);
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .child(
                                uniform_list(
                                    "git-diff-rows",
                                    diff.rows.len(),
                                    cx.processor(|this, range: std::ops::Range<usize>, window, cx| {
                                        this.render_diff_rows(range, window, cx)
                                    }),
                                )
                                // Rows are as wide as the longest line, so the list scrolls sideways by itself.
                                .with_horizontal_sizing_behavior(gpui::ListHorizontalSizingBehavior::Unconstrained)
                                .with_width_from_item(longest)
                                .track_scroll(&self.git_scroll)
                                .size_full(),
                            )
                            .children(self.render_git_bars(window, cx)),
                    )
                    .when(*truncated, |column| {
                        column.child(git_panel::message("The diff was cut at the size limit.".into(), &theme))
                    })
            }
        };
        base.on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
            if this.git_bars.grab.is_some() {
                if event.pressed_button == Some(MouseButton::Left) {
                    this.drag_git_bar(event.position, cx);
                } else {
                    this.git_bars.grab = None;
                }
            }
        }))
        .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &gpui::MouseUpEvent, _, _| this.git_bars.grab = None))
        .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _: &gpui::MouseUpEvent, _, _| this.git_bars.grab = None))
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            let key = &event.keystroke;
            if key.key == "escape" {
                this.close_git_page(window, cx);
                cx.stop_propagation();
            } else if key.key == "c" && (key.modifiers.control || key.modifiers.platform) {
                this.copy_git_selection(cx);
                cx.stop_propagation();
            }
        }))
        .child(list)
        .child(div().flex_1().min_w_0().h_full().flex().flex_col().child(header).child(body))
    }

    /// The diff list's two scrollbars (the ones that have something to scroll), over the list.
    fn render_git_bars(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let theme = theme(cx).clone();
        let Some(extent) = git_scrollbar::Extent::of(&self.git_scroll) else { return Vec::new() };
        self.git_bars.see(extent.offset);
        let level = self.git_bars.level();
        if level > 0.0 {
            window.request_animation_frame();
        }
        let color = git_scrollbar::thumb_color(&theme, level);
        let (vertical, horizontal) = extent.thumbs(theme.ui_scale);
        let corner = f32::from(theme.ui(git_scrollbar::WIDTH));
        let mut bars = Vec::new();
        if let Some(thumb) = vertical {
            let track = extent.viewport.1 - if horizontal.is_some() { corner } else { 0.0 };
            bars.push(
                div()
                    .id("git-bar-v")
                    .absolute()
                    .top_0()
                    .right_0()
                    .w(px(corner))
                    .h(px(track))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.press_git_bar(Bar::Vertical, event.position, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(git_scrollbar::thumb_div(Bar::Vertical, thumb, color, &theme))
                    .into_any_element(),
            );
        }
        if let Some(thumb) = horizontal {
            let track = extent.viewport.0 - if vertical.is_some() { corner } else { 0.0 };
            bars.push(
                div()
                    .id("git-bar-h")
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .h(px(corner))
                    .w(px(track))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.press_git_bar(Bar::Horizontal, event.position, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(git_scrollbar::thumb_div(Bar::Horizontal, thumb, color, &theme))
                    .into_any_element(),
            );
        }
        bars
    }

    /// A press on a scrollbar: grab its thumb, or move the thumb under the pointer when the press is on the track.
    fn press_git_bar(&mut self, bar: Bar, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(extent) = git_scrollbar::Extent::of(&self.git_scroll) else { return };
        let (vertical, horizontal) = extent.thumbs(theme(cx).ui_scale);
        let (thumb, along) = match bar {
            Bar::Vertical => (vertical, f32::from(position.y - extent.bounds.origin.y)),
            Bar::Horizontal => (horizontal, f32::from(position.x - extent.bounds.origin.x)),
        };
        let Some(thumb) = thumb else { return };
        let on_thumb = along >= thumb.top && along <= thumb.top + thumb.len;
        self.git_bars.grab = Some((bar, if on_thumb { along - thumb.top } else { thumb.len / 2.0 }));
        self.drag_git_bar(position, cx);
    }

    /// Scroll so the grabbed thumb follows the pointer.
    fn drag_git_bar(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some((bar, grab)) = self.git_bars.grab else { return };
        let Some(extent) = git_scrollbar::Extent::of(&self.git_scroll) else { return };
        let scale = theme(cx).ui_scale;
        let (vertical, horizontal) = extent.thumbs(scale);
        let (thumb, along) = match bar {
            Bar::Vertical => (vertical, f32::from(position.y - extent.bounds.origin.y)),
            Bar::Horizontal => (horizontal, f32::from(position.x - extent.bounds.origin.x)),
        };
        let Some(thumb) = thumb else { return };
        let offset = extent.offset_for(bar, along - grab, thumb.len, scale);
        let handle = self.git_scroll.0.borrow().base_handle.clone();
        let current = handle.offset();
        handle.set_offset(match bar {
            Bar::Vertical => gpui::point(current.x, px(-offset)),
            Bar::Horizontal => gpui::point(px(-offset), current.y),
        });
        cx.notify();
    }

    /// The visible rows of the picked file's diff.
    fn render_diff_rows(
        &self,
        range: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        use app_core::git_diff::RowKind;
        use app_core::theme_colors::mix;
        let Some(page) = &self.git_page else { return Vec::new() };
        let DiffState::Rows { diff, max_chars, .. } = &page.state else { return Vec::new() };
        let theme = theme(cx).clone();
        let metrics = self.git_metrics(window, cx);
        let gutter = px(metrics.cell_width * git_diff_view::gutter_digits(&diff.rows) as f32 + 14.0);
        let width = gutter * 2.0 + px(metrics.cell_width * (*max_chars as f32 + 2.0) + 32.0);
        let selected = page.selection.map(|(a, b)| a.min(b)..=a.max(b));
        let right_to_left = cx.global::<SettingsStore>().settings().window.right_to_left;
        let terminal = theme.tokens.terminal;
        let added = theme.tone(Tone::Done).dot;
        let removed = theme.tokens.danger;
        range
            .filter_map(|index| diff.rows.get(index).map(|row| (index, row)))
            .map(|(index, row)| {
                let (background, ink, prefix) = match row.kind {
                    RowKind::Added => (mix(added, terminal, 0.16), theme.tokens.text, "+"),
                    RowKind::Removed => (mix(removed, terminal, 0.18), theme.tokens.text, "-"),
                    RowKind::Hunk => (mix(theme.tokens.accent, terminal, 0.12), theme.tokens.accent, ""),
                    RowKind::Context => (terminal, theme.tokens.text, " "),
                    RowKind::NoNewline => (terminal, theme.tokens.muted, "\\"),
                };
                let marked = match row.kind {
                    RowKind::Added => mix(added, terminal, 0.42),
                    RowKind::Removed => mix(removed, terminal, 0.45),
                    _ => background,
                };
                let background = if selected.as_ref().is_some_and(|range| range.contains(&index)) {
                    mix(theme.tokens.accent, background, 0.30)
                } else {
                    background
                };
                let number = |line: Option<u32>| {
                    div()
                        .flex_none()
                        .w(gutter)
                        .pr(px(8.))
                        .flex()
                        .justify_end()
                        .text_color(rgb(theme.tokens.muted))
                        .child(line.map(|line| line.to_string()).unwrap_or_default())
                };
                // With right-to-left display on, a line with Hebrew or Arabic letters is drawn in display order, one
                // character at a time; the rows, selection and copy stay logical.
                let reordered = right_to_left.then(|| git_diff_view::display_chars(&row.text, &row.words)).flatten();
                let text = match reordered {
                    Some(chars) => div().flex().items_center().children(
                        chars
                            .into_iter()
                            .map(|(ch, changed)| div().when(changed, |part| part.bg(rgb(marked))).child(ch)),
                    ),
                    None => div().flex().items_center().children(
                        git_diff_view::pieces(&row.text, &row.words).into_iter().flat_map(|(piece, changed)| {
                            git_diff_view::segments(piece).into_iter().map(move |segment| {
                                div().when(changed, |part| part.bg(rgb(marked))).child(segment.text)
                            })
                        }),
                    ),
                };
                div()
                    .id(("git-diff-row", index))
                    .h(px(metrics.row_height))
                    .w_full()
                    .min_w(width)
                    .flex()
                    .items_center()
                    .whitespace_nowrap()
                    .font_family(metrics.family.clone())
                    .text_size(px(metrics.size))
                    .bg(rgb(background))
                    .text_color(rgb(ink))
                    .child(number(row.old_line))
                    .child(number(row.new_line))
                    .child(div().flex_none().w(px(metrics.cell_width * 2.0)).child(prefix))
                    .child(text)
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        this.select_git_rows(index, event.modifiers().shift, cx)
                    }))
                    .into_any_element()
            })
            .collect()
    }

    /// The git panel, as wide as it is drawn right now; nothing while it is closed.
    pub(super) fn render_git_panel(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<gpui::Stateful<Div>> {
        let width = self.panel_px();
        if self.panel_full <= 0.0 || width < 0.5 {
            return None;
        }
        let (host, folder) = self.leaf_repo(self.workspace.focused()?)?;
        let now = unix_now();
        let (changes, commits, body) = match self.git.view(&host, &folder) {
            Some(RepoView::Repo(state)) => {
                let listed = state.files.len() + state.more as usize;
                let commit_count = state.commits.as_ref().map_or(0, Vec::len);
                let body = match self.git_tab {
                    PanelTab::Changes => {
                        let added: u32 = state.files.iter().filter_map(|file| file.added).sum();
                        let removed: u32 = state.files.iter().filter_map(|file| file.removed).sum();
                        let totals = (added + removed > 0).then(|| format!("+{added} −{removed}"));
                        div()
                            .size_full()
                            .flex()
                            .flex_col()
                            .when(state.filters_skipped, |list| {
                                list.child(git_panel::message(
                                    "This repo's git filters are not run here, so some files may show as changed."
                                        .into(),
                                    theme,
                                ))
                            })
                            .child(git_panel::heading("Changed", totals, theme))
                            .child(
                                uniform_list(
                                    "git-panel-files",
                                    state.files.len(),
                                    cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                                        this.render_panel_file_rows(range, cx)
                                    }),
                                )
                                .flex_1()
                                .min_h_0(),
                            )
                            .when(state.more > 0, |list| {
                                list.child(git_panel::message(format!("+{} more files", state.more), theme))
                            })
                            .when(listed == 0, |list| list.child(git_panel::message("No changes".into(), theme)))
                            .into_any_element()
                    }
                    PanelTab::Commits => {
                        let list = state.commits.as_deref().unwrap_or_default();
                        let unpushed = list.iter().filter(|commit| commit.unpushed).count();
                        div()
                            .id("git-panel-commits")
                            .size_full()
                            .overflow_y_scroll()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(git_panel::heading(
                                        &git_panel::commits_heading(state),
                                        (unpushed > 0).then(|| format!("↑{unpushed} unpushed")),
                                        theme,
                                    ))
                                    .children(list.iter().enumerate().map(|(index, commit)| {
                                        let hash = commit.hash.clone();
                                        git_panel::commit_row(index, commit, now, theme).on_click(cx.listener(
                                            move |this, _, window, cx| this.open_git_commit(hash.clone(), window, cx),
                                        ))
                                    }))
                                    .when(list.is_empty(), |col| {
                                        col.child(git_panel::message("No commits yet".into(), theme))
                                    }),
                            )
                            .into_any_element()
                    }
                };
                (listed, commit_count, body)
            }
            Some(RepoView::NotRepo) => {
                (0, 0, git_panel::message("This folder is not in a git repository.".into(), theme).into_any_element())
            }
            Some(RepoView::NoGit) => {
                (0, 0, git_panel::message("git is not installed on this host.".into(), theme).into_any_element())
            }
            Some(RepoView::Failed(reason)) => {
                (0, 0, git_panel::message(format!("git said: {reason}"), theme).into_any_element())
            }
            Some(RepoView::Loading) | None => (0, 0, git_panel::message("Loading…".into(), theme).into_any_element()),
        };
        let tab_changes = git_panel::tab_button(
            "git-tab-changes",
            format!("Changes {changes}"),
            self.git_tab == PanelTab::Changes,
            theme,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.git_tab = PanelTab::Changes;
            cx.notify();
        }));
        let tab_commits = git_panel::tab_button(
            "git-tab-commits",
            format!("Commits {commits}"),
            self.git_tab == PanelTab::Commits,
            theme,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.git_tab = PanelTab::Commits;
            cx.notify();
        }));
        Some(
            div().id("git-panel").absolute().top_0().right_0().h_full().w(px(width)).overflow_hidden().child(
                git_panel::frame(theme)
                    .absolute()
                    .top_0()
                    .right_0()
                    .h_full()
                    .w(px(self.panel_full))
                    .flex()
                    .flex_col()
                    .child(git_panel::branch_line(
                        match self.git.view(&host, &folder) {
                            Some(RepoView::Repo(state)) => Some(state),
                            _ => None,
                        },
                        git_panel::close_button("git-panel-close", theme).on_click(
                            cx.listener(|this, _, window, cx| this.toggle_git_panel(&ToggleGitPanel, window, cx)),
                        ),
                        theme,
                    ))
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .border_b_1()
                            .border_color(rgb(theme.tokens.line))
                            .child(tab_changes)
                            .child(tab_commits),
                    )
                    .child(div().flex_1().min_h_0().child(body)),
            ),
        )
    }
}
