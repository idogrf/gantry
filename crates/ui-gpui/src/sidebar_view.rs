//! The sidebar: the Active list (every pane and every Claude session running outside Gantry, by project), colored
//! by what each needs, with a fuzzy filter.

use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use app_core::host_state::{HostDot, hosts_summary};
use app_core::hosts_fold::HostsFold;
use app_core::menus::{self, MenuAction, MenuContext};
use app_core::session_tree::{
    self, Activation, ActiveInput, Arrivals, DragSource, DropAt, GroupEdges, PaneEntry, ProjectItems, ProjectOrder,
    ProjectRoots, Row, RowKey, RowKind, TabEntry, Tone, display_path, project_items, reorder, session_title,
    shown_order,
};
use app_core::sessions::SessionStore;
use app_core::sidebar_compact::tile_letter;
use app_core::sidebar_order::SidebarOrder;
use app_core::split_tree::NodeId;
use app_core::text_field::TextField;
use app_core::theme_colors::ToneColors;
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, Render, ScrollStrategy, Stateful, Task,
    UniformListDecoration, UniformListScrollHandle, Window, div, prelude::*, px, relative, rgb, rgba, svg,
    uniform_list,
};
use protocol::host::HostId;
use protocol::message::SessionInfo;

use crate::assets::{CHEVRON_DOWN_ICON, CHEVRON_RIGHT_ICON, CLOSE_ICON};
use crate::context_menu::{self, OpenMenu};
use crate::field;
use crate::git_scrollbar::Extent;
use crate::hover_card::{self, HostsCard, HoverCard, Lead, lead, line_width};
use crate::pane_drag::DraggedPane;
use crate::settings_store::{SettingsStore, dir_badge_for, shortcut_label};
use crate::theme::{
    RADIUS, Theme, UI_FONT, container_icon, dir_badge, group_tint, pane_marker, pill_sweep, pin_icon, side_line,
    ssh_icon, sweeping, theme, working_sweep,
};

/// The width of the pill that shows a project's state, three dots wide.
const PILL_WIDTH: f32 = 24.0;

