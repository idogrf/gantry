//! Preview panes in the window: opening one beside a Claude pane, keeping its model current as the session list
//! changes, titles for its rows, and telling each daemon which sessions to watch for sent files.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use app_core::preview::{file_name, newly_sent, row_title};
use app_core::session_tree::session_title;
use app_core::split_tree::{Axis, NodeId};
use app_core::workspace::PreviewPlace;
use gpui::{App, AppContext, Context, Window};
use protocol::host::HostId;
use protocol::message::SentFile;

use super::layout::start_rects;
use super::{PreviewSlot, WorkspaceView};
use crate::preview_view::{Content, PreviewEvent, PreviewModel, PreviewTab, PreviewView};

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
        && std::mem::discriminant(&a.content) == std::mem::discriminant(&b.content)
        && a.tabs.len() == b.tabs.len()
        && a.tabs.iter().zip(&b.tabs).all(|(x, y)| {
            (&x.id, &x.name, &x.path, &x.caption, x.unseen) == (&y.id, &y.name, &y.path, &y.caption, y.unseen)
        })
}

impl WorkspaceView {
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
        let content = match self.previews.get(&leaf).map(|slot| slot.view.read(cx).model()) {
            _ if tabs.is_empty() => Content::NoFiles,
            // Another file than the one the content was made for: it is read first.
            Some(shown) if shown.active == active && !matches!(shown.content, Content::NoFiles) => {
                shown.content.clone()
            }
            _ => Content::Loading,
        };
        PreviewModel {
            title: info.map_or_else(|| "Preview".to_string(), |info| format!("Preview · {}", session_title(info))),
            tabs,
            active,
            content,
            host_card: self.workspace.leaves.get(&leaf).and_then(|entry| self.host_card(&entry.host)),
            // Needs the file reads of the next change.
            can_open: false,
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
        self.sync_watch(cx);
        if saved {
            self.save(cx); // also refreshes the sidebar entries
        } else if rows_changed {
            self.sync_panes(cx);
        }
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
            // The tabs, menus and system viewer come with the file reads.
            PreviewEvent::Activate(_)
            | PreviewEvent::Remove(_)
            | PreviewEvent::Menu { .. }
            | PreviewEvent::Hidden { .. }
            | PreviewEvent::OpenExternal => {}
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
}
