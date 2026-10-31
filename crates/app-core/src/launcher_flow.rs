//! The launcher's steps, without any drawing: the host step (pick a host, connect it in place), the folder step (that
//! host's folders) and the folder browser. The view applies what these return.

use std::path::{Path, PathBuf};

use protocol::host::HostId;
use protocol::message::DirEntry;

use crate::folder_browser::{self, Crumb, Shown};
use crate::host_state::{HostPick, HostState, host_pick};
use crate::launcher::{self, LauncherItem, OpenFolder, RecentFolder, Typed};
use crate::text_field::TextField;

/// One entry of the launcher's hosts: this machine, then each SSH host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherHost {
    /// The host.
    pub id: HostId,
    /// The host's daemon is connected (always true for this machine).
    pub connected: bool,
    /// Where its connection stands.
    pub state: HostState,
    /// The second line: this machine's host name, a remote's ssh target.
    pub detail: String,
    /// How many of its panes run now.
    pub sessions: usize,
}

/// How the second line of a host row reads, and in what tone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineTone {
    /// Secondary text.
    Muted,
    /// Working on it (amber).
    Busy,
    /// A failure (red).
    Bad,
}

/// The second line of a host row: where it stands, as the host step shows it.
pub fn host_line(host: &LauncherHost) -> (String, LineTone) {
    let with = |text: String| if host.detail.is_empty() { text } else { format!("{} · {text}", host.detail) };
    match &host.state {
        HostState::Connected if host.id.is_local() => (host.detail.clone(), LineTone::Muted),
        HostState::Connecting => ("Connecting…".into(), LineTone::Busy),
        HostState::Reconnecting { .. } => ("Reconnecting…".into(), LineTone::Busy),
        HostState::Failed { reason } => (format!("Couldn't connect: {reason}"), LineTone::Bad),
        state => (with(state.label()), LineTone::Muted),
    }
}

/// "N sessions" for the right of a connected host's row; nothing at 0.
pub fn sessions_label(sessions: usize) -> Option<String> {
    match sessions {
        0 => None,
        1 => Some("1 session".into()),
        n => Some(format!("{n} sessions")),
    }
}

/// The launcher's current step.
#[derive(Clone, Debug)]
pub enum Step {
    /// Pick where the session runs.
    Hosts,
    /// Pick a folder on this host.
    Folders(HostId),
    /// Browse the folders of a host.
    Browse(Browser),
}

/// What picking a host row asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Picked {
    /// Nothing to do (a wait started on its own, or no such row).
    Nothing,
    /// The host is connected: its folders are shown.
    Folders,
    /// Run the host's action (connect, install, update); `dismiss`: close the launcher, the wizard takes over.
    Action {
        /// The host.
        host: HostId,
        /// Close the launcher too.
        dismiss: bool,
    },
}

/// What Esc does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Escape {
    /// Clear the field with text in it.
    ClearField,
    /// Moved back one step.
    Back,
    /// Close the launcher.
    Dismiss,
}

/// The launcher's state: the hosts, the step, and the folder list of the step 2 host.
#[derive(Clone, Debug)]
pub struct Flow {
    /// This machine and the SSH hosts.
    pub hosts: Vec<LauncherHost>,
    /// The step.
    pub step: Step,
    /// The highlighted host row.
    pub host_row: usize,
    waiting: Option<HostId>,
    /// The folders with panes open now.
    pub open: Vec<OpenFolder>,
    /// The recent folders of every host.
    pub recent: Vec<RecentFolder>,
    /// The favorites, as saved in the settings.
    pub favorites: Vec<PathBuf>,
    home: Option<PathBuf>,
    /// The folder rows of the step 2 host.
    pub items: Vec<LauncherItem>,
    /// The highlighted folder row.
    pub selected: usize,
}

