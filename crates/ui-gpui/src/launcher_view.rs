//! The directory launcher overlay (`Ctrl+Shift+O`, and the New buttons): first the host (this machine or an SSH host,
//! connected in place), then a field and a Browse… button over that host's folders (open now, favorites, recent),
//! fuzzy filtered. Typing a path (`/…` or `~…`) lists that folder's subfolders instead.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use app_core::dir_complete::{self, Completion, TabAction};
use app_core::host_state::{HostDot, HostPick, HostState, away_line, host_pick};
use app_core::launcher::{self, LauncherItem, OpenFolder, RecentFolder, Typed};
use app_core::launcher_flow::{Escape, Flow, LauncherHost, LineTone, Picked, Step, host_line, sessions_label};
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

use crate::assets::FOLDER_ICON;
use crate::context_menu::host_dot_color;
use crate::field;
use crate::settings_store::SettingsStore;
use crate::theme::{FadeKind, RADIUS, RADIUS_LARGE, RADIUS_SMALL, Theme, fade_in, ssh_icon, theme};

/// Quiet time after typing before a folder listing is asked for.
const LIST_DELAY: Duration = Duration::from_millis(120);
/// Listing request ids, unique across launchers (a late reply to a closed launcher must not match a new one's).
static NEXT_LISTING: AtomicU64 = AtomicU64::new(1);

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
    /// Do what the host's pane card button does (connect, retry, cancel, install, update); the launcher stays open.
    HostAction {
        /// The host.
        host: HostId,
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
    /// The host's tag after the path, solid while the host is connected (`Some(true)`), dashed otherwise; none on a
    /// list of one host.
    pub host_tag: Option<bool>,
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
        children.extend(self.host_tag.and_then(|connected| host_tag(self.host, connected, theme)));
        children.extend(trailing);
        LauncherView::row(theme, id, self.selected, self.dimmed, children)
    }
}

