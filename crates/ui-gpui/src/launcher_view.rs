//! The directory launcher overlay (`Ctrl+Shift+O`, and the New buttons): a field and a Browse… button over one list of
//! folders from this machine and every SSH host (open now, favorites, recent), fuzzy filtered. Typing a path (`/…`,
//! `~…` or `<host>:<path>`) lists that folder's subfolders instead.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use app_core::dir_complete::{self, Completion, TabAction};
use app_core::host_state::{HostState, away_line};
use app_core::launcher::{self, LauncherItem, OpenFolder, RecentFolder, Typed};
use app_core::session_tree::display_path;
use app_core::settings::expand_home;
use app_core::text_field::TextField;
use app_core::workspace::ClaudeDirs;
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, EventEmitter, FocusHandle, Focusable, FontWeight,
    IntoElement, KeyDownEvent, MouseButton, Render, ScrollStrategy, Stateful, UniformListScrollHandle, Window, div,
    prelude::*, px, rgb, uniform_list,
};
use protocol::host::{HostId, split_project_key};
use protocol::message::DirEntry;

use crate::field;
use crate::settings_store::SettingsStore;
use crate::theme::{FadeKind, RADIUS, RADIUS_LARGE, RADIUS_SMALL, Theme, fade_in, theme};

/// Quiet time after typing before a folder listing is asked for.
const LIST_DELAY: Duration = Duration::from_millis(120);
/// Listing request ids, unique across launchers (a late reply to a closed launcher must not match a new one's).
static NEXT_LISTING: AtomicU64 = AtomicU64::new(1);

/// One entry of the launcher's hosts: this machine, then each SSH host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherHost {
    /// The host.
    pub id: HostId,
    /// The host's daemon is connected (always true for this machine).
    pub connected: bool,
    /// Where its connection stands.
    pub state: HostState,
}

#[allow(missing_docs)]
mod launcher_actions {
    gpui::actions!(launcher, [LauncherBrowse]);
}
pub(crate) use launcher_actions::LauncherBrowse;

/// What the launcher asks the workspace to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LauncherEvent {
    /// Open a new tab in `dir`.
    Open {
        /// The host the directory is on.
        host: HostId,
        /// Directory.
        dir: PathBuf,
        /// Claude (true) or a terminal.
        claude: bool,
        /// The Claude config dir (by name) chosen with the badge, when it differs from the one the folder resolves to.
        dir_pick: Option<String>,
    },
    /// Pick a directory of this machine with the system dialog, then open it.
    Browse {
        /// Claude (true) or a terminal.
        claude: bool,
    },
    /// List the folders in `path` on `host` (path completion); the reply goes to `set_listing` or
    /// `set_listing_failed` with the same `request`.
    ListDir {
        /// The host to ask.
        host: HostId,
        /// Request id.
        request: u64,
        /// The folder to list.
        path: String,
    },
    /// Close without opening anything.
    Dismiss,
}

/// A config dir picked with the badge: the dir's index, for the row of (host, folder) it was picked on.
type Pick = (HostId, PathBuf, usize);

/// The dir picked for `folder` on `host`: a pick belongs to the row it was made on, so another row has none.
fn pick_for(pick: Option<&Pick>, host: &HostId, folder: &Path) -> Option<usize> {
    pick.filter(|(pick_host, pick_folder, _)| pick_host == host && pick_folder == folder).map(|(_, _, dir)| *dir)
}

/// A host's name on a remote row: solid while it is connected, dashed with "not connected" otherwise.
fn host_tag(host: &HostId, connected: bool, theme: &Theme) -> Option<AnyElement> {
    let name = host.remote_name()?;
    Some(
        div()
            .flex_none()
            .h(theme.ui(18.))
            .px(theme.ui(6.))
            .flex()
            .items_center()
            .text_xs()
            .rounded(px(RADIUS_SMALL))
            .border_1()
            .when(!connected, |tag| tag.border_dashed())
            .border_color(rgb(if connected { theme.tokens.accent } else { theme.tokens.muted }))
            .text_color(rgb(if connected { theme.tokens.accent } else { theme.tokens.muted }))
            .child(if connected { name.to_string() } else { format!("{name} · not connected") })
            .into_any_element(),
    )
}