/// Every row is this tall (the list is uniform); session rows draw a pill inside it.
const ROW_HEIGHT: f32 = 39.0;
/// Empty space above and below a row's fill inside its slot; two neighbours are twice this far apart.
const ROW_PAD_Y: f32 = 2.0;
/// Space left of a project heading's fill, inside the group.
const HEADING_INSET: f32 = 10.0;
/// Space left of a session's (or tab's, or pane's) fill, inside the group. Its bar and text sit further in the fill
/// (`lined`) than the heading's name, so the fill starts 5 px, not 10, right of the heading's.
const SESSION_INSET: f32 = 15.0;
/// The same for a row in a split-tab group, which is indented under the split tab's heading.
const TAB_INSET: f32 = 37.0;
/// The least width the hairline in a project heading is drawn at.
const MIN_RULE: f32 = 16.0;
/// Space left of a compact tile's fill inside the row's own padding (the row's `PILL_INSET` comes on top).
const TILE_EDGE: f32 = 3.0;
/// Width of the ring around the selected row.
const RING: f32 = 2.0;
/// Empty space between the ring and the selected row's fill, besides `PILL_INSET`.
const RING_GAP: f32 = 1.0;
/// How far a row's fill is pulled in from its slot on every side; the ring stays where the slot puts it.
const PILL_INSET: f32 = 2.0;
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
    /// The user moved a project or a tab: this is the order to save in the layout.
    OrderChanged(SidebarOrder),
    /// Open the recent sessions pop-up.
    OpenRecents,
    /// The toggle button was pressed: collapse the sidebar to its compact column, or expand it.
    ToggleCompact,
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
    /// The SSH hosts heading was clicked: the block is now folded (`true`) or open, to be saved.
    HostsCollapsed(bool),
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
    /// The order the user gave projects and tabs, as saved in the layout.
    saved_order: SidebarOrder,
    /// The movable items of the listed projects (none while a filter is typed).
    items: Rc<Vec<ProjectItems>>,
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
    /// The workspace's focused leaf; its row is selected unless the user moved the ring.
    focused_leaf: Option<NodeId>,
    /// The open right-click menu of a row or heading.
    menu: Option<OpenMenu>,
    scroll: UniformListScrollHandle,
    home: Option<PathBuf>,
    /// Project of each directory seen (it stats parent directories; cleared on every refresh).
    roots: ProjectRoots,
    gantry_projects: Vec<String>,
    session_projects: BTreeMap<String, String>,
    /// The Settings tab is open (a row above the list), and is the visible tab.
    settings_open: bool,
    settings_active: bool,
    /// The git page is open: the label of its row (the repo's folder name).
    git_row: Option<String>,
    /// The SSH hosts block above Recent sessions: every configured host.
    hosts: Vec<HostRow>,
    /// Whether the SSH hosts block is folded to one line, by hand or by the sessions list starting to scroll.
    hosts_fold: HostsFold,
    /// What a middle press started on, until the button is released: a row, or the Settings row (`None`).
    middle_pressed: Option<Option<RowKey>>,
    /// How compact the sidebar is drawn: 0 = expanded, 1 = the compact column, between while it animates.
    amount: f32,
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
            saved_order: SidebarOrder::default(),
            items: Rc::new(Vec::new()),
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
            focused_leaf: None,
            menu: None,
            scroll: UniformListScrollHandle::new(),
            home: std::env::home_dir(),
            roots: ProjectRoots::new(std::env::home_dir()),
            gantry_projects: Vec::new(),
            session_projects: BTreeMap::new(),
            settings_open: false,
            settings_active: false,
            git_row: None,
            hosts: Vec::new(),
            hosts_fold: HostsFold::new(false),
            middle_pressed: None,
            amount: 0.0,
            _refresh: refresh,
        };
        sidebar.rebuild(cx);
        sidebar
    }

    /// Draw the sidebar `amount` of the way to its compact column (0 = expanded, 1 = compact). The rows follow the
    /// filter again, or stop following it, when this crosses half way.
    pub fn set_amount(&mut self, amount: f32, cx: &mut Context<Self>) {
        if self.amount != amount {
            let compact = self.amount > 0.5;
            self.amount = amount;
            if (amount > 0.5) != compact {
                self.rebuild(cx);
            }
            cx.notify();
        }
    }

    /// The filter text the rows follow: none while the sidebar is compact (the text itself is kept).
    fn filter_text(&self) -> &str {
        applied_filter(self.filter.text(), self.amount)
    }

    /// The `>_` button that opens a terminal: `scale` = 1 at full size, 0 collapsed to nothing.
    fn new_terminal_button(
        &self,
        id: &'static str,
        scale: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        div()
            .id(id)
            .flex_none()
            .w(theme.ui(36. * scale))
            .h(theme.ui(36.))
            .overflow_hidden()
            .opacity(scale)
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
            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(SidebarEvent::New { claude: false })))
    }

    /// The project a pane belongs to: the Gantry project `gantry` when given, else its directory's (its git
    /// repository, else the directory).
    pub(crate) fn project_of(&self, host: &HostId, cwd: &Path, gantry: Option<&str>) -> PathBuf {
        self.roots.of_member(host, cwd, gantry)
    }

    /// The Gantry projects and the projects sessions were put in.
    pub fn set_projects(
        &mut self,
        projects: Vec<String>,
        session_projects: BTreeMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        if projects != self.gantry_projects || session_projects != self.session_projects {
            self.gantry_projects = projects;
            self.session_projects = session_projects;
            self.rebuild(cx);
        }
    }

    /// Every Gantry leaf in sidebar order, whatever the filter or collapsed projects hide.
    pub fn leaf_order(&self) -> Vec<NodeId> {
        session_tree::leaves_in_order(&self.all_rows)
    }

    /// The leaves whose rows a collapsed project hides (none while a filter expands every project).
    pub fn collapsed_leaves(&self) -> Vec<NodeId> {
        if self.filter_text().is_empty() { session_tree::hidden_leaves(&self.all_rows, &self.rows) } else { Vec::new() }
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

    /// Starts the SSH hosts block as saved in the layout.
    pub fn set_hosts_collapsed(&mut self, collapsed: bool) {
        self.hosts_fold = HostsFold::new(collapsed);
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

    /// The SSH hosts block: a filled card with a heading that folds it, a row per host and "Add SSH host…"; folded, it
    /// is the heading with the number of connected hosts and one status dot.
    fn render_hosts(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let open = !self.hosts_fold.collapsed();
        // A connected host being updated shows the Busy dot on its row but still counts as connected.
        let summary_dot = |row: &HostRow| if row.up && row.label.is_none() { HostDot::Ok } else { row.dot };
        let summary = hosts_summary(&self.hosts.iter().map(summary_dot).collect::<Vec<_>>());
        let pill_color = if summary.connected > 0 { theme.tokens.accent } else { theme.tokens.muted };
        let amount = self.amount;
        let hosts_card = || HostsCard {
            hosts: self
                .hosts
                .iter()
                .map(|row| {
                    (summary_dot(row), row.host.name().to_string(), row.label.clone().unwrap_or("connected".into()))
                })
                .collect(),
        };
        // Compact: one tile with the connected count and the status dot; a click opens the sidebar.
        if amount > 0.5 {
            return div()
                .id("ssh-hosts-tile")
                .flex_none()
                .mx(theme.ui(lerp(12., 6., amount)))
                .mt_2()
                .h(theme.ui(36.))
                .flex()
                .items_center()
                .justify_center()
                .gap(theme.ui(5.))
                .rounded(px(RADIUS))
                .opacity(2. * amount - 1.)
                .cursor_pointer()
                .hover(|style| style.bg(rgba((theme.tokens.raised << 8) | 0x80)))
                .when(summary.total > 0, |tile| {
                    tile.child(
                        div()
                            .flex_none()
                            .h(theme.ui(16.))
                            .px(theme.ui(6.))
                            .flex()
                            .items_center()
                            .rounded_full()
                            .bg(rgba((pill_color << 8) | 0x26))
                            .text_size(theme.ui(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(pill_color))
                            .child(summary.connected.to_string()),
                    )
                    .tooltip(hover_card::tooltip(hosts_card()))
                })
                .child(
                    div()
                        .flex_none()
                        .size(px(7.))
                        .rounded_full()
                        .bg(rgb(crate::context_menu::host_dot_color(theme, summary.dot))),
                )
                .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ToggleCompact)))
                .into_any_element();
        }
        let heading = div()
            .id("ssh-hosts-heading")
            .px_2()
            .pt(px(2.))
            .pb_1()
            .flex()
            .items_center()
            .gap_1()
            .text_xs()
            .text_color(rgb(theme.tokens.muted))
            .cursor_pointer()
            .hover(|style| style.text_color(rgb(theme.tokens.text)))
            .child(div().flex_none().child(if open { "▾" } else { "▸" }))
            .child(div().flex_1().truncate().child("SSH hosts"))
            .when(!open && summary.total > 0, |heading| {
                heading.child(
                    div()
                        .flex_none()
                        .h(theme.ui(16.))
                        .px(theme.ui(6.))
                        .flex()
                        .items_center()
                        .rounded_full()
                        .bg(rgba((pill_color << 8) | 0x26))
                        .text_size(theme.ui(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(pill_color))
                        .child(summary.connected.to_string()),
                )
            })
            .when(!open, |heading| {
                heading.child(
                    div()
                        .flex_none()
                        .size(px(7.))
                        .rounded_full()
                        .bg(rgb(crate::context_menu::host_dot_color(theme, summary.dot))),
                )
            })
            .when(!open && summary.total > 0, |heading| heading.tooltip(hover_card::tooltip(hosts_card())))
            .on_click(cx.listener(|this, _, _, cx| {
                let collapsed = this.hosts_fold.toggle();
                cx.emit(SidebarEvent::HostsCollapsed(collapsed));
                cx.notify();
            }));
        let row_base = |id: (&'static str, usize)| {
            div().id(id).h(theme.ui(28.)).px_2().flex().items_center().gap_2().rounded(px(RADIUS - 2.)).text_xs()
        };
        div()
            .flex_none()
            .mx(theme.ui(lerp(12., 6., amount)))
            .mt_2()
            .p_1()
            .flex()
            .flex_col()
            .gap(px(2.))
            .rounded(px(RADIUS))
            .opacity(1. - 2. * amount)
            .bg(rgba((theme.tokens.raised << 8) | 0x80))
            .child(heading)
            .when(open, |card| {
                card.children(self.hosts.iter().enumerate().map(|(index, row)| {
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
                                    cx.emit(SidebarEvent::UpdateHost {
                                        host: host.clone(),
                                        position: event.position(),
                                    });
                                }))
                        });
                        let updating = row
                            .updating
                            .then(|| div().flex_none().text_color(rgb(theme.tokens.muted)).child("updating…"));
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
                .child(
                    row_base(("ssh-host-add", 0))
                        .cursor_pointer()
                        .text_color(rgb(theme.tokens.muted))
                        .hover(|style| style.bg(rgb(theme.tokens.raised)).text_color(rgb(theme.tokens.text)))
                        .child(div().flex_none().w(px(7.)).child("+"))
                        .child("Add SSH host…")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::AddSshHost))),
                )
            })
            .into_any_element()
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

    /// The order saved in the layout: where projects and tabs sit until the user moves them.
    pub fn set_saved_order(&mut self, order: SidebarOrder, cx: &mut Context<Self>) {
        if !order.projects.is_empty() {
            self.order.seed(&order.projects);
        }
        self.saved_order = order;
        self.rebuild(cx);
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
                    session_projects: &self.session_projects,
                    gantry_projects: &self.gantry_projects,
                    saved_order: &self.saved_order,
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
            let filter = applied_filter(self.filter.text(), self.amount);
            // The details of an outside session have no place in the compact column.
            let details = if self.amount > 0.5 { None } else { self.details.as_deref() };
            let plain = filter.is_empty() && self.toggled.is_empty() && details.is_none();
            let rows = if plain { all_rows.clone() } else { build(filter, &self.toggled, details, arrivals, order) };
            (all_rows, rows)
        };
        self.all_rows = all_rows;
        self.rows = rows;
        // Tabs can only be moved while nothing is filtered away.
        self.items = Rc::new(if self.filter_text().is_empty() { project_items(&self.rows) } else { Vec::new() });
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
        let mut context =
            folder.filter(|_| !remote).map(|folder| MenuContext::for_folder(settings, folder)).unwrap_or_default();
        let menu_leaf = match &row.kind {
            RowKind::Session { leaf, .. } => *leaf,
            RowKind::Pane { entry, .. } => Some(entry.leaf),
            _ => None,
        };
        context.pane_color = menu_leaf
            .and_then(|leaf| self.panes.iter().find(|pane| pane.leaf == leaf))
            .map(|pane| (pane.leaf, pane.color));
        let alone = |leaf: NodeId| self.panes.iter().find(|pane| pane.leaf == leaf).is_none_or(|pane| pane.alone);
        let items = match &row.kind {
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
            let (projects, leaves) = shown_order(&project_items(&self.all_rows), None);
            self.record_order(&projects, &leaves, cx);
        }
    }

    /// Half a row that takes a dragged tab (a pane's row or a split tab's heading) to `at`: it draws a line at its top
    /// or bottom edge while an item that can go there is over it.
    fn landing(&self, at: DropAt, top: bool, cx: &mut Context<Self>) -> gpui::Div {
        let theme = theme(cx).clone();
        let half = theme.ui(ROW_HEIGHT / 2.);
        let accent = rgb(theme.tokens.accent);
        let (pane_items, tab_items) = (self.items.clone(), self.items.clone());
        let line = move |style: gpui::StyleRefinement| {
            if top { style.border_t_2().border_color(accent) } else { style.border_b_2().border_color(accent) }
        };
        div()
            .absolute()
            .left_0()
            .right_0()
            .when(top, |half_row| half_row.top_0())
            .when(!top, |half_row| half_row.bottom_0())
            .h(half)
            .drag_over::<DraggedPane>(move |style, dragged, _, _| {
                if reorder(&pane_items, DragSource::Pane(dragged.leaf), at).is_some() { line(style) } else { style }
            })
            .drag_over::<DraggedTab>(move |style, dragged, _, _| {
                if reorder(&tab_items, DragSource::Tab(dragged.first), at).is_some() { line(style) } else { style }
            })
            .on_drop(cx.listener(move |this, dragged: &DraggedPane, _, cx| {
                this.drop_item(DragSource::Pane(dragged.leaf), at, cx)
            }))
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, _, cx| {
                this.drop_item(DragSource::Tab(dragged.first), at, cx)
            }))
    }

    /// A tab was dropped at `at`: move it inside its project and save the new order. A drop that is no move leaves
    /// the drop to the list (a pane of a split tab becomes a tab of its own).
    fn drop_item(&mut self, source: DragSource, at: DropAt, cx: &mut Context<Self>) {
        let Some((project, blocks)) = reorder(&self.items, source, at) else {
            if let DragSource::Pane(leaf) = source
                && session_tree::pullable_leaves(&self.panes).contains(&leaf)
            {
                cx.emit(SidebarEvent::PullOut(leaf));
            }
            return;
        };
        let all = project_items(&self.all_rows);
        let Some(shown) = all.iter().position(|items| items.path == self.items[project].path) else { return };
        let (projects, leaves) = shown_order(&all, Some((shown, &blocks)));
        self.record_order(&projects, &leaves, cx);
    }

    /// Save the order shown now (the user just changed it) and tell the workspace.
    fn record_order(&mut self, shown_projects: &[PathBuf], leaves: &[NodeId], cx: &mut Context<Self>) {
        let projects = self.order.to_save(shown_projects);
        self.saved_order.record(&projects, leaves);
        let panes = &self.panes;
        self.saved_order.retain_leaves(|leaf| panes.iter().any(|pane| pane.leaf == leaf));
        self.order.seed(&self.saved_order.projects);
        self.rebuild(cx);
        cx.emit(SidebarEvent::OrderChanged(self.saved_order.clone()));
    }

    /// One list row: the project's grey group border (drawn in segments, one per row) around the row itself, and
    /// where a dragged project heading may land.
    fn render_row(&self, index: usize, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
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
        let content = self.render_row_content(index, window, cx);
        // A dragged tab lands before this item (upper half of its first row) or before the next one (lower half of
        // its last row); there is no landing outside the item's own project.
        let place = self.items.iter().enumerate().find_map(|(project, items)| {
            items.blocks.iter().enumerate().find_map(|(block, item)| {
                (item.row..=item.last_row).contains(&index).then_some((
                    project,
                    block,
                    item.row == index,
                    item.last_row == index,
                    items.blocks.len(),
                ))
            })
        });
        let landings = place.map(|(project, block, first, last, count)| {
            let upper = first.then(|| self.landing(DropAt::Before { project, block }, true, cx));
            let lower = last.then(|| {
                let at = if block + 1 < count {
                    DropAt::Before { project, block: block + 1 }
                } else {
                    DropAt::End { project }
                };
                self.landing(at, false, cx)
            });
            (upper, lower)
        });
        let group_name = format!("row-{index}");
        let marker = div().absolute().left(theme.ui(14.)).right(theme.ui(14.)).h(px(3.)).rounded(px(2.));
        let marker = if edges.first { marker.top(px(0.)) } else { marker.bottom(px(0.)) };
        let amount = self.amount;
        let faded = 1.0 - amount;
        // A split tab's box: absent when expanded, an accent-tinted box around the heading and its panes when compact.
        let content = match session_tree::tab_group_edges(&self.rows, index) {
            Some(tab) => div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .ml(theme.ui(2. * amount))
                .mr(theme.ui(2. * amount))
                .when(amount > 0.0, |boxed| {
                    boxed
                        .border_l_1()
                        .border_r_1()
                        .border_color(rgba(with_opacity(theme.tokens.accent, 0.55 * amount)))
                        .bg(rgba(with_opacity(theme.tokens.accent, 0.16 * amount)))
                        .when(tab.first, |boxed| boxed.border_t_1().rounded_t(px(RADIUS)))
                        .when(tab.last, |boxed| boxed.border_b_1().rounded_b(px(RADIUS)))
                })
                .child(content)
                .into_any_element(),
            None => content.into_any_element(),
        };
        // The project group's border and tint fade out with the compact column.
        let group_alpha = |color: u32, opacity: f32| rgba(with_opacity(color, opacity * faded));
        let group = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_r_1()
            .border_color(group_alpha(theme.tokens.line, 1.0))
            .bg(rgba((theme.tokens.text << 8) | ((group_tint(theme.tokens.text) & 0xff) as f32 * faded).round() as u32))
            .when(edges.first, |group| group.border_t_1().rounded_t(px(RADIUS)))
            .when(edges.last, |group| group.border_b_1().rounded_b(px(RADIUS)))
            .child(content);
        div()
            .group(group_name.clone())
            .relative()
            .h(theme.ui(ROW_HEIGHT))
            .w_full()
            .px(theme.ui(6. * faded))
            .flex()
            .flex_col()
            // The whole gap between groups is in the heading row, which has no fill.
            .when(edges.first, |row| row.pt(theme.ui(8.)))
            .child(group)
            // Compact: a hairline in that gap above every project's dot but the first.
            .when(matches!(self.rows[index].kind, RowKind::Project { .. }) && index > 0 && amount > 0.0, |row| {
                row.child(
                    div()
                        .absolute()
                        .top(theme.ui(3.))
                        .left(theme.ui(12.))
                        .right(theme.ui(12.))
                        .h(px(1.))
                        .bg(rgba(with_opacity(theme.tokens.line, amount))),
                )
            })
            .when_some(landings, |row, (upper, lower)| row.children(upper).children(lower))
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

    /// The close button of row `index`, in the slot of its right-most mark. It replaces `badge` while the row is
    /// hovered; a row without a badge opens a slot of its own then. Rows that cannot be closed keep `badge`.
    fn close_slot(
        &self,
        index: usize,
        badge: Option<gpui::Div>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        let Some(action) = menus::middle_click(&self.rows[index]) else { return badge };
        let group = format!("row-{index}");
        let danger = theme.tokens.danger;
        let button = div()
            .id(("close-row", index))
            .flex_none()
            .size(theme.ui(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|style| style.bg(rgba((danger << 8) | 0x24)))
            .child(svg().path(CLOSE_ICON).size(theme.ui(14.)).text_color(rgba((danger << 8) | 0xc7)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                    cx.emit(SidebarEvent::Menu(action.clone()));
                    cx.stop_propagation();
                }),
            );
        Some(match badge {
            Some(badge) => {
                div().relative().flex_none().child(badge.group_hover(group.clone(), |style| style.opacity(0.))).child(
                    button
                        .absolute()
                        .top(px(-2.))
                        .left(px(-2.))
                        .opacity(0.)
                        .group_hover(group, |style| style.opacity(1.)),
                )
            }
            None => {
                let (gap, open) = (theme.ui(8.), theme.ui(20.));
                div()
                    .flex_none()
                    .w(px(0.))
                    .ml(-gap)
                    .overflow_hidden()
                    .group_hover(group, move |style| style.w(open).ml(px(0.)))
                    .child(button)
            }
        })
    }

    fn render_row_content(&self, index: usize, window: &Window, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let row = &self.rows[index];
        let key = row.key();
        let menu_key = key.clone();
        let (middle_down, middle_up) = (key.clone(), key.clone());
        let theme = theme(cx).clone();
        let amount = self.amount;
        let faded = 1.0 - amount;
        let base = div()
            .id(index)
            .flex_1()
            .min_h_0()
            .w_full()
            .pl(theme.ui(lerp(SESSION_INSET, TILE_EDGE, amount)) + px(PILL_INSET))
            .pr(theme.ui(lerp(HEADING_INSET, TILE_EDGE, amount)) + px(PILL_INSET))
            .py(px(ROW_PAD_Y + PILL_INSET))
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
                    this.activate(index, event.modifiers().secondary(), cx);
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
                .px(theme.ui(12. * faded))
                .relative()
                .overflow_hidden()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .rounded(px(RADIUS))
                .cursor_pointer()
                // The panes of a split tab are a little smaller than the other tiles when compact.
                .when(row.in_tab && amount > 0.0, |pill| {
                    pill.h(theme.ui(ROW_HEIGHT) - px(2. * (ROW_PAD_Y + PILL_INSET)) - theme.ui(4. * amount))
                })
                .map(|pill| match fill {
                    Some(fill) => pill.bg(rgb(fill)),
                    None => pill.hover(|style| style.bg(rgb(theme.tokens.raised))),
                })
        };
        // The row's project color, as a side line inside the pill's left edge.
        let line = row.project.as_ref().and_then(|root| theme.project(root));
        // The pane's own color, as a second bar beside it; a split tab's heading shows only the project line.
        let pane_color = match &row.kind {
            RowKind::Session { leaf: Some(leaf), .. } => {
                self.panes.iter().find(|pane| pane.leaf == *leaf).and_then(|pane| pane.color)
            }
            RowKind::Pane { entry, .. } => entry.color,
            _ => None,
        }
        .map(|tint| tint.value(theme.preset));
        let lined = |pill: gpui::Div| match (line, pane_color) {
            (Some(color), marker) => pill
                .pl(theme.ui(35. * faded))
                .child(side_line(color, &theme).left(theme.ui(9.)).opacity(faded))
                .children(marker.map(|marker| pane_marker(marker, lerp(17., 4., amount), &theme))),
            (None, Some(marker)) => {
                pill.pl(theme.ui(23. * faded)).child(pane_marker(marker, lerp(9., 4., amount), &theme))
            }
            (None, None) => pill.pl(theme.ui(17. * faded)),
        };
        // What a row says in words (marks, title, notes, badge); it fades out as the column closes.
        let line_content =
            |faded: f32| div().flex_1().min_w_0().flex().flex_row().items_center().gap_2().opacity(faded);
        let title = |text: String| div().flex_1().min_w_0().truncate().child(text);
        let aside =
            |text: String, ink: u32| div().flex_none().text_xs().text_color(rgba((ink << 8) | 0xcc)).child(text);
        let guide = row.in_tab.then(|| {
            div().absolute().left(theme.ui(30.)).top_0().bottom_0().w(px(2.)).opacity(faded).bg(rgb(theme.tokens.line))
        });
        let base = base
            .when(row.in_tab, |base| base.relative().pl(theme.ui(lerp(TAB_INSET, TILE_EDGE, amount)) + px(PILL_INSET)))
            .children(guide);
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
            RowKind::Project { path, label, expanded, urgent, working } => {
                let dot = urgent.map_or(theme.tokens.muted, |tone| {
                    let colors = theme.tone(tone);
                    colors.fill.unwrap_or(colors.dot)
                });
                // The glow over the pill says a session in the project is working, whatever state the pill shows; its
                // band is the saturated color of that state.
                let glow = *working && sweeping(Tone::Working, self.animate, true);
                let band = theme.tone(urgent.unwrap_or(Tone::Working)).dot;
                let color = theme.project(path);
                let dragged = DraggedProject { root: path.clone(), label: label.clone() };
                // The hairline needs room: with less than `MIN_RULE` left between the name and the dot it is not drawn
                // and the name takes the space, so no stub is left. The width is the list's of the last frame.
                let rule = Extent::of(&self.scroll).is_none_or(|extent| {
                    let name = line_width(window, label, window.rem_size() * 0.875, FontWeight::SEMIBOLD);
                    let marks = theme.ui(PILL_WIDTH) + theme.ui(14.);
                    let chrome = theme.ui(12. + 2. + HEADING_INSET * 2. + 8. + 6.) + theme.ui(6.) * 3.;
                    px(extent.viewport.0) - chrome - marks - name >= theme.ui(MIN_RULE)
                });
                // The name starts further left than the sessions' bars, so the heading reads as their parent; the
                // project dot and the fold chevron sit together at the right, joined to the name by a hairline.
                base.pl(theme.ui(lerp(HEADING_INSET, TILE_EDGE, amount)) + px(PILL_INSET))
                    .on_drag(dragged, |dragged: &DraggedProject, _offset, _window, cx: &mut App| {
                        cx.new(|_| dragged.clone())
                    })
                    .child(
                        pill(None)
                            .pl(theme.ui(8.))
                            .pr(theme.ui(6.))
                            .gap(theme.ui(6.))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .when_some(color, |name, color| name.text_color(rgb(color)))
                                    .when(!rule, |name| name.flex_1())
                                    .opacity(faded)
                                    .child(label.clone()),
                            )
                            // A hairline in the project color joins the name to the dot and the chevron.
                            .when(rule, |pill| {
                                pill.child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .h(px(1.))
                                        .opacity(faded)
                                        .bg(rgba((color.unwrap_or(theme.tokens.muted) << 8) | 0x61)),
                                )
                            })
                            // The dot keeps its place in the line (an empty slot) and is drawn over it, so that it can slide
                            // to the column's center when compact.
                            .child(div().flex_none().w(theme.ui(PILL_WIDTH)).h(theme.ui(8.)))
                            .child(
                                svg()
                                    .path(if *expanded { CHEVRON_DOWN_ICON } else { CHEVRON_RIGHT_ICON })
                                    .flex_none()
                                    .size(theme.ui(14.))
                                    .opacity(faded)
                                    .text_color(rgb(theme.tokens.muted)),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .right(theme.ui(26. - (PILL_WIDTH - 8.) * amount))
                                    .top(relative(0.5))
                                    .mt(theme.ui(-4.))
                                    .w(theme.ui(PILL_WIDTH))
                                    .h(theme.ui(8.))
                                    .rounded(px(3.))
                                    .bg(rgb(dot))
                                    .when(glow, |pill| {
                                        pill.child(pill_sweep(("pill-sweep", index), theme.ui(3.), band))
                                    }),
                            ),
                    )
            }
            RowKind::Session { session, tone, note, .. } => {
                let colors = theme.tone(*tone);
                let badge = dir_badge_for(
                    cx.global::<SettingsStore>().settings(),
                    session.claude_dir.as_deref(),
                    &session.host,
                )
                .map(|badge| dir_badge(&badge, &theme));
                let close = self.close_slot(index, badge, &theme, cx).filter(|_| close_slot_shown(amount));
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
                let (container, remote) = (session.container.is_some(), !session.host.is_local());
                let letter = tile_letter(session_title(session)).to_string();
                base.tooltip(hover_card::tooltip(card)).child(
                    lined(pill(colors.fill))
                        .when(sweeping(*tone, self.animate, true), |pill| {
                            pill.relative().child(working_sweep(("sweep-row", index), px(RADIUS), &theme))
                        })
                        .text_color(rgb(colors.ink))
                        .child(
                            line_content(faded)
                                .children(lead_mark(*tone, &colors, &theme))
                                .child(
                                    title(session_title(session).to_string())
                                        .when(*tone == Tone::NeedsInput, |title| {
                                            title.font_weight(FontWeight::SEMIBOLD)
                                        }),
                                )
                                .when(container, |line| line.child(container_icon(colors.ink, &theme)))
                                .when(remote, |line| line.child(ssh_icon(colors.ink, &theme)))
                                .children(close)
                                .children(project()),
                        )
                        .when(amount > 0.0, |pill| {
                            pill.child(tile_glyph(letter.into_any_element(), amount, &theme)).child(corner_marks(
                                lead_mark(*tone, &colors, &theme),
                                container,
                                remote,
                                colors.ink,
                                amount,
                                &theme,
                            ))
                        }),
                )
            }
            RowKind::Pane { entry, tone, note } if !entry.claude => {
                let close = self.close_slot(index, None, &theme, cx).filter(|_| close_slot_shown(amount));
                base.tooltip(hover_card::tooltip(HoverCard {
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
                        .child(
                            line_content(faded)
                                .child(div().flex_none().text_xs().text_color(rgb(theme.tokens.muted)).child(">_"))
                                .child(
                                    self.rename_field(RenameTarget::Pane(entry.leaf), &theme, cx)
                                        .unwrap_or_else(|| title(entry.title.clone())),
                                )
                                .when(!entry.host.is_local(), |line| line.child(ssh_icon(theme.tokens.muted, &theme)))
                                .children(project())
                                .children(close),
                        )
                        .when(amount > 0.0, |pill| {
                            pill.child(tile_glyph(">_".into_any_element(), amount, &theme)).child(corner_marks(
                                None,
                                false,
                                !entry.host.is_local(),
                                theme.tokens.muted,
                                amount,
                                &theme,
                            ))
                        }),
                )
            }
            RowKind::Pane { entry, tone, note } => {
                let colors = theme.tone(*tone);
                let close = self.close_slot(index, None, &theme, cx).filter(|_| close_slot_shown(amount));
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
                        .child(
                            line_content(faded)
                                .children(lead_mark(*tone, &colors, &theme))
                                .child(title("New session".into()))
                                .when(!entry.host.is_local(), |line| line.child(ssh_icon(colors.ink, &theme)))
                                .children(project())
                                .children(close),
                        )
                        .when(amount > 0.0, |pill| {
                            pill.child(tile_glyph("N".into_any_element(), amount, &theme)).child(corner_marks(
                                lead_mark(*tone, &colors, &theme),
                                false,
                                !entry.host.is_local(),
                                colors.ink,
                                amount,
                                &theme,
                            ))
                        }),
                )
            }
            RowKind::Tab { title: tab_title, panes, index: tab, first, .. } => {
                let close = self.close_slot(index, None, &theme, cx).filter(|_| close_slot_shown(amount));
                base.on_drag(
                    DraggedTab { first: *first, title: tab_title.clone() },
                    |dragged: &DraggedTab, _offset, _window, cx: &mut App| cx.new(|_| dragged.clone()),
                )
                .when(amount >= 1.0, |base| {
                    base.tooltip(hover_card::tooltip(HoverCard {
                        title: tab_title.clone(),
                        status: Some(format!("Split tab · {panes} panes")),
                        dir: None,
                        container: false,
                        host: None,
                        folder: None,
                    }))
                })
                .child(
                    lined(pill(None))
                        .h(theme.ui(28.))
                        .child(
                            line_content(faded)
                                .child(
                                    self.rename_field(RenameTarget::Tab(*tab), &theme, cx)
                                        .unwrap_or_else(|| title(tab_title.clone()))
                                        .font_weight(FontWeight::SEMIBOLD),
                                )
                                .child(aside(format!("{panes} panes"), theme.tokens.muted))
                                .children(close),
                        )
                        .when(amount > 0.0, |pill| {
                            // The pane count in a badge that fills with the accent as the column closes.
                            let badge = div()
                                .flex_none()
                                .size(theme.ui(22.))
                                .rounded(px(7.))
                                .border_1()
                                .border_color(rgb(theme.tokens.accent))
                                .bg(rgba(with_opacity(theme.tokens.accent, amount)))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(theme.ui(12.))
                                .text_color(rgb(if amount > 0.5 {
                                    theme.tokens.on_accent
                                } else {
                                    theme.tokens.accent
                                }))
                                .child(panes.to_string());
                            pill.child(tile_glyph(badge.into_any_element(), amount, &theme))
                        }),
                )
            }
            RowKind::Detail { text, .. } => base.child(
                div()
                    .pl(theme.ui(39.))
                    .overflow_hidden()
                    .text_xs()
                    .italic()
                    .text_color(rgb(theme.tokens.muted))
                    .child(text.clone()),
            ),
            RowKind::TakeOver { .. } => {
                base.child(pill(None).pl(theme.ui(37.)).text_color(rgb(theme.tokens.accent)).child("Take over…"))
            }
        }
    }
}