/// The launcher overlay.
pub struct LauncherView {
    focus_handle: FocusHandle,
    /// Enter starts Claude and Alt+Enter a terminal; false swaps them.
    claude_first: bool,
    home: Option<PathBuf>,
    /// The steps, the hosts and the picked host's folder rows.
    flow: Flow,
    /// The folder step's field.
    query: TextField,
    scroll: UniformListScrollHandle,
    /// The newest folder listing, of `completion_host`.
    completion: Completion,
    completion_host: HostId,
    /// The highlighted completion row; `None` = the typed path itself.
    completion_selected: Option<usize>,
    /// The pause before a listing request.
    list_task: Option<gpui::Task<()>>,
    scroll_completions: UniformListScrollHandle,
    scroll_browse: UniformListScrollHandle,
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
    /// A launcher over the `open` folders and the `recent` ones of every host in `hosts` (this machine first); it
    /// starts on the host step when an SSH host is listed. `claude_first`: Enter starts Claude, Alt+Enter a terminal;
    /// false swaps them.
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
        let home = std::env::home_dir();
        LauncherView {
            focus_handle: cx.focus_handle(),
            claude_first,
            flow: Flow::new(hosts, open, recent, favorites, home.clone()),
            home,
            query: TextField::default(),
            scroll: UniformListScrollHandle::new(),
            completion: Completion::default(),
            completion_host: HostId::local(),
            completion_selected: None,
            list_task: None,
            scroll_completions: UniformListScrollHandle::new(),
            scroll_browse: UniformListScrollHandle::new(),
            dirs,
            memory,
            pick: None,
            badge_open: false,
        }
    }

    /// The view waits for a host to connect.
    pub fn is_waiting(&self) -> bool {
        self.flow.is_waiting()
    }

    /// The host of the folder step.
    fn folder_host(&self) -> HostId {
        self.flow.picked().cloned().unwrap_or_else(HostId::local)
    }

    /// The folder rows follow the text.
    fn refresh(&mut self) {
        self.flow.rebuild(self.query.text());
    }

    /// The folder step opened: an empty field, nothing listed yet.
    fn entered_folders(&mut self) {
        self.query = TextField::default();
        self.completion = Completion::default();
        self.completion_selected = None;
        self.list_task = None;
        self.badge_open = false;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    /// Update the hosts' states. The host being waited for, once connected, opens its folders; a typed path whose host
    /// just connected lists the typed path's parent.
    pub fn set_hosts(&mut self, hosts: Vec<LauncherHost>, cx: &mut Context<Self>) {
        let was_away = self.typed_host_away();
        if self.flow.set_hosts(hosts).is_some() {
            self.entered_folders();
        }
        if let Some(path) = self.flow.browse_resend() {
            self.browse_to(path, cx);
        }
        if was_away && !self.typed_host_away() {
            self.completion = Completion::default();
            self.typed_changed(cx);
        }
        cx.notify();
    }

    /// The open and recent folders changed: the rows follow, keeping the highlight, unless a path is typed or the
    /// browser is showing.
    pub fn set_folders(&mut self, open: Vec<OpenFolder>, recent: Vec<RecentFolder>, cx: &mut Context<Self>) {
        self.flow.set_folders(open, recent, self.query.text());
        cx.notify();
    }

    /// The field's hint for the picked host.
    fn placeholder(&self) -> String {
        match self.folder_host() {
            host if host.is_local() => "Search folders or type a path".to_string(),
            host => format!("Search folders on {} or type a path", host.name()),
        }
    }

    /// Pick host row `index`.
    fn pick_host(&mut self, index: usize, cx: &mut Context<Self>) {
        match self.flow.pick(index) {
            Picked::Nothing => {}
            Picked::Folders => self.entered_folders(),
            Picked::Action { host, dismiss } => {
                cx.emit(LauncherEvent::HostAction { host });
                if dismiss {
                    cx.emit(LauncherEvent::Dismiss);
                }
            }
        }
        cx.notify();
    }

    fn host_connected(&self, host: &HostId) -> bool {
        self.flow.connected(host)
    }

    /// The typed text when it is a path on the picked host (only in the folder step), with the host.
    fn path_mode(&self) -> Option<(HostId, String)> {
        let Step::Folders(host) = &self.flow.step else { return None };
        match launcher::typed(self.query.text()) {
            Typed::Path(path) => Some((host.clone(), path)),
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
    pub fn set_listing(&mut self, request: u64, path: PathBuf, dirs: Vec<DirEntry>, cx: &mut Context<Self>) {
        if let Some(browser) = self.flow.browser() {
            if browser.accept(request, path.to_string_lossy().into_owned(), dirs) {
                self.scroll_browse.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            }
            return;
        }
        if request != self.completion.request {
            return;
        }
        self.completion.accept_listing(request, dirs);
        self.completion_selected = None;
        cx.notify();
    }

    /// The newest listing request failed; replies to older requests are ignored.
    pub fn set_listing_failed(&mut self, request: u64, reason: String, cx: &mut Context<Self>) {
        if let Some(browser) = self.flow.browser() {
            if browser.fail(request, reason) {
                cx.notify();
            }
            return;
        }
        if request != self.completion.request {
            return;
        }
        self.completion.accept_failure(request, reason);
        cx.notify();
    }

    /// Complete the typed path with completion row `index`: its folder and a `/`, which asks for the next listing.
    fn accept_completion(&mut self, index: usize, cx: &mut Context<Self>) {
        let (Some((_, path)), Some(dir)) = (self.path_mode(), self.completions().get(index).cloned()) else {
            return;
        };
        let (parent, _) = dir_complete::split_typed(&path);
        self.query = TextField::default();
        self.query.insert(&format!("{parent}{}/", dir.name));
        self.badge_open = false;
        self.typed_changed(cx);
    }

    /// A path of `host` as a launch uses it: `~` expanded on this machine, a remote path as typed.
    fn launch_path(&self, host: &HostId, path: &str) -> PathBuf {
        let path = PathBuf::from(path);
        if host.is_local() { expand_home(&path, self.home.as_deref()) } else { path }
    }

    /// The folder the highlight is on: a completion row in path mode, else the highlighted list row.
    fn highlighted(&self) -> Option<(HostId, PathBuf)> {
        if let Some((host, path)) = self.path_mode() {
            let dir = self.completions().get(self.completion_selected?)?.clone();
            let full = format!("{}{}", dir_complete::split_typed(&path).0, dir.name);
            return Some((host.clone(), self.launch_path(&host, &full)));
        }
        let item = self.flow.items.get(self.flow.selected)?;
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
        let missing = matches!(self.flow.items.get(self.flow.selected), Some(LauncherItem::Dir { exists: false, .. }));
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

    fn choose(&mut self, index: usize, alt: bool, cx: &mut Context<Self>) {
        let claude = self.claude_first != alt;
        let (host, folder) = match self.flow.items.get(index) {
            Some(LauncherItem::Open(folder)) => (folder.host.clone(), folder.path.clone()),
            Some(LauncherItem::Dir { host, path, exists: true, .. }) => (host.clone(), path.clone()),
            _ => return,
        };
        let dir_pick = self.dir_pick_for(&host, &folder, claude);
        cx.emit(LauncherEvent::Open { host, dir: folder, claude, dir_pick });
    }

    /// Browse… (button or Ctrl+O): the folder browser on the picked host, starting at `~`.
    fn browse(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.flow.step, Step::Folders(_)) {
            return;
        }
        let host = self.folder_host();
        self.flow.to_browse(host);
        self.badge_open = false;
        self.list_task = None;
        self.browse_to("~".to_string(), cx);
        cx.notify();
    }

    /// List `path` in the browser (at once: it is a click or a key, not typing).
    fn browse_to(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(host) = self.flow.browser().map(|browser| browser.host.clone()) else { return };
        let request = NEXT_LISTING.fetch_add(1, Ordering::Relaxed);
        let connected = self.host_connected(&host);
        if let Some(browser) = self.flow.browser() {
            browser.ask(request, &path, connected);
        }
        if connected {
            cx.emit(LauncherEvent::ListDir { host, request, path });
        }
    }

    /// Open the browser's current folder; `alt` swaps Claude and terminal.
    fn open_here(&mut self, alt: bool, cx: &mut Context<Self>) {
        let claude = self.claude_first != alt;
        let Some(host) = self.flow.picked().cloned() else { return };
        let connected = self.host_connected(&host);
        let Some(dir) =
            self.flow.browser().filter(|browser| browser.can_open_here(connected)).and_then(|b| b.open_here())
        else {
            return;
        };
        cx.emit(LauncherEvent::Open { host, dir, claude, dir_pick: None });
    }

    /// Back from the browser to the host's folders.
    fn leave_browser(&mut self) {
        if let Some(host) = self.flow.picked().cloned() {
            self.flow.to_folders(host);
            self.entered_folders();
        }
    }

    /// A key in the browser.
    fn browse_key(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) {
        let Some(browser) = self.flow.browser() else { return };
        let empty = browser.filter.text().is_empty();
        match keystroke.key.as_str() {
            "up" => browser.step(false),
            "down" => browser.step(true),
            "enter" if keystroke.modifiers.secondary() => self.open_here(keystroke.modifiers.alt, cx),
            "enter" | "right" => {
                if let Some(path) = browser.child_path(browser.selected) {
                    self.browse_to(path, cx);
                }
            }
            "backspace" | "left" if empty => {
                if let Some(path) = browser.parent_path() {
                    self.browse_to(path, cx);
                }
            }
            "escape" => {
                if self.flow.escape(empty) == Escape::Back {
                    self.entered_folders();
                } else if let Some(browser) = self.flow.browser() {
                    browser.filter = TextField::default();
                    browser.selected = 0;
                }
            }
            _ => {
                if field::edit(&mut browser.filter, keystroke, cx) {
                    browser.selected = 0;
                }
            }
        }
        if let Some(browser) = self.flow.browser() {
            self.scroll_browse.scroll_to_item(browser.selected, ScrollStrategy::Center);
        }
    }

    /// Move the highlighted row's pick one dir on (or back).
    fn cycle_dir(&mut self, backwards: bool) {
        let Some((folder, current)) = self.badge() else { return };
        let count = self.dirs.dirs.len();
        let next = if backwards { (current + count - 1) % count } else { (current + 1) % count };
        self.pick = Some((HostId::local(), folder, next));
    }

    /// Esc: clear the field, else step back (hosts, folders), else close.
    fn escape(&mut self, cx: &mut Context<Self>) {
        let field_empty = match &self.flow.step {
            Step::Folders(_) => self.query.text().is_empty(),
            _ => true,
        };
        match self.flow.escape(field_empty) {
            Escape::ClearField => {
                self.query = TextField::default();
                self.refresh();
                self.typed_changed(cx);
            }
            Escape::Back => self.entered_folders(),
            Escape::Dismiss => cx.emit(LauncherEvent::Dismiss),
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if matches!(self.flow.step, Step::Browse(_)) {
            self.browse_key(keystroke, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if matches!(self.flow.step, Step::Hosts) {
            match keystroke.key.as_str() {
                "up" => self.flow.move_host_row(false),
                "down" => self.flow.move_host_row(true),
                "enter" => self.pick_host(self.flow.host_row, cx),
                "escape" => self.escape(cx),
                _ => return,
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
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
            "up" => self.flow.selected = launcher::step(&self.flow.items, self.flow.selected, false),
            "down" => self.flow.selected = launcher::step(&self.flow.items, self.flow.selected, true),
            "enter" if path_mode.is_some() => self.open_typed(keystroke.modifiers.alt, cx),
            "enter" => self.choose(self.flow.selected, keystroke.modifiers.alt, cx),
            "escape" if self.badge_open => self.badge_open = false,
            "escape" => self.escape(cx),
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
        self.scroll.scroll_to_item(self.flow.selected, ScrollStrategy::Center);
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
        if let Some(folder) = self.flow.open.iter().find(|folder| folder.host == *host && folder.path == path) {
            return div()
                .size(theme.ui(8.))
                .rounded_full()
                .map(|dot| match folder.tone {
                    Some(tone) => dot.bg(rgb(theme.tone(tone).dot)),
                    None => dot.border_1().border_color(rgb(theme.tokens.muted)),
                })
                .into_any_element();
        }
        let favorite =
            self.flow.favorites.iter().any(|key| split_project_key(key) == (host.clone(), path.to_path_buf()));
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
        let selected = index == self.flow.selected;
        let (host, path, dimmed) = match &self.flow.items[index] {
            LauncherItem::Open(folder) => (folder.host.clone(), folder.path.clone(), false),
            LauncherItem::Dir { host, path, exists, .. } => (host.clone(), path.clone(), !exists),
        };
        let row = FolderRow {
            host: &host,
            path: &path,
            home: self.home.as_deref(),
            host_tag: None,
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
        let host = self.folder_host();
        if self.host_connected(&host) {
            return None;
        }
        let state = self.flow.host(&host).map_or(HostState::NotConnected, |known| known.state.clone());
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

impl LauncherView {
    /// The card around a step: key handling, size and look.
    fn card(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        div()
            .key_context("Launcher")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(|this, _: &LauncherBrowse, _, cx| this.browse(cx)))
            // A sign-in dialog opened by the wait sits outside the card; a click in it must not close the launcher.
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if !this.flow.is_waiting() {
                    cx.emit(LauncherEvent::Dismiss);
                }
            }))
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
    }

    /// The name a host goes by in the launcher.
    fn host_name(host: &HostId) -> String {
        if host.is_local() { "This machine".to_string() } else { host.name().to_string() }
    }

    /// A small status dot in the colors of the sidebar's SSH hosts card.
    fn state_dot(dot: HostDot, theme: &Theme) -> Div {
        let ring = dot == HostDot::None;
        div()
            .flex_none()
            .size(theme.ui(8.))
            .rounded_full()
            .when(ring, |el| el.border_1().border_color(rgb(theme.tokens.muted)))
            .when(!ring, |el| el.bg(rgb(host_dot_color(theme, dot))))
    }

    /// The right slot of a host row: what picking it does.
    fn host_slot(&self, host: &LauncherHost, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let button = |label: &'static str, primary: bool| {
            div()
                .flex_none()
                .h(theme.ui(24.))
                .px(theme.ui(10.))
                .flex()
                .items_center()
                .rounded(px(RADIUS_SMALL))
                .border_1()
                .border_color(rgb(if primary { theme.tokens.accent } else { theme.tokens.line }))
                .when(primary, |el| el.bg(rgb(theme.tokens.accent)).text_color(rgb(theme.tokens.on_accent)))
                .when(!primary, |el| el.bg(rgb(theme.tokens.raised)))
                .child(label)
        };
        let accent = |label: &'static str| div().flex_none().text_color(rgb(theme.tokens.accent)).child(label);
        Some(match host_pick(&host.state) {
            HostPick::Open => div()
                .flex_none()
                .text_xs()
                .text_color(rgb(theme.tokens.muted))
                .child(sessions_label(host.sessions)?)
                .into_any_element(),
            HostPick::Connect => accent("Connect").into_any_element(),
            HostPick::Wait => return None,
            HostPick::Cancel => {
                let id = host.id.clone();
                button("Cancel", false)
                    .id(("launcher-host-cancel", 0usize))
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.flow.cancel(&id);
                        cx.emit(LauncherEvent::HostAction { host: id.clone() });
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .into_any_element()
            }
            HostPick::Retry => button("Retry", true).into_any_element(),
            HostPick::Install => accent("Install…").into_any_element(),
            HostPick::Update => accent("Update and restart").into_any_element(),
        })
    }

    /// One row of the host step.
    fn render_host(&self, index: usize, theme: &Theme, cx: &mut Context<Self>) -> Stateful<Div> {
        let host = self.flow.hosts[index].clone();
        let selected = index == self.flow.host_row;
        let (line, tone) = host_line(&host);
        let line_color = match tone {
            LineTone::Muted => theme.tokens.muted,
            LineTone::Busy => theme.tone(app_core::session_tree::Tone::Working).dot,
            LineTone::Bad => theme.tokens.danger,
        };
        let name = div()
            .flex()
            .items_center()
            .gap(theme.ui(6.))
            .font_weight(FontWeight::SEMIBOLD)
            .child(Self::host_name(&host.id))
            .when(!host.id.is_local(), |el| el.child(ssh_icon(theme.tokens.muted, theme)));
        div()
            .id(("launcher-host", index))
            .w_full()
            .h(theme.ui(54.))
            .px_2()
            .py(px(2.))
            .child(
                div()
                    .size_full()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap(theme.ui(10.))
                    .rounded(px(RADIUS_SMALL))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .cursor_pointer()
                    .when(selected, |el| el.bg(rgb(theme.tokens.raised)))
                    .when(!selected, |el| el.hover(|style| style.bg(rgb(theme.tokens.raised))))
                    .child(Self::state_dot(host.state.dot(), theme))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(name)
                            .child(div().truncate().text_xs().text_color(rgb(line_color)).child(line)),
                    )
                    .children(self.host_slot(&host, theme, cx))
                    .when(selected, |el| el.child(Self::enter_mark(theme))),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.pick_host(index, cx)))
    }

    /// The host step: the title and one row per host.
    fn render_hosts(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let title = if self.claude_first { "New Claude session" } else { "New terminal" };
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_4()
                    .py(theme.ui(10.))
                    .flex()
                    .flex_col()
                    .border_b_1()
                    .border_color(rgb(theme.tokens.line))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(div().text_xs().text_color(rgb(theme.tokens.muted)).child("Where should it run?")),
            )
            .child(
                div()
                    .id("launcher-hosts")
                    .py(px(4.))
                    .max_h(theme.ui(420.))
                    .overflow_y_scroll()
                    .children((0..self.flow.hosts.len()).map(|index| self.render_host(index, theme, cx))),
            )
    }

    /// The folder step's top line: back to the hosts, and the host the folders are on.
    fn render_host_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let host = self.folder_host();
        let known = self.flow.host(&host);
        let dot = known.map_or(HostDot::None, |known| known.state.dot());
        div()
            .h(theme.ui(40.))
            .px_4()
            .flex()
            .items_center()
            .gap(theme.ui(10.))
            .border_b_1()
            .border_color(rgb(theme.tokens.line))
            .child(
                div()
                    .id("launcher-back-hosts")
                    .flex_none()
                    .cursor_pointer()
                    .text_color(rgb(theme.tokens.muted))
                    .hover(|style| style.text_color(rgb(theme.tokens.text)))
                    .child("← Hosts")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.flow.to_hosts();
                        cx.notify();
                    })),
            )
            .child(Self::state_dot(dot, theme))
            .child(div().flex_none().font_weight(FontWeight::SEMIBOLD).child(Self::host_name(&host)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(rgb(theme.tokens.muted))
                    .child(known.map(|known| known.detail.clone()).unwrap_or_default()),
            )
    }

    /// A folder name with the characters at `bold` in bold.
    fn folder_name(name: &str, bold: &[u32]) -> Div {
        let mut runs: Vec<(String, bool)> = Vec::new();
        for (at, ch) in name.chars().chain(['/']).enumerate() {
            let is_bold = bold.contains(&(at as u32));
            match runs.last_mut() {
                Some((text, last)) if *last == is_bold => text.push(ch),
                _ => runs.push((ch.to_string(), is_bold)),
            }
        }
        div().flex().children(
            runs.into_iter()
                .map(|(text, is_bold)| div().when(is_bold, |run| run.font_weight(FontWeight::BOLD)).child(text)),
        )
    }

    /// One subfolder row of the browser.
    fn render_browse_row(&self, row: usize, theme: &Theme, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let Step::Browse(browser) = &self.flow.step else { return None };
        let shown = browser.shown();
        let item = shown.get(row)?;
        let dir = browser.dirs.get(item.index)?;
        let selected = row == browser.selected;
        let children = vec![
            gpui::svg()
                .path(FOLDER_ICON)
                .flex_none()
                .size(theme.ui(14.))
                .text_color(rgb(theme.tokens.muted))
                .into_any_element(),
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .when(item.dimmed, |name| name.text_color(rgb(theme.tokens.muted)))
                .child(Self::folder_name(&dir.name, &item.bold))
                .into_any_element(),
        ];
        let mut children = children;
        if selected {
            children.push(Self::enter_mark(theme));
        }
        Some(Self::row(theme, ("launcher-browse-row", row), selected, false, children).on_click(cx.listener(
            move |this, _: &ClickEvent, _, cx| {
                if let Some(path) = this.flow.browser().and_then(|browser| browser.child_path(row)) {
                    this.browse_to(path, cx);
                    cx.notify();
                }
            },
        )))
    }

    /// The folder browser: top line, breadcrumbs, filter, subfolders and the Open here footer.
    fn render_browser(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let Step::Browse(browser) = &self.flow.step else { return div() };
        let host = browser.host.clone();
        let connected = self.host_connected(&host);
        let known = self.flow.host(&host);
        let dot = known.map_or(HostDot::None, |known| known.state.dot());
        let title = if host.is_local() { "Browse this machine".to_string() } else { format!("Browse {}", host.name()) };
        let crumbs = browser.crumbs();
        let last = crumbs.len().saturating_sub(1);
        let error = if connected {
            browser.error.clone()
        } else {
            Some(away_line(host.name(), &known.map_or(HostState::NotConnected, |known| known.state.clone())))
        };
        let can_open = browser.can_open_here(connected);
        let open_label = format!("Open here: {}", browser.shown_path().unwrap_or_default());
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(theme.ui(40.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap(theme.ui(10.))
                    .border_b_1()
                    .border_color(rgb(theme.tokens.line))
                    .child(
                        div()
                            .id("launcher-back-folders")
                            .flex_none()
                            .cursor_pointer()
                            .text_color(rgb(theme.tokens.muted))
                            .hover(|style| style.text_color(rgb(theme.tokens.text)))
                            .child("← Folders")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.leave_browser();
                                cx.notify();
                            })),
                    )
                    .child(Self::state_dot(dot, theme))
                    .child(div().flex_none().font_weight(FontWeight::SEMIBOLD).child(title)),
            )
            .child(div().px_4().py(theme.ui(6.)).flex().flex_wrap().items_center().gap(theme.ui(4.)).children(
                crumbs.into_iter().enumerate().flat_map(|(index, crumb)| {
                    let separator =
                        (index > 0).then(|| div().text_color(rgb(theme.tokens.muted)).child("›").into_any_element());
                    let label = if index == last {
                        div().font_weight(FontWeight::BOLD).child(crumb.label).into_any_element()
                    } else {
                        let path = crumb.path;
                        div()
                            .id(("launcher-crumb", index))
                            .cursor_pointer()
                            .text_color(rgb(theme.tokens.muted))
                            .hover(|style| style.text_color(rgb(theme.tokens.text)))
                            .child(crumb.label)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.browse_to(path.clone(), cx);
                                cx.notify();
                            }))
                            .into_any_element()
                    };
                    separator.into_iter().chain([label])
                }),
            ))
            .when_some(error, |card, text| {
                card.child(div().px_4().pb(theme.ui(6.)).text_xs().text_color(rgb(theme.tokens.danger)).child(text))
            })
            .child(
                div()
                    .h(theme.ui(40.))
                    .px_4()
                    .flex()
                    .items_center()
                    .border_y_1()
                    .border_color(rgb(theme.tokens.line))
                    .child(div().flex_1().min_w_0().child(field::render(
                        &browser.filter,
                        "Filter folders",
                        true,
                        theme,
                        field::on_press(cx, |this, offset, clicks, _, cx| {
                            if let Some(browser) = this.flow.browser() {
                                browser.filter.press(offset, clicks);
                            }
                            cx.notify();
                        }),
                    ))),
            )
            .child(
                uniform_list(
                    "launcher-browse-list",
                    browser.shown().len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        let theme = crate::theme::theme(cx).clone();
                        range.filter_map(|row| this.render_browse_row(row, &theme, cx)).collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll_browse)
                .h(theme.ui(280.)),
            )
            .child(
                div()
                    .px_4()
                    .py(theme.ui(8.))
                    .flex()
                    .items_center()
                    .gap(theme.ui(10.))
                    .border_t_1()
                    .border_color(rgb(theme.tokens.line))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(rgb(theme.tokens.muted))
                            .child("↵ / → enter · ⌫ up a level · Ctrl+↵ open here"),
                    )
                    .child(
                        div()
                            .id("launcher-open-here")
                            .flex_none()
                            .max_w(theme.ui(280.))
                            .truncate()
                            .h(theme.ui(28.))
                            .px(theme.ui(12.))
                            .flex()
                            .items_center()
                            .rounded(px(RADIUS_SMALL))
                            .when(can_open, |button| {
                                button
                                    .bg(rgb(theme.tokens.accent))
                                    .text_color(rgb(theme.tokens.on_accent))
                                    .cursor_pointer()
                            })
                            .when(!can_open, |button| {
                                button.bg(rgb(theme.tokens.raised)).text_color(rgb(theme.tokens.muted))
                            })
                            .child(open_label)
                            .on_click(
                                cx.listener(|this, event: &ClickEvent, _, cx| {
                                    this.open_here(event.modifiers().alt, cx)
                                }),
                            ),
                    ),
            )
    }

    /// The folder step: the host line, the field with Browse…, the notice and the host's folders (or the typed path's).
    fn render_folders(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let badge = self.badge().filter(|_| self.badge_open);
        div()
            .flex()
            .flex_col()
            .when(self.flow.has_host_step(), |card| card.child(self.render_host_bar(theme, cx)))
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
                        self.placeholder(),
                        true,
                        theme,
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
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.browse(cx))),
                    ),
            )
            .when_some(self.notice(), |card, (text, error)| {
                card.child(
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
            .children(badge.map(|(folder, dir)| self.render_dir_list(&folder, dir, theme, cx)))
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
                    self.flow.items.len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        range.map(|index| this.render_item(index, cx)).collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll)
                .h(theme.ui(320.))
            })
    }
}

impl Render for LauncherView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        let body = match &self.flow.step {
            Step::Hosts => self.render_hosts(&theme, cx),
            Step::Folders(_) => self.render_folders(&theme, cx),
            Step::Browse(_) => self.render_browser(&theme, cx),
        };
        let card = self.card(&theme, cx).child(body);
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
