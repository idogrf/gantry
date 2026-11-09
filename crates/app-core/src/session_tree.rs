//! The sidebar's Active list as plain rows: every pane of the layout and every Claude session running outside
//! Gantry, under the project it belongs to. Closed sessions live in the recent sessions pop-up (`recents`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use protocol::host::{HostId, PaneKey, project_key, split_project_key};
use protocol::message::{SessionInfo, SessionState, SessionStatus};

use crate::fuzzy::Fuzzy;
use crate::gantry_project;
use crate::sidebar_order::SidebarOrder;
use crate::split_tree::NodeId;
use crate::theme_colors::Tint;

/// How a session's row is colored: what it needs from the user, most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tone {
    /// Waiting for an answer.
    NeedsInput,
    /// Busy.
    Working,
    /// Idle with a result the user has not seen.
    Done,
    /// Idle, marked unread by hand.
    Unread,
    /// Idle (or not reporting yet), nothing new.
    Idle,
    /// Running outside Gantry.
    Elsewhere,
    /// Not running.
    Closed,
}

/// A pane of the layout, for the list (Claude panes and shells).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneEntry {
    /// Its leaf.
    pub leaf: NodeId,
    /// The host whose daemon runs it.
    pub host: HostId,
    /// Directory it was started in.
    pub cwd: PathBuf,
    /// Pane title.
    pub title: String,
    /// Claude session it runs (or last ran); `None` for other programs.
    pub session: Option<String>,
    /// It runs Claude Code.
    pub claude: bool,
    /// Its process ended; the pane shows Continue / Restart.
    pub ended: bool,
    /// Its host is not connected (an SSH host that is connecting, lost or never connected): the Closed look.
    pub offline: bool,
    /// Its tab has no other pane.
    pub alone: bool,
    /// Host folder of the dev container it runs in.
    pub container: Option<PathBuf>,
    /// The color the user gave the pane.
    pub color: Option<Tint>,
    /// The Gantry project the user put the pane in (display name).
    pub project: Option<String>,
}

/// What keeps an Active entry's place in the list: its leaf, or (outside Gantry) its session.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EntryKey {
    /// A pane of the layout (survives `/clear`, Continue and Resume in the same leaf).
    Leaf(NodeId),
    /// A session running outside Gantry.
    Session(String),
}

/// Arrival order of the listed entries: rows inside a project never move, and new ones go on top.
#[derive(Debug, Default)]
pub struct Arrivals {
    seq: HashMap<EntryKey, u64>,
    next: u64,
}

impl Arrivals {
    /// Number the keys not seen before, in the given order, and forget keys no longer listed.
    pub fn update(&mut self, keys: &[EntryKey]) {
        let listed: HashSet<&EntryKey> = keys.iter().collect();
        self.seq.retain(|key, _| listed.contains(key));
        for key in keys {
            if !self.seq.contains_key(key) {
                self.next += 1;
                self.seq.insert(key.clone(), self.next);
            }
        }
    }

    /// When `key` arrived (higher = later; 0 = unknown).
    pub fn of(&self, key: &EntryKey) -> u64 {
        self.seq.get(key).copied().unwrap_or(0)
    }
}

/// The sidebar's project order: the saved one if there is one, else activity order at first; then stable; new
/// projects go on top; the user can drag one.
#[derive(Debug, Default)]
pub struct ProjectOrder {
    order: Vec<PathBuf>,
    started: bool,
    /// Projects from the saved order: kept even while not listed (an SSH host that is away).
    kept: HashSet<PathBuf>,
}

impl ProjectOrder {
    /// Start from the saved order `saved` (top to bottom), which also keeps projects that are not listed for now.
    pub fn seed(&mut self, saved: &[PathBuf]) {
        self.order = saved.to_vec();
        self.kept = saved.iter().cloned().collect();
        self.started = true;
    }

    /// The order to save: every project in its place, but only those in `shown`, those of SSH hosts (which may only
    /// be away) and Gantry projects (which may be empty); a local project that is gone for good is forgotten.
    pub fn to_save(&self, shown: &[PathBuf]) -> Vec<PathBuf> {
        let kept = |path: &PathBuf| path.to_string_lossy().starts_with("ssh://") || gantry_project::is_key(path);
        self.order.iter().filter(|path| shown.contains(path) || kept(path)).cloned().collect()
    }

    /// The projects in order, top to bottom (including kept ones that are not listed).
    pub fn projects(&self) -> &[PathBuf] {
        &self.order
    }

    /// Fold in the projects listed now, latest activity first. The first call without a filter takes that order. Later calls put
    /// unknown projects on top (in the given order) and forget projects no longer listed. While `filtering`, only
    /// add, never forget.
    pub fn update(&mut self, by_activity: &[PathBuf], filtering: bool) {
        if !self.started {
            if filtering {
                return;
            }
            self.order = by_activity.to_vec();
            self.started = true;
            return;
        }
        if !filtering {
            self.order.retain(|known| by_activity.contains(known) || self.kept.contains(known));
        }
        let new: Vec<PathBuf> = by_activity.iter().filter(|path| !self.order.contains(path)).cloned().collect();
        self.order.splice(0..0, new);
    }

    /// Move `root` before `before` (`None` = to the end).
    pub fn move_project(&mut self, root: &Path, before: Option<&Path>) {
        let Some(from) = self.order.iter().position(|path| path == root) else { return };
        let moved = self.order.remove(from);
        let to =
            before.and_then(|before| self.order.iter().position(|path| path == before)).unwrap_or(self.order.len());
        self.order.insert(to, moved);
    }

    /// A project's key changed (a Gantry project was renamed): it keeps its place.
    pub fn rename(&mut self, from: &Path, to: &Path) {
        for known in self.order.iter_mut().filter(|known| known.as_path() == from) {
            *known = to.to_path_buf();
        }
        if self.kept.remove(from) {
            self.kept.insert(to.to_path_buf());
        }
    }

    /// Forget a project for good (a Gantry project was deleted), kept or not.
    pub fn forget(&mut self, root: &Path) {
        self.order.retain(|known| known.as_path() != root);
        self.kept.remove(root);
    }

    /// Where `root` sits (unknown = after every known project).
    pub fn position(&self, root: &Path) -> usize {
        self.order.iter().position(|path| path == root).unwrap_or(usize::MAX)
    }
}

/// One tab of the layout, for grouping split tabs in the Active list.
#[derive(Clone, Debug, PartialEq)]
pub struct TabEntry {
    /// Index in the workspace's tabs.
    pub index: usize,
    /// The name the user gave it; without one the heading shows its project.
    pub name: Option<String>,
    /// The leaf with focus in it.
    pub focused: NodeId,
    /// Its leaves in screen order (left/top first: `Node::leaves()`).
    pub leaves: Vec<NodeId>,
}

/// Everything the Active list is built from.
pub struct ActiveInput<'a> {
    /// Claude sessions.
    pub sessions: &'a [&'a SessionInfo],
    /// Panes of the layout.
    pub panes: &'a [PaneEntry],
    /// Home directory, shown as `~`.
    pub home: Option<&'a Path>,
    /// Fuzzy filter text (blank = everything).
    pub filter: &'a str,
    /// Projects the user collapsed.
    pub toggled: &'a HashSet<PathBuf>,
    /// External session whose details are open.
    pub details: Option<&'a str>,
    /// Sessions with an alert the user has not seen.
    pub unseen: &'a HashSet<String>,
    /// Sessions the user marked unread by hand (also in `unseen`).
    pub marked: &'a HashSet<String>,
    /// Tabs of the layout; those with several panes are listed as a heading with their panes under it.
    pub tabs: &'a [TabEntry],
    /// The project a directory belongs to (see [`project_root`]).
    pub project_of: &'a dyn Fn(&HostId, &Path) -> PathBuf,
    /// The Gantry project (display name) each session was put in, for sessions without a pane of ours.
    pub session_projects: &'a BTreeMap<String, String>,
    /// The Gantry projects that exist: each is listed, even with nothing in it.
    pub gantry_projects: &'a [String],
    /// The order the user gave the tabs inside their projects.
    pub saved_order: &'a SidebarOrder,
    /// Current time, ms since the Unix epoch.
    pub now_ms: u64,
}

/// Project roots of directories, cached (finding one stats parent directories).
#[derive(Debug, Default)]
pub struct ProjectRoots {
    home: Option<PathBuf>,
    cache: std::cell::RefCell<HashMap<PathBuf, PathBuf>>,
}

impl ProjectRoots {
    /// The project of something in the Gantry project `gantry` (its key), else of the folder `cwd` as [`Self::of`].
    pub fn of_member(&self, host: &HostId, cwd: &Path, gantry: Option<&str>) -> PathBuf {
        match gantry {
            Some(name) => gantry_project::key(name),
            None => self.of(host, cwd),
        }
    }

    /// Roots below `home`.
    pub fn new(home: Option<PathBuf>) -> ProjectRoots {
        ProjectRoots { home, cache: Default::default() }
    }

    /// The project `cwd` belongs to on `host`: its git repository, else the directory itself. Another machine's disk
    /// cannot be searched, so a remote project is the directory itself.
    pub fn of(&self, host: &HostId, cwd: &Path) -> PathBuf {
        if !host.is_local() {
            return project_key(host, cwd);
        }
        if let Some(root) = self.cache.borrow().get(cwd) {
            return root.clone();
        }
        let root = project_root(cwd, self.home.as_deref(), |dir| dir.join(".git").exists());
        self.cache.borrow_mut().insert(cwd.to_path_buf(), root.clone());
        root
    }

    /// Forget every root, so repositories created or removed since are noticed.
    pub fn clear(&mut self) {
        self.cache.get_mut().clear();
    }
}

/// One sidebar row.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// What it shows.
    pub kind: RowKind,
    /// Listed under a tab heading (drawn indented).
    pub in_tab: bool,
    /// The row's own project, when it is listed under another project's tab heading.
    pub project_label: Option<String>,
    /// The row's own project (a heading's path for project and tab headings); `None` for details and take-over.
    pub project: Option<PathBuf>,
}

/// Whether a row opens or closes its project's group (for drawing the group border in segments).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupEdges {
    /// The row is the project heading (the group's top).
    pub first: bool,
    /// The row is the group's last row.
    pub last: bool,
}

/// One item a project lists that the user can move: a pane of its own, or a split tab with its panes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemBlock {
    /// Index of its first row (the pane's row, or the split tab's heading).
    pub row: usize,
    /// Index of its last row.
    pub last_row: usize,
    /// Its panes' leaves, in screen order.
    pub leaves: Vec<NodeId>,
    /// A split tab.
    pub group: bool,
}

/// The movable items of one project, top to bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectItems {
    /// The project's key.
    pub path: PathBuf,
    /// Its items.
    pub blocks: Vec<ItemBlock>,
}

/// What is picked up to reorder: a pane's own row, or a split tab's heading (by the first leaf of its panes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragSource {
    /// A pane that is a tab of its own.
    Pane(NodeId),
    /// A split tab.
    Tab(NodeId),
}

/// Where a dragged item is dropped: before an item of a project (by index in [`ProjectItems::blocks`]), or after its
/// last item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropAt {
    /// Before block `block` of project `project`.
    Before {
        /// Index in the list of projects.
        project: usize,
        /// Index in the project's blocks.
        block: usize,
    },
    /// After the last block of project `project`.
    End {
        /// Index in the list of projects.
        project: usize,
    },
}

/// The projects of `rows` with their movable items. Outside sessions have no leaf and are not items.
pub fn project_items(rows: &[Row]) -> Vec<ProjectItems> {
    let mut projects: Vec<ProjectItems> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let leaf = match &row.kind {
            RowKind::Session { leaf, .. } => *leaf,
            RowKind::Pane { entry, .. } => Some(entry.leaf),
            _ => None,
        };
        match &row.kind {
            RowKind::Project { path, .. } => projects.push(ProjectItems { path: path.clone(), blocks: Vec::new() }),
            RowKind::Tab { .. } => {
                if let Some(project) = projects.last_mut() {
                    project.blocks.push(ItemBlock { row: index, last_row: index, leaves: Vec::new(), group: true });
                }
            }
            _ => {
                let (Some(leaf), Some(project)) = (leaf, projects.last_mut()) else { continue };
                if row.in_tab {
                    if let Some(block) = project.blocks.last_mut() {
                        block.leaves.push(leaf);
                        block.last_row = index;
                    }
                } else {
                    project.blocks.push(ItemBlock { row: index, last_row: index, leaves: vec![leaf], group: false });
                }
            }
        }
    }
    projects
}

/// The new order of the items of the project `source` is dropped in (its index and its blocks of leaves), or `None`
/// when the drop is no move: another project, a pane under a split heading, an unknown item, or no change.
pub fn reorder(projects: &[ProjectItems], source: DragSource, at: DropAt) -> Option<(usize, Vec<Vec<NodeId>>)> {
    let (project, before) = match at {
        DropAt::Before { project, block } => (project, Some(block)),
        DropAt::End { project } => (project, None),
    };
    let items = projects.get(project)?;
    let from = items.blocks.iter().position(|block| match source {
        DragSource::Pane(leaf) => !block.group && block.leaves == [leaf],
        DragSource::Tab(first) => block.group && block.leaves.first() == Some(&first),
    })?;
    let last = items.blocks.len() - 1;
    match before {
        Some(block) if block > last || block == from || block == from + 1 => return None,
        None if from == last => return None,
        _ => {}
    }
    let mut blocks: Vec<Vec<NodeId>> = items.blocks.iter().map(|block| block.leaves.clone()).collect();
    crate::sidebar_order::move_block(&mut blocks, from, before);
    Some((project, blocks))
}

