//! GPUI view that renders one terminal pane and exchanges bytes with its PTY.

use std::ops::Range;
use std::time::{Duration, Instant};

use crate::assets::ARROW_DOWN_ICON;
use crate::settings_store::SettingsStore;
use crate::theme::{RADIUS, Theme, UI_FONT, theme};
use app_core::bidi::{RowOrder, logical_hit, to_display};
use app_core::host_state::HostCard;
use app_core::image_paste;
use app_core::input::{KeyInput, arrow_presses, edit_bytes, encode_key};
use app_core::layout::grid_dims;
use app_core::mouse::{self, MouseAction, MouseMods, encode_mouse};
use app_core::paste::encode_paste;
use app_core::scroll::{
    ACTIVE_HOLD_MS, CHIP_FADE_MS, CHIP_HOLD_MS, JUMP_MS, ScrollAction, activity, chip_opacity, jump_offset,
    offset_for_thumb_top, scroll_action, thumb,
};
use app_core::settings::Settings;
use app_core::shortcuts::{self, Group, Platform};
use app_core::snapshot::{CellStyle, Rgb, ScreenSnapshot, row_spans};
use app_core::term_state::{Reach, TermState};
use app_core::theme_colors::{deepen, mix, terminal_palette};
use app_core::url::link_at;
use app_core::workspace::PaneLaunch;
use daemon::client::{ClientEvent, DaemonClient};
use gpui::{
    App, BorderStyle, Bounds, ClickEvent, ContentMask, Context, EventEmitter, FocusHandle, Focusable, Font, FontStyle,
    FontWeight, Hsla, Pixels, Point, StrikethroughStyle, Task, TextAlign, TextRun, UnderlineStyle, Window, canvas, div,
    fill, font, outline, point, prelude::*, px, rgb, size, svg,
};
use gpui::{
    ClipboardEntry, ClipboardItem, ElementInputHandler, EntityInputHandler, KeyBinding, KeyDownEvent, Modifiers,
    ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ScrollWheelEvent, UTF16Selection,
};
use protocol::message::PaneId;

#[allow(missing_docs)]
mod terminal_actions {
    gpui::actions!(terminal, [Copy, Paste, ZoomIn, ZoomOut, ZoomReset]);
}
use terminal_actions::{Copy, Paste, ZoomIn, ZoomOut, ZoomReset};

/// The binding of a terminal action (`id` is its settings key) at `key`.
pub(crate) fn terminal_binding(id: &str, key: &str) -> Option<KeyBinding> {
    let context = Some("Terminal");
    Some(match id {
        "copy" => KeyBinding::new(key, Copy, context),
        "paste" => KeyBinding::new(key, Paste, context),
        "zoomIn" => KeyBinding::new(key, ZoomIn, context),
        "zoomOut" => KeyBinding::new(key, ZoomOut, context),
        "zoomReset" => KeyBinding::new(key, ZoomReset, context),
        _ => return None,
    })
}

#[cfg(target_os = "linux")]
pub(crate) const FONT_FAMILY: &str = "Noto Sans Mono";
#[cfg(target_os = "macos")]
pub(crate) const FONT_FAMILY: &str = "Menlo";
#[cfg(target_os = "windows")]
pub(crate) const FONT_FAMILY: &str = "Consolas";

/// Tried before cosmic-text's own fallback list, which misses some installed symbol fonts (it asks for "Noto Sans
/// Symbols2"; `⏺` then lands on a COLRv1 emoji font that cannot be drawn). GPUI drops — and unloads — any fallback
/// font without an `m` glyph, so pure symbol or Hebrew fonts must not be listed here.
#[cfg(target_os = "linux")]
const FALLBACK_FAMILIES: &[&str] = &["Adwaita Mono", "Symbola"];

/// Monospace families tried, in order, when the chosen one is not installed (a missing family would be drawn in a
/// proportional font, out of the grid).
const MONOSPACE_FAMILIES: &[&str] =
    &[FONT_FAMILY, "DejaVu Sans Mono", "Ubuntu Mono", "Liberation Mono", "Adwaita Mono", "Menlo", "Consolas"];

/// `wanted` (else the platform's family) when it is installed, else the first installed of [`MONOSPACE_FAMILIES`].
pub(crate) fn installed_family<'a>(wanted: Option<&'a str>, installed: &[String]) -> &'a str {
    let wanted = wanted.unwrap_or(FONT_FAMILY);
    let has = |family: &str| installed.iter().any(|name| name == family);
    if has(wanted) {
        return wanted;
    }
    MONOSPACE_FAMILIES.iter().copied().find(|family| has(family)).unwrap_or(wanted)
}

/// The installed font families, read once (the text system loads them at start).
pub(crate) fn installed_families(cx: &App) -> &'static [String] {
    static FAMILIES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    FAMILIES.get_or_init(|| cx.text_system().all_font_names())
}

/// The pane font (`family`, else the platform's), with its fallback chain on Linux.
pub(crate) fn pane_font(family: Option<&str>) -> Font {
    #[allow(unused_mut)]
    let mut pane_font = font(family.unwrap_or(FONT_FAMILY).to_string());
    #[cfg(target_os = "linux")]
    {
        pane_font.fallbacks =
            Some(gpui::FontFallbacks::from_fonts(FALLBACK_FAMILIES.iter().map(|family| family.to_string()).collect()));
    }
    pane_font
}

/// An empty terminal of the given size, in the current theme's colors.
fn new_term(cols: u16, rows: u16, cx: &App) -> TermState {
    let lines = cx.global::<SettingsStore>().settings().window.scrollback as usize;
    let mut term = TermState::with_scrollback(cols, rows, lines);
    term.set_palette(terminal_palette(theme(cx).preset));
    term
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CellMetrics {
    width: Pixels,
    height: Pixels,
}

/// What the pane is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneState {
    /// Waiting for the daemon to start the process.
    Starting,
    /// Showing a live process.
    Live,
    /// The process ended (`None`: it ended while Gantry was closed).
    Exited(Option<u32>),
    /// The process could not be started.
    Failed(String),
    /// The daemon connection is gone.
    Disconnected,
}

/// What happens to input typed into a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputFate {
    /// Write it to the process.
    Send,
    /// Keep it until the process starts.
    Hold,
    /// Discard it (no process will read it).
    Drop,
}

fn input_fate(state: &PaneState) -> InputFate {
    match state {
        PaneState::Live => InputFate::Send,
        PaneState::Starting => InputFate::Hold,
        PaneState::Exited(_) | PaneState::Failed(_) | PaneState::Disconnected => InputFate::Drop,
    }
}

/// Whether mouse reports can reach the pane's program. Unlike keys they are never held: a click made while the
/// pane restarts would arrive in the new program as escape bytes.
fn mouse_reaches_program(state: &PaneState) -> bool {
    input_fate(state) == InputFate::Send
}

