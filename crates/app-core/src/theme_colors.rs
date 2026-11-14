//! Color presets (Mist, Mist light) and status colors, including those derived from a user-chosen fill.

use std::path::Path;

use crate::palette::{self, Palette};
use crate::session_tree::Tone;
use crate::settings::{StatusFills, ThemeChoice, fill_alpha};

/// Which preset is drawn (`System` resolves to one of these).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    /// Dark slate surfaces.
    Mist,
    /// Light surfaces.
    MistLight,
}

/// A preset's app colors, `0xRRGGBB`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tokens {
    /// Behind everything: the window and the gaps between panes.
    pub ground: u32,
    /// Sidebar and title bar.
    pub surface: u32,
    /// Fields, buttons, the active segment of a switch.
    pub raised: u32,
    /// Hairlines and outlines.
    pub line: u32,
    /// Body text.
    pub text: u32,
    /// Secondary text.
    pub muted: u32,
    /// Focus rings, the caret, primary buttons.
    pub accent: u32,
    /// Text on an accent fill.
    pub on_accent: u32,
    /// Inside a pane: the terminal's default background.
    pub terminal: u32,
    /// Destructive controls: the pane close button.
    pub danger: u32,
}

/// Colors of a session row, tab or pane header in a given tone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ToneColors {
    /// Background (`None` = the surface shows through).
    pub fill: Option<u32>,
    /// Text.
    pub ink: u32,
    /// The status dot.
    pub dot: u32,
}

const MIST: Tokens = Tokens {
    ground: 0x171b24,
    surface: 0x1d2230,
    raised: 0x272e3e,
    line: 0x2c3344,
    text: 0xe7eaf3,
    muted: 0x97a1b6,
    accent: 0x9ecbff,
    on_accent: 0x0f2440,
    terminal: 0x141821,
    danger: 0xe06c75,
};

const MIST_LIGHT: Tokens = Tokens {
    ground: 0xeef1f6,
    surface: 0xffffff,
    raised: 0xf3f5f9,
    line: 0xd6dce7,
    text: 0x1c2333,
    muted: 0x566073,
    accent: 0x2f74c4,
    on_accent: 0xffffff,
    terminal: 0xf8f9fc,
    danger: 0xc0392b,
};

/// The app colors of `preset`.
pub fn tokens(preset: Preset) -> Tokens {
    match preset {
        Preset::Mist => MIST,
        Preset::MistLight => MIST_LIGHT,
    }
}

/// The terminal palette of a preset (`palette::DARK` / `palette::LIGHT`).
pub fn terminal_palette(preset: Preset) -> Palette {
    match preset {
        Preset::Mist => palette::DARK,
        Preset::MistLight => palette::LIGHT,
    }
}

/// Resolve a choice; `system_dark` is the desktop's appearance.
pub fn resolve(choice: ThemeChoice, system_dark: bool) -> Preset {
    match choice {
        ThemeChoice::Mist => Preset::Mist,
        ThemeChoice::MistLight => Preset::MistLight,
        ThemeChoice::System if system_dark => Preset::Mist,
        ThemeChoice::System => Preset::MistLight,
    }
}

/// A project's color: one of eight, each with a value per theme preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProjectColor {
    /// Blue.
    Blue,
    /// Amber.
    Amber,
    /// Teal.
    Teal,
    /// Violet.
    Violet,
    /// Rose.
    Rose,
    /// Lime.
    Lime,
    /// Orange.
    Orange,
    /// Pink.
    Pink,
}

impl ProjectColor {
    /// Every color, in palette order.
    pub const ALL: [ProjectColor; 8] =
        [Self::Blue, Self::Amber, Self::Teal, Self::Violet, Self::Rose, Self::Lime, Self::Orange, Self::Pink];

