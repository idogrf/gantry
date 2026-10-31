use super::WorkspaceView;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use app_core::attention::{Alert, AlertKind, alert_channels, describe};
use app_core::split_tree::NodeId;
use daemon::client::ClientEvent;
use gpui::{Context, Window};
use protocol::host::{HostId, PaneKey};
use protocol::message::{SessionInfo, SessionState};

use app_core::session_tree::{Tone, Unseen, session_title, status_tone};

use crate::alerts::{self, Notice};
use crate::git_diff_view::DiffState;
use crate::recents_view::SEARCH_LIMIT;
use crate::settings_store::SettingsStore;

impl WorkspaceView {
    pub(super) fn on_events(
        &mut self,
        host: &HostId,
        events: Vec<ClientEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for event in events {
            match &event {
                ClientEvent::Output { pane, .. }
                | ClientEvent::Resync { pane, .. }
                | ClientEvent::PasteImageSaved { pane, .. }
                | ClientEvent::PasteImageFailed { pane, .. } => {
                    if let Some(slot) = self
                        .workspace
                        .leaf_for_pane(&PaneKey::new(host.clone(), *pane))
                        .and_then(|leaf| self.slots.get(&leaf))
                    {
                        slot.view.update(cx, |view, cx| view.on_event(&event, cx));
                    }
                }
                ClientEvent::Exited { pane, code } => {
                    if let Some(leaf) = self.workspace.pane_exited(&PaneKey::new(host.clone(), *pane)) {
                        self.slots[&leaf].view.update(cx, |view, cx| view.exited(Some(*code), cx));
                        self.save(cx);
                    }
                }
                ClientEvent::Spawned { request, pane, cwd } => match self.pending.remove(request) {
                    Some(leaf) => {
                        self.workspace.pane_started(leaf, *pane, cwd.clone());
                        let launch = self.workspace.leaves[&leaf].launch.clone();
                        self.slots[&leaf].view.update(cx, |view, cx| view.attach(*pane, launch, cx));
                        if self.workspace.focused() == Some(leaf)
                            && let Some(client) = self.client(host)
                        {
                            client.focus(*pane);
                        }
                        self.save(cx);
                    }
                    // The leaf was closed while its process was starting.
                    None => {
                        if let Some(client) = self.client(host) {
                            client.kill(*pane);
                        }
                    }
                },
                ClientEvent::SpawnFailed { request, error } => {
                    if let Some(leaf) = self.pending.remove(request) {
                        self.slots[&leaf].view.update(cx, |view, cx| view.failed(error.clone(), cx));
                    }
                }
                ClientEvent::Sessions { sessions } => {
                    self.track_sessions(host, sessions, cx);
                    if self.recents.is_some()
                        && !self.search_query.is_empty()
                        && !self.search_answers.contains_key(host)
                    {
                        // The host connected while a search is on: ask it the same query.
                        if let Some(client) = self.client(host) {
                            client.search(self.search_request, &self.search_query, SEARCH_LIMIT);
                        }
                        self.search_answers.insert(host.clone(), Default::default());
                        self.push_search_hits(cx);
                    }
                    self.attention.reset(host, sessions);
                    let sessions = sessions.clone();
                    self.sidebar.update(cx, |sidebar, cx| sidebar.set_sessions(host, sessions, cx));
                    self.remember_folders(host, cx);
                    self.push_recents(cx);
                    self.resume_finished(window, cx);
                }
                ClientEvent::ClaudeDirsIndexed => {
                    if host.is_local() {
                        self.local_dirs_indexed = true;
                        self.resume_finished(window, cx);
                    }
                }
                ClientEvent::SessionsChanged { upserted, removed } => {
                    self.track_sessions(host, upserted, cx);
                    self.drop_marks_of_changed(host, upserted, cx);
                    let alerts = self.attention.update(host, upserted, removed);
                    let upserted = upserted.clone();
                    self.sidebar.update(cx, |sidebar, cx| sidebar.apply(host, upserted, removed, cx));
                    self.remember_folders(host, cx);
                    for alert in alerts {
                        self.raise(alert, window, cx);
                    }
                }
                ClientEvent::TakeOverResult { session, outcome } => {
                    self.on_take_over(host.clone(), session.clone(), outcome.clone(), window, cx)
                }
                ClientEvent::ContainerFound { request, container } => {
                    self.on_container_found(*request, container.clone(), window, cx)
                }
                ClientEvent::SearchResults { request, hits } => {
                    if *request == self.search_request {
                        let answer = self.search_answers.entry(host.clone()).or_default();
                        (answer.hits, answer.answered) = (hits.clone(), true);
                        self.push_search_hits(cx);
                    }
                }
                ClientEvent::SearchIndexing { done, total } => {
                    let progress = (done < total).then(|| (done * 100 / (*total).max(1)) as u8);
                    self.search_answers.entry(host.clone()).or_default().progress = progress;
                    self.push_search_state(cx);
                }
                ClientEvent::SearchUnavailable { .. } => {
                    self.search_answers.entry(host.clone()).or_default().unavailable = true;
                    self.push_search_hits(cx);
                    self.push_search_state(cx);
                }
                ClientEvent::DirListing { request, path, dirs } => {
                    if let Some((view, _)) = &self.launcher {
                        let (path, dirs) = (path.clone(), dirs.clone());
                        view.update(cx, |view, cx| view.set_listing(*request, path, dirs, cx));
                    }
                }
                ClientEvent::DirListingFailed { request, reason } => {
                    if let Some((view, _)) = &self.launcher {
                        let reason = reason.clone();
                        view.update(cx, |view, cx| view.set_listing_failed(*request, reason, cx));
                    }
                }
                ClientEvent::GitStatus { request, state } => {
                    if self.git.apply(host, *request, Ok(state.clone()), Instant::now()) {
                        self.refresh_git_diff(false, cx);
                        cx.notify();
                    }
                }
                ClientEvent::GitDiff { request, body, .. } => {
                    if let Some(page) = self.git_page.as_mut().filter(|p| p.host == *host && p.request == *request) {
                        page.state = DiffState::from_body(body.clone());
                        cx.notify();
                    }
                }
                ClientEvent::GitCommit { request, detail } => {
                    let current = self.git_page.as_ref().is_some_and(|page| {
                        page.host == *host && page.commit.as_ref().is_some_and(|commit| commit.request == *request)
                    });
                    if current {
                        let first = detail.files.first().map(|file| (file.path.clone(), file.old_path.clone()));
                        if let Some(commit) = self.git_page.as_mut().and_then(|page| page.commit.as_mut()) {
                            commit.detail = Some(detail.clone());
                        }
                        if let Some((path, old_path)) = first {
                            self.pick_git_file(path, old_path, false, cx);
                        }
                        cx.notify();
                    }
                }
                ClientEvent::GitUnavailable { request, problem } => {
                    let reason = match problem {
                        protocol::message::GitProblem::NotRepo => "this folder is not in a git repository".to_string(),
                        protocol::message::GitProblem::NoGit => "git is not installed on this host".to_string(),
                        protocol::message::GitProblem::Failed { reason } => reason.clone(),
                    };
                    if let Some(commit) = self
                        .git_page
                        .as_mut()
                        .filter(|page| page.host == *host)
                        .and_then(|page| page.commit.as_mut())
                        .filter(|commit| commit.request == *request)
                    {
                        commit.failed = Some(reason);
                        cx.notify();
                    } else if let Some(page) =
                        self.git_page.as_mut().filter(|p| p.host == *host && p.request == *request)
                    {
                        page.state = DiffState::Failed(reason);
                        cx.notify();
                    } else if self.git.apply(host, *request, Err(problem.clone()), Instant::now()) {
                        cx.notify();
                    }
                }
                ClientEvent::TakenOver if !host.is_local() => self.host_taken_over(host, cx),
                ClientEvent::Disconnected if !host.is_local() => self.host_dropped(host, window, cx),
                ClientEvent::TakenOver | ClientEvent::Disconnected => {
                    if matches!(event, ClientEvent::TakenOver) {
                        cx.quit();
                        continue;
                    }
                    if let Some(conn) = self.hosts.get_mut(host) {
                        conn.connected = false;
                    }
                    let leaves: Vec<NodeId> = self
                        .workspace
                        .leaves
                        .iter()
                        .filter(|(_, leaf)| &leaf.host == host)
                        .map(|(id, _)| *id)
                        .collect();
                    for leaf in leaves {
                        if let Some(slot) = self.slots.get(&leaf) {
                            slot.view.update(cx, |view, cx| view.disconnected(cx));
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    /// Leaves follow the session their pane runs (it changes on `/clear`).
    fn track_sessions(&mut self, host: &HostId, sessions: &[SessionInfo], cx: &mut Context<Self>) {
        let mut changed = false;
        for session in sessions {
            if let SessionState::Managed { pane } = session.state {
                changed |= self.workspace.session_seen(&PaneKey::new(host.clone(), pane), &session.id);
            }
        }
        if changed {
            self.save(cx);
        }
    }

    /// Remember an SSH host's recent folders in the layout, for the launcher while the host is away.
    fn remember_folders(&mut self, host: &HostId, cx: &mut Context<Self>) {
        self.welcome_items.take();
        self.push_launcher_folders(cx);
        if host.is_local() {
            return;
        }
        let sessions: Vec<SessionInfo> =
            self.sidebar.read(cx).sessions().all().filter(|s| s.host == *host).cloned().collect();
        let refs: Vec<&SessionInfo> = sessions.iter().collect();
        if self.workspace.remember_host_folders(host, &refs) {
            self.save(cx);
        }
    }

    /// Drop the hand-made unread mark of every session whose status is no longer Idle.
    fn drop_marks_of_changed(&mut self, host: &HostId, sessions: &[SessionInfo], cx: &mut Context<Self>) {
        if self.marked_unread.is_empty() {
            return;
        }
        for session in sessions {
            let SessionState::Managed { pane } = session.state else { continue };
            let Some(leaf) = self.workspace.leaf_for_pane(&PaneKey::new(host.clone(), pane)) else { continue };
            if status_tone(session.status.as_ref(), Unseen::No) != Tone::Idle && self.marked_unread.remove(&leaf) {
                self.unseen.remove(&leaf);
                self.sync_unseen(cx);
            }
        }
    }

    /// Raise `alert` on the channels the settings allow for its pane (badge, notification, sound).
    fn raise(&mut self, alert: Alert, window: &mut Window, cx: &mut Context<Self>) {
        let Some(leaf) = self.workspace.leaf_for_pane(&alert.pane) else { return };
        let settings = cx.global::<SettingsStore>().settings().clone();
        let visible = self.workspace.is_visible(leaf);
        let channels = alert_channels(&settings.alerts, &alert.kind, visible, window.is_window_active());
        if channels.badge {
            self.unseen.insert(leaf, alert.kind.clone());
            self.sync_unseen(cx);
            self.clear_seen(window, cx);
        }
        if !channels.notification && channels.sound.is_none() {
            return;
        }
        let sidebar = self.sidebar.read(cx);
        let session = sidebar.sessions().get(&alert.pane.host, &alert.session);
        let title = session.map(|session| session_title(session).to_string()).unwrap_or_default();
        let last_prompt = session.and_then(|session| session.last_prompt.as_deref());
        let (summary, body) = describe(&alert.kind, &title, last_prompt);
        alerts::deliver(Notice { leaf, summary, body }, channels, settings.alerts.volume, self.clicked.clone());
    }

    /// Forget alerts of the panes the user can see now.
    pub(super) fn clear_seen(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !window.is_window_active() {
            return;
        }
        let before = self.unseen.len();
        let workspace = &self.workspace;
        forget_seen(&mut self.unseen, &self.marked_unread, |leaf| workspace.is_visible(leaf));
        if self.unseen.len() != before {
            self.sync_unseen(cx);
        }
        cx.notify();
    }

    /// Tell the sidebar which sessions carry the unseen badge.
    pub(super) fn sync_unseen(&self, cx: &mut Context<Self>) {
        let session_of = |leaf: &NodeId| self.workspace.leaves.get(leaf)?.session.clone();
        let sessions: HashSet<String> = self.unseen.keys().filter_map(session_of).collect();
        let marked: HashSet<String> = self.marked_unread.iter().filter_map(session_of).collect();
        self.sidebar.update(cx, |sidebar, cx| sidebar.set_unseen(sessions, marked, cx));
    }

    /// Why `leaf`'s result is unseen, if it is.
    pub(super) fn unseen_of(&self, leaf: NodeId) -> Unseen {
        if self.marked_unread.contains(&leaf) {
            Unseen::Marked
        } else if self.unseen.contains_key(&leaf) {
            Unseen::Alert
        } else {
            Unseen::No
        }
    }
}

/// Drop the alerts of the leaves the user can see, except those marked unread by hand.
fn forget_seen(unseen: &mut HashMap<NodeId, AlertKind>, marked: &HashSet<NodeId>, visible: impl Fn(NodeId) -> bool) {
    unseen.retain(|leaf, _| !visible(*leaf) || marked.contains(leaf));
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_leaf_marked_unread_keeps_its_alert_in_view_while_others_are_forgotten() {
        let (marked, seen): (NodeId, NodeId) = (1, 2);
        let mut unseen = HashMap::from([(marked, AlertKind::Finished), (seen, AlertKind::Finished)]);
        forget_seen(&mut unseen, &HashSet::from([marked]), |_| true);
        assert_eq!(unseen.keys().copied().collect::<Vec<_>>(), vec![marked]);
    }
}