/// The space left of the fill of a row, inside its group.
fn row_inset(row: &Row) -> f32 {
    match (&row.kind, row.in_tab) {
        (RowKind::Project { .. }, _) => HEADING_INSET,
        (_, true) => TAB_INSET,
        _ => SESSION_INSET,
    }
}

/// The selection ring, drawn by the list after all rows (inside its clip) so it passes over the neighbouring rows.
struct SelectionRing {
    /// The selected row, its group edges, the space left of its fill (`HEADING_INSET`, `SESSION_INSET` or `TAB_INSET`)
    /// and the ring color.
    row: Option<(usize, GroupEdges, f32, u32)>,
    /// How compact the sidebar is drawn.
    amount: f32,
    /// The row is inside a split tab's box (compact tiles there are narrower).
    boxed: bool,
    /// The row is a pane of a split tab (its tile is a little lower when compact).
    shrinks: bool,
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
        let Some((index, edges, inset, color)) = self.row.filter(|(index, ..)| visible_range.contains(index)) else {
            return root.into_any_element();
        };
        let theme = &self.theme;
        let out = px(RING + RING_GAP);
        // The fill sits inside the row's side padding, the group's 1 px side border and its own padding, and under
        // the group's top gap and top/bottom border.
        let top = item_height * index + if edges.first { theme.ui(8.) + px(1.) } else { px(0.) };
        let bottom = item_height * (index + 1) - if edges.last { px(1.) } else { px(0.) };
        let (left, side) = ring_insets(inset, self.boxed, self.amount);
        let (left, side) = (theme.ui(left) - out, theme.ui(side) - out);
        let shrink = theme.ui(if self.shrinks { 2. * self.amount } else { 0. });
        let ring = div()
            .absolute()
            .left(left)
            .right(side)
            .top(top + px(ROW_PAD_Y) + shrink - out)
            .h(bottom - top - px(ROW_PAD_Y * 2.) - shrink * 2. + out * 2.)
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

/// A split tab's heading being dragged to a new place inside its project.
#[derive(Clone)]
struct DraggedTab {
    /// The first leaf of the tab's panes, which names the tab among its project's items.
    first: NodeId,
    title: String,
}

impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .px(theme.ui(12.))
            .py(theme.ui(6.))
            .rounded(px(RADIUS))
            .border_1()
            .border_color(rgb(theme.tokens.accent))
            .bg(rgb(theme.tokens.raised))
            .text_color(rgb(theme.tokens.text))
            .text_sm()
            .font_family(UI_FONT)
            .font_weight(FontWeight::SEMIBOLD)
            .child(self.title.clone())
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
        // The sessions list started to scroll (or fits again): the SSH hosts block may fold by itself.
        // Not while compact: the column's own bottom rows are taller, and a fold made then would still be there when
        // the sidebar expands.
        if follows_list_scroll(self.amount)
            && let Some(extent) = Extent::of(&self.scroll)
        {
            self.hosts_fold.observe(extent.contents.1 > extent.viewport.1 + 0.5);
        }
        let theme = theme(cx).clone();
        let amount = self.amount;
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
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(theme.ui(36.))
                    .ml(theme.ui(10.))
                    .mr_3()
                    .mt_3()
                    .mb_1()
                    .when(self.amount < 1.0, |row| {
                        row.child(
                            div()
                                .id("filter")
                                .h(theme.ui(36.))
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .opacity(1.0 - self.amount)
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
                    }),
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
                    div()
                        .relative()
                        .h(theme.ui(ROW_HEIGHT - 4.))
                        .flex_none()
                        .p(px(PILL_INSET))
                        .ml(theme.ui(12. - 8. * amount))
                        .mr(theme.ui(12. - 8. * amount))
                        .mb_1()
                        .children(ring)
                        .child(
                            div()
                                .id("settings-row")
                                .size_full()
                                .overflow_hidden()
                                .pl(theme.ui(12. + 3. * amount))
                                .pr(theme.ui(12. * (1. - amount)))
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_sm()
                                .rounded(px(RADIUS))
                                .bg(rgb(theme.tokens.text))
                                .text_color(rgb(theme.tokens.ground))
                                .cursor_pointer()
                                .when(amount >= 1.0, |row| {
                                    row.tooltip(hover_card::tooltip(HoverCard {
                                        title: "Settings".into(),
                                        status: None,
                                        dir: None,
                                        container: false,
                                        host: None,
                                        folder: None,
                                    }))
                                })
                                .child("⚙")
                                .child(div().flex_1().min_w_0().truncate().opacity(1. - amount).child("Settings"))
                                .children((amount < 1.0).then(|| {
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
                                        }))
                                }))
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
                        .p(px(PILL_INSET))
                        .ml(theme.ui(12. - 8. * amount))
                        .mr(theme.ui(12. - 8. * amount))
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
                                .overflow_hidden()
                                .pl(theme.ui(12. + 3. * amount))
                                .pr(theme.ui(12. * (1. - amount)))
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_sm()
                                .rounded(px(RADIUS))
                                .bg(rgb(theme.tokens.text))
                                .text_color(rgb(theme.tokens.ground))
                                .when(amount >= 1.0, |row| {
                                    row.tooltip(hover_card::tooltip(HoverCard {
                                        title: "Git".into(),
                                        status: Some(label.to_string()),
                                        dir: None,
                                        container: false,
                                        host: None,
                                        folder: None,
                                    }))
                                })
                                .child("⎇")
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .opacity(1. - amount)
                                        .child(format!("Git · {label}")),
                                )
                                .children((amount < 1.0).then(|| {
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
                                        }))
                                })),
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
                            cx.processor(|this, range: Range<usize>, window, cx| {
                                range.map(|index| this.render_row(index, window, cx)).collect::<Vec<_>>()
                            }),
                        )
                        .with_decoration(SelectionRing {
                            row: self.selected_index().map(|index| {
                                let edges = session_tree::group_edges(&self.rows, index);
                                (index, edges, row_inset(&self.rows[index]), theme.selection())
                            }),
                            amount,
                            boxed: self
                                .selected_index()
                                .is_some_and(|index| session_tree::tab_group_edges(&self.rows, index).is_some()),
                            shrinks: self.selected_index().is_some_and(|index| self.rows[index].in_tab),
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
                    .relative()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .h(theme.ui(34.))
                    .mx(theme.ui(lerp(12., 6., amount)))
                    .mt_2()
                    .px(theme.ui(12. * (1. - amount)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(px(RADIUS))
                    .border_1()
                    .border_color(rgba(with_opacity(theme.tokens.line, 1. - amount)))
                    .text_xs()
                    .text_color(rgb(theme.tokens.muted))
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgb(theme.tokens.text)))
                    .child(div().flex_1().opacity(1. - amount).child("Recent sessions"))
                    .child(
                        div()
                            .px(px(6.))
                            .py(px(2.))
                            .rounded(px(6.))
                            .bg(rgb(theme.tokens.raised))
                            .opacity(1. - amount)
                            .child(shortcut_label(cx.global::<SettingsStore>().settings(), "openRecents")),
                    )
                    .when(amount > 0.0, |row| row.child(tile_glyph("↺".into_any_element(), amount, &theme)))
                    .when(amount >= 1.0, |row| {
                        row.tooltip(hover_card::tooltip(HoverCard {
                            title: "Recent sessions".into(),
                            status: Some(shortcut_label(cx.global::<SettingsStore>().settings(), "openRecents")),
                            dir: None,
                            container: false,
                            host: None,
                            folder: None,
                        }))
                    })
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenRecents))),
            )
            .child(
                div()
                    .flex_none()
                    .px(theme.ui(lerp(12., 6., amount)))
                    .py_3()
                    .flex()
                    .flex_col()
                    .text_sm()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(theme.ui(8. * (1. - amount)))
                            .child(
                                div()
                                    .id("new-claude")
                                    .relative()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .flex_1()
                                    .min_w_0()
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
                                    .child(div().opacity(1. - amount).child("New Claude session"))
                                    .when(amount > 0.0, |button| {
                                        button.child(tile_glyph("+".into_any_element(), amount, &theme))
                                    })
                                    .when(amount >= 1.0, |button| {
                                        button.tooltip(hover_card::tooltip(HoverCard {
                                            title: "New Claude session".into(),
                                            status: Some(shortcut_label(
                                                cx.global::<SettingsStore>().settings(),
                                                "openLauncher",
                                            )),
                                            dir: None,
                                            container: false,
                                            host: None,
                                            folder: None,
                                        }))
                                    })
                                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                        cx.emit(SidebarEvent::New { claude: true })
                                    })),
                            )
                            // Beside "New Claude session" when expanded; the compact column puts it below instead.
                            .child(self.new_terminal_button("new-shell", 1. - amount, &theme, cx)),
                    )
                    .when(amount > 0.0, |column| {
                        column.child(
                            div()
                                .h(theme.ui(36. * amount + 6. * amount))
                                .pt(theme.ui(6. * amount))
                                .overflow_hidden()
                                .opacity(amount)
                                .child(self.new_terminal_button("new-shell-compact", amount, &theme, cx).w_full()),
                        )
                    }),
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