/// The mark of a favorite folder (nothing for any other).
pub(crate) fn star(favorite: bool, theme: &Theme) -> AnyElement {
    div().text_color(rgb(theme.tokens.accent)).child(if favorite { "★" } else { "" }).into_any_element()
}

/// A folder's row as the launcher and the welcome screen draw it: mark, folder name, path, host tag, then `trailing`.
pub(crate) struct FolderRow<'a> {
    /// The host the folder is on.
    pub host: &'a HostId,
    /// The folder.
    pub path: &'a Path,
    /// This machine's home folder, shortened to `~` in the path.
    pub home: Option<&'a Path>,
    /// The host is connected (else its tag is dashed).
    pub connected: bool,
    /// The mark at the left: a status dot, a star or nothing.
    pub lead: AnyElement,
    /// The row is highlighted.
    pub selected: bool,
    /// The row is dimmed (a favorite whose folder is gone).
    pub dimmed: bool,
}

impl FolderRow<'_> {
    /// The row with `trailing` after the host tag; the caller adds the click.
    pub(crate) fn render(self, theme: &Theme, id: impl Into<ElementId>, trailing: Vec<AnyElement>) -> Stateful<Div> {
        let path = self.path;
        let name =
            path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned());
        let shown = if self.host.is_local() { display_path(path, self.home) } else { path.display().to_string() };
        let mut children = vec![
            div().flex_none().w(theme.ui(14.)).flex().items_center().child(self.lead).into_any_element(),
            div().flex_none().font_weight(FontWeight::SEMIBOLD).child(name).into_any_element(),
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(rgb(theme.tokens.muted))
                .child(shown)
                .into_any_element(),
        ];
        children.extend(host_tag(self.host, self.connected, theme));
        children.extend(trailing);
        LauncherView::row(theme, id, self.selected, self.dimmed, children)
    }
}

/// The launcher overlay.
pub struct LauncherView {
    focus_handle: FocusHandle,
    open: Vec<OpenFolder>,
    recent: Vec<RecentFolder>,
    /// Enter starts Claude and Alt+Enter a terminal; false swaps them.
    claude_first: bool,
    home: Option<PathBuf>,
    query: TextField,
    items: Vec<LauncherItem>,
    selected: usize,
    scroll: UniformListScrollHandle,
    favorites: Vec<PathBuf>,
    /// This machine and the SSH hosts.
    hosts: Vec<LauncherHost>,
    /// The newest folder listing, of `completion_host`.
    completion: Completion,
    completion_host: HostId,
    /// The highlighted completion row; `None` = the typed path itself.
    completion_selected: Option<usize>,
    /// The pause before a listing request.
    list_task: Option<gpui::Task<()>>,
    scroll_completions: UniformListScrollHandle,
    /// The Claude config dirs, with the default one's index.
    dirs: ClaudeDirs,
    /// Folder → dir name remembered for it.
    memory: BTreeMap<PathBuf, String>,
    /// The dir chosen with the badge (Tab or the list), for the row it was chosen on.
    pick: Option<Pick>,
    /// The badge's list of dirs is open.
    badge_open: bool,
}

impl EventEmitter<LauncherEvent> for LauncherView {}

