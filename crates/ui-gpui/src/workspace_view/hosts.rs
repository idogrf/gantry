use super::{CardKind, DialogAction, HostConn, HostUpdate, HostUpdateCard, UpdatePhase, WorkspaceView};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use app_core::dialogs;
use app_core::host_install::check;
use app_core::host_state::{DisconnectPlan, FailKind, HostDot, HostEvent, HostState, reconnect_delay};
use app_core::host_update::{Click, click_plan, needs_update};
use app_core::split_tree::NodeId;
use app_core::theme_colors::terminal_palette;
use app_core::version::Version;
use app_core::workspace::{HostLive, reconcile, without_killed_offline};
use daemon::client::Connection;
use daemon::paths::{Paths, current_profile};
use daemon::ssh::{Attempt, AttemptHandle, HANDSHAKE_TIMEOUT, Share, SshHostSpec, connect as ssh_connect};
use gpui::{App, Context, Focusable, Pixels, Point, Window, prelude::*};
use protocol::host::HostId;
use protocol::message::PaletteColors;

use app_core::settings::add_ssh_host;

use crate::settings_store::SettingsStore;
use crate::settings_view::ssh_hosts::{HostStatus, HostStatuses};
use crate::sidebar_view::HostRow;
use crate::ssh_wizard::run::{self, Msg, Questions};
use crate::ssh_wizard::{SshWizard, WizardEvent, WizardStart, describe, upload_preflight};

/// Tries (one second apart) a connect makes after an update's restart while the old daemon still answers.
const RESTART_RETRIES: u8 = 3;

impl WorkspaceView {
    /// Close the shared ssh connection kept for `host`'s connect, if any: `keep_sessions` after a good connect (the
    /// daemon connection runs through it and keeps it), else it exits now.
    fn close_share(&mut self, host: &HostId, keep_sessions: bool) {
        let Some(share) = self.shares.remove(host) else { return };
        let Some(entry) = self.ssh_hosts.iter().find(|entry| entry.name == host.name()) else {
            std::thread::spawn(move || share.close("ssh", "", keep_sessions));
            return;
        };
        let target = entry.target.clone();
        std::thread::spawn(move || share.close("ssh", &target, keep_sessions));
    }

    /// Take down `host`'s open sign-in dialog and drop its queued questions (each is answered Cancel).
    fn withdraw_questions(&mut self, host: &HostId, cx: &mut Context<Self>) {
        self.askpass_queue.retain(|prompt| prompt.host != host.name());
        if let Some((view, _)) = &self.askpass
            && view.read(cx).host() == host.name()
        {
            view.update(cx, |view, cx| view.cancel(cx));
        }
    }

    /// `host` is no longer in `sshHosts`: drop its connection, retry timer, attempt and questions, and show it as not
    /// connected.
    pub(super) fn forget_host(&mut self, host: &HostId, cx: &mut Context<Self>) {
        self.retries.remove(host);
        if let Some((_, handle)) = self.attempts.remove(host) {
            handle.cancel();
        }
        self.close_share(host, false);
        self.drop_host_update(host, cx);
        if self.update_host_card.as_ref().is_some_and(|card| card.host == *host) {
            self.update_host_card = None;
        }
        self.withdraw_questions(host, cx);
        self.hosts.remove(host);
        for (id, leaf) in &self.workspace.leaves {
            if &leaf.host == host
                && let Some(slot) = self.slots.get(id)
            {
                slot.view.update(cx, |view, _| view.set_client(None));
            }
        }
        self.set_host_state(host, HostState::NotConnected, cx);
    }

    /// Settings renamed SSH host `from` to `to`: its leaves, daemon instance and waiting state move to the new name. A
    /// connected host was dropped when the old name left `sshHosts`; `reconnect` connects it again under the new one
    /// (its panes reattach, its sessions are listed again).
    pub(super) fn host_renamed(
        &mut self,
        from: &HostId,
        to: &HostId,
        reconnect: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ssh_hosts = cx.global::<SettingsStore>().settings().ssh_hosts.clone();
        // Refused before anything is touched: the old host stays connected.
        if let Err(error) = self.workspace.rename_host(from, to) {
            tracing::warn!("host rename refused: {error}");
            return;
        }
        self.forget_host(from, cx);
        // The leaves already carry the new name: they lose the old connection too.
        for (id, leaf) in &self.workspace.leaves {
            if leaf.host == *to
                && let Some(slot) = self.slots.get(id)
            {
                slot.view.update(cx, |view, _| view.set_client(None));
            }
        }
        self.host_states.remove(from);
        self.hosts_with_rows.remove(from);
        if let Some(waiting) = self.queued.remove(from) {
            self.queued.entry(to.clone()).or_default().extend(waiting);
        }
        for killed in &mut self.killed_offline {
            if killed.0 == *from {
                killed.0 = to.clone();
            }
        }
        for open in self.pending_opens.values_mut() {
            if open.host == *from {
                open.host = to.clone();
            }
        }
        self.sidebar.update(cx, |sidebar, cx| sidebar.set_sessions(from, Vec::new(), cx));
        self.welcome_items.take();
        self.attention.reset(from, &[]);
        self.set_host_state(to, HostState::NotConnected, cx);
        self.save(cx);
        if reconnect {
            self.connect_host(to, true, window, cx);
        }
    }

