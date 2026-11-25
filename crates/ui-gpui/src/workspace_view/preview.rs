//! Preview panes in the window: opening one beside a Claude pane, keeping its model current as the session list
//! changes, titles for its rows, and telling each daemon which sessions to watch for sent files.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use app_core::menus::{hidden_tabs_menu, preview_menu};
use app_core::preview::{
    FileKind, ImageType, MAX_UNKNOWN_TEXT, TextDoc, file_name, image_size, image_too_large, is_link, is_read, kind_of,
    newly_sent, row_title,
};
use app_core::preview_reads::{FileKey, POLL, Shown};
use app_core::session_tree::session_title;
use app_core::split_tree::{Axis, NodeId};
use app_core::workspace::PreviewPlace;
use gpui::{App, AppContext, ClipboardItem, Context, Task, Window};
use preview_doc::highlight::highlight_file;
use preview_doc::markdown::{self, LinkTarget, MAX_MARKDOWN_BYTES};
use protocol::host::HostId;
use protocol::message::SentFile;

use super::layout::start_rects;
use super::{PreviewSlot, WorkspaceView};
use crate::context_menu::OpenMenu;
use crate::preview_view::{
    Content, MAX_IMAGES_IN_VIEW, PreviewEvent, PreviewModel, PreviewTab, PreviewView, same_content,
};
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
        && a.images.len() == b.images.len()
        && a.images.iter().all(|(path, x)| b.images.get(path).is_some_and(|y| same_content(x, y)))
        && a.tabs.len() == b.tabs.len()
        && a.tabs.iter().zip(&b.tabs).all(|(x, y)| {
            (&x.id, &x.name, &x.path, &x.caption, x.unseen) == (&y.id, &y.name, &y.path, &y.caption, y.unseen)
        })
}

/// How a file's bytes are parsed on the background executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ParseKind {
    /// A text file of a known kind: parsed and highlighted.
    Text,
    /// A markdown file: parsed into a document (plain text above `MAX_MARKDOWN_BYTES`).
    Markdown,
    /// A file of no known kind: text when it is UTF-8 and small enough.
    Unknown,
}

/// One file version waiting to be parsed.
#[derive(Clone)]
pub(super) struct ParseJob {
    /// What to make of the bytes.
    pub kind: ParseKind,
    /// The file's path (its extension picks the syntax, its folder resolves a markdown document's links).
    pub path: PathBuf,
    /// The file's bytes.
    pub data: Arc<[u8]>,
    /// The modification time the bytes came with, to tell a stale result.
    pub mtime_ms: u64,
}

/// What `decode_now` makes of a file: content at once, or a parse for the background executor.
enum Decode {
    Ready(Content),
    Parse(ParseJob),
}

/// What the pane shows for `path` given the last reply about it (`None` = no reply yet): everything that needs no
/// parsing at once (an image is only header-checked), text and markdown as a job.
fn decode_now(path: &Path, shown: Option<&Shown>) -> Decode {
    let kind = kind_of(path);
    if !is_read(kind) {
        return Decode::Ready(if kind == FileKind::Html { Content::Html } else { Content::Unsupported { size: None } });
    }
    let Some(shown) = shown else { return Decode::Ready(Content::Loading) };
    let (data, mtime_ms) = match shown {
        Shown::Missing => return Decode::Ready(Content::Missing),
        Shown::TooLarge { size } => return Decode::Ready(Content::TooLarge(Some(*size))),
        Shown::Failed { reason } => return Decode::Ready(Content::Failed(reason.clone())),
        Shown::Bytes { data, mtime_ms } => (data, *mtime_ms),
    };
    let job = |kind| Decode::Parse(ParseJob { kind, path: path.to_path_buf(), data: data.clone(), mtime_ms });
    match kind {
        FileKind::Image(kind) => {
            let Some((width, height)) = image_size(data, kind) else {
                return Decode::Ready(Content::Failed("not a readable image".into()));
            };
            if image_too_large(data, kind, (width, height)) {
                return Decode::Ready(Content::TooLarge(None));
            }
            let format = match kind {
                ImageType::Png => gpui::ImageFormat::Png,
                ImageType::Jpeg => gpui::ImageFormat::Jpeg,
                ImageType::Gif => gpui::ImageFormat::Gif,
                ImageType::Webp => gpui::ImageFormat::Webp,
                ImageType::Svg => gpui::ImageFormat::Svg,
            };
            Decode::Ready(Content::Image {
                image: Arc::new(gpui::Image::from_bytes(format, data.to_vec())),
                natural: (width as f32, height as f32),
            })
        }
        FileKind::Text => job(ParseKind::Text),
        FileKind::Markdown => job(ParseKind::Markdown),
        FileKind::Unknown => job(ParseKind::Unknown),
        _ => Decode::Ready(Content::Unsupported { size: Some(data.len() as u64) }),
    }
}

