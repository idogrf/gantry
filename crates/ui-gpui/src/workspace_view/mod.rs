//! The window's root view: tabs of split panes, routing daemon events to them, and saving the layout.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use app_core::attention::{AlertKind, AttentionTracker};
use app_core::host_state::HostState;
use app_core::release::Release;
use app_core::search_merge::HostSearch;
use app_core::settings::ThemeChoice;
use app_core::split_tree::{Axis, Divider, NodeId, Rect};
use app_core::text_field::TextField;
use app_core::theme_colors::Preset;
use app_core::version::Version;
use app_core::workspace::{ClaudeDirs, KilledOffline, Restored, Workspace};
use daemon::askpass::AskPrompt;
use daemon::client::{ClientEvent, Connection, DaemonClient, batch_events};
use daemon::ssh::{AttemptHandle, Share};
use gpui::{
    Bounds, Context, Entity, FocusHandle, Pixels, Point, SharedString, Subscription, Task, UniformListScrollHandle,
    Window, prelude::*,
};
use notify::RecommendedWatcher;
use protocol::host::{HostId, PaneKey};

use app_core::settings::SshHost;

use crate::askpass_view::AskpassView;
use crate::context_menu::OpenMenu;
use crate::git_diff_view::DiffState;
use crate::git_panel::PanelTab;
use crate::git_scrollbar::{self};
use crate::git_state::GitRepos;
use crate::launcher_view::LauncherView;
use crate::pane_drag::PaneDrop;
use crate::recents_view::RecentsView;
use crate::release_check::CheckRow;
use crate::settings_store::SettingsStore;
use crate::settings_view::SettingsView;
use crate::setup_view::{Mode as SetupMode, SetupView, opens_by_itself};
use crate::sidebar_view::{SidebarEvent, SidebarView};
use crate::ssh_wizard::SshWizard;
use crate::terminal_view::TerminalView;
use crate::theme::Theme;
use crate::update_watch::{self, DaemonBuild};

/// Space between split panes (and around them).
const PANE_GAP: f32 = 8.0;
/// Width of the sidebar's drag edge.
const SIDEBAR_EDGE: f32 = 1.0;
/// Largest amount of output routed per UI update, so input and rendering keep running during a flood.
const MAX_BATCH_BYTES: usize = 256 * 1024;
#[allow(missing_docs)]
mod workspace_actions {
    gpui::actions!(
        workspace,
        [
            NewClaudeTab,
            NewShellTab,
            SplitRightClaude,
            SplitRightShell,
            SplitDownClaude,
            SplitDownShell,
            ClosePane,
            FocusLeft,
            FocusRight,
            FocusUp,
            FocusDown,
            PreviousTab,
            NextTab,
            HistoryBack,
            HistoryForward,
            ToggleSidebar,
            ToggleGitPanel,
            FindSession,
            OpenLauncher,
            OpenRecents,
            OpenSettings
        ]
    );
}
use workspace_actions::*;

mod actions;
mod events;
mod git;
mod hosts;
mod layout;
mod overlays;
mod panes;
mod render;
mod resume;
mod sidebar_menus;
mod tab_content;
mod title_bar;

pub(crate) use actions::all_bindings;

struct Slot {
    view: Entity<TerminalView>,
    _subscription: Subscription,
}

/// Where a taken-over session is resumed.
enum TakeOverTarget {
    /// A new tab, or a split of the focused pane.
    NewPane {
        /// The session's directory.
        cwd: PathBuf,
        /// As a split.
        split: bool,
        /// Host folder of the dev container it ran in.
        container: Option<PathBuf>,
    },
    /// The finished leaf that last ran it (Continue).
    Leaf(NodeId),
}

/// A split or close the user asked for, drawn from `from` to the active tab's layout over `PANE_ANIM_MS`.
struct PaneAnim {
    /// Where each pane was drawn when the change happened (unit space); a new leaf starts as a sliver.
    from: Vec<(NodeId, Rect)>,
    started: Instant,
    /// The new leaf of a split and the axis it slides in along.
    entering: Option<(NodeId, Axis)>,
}

