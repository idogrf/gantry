use super::{DialogAction, TakeOverTarget, WorkspaceView};
use std::path::PathBuf;

use app_core::dialogs;
use app_core::split_tree::{Axis, NodeId};
use app_core::workspace::{PaneLaunch, Rerun};
use gpui::{App, Context, Window};
use protocol::host::HostId;
use protocol::message::{SessionState, TakeOverOutcome};

impl WorkspaceView {
    /// Start `launch` in the finished `leaf`.
    fn start_leaf(&mut self, leaf: NodeId, launch: PaneLaunch, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slot) = self.slots.get(&leaf) else { return };
        if let Some(entry) = self.workspace.leaves.get_mut(&leaf) {
            entry.launch = launch.clone();
        }
        slot.view.update(cx, |view, cx| view.restarting(launch, cx));
        self.spawn_leaf(leaf, None, window, cx);
        self.save(cx);
    }

    /// Start what the last run left finished, once the session list is in. A session that runs elsewhere stays a card.
    pub(super) fn resume_finished(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut waiting = Vec::new();
        for leaf in std::mem::take(&mut self.to_resume) {
            let Some(entry) = self.workspace.leaves.get(&leaf).filter(|entry| entry.pane.is_none()) else { continue };
            // Leaves of SSH hosts wait for Continue.
            if !entry.host.is_local() {
                continue;
            }
            // Each leaf waits for the session list of its own host (this machine's: with every configured dir).
            if !self.sidebar.read(cx).sessions().loaded(&entry.host) || !self.local_dirs_indexed {
                waiting.push(leaf);
                continue;
            }
            if let Rerun::Start(launch) =
                entry.rerun_with(self.sidebar.read(cx).sessions(), &self.dirs, self.daemon_dir.as_deref())
            {
                self.start_leaf(leaf, launch, window, cx);
            }
        }
        self.to_resume = waiting;
    }

    /// Continue / Restart a finished leaf. If its session already runs in another pane, show that one. If it runs
    /// outside Gantry on this host, offer to take it over.
    pub(super) fn rerun_leaf(&mut self, leaf: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.workspace.leaves.get(&leaf) else { return };
        let session = entry.session.clone();
        let host = entry.host.clone();
        match entry.rerun_with(self.sidebar.read(cx).sessions(), &self.dirs, self.daemon_dir.as_deref()) {
            Rerun::Start(launch) => self.start_leaf(leaf, launch, window, cx),
            Rerun::Focus(key) => {
                if let Some(running) = self.workspace.leaf_for_pane(&key) {
                    self.focus_leaf(running, window, cx);
                }
            }
            Rerun::Busy => {
                let sessions = self.sidebar.read(cx).sessions();
                let takeable = session.as_deref().and_then(|id| sessions.get(&host, id)).is_some_and(|info| {
                    matches!(info.state, SessionState::External { pid: Some(_) }) && info.cwd_exists
                });
                match (takeable, session) {
                    (true, Some(session)) => {
                        self.confirm_take_over(host, session, TakeOverTarget::Leaf(leaf), window, cx)
                    }
                    _ => {
                        self.open_dialog(dialogs::cannot_resume(), DialogAction::Ack, window, cx);
                    }
                }
            }
        }
    }

    /// Ask before stopping the outside process running `session` on `host`; on yes, request it and remember where
    /// to resume.
    pub(super) fn confirm_take_over(
        &mut self,
        host: HostId,
        session: String,
        target: TakeOverTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.take_overs.contains_key(&(host.clone(), session.clone())) {
            return;
        }
        if self.client(&host).is_none() {
            self.open_dialog(dialogs::cannot_take_over(host.name()), DialogAction::Ack, window, cx);
            return;
        }
        let spec = dialogs::take_over(host.remote_name());
        self.open_dialog(spec, DialogAction::TakeOver { host, session, target }, window, cx);
    }

    pub(super) fn on_take_over(
        &mut self,
        host: HostId,
        session: String,
        outcome: TakeOverOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let waiting = (host.clone(), session.clone());
        match outcome {
            TakeOverOutcome::Exited => {
                if matches!(&self.dialog, Some(open) if matches!(&open.action, DialogAction::ForceQuit { waiting: asked } if *asked == waiting))
                {
                    // The session is gone: Force quit has nothing left to act on.
                    self.dialog = None;
                    self.focus_current(window, cx);
                }
                match self.take_overs.remove(&waiting) {
                    Some(TakeOverTarget::NewPane { cwd, split, container }) => {
                        let axis = (split && self.workspace.can_split()).then_some(Axis::Horizontal);
                        let launch = self.resume_launch(&host, &session, cwd, container, cx);
                        let project = self.workspace.resume_project_for(&session);
                        self.open_launch(launch, axis, None, host, project, window, cx);
                    }
                    Some(TakeOverTarget::Leaf(leaf)) => {
                        let session_dir = self
                            .sidebar
                            .read(cx)
                            .sessions()
                            .get(&host, &session)
                            .and_then(|info| info.claude_dir.clone());
                        let launch = self.workspace.leaves.get(&leaf).map(|entry| {
                            entry.rerun_resolved(true, session_dir.as_deref(), &self.dirs, self.daemon_dir.as_deref())
                        });
                        if let Some(launch) = launch {
                            self.start_leaf(leaf, launch, window, cx);
                            self.focus_leaf(leaf, window, cx);
                        }
                    }
                    None => {}
                }
            }
            TakeOverOutcome::StillRunning => {
                if !self.take_overs.contains_key(&waiting) {
                    return;
                }
                let action = DialogAction::ForceQuit { waiting: waiting.clone() };
                if !self.open_dialog(dialogs::force_quit(), action, window, cx) {
                    self.take_overs.remove(&waiting);
                }
            }
            TakeOverOutcome::Failed { error } => {
                self.take_overs.remove(&waiting);
                let spec = dialogs::notice("Could not take over the session", &error);
                self.open_dialog(spec, DialogAction::Ack, window, cx);
            }
        }
    }

    /// Resume `session` where it ran: with the command and config dir recorded for it.
    /// A remote session resumes with the plain `claude`: the remote uses its own config dir.
    fn resume_launch(
        &self,
        host: &HostId,
        session: &str,
        cwd: PathBuf,
        container: Option<PathBuf>,
        cx: &App,
    ) -> PaneLaunch {
        if !host.is_local() {
            return PaneLaunch::resume(session, cwd, None);
        }
        let session_dir = self.sidebar.read(cx).sessions().get(host, session).and_then(|info| info.claude_dir.clone());
        PaneLaunch::resume_with(session, cwd, container, session_dir.as_deref(), &self.dirs, self.daemon_dir.as_deref())
    }

    /// Resume `session`: show the leaf that already has it (starting it again if it finished), else open it in a
    /// new tab (or a split) where it ran — its dev container included.
    pub(super) fn resume_session(
        &mut self,
        host: HostId,
        session: String,
        cwd: PathBuf,
        split: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A session already in the layout is shown there, so one session never runs in two panes.
        match self.workspace.leaf_for_session(&host, &session) {
            Some(leaf) => {
                self.focus_leaf(leaf, window, cx);
                let starting = self.pending.values().any(|waiting| *waiting == leaf);
                if self.workspace.leaves[&leaf].pane.is_none() && !starting {
                    self.rerun_leaf(leaf, window, cx);
                }
            }
            None => {
                let axis = (split && self.workspace.can_split()).then_some(Axis::Horizontal);
                let place =
                    self.sidebar.read(cx).sessions().get(&host, &session).and_then(|info| info.container.clone());
                match place {
                    Some(place) if host.is_local() && place.host_folder.is_none() => self.container_gone(window, cx),
                    place => {
                        let container = place.and_then(|place| place.host_folder);
                        let launch = self.resume_launch(&host, &session, cwd, container, cx);
                        let project = self.workspace.resume_project_for(&session);
                        self.open_launch(launch, axis, None, host, project, window, cx);
                    }
                }
            }
        }
    }
}
