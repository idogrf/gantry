//! The sidebar: the Active list (every pane and every Claude session running outside Gantry, by project), colored
//! by what each needs, with a fuzzy filter.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use app_core::host_state::{COLLAPSE_ABOVE, HostDot, hosts_heading};
use app_core::menus::{self, MenuAction, MenuContext};
use app_core::session_tree::{
    self, Activation, ActiveInput, Arrivals, GroupEdges, PaneEntry, ProjectOrder, ProjectRoots, Row, RowKey, RowKind,
    TabEntry, Tone, display_path, session_title,
};
use app_core::sessions::SessionStore;
use app_core::split_tree::NodeId;
use app_core::text_field::TextField;
use app_core::theme_colors::ToneColors;
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, Render, ScrollStrategy, Stateful, Task,
    UniformListDecoration, UniformListScrollHandle, Window, div, prelude::*, px, rgb, rgba, uniform_list,
};
use protocol::host::HostId;
use protocol::message::SessionInfo;

use crate::context_menu::{self, OpenMenu};
use crate::field;
use crate::hover_card::{self, HoverCard, Lead, lead};
use crate::pane_drag::DraggedPane;
use crate::settings_store::{SettingsStore, dir_badge_for, shortcut_label};
use crate::theme::{
    RADIUS, Theme, UI_FONT, container_icon, dir_badge, group_tint, pin_icon, side_line, ssh_icon, sweeping, theme,
    working_sweep,
};

/// Every row is this tall (the list is uniform); session rows draw a pill inside it.
const ROW_HEIGHT: f32 = 39.0;
/// Empty space above and below a row's fill inside its slot; two neighbours are twice this far apart.
const ROW_PAD_Y: f32 = 2.0;
/// Width of the ring around the selected row.
const RING: f32 = 2.0;
/// Empty space between the ring and the selected row's fill.
const RING_GAP: f32 = 1.0;
/// Ages ("5m") and project roots are recomputed this often.
const REFRESH: Duration = Duration::from_secs(30);

/// What an inline rename edits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RenameTarget {
    /// A shell pane, by leaf.
    Pane(NodeId),
    /// A tab, by index.
    Tab(usize),
}

/// What the sidebar asks the workspace to do.
#[derive(Clone, Debug, PartialEq)]
pub enum SidebarEvent {
    /// Focus or resume what a row stands for; `split` = as a split of the focused pane.
    Activate {
        /// What to do.
        activation: Activation,
        /// Open as a split instead of a new tab.
        split: bool,
    },
    /// A right-click menu entry was picked.
    Menu(MenuAction),
    /// Open the right-click menu of tab `index` (a tab heading was right-clicked).
    TabMenu {
        /// The tab.
        index: usize,
        /// Where the menu opens.
        position: Point<Pixels>,
    },
    /// The inline rename was confirmed; an empty `name` clears the name.
    Rename {
        /// What was renamed.
        target: RenameTarget,
        /// The new name, as typed.
        name: String,
    },
    /// A New button was pressed: open the launcher (Claude first, or terminal first).
    New {
        /// Claude (true) or a terminal.
        claude: bool,
    },
    /// A pane of a split tab was dropped on the sidebar: make it a tab of its own.
    PullOut(NodeId),
    /// Open the recent sessions pop-up.
    OpenRecents,
    /// Give keyboard focus back to the pane.
    Dismiss,
    /// The Settings row was clicked: show Settings.
    ShowSettings,
    /// The × of the Settings row was clicked: close Settings.
    CloseSettings,
    /// The × of the Git row was clicked: close the git page.
    CloseGit,
    /// The × of an SSH host in the SSH hosts block was clicked.
    DisconnectHost(HostId),
    /// A host row that is not up was clicked: connect it, or open its Install / Update.
    ConnectHost(HostId),
    /// "Add SSH host…" was clicked.
    AddSshHost,
    /// The update pill of an SSH host was clicked, at this window position.
    UpdateHost {
        /// The host.
        host: HostId,
        /// Where the click was, for the card.
        position: Point<Pixels>,
    },
}

/// A configured SSH host in the SSH hosts block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRow {
    /// The host.
    pub host: HostId,
    /// The dot before its name: green when connected.
    pub dot: HostDot,
    /// Its short state, shown while it is not connected ("connecting…", "not connected").
    pub label: Option<String>,
    /// Connected or on its way: the row has an × and a click does nothing.
    pub up: bool,
    /// The update pill's text (`↑ 0.9.1`) while its daemon is older than this Gantry; `None` = no pill.
    pub update: Option<String>,
    /// Its daemon is being updated: "updating…" replaces the pill and the ×.
    pub updating: bool,
}

/// The session sidebar.
pub struct SidebarView {
    focus_handle: FocusHandle,
    sessions: SessionStore,
    panes: Vec<PaneEntry>,
    tabs: Vec<TabEntry>,
    arrivals: Arrivals,
    order: ProjectOrder,
    filter: TextField,
    /// The row being renamed and its text field.
    renaming: Option<(RenameTarget, TextField)>,
    toggled: HashSet<PathBuf>,
    details: Option<String>,
    /// Sessions with an alert the user has not seen.
    unseen: HashSet<String>,
    marked: HashSet<String>,
    /// The Working glow runs: the setting is on and the window is active.
    animate: bool,
    /// Every row with no filter, every project expanded and no details: the order Ctrl+Tab and the title bar use.
    all_rows: Vec<Row>,
    rows: Vec<Row>,
    selected: Option<RowKey>,
    /// Picked with Shift-click to open together, in pick order.
    together: Vec<NodeId>,
    /// The workspace's focused leaf; its row is selected unless the user moved the ring.
    focused_leaf: Option<NodeId>,
    /// The open right-click menu of a row or heading.
    menu: Option<OpenMenu>,
    scroll: UniformListScrollHandle,
    home: Option<PathBuf>,
    /// Project of each directory seen (it stats parent directories; cleared on every refresh).
    roots: ProjectRoots,
    /// The Settings tab is open (a row above the list), and is the visible tab.
    settings_open: bool,
    settings_active: bool,
    /// The git page is open: the label of its row (the repo's folder name).
    git_row: Option<String>,
    /// The SSH hosts block above Recent sessions: every configured host.
    hosts: Vec<HostRow>,
    /// The block is expanded by hand (it starts collapsed with more than `COLLAPSE_ABOVE` hosts).
    hosts_open: bool,
    /// What a middle press started on, until the button is released: a row, or the Settings row (`None`).
    middle_pressed: Option<Option<RowKey>>,
    _refresh: Task<()>,
}