/// Whether a row has its close button (and config badge) at this compact `amount`: not on a compact tile, where
/// they are invisible and must not take clicks.
fn close_slot_shown(amount: f32) -> bool {
    amount <= 0.5
}

/// Whether the sessions list's scrolling may fold the SSH hosts card: only while the sidebar is fully expanded.
fn follows_list_scroll(amount: f32) -> bool {
    amount == 0.0
}

/// `from` at `amount` 0, `to` at 1.
fn lerp(from: f32, to: f32, amount: f32) -> f32 {
    from + (to - from) * amount
}

/// `color` (`0xRRGGBB`) at `opacity` (0..1) as `rgba` bits.
fn with_opacity(color: u32, opacity: f32) -> u32 {
    (color << 8) | (opacity.clamp(0., 1.) * 255.).round() as u32
}

/// How far in, from the list's left and right edges, the selection ring starts for a row whose fill is `inset` from
/// its group when expanded, with the sidebar `amount` compact; `boxed` = the row is inside a split tab's box (its tile
/// is narrower). In `theme.ui` units, before the ring's own width and gap are taken off.
fn ring_insets(inset: f32, boxed: bool, amount: f32) -> (f32, f32) {
    let compact = if boxed { 7.0 } else { 4.0 };
    (lerp(7.0 + inset, compact, amount), lerp(17.0, compact, amount))
}

