//! The preview pane's view: a header, a row of file tabs and the active file's body (text lines, an image or a card
//! saying why there is nothing to draw). It draws a `PreviewModel` and reports what the user did as `PreviewEvent`s;
//! `workspace_view` owns the model and acts on the events.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use app_core::host_state::HostCard;
use app_core::preview::{FileKind, TabFit, TextDoc, file_name, fit_tabs, kind_of};
use app_core::preview_zoom::{STEP, WHEEL_STEP_PX, Zoom, wheel_factor};
use app_core::split_tree::NodeId;
use app_core::workspace::DropFile;
use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, KeyDownEvent, ListAlignment,
    ListHorizontalSizingBehavior, ListOffset, ListState, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, PinchEvent, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, SharedString, Task,
    UniformListScrollHandle, Window, canvas, div, img, list, point, prelude::*, px, rgb, svg, uniform_list,
};
use preview_doc::markdown::{DocImage, ImageSrc, LinkTarget, MdDoc, RowKind};
use protocol::host::HostId;
use protocol::message::SentFile;

use crate::assets::{CLOSE_ICON, DIRECTION_ICON, EYE_ICON, FILE_ICON, OPEN_EXTERNAL_ICON};
use crate::git_diff_view::size_text;
use crate::hover_card::{self, HoverCard};
use crate::pane_drag::{DraggedFile, DraggedPane};
use crate::preview_markdown::{
    self, Gesture, Interact, RowState, Unit, code_line, display_spans, doc_rtl, drag_selection, pad,
};
use crate::settings_store::SettingsStore;
use crate::terminal_view::{host_card_element, installed_families, installed_family, pane_font};
use crate::theme::{RADIUS, RADIUS_SMALL, Theme, UI_FONT, theme};

/// The width of a file tab, in UI px.
pub(crate) const TAB_WIDTH: f32 = 160.0;
/// The width of the `+N` button, in UI px.
pub(crate) const PLUS_WIDTH: f32 = 44.0;

/// The most images of a markdown document read and decoded at once (those of the rows being drawn).
pub(crate) const MAX_IMAGES_IN_VIEW: usize = 12;

/// One file tab.
pub(crate) struct PreviewTab {
    /// The file's id (the key the pane's state uses).
    pub id: String,
    /// The label.
    pub name: String,
    /// The full path, shown in the hover card.
    pub path: PathBuf,
    /// What the session said about the file, shown in the hover card.
    pub caption: Option<String>,
    /// A new file the user has not looked at yet.
    pub unseen: bool,
    /// When Claude sent it, ms since the Unix epoch.
    pub sent_ms: u64,
}

/// The header's label and tag: a holding pane shows the file's name and `Holds this file`, a following one
/// `Preview · <session title>` (`Preview` while the title is empty) and `Follows session`.
pub(crate) fn header_text(holding: bool, title: &str) -> (String, &'static str) {
    match (holding, title.is_empty()) {
        (true, _) => (title.to_string(), "Holds this file"),
        (false, true) => ("Preview".to_string(), "Follows session"),
        (false, false) => (format!("Preview · {title}"), "Follows session"),
    }
}

/// What the body shows for the active file.
#[derive(Clone)]
pub(crate) enum Content {
    /// The session has sent no files.
    NoFiles,
    /// The file is being read.
    Loading,
    /// A text file, with its syntax colors when it has a known syntax and is small enough.
    Text {
        /// The parsed text.
        doc: Arc<TextDoc>,
        /// The colored spans of each line.
        highlights: Option<Arc<preview_doc::highlight::Highlights>>,
    },
    /// A markdown document.
    Markdown(Arc<preview_doc::markdown::MdDoc>),
    /// A decoded image with its natural size in pixels.
    Image {
        /// The image.
        image: Arc<gpui::Image>,
        /// Width and height in pixels.
        natural: (f32, f32),
    },
    /// The file is gone.
    Missing,
    /// Over the read cap (the size when known; `None` = over the pixel cap of an image).
    TooLarge(Option<u64>),
    /// A type the pane does not preview, with its size when known.
    Unsupported {
        /// Size in bytes.
        size: Option<u64>,
    },
    /// An HTML file, which opens in the browser.
    Html,
    /// The read failed, with the reason.
    Failed(String),
}

/// Whether the images a pane reported in view stay reported while it shows `content`: a document, and the moment it
/// is loading again, keep them (the workspace keeps those images decoded until the view reports again); anything else
/// has no images.
pub(crate) fn keeps_images(content: &Content) -> bool {
    matches!(content, Content::Markdown(_) | Content::Loading)
}

/// Whether two contents draw the same thing: the same parsed text or decoded image (a reload makes a new one), or
/// equal payloads.
pub(crate) fn same_content(a: &Content, b: &Content) -> bool {
    match (a, b) {
        (Content::Text { doc: a, highlights: x }, Content::Text { doc: b, highlights: y }) => {
            Arc::ptr_eq(a, b)
                && match (x, y) {
                    (Some(x), Some(y)) => Arc::ptr_eq(x, y),
                    (None, None) => true,
                    _ => false,
                }
        }
        (Content::Markdown(a), Content::Markdown(b)) => Arc::ptr_eq(a, b),
        (Content::Image { image: a, natural: x }, Content::Image { image: b, natural: y }) => {
            Arc::ptr_eq(a, b) && x == y
        }
        (Content::TooLarge(a), Content::TooLarge(b)) => a == b,
        (Content::Unsupported { size: a }, Content::Unsupported { size: b }) => a == b,
        (Content::Failed(a), Content::Failed(b)) => a == b,
        (Content::Html, Content::Html)
        | (Content::Loading, Content::Loading)
        | (Content::Missing, Content::Missing)
        | (Content::NoFiles, Content::NoFiles) => true,
        (
            Content::Text { .. }
            | Content::Markdown(_)
            | Content::Image { .. }
            | Content::TooLarge(_)
            | Content::Unsupported { .. }
            | Content::Failed(_)
            | Content::Html
            | Content::Loading
            | Content::Missing
            | Content::NoFiles,
            _,
        ) => false,
    }
}

/// Everything the view draws.
pub(crate) struct PreviewModel {
    /// The followed session's bare title; the header reads `Preview · <title>`, or `Preview` while it is empty. A
    /// holding pane's is the held file's name.
    pub title: String,
    /// Whether the pane holds one dropped file (no tab row, no session) instead of following a session.
    pub holding: bool,
    /// The leaf's host.
    pub host: Option<HostId>,
    /// The followed session's id; `None` for a holding pane.
    pub session: Option<String>,
    /// The file tabs, oldest first.
    pub tabs: Vec<PreviewTab>,
    /// The active tab's id.
    pub active: Option<String>,
    /// The active file's body.
    pub content: Content,
    /// The decoded or state content of the in-view images of the active markdown document, by path.
    pub images: HashMap<PathBuf, Content>,
    /// The state of the host when it is not usable; replaces the body.
    pub host_card: Option<HostCard>,
    /// Whether Open in system viewer can be used.
    pub can_open: bool,
}

/// What the user did in the pane.
pub(crate) enum PreviewEvent {
    /// A mouse press in the pane.
    Focused,
    /// Close the pane.
    Close,
    /// Show the tab with this id.
    Activate(String),
    /// Hide the file with this id from the list.
    Remove(String),
    /// A right-click: the pane's menu, for a file when it was on one.
    Menu {
        /// Where the pointer was.
        position: Point<Pixels>,
        /// The file's id.
        file: Option<String>,
    },
    /// The `+N` button: the hidden tabs.
    Hidden {
        /// Where the button was pressed.
        position: Point<Pixels>,
    },
    /// Open the active file in the system viewer.
    OpenExternal,
    /// The button on the host's card.
    HostAction,
    /// A click on a link of the markdown document that opens something (a web address or a file).
    Link(LinkTarget),
    /// The set of document images whose rows are being drawn changed (see `PreviewView::images_in_view`).
    ImagesInView,
}

