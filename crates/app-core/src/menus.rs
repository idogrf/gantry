//! Right-click menus as plain items: what a tab, a sidebar row and a project heading offer, and the VS Code
//! command line for a folder (inside its dev container when it has one).

use std::path::{Path, PathBuf};

use protocol::host::{HostId, split_project_key};
use protocol::message::{ContainerPlace, SessionState};

use crate::session_tree::{Row, RowKind, Tone};
use crate::settings::Settings;
use crate::split_tree::{Axis, NodeId};
use crate::theme_colors::ProjectColor;

/// A shortcut whose hint a menu entry shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    /// Split the tab right.
    SplitRight,
    /// Split the tab down.
    SplitDown,
    /// Split the tab right with a shell.
    SplitRightShell,
    /// Split the tab down with a shell.
    SplitDownShell,
    /// Close the pane or tab.
    Close,
    /// Recent sessions pop-up.
    Recents,
}

/// What picking a menu entry does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// Split tab `tab` along `axis`.
    Split {
        /// The tab index.
        tab: usize,
        /// The split direction.
        axis: Axis,
    },
    /// Split the pane `leaf` along `axis`, with Claude (else a shell).
    SplitPane {
        /// The pane to split.
        leaf: NodeId,
        /// The split direction.
        axis: Axis,
        /// Claude (else a shell).
        claude: bool,
    },
    /// Copy text to the clipboard.
    Copy(String),
    /// Show a folder in the system file manager.
    OpenFolder(PathBuf),
    /// Run `code` with these arguments.
    OpenInVsCode(Vec<String>),
    /// Mark a pane's session as having a result the user has not seen (its tab and row turn Done).
    MarkUnread(NodeId),
    /// Remove a pane's unread mark (its tab and row go back to the session's own color).
    MarkRead(NodeId),
    /// Rename a shell pane.
    RenamePane(NodeId),
    /// Rename a tab.
    RenameTab(usize),
    /// Close one pane.
    ClosePane(NodeId),
    /// Close every pane of a tab.
    CloseTab(usize),
    /// Start a new Claude session in a folder with the named config dir (and remember it for the folder).
    NewClaudeWith {
        /// The folder.
        dir: PathBuf,
        /// Name of the config dir.
        dir_name: String,
    },
    /// Pin a folder as a favorite.
    PinFavorite(PathBuf),
    /// Unpin a favorite folder.
    UnpinFavorite(PathBuf),
    /// Start a new Claude session or shell in a folder.
    NewHere {
        /// The folder (on `host`).
        dir: PathBuf,
        /// Claude (else a shell).
        claude: bool,
        /// The host the folder is on.
        host: HostId,
    },
    /// Open the recent sessions pop-up on a project.
    RecentsFor(PathBuf),
    /// Continue the session of an exited pane.
    Continue(NodeId),
    /// Set (`Some`) or clear (`None`, automatic) the color picked for a project.
    SetProjectColor {
        /// The project root.
        root: PathBuf,
        /// The color, or `None` for the automatic one.
        color: Option<ProjectColor>,
    },
    /// Open the sessions picked with Shift-click side by side, four to a tab.
    OpenTogether,
    /// Take over a session running outside Gantry.
    TakeOver {
        /// Session id.
        session: String,
        /// Its directory.
        cwd: PathBuf,
        /// The host it runs on.
        host: HostId,
    },
}

/// One pickable menu entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuEntry {
    /// Its text.
    pub label: String,
    /// The shortcut hint shown beside it.
    pub shortcut: Option<Shortcut>,
    /// Drawn in the accent color (the row's main action).
    pub accent: bool,
    /// What picking it does.
    pub action: MenuAction,
    /// Drawn as a color swatch (in a grid) instead of a text row.
    pub swatch: Option<ProjectColor>,
    /// Marked as the current choice.
    pub checked: bool,
}

/// A menu line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuItem {
    /// A pickable entry.
    Entry(MenuEntry),
    /// An entry that opens a list of entries beside it.
    Submenu {
        /// Its text.
        label: String,
        /// The entries.
        items: Vec<MenuItem>,
    },
    /// A line between groups.
    Separator,
}

/// What the menus need to know beyond the row: the Claude config dirs and whether the folder is a favorite.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuContext {
    /// Each config dir as (name, is the default, remembered for this folder). Fewer than two: no choice is offered.
    pub dirs: Vec<(String, bool, bool)>,
    /// Whether the folder is pinned as a favorite.
    pub favorite: bool,
    /// The project's color: `None` = project colors are off (no Color menu), `Some(None)` = automatic,
    /// `Some(Some(c))` = picked.
    pub project_color: Option<Option<ProjectColor>>,
}

impl MenuContext {
    /// The context for `folder` from the settings: the configured dirs (the default one marked, the one remembered
    /// for exactly this folder marked) and whether the folder is a favorite.
    pub fn for_folder(settings: &Settings, folder: &Path) -> MenuContext {
        let default = settings
            .default_claude_dir
            .as_deref()
            .filter(|name| settings.claude_dirs.iter().any(|dir| dir.name == *name))
            .or_else(|| settings.claude_dirs.first().map(|dir| dir.name.as_str()));
        let remembered = settings.folder_claude_dirs.get(folder);
        MenuContext {
            dirs: settings
                .claude_dirs
                .iter()
                .map(|dir| (dir.name.clone(), Some(dir.name.as_str()) == default, remembered == Some(&dir.name)))
                .collect(),
            favorite: settings.favorites.iter().any(|favorite| favorite == folder),
            project_color: settings
                .project_colors
                .enabled
                .then(|| settings.project_colors.folders.get(folder).copied()),
        }
    }
}

/// The menu of a row among several picked with Shift-click.
pub fn together_menu(count: usize) -> Vec<MenuItem> {
    vec![entry(&format!("Open {count} together"), MenuAction::OpenTogether)]
}

fn entry(label: &str, action: MenuAction) -> MenuItem {
    MenuItem::Entry(MenuEntry {
        label: label.into(),
        shortcut: None,
        accent: false,
        action,
        swatch: None,
        checked: false,
    })
}