impl EventEmitter<SidebarEvent> for SidebarView {}

impl Focusable for SidebarView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SidebarView {
    /// An empty sidebar; sessions arrive with `set_sessions`.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH).await;
                // Also forget project roots, so a repository created (or removed) since shows up.
                let refreshed = this.update(cx, |this, cx| {
                    this.roots.clear();
                    this.rebuild(cx);
                });
                if refreshed.is_err() {
                    return;
                }
            }
        });
        let mut sidebar = SidebarView {
            focus_handle: cx.focus_handle(),
            sessions: SessionStore::default(),
            panes: Vec::new(),
            tabs: Vec::new(),
            arrivals: Arrivals::default(),
            order: ProjectOrder::default(),
            filter: TextField::default(),
            renaming: None,
            toggled: HashSet::new(),
            details: None,
            unseen: HashSet::new(),
            marked: HashSet::new(),
            animate: false,
            all_rows: Vec::new(),
            rows: Vec::new(),
            selected: None,
            together: Vec::new(),
            focused_leaf: None,
            menu: None,
            scroll: UniformListScrollHandle::new(),
            home: std::env::home_dir(),
            roots: ProjectRoots::new(std::env::home_dir()),
            settings_open: false,
            settings_active: false,
            git_row: None,
            hosts: Vec::new(),
            hosts_open: false,
            middle_pressed: None,
            _refresh: refresh,
        };
        sidebar.rebuild(cx);
        sidebar
    }

    /// The project a directory belongs to (its git repository, else the directory).
    pub(crate) fn project_of(&self, host: &HostId, cwd: &Path) -> PathBuf {
        self.roots.of(host, cwd)
    }

    /// The picks to open together, in pick order, clearing them.
    pub fn take_together(&mut self, cx: &mut Context<Self>) -> Vec<NodeId> {
        cx.notify();
        std::mem::take(&mut self.together)
    }

    fn is_picked(&self, row: &Row) -> bool {
        row.pick().is_some_and(|pick| self.together.contains(&pick))
    }

    /// Every Gantry leaf in sidebar order, whatever the filter or collapsed projects hide.
    pub fn leaf_order(&self) -> Vec<NodeId> {
        session_tree::leaves_in_order(&self.all_rows)
    }

    /// The leaves whose session needs input, in sidebar order.
    pub fn needs_input(&self) -> Vec<NodeId> {
        session_tree::needs_input_leaves(&self.all_rows)
    }

    /// The leaf needing input after `focused` in sidebar order, wrapping around.
    pub fn next_needing_input(&self, focused: Option<NodeId>) -> Option<NodeId> {
        session_tree::next_needing_input(&self.all_rows, focused)
    }

    /// The name the sidebar gives the project `root`.
    pub fn project_label(&self, root: &Path) -> Option<String> {
        session_tree::project_label(&self.all_rows, root)
    }

    /// The title of `leaf`'s row.
    pub fn leaf_title(&self, leaf: NodeId) -> Option<String> {
        session_tree::leaf_title(&self.all_rows, leaf)
    }

    /// Whether Settings is open and whether it is the visible tab: its row above the list.
    pub fn set_settings(&mut self, open: bool, active: bool, cx: &mut Context<Self>) {
        if (open, active) != (self.settings_open, self.settings_active) {
            self.settings_open = open;
            self.settings_active = active;
            cx.notify();
        }
    }

    /// Whether the git page is open: its row (labelled `label`) sits under the Settings row.
    pub fn set_git(&mut self, label: Option<String>, cx: &mut Context<Self>) {
        if label != self.git_row {
            self.git_row = label;
            cx.notify();
        }
    }

    /// The daemon's sessions as last received.
    pub fn sessions(&self) -> &SessionStore {
        &self.sessions
    }

    /// Replace the session list of `host`.
    pub fn set_sessions(&mut self, host: &HostId, sessions: Vec<SessionInfo>, cx: &mut Context<Self>) {
        self.sessions.replace(host, sessions);
        self.rebuild(cx);
    }

    /// The hosts the SSH hosts block lists.
    pub fn set_hosts(&mut self, hosts: Vec<HostRow>, cx: &mut Context<Self>) {
        if self.hosts != hosts {
            self.hosts = hosts;
            cx.notify();
        }
    }

    /// The SSH hosts block: a filled card with a heading, a row per host (only the ones that are up while it is
    /// collapsed) and "Add SSH host…".
    fn render_hosts(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let total = self.hosts.len();
        let collapsible = total > COLLAPSE_ABOVE;
        let open = !collapsible || self.hosts_open;
        let up = self.hosts.iter().filter(|row| row.up).count();
        let heading = div().px_2().pt(px(2.)).pb_1().text_xs().text_color(rgb(theme.tokens.muted));
        let heading = if collapsible {
            heading
                .id("ssh-hosts-heading")
                .flex()
                .gap_1()
                .cursor_pointer()
                .hover(|style| style.text_color(rgb(theme.tokens.text)))
                .child(if open { "▾" } else { "▸" })
                .child(hosts_heading(up, total))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.hosts_open = !this.hosts_open;
                    cx.notify();
                }))
                .into_any_element()
        } else {
            heading.child(hosts_heading(up, total)).into_any_element()
        };
        let row_base = |id: (&'static str, usize)| {
            div().id(id).h(theme.ui(28.)).px_2().flex().items_center().gap_2().rounded(px(RADIUS - 2.)).text_xs()
        };
        div()
            .flex_none()
            .mx_3()
            .mt_2()
            .p_1()
            .flex()
            .flex_col()
            .gap(px(2.))
            .rounded(px(RADIUS))
            .bg(rgba((theme.tokens.raised << 8) | 0x80))
            .child(heading)
            .children(self.hosts.iter().enumerate().filter(|(_, row)| open || row.up).map(|(index, row)| {
                let host = row.host.clone();
                let dot = div()
                    .flex_none()
                    .size(px(7.))
                    .rounded_full()
                    .bg(rgb(crate::context_menu::host_dot_color(theme, row.dot)));
                let name_color = if row.up { theme.tokens.text } else { theme.tokens.muted };
                let name = div().flex_1().truncate().text_color(rgb(name_color)).child(host.name().to_string());
                let label = row.label.clone().map(|label| div().text_color(rgb(theme.tokens.muted)).child(label));
                if row.up {
                    let update = row.update.clone().filter(|_| !row.updating).map(|text| {
                        let host = host.clone();
                        div()
                            .id(("update-host", index))
                            .flex_none()
                            .h(theme.ui(18.))
                            .px(theme.ui(7.))
                            .flex()
                            .items_center()
                            .rounded_full()
                            .border_1()
                            .border_color(rgb(theme.tokens.accent))
                            .text_size(theme.ui(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(theme.tokens.accent))
                            .cursor_pointer()
                            .child(text)
                            .on_click(cx.listener(move |_, event: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                cx.emit(SidebarEvent::UpdateHost { host: host.clone(), position: event.position() });
                            }))
                    });
                    let updating =
                        row.updating.then(|| div().flex_none().text_color(rgb(theme.tokens.muted)).child("updating…"));
                    let close = (!row.updating).then(|| {
                        div()
                            .id(("disconnect-host", index))
                            .size(theme.ui(18.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .text_color(rgb(theme.tokens.muted))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgba(0x00000026)).text_color(rgb(theme.tokens.text)))
                            .child("×")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.emit(SidebarEvent::DisconnectHost(host.clone()));
                            }))
                    });
                    row_base(("ssh-host", index))
                        .child(dot)
                        .child(name)
                        .children(label)
                        .children(update)
                        .children(updating)
                        .children(close)
                } else {
                    row_base(("ssh-host", index))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(theme.tokens.raised)))
                        .child(dot)
                        .child(name)
                        .children(label)
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(SidebarEvent::ConnectHost(host.clone()))))
                }
            }))
            .children(open.then(|| {
                row_base(("ssh-host-add", 0))
                    .cursor_pointer()
                    .text_color(rgb(theme.tokens.muted))
                    .hover(|style| style.bg(rgb(theme.tokens.raised)).text_color(rgb(theme.tokens.text)))
                    .child(div().flex_none().w(px(7.)).child("+"))
                    .child("Add SSH host…")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::AddSshHost)))
            }))
    }

    /// Apply a session change of `host`.
    pub fn apply(&mut self, host: &HostId, upserted: Vec<SessionInfo>, removed: &[String], cx: &mut Context<Self>) {
        self.sessions.apply(host, upserted, removed);
        self.rebuild(cx);
    }

    /// Sessions to mark with the unseen badge, and those of them marked unread by hand.
    pub fn set_unseen(&mut self, unseen: HashSet<String>, marked: HashSet<String>, cx: &mut Context<Self>) {
        if unseen != self.unseen || marked != self.marked {
            self.unseen = unseen;
            self.marked = marked;
            self.rebuild(cx);
        }
    }

    /// The workspace's focused leaf: its row gets the selection ring.
    pub fn set_focused(&mut self, leaf: Option<NodeId>, cx: &mut Context<Self>) {
        self.focused_leaf = leaf;
        self.selected =
            leaf.and_then(|leaf| session_tree::row_of_leaf(&self.rows, leaf)).map(|index| self.rows[index].key());
        cx.notify();
    }

    /// The workspace's panes and tabs.
    pub fn set_panes(&mut self, panes: Vec<PaneEntry>, tabs: Vec<TabEntry>, cx: &mut Context<Self>) {
        if panes != self.panes || tabs != self.tabs {
            self.panes = panes;
            self.tabs = tabs;
            self.rebuild(cx);
        }
    }

    /// Edit the name of `target` in place on its row, starting from `current` (selected). False, changing nothing
    /// (focus stays), when the row is not in the drawn list (collapsed project, filter).
    pub fn start_rename(
        &mut self,
        target: RenameTarget,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(index) = self.rows.iter().position(|row| renames(&row.kind, target)) else { return false };
        self.scroll.scroll_to_item(index, ScrollStrategy::Center);
        let mut field = TextField::default();
        field.insert(current);
        field.select_all();
        self.renaming = Some((target, field));
        self.focus_handle.focus(window, cx);
        cx.notify();
        true
    }

    /// The inline field standing in for the title of the row of `target`, while it is being renamed.
    fn rename_field(&self, target: RenameTarget, theme: &Theme, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let (_, field) = self.renaming.as_ref().filter(|(editing, _)| *editing == target)?;
        let press = field::on_press(cx, |this, offset, clicks, _, cx| {
            if let Some((_, field)) = &mut this.renaming {
                field.press(offset, clicks);
            }
            cx.notify();
        });
        Some(
            div()
                .flex_1()
                .min_w_0()
                // A press in the field must not activate the row.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(field::render(field, "Name", true, theme, press)),
        )
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let (all_rows, rows) = {
            let sessions: Vec<&SessionInfo> = self.sessions.all().collect();
            let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
            let roots = &self.roots;
            let project_of = |host: &HostId, cwd: &Path| roots.of(host, cwd);
            let build = |filter: &str,
                         toggled: &HashSet<PathBuf>,
                         details: Option<&str>,
                         arrivals: &mut Arrivals,
                         order: &mut ProjectOrder| {
                let input = ActiveInput {
                    sessions: &sessions,
                    panes: &self.panes,
                    home: self.home.as_deref(),
                    filter,
                    toggled,
                    details,
                    unseen: &self.unseen,
                    marked: &self.marked,
                    tabs: &self.tabs,
                    project_of: &project_of,
                    now_ms,
                };
                session_tree::active(&input, arrivals, order)
            };
            // Until the session list arrives, number nothing: panes and sessions must get their arrival
            // numbers together, so that at startup leaves land above outside sessions.
            let mut unloaded = (Arrivals::default(), ProjectOrder::default());
            let (arrivals, order) = if self.sessions.loaded(&HostId::local()) {
                (&mut self.arrivals, &mut self.order)
            } else {
                (&mut unloaded.0, &mut unloaded.1)
            };
            let all_rows = build("", &HashSet::new(), None, arrivals, order);
            let plain = self.filter.text().is_empty() && self.toggled.is_empty() && self.details.is_none();
            let rows = if plain {
                all_rows.clone()
            } else {
                build(self.filter.text(), &self.toggled, self.details.as_deref(), arrivals, order)
            };
            (all_rows, rows)
        };
        self.all_rows = all_rows;
        self.rows = rows;
        // A row that is gone (its pane closed) ends its rename.
        if let Some((target, _)) = &self.renaming
            && !self.rows.iter().any(|row| renames(&row.kind, *target))
        {
            self.renaming = None;
        }
        // A selection whose row is gone falls back to the focused pane's row.
        if self.selected_index().is_none() {
            self.selected = self
                .focused_leaf
                .and_then(|leaf| session_tree::row_of_leaf(&self.rows, leaf))
                .map(|index| self.rows[index].key());
        }
        cx.notify();
    }

    fn selected_index(&self) -> Option<usize> {
        session_tree::find(&self.rows, self.selected.as_ref()?)
    }

    /// Move the selection to the next (or previous) selectable row; from nothing, to the first one.
    fn move_selection(&mut self, down: bool) {
        let selectable: Vec<usize> = (0..self.rows.len()).filter(|&i| self.rows[i].selectable()).collect();
        let Some(&first) = selectable.first() else { return };
        let next = match self.selected_index() {
            None => first,
            Some(current) if down => selectable.iter().copied().find(|&i| i > current).unwrap_or(current),
            Some(current) => selectable.iter().rev().copied().find(|&i| i < current).unwrap_or(current),
        };
        self.selected = Some(self.rows[next].key());
        self.scroll.scroll_to_item(next, ScrollStrategy::Center);
    }

    fn activate(&mut self, index: usize, split: bool, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index) else { return };
        self.selected = Some(row.key());
        match row.activation() {
            Activation::ToggleProject(dir) => {
                if !self.toggled.remove(&dir) {
                    self.toggled.insert(dir);
                }
                self.rebuild(cx);
            }
            Activation::ToggleDetails(id) => {
                self.details = if self.details.as_ref() == Some(&id) { None } else { Some(id) };
                self.rebuild(cx);
            }
            Activation::Nothing => cx.notify(),
            activation => cx.emit(SidebarEvent::Activate { activation, split }),
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if let Some((target, field)) = &mut self.renaming {
            match keystroke.key.as_str() {
                "enter" => {
                    let (target, name) = (*target, field.text().to_string());
                    self.renaming = None;
                    cx.emit(SidebarEvent::Rename { target, name });
                    cx.emit(SidebarEvent::Dismiss);
                }
                "escape" => {
                    self.renaming = None;
                    cx.emit(SidebarEvent::Dismiss);
                }
                _ => {
                    field::edit(field, keystroke, cx);
                }
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        match keystroke.key.as_str() {
            "up" => self.move_selection(false),
            "down" => self.move_selection(true),
            "enter" if !self.together.is_empty() => cx.emit(SidebarEvent::Menu(MenuAction::OpenTogether)),
            "escape" if !self.together.is_empty() => {
                self.together.clear();
                cx.notify();
            }
            "enter" => {
                if let Some(index) = self.selected_index() {
                    self.activate(index, keystroke.modifiers.secondary(), cx);
                }
            }
            "escape" if self.filter.text().is_empty() => cx.emit(SidebarEvent::Dismiss),
            "escape" => {
                self.filter.clear();
                self.rebuild(cx);
            }
            _ => {
                if !field::edit(&mut self.filter, keystroke, cx) {
                    return;
                }
                self.rebuild(cx);
                // Enter right after typing opens the first matching session (not a directory row).
                self.selected = self
                    .rows
                    .iter()
                    .position(|row| matches!(row.kind, RowKind::Session { .. } | RowKind::Pane { .. }))
                    .map(|index| {
                        self.scroll.scroll_to_item(index, ScrollStrategy::Center);
                        self.rows[index].key()
                    });
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Open the right-click menu of row `index` at `position`.
    fn open_menu(&mut self, index: usize, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index) else { return };
        if let RowKind::Tab { index: tab, .. } = row.kind {
            // The workspace owns the tab menu; only one menu may be open.
            self.menu = None;
            cx.emit(SidebarEvent::TabMenu { index: tab, position });
            cx.notify();
            return;
        }
        let settings = cx.global::<SettingsStore>().settings();
        let folder = match &row.kind {
            RowKind::Project { path, .. } => Some(path.as_path()),
            RowKind::Session { session, .. } => Some(session.cwd.as_path()),
            RowKind::Pane { entry, .. } => Some(entry.cwd.as_path()),
            _ => None,
        };
        // A remote folder has no Claude dirs or colors here, but a project heading can be a favorite (saved under its
        // project key), so a heading's context comes from the key itself.
        let remote = match &row.kind {
            RowKind::Project { .. } => false,
            RowKind::Session { session, .. } => !session.host.is_local(),
            RowKind::Pane { entry, .. } => !entry.host.is_local(),
            _ => false,
        };
        let context =
            folder.filter(|_| !remote).map(|folder| MenuContext::for_folder(settings, folder)).unwrap_or_default();
        let alone = |leaf: NodeId| self.panes.iter().find(|pane| pane.leaf == leaf).is_none_or(|pane| pane.alone);
        let items = match &row.kind {
            _ if self.together.len() > 1 && self.is_picked(row) => Some(menus::together_menu(self.together.len())),
            RowKind::Project { path, .. } => {
                // The dev container one of its listed sessions runs in.
                let container = self.rows[index + 1..]
                    .iter()
                    .take_while(|row| !matches!(row.kind, RowKind::Project { .. }))
                    // Rows of another project listed under a tab heading are not this project's.
                    .filter(|row| row.project_label.is_none())
                    .find_map(|row| match &row.kind {
                        RowKind::Session { session, .. } => {
                            session.container.as_ref().filter(|place| place.host_folder.is_some())
                        }
                        _ => None,
                    });
                Some(menus::project_menu(path, container, &context))
            }
            _ => menus::row_menu(row, &alone, &context),
        };
        let Some(items) = items else { return };
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.menu = Some(OpenMenu::new(items, position, focus));
        cx.notify();
    }

    /// A dragged project heading was dropped: place it before `before` (`None` = at the end).
    fn drop_project(&mut self, root: &Path, before: Option<&Path>, cx: &mut Context<Self>) {
        if before != Some(root) {
            self.order.move_project(root, before);
            self.rebuild(cx);
        }
    }

    /// One list row: the project's grey group border (drawn in segments, one per row) around the row itself, and
    /// where a dragged project heading may land.
    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> gpui::Div {
        let theme = theme(cx).clone();
        let edges = session_tree::group_edges(&self.rows, index);
        // A dragged project goes before this row's project (at its heading), or after it (at its last row).
        let target: Option<Option<PathBuf>> = match &self.rows[index].kind {
            RowKind::Project { path, .. } => Some(Some(path.clone())),
            _ if edges.last => Some(self.rows.get(index + 1).and_then(|next| match &next.kind {
                RowKind::Project { path, .. } => Some(path.clone()),
                _ => None,
            })),
            _ => None,
        };
        let content = self.render_row_content(index, cx);
        let group_name = format!("row-{index}");
        let marker = div().absolute().left(theme.ui(14.)).right(theme.ui(14.)).h(px(3.)).rounded(px(2.));
        let marker = if edges.first { marker.top(px(0.)) } else { marker.bottom(px(0.)) };
        let group = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_r_1()
            .border_color(rgb(theme.tokens.line))
            .bg(rgba(group_tint(theme.tokens.text)))
            .when(edges.first, |group| group.border_t_1().rounded_t(px(RADIUS)))
            .when(edges.last, |group| group.border_b_1().rounded_b(px(RADIUS)))
            .child(content);
        div()
            .group(group_name.clone())
            .relative()
            .h(theme.ui(ROW_HEIGHT))
            .w_full()
            .px(theme.ui(6.))
            .flex()
            .flex_col()
            // The whole gap between groups is in the heading row, which has no fill.
            .when(edges.first, |row| row.pt(theme.ui(8.)))
            .child(group)
            .when(target.is_some(), |row| {
                row.child(
                    marker.group_drag_over::<DraggedProject>(group_name, |style| style.bg(rgb(theme.tokens.accent))),
                )
            })
            // Every row takes the drop, so that one that is no landing place does not pass it on to the list.
            .on_drop(cx.listener(move |this, dragged: &DraggedProject, _, cx| {
                if let Some(before) = &target {
                    this.drop_project(&dragged.root, before.as_deref(), cx);
                }
            }))
    }

    fn render_row_content(&self, index: usize, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let row = &self.rows[index];
        let key = row.key();
        let menu_key = key.clone();
        let (middle_down, middle_up) = (key.clone(), key.clone());
        let theme = theme(cx).clone();
        let base = div()
            .id(index)
            .flex_1()
            .min_h_0()
            .w_full()
            .px(theme.ui(10.))
            .py(px(ROW_PAD_Y))
            .flex()
            .flex_row()
            .items_center()
            .text_sm()
            .whitespace_nowrap()
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.focus_handle.focus(window, cx);
                // The list may have been rebuilt since it was drawn: act on the row that was
                // clicked, by identity, or on nothing if it is gone.
                if let Some(index) = session_tree::find(&this.rows, &key) {
                    match this.rows[index].pick().filter(|_| event.modifiers().shift) {
                        Some(pick) => {
                            session_tree::toggle_pick(&mut this.together, pick);
                            cx.notify();
                        }
                        None => this.activate(index, event.modifiers().secondary(), cx),
                    }
                }
            }))
            // A middle click closes the pane or tab the row stands for (press and release on the same row).
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _, _, cx| {
                    this.middle_pressed = Some(Some(middle_down.clone()));
                    cx.stop_propagation();
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    if this.middle_pressed.take() != Some(Some(middle_up.clone())) {
                        return;
                    }
                    let action = session_tree::find(&this.rows, &middle_up)
                        .and_then(|index| menus::middle_click(&this.rows[index]));
                    if let Some(action) = action {
                        cx.emit(SidebarEvent::Menu(action));
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    // By identity: the list may have been rebuilt since this row was drawn.
                    if let Some(index) = session_tree::find(&this.rows, &menu_key) {
                        this.open_menu(index, event.position, window, cx);
                        // Keep focus on the menu: the tracked-focus handlers above must not take it back.
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                }),
            );
        // The visible part of a row: a rounded pill; the selection ring is drawn outside it.
        let pill = |fill: Option<u32>| {
            div()
                .size_full()
                .px(theme.ui(12.))
                .overflow_hidden()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .rounded(px(RADIUS))
                .cursor_pointer()
                .map(|pill| match fill {
                    Some(fill) => pill.bg(rgb(fill)),
                    None => pill.hover(|style| style.bg(rgb(theme.tokens.raised))),
                })
        };
        // The row's project color, as a side line inside the pill's left edge.
        let line = row.project.as_ref().and_then(|root| theme.project(root));
        let lined = |pill: gpui::Div| match line {
            Some(color) => pill.relative().pl(theme.ui(18.)).child(side_line(color, &theme)),
            None => pill,
        };
        let title = |text: String| div().flex_1().min_w_0().truncate().child(text);
        let aside =
            |text: String, ink: u32| div().flex_none().text_xs().text_color(rgba((ink << 8) | 0xcc)).child(text);
        let guide = row
            .in_tab
            .then(|| div().absolute().left(theme.ui(20.)).top_0().bottom_0().w(px(2.)).bg(rgb(theme.tokens.line)));
        let base = base.when(row.in_tab, |base| base.relative().pl(theme.ui(32.))).children(guide);
        let base =
            base.when(self.is_picked(row), |base| base.rounded(px(RADIUS)).bg(rgba((theme.tokens.accent << 8) | 0x2e)));
        // A row that stands for a pane of the layout can be picked up and dropped on a pane or on the sidebar.
        let dragged_pane = match &row.kind {
            RowKind::Session { session, leaf: Some(leaf), .. } => {
                Some(DraggedPane { leaf: *leaf, title: session_title(session).to_string() })
            }
            RowKind::Pane { entry, .. } => Some(DraggedPane { leaf: entry.leaf, title: entry.title.clone() }),
            _ => None,
        };
        let base = base.when_some(dragged_pane, |base, dragged| {
            base.on_drag(dragged, |dragged: &DraggedPane, _offset, _window, cx: &mut App| cx.new(|_| dragged.clone()))
        });
        // A row listed under another project's tab heading names its own project.
        let project = || {
            row.project_label
                .clone()
                .map(|label| div().flex_none().text_xs().text_color(rgb(theme.tokens.muted)).child(label))
        };
        match &row.kind {
            RowKind::Project { path, label, expanded } => {
                let color = theme.project(path);
                let dragged = DraggedProject { root: path.clone(), label: label.clone() };
                base.on_drag(dragged, |dragged: &DraggedProject, _offset, _window, cx: &mut App| {
                    cx.new(|_| dragged.clone())
                })
                .child(
                    pill(None)
                        .child(div().flex_none().text_color(rgb(theme.tokens.muted)).child(if *expanded {
                            "▾"
                        } else {
                            "▸"
                        }))
                        .children(
                            color.map(|color| div().flex_none().size(theme.ui(8.)).rounded(px(3.)).bg(rgb(color))),
                        )
                        .child(
                            title(label.clone())
                                .font_weight(FontWeight::SEMIBOLD)
                                .when_some(color, |title, color| title.text_color(rgb(color))),
                        ),
                )
            }
            RowKind::Session { session, tone, note, .. } => {
                let colors = theme.tone(*tone);
                let card = HoverCard {
                    title: session_title(session).to_string(),
                    status: Some(note.clone()),
                    dir: dir_badge_for(
                        cx.global::<SettingsStore>().settings(),
                        session.claude_dir.as_deref(),
                        &session.host,
                    ),
                    container: session.container.is_some(),
                    host: session.host.remote_name().map(String::from),
                    folder: Some(display_path(&session.cwd, std::env::home_dir().as_deref())),
                };
                base.tooltip(hover_card::tooltip(card)).child(
                    lined(pill(colors.fill))
                        .when(sweeping(*tone, self.animate, true), |pill| {
                            pill.relative().child(working_sweep(("sweep-row", index), px(RADIUS), &theme))
                        })
                        .text_color(rgb(colors.ink))
                        .children(lead_mark(*tone, &colors, &theme))
                        .child(
                            title(session_title(session).to_string())
                                .when(*tone == Tone::NeedsInput, |title| title.font_weight(FontWeight::SEMIBOLD)),
                        )
                        .when(session.container.is_some(), |pill| pill.child(container_icon(colors.ink, &theme)))
                        .when(!session.host.is_local(), |pill| pill.child(ssh_icon(colors.ink, &theme)))
                        .children(
                            dir_badge_for(
                                cx.global::<SettingsStore>().settings(),
                                session.claude_dir.as_deref(),
                                &session.host,
                            )
                            .map(|badge| dir_badge(&badge, &theme)),
                        )
                        .children(project()),
                )
            }
            RowKind::Pane { entry, tone, note } if !entry.claude => base
                .tooltip(hover_card::tooltip(HoverCard {
                    title: entry.title.clone(),
                    status: Some(note.clone()),
                    dir: None,
                    container: false,
                    host: entry.host.remote_name().map(String::from),
                    folder: Some(display_path(&entry.cwd, std::env::home_dir().as_deref())),
                }))
                .child(
                    lined(pill(None))
                        .text_color(rgb(if *tone == Tone::Closed { theme.tokens.muted } else { theme.tokens.text }))
                        .child(div().flex_none().text_xs().text_color(rgb(theme.tokens.muted)).child(">_"))
                        .child(
                            self.rename_field(RenameTarget::Pane(entry.leaf), &theme, cx)
                                .unwrap_or_else(|| title(entry.title.clone())),
                        )
                        .when(!entry.host.is_local(), |pill| pill.child(ssh_icon(theme.tokens.muted, &theme)))
                        .children(project()),
                ),
            RowKind::Pane { entry, tone, note } => {
                let colors = theme.tone(*tone);
                let card = HoverCard {
                    title: "New session".into(),
                    status: Some(note.clone()),
                    dir: None,
                    container: false,
                    host: entry.host.remote_name().map(String::from),
                    folder: Some(display_path(&entry.cwd, std::env::home_dir().as_deref())),
                };
                base.tooltip(hover_card::tooltip(card)).child(
                    lined(pill(colors.fill))
                        .when(sweeping(*tone, self.animate, true), |pill| {
                            pill.relative().child(working_sweep(("sweep-row", index), px(RADIUS), &theme))
                        })
                        .text_color(rgb(colors.ink))
                        .children(lead_mark(*tone, &colors, &theme))
                        .child(title("New session".into()))
                        .when(!entry.host.is_local(), |pill| pill.child(ssh_icon(colors.ink, &theme)))
                        .children(project()),
                )
            }
            RowKind::Tab { title: tab_title, panes, index: tab, .. } => base.child(
                lined(pill(None))
                    .h(theme.ui(28.))
                    .child(
                        self.rename_field(RenameTarget::Tab(*tab), &theme, cx)
                            .unwrap_or_else(|| title(tab_title.clone()))
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(aside(format!("{panes} panes"), theme.tokens.muted)),
            ),
            RowKind::Detail { text, .. } => base.child(
                div()
                    .pl(theme.ui(34.))
                    .overflow_hidden()
                    .text_xs()
                    .italic()
                    .text_color(rgb(theme.tokens.muted))
                    .child(text.clone()),
            ),
            RowKind::TakeOver { .. } => {
                base.child(pill(None).pl(theme.ui(32.)).text_color(rgb(theme.tokens.accent)).child("Take over…"))
            }
        }
    }
}

/// The selection ring, drawn by the list after all rows (inside its clip) so it passes over the neighbouring rows.
struct SelectionRing {
    /// The selected row, its group edges, whether it sits inside a split-tab group and the ring color.
    row: Option<(usize, GroupEdges, bool, u32)>,
    /// The theme's UI scale, for the paddings the row itself uses.
    theme: Theme,
}

impl UniformListDecoration for SelectionRing {
    fn compute(
        &self,
        visible_range: Range<usize>,
        bounds: Bounds<Pixels>,
        _scroll_offset: Point<Pixels>,
        item_height: Pixels,
        _item_count: usize,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        let root = div().w(bounds.size.width).h(bounds.size.height);
        let Some((index, edges, in_tab, color)) = self.row.filter(|(index, ..)| visible_range.contains(index)) else {
            return root.into_any_element();
        };
        let theme = &self.theme;
        let out = px(RING + RING_GAP);
        // The fill sits inside the row's side padding, the group's 1 px side border and its own padding, and under
        // the group's top gap and top/bottom border.
        let top = item_height * index + if edges.first { theme.ui(8.) + px(1.) } else { px(0.) };
        let bottom = item_height * (index + 1) - if edges.last { px(1.) } else { px(0.) };
        let side = theme.ui(6.) + px(1.) + theme.ui(10.) - out;
        // A row in a split-tab group has `pl(32)` instead of the usual 10 (see `render_row_content`).
        let left = if in_tab { theme.ui(6.) + px(1.) + theme.ui(32.) - out } else { side };
        let ring = div()
            .absolute()
            .left(left)
            .right(side)
            .top(top + px(ROW_PAD_Y) - out)
            .h(bottom - top - px(ROW_PAD_Y * 2.) + out * 2.)
            .rounded(px(RADIUS) + out)
            .border_2()
            .border_color(rgb(color));
        root.child(ring).into_any_element()
    }
}

/// Whether a row is the one `target` names.
fn renames(kind: &RowKind, target: RenameTarget) -> bool {
    match (kind, target) {
        (RowKind::Pane { entry, .. }, RenameTarget::Pane(leaf)) => entry.leaf == leaf,
        (RowKind::Tab { index, .. }, RenameTarget::Tab(tab)) => *index == tab,
        _ => false,
    }
}

/// A project heading being dragged to a new place in the list.
#[derive(Clone)]
struct DraggedProject {
    root: PathBuf,
    label: String,
}

impl Render for DraggedProject {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let color = theme.project(&self.root).unwrap_or(theme.tokens.accent);
        div()
            .px(theme.ui(12.))
            .py(theme.ui(6.))
            .rounded(px(RADIUS))
            .border_1()
            .border_color(rgb(color))
            .bg(rgb(theme.tokens.raised))
            .text_color(rgb(color))
            .text_sm()
            .font_family(UI_FONT)
            .font_weight(FontWeight::SEMIBOLD)
            .child(self.label.clone())
    }
}

