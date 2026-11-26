//! The in-app pop-up queue, without drawing: a pure model on a clock the caller supplies (no clock reads here).
//!
//! The view schedules one timer from [`Toasts::next_wake`] and calls [`Toasts::tick`] when it fires, so nothing runs
//! while no toast counts down.

use protocol::host::HostId;

use crate::attention::AlertKind;
use crate::split_tree::NodeId;

/// How long a pop-up stays, counted only while the window is active and the pointer is not over it.
pub const SHOW_MS: u64 = 6_000;
/// Pop-ups shown at once; the oldest go first.
pub const MAX_SHOWN: usize = 3;
/// The fade-out the caller passes to [`Toasts::tick`] (0 with `animateUi` off).
pub const FADE_OUT_MS: u64 = 250;

/// One pop-up.
#[derive(Clone, Debug, PartialEq)]
pub struct Toast {
    /// Stable across joins, so the view can key its fade on it.
    pub id: u64,
    /// The pane of the session.
    pub leaf: NodeId,
    /// The session's host.
    pub host: HostId,
    /// The session id.
    pub session: String,
    /// What it says; a `FileSent` toast collects the files of its session.
    pub kind: AlertKind,
    /// Fading out; it can no longer be joined.
    pub leaving: bool,
    /// Time left while paused (hover, inactive window); the full life when it has not started.
    left_ms: u64,
    /// When it starts to leave; `None` while paused or leaving.
    deadline: Option<u64>,
    hovered: bool,
    /// When a leaving toast is removed.
    gone_at: Option<u64>,
}

/// The queue, oldest first.
#[derive(Default)]
pub struct Toasts {
    list: Vec<Toast>,
    next_id: u64,
    running: bool,
}

impl Toast {
    fn is_file(&self) -> bool {
        matches!(self.kind, AlertKind::FileSent { .. })
    }

    /// Starts or restarts the count with `left_ms`, unless something holds it.
    fn arm(&mut self, now_ms: u64, running: bool) {
        self.deadline = (running && !self.hovered && !self.leaving).then(|| now_ms + self.left_ms);
    }

    fn pause(&mut self, now_ms: u64) {
        if let Some(deadline) = self.deadline.take() {
            self.left_ms = deadline.saturating_sub(now_ms);
        }
    }
}

impl Toasts {
    /// Shows `kind` for a session. A `FileSent` joins the session's shown file toast (the files it lacks, by id, its
    /// time restarted, now the newest); a status kind replaces the session's shown status toast; else a new toast.
    /// The oldest go while more than [`MAX_SHOWN`] are not leaving. `running` false (window inactive) = its time
    /// starts when [`Toasts::set_running`] says so. Returns the toast's id.
    pub fn push(
        &mut self,
        leaf: NodeId,
        host: HostId,
        session: String,
        kind: AlertKind,
        now_ms: u64,
        running: bool,
    ) -> u64 {
        self.running = running;
        let same = |t: &Toast| !t.leaving && t.host == host && t.session == session;
        let mut joined = None;
        if let AlertKind::FileSent { files } = &kind {
            if let Some(at) = self.list.iter().position(|t| same(t) && t.is_file()) {
                let mut toast = self.list.remove(at);
                if let AlertKind::FileSent { files: have } = &mut toast.kind {
                    for file in files {
                        if !have.iter().any(|h| h.id == file.id) {
                            have.push(file.clone());
                        }
                    }
                }
                toast.leaf = leaf;
                toast.left_ms = SHOW_MS;
                toast.arm(now_ms, running);
                joined = Some(toast.id);
                self.list.push(toast);
            }
        } else {
            self.list.retain(|t| !(same(t) && !t.is_file()));
        }
        let id = joined.unwrap_or_else(|| {
            self.next_id += 1;
            let mut toast = Toast {
                id: self.next_id,
                leaf,
                host,
                session,
                kind,
                leaving: false,
                left_ms: SHOW_MS,
                deadline: None,
                hovered: false,
                gone_at: None,
            };
            toast.arm(now_ms, running);
            self.list.push(toast);
            self.next_id
        });
        while self.list.iter().filter(|t| !t.leaving).count() > MAX_SHOWN {
            if let Some(at) = self.list.iter().position(|t| !t.leaving) {
                self.list.remove(at);
            }
        }
        id
    }

