use super::{PANE_GAP, PaneAnim, SIDEBAR_EDGE, WorkspaceView};
use std::time::{Duration, Instant};

use app_core::split_tree::{self, Axis, NodeId, PANE_ANIM_MS, Rect, drop_zone};
use gpui::{
    App, Context, Div, DragMoveEvent, FontWeight, MouseButton, MouseMoveEvent, Pixels, Size, Window, div, prelude::*,
    px, relative, rgb, rgba,
};

use crate::pane_drag::{DraggedPane, PaneDrop, preview_for, preview_label};
use crate::settings_store::SettingsStore;
use crate::theme::{RADIUS_LARGE, Theme};

/// A pane card's padding, between its border and its content.
pub(super) const CARD_PAD: f32 = 6.0;
/// A pane card's border width (`border_2`).
const CARD_BORDER: f32 = 2.0;

impl WorkspaceView {
    /// The active tab's pane rectangles as drawn right now: on their way while a split or close animates.
    pub(super) fn drawn_rects(&self) -> Vec<(NodeId, Rect)> {
        let Some(tab) = self.workspace.active_tab().filter(|tab| !tab.settings) else { return Vec::new() };
        let layout = tab.root.layout(self.final_region());
        match &self.pane_anim {
            Some(anim) => split_tree::tween(&anim.from, &layout, pane_progress(anim.started.elapsed())),
            None if self.panel_anim.is_some() => tab.root.layout(self.drawn_region()),
            None => layout,
        }
    }

    /// Animate the active tab from `from` to its layout, when animations are on and the window is active.
    pub(super) fn start_pane_anim(
        &mut self,
        from: Vec<(NodeId, Rect)>,
        entering: Option<(NodeId, Axis)>,
        window: &Window,
        cx: &App,
    ) {
        self.panel_anim = None;
        self.pane_anim = (cx.global::<SettingsStore>().settings().animate_ui && window.is_window_active())
            .then(|| PaneAnim { from, started: Instant::now(), entering });
    }

    /// End the pane animation: the panes jump to their layout.
    pub(super) fn cancel_pane_anim(&mut self) {
        self.pane_anim = None;
        self.panel_anim = None;
    }

    /// How much of the tab content's width the panes use when `panel_px` is taken by the git panel.
    fn panes_fraction(&self, panel_px: f32) -> f32 {
        let width = f32::from(self.content_bounds.size.width);
        if width <= 0.0 { 1.0 } else { (1.0 - panel_px / width).clamp(0.2, 1.0) }
    }

    /// The git panel's width as drawn right now.
    pub(super) fn panel_px(&self) -> f32 {
        match &self.panel_anim {
            Some(anim) => app_core::git_view::panel_width(
                self.panel_full,
                anim.started.elapsed().as_secs_f32() * 1000.0,
                anim.opening,
            ),
            None if self.git_panel_open => self.panel_full,
            None => 0.0,
        }
    }

    /// Where the panes go once the panel has finished opening or closing.
    pub(super) fn final_region(&self) -> Rect {
        let panel = if self.git_panel_open { self.panel_full } else { 0.0 };
        Rect { w: self.panes_fraction(panel), ..Rect::UNIT }
    }

    /// Where the panes are right now (the panel may be sliding).
    pub(super) fn drawn_region(&self) -> Rect {
        Rect { w: self.panes_fraction(self.panel_px()), ..Rect::UNIT }
    }