/// Parse (and highlight) a job's bytes. Runs on the background executor only: the first highlight loads the syntaxes.
fn run_parse(job: &ParseJob) -> Content {
    let unsupported = Content::Unsupported { size: Some(job.data.len() as u64) };
    let text = |highlight: bool| match TextDoc::parse(&job.data) {
        // The BOM-free text: highlight offsets are byte offsets into what `TextDoc` shows.
        Some(doc) => {
            let highlights = highlight.then(|| highlight_file(&job.path, doc.text())).flatten().map(Arc::new);
            Content::Text { doc: Arc::new(doc), highlights }
        }
        None => unsupported.clone(),
    };
    match job.kind {
        ParseKind::Text => text(true),
        ParseKind::Markdown if job.data.len() > MAX_MARKDOWN_BYTES => text(false),
        ParseKind::Markdown => match std::str::from_utf8(&job.data) {
            Ok(source) => {
                let source = source.strip_prefix('\u{feff}').unwrap_or(source);
                Content::Markdown(Arc::new(markdown::parse(source, job.path.parent().unwrap_or(Path::new("")))))
            }
            Err(_) => unsupported,
        },
        ParseKind::Unknown if job.data.len() <= MAX_UNKNOWN_TEXT => text(true),
        ParseKind::Unknown => unsupported,
    }
}

/// What a file shows before any read has been answered (`Loading`, or the card of a never-read type).
fn ready_content(path: &Path) -> Content {
    match decode_now(path, None) {
        Decode::Ready(content) => content,
        Decode::Parse(_) => Content::Loading,
    }
}

/// The source text of text or markdown content.
fn source_text(content: &Content) -> Option<&str> {
    match content {
        Content::Text { doc, .. } => Some(doc.text()),
        Content::Markdown(doc) => Some(doc.source()),
        _ => None,
    }
}

/// Whether a parse made from bytes modified at `mtime_ms` still matches what the cache holds.
fn parse_is_current(shown: Option<&Shown>, mtime_ms: u64) -> bool {
    matches!(shown, Some(Shown::Bytes { mtime_ms: now, .. }) if *now == mtime_ms)
}

/// What to do when a parse of bytes modified at `mtime_ms` ends: whether to show its content, and whether to start
/// the queued job (which `queued_mtime` is the time of). A result for older bytes is still shown while a newer job is
/// queued (the file may change faster than it parses; the queued job then replaces it), but never when the file is
/// gone or nothing newer is coming.
fn after_parse(shown: Option<&Shown>, mtime_ms: u64, queued_mtime: Option<u64>) -> (bool, bool) {
    let has_bytes = matches!(shown, Some(Shown::Bytes { .. }));
    let keep = has_bytes && (parse_is_current(shown, mtime_ms) || queued_mtime.is_some());
    (keep, queued_mtime.is_some_and(|mtime| parse_is_current(shown, mtime)))
}

/// Route a new job for `key`: it starts now (returned) unless a parse is `running`, when it replaces the queued job.
fn route_job(running: bool, queue: &mut HashMap<FileKey, ParseJob>, key: FileKey, job: ParseJob) -> Option<ParseJob> {
    if running {
        queue.insert(key, job);
        None
    } else {
        Some(job)
    }
}

/// Whether the unseen dot and the saved seen list cover tab `id`: not a file opened from a link.
fn tracks_seen(id: &str) -> bool {
    !is_link(id)
}

/// A path component made safe: no separators, and not `.` or `..` (empty becomes `fallback`).
fn safe_part(part: &str, fallback: &str) -> String {
    let part = part.replace(['/', '\\'], "-");
    if part.is_empty() {
        fallback.to_string()
    } else if part == "." || part == ".." {
        "-".to_string()
    } else {
        part
    }
}

/// Where the copy of a remote file for the system viewer goes in the private folder `root` (`preview_dir`): one folder
/// per host and file id, so two files with the same name never overwrite each other, and nothing a host, id or name
/// says can leave that folder.
fn temp_copy_path(root: &Path, host: &str, id: &str, name: &str) -> PathBuf {
    root.join(safe_part(host, "-")).join(safe_part(&id.replace(':', "-"), "-")).join(safe_part(name, "file"))
}

/// Whether the file is known to be gone. A local file of a type the pane never reads (`Unsupported`, `Html`) is never
/// reported `Missing`, so `exists` (a look at the disk, passed in to keep this pure) says it.
fn gone(local: bool, exists: bool, content: &Content) -> bool {
    matches!(content, Content::Missing) || (local && !exists)
}

/// Whether Open in system viewer works for the shown file. A local file opens unless it is gone; a remote one needs
/// its host and is read once for it, which the daemon refuses above its cap (`TooLarge(Some)`); an image over the
/// pixel cap (`TooLarge(None)`) is small enough to send and the viewer is the way to see it.
fn can_open(local: bool, connected: bool, exists: bool, content: &Content) -> bool {
    !gone(local, exists, content) && (local || (connected && !matches!(content, Content::TooLarge(Some(_)))))
}

/// Whether a file's decoded content must be made again from the byte cache: nothing decoded and no parse under way,
/// but the cache holds it.
fn needs_refill(decoded: bool, parsing: bool, shown: Option<&Shown>) -> bool {
    !decoded && !parsing && shown.is_some()
}