    /// Pauses (window inactive) or resumes every toast's time, keeping what is left.
    pub fn set_running(&mut self, running: bool, now_ms: u64) {
        self.running = running;
        for toast in &mut self.list {
            if running {
                if toast.deadline.is_none() {
                    toast.arm(now_ms, true);
                }
            } else {
                // A pointer that left with the window never reports its leave: forget the hover.
                toast.hovered = false;
                toast.pause(now_ms);
            }
        }
    }

    /// Pauses or resumes one toast's time (the pointer over it).
    pub fn set_hovered(&mut self, id: u64, hovered: bool, now_ms: u64) {
        let running = self.running;
        if let Some(toast) = self.list.iter_mut().find(|t| t.id == id) {
            toast.hovered = hovered;
            if hovered {
                toast.pause(now_ms);
            } else if toast.deadline.is_none() {
                toast.arm(now_ms, running);
            }
        }
    }

    /// Removes a toast at once.
    pub fn dismiss(&mut self, id: u64) {
        self.list.retain(|t| t.id != id);
    }

    /// Removes the toasts of a closed pane at once.
    pub fn remove_leaf(&mut self, leaf: NodeId) {
        self.list.retain(|t| t.leaf != leaf);
    }

    /// Moves the queue to `now_ms`: a toast past its deadline starts leaving (removed after `fade_ms`, at once when 0),
    /// a leaving one past its end is removed. Returns whether anything changed.
    pub fn tick(&mut self, now_ms: u64, fade_ms: u64) -> bool {
        let before = self.list.len();
        let mut changed = false;
        for toast in &mut self.list {
            if toast.deadline.is_some_and(|d| d <= now_ms) {
                toast.deadline = None;
                toast.leaving = true;
                toast.gone_at = Some(now_ms + fade_ms);
                changed = true;
            }
        }
        self.list.retain(|t| t.gone_at.is_none_or(|g| g > now_ms));
        changed || self.list.len() != before
    }

    /// Milliseconds until the next change (a deadline or the end of a fade); `None` while nothing counts down.
    pub fn next_wake(&self, now_ms: u64) -> Option<u64> {
        self.list.iter().filter_map(|t| t.deadline.or(t.gone_at)).min().map(|at| at.saturating_sub(now_ms))
    }

