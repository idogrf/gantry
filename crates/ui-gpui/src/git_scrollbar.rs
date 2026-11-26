//! The diff page's scrollbars: a vertical and a horizontal one over the list, drawn like a pane's (accent while the
//! view moves, muted when it rests). `workspace_view` wires the presses and drags; this holds the geometry and the look.

use std::cell::Cell;
use std::time::Instant;

use app_core::scroll::{Thumb, activity, offset_for_extent, thumb_for_extent};
use app_core::theme_colors::{deepen, mix};
use gpui::{Bounds, ListState, Pixels, Rgba, ScrollHandle, UniformListScrollHandle, div, prelude::*, px, rgb};

use crate::theme::Theme;

/// Thickness of a scrollbar track, before the interface scale.
pub(crate) const WIDTH: f32 = 10.0;
/// The shortest a thumb gets, before the interface scale.
const MIN_THUMB: f32 = 24.0;

/// The two directions a list scrolls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bar {
    Vertical,
    Horizontal,
}

/// How far a list is scrolled: all in pixels, `x` and `y` as (horizontal, vertical).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Extent {
    /// The visible size.
    pub viewport: (f32, f32),
    /// The size of everything.
    pub contents: (f32, f32),
    /// How far the start is scrolled out of view.
    pub offset: (f32, f32),
    /// Where the list is drawn.
    pub bounds: Bounds<Pixels>,
    /// Each track gives up a corner to the other bar when both show; off when the bars lie outside the list.
    pub shared_corner: bool,
}

impl Extent {
    /// The extent of a list, once it has been laid out.
    pub(crate) fn of(handle: &UniformListScrollHandle) -> Option<Extent> {
        let state = handle.0.borrow();
        let size = state.last_item_size?;
        let offset = state.base_handle.offset();
        Some(Extent {
            viewport: (f32::from(size.item.width), f32::from(size.item.height)),
            contents: (f32::from(size.contents.width), f32::from(size.contents.height)),
            offset: (-f32::from(offset.x), -f32::from(offset.y)),
            bounds: state.base_handle.bounds(),
            shared_corner: true,
        })
    }

    /// The extent of a variable-height list, from its own scrollbar measures (the height is an estimate until the
    /// rows are measured). Only a vertical bar, outside the list.
    pub(crate) fn of_list(state: &ListState) -> Option<Extent> {
        let bounds = state.viewport_bounds();
        if bounds.size.height <= px(0.) {
            return None;
        }
        let height = f32::from(bounds.size.height);
        Some(Extent {
            viewport: (f32::from(bounds.size.width), height),
            contents: (f32::from(bounds.size.width), height + f32::from(state.max_offset_for_scrollbar().y)),
            offset: (0.0, -f32::from(state.scroll_px_offset_for_scrollbar().y)),
            bounds,
            shared_corner: false,
        })
    }

    /// The extent of a plain scrolling element tracked by `handle`: only a vertical bar, outside the element.
    pub(crate) fn of_handle(handle: &ScrollHandle) -> Option<Extent> {
        let bounds = handle.bounds();
        if bounds.size.height <= px(0.) {
            return None;
        }
        let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        Some(Extent {
            viewport: (width, height),
            contents: (width, height + f32::from(handle.max_offset().y)),
            offset: (0.0, -f32::from(handle.offset().y)),
            bounds,
            shared_corner: false,
        })
    }

    /// What a track gives up to the other bar.
    fn corner(&self, ui_scale: f32) -> f32 {
        if self.shared_corner { WIDTH * ui_scale } else { 0.0 }
    }

    /// The thumbs, vertical then horizontal; each track gives up a corner to the other bar when both show.
    pub(crate) fn thumbs(&self, ui_scale: f32) -> (Option<Thumb>, Option<Thumb>) {
        let min = MIN_THUMB * ui_scale;
        let corner = self.corner(ui_scale);
        let (vx, vy) = (self.contents.0 > self.viewport.0 + 0.5, self.contents.1 > self.viewport.1 + 0.5);
        let v_track = self.viewport.1 - if vx { corner } else { 0.0 };
        let h_track = self.viewport.0 - if vy { corner } else { 0.0 };
        (
            thumb_for_extent(v_track, self.viewport.1, self.contents.1, self.offset.1, min),
            thumb_for_extent(h_track, self.viewport.0, self.contents.0, self.offset.0, min),
        )
    }

