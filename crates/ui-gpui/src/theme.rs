//! The "Mist" look: dark slate surfaces, a light-blue accent, pastel fills that say what a session needs, rounded
//! corners, and the interface font (Figtree, embedded; the terminal keeps its own font).

use std::borrow::Cow;
use std::time::Duration;

use app_core::session_tree::Tone;
use std::path::Path;

use app_core::settings::{ProjectColors, StatusFills};
use app_core::theme_colors::{Preset, Tokens, ToneColors, tone_colors};
use app_core::workspace::DirBadge;
use gpui::{
    Animation, AnimationExt, AnyElement, App, Div, ElementId, FontWeight, Global, Hsla, IntoElement, ParentElement,
    Pixels, Styled, Svg, div, hsla, linear_color_stop, linear_gradient, px, relative, rgb, rgba, svg,
};

use crate::assets::{CONTAINER_ICON, PIN_ICON, SSH_ICON};

/// The colors and sizes views draw with, a GPUI global replaced whenever the settings change.
#[derive(Clone)]
pub(crate) struct Theme {
    /// Which preset is drawn.
    pub preset: Preset,
    /// The preset's app colors.
    pub tokens: Tokens,
    /// The user's status fills that are readable on this preset.
    pub fills: StatusFills,
    /// The `selection` setting.
    pub selection: Option<u32>,
    /// `uiFontSize / 14`.
    pub ui_scale: f32,
    /// The `projectColors` setting.
    pub project_colors: ProjectColors,
}

impl Global for Theme {}

impl Theme {
    /// The colors of a session row, tab or pane header in `tone`.
    pub fn tone(&self, tone: Tone) -> ToneColors {
        tone_colors(self.preset, &self.fills, tone)
    }

    /// The color of the project at `root` on this theme, if project colors are on.
    pub fn project(&self, root: &Path) -> Option<u32> {
        self.project_colors.color_of(root).map(|color| color.value(self.preset))
    }

    /// The color of the ring around the selected row: the `selection` setting, else the accent.
    pub fn selection(&self) -> u32 {
        self.selection.unwrap_or(self.tokens.accent)
    }

    /// `size` scaled with the interface font size.
    pub fn ui(&self, size: f32) -> Pixels {
        px(size * self.ui_scale)
    }
}

/// A project's side line: 4 px wide, rounded, inset 4 px from the left edge and 6 px from the top and bottom of
/// the (relatively positioned) row, tab or header it is a child of.
pub(crate) fn side_line(color: u32, theme: &Theme) -> Div {
    div()
        .absolute()
        .left(theme.ui(4.))
        .top(theme.ui(6.))
        .bottom(theme.ui(6.))
        .w(theme.ui(4.))
        .rounded(px(2.))
        .bg(rgb(color))
}

/// A pane's own color: a 6 px wide rounded bar, 8 px from the top and bottom of the (relatively positioned) row,
/// header or chip it is a child of, `left` px from its left edge.
pub(crate) fn pane_marker(color: u32, left: f32, theme: &Theme) -> Div {
    div()
        .absolute()
        .left(theme.ui(left))
        .top(theme.ui(8.))
        .bottom(theme.ui(8.))
        .w(theme.ui(6.))
        .rounded(px(3.))
        .bg(rgb(color))
}

/// The theme global.
pub(crate) fn theme(cx: &App) -> &Theme {
    cx.global::<Theme>()
}

/// Corner radii, in pixels: chips and small buttons, rows and tabs, panels and panes.
pub(crate) const RADIUS_SMALL: f32 = 8.0;
pub(crate) const RADIUS: f32 = 12.0;
pub(crate) const RADIUS_LARGE: f32 = 16.0;

/// Interface font family (embedded, see [`load_fonts`]).
pub(crate) const UI_FONT: &str = "Figtree";

const FONTS: [&[u8]; 5] = [
    include_bytes!("../assets/fonts/Figtree-Regular.ttf"),
    include_bytes!("../assets/fonts/Figtree-Italic.ttf"),
    include_bytes!("../assets/fonts/Figtree-Medium.ttf"),
    include_bytes!("../assets/fonts/Figtree-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Figtree-Bold.ttf"),
];

/// The dev container icon, in `ink` (slightly softened).
pub(crate) fn container_icon(ink: u32, theme: &Theme) -> Svg {
    svg().path(CONTAINER_ICON).flex_none().size(theme.ui(14.)).text_color(rgba((ink << 8) | 0xd9))
}

