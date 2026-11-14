//! The Settings tab's page: a left menu of sections (Appearance, Terminal, Alerts, Shortcuts, Sessions, Window) and the section's controls.
//!
//! Every control is focusable (Tab / Shift+Tab, Space or Enter to press); changes go through
//! [`SettingsStore::update`], which saves and applies them at once.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use app_core::session_tree::{Tone, display_path};
use app_core::settings::{Problem, Settings, remove_ssh_host};
use app_core::snapshot::Rgb;
use app_core::text_field::TextField;
use gpui::{
    App, Bounds, Context, DispatchPhase, Div, ElementId, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Render, ScrollHandle,
    SharedString, Stateful, Subscription, Window, canvas, deferred, div, point, prelude::*, px, relative, rgb, rgba,
    svg,
};

use crate::assets::INFO_ICON;
use crate::field;
use crate::settings_store::SettingsStore;
use crate::theme::{RADIUS, RADIUS_LARGE, RADIUS_SMALL, Theme, theme};
use crate::update_watch::{DaemonBuild, footer_lines};

mod alerts;
mod appearance;
pub(crate) mod sessions;
mod shortcuts;
pub(crate) mod ssh_hosts;
mod terminal;
mod window;

use alerts::Event;
use appearance::Status;
use terminal::installed_monospace;

/// A group of settings, chosen in the left menu.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    /// Theme, status colors and fonts.
    Appearance,
    /// Terminal font, scrollback, text direction and shell.
    Terminal,
    /// What raises an alert, and how.
    Alerts,
    /// Rebindable keyboard shortcuts.
    Shortcuts,
    /// Claude config dirs, recent sessions and favorites.
    Sessions,
    /// SSH hosts: state, rename, edit, remove, add.
    SshHosts,
    /// Start-up behaviour.
    Window,
}

impl Section {
    const ALL: [Section; 7] = [
        Section::Appearance,
        Section::Terminal,
        Section::Alerts,
        Section::Shortcuts,
        Section::Sessions,
        Section::SshHosts,
        Section::Window,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Terminal => "Terminal",
            Section::Alerts => "Alerts",
            Section::Shortcuts => "Shortcuts",
            Section::Sessions => "Sessions",
            Section::SshHosts => "SSH hosts",
            Section::Window => "Window",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Section::Appearance => "Changes apply right away.",
            Section::Terminal => "Font, scrollback and shell for terminal panes.",
            Section::Alerts => "For sessions running in Gantry's panes.",
            Section::Shortcuts => "Click a shortcut, then press the new keys. Esc cancels, Backspace unbinds.",
            Section::Sessions => "How new Claude and terminal panes start.",
            Section::SshHosts => "Remote machines that run Gantry over ssh.",
            Section::Window => "How Gantry starts.",
        }
    }
}

/// A drop-down or pop-over that is open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Popover {
    Fill(Status),
    DirColor(usize),
    Font,
    Tone(Event),
}

/// A number setting edited with − / + buttons or the arrow keys.
struct Stepped {
    name: &'static str,
    get: fn(&Settings) -> f32,
    set: fn(&mut Settings, f32),
    delta: f32,
    min: f32,
    max: f32,
    /// Shown after the value ("lines", "s"); empty for a bare number.
    unit: &'static str,
}

/// A drop-down list shows this many rows (each this tall, before scaling) before it scrolls.
const DROPDOWN_ROWS: f32 = 8.;
const DROPDOWN_ROW: f32 = 30.;

/// `value` moved by `delta`, kept within `min..=max` and rounded to one decimal.
fn step(value: f32, delta: f32, min: f32, max: f32) -> f32 {
    ((value + delta).clamp(min, max) * 10.0).round() / 10.0
}

/// A number with its unit after it: "10,000 lines", "3 s"; the bare number when `unit` is empty.
fn with_unit(value: f32, unit: &str) -> String {
    if unit.is_empty() { format_number(value) } else { format!("{} {unit}", format_number(value)) }
}