fn with_shortcut(label: &str, shortcut: Shortcut, action: MenuAction) -> MenuItem {
    MenuItem::Entry(MenuEntry {
        label: label.into(),
        shortcut: Some(shortcut),
        accent: false,
        action,
        swatch: None,
        checked: false,
    })
}

fn accented(label: &str, action: MenuAction) -> MenuItem {
    MenuItem::Entry(MenuEntry {
        label: label.into(),
        shortcut: None,
        accent: true,
        action,
        swatch: None,
        checked: false,
    })
}

/// "Color ▸": the eight swatches, then "Automatic color". `current` is the project's pick (`None` = automatic).
fn color_menu(root: &Path, current: Option<ProjectColor>) -> MenuItem {
    let mut items: Vec<MenuItem> = ProjectColor::ALL
        .into_iter()
        .map(|color| {
            let name = color.name();
            let label = format!("{}{}", name[..1].to_uppercase(), &name[1..]);
            let action = MenuAction::SetProjectColor { root: root.to_path_buf(), color: Some(color) };
            MenuItem::Entry(MenuEntry {
                label,
                shortcut: None,
                accent: false,
                action,
                swatch: Some(color),
                checked: current == Some(color),
            })
        })
        .collect();
    items.push(MenuItem::Entry(MenuEntry {
        label: "Automatic color".into(),
        shortcut: None,
        accent: false,
        action: MenuAction::SetProjectColor { root: root.to_path_buf(), color: None },
        swatch: None,
        checked: current.is_none(),
    }));
    MenuItem::Submenu { label: "Color".into(), items }
}

/// "New Claude here with ▸" and the config dirs, when there are two or more.
fn claude_with(dir: &Path, context: &MenuContext) -> Option<MenuItem> {
    if context.dirs.len() < 2 {
        return None;
    }
    let items = context
        .dirs
        .iter()
        .map(|(name, default, remembered)| {
            let mut label = name.clone();
            if *default {
                label.push_str(" (default)");
            }
            if *remembered {
                label.push_str(" ✓ this folder");
            }
            entry(&label, MenuAction::NewClaudeWith { dir: dir.to_path_buf(), dir_name: name.clone() })
        })
        .collect();
    Some(MenuItem::Submenu { label: "New Claude here with".into(), items })
}

/// Copy folder path · Open folder · Open in VS Code; on a remote host only "Copy folder path (on <host>)".
fn folder_items(dir: &Path, vscode: Vec<String>, host: &HostId) -> Vec<MenuItem> {
    if let Some(name) = host.remote_name() {
        return vec![entry(&format!("Copy folder path (on {name})"), MenuAction::Copy(dir.display().to_string()))];
    }
    vec![
        entry("Copy folder path", MenuAction::Copy(dir.display().to_string())),
        entry("Open folder", MenuAction::OpenFolder(dir.to_path_buf())),
        entry("Open in VS Code", MenuAction::OpenInVsCode(vscode)),
    ]
}

/// "New Claude here with ▸" for a folder on this machine; remote hosts use their own Claude config.
fn claude_with_on(dir: &Path, context: &MenuContext, host: &HostId) -> Option<MenuItem> {
    host.is_local().then(|| claude_with(dir, context)).flatten()
}

/// Drop separators that lead, trail or follow another separator.
fn tidy(items: Vec<MenuItem>) -> Vec<MenuItem> {
    let mut tidy: Vec<MenuItem> = Vec::with_capacity(items.len());
    for item in items {
        if matches!(item, MenuItem::Separator) && tidy.last().is_none_or(|last| matches!(last, MenuItem::Separator)) {
            continue;
        }
        tidy.push(item);
    }
    if matches!(tidy.last(), Some(MenuItem::Separator)) {
        tidy.pop();
    }
    tidy
}

/// The arguments for `code`: inside the dev container when there is one (the session's own path there, else VS Code's
/// default `/workspaces/<name>`), else the folder.
pub fn vscode_args(folder: &Path, container: Option<&ContainerPlace>, pane_container: Option<&Path>) -> Vec<String> {
    let target = match (container, pane_container) {
        (Some(ContainerPlace { host_folder: Some(host), path }), _) => Some((host.as_path(), path.clone())),
        (_, Some(host)) => {
            let name = host.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            // A path inside the (Linux) container: always `/`, whatever the host's separator.
            Some((host, PathBuf::from(format!("/workspaces/{name}"))))
        }
        _ => None,
    };
    match target {
        Some((host, path)) => {
            let hex: String = host.to_string_lossy().bytes().map(|byte| format!("{byte:02x}")).collect();
            vec!["--folder-uri".into(), format!("vscode-remote://dev-container+{hex}{}", path.display())]
        }
        None => vec![folder.display().to_string()],
    }
}

/// A tab's menu; `dir` is its focused pane's directory. `mark` = the focused pane and whether it is unread (offers
/// "Mark as read"), `None` when it is neither unread nor idle.
/// `panes` = how many panes the tab has; only a split tab (its heading) can be renamed.
/// `context.dirs` is empty for a dev container tab. `host` = the host the focused pane runs on.
pub fn tab_menu(
    tab: usize,
    host: &HostId,
    dir: &Path,
    vscode: Vec<String>,
    context: &MenuContext,
    mark: Option<(NodeId, bool)>,
    panes: usize,
) -> Vec<MenuItem> {
    let mut items = vec![
        with_shortcut("Split right", Shortcut::SplitRight, MenuAction::Split { tab, axis: Axis::Horizontal }),
        with_shortcut("Split down", Shortcut::SplitDown, MenuAction::Split { tab, axis: Axis::Vertical }),
    ];
    items.extend(claude_with_on(dir, context, host));
    if let Some((leaf, unread)) = mark {
        items.push(read_mark_item(leaf, unread));
    }
    items.push(MenuItem::Separator);
    items.extend(folder_items(dir, vscode, host));
    items.push(MenuItem::Separator);
    if panes > 1 {
        items.push(entry("Rename…", MenuAction::RenameTab(tab)));
    }
    items.push(with_shortcut("Close tab", Shortcut::Close, MenuAction::CloseTab(tab)));
    tidy(items)
}