/// The projects and the items' leaves as listed, top to bottom, for [`SidebarOrder::record`]; `moved` replaces the
/// items of one project (by index) with their new order.
pub fn shown_order(projects: &[ProjectItems], moved: Option<(usize, &[Vec<NodeId>])>) -> (Vec<PathBuf>, Vec<NodeId>) {
    let paths = projects.iter().map(|project| project.path.clone()).collect();
    let mut items = Vec::new();
    for (index, project) in projects.iter().enumerate() {
        match moved {
            Some((at, blocks)) if at == index => items.extend(blocks.iter().flatten().copied()),
            _ => items.extend(project.blocks.iter().flat_map(|block| block.leaves.iter().copied())),
        }
    }
    (paths, items)
}

/// The leaves a drag of `source` carries, when it names an item of `projects`.
fn moved_leaves(projects: &[ProjectItems], source: DragSource) -> Option<Vec<NodeId>> {
    projects.iter().flat_map(|items| items.blocks.iter()).find_map(|block| match source {
        DragSource::Pane(leaf) => block.leaves.contains(&leaf).then(|| vec![leaf]),
        DragSource::Tab(first) => (block.group && block.leaves.first() == Some(&first)).then(|| block.leaves.clone()),
    })
}

/// The items of the project `at` names after `source` is dropped there (its index and its blocks of leaves): the
/// source's leaves leave their old block (a pane of a split tab becomes a block of its own) and go in at `at`, which
/// counts the blocks as listed before the move. `None` for an unknown item or project.
pub fn place_in(projects: &[ProjectItems], source: DragSource, at: DropAt) -> Option<(usize, Vec<Vec<NodeId>>)> {
    let (project, before) = match at {
        DropAt::Before { project, block } => (project, Some(block)),
        DropAt::End { project } => (project, None),
    };
    let moved = moved_leaves(projects, source)?;
    let mut blocks: Vec<Vec<NodeId>> = projects.get(project)?.blocks.iter().map(|block| block.leaves.clone()).collect();
    let mut position = before.unwrap_or(blocks.len()).min(blocks.len());
    let from = blocks.iter().position(|block| block.iter().any(|leaf| moved.contains(leaf)));
    if let Some(from) = from {
        match source {
            DragSource::Tab(_) => {
                blocks.remove(from);
                if from < position {
                    position -= 1;
                }
            }
            DragSource::Pane(leaf) => {
                blocks[from].retain(|other| *other != leaf);
                if blocks[from].is_empty() {
                    blocks.remove(from);
                    if from < position {
                        position -= 1;
                    }
                }
            }
        }
    }
    blocks.insert(position, moved);
    Some((project, blocks))
}

/// What [`shown_order`] lists after `source` is dropped at `at`: the source's leaves are taken out of every project
/// and listed in the target project at `at`. `None` when [`place_in`] is.
pub fn placed_order(projects: &[ProjectItems], source: DragSource, at: DropAt) -> Option<(Vec<PathBuf>, Vec<NodeId>)> {
    let (target, blocks) = place_in(projects, source, at)?;
    let moved = moved_leaves(projects, source)?;
    let paths = projects.iter().map(|project| project.path.clone()).collect();
    let mut leaves = Vec::new();
    for (index, project) in projects.iter().enumerate() {
        if index == target {
            leaves.extend(blocks.iter().flatten().copied());
        } else {
            leaves.extend(
                project
                    .blocks
                    .iter()
                    .flat_map(|block| block.leaves.iter().copied())
                    .filter(|leaf| !moved.contains(leaf)),
            );
        }
    }
    Some((paths, leaves))
}

/// What the sidebar shows while an item is dragged over a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropCue {
    /// Nothing: the drop is not for this target (it may still pull a pane out of its split tab).
    Neutral,
    /// The drop would do nothing: the target's box in the danger color, no line.
    Deny,
    /// The item lands at this place: the target's box and a line there.
    Land(DropAt),
}

/// The cue for dropping `source` at `at`. `current` is the project the item is listed in now and `real` the folder
/// project its pane belongs to without a Gantry project; `pullable` says a drop elsewhere pulls the pane out of its
/// split tab.
pub fn drop_cue(
    projects: &[ProjectItems],
    source: DragSource,
    current: &Path,
    real: &Path,
    pullable: bool,
    at: DropAt,
) -> DropCue {
    let (DropAt::Before { project, .. } | DropAt::End { project }) = at;
    let Some(target) = projects.get(project).map(|items| items.path.as_path()) else { return DropCue::Neutral };
    let nothing = if pullable { DropCue::Neutral } else { DropCue::Deny };
    if current == target {
        return if reorder(projects, source, at).is_some() { DropCue::Land(at) } else { nothing };
    }
    if gantry_project::is_key(target) {
        return DropCue::Land(at);
    }
    if gantry_project::is_key(current) {
        return if target == real { DropCue::Land(at) } else { DropCue::Neutral };
    }
    nothing
}

/// Where a drop at `fraction` (0 = top, 1 = bottom) of row `row`, listed in project `project`, lands: before the block
/// the row is in (its first half of rows) or before the next one (its second half); on the heading, before the first
/// block. `End` when no block follows.
pub fn hover_at(projects: &[ProjectItems], project: usize, row: usize, fraction: f32) -> Option<DropAt> {
    let blocks = &projects.get(project)?.blocks;
    let inside = blocks.iter().position(|block| (block.row..=block.last_row).contains(&row));
    let block = match inside {
        None => 0,
        Some(index) => {
            let item = &blocks[index];
            let rows = (item.last_row - item.row + 1) as f32;
            if ((row - item.row) as f32 + fraction.clamp(0., 1.)) / rows < 0.5 { index } else { index + 1 }
        }
    };
    Some(if block < blocks.len() { DropAt::Before { project, block } } else { DropAt::End { project } })
}

/// The group edges of `rows[index]`.
pub fn group_edges(rows: &[Row], index: usize) -> GroupEdges {
    let heading = |row: &Row| matches!(row.kind, RowKind::Project { .. });
    GroupEdges { first: heading(&rows[index]), last: rows.get(index + 1).is_none_or(heading) }
}

/// The edges of the split tab `rows[index]` belongs to, for drawing its box in segments: the tab's heading row (`first`)
/// and the panes listed under it (`last` on the final one). `None` for every other row.
pub fn tab_group_edges(rows: &[Row], index: usize) -> Option<GroupEdges> {
    let heading = matches!(rows[index].kind, RowKind::Tab { .. });
    if !heading && !rows[index].in_tab {
        return None;
    }
    Some(GroupEdges { first: heading, last: rows.get(index + 1).is_none_or(|next| !next.in_tab) })
}

/// What a row shows.
#[derive(Clone, Debug, PartialEq)]
pub enum RowKind {
    /// A project heading.
    Project {
        /// Project directory.
        path: PathBuf,
        /// Displayed name.
        label: String,
        /// Its entries are shown.
        expanded: bool,
        /// The most urgent tone among its sessions (`None` = nothing needs attention), whether or not it is expanded.
        urgent: Option<Tone>,
        /// A session in it is working, whether or not it is expanded and whatever `urgent` says.
        working: bool,
    },
    /// A Claude session (in a pane of the layout, or running outside Gantry).
    Session {
        /// The session.
        session: SessionInfo,
        /// Its color.
        tone: Tone,
        /// Right-hand note: what it is doing and since when.
        note: String,
        /// The leaf showing it, if any.
        leaf: Option<NodeId>,
    },
    /// A pane the session list does not know: a shell, or a Claude pane before its first transcript.
    Pane {
        /// The pane.
        entry: PaneEntry,
        /// Its color.
        tone: Tone,
        /// Right-hand note.
        note: String,
    },
    /// A tab with more than one pane; its panes follow it (with `in_tab`).
    Tab {
        /// Index in the workspace's tabs.
        index: usize,
        /// Its title, as the title bar shows it.
        title: String,
        /// How many panes it has.
        panes: usize,
        /// The leaf with focus in it.
        focused: NodeId,
        /// Its first leaf (top-left).
        first: NodeId,
    },
    /// The line under an expanded Gantry project heading that has nothing in it.
    Empty,
    /// Details of an external session.
    Detail {
        /// Session id.
        session: String,
        /// Text.
        text: String,
    },
    /// "Take over…" under an outside session's details.
    TakeOver {
        /// Session id.
        session: String,
        /// Its directory (where it is resumed).
        cwd: PathBuf,
        /// The host it runs on.
        host: HostId,
    },
}

/// Identifies a row across rebuilds (for keeping the selection).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowKey {
    /// Project heading.
    Project(PathBuf),
    /// Session.
    Session(String),
    /// A pane the session list does not know.
    Leaf(NodeId),
    /// A tab heading, by the tab's first leaf.
    Tab(NodeId),
    /// The "empty project" line under a Gantry project's heading, by the project's key.
    Empty(PathBuf),
    /// Details of a session.
    Detail(String),
    /// "Take over…" of a session.
    TakeOver(String),
}

/// What activating (click / Enter) a row does.
#[derive(Clone, Debug, PartialEq)]
pub enum Activation {
    /// Show the pane running this session.
    FocusPane(PaneKey),
    /// Show this leaf.
    FocusLeaf(NodeId),
    /// Resume a closed session.
    Resume {
        /// Session id.
        session: String,
        /// Its directory.
        cwd: PathBuf,
        /// The host it ran on.
        host: HostId,
    },
    /// Show or hide an external session's details.
    ToggleDetails(String),
    /// Stop the outside process running a session and resume it here.
    TakeOver {
        /// Session id.
        session: String,
        /// Its directory.
        cwd: PathBuf,
        /// The host it runs on.
        host: HostId,
    },
    /// Expand or collapse a project.
    ToggleProject(PathBuf),
    /// Nothing (headings; a closed session whose directory is gone).
    Nothing,
}

impl Row {
    /// A row that is not under a tab heading.
    pub fn new(kind: RowKind) -> Row {
        Row { kind, in_tab: false, project_label: None, project: None }
    }

    /// Identity of this row.
    pub fn key(&self) -> RowKey {
        match &self.kind {
            RowKind::Project { path, .. } => RowKey::Project(path.clone()),
            RowKind::Session { session, .. } => RowKey::Session(session.id.clone()),
            RowKind::Pane { entry, .. } => RowKey::Leaf(entry.leaf),
            RowKind::Tab { first, .. } => RowKey::Tab(*first),
            RowKind::Empty => RowKey::Empty(self.project.clone().unwrap_or_default()),
            RowKind::Detail { session, .. } => RowKey::Detail(session.clone()),
            RowKind::TakeOver { session, .. } => RowKey::TakeOver(session.clone()),
        }
    }

    /// Whether keyboard selection can land here (details are skipped).
    pub fn selectable(&self) -> bool {
        !matches!(self.kind, RowKind::Detail { .. } | RowKind::Empty)
    }

    /// What activating this row does.
    pub fn activation(&self) -> Activation {
        match &self.kind {
            RowKind::Project { path, .. } => Activation::ToggleProject(path.clone()),
            RowKind::Pane { entry, .. } => Activation::FocusLeaf(entry.leaf),
            RowKind::Tab { focused, .. } => Activation::FocusLeaf(*focused),
            RowKind::Session { session, leaf, .. } => match (session.state, leaf) {
                (SessionState::External { .. }, _) => Activation::ToggleDetails(session.id.clone()),
                (_, Some(leaf)) => Activation::FocusLeaf(*leaf),
                (SessionState::Managed { pane }, None) => {
                    Activation::FocusPane(PaneKey::new(session.host.clone(), pane))
                }
                (SessionState::Closed, None) if session.cwd_exists => Activation::Resume {
                    session: session.id.clone(),
                    cwd: session.cwd.clone(),
                    host: session.host.clone(),
                },
                (SessionState::Closed, None) => Activation::Nothing,
            },
            RowKind::TakeOver { session, cwd, host } => {
                Activation::TakeOver { session: session.clone(), cwd: cwd.clone(), host: host.clone() }
            }
            RowKind::Detail { .. } | RowKind::Empty => Activation::Nothing,
        }
    }
}

/// Whether a session's result is unseen, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unseen {
    /// Nothing new.
    No,
    /// An alert the user has not seen.
    Alert,
    /// Marked unread by hand.
    Marked,
}

/// Tone of a session in a Gantry pane from its status and whether its result is unseen.
pub fn status_tone(status: Option<&SessionStatus>, unseen: Unseen) -> Tone {
    match (status, unseen) {
        (Some(SessionStatus::NeedsInput { .. }), _) => Tone::NeedsInput,
        (Some(SessionStatus::Busy), _) => Tone::Working,
        (_, Unseen::Alert) => Tone::Done,
        (_, Unseen::Marked) => Tone::Unread,
        (_, Unseen::No) => Tone::Idle,
    }
}

/// Tone of a leaf's title-bar chip or pane header: Closed while its host is not connected, else from its status
/// (`None` = no status to color it by).
pub fn leaf_tone(status: Option<&SessionStatus>, unseen: Unseen, offline: bool) -> Option<Tone> {
    if offline { Some(Tone::Closed) } else { status.map(|status| status_tone(Some(status), unseen)) }
}

/// The tone a project's dot shows: the most urgent of `tones` (needs input, then done, working, unread), `None`
/// when none of them needs a color.
pub fn most_urgent(tones: impl IntoIterator<Item = Tone>) -> Option<Tone> {
    let rank = |tone: Tone| match tone {
        Tone::NeedsInput => Some(4),
        Tone::Done => Some(3),
        Tone::Working => Some(2),
        Tone::Unread => Some(1),
        Tone::Idle | Tone::Elsewhere | Tone::Closed => None,
    };
    tones
        .into_iter()
        .filter_map(|tone| rank(tone).map(|rank| (rank, tone)))
        .max_by_key(|(rank, _)| *rank)
        .map(|(_, tone)| tone)
}