/// Whether the setting `get` reads differs from its value in `Settings::default()`.
fn differs<T: PartialEq>(settings: &Settings, get: impl Fn(&Settings) -> T) -> bool {
    get(settings) != get(&Settings::default())
}

/// A number as the steppers show it: no decimals for whole numbers, thousands separated (10,000).
fn format_number(value: f32) -> String {
    if value.fract() != 0.0 {
        return format!("{value:.1}");
    }
    let digits = format!("{value:.0}");
    let (sign, digits) = digits.strip_prefix('-').map_or(("", digits.as_str()), |rest| ("-", rest));
    let mut grouped = String::new();
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{sign}{grouped}")
}

/// A problem as the banner lists it: `key: message`, or just the message for the whole file.
fn problem_line(problem: &Problem) -> String {
    if problem.key.is_empty() { problem.message.clone() } else { format!("{}: {}", problem.key, problem.message) }
}

fn color(value: Rgb) -> u32 {
    (u32::from(value.r) << 16) | (u32::from(value.g) << 8) | u32::from(value.b)
}

/// Create `path` as `{}` if it is missing, then open it with the system's editor.
fn open_settings_file(path: &Path, cx: &App) {
    if !path.exists() {
        if let Some(dir) = path.parent()
            && let Err(error) = std::fs::create_dir_all(dir)
        {
            tracing::warn!("settings directory not created: {error}");
        }
        if let Err(error) = std::fs::write(path, "{}\n") {
            tracing::warn!("settings file not created: {error}");
            return;
        }
    }
    cx.open_with_system(path);
}

type DropdownBounds = Rc<RefCell<HashMap<(&'static str, usize), Bounds<Pixels>>>>;

/// Where the guides in `docs/` are read.
const GUIDES_URL: &str = "https://github.com/idogrf/gantry/blob/main/docs/";

type Handler = Rc<dyn Fn(&mut SettingsView, &mut Window, &mut Context<SettingsView>)>;

fn handler(f: impl Fn(&mut SettingsView, &mut Window, &mut Context<SettingsView>) + 'static) -> Handler {
    Rc::new(f)
}

/// A handler that changes the settings.
fn change(f: impl Fn(&mut Settings) + 'static) -> Handler {
    handler(move |_, _, cx| SettingsStore::update(cx, |settings| f(settings)))
}

/// What the Settings page asks the workspace to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SettingsEvent {
    /// Open the setup wizard to look for Claude config dirs.
    DetectClaudeDirs,
    /// Open the Add SSH host wizard.
    AddSshHost,
    /// A host was renamed in `sshHosts`; `reconnect`: it was connected, so it should connect again under the new name.
    HostRenamed {
        /// The old name.
        from: String,
        /// The new name.
        to: String,
        /// Connect it again afterwards.
        reconnect: bool,
    },
    /// A host was removed from `sshHosts`: drop its panes and state.
    HostRemoved(String),
    /// Disconnect and connect the host again (its target was edited while connected).
    ReconnectHost(String),
    /// Ask GitHub for a newer release now.
    CheckNow,
    /// Open the custom color picker for a status fill.
    CustomColor(app_core::session_tree::Tone),
}

impl EventEmitter<SettingsEvent> for SettingsView {}