    /// The offset along `bar` for a thumb of length `len` grabbed so its start is at `top` on the track.
    pub(crate) fn offset_for(&self, bar: Bar, top: f32, len: f32, ui_scale: f32) -> f32 {
        let corner = self.corner(ui_scale);
        let (vx, vy) = (self.contents.0 > self.viewport.0 + 0.5, self.contents.1 > self.viewport.1 + 0.5);
        match bar {
            Bar::Vertical => {
                let track = self.viewport.1 - if vx { corner } else { 0.0 };
                offset_for_extent(top, track, len, self.viewport.1, self.contents.1)
            }
            Bar::Horizontal => {
                let track = self.viewport.0 - if vy { corner } else { 0.0 };
                offset_for_extent(top, track, len, self.viewport.0, self.contents.0)
            }
        }
    }
}

/// What the page remembers about its scrollbars between frames.
pub(crate) struct Bars {
    /// When the view last moved: the thumbs are in their active color for a while after.
    moved: Cell<Option<Instant>>,
    /// The offset seen at the last frame, to notice a movement.
    seen: Cell<(f32, f32)>,
    /// While a thumb is dragged: which one, and how far below its start it was grabbed.
    pub(crate) grab: Option<(Bar, f32)>,
}

impl Bars {
    pub(crate) fn new() -> Self {
        Bars { moved: Cell::new(None), seen: Cell::new((0.0, 0.0)), grab: None }
    }

    /// Note a frame's offset; a change counts as movement.
    pub(crate) fn see(&self, offset: (f32, f32)) {
        if self.seen.replace(offset) != offset {
            self.moved.set(Some(Instant::now()));
        }
    }

    /// The thumbs' color level: 1 = active (accent), 0 = resting (muted).
    pub(crate) fn level(&self) -> f32 {
        if self.grab.is_some() {
            return 1.0;
        }
        self.moved.get().map_or(0.0, |at| activity(at.elapsed().as_millis() as u64))
    }
}

/// A thumb's color at `level`: the accent, deepened, fading to grey as the view rests.
pub(crate) fn thumb_color(theme: &Theme, level: f32) -> Rgba {
    let mut color = rgb(mix(deepen(theme.tokens.accent, 0.62), theme.tokens.muted, level));
    color.a = 0.45 + 0.5 * level;
    color
}

/// One thumb, drawn inside its track.
pub(crate) fn thumb_div(bar: Bar, thumb: Thumb, color: Rgba, theme: &Theme) -> gpui::Div {
    let thickness = theme.ui(WIDTH - 4.);
    let base = div().absolute().rounded(theme.ui((WIDTH - 4.) / 2.)).bg(color);
    match bar {
        Bar::Vertical => base.top(px(thumb.top)).left(theme.ui(2.)).w(thickness).h(px(thumb.len)),
        Bar::Horizontal => base.left(px(thumb.top)).top(theme.ui(2.)).h(thickness).w(px(thumb.len)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(shared_corner: bool) -> Extent {
        Extent {
            viewport: (200.0, 100.0),
            contents: (400.0, 300.0),
            offset: (0.0, 0.0),
            bounds: Bounds::default(),
            shared_corner,
        }
    }

    #[test]
    fn a_shared_corner_shortens_both_tracks() {
        let (v, h) = extent(true).thumbs(1.0);
        let (v_apart, h_apart) = extent(false).thumbs(1.0);
        let (v, h, v_apart, h_apart) = (v.unwrap(), h.unwrap(), v_apart.unwrap(), h_apart.unwrap());
        assert!(v.len < v_apart.len);
        assert!(h.len < h_apart.len);
    }

    #[test]
    fn separate_tracks_use_the_whole_viewport() {
        let extent = extent(false);
        let (v, _) = extent.thumbs(1.0);
        let v = v.unwrap();
        // The thumb at the end of the track maps back to the largest offset.
        let end = extent.offset_for(Bar::Vertical, 100.0 - v.len, v.len, 1.0);
        assert!((end - 200.0).abs() < 0.01, "{end}");
    }
}
