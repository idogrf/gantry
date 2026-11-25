//! The preview pane's view: a header, a row of file tabs and the active file's body (text lines, an image or a card
//! saying why there is nothing to draw). It draws a `PreviewModel` and reports what the user did as `PreviewEvent`s;
//! `workspace_view` owns the model and acts on the events.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use app_core::host_state::HostCard;
use app_core::preview::{TabFit, TextDoc, file_name, fit_tabs};
use app_core::preview_zoom::{STEP, WHEEL_STEP_PX, Zoom, wheel_factor};
use app_core::split_tree::NodeId;
use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, KeyDownEvent,
    ListHorizontalSizingBehavior, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    PinchEvent, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, SharedString, UniformListScrollHandle, Window,
    canvas, div, img, point, prelude::*, px, rgb, svg, uniform_list,
};

use crate::assets::{CLOSE_ICON, EYE_ICON, FILE_ICON, OPEN_EXTERNAL_ICON};
use crate::git_diff_view::{display_chars, size_text};
use crate::hover_card::{self, HoverCard};
use crate::pane_drag::DraggedPane;
use crate::settings_store::SettingsStore;
use crate::terminal_view::{host_card_element, installed_families, installed_family, pane_font};
use crate::theme::{RADIUS, RADIUS_SMALL, Theme, UI_FONT, theme};
use app_core::theme_colors::mix;

/// The width of a file tab, in UI px.
pub(crate) const TAB_WIDTH: f32 = 160.0;
/// The width of the `+N` button, in UI px.
pub(crate) const PLUS_WIDTH: f32 = 44.0;

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
}