/// Tone of any session.
pub fn tone(session: &SessionInfo, unseen: Unseen) -> Tone {
    match session.state {
        SessionState::Managed { .. } => status_tone(session.status.as_ref(), unseen),
        SessionState::External { .. } => Tone::Elsewhere,
        SessionState::Closed => Tone::Closed,
    }
}

/// The project `cwd` belongs to: its nearest enclosing repository (`is_repo`), else `cwd` itself. Home and its
/// ancestors never count as an enclosing repository, so a dotfiles repository does not swallow everything under `~`.
pub fn project_root(cwd: &Path, home: Option<&Path>, is_repo: impl Fn(&Path) -> bool) -> PathBuf {
    cwd.ancestors()
        .take_while(|dir| *dir == cwd || home.is_none_or(|home| !home.starts_with(dir)))
        .find(|dir| is_repo(dir))
        .unwrap_or(cwd)
        .to_path_buf()
}

/// A session's display title ("New session" until it has one).
pub fn session_title(session: &SessionInfo) -> &str {
    if session.title.is_empty() { "New session" } else { &session.title }
}

/// `path` for display, with the home directory as `~`.
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
}

/// Compact time since `then_ms`: `now`, `5m`, `3h`, `2d`, `6w`.
pub fn age(now_ms: u64, then_ms: u64) -> String {
    let minutes = now_ms.saturating_sub(then_ms) / 60_000;
    let (hour, day) = (60, 60 * 24);
    match minutes {
        0 => "now".into(),
        m if m < hour => format!("{m}m"),
        m if m < day => format!("{}h", m / hour),
        m if m < 14 * day => format!("{}d", m / day),
        m => format!("{}w", m / (7 * day)),
    }
}

/// Index of the row identified by `key`, if it is still there.
pub fn find(rows: &[Row], key: &RowKey) -> Option<usize> {
    rows.iter().position(|row| &row.key() == key)
}

/// What one Active row stands for, before grouping.
struct Entry<'a> {
    key: EntryKey,
    host: &'a HostId,
    cwd: &'a Path,
    /// The Gantry project it was put in; it wins over the folder.
    gantry: Option<&'a str>,
    title: &'a str,
    /// `last_active_ms`; panes the session list does not know count as active now.
    activity: u64,
    kind: EntryKind<'a>,
}

enum EntryKind<'a> {
    Session { session: &'a SessionInfo, leaf: Option<&'a PaneEntry> },
    Pane(&'a PaneEntry),
}

/// The Active list.
pub fn active<'a>(input: &'a ActiveInput<'a>, arrivals: &mut Arrivals, order: &mut ProjectOrder) -> Vec<Row> {
    let by_id: HashMap<(&HostId, &str), &SessionInfo> =
        input.sessions.iter().map(|s| ((&s.host, s.id.as_str()), *s)).collect();
    let member = |pane: &'a PaneEntry, session: Option<&str>| member_project(input, pane, session);
    let mut entries: Vec<Entry<'_>> = Vec::new();
    let mut in_leaves: HashSet<(&HostId, &str)> = HashSet::new();
    for pane in input.panes {
        match pane.session.as_deref().and_then(|id| by_id.get(&(&pane.host, id)).copied()) {
            Some(session) => {
                in_leaves.insert((&session.host, session.id.as_str()));
                let title = session_title(session);
                let kind = EntryKind::Session { session, leaf: Some(pane) };
                entries.push(Entry {
                    key: EntryKey::Leaf(pane.leaf),
                    host: &session.host,
                    cwd: &session.cwd,
                    gantry: member(pane, Some(&session.id)),
                    title,
                    activity: session.last_active_ms,
                    kind,
                });
            }
            None => {
                let title = if pane.claude { "New session" } else { pane.title.as_str() };
                entries.push(Entry {
                    key: EntryKey::Leaf(pane.leaf),
                    host: &pane.host,
                    cwd: &pane.cwd,
                    gantry: member(pane, pane.session.as_deref()),
                    title,
                    activity: u64::MAX,
                    kind: EntryKind::Pane(pane),
                });
            }
        }
    }
    for session in input.sessions.iter().copied() {
        if session.state == SessionState::Closed || in_leaves.contains(&(&session.host, session.id.as_str())) {
            continue;
        }
        let kind = EntryKind::Session { session, leaf: None };
        entries.push(Entry {
            key: EntryKey::Session(session.id.clone()),
            host: &session.host,
            cwd: &session.cwd,
            gantry: None,
            title: session_title(session),
            activity: session.last_active_ms,
            kind,
        });
    }
    // New keys are numbered oldest first, so that at startup newer leaves sit above older ones and leaves above
    // outside sessions (which go by activity).
    let mut keys: Vec<(&EntryKey, (bool, u64))> = entries
        .iter()
        .map(|entry| match &entry.key {
            EntryKey::Leaf(leaf) => (&entry.key, (true, *leaf)),
            EntryKey::Session(_) => (&entry.key, (false, entry.activity)),
        })
        .collect();
    keys.sort_by_key(|(_, order)| *order);
    arrivals.update(&keys.into_iter().map(|(key, _)| key.clone()).collect::<Vec<_>>());

    // A tab with several panes is one item, placed in the project of its first leaf that has an entry.
    let project_of_entry = |entry: &Entry<'_>| match entry.gantry {
        Some(name) => gantry_project::key(name),
        None => (input.project_of)(entry.host, entry.cwd),
    };
    let leaf_project: HashMap<NodeId, PathBuf> =
        entries.iter().filter_map(|entry| Some((leaf_of(&entry.key)?, project_of_entry(entry)))).collect();
    let leaf_titles: HashMap<NodeId, &str> =
        entries.iter().filter_map(|entry| Some((leaf_of(&entry.key)?, entry.title))).collect();
    let mut groups: Vec<Group<'_>> = Vec::new();
    let mut group_of: HashMap<NodeId, usize> = HashMap::new();
    for tab in input.tabs.iter().filter(|tab| tab.leaves.len() >= 2) {
        let Some(project) = tab.leaves.iter().find_map(|leaf| leaf_project.get(leaf).cloned()) else { continue };
        let place = tab
            .leaves
            .iter()
            .filter(|leaf| leaf_project.contains_key(leaf))
            .map(|leaf| arrivals.of(&EntryKey::Leaf(*leaf)))
            .min()
            .unwrap_or(0);
        for leaf in &tab.leaves {
            group_of.insert(*leaf, groups.len());
        }
        let first_title = tab.leaves.iter().find_map(|leaf| leaf_titles.get(leaf).copied());
        groups.push(Group { tab, project, place, first_title, members: Vec::new() });
    }

    let mut fuzzy = Fuzzy::new(input.filter, entries.iter().map(|entry| keep_text(entry.title, entry.cwd, input.home)));
    let filtering = fuzzy.is_some();
    let mut by_project: HashMap<PathBuf, Vec<Entry<'_>>> = HashMap::new();
    for entry in entries {
        if !keep(&mut fuzzy, entry.title, entry.cwd, input.home) {
            continue;
        }
        match leaf_of(&entry.key).and_then(|leaf| group_of.get(&leaf)) {
            Some(&group) => groups[group].members.push(entry),
            None => by_project.entry(project_of_entry(&entry)).or_default().push(entry),
        }
    }
    let mut items_of: HashMap<PathBuf, Vec<Item<'_>>> = HashMap::new();
    for (path, entries) in by_project {
        items_of.insert(path, entries.into_iter().map(Item::Single).collect());
    }
    for mut group in groups.into_iter().filter(|group| !group.members.is_empty()) {
        let position = |entry: &Entry<'_>| group.tab.leaves.iter().position(|leaf| Some(*leaf) == leaf_of(&entry.key));
        group.members.sort_by_key(position);
        items_of.entry(group.project.clone()).or_default().push(Item::Group(group));
    }
    if !filtering {
        for name in input.gantry_projects {
            items_of.entry(gantry_project::key(name)).or_default();
        }
    }
    let mut projects: Vec<(PathBuf, Vec<Item<'_>>)> = items_of.into_iter().collect();
    // Tabs the user placed come below the new ones (newest first), in their saved order; outside sessions have no
    // saved place.
    for (_, items) in &mut projects {
        items.sort_by_key(|item| {
            let (rank, arrival) = match item {
                Item::Single(entry) => {
                    (leaf_of(&entry.key).and_then(|leaf| input.saved_order.rank_of(&[leaf])), arrivals.of(&entry.key))
                }
                Item::Group(group) => (input.saved_order.rank_of(&group.tab.leaves), group.place),
            };
            (rank.is_some(), rank.unwrap_or(0), std::cmp::Reverse(arrival))
        });
    }
    // Sessions decide; a project with only shells or new leaves counts as now.
    let latest = |items: &[Item<'_>]| {
        let sessions =
            items.iter().flat_map(Item::entries).filter(|entry| matches!(entry.kind, EntryKind::Session { .. }));
        sessions.map(|entry| entry.activity).max().unwrap_or(u64::MAX)
    };
    projects.sort_by(|(a_path, a), (b_path, b)| latest(b).cmp(&latest(a)).then_with(|| a_path.cmp(b_path)));
    order.update(&projects.iter().map(|(path, _)| path.clone()).collect::<Vec<_>>(), filtering);
    projects.sort_by_key(|(path, _)| order.position(path));
    // Rows of a group that belong to another project are labelled with it, so name those projects too.
    let mut paths: Vec<PathBuf> = projects.iter().map(|(path, _)| path.clone()).collect();
    for (_, items) in &projects {
        for entry in items.iter().flat_map(Item::entries) {
            let own = project_of_entry(entry);
            if !paths.contains(&own) {
                paths.push(own);
            }
        }
    }
    let labels = labels(&paths, input.home, input.gantry_projects);

    let mut rows = Vec::new();
    for (path, items) in projects {
        let mut rendered: Vec<Row> = Vec::new();
        for item in &items {
            match item {
                Item::Single(entry) => {
                    let mut row = entry_row(entry, input);
                    row.project = Some(project_of_entry(entry));
                    rendered.push(row);
                }
                Item::Group(group) => {
                    let tab = group.tab;
                    let mut heading = Row::new(RowKind::Tab {
                        index: tab.index,
                        title: tab
                            .name
                            .clone()
                            .or_else(|| group.first_title.map(String::from))
                            .unwrap_or_else(|| labels[&path].clone()),
                        panes: tab.leaves.len(),
                        focused: tab.focused,
                        first: tab.leaves[0],
                    });
                    heading.project = Some(path.clone());
                    rendered.push(heading);
                    for entry in &group.members {
                        let mut row = entry_row(entry, input);
                        row.in_tab = true;
                        let own = project_of_entry(entry);
                        row.project_label = (own != path).then(|| labels[&own].clone());
                        row.project = Some(own);
                        rendered.push(row);
                    }
                }
            }
        }
        let expanded = filtering || !input.toggled.contains(&path);
        let tones: Vec<Tone> = rendered
            .iter()
            .filter_map(|row| match &row.kind {
                RowKind::Session { tone, .. } | RowKind::Pane { tone, .. } => Some(*tone),
                _ => None,
            })
            .collect();
        let urgent = most_urgent(tones.iter().copied());
        let working = tones.contains(&Tone::Working);
        let mut heading =
            Row::new(RowKind::Project { label: labels[&path].clone(), path: path.clone(), expanded, urgent, working });
        let empty = rendered.is_empty() && gantry_project::is_key(&path);
        heading.project = Some(path.clone());
        rows.push(heading);
        if expanded && empty {
            let mut line = Row::new(RowKind::Empty);
            line.project = Some(path);
            rows.push(line);
        } else if expanded {
            for row in rendered {
                let details = match &row.kind {
                    RowKind::Session { session, .. } if input.details == Some(session.id.as_str()) => {
                        Some(session.clone())
                    }
                    _ => None,
                };
                rows.push(row);
                if let Some(session) = details {
                    push_details(&mut rows, &session);
                }
            }
        }
    }
    rows
}

/// The Gantry project a pane was put in: its own label wins; its session's label counts only while its project
/// exists (a deleted one lives on in Recents only). A session running outside Gantry is never in a project.
fn member_project<'a>(input: &'a ActiveInput<'a>, pane: &'a PaneEntry, session: Option<&str>) -> Option<&'a str> {
    pane.project.as_deref().or_else(|| {
        let name = input.session_projects.get(session?)?;
        let exists = input.gantry_projects.iter().any(|known| gantry_project::same(known, name));
        exists.then_some(name.as_str())
    })
}

/// A split tab and its kept entries, to be listed together.
struct Group<'a> {
    tab: &'a TabEntry,
    /// The project it is listed under.
    project: PathBuf,
    /// Where it sorts: the oldest arrival among its leaves.
    place: u64,
    /// The title of its first pane, which names the tab when the user gave it no name.
    first_title: Option<&'a str>,
    /// Entries that passed the filter, in screen order.
    members: Vec<Entry<'a>>,
}

