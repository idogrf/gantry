use super::layout::start_rects;
use super::{PendingOpen, Slot, WorkspaceView};
use std::path::{Path, PathBuf};
use std::time::Duration;

use app_core::attention::without_claude_glyph;
use app_core::host_state::HostState;
use app_core::split_tree::{Axis, NodeId};
use app_core::workspace::{DirBadge, PaneLaunch};
use app_core::{layout_store, window_state};
use daemon::containers::GONE as CONTAINER_GONE;
use gpui::{App, Context, Focusable, PromptLevel, Window, WindowBounds, prelude::*};
use protocol::host::{HostId, PaneKey};
use protocol::message::ContainerChoice;

use app_core::session_tree::{PaneEntry, TabEntry};
use app_core::settings::ShellSettings;

use crate::settings_store::{SettingsStore, dir_badge_for};
use crate::terminal_view::{PaneEvent, TerminalView};
use crate::theme::theme;

impl WorkspaceView {
    pub(super) fn add_slot(&mut self, leaf: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let launch = self.workspace.leaves[&leaf].launch.clone();
        let host = self.workspace.leaves[&leaf].host.clone();
        let client = self.client(&host).cloned();
        let remote = !host.is_local();
        let view = cx.new(|cx| TerminalView::new(client, launch, remote, window, cx));
        let subscription = cx.subscribe_in(&view, window, move |this, _, event: &PaneEvent, window, cx| {
            this.on_pane_event(leaf, *event, window, cx)
        });
        self.slots.insert(leaf, Slot { view, _subscription: subscription });
    }

    pub(super) fn spawn_leaf(
        &mut self,
        leaf: NodeId,
        cwd_from: Option<PaneKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.slots.get(&leaf).map(|slot| slot.view.clone()) else { return };
        let host = self.workspace.leaves[&leaf].host.clone();
        let Some(client) = self.client(&host).cloned() else {
            if self.ssh_hosts.iter().any(|entry| entry.name == host.name()) {
                // A listed SSH host: the leaf waits for the connection, which starts now if it can.
                let waiting = self.queued.entry(host.clone()).or_default();
                if !waiting.contains(&leaf) {
                    waiting.push(leaf);
                }
                self.refresh_host_cards(&host, cx);
                if self.host_state(&host).can_connect() {
                    self.connect_host(&host, true, window, cx);
                }
                return;
            }
            let message = format!("Host {} is not connected", host.name());
            view.update(cx, |view, cx| view.failed(message, cx));
            return;
        };
        let (cols, rows) = view.read(cx).grid_size();
        let scrollback = cx.global::<SettingsStore>().settings().window.scrollback;
        // Pane ids belong to one daemon: only a pane of the same host can lend its directory.
        let cwd_from = cwd_from.filter(|key| key.host == host).map(|key| key.pane);
        self.next_request += 1;
        self.pending.insert(self.next_request, leaf);
        client.spawn(self.next_request, self.workspace.leaves[&leaf].launch.request(cwd_from, cols, rows, scrollback));
    }

    /// The badge of the Claude config dir `leaf` runs in, when two or more are configured (never on an SSH host).
    pub(super) fn leaf_dir_badge(&self, leaf: NodeId, cx: &App) -> Option<DirBadge> {
        let entry = self.workspace.leaves.get(&leaf)?;
        if !entry.launch.is_claude() || entry.launch.container.is_some() || !entry.host.is_local() {
            return None;
        }
        let recorded = entry
            .session
            .as_deref()
            .and_then(|id| self.sidebar.read(cx).sessions().get(&entry.host, id))
            .and_then(|info| info.claude_dir.clone());
        let path = recorded.or_else(|| entry.launch.claude_dir.clone()).or_else(|| self.daemon_dir.clone());
        dir_badge_for(cx.global::<SettingsStore>().settings(), path.as_deref(), &entry.host)
    }