/// What the body shows for the active file.
#[derive(Clone)]
pub(crate) enum Content {
    /// The session has sent no files.
    NoFiles,
    /// The file is being read.
    Loading,
    /// A text file.
    Text(Arc<TextDoc>),
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

/// Everything the view draws.
pub(crate) struct PreviewModel {
    /// The session's title, after `Preview ·` in the header.
    pub title: String,
    /// The file tabs, oldest first.
    pub tabs: Vec<PreviewTab>,
    /// The active tab's id.
    pub active: Option<String>,
    /// The active file's body.
    pub content: Content,
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
}

/// The lines between the two ends of a selection, whichever end is first.
fn selection_lines(anchor: usize, head: usize) -> Range<usize> {
    anchor.min(head)..anchor.max(head) + 1
}

/// The text copied for a selection: whole lines, as the file has them.
fn copy_lines(doc: &TextDoc, lines: Range<usize>) -> String {
    doc.copy(lines)
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
    /// The selected lines as (anchor, head).
    selection: Option<(usize, usize)>,
    dragging: bool,
    /// The tab row's width in px, measured while drawing.
    row_width: f32,
    /// How the active image is placed; `None` until an image is shown, reset when the active file changes.
    zoom: Option<Zoom>,
    /// The image body's bounds in the window, measured while drawing.
    viewport: Bounds<Pixels>,
    /// The last pointer position of a drag that pans the image.
    panning: Option<Point<Pixels>>,
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
                tabs: Vec::new(),
                active: None,
                content: Content::NoFiles,
                host_card: None,
                can_open: false,
            },
            scroll: UniformListScrollHandle::new(),
            selection: None,
            dragging: false,
            row_width: 0.0,
            zoom: None,
            viewport: Bounds::default(),
            panning: None,
        }
    }

    /// Show `model`. Scroll and selection stay while the active file is the same (the selection cut to the new line
    /// count) and start over when it changes.
    pub(crate) fn set_model(&mut self, model: PreviewModel, cx: &mut Context<Self>) {
        if model.active != self.model.active {
            self.scroll = UniformListScrollHandle::new();
            self.selection = None;
            self.zoom = None;
            self.panning = None;
        }
        self.model = model;
        // Other content (loading, an error) keeps the selection, so Text -> Loading -> Text on one file still has it.
        if let Content::Text(doc) = &self.model.content {
            self.selection = self.selection.and_then(|(anchor, head)| {
                let last = doc.line_count().checked_sub(1)?;
                Some((anchor.min(last), head.min(last)))
            });
        }
        cx.notify();
    }

    /// What the pane shows now.
    pub(crate) fn model(&self) -> &PreviewModel {
        &self.model
    }

    /// The selected lines as text, `None` with no selection.
    pub(crate) fn copy_selection(&self) -> Option<String> {
        let Content::Text(doc) = &self.model.content else { return None };
        let (anchor, head) = self.selection?;
        Some(copy_lines(doc, selection_lines(anchor, head)))
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
        let leaf = self.leaf;
        let title = format!("Preview · {}", self.model.title);
        let can_open = self.model.can_open;
        let icon_button = |id: &'static str, path: &'static str| {
            div()
                .id(id)
                .flex_none()
                .size(theme.ui(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(RADIUS_SMALL))
                .child(svg().path(path).size(theme.ui(14.)).text_color(rgb(theme.tokens.muted)))
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
            .child(div().min_w_0().truncate().child(title))
            .child(
                div()
                    .flex_none()
                    .px(theme.ui(8.))
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(theme.tokens.line))
                    .text_color(rgb(theme.tokens.muted))
                    .font_weight(FontWeight::NORMAL)
                    .child("Follows session"),
            )
            .child(div().flex_1())
            .child(
                icon_button("preview-open", OPEN_EXTERNAL_ICON)
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
                icon_button("preview-close", CLOSE_ICON)
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
            .children(tabs.map(|(index, tab)| {
                let active = self.model.active.as_deref() == Some(tab.id.as_str());
                let group = SharedString::from(format!("preview-tab-{index}"));
                let (activate, remove, menu) = (tab.id.clone(), tab.id.clone(), tab.id.clone());
                let card = HoverCard {
                    title: tab.name.clone(),
                    status: tab.caption.clone(),
                    dir: None,
                    container: false,
                    host: None,
                    folder: Some(tab.path.display().to_string()),
                };
                div()
                    .id(("preview-tab", index))
                    .group(group.clone())
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
                            .child(tab.name.clone()),
                    )
                    .when(tab.unseen, |row| {
                        row.child(div().flex_none().size(theme.ui(6.)).rounded_full().bg(rgb(theme.tokens.accent)))
                    })
                    .child(
                        div()
                            .id(("preview-tab-x", index))
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
        let Content::Text(doc) = &self.model.content else { return Vec::new() };
        let theme = theme(cx).clone();
        let metrics = Self::metrics(window, cx);
        let width = px(metrics.cell_width * (doc.widest() as f32 + 2.0) + 24.0);
        let right_to_left = cx.global::<SettingsStore>().settings().window.right_to_left;
        let selected = self.selection.map(|(anchor, head)| selection_lines(anchor, head));
        range
            .filter(|&index| index < doc.line_count())
            .map(|index| {
                let background = if selected.as_ref().is_some_and(|lines| lines.contains(&index)) {
                    mix(theme.tokens.accent, theme.tokens.terminal, 0.30)
                } else {
                    theme.tokens.terminal
                };
                div()
                    .id(("preview-row", index))
                    .h(px(metrics.row_height))
                    .w_full()
                    .min_w(width)
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .whitespace_nowrap()
                    .font_family(metrics.family.clone())
                    .text_size(px(metrics.size))
                    .bg(rgb(background))
                    .text_color(rgb(theme.tokens.text))
                    .child(line_text(&doc.display(index), right_to_left))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            let anchor = if event.modifiers.shift {
                                this.selection.map_or(index, |(anchor, _)| anchor)
                            } else {
                                index
                            };
                            this.selection = Some((anchor, index));
                            this.dragging = true;
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                        if this.dragging
                            && event.pressed_button == Some(MouseButton::Left)
                            && let Some((_, head)) = this.selection.as_mut().filter(|(_, head)| *head != index)
                        {
                            *head = index;
                            cx.notify();
                        }
                    }))
                    .into_any_element()
            })
            .collect()
    }

    fn body(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
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
                div().font_weight(FontWeight::SEMIBOLD).child(name.clone()).into_any_element(),
            ];
            all.extend(lines);
            centered(all)
        };
        match &self.model.content {
            Content::NoFiles => {
                div().size_full().flex().items_center().justify_center().child(muted("No files yet")).into_any_element()
            }
            Content::Loading => div().size_full().into_any_element(),
            Content::Text(doc) => {
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

/// One line as an element. With right-to-left display on, a line with Hebrew or Arabic letters is drawn in display
/// order, one character at a time (the shaper never gets right-to-left text); selection and copy stay logical.
fn line_text(text: &str, right_to_left: bool) -> gpui::Div {
    let reordered = right_to_left.then(|| display_chars(text, &[])).flatten();
    match reordered {
        Some(chars) => div().flex().items_center().children(chars.into_iter().map(|(ch, _)| div().child(ch))),
        None => div().child(SharedString::from(text.to_string())),
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

/// A hover card with only a title.
fn plain_card(title: &str) -> HoverCard {
    HoverCard { title: title.to_string(), status: None, dir: None, container: false, host: None, folder: None }
}

impl Render for PreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_zoom();
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
                cx.listener(|_, _, _, cx| {
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
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, _| this.dragging = false))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, _| this.dragging = false))
            .child(self.header(&theme, cx))
            .when(!self.model.tabs.is_empty(), |pane| pane.child(self.tab_row(&theme, cx)))
            .child(div().relative().flex_1().min_h_0().bg(rgb(theme.tokens.terminal)).child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_runs_either_way() {
        assert_eq!(selection_lines(5, 2), 2..6);
        assert_eq!(selection_lines(2, 5), 2..6);
    }

    #[test]
    fn copy_selection_copies_whole_lines() {
        let doc = TextDoc::parse(b"a\nb\nc").unwrap();
        assert_eq!(copy_lines(&doc, 1..3), "b\nc");
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