/// The marks a compact tile shows in its corners: the unread pin, closed ring or ↗ top right, the container and SSH
/// icons top left. They fade in with `amount`.
fn corner_marks(
    lead: Option<gpui::Div>,
    container: bool,
    ssh: bool,
    ink: u32,
    amount: f32,
    theme: &Theme,
) -> gpui::Div {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
        .opacity(amount)
        .children(lead.map(|mark| mark.absolute().top(theme.ui(2.)).right(theme.ui(4.))))
        .child(
            div()
                .absolute()
                .top(theme.ui(2.))
                .left(theme.ui(12.))
                .flex()
                .flex_row()
                .gap(px(2.))
                .when(container, |icons| icons.child(container_icon(ink, theme).size(theme.ui(10.))))
                .when(ssh, |icons| icons.child(ssh_icon(ink, theme).size(theme.ui(10.)))),
        )
}

/// What a compact tile shows in its middle, centered: the title's letter, `>_` or the pane count. It fades in with
/// `amount`.
fn tile_glyph(content: AnyElement, amount: f32, theme: &Theme) -> gpui::Div {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
        .flex()
        .items_center()
        .justify_center()
        .opacity(amount)
        .font_weight(FontWeight::BOLD)
        .text_size(theme.ui(15.))
        .child(content)
}