impl Focusable for LauncherView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl LauncherView {
    /// A launcher over the `open` folders and the `recent` ones of every host in `hosts` (this machine first).
    /// `claude_first`: Enter starts Claude, Alt+Enter a terminal; false swaps them.
    pub fn new(
        open: Vec<OpenFolder>,
        recent: Vec<RecentFolder>,
        claude_first: bool,
        hosts: Vec<LauncherHost>,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = cx.global::<SettingsStore>().settings();
        let (favorites, memory) = (settings.favorites.clone(), settings.folder_claude_dirs.clone());
        let dirs = ClaudeDirs::from_settings(settings, None, None);
        let mut launcher = LauncherView {
            focus_handle: cx.focus_handle(),
            open,
            recent,
            claude_first,
            home: std::env::home_dir(),
            query: TextField::default(),
            items: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            favorites,
            hosts,
            completion: Completion::default(),
            completion_host: HostId::local(),
            completion_selected: None,
            list_task: None,
            scroll_completions: UniformListScrollHandle::new(),
            dirs,
            memory,
            pick: None,
            badge_open: false,
        };
        launcher.refresh();
        launcher
    }

    /// The SSH hosts (everything but this machine).
    fn ssh_hosts(&self) -> Vec<HostId> {
        self.hosts.iter().filter(|host| !host.id.is_local()).map(|host| host.id.clone()).collect()
    }

    fn refresh(&mut self) {
        self.items = launcher::items(
            &self.open,
            &self.recent,
            self.query.text(),
            self.home.as_deref(),
            &self.favorites,
            &self.ssh_hosts(),
        );
        self.selected = 0;
    }

    /// Update the hosts' states. A typed host that just connected lists the typed path's parent.
    pub fn set_hosts(&mut self, hosts: Vec<LauncherHost>, cx: &mut Context<Self>) {
        let was_away = self.typed_host_away();
        self.hosts = hosts;
        let kept = (self.items.len(), self.selected);
        if was_away && !self.typed_host_away() {
            self.completion = Completion::default();
            self.typed_changed(cx);
        }
        self.refresh();
        if self.items.len() == kept.0 {
            self.selected = kept.1; // only a host's state changed: the highlight stays
        }
        cx.notify();
    }

    fn host_connected(&self, host: &HostId) -> bool {
        host.is_local() || self.hosts.iter().any(|known| known.id == *host && known.connected)
    }

    /// The typed text when it is a path (on this machine or a host), with the host.
    fn path_mode(&self) -> Option<(HostId, String)> {
        match launcher::typed(self.query.text(), &self.ssh_hosts()) {
            Typed::Path { host, path } => Some((host, path)),
            Typed::Filter => None,
        }
    }

    /// The typed path is on an SSH host that is not connected.
    fn typed_host_away(&self) -> bool {
        self.path_mode().is_some_and(|(host, _)| !self.host_connected(&host))
    }

    /// The folders of the typed path's parent that match what follows its last `/`.
    fn completions(&self) -> Vec<DirEntry> {
        let Some((host, path)) = self.path_mode() else { return Vec::new() };
        let (parent, prefix) = dir_complete::split_typed(&path);
        if self.completion_host != host || self.completion.parent != parent {
            return Vec::new();
        }
        dir_complete::filter(&self.completion.dirs, &prefix)
    }

    /// Why the typed path's parent could not be listed, while it is the typed one.
    fn listing_error(&self) -> Option<&str> {
        let (host, path) = self.path_mode()?;
        let (parent, _) = dir_complete::split_typed(&path);
        self.completion.error.as_deref().filter(|_| self.completion_host == host && self.completion.requested == parent)
    }

