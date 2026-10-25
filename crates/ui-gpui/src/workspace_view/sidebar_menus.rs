use super::{FindSession, OpenRecents, OpenSettings, TakeOverTarget, ToggleSidebar, WorkspaceView};

use app_core::attention::AlertKind;
use gpui::{ClipboardItem, Context, Focusable, MouseButton, MouseMoveEvent, Pixels, Point, PromptLevel, Window};
use protocol::host::HostId;
use protocol::message::SessionState;

use app_core::menus::{self, MenuAction, MenuContext, MenuEntry, MenuItem, Shortcut};
use app_core::session_tree::{Activation, Tone, Unseen, status_tone};

use crate::context_menu::OpenMenu;
use crate::settings_store::SettingsStore;
use crate::sidebar_view::{RenameTarget, SidebarEvent};
use crate::ssh_wizard::WizardStart;
use crate::theme::theme;

const SIDEBAR_MIN: f32 = 160.0;
const SIDEBAR_MAX: f32 = 600.0;

impl WorkspaceView {
    /// Open the panes picked in the sidebar as 2×2 grids, a new tab per four.
    fn open_together(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let leaves = self.sidebar.update(cx, |sidebar, cx| sidebar.take_together(cx));
        self.workspace.open_together(&leaves);
        if let Some(&first) = leaves.first() {
            self.focus_leaf(first, window, cx);
        }
        self.save(cx);
        cx.notify();
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
                self.marked_unread.insert(leaf);
                self.sync_unseen(cx);
                cx.notify();
            }
            MenuAction::MarkRead(leaf) => {
                self.unseen.remove(&leaf);
                self.marked_unread.remove(&leaf);
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
                self.open_new(claude, dir, None, None, host, None)
            }
            MenuAction::NewHere { dir, claude, host } => self.open_on_host(&host, claude, dir, window, cx),
            MenuAction::NewClaudeWith { dir, dir_name } => {
                self.remember_pick(&dir, &dir_name, cx);
                self.open_new(true, dir, None, None, HostId::local(), Some(dir_name));
            }
            MenuAction::PinFavorite(dir) => SettingsStore::update(cx, |settings| {
                if !settings.favorites.contains(&dir) {
                    settings.favorites.push(dir);
                }
            }),
            MenuAction::UnpinFavorite(dir) => {
                SettingsStore::update(cx, |settings| settings.favorites.retain(|favorite| *favorite != dir))
            }
            MenuAction::SetProjectColor { root, color } => SettingsStore::update(cx, |settings| match color {
                Some(color) => {
                    settings.project_colors.folders.insert(root, color);
                }
                None => {
                    settings.project_colors.folders.remove(&root);
                }
            }),
            MenuAction::RecentsFor(project) => self.show_recents(Some(project), window, cx),
            MenuAction::Continue(leaf) => {
                self.focus_leaf(leaf, window, cx);
                self.rerun_leaf(leaf, window, cx);
            }
            MenuAction::OpenTogether => self.open_together(window, cx),
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
                let answer = window.prompt(PromptLevel::Warning, "Open in VS Code", Some(&detail), &["OK"], cx);
                cx.spawn(async move |_, _| {
                    let _ = answer.await;
                })
                .detach();
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
            let context = if leaf.launch.container.is_some() || !leaf.host.is_local() {
                MenuContext::default()
            } else {
                MenuContext::for_folder(cx.global::<SettingsStore>().settings(), &leaf.launch.cwd)
            };
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
            SidebarEvent::TabMenu { index, position } => self.open_tab_menu(index, position, window, cx),
            SidebarEvent::New { claude } => self.show_launcher(claude, window, cx),
            SidebarEvent::PullOut(leaf) => {
                if self.workspace.pull_out(leaf) {
                    self.focus_current(window, cx);
                    self.save(cx);
                    cx.notify();
                }
            }
            SidebarEvent::OpenRecents => self.open_recents(&OpenRecents, window, cx),
            SidebarEvent::Dismiss => self.focus_current(window, cx),
            SidebarEvent::ShowSettings => self.open_settings(&OpenSettings, window, cx),
            SidebarEvent::CloseSettings => self.close_settings(window, cx),
            SidebarEvent::DisconnectHost(host) => self.disconnect_host(&host, window, cx),
            SidebarEvent::ConnectHost(host) => self.run_host_action(&host, window, cx),
            SidebarEvent::AddSshHost => self.open_ssh_wizard(WizardStart::Add, window, cx),
            SidebarEvent::UpdateHost { host, position } => self.click_host_update(&host, position, window, cx),
        }
    }

    /// Start the inline rename on the sidebar, showing the sidebar first if it is hidden.
    fn start_rename(&mut self, target: RenameTarget, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.workspace.sidebar.visible {
            self.workspace.sidebar.visible = true;
            self.save(cx);
            cx.notify();
        }
        self.sidebar.update(cx, |sidebar, cx| sidebar.start_rename(target, &name, window, cx));
    }

    pub(super) fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        let state = &mut self.workspace.sidebar;
        state.visible = !state.visible;
        if !state.visible {
            self.focus_current(window, cx);
        }
        self.save(cx);
        cx.notify();
    }

    pub(super) fn find_session(&mut self, _: &FindSession, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.sidebar.visible = true;
        self.sidebar.focus_handle(cx).focus(window, cx);
        self.save(cx);
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