/// A selection inside one block of a markdown document, as byte offsets into the block's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockSelection {
    /// The block.
    pub block: usize,
    /// Where the selection started.
    pub anchor: usize,
    /// Where the pointer is now.
    pub head: usize,
}

impl BlockSelection {
    /// The selected bytes, whichever end is first.
    pub(crate) fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }
}

/// A selection in a text file: `(line, byte in the raw line)` at both ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextSelection {
    /// Where the selection started.
    pub anchor: (usize, usize),
    /// Where the pointer is now.
    pub head: (usize, usize),
}

/// The text of a block selection (a selection never leaves its block).
fn copy_block_selection(doc: &MdDoc, selection: &BlockSelection) -> String {
    doc.copy(selection.block, selection.range())
}

/// The text of a text selection, lines joined by `\n`.
fn copy_text_selection(doc: &TextDoc, selection: &TextSelection) -> String {
    doc.copy_span(selection.anchor, selection.head)
}

/// The selected bytes of raw line `line` (of `len` bytes), `None` when none of it is selected.
fn line_range(selection: &TextSelection, line: usize, len: usize) -> Option<Range<usize>> {
    let (from, to) = if selection.anchor <= selection.head {
        (selection.anchor, selection.head)
    } else {
        (selection.head, selection.anchor)
    };
    if line < from.0 || line > to.0 {
        return None;
    }
    let start = if line == from.0 { from.1.min(len) } else { 0 };
    let end = if line == to.0 { to.1.min(len) } else { len };
    (start < end).then_some(start..end)
}