    /// The pointer moved while a pane is dragged over `leaf`'s overlay: work out where the drop would land.
    pub(super) fn drag_over_pane(&mut self, leaf: NodeId, event: &DragMoveEvent<DraggedPane>, cx: &mut Context<Self>) {
        let (bounds, position) = (event.bounds, event.event.position);
        let src = event.drag(cx).leaf;
        let next = bounds.contains(&position).then(|| {
            let area = Rect {
                x: f32::from(bounds.origin.x),
                y: f32::from(bounds.origin.y),
                w: f32::from(bounds.size.width),
                h: f32::from(bounds.size.height),
            };
            let zone = drop_zone(area, f32::from(position.x), f32::from(position.y));
            let check = self.workspace.drop_check(src, leaf, zone);
            let same_tab = self.workspace.tab_of(src) == Some(self.workspace.active);
            let title = self
                .workspace
                .leaf_name(leaf)
                .map(String::from)
                .or_else(|| self.slots.get(&leaf).map(|slot| slot.view.read(cx).title()))
                .unwrap_or_default();
            PaneDrop { target: leaf, zone, check, label: preview_label(zone, check, same_tab, &title) }
        });
        match next {
            Some(drop) if self.pane_drop.as_ref() != Some(&drop) => {
                self.pane_drop = Some(drop);
                cx.notify();
            }
            None if self.pane_drop.as_ref().is_some_and(|drop| drop.target == leaf) => {
                self.pane_drop = None;
                cx.notify();
            }
            _ => {}
        }
    }

    /// The button was released over `target`'s overlay with pane `src`: do what the preview showed.
    pub(super) fn drop_pane(&mut self, src: NodeId, target: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drop) = self.pane_drop.take().filter(|drop| drop.target == target) else { return };
        if self.workspace.move_leaf(src, drop.target, drop.zone) {
            self.focus_current(window, cx);
            self.save(cx);
        }
        cx.notify();
    }

    /// The translucent landing area over the visible tab while a pane is dragged over one of its panes: the accent when
    /// the drop works, the danger color when it is refused.
    pub(super) fn pane_drop_preview(&self, tab: &app_core::workspace::Tab, theme: &Theme, cx: &App) -> Option<Div> {
        if !cx.has_active_drag() {
            return None;
        }
        let drop = self.pane_drop.as_ref()?;
        let (_, pane) = tab.root.layout(self.drawn_region()).into_iter().find(|(leaf, _)| *leaf == drop.target)?;
        let (rect, refused) = preview_for(pane, drop.zone, drop.check)?;
        let color = if refused { theme.tokens.danger } else { theme.tokens.accent };
        // The same padded box as a pane's wrapper, so the landing area lines up with the pane's card.
        Some(
            div()
                .absolute()
                .left(relative(rect.x))
                .top(relative(rect.y))
                .w(relative(rect.w))
                .h(relative(rect.h))
                .p(px(PANE_GAP / 2.))
                .child(
                    div()
                        .size_full()
                        .rounded(px(RADIUS_LARGE))
                        .border_2()
                        .border_dashed()
                        .border_color(rgb(color))
                        .bg(rgba((color << 8) | 0x2e))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(color))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(drop.label.clone()),
                ),
        )
    }

    pub(super) fn drag_divider(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(divider) = self.dragging else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            self.dragging = None;
            self.save(cx);
            return;
        }
        self.cancel_pane_anim();
        let bounds = self.content_bounds;
        let ratio = match divider.axis {
            Axis::Horizontal => {
                let x = f32::from(event.position.x - bounds.origin.x) / f32::from(bounds.size.width);
                (x - divider.area.x) / divider.area.w
            }
            Axis::Vertical => {
                let y = f32::from(event.position.y - bounds.origin.y) / f32::from(bounds.size.height);
                (y - divider.area.y) / divider.area.h
            }
        };
        if ratio.is_finite() {
            self.workspace.set_ratio(divider.split, ratio);
            cx.notify();
        }
    }
}

/// The start rectangles of a split of `target` along `axis`: every pane where it is drawn now, plus `new_leaf` as a
/// sliver on the far side of the target's drawn rectangle.
pub(super) fn start_rects(
    drawn: &[(NodeId, Rect)],
    target: NodeId,
    new_leaf: NodeId,
    axis: Axis,
) -> Vec<(NodeId, Rect)> {
    let mut from = drawn.to_vec();
    if let Some(&(_, rect)) = drawn.iter().find(|(leaf, _)| *leaf == target) {
        from.push((new_leaf, split_tree::entry_rect(rect, axis)));
    }
    from
}