    /// Settings removed SSH host `host`: it is disconnected and its panes leave the layout. Nothing on the remote is
    /// touched.
    pub(super) fn host_removed(&mut self, host: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        self.ssh_hosts = cx.global::<SettingsStore>().settings().ssh_hosts.clone();
        self.forget_host(host, cx);
        let leaves: Vec<NodeId> =
            self.workspace.leaves.iter().filter(|(_, leaf)| leaf.host == *host).map(|(id, _)| *id).collect();
        self.workspace.remove_host(host);
        for leaf in leaves {
            self.slots.remove(&leaf);
            self.marked_unread.remove(&leaf);
            self.unseen.remove(&leaf);
            self.pending.retain(|_, waiting| *waiting != leaf);
        }
        self.queued.remove(host);
        self.killed_offline.retain(|killed| killed.0 != *host);
        self.pending_opens.retain(|_, open| open.host != *host);
        self.host_states.remove(host);
        self.hosts_with_rows.remove(host);
        self.sidebar.update(cx, |sidebar, cx| sidebar.set_sessions(host, Vec::new(), cx));
        self.welcome_items.take();
        self.attention.reset(host, &[]);
        self.sync_unseen(cx);
        self.focus_current(window, cx);
        self.save(cx);
        cx.notify();
    }

    /// Tell the Settings page where each listed host stands.
    pub(super) fn publish_hosts(&self, cx: &mut Context<Self>) {
        let statuses: HashMap<String, HostStatus> = self
            .ssh_hosts
            .iter()
            .map(|entry| {
                let host = HostId::named(&entry.name);
                let state = self.host_state(&host);
                let panes = self.workspace.leaves.values().filter(|leaf| leaf.host == host).count();
                (entry.name.clone(), HostStatus { label: state.label(), dot: state.dot(), panes })
            })
            .collect();
        let rows: Vec<HostRow> = self
            .ssh_hosts
            .iter()
            .map(|entry| {
                let host = HostId::named(&entry.name);
                let state = self.host_state(&host);
                let label = (state != HostState::Connected).then(|| state.label());
                let updating = self.host_update.contains_key(&host);
                let local = Version::parse(protocol::build::VERSION);
                let remote = self.hosts.get(&host).and_then(|conn| conn.daemon_version);
                let update =
                    local.filter(|local| needs_update(&state, remote, *local)).map(|local| format!("↑ {local}"));
                // The row shows the Busy dot while the update runs, as the wizard's steps do.
                let dot = if updating { HostDot::Busy } else { state.dot() };
                HostRow { dot, label, up: state.in_connected_list(), host, update, updating }
            })
            .collect();
        self.sidebar.update(cx, |sidebar, cx| sidebar.set_hosts(rows, cx));
        let listed: HashSet<&str> = self.ssh_hosts.iter().map(|entry| entry.name.as_str()).collect();
        let taken: HashSet<String> = self
            .workspace
            .host_names()
            .into_iter()
            .filter(|host| !host.is_local() && !listed.contains(host.name()))
            .map(|host| host.name().to_string())
            .collect();
        let next = HostStatuses { hosts: statuses, taken };
        if cx.try_global::<HostStatuses>().is_none_or(|known| *known != next) {
            cx.set_global(next);
        }
    }

    /// The state of `host`: connected for this machine, else what the lifecycle recorded.
    pub(super) fn host_state(&self, host: &HostId) -> HostState {
        if host.is_local() {
            return HostState::Connected;
        }
        self.host_states.get(host).cloned().unwrap_or(HostState::NotConnected)
    }

