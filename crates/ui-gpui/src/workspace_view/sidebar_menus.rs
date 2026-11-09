use super::layout::content_width_beside;
use super::{
    DialogAction, FindSession, OpenRecents, OpenSettings, SIDEBAR_EDGE, SidebarAnim, TakeOverTarget, ToggleSidebar,
    WorkspaceView,
};

use app_core::attention::AlertKind;
use app_core::dialogs;
use app_core::gantry_project::Assign;
use app_core::sidebar_compact;
use gpui::{
    ClipboardItem, Context, Focusable, IntoElement, MouseButton, MouseMoveEvent, Pixels, Point, Window, div,
    prelude::*, px, relative, rgb, svg,
};
use protocol::host::HostId;
use protocol::message::SessionState;
use std::time::Instant;

use app_core::menus::{self, MenuAction, MenuContext, MenuEntry, MenuItem, Shortcut};
use app_core::session_tree::{Activation, Tone, Unseen, status_tone};
use app_core::workspace::PaneLaunch;

use crate::assets::{CHEVRON_LEFT_ICON, CHEVRON_RIGHT_ICON};
use crate::context_menu::OpenMenu;
use crate::hover_card::{self, HoverCard};
use crate::settings_store::{SettingsStore, shortcut_label};
use crate::sidebar_view::{RenameTarget, SidebarEvent};
use crate::ssh_wizard::WizardStart;
use crate::theme::theme;

const SIDEBAR_MIN: f32 = 160.0;
const SIDEBAR_MAX: f32 = 600.0;