/// Width of the scrollbar column at the right edge of a pane, before the interface scale.
const SCROLLBAR_WIDTH: f32 = 10.0;
/// The shortest the scrollbar thumb gets.
const SCROLLBAR_MIN_THUMB: f32 = 24.0;

/// The shortest the scrollbar thumb gets, in pixels.
fn min_thumb(cx: &App) -> f32 {
    f32::from(theme(cx).ui(SCROLLBAR_MIN_THUMB))
}

/// What the pressed mouse button is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Drag {
    /// A local selection is being dragged.
    selecting: bool,
    /// Button whose press was reported to the program (its release goes there too).
    reporting: Option<mouse::Button>,
    /// Last cell a mouse report named.
    reported_cell: Option<(u16, u16)>,
}

impl Drag {
    /// A press went to the program; any selection drag whose release was missed ends.
    fn reported_press(&mut self, button: mouse::Button, cell: (u16, u16)) {
        *self = Drag { selecting: false, reporting: Some(button), reported_cell: Some(cell) };
    }

    /// A local selection drag begins.
    fn selection_press(&mut self) {
        *self = Drag { selecting: true, ..Drag::default() };
    }

    /// The button came up, inside the pane or not: the reported press to release and the last cell reported.
    fn release(&mut self) -> Option<(mouse::Button, Option<(u16, u16)>)> {
        let released = self.reporting.map(|button| (button, self.reported_cell));
        *self = Drag::default();
        released
    }
}

/// Something the workspace should react to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneEvent {
    /// The user clicked into the pane.
    Focused,
    /// The user asked to run the pane again (Continue / Restart).
    Rerun,
    /// The user asked to close the pane.
    Close,
    /// The program changed the title.
    TitleChanged,
    /// The button on the host's card was pressed (Connect, Reconnect, Sign in).
    HostAction,
}

/// One terminal pane, showing a daemon-owned session.
pub struct TerminalView {
    focus_handle: FocusHandle,
    term: TermState,
    client: Option<DaemonClient>,
    pane: Option<PaneId>,
    launch: PaneLaunch,
    /// Whether this pane lives on an SSH host (its leaf's host is not this machine).
    remote: bool,
    state: PaneState,
    title: Option<String>,
    cell: (u16, u16),
    grid_origin: Point<Pixels>,
    grid_size: (u16, u16),
    metrics: CellMetrics,
    scroll_accum: f32,
    /// Where the scrollbar track was drawn last frame.
    scrollbar_track: Bounds<Pixels>,
    /// While the scrollbar thumb is dragged: how far below the thumb's top edge it was grabbed.
    scrollbar_grab: Option<f32>,
    /// When the view last scrolled by wheel, drag or the jump to bottom chip: the thumb is in its active color for a
    /// moment after.
    scrollbar_moved: Option<Instant>,
    /// Repaints the thumb when its active color should start to fade.
    scrollbar_timer: Option<Task<()>>,
    /// While the jump to bottom chip scrolls the view down: the offset it started at and when.
    jump: Option<(usize, Instant)>,
    drag: Drag,
    /// A selection made with Shift+arrows or Cmd+A on the prompt line: its fixed end and its moving end.
    prompt_select: Option<((usize, usize), (usize, usize))>,
    /// Cell of a plain single click, which moves the prompt's cursor there when released without dragging.
    click_cell: Option<(u16, u16)>,
    mouse_position: Option<Point<Pixels>>,
    hovered_link: Option<(u16, Range<usize>)>,
    /// Display order of each viewport row in the last frame (`None`: drawn as stored).
    row_orders: Vec<Option<RowOrder>>,
    /// Input typed while Starting, sent on attach.
    held_input: Vec<u8>,
    /// The state card of this pane's host, while the host is not usable.
    host_card: Option<HostCard>,
}

