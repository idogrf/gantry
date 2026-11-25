//! Preview panes in the window: opening one beside a Claude pane, keeping its model current as the session list
//! changes, titles for its rows, and telling each daemon which sessions to watch for sent files.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use app_core::menus::{hidden_tabs_menu, preview_menu};
use app_core::preview::{
    FileKind, ImageType, MAX_IMAGE_PIXELS, MAX_UNKNOWN_TEXT, TextDoc, file_name, image_size, is_read, kind_of,
    newly_sent, row_title,
};
use app_core::preview_reads::{FileKey, POLL, Shown};
use app_core::session_tree::session_title;
use app_core::split_tree::{Axis, NodeId};
use app_core::workspace::PreviewPlace;
use gpui::{App, AppContext, ClipboardItem, Context, Task, Window};
use protocol::host::HostId;
use protocol::message::SentFile;

use super::layout::start_rects;
use super::{PreviewSlot, WorkspaceView};
use crate::context_menu::OpenMenu;
use crate::preview_view::{Content, PreviewEvent, PreviewModel, PreviewTab, PreviewView};
use crate::theme::theme;

/// What to send each host's daemon with `WatchSentFiles`: the hosts (among `connected`) whose wanted session list
/// differs from the one `sent` last, with the new list (empty when the host lost its last preview). A `fresh` host
/// (its session list just arrived, so possibly a daemon that still watches old sessions) always gets its list.
fn watch_changes(
    want: &BTreeMap<HostId, Vec<String>>,
    sent: &HashMap<HostId, Vec<String>>,
    fresh: &HashSet<HostId>,
    connected: &[HostId],
) -> Vec<(HostId, Vec<String>)> {
    let hosts: BTreeSet<&HostId> = want.keys().chain(sent.keys()).chain(fresh.iter()).collect();
    hosts
        .into_iter()
        .filter(|host| connected.contains(host))
        .filter_map(|host| {
            let wanted = want.get(host).cloned().unwrap_or_default();
            let before = sent.get(host).map_or(&[][..], Vec::as_slice);
            (fresh.contains(host) || wanted != before).then(|| (host.clone(), wanted))
        })
        .collect()
}

/// Whether two models draw the same thing, so the view need not be given the new one.
fn same_model(a: &PreviewModel, b: &PreviewModel) -> bool {
    a.title == b.title
        && a.active == b.active
        && a.host_card == b.host_card
        && a.can_open == b.can_open
        && same_content(&a.content, &b.content)
        && a.tabs.len() == b.tabs.len()
        && a.tabs.iter().zip(&b.tabs).all(|(x, y)| {
            (&x.id, &x.name, &x.path, &x.caption, x.unseen) == (&y.id, &y.name, &y.path, &y.caption, y.unseen)
        })
}

/// Whether two contents draw the same thing: the same parsed text or decoded image (a reload makes a new one), or
/// equal payloads.
fn same_content(a: &Content, b: &Content) -> bool {
    match (a, b) {
        (Content::Text(a), Content::Text(b)) => Arc::ptr_eq(a, b),
        (Content::Image { image: a, natural: x }, Content::Image { image: b, natural: y }) => {
            Arc::ptr_eq(a, b) && x == y
        }
        (Content::TooLarge(a), Content::TooLarge(b)) => a == b,
        (Content::Unsupported { size: a }, Content::Unsupported { size: b }) => a == b,
        (Content::Failed(a), Content::Failed(b)) => a == b,
        _ => std::mem::discriminant(a) == std::mem::discriminant(b),
    }
}