/// The git page: one repo's changed files and the diff of the file picked.
struct GitPage {
    host: HostId,
    folder: String,
    /// The repo's folder name, for the sidebar row.
    label: String,
    /// The file picked: its path, and the old path of a rename.
    picked: Option<(String, Option<String>)>,
    /// The diff request in flight or last answered.
    request: u64,
    state: DiffState,
    /// The picked file's added and removed counts when its diff was asked for; a change asks again.
    counts: Option<(Option<u32>, Option<u32>)>,
    /// Rows selected for copying: where the selection started and its other end.
    selection: Option<(usize, usize)>,
    /// The commit shown instead of the working changes.
    commit: Option<CommitView>,
}

/// A commit on the git page: asked for by hash, then its message and files.
struct CommitView {
    hash: String,
    /// The `GitCommit` request in flight or last answered.
    request: u64,
    detail: Option<protocol::message::GitCommitDetail>,
    failed: Option<String>,
}

/// The diff page's font and cell sizes.
struct GitMetrics {
    family: SharedString,
    size: f32,
    row_height: f32,
    cell_width: f32,
}

/// The git panel opening or closing, from `started`.
struct PanelAnim {
    started: Instant,
    opening: bool,
}

/// Now, in seconds since the Unix epoch.
fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// A new session waiting to learn whether a dev container mounts its directory.
struct PendingOpen {
    claude: bool,
    /// The config dir chosen for this one launch (launcher chip or menu), by name.
    pick: Option<String>,
    cwd: PathBuf,
    axis: Option<Axis>,
    cwd_from: Option<PaneKey>,
    /// The host a split runs on (the source pane's); `local` otherwise.
    host: HostId,
}

/// One connected host's daemon.
struct HostConn {
    client: DaemonClient,
    /// False once the connection ended; it is not re-established.
    connected: bool,
    /// The daemon's app version, if it sent one (`None` for this machine and the debug seam's hosts: no update pill).
    daemon_version: Option<Version>,
    /// Keeps routing this host's events to the view.
    _events: Task<()>,
}

/// Where an SSH host's daemon update stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdatePhase {
    /// `gantryd` is being uploaded and verified; the old daemon keeps running.
    Uploading,
    /// The old daemon was asked to stop; the next connect starts the new one.
    Restarting,
}

/// A running update of one SSH host's daemon. The entry exists only while the update runs: a failure or a drop removes it.
struct HostUpdate {
    phase: UpdatePhase,
    /// Cancels the upload.
    handle: AttemptHandle,
    /// Reads the upload's messages, then (restarting) waits out the 5 s cap for the old connection to end.
    task: Task<()>,
    /// The ssh connection the upload ran through, handed to the connect that follows; not in `shares` before that.
    share: Option<Share>,
    /// Where the pill was clicked, for the card that reports a failure.
    position: Point<Pixels>,
}

/// What the host update card says.
enum CardKind {
    /// Asks before restarting: this many running panes end.
    Ask {
        /// Running panes of the host when the pill was clicked.
        running: usize,
    },
    /// The update cannot start, and why.
    Blocked(String),
    /// The update failed, and why.
    Failed(String),
}

/// The open card of an SSH host's update; its focus handle takes Esc.
struct HostUpdateCard {
    host: HostId,
    /// Where the pill was clicked.
    position: Point<Pixels>,
    focus: FocusHandle,
    kind: CardKind,
}