impl Flow {
    /// The flow over `hosts` (this machine first): the host step when there is an SSH host, else this machine's folders.
    pub fn new(
        hosts: Vec<LauncherHost>,
        open: Vec<OpenFolder>,
        recent: Vec<RecentFolder>,
        favorites: Vec<PathBuf>,
        home: Option<PathBuf>,
    ) -> Flow {
        let step = if hosts.len() > 1 { Step::Hosts } else { Step::Folders(HostId::local()) };
        let mut flow = Flow {
            hosts,
            step,
            host_row: 0,
            waiting: None,
            open,
            recent,
            favorites,
            home,
            items: Vec::new(),
            selected: 0,
        };
        flow.rebuild("");
        flow
    }

    /// There is a host to pick (an SSH host is listed).
    pub fn has_host_step(&self) -> bool {
        self.hosts.len() > 1
    }

    /// The row of `host`.
    pub fn host(&self, host: &HostId) -> Option<&LauncherHost> {
        self.hosts.iter().find(|known| known.id == *host)
    }

    /// The host is connected (this machine always is).
    pub fn connected(&self, host: &HostId) -> bool {
        host.is_local() || self.host(host).is_some_and(|known| known.connected)
    }

    /// The host of the folder step or the browser.
    pub fn picked(&self) -> Option<&HostId> {
        match &self.step {
            Step::Hosts => None,
            Step::Folders(host) => Some(host),
            Step::Browse(browser) => Some(&browser.host),
        }
    }

    /// The view waits for a host to connect.
    pub fn is_waiting(&self) -> bool {
        self.waiting.is_some()
    }

    /// Move the highlighted host row.
    pub fn move_host_row(&mut self, down: bool) {
        let last = self.hosts.len().saturating_sub(1);
        self.host_row = if down { (self.host_row + 1).min(last) } else { self.host_row.saturating_sub(1) };
    }

    /// Pick host row `index`: a connected host opens its folders; any other waits and asks for its action.
    pub fn pick(&mut self, index: usize) -> Picked {
        let Some(row) = self.hosts.get(index) else { return Picked::Nothing };
        let host = row.id.clone();
        self.host_row = index;
        match host_pick(&row.state) {
            HostPick::Open => {
                self.waiting = None;
                self.to_folders(host);
                Picked::Folders
            }
            HostPick::Connect | HostPick::Retry => {
                self.waiting = Some(host.clone());
                Picked::Action { host, dismiss: false }
            }
            HostPick::Wait | HostPick::Cancel => {
                self.waiting = Some(host);
                Picked::Nothing
            }
            HostPick::Install | HostPick::Update => {
                self.waiting = None;
                Picked::Action { host, dismiss: true }
            }
        }
    }

    /// The Cancel button of a connecting host: ends the wait for it.
    pub fn cancel(&mut self, host: &HostId) {
        if self.waiting.as_ref() == Some(host) {
            self.waiting = None;
        }
    }

    /// New host states. The host being waited for, once connected, opens its folders: returns it.
    pub fn set_hosts(&mut self, hosts: Vec<LauncherHost>) -> Option<HostId> {
        self.hosts = hosts;
        self.host_row = self.host_row.min(self.hosts.len().saturating_sub(1));
        let waited = self.waiting.clone()?;
        if !matches!(self.step, Step::Hosts) {
            self.waiting = None;
            return None;
        }
        let up = self.host(&waited).is_some_and(|known| host_pick(&known.state) == HostPick::Open);
        if !up {
            return None;
        }
        self.waiting = None;
        self.to_folders(waited.clone());
        Some(waited)
    }

    /// Show `host`'s folders.
    pub fn to_folders(&mut self, host: HostId) {
        self.waiting = None;
        self.step = Step::Folders(host);
        self.rebuild("");
    }

    /// Back to the host step.
    pub fn to_hosts(&mut self) {
        self.waiting = None;
        self.step = Step::Hosts;
    }

    /// Esc: clear a non-empty field (`field_empty` false), else step back; the browser goes back to the folders, the
    /// folder step to the hosts, or closes without a host step.
    pub fn escape(&mut self, field_empty: bool) -> Escape {
        if !field_empty {
            return Escape::ClearField;
        }
        match &self.step {
            Step::Hosts => Escape::Dismiss,
            Step::Folders(_) if !self.has_host_step() => Escape::Dismiss,
            Step::Folders(_) => {
                self.to_hosts();
                Escape::Back
            }
            Step::Browse(browser) => {
                let host = browser.host.clone();
                self.to_folders(host);
                Escape::Back
            }
        }
    }