    /// Record `host`'s state and show it on the host's panes.
    fn set_host_state(&mut self, host: &HostId, state: HostState, cx: &mut Context<Self>) {
        if state.shows_rows(self.hosts_with_rows.contains(host)) {
            self.hosts_with_rows.insert(host.clone());
        } else {
            self.hosts_with_rows.remove(host);
        }
        self.host_states.insert(host.clone(), state);
        if self.host_state(host) != HostState::Connected
            && matches!(&self.dialog, Some(open) if matches!(&open.action, DialogAction::Disconnect(asked) if asked == host))
        {
            // The dialog asks about a connection that is gone: close it and give the focus back.
            self.dialog = None;
            if let Some(window) = cx.windows().into_iter().next() {
                cx.spawn(async move |this, cx| {
                    let _ = window.update(cx, |_, window, cx| {
                        let _ = this.update(cx, |view, cx| view.focus_current(window, cx));
                    });
                })
                .detach();
            }
        }
        if self.is_away(host) && !self.host_state(host).asks_user() {
            // Its tabs left the sidebar, so the one on screen goes too; checked once the state settled, since a
            // reconnect may follow at once (a rename, Settings' Reconnect).
            if let Some(window) = cx.windows().into_iter().next() {
                cx.spawn(async move |this, cx| {
                    let _ = window.update(cx, |_, window, cx| {
                        let _ = this.update(cx, |view, cx| view.leave_off_screen_tab(window, cx));
                    });
                })
                .detach();
            }
        }
        if self.is_away(host) {
            // Its sessions are not known any more; they are listed again when it connects.
            self.sidebar.update(cx, |sidebar, cx| sidebar.set_sessions(host, Vec::new(), cx));
            self.welcome_items.take();
            self.attention.reset(host, &[]);
            self.sync_unseen(cx);
            self.git.forget_host(host);
        }
        self.push_launcher_hosts(cx);
        self.push_recents(cx);
        self.refresh_host_cards(host, cx);
        self.sync_panes(cx);
        cx.notify();
    }

    /// Put the card of `host`'s state on every pane of the host (or take it off when connected).
    pub(super) fn refresh_host_cards(&mut self, host: &HostId, cx: &mut Context<Self>) {
        let mut card = self.host_state(host).card(host.name());
        // A host that is not in `sshHosts` cannot be connected: its card has no button (and a host that was never
        // connected says it is not listed).
        if !self.ssh_hosts.iter().any(|entry| entry.name == host.name())
            && let Some(card) = &mut card
        {
            if self.host_state(host) == HostState::NotConnected {
                card.title = "Not listed".into();
                card.detail = format!("{} is not listed in sshHosts.", host.name());
            }
            card.button = None;
        }
        for (id, leaf) in &self.workspace.leaves {
            if &leaf.host == host
                && let Some(slot) = self.slots.get(id)
            {
                slot.view.update(cx, |view, cx| view.set_host_card(card.clone(), cx));
            }
        }
    }