impl EventEmitter<PaneEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    /// A pane that will show `launch` once `attach` is called.
    pub fn new(
        client: Option<DaemonClient>,
        launch: PaneLaunch,
        remote: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        TerminalView {
            focus_handle: cx.focus_handle(),
            term: new_term(80, 24, cx),
            client,
            pane: None,
            launch,
            remote,
            state: PaneState::Starting,
            title: None,
            cell: (0, 0),
            grid_origin: Point::default(),
            grid_size: (80, 24),
            metrics: CellMetrics { width: px(8.), height: px(18.) },
            scroll_accum: 0.0,
            scrollbar_track: Bounds::default(),
            scrollbar_grab: None,
            scrollbar_moved: None,
            scrollbar_timer: None,
            jump: None,
            drag: Drag::default(),
            click_cell: None,
            prompt_select: None,
            mouse_position: None,
            hovered_link: None,
            row_orders: Vec::new(),
            held_input: Vec::new(),
            host_card: None,
        }
    }

    /// Use `client` for this pane's input and resizes (the host reconnected, or connected for the first time).
    pub fn set_client(&mut self, client: Option<DaemonClient>) {
        self.client = client;
    }

    /// Show the state card of this pane's host (`None` clears it). A pane that is attached stops taking input while
    /// the card is up.
    pub fn set_host_card(&mut self, card: Option<HostCard>, cx: &mut Context<Self>) {
        if card.is_some() && self.pane.is_some() {
            self.state = PaneState::Disconnected;
        }
        self.host_card = card;
        cx.notify();
    }

    /// The theme changed: use its terminal colors.
    pub fn apply_theme(&mut self, cx: &mut Context<Self>) {
        self.term.set_palette(terminal_palette(theme(cx).preset));
        cx.notify();
    }

    /// Show daemon pane `pane`, running `launch` (a fresh terminal; its output or replay follows).
    pub fn attach(&mut self, pane: PaneId, launch: PaneLaunch, cx: &mut Context<Self>) {
        self.pane = Some(pane);
        self.host_card = None;
        self.launch = launch;
        self.state = PaneState::Live;
        self.term = new_term(self.grid_size.0, self.grid_size.1, cx);
        self.title = None;
        if let Some((cols, rows, width, height)) = resize_on_attach(self.grid_size, self.cell)
            && let Some(client) = &self.client
        {
            client.resize(pane, cols, rows, width, height);
        }
        if !self.held_input.is_empty()
            && let Some(client) = &self.client
        {
            client.input(pane, &std::mem::take(&mut self.held_input));
        }
        cx.notify();
    }

    /// Output or resync for this pane.
    pub fn on_event(&mut self, event: &ClientEvent, cx: &mut Context<Self>) {
        match event {
            ClientEvent::Output { bytes, .. } => {
                self.term.feed(bytes);
                // The daemon answers terminal queries; replies from this copy would be duplicates.
                self.term.take_pty_writes();
                // OSC 52: programs on a container or SSH host have no clipboard of their own.
                if let Some(text) = self.term.take_clipboard_stores().pop() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                let title = self.term.title();
                if title != self.title {
                    self.title = title;
                    cx.emit(PaneEvent::TitleChanged);
                }
            }
            ClientEvent::PasteImageSaved { path, .. } => self.paste_text(path, cx),
            ClientEvent::PasteImageFailed { reason, .. } => tracing::warn!("pasted image not saved: {reason}"),
            ClientEvent::Resync { cols, rows, .. } => {
                self.term = new_term(*cols, *rows, cx);
                // The next layout pass resizes to the window if it differs.
                self.grid_size = (*cols, *rows);
            }
            _ => return,
        }
        cx.notify();
    }

    /// The process ended.
    pub fn exited(&mut self, code: Option<u32>, cx: &mut Context<Self>) {
        self.pane = None;
        self.host_card = None;
        self.state = PaneState::Exited(code);
        self.held_input.clear();
        cx.notify();
    }

    /// The process could not be started.
    pub fn failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.state = PaneState::Failed(error);
        self.held_input.clear();
        cx.notify();
    }

    /// The daemon connection is gone.
    pub fn disconnected(&mut self, cx: &mut Context<Self>) {
        self.state = PaneState::Disconnected;
        self.held_input.clear();
        cx.notify();
    }

    /// About to start `launch` in this pane again.
    pub fn restarting(&mut self, launch: PaneLaunch, cx: &mut Context<Self>) {
        self.launch = launch;
        self.state = PaneState::Starting;
        cx.notify();
    }

    /// Whether a live process is running here.
    pub fn is_running(&self) -> bool {
        self.state == PaneState::Live
    }

    /// Whether the process ended (or could not start): the pane shows Continue / Restart.
    pub fn has_ended(&self) -> bool {
        matches!(self.state, PaneState::Exited(_) | PaneState::Failed(_))
    }

    /// Program title, else `program · dir`.
    pub fn title(&self) -> String {
        self.title.clone().filter(|title| !title.trim().is_empty()).unwrap_or_else(|| self.launch.label())
    }

    /// Current grid size (columns, rows).
    pub fn grid_size(&self) -> (u16, u16) {
        self.grid_size
    }

    fn write_pty(&mut self, bytes: &[u8]) {
        match input_fate(&self.state) {
            InputFate::Send => {
                if let Some(pane) = self.pane
                    && let Some(client) = &self.client
                {
                    client.input(pane, bytes);
                }
            }
            InputFate::Hold => self.held_input.extend_from_slice(bytes),
            InputFate::Drop => {}
        }
    }

    /// Banner text and whether Continue/Restart + Close apply.
    fn banner(&self) -> Option<(String, bool)> {
        if self.host_card.is_some() {
            return None;
        }
        match &self.state {
            PaneState::Starting | PaneState::Live => None,
            PaneState::Exited(Some(code)) => Some((format!("Process exited (code {code})"), true)),
            PaneState::Exited(None) => Some(("Session ended while Gantry was closed".into(), true)),
            PaneState::Failed(error) => Some((format!("Could not start: {error}"), true)),
            PaneState::Disconnected => Some(("Lost connection to the Gantry daemon — restart Gantry".into(), false)),
        }
    }

    fn cell_metrics(font: &Font, font_size: Pixels, line_height: f32, window: &Window) -> CellMetrics {
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(font);
        let width =
            text_system.advance(font_id, font_size, 'm').map(|advance| advance.width).unwrap_or(font_size * 0.6);
        CellMetrics { width, height: (font_size * line_height).round() }
    }

    fn layout(&mut self, bounds: Bounds<Pixels>, metrics: CellMetrics, cx: &mut Context<Self>) {
        self.grid_origin = bounds.origin;
        self.metrics = metrics;
        let cell = (f32::from(metrics.width).round() as u16, f32::from(metrics.height).round() as u16);
        self.term.set_cell_size(cell.0, cell.1);
        self.cell = cell;
        let dims = grid_dims(
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
            f32::from(metrics.width),
            f32::from(metrics.height),
        );
        if dims != self.grid_size {
            self.grid_size = dims;
            self.term.resize(dims.0, dims.1);
            if let Some(pane) = self.pane
                && let Some(client) = &self.client
            {
                client.resize(pane, dims.0, dims.1, cell.0, cell.1);
            }
            cx.notify();
        }
    }

    /// Text-field selection on the prompt line: Shift+arrows by character, with Option by word, with Cmd to the line's
    /// edge; Cmd+A selects the whole line. Returns whether the key was taken.
    fn select_on_prompt(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let Some(cursor) = self.term.prompt_cursor() else { return false };
        let (reach, forward) = match id {
            "selectLine" => {
                let Some((anchor, focus)) = self.term.prompt_line() else { return false };
                self.term.select_cells(anchor, focus);
                self.prompt_select = Some((anchor, focus));
                cx.notify();
                return true;
            }
            "selectLeft" => (Reach::Char, false),
            "selectRight" => (Reach::Char, true),
            "selectWordLeft" => (Reach::Word, false),
            "selectWordRight" => (Reach::Word, true),
            "selectToLineStart" => (Reach::Line, false),
            "selectToLineEnd" => (Reach::Line, true),
            _ => return false,
        };
        let (anchor, focus) = self.prompt_select.unwrap_or((cursor, cursor));
        let (anchor, focus) = (anchor, self.term.prompt_step(focus, reach, forward));
        self.term.select_cells(anchor, focus);
        self.prompt_select = Some((anchor, focus));
        cx.notify();
        true
    }

    /// Keys that delete the selection, when it lies on the prompt's line: arrows to its end, then Backspaces.
    fn delete_prompt_selection(&self) -> Option<Vec<u8>> {
        let (moves, length) = self.term.prompt_selection_edit()?;
        let mut bytes = arrow_presses(moves, self.term.key_modes().app_cursor);
        bytes.extend(std::iter::repeat_n(0x7f, length));
        Some(bytes)
    }

    fn send_input(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.jump = None;
        self.prompt_select = None;
        self.term.scroll_to_bottom();
        self.term.clear_selection();
        self.write_pty(bytes);
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if input_fate(&self.state) == InputFate::Drop {
            if matches!(self.state, PaneState::Exited(_) | PaneState::Failed(_)) {
                let keystroke = &event.keystroke;
                if keystroke.key == "enter" {
                    cx.emit(PaneEvent::Rerun);
                } else if keystroke.key == "d"
                    && keystroke.modifiers.control
                    && !keystroke.modifiers.shift
                    && !keystroke.modifiers.alt
                {
                    cx.emit(PaneEvent::Close);
                }
            }
            cx.stop_propagation();
            return;
        }
        let keystroke = &event.keystroke;
        if event.prefer_character_input && keystroke.key_char.is_some() {
            return;
        }
        let input = KeyInput {
            key: keystroke.key.clone(),
            key_char: keystroke.key_char.clone(),
            ctrl: keystroke.modifiers.control,
            alt: keystroke.modifiers.alt,
            shift: keystroke.modifiers.shift,
            platform: keystroke.modifiers.platform,
        };
        if self.paste_image(&input, cx) {
            cx.stop_propagation();
            return;
        }
        let plain = !(input.ctrl || input.alt || input.shift || input.platform);
        if plain
            && matches!(input.key.as_str(), "backspace" | "delete")
            && let Some(bytes) = self.delete_prompt_selection()
        {
            self.send_input(&bytes, cx);
            cx.stop_propagation();
            return;
        }
        if let Some(id) = editing_action(keystroke, cx)
            && !self.term.alt_screen()
        {
            match edit_bytes(id) {
                Some(bytes) => self.send_input(bytes, cx),
                None if self.select_on_prompt(id, cx) => {}
                None => return,
            }
            cx.stop_propagation();
            return;
        }
        if let Some(bytes) = encode_key(&input, self.term.key_modes()) {
            tracing::debug!(key = %keystroke.key, kitty = self.term.key_modes().kitty_disambiguate, "encoded key");
            self.send_input(&bytes, cx);
            cx.stop_propagation();
        }
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.term.selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.paste_text(&text, cx);
        }
    }

    fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let bytes = encode_paste(text, self.term.bracketed_paste());
        self.send_input(&bytes, cx);
    }

    /// Image paste (Ctrl+V with an image on the clipboard) in a Claude pane on an SSH host or in a dev container:
    /// the image goes to the pane's daemon, which saves it where Claude can read it and answers with the path.
    /// Returns whether the key was taken; anything else (a local pane, text on the clipboard, an older daemon) is
    /// left to the key encoding as before.
    fn paste_image(&mut self, key: &KeyInput, cx: &mut Context<Self>) -> bool {
        if !image_paste::is_paste_key(&key.key, key.ctrl, key.alt, key.shift, key.platform) {
            return false;
        }
        let (Some(pane), Some(client)) = (self.pane, &self.client) else { return false };
        if !image_paste::eligible(
            self.launch.is_claude(),
            self.remote,
            self.launch.container.is_some(),
            client.paste_image(),
        ) {
            return false;
        }
        let Some((format, bytes)) = cx.read_from_clipboard().as_ref().and_then(image_for_paste) else { return false };
        client.save_paste_image(pane, format, bytes);
        true
    }

    /// Zoom every pane: `size` is the terminal font size, kept in the settings file.
    fn set_font_size(size: f32, cx: &mut App) {
        SettingsStore::update(cx, |settings| settings.terminal_font.size = size.clamp(8.0, 40.0));
    }

    fn zoom_in(&mut self, _: &ZoomIn, _window: &mut Window, cx: &mut Context<Self>) {
        Self::set_font_size(cx.global::<SettingsStore>().settings().terminal_font.size + 1.0, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _window: &mut Window, cx: &mut Context<Self>) {
        Self::set_font_size(cx.global::<SettingsStore>().settings().terminal_font.size - 1.0, cx);
    }

    /// Back to the default size, which removes the key from the file.
    fn zoom_reset(&mut self, _: &ZoomReset, _window: &mut Window, cx: &mut Context<Self>) {
        Self::set_font_size(Settings::default().terminal_font.size, cx);
    }

    /// Viewport cell under a window position, plus whether it is in the cell's right half.
    fn cell_at(&self, position: Point<Pixels>) -> Option<(u16, u16, bool)> {
        let x = f32::from(position.x - self.grid_origin.x);
        let y = f32::from(position.y - self.grid_origin.y);
        let (w, h) = (f32::from(self.metrics.width), f32::from(self.metrics.height));
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let (col, row) = ((x / w).floor() as u16, (y / h).floor() as u16);
        if col >= self.grid_size.0 || row >= self.grid_size.1 {
            return None;
        }
        Some((col, row, x - col as f32 * w > w / 2.0))
    }

    /// The logical cell under a window position: `cell_at` mapped through the last frame's display order.
    fn logical_cell_at(&self, position: Point<Pixels>) -> Option<(u16, u16, bool)> {
        let (col, row, right_half) = self.cell_at(position)?;
        let (col, right_half) = logical_hit(&self.row_orders, col as usize, row as usize, right_half);
        Some((col as u16, row, right_half))
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        cx.emit(PaneEvent::Focused);
        let Some((col, row, right_half)) = self.logical_cell_at(event.position) else { return };
        if link_modifier(event.modifiers)
            && let Some(link) = link_at(&self.term.row_text(row), col as usize)
        {
            cx.open_url(&link.url);
            return;
        }
        if let Some(button) = report_button(event.button)
            && self.report(MouseAction::Press(button), event.modifiers, Some((col, row)))
        {
            self.drag.reported_press(button, (col, row));
            return;
        }
        self.drag = Drag::default();
        if event.button != MouseButton::Left {
            return;
        }
        let plain = !(event.modifiers.shift || event.modifiers.alt || event.modifiers.control);
        self.click_cell = (plain && event.click_count == 1).then_some((col, row));
        self.prompt_select = None;
        self.term.start_selection(col, row, right_half);
        self.drag.selection_press();
        cx.notify();
    }

    /// Send `action` at `cell` to the program when its mouse mode asks for it. Shift keeps the mouse for selecting,
    /// except for the release of a press that was already reported.
    fn report(&mut self, action: MouseAction, modifiers: Modifiers, cell: Option<(u16, u16)>) -> bool {
        if !mouse_reaches_program(&self.state) || modifiers.shift && !matches!(action, MouseAction::Release(_)) {
            return false;
        }
        let Some((col, row)) = cell else { return false };
        match encode_mouse(self.term.scroll_modes(), action, report_mods(modifiers), col, row) {
            Some(bytes) => {
                self.write_pty(&bytes);
                true
            }
            None => false,
        }
    }

    /// Underline the link under the mouse while the link modifier (Ctrl, or Cmd on macOS) is held.
    fn update_hovered_link(&mut self, modifiers: Modifiers, cx: &mut Context<Self>) {
        let hovered = self
            .mouse_position
            .filter(|_| link_modifier(modifiers))
            .and_then(|position| self.logical_cell_at(position))
            .and_then(|(col, row, _)| link_at(&self.term.row_text(row), col as usize).map(|link| (row, link.cols)));
        if hovered != self.hovered_link {
            self.hovered_link = hovered;
            cx.notify();
        }
    }

    fn modifiers_changed(&mut self, event: &ModifiersChangedEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.update_hovered_link(event.modifiers, cx);
    }

    /// The view just scrolled: show the thumb in its active color, and repaint once it should fade.
    fn scrollbar_scrolled(&mut self, cx: &mut Context<Self>) {
        self.scrollbar_moved = Some(Instant::now());
        self.scrollbar_timer = Some(cx.spawn(async move |this, cx| {
            // Repaint when the thumb starts to fade, then when the chip does; the fades run on animation frames.
            cx.background_executor().timer(Duration::from_millis(ACTIVE_HOLD_MS + 20)).await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                return;
            }
            let chip_fade_from = CHIP_HOLD_MS - CHIP_FADE_MS;
            cx.background_executor().timer(Duration::from_millis(chip_fade_from - ACTIVE_HOLD_MS)).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
    }

    /// The jump to bottom chip: back to live output.
    fn jump_to_bottom(&mut self, _: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.focus_handle.focus(window, cx);
        cx.emit(PaneEvent::Focused);
        let (_, offset) = self.term.scroll_extent();
        self.jump = Some((offset, Instant::now()));
        self.scrollbar_scrolled(cx);
        cx.notify();
    }

    /// A press on the scrollbar column: grab the thumb, or move it under the pointer when the press is on the track.
    fn scrollbar_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.jump = None;
        self.focus_handle.focus(window, cx);
        cx.emit(PaneEvent::Focused);
        let (history, offset) = self.term.scroll_extent();
        let track = f32::from(self.scrollbar_track.size.height);
        let rows = self.grid_size.1 as usize;
        let Some(thumb) = thumb(track, rows, history, offset, min_thumb(cx)).filter(|_| !self.term.alt_screen()) else {
            return;
        };
        let y = f32::from(event.position.y - self.scrollbar_track.origin.y);
        let on_thumb = y >= thumb.top && y <= thumb.top + thumb.len;
        self.scrollbar_grab = Some(if on_thumb { y - thumb.top } else { thumb.len / 2.0 });
        self.drag_scrollbar(event.position.y, cx);
    }

    /// Move the grabbed thumb to the pointer height `y`.
    fn drag_scrollbar(&mut self, y: Pixels, cx: &mut Context<Self>) {
        let Some(grab) = self.scrollbar_grab else { return };
        let (history, _) = self.term.scroll_extent();
        let track = f32::from(self.scrollbar_track.size.height);
        let Some(thumb) = thumb(track, self.grid_size.1 as usize, history, 0, min_thumb(cx)) else { return };
        let top = f32::from(y - self.scrollbar_track.origin.y) - grab;
        self.term.scroll_to_offset(offset_for_thumb_top(top, track, thumb.len, history));
        self.scrollbar_scrolled(cx);
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.scrollbar_grab.is_some() {
            if event.pressed_button == Some(MouseButton::Left) {
                self.drag_scrollbar(event.position.y, cx);
            } else {
                self.scrollbar_grab = None;
                cx.notify();
            }
            return;
        }
        self.mouse_position = Some(event.position);
        self.update_hovered_link(event.modifiers, cx);
        if self.term.scroll_modes().mouse_report && !event.modifiers.shift && !self.drag.selecting {
            let cell = self.logical_cell_at(event.position).map(|(col, row, _)| (col, row));
            if cell.is_some() && cell != self.drag.reported_cell {
                self.report(MouseAction::Motion(event.pressed_button.and_then(report_button)), event.modifiers, cell);
                self.drag.reported_cell = cell;
            }
            return;
        }
        if !self.drag.selecting || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if let Some((col, row, right_half)) = self.logical_cell_at(event.position) {
            self.term.update_selection(col, row, right_half);
            cx.notify();
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let cell = self.logical_cell_at(event.position).map(|(col, row, _)| (col, row));
        self.end_drag(event.modifiers, cell, cx);
    }

    /// The button came up over another element: a drag that left the pane still ends here.
    fn mouse_up_out(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.end_drag(event.modifiers, None, cx);
    }

    /// Finish the current drag: report the release of a reported press (at `cell`, else the last cell the program
    /// heard about), or end a selection drag.
    fn end_drag(&mut self, modifiers: Modifiers, cell: Option<(u16, u16)>, cx: &mut Context<Self>) {
        if self.scrollbar_grab.take().is_some() {
            cx.notify();
            return;
        }
        if let Some((button, reported_cell)) = self.drag.release() {
            self.report(MouseAction::Release(button), modifiers, cell.or(reported_cell));
            return;
        }
        let click = self.click_cell.take();
        if self.term.selection_text().is_none() {
            self.term.clear_selection();
            if let Some((col, row)) = cell.filter(|cell| click == Some(*cell))
                && let Some(steps) = self.term.cursor_moves_to(col, row)
            {
                self.send_input(&arrow_presses(steps, self.term.key_modes().app_cursor), cx);
            }
        }
        cx.notify();
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(self.metrics.height);
        self.scroll_accum += f32::from(delta.y) / f32::from(self.metrics.height);
        let lines = self.scroll_accum.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_accum -= lines as f32;
        let (col, row) = self.logical_cell_at(event.position).map_or((0, 0), |(col, row, _)| (col, row));
        // A program that ended or is restarting cannot read wheel reports; scroll the history instead.
        let modes = if mouse_reaches_program(&self.state) { self.term.scroll_modes() } else { Default::default() };
        match scroll_action(modes, lines, col, row) {
            ScrollAction::Viewport(n) => {
                self.jump = None;
                self.term.scroll_viewport(n);
                self.scrollbar_scrolled(cx);
            }
            ScrollAction::Send(bytes) => self.write_pty(&bytes),
        }
        cx.notify();
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if input_fate(&self.state) == InputFate::Drop {
            return;
        }
        // Typing over a selection on the prompt replaces it.
        let mut bytes = self.delete_prompt_selection().unwrap_or_default();
        bytes.extend_from_slice(text.as_bytes());
        self.send_input(&bytes, cx);
    }

    // IME composition (pre-edit) is not displayed in M1; only committed text is sent.
    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        _new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (col, row) = self.term.cursor_cell()?;
        let col = self.row_orders.get(row).and_then(Option::as_ref).map_or(col, |order| order.visual(col));
        Some(Bounds::new(
            point(
                element_bounds.origin.x + self.metrics.width * col as f32,
                element_bounds.origin.y + self.metrics.height * row as f32,
            ),
            size(self.metrics.width, self.metrics.height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        let terminal_font = cx.global::<SettingsStore>().settings().terminal_font.clone();
        let font = pane_font(Some(installed_family(terminal_font.family.as_deref(), installed_families(cx))));
        let font_size = px(terminal_font.size);
        let metrics = Self::cell_metrics(&font, font_size, terminal_font.line_height, window);
        if let Some((start, at)) = self.jump {
            // The chip's scroll to the bottom: one step per frame, the thumb staying in its active color.
            let elapsed = at.elapsed().as_millis() as u64;
            self.term.scroll_to_offset(jump_offset(start, elapsed));
            if elapsed >= JUMP_MS {
                self.jump = None;
            } else {
                window.request_animation_frame();
            }
            self.scrollbar_scrolled(cx);
        }
        let mut snapshot = self.term.snapshot();
        if let Some((row, cols)) = &self.hovered_link
            && let Some(cells) = snapshot.rows.get_mut(*row as usize)
        {
            for cell in cells.iter_mut().take(cols.end).skip(cols.start) {
                cell.style.underline = true;
            }
        }
        self.row_orders = to_display(&mut snapshot, cx.global::<SettingsStore>().settings().window.right_to_left);
        let hovering_link = self.hovered_link.is_some();
        let focused = self.focus_handle.is_focused(window);
        let background = to_hsla(snapshot.background);
        let layout_view = cx.entity();
        let input_view = cx.entity();
        let track_view = cx.entity();
        // Shell panes always have the bar; Claude panes when `scrollbar.claude` is on.
        let show_bar = !self.launch.is_claude() || cx.global::<SettingsStore>().settings().scrollbar.claude;
        let (history, offset) = self.term.scroll_extent();
        let thumb = thumb(
            f32::from(self.scrollbar_track.size.height),
            self.grid_size.1 as usize,
            history,
            offset,
            min_thumb(cx),
        )
        .filter(|_| show_bar && !self.term.alt_screen());
        // The chip shows while scrolled up, until it has faded out after the last movement (it stays during a drag).
        let chip = if self.scrollbar_grab.is_some() {
            1.0
        } else {
            self.scrollbar_moved.map_or(0.0, |at| chip_opacity(at.elapsed().as_millis() as u64))
        };
        if chip > 0.0 && chip < 1.0 {
            window.request_animation_frame();
        }
        let jump_to_bottom = thumb.is_some() && offset > 0 && chip > 0.0;
        // The thumb is in the accent color while the view moves and fades to grey once it rests.
        let level = if self.scrollbar_grab.is_some() {
            1.0
        } else {
            self.scrollbar_moved.map_or(0.0, |at| activity(at.elapsed().as_millis() as u64))
        };
        if level > 0.0 && level < 1.0 {
            window.request_animation_frame();
        }
        let mut thumb_color = rgb(mix(deepen(theme.tokens.accent, 0.62), theme.tokens.muted, level));
        thumb_color.a = 0.45 + 0.5 * level;
        let focus = self.focus_handle.clone();
        div()
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::zoom_reset))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up_out))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::mouse_up_out))
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::mouse_up_out))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .on_modifiers_changed(cx.listener(Self::modifiers_changed))
            .when(hovering_link, |this| this.cursor_pointer())
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(background)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .w_full()
                    .flex()
                    .flex_row()
                    .child(
                        canvas(
                            move |bounds, _window, cx| {
                                layout_view.update(cx, |view, cx| view.layout(bounds, metrics, cx));
                            },
                            move |bounds, (), window, cx| {
                                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                                    paint_grid(
                                        &snapshot,
                                        bounds.origin,
                                        metrics,
                                        &font,
                                        font_size,
                                        focused,
                                        window,
                                        cx,
                                    );
                                });
                                window.handle_input(&focus, ElementInputHandler::new(bounds, input_view), cx);
                            },
                        )
                        .flex_1()
                        .h_full(),
                    )
                    // The scrollbar column is reserved whenever the pane has a bar, so the grid does not resize when
                    // the thumb appears.
                    .when(show_bar, |row| {
                        row.child(
                            div()
                                .relative()
                                .flex_none()
                                .w(theme.ui(SCROLLBAR_WIDTH))
                                .h_full()
                                .on_mouse_down(MouseButton::Left, cx.listener(Self::scrollbar_down))
                                .child(
                                    canvas(
                                        move |bounds, _window, cx| {
                                            track_view.update(cx, |view, cx| {
                                                if view.scrollbar_track != bounds {
                                                    view.scrollbar_track = bounds;
                                                    cx.notify();
                                                }
                                            });
                                        },
                                        |_, (), _, _| {},
                                    )
                                    .absolute()
                                    .size_full(),
                                )
                                .when_some(thumb, |bar, thumb| {
                                    bar.child(
                                        div()
                                            .absolute()
                                            .top(px(thumb.top))
                                            .left(theme.ui(2.))
                                            .w(theme.ui(SCROLLBAR_WIDTH - 4.))
                                            .h(px(thumb.len))
                                            .rounded(theme.ui((SCROLLBAR_WIDTH - 4.) / 2.))
                                            .bg(thumb_color),
                                    )
                                }),
                        )
                    })
                    .when(jump_to_bottom, |row| {
                        row.child(
                            div()
                                .absolute()
                                .left_0()
                                .right(theme.ui(SCROLLBAR_WIDTH))
                                .bottom(theme.ui(14.))
                                .flex()
                                .justify_center()
                                .child(
                                    div()
                                        .opacity(chip)
                                        .h(theme.ui(28.))
                                        .px(theme.ui(12.))
                                        .flex()
                                        .items_center()
                                        .gap(theme.ui(6.))
                                        .rounded(theme.ui(14.))
                                        .bg(rgb(theme.tokens.raised))
                                        .border_1()
                                        .border_color(rgb(theme.tokens.accent))
                                        .text_color(rgb(theme.tokens.accent))
                                        .font_family(UI_FONT)
                                        .text_xs()
                                        .cursor_pointer()
                                        .on_mouse_down(MouseButton::Left, cx.listener(Self::jump_to_bottom))
                                        .child(
                                            svg()
                                                .path(ARROW_DOWN_ICON)
                                                .size(theme.ui(12.))
                                                .text_color(rgb(theme.tokens.accent)),
                                        )
                                        .child("Jump to bottom"),
                                ),
                        )
                    }),
            )
            .when_some(self.host_card.clone(), |this, card| {
                // The host is not usable: its state, in the middle of the pane.
                this.child(
                    div().absolute().size_full().flex().items_center().justify_center().p(theme.ui(16.)).child(
                        div()
                            .max_w(theme.ui(320.))
                            .px(theme.ui(20.))
                            .py(theme.ui(18.))
                            .flex()
                            .flex_col()
                            .gap(theme.ui(14.))
                            .rounded(px(RADIUS))
                            .bg(rgb(theme.tokens.raised))
                            .border_1()
                            .border_color(rgb(theme.tokens.line))
                            .shadow_lg()
                            .text_color(rgb(theme.tokens.text))
                            .font_family(UI_FONT)
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(card.title))
                            .child(div().text_xs().text_color(rgb(theme.tokens.muted)).child(card.detail))
                            .child(
                                div()
                                    .flex()
                                    .gap(theme.ui(8.))
                                    .when_some(card.button, |row, label| {
                                        row.child(banner_button(
                                            "host-action",
                                            label,
                                            true,
                                            &theme,
                                            cx.listener(|_, _, _, cx| cx.emit(PaneEvent::HostAction)),
                                        ))
                                    })
                                    .child(banner_button(
                                        "host-close",
                                        "Close",
                                        false,
                                        &theme,
                                        cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Close)),
                                    )),
                            ),
                    ),
                )
            })
            .when_some(self.banner(), |this, (text, actions)| {
                let rerun = if self.launch.is_claude() { "Continue" } else { "Restart" };
                // A message in the middle of the pane, over the (still visible) terminal text.
                this.child(
                    div().absolute().size_full().flex().items_center().justify_center().p(theme.ui(16.)).child(
                        div()
                            .max_w(theme.ui(320.))
                            .px(theme.ui(20.))
                            .py(theme.ui(18.))
                            .flex()
                            .flex_col()
                            .gap(theme.ui(14.))
                            .rounded(px(RADIUS))
                            .bg(rgb(theme.tokens.raised))
                            .border_1()
                            .border_color(rgb(theme.tokens.line))
                            .shadow_lg()
                            .text_color(rgb(theme.tokens.text))
                            .font_family(UI_FONT)
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(text))
                            .when(actions, |card| {
                                card.child(
                                    div()
                                        .flex()
                                        .gap(theme.ui(8.))
                                        .child(banner_button(
                                            "rerun",
                                            rerun,
                                            true,
                                            &theme,
                                            cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Rerun)),
                                        ))
                                        .child(banner_button(
                                            "close",
                                            "Close",
                                            false,
                                            &theme,
                                            cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Close)),
                                        )),
                                )
                            }),
                    ),
                )
            })
    }
}