/// Small rounded counters in the tones of a project's live sessions.
/// The mark before a row's title: a ring for Closed, `↗` for Elsewhere, a pin for Unread, none for the other filled
/// tones.
fn lead_mark(tone: Tone, colors: &ToneColors, theme: &Theme) -> Option<gpui::Div> {
    match lead(tone) {
        Lead::None => None,
        Lead::Pin => Some(div().flex_none().child(pin_icon(colors.dot, theme))),
        Lead::Ring => {
            Some(div().flex_none().size(theme.ui(8.)).rounded_full().border_1().border_color(rgb(colors.dot)))
        }
        Lead::Arrow => Some(div().flex_none().w(theme.ui(10.)).text_color(rgb(colors.dot)).child("↗")),
    }
}

impl Render for SidebarView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.animate = cx.global::<SettingsStore>().settings().animate_working && window.is_window_active();
        let focused = self.focus_handle.is_focused(window);
        // Clicking elsewhere (focus left the sidebar) cancels a rename.
        if !focused {
            self.renaming = None;
        }
        // The rename field has the focus then, not the filter.
        let focused = focused && self.renaming.is_none();
        let theme = theme(cx).clone();
        div()
            .key_context("Sidebar")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_up(MouseButton::Middle, cx.listener(|this, _, _, _| this.middle_pressed = None))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme.tokens.surface))
            .text_color(rgb(theme.tokens.text))
            .font_family(UI_FONT)
            // A pane of a split tab dropped anywhere on the sidebar becomes a tab of its own.
            .drag_over::<DraggedPane>({
                let pullable = session_tree::pullable_leaves(&self.panes);
                let tint = rgba((theme.tokens.accent << 8) | 0x2e);
                move |style, dragged, _, _| if pullable.contains(&dragged.leaf) { style.bg(tint) } else { style }
            })
            .on_drop(cx.listener(|this, dragged: &DraggedPane, _, cx| {
                if session_tree::pullable_leaves(&this.panes).contains(&dragged.leaf) {
                    cx.emit(SidebarEvent::PullOut(dragged.leaf));
                }
            }))
            .child(
                div()
                    .id("filter")
                    .h(theme.ui(36.))
                    .flex_none()
                    .mx_3()
                    .mt_3()
                    .mb_1()
                    .px_3()
                    .flex()
                    .items_center()
                    .text_sm()
                    .rounded(px(RADIUS))
                    .bg(rgb(theme.tokens.raised))
                    .border_1()
                    .border_color(rgb(if focused { theme.tokens.accent } else { theme.tokens.line }))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.focus_handle.focus(window, cx);
                        cx.notify();
                    }))
                    .child(field::render(
                        &self.filter,
                        "Filter active sessions",
                        focused,
                        &theme,
                        field::on_press(cx, |this, offset, clicks, _, cx| {
                            this.filter.press(offset, clicks);
                            cx.notify();
                        }),
                    )),
            )
            .when(self.settings_open, |this| {
                let out = RING + RING_GAP;
                let ring = self.settings_active.then(|| {
                    div()
                        .absolute()
                        .top(px(-out))
                        .bottom(px(-out))
                        .left(px(-out))
                        .right(px(-out))
                        .rounded(px(RADIUS + out))
                        .border_2()
                        .border_color(rgb(theme.selection()))
                });
                this.child(
                    div().relative().h(theme.ui(ROW_HEIGHT - 4.)).flex_none().mx_3().mb_1().children(ring).child(
                        div()
                            .id("settings-row")
                            .size_full()
                            .px(theme.ui(12.))
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_sm()
                            .rounded(px(RADIUS))
                            .bg(rgb(theme.tokens.text))
                            .text_color(rgb(theme.tokens.ground))
                            .cursor_pointer()
                            .child("⚙")
                            .child(div().flex_1().min_w_0().truncate().child("Settings"))
                            .child(
                                div()
                                    .id("close-settings")
                                    .flex_none()
                                    .size(theme.ui(20.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .text_xs()
                                    .hover(|style| style.bg(rgba(0x00000026)))
                                    .child("×")
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(SidebarEvent::CloseSettings);
                                    })),
                            )
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ShowSettings)))
                            .on_mouse_down(
                                MouseButton::Middle,
                                cx.listener(|this, _, _, cx| {
                                    this.middle_pressed = Some(None);
                                    cx.stop_propagation();
                                }),
                            )
                            .on_mouse_up(
                                MouseButton::Middle,
                                cx.listener(|this, _, _, cx| {
                                    cx.stop_propagation();
                                    if this.middle_pressed.take() == Some(None) {
                                        cx.emit(SidebarEvent::CloseSettings);
                                    }
                                }),
                            ),
                    ),
                )
            })
            .when_some(self.git_row.clone(), |this, label| {
                let out = RING + RING_GAP;
                this.child(
                    div()
                        .relative()
                        .h(theme.ui(ROW_HEIGHT - 4.))
                        .flex_none()
                        .mx_3()
                        .mb_1()
                        .child(
                            div()
                                .absolute()
                                .top(px(-out))
                                .bottom(px(-out))
                                .left(px(-out))
                                .right(px(-out))
                                .rounded(px(RADIUS + out))
                                .border_2()
                                .border_color(rgb(theme.selection())),
                        )
                        .child(
                            div()
                                .id("git-row")
                                .size_full()
                                .px(theme.ui(12.))
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_sm()
                                .rounded(px(RADIUS))
                                .bg(rgb(theme.tokens.text))
                                .text_color(rgb(theme.tokens.ground))
                                .child("⎇")
                                .child(div().flex_1().min_w_0().truncate().child(format!("Git · {label}")))
                                .child(
                                    div()
                                        .id("close-git")
                                        .flex_none()
                                        .size(theme.ui(20.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_full()
                                        .text_xs()
                                        .cursor_pointer()
                                        .hover(|style| style.bg(rgba(0x00000026)))
                                        .child("×")
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.stop_propagation();
                                            cx.emit(SidebarEvent::CloseGit);
                                        })),
                                ),
                        ),
                )
            })
            .child(
                // Dropping a project heading on the empty space below the rows puts it last.
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .on_drop(
                        cx.listener(|this, dragged: &DraggedProject, _, cx| this.drop_project(&dragged.root, None, cx)),
                    )
                    .child(
                        uniform_list(
                            "session-rows",
                            self.rows.len(),
                            cx.processor(|this, range: Range<usize>, _window, cx| {
                                range.map(|index| this.render_row(index, cx)).collect::<Vec<_>>()
                            }),
                        )
                        .with_decoration(SelectionRing {
                            row: self.selected_index().map(|index| {
                                let edges = session_tree::group_edges(&self.rows, index);
                                (index, edges, self.rows[index].in_tab, theme.selection())
                            }),
                            theme: theme.clone(),
                        })
                        .track_scroll(&self.scroll)
                        // Room under the last row for its ring.
                        .pb(px(RING + RING_GAP))
                        .flex_1(),
                    ),
            )
            .child(self.render_hosts(&theme, cx))
            .child(
                div()
                    .id("recent-sessions")
                    .flex_none()
                    .h(theme.ui(34.))
                    .mx_3()
                    .mt_2()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(px(RADIUS))
                    .border_1()
                    .border_color(rgb(theme.tokens.line))
                    .text_xs()
                    .text_color(rgb(theme.tokens.muted))
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgb(theme.tokens.text)))
                    .child(div().flex_1().child("Recent sessions"))
                    .child(
                        div()
                            .px(px(6.))
                            .py(px(2.))
                            .rounded(px(6.))
                            .bg(rgb(theme.tokens.raised))
                            .child(shortcut_label(cx.global::<SettingsStore>().settings(), "openRecents")),
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenRecents))),
            )
            .child(
                div()
                    .flex_none()
                    .p_3()
                    .flex()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .id("new-claude")
                            .flex_1()
                            .h(theme.ui(36.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(RADIUS))
                            .bg(rgb(theme.tokens.accent))
                            .text_color(rgb(theme.tokens.on_accent))
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .hover(|style| style.opacity(0.9))
                            .child("New Claude session")
                            .on_click(
                                cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(SidebarEvent::New { claude: true })),
                            ),
                    )
                    .child(
                        div()
                            .id("new-shell")
                            .flex_none()
                            .size(theme.ui(36.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(RADIUS))
                            .border_1()
                            .border_color(rgb(theme.tokens.line))
                            .text_color(rgb(theme.tokens.muted))
                            .text_xs()
                            .cursor_pointer()
                            .hover(|style| style.text_color(rgb(theme.tokens.text)).bg(rgb(theme.tokens.raised)))
                            .child(">_")
                            .tooltip(hover_card::tooltip(HoverCard {
                                title: "New terminal".into(),
                                status: None,
                                dir: None,
                                container: false,
                                host: None,
                                folder: None,
                            }))
                            .on_click(
                                cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(SidebarEvent::New { claude: false })),
                            ),
                    ),
            )
            .when_some(self.menu.as_ref(), |el, menu| {
                let pick = cx.listener(|this, action: &MenuAction, _, cx| {
                    this.menu = None;
                    cx.emit(SidebarEvent::Dismiss);
                    cx.emit(SidebarEvent::Menu(action.clone()));
                    cx.notify();
                });
                let dismiss = cx.listener(|this, _: &(), _, cx| {
                    this.menu = None;
                    cx.emit(SidebarEvent::Dismiss);
                    cx.notify();
                });
                el.child(context_menu::render(
                    "sidebar-menu",
                    menu,
                    &theme,
                    cx.global::<SettingsStore>().settings(),
                    move |action, window, cx| pick(action, window, cx),
                    move |window, cx| dismiss(&(), window, cx),
                ))
            })
    }
}