/// The popup of a pane's split button: what the new pane runs, Claude or a terminal.
pub fn split_menu(leaf: NodeId, axis: Axis) -> Vec<MenuItem> {
    let (claude, shell) = match axis {
        Axis::Horizontal => (Shortcut::SplitRight, Shortcut::SplitRightShell),
        Axis::Vertical => (Shortcut::SplitDown, Shortcut::SplitDownShell),
    };
    vec![
        with_shortcut("Claude", claude, MenuAction::SplitPane { leaf, axis, claude: true }),
        with_shortcut("Terminal", shell, MenuAction::SplitPane { leaf, axis, claude: false }),
    ]
}

/// What a middle click on a sidebar row does: a split tab's heading closes the tab, a pane's row closes the pane
/// (and its tab with its last pane); rows without a pane do nothing.
pub fn middle_click(row: &Row) -> Option<MenuAction> {
    match &row.kind {
        RowKind::Tab { index, .. } => Some(MenuAction::CloseTab(*index)),
        RowKind::Session { leaf: Some(leaf), .. } => Some(MenuAction::ClosePane(*leaf)),
        RowKind::Pane { entry, .. } => Some(MenuAction::ClosePane(entry.leaf)),
        _ => None,
    }
}

/// "Mark as read" for an unread pane, "Mark as unread" for an idle one.
fn read_mark_item(leaf: NodeId, unread: bool) -> MenuItem {
    if unread {
        entry("Mark as read", MenuAction::MarkRead(leaf))
    } else {
        entry("Mark as unread", MenuAction::MarkUnread(leaf))
    }
}

fn close_item(leaf: NodeId, alone: &dyn Fn(NodeId) -> bool) -> MenuItem {
    let label = if alone(leaf) { "Close tab" } else { "Close pane" };
    with_shortcut(label, Shortcut::Close, MenuAction::ClosePane(leaf))
}

/// A sidebar row's menu; `None` for rows without one (headings use [`project_menu`], details and "Take over…" rows
/// have none). `alone(leaf)` = that leaf's tab has no other pane.
pub fn row_menu(row: &Row, alone: &dyn Fn(NodeId) -> bool, context: &MenuContext) -> Option<Vec<MenuItem>> {
    let mut items = Vec::new();
    match &row.kind {
        RowKind::Session { session, tone, leaf, .. } => {
            let vscode = vscode_args(&session.cwd, session.container.as_ref(), None);
            match (session.state, leaf) {
                (SessionState::External { pid }, _) => {
                    if pid.is_some() && session.cwd_exists {
                        items.push(accented(
                            "Take over…",
                            MenuAction::TakeOver {
                                session: session.id.clone(),
                                cwd: session.cwd.clone(),
                                host: session.host.clone(),
                            },
                        ));
                        items.push(MenuItem::Separator);
                    }
                }
                (SessionState::Closed, Some(leaf)) if *tone == Tone::Closed => {
                    items.push(accented("Continue", MenuAction::Continue(*leaf)));
                    items.push(MenuItem::Separator);
                }
                _ => {
                    items.push(entry(
                        "New Claude here",
                        MenuAction::NewHere { dir: session.cwd.clone(), claude: true, host: session.host.clone() },
                    ));
                    if session.container.is_none() {
                        items.extend(claude_with_on(&session.cwd, context, &session.host));
                    }
                    items.push(entry(
                        "New terminal here",
                        MenuAction::NewHere { dir: session.cwd.clone(), claude: false, host: session.host.clone() },
                    ));
                    items.push(MenuItem::Separator);
                }
            }
            if let (Tone::Idle | Tone::Done | Tone::Unread, Some(leaf)) = (tone, leaf) {
                items.push(read_mark_item(*leaf, *tone != Tone::Idle));
            }
            items.push(entry("Copy session id", MenuAction::Copy(session.id.clone())));
            items.extend(folder_items(&session.cwd, vscode, &session.host));
            if let Some(leaf) = leaf {
                items.push(MenuItem::Separator);
                items.push(close_item(*leaf, alone));
            }
        }
        RowKind::Pane { entry: pane, .. } => {
            let vscode = vscode_args(&pane.cwd, None, pane.container.as_deref());
            match (pane.claude, pane.ended) {
                (true, true) => items.push(accented("Continue", MenuAction::Continue(pane.leaf))),
                (claude, _) => {
                    let label = if claude { "New Claude here" } else { "New terminal here" };
                    items.push(entry(
                        label,
                        MenuAction::NewHere { dir: pane.cwd.clone(), claude, host: pane.host.clone() },
                    ));
                    if claude && pane.container.is_none() {
                        items.extend(claude_with_on(&pane.cwd, context, &pane.host));
                    }
                    if claude {
                        items.push(entry(
                            "New terminal here",
                            MenuAction::NewHere { dir: pane.cwd.clone(), claude: false, host: pane.host.clone() },
                        ));
                    }
                    if !claude {
                        items.push(entry("Rename…", MenuAction::RenamePane(pane.leaf)));
                    }
                }
            }
            items.push(MenuItem::Separator);
            if let Some(id) = &pane.session {
                items.push(entry("Copy session id", MenuAction::Copy(id.clone())));
            }
            items.extend(folder_items(&pane.cwd, vscode, &pane.host));
            items.push(MenuItem::Separator);
            items.push(close_item(pane.leaf, alone));
        }
        RowKind::Project { .. } | RowKind::Tab { .. } | RowKind::Detail { .. } | RowKind::TakeOver { .. } => {
            return None;
        }
    }
    Some(tidy(items))
}