/// The decoded files to drop: those no preview lists (and no document shows as an `in_view` image), those whose bytes
/// the cache no longer holds, and the images that are neither the active file of some preview nor in view in a
/// document (GPUI keeps a full bitmap and an atlas tile for each decoded image; one is decoded again from the cached
/// bytes when its tab is shown or it scrolls back).
fn stale_decoded(
    decoded: &HashMap<FileKey, Content>,
    listed: &HashSet<FileKey>,
    active: &HashSet<FileKey>,
    in_view: &HashSet<FileKey>,
    cached: impl Fn(&FileKey) -> bool,
) -> Vec<FileKey> {
    decoded
        .iter()
        .filter(|(key, content)| {
            let shown = in_view.contains(*key);
            !(listed.contains(*key) || shown)
                || !cached(key)
                || (matches!(content, Content::Image { .. }) && !active.contains(*key) && !shown)
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// The files to read for a document's in-view image paths: the ones whose type is an image, at most
/// `MAX_IMAGES_IN_VIEW`, on `host`.
fn doc_image_keys(host: &HostId, paths: &[PathBuf]) -> Vec<FileKey> {
    paths
        .iter()
        .filter(|path| matches!(kind_of(path), FileKind::Image(_)))
        .take(MAX_IMAGES_IN_VIEW)
        .map(|path| FileKey { host: host.clone(), path: path.clone() })
        .collect()
}

/// What following a markdown link does.
#[derive(Debug, PartialEq)]
enum LinkAction<'a> {
    /// Open the address in the browser.
    Browser(&'a str),
    /// Show the file as a linked tab.
    Tab(&'a Path),
}

/// What a click on `target` does at the workspace: only a web address goes to the browser and only a file path opens a
/// tab; an anchor is the view's own and anything inert does nothing.
fn link_action(target: &LinkTarget) -> Option<LinkAction<'_>> {
    match target {
        LinkTarget::Web(url) => Some(LinkAction::Browser(url)),
        LinkTarget::File(path) => Some(LinkAction::Tab(path)),
        LinkTarget::Anchor(_) | LinkTarget::Inert(_) => None,
    }
}

/// The images among `contents`, which GPUI holds a decoded bitmap and an atlas tile for until released.
fn images_of(contents: Vec<Content>) -> Vec<Arc<gpui::Image>> {
    contents
        .into_iter()
        .filter_map(|content| match content {
            Content::Image { image, .. } => Some(image),
            _ => None,
        })
        .collect()
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
        let mut wanted = Vec::new();
        for leaf in tab.root.leaves().into_iter().filter(|leaf| self.previews.contains_key(leaf)) {
            let Some(host) = self.workspace.leaves.get(&leaf).map(|entry| &entry.host) else { continue };
            if self.client(host).is_none() {
                continue;
            }
            let Some(file) = self.workspace.preview_active(leaf, sessions) else { continue };
            if is_read(kind_of(&file.path)) {
                wanted.push(FileKey { host: host.clone(), path: file.path.clone() });
            }
        }
        // The images of the document rows in view are read too (on the pane's host), and nothing else.
        wanted.extend(self.doc_images(cx).into_iter().filter(|key| self.client(&key.host).is_some()));
        wanted
    }

    /// The image files of the markdown documents in the visible tab's previews, in view as last reported: at most
    /// `MAX_IMAGES_IN_VIEW` per pane.
    fn doc_images(&self, cx: &App) -> Vec<FileKey> {
        let Some(tab) = self.workspace.active_tab().filter(|tab| !tab.settings) else { return Vec::new() };
        let mut out = Vec::new();
        for leaf in tab.root.leaves() {
            let Some(slot) = self.previews.get(&leaf) else { continue };
            let Some(host) = self.workspace.leaves.get(&leaf).map(|entry| entry.host.clone()) else { continue };
            let view = slot.view.read(cx);
            if matches!(view.model().content, Content::Markdown(_)) {
                out.extend(doc_image_keys(&host, view.images_in_view()));
            }
        }
        out
    }

    fn send_read(&self, key: &FileKey, request: u64, known: Option<u64>) {
        if let Some(client) = self.client(&key.host) {
            client.read_file(request, &key.path.to_string_lossy(), known);
        }
    }

    /// Ask for the wanted files that are due (the timer).
    fn preview_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.flush_releases(window, cx);
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
                let id = self.workspace.preview_active(leaf, sessions)?.id.clone();
                tracks_seen(&id).then(|| (session.to_string(), id))
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.reads.apply(host, request, body);
        let mut old = Vec::new();
        if let Some(key) = &changed {
            old.extend(self.start_decode(key.clone(), cx));
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
        // After the views have the new content, so a replaced image is not drawn (and decoded) again.
        old.extend(self.prune_decoded(cx));
        self.pending_release.extend(images_of(old));
        self.flush_releases(window, cx);
    }

    /// Free what GPUI holds for the images that were replaced or dropped: the atlas tile of each (it is keyed by a new
    /// id per version) and its decoded bitmap.
    pub(super) fn flush_releases(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for image in std::mem::take(&mut self.pending_release) {
            if let Some(render) = image.clone().get_render_image(window, cx) {
                cx.drop_image(render, Some(window));
            }
            image.remove_asset(cx);
        }
    }

    /// Decode each shown file the cache holds but `decoded` lost (it was unlisted for a while, and the daemon answers
    /// `Unchanged` for it), once: a file whose parse is running is left to it.
    fn refill_decoded(&mut self, cx: &mut Context<Self>) {
        let wanted: Vec<FileKey> = self
            .previews
            .keys()
            .filter_map(|leaf| self.active_file(*leaf, cx))
            .map(|(key, _)| key)
            .chain(self.doc_images(cx))
            .filter(|key| is_read(kind_of(&key.path)))
            .filter(|key| {
                needs_refill(self.decoded.contains_key(key), self.parsing.contains_key(key), self.reads.shown(key))
            })
            .collect();
        for key in wanted {
            let old = self.start_decode(key, cx);
            self.pending_release.extend(images_of(old.into_iter().collect()));
        }
    }

    /// The one way into `decoded`: content that needs no parse goes in at once, a text or markdown file is parsed on
    /// the background executor (one parse per file at a time, the newest job waits). While it runs the old content stays
    /// shown. Returns the content a direct insert replaced.
    fn start_decode(&mut self, key: FileKey, cx: &mut Context<Self>) -> Option<Content> {
        match decode_now(&key.path, self.reads.shown(&key)) {
            Decode::Ready(content) => {
                self.parse_queued.remove(&key);
                self.decoded.insert(key, content)
            }
            Decode::Parse(job) => {
                if let Some(job) = route_job(self.parsing.contains_key(&key), &mut self.parse_queued, key.clone(), job)
                {
                    self.spawn_parse(key, job, cx);
                }
                None
            }
        }
    }

    fn spawn_parse(&mut self, key: FileKey, job: ParseJob, cx: &mut Context<Self>) {
        let mtime_ms = job.mtime_ms;
        let done_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let content = cx.background_executor().spawn(async move { run_parse(&job) }).await;
            this.update(cx, |view, cx| view.parse_done(done_key, mtime_ms, content, cx)).ok();
        });
        self.parsing.insert(key, task);
    }

    /// A parse finished: keep its content (see `after_parse`), then start the queued job.
    fn parse_done(&mut self, key: FileKey, mtime_ms: u64, content: Content, cx: &mut Context<Self>) {
        self.parsing.remove(&key);
        let queued = self.parse_queued.get(&key).map(|job| job.mtime_ms);
        let (keep, run_queued) = after_parse(self.reads.shown(&key), mtime_ms, queued);
        if keep {
            let old = self.decoded.insert(key.clone(), content);
            self.pending_release.extend(images_of(old.into_iter().collect()));
            self.push_preview_models(cx);
            cx.notify();
        }
        if let Some(job) = self.parse_queued.remove(&key)
            && run_queued
        {
            self.spawn_parse(key, job, cx);
        }
    }

    /// Drop the decoded content that is no longer needed (see `stale_decoded`); returns it.
    fn prune_decoded(&mut self, cx: &App) -> Vec<Content> {
        let sessions = self.sidebar.read(cx).sessions();
        let mut listed: HashSet<FileKey> = HashSet::new();
        for leaf in self.previews.keys() {
            let Some(entry) = self.workspace.leaves.get(leaf) else { continue };
            for file in self.workspace.preview_files(*leaf, sessions) {
                listed.insert(FileKey { host: entry.host.clone(), path: file.path.clone() });
            }
        }
        let active: HashSet<FileKey> =
            self.previews.keys().filter_map(|leaf| self.active_file(*leaf, cx)).map(|(key, _)| key).collect();
        // A parse of a file nothing lists any more (or whose bytes left the cache) is cancelled by dropping its task.
        let wanted = |key: &FileKey| listed.contains(key) && self.reads.shown(key).is_some();
        self.parsing.retain(|key, _| wanted(key));
        self.parse_queued.retain(|key, _| wanted(key));
        let in_view: HashSet<FileKey> = self.doc_images(cx).into_iter().collect();
        let gone = stale_decoded(&self.decoded, &listed, &active, &in_view, |key| self.reads.shown(key).is_some());
        gone.iter().filter_map(|key| self.decoded.remove(key)).collect()
    }

    /// Drop and release the decoded content nothing lists any more.
    pub(super) fn release_unlisted_previews(&mut self, cx: &mut Context<Self>) {
        let old = self.prune_decoded(cx);
        self.pending_release.extend(images_of(old));
    }

    /// Forget a host's reads and the Open in system viewer wishes waiting on them.
    pub(super) fn forget_host_reads(&mut self, host: &HostId) {
        self.reads.forget_host(host);
        self.parsing.retain(|key, _| &key.host != host);
        self.parse_queued.retain(|key, _| &key.host != host);
        self.open_after_read.retain(|key, _| &key.host != host);
    }

    /// Write a remote file's bytes to a temporary copy and open that in the system viewer.
    fn open_copy(key: &FileKey, id: &str, data: &[u8], cx: &mut Context<Self>) {
        let Some(root) = crate::preview_dir::root(&daemon::paths::current_profile()) else {
            tracing::warn!("no private folder for the preview copy");
            return;
        };
        let path = temp_copy_path(&root, key.host.name(), id, &file_name(&key.path));
        match crate::preview_dir::write_private(&root, &path, data) {
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
        // A type the pane never reads has no live copy: every open asks again (an unchanged file is one tiny reply).
        if is_read(kind_of(&key.path))
            && let Some(Shown::Bytes { data, .. }) = self.reads.shown(&key)
        {
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
        cx.defer_in(window, |this, window, cx| this.flush_releases(window, cx));
    }

    /// Hide file `id` from preview `leaf`'s list.
    pub(super) fn remove_file(&mut self, leaf: NodeId, id: &str, window: &Window, cx: &mut Context<Self>) {
        self.workspace.remove_sent(leaf, id, self.sidebar.read(cx).sessions());
        self.save(cx);
        self.previews_in_view(window, cx);
        self.push_preview_models(cx);
        cx.defer_in(window, |this, window, cx| this.flush_releases(window, cx));
    }

    /// Hide every file of preview `leaf`'s session.
    pub(super) fn clear_files(&mut self, leaf: NodeId, window: &Window, cx: &mut Context<Self>) {
        self.workspace.clear_sent(leaf, self.sidebar.read(cx).sessions());
        self.save(cx);
        self.previews_in_view(window, cx);
        self.push_preview_models(cx);
        cx.defer_in(window, |this, window, cx| this.flush_releases(window, cx));
    }

    /// The decoded text or markdown of file `id` of preview `leaf`.
    fn source_of(&self, leaf: NodeId, id: &str, cx: &App) -> Option<Content> {
        let host = self.workspace.leaves.get(&leaf)?.host.clone();
        let file =
            self.workspace.preview_files(leaf, self.sidebar.read(cx).sessions()).into_iter().find(|f| f.id == id)?;
        let content = self.decoded.get(&FileKey { host, path: file.path.clone() })?;
        source_text(content).map(|_| content.clone())
    }

    /// Put the text of file `id` on the clipboard.
    pub(super) fn copy_file_contents(&mut self, leaf: NodeId, id: &str, cx: &mut Context<Self>) {
        if let Some(content) = self.source_of(leaf, id, cx)
            && let Some(text) = source_text(&content)
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
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
                unseen: tracks_seen(&file.id)
                    && !seen_in.is_some_and(|session| self.workspace.is_seen(session, &file.id)),
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
                    // Never-read types (HTML, PDF) have no decoded entry: their card comes from the type alone.
                    None => ready_content(&file.path),
                }
            }
            _ => Content::NoFiles,
        };
        let images = match (&host, &content) {
            (Some(host), Content::Markdown(_)) => self
                .previews
                .get(&leaf)
                .map(|slot| doc_image_keys(host, slot.view.read(cx).images_in_view()))
                .unwrap_or_default()
                .into_iter()
                .filter_map(|key| Some((key.path.clone(), self.decoded.get(&key)?.clone())))
                .collect(),
            _ => HashMap::new(),
        };
        let can_open = match (&host, file) {
            (Some(host), Some(file)) => {
                // Only a local file of a never-read type needs a look at the disk; the others know from their read.
                let exists = !host.is_local()
                    || !matches!(content, Content::Unsupported { .. } | Content::Html)
                    || file.path.exists();
                can_open(host.is_local(), self.client(host).is_some(), exists, &content)
            }
            _ => false,
        };
        PreviewModel {
            title: info.map(|info| session_title(info).to_string()).unwrap_or_default(),
            tabs,
            active,
            content,
            images,
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
        self.release_unlisted_previews(cx);
        self.sync_watch(cx);
        if saved {
            self.save(cx); // also refreshes the sidebar entries
        } else if rows_changed {
            self.sync_panes(cx);
        }
    }

    /// Give each preview view its model when something it draws changed; whether a sidebar entry's row changed.
    fn push_preview_models(&mut self, cx: &mut Context<Self>) -> bool {
        self.refill_decoded(cx);
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
        // After the views have their models, so a replaced or switched-away image is no longer drawn.
        self.release_unlisted_previews(cx);
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

    /// Follow a click on a link of preview `leaf`'s markdown document: a web address opens in the browser and a file
    /// path as a linked tab (read at once); anything else is the view's own or does nothing.
    fn follow_link(&mut self, leaf: NodeId, target: &LinkTarget, window: &mut Window, cx: &mut Context<Self>) {
        match link_action(target) {
            Some(LinkAction::Browser(url)) => cx.open_url(url),
            Some(LinkAction::Tab(path)) => {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64);
                if self.workspace.open_link(leaf, path, now_ms, self.sidebar.read(cx).sessions()).is_some() {
                    self.save(cx);
                    self.push_preview_models(cx);
                    self.previews_in_view(window, cx);
                    cx.defer_in(window, |this, window, cx| this.flush_releases(window, cx));
                }
            }
            None => {}
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
                let active = self.active_file(leaf, cx).map(|(_, id)| id);
                let target = file.clone().or(active);
                let copy = target.filter(|id| self.source_of(leaf, id, cx).is_some());
                self.open_preview_menu(preview_menu(leaf, file.as_deref(), copy.as_deref()), *position, window, cx);
            }
            PreviewEvent::Hidden { position } => {
                let Some(slot) = self.previews.get(&leaf) else { return };
                let files = slot.view.read(cx).hidden_tabs(theme(cx));
                self.open_preview_menu(hidden_tabs_menu(leaf, &files), *position, window, cx);
            }
            PreviewEvent::OpenExternal => self.open_in_system_viewer(leaf, cx),
            PreviewEvent::Link(target) => self.follow_link(leaf, target, window, cx),
            PreviewEvent::ImagesInView => {
                self.ask_previews(window, cx);
                self.push_preview_models(cx);
                cx.defer_in(window, |this, window, cx| this.flush_releases(window, cx));
            }
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
            title: String::new(),
            tabs: vec![PreviewTab {
                id: "a".into(),
                name: "a.md".into(),
                path: "/r/a.md".into(),
                caption: None,
                unseen: true,
            }],
            active: Some("a".into()),
            content: Content::Loading,
            images: HashMap::new(),
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
        changed.title = "x".into();
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

    /// What `decode_now` answers at once; a deferred parse panics.
    fn ready(path: &str, shown: Option<&Shown>) -> Content {
        match decode_now(Path::new(path), shown) {
            Decode::Ready(content) => content,
            Decode::Parse(_) => panic!("{path} was deferred"),
        }
    }

    /// The content of a file after its parse ran (what the background executor makes).
    fn parsed(path: &str, shown: &Shown) -> Content {
        match decode_now(Path::new(path), Some(shown)) {
            Decode::Ready(content) => content,
            Decode::Parse(job) => run_parse(&job),
        }
    }

    fn job_kind(path: &str, shown: &Shown) -> Option<ParseKind> {
        match decode_now(Path::new(path), Some(shown)) {
            Decode::Parse(job) => Some(job.kind),
            Decode::Ready(_) => None,
        }
    }

    #[test]
    fn decode_now_defers_text_and_markdown() {
        assert_eq!(job_kind("/a.md", &bytes(b"# hi".to_vec())), Some(ParseKind::Markdown));
        assert_eq!(job_kind("/a.rs", &bytes(b"fn x(){}".to_vec())), Some(ParseKind::Text));
        assert_eq!(job_kind("/a.xyz", &bytes(b"x".to_vec())), Some(ParseKind::Unknown));
        assert!(matches!(ready("/a.png", Some(&bytes(png(1, 1)))), Content::Image { natural: (1., 1.), .. }));
        assert!(matches!(ready("/a.md", None), Content::Loading));
        assert!(matches!(ready("/report.pdf", None), Content::Unsupported { size: None }));
        assert!(matches!(ready("/page.html", None), Content::Html));
        assert!(matches!(ready("/a.md", Some(&Shown::Missing)), Content::Missing));
        assert!(matches!(ready("/a.md", Some(&Shown::TooLarge { size: 9 })), Content::TooLarge(Some(9))));
        assert!(matches!(
            ready("/a.md", Some(&Shown::Failed { reason: "x".into() })),
            Content::Failed(reason) if reason == "x"
        ));
        assert!(matches!(ready("/a.png", Some(&bytes(png(20000, 20000)))), Content::TooLarge(None)));
        assert!(matches!(ready("/a.png", Some(&bytes(b"junk".to_vec()))), Content::Failed(_)));
    }

    #[test]
    fn run_parse_maps_every_kind() {
        assert!(matches!(parsed("/a.md", &bytes(b"# hi".to_vec())), Content::Markdown(_)));
        assert!(matches!(parsed("/a.rs", &bytes(b"fn x(){}".to_vec())), Content::Text { highlights: Some(_), .. }));
        assert!(matches!(parsed("/a.txt", &bytes(b"x".to_vec())), Content::Text { highlights: None, .. }));
        assert!(matches!(parsed("/a.md", &bytes(vec![0xff, 0xfe])), Content::Unsupported { size: Some(2) }));
        let big = bytes(vec![b'a'; 2 * 1024 * 1024]);
        assert!(matches!(parsed("/x.xyz", &big), Content::Unsupported { size: Some(_) }));
        assert!(matches!(parsed("/x.xyz", &bytes(b"ok".to_vec())), Content::Text { .. }));
    }

    #[test]
    fn a_bom_does_not_shift_the_highlight_offsets() {
        let mut data = vec![0xef, 0xbb, 0xbf];
        data.extend(b"fn x(){}");
        let Content::Text { doc, highlights: Some(highlights) } = parsed("/a.rs", &bytes(data)) else {
            panic!("highlighted text expected");
        };
        assert!(!doc.text().starts_with('\u{feff}'));
        assert!(highlights.line(0).iter().all(|span| span.range.end <= doc.line_len(0)));
    }

    #[test]
    fn markdown_over_the_cap_shows_as_plain_text() {
        let big = bytes(vec![b'a'; MAX_MARKDOWN_BYTES + 1]);
        assert!(matches!(parsed("/a.md", &big), Content::Text { highlights: None, .. }));
    }

    #[test]
    fn same_content_compares_markdown_by_pointer() {
        let a = parsed("/a.md", &bytes(b"# hi".to_vec()));
        let b = parsed("/a.md", &bytes(b"# hi".to_vec()));
        assert!(!same_content(&a, &b));
        assert!(same_content(&a, &a.clone()));
        let text = parsed("/a.txt", &bytes(b"# hi".to_vec()));
        assert!(!same_content(&a, &text));
        assert!(!same_content(&text, &a));
        let rust = parsed("/a.rs", &bytes(b"fn x(){}".to_vec()));
        let again = parsed("/a.rs", &bytes(b"fn x(){}".to_vec()));
        assert!(same_content(&rust, &rust.clone()));
        assert!(!same_content(&rust, &again));
    }

    #[test]
    fn source_text_covers_text_and_markdown() {
        let md = parsed("/a.md", &bytes(b"# hi".to_vec()));
        assert_eq!(source_text(&md), Some("# hi"));
        assert_eq!(source_text(&parsed("/a.txt", &bytes(b"x".to_vec()))), Some("x"));
        assert_eq!(source_text(&image()), None);
        assert_eq!(source_text(&Content::Loading), None);
    }

    #[test]
    fn a_parse_result_is_kept_only_for_the_bytes_it_was_made_from() {
        let shown = Shown::Bytes { data: b"x".to_vec().into(), mtime_ms: 5 };
        assert!(parse_is_current(Some(&shown), 5));
        assert!(!parse_is_current(Some(&shown), 4));
        assert!(!parse_is_current(Some(&Shown::Missing), 5));
        assert!(!parse_is_current(None, 5));
    }

    #[test]
    fn a_never_read_file_without_a_decoded_entry_shows_its_card() {
        assert!(matches!(ready_content(Path::new("/page.html")), Content::Html));
        let pdf = ready_content(Path::new("/report.pdf"));
        assert!(matches!(pdf, Content::Unsupported { size: None }));
        assert!(matches!(ready_content(Path::new("/a.md")), Content::Loading));
        // A deleted local file of that type cannot open (the existence check applies to these cards).
        assert!(!can_open(true, false, false, &pdf));
        assert!(can_open(true, false, true, &pdf));
    }

    fn job(mtime_ms: u64) -> ParseJob {
        ParseJob { kind: ParseKind::Text, path: "/a.rs".into(), data: Arc::from(&b"x"[..]), mtime_ms }
    }

    #[test]
    fn a_change_during_a_parse_queues_one_job_and_the_stale_result_is_kept_until_it_lands() {
        let mut queue = HashMap::new();
        let k = key("/a.rs");
        let at = |m| Shown::Bytes { data: b"x".to_vec().into(), mtime_ms: m };
        // First job runs at once; two changes while it runs leave only the newest queued.
        assert!(route_job(false, &mut queue, k.clone(), job(1)).is_some());
        assert!(route_job(true, &mut queue, k.clone(), job(2)).is_none());
        assert!(route_job(true, &mut queue, k.clone(), job(3)).is_none());
        let queued = queue.get(&k).map(|job| job.mtime_ms);
        assert_eq!(queued, Some(3));
        // The first result is stale but nothing newer is shown yet: kept, and the queued job starts.
        assert_eq!(after_parse(Some(&at(3)), 1, queued), (true, true));
        // The queued job's own result is current; nothing is queued after it.
        assert_eq!(after_parse(Some(&at(3)), 3, None), (true, false));
        // A stale result with nothing queued, or for a file that is gone, is dropped.
        assert_eq!(after_parse(Some(&at(3)), 1, None), (false, false));
        assert_eq!(after_parse(Some(&Shown::Missing), 1, Some(3)), (false, false));
    }

    #[test]
    fn highlighted_and_plain_text_are_different_content() {
        let Content::Text { doc, highlights: Some(highlights) } = parsed("/a.rs", &bytes(b"fn x(){}".to_vec())) else {
            panic!("highlighted text expected");
        };
        let with = Content::Text { doc: doc.clone(), highlights: Some(highlights) };
        let without = Content::Text { doc, highlights: None };
        assert!(!same_content(&with, &without));
        assert!(!same_content(&without, &with));
    }

    #[test]
    fn linked_tabs_never_count_as_unseen() {
        assert!(tracks_seen("toolu_1:0"));
        assert!(!tracks_seen("link:/a/b.md"));
    }

    #[test]
    fn a_file_being_parsed_is_not_decoded_again() {
        let held = bytes(b"x".to_vec());
        assert!(!needs_refill(false, true, Some(&held)));
        assert!(needs_refill(false, false, Some(&held)));
    }

    #[test]
    fn temp_copy_path_is_per_host_and_file() {
        assert_eq!(
            temp_copy_path(Path::new("/tmp"), "box", "toolu_1:0", "after.png"),
            PathBuf::from("/tmp/box/toolu_1-0/after.png")
        );
    }

    #[test]
    fn temp_copy_path_cannot_leave_its_folder() {
        let root = Path::new("/tmp");
        assert_eq!(temp_copy_path(root, "..", "a/b:0", ".."), PathBuf::from("/tmp/-/a-b-0/-"));
        assert_eq!(temp_copy_path(root, "x\\y", "i", ""), PathBuf::from("/tmp/x-y/i/file"));
    }

    #[test]
    fn open_in_system_viewer_is_blocked_only_where_it_cannot_work() {
        let text = Content::NoFiles;
        assert!(can_open(true, false, true, &text));
        assert!(!can_open(true, false, true, &Content::Missing));
        assert!(can_open(true, false, true, &Content::TooLarge(Some(9))));
        assert!(!can_open(false, false, true, &text));
        assert!(can_open(false, true, true, &text));
        assert!(!can_open(false, true, true, &Content::TooLarge(Some(9))));
        assert!(can_open(false, true, true, &Content::TooLarge(None)));
        assert!(!can_open(false, true, true, &Content::Missing));
    }

    #[test]
    fn a_deleted_local_file_of_a_never_read_type_cannot_open() {
        let pdf = Content::Unsupported { size: None };
        assert!(can_open(true, false, true, &pdf));
        assert!(!can_open(true, false, false, &pdf));
        assert!(!can_open(true, false, false, &Content::Html));
        // A remote file's existence is not looked up on this disk.
        assert!(can_open(false, true, false, &pdf));
    }

    fn key(path: &str) -> FileKey {
        FileKey { host: HostId::local(), path: PathBuf::from(path) }
    }

    fn image() -> Content {
        ready("/a.png", Some(&bytes(png(1, 1))))
    }

    #[test]
    fn only_the_active_file_keeps_its_decoded_image() {
        let decoded: HashMap<FileKey, Content> = [
            (key("/shown.png"), image()),
            (key("/other.png"), image()),
            (key("/other.md"), Content::NoFiles),
            (key("/unlisted.png"), image()),
            (key("/evicted.png"), image()),
        ]
        .into();
        let listed: HashSet<FileKey> =
            ["/shown.png", "/other.png", "/other.md", "/evicted.png"].into_iter().map(key).collect();
        let active: HashSet<FileKey> = [key("/shown.png"), key("/evicted.png")].into();
        let in_view = HashSet::new();
        let mut gone = stale_decoded(&decoded, &listed, &active, &in_view, |key| key != &self::key("/evicted.png"));
        gone.sort_by(|a, b| a.path.cmp(&b.path));
        // The inactive image, the unlisted one and the one whose bytes left the cache; the text stays.
        assert_eq!(gone, [key("/evicted.png"), key("/other.png"), key("/unlisted.png")]);
    }

    #[test]
    fn a_cached_file_is_decoded_again_instead_of_loading() {
        let held = bytes(b"x".to_vec());
        assert!(needs_refill(false, false, Some(&held)));
        assert!(!needs_refill(true, false, Some(&held)));
        assert!(!needs_refill(false, false, None));
    }

    #[test]
    fn a_reloaded_file_is_a_different_content() {
        let a = parsed("/a.md", &bytes(b"one".to_vec()));
        let b = parsed("/a.md", &bytes(b"two".to_vec()));
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

    #[test]
    fn in_view_images_are_kept_and_others_released() {
        let decoded: HashMap<FileKey, Content> = [
            (key("/doc.md"), Content::NoFiles),
            (key("/in.png"), image()),
            (key("/out.png"), image()),
            (key("/in-missing.png"), Content::Missing),
            (key("/gone.png"), image()),
        ]
        .into();
        let listed: HashSet<FileKey> = [key("/doc.md")].into();
        let active: HashSet<FileKey> = [key("/doc.md")].into();
        let in_view: HashSet<FileKey> = [key("/in.png"), key("/in-missing.png"), key("/gone.png")].into();
        let mut gone = stale_decoded(&decoded, &listed, &active, &in_view, |key| key != &self::key("/gone.png"));
        gone.sort_by(|a, b| a.path.cmp(&b.path));
        // Listed by no tab, yet kept while in view; the one out of view and the one whose bytes left the cache go.
        assert_eq!(gone, [key("/gone.png"), key("/out.png")]);
    }

    #[test]
    fn only_image_paths_are_wanted() {
        let paths: Vec<PathBuf> = ["/a.png", "/notes.txt", "/b.svg", "/c.md"].iter().map(PathBuf::from).collect();
        assert_eq!(doc_image_keys(&HostId::local(), &paths), [key("/a.png"), key("/b.svg")]);
        let many: Vec<PathBuf> = (0..20).map(|n| PathBuf::from(format!("/{n}.png"))).collect();
        assert_eq!(doc_image_keys(&HostId::local(), &many).len(), crate::preview_view::MAX_IMAGES_IN_VIEW);
    }

    #[test]
    fn models_differ_when_an_image_arrives() {
        let mut changed = model();
        changed.images.insert(PathBuf::from("/a.png"), image());
        assert!(!same_model(&model(), &changed));
        let mut other = model();
        other.images.insert(PathBuf::from("/a.png"), image());
        // A different decode of the same file is a different image.
        assert!(!same_model(&changed, &other));
        other.images = changed.images.clone();
        assert!(same_model(&changed, &other));
        other.images.insert(PathBuf::from("/b.png"), Content::Missing);
        assert!(!same_model(&changed, &other));
    }

    #[test]
    fn only_a_web_link_opens_the_browser_and_only_a_file_link_a_tab() {
        use preview_doc::markdown::LinkTarget;
        let web = LinkTarget::Web("https://example.com".into());
        let file = LinkTarget::File(PathBuf::from("/r/x.md"));
        assert_eq!(link_action(&web), Some(LinkAction::Browser("https://example.com")));
        assert_eq!(link_action(&file), Some(LinkAction::Tab(Path::new("/r/x.md"))));
        assert_eq!(link_action(&LinkTarget::Anchor("a".into())), None);
        assert_eq!(link_action(&LinkTarget::Inert("javascript:x".into())), None);
    }
}