/// The resize to send when attaching: the laid-out size, since a spawn request may have used a size
/// from before the first layout. Before any layout (`cell` unknown) the daemon's size stands.
fn resize_on_attach(grid: (u16, u16), cell: (u16, u16)) -> Option<(u16, u16, u16, u16)> {
    (cell != (0, 0)).then_some((grid.0, grid.1, cell.0, cell.1))
}

fn banner_button(
    id: &'static str,
    label: &'static str,
    primary: bool,
    theme: &Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex_none()
        .h(theme.ui(32.))
        .px(theme.ui(16.))
        .flex()
        .items_center()
        .rounded_full()
        .cursor_pointer()
        .map(|button| {
            if primary {
                button
                    .bg(rgb(theme.tokens.accent))
                    .text_color(rgb(theme.tokens.on_accent))
                    .font_weight(FontWeight::SEMIBOLD)
            } else {
                button
                    .border_1()
                    .border_color(rgb(theme.tokens.line))
                    .hover(|style| style.bg(rgb(theme.tokens.surface)))
            }
        })
        .child(label)
        .on_click(on_click)
}

fn report_button(button: MouseButton) -> Option<mouse::Button> {
    match button {
        MouseButton::Left => Some(mouse::Button::Left),
        MouseButton::Middle => Some(mouse::Button::Middle),
        MouseButton::Right => Some(mouse::Button::Right),
        _ => None,
    }
}