/// What a project lists: an entry, or a split tab with its entries.
enum Item<'a> {
    Single(Entry<'a>),
    Group(Group<'a>),
}

impl<'a> Item<'a> {
    fn entries(&self) -> &[Entry<'a>] {
        match self {
            Item::Single(entry) => std::slice::from_ref(entry),
            Item::Group(group) => &group.members,
        }
    }
}

fn leaf_of(key: &EntryKey) -> Option<NodeId> {
    match key {
        EntryKey::Leaf(leaf) => Some(*leaf),
        EntryKey::Session(_) => None,
    }
}

/// The row of one entry: its tone and its note.
fn entry_row(entry: &Entry<'_>, input: &ActiveInput<'_>) -> Row {
    match &entry.kind {
        EntryKind::Session { session, leaf } => {
            let age = age(input.now_ms, session.last_active_ms);
            let (tone, note) = match (session.state, leaf) {
                (SessionState::Closed, Some(pane)) if pane.ended => (Tone::Closed, "Exited · tab open".to_string()),
                (_, Some(pane)) if pane.offline => (Tone::Closed, "Not connected".to_string()),
                (SessionState::Closed, _) => (Tone::Idle, "Starting".to_string()),
                _ => {
                    let unseen = match (input.unseen.contains(&session.id), input.marked.contains(&session.id)) {
                        (_, true) => Unseen::Marked,
                        (true, false) => Unseen::Alert,
                        (false, false) => Unseen::No,
                    };
                    let tone = tone(session, unseen);
                    (tone, format!("{} · {age}", tone_word(tone)))
                }
            };
            Row::new(RowKind::Session { session: (*session).clone(), tone, note, leaf: leaf.map(|pane| pane.leaf) })
        }
        EntryKind::Pane(pane) => {
            let (tone, note) = match (pane.claude, pane.ended) {
                (_, false) if pane.offline => (Tone::Closed, "Not connected"),
                (true, true) => (Tone::Closed, "Exited · tab open"),
                (true, false) => (Tone::Idle, "Idle"),
                (false, true) => (Tone::Closed, "Exited"),
                (false, false) => (Tone::Idle, "Terminal"),
            };
            Row::new(RowKind::Pane { entry: (*pane).clone(), tone, note: note.into() })
        }
    }
}

/// The word for a live tone in a row's note.
pub fn tone_word(tone: Tone) -> &'static str {
    match tone {
        Tone::NeedsInput => "Needs input",
        Tone::Working => "Working",
        Tone::Done => "Done",
        Tone::Unread => "Unread",
        Tone::Idle => "Idle",
        Tone::Elsewhere => "Elsewhere",
        Tone::Closed => "Closed",
    }
}

/// An external session's details and, for host sessions whose folder exists, "Take over…".
fn push_details(rows: &mut Vec<Row>, session: &SessionInfo) {
    if let Some(text) = detail_text(session) {
        rows.push(Row::new(RowKind::Detail { session: session.id.clone(), text }));
        if matches!(session.state, SessionState::External { pid: Some(_) }) && session.cwd_exists {
            rows.push(Row::new(RowKind::TakeOver {
                session: session.id.clone(),
                cwd: session.cwd.clone(),
                host: session.host.clone(),
            }));
        }
    }
}

/// Index of the row showing `leaf` (its session's row, or its pane's).
pub fn row_of_leaf(rows: &[Row], leaf: NodeId) -> Option<usize> {
    rows.iter().position(|row| match &row.kind {
        RowKind::Session { leaf: Some(shown), .. } => *shown == leaf,
        RowKind::Pane { entry, .. } => entry.leaf == leaf,
        _ => false,
    })
}

/// The leaf of a row: a pane's, a session's in a pane, or a split tab heading's first leaf.
fn row_leaf(row: &Row) -> Option<NodeId> {
    match &row.kind {
        RowKind::Session { leaf, .. } => *leaf,
        RowKind::Pane { entry, .. } => Some(entry.leaf),
        RowKind::Tab { first, .. } => Some(*first),
        _ => None,
    }
}

/// The leaves of `panes` that share their tab with another pane, so can be pulled out into a tab of their own.
pub fn pullable_leaves(panes: &[PaneEntry]) -> HashSet<NodeId> {
    panes.iter().filter(|pane| !pane.alone).map(|pane| pane.leaf).collect()
}

/// Every leaf listed in `rows`, top to bottom, once each (the tab order of Ctrl+Tab).
pub fn leaves_in_order(rows: &[Row]) -> Vec<NodeId> {
    let mut leaves = Vec::new();
    for leaf in rows.iter().filter_map(row_leaf) {
        if !leaves.contains(&leaf) {
            leaves.push(leaf);
        }
    }
    leaves
}

/// The leaves listed in `all` that `shown` leaves out, top to bottom (those under a collapsed project).
pub fn hidden_leaves(all: &[Row], shown: &[Row]) -> Vec<NodeId> {
    let shown = leaves_in_order(shown);
    leaves_in_order(all).into_iter().filter(|leaf| !shown.contains(leaf)).collect()
}

/// Leaves whose session needs input, top to bottom (sessions outside Gantry have no leaf and are left out).
pub fn needs_input_leaves(rows: &[Row]) -> Vec<NodeId> {
    rows.iter()
        .filter_map(|row| match &row.kind {
            RowKind::Session { tone: Tone::NeedsInput, leaf, .. } => *leaf,
            _ => None,
        })
        .collect()
}

/// The first leaf needing input after `focused` in list order, wrapping around (`focused` itself when it is the only one).
pub fn next_needing_input(rows: &[Row], focused: Option<NodeId>) -> Option<NodeId> {
    let waiting = needs_input_leaves(rows);
    let order = leaves_in_order(rows);
    let at = focused.and_then(|leaf| order.iter().position(|l| *l == leaf));
    let place = |leaf: &NodeId| order.iter().position(|l| l == leaf).unwrap_or(usize::MAX);
    match at {
        Some(at) => waiting.iter().find(|leaf| place(leaf) > at).or(waiting.first()).copied(),
        None => waiting.first().copied(),
    }
}

/// The title of the row showing `leaf`, as the sidebar shows it.
pub fn leaf_title(rows: &[Row], leaf: NodeId) -> Option<String> {
    let row = &rows[row_of_leaf(rows, leaf)?];
    match &row.kind {
        RowKind::Session { session, .. } => Some(session_title(session).to_string()),
        RowKind::Pane { entry, .. } if entry.claude => Some("New session".to_string()),
        RowKind::Pane { entry, .. } => Some(entry.title.clone()),
        _ => None,
    }
}

/// The label of the project heading for `root`.
pub fn project_label(rows: &[Row], root: &Path) -> Option<String> {
    rows.iter().find_map(|row| match &row.kind {
        RowKind::Project { path, label, .. } if path == root => Some(label.clone()),
        _ => None,
    })
}

/// Project names: a Gantry project's name, else the folder's name, or its whole displayed path where two projects share a name. A remote project
/// is `<host> - <name>`.
pub(crate) fn labels(paths: &[PathBuf], home: Option<&Path>, gantry: &[String]) -> HashMap<PathBuf, String> {
    let name = |path: &Path, whole: bool| {
        if let Some(name) = gantry_project::display_name(path, gantry, None) {
            return name;
        }
        let (host, root) = split_project_key(path);
        let home = home.filter(|_| host.is_local());
        let text = match root.file_name() {
            Some(name) if !whole && Some(root.as_path()) != home => name.to_string_lossy().into_owned(),
            _ => display_path(&root, home),
        };
        match host.remote_name() {
            Some(host) => format!("{host} - {text}"),
            None => text,
        }
    };
    let mut uses: HashMap<String, HashSet<&Path>> = HashMap::new();
    for path in paths {
        uses.entry(name(path, false)).or_default().insert(path);
    }
    paths
        .iter()
        .map(|path| {
            let short = name(path, false);
            let label = if uses[&short].len() > 1 { name(path, true) } else { short };
            (path.clone(), label)
        })
        .collect()
}

/// Whether an entry passes the filter (matched against its title and displayed directory).
pub(crate) fn keep(fuzzy: &mut Option<Fuzzy>, title: &str, cwd: &Path, home: Option<&Path>) -> bool {
    fuzzy.as_mut().is_none_or(|fuzzy| fuzzy.score(&keep_text(title, cwd, home)).is_some())
}

/// The text `keep` matches: the title and the displayed directory.
pub(crate) fn keep_text(title: &str, cwd: &Path, home: Option<&Path>) -> String {
    format!("{title} {}", display_path(cwd, home))
}