/// Where a surviving pane's content sits inside its card while a split or close animates, in px from the card's
/// padding box: `[left, top, width, height]`, top-left and at its final size (so the terminal resizes once, the card
/// clips it). `width`/`height` are the tab content's size.
pub(super) fn content_frame(end: Rect, width: f32, height: f32) -> [f32; 4] {
    let inset = PANE_GAP + 2.0 * (CARD_BORDER + CARD_PAD);
    [CARD_PAD, CARD_PAD, end.w * width - inset, end.h * height - inset]
}

/// The tab content's width when the sidebar is `sidebar` wide in a body `body` wide: what is left after the sidebar,
/// its hairline and the panes' own gap.
pub(super) fn content_width_beside(body: Pixels, sidebar: Pixels) -> Pixels {
    (body - sidebar - px(SIDEBAR_EDGE) - px(PANE_GAP)).max(px(0.))
}

/// The window changed size since `previous` (a zero `previous` is the first frame, not a resize).
pub(super) fn window_resized(previous: Size<Pixels>, now: Size<Pixels>) -> bool {
    previous != Size::default() && previous != now
}

/// How far a pane animation is after `elapsed`: 0 at the start, 1 (or more) when it is done.
pub(super) fn pane_progress(elapsed: Duration) -> f32 {
    elapsed.as_secs_f32() * 1000.0 / PANE_ANIM_MS as f32
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn second_split_starts_from_drawn_rects() {
        let (left, right, new) = (1, 2, 3);
        let old = [(left, Rect::UNIT)];
        let layout = [(left, Rect { w: 0.5, ..Rect::UNIT }), (right, Rect { x: 0.5, w: 0.5, ..Rect::UNIT })];
        let drawn = split_tree::tween(&old, &layout, 0.5);
        let from = start_rects(&drawn, right, new, Axis::Vertical);
        assert_eq!(&from[..2], &drawn[..]);
        assert_ne!(from[0].1, layout[0].1, "mid-animation, not the old or new layout");
        assert_eq!(from[2], (new, split_tree::entry_rect(drawn[1].1, Axis::Vertical)));
    }

    #[test]
    fn a_surviving_pane_keeps_its_content_top_left_at_its_final_size() {
        let end = Rect { w: 0.5, ..Rect::UNIT };
        let [left, top, w, h] = content_frame(end, 1000.0, 600.0);
        assert_eq!((left, top), (CARD_PAD, CARD_PAD));
        // What `size_full` gives inside the card at rest: the final box minus the gap, border and padding.
        assert_eq!(w, 500.0 - PANE_GAP - 2.0 * (CARD_BORDER + CARD_PAD));
        assert_eq!(h, 600.0 - PANE_GAP - 2.0 * (CARD_BORDER + CARD_PAD));
    }

    #[test]
    fn start_rects_without_the_target_adds_nothing() {
        let drawn = [(1, Rect::UNIT)];
        assert_eq!(start_rects(&drawn, 7, 8, Axis::Horizontal), drawn.to_vec());
    }

    #[test]
    fn content_width_beside_takes_the_sidebar_its_hairline_and_the_panes_gap() {
        use gpui::px;
        let beside = content_width_beside(px(1000.), px(280.));
        assert_eq!(beside, px(1000. - 280. - super::super::SIDEBAR_EDGE - super::super::PANE_GAP));
        assert_eq!(content_width_beside(px(100.), px(280.)), px(0.));
    }

    #[test]
    fn only_a_changed_known_window_size_counts_as_a_resize() {
        use gpui::size;
        let (a, b) = (size(gpui::px(800.), gpui::px(600.)), size(gpui::px(900.), gpui::px(600.)));
        assert!(window_resized(a, b));
        assert!(!window_resized(a, a));
        assert!(!window_resized(Size::default(), a), "the first frame is not a resize");
    }

    #[test]
    fn pane_progress_runs_from_zero_to_one_over_the_animation() {
        assert_eq!(pane_progress(Duration::ZERO), 0.0);
        assert!((pane_progress(Duration::from_millis(split_tree::PANE_ANIM_MS / 2)) - 0.5).abs() < 1e-6);
        assert!(pane_progress(Duration::from_millis(split_tree::PANE_ANIM_MS)) >= 1.0);
    }
}