/// The SSH host icon, in `ink` (slightly softened). Marks a session that runs on another machine.
pub(crate) fn ssh_icon(ink: u32, theme: &Theme) -> Svg {
    svg().path(SSH_ICON).flex_none().size(theme.ui(14.)).text_color(rgba((ink << 8) | 0xd9))
}

/// The pin of a row marked unread, in `dot`.
pub(crate) fn pin_icon(dot: u32, theme: &Theme) -> Svg {
    svg().path(PIN_ICON).flex_none().size(theme.ui(12.)).text_color(rgb(dot))
}

/// A config dir's badge: its letter on its color. Shown only with two or more config dirs.
pub(crate) fn dir_badge(badge: &DirBadge, theme: &Theme) -> Div {
    div()
        .flex_none()
        .size(theme.ui(16.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .bg(rgb(badge.color.value(theme.preset)))
        .text_color(rgb(theme.tokens.ground))
        .text_size(theme.ui(10.))
        .font_weight(FontWeight::BOLD)
        .child(badge.letter.clone())
}

/// How much of the text color (alpha, of 255) tints a project's group box in the sidebar: about 11%.
const GROUP_TINT_ALPHA: u32 = 0x1c;

/// The color laid over a project's group box in the sidebar: `text` at [`GROUP_TINT_ALPHA`], as `rgba` bits.
pub(crate) fn group_tint(text: u32) -> u32 {
    (text << 8) | GROUP_TINT_ALPHA
}

/// Width of the Working glow, as a fraction of the row.
pub(crate) const SWEEP_BAND: f32 = 0.45;
/// One pass of the Working glow.
const SWEEP_PERIOD: Duration = Duration::from_millis(2200);
/// Redraws per second while a glow runs: each one redraws the whole window, so this sets its CPU cost.
const SWEEP_FPS: f32 = 15.;

/// Whether a row of `tone` shows the Working glow: the setting is on and the window is active (an inactive window
/// requests no animation frames at all).
pub(crate) fn sweeping(tone: Tone, enabled: bool, window_active: bool) -> bool {
    tone == Tone::Working && enabled && window_active
}

/// Where the glow is at `progress` (0..1 of a pass): the band's left edge as a fraction of the track (it enters
/// from the left and leaves at the right) and its strength, which fades in and out at the ends so the clip at the
/// track's edges never shows.
pub(crate) fn sweep_band(progress: f32) -> (f32, f32) {
    band_at(progress, SWEEP_BAND)
}

/// [`sweep_band`] for a band `band` of the track wide.
fn band_at(progress: f32, band: f32) -> (f32, f32) {
    let progress = progress.clamp(0., 1.);
    (progress * (1. + band) - band, (std::f32::consts::PI * progress).sin())
}

/// Width of the glow over a project's pill, as a fraction of the pill.
const PILL_BAND: f32 = 0.40;
/// How strong that glow is at its middle: it is drawn in the status color, over the pale fill.
const PILL_PEAK: f32 = 0.9;

/// The Working glow of a project's pill: a band in `color` (the status color) over the pill's pale fill. Put it
/// first inside a relatively positioned pill; `inset` is the pill's corner radius. It runs on the clock of every
/// other glow.
pub(crate) fn pill_sweep(id: impl Into<ElementId>, inset: Pixels, color: u32) -> impl IntoElement {
    let tint: Hsla = rgb(color).into();
    let glow = move |alpha: f32| Hsla { a: alpha, ..tint };
    div().absolute().top_0().bottom_0().left(inset).right(inset).overflow_hidden().child(
        div().absolute().top_0().bottom_0().w(relative(PILL_BAND)).flex().with_animation(
            id,
            Animation::new(SWEEP_PERIOD).repeat_synced().with_max_fps(SWEEP_FPS),
            move |band, progress| {
                let (left, strength) = band_at(progress, PILL_BAND);
                let alpha = PILL_PEAK * strength;
                band.left(relative(left))
                    .child(div().h_full().flex_1().bg(linear_gradient(
                        90.,
                        linear_color_stop(glow(0.), 0.),
                        linear_color_stop(glow(alpha), 1.),
                    )))
                    .child(div().h_full().flex_1().bg(linear_gradient(
                        90.,
                        linear_color_stop(glow(alpha), 0.),
                        linear_color_stop(glow(0.), 1.),
                    )))
            },
        ),
    )
}

/// The Working glow: put it first inside a relatively positioned, filled row, tab or header. It runs on a track
/// inset by `inset` (the element's corner radius) from both ends, because GPUI clips to rectangles, not rounded
/// corners. All glows share one clock.
pub(crate) fn working_sweep(id: impl Into<ElementId>, inset: Pixels, theme: &Theme) -> impl IntoElement {
    let peak = if theme.preset == Preset::MistLight { 0.65 } else { 0.45 };
    let glow = |alpha: f32| hsla(0., 0., 1., alpha);
    div().absolute().top_0().bottom_0().left(inset).right(inset).overflow_hidden().child(
        div().absolute().top_0().bottom_0().w(relative(SWEEP_BAND)).flex().with_animation(
            id,
            Animation::new(SWEEP_PERIOD).repeat_synced().with_max_fps(SWEEP_FPS),
            move |band, progress| {
                let (left, strength) = sweep_band(progress);
                let alpha = peak * strength;
                band.left(relative(left))
                    .child(div().h_full().flex_1().bg(linear_gradient(
                        90.,
                        linear_color_stop(glow(0.), 0.),
                        linear_color_stop(glow(alpha), 1.),
                    )))
                    .child(div().h_full().flex_1().bg(linear_gradient(
                        90.,
                        linear_color_stop(glow(alpha), 0.),
                        linear_color_stop(glow(0.), 1.),
                    )))
            },
        ),
    )
}

/// How long a scrim fades in.
pub(crate) const FADE_SCRIM: Duration = Duration::from_millis(250);
/// How long a card (an overlay's panel, a menu, a hover card) fades in.
pub(crate) const FADE_CARD: Duration = Duration::from_millis(250);
/// How far, in pixels, a card starts below its place and rises.
pub(crate) const FADE_RISE: f32 = 4.0;

/// What a fade-in is applied to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FadeKind {
    /// A full-window backdrop: fades, does not move.
    Scrim,
    /// A panel, menu or hover card: fades and rises `FADE_RISE` px.
    Card,
}

/// The look of a fade-in at `progress` (0..1, already eased): its opacity and how many pixels it still has to rise.
pub(crate) fn fade_progress(progress: f32) -> (f32, f32) {
    let progress = progress.clamp(0., 1.);
    (progress, FADE_RISE * (1. - progress))
}

/// Fade `element` in once, when it first appears (`id` must be stable while it is open, so a re-render does not
/// replay it). A `Card` also rises into place. With `enabled` off the element is returned as it is.
pub(crate) fn fade_in<E: IntoElement + Styled + 'static>(
    element: E,
    id: impl Into<ElementId>,
    kind: FadeKind,
    enabled: bool,
) -> AnyElement {
    if !enabled {
        return element.into_any_element();
    }
    let duration = if kind == FadeKind::Card { FADE_CARD } else { FADE_SCRIM };
    element
        .with_animation(
            id,
            Animation::new(duration).with_easing(app_core::split_tree::ease_out_cubic),
            move |element, progress| {
                let (opacity, rise) = fade_progress(progress);
                let element = element.opacity(opacity);
                if kind == FadeKind::Card { element.relative().top(px(rise)) } else { element }
            },
        )
        .into_any_element()
}