    /// The step 2 rows for `query` (this host only); the highlight goes to the first row.
    pub fn rebuild(&mut self, query: &str) {
        let Step::Folders(host) = &self.step else { return };
        let home = self.home.as_deref().filter(|_| host.is_local());
        self.items = launcher::items_on(host, &self.open, &self.recent, query, home, &self.favorites);
        self.selected = 0;
    }

    /// The open and recent folders changed: the rows follow while the step 2 list is showing a filter (not a typed
    /// path, not the host or browser steps), and the highlight stays on the same (host, folder) row if it is still listed.
    pub fn set_folders(&mut self, open: Vec<OpenFolder>, recent: Vec<RecentFolder>, query: &str) {
        self.open = open;
        self.recent = recent;
        if !matches!(self.step, Step::Folders(_)) || launcher::typed(query) != Typed::Filter {
            return;
        }
        let kept = self.items.get(self.selected).map(|item| (item.host().clone(), item.folder().cloned()));
        self.rebuild(query);
        if let Some((host, folder)) = kept
            && let Some(at) =
                self.items.iter().position(|item| *item.host() == host && item.folder().cloned() == folder)
        {
            self.selected = at;
        }
    }

    /// Start the browser on the step 2 host.
    pub fn to_browse(&mut self, host: HostId) {
        self.waiting = None;
        self.step = Step::Browse(Browser::new(host));
    }

    /// The browser, when it is the step.
    pub fn browser(&mut self) -> Option<&mut Browser> {
        match &mut self.step {
            Step::Browse(browser) => Some(browser),
            _ => None,
        }
    }
}

/// The folder browser: the listed folder, its subfolders and the filter over them.
#[derive(Clone, Debug)]
pub struct Browser {
    /// The host browsed.
    pub host: HostId,
    /// The folder shown (absolute, as the daemon expanded it); none until the first answer.
    pub path: Option<String>,
    /// The host's home folder, learned from the first answer (the listing of `~`).
    pub home: Option<String>,
    /// The subfolders of `path`, in the daemon's order.
    pub dirs: Vec<DirEntry>,
    /// Why the last listing failed.
    pub error: Option<String>,
    /// The newest listing request; replies to any other are ignored.
    pub request: u64,
    /// The filter over the subfolders.
    pub filter: TextField,
    /// The highlighted shown row.
    pub selected: usize,
    asked: String,
}

impl Browser {
    /// A browser on `host` that has not asked for anything yet.
    pub fn new(host: HostId) -> Browser {
        Browser {
            host,
            path: None,
            home: None,
            dirs: Vec::new(),
            error: None,
            request: 0,
            filter: TextField::default(),
            selected: 0,
            asked: String::new(),
        }
    }

    /// `request` was sent for `path` (`~` the first time): its answer is the one to take.
    pub fn ask(&mut self, request: u64, path: &str) {
        self.request = request;
        self.asked = path.to_string();
    }

    /// The listing of `request`; false (and nothing changes) when it is not the newest request.
    pub fn accept(&mut self, request: u64, path: String, dirs: Vec<DirEntry>) -> bool {
        if request != self.request {
            return false;
        }
        if self.asked == "~" {
            self.home = Some(path.clone());
        }
        self.path = Some(path);
        self.dirs = dirs;
        self.error = None;
        self.filter = TextField::default();
        self.selected = 0;
        true
    }

    /// `request` failed; false when it is not the newest request. The list is empty and the asked folder is shown.
    pub fn fail(&mut self, request: u64, reason: String) -> bool {
        if request != self.request {
            return false;
        }
        if self.asked != "~" {
            self.path = Some(self.asked.clone());
        }
        self.dirs.clear();
        self.error = Some(reason);
        self.filter = TextField::default();
        self.selected = 0;
        true
    }

    /// The subfolders that pass the filter.
    pub fn shown(&self) -> Vec<Shown> {
        folder_browser::filter(&self.dirs, self.filter.text())
    }