fn report_mods(modifiers: Modifiers) -> MouseMods {
    MouseMods { alt: modifiers.alt, ctrl: modifiers.control }
}

fn link_modifier(modifiers: Modifiers) -> bool {
    modifiers.control || modifiers.platform
}

#[allow(clippy::too_many_arguments)]
fn paint_grid(
    snapshot: &ScreenSnapshot,
    origin: Point<Pixels>,
    m: CellMetrics,
    base_font: &Font,
    font_size: Pixels,
    focused: bool,
    window: &mut Window,
    cx: &mut App,
) {
    for (row_index, row) in snapshot.rows.iter().enumerate() {
        let y = origin.y + m.height * row_index as f32;
        for span in row_spans(row) {
            let x = origin.x + m.width * span.start_col as f32;
            if span.style.bg != snapshot.background {
                let bounds = Bounds::new(point(x, y), size(m.width * span.width_cols as f32, m.height));
                window.paint_quad(fill(bounds, to_hsla(span.style.bg)));
            }
            if span.text.trim().is_empty() {
                continue;
            }
            let single = span.width_cols == 1 && !span.text.is_ascii();
            let force_width = (!span.wide && !single).then_some(m.width);
            paint_text(&span.text, &span.style, point(x, y), force_width, single, m, base_font, font_size, window, cx);
        }
    }
    if let Some(cursor) = &snapshot.cursor {
        let cells = if cursor.wide { 2.0 } else { 1.0 };
        let bounds = Bounds::new(
            point(origin.x + m.width * cursor.col as f32, origin.y + m.height * cursor.row as f32),
            size(m.width * cells, m.height),
        );
        let color = to_hsla(snapshot.cursor_color);
        if focused {
            window.paint_quad(fill(bounds, color));
            if cursor.ch != ' ' {
                let style = CellStyle { fg: snapshot.background, ..cursor.style };
                let single = !cursor.wide && !cursor.ch.is_ascii();
                let force_width = (!cursor.wide && !single).then_some(m.width);
                paint_text(
                    &cursor.ch.to_string(),
                    &style,
                    bounds.origin,
                    force_width,
                    single,
                    m,
                    base_font,
                    font_size,
                    window,
                    cx,
                );
            }
        } else {
            window.paint_quad(outline(bounds, color, BorderStyle::Solid));
        }
    }
}