/// Make the embedded interface font available.
pub(crate) fn load_fonts(cx: &App) {
    if let Err(error) = cx.text_system().add_fonts(embedded_fonts()) {
        tracing::warn!("interface font not loaded: {error:#}");
    }
}

fn embedded_fonts() -> Vec<Cow<'static, [u8]>> {
    FONTS.iter().map(|font| Cow::Borrowed(*font)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_core::theme_colors::{contrast, terminal_palette, tokens};

    const PRESETS: [Preset; 2] = [Preset::Mist, Preset::MistLight];
    const TONES: [Tone; 7] =
        [Tone::NeedsInput, Tone::Working, Tone::Done, Tone::Unread, Tone::Idle, Tone::Elsewhere, Tone::Closed];

    #[test]
    fn only_a_working_tone_in_an_active_window_with_the_setting_on_sweeps() {
        assert!(sweeping(Tone::Working, true, true));
        assert!(!sweeping(Tone::Working, false, true), "setting off");
        assert!(!sweeping(Tone::Working, true, false), "window inactive");
        for tone in [Tone::NeedsInput, Tone::Done, Tone::Unread, Tone::Idle, Tone::Closed, Tone::Elsewhere] {
            assert!(!sweeping(tone, true, true), "{tone:?}");
        }
    }

    #[test]
    fn the_group_tint_is_the_text_color_at_about_eleven_percent() {
        assert_eq!(group_tint(0xe7eaf3), 0xe7ea_f31c);
        assert_eq!(group_tint(0x1f2533) & 0xff, 0x1c);
    }

    #[test]
    fn fade_progress_starts_transparent_and_raised_and_ends_opaque_and_in_place() {
        assert_eq!(fade_progress(0.), (0., FADE_RISE));
        assert_eq!(fade_progress(1.), (1., 0.));
        let mut last = fade_progress(0.);
        for step in 1..=10 {
            let now = fade_progress(step as f32 / 10.);
            assert!(now.0 >= last.0 && now.1 <= last.1, "monotonic at step {step}");
            last = now;
        }
    }

    #[test]
    fn the_band_enters_left_leaves_right_and_fades_at_both_ends() {
        let (start, faint) = sweep_band(0.);
        assert_eq!(start, -SWEEP_BAND);
        assert!(faint.abs() < 1e-6);
        let (middle, full) = sweep_band(0.5);
        assert!((middle - (1. - SWEEP_BAND) / 2.).abs() < 1e-6, "centred halfway through");
        assert!((full - 1.).abs() < 1e-6);
        let (end, faint) = sweep_band(1.);
        assert!((end - 1.).abs() < 1e-6);
        assert!(faint.abs() < 1e-6);
    }

    fn theme(preset: Preset) -> Theme {
        Theme {
            preset,
            tokens: tokens(preset),
            fills: StatusFills::default(),
            selection: None,
            ui_scale: 1.0,
            project_colors: ProjectColors::default(),
        }
    }

    #[test]
    fn row_text_is_readable_on_every_tone() {
        for preset in PRESETS {
            let theme = theme(preset);
            let t = theme.tokens;
            for tone in TONES {
                let colors = theme.tone(tone);
                let ratio = contrast(colors.ink, colors.fill.unwrap_or(t.surface));
                assert!(ratio >= 4.5, "{preset:?} {tone:?}: {ratio:.2}");
            }
            assert!(contrast(t.text, t.surface) >= 4.5 && contrast(t.muted, t.surface) >= 4.5);
            assert!(contrast(t.on_accent, t.accent) >= 4.5, "{preset:?}");
        }
    }

    #[test]
    fn states_that_ask_for_attention_stand_out_from_quiet_ones() {
        // On Mist the pastel fills are far lighter than the surface; quiet states stay close to it.
        let theme = theme(Preset::Mist);
        for tone in [Tone::NeedsInput, Tone::Working, Tone::Done] {
            assert!(contrast(theme.tone(tone).fill.unwrap(), theme.tokens.surface) >= 7.0, "{tone:?}");
        }
        assert!(contrast(theme.tone(Tone::Idle).fill.unwrap(), theme.tokens.surface) < 1.5);
    }

    #[test]
    fn pane_cards_match_the_terminal_background() {
        for preset in PRESETS {
            assert_eq!(
                app_core::snapshot::Rgb::hex(tokens(preset).terminal),
                terminal_palette(preset).background,
                "{preset:?}"
            );
        }
    }

    #[test]
    fn project_colors_follow_the_preset_and_the_switch() {
        let root = Path::new("/r/app");
        let mut dark = theme(Preset::Mist);
        let light = Theme { preset: Preset::MistLight, ..dark.clone() };
        let color = app_core::theme_colors::ProjectColor::auto(root);
        assert_eq!(dark.project(root), Some(color.value(Preset::Mist)));
        assert_eq!(light.project(root), Some(color.value(Preset::MistLight)));
        dark.project_colors.enabled = false;
        assert_eq!(dark.project(root), None);
    }

    #[test]
    fn ui_sizes_scale_with_the_interface_font() {
        let theme = Theme { ui_scale: 1.5, ..theme(Preset::Mist) };
        assert_eq!(theme.ui(48.0), px(72.0));
    }

    // macOS only creates its platform on the main thread, which a test thread is not; on Windows the font is not
    // listed after loading (not handled yet).
    #[cfg(not(any(target_os = "macos", windows)))]
    #[test]
    fn the_embedded_interface_font_loads() {
        let text_system = gpui_platform::current_platform(true).text_system();
        text_system.add_fonts(embedded_fonts()).unwrap();
        assert!(text_system.all_font_names().iter().any(|name| name == UI_FONT));
    }
}