    /// The breadcrumbs of the current folder.
    pub fn crumbs(&self) -> Vec<Crumb> {
        self.path.as_deref().map(|path| folder_browser::crumbs(path, self.home.as_deref())).unwrap_or_default()
    }

    /// The current folder as shown in "Open here": `~/a`, or absolute.
    pub fn shown_path(&self) -> Option<String> {
        self.path.as_deref().map(|path| folder_browser::shown(path, self.home.as_deref()))
    }

    /// The path of shown row `row`.
    pub fn child_path(&self, row: usize) -> Option<String> {
        let shown = self.shown();
        let dir = self.dirs.get(shown.get(row)?.index)?;
        Some(folder_browser::child(self.path.as_deref()?, &dir.name))
    }

    /// The folder above the current one.
    pub fn parent_path(&self) -> Option<String> {
        folder_browser::parent(self.path.as_deref()?)
    }

    /// Move the highlight within the shown rows.
    pub fn step(&mut self, down: bool) {
        let last = self.shown().len().saturating_sub(1);
        self.selected = if down { (self.selected + 1).min(last) } else { self.selected.saturating_sub(1) };
    }

    /// "Open here" works: a folder is listed without error, and the host is connected.
    pub fn can_open_here(&self, connected: bool) -> bool {
        connected && self.error.is_none() && self.path.is_some()
    }