/// `center`: draw a glyph narrower than one cell in the middle of that cell.
#[allow(clippy::too_many_arguments)]
fn paint_text(
    text: &str,
    style: &CellStyle,
    origin: Point<Pixels>,
    force_width: Option<Pixels>,
    center: bool,
    m: CellMetrics,
    base_font: &Font,
    font_size: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let color = to_hsla(style.fg);
    let mut run_font = base_font.clone();
    if style.bold {
        run_font.weight = FontWeight::BOLD;
    }
    if style.italic {
        run_font.style = FontStyle::Italic;
    }
    let run = TextRun {
        len: text.len(),
        font: run_font,
        color,
        background_color: None,
        underline: style.underline.then_some(UnderlineStyle { thickness: px(1.), color: Some(color), wavy: false }),
        strikethrough: style.strikethrough.then_some(StrikethroughStyle { thickness: px(1.), color: Some(color) }),
    };
    let line = window.text_system().shape_line(text.to_string().into(), font_size, &[run], force_width);
    let inset = if center { ((m.width - line.width) / 2.).max(px(0.)) } else { px(0.) };
    if let Err(err) = line.paint(point(origin.x + inset, origin.y), m.height, TextAlign::Left, None, window, cx) {
        tracing::warn!("painting text failed: {err:#}");
    }
}