    /// Whether `leaf` runs on an SSH host that is not connected (it gets the Closed look).
    pub(super) fn leaf_offline(&self, leaf: NodeId) -> bool {
        self.workspace
            .leaves
            .get(&leaf)
            .is_some_and(|leaf| !leaf.host.is_local() && self.host_state(&leaf.host) != HostState::Connected)
    }

    /// The color of the project `leaf` belongs to (its session's directory, else the pane's), if project colors are on.
    pub(super) fn leaf_project_color(&self, leaf: NodeId, cx: &App) -> Option<u32> {
        let cwd = self.leaf_cwd(leaf, cx)?;
        let host = self.workspace.leaves.get(&leaf)?.host.clone();
        theme(cx).project(&self.sidebar.read(cx).project_of(&host, &cwd))
    }

    /// The directory `leaf` works in: its session's, else the pane's.
    pub(super) fn leaf_cwd(&self, leaf: NodeId, cx: &App) -> Option<PathBuf> {
        let entry = self.workspace.leaves.get(&leaf)?;
        let sessions = self.sidebar.read(cx).sessions();
        Some(
            entry
                .session
                .as_deref()
                .and_then(|id| sessions.get(&entry.host, id))
                .map_or(&entry.launch.cwd, |info| &info.cwd)
                .clone(),
        )
    }