    /// The text changed: ask for the typed path's parent after a short pause, unless it is listed (or its host is away).
    fn typed_changed(&mut self, cx: &mut Context<Self>) {
        self.completion_selected = None;
        self.list_task = None;
        let Some((host, path)) = self.path_mode().filter(|(host, _)| self.host_connected(host)) else { return };
        if self.completion_host != host {
            self.completion = Completion::default();
            self.completion_host = host.clone();
        }
        if !self.completion.wants_request(&path) {
            return;
        }
        let (parent, _) = dir_complete::split_typed(&path);
        self.list_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LIST_DELAY).await;
            let _ = this.update(cx, |this, cx| this.request_listing(host, parent, cx));
        }));
    }

    fn request_listing(&mut self, host: HostId, parent: String, cx: &mut Context<Self>) {
        let request = NEXT_LISTING.fetch_add(1, Ordering::Relaxed);
        self.completion.start_request(request, parent.clone());
        cx.emit(LauncherEvent::ListDir { host, request, path: parent });
    }

    /// The folders of the newest listing request; replies to older requests are ignored.
    pub fn set_listing(&mut self, request: u64, dirs: Vec<DirEntry>, cx: &mut Context<Self>) {
        if request != self.completion.request {
            return;
        }
        self.completion.accept_listing(request, dirs);
        self.completion_selected = None;
        cx.notify();
    }

    /// The newest listing request failed; replies to older requests are ignored.
    pub fn set_listing_failed(&mut self, request: u64, reason: String, cx: &mut Context<Self>) {
        if request != self.completion.request {
            return;
        }
        self.completion.accept_failure(request, reason);
        cx.notify();
    }

    /// Complete the typed path with completion row `index`: its folder and a `/`, which asks for the next listing.
    fn accept_completion(&mut self, index: usize, cx: &mut Context<Self>) {
        let (Some((host, path)), Some(dir)) = (self.path_mode(), self.completions().get(index).cloned()) else {
            return;
        };
        let (parent, _) = dir_complete::split_typed(&path);
        let prefix = if host.is_local() { String::new() } else { format!("{}:", host.name()) };
        self.query = TextField::default();
        self.query.insert(&format!("{prefix}{parent}{}/", dir.name));
        self.badge_open = false;
        self.typed_changed(cx);
    }

    /// A path of `host` as a launch uses it: `~` expanded on this machine (with Windows separators there), a remote path
    /// as typed.
    fn launch_path(&self, host: &HostId, path: &str) -> PathBuf {
        let path =
            if cfg!(windows) && host.is_local() { PathBuf::from(path.replace('/', "\\")) } else { PathBuf::from(path) };
        if host.is_local() { expand_home(&path, self.home.as_deref()) } else { path }
    }

    /// The folder the highlight is on: a completion row in path mode, else the highlighted list row.
    fn highlighted(&self) -> Option<(HostId, PathBuf)> {
        if let Some((host, path)) = self.path_mode() {
            let dir = self.completions().get(self.completion_selected?)?.clone();
            let full = format!("{}{}", dir_complete::split_typed(&path).0, dir.name);
            return Some((host.clone(), self.launch_path(&host, &full)));
        }
        let item = self.items.get(self.selected)?;
        Some((item.host().clone(), item.folder()?.clone()))
    }

    /// The index of the dir `folder` resolves to without a pick: the one remembered for it, else the default.
    fn resolved_dir(&self, folder: &Path) -> usize {
        let default = &self.dirs.dirs[self.dirs.default].name;
        let name = self.memory.get(folder).unwrap_or(default);
        self.dirs.dirs.iter().position(|dir| dir.name == *name).unwrap_or(self.dirs.default)
    }

    /// The dir the badge shows, with the folder it is for: only for a Claude launch of a folder of this machine with two
    /// or more dirs. The row's own pick, else what the folder resolves to.
    fn badge(&self) -> Option<(PathBuf, usize)> {
        let (host, folder) = self.highlighted().filter(|(host, _)| host.is_local())?;
        let missing = matches!(self.items.get(self.selected), Some(LauncherItem::Dir { exists: false, .. }));
        if !self.claude_first || (missing && self.path_mode().is_none()) || self.dirs.dirs.len() < 2 {
            return None;
        }
        let dir = pick_for(self.pick.as_ref(), &host, &folder).unwrap_or_else(|| self.resolved_dir(&folder));
        Some((folder, dir))
    }

    /// The pick to hand to the workspace for `folder`: only an explicit choice on this row that differs from what it
    /// resolves to, for a Claude launch on this machine.
    fn dir_pick_for(&self, host: &HostId, folder: &Path, claude: bool) -> Option<String> {
        if !host.is_local() || !claude {
            return None;
        }
        let pick = pick_for(self.pick.as_ref(), host, folder).filter(|pick| *pick != self.resolved_dir(folder))?;
        Some(self.dirs.dirs[pick].name.clone())
    }

    /// Open the typed path (or the highlighted completion row) on its host.
    fn open_typed(&mut self, alt: bool, cx: &mut Context<Self>) {
        let Some((host, path)) = self.path_mode() else { return };
        if self.listing_error().is_some() {
            return;
        }
        let claude = self.claude_first != alt;
        let typed = match self.completion_selected.and_then(|index| self.completions().get(index).cloned()) {
            Some(dir) => format!("{}{}", dir_complete::split_typed(&path).0, dir.name),
            None => path,
        };
        let dir = self.launch_path(&host, if typed.len() > 1 { typed.trim_end_matches('/') } else { typed.as_str() });
        let dir_pick = self.dir_pick_for(&host, &dir, claude);
        cx.emit(LauncherEvent::Open { host, dir, claude, dir_pick });
    }

    /// Fill the field with `<host>:~/` and list it.
    fn fill_host(&mut self, host: &HostId, cx: &mut Context<Self>) {
        self.query = TextField::default();
        self.query.insert(&format!("{}:~/", host.name()));
        self.badge_open = false;
        self.refresh();
        self.typed_changed(cx);
    }

    fn choose(&mut self, index: usize, alt: bool, cx: &mut Context<Self>) {
        let claude = self.claude_first != alt;
        let (host, folder) = match self.items.get(index) {
            Some(LauncherItem::Open(folder)) => (folder.host.clone(), folder.path.clone()),
            Some(LauncherItem::Dir { host, path, exists: true, .. }) => (host.clone(), path.clone()),
            Some(LauncherItem::HostHint(host)) => {
                let host = host.clone();
                self.fill_host(&host, cx);
                cx.notify();
                return;
            }
            _ => return,
        };
        let dir_pick = self.dir_pick_for(&host, &folder, claude);
        cx.emit(LauncherEvent::Open { host, dir: folder, claude, dir_pick });
    }

    fn browse(&mut self, alt: bool, cx: &mut Context<Self>) {
        cx.emit(LauncherEvent::Browse { claude: self.claude_first != alt });
    }

    /// Move the highlighted row's pick one dir on (or back).
    fn cycle_dir(&mut self, backwards: bool) {
        let Some((folder, current)) = self.badge() else { return };
        let count = self.dirs.dirs.len();
        let next = if backwards { (current + count - 1) % count } else { (current + 1) % count };
        self.pick = Some((HostId::local(), folder, next));
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let path_mode = self.path_mode();
        if matches!(keystroke.key.as_str(), "up" | "down") {
            self.badge_open = false;
        }
        match keystroke.key.as_str() {
            "tab" if path_mode.is_some() => {
                let path = path_mode.as_ref().map(|(_, path)| path.clone()).unwrap_or_default();
                let rows = self.completions().len();
                match dir_complete::tab_action(&path, &self.completion.requested, rows, self.completion_selected) {
                    TabAction::Accept(index) => self.accept_completion(index, cx),
                    TabAction::Request(parent) => {
                        if let Some((host, _)) = self.path_mode().filter(|(host, _)| self.host_connected(host)) {
                            self.list_task = None;
                            self.request_listing(host, parent, cx);
                        }
                    }
                    TabAction::Nothing => {}
                }
            }
            "tab" => self.cycle_dir(keystroke.modifiers.shift),
            "up" if path_mode.is_some() => {
                self.completion_selected = self.completion_selected.and_then(|index| index.checked_sub(1));
            }
            "down" if path_mode.is_some() => {
                let last = self.completions().len().saturating_sub(1);
                self.completion_selected = Some(self.completion_selected.map_or(0, |index| (index + 1).min(last)));
            }
            "up" => self.selected = launcher::step(&self.items, self.selected, false),
            "down" => self.selected = launcher::step(&self.items, self.selected, true),
            "enter" if path_mode.is_some() => self.open_typed(keystroke.modifiers.alt, cx),
            "enter" => self.choose(self.selected, keystroke.modifiers.alt, cx),
            "escape" if self.badge_open => self.badge_open = false,
            "escape" => cx.emit(LauncherEvent::Dismiss),
            _ => {
                if !field::edit(&mut self.query, keystroke, cx) {
                    return;
                }
                self.badge_open = false;
                self.refresh();
                self.typed_changed(cx);
            }
        }
        if let Some(index) = self.completion_selected.filter(|_| path_mode.is_some()) {
            self.scroll_completions.scroll_to_item(index, ScrollStrategy::Center);
        }
        self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
        cx.stop_propagation();
        cx.notify();
    }

    /// A row's frame: the highlight, dimming and hover, around `children`.
    fn row(
        theme: &Theme,
        id: impl Into<ElementId>,
        selected: bool,
        dimmed: bool,
        children: Vec<AnyElement>,
    ) -> Stateful<Div> {
        div().id(id).w_full().h(theme.ui(34.)).px_2().py(px(2.)).child(
            div()
                .size_full()
                .px_3()
                .flex()
                .items_center()
                .gap(theme.ui(10.))
                .rounded(px(RADIUS_SMALL))
                .overflow_hidden()
                .whitespace_nowrap()
                .when(!dimmed, |el| el.cursor_pointer())
                .when(selected, |el| el.bg(rgb(theme.tokens.raised)))
                .when(!selected && !dimmed, |el| el.hover(|style| style.bg(rgb(theme.tokens.raised))))
                .when(dimmed, |el| el.text_color(rgb(theme.tokens.muted)))
                .children(children),
        )
    }

    /// A row's mark: the status dot of an open folder, a star for a favorite, else nothing.
    fn mark(&self, host: &HostId, path: &Path, theme: &Theme) -> AnyElement {
        if let Some(folder) = self.open.iter().find(|folder| folder.host == *host && folder.path == path) {
            return div()
                .size(theme.ui(8.))
                .rounded_full()
                .map(|dot| match folder.tone {
                    Some(tone) => dot.bg(rgb(theme.tone(tone).dot)),
                    None => dot.border_1().border_color(rgb(theme.tokens.muted)),
                })
                .into_any_element();
        }
        let favorite = self.favorites.iter().any(|key| split_project_key(key) == (host.clone(), path.to_path_buf()));
        star(favorite, theme)
    }

    /// The dir badge of the highlighted row: the dir it will use, a click opens the list of dirs.
    fn render_badge(&self, dir: usize, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("launcher-badge")
            .flex_none()
            .h(theme.ui(18.))
            .px(theme.ui(6.))
            .flex()
            .items_center()
            .text_xs()
            .rounded(px(RADIUS_SMALL))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .text_color(rgb(theme.tokens.text))
            .child(self.dirs.dirs[dir].name.clone())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.badge_open = !this.badge_open;
                cx.stop_propagation();
                cx.notify();
            }))
            .into_any_element()
    }

    /// The `↵` on the highlighted row.
    fn enter_mark(theme: &Theme) -> AnyElement {
        div().flex_none().text_color(rgb(theme.tokens.muted)).child("↵").into_any_element()
    }

    /// One row of the list.
    fn render_item(&self, index: usize, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = theme(cx).clone();
        let selected = index == self.selected;
        let (host, path, dimmed) = match &self.items[index] {
            LauncherItem::HostHint(host) => {
                let id = host.clone();
                let text = format!("{}: type a path on this host", host.name());
                let children = vec![
                    div().flex_none().w(theme.ui(14.)).into_any_element(),
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(theme.tokens.muted))
                        .child(text)
                        .into_any_element(),
                ];
                return Self::row(&theme, index, selected, false, children).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.fill_host(&id, cx);
                        cx.notify();
                    },
                ));
            }
            LauncherItem::Open(folder) => (folder.host.clone(), folder.path.clone(), false),
            LauncherItem::Dir { host, path, exists, .. } => (host.clone(), path.clone(), !exists),
        };
        let row = FolderRow {
            host: &host,
            path: &path,
            home: self.home.as_deref(),
            connected: self.host_connected(&host),
            lead: self.mark(&host, &path, &theme),
            selected,
            dimmed,
        };
        let mut trailing = Vec::new();
        if let Some((_, dir)) = self.badge().filter(|_| selected && !dimmed) {
            trailing.push(self.render_badge(dir, &theme, cx));
        }
        if selected && !dimmed {
            trailing.push(Self::enter_mark(&theme));
        }
        row.render(&theme, index, trailing)
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| this.choose(index, event.modifiers().alt, cx)))
    }

    /// One folder of the typed path's parent.
    fn render_completion(&self, index: usize, dir: &DirEntry, host: &HostId, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = theme(cx).clone();
        let selected = self.completion_selected == Some(index);
        let path = self.path_mode().map(|(_, path)| path).unwrap_or_default();
        let folder = self.launch_path(host, &format!("{}{}", dir_complete::split_typed(&path).0, dir.name));
        let lead = self.mark(host, &folder, &theme);
        let mut children = vec![
            div().flex_none().w(theme.ui(14.)).flex().items_center().child(lead).into_any_element(),
            div().flex_1().min_w_0().truncate().child(format!("{}/", dir.name)).into_any_element(),
        ];
        children.extend(host_tag(host, self.host_connected(host), &theme));
        if let Some((_, badge)) = self.badge().filter(|_| selected) {
            children.push(self.render_badge(badge, &theme, cx));
        }
        if selected {
            children.push(Self::enter_mark(&theme));
        }
        Self::row(&theme, ("launcher-completion", index), selected, false, children).on_click(cx.listener(
            move |this, _, _, cx| {
                this.accept_completion(index, cx);
                cx.notify();
            },
        ))
    }

    /// What shows under the field: why the typed path cannot open (error), or where an away host stands.
    fn notice(&self) -> Option<(String, bool)> {
        if let Some(reason) = self.listing_error() {
            return Some((reason.to_string(), true));
        }
        let (host, _) = self.path_mode().filter(|(host, _)| !self.host_connected(host))?;
        let state = self
            .hosts
            .iter()
            .find(|known| known.id == host)
            .map_or(HostState::NotConnected, |known| known.state.clone());
        Some((away_line(host.name(), &state), false))
    }

    /// The list of dirs the badge opens, marked "default" / "this folder".
    fn render_dir_list(&self, folder: &Path, current: usize, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let remembered = self.memory.contains_key(folder);
        let folder_dir = self.resolved_dir(folder);
        let host = HostId::local();
        let rows = self.dirs.dirs.iter().enumerate().map(|(position, dir)| {
            let tag = if position == self.dirs.default {
                "default"
            } else if remembered && position == folder_dir {
                "this folder"
            } else {
                ""
            };
            let folder = folder.to_path_buf();
            let host = host.clone();
            div()
                .id(("launcher-dir", position))
                .h(theme.ui(30.))
                .px(theme.ui(10.))
                .flex()
                .items_center()
                .gap(theme.ui(8.))
                .rounded(px(RADIUS_SMALL))
                .cursor_pointer()
                .when(position == current, |row| row.bg(rgb(theme.tokens.line)))
                .hover(|style| style.bg(rgb(theme.tokens.line)))
                .child(div().flex_1().child(dir.name.clone()))
                .child(div().text_xs().text_color(rgb(theme.tokens.muted)).child(tag))
                .child(div().w(theme.ui(14.)).text_color(rgb(theme.tokens.accent)).child(if position == current {
                    "✓"
                } else {
                    ""
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.pick = Some((host.clone(), folder.clone(), position));
                    this.badge_open = false;
                    cx.notify();
                }))
        });
        div()
            .mx_4()
            .my(px(6.))
            .p(px(6.))
            .flex()
            .flex_col()
            .rounded(px(RADIUS))
            .bg(rgb(theme.tokens.raised))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .text_sm()
            .text_color(rgb(theme.tokens.text))
            .children(rows)
    }
}