/// The Text editing action (settings key) bound to `keystroke` in the user's shortcuts, if any.
fn editing_action(keystroke: &gpui::Keystroke, cx: &App) -> Option<&'static str> {
    let settings = cx.global::<SettingsStore>().settings();
    let platform = Platform::current();
    let same = |bound: &gpui::Keystroke| {
        let (a, b) = (&bound.modifiers, &keystroke.modifiers);
        bound.key == keystroke.key && (a.control, a.alt, a.shift, a.platform) == (b.control, b.alt, b.shift, b.platform)
    };
    shortcuts::ACTIONS.iter().filter(|action| action.group == Group::Editing).map(|action| action.id).find(|id| {
        shortcuts::current(id, &settings.shortcuts, platform)
            .and_then(|key| gpui::Keystroke::parse(&key).ok())
            .is_some_and(|bound| same(&bound))
    })
}

/// The first image on the clipboard that the daemon can save: PNG, JPEG, GIF or WebP, within the size cap. A
/// clipboard that also offers text is a text paste, left to the program.
fn image_for_paste(item: &ClipboardItem) -> Option<(protocol::message::ImageFormat, Vec<u8>)> {
    use protocol::message::ImageFormat;
    if item.text().is_some() {
        return None;
    }
    item.entries().iter().find_map(|entry| {
        let ClipboardEntry::Image(image) = entry else { return None };
        let format = match image.format {
            gpui::ImageFormat::Png => ImageFormat::Png,
            gpui::ImageFormat::Jpeg => ImageFormat::Jpeg,
            gpui::ImageFormat::Gif => ImageFormat::Gif,
            gpui::ImageFormat::Webp => ImageFormat::Webp,
            _ => return None,
        };
        image_paste::fits(image.bytes.len()).then(|| (format, image.bytes.clone()))
    })
}