    /// Its name in settings.json.
    pub fn name(self) -> &'static str {
        match self {
            Self::Blue => "blue",
            Self::Amber => "amber",
            Self::Teal => "teal",
            Self::Violet => "violet",
            Self::Rose => "rose",
            Self::Lime => "lime",
            Self::Orange => "orange",
            Self::Pink => "pink",
        }
    }

    /// The color called `name` (lower case).
    pub fn from_name(name: &str) -> Option<ProjectColor> {
        Self::ALL.into_iter().find(|color| color.name() == name)
    }

    /// Its value on `preset`'s surfaces.
    pub fn value(self, preset: Preset) -> u32 {
        let (mist, light) = match self {
            Self::Blue => (0x5b9cf0, 0x2f6fc4),
            Self::Amber => (0xe5b65c, 0xa8740f),
            Self::Teal => (0x4fc4b4, 0x16857a),
            Self::Violet => (0xb49cf5, 0x6f52c9),
            Self::Rose => (0xec8a96, 0xc24156),
            Self::Lime => (0xa9cc5c, 0x5f7f17),
            Self::Orange => (0xef9a5a, 0xb85a1b),
            Self::Pink => (0xe48ad0, 0xa8378f),
        };
        match preset {
            Preset::Mist => mist,
            Preset::MistLight => light,
        }
    }

    /// The automatic color of the project at `root`: FNV-1a 64 of the path's bytes, mod 8 (stable across runs).
    pub fn auto(root: &Path) -> ProjectColor {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in root.as_os_str().as_encoded_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self::ALL[(hash % 8) as usize]
    }
}

/// A color the user picked: one of the eight palette colors, or any `#rrggbb`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tint {
    /// A palette color, with a value per preset.
    Palette(ProjectColor),
    /// A custom color, the same on every preset.
    Custom(u32),
}

impl serde::Serialize for Tint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_setting())
    }
}

impl<'de> serde::Deserialize<'de> for Tint {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Tint, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        Tint::parse(&text).ok_or_else(|| serde::de::Error::custom(format!("not a color: {text}")))
    }
}

impl From<ProjectColor> for Tint {
    fn from(color: ProjectColor) -> Tint {
        Tint::Palette(color)
    }
}

impl Tint {
    /// The tint a setting names: a palette name (lower case) or `#rrggbb`.
    pub fn parse(text: &str) -> Option<Tint> {
        if text.starts_with('#') {
            return parse_hex(text).map(Tint::Custom);
        }
        ProjectColor::from_name(text).map(Tint::Palette)
    }

    /// How it is saved: the palette name, or `#rrggbb` in lower case.
    pub fn to_setting(self) -> String {
        match self {
            Tint::Palette(color) => color.name().to_string(),
            Tint::Custom(rgb) => format!("#{rgb:06x}"),
        }
    }

    /// Its value on `preset`'s surfaces.
    pub fn value(self, preset: Preset) -> u32 {
        match self {
            Tint::Palette(color) => color.value(preset),
            Tint::Custom(rgb) => rgb,
        }
    }
}

/// `#rrggbb` or `rrggbb` (any case, exactly six hex digits) as `0xRRGGBB`.
pub fn parse_hex(text: &str) -> Option<u32> {
    let digits = text.strip_prefix('#').unwrap_or(text);
    if digits.len() == 6 && digits.chars().all(|c| c.is_ascii_hexdigit()) {
        u32::from_str_radix(digits, 16).ok()
    } else {
        None
    }
}

fn default_tone_colors(preset: Preset, tone: Tone) -> ToneColors {
    let t = tokens(preset);
    let light = preset == Preset::MistLight;
    let (fill, ink, dot) = match tone {
        Tone::NeedsInput => (Some(0xf7c9a3), 0x3b2414, 0xc9773d),
        Tone::Working => (Some(0xb9d5f6), 0x132a43, 0x4f8fd6),
        Tone::Done | Tone::Unread => (Some(0xc1e5cd), 0x143322, 0x4c9e6b),
        Tone::Idle if light => (Some(0xe4e8ef), 0x2a3242, t.muted),
        Tone::Idle => (Some(0x2f3647), 0xd5dae6, t.muted),
        Tone::Elsewhere if light => (None, 0x3a4356, t.muted),
        Tone::Elsewhere => (None, 0xc9d0de, t.muted),
        Tone::Closed if light => (None, t.muted, 0x8a93a5),
        Tone::Closed => (None, t.muted, 0x555e70),
    };
    ToneColors { fill, ink, dot }
}