/// Root view of the Gantry window.
pub struct WorkspaceView {
    workspace: Workspace,
    /// Finished leaves waiting for the session list, to resume on start.
    to_resume: Vec<NodeId>,
    /// This machine's daemon has sent the sessions of every configured Claude dir (`ClaudeDirsIndexed`): before that
    /// its list may miss the sessions of dirs it has not scanned, and resuming one of those would start it afresh.
    local_dirs_indexed: bool,
    slots: HashMap<NodeId, Slot>,
    /// Spawn request id → leaf waiting for it.
    pending: HashMap<u64, NodeId>,
    next_request: u64,
    /// What each daemon said about conversation search (hits of the current query, index progress, unavailable).
    search_answers: HashMap<HostId, HostSearch>,
    /// The conversation search request the pop-up sent last (0 = none).
    search_request: u64,
    /// The query the open recent sessions pop-up last searched for (sent again to a host that connects meanwhile).
    search_query: String,
    hosts: HashMap<HostId, HostConn>,
    /// The SSH hosts of the settings.
    ssh_hosts: Vec<SshHost>,
    /// Where each SSH host's connection stands (absent = `NotConnected`).
    host_states: HashMap<HostId, HostState>,
    /// New leaves waiting for their host to connect, to be spawned then.
    queued: HashMap<HostId, Vec<NodeId>>,
    /// The pending reconnect timer of each host.
    retries: HashMap<HostId, Task<()>>,
    /// The attempt running for each host (its number, to ignore a stale result, and its cancel handle).
    attempts: HashMap<HostId, (u64, AttemptHandle)>,
    /// Numbers the attempts.
    next_attempt: u64,
    /// Panes closed while their host was not connected, to kill when it is (and not to adopt again).
    killed_offline: Vec<KilledOffline>,
    /// Hosts that came from the debug seam (`GANTRY_EXTRA_HOSTS`): connected without being in `sshHosts`.
    seam_hosts: HashSet<HostId>,
    /// Where ssh's questions go (the dialog is drawn from the receiver, see the sign-in task).
    askpass_prompts: async_channel::Sender<AskPrompt>,
    /// The sign-in dialog on screen, while ssh asks something.
    askpass: Option<(Entity<AskpassView>, Subscription)>,
    /// Questions waiting for the one on screen.
    askpass_queue: VecDeque<AskPrompt>,
    /// Receives ssh's questions.
    _askpass_task: Task<()>,
    layout_file: PathBuf,
    default_cwd: PathBuf,
    /// The daemon's own Claude config dir (its panes need no `CLAUDE_CONFIG_DIR`).
    daemon_dir: Option<PathBuf>,
    /// The Claude config dirs in effect, from the settings.
    dirs: ClaudeDirs,
    /// The paths last sent to the daemon with `SetClaudeDirs`.
    sent_dirs: Option<Vec<PathBuf>>,
    focus_handle: FocusHandle,
    prompting: Rc<Cell<bool>>,
    /// Divider being dragged.
    dragging: Option<Divider>,
    /// Window bounds of the visible tab's pane area (for divider drags).
    content_bounds: Bounds<Pixels>,
    /// The running split or close animation of the active tab, if any.
    pane_anim: Option<PaneAnim>,
    /// Where the pane being dragged would land, while it is over a pane of the visible tab.
    pane_drop: Option<PaneDrop>,
    sidebar: Entity<SidebarView>,
    /// Window bounds of the sidebar (for width drags).
    sidebar_bounds: Bounds<Pixels>,
    /// The sidebar's edge is being dragged.
    resizing_sidebar: bool,
    _sidebar_events: Subscription,
    /// The open launcher overlay.
    launcher: Option<(Entity<LauncherView>, Subscription)>,
    /// The open recent sessions overlay.
    recents: Option<(Entity<RecentsView>, Subscription)>,
    /// The open setup wizard overlay.
    setup: Option<(Entity<SetupView>, Subscription)>,
    /// The shared ssh connection a wizard handed to a host's next interactive connect (closed after it).
    shares: HashMap<HostId, Share>,
    /// Hosts whose daemon was just stopped for an update: how many more times a connect that meets the old daemon
    /// (VersionMismatch) is tried again before the card is shown.
    restart_retries: HashMap<HostId, u8>,
    /// The daemon updates running from the sidebar's pill.
    host_update: HashMap<HostId, HostUpdate>,
    /// The open card of a host's update.
    update_host_card: Option<HostUpdateCard>,
    /// The open SSH install wizard overlay.
    ssh_wizard: Option<(Entity<SshWizard>, Subscription)>,
    /// The Settings page, while its tab is open.
    settings: Option<Entity<SettingsView>>,
    /// Listens to the open Settings page.
    _settings_events: Option<Subscription>,
    /// The leaf that had focus when Settings opened; closing Settings goes back to it.
    before_settings: Option<NodeId>,
    /// The open right-click menu of a tab.
    tab_menu: Option<OpenMenu>,
    /// The open popup of a pane's split button (Claude or Terminal).
    pane_menu: Option<OpenMenu>,
    /// A pane being renamed in place on its header, with its field; losing focus cancels it.
    header_rename: Option<(NodeId, TextField, Subscription)>,
    header_rename_focus: FocusHandle,
    /// The binary this window started from (resolved at start, before an install can replace it).
    start_path: Option<PathBuf>,
    /// The version found at `start_path` after a change; `None` until one was probed.
    installed: Option<Version>,
    /// The daemon's version, if it sent one.
    daemon_version: Option<Version>,
    /// Watches `start_path`'s directory (Linux).
    update_watch: Option<(RecommendedWatcher, Task<()>)>,
    /// The open update card; its focus handle takes Esc.
    update_card: Option<FocusHandle>,
    /// Why Restart and update failed, shown in the card.
    update_error: Option<String>,
    /// Whether `checkForUpdates` is on.
    check_updates: bool,
    /// A manual check found releases while the daily check is off: the notice still shows.
    manual_found: bool,
    /// The published releases the last good check found, newest first.
    releases: Vec<Release>,
    /// The `ETag` of that answer.
    release_etag: Option<String>,
    /// The open release notes card; its focus handle takes Esc.
    release_card: Option<FocusHandle>,
    /// The daily check's timer.
    release_task: Option<Task<()>>,
    /// What is known about the git repos on screen.
    git: GitRepos,
    /// The git panel is open (at the right of the tab's panes).
    git_panel_open: bool,
    /// The panel sliding in or out.
    panel_anim: Option<PanelAnim>,
    /// The panel's width when open, in px (0 until the tab content has been measured).
    panel_full: f32,
    /// The panel's tab.
    git_tab: PanelTab,
    /// The git page (a file's diff over the whole tab area), while it is open.
    git_page: Option<GitPage>,
    git_focus: FocusHandle,
    git_scroll: UniformListScrollHandle,
    git_bars: git_scrollbar::Bars,
    /// The timer that asks the daemons for them when due.
    git_task: Option<Task<()>>,
    /// Take-over requests waiting for the host's daemon, by host and session.
    take_overs: HashMap<(HostId, String), TakeOverTarget>,
    /// New sessions waiting for `ContainerFound`, by request id.
    pending_opens: HashMap<u64, PendingOpen>,
    /// The welcome screen's folder rows; dropped when the session lists, the settings or a tab change what they say.
    welcome_items: RefCell<Option<Vec<app_core::launcher::LauncherItem>>>,
    /// Status changes seen so far, to tell which ones alert.
    attention: AttentionTracker,
    /// Leaves with an alert the user has not seen yet.
    unseen: HashMap<NodeId, AlertKind>,
    /// Leaves marked unread by hand: they keep the mark, even in view, until Claude's status changes or the user marks
    /// them read.
    marked_unread: HashSet<NodeId>,
    _activation: Subscription,
    /// The theme whose terminal colors the daemon was last told.
    sent_palette: Option<Preset>,
    _theme: Subscription,
    _appearance: Subscription,
    /// Leaves whose notification was clicked.
    clicked: async_channel::Sender<NodeId>,
    _clicks: Task<()>,
}