    /// The toasts, oldest first (drawn bottom-up: the newest nearest the corner).
    pub fn shown(&self) -> &[Toast] {
        &self.list
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::message::SentFile;

    fn push(t: &mut Toasts, leaf: NodeId, session: &str, kind: AlertKind, now: u64, running: bool) -> u64 {
        t.push(leaf, HostId::local(), session.into(), kind, now, running)
    }

    fn needs() -> AlertKind {
        AlertKind::NeedsInput { reason: None }
    }

    fn files(ids: &[&str]) -> AlertKind {
        let files = ids
            .iter()
            .map(|id| SentFile {
                id: (*id).into(),
                path: format!("/r/{id}.md").into(),
                sent_ms: 0,
                caption: None,
                backlog: false,
            })
            .collect();
        AlertKind::FileSent { files }
    }

    fn file_ids(t: &Toast) -> Vec<String> {
        match &t.kind {
            AlertKind::FileSent { files } => files.iter().map(|f| f.id.clone()).collect(),
            _ => panic!("not a file toast"),
        }
    }

    #[test]
    fn a_toast_leaves_after_six_seconds() {
        let mut t = Toasts::default();
        push(&mut t, 1, "s", needs(), 0, true);
        assert_eq!(t.next_wake(0), Some(6000));
        assert!(!t.tick(5999, 250));
        assert!(t.tick(6000, 250));
        assert!(t.shown()[0].leaving);
        assert_eq!(t.next_wake(6000), Some(250));
        assert!(!t.tick(6249, 250));
        assert!(t.tick(6250, 250));
        assert!(t.shown().is_empty());
        assert_eq!(t.next_wake(6250), None);

        push(&mut t, 1, "s", needs(), 0, true);
        assert!(t.tick(6000, 0));
        assert!(t.shown().is_empty());
    }

    #[test]
    fn hover_pauses_only_that_toast() {
        let mut t = Toasts::default();
        let a = push(&mut t, 1, "a", needs(), 0, true);
        push(&mut t, 2, "b", needs(), 0, true);
        t.set_hovered(a, true, 1000);
        assert!(t.tick(7000, 250));
        assert_eq!(t.shown().len(), 2);
        assert!(!t.shown()[0].leaving);
        assert!(t.shown()[1].leaving);
        t.set_hovered(a, false, 8000);
        // 5000 ms were left at the hover: the deadline is 13000.
        assert!(t.tick(8250, 250));
        assert_eq!(t.shown().len(), 1);
        assert_eq!(t.next_wake(8250), Some(4750));
    }

    #[test]
    fn an_inactive_window_holds_the_time() {
        let mut t = Toasts::default();
        push(&mut t, 1, "s", needs(), 0, false);
        assert_eq!(t.next_wake(0), None);
        assert!(!t.tick(100_000, 250));
        t.set_running(true, 50_000);
        assert_eq!(t.next_wake(50_000), Some(6000));
        t.set_running(false, 52_000);
        assert_eq!(t.next_wake(52_000), None);
        t.set_running(true, 60_000);
        assert_eq!(t.next_wake(60_000), Some(4000));
    }

    #[test]
    fn pausing_the_window_forgets_a_hover() {
        let mut t = Toasts::default();
        let a = push(&mut t, 1, "a", needs(), 0, true);
        t.set_hovered(a, true, 1000);
        t.set_running(false, 2000);
        assert!(!t.shown()[0].hovered);
        t.set_running(true, 3000);
        assert!(t.next_wake(3000).is_some(), "the time runs again with no pointer leave");
    }

    #[test]
    fn at_most_three_and_the_oldest_goes() {
        let mut t = Toasts::default();
        for (i, s) in ["a", "b", "c", "d"].iter().enumerate() {
            push(&mut t, i as u64, s, needs(), 0, true);
        }
        let sessions: Vec<_> = t.shown().iter().map(|x| x.session.as_str()).collect();
        assert_eq!(sessions, ["b", "c", "d"]);
    }

    #[test]
    fn file_sent_joins_its_session_toast() {
        let mut t = Toasts::default();
        let first = push(&mut t, 1, "s", files(&["a", "b"]), 0, true);
        push(&mut t, 2, "other", needs(), 100, true);
        let again = push(&mut t, 1, "s", files(&["b", "c", "d"]), 2000, true);
        assert_eq!(first, again);
        assert_eq!(t.shown().len(), 2);
        let last = t.shown().last().unwrap();
        assert_eq!(last.id, first);
        assert_eq!(file_ids(last), ["a", "b", "c", "d"]);
        // The other toast (deadline 6100) is nearer than the restarted one (8000).
        assert_eq!(t.next_wake(2000), Some(4100));
        assert!(t.tick(6100, 250));
        assert_eq!(t.next_wake(6100), Some(250));
        t.tick(6350, 250);
        assert_eq!(t.next_wake(6350), Some(1650));
    }

    #[test]
    fn a_leaving_toast_is_not_joined() {
        let mut t = Toasts::default();
        push(&mut t, 1, "s", files(&["a"]), 0, true);
        t.tick(6000, 250);
        push(&mut t, 1, "s", files(&["b"]), 6100, true);
        assert_eq!(t.shown().len(), 2);
        assert!(t.shown()[0].leaving);
        assert_eq!(file_ids(&t.shown()[1]), ["b"]);
    }

    #[test]
    fn a_status_toast_replaces_the_previous_one_of_its_session() {
        let mut t = Toasts::default();
        push(&mut t, 1, "s", needs(), 0, true);
        push(&mut t, 1, "s", AlertKind::Finished, 10, true);
        assert_eq!(t.shown().len(), 1);
        assert_eq!(t.shown()[0].kind, AlertKind::Finished);
    }

    #[test]
    fn closing_a_pane_drops_its_toasts() {
        let mut t = Toasts::default();
        push(&mut t, 1, "a", needs(), 0, true);
        push(&mut t, 2, "b", needs(), 0, true);
        t.remove_leaf(1);
        assert_eq!(t.shown().len(), 1);
        assert_eq!(t.shown()[0].leaf, 2);
        let id = t.shown()[0].id;
        t.dismiss(id);
        assert!(t.shown().is_empty());
    }

    #[test]
    fn twenty_updates_of_one_session_stay_one_toast() {
        let mut t = Toasts::default();
        for i in 0..20u64 {
            push(&mut t, 1, "s", needs(), i, true);
            push(&mut t, 1, "s", files(&[&format!("f{i}")]), i, true);
        }
        // One status toast and one file toast.
        assert_eq!(t.shown().len(), 2);
        assert_eq!(file_ids(&t.shown()[1]).len(), 20);
    }
}