/// The colors of `tone` in `preset`, with the user's fills applied (via [`derive`]) where set and readable.
pub fn tone_colors(preset: Preset, fills: &StatusFills, tone: Tone) -> ToneColors {
    let custom = match tone {
        Tone::NeedsInput => fills.needs_input,
        Tone::Working => fills.working,
        Tone::Done => fills.done,
        Tone::Idle => fills.idle,
        Tone::Unread => fills.unread,
        Tone::Elsewhere | Tone::Closed => None,
    };
    let t = tokens(preset);
    let fallback = || match tone {
        Tone::Unread => unread_colors(preset, fills),
        _ => default_tone_colors(preset, tone),
    };
    match custom {
        Some(fill) if usable_fill(fill, &t) => match fill_alpha(fill) {
            255 => derive(fill, &t),
            0 => ToneColors { fill: None, ink: t.text, dot: fallback().dot },
            _ => {
                let shown = effective_fill(fill, &t);
                ToneColors { fill: Some(shown), ink: derive(shown, &t).ink, dot: derive(fill & 0xff_ffff, &t).dot }
            }
        },
        _ => fallback(),
    }
}

/// The opaque color a status `fill` shows: blended over the surface by its opacity (see
/// [`crate::settings::with_alpha`]).
pub fn effective_fill(fill: u32, tokens: &Tokens) -> u32 {
    mix(fill & 0xff_ffff, tokens.surface, f32::from(fill_alpha(fill)) / 255.0)
}

/// Whether a status `fill` may be used: no fill at all, or text readable on the color it shows.
pub fn usable_fill(fill: u32, tokens: &Tokens) -> bool {
    fill_alpha(fill) == 0 || readable_fill(effective_fill(fill, tokens), tokens)
}

/// A row marked unread by hand with no fill of its own: the Done colors with the fill faded into the surface, so it
/// reads as quieter than an alert. Follows a custom Done fill.
fn unread_colors(preset: Preset, fills: &StatusFills) -> ToneColors {
    let done = tone_colors(preset, fills, Tone::Done);
    let t = tokens(preset);
    let fill = mix(done.fill.unwrap_or(t.surface), t.surface, UNREAD_FILL_STRENGTH);
    let ink = if contrast(done.ink, fill) >= contrast(t.text, fill) { done.ink } else { t.text };
    ToneColors { fill: Some(fill), ink, dot: done.dot }
}

/// How much of the Done fill an Unread fill keeps (0..=1).
const UNREAD_FILL_STRENGTH: f32 = 0.7;

/// Ink and dot for a user-chosen fill.
pub fn derive(fill: u32, tokens: &Tokens) -> ToneColors {
    let (h, s, _) = rgb_to_hsl(fill);
    let (ink, dot) = if luminance(fill) > 0.18 {
        (hsl_to_rgb(h, s.max(0.40), 0.14), hsl_to_rgb(h, s.clamp(0.35, 0.55), 0.46))
    } else {
        (hsl_to_rgb(h, s.min(0.20), 0.88), tokens.muted)
    };
    ToneColors { fill: Some(fill), ink, dot }
}

/// Whether `fill` is acceptable as a custom status fill (derived ink at least 4.5:1 on it).
pub fn readable_fill(fill: u32, tokens: &Tokens) -> bool {
    contrast(derive(fill, tokens).ink, fill) >= 4.5
}