/// The Settings page, shown in the pane area while the Settings tab is active.
pub(crate) struct SettingsView {
    section: Section,
    focus_handle: FocusHandle,
    /// One handle per control, by name and index, created on first draw.
    stops: HashMap<(&'static str, usize), FocusHandle>,
    /// Installed monospace families (without the platform default), found when the page opened.
    families: Vec<String>,
    popover: Option<Popover>,
    /// The volume being dragged, written when the button is released.
    volume_drag: Option<f32>,
    slider_bounds: Rc<Cell<Bounds<Pixels>>>,
    body_scroll: ScrollHandle,
    /// Where each drop-down's button was last drawn, to open its list upward near the window's bottom.
    dropdown_bounds: DropdownBounds,
    /// The Shortcuts page: the action whose key is being recorded.
    recording: Option<&'static str>,
    /// Why the last recorded key was refused, under that action's row.
    shortcut_message: Option<(&'static str, String)>,
    /// A key another action has, waiting for "Replace".
    pending_replace: Option<shortcuts::Replace>,
    /// A key programs in the terminal use, waiting for "Use anyway".
    pending_take: Option<shortcuts::Replace>,
    /// The Shortcuts page's filter field.
    shortcut_filter: TextField,
    filter_focus: FocusHandle,
    _keystrokes: Subscription,
    /// The daemon's own Claude config dir, shown as the implicit Default.
    daemon_dir: Option<PathBuf>,
    /// The Sessions page's text field being edited, and its text.
    editing: Option<sessions::FieldKey>,
    edit: TextField,
    /// Each text field's focus subscriptions, by field.
    field_subs: HashMap<sessions::FieldKey, Vec<Subscription>>,
    /// Why the last edit on the Sessions page was refused.
    sessions_message: Option<String>,
    /// The SSH hosts page: the host waiting for a remove confirmation.
    confirm_remove: Option<String>,
    /// Hosts whose target was edited while connected (they offer "Reconnect to apply").
    edited_targets: std::collections::HashSet<String>,
    /// Why the last edit on the SSH hosts page was refused.
    ssh_message: Option<String>,
    _host_statuses: Subscription,
    _check_row: Subscription,
}

impl SettingsView {
    /// A page on the Appearance section.
    pub fn new(daemon_dir: Option<PathBuf>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Recording a shortcut must see keys before the key bindings do, or the chord would run its action.
        let view = cx.weak_entity();
        let keystrokes = cx.intercept_keystrokes(move |event, window, cx| {
            let consumed = view.update(cx, |this, cx| this.record(&event.keystroke, window, cx));
            if consumed.unwrap_or(false) {
                cx.stop_propagation();
            }
        });
        SettingsView {
            section: Section::Appearance,
            focus_handle: cx.focus_handle(),
            stops: HashMap::new(),
            families: installed_monospace(cx),
            popover: None,
            volume_drag: None,
            slider_bounds: Rc::new(Cell::new(Bounds::default())),
            body_scroll: ScrollHandle::new(),
            dropdown_bounds: Rc::new(RefCell::new(HashMap::new())),
            recording: None,
            shortcut_message: None,
            pending_replace: None,
            pending_take: None,
            shortcut_filter: TextField::default(),
            filter_focus: cx.focus_handle(),
            _keystrokes: keystrokes,
            daemon_dir,
            editing: None,
            edit: TextField::default(),
            field_subs: HashMap::new(),
            sessions_message: None,
            confirm_remove: None,
            edited_targets: Default::default(),
            ssh_message: None,
            _host_statuses: cx.observe_global::<ssh_hosts::HostStatuses>(|_, cx| cx.notify()),
            _check_row: cx.observe_global::<crate::release_check::CheckRow>(|_, cx| cx.notify()),
        }
    }

    fn stop(&mut self, name: &'static str, index: usize, cx: &mut Context<Self>) -> FocusHandle {
        self.stops.entry((name, index)).or_insert_with(|| cx.focus_handle()).clone()
    }

    fn close_popover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.popover.take().is_some() {
            self.focus_handle.focus(window, cx);
            cx.notify();
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "escape" if self.popover.is_some() => self.close_popover(window, cx),
            "tab" if !keystroke.modifiers.control && !keystroke.modifiers.alt => {
                if keystroke.modifiers.shift {
                    window.focus_prev(cx);
                } else {
                    window.focus_next(cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// A focusable, clickable box: Space or Enter presses it like a click. The ring is `border` (none: invisible)
    /// and turns to the text color while focused.
    fn pressable(
        &mut self,
        name: &'static str,
        index: usize,
        border: Option<u32>,
        window: &Window,
        cx: &mut Context<Self>,
        press: Handler,
    ) -> Stateful<Div> {
        let theme = theme(cx).clone();
        let handle = self.stop(name, index, cx);
        let ring = if handle.is_focused(window) { Some(theme.tokens.text) } else { border };
        let on_key = press.clone();
        div()
            .id(ElementId::from((name, index)))
            .track_focus(&handle)
            .tab_stop(true)
            .cursor_pointer()
            .border_1()
            .border_color(ring.map_or(rgba(0), rgb))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.stop(name, index, cx).focus(window, cx);
                press(this, window, cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let modifiers = &event.keystroke.modifiers;
                if matches!(event.keystroke.key.as_str(), "enter" | "space")
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                {
                    on_key(this, window, cx);
                    cx.stop_propagation();
                }
            }))
    }

    fn button(
        &mut self,
        name: &'static str,
        index: usize,
        label: &'static str,
        window: &Window,
        cx: &mut Context<Self>,
        press: Handler,
    ) -> Stateful<Div> {
        let theme = theme(cx).clone();
        self.pressable(name, index, Some(theme.tokens.line), window, cx, press)
            .h(theme.ui(34.))
            .px(theme.ui(14.))
            .flex()
            .items_center()
            .rounded(px(RADIUS_SMALL))
            .bg(rgb(theme.tokens.raised))
            .text_sm()
            .child(label)
    }

    /// An accent link (an i in a circle, then `label`) that opens the guide `file` under `docs/`.
    fn guide_link(
        &mut self,
        name: &'static str,
        label: &'static str,
        file: &'static str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = theme(cx).clone();
        let url = format!("{GUIDES_URL}{file}");
        self.pressable(name, 0, None, window, cx, handler(move |_, _, cx| cx.open_url(&url)))
            .py(theme.ui(2.))
            .px(theme.ui(2.))
            .flex()
            .items_center()
            .gap(theme.ui(5.))
            .rounded(px(RADIUS_SMALL))
            .text_sm()
            .text_color(rgb(theme.tokens.accent))
            .child(svg().path(INFO_ICON).flex_none().size(theme.ui(14.)).text_color(rgb(theme.tokens.accent)))
            .child(label)
    }

    fn switch(
        &mut self,
        name: &'static str,
        index: usize,
        on: bool,
        window: &Window,
        cx: &mut Context<Self>,
        toggle: Handler,
    ) -> Stateful<Div> {
        let theme = theme(cx).clone();
        let track = if on { theme.tokens.accent } else { theme.tokens.line };
        let knob = if on { theme.tokens.on_accent } else { theme.tokens.muted };
        self.pressable(name, index, None, window, cx, toggle)
            .flex_none()
            .w(theme.ui(40.))
            .h(theme.ui(24.))
            .p(px(2.))
            .flex()
            .items_center()
            .when(on, |el| el.justify_end())
            .rounded_full()
            .bg(rgb(track))
            .child(div().size(theme.ui(18.)).rounded_full().bg(rgb(knob)))
    }

    fn stepper(
        &mut self,
        stepped: &'static Stepped,
        settings: &Settings,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = theme(cx).clone();
        let value = (stepped.get)(settings);
        let by = |sign: f32| {
            change(move |settings| {
                (stepped.set)(settings, step((stepped.get)(settings), sign * stepped.delta, stepped.min, stepped.max))
            })
        };
        let (less, more) = (by(-1.0), by(1.0));
        let (key_less, key_more) = (less.clone(), more.clone());
        let square = |this: &mut Self, index: usize, label: &'static str, press: Handler, cx: &mut Context<Self>| {
            this.pressable(stepped.name, index, Some(theme.tokens.line), window, cx, press)
                .size(theme.ui(34.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(RADIUS_SMALL))
                .bg(rgb(theme.tokens.raised))
                .child(label)
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(theme.ui(6.))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let handler = match event.keystroke.key.as_str() {
                    "left" | "down" => &key_less,
                    "right" | "up" => &key_more,
                    _ => return,
                };
                handler(this, window, cx);
                cx.stop_propagation();
            }))
            .child(square(self, 0, "−", less, cx))
            .child(
                div()
                    .min_w(theme.ui(56.))
                    .px(theme.ui(4.))
                    .flex()
                    .justify_center()
                    .whitespace_nowrap()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(with_unit(value, stepped.unit)),
            )
            .child(square(self, 1, "+", more, cx))
    }

    /// A drop-down: a button showing `label`, and while open the `options` under it.
    #[allow(clippy::too_many_arguments)]
    fn dropdown(
        &mut self,
        name: &'static str,
        index: usize,
        popover: Popover,
        label: String,
        width: Pixels,
        options: Vec<(String, bool, Handler)>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = theme(cx).clone();
        let open = handler(move |this, _, cx| {
            this.popover = Some(popover);
            cx.notify();
        });
        let button = self
            .pressable(
                name,
                index,
                Some(if self.popover == Some(popover) { theme.tokens.accent } else { theme.tokens.line }),
                window,
                cx,
                open,
            )
            .h(theme.ui(34.))
            .w(width)
            .px(theme.ui(10.))
            .flex()
            .items_center()
            .justify_between()
            .rounded(px(RADIUS_SMALL))
            .bg(rgb(theme.tokens.raised))
            .text_sm()
            .child(div().min_w_0().truncate().child(label))
            .child(div().flex_none().text_color(rgb(theme.tokens.muted)).child("▾"));
        let menu = (self.popover == Some(popover)).then(|| {
            let max_height = theme.ui(DROPDOWN_ROWS * DROPDOWN_ROW);
            let needed = theme.ui(options.len() as f32 * DROPDOWN_ROW + 8.).min(max_height);
            let button_bounds = self.dropdown_bounds.borrow().get(&(name, index)).copied().unwrap_or_default();
            let below = f32::from(window.viewport_size().height - button_bounds.bottom());
            let upward = below < f32::from(needed) + 8. && f32::from(button_bounds.top()) > below;
            let items: Vec<_> = options
                .into_iter()
                .enumerate()
                .map(|(position, (text, selected, pick))| {
                    let pick = handler(move |this, window, cx| {
                        this.popover = None;
                        this.focus_handle.focus(window, cx);
                        pick(this, window, cx);
                    });
                    self.pressable(name, 100 + index * 1000 + position, None, window, cx, pick)
                        .px_3()
                        .py_1()
                        .rounded(px(RADIUS_SMALL))
                        .whitespace_nowrap()
                        .hover(|style| style.bg(rgb(theme.tokens.line)))
                        .when(selected, |item| {
                            item.text_color(rgb(theme.tokens.accent)).font_weight(FontWeight::SEMIBOLD)
                        })
                        .child(text)
                })
                .collect();
            deferred(
                div()
                    .id(ElementId::from((name, 99 + index * 1000)))
                    .occlude()
                    .absolute()
                    .when(upward, |menu| menu.bottom(theme.ui(38.)))
                    .when(!upward, |menu| menu.top(theme.ui(38.)))
                    .left_0()
                    .w(width)
                    .max_h(max_height)
                    .overflow_y_scroll()
                    .p_1()
                    .flex()
                    .flex_col()
                    .text_sm()
                    .bg(rgb(theme.tokens.raised))
                    .border_1()
                    .border_color(rgb(theme.tokens.line))
                    .rounded(px(RADIUS))
                    .shadow_lg()
                    .children(items),
            )
            .with_priority(1)
        });
        let bounds = self.dropdown_bounds.clone();
        let measure = canvas(
            move |area, _, _| {
                bounds.borrow_mut().insert((name, index), area);
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        div().relative().child(button).child(measure).children(menu)
    }

    /// The trailing slot with a "Reset" text button when `show`, empty otherwise.
    fn reset_button(
        &mut self,
        name: &'static str,
        index: usize,
        show: bool,
        reset: Handler,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = theme(cx).clone();
        let slot = div().flex_none().w(theme.ui(SLOT_WIDTH)).flex().justify_end();
        if !show {
            return slot;
        }
        slot.child(
            self.pressable(name, index, None, window, cx, reset)
                .h(theme.ui(28.))
                .px(theme.ui(8.))
                .flex()
                .items_center()
                .rounded(px(RADIUS_SMALL))
                .text_xs()
                .text_color(rgb(theme.tokens.accent))
                .child("Reset"),
        )
    }

    /// A trailing slot whose "Reset" shows when the setting `get` reads differs from its default and puts the default back with `set`.
    #[allow(clippy::too_many_arguments)]
    fn reset_slot<T: PartialEq + 'static>(
        &mut self,
        name: &'static str,
        index: usize,
        settings: &Settings,
        get: impl Fn(&Settings) -> T + Copy + 'static,
        set: impl Fn(&mut Settings, T) + Copy + 'static,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let show = differs(settings, get);
        self.reset_button(
            name,
            index,
            show,
            change(move |settings| set(settings, get(&Settings::default()))),
            window,
            cx,
        )
    }

    /// A segmented bar: one button per label, the `chosen` one filled; `pick(i)` is the handler for button `i`.
    fn segmented(
        &mut self,
        name: &'static str,
        labels: &[&'static str],
        chosen: usize,
        pick: impl Fn(usize) -> Handler,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = theme(cx).clone();
        let segments: Vec<_> = labels
            .iter()
            .enumerate()
            .map(|(index, label)| {
                self.pressable(name, index, None, window, cx, pick(index))
                    .h(theme.ui(32.))
                    .px(theme.ui(16.))
                    .flex()
                    .items_center()
                    .rounded(px(RADIUS_SMALL))
                    .text_sm()
                    .whitespace_nowrap()
                    .when(index == chosen, |segment| {
                        segment
                            .bg(rgb(theme.tokens.accent))
                            .text_color(rgb(theme.tokens.on_accent))
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(index != chosen, |segment| segment.text_color(rgb(theme.tokens.text)))
                    .child(*label)
            })
            .collect();
        div()
            .flex_none()
            .flex()
            .p(px(3.))
            .gap(px(3.))
            .rounded(px(RADIUS_SMALL + 2.))
            .bg(rgb(theme.tokens.raised))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .children(segments)
    }

    fn open_settings_button(&mut self, index: usize, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let open = handler(|_, _, cx| {
            let path = cx.global::<SettingsStore>().path().to_path_buf();
            open_settings_file(&path, cx);
        });
        let ink = theme(cx).tokens.text;
        self.button("open-settings-json", index, "Open settings.json", window, cx, open)
            .flex_none()
            .text_color(rgb(ink))
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn heading(theme: &Theme, text: &'static str) -> Div {
    div().text_size(theme.ui(15.)).font_weight(FontWeight::SEMIBOLD).child(text)
}

fn muted(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(rgb(theme.tokens.muted)).child(text.into())
}

/// Width (before scaling) of a row's trailing slot, where "Reset" shows.
const SLOT_WIDTH: f32 = 56.;

/// A section of a page: heading, optional description under it, then a bordered box of `rows` divided by lines.
fn group(theme: &Theme, title: &'static str, description: Option<&'static str>, rows: Vec<Div>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(theme.ui(10.))
        .child(div().child(heading(theme, title)).children(description.map(|text| muted(theme, text).mt(theme.ui(2.)))))
        .child(
            div()
                .flex()
                .flex_col()
                .rounded(px(RADIUS))
                .bg(rgb(theme.tokens.surface))
                .border_1()
                .border_color(rgb(theme.tokens.line))
                .children(rows.into_iter().enumerate().map(|(index, row)| {
                    row.when(index > 0, |row| row.border_t_1().border_color(rgb(theme.tokens.line)))
                })),
        )
}

/// A row's label column: the title (medium weight) and its description under it.
fn row_label(title: impl Into<SharedString>, description: impl IntoElement) -> Div {
    div().child(div().font_weight(FontWeight::MEDIUM).child(title.into())).child(description)
}

/// A row: `label` on the left (it flexes and wraps), then `control` and the `trailing` slot on the right. They wrap under the
/// label when the row is narrow.
fn row_shell(theme: &Theme, label: Div, control: impl IntoElement, trailing: Div) -> Div {
    div()
        .min_h(theme.ui(56.))
        .px(theme.ui(16.))
        .py(theme.ui(12.))
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(theme.ui(16.))
        .gap_y(theme.ui(8.))
        .child(label.flex_1().min_w(theme.ui(200.)))
        .child(div().flex_none().ml_auto().flex().items_center().gap(theme.ui(12.)).child(control).child(trailing))
}

/// A setting as a row: title, one line of description, the control and the trailing slot.
fn row(
    theme: &Theme,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: impl IntoElement,
    trailing: Div,
) -> Div {
    row_shell(theme, row_label(title, muted(theme, description)), control, trailing)
}

/// A row whose `content` takes the full width under the optional label (title and description).
fn stacked_row(theme: &Theme, label: Option<(&'static str, &'static str)>, content: impl IntoElement) -> Div {
    div()
        .px(theme.ui(16.))
        .py(theme.ui(12.))
        .flex()
        .flex_col()
        .gap(theme.ui(10.))
        .children(label.map(|(title, description)| row_label(title, muted(theme, description))))
        .child(content)
}

/// A trailing slot with nothing in it, so rows without a Reset line up with the ones that have one.
fn empty_slot(theme: &Theme) -> Div {
    div().flex_none().w(theme.ui(SLOT_WIDTH))
}

/// The trailing slot of a list row: as wide as its `actions` need.
fn actions_slot(theme: &Theme) -> Div {
    div().flex_none().flex().items_center().gap(theme.ui(4.))
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        let store = cx.global::<SettingsStore>();
        let settings = store.settings().clone();
        let problems = store.problems().to_vec();
        let path = display_path(store.path(), std::env::home_dir().as_deref());
        let section = self.section;
        self.end_recording_without_focus(window);

        let items = Section::ALL.map(|item| {
            let active = item == section;
            let choose = handler(move |this, _, cx| {
                this.section = item;
                this.popover = None;
                this.recording = None;
                this.shortcut_message = None;
                this.pending_replace = None;
                this.pending_take = None;
                this.body_scroll.set_offset(point(px(0.), px(0.)));
                cx.notify();
            });
            self.pressable("section", item as usize, None, window, cx, choose)
                .h(theme.ui(36.))
                .px_3()
                .flex()
                .items_center()
                .rounded(px(RADIUS_SMALL))
                .text_color(rgb(if active { theme.tokens.text } else { theme.tokens.muted }))
                // The fill already shows the active item, so it draws no focus ring.
                .when(active, |el| {
                    el.bg(rgb(theme.tokens.raised)).font_weight(FontWeight::SEMIBOLD).border_color(rgba(0))
                })
                .when(!active, |el| el.hover(|style| style.bg(rgb(theme.tokens.raised))))
                .child(item.label())
        });
        let [app_line, daemon_line] = footer_lines(cx.try_global::<DaemonBuild>());
        let foot = div()
            .flex()
            .flex_col()
            .items_start()
            .gap(theme.ui(8.))
            .px_3()
            .child(muted(&theme, format!("Saved to {path}")).max_w_full())
            .child(self.open_settings_button(0, window, cx))
            .child(div().w_full().h(px(1.)).bg(rgb(theme.tokens.line)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.ui(2.))
                    .child(muted(&theme, app_line))
                    .child(muted(&theme, daemon_line)),
            );
        let menu = div()
            .flex_none()
            .w(theme.ui(230.))
            .py(theme.ui(24.))
            .px(theme.ui(12.))
            .flex()
            .flex_col()
            .gap(theme.ui(4.))
            .border_r_1()
            .border_color(rgb(theme.tokens.line))
            .children(items)
            .child(div().flex_1())
            .child(foot);

        let header = div()
            .flex_none()
            .child(div().text_xl().font_weight(FontWeight::BOLD).child(section.label()))
            .child(div().mt(theme.ui(4.)).text_sm().text_color(rgb(theme.tokens.muted)).child(section.subtitle()));

        let banner = (!problems.is_empty()).then(|| {
            let colors = theme.tone(Tone::NeedsInput);
            div()
                .flex_none()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(theme.ui(12.))
                .p(theme.ui(12.))
                .rounded(px(RADIUS))
                .bg(rgb(colors.fill.unwrap_or(theme.tokens.raised)))
                .text_color(rgb(colors.ink))
                .child(
                    // The button wraps under the text when the text column would get narrower than this.
                    div()
                        .flex_1()
                        .min_w(theme.ui(240.))
                        .text_sm()
                        .flex()
                        .flex_col()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("The settings file has problems"))
                        .children(problems.iter().map(problem_line).map(|line| div().child(line))),
                )
                .child(self.open_settings_button(1, window, cx))
        });

        let body = match section {
            Section::Appearance => self.appearance(&theme, &settings, window, cx),
            Section::Terminal => self.terminal(&theme, &settings, window, cx),
            Section::Alerts => self.alerts(&theme, &settings, window, cx),
            Section::Shortcuts => self.shortcuts(&theme, &settings, window, cx),
            Section::Sessions => self.sessions(&theme, &settings, window, cx),
            Section::SshHosts => self.ssh_hosts_page(&theme, &settings, window, cx),
            Section::Window => self.window_page(&theme, &settings, window, cx),
        };

        let backdrop =
            self.popover.is_some().then(|| {
                deferred(div().absolute().top_0().left_0().size_full().occlude().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.close_popover(window, cx)),
                ))
            });

        div()
            .id("settings-page")
            .key_context("Settings")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .overflow_hidden()
            .bg(rgb(theme.tokens.ground))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .rounded(px(RADIUS_LARGE))
            .text_sm()
            .text_color(rgb(theme.tokens.text))
            .child(menu)
            .child(
                div()
                    .id("settings-body")
                    .track_scroll(&self.body_scroll)
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .px(theme.ui(40.))
                    .py(theme.ui(28.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .w_full()
                            .max_w(theme.ui(780.))
                            .flex()
                            .flex_col()
                            .gap(theme.ui(26.))
                            .child(header)
                            .children(banner)
                            .child(body),
                    ),
            )
            .children(backdrop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_stay_in_range_and_free_of_float_noise() {
        assert_eq!(step(1.3, 0.1, 1.0, 2.0), 1.4);
        assert_eq!(step(2.0, 0.1, 1.0, 2.0), 2.0);
        assert_eq!(step(1.0, -0.1, 1.0, 2.0), 1.0);
        assert_eq!(step(40.0, 1.0, 8.0, 40.0), 40.0);
        assert_eq!(step(8.0, -1.0, 8.0, 40.0), 8.0);
        assert_eq!(step(14.0, 1.0, 8.0, 40.0), 15.0);
        assert_eq!(step(95.0, 5.0, 0.0, 100.0), 100.0);
    }

    #[test]
    fn numbers_show_decimals_only_when_needed() {
        assert_eq!(format_number(14.0), "14");
        assert_eq!(format_number(1.3), "1.3");
        assert_eq!(format_number(600.0), "600");
    }

    #[test]
    fn thousands_are_separated() {
        assert_eq!(format_number(1000.0), "1,000");
        assert_eq!(format_number(10000.0), "10,000");
        assert_eq!(format_number(100000.0), "100,000");
        assert_eq!(format_number(999.0), "999");
    }

    #[test]
    fn units_follow_the_number() {
        assert_eq!(with_unit(10000.0, "lines"), "10,000 lines");
        assert_eq!(with_unit(3.0, "s"), "3 s");
        assert_eq!(with_unit(14.0, ""), "14");
    }

    #[test]
    fn a_setting_differs_only_from_its_default() {
        let mut settings = Settings::default();
        assert!(!differs(&settings, |s| s.ui_font_size));
        settings.ui_font_size += 1.0;
        assert!(differs(&settings, |s| s.ui_font_size));
        assert!(!differs(&settings, |s| s.window.scrollback));
    }

    #[test]
    fn problem_lines_name_the_key_when_there_is_one() {
        let keyed = Problem { key: "alerts.volume".into(), message: "expected a number".into() };
        let whole = Problem { key: String::new(), message: "not valid JSON".into() };
        assert_eq!(problem_line(&keyed), "alerts.volume: expected a number");
        assert_eq!(problem_line(&whole), "not valid JSON");
    }
}