/// The bytes of the displayed line `line` (tabs as four spaces, long lines cut) that show the raw bytes `raw`.
fn display_range(doc: &TextDoc, line: usize, raw: Range<usize>) -> Range<usize> {
    let shown = doc.display(line).len();
    // The first displayed byte whose raw offset reaches `at`; raw offsets never go back as the display advances.
    let first_at = |at: usize| {
        if at == 0 {
            return 0;
        }
        if at >= doc.line_len(line) {
            return shown;
        }
        let (mut low, mut high) = (0, shown);
        while low < high {
            let mid = (low + high) / 2;
            if doc.raw_offset(line, mid) < at {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    };
    first_at(raw.start)..first_at(raw.end)
}

/// How long a pane may go undrawn before a drag scroll takes it for hidden (one late frame is not that).
const HIDDEN_AFTER: Duration = Duration::from_millis(100);

/// Whether a pane last drawn `since_drawn` ago is hidden (not in the visible tab).
fn pane_hidden(since_drawn: Duration) -> bool {
    since_drawn > HIDDEN_AFTER
}

/// The width of the zone along the top and bottom edge of the body where a drag scrolls it, in px.
const EDGE_PX: f32 = 24.0;
/// The most a drag scrolls per tick, in px.
const MAX_STEP_PX: f32 = 20.0;
/// The time between two drag scroll ticks.
const AUTO_SCROLL_TICK: Duration = Duration::from_millis(16);

/// How far a drag at height `y` scrolls a body between `top` and `bottom`: negative (up) inside the top edge zone,
/// positive (down) inside the bottom one, 0 elsewhere; grows linearly with the depth into the zone, up to
/// `MAX_STEP_PX`, and a pointer outside the body counts as the deepest.
pub(crate) fn auto_scroll_step(y: f32, top: f32, bottom: f32) -> f32 {
    if y < top + EDGE_PX {
        -MAX_STEP_PX * (top + EDGE_PX - y).min(EDGE_PX) / EDGE_PX
    } else if y > bottom - EDGE_PX {
        MAX_STEP_PX * (y - (bottom - EDGE_PX)).min(EDGE_PX) / EDGE_PX
    } else {
        0.0
    }
}

/// Pixels one wheel line pans an image.
const PAN_LINE_PX: f32 = 20.0;

/// The longest side of the small map, in UI px.
const MAP_SIZE: f32 = 120.0;

/// The size of the small map: the image's shape fitted inside a `limit` x `limit` box.
fn map_box(natural: (f32, f32), limit: f32) -> (f32, f32) {
    let longest = natural.0.max(natural.1);
    if !(longest.is_finite() && longest > 0.0 && natural.0.min(natural.1) > 0.0) {
        return (limit, limit);
    }
    (limit * natural.0 / longest, limit * natural.1 / longest)
}

/// The label of the zoom bar: the scale as a rounded percent.
fn zoom_label(zoom: &Zoom) -> String {
    format!("{}%", zoom.percent())
}

/// The zoom factor of a wheel movement: one line (or 50 px) is one step, wheel up zooms in.
fn wheel_zoom_factor(delta: ScrollDelta) -> f32 {
    let dy = match delta {
        ScrollDelta::Lines(lines) => lines.y * WHEEL_STEP_PX,
        ScrollDelta::Pixels(pixels) => f32::from(pixels.y),
    };
    wheel_factor(-dy)
}

/// The pan of a wheel movement in pixels: a line is 20 px, the content follows the delta.
fn wheel_pan(delta: ScrollDelta) -> (f32, f32) {
    match delta {
        ScrollDelta::Lines(lines) => (lines.x * PAN_LINE_PX, lines.y * PAN_LINE_PX),
        ScrollDelta::Pixels(pixels) => (f32::from(pixels.x), f32::from(pixels.y)),
    }
}

/// A font's measures at the terminal font size.
struct Metrics {
    family: SharedString,
    size: f32,
    row_height: f32,
    cell_width: f32,
}

/// One preview pane.
pub(crate) struct PreviewView {
    leaf: NodeId,
    focus_handle: FocusHandle,
    model: PreviewModel,
    scroll: UniformListScrollHandle,
    /// The selection in a text file.
    selection: Option<TextSelection>,
    /// The selection in a markdown document.
    md_selection: Option<BlockSelection>,
    /// The left button is held after a press on text.
    dragging: bool,
    /// What the last markdown press selected: its unit and the range it covered.
    md_press: Option<(Unit, Range<usize>)>,
    /// A press landed on text since the root view last saw one (a press elsewhere clears the selection).
    pressed_text: bool,
    /// When the pane was last drawn (a pane not drawn for `HIDDEN_AFTER` is hidden and stops the drag scroll).
    drawn: Instant,
    /// The loop that scrolls while a drag sits at an edge of the body.
    auto_scroll: Option<Task<()>>,
    /// The tab row's width in px, measured while drawing.
    row_width: f32,
    /// How the active image is placed; `None` until an image is shown, reset when the active file changes.
    zoom: Option<Zoom>,
    /// The image body's bounds in the window, measured while drawing.
    viewport: Bounds<Pixels>,
    /// The last pointer position of a drag that pans the image.
    panning: Option<Point<Pixels>>,
    /// The markdown document's rows.
    md_list: ListState,
    /// The document `md_list` was reset for.
    md_doc: Option<Arc<MdDoc>>,
    /// The code block whose Copy button shows `Copied`, with the timer that puts the label back.
    copied: Option<(usize, Task<()>)>,
    /// The ⇄ toggle: the document's direction is flipped. Reset when the active file changes, kept on a reload.
    flipped: bool,
    /// The measured `char_px` with what it was measured for (UI scale bits, font family, size bits).
    char_px_cache: Option<((u32, SharedString, u32), f32)>,
    /// What the rows were last measured with; a change remeasures the list.
    md_look: Option<MdLook>,
    /// The image files of the rows laid out in the frame being drawn (cleared at its start).
    frame_images: Vec<PathBuf>,
    /// The list's width in px the rows were drawn with (read in `render`, never inside a row closure).
    md_width: f32,
    /// The image files of the rows drawn when `ImagesInView` was last sent.
    images_in_view: Vec<PathBuf>,
}

/// The characters of a label in the order they are drawn when it must not reach the shaper as one run: `None` for
/// ASCII-only text; else display order (`bidi::display_line`) when `right_to_left` is on, logical order when off.
fn label_parts(text: &str, right_to_left: bool) -> Option<Vec<char>> {
    if text.is_ascii() {
        return None;
    }
    if right_to_left && let Some(line) = app_core::bidi::display_line(text) {
        return Some(line.into_iter().map(|(_, c)| c).collect());
    }
    Some(text.chars().collect())
}

/// A file or session name as an element: one plain string when it is ASCII, else one element per character in a row, so
/// right-to-left letters never reach the shaper as one run.
pub(crate) fn bidi_label(text: &str, right_to_left: bool) -> gpui::AnyElement {
    match label_parts(text, right_to_left) {
        None => text.to_string().into_any_element(),
        Some(chars) => div()
            .flex()
            .whitespace_nowrap()
            .overflow_hidden()
            .children(chars.into_iter().map(|c| div().flex_none().child(c.to_string())))
            .into_any_element(),
    }
}

/// The settings a markdown row's height depends on besides the width.
#[derive(Clone, Default, PartialEq)]
struct MdLook {
    family: SharedString,
    size: f32,
    line_height: f32,
    ui_scale: f32,
    rtl: bool,
    /// The document is laid out right to left (`doc_rtl`).
    mirror: bool,
    /// The ⇄ toggle is on.
    flipped: bool,
    /// The measured average width of a body text character, for table columns.
    char_px: f32,
}

impl EventEmitter<PreviewEvent> for PreviewView {}

impl Focusable for PreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl PreviewView {
    /// An empty preview pane for `leaf`.
    pub(crate) fn new(leaf: NodeId, cx: &mut Context<Self>) -> Self {
        Self {
            leaf,
            focus_handle: cx.focus_handle(),
            model: PreviewModel {
                title: String::new(),
                holding: false,
                host: None,
                session: None,
                tabs: Vec::new(),
                active: None,
                content: Content::NoFiles,
                images: HashMap::new(),
                host_card: None,
                can_open: false,
            },
            scroll: UniformListScrollHandle::new(),
            selection: None,
            md_selection: None,
            auto_scroll: None,
            dragging: false,
            md_press: None,
            pressed_text: false,
            drawn: Instant::now(),
            row_width: 0.0,
            zoom: None,
            viewport: Bounds::default(),
            panning: None,
            md_list: ListState::new(0, ListAlignment::Top, px(400.)),
            md_doc: None,
            copied: None,
            md_look: None,
            flipped: false,
            char_px_cache: None,
            frame_images: Vec::new(),
            md_width: 0.0,
            images_in_view: Vec::new(),
        }
    }

    /// Show `model`. Scroll and selection stay while the active file is the same (the selection cut to the new line
    /// count) and start over when it changes.
    pub(crate) fn set_model(&mut self, model: PreviewModel, cx: &mut Context<Self>) {
        let same_file = model.active == self.model.active;
        if !keep_flip(self.model.active.as_deref(), model.active.as_deref()) {
            self.flipped = false;
        }
        if !same_file {
            self.scroll = UniformListScrollHandle::new();
            self.selection = None;
            self.md_selection = None;
            self.zoom = None;
            self.panning = None;
            self.md_doc = None;
            self.copied = None;
        }
        if !same_file || !keeps_images(&model.content) {
            self.images_in_view.clear();
        }
        // Rows whose image arrived or changed have another height now.
        if let Content::Markdown(doc) = &model.content {
            let changed = |path: &PathBuf| {
                model
                    .images
                    .get(path)
                    .is_some_and(|new| !self.model.images.get(path).is_some_and(|old| same_content(old, new)))
            };
            for (ix, row) in doc.rows.iter().enumerate() {
                if let RowKind::Image { image } = &row.kind
                    && let Some(DocImage { src: ImageSrc::File(path), .. }) = doc.images.get(*image)
                    && changed(path)
                {
                    self.md_list.remeasure_items(ix..ix + 1);
                }
            }
        }
        self.model = model;
        if let Content::Markdown(doc) = &self.model.content
            && !self.md_doc.as_ref().is_some_and(|shown| Arc::ptr_eq(shown, doc))
        {
            // A new parse of the same file (a live reload) keeps the place; another file starts at the top.
            let top = self.md_doc.is_some().then(|| self.md_list.logical_scroll_top());
            self.md_list.reset(doc.rows.len());
            if let Some(top) = top.filter(|_| !doc.rows.is_empty()) {
                let item_ix = top.item_ix.min(doc.rows.len() - 1);
                self.md_list.scroll_to(ListOffset { item_ix, ..top });
            }
            self.md_selection = None;
            self.copied = None;
            self.md_doc = Some(doc.clone());
        }
        // Other content (loading, an error) keeps the selection, so Text -> Loading -> Text on one file still has it.
        if let Content::Text { doc, .. } = &self.model.content {
            self.selection = self.selection.and_then(|selection| {
                let last = doc.line_count().checked_sub(1)?;
                let clamp = |(line, byte): (usize, usize)| {
                    let line = line.min(last);
                    (line, byte.min(doc.line_len(line)))
                };
                Some(TextSelection { anchor: clamp(selection.anchor), head: clamp(selection.head) })
            });
        }
        cx.notify();
    }

    /// Open a link of the markdown document: an anchor scrolls to its row, anything else goes to the workspace.
    pub(crate) fn open_link(&mut self, target: LinkTarget, cx: &mut Context<Self>) {
        match target {
            LinkTarget::Anchor(anchor) => {
                if let Some(row) = self.md_doc.as_ref().and_then(|doc| doc.anchor_row(&anchor)) {
                    self.md_list.scroll_to_reveal_item(row);
                    cx.notify();
                }
            }
            LinkTarget::Inert(_) => {}
            target => cx.emit(PreviewEvent::Link(target)),
        }
    }

    /// Copy a markdown code block's text and show `Copied` on its button for 1.5 s.
    pub(crate) fn copy_block(&mut self, block: usize, cx: &mut Context<Self>) {
        let Content::Markdown(doc) = &self.model.content else { return };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(doc.copy(block, 0..usize::MAX)));
        // A second click replaces the timer: dropping the old task cancels it.
        let timer = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1500)).await;
            this.update(cx, |this, cx| {
                this.copied = None;
                cx.notify();
            })
            .ok();
        });
        self.copied = Some((block, timer));
        cx.notify();
    }

    /// Measure the settings markdown rows depend on once per frame; a change remeasures the list, so the heights of
    /// rows out of view do not go stale.
    fn sync_md(&mut self, window: &Window, cx: &App) {
        let Content::Markdown(doc) = &self.model.content else { return };
        let settings = cx.global::<SettingsStore>().settings();
        let mirror = doc_rtl(settings.window.right_to_left, doc.rtl, self.flipped);
        let metrics = Self::metrics(window, cx);
        let key = (theme(cx).ui_scale.to_bits(), metrics.family.clone(), metrics.size.to_bits());
        let char_px = match &self.char_px_cache {
            Some((cached, px)) if *cached == key => *px,
            _ => {
                let px = preview_markdown::char_px(window, theme(cx));
                self.char_px_cache = Some((key, px));
                px
            }
        };
        let look = MdLook {
            family: metrics.family,
            size: metrics.size,
            line_height: metrics.row_height,
            ui_scale: theme(cx).ui_scale,
            rtl: settings.window.right_to_left,
            mirror,
            flipped: self.flipped,
            char_px,
        };
        if self.md_look.as_ref() != Some(&look) {
            if self.md_look.is_some() {
                self.md_list.remeasure();
            }
            self.md_look = Some(look);
        }
    }

    /// One row of the markdown document's list.
    fn md_row(&mut self, ix: usize, theme: &Theme, look: &MdLook, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Content::Markdown(doc) = &self.model.content else { return div().into_any_element() };
        let doc = doc.clone();
        self.note_row_image(&doc, ix);
        let image = match doc.rows.get(ix).map(|row| &row.kind) {
            Some(RowKind::Image { image }) => match doc.images.get(*image).map(|image| &image.src) {
                Some(ImageSrc::File(path)) => self.model.images.get(path).cloned(),
                _ => None,
            },
            _ => None,
        };
        let state = RowState {
            image,
            viewport: self.md_width,
            code_font: look.family.clone(),
            rtl: look.rtl,
            mirror: look.mirror,
            flipped: look.flipped,
            copied: self.copied.as_ref().map(|(block, _)| *block),
            selection: self.md_selection,
            char_px: look.char_px,
        };
        preview_markdown::row(&doc, ix, &state, theme, cx)
    }

    /// The image files of the markdown rows drawn at the last `ImagesInView`: the ones to read and keep decoded.
    pub(crate) fn images_in_view(&self) -> &[PathBuf] {
        &self.images_in_view
    }

    /// Note that the row `ix` is being laid out: its image file, when it has one that is an image, joins the frame's
    /// list (at most `MAX_IMAGES_IN_VIEW`).
    fn note_row_image(&mut self, doc: &MdDoc, ix: usize) {
        let Some(RowKind::Image { image }) = doc.rows.get(ix).map(|row| &row.kind) else { return };
        let Some(ImageSrc::File(path)) = doc.images.get(*image).map(|image| &image.src) else { return };
        if self.frame_images.len() < MAX_IMAGES_IN_VIEW
            && matches!(kind_of(path), FileKind::Image(_))
            && !self.frame_images.contains(path)
        {
            self.frame_images.push(path.clone());
        }
    }

    /// After the list laid out: tell the workspace when the images in view differ from the last report.
    fn sync_images(&mut self, cx: &mut Context<Self>) {
        // The list lays out with the width it had before this frame: one more frame when it changed.
        if f32::from(self.md_list.viewport_bounds().size.width) != self.md_width {
            cx.notify();
        }
        if self.frame_images != self.images_in_view {
            self.images_in_view = std::mem::take(&mut self.frame_images);
            cx.emit(PreviewEvent::ImagesInView);
        }
    }

    /// What the pane shows now.
    pub(crate) fn model(&self) -> &PreviewModel {
        &self.model
    }

    /// The selected text, `None` with nothing selected.
    pub(crate) fn copy_selection(&self) -> Option<String> {
        match &self.model.content {
            Content::Text { doc, .. } => {
                let selection = self.selection.filter(|s| s.anchor != s.head)?;
                Some(copy_text_selection(doc, &selection))
            }
            Content::Markdown(doc) => {
                let selection = self.md_selection.filter(|s| s.anchor != s.head)?;
                Some(copy_block_selection(doc, &selection))
            }
            _ => None,
        }
    }

    /// Whether a markdown selection with something in it exists (a click on a link that ends one does not open it).
    pub(crate) fn md_selecting(&self) -> bool {
        self.md_selection.is_some_and(|s| s.anchor != s.head)
    }

    /// A press or drag on the text of markdown block `block`, at byte `offset` of it.
    pub(crate) fn md_gesture(&mut self, block: usize, offset: usize, gesture: Gesture, cx: &mut Context<Self>) {
        let Content::Markdown(doc) = &self.model.content else { return };
        let Some(text) = doc.blocks.get(block).map(|b| b.text.as_str()) else { return };
        let mut offset = offset.min(text.len());
        while !text.is_char_boundary(offset) {
            offset -= 1;
        }
        match gesture {
            Gesture::Press { shift, clicks } => {
                let selection =
                    preview_markdown::press_selection(self.md_selection, block, offset, shift, clicks, text);
                let unit = Unit::of(clicks);
                let origin = if unit == Unit::Char { selection.anchor..selection.anchor } else { selection.range() };
                self.md_press = Some((unit, origin));
                self.md_selection = Some(selection);
                self.selection = None;
                self.dragging = true;
                self.pressed_text = true;
            }
            Gesture::Drag => {
                // A selection never leaves its block: another block's text does not move it.
                let (Some(selection), Some((unit, origin))) = (self.md_selection.as_mut(), self.md_press.as_ref())
                else {
                    return;
                };
                if !self.dragging || selection.block != block {
                    return;
                }
                let next = drag_selection(*unit, origin, block, offset, text);
                if next == *selection {
                    return;
                }
                *selection = next;
            }
        }
        cx.notify();
    }

    /// A press or drag on line `line` of a text file, at byte `shown` of the displayed line.
    fn text_gesture(&mut self, line: usize, shown: usize, gesture: Gesture, cx: &mut Context<Self>) {
        let Content::Text { doc, .. } = &self.model.content else { return };
        let at = (line, doc.raw_offset(line, shown));
        match gesture {
            Gesture::Press { shift, .. } => {
                let anchor = self.selection.filter(|_| shift).map_or(at, |s| s.anchor);
                self.selection = Some(TextSelection { anchor, head: at });
                self.md_selection = None;
                self.dragging = true;
                self.pressed_text = true;
            }
            Gesture::Drag => match self.selection.as_mut() {
                Some(selection) if self.dragging && selection.head != at => selection.head = at,
                _ => return,
            },
        }
        cx.notify();
    }

    /// A left press reached the root view: unless it was on text (or extends with Shift) it clears the selection.
    fn press_elsewhere(&mut self, shift: bool, cx: &mut Context<Self>) {
        let on_text = std::mem::take(&mut self.pressed_text);
        if !on_text && !shift && (self.selection.is_some() || self.md_selection.is_some()) {
            self.selection = None;
            self.md_selection = None;
            cx.notify();
        }
    }

    /// The button went up: the drag and its scroll loop end.
    fn end_drag(&mut self) {
        self.dragging = false;
        self.auto_scroll = None;
    }

    /// The top and bottom of the body's scrolling list in the window, when it shows text that can be selected.
    fn body_edges(&self) -> Option<(f32, f32)> {
        let bounds = match &self.model.content {
            Content::Markdown(_) => self.md_list.viewport_bounds(),
            Content::Text { .. } => self.scroll.0.borrow().base_handle.bounds(),
            _ => return None,
        };
        Some((f32::from(bounds.top()), f32::from(bounds.bottom())))
    }

    /// The pointer moved over the pane: while a selection drag holds it in an edge zone of the body, scroll.
    fn pointer_moved(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            // The release happened where this pane did not see it.
            self.dragging = false;
            self.auto_scroll = None;
            return;
        }
        let Some((top, bottom)) = self.body_edges() else { return };
        if self.auto_scroll.is_none() && auto_scroll_step(f32::from(event.position.y), top, bottom) != 0.0 {
            self.auto_scroll = Some(cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor().timer(AUTO_SCROLL_TICK).await;
                    let go = this.update_in(cx, |this, window, cx| this.auto_scroll_tick(window, cx)).unwrap_or(false);
                    if !go {
                        break;
                    }
                }
                this.update(cx, |this, _| this.auto_scroll = None).ok();
            }));
        }
    }

    /// One step of the drag scroll; `false` ends the loop (button up, window inactive or the pointer left the zone).
    fn auto_scroll_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.dragging || !window.is_window_active() {
            self.dragging = false;
            return false;
        }
        // Nothing changing (the end of the list) draws nothing either, so this only ends the loop: the next pointer
        // move over a visible pane starts it again.
        if pane_hidden(self.drawn.elapsed()) {
            return false;
        }
        let Some((top, bottom)) = self.body_edges() else { return false };
        let step = auto_scroll_step(f32::from(window.mouse_position().y), top, bottom);
        if step == 0.0 {
            return false;
        }
        let changed = match &self.model.content {
            Content::Markdown(doc) => {
                let (top, head) = (self.md_list.logical_scroll_top(), self.md_selection.map(|s| s.head));
                let place = |o: ListOffset| (o.item_ix, f32::from(o.offset_in_item));
                self.md_list.scroll_by(px(step));
                if let Some(selection) = self.md_selection.as_mut() {
                    let length = doc.blocks.get(selection.block).map_or(0, |b| b.text.len());
                    selection.head = if step < 0.0 { 0 } else { length };
                }
                place(top) != place(self.md_list.logical_scroll_top()) || head != self.md_selection.map(|s| s.head)
            }
            Content::Text { doc, .. } => {
                let handle = self.scroll.0.borrow().base_handle.clone();
                let (offset, max) = (handle.offset(), handle.max_offset());
                let moved = (f32::from(offset.y) - step).clamp(-f32::from(max.y), 0.0);
                handle.set_offset(point(offset.x, px(moved)));
                let row = Self::metrics(window, cx).row_height;
                let last = doc.line_count().saturating_sub(1);
                let first_in_view = ((-moved / row) as usize).min(last);
                let last_in_view = (((-moved + f32::from(handle.bounds().size.height)) / row) as usize).min(last);
                let head = self.selection.map(|s| s.head);
                if let Some(selection) = self.selection.as_mut() {
                    selection.head =
                        if step < 0.0 { (first_in_view, 0) } else { (last_in_view, doc.line_len(last_in_view)) };
                }
                moved != f32::from(offset.y) || head != self.selection.map(|s| s.head)
            }
            _ => return false,
        };
        if changed {
            cx.notify();
        }
        true
    }

    /// The active image's natural size.
    fn natural(&self) -> Option<(f32, f32)> {
        match &self.model.content {
            Content::Image { natural, .. } => Some(*natural),
            _ => None,
        }
    }

    /// The image body's size in px.
    fn viewport_size(&self) -> (f32, f32) {
        (f32::from(self.viewport.size.width), f32::from(self.viewport.size.height))
    }

    /// A window position as a point in the image body.
    fn in_body(&self, position: Point<Pixels>) -> (f32, f32) {
        (f32::from(position.x - self.viewport.origin.x), f32::from(position.y - self.viewport.origin.y))
    }

    /// Changes the zoom with `change` (given the natural and viewport sizes) and redraws.
    fn change_zoom(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Zoom, (f32, f32), (f32, f32))) {
        let (Some(natural), viewport) = (self.natural(), self.viewport_size()) else { return };
        if let Some(zoom) = self.zoom.as_mut() {
            change(zoom, natural, viewport);
            cx.notify();
        }
    }

    /// Zooms by `factor` around the center of the body (the zoom bar's buttons).
    fn zoom_center(&mut self, factor: f32, cx: &mut Context<Self>) {
        let (width, height) = self.viewport_size();
        self.change_zoom(cx, |zoom, natural, viewport| {
            zoom.zoom_at(factor, (width / 2.0, height / 2.0), natural, viewport)
        });
    }

    /// Places the image for the current content: first Fit, then adapted to the body's size.
    fn sync_zoom(&mut self) {
        let (Some(natural), viewport) = (self.natural(), self.viewport_size()) else { return };
        if viewport.0 <= 0.0 || viewport.1 <= 0.0 {
            return;
        }
        match self.zoom.as_mut() {
            Some(zoom) => zoom.relayout(natural, viewport),
            None => self.zoom = Some(Zoom::fit(natural, viewport)),
        }
    }

    /// One button of the zoom bar.
    fn zoom_button(
        &self,
        id: &'static str,
        label: &'static str,
        theme: &Theme,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .px(theme.ui(8.))
            .h(theme.ui(22.))
            .flex()
            .items_center()
            .rounded(px(RADIUS_SMALL))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme.tokens.surface)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.emit(PreviewEvent::Focused);
                    action(this, cx);
                    cx.stop_propagation();
                }),
            )
            .child(label)
    }

    /// The image with its zoom bar and, while part of it is out of view, the small map.
    fn image_body(
        &self,
        image: &Arc<gpui::Image>,
        natural: (f32, f32),
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let zoom = self.zoom;
        let view = cx.entity();
        let minimap = zoom.and_then(|zoom| zoom.minimap(natural, self.viewport_size()));
        let bar = div().absolute().left_0().right_0().bottom(theme.ui(12.)).flex().justify_center().child(
            div()
                .flex()
                .items_center()
                .gap(theme.ui(2.))
                .p(theme.ui(3.))
                .rounded(px(RADIUS_SMALL))
                .bg(rgb(theme.tokens.raised))
                .border_1()
                .border_color(rgb(theme.tokens.line))
                .text_xs()
                .font_family(UI_FONT)
                .text_color(rgb(theme.tokens.text))
                .child(self.zoom_button("zoom-out", "−", theme, cx, |this, cx| this.zoom_center(1.0 / STEP, cx)))
                .child(
                    div()
                        .min_w(theme.ui(40.))
                        .flex()
                        .justify_center()
                        .child(zoom.map(|zoom| zoom_label(&zoom)).unwrap_or_default()),
                )
                .child(self.zoom_button("zoom-in", "+", theme, cx, |this, cx| this.zoom_center(STEP, cx)))
                .child(self.zoom_button("zoom-fit", "Fit", theme, cx, |this, cx| {
                    this.change_zoom(cx, |zoom, natural, viewport| *zoom = Zoom::fit(natural, viewport));
                }))
                .child(self.zoom_button("zoom-100", "100%", theme, cx, |this, cx| {
                    let scale = this.zoom.map_or(1.0, |zoom| zoom.scale);
                    this.zoom_center(1.0 / scale, cx);
                })),
        );
        let map = minimap.map(|[x, y, w, h]| {
            let (box_width, box_height) = map_box(natural, f32::from(theme.ui(MAP_SIZE)));
            let (width, height) = (px(box_width), px(box_height));
            div()
                .absolute()
                .right(theme.ui(12.))
                .bottom(theme.ui(12.))
                .w(width)
                .h(height)
                .bg(rgb(theme.tokens.raised))
                .border_1()
                .border_color(rgb(theme.tokens.line))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_, _: &MouseDownEvent, _, cx| {
                        cx.emit(PreviewEvent::Focused);
                        cx.stop_propagation();
                    }),
                )
                .child(img(image.clone()).absolute().size_full().object_fit(ObjectFit::Contain))
                .child(
                    div()
                        .absolute()
                        .left(width * x)
                        .top(height * y)
                        .w(width * w)
                        .h(height * h)
                        .border_2()
                        .border_color(rgb(theme.tokens.accent)),
                )
        });
        div()
            .id("preview-image")
            .relative()
            .size_full()
            .overflow_hidden()
            .child(
                canvas(
                    move |bounds, _, cx| {
                        view.update(cx, |this, cx| {
                            let resized = (this.viewport.size.width - bounds.size.width).abs() > px(0.5)
                                || (this.viewport.size.height - bounds.size.height).abs() > px(0.5);
                            this.viewport = bounds;
                            if resized {
                                cx.notify();
                            }
                        })
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(zoom.map(|zoom| {
                img(image.clone())
                    .object_fit(ObjectFit::Fill)
                    .absolute()
                    .left(px(zoom.origin.0))
                    .top(px(zoom.origin.1))
                    .w(px(natural.0 * zoom.scale))
                    .h(px(natural.1 * zoom.scale))
            }))
            .child(bar)
            .children(map)
            .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                let pointer = this.in_body(event.position);
                let delta = event.delta;
                if event.modifiers.secondary() {
                    let factor = wheel_zoom_factor(delta);
                    this.change_zoom(cx, |zoom, natural, viewport| zoom.zoom_at(factor, pointer, natural, viewport));
                } else {
                    let pan = wheel_pan(delta);
                    this.change_zoom(cx, |zoom, natural, viewport| zoom.pan(pan, natural, viewport));
                }
                cx.stop_propagation();
            }))
            .on_pinch(cx.listener(|this, event: &PinchEvent, _, cx| {
                let pointer = this.in_body(event.position);
                let factor = 1.0 + event.delta;
                this.change_zoom(cx, |zoom, natural, viewport| zoom.zoom_at(factor, pointer, natural, viewport));
                cx.stop_propagation();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    cx.emit(PreviewEvent::Focused);
                    if event.click_count == 2 {
                        let pointer = this.in_body(event.position);
                        this.panning = None;
                        this.change_zoom(cx, |zoom, natural, viewport| zoom.toggle(pointer, natural, viewport));
                    } else {
                        this.panning = Some(event.position);
                    }
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let Some(last) = this.panning else { return };
                if event.pressed_button != Some(MouseButton::Left) {
                    this.panning = None;
                    return;
                }
                let delta = point(event.position.x - last.x, event.position.y - last.y);
                this.panning = Some(event.position);
                let pan = (f32::from(delta.x), f32::from(delta.y));
                this.change_zoom(cx, |zoom, natural, viewport| zoom.pan(pan, natural, viewport));
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, _| this.panning = None))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, _| this.panning = None))
            .into_any_element()
    }

    fn active_tab(&self) -> Option<&PreviewTab> {
        let id = self.model.active.as_ref()?;
        self.model.tabs.iter().find(|tab| &tab.id == id)
    }

    fn metrics(window: &Window, cx: &App) -> Metrics {
        let font = cx.global::<SettingsStore>().settings().terminal_font.clone();
        let family = installed_family(font.family.as_deref(), installed_families(cx)).to_string();
        let face = pane_font(Some(&family));
        let text_system = window.text_system();
        let id = text_system.resolve_font(&face);
        let cell_width =
            text_system.advance(id, px(font.size), 'm').map_or(font.size * 0.6, |advance| f32::from(advance.width));
        Metrics {
            family: family.into(),
            size: font.size,
            row_height: (font.size * font.line_height).round(),
            cell_width,
        }
    }

    fn header(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let label_rtl = cx.global::<SettingsStore>().settings().window.right_to_left;
        let leaf = self.leaf;
        let holding = self.model.holding;
        let (title, tag) = header_text(holding, &self.model.title);
        // The part of the label before the title: drawn as text, while the title goes through `bidi_label`.
        let prefix = title.strip_suffix(self.model.title.as_str()).unwrap_or("").trim_end().to_string();
        let has_title = !self.model.title.is_empty();
        let can_open = self.model.can_open;
        let can_flip = matches!(self.model.content, Content::Markdown(_))
            && cx.global::<SettingsStore>().settings().window.right_to_left;
        let flipped = self.flipped;
        let icon_button = |id: &'static str, path: &'static str, color: u32| {
            div()
                .id(id)
                .flex_none()
                .size(theme.ui(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(RADIUS_SMALL))
                .child(svg().path(path).size(theme.ui(14.)).text_color(rgb(color)))
        };
        div()
            .id("preview-header")
            .on_drag(
                DraggedPane { leaf, title: title.clone() },
                |dragged: &DraggedPane, _offset, _window, cx: &mut App| cx.new(|_| dragged.clone()),
            )
            .flex_none()
            .h(theme.ui(28.))
            .mb(theme.ui(4.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .rounded(px(RADIUS_SMALL))
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .bg(rgb(theme.tokens.raised))
            .text_color(rgb(theme.tokens.text))
            .child(svg().path(EYE_ICON).flex_none().size(theme.ui(14.)).text_color(rgb(theme.tokens.muted)))
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .truncate()
                    .when(!prefix.is_empty(), |label| {
                        label.child(div().flex_none().when(has_title, |d| d.mr(theme.ui(4.))).child(prefix))
                    })
                    .child(bidi_label(&self.model.title, label_rtl)),
            )
            .child(
                div()
                    .flex_none()
                    .px(theme.ui(8.))
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(theme.tokens.line))
                    .text_color(rgb(theme.tokens.muted))
                    .font_weight(FontWeight::NORMAL)
                    .child(tag),
            )
            .child(div().flex_1())
            .when(can_flip, |header| {
                header.child(
                    icon_button(
                        "preview-flip",
                        DIRECTION_ICON,
                        if flipped { theme.tokens.accent } else { theme.tokens.muted },
                    )
                    .when(flipped, |button| {
                        button.bg(rgb(theme.tokens.raised)).border_1().border_color(rgb(theme.tokens.accent))
                    })
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(theme.tokens.surface)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.flipped = !this.flipped;
                        cx.notify();
                    }))
                    .tooltip(hover_card::tooltip(plain_card("Flip text direction"))),
                )
            })
            .child(
                icon_button("preview-open", OPEN_EXTERNAL_ICON, theme.tokens.muted)
                    .when(!can_open, |button| button.opacity(0.4))
                    .when(can_open, |button| {
                        button
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(theme.tokens.surface)))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(PreviewEvent::OpenExternal)))
                    })
                    .tooltip(hover_card::tooltip(plain_card("Open in system viewer"))),
            )
            .child(
                icon_button("preview-close", CLOSE_ICON, theme.tokens.muted)
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(theme.tokens.surface)))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PreviewEvent::Close))),
            )
    }

    /// Which tabs fit the row and which are behind the `+N` button.
    fn tab_fit(&self, theme: &Theme) -> TabFit {
        let active_index = self.model.active.as_ref().and_then(|id| self.model.tabs.iter().position(|t| &t.id == id));
        fit_tabs(
            self.model.tabs.len(),
            active_index,
            self.row_width,
            f32::from(theme.ui(TAB_WIDTH)),
            f32::from(theme.ui(PLUS_WIDTH)),
        )
    }

    /// The tabs behind the `+N` button as `(id, name)`.
    pub(crate) fn hidden_tabs(&self, theme: &Theme) -> Vec<(String, String)> {
        let fit = self.tab_fit(theme);
        fit.hidden
            .iter()
            .filter_map(|&i| self.model.tabs.get(i))
            .map(|tab| (tab.id.clone(), tab.name.clone()))
            .collect()
    }

    fn tab_row(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let label_rtl = cx.global::<SettingsStore>().settings().window.right_to_left;
        let fit = self.tab_fit(theme);
        let view = cx.entity();
        let tabs = fit.shown.iter().filter_map(|&index| self.model.tabs.get(index).map(|tab| (index, tab)));
        div()
            .relative()
            .flex_none()
            .h(theme.ui(28.))
            .mb(theme.ui(4.))
            .flex()
            .items_center()
            .overflow_hidden()
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let width = f32::from(bounds.size.width);
                        view.update(cx, |this, cx| {
                            if (this.row_width - width).abs() > 0.5 {
                                this.row_width = width;
                                cx.notify();
                            }
                        })
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(tabs.map(|(_, tab)| {
                let active = self.model.active.as_deref() == Some(tab.id.as_str());
                let group = SharedString::from(format!("preview-tab-{}", tab.id));
                let (activate, remove, menu) = (tab.id.clone(), tab.id.clone(), tab.id.clone());
                let card = HoverCard {
                    title: tab.name.clone(),
                    status: tab.caption.clone(),
                    dir: None,
                    container: false,
                    host: None,
                    folder: Some(tab.path.display().to_string()),
                };
                // A tab of a following preview drags its file out (a copy: the tab stays).
                let dragged = self.model.host.clone().zip(self.model.session.clone()).map(|(host, session)| {
                    let file = SentFile {
                        id: tab.id.clone(),
                        path: tab.path.clone(),
                        sent_ms: tab.sent_ms,
                        caption: tab.caption.clone(),
                        backlog: false,
                    };
                    DraggedFile { file: DropFile { host, session: Some(session), file }, name: tab.name.clone() }
                });
                div()
                    .id(SharedString::from(format!("preview-tab-{}", tab.id)))
                    .group(group.clone())
                    .when_some(dragged, |row, dragged| {
                        row.on_drag(dragged, |dragged: &DraggedFile, _offset, _window, cx: &mut App| {
                            cx.new(|_| dragged.clone())
                        })
                    })
                    .flex_none()
                    .w(theme.ui(TAB_WIDTH))
                    .h_full()
                    .px(theme.ui(8.))
                    .flex()
                    .items_center()
                    .gap(theme.ui(6.))
                    .text_xs()
                    .cursor_pointer()
                    .border_b_2()
                    .border_color(rgb(if active { theme.tokens.accent } else { theme.tokens.ground }))
                    .when(active, |row| row.bg(rgb(theme.tokens.raised)).text_color(rgb(theme.tokens.text)))
                    .when(!active, |row| {
                        row.text_color(rgb(theme.tokens.muted)).hover(|style| style.bg(rgb(theme.tokens.surface)))
                    })
                    .child(svg().path(FILE_ICON).flex_none().size(theme.ui(13.)).text_color(rgb(theme.tokens.muted)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(bidi_label(&tab.name, label_rtl)),
                    )
                    .when(tab.unseen, |row| {
                        row.child(div().flex_none().size(theme.ui(6.)).rounded_full().bg(rgb(theme.tokens.accent)))
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("preview-tab-x-{}", tab.id)))
                            .flex_none()
                            .size(theme.ui(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(RADIUS_SMALL))
                            .invisible()
                            .group_hover(group, |style| style.visible())
                            .hover(|style| style.bg(rgb(theme.tokens.line)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |_, _, _, cx| {
                                    cx.emit(PreviewEvent::Focused);
                                    cx.emit(PreviewEvent::Remove(remove.clone()));
                                    cx.stop_propagation();
                                }),
                            )
                            .child(svg().path(CLOSE_ICON).size(theme.ui(10.)).text_color(rgb(theme.tokens.muted))),
                    )
                    .tooltip(hover_card::tooltip(card))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(PreviewEvent::Activate(activate.clone()))))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                            cx.emit(PreviewEvent::Focused);
                            cx.emit(PreviewEvent::Menu { position: event.position, file: Some(menu.clone()) });
                            cx.stop_propagation();
                        }),
                    )
            }))
            .when(!fit.hidden.is_empty(), |row| {
                row.child(
                    div()
                        .id("preview-hidden")
                        .flex_none()
                        .w(theme.ui(PLUS_WIDTH))
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(rgb(theme.tokens.muted))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(theme.tokens.surface)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, event: &MouseDownEvent, _, cx| {
                                cx.emit(PreviewEvent::Focused);
                                cx.emit(PreviewEvent::Hidden { position: event.position });
                                cx.stop_propagation();
                            }),
                        )
                        .child(format!("+{}", fit.hidden.len())),
                )
            })
    }

    /// The visible lines of a text file.
    fn text_rows(&self, range: Range<usize>, window: &mut Window, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let Content::Text { doc, highlights } = &self.model.content else { return Vec::new() };
        let theme = theme(cx).clone();
        let metrics = Self::metrics(window, cx);
        let font = pane_font(Some(&metrics.family));
        let width = px(metrics.cell_width * (doc.widest() as f32 + 2.0) + 24.0);
        let right_to_left = cx.global::<SettingsStore>().settings().window.right_to_left;
        let view = cx.entity();
        range
            .filter(|&index| index < doc.line_count())
            .map(|index| {
                let selected = self
                    .selection
                    .and_then(|selection| line_range(&selection, index, doc.line_len(index)))
                    .map(|raw| display_range(doc, index, raw));
                let view = view.clone();
                let interact = Interact {
                    hit: Rc::new(move |gesture, shown, app| {
                        view.update(app, |this, cx| this.text_gesture(index, shown, gesture, cx))
                    }),
                    selected,
                };
                let shown = doc.display(index);
                let spans = match highlights {
                    Some(highlights) if doc.line_len(index) == shown.len() => {
                        highlights.line(index).iter().map(|s| (s.range.clone(), s.token)).collect()
                    }
                    Some(highlights) => display_spans(&doc.copy(index..index + 1), highlights.line(index)),
                    None => Vec::new(),
                };
                div()
                    .id(("preview-row", index))
                    .h(px(metrics.row_height))
                    .w_full()
                    .min_w(width)
                    .flex()
                    .items_center()
                    .whitespace_nowrap()
                    .font_family(metrics.family.clone())
                    .text_size(px(metrics.size))
                    .bg(rgb(theme.tokens.terminal))
                    .text_color(rgb(theme.tokens.text))
                    .child(pad(px(12.), &interact.hit, 0))
                    .child(code_line(&shown, &spans, &font, right_to_left, false, &theme, &interact))
                    .child(pad(px(12.), &interact.hit, shown.len()))
                    .into_any_element()
            })
            .collect()
    }

    fn body(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let label_rtl = cx.global::<SettingsStore>().settings().window.right_to_left;
        let muted = |text: &'static str| div().text_sm().text_color(rgb(theme.tokens.muted)).child(text);
        let centered = |children: Vec<gpui::AnyElement>| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p(theme.ui(16.))
                .child(card(theme).children(children))
                .into_any_element()
        };
        let name = self.active_tab().map(|tab| file_name(&tab.path)).unwrap_or_default();
        let folder = self.active_tab().and_then(|tab| tab.path.parent().map(Path::to_path_buf));
        let file_card = |lines: Vec<gpui::AnyElement>| {
            let mut all = vec![
                svg().path(FILE_ICON).size(theme.ui(28.)).text_color(rgb(theme.tokens.muted)).into_any_element(),
                div().font_weight(FontWeight::SEMIBOLD).child(bidi_label(&name, label_rtl)).into_any_element(),
            ];
            all.extend(lines);
            centered(all)
        };
        match &self.model.content {
            Content::NoFiles => {
                div().size_full().flex().items_center().justify_center().child(muted("No files yet")).into_any_element()
            }
            Content::Loading => div().size_full().into_any_element(),
            Content::Markdown(_) => {
                let view = cx.entity();
                let theme = theme.clone();
                let look = self.md_look.clone().unwrap_or_default();
                let after = cx.entity();
                div()
                    .size_full()
                    .child(
                        list(self.md_list.clone(), move |ix, _, cx| {
                            view.update(cx, |this, cx| this.md_row(ix, &theme, &look, cx))
                        })
                        .size_full(),
                    )
                    // Paints after the list laid out its rows, so the frame's images are complete.
                    .child(
                        canvas(move |_, _, cx| after.update(cx, |this, cx| this.sync_images(cx)), |_, _, _, _| {})
                            .absolute()
                            .size_0(),
                    )
                    .into_any_element()
            }
            Content::Text { doc, .. } => {
                // Every row has its own `min_w`, so the width needs no scan of the document.
                let longest = (doc.line_count() > 0).then_some(0);
                uniform_list(
                    "preview-rows",
                    doc.line_count(),
                    cx.processor(|this, range: Range<usize>, window, cx| this.text_rows(range, window, cx)),
                )
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .with_width_from_item(longest)
                .track_scroll(&self.scroll)
                .size_full()
                .into_any_element()
            }
            Content::Image { image, natural } => self.image_body(image, *natural, theme, cx),
            Content::Missing => centered(vec![muted("This file no longer exists").into_any_element()]),
            Content::TooLarge(size) => {
                let mut lines =
                    vec![div().font_weight(FontWeight::SEMIBOLD).child("Too large to preview").into_any_element()];
                if let Some(size) = size {
                    lines.push(muted_line(size_text(*size), theme));
                }
                centered(lines)
            }
            Content::Unsupported { size } => {
                let mut lines = Vec::new();
                if let Some(folder) = folder {
                    lines.push(muted_line(folder.display().to_string(), theme));
                }
                if let Some(size) = size {
                    lines.push(muted_line(size_text(*size), theme));
                }
                lines.push(muted("This file type has no preview").into_any_element());
                file_card(lines)
            }
            Content::Html => file_card(vec![muted("HTML files open in your browser").into_any_element()]),
            Content::Failed(reason) => centered(vec![muted_line(reason.clone(), theme)]),
        }
    }
}

