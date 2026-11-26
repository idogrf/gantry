//! A right-click menu: entries in groups, a hint for the entry's shortcut, the row's main action in the accent color.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use app_core::host_state::HostDot;
use app_core::menus::{MenuAction, MenuEntry, MenuItem, Shortcut};
use app_core::session_tree::Tone;
use app_core::settings::Settings;
use gpui::{
    App, Bounds, FocusHandle, FontWeight, IntoElement, Pixels, Point, Window, anchored, canvas, deferred, div,
    prelude::*, px, relative, rgb, rgba,
};

use crate::settings_store::shortcut_label;
use crate::theme::{FadeKind, RADIUS, RADIUS_SMALL, Theme, UI_FONT, fade_in};

/// How many menus are open.
static OPEN: AtomicUsize = AtomicUsize::new(0);

/// Whether any menu is open.
pub(crate) fn any_open() -> bool {
    OPEN.load(Ordering::Relaxed) > 0
}

/// An open menu: its items, where it opened, and the focus handle that takes `Esc`.
pub(crate) struct OpenMenu {
    /// Its items.
    pub items: Vec<MenuItem>,
    /// Top-left corner, in window coordinates.
    pub position: Point<Pixels>,
    /// Focused while open, so `Esc` closes it.
    pub focus: FocusHandle,
    /// The index of the submenu that is open, if any.
    pub sub: Rc<Cell<Option<usize>>>,
    /// Where the open submenu was drawn: it hangs outside the menu's own bounds, so a press inside it must not
    /// count as a click outside the menu.
    sub_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl OpenMenu {
    /// A closed-submenu menu at `position`.
    pub fn new(items: Vec<MenuItem>, position: Point<Pixels>, focus: FocusHandle) -> OpenMenu {
        OPEN.fetch_add(1, Ordering::Relaxed);
        OpenMenu { items, position, focus, sub: Rc::new(Cell::new(None)), sub_bounds: Rc::new(Cell::new(None)) }
    }
}

impl Drop for OpenMenu {
    fn drop(&mut self) {
        OPEN.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The color of a host's state dot: the status tones of the sidebar, the danger and muted tokens.
pub(crate) fn host_dot_color(theme: &Theme, dot: HostDot) -> u32 {
    match dot {
        HostDot::Ok => theme.tone(Tone::Done).dot,
        HostDot::Busy => theme.tone(Tone::Working).dot,
        HostDot::Warn => theme.tone(Tone::NeedsInput).dot,
        HostDot::Bad => theme.tokens.danger,
        HostDot::None => theme.tokens.muted,
    }
}

/// The hint shown for `shortcut`, following the user's key bindings.
fn shortcut_text(shortcut: Shortcut, settings: &Settings) -> String {
    let action = match shortcut {
        Shortcut::SplitRight => "splitRightClaude",
        Shortcut::SplitDown => "splitDownClaude",
        Shortcut::SplitRightShell => "splitRightShell",
        Shortcut::SplitDownShell => "splitDownShell",
        Shortcut::Close => "closePane",
        Shortcut::Recents => "openRecents",
    };
    shortcut_label(settings, action)
}

/// Draw `menu` floating above everything; a pick or a click outside calls back, `Esc` dismisses. A submenu opens beside
/// its row on hover or `Right`, and closes on `Left` or `Esc`.
pub(crate) fn render(
    id: &'static str,
    menu: &OpenMenu,
    theme: &Theme,
    settings: &Settings,
    on_pick: impl Fn(&MenuAction, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let on_pick = Rc::new(on_pick);
    let on_dismiss = Rc::new(on_dismiss);
    let dismiss_key = on_dismiss.clone();
    let open_sub = menu.sub.get();
    let entry_row = |row_id: (&'static str, usize), entry: &MenuEntry, closes_sub: Option<Rc<Cell<Option<usize>>>>| {
        let action = entry.action.clone();
        let on_pick = on_pick.clone();
        div()
            .id(row_id)
            .px_3()
            .py_1()
            .flex()
            .items_center()
            .gap_4()
            .rounded(px(RADIUS_SMALL))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme.tokens.line)))
            .when(entry.accent, |item| item.text_color(rgb(theme.tokens.accent)).font_weight(FontWeight::SEMIBOLD))
            .child(div().flex_1().whitespace_nowrap().child(entry.label.clone()))
            .when(entry.checked, |item| item.child(div().flex_none().text_color(rgb(theme.tokens.accent)).child("✓")))
            .when_some(entry.detail.clone(), |item, detail| {
                item.child(div().text_xs().text_color(rgb(theme.tokens.muted)).child(detail))
            })
            .when_some(entry.shortcut, |item, shortcut| {
                item.child(div().text_xs().text_color(rgb(theme.tokens.muted)).child(shortcut_text(shortcut, settings)))
            })
            .when_some(closes_sub, |item, sub| {
                item.on_hover(move |hovered, window, _| {
                    if *hovered && sub.get().is_some() {
                        sub.set(None);
                        window.refresh();
                    }
                })
            })
            .on_click(move |_, window, cx| on_pick(&action, window, cx))
            .into_any_element()
    };
    let rows = menu.items.iter().enumerate().map(|(index, item)| match item {
        MenuItem::Separator => div().h(px(1.)).mx_2().my_1().bg(rgb(theme.tokens.line)).into_any_element(),
        MenuItem::Heading(text) => {
            div().px_3().pt_1().text_xs().text_color(rgb(theme.tokens.muted)).child(text.clone()).into_any_element()
        }
        MenuItem::Entry(entry) => entry_row(("menu-item", index), entry, Some(menu.sub.clone())),
        MenuItem::Submenu { label, items } => {
            let (hover, click) = (menu.sub.clone(), menu.sub.clone());
            let open = open_sub == Some(index);
            let bounds = menu.sub_bounds.clone();
            let children = open.then(|| {
                // Color swatches are drawn together as a 4-column grid, the other entries below it.
                let swatches: Vec<_> = items
                    .iter()
                    .enumerate()
                    .filter_map(|(position, item)| match item {
                        MenuItem::Entry(MenuEntry { swatch: Some(color), checked, action, .. }) => {
                            let (action, on_pick) = (action.clone(), on_pick.clone());
                            Some(
                                div()
                                    .id(("menu-swatch", index * 100 + position))
                                    .flex_none()
                                    .size(theme.ui(36.))
                                    .rounded(px(RADIUS_SMALL))
                                    .bg(rgb(color.value(theme.preset)))
                                    .border_2()
                                    .border_color(if *checked { rgb(theme.tokens.text) } else { rgba(0) })
                                    .cursor_pointer()
                                    .on_click(move |_, window, cx| on_pick(&action, window, cx)),
                            )
                        }
                        _ => None,
                    })
                    .collect();
                let mut swatches = swatches.into_iter();
                let mut grid = div().flex().flex_col().gap(theme.ui(6.)).p(theme.ui(6.));
                loop {
                    let row: Vec<_> = swatches.by_ref().take(4).collect();
                    if row.is_empty() {
                        break;
                    }
                    grid = grid.child(div().flex().flex_row().gap(theme.ui(6.)).children(row));
                }
                let has_swatches =
                    items.iter().any(|item| matches!(item, MenuItem::Entry(MenuEntry { swatch: Some(_), .. })));
                let entries = items.iter().enumerate().filter_map(|(position, item)| match item {
                    MenuItem::Entry(entry) if entry.swatch.is_none() => {
                        Some(entry_row(("menu-sub-item", index * 100 + position), entry, None))
                    }
                    _ => None,
                });
                div()
                    .absolute()
                    .left(relative(1.))
                    .top(px(-4.))
                    .min_w(theme.ui(200.))
                    .p_1()
                    .flex()
                    .flex_col()
                    .bg(rgb(theme.tokens.raised))
                    .border_1()
                    .border_color(rgb(theme.tokens.line))
                    .rounded(px(RADIUS))
                    .shadow_lg()
                    .when(has_swatches, |sub| sub.child(grid))
                    .children(entries)
                    .child(canvas(move |area, _, _| bounds.set(Some(area)), |_, _, _, _| {}).absolute().size_full())
            });
            div()
                .id(("menu-item", index))
                .relative()
                .px_3()
                .py_1()
                .flex()
                .items_center()
                .gap_4()
                .rounded(px(RADIUS_SMALL))
                .cursor_pointer()
                .when(open, |item| item.bg(rgb(theme.tokens.line)))
                .hover(|style| style.bg(rgb(theme.tokens.line)))
                .on_hover(move |hovered, window, _| {
                    if *hovered && hover.get() != Some(index) {
                        hover.set(Some(index));
                        window.refresh();
                    }
                })
                .on_click(move |_, window, _| {
                    click.set(Some(index));
                    window.refresh();
                })
                .child(div().flex_1().whitespace_nowrap().child(label.clone()))
                .child(div().text_color(rgb(theme.tokens.muted)).child("▸"))
                .children(children)
                .into_any_element()
        }
    });
    let sub = menu.sub.clone();
    let (sub_open, sub_bounds) = (menu.sub.clone(), menu.sub_bounds.clone());
    let first_submenu = menu.items.iter().position(|item| matches!(item, MenuItem::Submenu { .. }));
    let card = div()
        .id(id)
        // A floating layer: clicks must not reach what is beneath it.
        .occlude()
        .track_focus(&menu.focus)
        .on_key_down(move |event, window, cx| {
            match event.keystroke.key.as_str() {
                "escape" | "left" if sub.get().is_some() => {
                    sub.set(None);
                    window.refresh();
                }
                "escape" => dismiss_key(window, cx),
                "right" if sub.get().is_none() && first_submenu.is_some() => {
                    sub.set(first_submenu);
                    window.refresh();
                }
                _ => {}
            }
            // Keys must not reach what is beneath the menu (the sidebar filter).
            cx.stop_propagation();
        })
        .on_mouse_down_out(move |_, window, cx| {
            let in_sub = sub_open.get().is_some()
                && sub_bounds.get().is_some_and(|bounds| bounds.contains(&window.mouse_position()));
            if !in_sub {
                on_dismiss(window, cx);
            }
        })
        .min_w(theme.ui(220.))
        .p_1()
        .flex()
        .flex_col()
        .text_sm()
        .font_family(UI_FONT)
        .text_color(rgb(theme.tokens.text))
        .bg(rgb(theme.tokens.raised))
        .border_1()
        .border_color(rgb(theme.tokens.line))
        .rounded(px(RADIUS))
        .shadow_lg()
        .children(rows);
    deferred(anchored().position(menu.position).snap_to_window().child(fade_in(
        card,
        "fade-context-menu",
        FadeKind::Card,
        settings.animate_ui,
    )))
    .with_priority(1)
}