/// What the pane shows for `path` given the last reply about it (`None` = no reply yet).
fn content_for(path: &Path, shown: Option<&Shown>) -> Content {
    let kind = kind_of(path);
    if !is_read(kind) {
        return if kind == FileKind::Html { Content::Html } else { Content::Unsupported { size: None } };
    }
    let Some(shown) = shown else { return Content::Loading };
    let (data, len) = match shown {
        Shown::Missing => return Content::Missing,
        Shown::TooLarge { size } => return Content::TooLarge(Some(*size)),
        Shown::Failed { reason } => return Content::Failed(reason.clone()),
        Shown::Bytes { data, .. } => (data, data.len() as u64),
    };
    let unsupported = Content::Unsupported { size: Some(len) };
    let text =
        |data: &[u8]| TextDoc::parse(data).map_or_else(|| unsupported.clone(), |doc| Content::Text(Arc::new(doc)));
    match kind {
        FileKind::Image(kind) => {
            let Some((width, height)) = image_size(data, kind) else {
                return Content::Failed("not a readable image".into());
            };
            if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
                return Content::TooLarge(None);
            }
            let format = match kind {
                ImageType::Png => gpui::ImageFormat::Png,
                ImageType::Jpeg => gpui::ImageFormat::Jpeg,
                ImageType::Gif => gpui::ImageFormat::Gif,
                ImageType::Webp => gpui::ImageFormat::Webp,
                ImageType::Svg => gpui::ImageFormat::Svg,
            };
            Content::Image {
                image: Arc::new(gpui::Image::from_bytes(format, data.to_vec())),
                natural: (width as f32, height as f32),
            }
        }
        FileKind::Text => text(data),
        FileKind::Unknown if data.len() <= MAX_UNKNOWN_TEXT => text(data),
        _ => unsupported,
    }
}

/// Where the copy of a remote file for the system viewer goes: one folder per host and file id, so two files with the
/// same name never overwrite each other.
fn temp_copy_path(root: &Path, host: &str, id: &str, name: &str) -> PathBuf {
    root.join("gantry-preview").join(host).join(id.replace(':', "-")).join(name)
}