fn to_hsla(color: Rgb) -> Hsla {
    rgb(((color.r as u32) << 16) | ((color.g as u32) << 8) | color.b as u32).into()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use gpui::{Keystroke, Modifiers};

    fn action_for(keystroke: Keystroke) -> Option<&'static str> {
        crate::workspace_view::all_bindings(&Default::default(), app_core::shortcuts::Platform::Linux)
            .into_iter()
            .find(|binding| binding.match_keystrokes(std::slice::from_ref(&keystroke)) == Some(false))
            .map(|binding| binding.action().name())
    }

    /// Shape of the keystrokes GPUI's Linux backend emits: Shift is dropped for symbols and digits
    /// and the shifted character is reported instead (Ctrl+Shift+= arrives as `ctrl-+`).
    fn keystroke(key: &str, shift: bool, key_char: Option<&str>) -> Keystroke {
        Keystroke {
            modifiers: Modifiers { control: true, shift, ..Default::default() },
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    fn image_item(format: gpui::ImageFormat, len: usize) -> ClipboardItem {
        ClipboardItem { entries: vec![ClipboardEntry::Image(gpui::Image { format, bytes: vec![7; len], id: 1 })] }
    }

    #[test]
    fn only_saveable_images_go_to_the_daemon() {
        let (format, bytes) = image_for_paste(&image_item(gpui::ImageFormat::Png, 3)).unwrap();
        assert_eq!((format, bytes), (protocol::message::ImageFormat::Png, vec![7; 3]));
        assert!(image_for_paste(&ClipboardItem::new_string("text".into())).is_none());
        assert!(image_for_paste(&image_item(gpui::ImageFormat::Bmp, 3)).is_none());
        let mut with_text = image_item(gpui::ImageFormat::Png, 3);
        with_text.entries.push(ClipboardEntry::String(gpui::ClipboardString::new("alt text".into())));
        assert!(image_for_paste(&with_text).is_none());
        assert!(image_for_paste(&image_item(gpui::ImageFormat::Png, image_paste::MAX_BYTES + 1)).is_none());
    }

    #[test]
    fn the_pane_font_falls_back_to_fonts_gpui_keeps() {
        let fallbacks = pane_font(None).fallbacks.expect("Linux panes have a fallback chain");
        assert_eq!(fallbacks.fallback_list(), ["Adwaita Mono", "Symbola"]);
    }

    #[test]
    fn a_missing_family_falls_back_to_an_installed_monospace_one() {
        let installed = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        let some = installed(&["DejaVu Sans", "DejaVu Sans Mono", "Fira Code", "Liberation Mono"]);
        assert_eq!(installed_family(Some("Fira Code"), &some), "Fira Code");
        assert_eq!(installed_family(Some("Iosevka"), &some), "DejaVu Sans Mono");
        assert_eq!(installed_family(None, &installed(&["Liberation Mono", FONT_FAMILY])), FONT_FAMILY);
        assert_eq!(installed_family(None, &installed(&["Liberation Mono"])), "Liberation Mono");
        assert_eq!(installed_family(Some("Iosevka"), &installed(&["DejaVu Sans"])), "Iosevka");
    }

    #[test]
    fn input_is_held_while_starting_and_sent_when_live() {
        assert_eq!(input_fate(&PaneState::Starting), InputFate::Hold);
        assert_eq!(input_fate(&PaneState::Live), InputFate::Send);
    }

    #[test]
    fn held_input_is_dropped_when_the_pane_fails() {
        for state in [PaneState::Exited(Some(1)), PaneState::Failed("x".into()), PaneState::Disconnected] {
            assert_eq!(input_fate(&state), InputFate::Drop);
        }
    }

    #[test]
    fn a_reported_press_ends_a_stale_selection_drag() {
        // A drag released outside the pane never sees its mouse-up; the next press must not inherit it.
        let mut drag = Drag { selecting: true, ..Drag::default() };
        drag.reported_press(mouse::Button::Left, (1, 2));
        assert!(!drag.selecting);
        assert_eq!(drag.reporting, Some(mouse::Button::Left));
    }

    #[test]
    fn a_release_anywhere_ends_the_drag_and_names_the_reported_cell() {
        let mut drag = Drag::default();
        drag.reported_press(mouse::Button::Right, (3, 4));
        assert_eq!(drag.release(), Some((mouse::Button::Right, Some((3, 4)))));
        assert_eq!(drag, Drag::default());
        drag.selection_press();
        assert_eq!(drag.release(), None);
        assert!(!drag.selecting);
    }

    #[test]
    fn mouse_reports_only_reach_a_live_process() {
        assert!(mouse_reaches_program(&PaneState::Live));
        for state in [PaneState::Starting, PaneState::Exited(None), PaneState::Disconnected] {
            assert!(!mouse_reaches_program(&state), "{state:?}");
        }
    }

    #[test]
    fn attaching_before_the_first_layout_keeps_the_daemon_size() {
        assert_eq!(resize_on_attach((80, 24), (0, 0)), None);
        assert_eq!(resize_on_attach((120, 40), (8, 18)), Some((120, 40, 8, 18)));
    }

    #[test]
    fn zoom_is_on_plain_ctrl_and_ctrl_underscore_reaches_the_pane() {
        assert_eq!(action_for(keystroke("=", false, Some("="))), Some("terminal::ZoomIn"));
        assert_eq!(action_for(keystroke("+", false, Some("+"))), Some("terminal::ZoomIn"));
        assert_eq!(action_for(keystroke("-", false, Some("-"))), Some("terminal::ZoomOut"));
        assert_eq!(action_for(keystroke("0", false, Some("0"))), Some("terminal::ZoomReset"));
        // Ctrl+Shift+- arrives as ctrl-_ on Linux: readline/Claude undo.
        assert_eq!(action_for(keystroke("_", false, Some("_"))), None);
    }

    #[test]
    fn clipboard_shortcuts_keep_shift_for_letters() {
        assert_eq!(action_for(keystroke("c", true, None)), Some("terminal::Copy"));
        assert_eq!(action_for(keystroke("v", true, None)), Some("terminal::Paste"));
    }
}