/// A project heading's menu; `path` = the project key (see `protocol::host::project_key`), `container` = the dev
/// container one of its sessions runs in, if any. A remote project has a pin (saved under its key) but no color or
/// local folder entries.
pub fn project_menu(path: &Path, container: Option<&ContainerPlace>, context: &MenuContext) -> Vec<MenuItem> {
    let (host, dir) = split_project_key(path);
    let new_here = |claude| MenuAction::NewHere { dir: dir.clone(), claude, host: host.clone() };
    let mut items = vec![entry("New Claude here", new_here(true))];
    if container.is_none() {
        items.extend(claude_with_on(path, context, &host));
    }
    items.push(entry("New terminal here", new_here(false)));
    items.push(if context.favorite {
        entry("Unpin", MenuAction::UnpinFavorite(path.to_path_buf()))
    } else {
        entry("Pin as favorite", MenuAction::PinFavorite(path.to_path_buf()))
    });
    if let Some(current) = context.project_color.filter(|_| host.is_local()) {
        items.push(color_menu(path, current));
    }
    items.extend([
        with_shortcut("Recent sessions here", Shortcut::Recents, MenuAction::RecentsFor(path.to_path_buf())),
        MenuItem::Separator,
    ]);
    items.extend(folder_items(&dir, vscode_args(&dir, container, None), &host));
    tidy(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_tree::{PaneEntry, Row, RowKind, Tone};
    use protocol::message::{SessionInfo, SessionState};

    fn labels(items: &[MenuItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                MenuItem::Entry(entry) => entry.label.clone(),
                MenuItem::Submenu { label, .. } => format!("{label} ▸"),
                MenuItem::Separator => "│".into(),
            })
            .collect()
    }

    fn two_dirs(remembered: Option<usize>) -> MenuContext {
        let dirs = ["Personal", "Work"]
            .iter()
            .enumerate()
            .map(|(n, name)| (name.to_string(), n == 0, remembered == Some(n)))
            .collect();
        MenuContext { dirs, favorite: false, project_color: None }
    }

    fn submenu_labels(items: &[MenuItem]) -> Vec<String> {
        let Some(MenuItem::Submenu { items, .. }) = items.iter().find(|item| matches!(item, MenuItem::Submenu { .. }))
        else {
            return vec![];
        };
        labels(items)
    }

    fn session(state: SessionState) -> SessionInfo {
        SessionInfo {
            id: "s1".into(),
            cwd: "/r/app".into(),
            title: "t".into(),
            last_active_ms: 0,
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

    fn session_row(state: SessionState, tone: Tone, leaf: Option<NodeId>) -> Row {
        Row::new(RowKind::Session { session: session(state), tone, note: String::new(), leaf })
    }

    fn pane_row(claude: bool, ended: bool) -> Row {
        let entry = PaneEntry {
            leaf: 7,
            host: protocol::host::HostId::local(),
            cwd: "/r/app".into(),
            title: "p".into(),
            session: claude.then(|| "new".to_string()),
            claude,
            ended,
            offline: false,
            alone: true,
            container: None,
        };
        Row::new(RowKind::Pane { entry, tone: if ended { Tone::Closed } else { Tone::Idle }, note: String::new() })
    }

    const ALONE: &dyn Fn(NodeId) -> bool = &|_| true;

    #[test]
    fn a_middle_click_closes_the_pane_or_tab_a_row_stands_for() {
        let running = SessionState::Managed { pane: 1 };
        assert_eq!(middle_click(&session_row(running, Tone::Working, Some(4))), Some(MenuAction::ClosePane(4)));
        assert_eq!(middle_click(&pane_row(false, false)), Some(MenuAction::ClosePane(7)));
        let heading = Row::new(RowKind::Tab { index: 2, title: "t".into(), panes: 2, focused: 4, first: 3 });
        assert_eq!(middle_click(&heading), Some(MenuAction::CloseTab(2)));
        assert_eq!(
            middle_click(&session_row(SessionState::External { pid: Some(9) }, Tone::Elsewhere, None)),
            None,
            "not in Gantry"
        );
        assert_eq!(
            middle_click(&Row::new(RowKind::Project { path: "/r/app".into(), label: "app".into(), expanded: true })),
            None
        );
    }

    #[test]
    fn a_split_button_offers_claude_then_a_terminal() {
        let entries: Vec<_> = split_menu(7, Axis::Vertical)
            .into_iter()
            .map(|item| {
                let MenuItem::Entry(entry) = item else { panic!("only entries") };
                (entry.label, entry.action, entry.shortcut)
            })
            .collect();
        assert_eq!(
            entries,
            [
                (
                    "Claude".to_string(),
                    MenuAction::SplitPane { leaf: 7, axis: Axis::Vertical, claude: true },
                    Some(Shortcut::SplitDown)
                ),
                (
                    "Terminal".to_string(),
                    MenuAction::SplitPane { leaf: 7, axis: Axis::Vertical, claude: false },
                    Some(Shortcut::SplitDownShell)
                ),
            ]
        );
        let MenuItem::Entry(terminal) = &split_menu(7, Axis::Horizontal)[1] else { panic!() };
        assert_eq!(terminal.shortcut, Some(Shortcut::SplitRightShell));
    }

    #[test]
    fn a_tab_offers_splits_folder_items_and_close() {
        let items =
            tab_menu(2, &HostId::local(), Path::new("/r/app"), vec!["/r/app".into()], &MenuContext::default(), None, 2);
        assert_eq!(
            labels(&items),
            [
                "Split right",
                "Split down",
                "│",
                "Copy folder path",
                "Open folder",
                "Open in VS Code",
                "│",
                "Rename…",
                "Close tab"
            ]
        );
        let MenuItem::Entry(first) = &items[0] else { panic!() };
        assert_eq!(
            (first.action.clone(), first.shortcut),
            (MenuAction::Split { tab: 2, axis: Axis::Horizontal }, Some(Shortcut::SplitRight))
        );
        let MenuItem::Entry(last) = items.last().unwrap() else { panic!() };
        assert_eq!(last.action, MenuAction::CloseTab(2));
    }

    #[test]
    fn only_shell_panes_and_tabs_offer_rename() {
        let rename = |items: &[MenuItem]| {
            items.iter().find_map(|item| match item {
                MenuItem::Entry(entry) if entry.label == "Rename…" => Some(entry.action.clone()),
                _ => None,
            })
        };
        let menus_row_menu = |row: &Row| row_menu(row, ALONE, &MenuContext::default()).unwrap();
        let shell = menus_row_menu(&pane_row(false, false));
        assert_eq!(rename(&shell), Some(MenuAction::RenamePane(7)));
        assert_eq!(rename(&menus_row_menu(&pane_row(true, false))), None);
        assert_eq!(rename(&menus_row_menu(&pane_row(true, true))), None);
        let running = SessionState::Managed { pane: 1 };
        assert_eq!(rename(&menus_row_menu(&session_row(running, Tone::Idle, Some(4)))), None);
        let items = tab_menu(3, &HostId::local(), Path::new("/r/app"), vec![], &MenuContext::default(), None, 2);
        assert_eq!(rename(&items), Some(MenuAction::RenameTab(3)));
        let single = tab_menu(3, &HostId::local(), Path::new("/r/app"), vec![], &MenuContext::default(), None, 1);
        assert_eq!(rename(&single), None, "a one-pane tab has no heading to rename");
    }

    #[test]
    fn a_tab_with_an_idle_claude_offers_mark_as_unread() {
        let items =
            tab_menu(0, &HostId::local(), Path::new("/r/app"), vec![], &MenuContext::default(), Some((7, false)), 2);
        assert_eq!(
            labels(&items),
            [
                "Split right",
                "Split down",
                "Mark as unread",
                "│",
                "Copy folder path",
                "Open folder",
                "Open in VS Code",
                "│",
                "Rename…",
                "Close tab"
            ]
        );
        let MenuItem::Entry(unread) = &items[2] else { panic!() };
        assert_eq!(unread.action, MenuAction::MarkUnread(7));
    }

    #[test]
    fn an_unread_tab_offers_mark_as_read() {
        let items =
            tab_menu(0, &HostId::local(), Path::new("/r/app"), vec![], &MenuContext::default(), Some((7, true)), 1);
        let MenuItem::Entry(read) = &items[2] else { panic!() };
        assert_eq!((read.label.as_str(), &read.action), ("Mark as read", &MenuAction::MarkRead(7)));
    }

    #[test]
    fn idle_session_rows_offer_mark_as_unread_and_done_ones_mark_as_read() {
        for (tone, label) in [
            (Tone::Idle, Some("Mark as unread")),
            (Tone::Done, Some("Mark as read")),
            (Tone::Unread, Some("Mark as read")),
            (Tone::Working, None),
            (Tone::NeedsInput, None),
        ] {
            let row = session_row(SessionState::Managed { pane: 1 }, tone, Some(4));
            let items = row_menu(&row, ALONE, &MenuContext::default()).unwrap();
            let marks: Vec<_> =
                labels(&items).into_iter().filter(|l| l == "Mark as unread" || l == "Mark as read").collect();
            assert_eq!(marks, label.map(String::from).into_iter().collect::<Vec<_>>(), "{tone:?}");
        }
    }

    #[test]
    fn a_live_claude_row_offers_new_here_copy_and_close() {
        let row = session_row(SessionState::Managed { pane: 1 }, Tone::Idle, Some(4));
        let items = row_menu(&row, ALONE, &MenuContext::default()).unwrap();
        assert_eq!(
            labels(&items),
            [
                "New Claude here",
                "New terminal here",
                "│",
                "Mark as unread",
                "Copy session id",
                "Copy folder path",
                "Open folder",
                "Open in VS Code",
                "│",
                "Close tab"
            ]
        );
        let MenuItem::Entry(copy) = &items[4] else { panic!() };
        assert_eq!(copy.action, MenuAction::Copy("s1".into()));
    }

    #[test]
    fn close_says_pane_in_a_split_and_tab_when_alone() {
        let row = session_row(SessionState::Managed { pane: 1 }, Tone::Idle, Some(4));
        let split: &dyn Fn(NodeId) -> bool = &|_| false;
        let items = row_menu(&row, split, &MenuContext::default()).unwrap();
        let MenuItem::Entry(close) = items.last().unwrap() else { panic!() };
        assert_eq!((close.label.as_str(), close.action.clone()), ("Close pane", MenuAction::ClosePane(4)));
    }

    #[test]
    fn an_outside_session_offers_take_over_only_with_a_host_pid_and_folder() {
        let row = session_row(SessionState::External { pid: Some(42) }, Tone::Elsewhere, None);
        let items = row_menu(&row, ALONE, &MenuContext::default()).unwrap();
        assert_eq!(
            labels(&items),
            ["Take over…", "│", "Copy session id", "Copy folder path", "Open folder", "Open in VS Code"]
        );
        let MenuItem::Entry(take) = &items[0] else { panic!() };
        assert!(take.accent);
        assert_eq!(
            take.action,
            MenuAction::TakeOver { session: "s1".into(), cwd: "/r/app".into(), host: HostId::local() }
        );
        let boxed = session_row(SessionState::External { pid: None }, Tone::Elsewhere, None);
        assert_eq!(labels(&row_menu(&boxed, ALONE, &MenuContext::default()).unwrap())[0], "Copy session id");
    }

    #[test]
    fn an_exited_claude_offers_continue() {
        let row = session_row(SessionState::Closed, Tone::Closed, Some(4));
        let items = row_menu(&row, ALONE, &MenuContext::default()).unwrap();
        assert_eq!(
            labels(&items),
            [
                "Continue",
                "│",
                "Copy session id",
                "Copy folder path",
                "Open folder",
                "Open in VS Code",
                "│",
                "Close tab"
            ]
        );
        let MenuItem::Entry(cont) = &items[0] else { panic!() };
        assert_eq!((cont.accent, cont.action.clone()), (true, MenuAction::Continue(4)));
        let items = row_menu(&pane_row(true, true), ALONE, &MenuContext::default()).unwrap();
        assert_eq!(labels(&items)[0], "Continue");
    }

    #[test]
    fn a_new_claude_pane_and_a_shell_have_their_menus() {
        assert_eq!(
            labels(&row_menu(&pane_row(true, false), ALONE, &MenuContext::default()).unwrap()),
            [
                "New Claude here",
                "New terminal here",
                "│",
                "Copy session id",
                "Copy folder path",
                "Open folder",
                "Open in VS Code",
                "│",
                "Close tab",
            ]
        );
        let shell = row_menu(&pane_row(false, false), ALONE, &MenuContext::default()).unwrap();
        assert_eq!(
            labels(&shell),
            [
                "New terminal here",
                "Rename…",
                "│",
                "Copy folder path",
                "Open folder",
                "Open in VS Code",
                "│",
                "Close tab"
            ]
        );
        let MenuItem::Entry(new) = &shell[0] else { panic!() };
        assert_eq!(new.action, MenuAction::NewHere { dir: "/r/app".into(), claude: false, host: HostId::local() });
    }

    #[test]
    fn a_session_row_offers_a_terminal_too() {
        let live = session_row(SessionState::Managed { pane: 1 }, Tone::Idle, Some(4));
        let items = row_menu(&live, ALONE, &MenuContext::default()).unwrap();
        assert_eq!(labels(&items)[..3], ["New Claude here", "New terminal here", "│"]);
        let MenuItem::Entry(new) = &items[1] else { panic!() };
        assert_eq!(new.action, MenuAction::NewHere { dir: "/r/app".into(), claude: false, host: HostId::local() });
        let items = row_menu(&live, ALONE, &two_dirs(None)).unwrap();
        assert_eq!(labels(&items)[..4], ["New Claude here", "New Claude here with ▸", "New terminal here", "│"]);
        let pane = row_menu(&pane_row(true, false), ALONE, &two_dirs(None)).unwrap();
        assert_eq!(labels(&pane)[..4], ["New Claude here", "New Claude here with ▸", "New terminal here", "│"]);
    }

    #[test]
    fn a_project_heading_offers_new_sessions_and_recents() {
        let items = project_menu(Path::new("/r/app"), None, &MenuContext::default());
        assert_eq!(
            labels(&items),
            [
                "New Claude here",
                "New terminal here",
                "Pin as favorite",
                "Recent sessions here",
                "│",
                "Copy folder path",
                "Open folder",
                "Open in VS Code"
            ]
        );
        let MenuItem::Entry(recents) = &items[3] else { panic!() };
        assert_eq!(
            (recents.action.clone(), recents.shortcut),
            (MenuAction::RecentsFor("/r/app".into()), Some(Shortcut::Recents))
        );
        let row = Row::new(RowKind::Project { path: "/r/app".into(), label: "app".into(), expanded: true });
        assert_eq!(
            row_menu(&row, ALONE, &MenuContext::default()),
            None,
            "headings are built with project_menu (they need the container)"
        );
    }

    #[test]
    fn detail_rows_have_no_menu() {
        let row = Row::new(RowKind::Detail { session: "s".into(), text: "x".into() });
        assert_eq!(row_menu(&row, ALONE, &MenuContext::default()), None);
    }

    #[test]
    fn vscode_opens_a_container_session_inside_its_container() {
        let place = ContainerPlace { host_folder: Some("/tmp/a".into()), path: "/workspaces/a/sub".into() };
        assert_eq!(
            vscode_args(Path::new("/tmp/a/sub"), Some(&place), None),
            ["--folder-uri".to_string(), "vscode-remote://dev-container+2f746d702f61/workspaces/a/sub".to_string(),]
        );
    }

    #[test]
    fn vscode_uses_the_default_workspace_folder_for_a_container_pane() {
        assert_eq!(
            vscode_args(Path::new("/tmp/a"), None, Some(Path::new("/tmp/a"))),
            ["--folder-uri".to_string(), "vscode-remote://dev-container+2f746d702f61/workspaces/a".to_string(),]
        );
    }

    #[test]
    fn vscode_falls_back_to_the_folder_without_a_host_folder() {
        let unknown = ContainerPlace { host_folder: None, path: "/workspaces/x".into() };
        assert_eq!(vscode_args(Path::new("/r/app"), Some(&unknown), None), ["/r/app".to_string()]);
        assert_eq!(vscode_args(Path::new("/r/app"), None, None), ["/r/app".to_string()]);
    }

    #[test]
    fn a_container_row_opens_vscode_in_its_container() {
        let mut row = session_row(SessionState::Managed { pane: 1 }, Tone::Idle, Some(4));
        let RowKind::Session { session, .. } = &mut row.kind else { panic!() };
        session.container = Some(ContainerPlace { host_folder: Some("/tmp/a".into()), path: "/workspaces/a".into() });
        let items = row_menu(&row, ALONE, &MenuContext::default()).unwrap();
        let code = items.iter().find_map(|item| match item {
            MenuItem::Entry(e) if e.label == "Open in VS Code" => Some(e.action.clone()),
            _ => None,
        });
        assert_eq!(
            code,
            Some(MenuAction::OpenInVsCode(vec![
                "--folder-uri".into(),
                "vscode-remote://dev-container+2f746d702f61/workspaces/a".into()
            ]))
        );
    }

    #[test]
    fn the_dir_submenu_needs_two_dirs_and_a_host_folder() {
        let live = session_row(SessionState::Managed { pane: 1 }, Tone::Idle, Some(4));
        let one = MenuContext { dirs: vec![("Default".into(), true, false)], favorite: false, project_color: None };
        assert!(submenu_labels(&row_menu(&live, ALONE, &one).unwrap()).is_empty());
        let items = row_menu(&live, ALONE, &two_dirs(None)).unwrap();
        assert_eq!(labels(&items)[..3], ["New Claude here", "New Claude here with ▸", "New terminal here"]);
        assert_eq!(submenu_labels(&items), ["Personal (default)", "Work"]);
        // Closed rows offer Continue instead of New Claude here, so no choice either.
        let closed = session_row(SessionState::Closed, Tone::Closed, Some(4));
        assert!(submenu_labels(&row_menu(&closed, ALONE, &two_dirs(None)).unwrap()).is_empty());
        // A container session is not offered a pick.
        let mut boxed = live.clone();
        let RowKind::Session { session, .. } = &mut boxed.kind else { panic!() };
        session.container = Some(ContainerPlace { host_folder: Some("/tmp/a".into()), path: "/workspaces/a".into() });
        assert!(submenu_labels(&row_menu(&boxed, ALONE, &two_dirs(None)).unwrap()).is_empty());
        let place = ContainerPlace { host_folder: Some("/tmp/a".into()), path: "/workspaces/a".into() };
        assert!(submenu_labels(&project_menu(Path::new("/r/app"), Some(&place), &two_dirs(None))).is_empty());
        // A shell pane has no Claude to pick for.
        assert!(submenu_labels(&row_menu(&pane_row(false, false), ALONE, &two_dirs(None)).unwrap()).is_empty());
        assert_eq!(submenu_labels(&row_menu(&pane_row(true, false), ALONE, &two_dirs(None)).unwrap()).len(), 2);
    }

    #[test]
    fn the_submenu_marks_the_default_and_the_remembered_dir() {
        let items = project_menu(Path::new("/r/app"), None, &two_dirs(Some(1)));
        assert_eq!(submenu_labels(&items), ["Personal (default)", "Work ✓ this folder"]);
        let Some(MenuItem::Submenu { items, .. }) = items.iter().find(|item| matches!(item, MenuItem::Submenu { .. }))
        else {
            panic!()
        };
        let MenuItem::Entry(work) = &items[1] else { panic!() };
        assert_eq!(work.action, MenuAction::NewClaudeWith { dir: "/r/app".into(), dir_name: "Work".into() });
        let remembered_default = project_menu(Path::new("/r/app"), None, &two_dirs(Some(0)));
        assert_eq!(submenu_labels(&remembered_default)[0], "Personal (default) ✓ this folder");
    }

    #[test]
    fn the_context_follows_the_settings() {
        use crate::settings::ClaudeDir;
        let dir = |name: &str| ClaudeDir {
            name: name.into(),
            path: format!("~/.{name}").into(),
            command: "claude".into(),
            args: vec![],
            color: None,
        };
        let mut settings = Settings { claude_dirs: vec![dir("Personal"), dir("Work")], ..Settings::default() };
        settings.default_claude_dir = Some("Work".into());
        settings.folder_claude_dirs.insert("/r/app".into(), "Personal".into());
        settings.favorites = vec!["/r/app".into()];
        let here = MenuContext::for_folder(&settings, Path::new("/r/app"));
        assert_eq!(here.dirs, [("Personal".to_string(), false, true), ("Work".to_string(), true, false)]);
        assert!(here.favorite);
        let elsewhere = MenuContext::for_folder(&settings, Path::new("/r/other"));
        assert_eq!(elsewhere.dirs, [("Personal".to_string(), false, false), ("Work".to_string(), true, false)]);
        assert!(!elsewhere.favorite);
        // No configured dirs: the implicit Default is not a choice.
        assert!(MenuContext::for_folder(&Settings::default(), Path::new("/r/app")).dirs.is_empty());
    }

    #[test]
    fn a_tab_menu_offers_the_choice_after_the_splits() {
        let items = tab_menu(0, &HostId::local(), Path::new("/r/app"), vec![], &two_dirs(None), None, 1);
        assert_eq!(labels(&items)[..4], ["Split right", "Split down", "New Claude here with ▸", "│"]);
    }

    #[test]
    fn project_menu_has_color_submenu_when_on() {
        let mut ctx = MenuContext { project_color: Some(Some(ProjectColor::Teal)), ..MenuContext::default() };
        let items = project_menu(Path::new("/r/app"), None, &ctx);
        let Some(MenuItem::Submenu { label, items: colors }) =
            items.iter().find(|i| matches!(i, MenuItem::Submenu { label, .. } if label == "Color"))
        else {
            panic!("no Color submenu")
        };
        assert_eq!(label, "Color");
        let swatches: Vec<&MenuEntry> = colors
            .iter()
            .filter_map(|i| match i {
                MenuItem::Entry(e) if e.swatch.is_some() => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(swatches.len(), 8);
        assert!(swatches.iter().any(|e| e.checked && e.swatch == Some(ProjectColor::Teal)));
        assert_eq!(swatches.iter().filter(|e| e.checked).count(), 1);
        assert!(colors.iter().any(|i| matches!(i, MenuItem::Entry(e) if e.label == "Automatic color" && !e.checked
            && e.action == MenuAction::SetProjectColor { root: "/r/app".into(), color: None })));
        ctx.project_color = None;
        assert!(
            !project_menu(Path::new("/r/app"), None, &ctx)
                .iter()
                .any(|i| matches!(i, MenuItem::Submenu { label, .. } if label == "Color"))
        );
    }

    #[test]
    fn the_context_reads_the_folders_color_from_settings() {
        let mut settings = Settings::default();
        assert_eq!(MenuContext::for_folder(&settings, Path::new("/r/app")).project_color, Some(None));
        settings.project_colors.folders.insert("/r/app".into(), ProjectColor::Lime);
        assert_eq!(
            MenuContext::for_folder(&settings, Path::new("/r/app")).project_color,
            Some(Some(ProjectColor::Lime))
        );
        settings.project_colors.enabled = false;
        assert_eq!(MenuContext::for_folder(&settings, Path::new("/r/app")).project_color, None);
    }

    #[test]
    fn a_project_menu_pins_and_unpins() {
        let pinned = MenuContext { favorite: true, ..MenuContext::default() };
        let items = project_menu(Path::new("/r/app"), None, &pinned);
        assert_eq!(labels(&items)[..4], ["New Claude here", "New terminal here", "Unpin", "Recent sessions here"]);
        let MenuItem::Entry(unpin) = &items[2] else { panic!() };
        assert_eq!(unpin.action, MenuAction::UnpinFavorite("/r/app".into()));
        let MenuItem::Entry(pin) = &project_menu(Path::new("/r/app"), None, &MenuContext::default())[2] else {
            panic!()
        };
        assert_eq!(pin.action, MenuAction::PinFavorite("/r/app".into()));
    }

    fn devbox() -> HostId {
        HostId::named("devbox")
    }

    fn remote_session_row(state: SessionState, tone: Tone, leaf: Option<NodeId>) -> Row {
        let mut row = session_row(state, tone, leaf);
        let RowKind::Session { session, .. } = &mut row.kind else { panic!() };
        session.host = devbox();
        row
    }

    #[test]
    fn a_remote_session_row_hides_local_only_entries() {
        let row = remote_session_row(SessionState::Managed { pane: 1 }, Tone::Idle, Some(4));
        let items = row_menu(&row, &|_| false, &two_dirs(None)).unwrap();
        let shown = labels(&items);
        for hidden in ["Open folder", "Open in VS Code", "New Claude here with ▸"] {
            assert!(!shown.contains(&hidden.to_string()), "{hidden} in {shown:?}");
        }
        for kept in ["Copy folder path (on devbox)", "Copy session id", "Close pane"] {
            assert!(shown.contains(&kept.to_string()), "{kept} missing in {shown:?}");
        }
    }

    #[test]
    fn a_remote_external_session_still_offers_take_over_with_its_host() {
        let row = remote_session_row(SessionState::External { pid: Some(42) }, Tone::Elsewhere, None);
        let items = row_menu(&row, ALONE, &MenuContext::default()).unwrap();
        let MenuItem::Entry(take) = &items[0] else { panic!() };
        assert_eq!(take.action, MenuAction::TakeOver { session: "s1".into(), cwd: "/r/app".into(), host: devbox() });
    }

    #[test]
    fn new_here_on_a_remote_row_carries_the_host() {
        let row = remote_session_row(SessionState::Managed { pane: 1 }, Tone::Idle, Some(4));
        let items = row_menu(&row, ALONE, &MenuContext::default()).unwrap();
        let MenuItem::Entry(new) = &items[0] else { panic!() };
        assert_eq!(new.action, MenuAction::NewHere { dir: "/r/app".into(), claude: true, host: devbox() });
        let mut pane = pane_row(false, false);
        let RowKind::Pane { entry, .. } = &mut pane.kind else { panic!() };
        entry.host = devbox();
        let shell = row_menu(&pane, ALONE, &MenuContext::default()).unwrap();
        let MenuItem::Entry(new) = &shell[0] else { panic!() };
        assert_eq!(new.action, MenuAction::NewHere { dir: "/r/app".into(), claude: false, host: devbox() });
    }

    #[test]
    fn a_remote_project_heading_has_a_pin_but_no_color_or_folder_entries() {
        let context = MenuContext { project_color: Some(None), ..two_dirs(None) };
        let items = project_menu(Path::new("ssh://devbox/r/app"), None, &context);
        assert_eq!(
            labels(&items),
            [
                "New Claude here",
                "New terminal here",
                "Pin as favorite",
                "Recent sessions here",
                "│",
                "Copy folder path (on devbox)"
            ]
        );
        let MenuItem::Entry(new) = &items[0] else { panic!() };
        assert_eq!(new.action, MenuAction::NewHere { dir: "/r/app".into(), claude: true, host: devbox() });
        let MenuItem::Entry(pin) = &items[2] else { panic!() };
        assert_eq!(pin.action, MenuAction::PinFavorite("ssh://devbox/r/app".into()));
        let MenuItem::Entry(copy) = items.last().unwrap() else { panic!() };
        assert_eq!(copy.action, MenuAction::Copy("/r/app".into()));
    }

    #[test]
    fn a_pinned_remote_heading_offers_unpin_by_its_key() {
        let settings = Settings { favorites: vec!["ssh://devbox/r/app".into()], ..Settings::default() };
        let key = Path::new("ssh://devbox/r/app");
        let context = MenuContext::for_folder(&settings, key);
        assert!(context.favorite);
        let items = project_menu(key, None, &context);
        let MenuItem::Entry(unpin) = &items[2] else { panic!() };
        assert_eq!((unpin.label.as_str(), &unpin.action), ("Unpin", &MenuAction::UnpinFavorite(key.to_path_buf())));
    }

    #[test]
    fn no_menu_starts_ends_or_doubles_with_a_separator() {
        let clean = |items: &[MenuItem]| {
            assert!(!matches!(items.first(), Some(MenuItem::Separator)), "{:?}", labels(items));
            assert!(!matches!(items.last(), Some(MenuItem::Separator)), "{:?}", labels(items));
            assert!(
                !items.windows(2).any(|pair| pair.iter().all(|item| matches!(item, MenuItem::Separator))),
                "{:?}",
                labels(items)
            );
        };
        for host in [HostId::local(), devbox()] {
            clean(&tab_menu(0, &host, Path::new("/r/app"), vec![], &MenuContext::default(), None, 1));
            clean(&tab_menu(0, &host, Path::new("/r/app"), vec![], &two_dirs(None), Some((7, true)), 2));
        }
        for row in [
            session_row(SessionState::Closed, Tone::Closed, None),
            session_row(SessionState::External { pid: None }, Tone::Elsewhere, None),
            remote_session_row(SessionState::External { pid: None }, Tone::Elsewhere, None),
            remote_session_row(SessionState::Closed, Tone::Closed, Some(4)),
            remote_session_row(SessionState::Managed { pane: 1 }, Tone::Idle, Some(4)),
            pane_row(true, true),
        ] {
            if let Some(items) = row_menu(&row, ALONE, &two_dirs(None)) {
                clean(&items);
            }
        }
        for key in ["/r/app", "ssh://devbox/r/app"] {
            clean(&project_menu(Path::new(key), None, &MenuContext::default()));
        }
    }
}