impl WorkspaceView {
    /// The timer that asks for the shown files again every `POLL`. It only reads state and sends requests.
    pub(super) fn start_preview_timer(window: &mut Window, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                if this.update_in(cx, |view, window, cx| view.preview_tick(window, cx)).is_err() {
                    return;
                }
            }
        })
    }

    /// The files to keep read: with the window active, the active file of each preview pane in the visible tab whose
    /// host is connected and whose type is read at all. Nothing else is ever read.
    fn wanted_reads(&self, window: &Window, cx: &App) -> Vec<FileKey> {
        if !window.is_window_active() {
            return Vec::new();
        }
        let Some(tab) = self.workspace.active_tab().filter(|tab| !tab.settings) else { return Vec::new() };
        let sessions = self.sidebar.read(cx).sessions();
        tab.root
            .leaves()
            .into_iter()
            .filter(|leaf| self.previews.contains_key(leaf))
            .filter_map(|leaf| {
                let host = &self.workspace.leaves.get(&leaf)?.host;
                self.client(host)?;
                let file = self.workspace.preview_active(leaf, sessions)?;
                is_read(kind_of(&file.path)).then(|| FileKey { host: host.clone(), path: file.path.clone() })
            })
            .collect()
    }

    fn send_read(&self, key: &FileKey, request: u64, known: Option<u64>) {
        if let Some(client) = self.client(&key.host) {
            client.read_file(request, &key.path.to_string_lossy(), known);
        }
    }

    /// Ask for the wanted files that are due (the timer).
    fn preview_tick(&mut self, window: &Window, cx: &mut Context<Self>) {
        let wanted = self.wanted_reads(window, cx);
        for (request, key, known) in self.reads.due(&wanted, Instant::now()) {
            self.send_read(&key, request, known);
        }
    }

    /// Ask at once for a shown file the cache does not hold yet (a tab click, a new send, a pane coming into view).
    fn ask_previews(&mut self, window: &Window, cx: &mut Context<Self>) {
        for key in self.wanted_reads(window, cx) {
            if self.reads.shown(&key).is_none()
                && let Some((request, known)) = self.reads.request_now(&key, Instant::now())
            {
                self.send_read(&key, request, known);
            }
        }
    }

    /// What the previews in view need after the session list, the tab or the window changed: their files marked seen
    /// and the ones not cached asked for.
    pub(super) fn previews_in_view(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.mark_previews_seen(window, cx);
        self.ask_previews(window, cx);
    }

    /// With the window active, each preview in the visible tab has seen its active file.
    fn mark_previews_seen(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !window.is_window_active() {
            return;
        }
        let Some(tab) = self.workspace.active_tab().filter(|tab| !tab.settings) else { return };
        let sessions = self.sidebar.read(cx).sessions();
        let seen: Vec<(String, String)> = tab
            .root
            .leaves()
            .into_iter()
            .filter(|leaf| self.previews.contains_key(leaf))
            .filter_map(|leaf| {
                let (_, session) = self.workspace.preview_session(leaf)?;
                Some((session.to_string(), self.workspace.preview_active(leaf, sessions)?.id.clone()))
            })
            .collect();
        let mut changed = false;
        for (session, id) in seen {
            changed |= self.workspace.mark_seen(&session, &id);
        }
        if changed {
            self.save(cx);
            self.push_preview_models(cx);
        }
    }

    /// A reply to a `ReadFile`: decode what changed once, show it, and finish an Open in system viewer that waited.
    pub(super) fn on_file_contents(
        &mut self,
        host: &HostId,
        request: u64,
        body: protocol::message::FileBody,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.reads.apply(host, request, body);
        if let Some(key) = &changed {
            let content = content_for(&key.path, self.reads.shown(key));
            self.decoded.insert(key.clone(), content);
            // Bytes the cache evicted are not kept decoded either.
            let reads = &self.reads;
            self.decoded.retain(|key, _| reads.shown(key).is_some());
        }
        // A wish whose request is over is answered (or dropped when the file could not be read).
        let waiting: Vec<FileKey> =
            self.open_after_read.keys().filter(|key| !self.reads.in_flight(key)).cloned().collect();
        for key in waiting {
            let Some(id) = self.open_after_read.remove(&key) else { continue };
            if let Some(Shown::Bytes { data, .. }) = self.reads.shown(&key) {
                Self::open_copy(&key, &id, data, cx);
            }
        }
        if changed.is_some() {
            self.push_preview_models(cx);
            cx.notify();
        }
    }

    /// Write a remote file's bytes to a temporary copy and open that in the system viewer.
    fn open_copy(key: &FileKey, id: &str, data: &[u8], cx: &mut Context<Self>) {
        let path = temp_copy_path(&std::env::temp_dir(), key.host.name(), id, &file_name(&key.path));
        let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&path, data));
        match written {
            Ok(()) => cx.open_with_system(&path),
            Err(err) => tracing::warn!("writing the preview copy failed: {err}"),
        }
    }

    /// The file preview `leaf` shows, as its key and sent-file id.
    fn active_file(&self, leaf: NodeId, cx: &App) -> Option<(FileKey, String)> {
        let host = self.workspace.leaves.get(&leaf)?.host.clone();
        let file = self.workspace.preview_active(leaf, self.sidebar.read(cx).sessions())?;
        Some((FileKey { host, path: file.path.clone() }, file.id.clone()))
    }

    /// Open preview `leaf`'s active file in the system viewer: a local file directly, a remote one as a copy.
    fn open_in_system_viewer(&mut self, leaf: NodeId, cx: &mut Context<Self>) {
        let Some((key, id)) = self.active_file(leaf, cx) else { return };
        if key.host.is_local() {
            cx.open_with_system(&key.path);
            return;
        }
        if let Some(Shown::Bytes { data, .. }) = self.reads.shown(&key) {
            Self::open_copy(&key, &id, data, cx);
            return;
        }
        if let Some((request, known)) = self.reads.request_now(&key, Instant::now()) {
            self.send_read(&key, request, known);
        }
        self.open_after_read.insert(key, id);
    }

    /// Show file `id` in preview `leaf`.
    pub(super) fn activate_file(&mut self, leaf: NodeId, id: &str, window: &Window, cx: &mut Context<Self>) {
        self.workspace.set_preview_active(leaf, id);
        self.save(cx);
        self.mark_previews_seen(window, cx);
        self.push_preview_models(cx);
        self.ask_previews(window, cx);
    }

    /// Hide file `id` from preview `leaf`'s list.
    pub(super) fn remove_file(&mut self, leaf: NodeId, id: &str, window: &Window, cx: &mut Context<Self>) {
        self.workspace.remove_sent(leaf, id, self.sidebar.read(cx).sessions());
        self.save(cx);
        self.previews_in_view(window, cx);
        self.push_preview_models(cx);
    }

    /// Hide every file of preview `leaf`'s session.
    pub(super) fn clear_files(&mut self, leaf: NodeId, window: &Window, cx: &mut Context<Self>) {
        self.workspace.clear_sent(leaf, self.sidebar.read(cx).sessions());
        self.save(cx);
        self.previews_in_view(window, cx);
        self.push_preview_models(cx);
    }

    /// Put the active text file's contents on the clipboard.
    pub(super) fn copy_file_contents(&mut self, leaf: NodeId, cx: &mut Context<Self>) {
        let Some(slot) = self.previews.get(&leaf) else { return };
        if let Content::Text(doc) = &slot.view.read(cx).model().content {
            cx.write_to_clipboard(ClipboardItem::new_string(doc.text().to_string()));
        }
    }

    /// Show a pane menu made of `items` at `position`.
    fn open_preview_menu(
        &mut self,
        items: Vec<app_core::menus::MenuItem>,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        self.pane_menu = Some(OpenMenu::new(items, position, focus));
        cx.notify();
    }

    /// Create the view of preview leaf `leaf`.
    pub(super) fn add_preview(&mut self, leaf: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| PreviewView::new(leaf, cx));
        let subscription = cx.subscribe_in(&view, window, move |this, _, event: &PreviewEvent, window, cx| {
            this.on_preview_event(leaf, event, window, cx)
        });
        self.previews.insert(leaf, PreviewSlot { view, _subscription: subscription });
    }

    /// Open (or focus) the preview pane that follows Claude pane `claude`. Does nothing unless `claude` is a Claude
    /// leaf of a tab (`Workspace::open_preview` is the one rule).
    pub(super) fn open_preview_for(&mut self, claude: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.drawn_rects();
        let Some(place) = self.workspace.open_preview(claude) else { return };
        match place {
            PreviewPlace::Existing(_) => {}
            PreviewPlace::Split { leaf, beside } => {
                // Only a tab that was on screen slides the new pane in.
                if from.iter().any(|(drawn, _)| *drawn == beside) {
                    self.start_pane_anim(
                        start_rects(&from, beside, leaf, Axis::Horizontal),
                        Some((leaf, Axis::Horizontal)),
                        window,
                        cx,
                    );
                }
                self.add_preview(leaf, window, cx);
            }
            PreviewPlace::NewTab(leaf) => self.add_preview(leaf, window, cx),
        }
        self.update_previews(cx);
        self.focus_current(window, cx);
        self.save(cx);
    }

    /// A preview pane's row title (and the title bar's): `Preview · <file>`, else a terminal's title.
    pub(super) fn leaf_title(&self, leaf: NodeId, cx: &App) -> String {
        let Some(entry) = self.workspace.leaves.get(&leaf) else { return String::new() };
        if entry.preview.is_some() {
            let sessions = self.sidebar.read(cx).sessions();
            return row_title(self.workspace.preview_active(leaf, sessions));
        }
        entry.name.clone().unwrap_or_else(|| {
            self.slots.get(&leaf).map_or_else(|| entry.launch.label(), |slot| slot.view.read(cx).title())
        })
    }

    /// The model of preview pane `leaf` from the session list, the layout and its host's state.
    pub(super) fn preview_model(&self, leaf: NodeId, cx: &App) -> PreviewModel {
        let sessions = self.sidebar.read(cx).sessions();
        let info = self.workspace.preview_session(leaf).and_then(|(host, id)| sessions.get(host, id));
        let seen_in = info.map(|info| info.id.as_str());
        let tabs: Vec<PreviewTab> = self
            .workspace
            .preview_files(leaf, sessions)
            .into_iter()
            .map(|file| PreviewTab {
                id: file.id.clone(),
                name: file_name(&file.path),
                path: file.path.clone(),
                caption: file.caption.clone(),
                unseen: !seen_in.is_some_and(|session| self.workspace.is_seen(session, &file.id)),
            })
            .collect();
        let active = self.workspace.preview_active(leaf, sessions).map(|file| file.id.clone());
        let host = self.workspace.leaves.get(&leaf).map(|entry| entry.host.clone());
        let file = self.workspace.preview_active(leaf, sessions);
        let content = match (&host, file) {
            (Some(host), Some(file)) => {
                let key = FileKey { host: host.clone(), path: file.path.clone() };
                match self.decoded.get(&key) {
                    Some(content) => content.clone(),
                    None => content_for(&file.path, None),
                }
            }
            _ => Content::NoFiles,
        };
        let can_open = match (&host, file) {
            (Some(host), Some(_)) if !matches!(content, Content::Missing) => {
                host.is_local() || (self.client(host).is_some() && !matches!(content, Content::TooLarge(_)))
            }
            _ => false,
        };
        PreviewModel {
            title: info.map_or_else(|| "Preview".to_string(), |info| format!("Preview · {}", session_title(info))),
            tabs,
            active,
            content,
            host_card: self.workspace.leaves.get(&leaf).and_then(|entry| self.host_card(&entry.host)),
            can_open,
        }
    }

    /// The project directory a preview's sidebar entry sits in: the shown session's, else the pane's.
    pub(super) fn preview_cwd(&self, leaf: NodeId, cx: &App) -> PathBuf {
        let sessions = self.sidebar.read(cx).sessions();
        self.workspace
            .preview_session(leaf)
            .and_then(|(host, session)| sessions.get(host, session))
            .map(|info| info.cwd.clone())
            .or_else(|| self.workspace.leaves.get(&leaf).map(|entry| entry.launch.cwd.clone()))
            .unwrap_or_default()
    }

    /// The session list or a host's state changed: jump each preview to a file sent since it last looked, push the
    /// models that changed, refresh the sidebar entries whose row changed and tell the daemons what to watch.
    pub(super) fn update_previews(&mut self, cx: &mut Context<Self>) {
        let leaves: Vec<NodeId> = self.previews.keys().copied().collect();
        let mut saved = false;
        for leaf in &leaves {
            let listed: Option<Vec<SentFile>> = self
                .workspace
                .preview_session(*leaf)
                .and_then(|(host, id)| self.sidebar.read(cx).sessions().get(host, id))
                .map(|info| info.sent_files.clone());
            if let Some(files) = listed {
                if let Some(newest) = newly_sent(self.preview_known.get(leaf), &files) {
                    saved |= self.workspace.set_preview_active(*leaf, &newest);
                }
                self.preview_known.insert(*leaf, files.iter().map(|file| file.id.clone()).collect::<HashSet<_>>());
            }
        }
        let rows_changed = self.push_preview_models(cx);
        self.sync_watch(cx);
        if saved {
            self.save(cx); // also refreshes the sidebar entries
        } else if rows_changed {
            self.sync_panes(cx);
        }
    }

    /// Give each preview view its model when something it draws changed; whether a sidebar entry's row changed.
    fn push_preview_models(&mut self, cx: &mut Context<Self>) -> bool {
        let leaves: Vec<NodeId> = self.previews.keys().copied().collect();
        let mut rows_changed = false;
        for leaf in &leaves {
            let model = self.preview_model(*leaf, cx);
            if let Some(slot) = self.previews.get(leaf)
                && !same_model(slot.view.read(cx).model(), &model)
            {
                slot.view.update(cx, |view, cx| view.set_model(model, cx));
            }
            let row = (self.leaf_title(*leaf, cx), self.preview_cwd(*leaf, cx));
            rows_changed |= self.preview_rows.insert(*leaf, row.clone()) != Some(row);
        }
        rows_changed
    }

    /// Send each connected host the sessions its preview panes follow, when that list changed.
    pub(super) fn sync_watch(&mut self, _cx: &mut Context<Self>) {
        let connected: Vec<HostId> =
            self.hosts.iter().filter(|(_, conn)| conn.connected).map(|(host, _)| host.clone()).collect();
        let changes =
            watch_changes(&self.workspace.watched_sessions(), &self.watch_sent, &self.watch_fresh, &connected);
        for (host, sessions) in changes {
            self.watch_fresh.remove(&host);
            if let Some(client) = self.client(&host) {
                client.watch_sent_files(&sessions);
            }
            if sessions.is_empty() {
                self.watch_sent.remove(&host);
            } else {
                self.watch_sent.insert(host, sessions);
            }
        }
    }

    /// Something happened in preview pane `leaf`.
    fn on_preview_event(&mut self, leaf: NodeId, event: &PreviewEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            PreviewEvent::Focused => self.focus_leaf(leaf, window, cx),
            PreviewEvent::Close => self.close_leaf(leaf, window, cx),
            PreviewEvent::HostAction => {
                if let Some(host) = self.workspace.leaves.get(&leaf).map(|entry| entry.host.clone()) {
                    self.run_host_action(&host, window, cx);
                }
            }
            PreviewEvent::Activate(id) => self.activate_file(leaf, id, window, cx),
            PreviewEvent::Remove(id) => self.remove_file(leaf, id, window, cx),
            PreviewEvent::Menu { position, file } => {
                let text = self
                    .previews
                    .get(&leaf)
                    .is_some_and(|slot| matches!(slot.view.read(cx).model().content, Content::Text(_)));
                self.open_preview_menu(preview_menu(leaf, file.as_deref(), text), *position, window, cx);
            }
            PreviewEvent::Hidden { position } => {
                let Some(slot) = self.previews.get(&leaf) else { return };
                let files = slot.view.read(cx).hidden_tabs(theme(cx));
                self.open_preview_menu(hidden_tabs_menu(leaf, &files), *position, window, cx);
            }
            PreviewEvent::OpenExternal => self.open_in_system_viewer(leaf, cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str) -> HostId {
        HostId::named(name)
    }

    fn want(entries: &[(&str, &[&str])]) -> BTreeMap<HostId, Vec<String>> {
        entries.iter().map(|(h, s)| (host(h), s.iter().map(|s| s.to_string()).collect())).collect()
    }

    fn sent(entries: &[(&str, &[&str])]) -> HashMap<HostId, Vec<String>> {
        want(entries).into_iter().collect()
    }

    #[test]
    fn watch_is_sent_only_on_change_and_to_connected_hosts() {
        let connected = [HostId::local(), host("box")];
        let none = HashSet::new();
        // Unchanged: nothing.
        let same = want(&[("box", &["a", "b"])]);
        assert!(watch_changes(&same, &sent(&[("box", &["a", "b"])]), &none, &connected).is_empty());
        // Nothing wanted, nothing sent: nothing.
        assert!(watch_changes(&BTreeMap::new(), &HashMap::new(), &none, &connected).is_empty());
        // A new session list is sent.
        assert_eq!(
            watch_changes(&same, &sent(&[("box", &["a"])]), &none, &connected),
            vec![(host("box"), vec!["a".to_string(), "b".to_string()])]
        );
        // A host that lost its last preview is told to watch nothing.
        assert_eq!(
            watch_changes(&BTreeMap::new(), &sent(&[("box", &["a"])]), &none, &connected),
            vec![(host("box"), vec![])]
        );
        // A host that is not connected is skipped, in both directions.
        let away = want(&[("away", &["x"])]);
        assert!(watch_changes(&away, &HashMap::new(), &none, &connected).is_empty());
        assert!(watch_changes(&BTreeMap::new(), &sent(&[("away", &["x"])]), &none, &connected).is_empty());
    }

    #[test]
    fn a_fresh_host_gets_its_list_once_even_when_empty_or_unchanged() {
        let connected = [host("box")];
        let fresh: HashSet<HostId> = [host("box")].into();
        assert_eq!(watch_changes(&BTreeMap::new(), &HashMap::new(), &fresh, &connected), vec![(host("box"), vec![])]);
        let same = want(&[("box", &["a"])]);
        assert_eq!(
            watch_changes(&same, &sent(&[("box", &["a"])]), &fresh, &connected),
            vec![(host("box"), vec!["a".to_string()])]
        );
        // Not connected yet: it waits.
        assert!(watch_changes(&BTreeMap::new(), &HashMap::new(), &fresh, &[]).is_empty());
    }

    fn model() -> PreviewModel {
        PreviewModel {
            title: "Preview".into(),
            tabs: vec![PreviewTab {
                id: "a".into(),
                name: "a.md".into(),
                path: "/r/a.md".into(),
                caption: None,
                unseen: true,
            }],
            active: Some("a".into()),
            content: Content::Loading,
            host_card: None,
            can_open: false,
        }
    }

    #[test]
    fn a_model_is_pushed_only_when_something_it_draws_changed() {
        assert!(same_model(&model(), &model()));
        let mut changed = model();
        changed.tabs[0].unseen = false;
        assert!(!same_model(&model(), &changed));
        let mut changed = model();
        changed.title = "Preview · x".into();
        assert!(!same_model(&model(), &changed));
        let mut changed = model();
        changed.content = Content::NoFiles;
        assert!(!same_model(&model(), &changed));
        let mut changed = model();
        changed.tabs.clear();
        assert!(!same_model(&model(), &changed));
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        out.extend(width.to_be_bytes());
        out.extend(height.to_be_bytes());
        out.extend([8, 6, 0, 0, 0]);
        out
    }

    fn bytes(data: Vec<u8>) -> Shown {
        Shown::Bytes { data: data.into(), mtime_ms: 1 }
    }

    #[test]
    fn content_for_maps_every_answer() {
        let p = Path::new;
        assert!(matches!(content_for(p("/a.png"), Some(&bytes(png(1, 1)))), Content::Image { natural: (1., 1.), .. }));
        assert!(matches!(content_for(p("/a.png"), Some(&bytes(png(20000, 20000)))), Content::TooLarge(None)));
        assert!(matches!(content_for(p("/a.png"), Some(&bytes(b"junk".to_vec()))), Content::Failed(_)));
        assert!(matches!(content_for(p("/a.md"), Some(&bytes(b"# hi".to_vec()))), Content::Text(_)));
        assert!(matches!(content_for(p("/a.md"), Some(&bytes(vec![0xff, 0xfe]))), Content::Unsupported { .. }));
        let big = bytes(vec![b'a'; 2 * 1024 * 1024]);
        assert!(matches!(content_for(p("/x.xyz"), Some(&big)), Content::Unsupported { size: Some(_) }));
        assert!(matches!(content_for(p("/x.xyz"), Some(&bytes(b"ok".to_vec()))), Content::Text(_)));
        assert!(matches!(content_for(p("/report.pdf"), None), Content::Unsupported { size: None }));
        assert!(matches!(content_for(p("/page.html"), None), Content::Html));
        assert!(matches!(content_for(p("/a.md"), Some(&Shown::Missing)), Content::Missing));
        assert!(matches!(content_for(p("/a.md"), Some(&Shown::TooLarge { size: 9 })), Content::TooLarge(Some(9))));
        assert!(matches!(
            content_for(p("/a.md"), Some(&Shown::Failed { reason: "x".into() })),
            Content::Failed(reason) if reason == "x"
        ));
        assert!(matches!(content_for(p("/a.md"), None), Content::Loading));
    }

    #[test]
    fn temp_copy_path_is_per_host_and_file() {
        assert_eq!(
            temp_copy_path(Path::new("/tmp"), "box", "toolu_1:0", "after.png"),
            PathBuf::from("/tmp/gantry-preview/box/toolu_1-0/after.png")
        );
    }

    #[test]
    fn a_reloaded_file_is_a_different_content() {
        let a = content_for(Path::new("/a.md"), Some(&bytes(b"one".to_vec())));
        let b = content_for(Path::new("/a.md"), Some(&bytes(b"two".to_vec())));
        assert!(same_content(&a, &a.clone()));
        assert!(!same_content(&a, &b));
        assert!(same_content(&Content::TooLarge(Some(1)), &Content::TooLarge(Some(1))));
        assert!(!same_content(&Content::TooLarge(Some(1)), &Content::TooLarge(Some(2))));
        assert!(!same_content(&Content::Failed("a".into()), &Content::Failed("b".into())));
        let mut changed = model();
        changed.content = a.clone();
        let mut other = model();
        other.content = b;
        assert!(!same_model(&changed, &other));
        other.content = a;
        assert!(same_model(&changed, &other));
    }
}
