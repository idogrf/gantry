//! Dragging a pane to rearrange the layout: what is carried while the button is down.

use app_core::split_tree::{Direction, DropZone, NodeId, Rect, zone_rect};
use app_core::workspace::{DropCheck, MAX_PANES_PER_TAB};
use gpui::{Context, FontWeight, IntoElement, Render, Window, div, prelude::*, px, rgb};

use crate::preview_view::bidi_label;
use crate::settings_store::SettingsStore;
use crate::theme::{RADIUS, UI_FONT, theme};

/// Where a dragged pane would land: the part of the target pane (`pane`, in fractions of the tab) that `zone` covers,
/// and whether the drop is refused. `None` when there is nothing to drop on.
pub fn preview_for(pane: Rect, zone: DropZone, check: DropCheck) -> Option<(Rect, bool)> {
    match check {
        DropCheck::Nothing => None,
        DropCheck::Allowed => Some((zone_rect(pane, zone), false)),
        DropCheck::Full => Some((zone_rect(pane, zone), true)),
    }
}

/// The words on the preview. `same_tab`: the dragged pane already belongs to the visible tab; `target` is the title
/// of the pane under the pointer.
pub fn preview_label(zone: DropZone, check: DropCheck, same_tab: bool, target: &str) -> String {
    match (check, zone) {
        (DropCheck::Full, _) => format!("Tab is full ({MAX_PANES_PER_TAB} panes max)"),
        (_, DropZone::Center) if same_tab => "Swap".into(),
        (_, DropZone::Center) => format!("Swap: {target} becomes its own tab"),
        (_, DropZone::Edge(Direction::Left)) => "Split left".into(),
        (_, DropZone::Edge(Direction::Right)) => "Split right".into(),
        (_, DropZone::Edge(Direction::Up)) => "Split above".into(),
        (_, DropZone::Edge(Direction::Down)) => "Split below".into(),
    }
}

/// Where the pane being dragged would land: the pane under the pointer, the zone of it, what the drop would do and
/// the words for the preview.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneDrop {
    /// The pane under the pointer.
    pub target: NodeId,
    /// The part of it the pointer is in.
    pub zone: DropZone,
    /// What the drop would do.
    pub check: DropCheck,
    /// The words on the preview.
    pub label: String,
}

/// A pane being dragged by its header or its sidebar row.
#[derive(Clone)]
pub struct DraggedPane {
    /// The pane's leaf.
    pub leaf: NodeId,
    /// Its title, shown next to the pointer.
    pub title: String,
}

impl Render for DraggedPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let rtl = cx.global::<SettingsStore>().settings().window.right_to_left;
        div()
            .px(theme.ui(12.))
            .py(theme.ui(6.))
            .rounded(px(RADIUS))
            .border_1()
            .border_color(rgb(theme.tokens.accent))
            .bg(rgb(theme.tokens.raised))
            .text_color(rgb(theme.tokens.accent))
            .text_sm()
            .font_family(UI_FONT)
            .font_weight(FontWeight::SEMIBOLD)
            .child(bidi_label(&self.title, rtl))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect { x: 0.5, y: 0.0, w: 0.5, h: 1.0 };

    #[test]
    fn the_preview_covers_the_half_where_the_pane_lands() {
        let right = DropZone::Edge(Direction::Right);
        assert_eq!(
            preview_for(AREA, right, DropCheck::Allowed),
            Some((Rect { x: 0.75, y: 0.0, w: 0.25, h: 1.0 }, false))
        );
        assert_eq!(preview_for(AREA, DropZone::Center, DropCheck::Allowed), Some((AREA, false)));
    }

    #[test]
    fn a_full_tab_previews_a_refusal() {
        let up = DropZone::Edge(Direction::Up);
        assert_eq!(preview_for(AREA, up, DropCheck::Full), Some((Rect { h: 0.5, ..AREA }, true)));
    }

    #[test]
    fn nothing_to_drop_has_no_preview() {
        assert_eq!(preview_for(AREA, DropZone::Center, DropCheck::Nothing), None);
    }

    #[test]
    fn labels_say_what_the_drop_does() {
        let label = |zone, check, same| preview_label(zone, check, same, "docs");
        assert_eq!(label(DropZone::Center, DropCheck::Allowed, true), "Swap");
        assert_eq!(label(DropZone::Center, DropCheck::Allowed, false), "Swap: docs becomes its own tab");
        assert_eq!(label(DropZone::Edge(Direction::Left), DropCheck::Allowed, false), "Split left");
        assert_eq!(label(DropZone::Edge(Direction::Down), DropCheck::Allowed, true), "Split below");
        assert_eq!(label(DropZone::Edge(Direction::Up), DropCheck::Allowed, true), "Split above");
        assert_eq!(label(DropZone::Edge(Direction::Right), DropCheck::Full, false), "Tab is full (4 panes max)");
    }

    #[test]
    fn a_dragged_pane_remembers_its_leaf_and_title() {
        let dragged = DraggedPane { leaf: 7, title: "api".into() };
        let copy = dragged.clone();
        assert_eq!((copy.leaf, copy.title.as_str()), (7, "api"));
    }
}