fn detail_text(session: &SessionInfo) -> Option<String> {
    match session.state {
        SessionState::External { .. } if session.container.is_some() => Some("Running in a dev container".into()),
        SessionState::External { pid: Some(pid) } => Some(format!("Running outside Gantry (pid {pid})")),
        SessionState::External { pid: None } => Some("Running in a container".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::message::{ContainerPlace, SessionStatus};

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    const NOW: u64 = 100 * DAY_MS;
    const HOUR_MS: u64 = 60 * 60 * 1000;

    #[test]
    fn project_dot_shows_the_most_urgent_tone() {
        use Tone::*;
        assert_eq!(most_urgent([Idle, Working, NeedsInput, Done]), Some(NeedsInput));
        assert_eq!(most_urgent([Working, Unread, Done]), Some(Done));
        assert_eq!(most_urgent([Idle, Working, Unread]), Some(Working));
        assert_eq!(most_urgent([Idle, Unread, Closed]), Some(Unread));
        assert_eq!(most_urgent([Closed, Working, Elsewhere]), Some(Working));
        assert_eq!(most_urgent([Idle, Closed, Elsewhere]), None);
        assert_eq!(most_urgent([]), None);
    }

    fn session(id: &str, cwd: &str, state: SessionState, last_active_ms: u64) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            cwd: cwd.into(),
            title: format!("title {id}"),
            last_active_ms,
            state,
            cwd_exists: true,
            status: None,
            status_since_ms: 0,
            container: None,
            claude_dir: None,
            last_prompt: None,
            host: protocol::host::HostId::local(),
        }
    }

    fn managed(id: &str, cwd: &str, last: u64) -> SessionInfo {
        session(id, cwd, SessionState::Managed { pane: 1 }, last)
    }

    fn closed(id: &str, cwd: &str, last: u64) -> SessionInfo {
        session(id, cwd, SessionState::Closed, last)
    }

    fn external(id: &str, cwd: &str, last: u64) -> SessionInfo {
        session(id, cwd, SessionState::External { pid: Some(42) }, last)
    }

    fn with(mut session: SessionInfo, status: SessionStatus) -> SessionInfo {
        session.status = Some(status);
        session
    }

    /// Repositories of the fixtures: every `/r/<name>` directory.
    fn project_of(host: &HostId, cwd: &Path) -> PathBuf {
        let root = project_root(cwd, Some(Path::new("/home/me")), |dir| dir.parent() == Some(Path::new("/r")));
        project_key(host, &root)
    }

    fn pane(leaf: NodeId, cwd: &str, session: Option<&str>) -> PaneEntry {
        PaneEntry {
            leaf,
            host: HostId::local(),
            cwd: cwd.into(),
            title: format!("pane {leaf}"),
            session: session.map(String::from),
            claude: session.is_some(),
            ended: false,
            offline: false,
            alone: true,
            container: None,
            color: None,
            project: None,
        }
    }

    fn shell_pane(leaf: NodeId, cwd: &str) -> PaneEntry {
        PaneEntry { claude: false, ..pane(leaf, cwd, None) }
    }

    #[derive(Default)]
    struct Fixture {
        sessions: Vec<SessionInfo>,
        panes: Vec<PaneEntry>,
        filter: String,
        toggled: HashSet<PathBuf>,
        details: Option<String>,
        unseen: HashSet<String>,
        marked: HashSet<String>,
        tabs: Vec<TabEntry>,
        arrivals: Arrivals,
        order: ProjectOrder,
        saved: crate::sidebar_order::SidebarOrder,
        session_projects: BTreeMap<String, String>,
        gantry_projects: Vec<String>,
    }

    impl Fixture {
        fn of(sessions: Vec<SessionInfo>, panes: Vec<PaneEntry>) -> Fixture {
            Fixture { sessions, panes, ..Fixture::default() }
        }

        fn rows(&mut self) -> Vec<Row> {
            let refs: Vec<&SessionInfo> = self.sessions.iter().collect();
            let input = ActiveInput {
                sessions: &refs,
                panes: &self.panes,
                home: Some(Path::new("/home/me")),
                filter: &self.filter,
                toggled: &self.toggled,
                details: self.details.as_deref(),
                unseen: &self.unseen,
                marked: &self.marked,
                tabs: &self.tabs,
                project_of: &project_of,
                session_projects: &self.session_projects,
                gantry_projects: &self.gantry_projects,
                saved_order: &self.saved,
                now_ms: NOW,
            };
            active(&input, &mut self.arrivals, &mut self.order)
        }

        fn outline(&mut self) -> Vec<String> {
            self.rows()
                .iter()
                .map(|row| match &row.kind {
                    RowKind::Project { label, expanded, .. } => {
                        format!("{} {label}", if *expanded { "▾" } else { "▸" })
                    }
                    RowKind::Session { session, note, .. } => format!("  {} ({note})", session.id),
                    RowKind::Pane { entry, note, .. } => format!("  leaf {} ({note})", entry.leaf),
                    RowKind::Empty => "    (empty)".into(),
                    RowKind::Detail { text, .. } => format!("    ({text})"),
                    RowKind::TakeOver { .. } => "    Take over…".into(),
                    RowKind::Tab { index, title, panes, .. } => format!("  tab {} {title} ({panes} panes)", index + 1),
                })
                .collect()
        }
    }

    #[test]
    fn a_project_is_working_whenever_one_of_its_sessions_is_whatever_its_dot_shows() {
        let mut fixture = Fixture::of(
            vec![
                with(managed("w", "/r/app", NOW - 60_000), SessionStatus::Busy),
                with(session("d", "/r/app", SessionState::Managed { pane: 2 }, NOW - 90_000), SessionStatus::Idle),
                with(session("i", "/r/web", SessionState::Managed { pane: 3 }, NOW - 90_000), SessionStatus::Idle),
            ],
            vec![pane(1, "/r/app", Some("w")), pane(2, "/r/app", Some("d")), pane(3, "/r/web", Some("i"))],
        );
        fixture.unseen.insert("d".into());
        let projects: Vec<(String, Option<Tone>, bool)> = fixture
            .rows()
            .into_iter()
            .filter_map(|row| match row.kind {
                RowKind::Project { label, urgent, working, .. } => Some((label, urgent, working)),
                _ => None,
            })
            .collect();
        assert!(projects.contains(&("app".into(), Some(Tone::Done), true)));
        assert!(projects.contains(&("web".into(), None, false)));
    }

    #[test]
    fn active_lists_panes_and_outside_sessions_by_project_and_nothing_closed() {
        let mut ended = pane(4, "/r/app", Some("gone"));
        ended.ended = true;
        let mut fixture = Fixture::of(
            vec![
                with(session("m", "/r/app", SessionState::Managed { pane: 1 }, NOW - 60_000), SessionStatus::Busy),
                closed("gone", "/r/app", NOW - HOUR_MS),
                closed("history", "/r/app", NOW - 10),
                external("e", "/r/web", NOW - 2 * HOUR_MS),
            ],
            vec![pane(1, "/r/app", Some("m")), pane(2, "/r/app", Some("fresh")), shell_pane(3, "/r/app/src"), ended],
        );
        assert_eq!(
            fixture.outline(),
            vec![
                "▾ app",
                "  gone (Exited · tab open)",
                "  leaf 3 (Terminal)",
                "  leaf 2 (Idle)",
                "  m (Working · 1m)",
                "▾ web",
                "  e (Elsewhere · 2h)",
            ]
        );
    }

    #[test]
    fn a_leaf_of_a_host_that_is_not_connected_has_the_closed_look_whatever_its_last_status() {
        let mut busy = pane(4, "/r/app", Some("m"));
        busy.offline = true;
        let mut shell = shell_pane(5, "/r/app");
        shell.offline = true;
        let mut fixture =
            Fixture::of(vec![with(managed("m", "/r/app", NOW - HOUR_MS), SessionStatus::Busy)], vec![busy, shell]);
        let rows = fixture.rows();
        let tones: Vec<(Tone, &str)> = rows
            .iter()
            .filter_map(|row| match &row.kind {
                RowKind::Session { tone, note, .. } | RowKind::Pane { tone, note, .. } => Some((*tone, note.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(tones, vec![(Tone::Closed, "Not connected"), (Tone::Closed, "Not connected")]);
        assert_eq!(leaf_tone(Some(&SessionStatus::Busy), Unseen::No, true), Some(Tone::Closed));
        assert_eq!(leaf_tone(Some(&SessionStatus::Busy), Unseen::No, false), Some(Tone::Working));
        assert_eq!(leaf_tone(None, Unseen::No, false), None);
    }

    #[test]
    fn an_exited_leaf_whose_session_is_known_shows_as_that_session() {
        let mut ended = pane(4, "/r/app", Some("gone"));
        ended.ended = true;
        let mut fixture = Fixture::of(vec![closed("gone", "/r/app", NOW - HOUR_MS)], vec![ended]);
        let rows = fixture.rows();
        let RowKind::Session { tone, note, leaf, .. } = &rows[1].kind else { panic!("{rows:?}") };
        assert_eq!((*tone, note.as_str(), *leaf), (Tone::Closed, "Exited · tab open", Some(4)));
        assert_eq!(rows[1].activation(), Activation::FocusLeaf(4));
        // While Continue restarts it, it is starting, not exited.
        fixture.panes[0].ended = false;
        let rows = fixture.rows();
        let RowKind::Session { tone, note, .. } = &rows[1].kind else { panic!("{rows:?}") };
        assert_eq!((*tone, note.as_str()), (Tone::Idle, "Starting"));
    }

    #[test]
    fn a_session_running_in_a_leaf_is_listed_once() {
        // Just taken over: the daemon reports it Managed, and a leaf names it.
        let mut fixture = Fixture::of(vec![managed("t", "/r/app", NOW - 1)], vec![pane(7, "/r/app", Some("t"))]);
        assert_eq!(fixture.outline(), vec!["▾ app", "  t (Idle · now)"]);
        let rows = fixture.rows();
        assert_eq!(rows[1].activation(), Activation::FocusLeaf(7));
    }

    #[test]
    fn rows_inside_a_project_keep_their_place_and_new_ones_land_on_top() {
        let mut fixture = Fixture::of(
            vec![managed("a", "/r/app", NOW - 3 * HOUR_MS), managed("b", "/r/app", NOW - HOUR_MS)],
            vec![pane(1, "/r/app", Some("a")), pane(2, "/r/app", Some("b"))],
        );
        assert_eq!(fixture.outline(), vec!["▾ app", "  b (Idle · 1h)", "  a (Idle · 3h)"]);
        // "a" becomes busy and more recent: it does not move.
        fixture.sessions[0] = with(managed("a", "/r/app", NOW), SessionStatus::Busy);
        assert_eq!(fixture.outline(), vec!["▾ app", "  b (Idle · 1h)", "  a (Working · now)"]);
        // A new outside session arrives: it goes on top, whatever its activity.
        fixture.sessions.push(external("x", "/r/app", NOW - 9 * HOUR_MS));
        assert_eq!(fixture.outline()[1], "  x (Elsewhere · 9h)");
    }

    #[test]
    fn a_leaf_keeps_its_place_when_its_session_changes() {
        let mut fixture = Fixture::of(
            vec![managed("a", "/r/app", 1), managed("b", "/r/app", 2)],
            vec![pane(1, "/r/app", Some("a")), pane(2, "/r/app", Some("b"))],
        );
        let before = fixture.outline();
        // `/clear` in leaf 1: a new session id, same leaf.
        fixture.sessions[0] = managed("a2", "/r/app", 3);
        fixture.panes[0].session = Some("a2".into());
        let after = fixture.outline();
        assert_eq!(after.iter().position(|r| r.starts_with("  a2")), before.iter().position(|r| r.starts_with("  a ")));
    }

    #[test]
    fn filtering_does_not_reset_the_order() {
        let mut fixture = Fixture::of(
            vec![managed("a", "/r/app", NOW - 1), managed("b", "/r/app", NOW - 2)],
            vec![pane(1, "/r/app", Some("a")), pane(2, "/r/app", Some("b"))],
        );
        let before = fixture.outline();
        fixture.filter = "title b".into();
        assert_eq!(fixture.outline(), vec!["▾ app", "  b (Idle · now)"]);
        fixture.filter.clear();
        assert_eq!(fixture.outline(), before);
    }

    #[test]
    fn at_startup_newer_leaves_come_first_then_outside_sessions_newest_first() {
        let mut fixture = Fixture::of(
            vec![
                managed("one", "/r/app", 5),
                managed("two", "/r/app", 4),
                external("x", "/r/app", 3),
                external("y", "/r/app", 9),
            ],
            vec![pane(1, "/r/app", Some("one")), pane(2, "/r/app", Some("two"))],
        );
        let ids: Vec<String> =
            fixture.outline().into_iter().skip(1).map(|r| r.split_whitespace().next().unwrap().to_string()).collect();
        assert_eq!(ids, ["two", "one", "y", "x"]);
    }

    #[test]
    fn projects_start_in_activity_order_then_stay() {
        let mut fixture = Fixture::of(
            vec![managed("old", "/r/a", NOW - 5 * HOUR_MS), managed("new", "/r/b", NOW - HOUR_MS)],
            vec![pane(1, "/r/a", Some("old")), pane(2, "/r/b", Some("new"))],
        );
        assert_eq!(fixture.outline(), vec!["▾ b", "  new (Idle · 1h)", "▾ a", "  old (Idle · 5h)"]);
        fixture.sessions[0].last_active_ms = NOW;
        assert_eq!(fixture.outline()[0], "▾ b", "later activity does not move a project");
    }

    #[test]
    fn a_shell_does_not_pin_its_project_above_newer_sessions() {
        let mut fixture = Fixture::of(
            vec![managed("old", "/r/a", NOW - 5 * HOUR_MS), managed("new", "/r/b", NOW - HOUR_MS)],
            vec![pane(1, "/r/a", Some("old")), shell_pane(2, "/r/a"), pane(3, "/r/b", Some("new"))],
        );
        assert_eq!(fixture.outline()[0], "▾ b");
        // A project with nothing but shells counts as now.
        fixture.panes.push(shell_pane(4, "/r/c"));
        assert_eq!(fixture.outline()[0], "▾ c");
    }

    #[test]
    fn a_collapsed_project_hides_its_rows_until_filtering() {
        let mut fixture = Fixture::of(vec![], vec![shell_pane(1, "/r/p"), shell_pane(2, "/r/q")]);
        fixture.toggled.insert("/r/p".into());
        assert_eq!(fixture.outline(), vec!["▸ p", "▾ q", "  leaf 2 (Terminal)"]);
        fixture.filter = "pane 1".into();
        assert_eq!(fixture.outline(), vec!["▾ p", "  leaf 1 (Terminal)"]);
    }

    #[test]
    fn the_leaves_under_a_collapsed_project_are_hidden() {
        let mut fixture = Fixture::of(vec![], vec![shell_pane(1, "/r/p"), shell_pane(2, "/r/q")]);
        let all = fixture.rows();
        assert!(hidden_leaves(&all, &all).is_empty());
        fixture.toggled.insert("/r/p".into());
        assert_eq!(hidden_leaves(&all, &fixture.rows()), [1]);
    }

    #[test]
    fn an_exited_shell_stays_listed() {
        let mut shell = shell_pane(5, "/r/p");
        shell.ended = true;
        let mut fixture = Fixture::of(vec![], vec![shell]);
        assert_eq!(fixture.outline(), vec!["▾ p", "  leaf 5 (Exited)"]);
    }

    #[test]
    fn the_row_of_a_leaf_is_found_for_sessions_and_panes() {
        let mut fixture =
            Fixture::of(vec![managed("m", "/r/p", 1)], vec![pane(1, "/r/p", Some("m")), shell_pane(2, "/r/p")]);
        let rows = fixture.rows();
        assert!(
            matches!(&rows[row_of_leaf(&rows, 1).unwrap()].kind, RowKind::Session { session, .. } if session.id == "m")
        );
        assert!(matches!(&rows[row_of_leaf(&rows, 2).unwrap()].kind, RowKind::Pane { entry, .. } if entry.leaf == 2));
        assert_eq!(row_of_leaf(&rows, 9), None);
    }

    #[test]
    fn take_over_is_offered_for_outside_host_sessions_whose_directory_exists() {
        let mut fixture = Fixture::of(
            vec![external("e", "/p", 3), SessionInfo { cwd_exists: false, ..external("gone", "/p", 2) }],
            vec![],
        );
        fixture.details = Some("gone".into());
        assert!(!fixture.outline().contains(&"    Take over…".to_string()));
        fixture.details = Some("e".into());
        let rows = fixture.rows();
        let index = find(&rows, &RowKey::TakeOver("e".into())).unwrap();
        assert!(rows[index].selectable());
        assert_eq!(
            rows[index].activation(),
            Activation::TakeOver { session: "e".into(), cwd: "/p".into(), host: HostId::local() }
        );
    }

    #[test]
    fn details_appear_under_their_external_session() {
        let mut boxed = external("box", "/p", 1);
        boxed.container = Some(ContainerPlace { host_folder: Some("/p".into()), path: "/workspaces/p".into() });
        let mut fixture = Fixture::of(vec![external("e", "/p", 2), boxed], vec![]);
        fixture.details = Some("e".into());
        assert_eq!(
            fixture.outline()[2..4],
            ["    (Running outside Gantry (pid 42))".to_string(), "    Take over…".to_string()]
        );
        fixture.details = Some("box".into());
        assert!(fixture.outline().contains(&"    (Running in a dev container)".to_string()));
    }

    #[test]
    fn activation_follows_the_row() {
        let mut fixture = Fixture::of(
            vec![external("e", "/r/p", 2), managed("orphan", "/r/p", 1)],
            vec![shell_pane(9, "/r/p"), pane(8, "/r/p", Some("unknown"))],
        );
        let rows = fixture.rows();
        let by = |key: RowKey| rows[find(&rows, &key).unwrap()].activation();
        assert_eq!(by(RowKey::Leaf(9)), Activation::FocusLeaf(9));
        assert_eq!(by(RowKey::Leaf(8)), Activation::FocusLeaf(8));
        assert_eq!(by(RowKey::Session("e".into())), Activation::ToggleDetails("e".into()));
        assert_eq!(by(RowKey::Session("orphan".into())), Activation::FocusPane(PaneKey::local(1)));
        assert_eq!(by(RowKey::Project("/r/p".into())), Activation::ToggleProject("/r/p".into()));
    }

    // Unix-style paths: Windows joins them with `\`.
    #[cfg(unix)]
    #[test]
    fn projects_with_the_same_name_show_their_paths() {
        let mut fixture = Fixture::of(vec![], vec![shell_pane(1, "/r/api"), shell_pane(2, "/home/me/work/api")]);
        let outline = fixture.outline();
        assert!(
            outline.contains(&"▾ /r/api".to_string()) && outline.contains(&"▾ ~/work/api".to_string()),
            "{outline:?}"
        );
    }

    #[test]
    fn tone_follows_state_status_and_whether_the_result_was_seen() {
        let idle = with(managed("i", "/a", 1), SessionStatus::Idle);
        assert_eq!(tone(&idle, Unseen::No), Tone::Idle);
        assert_eq!(tone(&idle, Unseen::Alert), Tone::Done);
        assert_eq!(tone(&idle, Unseen::Marked), Tone::Unread);
        assert_eq!(tone(&managed("starting", "/a", 1), Unseen::No), Tone::Idle);
        for unseen in [Unseen::Alert, Unseen::Marked] {
            assert_eq!(tone(&with(managed("b", "/a", 1), SessionStatus::Busy), unseen), Tone::Working);
            assert_eq!(
                tone(&with(managed("n", "/a", 1), SessionStatus::NeedsInput { reason: None }), unseen),
                Tone::NeedsInput
            );
            assert_eq!(tone(&closed("c", "/a", 1), unseen), Tone::Closed);
        }
        assert_eq!(
            tone(&with(external("e", "/a", 1), SessionStatus::NeedsInput { reason: None }), Unseen::No),
            Tone::Elsewhere
        );
        assert_eq!(status_tone(None, Unseen::No), Tone::Idle);
    }

    #[test]
    fn a_session_marked_unread_by_hand_has_the_unread_tone_not_done() {
        let mut fx = Fixture::of(
            vec![with(managed("m", "/a", 1), SessionStatus::Idle), with(managed("d", "/a", 2), SessionStatus::Idle)],
            vec![],
        );
        fx.unseen = HashSet::from(["m".to_string(), "d".to_string()]);
        fx.marked = HashSet::from(["m".to_string()]);
        let tones: HashMap<String, Tone> = fx
            .rows()
            .into_iter()
            .filter_map(|row| match row.kind {
                RowKind::Session { session, tone, .. } => Some((session.id, tone)),
                _ => None,
            })
            .collect();
        assert_eq!((tones["m"], tones["d"]), (Tone::Unread, Tone::Done));
    }

    #[test]
    fn a_project_is_the_nearest_repository_below_home() {
        let home = Some(Path::new("/home/me"));
        let repos = [Path::new("/home/me"), Path::new("/home/me/code/app"), Path::new("/srv")];
        let root = |cwd: &str| project_root(Path::new(cwd), home, |dir| repos.contains(&dir));
        assert_eq!(root("/home/me/code/app/crates/core"), Path::new("/home/me/code/app"));
        assert_eq!(root("/home/me/code/app"), Path::new("/home/me/code/app"));
        // A repository at (or above) home does not swallow everything under it.
        assert_eq!(root("/home/me/notes"), Path::new("/home/me/notes"));
        assert_eq!(root("/home/me"), Path::new("/home/me"));
        assert_eq!(root("/srv/www"), Path::new("/srv"));
        assert_eq!(root("/tmp/x"), Path::new("/tmp/x"));
    }

    // Unix-style paths: Windows joins them with `\`.
    #[cfg(unix)]
    #[test]
    fn helpers_format_titles_paths_and_ages() {
        let mut untitled = closed("u", "/", 0);
        untitled.title.clear();
        assert_eq!(session_title(&untitled), "New session");
        let home = Some(Path::new("/home/me"));
        assert_eq!(display_path(Path::new("/home/me"), home), "~");
        assert_eq!(display_path(Path::new("/home/me/x/y"), home), "~/x/y");
        assert_eq!(display_path(Path::new("/home/meow"), home), "/home/meow");
        let minute = 60_000;
        assert_eq!(
            [0, 5 * minute, 3 * 60 * minute, 2 * DAY_MS, 42 * DAY_MS].map(|ago| age(NOW, NOW - ago)),
            ["now", "5m", "3h", "2d", "6w"]
        );
    }

    fn tab(index: usize, focused: NodeId, leaves: &[NodeId]) -> TabEntry {
        TabEntry { index, name: None, focused, leaves: leaves.to_vec() }
    }

    fn titled_shell(leaf: NodeId, cwd: &str, title: &str) -> PaneEntry {
        PaneEntry { title: title.into(), ..shell_pane(leaf, cwd) }
    }

    fn keys(rows: &[Row]) -> Vec<RowKey> {
        rows.iter().map(Row::key).collect()
    }

    fn row_of<'a>(rows: &'a [Row], key: &RowKey) -> &'a Row {
        &rows[find(rows, key).unwrap()]
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn first_update_takes_activity_order_then_new_go_on_top() {
        let mut order = ProjectOrder::default();
        order.update(&paths(&["/b", "/a"]), false);
        assert!(order.position(Path::new("/b")) < order.position(Path::new("/a")));
        order.update(&paths(&["/a", "/c", "/b"]), false);
        let mut sorted = paths(&["/a", "/b", "/c"]);
        sorted.sort_by_key(|p| order.position(p));
        assert_eq!(sorted, paths(&["/c", "/b", "/a"]), "known projects keep their place, the new one is on top");
    }

    #[test]
    fn a_first_update_while_filtering_does_not_start_the_order() {
        let mut order = ProjectOrder::default();
        order.update(&paths(&["/b"]), true);
        order.update(&paths(&["/a", "/b", "/c"]), false);
        let mut sorted = paths(&["/c", "/b", "/a"]);
        sorted.sort_by_key(|p| order.position(p));
        assert_eq!(sorted, paths(&["/a", "/b", "/c"]));
    }

    #[test]
    fn a_project_that_leaves_and_returns_goes_on_top() {
        let mut order = ProjectOrder::default();
        order.update(&paths(&["/a", "/b"]), false);
        order.update(&paths(&["/b"]), false);
        order.update(&paths(&["/b", "/a"]), false);
        assert!(order.position(Path::new("/a")) < order.position(Path::new("/b")));
    }

    #[test]
    fn filtering_does_not_forget_order() {
        let mut order = ProjectOrder::default();
        order.update(&paths(&["/a", "/b", "/c"]), false);
        order.update(&paths(&["/c"]), true);
        order.update(&paths(&["/a", "/b", "/c"]), false);
        let mut sorted = paths(&["/c", "/b", "/a"]);
        sorted.sort_by_key(|p| order.position(p));
        assert_eq!(sorted, paths(&["/a", "/b", "/c"]));
    }

    #[test]
    fn saved_order_decides_item_places_inside_a_project() {
        let mut fixture = Fixture::of(vec![], (1..=3).map(|leaf| shell_pane(leaf, "/r/app")).collect());
        assert_eq!(keys(&fixture.rows())[1..], [RowKey::Leaf(3), RowKey::Leaf(2), RowKey::Leaf(1)]);
        fixture.saved.items = vec![1, 3, 2, 77];
        assert_eq!(keys(&fixture.rows())[1..], [RowKey::Leaf(1), RowKey::Leaf(3), RowKey::Leaf(2)]);
        fixture.panes.push(shell_pane(4, "/r/app"));
        assert_eq!(
            keys(&fixture.rows())[1..],
            [RowKey::Leaf(4), RowKey::Leaf(1), RowKey::Leaf(3), RowKey::Leaf(2)],
            "a tab that is not in the saved order is new and goes on top"
        );
    }

    #[test]
    fn a_split_tab_is_ranked_by_its_best_leaf() {
        let mut fixture = Fixture::of(vec![], (1..=3).map(|leaf| shell_pane(leaf, "/r/app")).collect());
        fixture.tabs = vec![tab(0, 2, &[2, 3])];
        assert_eq!(keys(&fixture.rows())[1], RowKey::Tab(2), "newest first without a saved order");
        fixture.saved.items = vec![1, 3];
        let rows = keys(&fixture.rows());
        assert_eq!(rows[1], RowKey::Leaf(1));
        assert_eq!(rows[2], RowKey::Tab(2));
    }

    /// Project 0 of `rows` as (kind, leaves) per block, for short assertions.
    fn blocks_of(projects: &[ProjectItems], project: usize) -> Vec<(bool, Vec<NodeId>)> {
        projects[project].blocks.iter().map(|block| (block.group, block.leaves.clone())).collect()
    }

    fn app_fixture() -> Fixture {
        // Display order in /r/app: tab [4, 5], then leaf 3, leaf 2, leaf 1; /r/web has leaf 6.
        let mut panes: Vec<PaneEntry> = (1..=5).map(|leaf| shell_pane(leaf, "/r/app")).collect();
        panes.push(shell_pane(6, "/r/web"));
        let mut fixture = Fixture::of(vec![], panes);
        fixture.tabs = vec![tab(0, 4, &[4, 5])];
        fixture
    }

    #[test]
    fn project_items_group_a_split_tab_with_its_panes() {
        let rows = app_fixture().rows();
        let projects = project_items(&rows);
        assert_eq!(projects.iter().map(|p| p.path.clone()).collect::<Vec<_>>(), paths(&["/r/app", "/r/web"]));
        assert_eq!(
            blocks_of(&projects, 0),
            vec![(true, vec![4, 5]), (false, vec![3]), (false, vec![2]), (false, vec![1])]
        );
        assert_eq!(blocks_of(&projects, 1), vec![(false, vec![6])]);
        let tab = &projects[0].blocks[0];
        assert_eq!((tab.row, tab.last_row), (1, 3), "the heading and its two panes");
    }

    #[test]
    fn reorder_moves_a_pane_or_a_split_tab_inside_its_project() {
        let rows = app_fixture().rows();
        let projects = project_items(&rows);
        let before = |block| DropAt::Before { project: 0, block };
        let (project, blocks) = reorder(&projects, DragSource::Pane(1), before(1)).unwrap();
        assert_eq!((project, blocks), (0, vec![vec![4, 5], vec![1], vec![3], vec![2]]));
        let (_, blocks) = reorder(&projects, DragSource::Pane(3), DropAt::End { project: 0 }).unwrap();
        assert_eq!(blocks, vec![vec![4, 5], vec![2], vec![1], vec![3]]);
        let (_, blocks) = reorder(&projects, DragSource::Tab(4), DropAt::End { project: 0 }).unwrap();
        assert_eq!(blocks, vec![vec![3], vec![2], vec![1], vec![4, 5]]);
    }

    #[test]
    fn reorder_refuses_what_is_not_a_move_inside_one_project() {
        let rows = app_fixture().rows();
        let projects = project_items(&rows);
        let at = |block| DropAt::Before { project: 0, block };
        assert_eq!(
            reorder(&projects, DragSource::Pane(4), at(2)),
            None,
            "a pane under a split heading is not a source"
        );
        assert_eq!(reorder(&projects, DragSource::Pane(6), at(1)), None, "another project's tab");
        assert_eq!(reorder(&projects, DragSource::Pane(3), at(1)), None, "before itself");
        assert_eq!(reorder(&projects, DragSource::Pane(3), at(2)), None, "before the one that follows it already");
        assert_eq!(reorder(&projects, DragSource::Pane(1), DropAt::End { project: 0 }), None, "already last");
        assert_eq!(reorder(&projects, DragSource::Pane(3), at(9)), None, "no such place");
        assert_eq!(reorder(&projects, DragSource::Tab(3), at(2)), None, "a single pane is not a split tab");
        assert_eq!(reorder(&projects, DragSource::Pane(99), at(1)), None, "an unknown leaf");
    }

    /// /r/app: tab [4, 5] (rows 1..=3), 3 (row 4), 2 (row 5); gantry://work: 1 (row 7); gantry://empty: nothing.
    fn drop_fixture() -> Vec<ProjectItems> {
        let block =
            |row, last_row, leaves: &[NodeId], group| ItemBlock { row, last_row, leaves: leaves.to_vec(), group };
        vec![
            ProjectItems {
                path: PathBuf::from("/r/app"),
                blocks: vec![block(1, 3, &[4, 5], true), block(4, 4, &[3], false), block(5, 5, &[2], false)],
            },
            ProjectItems { path: gantry_project::key("Work"), blocks: vec![block(7, 7, &[1], false)] },
            ProjectItems { path: gantry_project::key("Empty"), blocks: vec![] },
        ]
    }

    #[test]
    fn place_in_inserts_into_another_project_and_leaves_the_old_one() {
        let projects = drop_fixture();
        let before = |project, block| DropAt::Before { project, block };
        let placed = |source, at| place_in(&projects, source, at).unwrap();
        assert_eq!(placed(DragSource::Pane(3), before(1, 0)), (1, vec![vec![3], vec![1]]));
        assert_eq!(placed(DragSource::Pane(2), DropAt::End { project: 1 }), (1, vec![vec![1], vec![2]]));
        assert_eq!(placed(DragSource::Pane(2), DropAt::End { project: 2 }), (2, vec![vec![2]]));
        assert_eq!(placed(DragSource::Pane(1), before(0, 1)), (0, vec![vec![4, 5], vec![1], vec![3], vec![2]]));
    }

    #[test]
    fn place_in_moves_a_split_tab_whole_and_a_pane_of_it_alone() {
        let projects = drop_fixture();
        assert_eq!(
            place_in(&projects, DragSource::Tab(4), DropAt::Before { project: 1, block: 0 }),
            Some((1, vec![vec![4, 5], vec![1]]))
        );
        assert_eq!(
            place_in(&projects, DragSource::Pane(5), DropAt::End { project: 1 }),
            Some((1, vec![vec![1], vec![5]])),
            "a pane of a split tab becomes a block of its own"
        );
        // Inside one project the blocks are counted before the move.
        assert_eq!(
            place_in(&projects, DragSource::Pane(3), DropAt::End { project: 0 }),
            Some((0, vec![vec![4, 5], vec![2], vec![3]]))
        );
        assert_eq!(
            place_in(&projects, DragSource::Pane(2), DropAt::Before { project: 0, block: 0 }),
            Some((0, vec![vec![2], vec![4, 5], vec![3]]))
        );
    }

    #[test]
    fn place_in_refuses_unknown_items_and_projects() {
        let projects = drop_fixture();
        assert_eq!(place_in(&projects, DragSource::Pane(99), DropAt::End { project: 1 }), None);
        assert_eq!(place_in(&projects, DragSource::Tab(3), DropAt::End { project: 1 }), None, "a pane is no tab");
        assert_eq!(place_in(&projects, DragSource::Pane(3), DropAt::End { project: 9 }), None);
    }

    #[test]
    fn placed_order_lists_the_moved_leaves_once_at_their_new_place() {
        let projects = drop_fixture();
        let (paths, leaves) =
            placed_order(&projects, DragSource::Pane(3), DropAt::Before { project: 1, block: 0 }).unwrap();
        assert_eq!(paths.len(), 3);
        assert_eq!(leaves, vec![4, 5, 2, 3, 1]);
        let (_, leaves) = placed_order(&projects, DragSource::Pane(5), DropAt::End { project: 2 }).unwrap();
        assert_eq!(leaves, vec![4, 3, 2, 1, 5]);
    }

    #[test]
    fn hover_at_picks_the_block_edge_nearest_the_pointer() {
        let projects = drop_fixture();
        let before = |project, block| Some(DropAt::Before { project, block });
        assert_eq!(hover_at(&projects, 0, 0, 0.9), before(0, 0), "the heading");
        assert_eq!(hover_at(&projects, 0, 4, 0.2), before(0, 1), "a single row, upper half");
        assert_eq!(hover_at(&projects, 0, 4, 0.8), before(0, 2), "a single row, lower half");
        assert_eq!(hover_at(&projects, 0, 5, 0.8), Some(DropAt::End { project: 0 }), "the last block, lower half");
        assert_eq!(hover_at(&projects, 0, 1, 0.9), before(0, 0), "a split tab's first row, upper half of the block");
        assert_eq!(hover_at(&projects, 0, 2, 0.5), before(0, 1), "a split tab's middle row, lower half of the block");
        assert_eq!(hover_at(&projects, 0, 3, 0.1), before(0, 1));
        assert_eq!(hover_at(&projects, 2, 9, 0.5), Some(DropAt::End { project: 2 }), "an empty project");
        assert_eq!(hover_at(&projects, 5, 0, 0.5), None);
    }

    #[test]
    fn drop_cue_decides_land_deny_or_neutral() {
        let projects = drop_fixture();
        let app = Path::new("/r/app");
        let web = Path::new("/r/web");
        let work = gantry_project::key("Work");
        let at = |project, block| DropAt::Before { project, block };
        let cue = |source, current: &Path, real: &Path, pullable, at| {
            drop_cue(&projects, source, current, real, pullable, at)
        };
        // (a) the item's own project: a real move lands, no change is denied.
        assert_eq!(cue(DragSource::Pane(2), app, app, false, at(0, 0)), DropCue::Land(at(0, 0)));
        assert_eq!(cue(DragSource::Pane(3), app, app, false, at(0, 1)), DropCue::Deny);
        assert_eq!(cue(DragSource::Pane(5), app, app, true, at(0, 2)), DropCue::Neutral, "pulled out instead");
        // (b) a Gantry project the item is not in.
        assert_eq!(cue(DragSource::Pane(3), app, app, false, at(1, 0)), DropCue::Land(at(1, 0)));
        assert_eq!(
            cue(DragSource::Pane(3), app, app, false, DropAt::End { project: 2 }),
            DropCue::Land(DropAt::End { project: 2 })
        );
        // (c) a folder project: only the item's real one takes it back from a Gantry project.
        assert_eq!(cue(DragSource::Pane(1), &work, app, false, at(0, 1)), DropCue::Land(at(0, 1)));
        assert_eq!(cue(DragSource::Pane(1), &work, web, false, at(0, 1)), DropCue::Neutral);
        // (d) anything else: denied, or neutral when the drop pulls a pane out.
        assert_eq!(cue(DragSource::Pane(6), web, web, false, at(0, 1)), DropCue::Deny);
        assert_eq!(
            cue(DragSource::Pane(5), app, app, true, at(1, 0)),
            DropCue::Land(at(1, 0)),
            "a Gantry project takes it"
        );
        assert_eq!(cue(DragSource::Pane(3), app, app, false, at(7, 0)), DropCue::Neutral);
    }

    #[test]
    fn shown_order_lists_projects_and_tabs_top_to_bottom_with_a_moved_project() {
        let rows = app_fixture().rows();
        let projects = project_items(&rows);
        assert_eq!(shown_order(&projects, None), (paths(&["/r/app", "/r/web"]), vec![4, 5, 3, 2, 1, 6]));
        let moved = vec![vec![1], vec![4, 5]];
        let (shown_projects, items) = shown_order(&projects, Some((0, &moved)));
        assert_eq!(shown_projects, paths(&["/r/app", "/r/web"]));
        assert_eq!(items, vec![1, 4, 5, 6]);
    }

    #[test]
    fn the_order_to_save_keeps_places_of_ssh_projects_that_are_away_and_drops_other_unlisted_ones() {
        let mut order = ProjectOrder::default();
        order.seed(&paths(&["ssh://box/app", "/a", "/gone", "/b"]));
        order.update(&paths(&["/b", "/a"]), false);
        assert_eq!(order.to_save(&paths(&["/a", "/b"])), paths(&["ssh://box/app", "/a", "/b"]));
    }

    #[test]
    fn seeded_project_order_keeps_projects_that_are_not_listed() {
        let mut order = ProjectOrder::default();
        order.seed(&paths(&["/a", "/b", "/c"]));
        order.update(&paths(&["/b", "/a"]), false);
        assert_eq!(order.projects(), paths(&["/a", "/b", "/c"]).as_slice());
        order.update(&paths(&["/b", "/a", "/d"]), false);
        assert_eq!(order.projects(), paths(&["/d", "/a", "/b", "/c"]).as_slice(), "a new project goes on top");
    }

    #[test]
    fn dragging_moves_a_project() {
        let mut order = ProjectOrder::default();
        order.update(&paths(&["/a", "/b", "/c"]), false);
        order.move_project(Path::new("/c"), Some(Path::new("/a")));
        order.move_project(Path::new("/a"), None);
        let mut sorted = paths(&["/a", "/b", "/c"]);
        sorted.sort_by_key(|p| order.position(p));
        assert_eq!(sorted, paths(&["/c", "/b", "/a"]));
    }

    #[test]
    fn active_keeps_project_places_while_activity_changes() {
        let mut fixture =
            Fixture::of(vec![external("a", "/r/alpha", NOW - 5), external("b", "/r/beta", NOW - 10)], vec![]);
        assert_eq!(fixture.outline()[0], "▾ alpha");
        fixture.sessions[1].last_active_ms = NOW;
        assert_eq!(fixture.outline()[0], "▾ alpha", "activity no longer moves projects");
        fixture.sessions.push(external("c", "/r/gamma", NOW - 20));
        assert_eq!(fixture.outline()[0], "▾ gamma", "a new project goes on top");
    }

    #[test]
    fn filtering_does_not_reorder_rows() {
        let mut fixture =
            Fixture::of(vec![external("a", "/r/alpha", NOW - 5), external("b", "/r/beta", NOW - 10)], vec![]);
        fixture.sessions.push(external("c", "/r/gamma", NOW - 20));
        let before = fixture.outline();
        fixture.filter = "gamma".into();
        fixture.rows();
        fixture.filter.clear();
        assert_eq!(fixture.outline(), before);
    }

    #[test]
    fn rows_carry_their_own_project() {
        let mut fixture = Fixture::of(vec![], vec![shell_pane(1, "/r/alpha"), shell_pane(2, "/r/beta")]);
        fixture.tabs = vec![tab(0, 1, &[1, 2])];
        let rows = fixture.rows();
        assert_eq!(rows[0].project.as_deref(), Some(Path::new("/r/alpha")));
        assert_eq!(
            rows[1].project.as_deref(),
            Some(Path::new("/r/alpha")),
            "the tab heading takes the group's project"
        );
        assert_eq!(rows[2].project.as_deref(), Some(Path::new("/r/alpha")));
        assert_eq!(rows[3].project.as_deref(), Some(Path::new("/r/beta")));
    }

    #[test]
    fn group_edges_mark_first_and_last_rows() {
        let mut fixture =
            Fixture::of(vec![], vec![shell_pane(1, "/r/p"), shell_pane(2, "/r/p"), shell_pane(3, "/r/q")]);
        fixture.toggled.insert("/r/q".into());
        let rows = fixture.rows();
        let edges: Vec<(bool, bool)> =
            (0..rows.len()).map(|i| group_edges(&rows, i)).map(|e| (e.first, e.last)).collect();
        assert_eq!(keys(&rows).len(), 4);
        assert_eq!(edges, vec![(true, false), (false, false), (false, true), (true, true)]);
    }

    #[test]
    fn split_tab_groups_panes_in_screen_order() {
        let mut fixture = Fixture::of(vec![], vec![shell_pane(1, "/r/alpha"), shell_pane(2, "/r/alpha")]);
        fixture.tabs = vec![tab(0, 1, &[2, 1])];
        let rows = fixture.rows();
        assert_eq!(
            keys(&rows),
            vec![RowKey::Project("/r/alpha".into()), RowKey::Tab(2), RowKey::Leaf(2), RowKey::Leaf(1)]
        );
        assert!(
            matches!(&rows[1].kind, RowKind::Tab { index: 0, panes: 2, first: 2, focused: 1, .. }),
            "{:?}",
            rows[1]
        );
        assert!(!rows[1].in_tab);
        assert!(rows[2].in_tab && rows[3].in_tab);
    }

    #[test]
    fn a_split_tab_heading_shows_its_name_else_its_first_pane() {
        let mut fixture = Fixture::of(vec![], vec![shell_pane(1, "/r/alpha"), shell_pane(2, "/r/alpha")]);
        fixture.tabs = vec![tab(0, 1, &[2, 1])];
        let heading = |fixture: &mut Fixture| match fixture.rows()[1].kind.clone() {
            RowKind::Tab { title, .. } => title,
            other => panic!("{other:?}"),
        };
        assert_eq!(heading(&mut fixture), "pane 2");
        fixture.tabs[0].name = Some("Test cleanup".into());
        assert_eq!(heading(&mut fixture), "Test cleanup");
    }

    #[test]
    fn one_pane_tab_is_a_plain_row() {
        let mut fixture = Fixture::of(vec![], vec![shell_pane(1, "/r/alpha")]);
        fixture.tabs = vec![tab(0, 1, &[1])];
        let rows = fixture.rows();
        assert_eq!(keys(&rows), vec![RowKey::Project("/r/alpha".into()), RowKey::Leaf(1)]);
        assert!(!rows[1].in_tab);
    }

    #[test]
    fn cross_project_tab_sits_under_first_leaf_project() {
        let mut fixture = Fixture::of(vec![], vec![shell_pane(1, "/r/alpha"), shell_pane(2, "/r/beta")]);
        fixture.tabs = vec![tab(0, 1, &[1, 2])];
        let rows = fixture.rows();
        assert_eq!(
            keys(&rows),
            vec![RowKey::Project("/r/alpha".into()), RowKey::Tab(1), RowKey::Leaf(1), RowKey::Leaf(2)]
        );
        assert_eq!(rows[2].project_label, None);
        assert_eq!(rows[3].project_label.as_deref(), Some("beta"));
    }

    #[test]
    fn only_panes_of_split_tabs_can_be_pulled_out() {
        let alone = shell_pane(1, "/r/alpha");
        let in_a_tab = PaneEntry { alone: false, ..shell_pane(2, "/r/alpha") };
        let pullable = pullable_leaves(&[alone, in_a_tab]);
        assert!(!pullable.contains(&1));
        assert!(pullable.contains(&2));
        assert!(!pullable.contains(&99), "an unknown leaf is not pullable");
    }

    #[test]
    fn a_tab_mixing_two_hosts_lists_both_panes_under_the_first_panes_project() {
        let remote = PaneEntry { host: HostId::named("prod-1"), ..shell_pane(2, "/r/alpha") };
        let mut fixture = Fixture::of(vec![], vec![shell_pane(1, "/r/alpha"), remote]);
        fixture.tabs = vec![tab(0, 1, &[1, 2])];
        let rows = fixture.rows();
        assert!(matches!(&rows[1].kind, RowKind::Tab { panes: 2, first: 1, .. }), "{:?}", rows[1].kind);
        let leaves: Vec<_> = rows.iter().filter(|row| row.in_tab).map(Row::key).collect();
        assert_eq!(leaves, vec![RowKey::Leaf(1), RowKey::Leaf(2)]);
        assert_eq!(rows.iter().filter(|row| matches!(row.kind, RowKind::Project { .. })).count(), 1);
    }

    #[test]
    fn other_project_keeps_heading_for_its_own_rows() {
        let mut fixture =
            Fixture::of(vec![external("e", "/r/beta", 5)], vec![shell_pane(1, "/r/alpha"), shell_pane(2, "/r/beta")]);
        fixture.tabs = vec![tab(0, 1, &[1, 2])];
        let rows = fixture.rows();
        let beta = find(&rows, &RowKey::Project("/r/beta".into())).unwrap();
        assert_eq!(keys(&rows[beta..]), vec![RowKey::Project("/r/beta".into()), RowKey::Session("e".into())]);
        assert!(find(&rows, &RowKey::Leaf(2)).unwrap() < beta);
    }

    #[test]
    fn group_order_is_stable_across_focus_and_tab_moves() {
        let panes = (1..=4).map(|leaf| shell_pane(leaf, "/r/alpha")).collect();
        let mut fixture = Fixture::of(vec![], panes);
        fixture.tabs = vec![tab(0, 1, &[1, 2]), tab(1, 3, &[3, 4])];
        let before = keys(&fixture.rows());
        assert_eq!(&before[1..3], [RowKey::Tab(3), RowKey::Leaf(3)]);
        fixture.tabs = vec![tab(1, 2, &[1, 2]), tab(0, 4, &[3, 4])];
        assert_eq!(keys(&fixture.rows()), before);
        fixture.panes.extend([shell_pane(5, "/r/alpha"), shell_pane(6, "/r/alpha")]);
        fixture.tabs.push(tab(2, 5, &[5, 6]));
        let rows = fixture.rows();
        assert_eq!(keys(&rows)[..3], [RowKey::Project("/r/alpha".into()), RowKey::Tab(5), RowKey::Leaf(5)]);
    }

    #[test]
    fn splitting_a_tab_keeps_its_place() {
        let mut fixture = Fixture::of(vec![], vec![shell_pane(1, "/r/alpha"), shell_pane(3, "/r/alpha")]);
        fixture.tabs = vec![tab(0, 1, &[1]), tab(1, 3, &[3])];
        assert_eq!(keys(&fixture.rows())[1..], [RowKey::Leaf(3), RowKey::Leaf(1)]);
        fixture.panes.push(shell_pane(4, "/r/alpha"));
        fixture.tabs = vec![tab(0, 1, &[1, 4]), tab(1, 3, &[3])];
        assert_eq!(keys(&fixture.rows())[1..], [RowKey::Leaf(3), RowKey::Tab(1), RowKey::Leaf(1), RowKey::Leaf(4)]);
    }

    #[test]
    fn filter_keeps_group_heading_with_matching_panes() {
        let mut fixture = Fixture::of(
            vec![],
            vec![titled_shell(1, "/r/alpha", "Alpha task"), titled_shell(2, "/r/beta", "Beta task")],
        );
        fixture.tabs = vec![tab(0, 1, &[1, 2])];
        fixture.filter = "beta".into();
        let rows = fixture.rows();
        assert_eq!(keys(&rows), vec![RowKey::Project("/r/alpha".into()), RowKey::Tab(1), RowKey::Leaf(2)]);
        assert_eq!(rows[2].project_label.as_deref(), Some("beta"));
    }

    #[test]
    fn tab_group_edges_marks_a_split_tab_and_its_panes() {
        let mut fixture =
            Fixture::of(vec![], vec![shell_pane(1, "/r/alpha"), shell_pane(2, "/r/alpha"), shell_pane(3, "/r/alpha")]);
        fixture.tabs = vec![tab(0, 2, &[2, 1]), tab(1, 3, &[3])];
        let rows = fixture.rows();
        let edges: Vec<Option<(bool, bool)>> =
            (0..rows.len()).map(|i| tab_group_edges(&rows, i)).map(|e| e.map(|e| (e.first, e.last))).collect();
        assert_eq!(edges, vec![None, None, Some((true, false)), Some((false, false)), Some((false, true))]);
    }

    #[test]
    fn a_split_tab_at_the_end_of_the_list_closes_its_box() {
        let mut fixture = Fixture::of(vec![], vec![shell_pane(1, "/r/alpha"), shell_pane(2, "/r/alpha")]);
        fixture.tabs = vec![tab(0, 2, &[1, 2])];
        let rows = fixture.rows();
        let last = rows.len() - 1;
        assert_eq!(tab_group_edges(&rows, last).map(|e| e.last), Some(true));
    }

    #[test]
    fn tab_row_key_and_activation() {
        let mut fixture = Fixture::of(vec![], vec![shell_pane(1, "/r/alpha"), shell_pane(2, "/r/alpha")]);
        fixture.tabs = vec![tab(0, 2, &[1, 2])];
        let rows = fixture.rows();
        let heading = row_of(&rows, &RowKey::Tab(1));
        assert!(heading.selectable());
        assert_eq!(heading.activation(), Activation::FocusLeaf(2));
    }

    fn waiting(id: &str, cwd: &str) -> SessionInfo {
        with(managed(id, cwd, NOW - 1000), SessionStatus::NeedsInput { reason: None })
    }

    #[test]
    fn leaves_in_order_lists_each_pane_once_and_a_split_tab_by_its_rows() {
        let mut fixture =
            Fixture::of(vec![], vec![shell_pane(1, "/r/alpha"), shell_pane(2, "/r/alpha"), shell_pane(3, "/r/alpha")]);
        fixture.tabs = vec![tab(0, 2, &[2, 1]), tab(1, 3, &[3])];
        let rows = fixture.rows();
        assert_eq!(
            keys(&rows),
            vec![RowKey::Project("/r/alpha".into()), RowKey::Leaf(3), RowKey::Tab(2), RowKey::Leaf(2), RowKey::Leaf(1)]
        );
        assert_eq!(leaves_in_order(&rows), vec![3, 2, 1]);
    }

    #[test]
    fn needs_input_counts_only_gantry_panes() {
        let busy = with(managed("b", "/r/alpha", NOW - 2000), SessionStatus::Busy);
        let outside = with(external("e", "/r/alpha", NOW - 3000), SessionStatus::NeedsInput { reason: None });
        let mut fixture = Fixture::of(
            vec![waiting("w", "/r/alpha"), busy, outside],
            vec![pane(1, "/r/alpha", Some("w")), pane(2, "/r/alpha", Some("b"))],
        );
        let rows = fixture.rows();
        assert_eq!(needs_input_leaves(&rows), vec![1]);
    }

    #[test]
    fn next_needing_input_wraps_and_stays_on_the_only_one() {
        let mut fixture = Fixture::of(
            vec![waiting("x", "/r/alpha"), waiting("y", "/r/alpha")],
            vec![pane(1, "/r/alpha", Some("x")), pane(2, "/r/alpha", Some("y"))],
        );
        let rows = fixture.rows();
        let (x, y) = (needs_input_leaves(&rows)[0], needs_input_leaves(&rows)[1]);
        assert_eq!(next_needing_input(&rows, Some(x)), Some(y));
        assert_eq!(next_needing_input(&rows, Some(y)), Some(x));
        assert_eq!(next_needing_input(&rows, None), Some(x));
        let mut one = Fixture::of(vec![waiting("x", "/r/alpha")], vec![pane(1, "/r/alpha", Some("x"))]);
        let rows = one.rows();
        assert_eq!(next_needing_input(&rows, Some(1)), Some(1));
        let mut none = Fixture::of(vec![], vec![shell_pane(1, "/r/alpha")]);
        assert_eq!(next_needing_input(&none.rows(), Some(1)), None);
    }

    #[test]
    fn project_label_is_the_heading_label() {
        let mut fixture = Fixture::of(vec![], vec![shell_pane(1, "/r/alpha")]);
        let rows = fixture.rows();
        assert_eq!(project_label(&rows, Path::new("/r/alpha")).as_deref(), Some("alpha"));
        assert_eq!(project_label(&rows, Path::new("/r/none")), None);
    }

    #[test]
    fn leaf_title_is_the_row_title() {
        let w = waiting("w", "/r/alpha");
        let expected = session_title(&w).to_string();
        let mut fixture = Fixture::of(vec![w], vec![pane(1, "/r/alpha", Some("w")), shell_pane(2, "/r/alpha")]);
        let rows = fixture.rows();
        assert_eq!(leaf_title(&rows, 1), Some(expected));
        assert_eq!(leaf_title(&rows, 2), Some(shell_pane(2, "/r/alpha").title));
        assert_eq!(leaf_title(&rows, 9), None);
    }

    fn remote(session: SessionInfo) -> SessionInfo {
        SessionInfo { host: HostId::named("devbox"), ..session }
    }

    #[test]
    fn the_same_folder_on_two_hosts_is_two_projects_and_only_the_remote_one_names_its_host() {
        let mut fixture =
            Fixture::of(vec![managed("l", "/r/app", NOW - 1), remote(managed("r", "/r/app", NOW - 2))], vec![]);
        assert_eq!(fixture.outline(), vec!["▾ app", "  l (Idle · now)", "▾ devbox - app", "  r (Idle · now)"]);
    }

    #[test]
    fn activating_a_running_session_focuses_its_pane_on_its_own_host() {
        let mut fixture = Fixture::of(vec![remote(managed("r", "/r/app", NOW - 1))], vec![]);
        let rows = fixture.rows();
        let row = rows.iter().find(|row| matches!(row.kind, RowKind::Session { .. })).unwrap();
        assert_eq!(row.activation(), Activation::FocusPane(PaneKey::new(HostId::named("devbox"), 1)));
    }

    #[test]
    fn labels_add_the_host_to_remote_projects_only() {
        let devbox = HostId::named("devbox");
        let paths = vec![
            PathBuf::from("/r/app"),
            project_key(&devbox, Path::new("/home/u/app")),
            project_key(&devbox, Path::new("/home/u/api")),
        ];
        let labels = labels(&paths, Some(Path::new("/home/me")), &[]);
        assert_eq!(labels[&paths[0]], "app");
        assert_eq!(labels[&paths[1]], "devbox - app");
        assert_eq!(labels[&paths[2]], "devbox - api");
    }

    #[test]
    fn two_remote_folders_with_one_name_show_their_paths() {
        let devbox = HostId::named("devbox");
        let paths = vec![project_key(&devbox, Path::new("/srv/a/app")), project_key(&devbox, Path::new("/srv/b/app"))];
        let labels = labels(&paths, None, &[]);
        assert_eq!(labels[&paths[0]], "devbox - /srv/a/app");
        assert_eq!(labels[&paths[1]], "devbox - /srv/b/app");
    }

    fn gantry_pane(leaf: NodeId, cwd: &str, project: &str) -> PaneEntry {
        PaneEntry { project: Some(project.into()), ..shell_pane(leaf, cwd) }
    }

    #[test]
    fn pane_with_project_is_listed_under_its_gantry_heading_only() {
        let mut fixture = Fixture::of(vec![], vec![gantry_pane(1, "/r/alpha", "Billing"), shell_pane(2, "/r/alpha")]);
        fixture.gantry_projects = vec!["Billing".into()];
        let rows = fixture.rows();
        let headings: Vec<_> = rows
            .iter()
            .filter_map(|row| match &row.kind {
                RowKind::Project { label, path, .. } => Some((label.clone(), path.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(headings.len(), 2);
        assert!(headings.contains(&("Billing".into(), PathBuf::from("gantry://billing"))));
        assert!(headings.contains(&("alpha".into(), PathBuf::from("/r/alpha"))));
        let at = |key: RowKey| find(&rows, &key).unwrap();
        let billing = at(RowKey::Project("gantry://billing".into()));
        assert_eq!(at(RowKey::Leaf(1)), billing + 1);
        assert_eq!(rows[billing + 1].project, Some(PathBuf::from("gantry://billing")));
        assert_eq!(rows.iter().filter(|row| row.key() == RowKey::Leaf(1)).count(), 1);
    }

    #[test]
    fn empty_gantry_project_gets_a_heading() {
        let mut fixture = Fixture::of(vec![], vec![]);
        fixture.gantry_projects = vec!["Billing".into()];
        assert_eq!(fixture.outline(), vec!["▾ Billing", "    (empty)"]);
        fixture.toggled.insert(PathBuf::from("gantry://billing"));
        assert_eq!(fixture.outline(), vec!["▸ Billing"]);
        fixture.toggled.clear();
        // A filter hides it, like any project with nothing matching.
        fixture.filter = "zzz".into();
        assert!(fixture.outline().is_empty());
    }

    #[test]
    fn split_tab_sits_under_first_panes_project() {
        let mut fixture = Fixture::of(vec![], vec![gantry_pane(1, "/r/alpha", "Billing"), shell_pane(2, "/r/beta")]);
        fixture.gantry_projects = vec!["Billing".into()];
        fixture.tabs = vec![tab(0, 1, &[1, 2])];
        let rows = fixture.rows();
        assert_eq!(
            keys(&rows),
            vec![RowKey::Project("gantry://billing".into()), RowKey::Tab(1), RowKey::Leaf(1), RowKey::Leaf(2)]
        );
        assert_eq!(rows[2].project_label, None);
        assert_eq!(rows[3].project_label.as_deref(), Some("beta"));
    }

    #[test]
    fn an_external_session_is_never_listed_in_a_gantry_project() {
        let external = session("e1", "/r/alpha", SessionState::External { pid: Some(5) }, NOW - HOUR_MS);
        let mut fixture = Fixture::of(vec![external], vec![]);
        fixture.session_projects.insert("e1".into(), "Billing".into());
        fixture.gantry_projects = vec!["Billing".into()];
        let rows = fixture.rows();
        let heading = |name: &str| {
            rows.iter().position(|row| matches!(&row.kind, RowKind::Project { label, .. } if label == name))
        };
        let (alpha, billing) = (heading("alpha").unwrap(), heading("Billing").unwrap());
        assert!(rows[alpha + 1..].iter().any(|row| matches!(&row.kind, RowKind::Session { .. })), "under its folder");
        assert!(
            !rows[billing + 1..]
                .iter()
                .take_while(|row| !matches!(&row.kind, RowKind::Project { .. }))
                .any(|row| matches!(&row.kind, RowKind::Session { .. })),
            "the project stays empty"
        );
    }

    #[test]
    fn a_renamed_project_keeps_its_place_and_a_deleted_one_is_forgotten() {
        let mut order = ProjectOrder::default();
        order.seed(&paths(&["/a", "gantry://billing", "/b"]));
        order.rename(Path::new("gantry://billing"), Path::new("gantry://invoices"));
        assert_eq!(order.projects(), paths(&["/a", "gantry://invoices", "/b"]));
        order.forget(Path::new("gantry://invoices"));
        assert_eq!(order.projects(), paths(&["/a", "/b"]));
        assert_eq!(order.to_save(&paths(&["/a", "/b"])), paths(&["/a", "/b"]));
    }

    #[test]
    fn to_save_keeps_gantry_keys_even_when_empty() {
        let mut order = ProjectOrder::default();
        order.seed(&paths(&["gantry://billing", "/gone", "/a"]));
        order.update(&paths(&["/a"]), false);
        assert_eq!(order.to_save(&paths(&["/a"])), paths(&["gantry://billing", "/a"]));
    }
}