impl Render for LauncherView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        let badge = self.badge().filter(|_| self.badge_open);
        let card = div()
            .key_context("Launcher")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(|this, _: &LauncherBrowse, _, cx| this.browse(false, cx)))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(LauncherEvent::Dismiss)))
            .w(theme.ui(560.))
            .flex()
            .flex_col()
            .bg(rgb(theme.tokens.surface))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .rounded(px(RADIUS_LARGE))
            .overflow_hidden()
            .shadow_lg()
            .text_sm()
            .text_color(rgb(theme.tokens.text))
            .child(
                div()
                    .h(theme.ui(46.))
                    .px_4()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(theme.tokens.line))
                    .gap(theme.ui(10.))
                    .child(div().flex_1().min_w_0().child(field::render(
                        &self.query,
                        "Search folders or type a path",
                        true,
                        &theme,
                        field::on_press(cx, |this, offset, clicks, _, cx| {
                            this.query.press(offset, clicks);
                            cx.notify();
                        }),
                    )))
                    .child(
                        div()
                            .id("launcher-browse")
                            .flex_none()
                            .h(theme.ui(28.))
                            .px(theme.ui(12.))
                            .flex()
                            .items_center()
                            .rounded(px(RADIUS_SMALL))
                            .border_1()
                            .border_color(rgb(theme.tokens.line))
                            .bg(rgb(theme.tokens.raised))
                            .hover(|style| style.border_color(rgb(theme.tokens.accent)))
                            .cursor_pointer()
                            .child("Browse…")
                            .on_click(
                                cx.listener(|this, event: &ClickEvent, _, cx| this.browse(event.modifiers().alt, cx)),
                            ),
                    ),
            )
            .when_some(self.notice(), |launcher, (text, error)| {
                launcher.child(
                    div()
                        .px_4()
                        .py(theme.ui(6.))
                        .text_xs()
                        .text_color(rgb(if error { theme.tokens.danger } else { theme.tokens.muted }))
                        .border_b_1()
                        .border_color(rgb(theme.tokens.line))
                        .child(text),
                )
            })
            .children(badge.map(|(folder, dir)| self.render_dir_list(&folder, dir, &theme, cx)))
            .child(if let Some((host, _)) = self.path_mode() {
                uniform_list(
                    "launcher-completions",
                    self.completions().len(),
                    cx.processor(move |this, range: Range<usize>, _window, cx| {
                        let rows = this.completions();
                        range
                            .filter_map(|index| {
                                rows.get(index).map(|dir| this.render_completion(index, dir, &host, cx))
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll_completions)
                .h(theme.ui(320.))
            } else {
                uniform_list(
                    "launcher-items",
                    self.items.len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        range.map(|index| this.render_item(index, cx)).collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll)
                .h(theme.ui(320.))
            });
        fade_in(card, "fade-launcher", FadeKind::Card, cx.global::<SettingsStore>().settings().animate_ui)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pick_applies_only_to_the_row_it_was_made_on() {
        let pick = (HostId::local(), PathBuf::from("/a"), 2);
        assert_eq!(pick_for(Some(&pick), &HostId::local(), Path::new("/a")), Some(2));
        assert_eq!(pick_for(Some(&pick), &HostId::local(), Path::new("/b")), None);
        assert_eq!(pick_for(Some(&pick), &HostId::named("devbox"), Path::new("/a")), None);
        assert_eq!(pick_for(None, &HostId::local(), Path::new("/a")), None);
    }
}