/// Which finished leaves resume by themselves on start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resume {
    /// None: they wait for Continue.
    Nothing,
    /// Every finished leaf (`window.resumeSessions`).
    Finished,
    /// Only these (`--resume-sessions`: what was running when Restart and update stopped everything).
    Only(HashSet<NodeId>),
}

impl WorkspaceView {
    /// The connection to `host`, if it is connected.
    fn client(&self, host: &HostId) -> Option<&DaemonClient> {
        self.hosts.get(host).filter(|conn| conn.connected).map(|conn| &conn.client)
    }

    /// This machine's daemon (always connected: the window needs it to start).
    fn local_client(&self) -> &DaemonClient {
        &self.hosts[&HostId::local()].client
    }

    /// Route `host`'s events to the view, in batches, until the connection ends.
    fn listen(
        host: HostId,
        events: async_channel::Receiver<ClientEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(first) = events.recv().await {
                let batch = batch_events(first, &events, MAX_BATCH_BYTES);
                if this.update_in(cx, |view, window, cx| view.on_events(&host, batch, window, cx)).is_err() {
                    return;
                }
                // Yield so keystrokes (e.g. Ctrl+C) are handled between batches.
                cx.background_executor().timer(Duration::ZERO).await;
            }
            let _ = this
                .update_in(cx, |view, window, cx| view.on_events(&host, vec![ClientEvent::Disconnected], window, cx));
        })
    }

    /// Show `restored`, attaching and spawning what it says, and start routing the events of `connection` and of the
    /// `extra` hosts.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        connection: Connection,
        extra: Vec<(HostId, Connection)>,
        restored: Restored,
        layout_file: PathBuf,
        default_cwd: PathBuf,
        resume: Resume,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let Connection { client, events, claude_dir: daemon_dir, daemon_version, daemon_commit, .. } = connection;
        cx.set_global(DaemonBuild { version: daemon_version.clone(), commit: daemon_commit });
        let mut hosts = HashMap::new();
        hosts.insert(
            HostId::local(),
            HostConn {
                client,
                connected: true,
                daemon_version: None,
                _events: Self::listen(HostId::local(), events, window, cx),
            },
        );
        let seam_hosts: HashSet<HostId> = extra.iter().map(|(host, _)| host.clone()).collect();
        for (host, connection) in extra {
            let conn = HostConn {
                client: connection.client,
                connected: true,
                daemon_version: None,
                _events: Self::listen(host.clone(), connection.events, window, cx),
            };
            hosts.insert(host, conn);
        }
        let sidebar = cx.new(SidebarView::new);
        let sidebar_events = cx.subscribe_in(&sidebar, window, |this, _, event: &SidebarEvent, window, cx| {
            this.on_sidebar_event(event.clone(), window, cx)
        });
        // The title bar shows what the sidebar's rows say, so it redraws with them.
        cx.observe(&sidebar, |_, _, cx| cx.notify()).detach();
        // Coming back to the window counts as seeing its visible tab.
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.git.refresh_now();
            }
            this.clear_seen(window, cx);
            this.sidebar.update(cx, |_, cx| cx.notify());
        });
        let theme_changed = cx.observe_global_in::<Theme>(window, |this, _, cx| this.apply_settings(cx));
        // Following the system: a light/dark switch resolves the theme again.
        let appearance = cx.observe_window_appearance(window, |_, _, cx| {
            if cx.global::<SettingsStore>().settings().theme == ThemeChoice::System {
                SettingsStore::reapply(cx);
            }
        });
        let (clicked, clicks) = async_channel::unbounded::<NodeId>();
        let clicks_task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(leaf) = clicks.recv().await {
                let _ = this.update_in(cx, |this, window, cx| {
                    window.activate_window();
                    this.focus_leaf(leaf, window, cx);
                });
            }
        });
        let (askpass_prompts, askpass_receiver) = async_channel::unbounded::<AskPrompt>();
        let askpass_task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(prompt) = askpass_receiver.recv().await {
                if this.update_in(cx, |view, window, cx| view.ask(prompt, window, cx)).is_err() {
                    return;
                }
            }
        });
        let host_states: HashMap<HostId, HostState> =
            hosts.keys().filter(|host| !host.is_local()).map(|host| (host.clone(), HostState::Connected)).collect();
        let mut this = WorkspaceView {
            workspace: restored.workspace,
            to_resume: Vec::new(),
            local_dirs_indexed: false,
            slots: HashMap::new(),
            pending: HashMap::new(),
            next_request: 0,
            search_answers: HashMap::new(),
            search_request: 0,
            search_query: String::new(),
            hosts,
            ssh_hosts: cx.global::<SettingsStore>().settings().ssh_hosts.clone(),
            host_states,
            queued: HashMap::new(),
            retries: HashMap::new(),
            attempts: HashMap::new(),
            next_attempt: 0,
            killed_offline: Vec::new(),
            seam_hosts,
            askpass_prompts,
            askpass: None,
            askpass_queue: VecDeque::new(),
            _askpass_task: askpass_task,
            layout_file,
            default_cwd,
            dirs: ClaudeDirs::from_settings(
                cx.global::<SettingsStore>().settings(),
                daemon_dir.as_deref(),
                std::env::home_dir().as_deref(),
            ),
            daemon_dir,
            sent_dirs: None,
            focus_handle: cx.focus_handle(),
            prompting: Rc::new(Cell::new(false)),
            dragging: None,
            content_bounds: Bounds::default(),
            pane_anim: None,
            pane_drop: None,
            sidebar,
            sidebar_bounds: Bounds::default(),
            resizing_sidebar: false,
            _sidebar_events: sidebar_events,
            launcher: None,
            recents: None,
            setup: None,
            shares: HashMap::new(),
            restart_retries: HashMap::new(),
            host_update: HashMap::new(),
            update_host_card: None,
            ssh_wizard: None,
            settings: None,
            _settings_events: None,
            before_settings: None,
            tab_menu: None,
            pane_menu: None,
            header_rename: None,
            header_rename_focus: cx.focus_handle(),
            start_path: None,
            installed: None,
            daemon_version: None,
            update_watch: None,
            update_card: None,
            update_error: None,
            check_updates: cx.global::<SettingsStore>().settings().check_for_updates,
            manual_found: false,
            releases: Vec::new(),
            release_etag: None,
            release_card: None,
            release_task: None,
            git: GitRepos::default(),
            git_panel_open: false,
            panel_anim: None,
            panel_full: 0.0,
            git_tab: PanelTab::Changes,
            git_page: None,
            git_focus: cx.focus_handle(),
            git_scroll: UniformListScrollHandle::new(),
            git_bars: git_scrollbar::Bars::new(),
            git_task: None,
            take_overs: HashMap::new(),
            pending_opens: HashMap::new(),
            welcome_items: RefCell::new(None),
            attention: AttentionTracker::default(),
            unseen: HashMap::new(),
            marked_unread: HashSet::new(),
            _activation: activation,
            sent_palette: None,
            _theme: theme_changed,
            _appearance: appearance,
            clicked,
            _clicks: clicks_task,
        };
        this.start_path = std::env::current_exe().ok();
        this.daemon_version = daemon_version.as_deref().and_then(Version::parse);
        #[cfg(target_os = "linux")]
        if let Some(path) = this.start_path.clone() {
            this.update_watch = update_watch::watch(&path, cx);
        }
        cx.set_global(CheckRow::default());
        this.release_task = Some(this.start_release_timer(cx));
        this.git_task = Some(Self::start_git_timer(window, cx));
        let leaves: Vec<NodeId> = this.workspace.leaves.keys().copied().collect();
        for leaf in leaves {
            this.add_slot(leaf, window, cx);
        }
        for (leaf, key) in &restored.attach {
            let launch = this.workspace.leaves[leaf].launch.clone();
            this.slots[leaf].view.update(cx, |view, cx| view.attach(key.pane, launch, cx));
            this.hosts[&key.host].client.attach(key.pane);
        }
        for (leaf, slot) in &this.slots {
            if this.workspace.leaves[leaf].pane.is_none() && !restored.spawn.contains(leaf) {
                slot.view.update(cx, |view, cx| view.exited(None, cx));
            }
        }
        // The session list decides how each one resumes, so they wait for it.
        let finished = this.workspace.finished_leaves().into_iter().filter(|leaf| !restored.spawn.contains(leaf));
        match resume {
            Resume::Nothing => {}
            Resume::Finished => this.to_resume = finished.collect(),
            Resume::Only(only) => this.to_resume = finished.filter(|leaf| only.contains(leaf)).collect(),
        }
        for leaf in restored.spawn {
            this.spawn_leaf(leaf, None, window, cx);
        }
        // Restored leaves of SSH hosts wait for the user: their panes say "Not connected".
        let remote: HashSet<HostId> =
            this.workspace.leaves.values().filter(|leaf| !leaf.host.is_local()).map(|leaf| leaf.host.clone()).collect();
        for host in remote {
            this.host_states.entry(host.clone()).or_insert(HostState::NotConnected);
            this.refresh_host_cards(&host, cx);
        }
        this.focus_current(window, cx);
        // Every connection starts with the GUI's colors: the daemon outlives the GUI.
        this.apply_settings(cx);
        this.save(cx);
        let store = cx.global::<SettingsStore>();
        if opens_by_itself(store.settings(), store.broken()) {
            this.show_setup(SetupMode::FirstRun, window, cx);
        }
        this
    }
}