    /// The folder "Open here" opens.
    pub fn open_here(&self) -> Option<PathBuf> {
        self.path.as_deref().map(|path| Path::new(path).to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, state: HostState) -> LauncherHost {
        let id = if name == "local" { HostId::local() } else { HostId::named(name) };
        LauncherHost { connected: state == HostState::Connected, id, state, detail: String::new(), sessions: 0 }
    }

    fn local() -> LauncherHost {
        host("local", HostState::Connected)
    }

    fn flow(hosts: Vec<LauncherHost>) -> Flow {
        Flow::new(hosts, Vec::new(), Vec::new(), Vec::new(), None)
    }

    fn dirs(names: &[&str]) -> Vec<DirEntry> {
        names.iter().map(|name| DirEntry { name: (*name).into(), symlink: false }).collect()
    }

    fn recent(host: &HostId, path: &str, last_active_ms: u64) -> RecentFolder {
        RecentFolder { host: host.clone(), path: path.into(), last_active_ms }
    }

    #[test]
    fn host_lines_and_session_counts() {
        let line = |state| host_line(&LauncherHost { detail: "me@box".into(), ..host("devbox", state) });
        assert_eq!(line(HostState::Connected), ("me@box · connected".into(), LineTone::Muted));
        assert_eq!(line(HostState::NotConnected), ("me@box · not connected".into(), LineTone::Muted));
        assert_eq!(line(HostState::Connecting), ("Connecting…".into(), LineTone::Busy));
        assert_eq!(line(HostState::Reconnecting { attempt: 1 }), ("Reconnecting…".into(), LineTone::Busy));
        assert_eq!(
            line(HostState::Failed { reason: "timed out".into() }),
            ("Couldn't connect: timed out".into(), LineTone::Bad)
        );
        assert_eq!(line(HostState::TakenOver).0, "me@box · opened elsewhere");
        let this = LauncherHost { detail: "idogr-pc".into(), ..local() };
        assert_eq!(host_line(&this).0, "idogr-pc");
        assert_eq!(sessions_label(0), None);
        assert_eq!(sessions_label(1).as_deref(), Some("1 session"));
        assert_eq!(sessions_label(3).as_deref(), Some("3 sessions"));
    }

    #[test]
    fn starts_on_hosts_only_with_ssh_hosts() {
        assert!(matches!(flow(vec![local()]).step, Step::Folders(ref h) if h.is_local()));
        let two = flow(vec![local(), host("devbox", HostState::NotConnected)]);
        assert!(matches!(two.step, Step::Hosts));
        assert!(two.has_host_step());
    }

    #[test]
    fn a_waited_host_that_connects_opens_its_folders() {
        let mut flow = flow(vec![local(), host("devbox", HostState::NotConnected)]);
        let picked = flow.pick(1);
        assert_eq!(picked, Picked::Action { host: HostId::named("devbox"), dismiss: false });
        assert!(matches!(flow.step, Step::Hosts));
        assert!(flow.is_waiting());
        let moved = flow.set_hosts(vec![local(), host("devbox", HostState::Connected)]);
        assert_eq!(moved, Some(HostId::named("devbox")));
        assert!(matches!(flow.step, Step::Folders(ref h) if h.name() == "devbox"));
        assert!(!flow.is_waiting());
    }

    #[test]
    fn a_wait_ends_when_going_back_or_picking_another() {
        let hosts = |devbox, gpu| vec![local(), host("devbox", devbox), host("gpu", gpu)];
        let mut flow = flow(hosts(HostState::NotConnected, HostState::NotConnected));
        flow.pick(1);
        flow.pick(2);
        assert_eq!(flow.set_hosts(hosts(HostState::Connected, HostState::Connecting)), None);
        assert!(matches!(flow.step, Step::Hosts));
        flow.set_hosts(hosts(HostState::Connected, HostState::Connecting));
        assert_eq!(flow.set_hosts(hosts(HostState::Connected, HostState::Connected)), Some(HostId::named("gpu")));

        // Back to the hosts: the wait is over.
        let mut flow = self::flow(hosts(HostState::NotConnected, HostState::NotConnected));
        flow.pick(1);
        flow.to_hosts();
        assert_eq!(flow.set_hosts(hosts(HostState::Connected, HostState::NotConnected)), None);
        assert!(matches!(flow.step, Step::Hosts));
    }

    #[test]
    fn a_cancelled_wait_does_not_jump() {
        let mut flow = flow(vec![local(), host("devbox", HostState::Connecting)]);
        flow.pick(1);
        flow.cancel(&HostId::named("devbox"));
        assert_eq!(flow.set_hosts(vec![local(), host("devbox", HostState::Connected)]), None);
    }

    #[test]
    fn picking_install_dismisses() {
        let mut flow = flow(vec![local(), host("devbox", HostState::NotInstalled)]);
        assert_eq!(flow.pick(1), Picked::Action { host: HostId::named("devbox"), dismiss: true });
        assert!(!flow.is_waiting());
        let mut flow = self::flow(vec![local(), host("devbox", HostState::UpdateNeeded { remote: None })]);
        assert_eq!(flow.pick(1), Picked::Action { host: HostId::named("devbox"), dismiss: true });
    }

    #[test]
    fn picking_a_connected_host_opens_its_folders_and_a_reconnecting_one_only_waits() {
        let mut flow = flow(vec![local(), host("devbox", HostState::Reconnecting { attempt: 1 })]);
        assert_eq!(flow.pick(1), Picked::Nothing);
        assert!(flow.is_waiting());
        assert_eq!(flow.pick(0), Picked::Folders);
        assert!(!flow.is_waiting());
    }

    #[test]
    fn folders_list_only_the_picked_host() {
        let devbox = HostId::named("devbox");
        let recent = vec![recent(&HostId::local(), "/l", 9), recent(&devbox, "/d", 1)];
        let mut flow =
            Flow::new(vec![local(), host("devbox", HostState::Connected)], Vec::new(), recent, Vec::new(), None);
        flow.pick(1);
        assert_eq!(flow.items.len(), 1);
        assert_eq!(*flow.items[0].host(), devbox);
    }

    #[test]
    fn escape_steps_back() {
        let hosts = vec![local(), host("devbox", HostState::Connected)];
        let mut flow = flow(hosts);
        flow.pick(1);
        assert_eq!(flow.escape(false), Escape::ClearField);
        assert!(matches!(flow.step, Step::Folders(_)));
        assert_eq!(flow.escape(true), Escape::Back);
        assert!(matches!(flow.step, Step::Hosts));
        assert_eq!(flow.escape(true), Escape::Dismiss);

        let mut solo = self::flow(vec![local()]);
        assert_eq!(solo.escape(true), Escape::Dismiss);

        flow.pick(1);
        flow.to_browse(HostId::named("devbox"));
        assert_eq!(flow.escape(false), Escape::ClearField);
        assert_eq!(flow.escape(true), Escape::Back);
        assert!(matches!(flow.step, Step::Folders(_)));
    }

    #[test]
    fn new_folders_keep_the_highlight() {
        let local = HostId::local();
        let mut flow = Flow::new(
            vec![self::local()],
            Vec::new(),
            vec![recent(&local, "/a", 30), recent(&local, "/b", 20), recent(&local, "/c", 10)],
            Vec::new(),
            None,
        );
        flow.selected = 1;
        let more = vec![recent(&local, "/n", 40), recent(&local, "/a", 30), recent(&local, "/b", 20)];
        flow.set_folders(Vec::new(), more, "");
        assert_eq!(flow.items[flow.selected].folder(), Some(&PathBuf::from("/b")));
        assert_eq!(flow.selected, 2);
        // A folder that left the list sends the highlight to the first row.
        flow.set_folders(Vec::new(), vec![recent(&local, "/z", 1)], "");
        assert_eq!(flow.selected, 0);
    }

    #[test]
    fn no_rebuild_in_path_mode() {
        let local = HostId::local();
        let mut flow = Flow::new(vec![self::local()], Vec::new(), vec![recent(&local, "/a", 1)], Vec::new(), None);
        flow.set_folders(Vec::new(), vec![recent(&local, "/b", 2), recent(&local, "/a", 1)], "~/x");
        assert_eq!(flow.items.len(), 1);
        // Nor in the browser or on the host step.
        flow.to_browse(local);
        flow.set_folders(Vec::new(), Vec::new(), "");
        assert_eq!(flow.items.len(), 1);
    }

    #[test]
    fn browse_starts_at_home() {
        let mut browser = Browser::new(HostId::local());
        browser.ask(1, "~");
        assert!(browser.accept(1, "/home/u".into(), dirs(&["work"])));
        let labels: Vec<String> = browser.crumbs().into_iter().map(|crumb| crumb.label).collect();
        assert_eq!(labels, ["~"]);
        assert_eq!(browser.shown_path().as_deref(), Some("~"));
    }

    #[test]
    fn stale_listing_is_ignored() {
        let mut browser = Browser::new(HostId::local());
        browser.ask(1, "~");
        browser.accept(1, "/home/u".into(), dirs(&["work"]));
        browser.ask(2, "/home/u/work");
        browser.ask(3, "/home/u");
        assert!(!browser.accept(2, "/home/u/work".into(), dirs(&["x"])));
        assert_eq!(browser.path.as_deref(), Some("/home/u"));
        assert!(!browser.fail(2, "late".into()));
        assert!(browser.error.is_none());
        assert!(browser.accept(3, "/home/u".into(), dirs(&["work", "docs"])));
        assert_eq!(browser.dirs.len(), 2);
    }

    #[test]
    fn open_here_is_disabled_after_a_failed_listing() {
        let mut browser = Browser::new(HostId::local());
        browser.ask(1, "~");
        browser.accept(1, "/home/u".into(), dirs(&["work"]));
        assert!(browser.can_open_here(true));
        assert!(!browser.can_open_here(false));
        browser.ask(2, "/home/u/work");
        assert!(browser.fail(2, "Permission denied".into()));
        assert!(!browser.can_open_here(true));
        assert_eq!(browser.path.as_deref(), Some("/home/u/work"));
        assert_eq!(browser.parent_path().as_deref(), Some("/home/u"));
        assert!(browser.dirs.is_empty());
    }

    #[test]
    fn children_and_parents_follow_the_filter() {
        let mut browser = Browser::new(HostId::local());
        browser.ask(1, "~");
        browser.accept(1, "/home/u".into(), dirs(&["docs", "work"]));
        browser.filter.insert("wo");
        assert_eq!(browser.child_path(0).as_deref(), Some("/home/u/work"));
        assert_eq!(browser.parent_path().as_deref(), Some("/home"));
        assert_eq!(browser.open_here(), Some(PathBuf::from("/home/u")));
    }
}