/// The card the state bodies sit in.
fn card(theme: &Theme) -> gpui::Div {
    div()
        .max_w(theme.ui(360.))
        .px(theme.ui(20.))
        .py(theme.ui(18.))
        .flex()
        .flex_col()
        .items_center()
        .gap(theme.ui(10.))
        .rounded(px(RADIUS))
        .bg(rgb(theme.tokens.raised))
        .border_1()
        .border_color(rgb(theme.tokens.line))
        .text_color(rgb(theme.tokens.text))
        .font_family(UI_FONT)
        .text_sm()
}

/// A small muted line in a card.
fn muted_line(text: String, theme: &Theme) -> gpui::AnyElement {
    div().text_xs().text_color(rgb(theme.tokens.muted)).child(text).into_any_element()
}

/// Whether the flip toggle survives a model change: a reload of the same file keeps it, another file (or none) resets.
pub(crate) fn keep_flip(old_active: Option<&str>, new_active: Option<&str>) -> bool {
    old_active.is_some() && old_active == new_active
}

/// A hover card with only a title.
fn plain_card(title: &str) -> HoverCard {
    HoverCard { title: title.to_string(), status: None, dir: None, container: false, host: None, folder: None }
}

impl Render for PreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.drawn = Instant::now();
        self.sync_zoom();
        self.sync_md(window, cx);
        self.frame_images.clear();
        if matches!(self.model.content, Content::Markdown(_)) {
            self.md_width = f32::from(self.md_list.viewport_bounds().size.width);
        }
        let theme = theme(cx).clone();
        let active = self.model.active.clone();
        let host_card = self.model.host_card.clone();
        let body = match host_card {
            Some(card) => host_card_element(
                card,
                &theme,
                cx.listener(|_, _, _, cx| cx.emit(PreviewEvent::HostAction)),
                cx.listener(|_, _, _, cx| cx.emit(PreviewEvent::Close)),
            )
            .into_any_element(),
            None => self.body(&theme, cx),
        };
        div()
            .key_context("Preview")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let key = &event.keystroke;
                if key.key == "delete" && !key.modifiers.modified() {
                    if let Some(id) = this.model.active.clone() {
                        cx.emit(PreviewEvent::Remove(id));
                        cx.stop_propagation();
                    }
                } else if key.key == "c"
                    && key.modifiers == Modifiers::secondary_key()
                    && let Some(text) = this.copy_selection()
                {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.press_elsewhere(event.modifiers.shift, cx);
                    cx.emit(PreviewEvent::Focused);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                    cx.emit(PreviewEvent::Focused);
                    cx.emit(PreviewEvent::Menu { position: event.position, file: active.clone() });
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &MouseMoveEvent, window, cx| this.pointer_moved(event, window, cx)),
            )
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, _| this.end_drag()))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, _| this.end_drag()))
            .child(self.header(&theme, cx))
            .when(!self.model.tabs.is_empty(), |pane| pane.child(self.tab_row(&theme, cx)))
            .child(div().relative().flex_1().min_h_0().bg(rgb(theme.tokens.terminal)).child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_text_depends_on_the_mode() {
        assert_eq!(header_text(true, "plan.md"), ("plan.md".to_string(), "Holds this file"));
        assert_eq!(header_text(false, "api"), ("Preview · api".to_string(), "Follows session"));
        assert_eq!(header_text(false, ""), ("Preview".to_string(), "Follows session"));
    }

    #[test]
    fn labels_with_hebrew_are_split_per_character() {
        assert_eq!(label_parts("notes.md", true), None);
        assert_eq!(label_parts("Fix the build", false), None, "an ASCII title is one element");
        let text = "תוכנית.md";
        let display: Vec<char> = app_core::bidi::display_line(text).unwrap().into_iter().map(|(_, c)| c).collect();
        assert_eq!(label_parts(text, true), Some(display));
        assert_eq!(label_parts(text, false), Some(text.chars().collect()));
    }

    #[test]
    fn the_flip_resets_on_another_file_but_not_on_reload() {
        assert!(keep_flip(Some("a.md"), Some("a.md")));
        assert!(!keep_flip(Some("a.md"), Some("b.md")));
        assert!(!keep_flip(Some("a.md"), None));
        assert!(!keep_flip(None, Some("a.md")));
        assert!(!keep_flip(None, None));
    }

    #[test]
    fn block_selection_orders_its_range() {
        assert_eq!(BlockSelection { block: 0, anchor: 7, head: 2 }.range(), 2..7);
        assert_eq!(BlockSelection { block: 0, anchor: 2, head: 7 }.range(), 2..7);
    }

    #[test]
    fn text_selection_copies_across_lines() {
        let doc = TextDoc::parse(b"abc\ndef").unwrap();
        let selection = TextSelection { anchor: (1, 1), head: (0, 1) };
        assert_eq!(copy_text_selection(&doc, &selection), "bc\nd");
    }

    #[test]
    fn markdown_copy_stays_in_the_block() {
        let doc = preview_doc::markdown::parse("first one\n\nsecond", Path::new("/"));
        let selection = BlockSelection { block: 0, anchor: 6, head: 999 };
        assert_eq!(copy_block_selection(&doc, &selection), "one");
        let nothing = BlockSelection { block: 0, anchor: 3, head: 3 };
        assert_eq!(copy_block_selection(&doc, &nothing), "");
    }

    #[test]
    fn a_text_selection_paints_its_part_of_each_line() {
        let selection = TextSelection { anchor: (3, 4), head: (1, 2) };
        assert_eq!(line_range(&selection, 0, 9), None);
        assert_eq!(line_range(&selection, 1, 9), Some(2..9));
        assert_eq!(line_range(&selection, 2, 5), Some(0..5));
        assert_eq!(line_range(&selection, 3, 9), Some(0..4));
        assert_eq!(line_range(&selection, 4, 9), None);
        let one_line = TextSelection { anchor: (2, 6), head: (2, 3) };
        assert_eq!(line_range(&one_line, 2, 9), Some(3..6));
        assert_eq!(line_range(&one_line, 2, 4), Some(3..4));
    }

    #[test]
    fn a_pane_is_hidden_only_after_a_while_undrawn() {
        assert!(!pane_hidden(Duration::from_millis(20)));
        assert!(pane_hidden(Duration::from_millis(150)));
    }

    #[test]
    fn auto_scroll_is_zero_away_from_the_edges_and_grows_into_them() {
        assert_eq!(auto_scroll_step(250.0, 0.0, 500.0), 0.0);
        assert_eq!(auto_scroll_step(0.0, 0.0, 500.0), -20.0);
        assert_eq!(auto_scroll_step(12.0, 0.0, 500.0), -10.0);
        assert_eq!(auto_scroll_step(500.0, 0.0, 500.0), 20.0);
        assert_eq!(auto_scroll_step(600.0, 0.0, 500.0), 20.0);
        assert_eq!(auto_scroll_step(-50.0, 0.0, 500.0), -20.0);
    }

    #[test]
    fn a_display_offset_maps_back_to_the_raw_line() {
        let doc = TextDoc::parse(b"\tab\tc").unwrap();
        // Shown as "    ab    c".
        assert_eq!(display_range(&doc, 0, 0..1), 0..4);
        assert_eq!(display_range(&doc, 0, 1..3), 4..6);
        assert_eq!(display_range(&doc, 0, 0..doc.line_len(0)), 0..doc.display(0).len());
    }

    #[test]
    fn the_map_fits_a_square_box() {
        assert_eq!(map_box((1000.0, 20000.0), 120.0), (6.0, 120.0));
        assert_eq!(map_box((2000.0, 1000.0), 120.0), (120.0, 60.0));
        assert_eq!(map_box((50.0, 50.0), 120.0), (120.0, 120.0));
    }

    #[test]
    fn zoom_bar_label_is_the_rounded_percent() {
        let mut zoom = Zoom::fit((1000.0, 1000.0), (333.0, 333.0));
        zoom.scale = 0.333;
        assert_eq!(zoom_label(&zoom), "33%");
    }

    #[test]
    fn a_wheel_line_zooms_one_step_and_up_zooms_in() {
        let line = ScrollDelta::Lines(gpui::point(0.0, 1.0));
        assert_eq!(wheel_zoom_factor(line), STEP);
        assert!((wheel_zoom_factor(ScrollDelta::Pixels(gpui::point(px(0.), px(-50.)))) - 1.0 / STEP).abs() < 1e-6);
    }

    #[test]
    fn a_wheel_line_pans_20_px_and_pixels_pass_through() {
        assert_eq!(wheel_pan(ScrollDelta::Lines(gpui::point(1.0, -2.0))), (20.0, -40.0));
        assert_eq!(wheel_pan(ScrollDelta::Pixels(gpui::point(px(3.), px(4.)))), (3.0, 4.0));
    }
}