    /// Start an attempt to connect `host`: interactive when the user asked (sign-in dialogs may open), else a
    /// reconnect retry (never prompts, and only while the host is still `Reconnecting`).
    pub(super) fn connect_host(
        &mut self,
        host: &HostId,
        interactive: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.ssh_hosts.iter().find(|entry| entry.name == host.name()).cloned() else {
            // Not (or no longer) in `sshHosts`: nothing to connect to, and no timer left running.
            self.retries.remove(host);
            if self.host_state(host) != HostState::Connected {
                let reason = format!("{} is not listed in sshHosts", host.name());
                self.set_host_state(host, HostState::Failed { reason }, cx);
            }
            return;
        };
        let state = self.host_state(host);
        let attempt = if interactive {
            if !state.can_connect() {
                return;
            }
            // The user got here first: a pending retry timer must not fire.
            self.retries.remove(host);
            self.set_host_state(host, state.next(HostEvent::Connect), cx);
            Attempt::Interactive {
                prompts: self.askpass_prompts.clone(),
                runtime_dir: Paths::for_profile(&current_profile()).runtime_dir,
            }
        } else {
            if !matches!(state, HostState::Reconnecting { .. }) {
                return;
            }
            Attempt::Background
        };
        // Only a user's connect rides on a wizard's shared connection.
        let share = if matches!(attempt, Attempt::Interactive { .. }) { self.shares.get(host).cloned() } else { None };
        let handle = AttemptHandle::default();
        self.next_attempt += 1;
        let number = self.next_attempt;
        self.attempts.insert(host.clone(), (number, handle.clone()));
        let (host, spec) = (
            host.clone(),
            SshHostSpec {
                name: entry.name,
                target: entry.target,
                command: entry.command,
                auth: entry.auth,
                port: entry.port,
                share,
            },
        );
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ssh_connect("ssh", &spec, attempt, &handle, HANDSHAKE_TIMEOUT) })
                .await;
            let _ = this.update_in(cx, |view, window, cx| view.on_attempt(&host, number, result, window, cx));
        })
        .detach();
    }

    /// The user cancelled the attempt that is connecting `host`: ssh is killed, its questions are withdrawn and the
    /// host is back to not connected (leaves waiting for it stay queued, with the Not connected card).
    fn cancel_connecting(&mut self, host: &HostId, cx: &mut Context<Self>) {
        let state = self.host_state(host);
        if state != HostState::Connecting {
            return;
        }
        if let Some((_, handle)) = self.attempts.remove(host) {
            handle.cancel();
        }
        self.retries.remove(host);
        self.restart_retries.remove(host);
        self.close_share(host, false);
        self.withdraw_questions(host, cx);
        self.set_host_state(host, state.next(HostEvent::Cancel), cx);
    }

    /// An attempt ended: connected, or a state that says why not (and a retry while reconnecting).
    fn on_attempt(
        &mut self,
        host: &HostId,
        number: u64,
        result: Result<Connection, FailKind>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The attempt was cancelled, or replaced by a newer one: its result means nothing now.
        if self.attempts.get(host).map(|(current, _)| *current) != Some(number) {
            return;
        }
        self.attempts.remove(host);
        match result {
            Ok(connection) => {
                self.close_share(host, true);
                self.host_connected(host, connection, window, cx)
            }
            Err(kind) => {
                // Whatever ended it, a question of this attempt still on screen is stale.
                self.withdraw_questions(host, cx);
                // Right after an update's restart the old daemon may still answer: try again shortly.
                if matches!(kind, FailKind::UpdateNeeded { .. })
                    && let Some(left) = self.restart_retries.get_mut(host)
                    && *left > 0
                {
                    *left -= 1;
                    let state = self.host_state(host);
                    self.set_host_state(host, state.next(HostEvent::Cancel), cx);
                    let host_for_task = host.clone();
                    let task = cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(Duration::from_secs(1)).await;
                        let _ =
                            this.update_in(cx, |view, window, cx| view.connect_host(&host_for_task, true, window, cx));
                    });
                    self.retries.insert(host.clone(), task);
                    return;
                }
                self.restart_retries.remove(host);
                self.close_share(host, false);
                let next = self.host_state(host).next(HostEvent::AttemptFailed(kind));
                self.set_host_state(host, next.clone(), cx);
                if let HostState::Reconnecting { attempt } = next {
                    self.schedule_retry(host, attempt, window, cx);
                }
            }
        }
    }

    /// Run retry `attempt` of `host` after its delay; replaces (cancels) a timer still pending.
    fn schedule_retry(&mut self, host: &HostId, attempt: u8, window: &mut Window, cx: &mut Context<Self>) {
        let Some(delay) = reconnect_delay(attempt) else { return };
        let host_for_task = host.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update_in(cx, |view, window, cx| view.connect_host(&host_for_task, false, window, cx));
        });
        self.retries.insert(host.clone(), task);
    }

    /// `host` is connected (first time, or again after a drop): reattach its panes, adopt what runs there, and spawn
    /// the leaves that were waiting for it.
    fn host_connected(&mut self, host: &HostId, connection: Connection, window: &mut Window, cx: &mut Context<Self>) {
        let Connection { client, events, instance, panes, daemon_version, .. } = connection;
        self.retries.remove(host);
        self.restart_retries.remove(host);
        // Panes closed while the host was away are killed now, and not adopted as tabs again.
        let (panes, to_kill) = without_killed_offline(&mut self.killed_offline, host, &instance, &panes);
        for pane in to_kill {
            client.kill(pane);
        }
        self.hosts.insert(
            host.clone(),
            HostConn {
                client: client.clone(),
                connected: true,
                daemon_version: daemon_version.as_deref().and_then(Version::parse),
                _events: Self::listen(host.clone(), events, window, cx),
            },
        );
        let state = self.host_state(host).next(HostEvent::Established);
        self.set_host_state(host, state, cx); // takes the cards off
        let before: HashSet<NodeId> = self.workspace.leaves.keys().copied().collect();
        let live = HostLive { host: host.clone(), instance: &instance, panes: &panes };
        let attach = reconcile(&mut self.workspace, &live);
        // Tabs adopted from the host's live panes need slots.
        let adopted: Vec<NodeId> = self.workspace.leaves.keys().copied().filter(|id| !before.contains(id)).collect();
        for leaf in adopted {
            self.add_slot(leaf, window, cx);
        }
        // Every pane of the host uses the new connection (also the dead ones, for when they run again).
        let leaves: Vec<NodeId> =
            self.workspace.leaves.iter().filter(|(_, leaf)| &leaf.host == host).map(|(id, _)| *id).collect();
        for leaf in &leaves {
            if let Some(slot) = self.slots.get(leaf) {
                let client = client.clone();
                slot.view.update(cx, |view, _| view.set_client(Some(client)));
            }
        }
        let waiting = self.queued.remove(host).unwrap_or_default();
        for (leaf, key) in &attach {
            let launch = self.workspace.leaves[leaf].launch.clone();
            self.slots[leaf].view.update(cx, |view, cx| view.attach(key.pane, launch, cx));
            client.attach(key.pane);
        }
        // Leaves of this host that lost their pane show Continue / Restart (except the ones about to be spawned).
        for leaf in &leaves {
            if self.workspace.leaves[leaf].pane.is_none()
                && !waiting.contains(leaf)
                && let Some(slot) = self.slots.get(leaf)
            {
                slot.view.update(cx, |view, cx| view.exited(None, cx));
            }
        }
        for leaf in waiting {
            self.spawn_leaf(leaf, None, window, cx);
        }
        if let Some(preset) = self.sent_palette {
            client.set_palette(PaletteColors::from(terminal_palette(preset)));
        }
        self.sync_panes(cx);
        self.focus_current(window, cx);
        self.save(cx);
    }

    /// An established connection of SSH host `host` ended: its panes show the state, and retries begin.
    pub(super) fn host_dropped(&mut self, host: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        if self.host_state(host) != HostState::Connected {
            return; // a stale event of an older connection
        }
        match self.host_update.get(host).map(|update| update.phase) {
            // The old daemon is gone, as asked: the update owns the next connect, not the retry timer.
            Some(UpdatePhase::Restarting) => return self.finish_restart(host, window, cx),
            Some(UpdatePhase::Uploading) => self.drop_host_update(host, cx),
            None => {}
        }
        self.close_update_card(host);
        if let Some(conn) = self.hosts.get_mut(host) {
            conn.connected = false;
        }
        self.take_overs.retain(|(taking, _), _| taking != host);
        if self.search_answers.remove(host).is_some() {
            self.push_search_hits(cx);
            self.push_search_state(cx);
        }
        for (id, leaf) in &self.workspace.leaves {
            if &leaf.host == host
                && let Some(slot) = self.slots.get(id)
            {
                slot.view.update(cx, |view, cx| view.disconnected(cx));
            }
        }
        if !self.ssh_hosts.iter().any(|entry| entry.name == host.name()) {
            // Not in `sshHosts` (the debug seam): there is no ssh command to retry with.
            self.set_host_state(host, HostState::Disconnected, cx);
            return;
        }
        let next = HostState::Connected.next(HostEvent::Dropped);
        self.set_host_state(host, next.clone(), cx);
        if let HostState::Reconnecting { attempt } = next {
            self.schedule_retry(host, attempt, window, cx);
        }
    }

    /// Another Gantry took over the daemon of SSH host `host`: no retry (two windows would fight over it); its panes
    /// say so and wait for the user's Reconnect.
    pub(super) fn host_taken_over(&mut self, host: &HostId, cx: &mut Context<Self>) {
        if self.host_state(host) != HostState::Connected {
            return;
        }
        // Taken over wins over a running update.
        self.drop_host_update(host, cx);
        self.close_update_card(host);
        if let Some(conn) = self.hosts.get_mut(host) {
            conn.connected = false;
        }
        self.take_overs.retain(|(taking, _), _| taking != host);
        for (id, leaf) in &self.workspace.leaves {
            if &leaf.host == host
                && let Some(slot) = self.slots.get(id)
            {
                slot.view.update(cx, |view, cx| view.disconnected(cx));
            }
        }
        self.set_host_state(host, HostState::Connected.next(HostEvent::TakenOver), cx);
    }

    /// The button of `host`'s state card, or a click on its row in the sidebar: cancel the attempt, open the wizard to
    /// install or update, or connect.
    pub(super) fn run_host_action(&mut self, host: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        let entry = self.ssh_hosts.iter().find(|entry| entry.name == host.name()).cloned();
        match (self.host_state(host), entry) {
            (HostState::Connecting, _) => self.cancel_connecting(host, cx),
            (HostState::NotInstalled, Some(entry)) => self.open_ssh_wizard(WizardStart::Install(entry), window, cx),
            (HostState::UpdateNeeded { .. }, Some(entry)) => {
                self.open_ssh_wizard(WizardStart::Update(entry), window, cx)
            }
            _ => self.connect_host(host, true, window, cx),
        }
    }

    /// Whether `host` is a listed SSH host the sidebar shows nothing of: it has not connected yet, or was disconnected
    /// or found unusable since (`HostState::shows_rows`). Its panes and sessions come back when it connects. A host that is not listed keeps its rows, so they can be closed.
    pub(super) fn is_away(&self, host: &HostId) -> bool {
        self.ssh_hosts.iter().any(|entry| entry.name == host.name()) && !self.hosts_with_rows.contains(host)
    }

    /// The leaves of hosts that are away: the sidebar does not list them.
    pub(super) fn unlisted_leaves(&self) -> HashSet<NodeId> {
        self.workspace.leaves.iter().filter(|(_, leaf)| self.is_away(&leaf.host)).map(|(id, _)| *id).collect()
    }

    /// The leaves the sidebar does not show: those of hosts that are away and those under a collapsed project. Their
    /// tabs are passed by when switching tabs.
    pub(super) fn away_leaves(&self, cx: &App) -> HashSet<NodeId> {
        let mut away = self.unlisted_leaves();
        away.extend(self.sidebar.read(cx).collapsed_leaves());
        away
    }

    /// The leaves never on screen: those of hosts that are away, except a host waiting for the user (`asks_user`),
    /// whose visible tab keeps its card.
    fn off_screen_leaves(&self) -> HashSet<NodeId> {
        let mut leaves = self.unlisted_leaves();
        leaves.retain(|leaf| !self.host_state(&self.workspace.leaves[leaf].host).asks_user());
        leaves
    }

    /// Whether the visible tab is never on screen (`off_screen_leaves`): the welcome screen shows in its place.
    pub(super) fn active_tab_off_screen(&self) -> bool {
        let off = self.off_screen_leaves();
        self.workspace.active_hidden(|leaf| off.contains(&leaf))
    }

    /// When the visible tab is never on screen (`off_screen_leaves`), show the last tab visited that is, else the
    /// first in the sidebar, else the welcome screen.
    pub(super) fn leave_off_screen_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let off = self.off_screen_leaves();
        let order = self.sidebar.read(cx).leaf_order();
        let before = self.workspace.active;
        if !self.workspace.leave_hidden_tab(&order, |leaf| off.contains(&leaf)) {
            // The welcome screen: the hidden pane keeps no keyboard focus.
            self.focus_handle.focus(window, cx);
        } else if self.workspace.active != before {
            self.focus_current(window, cx);
            self.save(cx);
        }
    }

    /// The × of `host` in the sidebar's Connected block: cancels a connection that is not up, stops an idle host's
    /// daemon, and asks Disconnect only / End sessions and disconnect / Cancel when sessions run there.
    pub(super) fn disconnect_host(&mut self, host: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        let leaves: Vec<NodeId> =
            self.workspace.leaves.iter().filter(|(_, leaf)| leaf.host == *host).map(|(id, _)| *id).collect();
        let running = self.running_panes(&leaves, cx);
        match self.host_state(host).disconnect_plan(running) {
            None => {}
            Some(DisconnectPlan::Cancel) => {
                if self.host_state(host) == HostState::Connecting {
                    self.cancel_connecting(host, cx);
                } else {
                    self.forget_host(host, cx);
                }
            }
            Some(DisconnectPlan::StopNow) => self.leave_host(host, true, cx),
            Some(DisconnectPlan::Ask) => {
                let spec = dialogs::disconnect(host.name(), &self.counts_for(&leaves, cx));
                self.open_dialog(spec, DialogAction::Disconnect(host.clone()), window, cx);
            }
        }
    }

    /// Disconnect from connected SSH host `host`, first asking its daemon to stop (which ends its sessions) when
    /// `stop`. Nothing reconnects by itself; its panes show Not connected.
    pub(super) fn leave_host(&mut self, host: &HostId, stop: bool, cx: &mut Context<Self>) {
        if stop && let Some(client) = self.client(host).cloned() {
            // The thread's clone keeps the connection open until the request is written.
            std::thread::spawn(move || client.stop_all_and_wait(Duration::from_secs(2)));
        }
        self.take_overs.retain(|(taking, _), _| taking != host);
        if self.search_answers.remove(host).is_some() {
            self.push_search_hits(cx);
            self.push_search_state(cx);
        }
        self.forget_host(host, cx);
    }

    /// The file to upload for `host`'s update, or why nothing can be; `None` when the host is not listed.
    fn host_update_file(&self, host: &HostId) -> Option<Result<PathBuf, String>> {
        let entry = self.ssh_hosts.iter().find(|entry| entry.name == host.name())?;
        Some(upload_preflight(&entry.command).map_err(|reason| blocked_text(&reason)))
    }

    /// Open the update card of `host`, focused.
    fn open_host_update_card(
        &mut self,
        host: &HostId,
        position: Point<Pixels>,
        kind: CardKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.update_host_card = Some(HostUpdateCard { host: host.clone(), position, focus, kind });
        cx.notify();
    }

    /// The update pill of `host` was clicked at `position`: the card says why nothing can be done, or asks before
    /// ending running sessions, or the update starts at once.
    pub(super) fn click_host_update(
        &mut self,
        host: &HostId,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.host_state(host) != HostState::Connected || self.host_update.contains_key(host) {
            return;
        }
        let local = match self.host_update_file(host) {
            None => return,
            Some(Err(reason)) => {
                return self.open_host_update_card(host, position, CardKind::Blocked(reason), window, cx);
            }
            Some(Ok(local)) => local,
        };
        let leaves: Vec<NodeId> =
            self.workspace.leaves.iter().filter(|(_, leaf)| leaf.host == *host).map(|(id, _)| *id).collect();
        match click_plan(self.running_panes(&leaves, cx)) {
            Click::Start => self.start_host_update(host, local, position, window, cx),
            Click::Ask { running } => self.open_host_update_card(host, position, CardKind::Ask { running }, window, cx),
        }
    }

    /// The card's Update and restart / Try again: check the file again, then start.
    pub(super) fn retry_host_update(
        &mut self,
        host: &HostId,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.host_update_file(host) {
            None => {}
            Some(Err(reason)) => self.open_host_update_card(host, position, CardKind::Blocked(reason), window, cx),
            Some(Ok(local)) => self.start_host_update(host, local, position, window, cx),
        }
    }

    /// Close the ssh connection an update's upload ran through (it never was in `shares`).
    fn close_update_share(&self, host: &HostId, share: Share) {
        let target = self.ssh_hosts.iter().find(|entry| entry.name == host.name()).map(|entry| entry.target.clone());
        std::thread::spawn(move || share.close("ssh", &target.unwrap_or_default(), false));
    }

    /// Close the update card if it is `host`'s.
    fn close_update_card(&mut self, host: &HostId) {
        if self.update_host_card.as_ref().is_some_and(|card| card.host == *host) {
            self.update_host_card = None;
        }
    }

    /// Cancel `host`'s running update, if any, close its ssh connection and take down its sign-in dialog.
    fn drop_host_update(&mut self, host: &HostId, cx: &mut Context<Self>) {
        let Some(update) = self.host_update.remove(host) else { return };
        update.handle.cancel();
        self.withdraw_questions(host, cx);
        if let Some(share) = update.share {
            self.close_update_share(host, share);
        }
    }

    /// Update the daemon of connected SSH host `host` with the `gantryd` at `local`: upload and verify it while the
    /// old daemon keeps running, then restart. `position` is where the card of a failure opens.
    pub(super) fn start_host_update(
        &mut self,
        host: &HostId,
        local: PathBuf,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.host_state(host) != HostState::Connected || self.host_update.contains_key(host) {
            return;
        }
        let Some(entry) = self.ssh_hosts.iter().find(|entry| entry.name == host.name()).cloned() else { return };
        let runtime_dir = Paths::for_profile(&current_profile()).runtime_dir;
        let share = Share::new(&runtime_dir);
        let spec = SshHostSpec {
            name: entry.name,
            target: entry.target,
            command: entry.command,
            auth: entry.auth,
            port: entry.port,
            share: share.clone(),
        };
        let questions = Questions { prompts: self.askpass_prompts.clone(), runtime_dir };
        let run::Job { handle, messages: probed } = run::probe(spec.clone(), &questions);
        let host_for_task = host.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            // The wizard's check: a remote that is not Linux x86_64 keeps its working binary.
            let probe = match probed.recv().await {
                Ok(Msg::Probed(Ok(probe))) => probe,
                Ok(Msg::Probed(Err(kind))) => {
                    let _ = this.update_in(cx, |view, window, cx| {
                        view.update_failed(&host_for_task, describe(&kind), window, cx)
                    });
                    return;
                }
                _ => return,
            };
            if !check(&probe, protocol::build::VERSION).supported {
                let reason =
                    format!("Gantry installs on Linux x86_64 hosts only ({}). Nothing was uploaded.", probe.os_arch);
                let _ = this.update_in(cx, |view, window, cx| view.update_failed(&host_for_task, reason, window, cx));
                return;
            }
            let run::Job { handle, messages } = run::install(spec, &questions, local);
            let installing = this.update(cx, |view, _| match view.host_update.get_mut(&host_for_task) {
                Some(update) => {
                    update.handle = handle.clone();
                    true
                }
                None => false,
            });
            if !installing.unwrap_or(false) {
                handle.cancel();
                return;
            }
            while let Ok(message) = messages.recv().await {
                if let Msg::Installed(result) = message {
                    let _ = this.update_in(cx, |view, window, cx| match result {
                        Ok(_) => view.begin_restart(&host_for_task, window, cx),
                        Err(kind) => view.update_failed(&host_for_task, describe(&kind), window, cx),
                    });
                    return;
                }
            }
            let _ = this.update_in(cx, |view, window, cx| {
                view.update_failed(&host_for_task, "The update stopped unexpectedly.".into(), window, cx)
            });
        });
        self.host_update
            .insert(host.clone(), HostUpdate { phase: UpdatePhase::Uploading, handle, task, share, position });
        self.publish_hosts(cx);
        cx.notify();
    }

    /// The new `gantryd` is in place: ask the old daemon to stop through the connection already open (a second bridge
    /// would take that connection over), and finish the restart when it ends, or after 5 s.
    fn begin_restart(&mut self, host: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        if self.host_update.get(host).is_none_or(|update| update.phase != UpdatePhase::Uploading) {
            return;
        }
        let Some(client) = self.client(host).cloned() else {
            return self.update_failed(host, "The connection to the host is gone.".into(), window, cx);
        };
        // The thread's clone keeps the connection open until the request is written.
        std::thread::spawn(move || client.stop_all_and_wait(Duration::from_secs(2)));
        let host_for_task = host.clone();
        let timer = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(5)).await;
            let _ = this.update_in(cx, |view, window, cx| view.finish_restart(&host_for_task, window, cx));
        });
        if let Some(update) = self.host_update.get_mut(host) {
            update.phase = UpdatePhase::Restarting;
            update.task = timer;
        }
    }

    /// The old daemon is gone (or 5 s passed): drop the old connection and connect again, on the update's ssh
    /// connection. Runs once; the first of the closed connection and the timer wins.
    fn finish_restart(&mut self, host: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(update) = self.host_update.remove(host) else { return };
        self.take_overs.retain(|(taking, _), _| taking != host);
        if self.search_answers.remove(host).is_some() {
            self.push_search_hits(cx);
            self.push_search_state(cx);
        }
        for (id, leaf) in &self.workspace.leaves {
            if &leaf.host == host
                && let Some(slot) = self.slots.get(id)
            {
                slot.view.update(cx, |view, cx| view.disconnected(cx));
            }
        }
        // `forget_host` closes the host's shared ssh connection: the update's goes in only after it.
        self.forget_host(host, cx);
        if let Some(share) = update.share {
            self.shares.insert(host.clone(), share);
        }
        // The old daemon may still answer for a moment after its stop.
        self.restart_retries.insert(host.clone(), RESTART_RETRIES);
        self.connect_host(host, true, window, cx);
    }

    /// A step of `host`'s update failed: the host stays as it was, the pill is back and a card says why.
    fn update_failed(&mut self, host: &HostId, reason: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(update) = self.host_update.remove(host) else { return };
        self.withdraw_questions(host, cx);
        if let Some(share) = update.share {
            self.close_update_share(host, share);
        }
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.update_host_card = Some(HostUpdateCard {
            host: host.clone(),
            position: update.position,
            focus,
            kind: CardKind::Failed(reason),
        });
        self.publish_hosts(cx);
        cx.notify();
    }

    /// Open the SSH install wizard for `start`; one at a time, a second request focuses the open one.
    pub(super) fn open_ssh_wizard(&mut self, start: WizardStart, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((wizard, _)) = &self.ssh_wizard {
            wizard.focus_handle(cx).focus(window, cx);
            return;
        }
        self.launcher = None;
        self.recents = None;
        let existing: Vec<String> = self.ssh_hosts.iter().map(|host| host.name.clone()).collect();
        let prompts = self.askpass_prompts.clone();
        let runtime_dir = Paths::for_profile(&current_profile()).runtime_dir;
        let view = cx.new(|cx| SshWizard::new(start, existing, prompts, runtime_dir, cx));
        let subscription = cx.subscribe_in(&view, window, |this, _, event: &WizardEvent, window, cx| match event {
            WizardEvent::Closed { draft } => {
                this.ssh_wizard = None;
                this.withdraw_questions(&HostId::named(draft), cx);
                this.focus_current(window, cx);
                cx.notify();
            }
            WizardEvent::Finished { host, add, restarted } => {
                if *add {
                    let entry = host.clone();
                    let mut saved = Ok(());
                    SettingsStore::update(cx, |settings| saved = add_ssh_host(&mut settings.ssh_hosts, entry));
                    if let Err(error) = saved {
                        // Not saved: stay in the wizard and never connect to a different entry.
                        if let Some((wizard, _)) = &this.ssh_wizard {
                            wizard.update(cx, |wizard, cx| wizard.refuse(error, cx));
                        }
                        return;
                    }
                    this.ssh_hosts = cx.global::<SettingsStore>().settings().ssh_hosts.clone();
                }
                let share =
                    this.ssh_wizard.as_ref().and_then(|(wizard, _)| wizard.update(cx, |wizard, _| wizard.take_share()));
                if let Some(share) = share {
                    this.shares.insert(HostId::named(&host.name), share);
                }
                this.ssh_wizard = None;
                if *restarted {
                    // The old daemon may still answer for a moment after its stop.
                    this.restart_retries.insert(HostId::named(&host.name), RESTART_RETRIES);
                }
                this.focus_current(window, cx);
                this.connect_host(&HostId::named(&host.name), true, window, cx);
                cx.notify();
            }
        });
        view.focus_handle(cx).focus(window, cx);
        self.ssh_wizard = Some((view, subscription));
        cx.notify();
    }
}

/// A preflight reason as the card shows it: a sentence, and for a Gantry that cannot be uploaded at all, what to do.
fn blocked_text(reason: &str) -> String {
    let mut text: String = reason.chars().take(1).flat_map(char::to_uppercase).chain(reason.chars().skip(1)).collect();
    if !text.ends_with('.') {
        text.push('.');
    }
    if text.ends_with("install it by hand.") {
        text.push_str(" Update it by hand, then reconnect.");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::blocked_text;

    #[test]
    fn a_blocked_update_reads_as_sentences() {
        assert_eq!(
            blocked_text("This Gantry cannot be uploaded to a Linux x86_64 host: install it by hand."),
            "This Gantry cannot be uploaded to a Linux x86_64 host: install it by hand. Update it by hand, then reconnect."
        );
        assert_eq!(
            blocked_text("the command is not a plain path; install Gantry on that host by hand"),
            "The command is not a plain path; install Gantry on that host by hand."
        );
        assert_eq!(
            blocked_text("/opt/gantryd is missing: install Gantry again."),
            "/opt/gantryd is missing: install Gantry again."
        );
    }
}