/// WCAG relative luminance of an `0xRRGGBB` color.
pub fn luminance(color: u32) -> f64 {
    let channel = |shift: u32| {
        let c = f64::from((color >> shift) & 0xff) / 255.0;
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
}

/// WCAG contrast ratio between two `0xRRGGBB` colors.
pub fn contrast(a: u32, b: u32) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (light, dark) = if la > lb { (la, lb) } else { (lb, la) };
    (light + 0.05) / (dark + 0.05)
}

/// `color` with its lightness capped at `max_lightness` (0..=1), keeping hue and saturation: a pale accent becomes a
/// deeper shade of the same color, one that is already deep stays as it is.
pub fn deepen(color: u32, max_lightness: f64) -> u32 {
    let (h, s, l) = rgb_to_hsl(color);
    if l <= max_lightness { color } else { hsl_to_rgb(h, s, max_lightness) }
}

/// `top` laid over `under` at `amount` (0..=1) of opacity, as `0xRRGGBB`.
pub fn mix(top: u32, under: u32, amount: f32) -> u32 {
    let channel = |shift: u32| {
        let (t, u) = (((top >> shift) & 0xff) as f32, ((under >> shift) & 0xff) as f32);
        ((t * amount + u * (1.0 - amount)).round() as u32).min(0xff) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// Hue in degrees, saturation and lightness in 0..=1.
fn rgb_to_hsl(color: u32) -> (f64, f64, f64) {
    let channel = |shift: u32| f64::from((color >> shift) & 0xff) / 255.0;
    let (r, g, b) = (channel(16), channel(8), channel(0));
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d == 0.0 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> u32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let byte = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u32;
    (byte(r) << 16) | (byte(g) << 8) | byte(b)
}

#[cfg(test)]
mod tests {
    #[test]
    fn mix_blends_each_channel() {
        assert_eq!(mix(0xffffff, 0x000000, 0.5), 0x808080);
        assert_eq!(mix(0x102030, 0x405060, 0.0), 0x405060);
        assert_eq!(mix(0x102030, 0x405060, 1.0), 0x102030);
    }

    #[test]
    fn deepen_caps_lightness_and_keeps_the_hue() {
        let (hue, sat, _) = rgb_to_hsl(0x9ecbff);
        let (h, s, l) = rgb_to_hsl(deepen(0x9ecbff, 0.62));
        assert!((l - 0.62).abs() < 0.01, "{l}");
        assert!((h - hue).abs() < 1.5 && (s - sat).abs() < 0.02, "{h} {s}");
        assert_eq!(deepen(0x2f74c4, 0.62), 0x2f74c4);
    }

    #[test]
    fn text_stays_readable_on_every_tinted_card() {
        for preset in [Preset::Mist, Preset::MistLight] {
            let t = tokens(preset);
            for color in std::iter::once(t.accent).chain(ProjectColor::ALL.map(|c| c.value(preset))) {
                assert!(contrast(t.text, mix(color, t.surface, 0.18)) >= 4.5, "{preset:?} {color:06x}");
            }
        }
    }

    use super::*;
    use crate::snapshot::Rgb;

    const PRESETS: [Preset; 2] = [Preset::Mist, Preset::MistLight];
    const TONES: [Tone; 7] =
        [Tone::NeedsInput, Tone::Working, Tone::Done, Tone::Idle, Tone::Elsewhere, Tone::Closed, Tone::Unread];

    #[test]
    fn palette_names_round_trip_and_values_are_verbatim() {
        for color in ProjectColor::ALL {
            assert_eq!(ProjectColor::from_name(color.name()), Some(color));
        }
        assert_eq!(ProjectColor::Blue.value(Preset::Mist), 0x5b9cf0);
        assert_eq!(ProjectColor::Pink.value(Preset::MistLight), 0xa8378f);
        assert_eq!(ProjectColor::from_name("Teal"), None, "names are lower case");
    }

    #[test]
    fn light_palette_reads_on_the_light_ground() {
        for color in ProjectColor::ALL {
            let ground = tokens(Preset::MistLight).ground;
            assert!(contrast(color.value(Preset::MistLight), ground) >= 3.0, "{color:?}");
        }
    }

    #[test]
    fn auto_is_stable_and_spread() {
        let ops = Path::new("/home/u/work/ops-agent");
        assert_eq!(ProjectColor::auto(ops), ProjectColor::Rose);
        let distinct: std::collections::HashSet<ProjectColor> =
            ["/a", "/b", "/c", "/d", "/e", "/f", "/g", "/h", "/i", "/j"]
                .iter()
                .map(|p| ProjectColor::auto(Path::new(p)))
                .collect();
        assert!(distinct.len() >= 4, "ten paths should spread over several colors");
    }

    #[cfg(unix)]
    #[test]
    fn auto_uses_raw_path_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let odd = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff\xfe"));
        assert_eq!(ProjectColor::auto(odd), ProjectColor::auto(odd));
        assert_ne!(
            (0..=255u8)
                .map(|b| ProjectColor::auto(Path::new(std::ffi::OsStr::from_bytes(&[b'/', b]))))
                .collect::<std::collections::HashSet<_>>()
                .len(),
            1,
            "distinct byte values (including non-UTF-8) hash differently"
        );
    }

    #[test]
    fn mist_defaults_match_todays_constants() {
        let t = tokens(Preset::Mist);
        assert_eq!(
            [t.ground, t.surface, t.raised, t.line, t.text, t.muted, t.accent, t.on_accent, t.terminal],
            [0x171b24, 0x1d2230, 0x272e3e, 0x2c3344, 0xe7eaf3, 0x97a1b6, 0x9ecbff, 0x0f2440, 0x141821]
        );
        let none = StatusFills::default();
        let expected = [
            (Some(0xf7c9a3), 0x3b2414, 0xc9773d),
            (Some(0xb9d5f6), 0x132a43, 0x4f8fd6),
            (Some(0xc1e5cd), 0x143322, 0x4c9e6b),
            (Some(0x2f3647), 0xd5dae6, 0x97a1b6),
            (None, 0xc9d0de, 0x97a1b6),
            (None, 0x97a1b6, 0x555e70),
        ];
        for (tone, (fill, ink, dot)) in TONES.into_iter().zip(expected) {
            assert_eq!(tone_colors(Preset::Mist, &none, tone), ToneColors { fill, ink, dot }, "{tone:?}");
        }
    }

    #[test]
    fn row_text_is_readable_on_every_tone() {
        for preset in PRESETS {
            let t = tokens(preset);
            for tone in TONES {
                let colors = tone_colors(preset, &StatusFills::default(), tone);
                let ratio = contrast(colors.ink, colors.fill.unwrap_or(t.surface));
                assert!(ratio >= 4.5, "{preset:?} {tone:?}: {ratio:.2}");
            }
            assert!(contrast(t.text, t.surface) >= 4.5, "{preset:?} text");
            assert!(contrast(t.muted, t.surface) >= 4.5, "{preset:?} muted");
            assert!(contrast(t.on_accent, t.accent) >= 4.5, "{preset:?} on_accent");
        }
    }

    #[test]
    fn attention_fills_stand_out_in_mist() {
        let none = StatusFills::default();
        let surface = tokens(Preset::Mist).surface;
        for tone in [Tone::NeedsInput, Tone::Working, Tone::Done] {
            assert!(contrast(tone_colors(Preset::Mist, &none, tone).fill.unwrap(), surface) >= 7.0, "{tone:?}");
        }
        assert!(contrast(tone_colors(Preset::Mist, &none, Tone::Idle).fill.unwrap(), surface) < 1.5);
    }

    #[test]
    fn derive_is_readable_for_a_spread_of_fills() {
        let t = tokens(Preset::Mist);
        let readable = [0xf7c9a3, 0xb9d5f6, 0xc1e5cd, 0xffe08a, 0xe9c6ef, 0x2f3647, 0x1d2230];
        for fill in readable {
            assert!(readable_fill(fill, &t), "{fill:06x} refused");
            assert!(contrast(derive(fill, &t).ink, fill) >= 4.5, "{fill:06x}");
        }
        let fill = 0x5a6b8c;
        assert!(!readable_fill(fill, &t) || contrast(derive(fill, &t).ink, fill) >= 4.5);
    }

    #[test]
    fn unreadable_fill_is_refused() {
        let t = tokens(Preset::Mist);
        assert!(!readable_fill(0x808080, &t));
        let fills = StatusFills { done: Some(0x808080), ..StatusFills::default() };
        assert_eq!(
            tone_colors(Preset::Mist, &fills, Tone::Done),
            tone_colors(Preset::Mist, &StatusFills::default(), Tone::Done)
        );
    }

    #[test]
    fn a_readable_fill_is_applied_and_elsewhere_ignores_fills() {
        let fills = StatusFills { working: Some(0xffe08a), idle: Some(0x1d2230), ..StatusFills::default() };
        let working = tone_colors(Preset::Mist, &fills, Tone::Working);
        assert_eq!(working.fill, Some(0xffe08a));
        assert_ne!(working.ink, 0x132a43);
        assert_eq!(tone_colors(Preset::Mist, &fills, Tone::Idle).fill, Some(0x1d2230));
        assert_eq!(tone_colors(Preset::Mist, &fills, Tone::Elsewhere).fill, None);
    }

    #[test]
    fn defaults_untouched_without_fills() {
        let done = tone_colors(Preset::Mist, &StatusFills::default(), Tone::Done);
        assert_eq!(done, ToneColors { fill: Some(0xc1e5cd), ink: 0x143322, dot: 0x4c9e6b });
    }

    #[test]
    fn unread_is_a_quieter_done_that_follows_a_custom_done_fill() {
        for preset in PRESETS {
            let t = tokens(preset);
            let done = tone_colors(preset, &StatusFills::default(), Tone::Done);
            let unread = tone_colors(preset, &StatusFills::default(), Tone::Unread);
            assert_eq!(unread.dot, done.dot);
            assert_ne!(unread.fill, done.fill);
            assert!(
                contrast(unread.fill.unwrap(), t.surface) < contrast(done.fill.unwrap(), t.surface),
                "{preset:?}: closer to the surface than Done"
            );
            let fills = StatusFills { done: Some(0xffe08a), ..StatusFills::default() };
            assert_ne!(tone_colors(preset, &fills, Tone::Unread), unread);
        }
    }

    #[test]
    fn a_see_through_fill_is_blended_over_the_surface() {
        for preset in PRESETS {
            let t = tokens(preset);
            let half = crate::settings::with_alpha(0xe9c6ef, 0x80);
            let fills = StatusFills { done: Some(half), ..StatusFills::default() };
            let shown = effective_fill(half, &t);
            assert_eq!(shown, mix(0xe9c6ef, t.surface, 128.0 / 255.0));
            if usable_fill(half, &t) {
                let colors = tone_colors(preset, &fills, Tone::Done);
                assert_eq!(colors.fill, Some(shown));
                assert!(contrast(colors.ink, shown) >= 4.5, "{preset:?}");
                assert_eq!(colors.dot, derive(0xe9c6ef, &t).dot);
            }
        }
        assert_eq!(effective_fill(0xe9c6ef, &tokens(Preset::Mist)), 0xe9c6ef);
        assert!(usable_fill(crate::settings::with_alpha(0xe9c6ef, 0x80), &tokens(Preset::MistLight)));
        assert!(usable_fill(crate::settings::TRANSPARENT, &tokens(Preset::Mist)));
    }

    #[test]
    fn unread_takes_its_own_fill_or_none() {
        for preset in PRESETS {
            let t = tokens(preset);
            let done = tone_colors(preset, &StatusFills::default(), Tone::Done);
            let fills = StatusFills { unread: Some(0xe9c6ef), ..StatusFills::default() };
            assert_eq!(tone_colors(preset, &fills, Tone::Unread), derive(0xe9c6ef, &t));
            let fills = StatusFills { unread: Some(crate::settings::TRANSPARENT), ..StatusFills::default() };
            let clear = tone_colors(preset, &fills, Tone::Unread);
            assert_eq!(clear, ToneColors { fill: None, ink: t.text, dot: done.dot });
            assert!(contrast(clear.ink, t.surface) >= 4.5, "{preset:?}");
            let fills = StatusFills { unread: Some(0x808080), ..StatusFills::default() };
            assert_eq!(
                tone_colors(preset, &fills, Tone::Unread),
                tone_colors(preset, &StatusFills::default(), Tone::Unread)
            );
        }
    }

    #[test]
    fn system_follows_the_desktop() {
        assert_eq!(resolve(ThemeChoice::System, true), Preset::Mist);
        assert_eq!(resolve(ThemeChoice::System, false), Preset::MistLight);
        assert_eq!(resolve(ThemeChoice::Mist, false), Preset::Mist);
        assert_eq!(resolve(ThemeChoice::MistLight, true), Preset::MistLight);
    }

    #[test]
    fn light_palette_is_readable() {
        let bg = 0xf8f9fc;
        let hex = |c: Rgb| (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b);
        let light = palette::LIGHT;
        assert!(contrast(hex(light.foreground), bg) >= 7.0);
        for i in (1..=6).chain(9..=14) {
            let ratio = contrast(hex(light.ansi[i]), bg);
            assert!(ratio >= 3.0, "ansi {i}: {ratio:.2}");
        }
    }

    #[test]
    fn terminal_token_matches_palette() {
        for preset in PRESETS {
            assert_eq!(Rgb::hex(tokens(preset).terminal), terminal_palette(preset).background, "{preset:?}");
        }
    }
    #[test]
    fn tint_parses_names_and_hex() {
        use super::{ProjectColor, Tint};
        assert_eq!(Tint::parse("blue"), Some(Tint::Palette(ProjectColor::Blue)));
        assert_eq!(Tint::parse("#5B9CF0"), Some(Tint::Custom(0x5b9cf0)));
        assert_eq!(Tint::parse("#5b9cf0"), Some(Tint::Custom(0x5b9cf0)));
        for bad in ["Blue", "#12", "#gggggg", "#5b9cf0ff", "5b9cf0", ""] {
            assert_eq!(Tint::parse(bad), None, "{bad:?}");
        }
        assert_eq!(super::parse_hex("5B9CF0"), Some(0x5b9cf0));
        assert_eq!(super::parse_hex("#5b9cf0"), Some(0x5b9cf0));
        assert_eq!(super::parse_hex("#5b9cf"), None);
    }

    #[test]
    fn tint_round_trips_as_setting() {
        use super::{ProjectColor, Tint};
        assert_eq!(Tint::Custom(0xab12ef).to_setting(), "#ab12ef");
        assert_eq!(Tint::Palette(ProjectColor::Teal).to_setting(), "teal");
        for tint in [Tint::Custom(0x00ff10), Tint::Palette(ProjectColor::Pink)] {
            assert_eq!(Tint::parse(&tint.to_setting()), Some(tint));
        }
    }

    #[test]
    fn tint_values() {
        use super::{Preset, ProjectColor, Tint};
        assert_eq!(Tint::Palette(ProjectColor::Blue).value(Preset::Mist), 0x5b9cf0);
        assert_eq!(Tint::Custom(0x123456).value(Preset::MistLight), 0x123456);
        assert_eq!(Tint::from(ProjectColor::Lime), Tint::Palette(ProjectColor::Lime));
    }
}