impl WorkspaceView {
    /// The tab on the sidebar's outer edge, halfway down, that collapses or expands it: drawn only while the pointer
    /// is over the sidebar or the tab, and fused to the edge so it follows it while the width animates.
    pub(super) fn render_sidebar_toggle(&self, sidebar_px: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        let compact = self.sidebar_amount() > 0.5;
        let shown = self.sidebar_hovered || self.toggle_hovered;
        div()
            .id("sidebar-toggle")
            .absolute()
            .left(theme.ui(sidebar_px) + px(SIDEBAR_EDGE))
            .top(relative(0.5))
            .mt(theme.ui(-23.))
            .w(theme.ui(16.))
            .h(theme.ui(46.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_r(theme.ui(10.))
            .bg(rgb(theme.tokens.raised))
            .border_1()
            .border_l_0()
            .border_color(rgb(theme.tokens.line))
            .cursor_pointer()
            .opacity(if shown { 1.0 } else { 0.0 })
            .hover(|style| style.border_color(rgb(theme.tokens.accent)))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.toggle_hovered = *hovered;
                cx.notify();
            }))
            .child(
                svg()
                    .path(if compact { CHEVRON_RIGHT_ICON } else { CHEVRON_LEFT_ICON })
                    .size(theme.ui(12.))
                    .text_color(rgb(if self.toggle_hovered { theme.tokens.accent } else { theme.tokens.muted })),
            )
            .tooltip(hover_card::tooltip(HoverCard {
                title: if compact { "Expand sidebar" } else { "Collapse sidebar" }.into(),
                status: Some(shortcut_label(cx.global::<SettingsStore>().settings(), "toggleSidebar")),
                dir: None,
                container: false,
                host: None,
                folder: None,
            }))
            .on_click(
                cx.listener(|this, _, window, cx| {
                    this.set_sidebar_compact(!this.workspace.sidebar.compact, window, cx)
                }),
            )
    }

    /// Do what a right-click menu entry says.
    pub(super) fn run_menu_action(&mut self, action: MenuAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            MenuAction::Split { tab, axis } => {
                // The right-clicked tab, which becomes the visible one.
                if self.workspace.select_tab(tab) {
                    self.focus_current(window, cx);
                    self.open(true, Some(axis), window, cx);
                }
            }
            MenuAction::SplitPane { leaf, axis, claude } => {
                // The pane whose button was clicked, which takes focus before it splits (it may be gone by now).
                if self.workspace.focus(leaf) {
                    self.focus_current(window, cx);
                    self.save(cx);
                    self.open(claude, Some(axis), window, cx);
                }
            }
            MenuAction::Copy(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            MenuAction::OpenFolder(dir) => cx.open_with_system(&dir),
            MenuAction::OpenInVsCode(args) => self.open_in_vscode(args, window, cx),
            MenuAction::MarkUnread(leaf) => {
                self.unseen.insert(leaf, AlertKind::Finished);
                if self.workspace.set_unread(leaf, true) {
                    self.save(cx);
                }
                self.sync_unseen(cx);
                cx.notify();
            }
            MenuAction::MarkRead(leaf) => {
                self.unseen.remove(&leaf);
                if self.workspace.set_unread(leaf, false) {
                    self.save(cx);
                }
                self.sync_unseen(cx);
                cx.notify();
            }
            MenuAction::RenamePane(leaf) => {
                let name = self.workspace.leaf_name(leaf).unwrap_or_default().to_string();
                self.start_rename(RenameTarget::Pane(leaf), name, window, cx);
            }
            MenuAction::RenameTab(index) => {
                let name = self.workspace.tab_name(index).unwrap_or_default().to_string();
                self.start_rename(RenameTarget::Tab(index), name, window, cx);
            }
            MenuAction::ClosePane(leaf) => self.confirm_close(vec![leaf], window, cx),
            MenuAction::CloseTab(index) => self.close_tab_at(index, window, cx),
            MenuAction::NewHere { dir, claude, host } if host.is_local() => {
                self.open_new(claude, dir, None, None, host, None, None)
            }
            MenuAction::NewHere { dir, claude, host } => self.open_on_host(&host, claude, dir, None, window, cx),
            MenuAction::NewClaudeWith { dir, dir_name } => {
                self.remember_pick(&dir, &dir_name, cx);
                self.open_new(true, dir, None, None, HostId::local(), Some(dir_name), None);
            }
            MenuAction::NewInProject { name, claude: true } => self.new_claude_in_project(&name, window, cx),
            MenuAction::NewInProject { name, claude: false } => {
                // A shell at once, in the home directory of this machine.
                let home = std::env::home_dir().unwrap_or_else(|| self.default_cwd.clone());
                let shell = cx.global::<SettingsStore>().settings().shell.clone();
                self.open_launch(PaneLaunch::shell(home, &shell), None, None, HostId::local(), Some(name), window, cx);
            }
            MenuAction::RemoveFromProject(leaf) => {
                self.on_sidebar_event(
                    SidebarEvent::AssignToProject { source: Assign::Leaf(leaf), project: None },
                    window,
                    cx,
                );
            }
            MenuAction::RenameProject(name) => {
                self.set_sidebar_compact(false, window, cx);
                self.sidebar.update(cx, |sidebar, cx| sidebar.start_project_rename(&name, window, cx));
            }
            MenuAction::DeleteProject(name) => self.delete_project(&name, window, cx),
            MenuAction::SetProjectColor { root, color } => SettingsStore::update(cx, |settings| match color {
                Some(color) => {
                    settings.project_colors.folders.insert(root, color);
                }
                None => {
                    settings.project_colors.folders.remove(&root);
                }
            }),
            MenuAction::SetPaneColor { leaf, color } => {
                if self.workspace.set_leaf_color(leaf, color) {
                    self.save(cx);
                    self.sync_panes(cx);
                    cx.notify();
                }
            }
            MenuAction::CustomColor(target) => self.show_color_picker(target, window, cx),
            MenuAction::RecentsFor(project) => self.show_recents(Some(project), window, cx),
            MenuAction::Continue(leaf) => {
                self.focus_leaf(leaf, window, cx);
                self.rerun_leaf(leaf, window, cx);
            }
            MenuAction::TakeOver { session, cwd, host } => self.on_sidebar_event(
                SidebarEvent::Activate { activation: Activation::TakeOver { session, cwd, host }, split: false },
                window,
                cx,
            ),
        }
    }

    /// Start VS Code (`code <args>`), detached; say so if it cannot start.
    fn open_in_vscode(&mut self, args: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let started = std::process::Command::new("code")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match started {
            // `code` hands off to VS Code and exits; reap it so it does not linger as a zombie.
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(error) => {
                let detail = format!("Could not start VS Code (`code`): {error}");
                self.open_dialog(dialogs::notice("Open in VS Code", &detail), DialogAction::Ack, window, cx);
            }
        }
    }

    /// Open tab `index`'s right-click menu at `position` (it acts on that tab, not the visible one).
    pub(super) fn open_tab_menu(
        &mut self,
        index: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tabs.get(index) else { return };
        let items = if tab.settings {
            vec![MenuItem::Entry(MenuEntry {
                label: "Close tab".into(),
                shortcut: Some(Shortcut::Close),
                accent: false,
                action: MenuAction::CloseTab(index),
                swatch: None,
                checked: false,
            })]
        } else {
            let Some(leaf) = self.workspace.leaves.get(&tab.focused) else { return };
            let sessions = self.sidebar.read(cx).sessions();
            let place = leaf
                .session
                .as_deref()
                .and_then(|id| sessions.get(&leaf.host, id))
                .and_then(|info| info.container.clone());
            let vscode = menus::vscode_args(&leaf.launch.cwd, place.as_ref(), leaf.launch.container.as_deref());
            let mut context = MenuContext::default();
            // A split tab's heading would color whichever pane has focus; its panes' own rows have the menu.
            if tab.root.leaves().len() == 1 {
                context.pane_color = Some((tab.focused, leaf.color));
            }
            let idle = leaf.session.as_deref().and_then(|id| sessions.get(&leaf.host, id)).is_some_and(|info| {
                matches!(info.state, SessionState::Managed { .. })
                    && status_tone(info.status.as_ref(), Unseen::No) == Tone::Idle
            });
            let unread = self.unseen.contains_key(&tab.focused);
            let mark = (idle || unread).then_some((tab.focused, unread));
            menus::tab_menu(index, &leaf.host, &leaf.launch.cwd, vscode, &context, mark, tab.root.leaves().len())
        };
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.tab_menu = Some(OpenMenu::new(items, position, focus));
        cx.notify();
    }

    pub(super) fn on_sidebar_event(&mut self, event: SidebarEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Picking a row or starting something new leaves the git page.
        if matches!(event, SidebarEvent::Activate { .. } | SidebarEvent::New { .. } | SidebarEvent::PullOut(_)) {
            self.close_git_page(window, cx);
        }
        match event {
            SidebarEvent::Activate { activation, split } => match activation {
                Activation::FocusPane(key) => {
                    if let Some(leaf) = self.workspace.leaf_for_pane(&key) {
                        self.focus_leaf(leaf, window, cx);
                    }
                }
                Activation::FocusLeaf(leaf) => self.focus_leaf(leaf, window, cx),
                Activation::Resume { session, cwd, host } => self.resume_session(host, session, cwd, split, window, cx),
                Activation::TakeOver { session, cwd, host } => {
                    let target = match self.workspace.leaf_for_session(&host, &session) {
                        Some(leaf) if self.workspace.leaves[&leaf].pane.is_none() => TakeOverTarget::Leaf(leaf),
                        _ => {
                            let sessions = self.sidebar.read(cx).sessions();
                            let container = sessions
                                .get(&host, &session)
                                .and_then(|info| info.container.clone())
                                .and_then(|place| place.host_folder);
                            TakeOverTarget::NewPane { cwd, split, container }
                        }
                    };
                    self.confirm_take_over(host, session, target, window, cx);
                }
                Activation::ToggleDetails(_) | Activation::ToggleProject(_) | Activation::Nothing => {}
            },
            SidebarEvent::Menu(action) => self.run_menu_action(action, window, cx),
            SidebarEvent::Rename { target, name } => {
                let renamed = match target {
                    RenameTarget::Pane(leaf) => self.workspace.rename_leaf(leaf, &name),
                    RenameTarget::Tab(index) => self.workspace.rename_tab(index, &name),
                };
                if renamed {
                    self.save(cx);
                    cx.notify();
                }
            }
            SidebarEvent::CloseGit => self.close_git_page(window, cx),
            SidebarEvent::StartProject => {
                self.set_sidebar_compact(false, window, cx);
                self.sidebar.update(cx, |sidebar, cx| sidebar.start_new_project(window, cx));
            }
            SidebarEvent::NewProject(name) => {
                if self.workspace.create_project(&name).is_ok() {
                    self.save(cx);
                    cx.notify();
                }
            }
            SidebarEvent::RenameProject { from, to } => self.rename_project(&from, &to, cx),
            SidebarEvent::DeleteProject(name) => self.delete_project(&name, window, cx),
            SidebarEvent::AssignToProject { source, project } => {
                let assigned = self.workspace.assign_to_project(&source, project.as_deref());
                match (assigned, &source, &project) {
                    (true, ..) => {
                        self.focus_current(window, cx);
                        self.save(cx);
                        cx.notify();
                    }
                    // Dropped on a folder heading with nothing to take it out of: as on any sidebar space, a
                    // pane of a split tab becomes a tab of its own.
                    (false, Assign::Leaf(leaf), None) if self.workspace.pull_out(*leaf) => {
                        self.focus_current(window, cx);
                        self.save(cx);
                        cx.notify();
                    }
                    _ => {}
                }
            }
            SidebarEvent::TabMenu { index, position } => self.open_tab_menu(index, position, window, cx),
            SidebarEvent::New { claude } => self.show_launcher(claude, window, cx),
            SidebarEvent::PullOut(leaf) => {
                if self.workspace.pull_out(leaf) {
                    self.focus_current(window, cx);
                    self.save(cx);
                    cx.notify();
                }
            }
            SidebarEvent::OrderChanged(order) => {
                self.workspace.sidebar_order = order;
                self.save(cx);
            }
            SidebarEvent::OpenRecents => self.open_recents(&OpenRecents, window, cx),
            SidebarEvent::Dismiss => self.focus_current(window, cx),
            SidebarEvent::ShowSettings => self.open_settings(&OpenSettings, window, cx),
            SidebarEvent::CloseSettings => self.close_settings(window, cx),
            SidebarEvent::DisconnectHost(host) => self.disconnect_host(&host, window, cx),
            SidebarEvent::ConnectHost(host) => self.run_host_action(&host, window, cx),
            SidebarEvent::HostsCollapsed(collapsed) => {
                self.workspace.sidebar.hosts_collapsed = collapsed;
                self.save(cx);
            }
            SidebarEvent::ToggleCompact => self.set_sidebar_compact(!self.workspace.sidebar.compact, window, cx),
            SidebarEvent::AddSshHost => self.open_ssh_wizard(WizardStart::Add, window, cx),
            SidebarEvent::UpdateHost { host, position } => self.click_host_update(&host, position, window, cx),
        }
    }

    /// Start the inline rename on the sidebar, showing the sidebar first if it is hidden.
    fn start_rename(&mut self, target: RenameTarget, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.set_sidebar_compact(false, window, cx);
        self.sidebar.update(cx, |sidebar, cx| sidebar.start_rename(target, &name, window, cx));
    }

    pub(super) fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        self.set_sidebar_compact(!self.workspace.sidebar.compact, window, cx);
    }

    /// Collapse the sidebar to its compact column or expand it, with the animation when it is on. Collapsing moves the
    /// focus to the pane: the filter field it may have had is gone.
    pub(super) fn set_sidebar_compact(&mut self, compact: bool, window: &mut Window, cx: &mut Context<Self>) {
        let was = self.workspace.sidebar.compact;
        if was == compact {
            return;
        }
        let theme = theme(cx).clone();
        let at = |compact: bool| sidebar_compact::width(self.workspace.sidebar.width, f32::from(u8::from(compact)));
        // The width the tab content has once this ends, from the body's width, so that it is right on every screen
        // (a pane tab, Settings, the welcome screen) and when this reverses an animation under way.
        let content_width = content_width_beside(self.body_width, theme.ui(at(compact)));
        let animate =
            cx.global::<SettingsStore>().settings().animate_ui && window.is_window_active() && self.body_width > px(0.);
        let from = self.sidebar_amount();
        self.workspace.sidebar.compact = compact;
        self.sidebar_anim = animate.then(|| SidebarAnim {
            started: Instant::now(),
            from,
            to: f32::from(u8::from(compact)),
            content_width,
        });
        if compact {
            self.focus_current(window, cx);
        }
        self.save(cx);
        cx.notify();
    }

    /// How compact the sidebar is drawn right now: 0 = expanded, 1 = compact, between while it animates.
    pub(super) fn sidebar_amount(&self) -> f32 {
        match &self.sidebar_anim {
            Some(anim) => sidebar_compact::amount(anim.from, anim.to, anim.started.elapsed().as_secs_f32() * 1000.0),
            None => f32::from(u8::from(self.workspace.sidebar.compact)),
        }
    }

    pub(super) fn find_session(&mut self, _: &FindSession, window: &mut Window, cx: &mut Context<Self>) {
        self.set_sidebar_compact(false, window, cx);
        self.sidebar.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub(super) fn drag_sidebar(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ui_scale = theme(cx).ui_scale;
        if !self.resizing_sidebar {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.resizing_sidebar = false;
            self.save(cx);
            return;
        }
        let width = f32::from(event.position.x - self.sidebar_bounds.origin.x) / ui_scale;
        self.workspace.sidebar.width = width.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        cx.notify();
    }
}