/// The filter text that applies when the sidebar is `amount` compact: none once it is past half way, the text itself
/// is kept for when it expands again.
fn applied_filter(text: &str, amount: f32) -> &str {
    if amount > 0.5 { "" } else { text }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_tiles_have_no_close_slot_and_the_list_does_not_feed_the_hosts_fold() {
        assert!(close_slot_shown(0.0));
        assert!(close_slot_shown(0.5));
        assert!(!close_slot_shown(0.6));
        assert!(!close_slot_shown(1.0));
        assert!(follows_list_scroll(0.0));
        assert!(!follows_list_scroll(0.01));
        assert!(!follows_list_scroll(1.0));
    }

    #[test]
    fn ring_insets_follow_the_amount() {
        // Expanded: the existing values (group padding + border + the row's inset on the left, 17 on the right).
        assert_eq!(ring_insets(SESSION_INSET, false, 0.0), (22.0, 17.0));
        assert_eq!(ring_insets(TAB_INSET, true, 0.0), (44.0, 17.0));
        // Compact: 6 px from the column's edge (9 px inside a split tab's box), less the row's own `PILL_INSET`.
        assert_eq!(ring_insets(SESSION_INSET, false, 1.0), (4.0, 4.0));
        assert_eq!(ring_insets(TAB_INSET, true, 1.0), (7.0, 7.0));
        assert_eq!(ring_insets(SESSION_INSET, false, 0.5), (13.0, 10.5));
    }

    #[test]
    fn colors_fade_with_their_opacity() {
        assert_eq!(with_opacity(0x112233, 1.0), 0x112233ff);
        assert_eq!(with_opacity(0x112233, 0.0), 0x11223300);
        assert_eq!(with_opacity(0x112233, 0.5), 0x11223380);
        assert_eq!(lerp(10.0, 3.0, 0.5), 6.5);
    }

    #[test]
    fn the_filter_is_ignored_while_compact_and_kept_for_later() {
        assert_eq!(applied_filter("abc", 0.0), "abc");
        assert_eq!(applied_filter("abc", 0.5), "abc");
        assert_eq!(applied_filter("abc", 0.6), "");
        assert_eq!(applied_filter("abc", 1.0), "");
    }
}