    fn on_pane_event(&mut self, leaf: NodeId, event: PaneEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            PaneEvent::Focused => self.focus_leaf(leaf, window, cx),
            PaneEvent::Rerun => self.rerun_leaf(leaf, window, cx),
            PaneEvent::Close => self.close_leaf(leaf, window, cx),
            PaneEvent::HostAction => {
                if let Some(host) = self.workspace.leaves.get(&leaf).map(|leaf| leaf.host.clone()) {
                    self.run_host_action(&host, window, cx);
                }
            }
            PaneEvent::TitleChanged => {
                self.sync_panes(cx);
                cx.notify()
            }
        }
    }

    /// Open a new Claude or shell pane — as a new tab, or splitting the focused pane along `axis` —
    /// in the focused pane's current directory, on its host.
    pub(super) fn open(&mut self, claude: bool, axis: Option<Axis>, window: &mut Window, cx: &mut Context<Self>) {
        if axis.is_some() && (self.settings_active() || !self.workspace.can_split()) {
            return;
        }
        let source = self.workspace.focused().and_then(|leaf| self.workspace.leaves.get(&leaf));
        // A remote pane whose host is neither listed nor connected ("Not listed") starts a local tab.
        let source = source.filter(|leaf| {
            leaf.host.is_local()
                || self.client(&leaf.host).is_some()
                || self.ssh_hosts.iter().any(|entry| entry.name == leaf.host.name())
        });
        let cwd = source.map_or_else(|| self.default_cwd.clone(), |leaf| leaf.launch.cwd.clone());
        let cwd_from = source.and_then(|leaf| leaf.key());
        let host = source.map(|leaf| leaf.host.clone()).unwrap_or_default();
        if host.is_local() {
            self.open_new(claude, cwd, axis, cwd_from, host, None);
        } else {
            // A new tab or split of a remote pane: no container lookup and no local Claude dir or shell settings.
            let launch =
                if claude { PaneLaunch::claude(cwd) } else { PaneLaunch::shell(cwd, &ShellSettings::default()) };
            self.open_launch(launch, axis, cwd_from, host, window, cx);
        }
    }

    /// The Settings tab is the active one (it has no pane to split).
    pub(super) fn settings_active(&self) -> bool {
        self.workspace.settings_tab() == Some(self.workspace.active)
    }

    /// Start a new Claude or shell pane in `cwd` — after asking where, when a dev container mounts `cwd`.
    pub(super) fn open_new(
        &mut self,
        claude: bool,
        cwd: PathBuf,
        axis: Option<Axis>,
        cwd_from: Option<PaneKey>,
        host: HostId,
        pick: Option<String>,
    ) {
        self.next_request += 1;
        self.local_client().find_container(self.next_request, &cwd);
        self.pending_opens.insert(self.next_request, PendingOpen { claude, pick, cwd, axis, cwd_from, host });
    }

    /// Start a new Claude session (or shell) in `cwd`: Claude with the config dir `pick` names, else the one remembered
    /// for exactly this folder, else the default.
    fn new_launch(
        &self,
        claude: bool,
        cwd: PathBuf,
        pick: Option<&str>,
        container: Option<PathBuf>,
        cx: &App,
    ) -> PaneLaunch {
        let settings = cx.global::<SettingsStore>().settings();
        if !claude {
            return PaneLaunch::shell(cwd, &settings.shell).in_container(container);
        }
        let dir = self.dirs.for_folder(&cwd, pick, &settings.folder_claude_dirs);
        PaneLaunch::claude_with(cwd, dir, self.daemon_dir.as_deref(), container)
    }

    /// A new session from the launcher; an explicit config dir pick is remembered for the folder.
    pub(super) fn open_launcher_pick(
        &mut self,
        claude: bool,
        dir: PathBuf,
        dir_pick: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(name) = &dir_pick {
            self.remember_pick(&dir, name, cx);
        }
        self.open_new(claude, dir, None, None, HostId::local(), dir_pick);
    }

    /// An explicit pick of a config dir for `folder` is remembered for it; picking the default forgets it.
    pub(super) fn remember_pick(&self, folder: &Path, name: &str, cx: &mut App) {
        let is_default = self.dirs.dirs.get(self.dirs.default).is_some_and(|dir| dir.name == name);
        let (folder, name) = (folder.to_path_buf(), name.to_string());
        SettingsStore::update(cx, move |settings| {
            if is_default {
                settings.folder_claude_dirs.remove(&folder);
            } else {
                settings.folder_claude_dirs.insert(folder, name);
            }
        });
    }

    /// The daemon said which dev container (if any) mounts a waiting session's directory: open it locally, or ask.
    pub(super) fn on_container_found(
        &mut self,
        request: u64,
        container: Option<ContainerChoice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(PendingOpen { claude, pick, cwd, axis, cwd_from, host }) = self.pending_opens.remove(&request) else {
            return;
        };
        let Some(container) = container else {
            let launch = self.new_launch(claude, cwd, pick.as_deref(), None, cx);
            self.open_launch(launch, axis, cwd_from, host, window, cx);
            return;
        };
        let folder = &container.host_folder;
        let project =
            folder.file_name().map_or_else(|| folder.display().to_string(), |name| name.to_string_lossy().into_owned());
        let state = if container.running { "running" } else { "stopped, starts on open" };
        let detail = format!("{} · {state}", container.name);
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("Open {project} in"),
            Some(&detail),
            &["Dev container", "This machine", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await;
            let _ = this.update_in(cx, |this, window, cx| match answer {
                Ok(0) => {
                    // Splits of a container pane start in the session's directory, not the docker client's.
                    let launch = this.new_launch(claude, cwd, pick.as_deref(), Some(container.host_folder), cx);
                    this.open_launch(launch, axis, None, HostId::local(), window, cx)
                }
                Ok(1) => {
                    let launch = this.new_launch(claude, cwd, pick.as_deref(), None, cx);
                    this.open_launch(launch, axis, cwd_from, host, window, cx)
                }
                _ => this.focus_current(window, cx),
            });
        })
        .detach();
    }

    /// Say why a container session cannot be opened.
    pub(super) fn container_gone(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(PromptLevel::Info, "Cannot open this session", Some(CONTAINER_GONE), &["OK"], cx);
        cx.spawn(async move |_, _| {
            let _ = answer.await;
        })
        .detach();
    }

    /// Start `launch` in a new tab, or splitting the focused pane along `axis` (a split runs on `host`).
    pub(crate) fn open_launch(
        &mut self,
        launch: PaneLaunch,
        axis: Option<Axis>,
        cwd_from: Option<PaneKey>,
        host: HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if axis.is_some() && (self.settings_active() || !self.workspace.can_split()) {
            return;
        }
        let target = self.workspace.focused();
        let from = axis.map(|_| self.drawn_rects());
        let leaf = match axis.and_then(|axis| self.workspace.split_on(axis, launch.clone(), host.clone())) {
            Some(leaf) => {
                if let (Some(axis), Some(from), Some(target)) = (axis, from, target) {
                    self.start_pane_anim(start_rects(&from, target, leaf, axis), Some((leaf, axis)), window, cx);
                }
                leaf
            }
            None => self.workspace.new_tab_on(launch, host),
        };
        self.add_slot(leaf, window, cx);
        self.spawn_leaf(leaf, cwd_from, window, cx);
        self.focus_current(window, cx);
        self.save(cx);
    }

    fn close_leaf(&mut self, leaf: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.drawn_rects();
        // A pane of the visible tab that keeps other panes: the survivors grow into its space.
        let swipe = from.len() > 1 && from.iter().any(|(drawn, _)| *drawn == leaf);
        let removed = self.workspace.close(leaf);
        if swipe {
            self.start_pane_anim(from, None, window, cx);
        } else {
            self.cancel_pane_anim();
        }
        if let Some(key) = removed.and_then(|removed| removed.key()) {
            if let Some(client) = self.client(&key.host) {
                client.kill(key.pane);
            } else if let Some(instance) = self.workspace.instances.get(&key.host) {
                // The host is away: kill the pane when it is back, and do not adopt it as a tab then.
                self.killed_offline.push((key.host.clone(), instance.clone(), key.pane));
            }
        }
        self.slots.remove(&leaf);
        self.marked_unread.remove(&leaf);
        if self.unseen.remove(&leaf).is_some() {
            self.sync_unseen(cx);
        }
        self.pending.retain(|_, waiting| *waiting != leaf);
        for waiting in self.queued.values_mut() {
            waiting.retain(|queued| *queued != leaf);
        }
        self.focus_current(window, cx);
        self.save(cx);
    }

    pub(super) fn focus_leaf(&mut self, leaf: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace.focus(leaf) {
            self.focus_current(window, cx);
            self.save(cx);
        }
    }

    /// Give keyboard focus to the workspace's focused leaf (or the empty window).
    pub(super) fn focus_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Another tab is showing: none of the animated panes is in it.
        if let Some(anim) = &self.pane_anim {
            let leaves = self.workspace.active_tab().map(|tab| tab.root.leaves()).unwrap_or_default();
            if !anim.from.iter().any(|(leaf, _)| leaves.contains(leaf)) {
                self.cancel_pane_anim();
            }
        }
        self.workspace.note_visit();
        if let Some((setup, _)) = &self.setup {
            setup.focus_handle(cx).focus(window, cx);
            return;
        }
        if let Some((askpass, _)) = &self.askpass {
            askpass.focus_handle(cx).focus(window, cx);
            return;
        }
        if let Some((wizard, _)) = &self.ssh_wizard {
            wizard.focus_handle(cx).focus(window, cx);
            return;
        }
        if self.git_page.is_some() {
            self.git_focus.focus(window, cx);
            return;
        }
        let (open, active) = (self.workspace.settings_tab().is_some(), self.settings_active());
        self.sidebar.update(cx, |sidebar, cx| sidebar.set_settings(open, active, cx));
        let settings =
            self.workspace.active_tab().is_some_and(|tab| tab.settings).then(|| self.settings.clone()).flatten();
        if let Some(settings) = settings {
            settings.focus_handle(cx).focus(window, cx);
            self.sidebar.update(cx, |sidebar, cx| sidebar.set_focused(None, cx));
            self.clear_seen(window, cx);
            return;
        }
        match self.workspace.focused().and_then(|leaf| self.slots.get(&leaf).map(|slot| (leaf, slot))) {
            Some((leaf, slot)) => {
                slot.view.focus_handle(cx).focus(window, cx);
                if let Some(key) = self.workspace.leaves[&leaf].key()
                    && let Some(client) = self.client(&key.host)
                {
                    client.focus(key.pane);
                }
            }
            None => self.focus_handle.focus(window, cx),
        }
        let focused = self.workspace.focused();
        self.sidebar.update(cx, |sidebar, cx| sidebar.set_focused(focused, cx));
        self.clear_seen(window, cx);
    }

    /// Write the layout (atomically) and refresh the sidebar's pane entries. With "Start with one new tab" the
    /// finished leaves dropped at startup are written back too, so switching back restores them.
    pub(super) fn save(&self, cx: &mut Context<Self>) {
        if let Err(err) = layout_store::save(&self.layout_file, &self.workspace) {
            tracing::warn!("saving the layout failed: {err}");
        }
        self.sync_panes(cx);
    }

    /// Tab `index`'s title as the title bar shows it when no pane row gives one: the focused pane's, without the
    /// Claude glyph while the tab has a session status.
    pub(super) fn tab_title(&self, index: usize, cx: &App) -> String {
        let Some(tab) = self.workspace.tabs.get(index) else { return String::new() };
        if tab.settings {
            return "Settings".to_string();
        }
        let title = self.slots.get(&tab.focused).map_or_else(String::new, |slot| slot.view.read(cx).title());
        if self.workspace.tab_status(index, self.sidebar.read(cx).sessions()).is_some() {
            without_claude_glyph(&title).to_string()
        } else {
            title
        }
    }

    pub(super) fn sync_panes(&self, cx: &mut Context<Self>) {
        self.publish_hosts(cx);
        let panes: Vec<PaneEntry> = self
            .workspace
            .leaves
            .iter()
            .filter(|(_, leaf)| !self.is_away(&leaf.host))
            .map(|(id, leaf)| {
                let view = self.slots.get(id).map(|slot| slot.view.read(cx));
                PaneEntry {
                    leaf: *id,
                    host: leaf.host.clone(),
                    cwd: leaf.launch.cwd.clone(),
                    title: leaf
                        .name
                        .clone()
                        .unwrap_or_else(|| view.map_or_else(|| leaf.launch.label(), |view| view.title())),
                    name: leaf.name.clone(),
                    session: leaf.session.clone(),
                    claude: leaf.launch.is_claude(),
                    ended: view.is_some_and(|view| view.has_ended()),
                    offline: self.leaf_offline(*id),
                    alone: self
                        .workspace
                        .tab_of(*id)
                        .is_some_and(|tab| self.workspace.tabs[tab].root.leaves().len() == 1),
                    container: leaf.launch.container.clone(),
                }
            })
            .collect();
        // A tab can mix hosts: it lists the panes whose host is live, and is not listed when none is.
        let tabs: Vec<TabEntry> = self
            .workspace
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| !tab.settings)
            .filter_map(|(index, tab)| {
                let leaves: Vec<NodeId> = tab
                    .root
                    .leaves()
                    .into_iter()
                    .filter(|leaf| !self.is_away(&self.workspace.leaves[leaf].host))
                    .collect();
                let focused = if leaves.contains(&tab.focused) { tab.focused } else { *leaves.first()? };
                Some(TabEntry { index, name: tab.name.clone(), focused, leaves })
            })
            .collect();
        self.sidebar.update(cx, |sidebar, cx| sidebar.set_panes(panes, tabs, cx));
    }

    /// Start a new Claude session (or shell) in the folder `dir` of a remote host in a new tab; connects the host when
    /// it is not connected. No container lookup and no Claude config dir: the remote uses its own.
    pub(super) fn open_on_host(
        &mut self,
        host: &HostId,
        claude: bool,
        dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let launch = if claude { PaneLaunch::claude(dir) } else { PaneLaunch::shell(dir, &ShellSettings::default()) };
        let leaf = self.workspace.new_tab_on(launch, host.clone());
        self.add_slot(leaf, window, cx);
        self.spawn_leaf(leaf, None, window, cx); // queues the leaf and connects when the host is not connected
        self.focus_current(window, cx);
        self.save(cx);
    }

    pub(super) fn running_panes(&self, leaves: &[NodeId], cx: &Context<Self>) -> usize {
        leaves.iter().filter(|leaf| self.slots.get(leaf).is_some_and(|slot| slot.view.read(cx).is_running())).count()
    }

    /// Window close requested: true to close now; otherwise asks Detach / Stop all / Cancel.
    pub fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (bounds, maximized) = match window.window_bounds() {
            WindowBounds::Windowed(bounds) | WindowBounds::Fullscreen(bounds) => (bounds, false),
            WindowBounds::Maximized(bounds) => (bounds, true),
        };
        let state = window_state::WindowState {
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
            maximized,
        };
        if let Err(err) = window_state::save(&window_state::file_beside(&self.layout_file), &state) {
            tracing::warn!("saving the window size failed: {err}");
        }
        // Stop all only reaches this machine's daemon: SSH hosts are just disconnected and keep running.
        let local: Vec<NodeId> =
            self.workspace.leaves.iter().filter(|(_, leaf)| leaf.host.is_local()).map(|(id, _)| *id).collect();
        if self.running_panes(&local, cx) == 0 {
            return true;
        }
        let remote_running = self.workspace.leaves.iter().any(|(id, leaf)| {
            !leaf.host.is_local() && self.slots.get(id).is_some_and(|slot| slot.view.read(cx).is_running())
        });
        let detail = if remote_running {
            "Sessions keep running after Detach. Stop all sessions ends this machine's sessions only; sessions on SSH hosts keep running."
        } else {
            "Sessions keep running after Detach."
        };
        if self.prompting.replace(true) {
            return false;
        }
        let answer = window.prompt(
            PromptLevel::Info,
            "Quit Gantry?",
            Some(detail),
            &["Detach", "Stop all sessions", "Cancel"],
            cx,
        );
        let (client, prompting) = (self.local_client().clone(), self.prompting.clone());
        cx.spawn(async move |_, cx| match answer.await {
            Ok(0) => cx.update(|cx| cx.quit()),
            Ok(1) => {
                client.stop_all_and_wait(Duration::from_secs(2));
                cx.update(|cx| cx.quit());
            }
            _ => prompting.set(false),
        })
        .detach();
        false
    }

    /// Close tab `index`: the Settings tab right away, any other after `confirm_close` of its leaves.
    pub(super) fn close_tab_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        match self.workspace.tabs.get(index) {
            Some(tab) if tab.settings => self.close_settings(window, cx),
            Some(tab) => {
                let leaves = tab.root.leaves();
                self.confirm_close(leaves, window, cx);
            }
            None => {}
        }
    }

    /// Close `leaves`, asking first if any still runs a process.
    pub(super) fn confirm_close(&mut self, leaves: Vec<NodeId>, window: &mut Window, cx: &mut Context<Self>) {
        let running = self.running_panes(&leaves, cx);
        if running == 0 {
            for leaf in leaves {
                self.close_leaf(leaf, window, cx);
            }
            return;
        }
        let (title, detail) = if leaves.len() == 1 {
            ("Close pane?", "Its process is still running.".to_string())
        } else {
            ("Close tab?", format!("{running} running process(es) will be stopped."))
        };
        let answer = window.prompt(PromptLevel::Warning, title, Some(&detail), &["Close", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |this, window, cx| {
                    for leaf in leaves {
                        this.close_leaf(leaf, window, cx);
                    }
                });
            }
        })
        .detach();
    }
}
