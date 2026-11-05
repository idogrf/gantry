//! The settings file (`settings.json`): typed model, tolerant loading, and writing only changed keys.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::ssh_config::{validate_name, validate_target};
use crate::theme_colors::{ProjectColor, Tint};

/// Theme choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    /// The dark Mist theme.
    #[default]
    Mist,
    /// The light Mist theme.
    MistLight,
    /// Follow the system's light/dark setting.
    System,
}

impl ThemeChoice {
    fn name(self) -> &'static str {
        match self {
            ThemeChoice::Mist => "mist",
            ThemeChoice::MistLight => "mist-light",
            ThemeChoice::System => "system",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "mist" => Some(ThemeChoice::Mist),
            "mist-light" => Some(ThemeChoice::MistLight),
            "system" => Some(ThemeChoice::System),
            _ => None,
        }
    }
}

/// Built-in alert sound (a recording embedded in the app).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertTone {
    /// Finished: a short, classic blip.
    ClassicBlip,
    /// Finished: a gentle rise.
    GentleRise,
    /// Finished: a soft bloop.
    SoftBloop,
    /// Finished: a soft drop.
    SoftDrop,
    /// Needs input: a falling blip.
    FallingBlip,
    /// Needs input: a soft fall.
    SoftFall,
    /// Needs input: a soft flutter.
    SoftFlutter,
    /// Needs input: a warm hum.
    WarmHum,
}

impl AlertTone {
    /// Every tone.
    pub const ALL: [AlertTone; 8] = [
        AlertTone::ClassicBlip,
        AlertTone::GentleRise,
        AlertTone::SoftBloop,
        AlertTone::SoftDrop,
        AlertTone::FallingBlip,
        AlertTone::SoftFall,
        AlertTone::SoftFlutter,
        AlertTone::WarmHum,
    ];

    /// The tones offered for a finished session, in the order the Settings list shows them.
    pub const FINISHED: [AlertTone; 4] =
        [AlertTone::ClassicBlip, AlertTone::GentleRise, AlertTone::SoftBloop, AlertTone::SoftDrop];

    /// The tones offered for a session that needs input, in the order the Settings list shows them.
    pub const NEEDS_INPUT: [AlertTone; 4] =
        [AlertTone::FallingBlip, AlertTone::SoftFall, AlertTone::SoftFlutter, AlertTone::WarmHum];

    fn name(self) -> &'static str {
        match self {
            AlertTone::ClassicBlip => "classic-blip",
            AlertTone::GentleRise => "gentle-rise",
            AlertTone::SoftBloop => "soft-bloop",
            AlertTone::SoftDrop => "soft-drop",
            AlertTone::FallingBlip => "falling-blip",
            AlertTone::SoftFall => "soft-fall",
            AlertTone::SoftFlutter => "soft-flutter",
            AlertTone::WarmHum => "warm-hum",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tone| tone.name() == name)
    }
}

/// Which channels an event uses, and its sound.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EventAlerts {
    /// Show the unseen badge.
    pub badge: bool,
    /// Raise a native notification.
    pub notification: bool,
    /// Play the sound.
    pub sound: bool,
    /// Which sound to play.
    pub tone: AlertTone,
}

/// Status fills chosen by the user (`None` = the theme's default).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct StatusFills {
    /// Fill for a session that needs input.
    pub needs_input: Option<u32>,
    /// Fill for a working session.
    pub working: Option<u32>,
    /// Fill for a finished session.
    pub done: Option<u32>,
    /// Fill for an idle session.
    pub idle: Option<u32>,
}

/// Terminal font (`family: None` = the platform default).
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalFont {
    /// Font family, or the platform default.
    pub family: Option<String>,
    /// Size in points.
    pub size: f32,
    /// Line height as a multiple of the size.
    pub line_height: f32,
}

/// Alert settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AlertSettings {
    /// Alerts for a session that needs input.
    pub needs_input: EventAlerts,
    /// Alerts for a session that finished.
    pub finished: EventAlerts,
    /// Seconds a session must be busy before finishing counts.
    pub finished_after_seconds: u32,
    /// Alert even when the pane is visible.
    pub when_visible: bool,
    /// Alert only while the window is unfocused.
    pub only_when_unfocused: bool,
    /// Master switch for sounds.
    pub play_sounds: bool,
    /// Sound volume, 0 to 1.
    pub volume: f32,
}

/// One Claude config dir the user listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeDir {
    /// Unique, non-empty display name.
    pub name: String,
    /// Directory as written (`~` kept).
    pub path: PathBuf,
    /// Program to run for Claude in this dir.
    pub command: String,
    /// Arguments placed before Gantry's own.
    pub args: Vec<String>,
    /// Color of its badge; `None` = the automatic color of its path.
    pub color: Option<ProjectColor>,
}

/// How the sidebar looks when Gantry starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SidebarAtStart {
    /// Use the saved layout's state.
    #[default]
    Remember,
    /// Always the full sidebar.
    Expanded,
    /// Always the thin icon column.
    Compact,
}

impl SidebarAtStart {
    /// Whether the sidebar starts compact, given whether the saved layout had it compact.
    pub fn compact(self, saved: bool) -> bool {
        match self {
            SidebarAtStart::Remember => saved,
            SidebarAtStart::Expanded => false,
            SidebarAtStart::Compact => true,
        }
    }

    fn name(self) -> &'static str {
        match self {
            SidebarAtStart::Remember => "remember",
            SidebarAtStart::Expanded => "expanded",
            SidebarAtStart::Compact => "compact",
        }
    }

    /// The names of earlier versions, `shown` and `hidden`, still read.
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "remember" => Some(SidebarAtStart::Remember),
            "expanded" | "shown" => Some(SidebarAtStart::Expanded),
            "compact" | "hidden" => Some(SidebarAtStart::Compact),
            _ => None,
        }
    }
}

/// The shell new shell panes run.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ShellSettings {
    /// Program; empty = `$SHELL`.
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
}

/// Sizes of the recent sessions pop-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecentsSettings {
    /// Sessions listed per project (1 to 20).
    pub per_project: usize,
    /// Project chips (0 to 12).
    pub chips: usize,
}

/// Window behaviour.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowSettings {
    /// Restore tabs and panes on start.
    pub restore_layout: bool,
    /// Resume finished sessions by themselves on start (instead of waiting for Continue).
    pub resume_sessions: bool,
    /// Sidebar state at start.
    pub sidebar_at_start: SidebarAtStart,
    /// Scrollback lines for new panes (1000 to 100000).
    pub scrollback: u32,
    /// Reorder right-to-left text for display.
    pub right_to_left: bool,
}

/// The `lastPrompt` setting: the card with your last prompt above each Claude pane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LastPromptSettings {
    /// Show your last prompt above each Claude pane.
    pub enabled: bool,
    /// Text size of the card (11 to 32, scaled with the interface size).
    pub size: f32,
    /// Background color of the card; unset = the theme's accent.
    pub color: Option<u32>,
}

/// The `scrollbar` setting: which panes get a scrollbar. Shell panes always have one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarSettings {
    /// Give Claude panes a scrollbar too. It shows something only while Claude runs in its classic renderer; in
    /// fullscreen mode Claude does its own scrolling and the bar stays empty.
    pub claude: bool,
}

/// `~/x` becomes `<home>/x`; other paths are unchanged.
pub fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

/// The remote Gantry command when a host does not name one: where the SSH wizard puts `gantryd`, Gantry without its
/// window.
pub const DEFAULT_REMOTE_COMMAND: &str = "~/.local/bin/gantryd";

/// How Gantry signs in to an SSH host. It only picks ssh's options; nothing is stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SshAuth {
    /// ssh decides (agent, keys, then a password), as `~/.ssh/config` says.
    #[default]
    Automatic,
    /// Only a password: keys and the agent are not tried.
    Password,
    /// Only keys (the agent and the default keys): no password fallback.
    Key,
}

impl SshAuth {
    /// The value in settings.json.
    pub fn as_str(self) -> &'static str {
        match self {
            SshAuth::Automatic => "automatic",
            SshAuth::Password => "password",
            SshAuth::Key => "key",
        }
    }

    /// Parse a settings.json value.
    pub fn parse(text: &str) -> Option<SshAuth> {
        match text {
            "automatic" => Some(SshAuth::Automatic),
            "password" => Some(SshAuth::Password),
            "key" => Some(SshAuth::Key),
            _ => None,
        }
    }

    /// The `-o` options ssh needs for this method (empty for `Automatic`).
    pub fn ssh_options(self) -> &'static [&'static str] {
        match self {
            SshAuth::Automatic => &[],
            SshAuth::Password => {
                &["-o", "PreferredAuthentications=password,keyboard-interactive", "-o", "IdentitiesOnly=yes"]
            }
            SshAuth::Key => &["-o", "PreferredAuthentications=publickey"],
        }
    }
}

/// One SSH host the user listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshHost {
    /// Unique, non-empty name without `/`, never `local`: shown in menus and project headings.
    pub name: String,
    /// What follows `ssh`: an alias from `~/.ssh/config` or `user@host`.
    pub target: String,
    /// The remote Gantry binary.
    pub command: String,
    /// How to sign in.
    pub auth: SshAuth,
    /// The ssh port; `None` = what ssh (and `~/.ssh/config`) says.
    pub port: Option<u16>,
}

/// The port typed in a field: empty = none, else a number from 1 to 65535.
pub fn parse_port(text: &str) -> Result<Option<u16>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    match text.parse::<u16>() {
        Ok(port) if port > 0 => Ok(Some(port)),
        _ => Err("The port is a number from 1 to 65535".into()),
    }
}

/// Add `host` to the list; its name must be valid and unused, its target valid.
pub fn add_ssh_host(hosts: &mut Vec<SshHost>, host: SshHost) -> Result<(), String> {
    let names: Vec<&str> = hosts.iter().map(|known| known.name.as_str()).collect();
    validate_name(&host.name, &names)?;
    validate_target(&host.target)?;
    hosts.push(host);
    Ok(())
}

/// Replace the entry called `old_name` with `host` (same place in the list); the name may stay or change but must not
/// clash with another entry.
pub fn replace_ssh_host(hosts: &mut [SshHost], old_name: &str, host: SshHost) -> Result<(), String> {
    let Some(index) = hosts.iter().position(|known| known.name == old_name) else {
        return Err(format!("no host called {old_name}"));
    };
    let others: Vec<&str> =
        hosts.iter().enumerate().filter(|(at, _)| *at != index).map(|(_, known)| known.name.as_str()).collect();
    validate_name(&host.name, &others)?;
    validate_target(&host.target)?;
    hosts[index] = host;
    Ok(())
}

/// Remove the entry called `name`; whether there was one.
pub fn remove_ssh_host(hosts: &mut Vec<SshHost>, name: &str) -> bool {
    let before = hosts.len();
    hosts.retain(|host| host.name != name);
    hosts.len() != before
}

/// Everything in settings.json.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Theme choice.
    pub theme: ThemeChoice,
    /// Status fills.
    pub status_fills: StatusFills,
    /// Color of the ring around the selected sidebar row (`None` = the theme's accent).
    pub selection: Option<u32>,
    /// Terminal font.
    pub terminal_font: TerminalFont,
    /// Interface font size in points.
    pub ui_font_size: f32,
    /// Alert settings.
    pub alerts: AlertSettings,
    /// Action id to keystroke; `None` = explicitly unset; absent = the default.
    pub shortcuts: BTreeMap<String, Option<String>>,
    /// Configured Claude config dirs; empty = the implicit "Default".
    pub claude_dirs: Vec<ClaudeDir>,
    /// Name of the default dir; `None` or unknown = the first entry.
    pub default_claude_dir: Option<String>,
    /// Folder to the name of the dir picked for it.
    pub folder_claude_dirs: BTreeMap<PathBuf, String>,
    /// SSH hosts, connected on first use.
    pub ssh_hosts: Vec<SshHost>,
    /// Shell for new shell panes.
    pub shell: ShellSettings,
    /// Recent sessions pop-up sizes.
    pub recents: RecentsSettings,
    /// Pinned folders, in order.
    pub favorites: Vec<PathBuf>,
    /// Window behaviour.
    pub window: WindowSettings,
    /// The last prompt card above Claude panes.
    pub last_prompt: LastPromptSettings,
    /// The scrollbar at the right edge of panes.
    pub scrollbar: ScrollbarSettings,
    /// Project colors.
    pub project_colors: ProjectColors,
    /// Whether Working sessions show the moving glow.
    pub animate_working: bool,
    /// Whether splits slide in and menus, overlays and cards fade in.
    pub animate_ui: bool,
    /// Whether Gantry asks GitHub for a newer release (when it opens, then every few hours).
    pub check_for_updates: bool,
    /// The first-run wizard has been answered (imported or skipped), so it does not open by itself.
    pub setup_done: bool,
}

/// The `projectColors` setting: whether projects are colored, and the colors picked per folder.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectColors {
    /// Whether projects get a color at all.
    pub enabled: bool,
    /// Project root to the color picked for it; others get the automatic color.
    pub folders: BTreeMap<PathBuf, Tint>,
}

impl Default for ProjectColors {
    fn default() -> Self {
        ProjectColors { enabled: true, folders: BTreeMap::new() }
    }
}

/// The color of the project at `root`: its pick, else the automatic one; `None` when project colors are off.
pub fn project_color(settings: &Settings, root: &Path) -> Option<Tint> {
    settings.project_colors.color_of(root)
}

impl ProjectColors {
    /// The color of the project at `root`: its pick, else the automatic one; `None` when colors are off.
    pub fn color_of(&self, root: &Path) -> Option<Tint> {
        self.enabled.then(|| self.folders.get(root).copied().unwrap_or_else(|| Tint::Palette(ProjectColor::auto(root))))
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: ThemeChoice::Mist,
            status_fills: StatusFills::default(),
            selection: None,
            terminal_font: TerminalFont { family: None, size: 14.0, line_height: 1.2 },
            ui_font_size: 14.0,
            alerts: AlertSettings {
                needs_input: EventAlerts { badge: true, notification: true, sound: true, tone: AlertTone::FallingBlip },
                finished: EventAlerts { badge: true, notification: true, sound: true, tone: AlertTone::ClassicBlip },
                finished_after_seconds: 1,
                when_visible: false,
                only_when_unfocused: false,
                play_sounds: true,
                volume: 0.4,
            },
            shortcuts: BTreeMap::new(),
            claude_dirs: Vec::new(),
            default_claude_dir: None,
            folder_claude_dirs: BTreeMap::new(),
            ssh_hosts: Vec::new(),
            shell: ShellSettings::default(),
            recents: RecentsSettings { per_project: 5, chips: 6 },
            favorites: Vec::new(),
            window: WindowSettings {
                restore_layout: true,
                resume_sessions: true,
                sidebar_at_start: SidebarAtStart::Remember,
                scrollback: 10_000,
                right_to_left: true,
            },
            last_prompt: LastPromptSettings { enabled: true, size: 18.0, color: None },
            scrollbar: ScrollbarSettings { claude: true },
            project_colors: ProjectColors::default(),
            animate_working: true,
            animate_ui: true,
            check_for_updates: true,
            setup_done: false,
        }
    }
}

/// One problem found while loading: a dotted key path (e.g. "alerts.volume", "statusColors.done") or "" for
/// the whole file, and what was wrong (for the banner).
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    /// Dotted key path, or "" for the whole file.
    pub key: String,
    /// What was wrong.
    pub message: String,
}

/// The result of reading the file.
pub struct Loaded {
    /// The settings to apply: `None` only when the file is not JSON at all (keep the last good ones).
    pub settings: Option<Settings>,
    /// Everything that was wrong.
    pub problems: Vec<Problem>,
    /// The raw JSON object as read (unknown keys included), kept so writes preserve them.
    pub raw: Map<String, Value>,
}

/// `#rrggbb` → 0xRRGGBB.
pub fn parse_hex(text: &str) -> Option<u32> {
    let digits = text.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// 0xRRGGBB → `#rrggbb`.
pub fn hex(color: u32) -> String {
    format!("#{:06x}", color & 0xff_ffff)
}

/// Read `path`. A missing file gives defaults, no problems, and an empty `raw`.
pub fn load(path: &Path) -> Loaded {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Loaded { settings: Some(Settings::default()), problems: Vec::new(), raw: Map::new() }
        }
        Err(error) => Loaded {
            settings: None,
            problems: vec![Problem { key: String::new(), message: format!("cannot read the settings file: {error}") }],
            raw: Map::new(),
        },
    }
}

/// Parse JSON text (`load` reads the file and calls this).
pub fn parse(text: &str) -> Loaded {
    let raw = match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => map,
        Ok(_) => return broken("the top level must be a JSON object".into()),
        Err(error) => return broken(format!("not valid JSON: {error}")),
    };
    let mut reader = Reader { problems: Vec::new() };
    let settings = reader.settings(&raw);
    Loaded { settings: Some(settings), problems: reader.problems, raw }
}

fn broken(message: String) -> Loaded {
    Loaded { settings: None, problems: vec![Problem { key: String::new(), message }], raw: Map::new() }
}

struct Reader {
    problems: Vec<Problem>,
}

impl Reader {
    fn problem(&mut self, key: &str, message: String) {
        self.problems.push(Problem { key: key.into(), message });
    }

    /// The object at `key`, or an empty one (with a problem when it is present but not an object).
    fn object(&mut self, parent: &Map<String, Value>, prefix: &str, key: &str) -> Map<String, Value> {
        match parent.get(key) {
            None => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => {
                self.problem(&format!("{prefix}{key}"), "expected an object".into());
                Map::new()
            }
        }
    }

    fn bool(&mut self, map: &Map<String, Value>, prefix: &str, key: &str, default: bool) -> bool {
        match map.get(key) {
            None => default,
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                self.problem(&format!("{prefix}{key}"), "expected true or false".into());
                default
            }
        }
    }

    fn number(&mut self, map: &Map<String, Value>, prefix: &str, key: &str, default: f64, range: (f64, f64)) -> f64 {
        match map.get(key) {
            None => default,
            Some(Value::Number(number)) => {
                let value = number.as_f64().unwrap_or(default);
                if value < range.0 || value > range.1 {
                    self.problem(
                        &format!("{prefix}{key}"),
                        format!("{value} is outside {} to {}; clamped", range.0, range.1),
                    );
                }
                value.clamp(range.0, range.1)
            }
            Some(_) => {
                self.problem(&format!("{prefix}{key}"), "expected a number".into());
                default
            }
        }
    }

    fn choice<T: Copy>(
        &mut self,
        map: &Map<String, Value>,
        prefix: &str,
        key: &str,
        default: T,
        from_name: fn(&str) -> Option<T>,
    ) -> T {
        match map.get(key) {
            None => default,
            Some(Value::String(name)) => from_name(name).unwrap_or_else(|| {
                self.problem(&format!("{prefix}{key}"), format!("unknown value \"{name}\""));
                default
            }),
            Some(_) => {
                self.problem(&format!("{prefix}{key}"), "expected a string".into());
                default
            }
        }
    }

    fn color(&mut self, map: &Map<String, Value>, prefix: &str, key: &str) -> Option<u32> {
        match map.get(key)? {
            Value::String(text) => parse_hex(text).or_else(|| {
                self.problem(&format!("{prefix}{key}"), format!("\"{text}\" is not a #rrggbb color"));
                None
            }),
            _ => {
                self.problem(&format!("{prefix}{key}"), "expected a #rrggbb color".into());
                None
            }
        }
    }

    fn string(&mut self, map: &Map<String, Value>, prefix: &str, key: &str) -> String {
        match map.get(key) {
            None => String::new(),
            Some(Value::String(text)) => text.clone(),
            Some(_) => {
                self.problem(&format!("{prefix}{key}"), "expected a string".into());
                String::new()
            }
        }
    }

    fn strings(&mut self, map: &Map<String, Value>, prefix: &str, key: &str) -> Vec<String> {
        match map.get(key) {
            None => Vec::new(),
            Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
                items.iter().filter_map(|item| item.as_str().map(String::from)).collect()
            }
            Some(_) => {
                self.problem(&format!("{prefix}{key}"), "expected a list of strings".into());
                Vec::new()
            }
        }
    }

    fn shortcuts(&mut self, raw: &Map<String, Value>) -> BTreeMap<String, Option<String>> {
        let map = self.object(raw, "", "shortcuts");
        let mut out = BTreeMap::new();
        for (id, value) in map {
            match value {
                Value::String(keystroke) => {
                    out.insert(id, Some(keystroke));
                }
                Value::Null => {
                    out.insert(id, None);
                }
                _ => self.problem(&format!("shortcuts.{id}"), "expected a keystroke or null".into()),
            }
        }
        out
    }

    fn ssh_hosts(&mut self, raw: &Map<String, Value>) -> Vec<SshHost> {
        let items = match raw.get("sshHosts") {
            None => return Vec::new(),
            Some(Value::Array(items)) => items,
            Some(_) => {
                self.problem("sshHosts", "expected a list".into());
                return Vec::new();
            }
        };
        let mut hosts: Vec<SshHost> = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let key = format!("sshHosts[{index}]");
            let Value::Object(map) = item else {
                self.problem(&key, "expected an object; dropped".into());
                continue;
            };
            let name = self.string(map, &format!("{key}."), "name");
            let target = self.string(map, &format!("{key}."), "target");
            let command = match map.get("command") {
                None => DEFAULT_REMOTE_COMMAND.to_string(),
                Some(_) => self.string(map, &format!("{key}."), "command"),
            };
            let auth = match map.get("auth") {
                None => SshAuth::Automatic,
                Some(Value::String(text)) => SshAuth::parse(text).unwrap_or_else(|| {
                    self.problem(&format!("{key}.auth"), "expected automatic, password or key".into());
                    SshAuth::Automatic
                }),
                Some(_) => {
                    self.problem(&format!("{key}.auth"), "expected automatic, password or key".into());
                    SshAuth::Automatic
                }
            };
            let port = match map.get("port") {
                None => None,
                Some(value) => {
                    match value.as_u64().and_then(|port| u16::try_from(port).ok()).filter(|port| *port > 0) {
                        Some(port) => Some(port),
                        None => {
                            self.problem(&format!("{key}.port"), "expected a number from 1 to 65535".into());
                            None
                        }
                    }
                }
            };
            if name.is_empty() || target.is_empty() || command.is_empty() {
                self.problem(&key, "needs a name, a target and a command; dropped".into());
            } else if name.contains('/') {
                self.problem(&key, format!("the name \"{name}\" may not contain /; dropped"));
            } else if name.eq_ignore_ascii_case("local") {
                self.problem(&key, "\"local\" is this machine; dropped".into());
            } else if hosts.iter().any(|host| host.name == name) {
                self.problem(&key, format!("the name \"{name}\" is used twice; dropped"));
            } else {
                hosts.push(SshHost { name, target, command, auth, port });
            }
        }
        hosts
    }

    fn claude_dirs(&mut self, raw: &Map<String, Value>) -> Vec<ClaudeDir> {
        let items = match raw.get("claudeDirs") {
            None => return Vec::new(),
            Some(Value::Array(items)) => items,
            Some(_) => {
                self.problem("claudeDirs", "expected a list".into());
                return Vec::new();
            }
        };
        let mut dirs: Vec<ClaudeDir> = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let key = format!("claudeDirs[{index}]");
            let Value::Object(map) = item else {
                self.problem(&key, "expected an object; dropped".into());
                continue;
            };
            let name = self.string(map, &format!("{key}."), "name");
            let path = self.string(map, &format!("{key}."), "path");
            let command = match map.get("command") {
                None => "claude".to_string(),
                Some(_) => self.string(map, &format!("{key}."), "command"),
            };
            let args = self.strings(map, &format!("{key}."), "args");
            let color = match map.get("color") {
                None => None,
                Some(Value::String(name)) if ProjectColor::from_name(name).is_some() => ProjectColor::from_name(name),
                Some(_) => {
                    self.problem(&format!("{key}.color"), "expected a color name; the automatic color is used".into());
                    None
                }
            };
            if name.is_empty() || path.is_empty() || command.is_empty() {
                self.problem(&key, "needs a name, a path and a command; dropped".into());
            } else if dirs.iter().any(|dir| dir.name == name) {
                self.problem(&key, format!("the name \"{name}\" is used twice; dropped"));
            } else if dirs.iter().any(|dir| dir.path == Path::new(&path)) {
                self.problem(&key, format!("the path \"{path}\" is used twice; dropped"));
            } else {
                dirs.push(ClaudeDir { name, path: path.into(), command, args, color });
            }
        }
        dirs
    }

    fn folder_claude_dirs(&mut self, raw: &Map<String, Value>, names: &BTreeSet<&str>) -> BTreeMap<PathBuf, String> {
        let map = self.object(raw, "", "folderClaudeDirs");
        let mut out = BTreeMap::new();
        for (folder, value) in map {
            let Value::String(name) = value else {
                self.problem(&format!("folderClaudeDirs.{folder}"), "expected a dir name".into());
                continue;
            };
            if !names.contains(name.as_str()) {
                self.problem(
                    &format!("folderClaudeDirs.{folder}"),
                    format!("no config dir named \"{name}\"; the default is used"),
                );
            }
            out.insert(PathBuf::from(folder), name);
        }
        out
    }

    fn favorites(&mut self, raw: &Map<String, Value>) -> Vec<PathBuf> {
        match raw.get("favorites") {
            None => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| match item {
                    Value::String(path) => Some(PathBuf::from(path)),
                    _ => {
                        self.problem(&format!("favorites[{index}]"), "expected a folder; dropped".into());
                        None
                    }
                })
                .collect(),
            Some(_) => {
                self.problem("favorites", "expected a list of strings".into());
                Vec::new()
            }
        }
    }

    fn event(&mut self, parent: &Map<String, Value>, key: &str, default: EventAlerts) -> EventAlerts {
        let map = self.object(parent, "alerts.", key);
        let prefix = format!("alerts.{key}.");
        EventAlerts {
            badge: self.bool(&map, &prefix, "badge", default.badge),
            notification: self.bool(&map, &prefix, "notification", default.notification),
            sound: self.bool(&map, &prefix, "sound", default.sound),
            tone: self.choice(&map, &prefix, "tone", default.tone, AlertTone::from_name),
        }
    }

    fn settings(&mut self, raw: &Map<String, Value>) -> Settings {
        let defaults = Settings::default();
        let theme = self.choice(raw, "", "theme", defaults.theme, ThemeChoice::from_name);

        let colors = self.object(raw, "", "statusColors");
        let status_fills = StatusFills {
            needs_input: self.color(&colors, "statusColors.", "needsInput"),
            working: self.color(&colors, "statusColors.", "working"),
            done: self.color(&colors, "statusColors.", "done"),
            idle: self.color(&colors, "statusColors.", "idle"),
        };

        let selection = self.color(raw, "", "selection");

        let font = self.object(raw, "", "terminalFont");
        let family = match font.get("family") {
            None => None,
            Some(Value::String(name)) if !name.is_empty() => Some(name.clone()),
            Some(Value::String(_)) => None,
            Some(_) => {
                self.problem("terminalFont.family", "expected a font name".into());
                None
            }
        };
        let terminal_font = TerminalFont {
            family,
            size: self.number(&font, "terminalFont.", "size", 14.0, (8.0, 40.0)) as f32,
            line_height: self.number(&font, "terminalFont.", "lineHeight", 1.2, (1.0, 2.0)) as f32,
        };

        let ui_font_size = self.number(raw, "", "uiFontSize", 14.0, (11.0, 20.0)) as f32;
        let animate_working = self.bool(raw, "", "animateWorking", true);
        let animate_ui = self.bool(raw, "", "animateUi", true);
        let check_for_updates = self.bool(raw, "", "checkForUpdates", true);
        let setup_done = self.bool(raw, "", "setupDone", false);

        let map = self.object(raw, "", "alerts");
        let default = defaults.alerts;
        let alerts = AlertSettings {
            needs_input: self.event(&map, "needsInput", default.needs_input),
            finished: self.event(&map, "finished", default.finished),
            finished_after_seconds: self.number(&map, "alerts.", "finishedAfterSeconds", 1.0, (0.0, 600.0)).round()
                as u32,
            when_visible: self.bool(&map, "alerts.", "whenVisible", default.when_visible),
            only_when_unfocused: self.bool(&map, "alerts.", "onlyWhenUnfocused", default.only_when_unfocused),
            play_sounds: self.bool(&map, "alerts.", "playSounds", default.play_sounds),
            volume: self.number(&map, "alerts.", "volume", 0.4, (0.0, 1.0)) as f32,
        };

        let shortcuts = self.shortcuts(raw);
        let claude_dirs = self.claude_dirs(raw);
        let implicit = BTreeSet::from(["Default"]);
        let names: BTreeSet<&str> =
            if claude_dirs.is_empty() { implicit } else { claude_dirs.iter().map(|dir| dir.name.as_str()).collect() };
        let default_claude_dir = match raw.get("defaultClaudeDir") {
            None => None,
            Some(Value::String(name)) => {
                if !names.contains(name.as_str()) {
                    self.problem(
                        "defaultClaudeDir",
                        format!("no config dir named \"{name}\"; the first one is the default"),
                    );
                }
                Some(name.clone())
            }
            Some(_) => {
                self.problem("defaultClaudeDir", "expected a dir name".into());
                None
            }
        };
        let folder_claude_dirs = self.folder_claude_dirs(raw, &names);
        let ssh_hosts = self.ssh_hosts(raw);

        let map = self.object(raw, "", "shell");
        let shell = ShellSettings {
            program: self.string(&map, "shell.", "program"),
            args: self.strings(&map, "shell.", "args"),
        };

        let map = self.object(raw, "", "recents");
        let recents = RecentsSettings {
            per_project: self.number(&map, "recents.", "perProject", 5.0, (1.0, 20.0)).round() as usize,
            chips: self.number(&map, "recents.", "chips", 6.0, (0.0, 12.0)).round() as usize,
        };

        let favorites = self.favorites(raw);

        let map = self.object(raw, "", "window");
        let window = WindowSettings {
            restore_layout: self.bool(&map, "window.", "restoreLayout", defaults.window.restore_layout),
            resume_sessions: self.bool(&map, "window.", "resumeSessions", defaults.window.resume_sessions),
            sidebar_at_start: self.choice(
                &map,
                "window.",
                "sidebarAtStart",
                defaults.window.sidebar_at_start,
                SidebarAtStart::from_name,
            ),
            scrollback: self.number(&map, "window.", "scrollback", 10_000.0, (1000.0, 100_000.0)).round() as u32,
            right_to_left: self.bool(&map, "window.", "rightToLeft", defaults.window.right_to_left),
        };

        let map = self.object(raw, "", "lastPrompt");
        let last_prompt = LastPromptSettings {
            enabled: self.bool(&map, "lastPrompt.", "enabled", defaults.last_prompt.enabled),
            size: self.number(&map, "lastPrompt.", "size", 18.0, (11.0, 32.0)) as f32,
            color: self.color(&map, "lastPrompt.", "color"),
        };

        let map = self.object(raw, "", "scrollbar");
        let scrollbar =
            ScrollbarSettings { claude: self.bool(&map, "scrollbar.", "claude", defaults.scrollbar.claude) };

        let map = self.object(raw, "", "projectColors");
        let enabled = self.bool(&map, "projectColors.", "enabled", true);
        let folders_map = self.object(&map, "projectColors.", "folders");
        let mut folders = BTreeMap::new();
        for (folder, value) in folders_map {
            match value.as_str().and_then(Tint::parse) {
                Some(color) => {
                    folders.insert(PathBuf::from(folder), color);
                }
                None => self.problem(
                    &format!("projectColors.folders.{folder}"),
                    "expected a color name (blue, amber, teal, violet, rose, lime, orange, pink) or #rrggbb; dropped"
                        .into(),
                ),
            }
        }
        let project_colors = ProjectColors { enabled, folders };

        Settings {
            theme,
            status_fills,
            selection,
            terminal_font,
            ui_font_size,
            alerts,
            shortcuts,
            claude_dirs,
            default_claude_dir,
            folder_claude_dirs,
            ssh_hosts,
            shell,
            recents,
            favorites,
            window,
            last_prompt,
            scrollbar,
            project_colors,
            animate_working,
            animate_ui,
            check_for_updates,
            setup_done,
        }
    }
}

/// A float as JSON without f32 noise (0.4 stays 0.4); whole numbers are written as integers.
fn number(value: f32) -> Value {
    let value: f64 = value.to_string().parse().unwrap_or(0.0);
    if value.fract() == 0.0 && value.abs() < 1e9 { Value::from(value as i64) } else { Value::from(value) }
}

/// Every known key of `settings` as (path, value, differs from the default).
fn leaves(settings: &Settings) -> Vec<(Vec<&'static str>, Value, bool)> {
    let default = Settings::default();
    let mut out = Vec::new();
    let mut add = |path: &[&'static str], value: Value, changed: bool| out.push((path.to_vec(), value, changed));
    add(&["theme"], settings.theme.name().into(), settings.theme != default.theme);
    let fills = [
        ("needsInput", settings.status_fills.needs_input),
        ("working", settings.status_fills.working),
        ("done", settings.status_fills.done),
        ("idle", settings.status_fills.idle),
    ];
    for (key, fill) in fills {
        add(&["statusColors", key], fill.map_or(Value::Null, |color| hex(color).into()), fill.is_some());
    }
    add(
        &["selection"],
        settings.selection.map_or(Value::Null, |color| hex(color).into()),
        settings.selection.is_some(),
    );
    let font = &settings.terminal_font;
    add(&["terminalFont", "family"], font.family.clone().map_or(Value::Null, Value::from), font.family.is_some());
    add(&["terminalFont", "size"], number(font.size), font.size != default.terminal_font.size);
    add(
        &["terminalFont", "lineHeight"],
        number(font.line_height),
        font.line_height != default.terminal_font.line_height,
    );
    add(&["uiFontSize"], number(settings.ui_font_size), settings.ui_font_size != default.ui_font_size);
    let (alerts, base) = (&settings.alerts, &default.alerts);
    for (key, event, base) in
        [("needsInput", alerts.needs_input, base.needs_input), ("finished", alerts.finished, base.finished)]
    {
        add(&["alerts", key, "badge"], event.badge.into(), event.badge != base.badge);
        add(&["alerts", key, "notification"], event.notification.into(), event.notification != base.notification);
        add(&["alerts", key, "sound"], event.sound.into(), event.sound != base.sound);
        add(&["alerts", key, "tone"], event.tone.name().into(), event.tone != base.tone);
    }
    add(
        &["alerts", "finishedAfterSeconds"],
        alerts.finished_after_seconds.into(),
        alerts.finished_after_seconds != base.finished_after_seconds,
    );
    add(&["alerts", "whenVisible"], alerts.when_visible.into(), alerts.when_visible != base.when_visible);
    add(
        &["alerts", "onlyWhenUnfocused"],
        alerts.only_when_unfocused.into(),
        alerts.only_when_unfocused != base.only_when_unfocused,
    );
    add(&["alerts", "playSounds"], alerts.play_sounds.into(), alerts.play_sounds != base.play_sounds);
    add(&["alerts", "volume"], number(alerts.volume), alerts.volume != base.volume);
    let shortcuts: Map<String, Value> =
        settings.shortcuts.iter().map(|(id, key)| (id.clone(), key.clone().map_or(Value::Null, Value::from))).collect();
    add(&["shortcuts"], Value::Object(shortcuts), !settings.shortcuts.is_empty());
    let dirs: Vec<Value> = settings
        .claude_dirs
        .iter()
        .map(|dir| {
            let mut value = serde_json::json!({"name": dir.name, "path": dir.path.to_string_lossy(), "command": dir.command, "args": dir.args});
            if let Some(color) = dir.color {
                value["color"] = color.name().into();
            }
            value
        })
        .collect();
    add(&["claudeDirs"], Value::Array(dirs), !settings.claude_dirs.is_empty());
    let ssh_hosts: Vec<Value> = settings
        .ssh_hosts
        .iter()
        .map(|host| {
            let mut value = serde_json::json!({"name": host.name, "target": host.target, "command": host.command});
            if host.auth != SshAuth::Automatic {
                value["auth"] = host.auth.as_str().into();
            }
            if let Some(port) = host.port {
                value["port"] = port.into();
            }
            value
        })
        .collect();
    add(&["sshHosts"], Value::Array(ssh_hosts), !settings.ssh_hosts.is_empty());
    add(
        &["defaultClaudeDir"],
        settings.default_claude_dir.clone().map_or(Value::Null, Value::from),
        settings.default_claude_dir.is_some(),
    );
    let folders: Map<String, Value> = settings
        .folder_claude_dirs
        .iter()
        .map(|(folder, name)| (folder.to_string_lossy().into_owned(), name.clone().into()))
        .collect();
    add(&["folderClaudeDirs"], Value::Object(folders), !settings.folder_claude_dirs.is_empty());
    add(&["shell", "program"], settings.shell.program.clone().into(), settings.shell.program != default.shell.program);
    add(&["shell", "args"], settings.shell.args.clone().into(), settings.shell.args != default.shell.args);
    add(
        &["recents", "perProject"],
        settings.recents.per_project.into(),
        settings.recents.per_project != default.recents.per_project,
    );
    add(&["recents", "chips"], settings.recents.chips.into(), settings.recents.chips != default.recents.chips);
    let favorites: Vec<Value> =
        settings.favorites.iter().map(|path| path.to_string_lossy().into_owned().into()).collect();
    add(&["favorites"], Value::Array(favorites), !settings.favorites.is_empty());
    let (window, base) = (&settings.window, &default.window);
    add(&["window", "restoreLayout"], window.restore_layout.into(), window.restore_layout != base.restore_layout);
    add(&["window", "resumeSessions"], window.resume_sessions.into(), window.resume_sessions != base.resume_sessions);
    add(
        &["window", "sidebarAtStart"],
        window.sidebar_at_start.name().into(),
        window.sidebar_at_start != base.sidebar_at_start,
    );
    add(&["window", "scrollback"], window.scrollback.into(), window.scrollback != base.scrollback);
    add(&["window", "rightToLeft"], window.right_to_left.into(), window.right_to_left != base.right_to_left);
    let (last_prompt, base) = (&settings.last_prompt, &default.last_prompt);
    add(&["lastPrompt", "enabled"], last_prompt.enabled.into(), last_prompt.enabled != base.enabled);
    add(&["lastPrompt", "size"], number(last_prompt.size), last_prompt.size != base.size);
    add(
        &["lastPrompt", "color"],
        last_prompt.color.map_or(Value::Null, |color| hex(color).into()),
        last_prompt.color != base.color,
    );
    add(
        &["scrollbar", "claude"],
        settings.scrollbar.claude.into(),
        settings.scrollbar.claude != default.scrollbar.claude,
    );
    add(
        &["projectColors", "enabled"],
        settings.project_colors.enabled.into(),
        settings.project_colors.enabled != default.project_colors.enabled,
    );
    let colors: Map<String, Value> = settings
        .project_colors
        .folders
        .iter()
        .map(|(folder, color)| (folder.to_string_lossy().into_owned(), color.to_setting().into()))
        .collect();
    add(&["projectColors", "folders"], Value::Object(colors), !settings.project_colors.folders.is_empty());
    add(&["animateWorking"], settings.animate_working.into(), settings.animate_working != default.animate_working);
    add(&["animateUi"], settings.animate_ui.into(), settings.animate_ui != default.animate_ui);
    add(
        &["checkForUpdates"],
        settings.check_for_updates.into(),
        settings.check_for_updates != default.check_for_updates,
    );
    add(&["setupDone"], settings.setup_done.into(), settings.setup_done != default.setup_done);
    out
}

/// Set (`Some`) or remove (`None`) the value at `path`; objects left empty by a removal are removed too.
fn put(map: &mut Map<String, Value>, path: &[&str], value: Option<Value>) {
    let (key, rest) = path.split_first().expect("a non-empty path");
    if rest.is_empty() {
        match value {
            Some(value) => {
                map.insert((*key).into(), value);
            }
            None => {
                map.remove(*key);
            }
        }
        return;
    }
    if value.is_some() && !matches!(map.get(*key), Some(Value::Object(_))) {
        map.insert((*key).into(), Value::Object(Map::new()));
    }
    if let Some(Value::Object(child)) = map.get_mut(*key) {
        put(child, rest, value);
        if child.is_empty() {
            map.remove(*key);
        }
    }
}

/// Write `settings` to `path` atomically: `raw` with every known key replaced by its value in `settings`, and
/// known keys equal to their default removed (an object left empty is removed too). Unknown keys stay. The
/// whole-list keys (`claudeDirs`, `favorites`, `folderClaudeDirs`, `sshHosts`, `shortcuts`, `projectColors.folders`) are only rewritten when `settings`
/// differs from what `raw` loads as, so hand-written entries that failed validation, and unknown fields in them,
/// survive other changes.
pub fn save(path: &Path, settings: &Settings, raw: &Map<String, Value>) -> io::Result<()> {
    let loaded = Reader { problems: Vec::new() }.settings(raw);
    let untouched = |key: &[&str]| match key {
        ["claudeDirs"] => settings.claude_dirs == loaded.claude_dirs,
        ["favorites"] => settings.favorites == loaded.favorites,
        ["folderClaudeDirs"] => settings.folder_claude_dirs == loaded.folder_claude_dirs,
        ["sshHosts"] => settings.ssh_hosts == loaded.ssh_hosts,
        ["shortcuts"] => settings.shortcuts == loaded.shortcuts,
        ["projectColors", "folders"] => settings.project_colors.folders == loaded.project_colors.folders,
        _ => false,
    };
    let mut out = raw.clone();
    for (key, value, changed) in leaves(settings) {
        if untouched(&key) {
            continue;
        }
        put(&mut out, &key, changed.then_some(value));
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(out)).map_err(io::Error::other)?;
    text.push('\n');
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = std::path::PathBuf::from(temp);
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dir_color_round_trips_and_a_bad_one_keeps_the_dir() {
        let loaded = parse(
            r#"{"claudeDirs": [
            {"name": "A", "path": "/a", "color": "violet"},
            {"name": "B", "path": "/b", "color": "purple"},
            {"name": "C", "path": "/c", "color": 3}
        ]}"#,
        );
        let keys: Vec<_> = loaded.problems.iter().map(|problem| problem.key.as_str()).collect();
        assert_eq!(keys, ["claudeDirs[1].color", "claudeDirs[2].color"]);
        let settings = loaded.settings.unwrap();
        let colors: Vec<_> = settings.claude_dirs.iter().map(|dir| dir.color).collect();
        assert_eq!(colors, [Some(ProjectColor::Violet), None, None]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut changed = settings.clone();
        changed.claude_dirs[1].color = Some(ProjectColor::Teal);
        save(&path, &changed, &loaded.raw).unwrap();
        let written = read_json(&path);
        assert_eq!(written["claudeDirs"][0]["color"], "violet");
        assert_eq!(written["claudeDirs"][1]["color"], "teal");
        assert!(written["claudeDirs"][2].get("color").is_none(), "no color picked, none written");
    }

    #[test]
    fn animate_ui_defaults_on_and_round_trips() {
        assert!(Settings::default().animate_ui);
        let loaded = parse(r#"{"animateUi": false}"#);
        assert!(loaded.problems.is_empty());
        let mut settings = loaded.settings.unwrap();
        assert!(!settings.animate_ui);
        let bad = parse(r#"{"animateUi": "no"}"#);
        assert_eq!(bad.problems.len(), 1);
        assert_eq!(bad.problems[0].key, "animateUi");
        assert!(bad.settings.unwrap().animate_ui);
        settings.animate_ui = true;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save(&path, &settings, &Map::new()).unwrap();
        assert!(read_json(&path).get("animateUi").is_none(), "the default writes nothing");
    }

    #[test]
    fn animate_working_defaults_on_and_round_trips() {
        assert!(Settings::default().animate_working);
        let loaded = parse(r#"{"animateWorking": false}"#);
        assert!(loaded.problems.is_empty());
        let mut settings = loaded.settings.unwrap();
        assert!(!settings.animate_working);
        let bad = parse(r#"{"animateWorking": "no"}"#);
        assert_eq!(bad.problems.len(), 1);
        assert!(bad.settings.unwrap().animate_working);
        settings.animate_working = true;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save(&path, &settings, &Map::new()).unwrap();
        assert!(read_json(&path).get("animateWorking").is_none(), "the default writes nothing");
    }

    #[test]
    fn check_for_updates_defaults_on_and_round_trips() {
        assert!(Settings::default().check_for_updates);
        let loaded = parse(r#"{"checkForUpdates": false}"#);
        assert!(loaded.problems.is_empty());
        let mut settings = loaded.settings.unwrap();
        assert!(!settings.check_for_updates);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save(&path, &settings, &Map::new()).unwrap();
        assert_eq!(read_json(&path).get("checkForUpdates"), Some(&Value::Bool(false)));
        let bad = parse(r#"{"checkForUpdates": "no"}"#);
        assert_eq!(bad.problems.len(), 1);
        assert!(bad.settings.unwrap().check_for_updates);
        settings.check_for_updates = true;
        save(&path, &settings, &Map::new()).unwrap();
        assert!(read_json(&path).get("checkForUpdates").is_none(), "the default writes nothing");
    }

    #[test]
    fn setup_done_defaults_off_and_round_trips() {
        assert!(!Settings::default().setup_done);
        let loaded = parse(r#"{"setupDone": true}"#);
        assert!(loaded.problems.is_empty());
        let mut settings = loaded.settings.unwrap();
        assert!(settings.setup_done);
        let bad = parse(r#"{"setupDone": "yes"}"#);
        assert_eq!(bad.problems.iter().map(|problem| problem.key.as_str()).collect::<Vec<_>>(), ["setupDone"]);
        assert!(!bad.settings.unwrap().setup_done);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let raw = serde_json::json!({"unknownKey": 1, "theme": "Mist light"}).as_object().unwrap().clone();
        save(&path, &settings, &raw).unwrap();
        let written = read_json(&path);
        assert_eq!(written["setupDone"], true);
        assert_eq!(written["unknownKey"], 1, "other keys survive");
        settings.setup_done = false;
        save(&path, &settings, &raw).unwrap();
        assert!(read_json(&path).get("setupDone").is_none(), "the default writes nothing");
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load(&dir.path().join("settings.json"));
        assert_eq!(loaded.settings, Some(Settings::default()));
        assert!(loaded.problems.is_empty());
        assert!(loaded.raw.is_empty());
    }

    #[test]
    fn defaults_match_spec() {
        let s = Settings::default();
        assert_eq!(s.theme, ThemeChoice::Mist);
        assert_eq!(
            (s.terminal_font.size, s.terminal_font.line_height, s.terminal_font.family.clone()),
            (14.0, 1.2, None)
        );
        assert_eq!(s.ui_font_size, 14.0);
        assert_eq!(s.alerts.finished_after_seconds, 1);
        assert_eq!(s.alerts.volume, 0.4);
        assert_eq!(
            (s.alerts.needs_input.tone, s.alerts.finished.tone),
            (AlertTone::FallingBlip, AlertTone::ClassicBlip)
        );
        assert_eq!(s.status_fills, StatusFills::default());
        assert_eq!(s.selection, None);
        assert!(!s.alerts.when_visible && !s.alerts.only_when_unfocused && s.alerts.play_sounds);
    }

    #[test]
    fn default_values_are_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save(&path, &Settings::default(), &Map::new()).unwrap();
        assert_eq!(read_json(&path), serde_json::json!({}));
    }

    #[test]
    fn only_changed_keys_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("settings.json");
        let mut settings = Settings::default();
        settings.status_fills.done = Some(0xaabbcc);
        settings.alerts.volume = 0.7;
        save(&path, &settings, &Map::new()).unwrap();
        assert_eq!(
            read_json(&path),
            serde_json::json!({"statusColors": {"done": "#aabbcc"}, "alerts": {"volume": 0.7}})
        );
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("}\n"));
        assert!(!path.with_file_name("settings.json.tmp").exists());
    }

    #[test]
    fn unknown_keys_survive_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let loaded = parse(r#"{"futureKey": 1, "alerts": {"futureNested": true}}"#);
        assert!(loaded.problems.is_empty());
        save(&path, &loaded.settings.unwrap(), &loaded.raw).unwrap();
        assert_eq!(read_json(&path), serde_json::json!({"futureKey": 1, "alerts": {"futureNested": true}}));
    }

    #[test]
    fn out_of_range_is_clamped_with_a_problem() {
        let loaded = parse(r#"{"terminalFont": {"size": 99}}"#);
        assert_eq!(loaded.settings.unwrap().terminal_font.size, 40.0);
        assert_eq!(loaded.problems.len(), 1);
        assert_eq!(loaded.problems[0].key, "terminalFont.size");
    }

    #[test]
    fn bad_values_fall_back_per_key() {
        let loaded = parse(
            r#"{"theme": "neon", "statusColors": {"done": "green"}, "alerts": {"volume": "loud"}, "uiFontSize": 16}"#,
        );
        let settings = loaded.settings.unwrap();
        let defaults = Settings::default();
        assert_eq!(settings.theme, defaults.theme);
        assert_eq!(settings.status_fills.done, None);
        assert_eq!(settings.alerts.volume, defaults.alerts.volume);
        assert_eq!(settings.ui_font_size, 16.0);
        let keys: Vec<_> = loaded.problems.iter().map(|problem| problem.key.as_str()).collect();
        assert_eq!(keys, ["theme", "statusColors.done", "alerts.volume"]);
    }

    #[test]
    fn broken_file_keeps_last_good() {
        let loaded = parse("{ \"theme\": ");
        assert!(loaded.settings.is_none());
        assert_eq!(loaded.problems.len(), 1);
        assert_eq!(loaded.problems[0].key, "");
        assert!(loaded.problems[0].message.contains("line 1"), "{}", loaded.problems[0].message);
        assert!(parse("[1]").settings.is_none());
    }

    #[test]
    fn round_trip() {
        let settings = Settings {
            theme: ThemeChoice::MistLight,
            status_fills: StatusFills {
                needs_input: Some(0x112233),
                working: Some(0x445566),
                done: Some(0x778899),
                idle: Some(0xaabbcc),
            },
            selection: Some(0xff7eb6),
            terminal_font: TerminalFont { family: Some("Fira Code".into()), size: 17.5, line_height: 1.45 },
            ui_font_size: 16.0,
            alerts: AlertSettings {
                needs_input: EventAlerts { badge: false, notification: false, sound: false, tone: AlertTone::SoftFall },
                finished: EventAlerts { badge: false, notification: true, sound: false, tone: AlertTone::SoftDrop },
                finished_after_seconds: 30,
                when_visible: true,
                only_when_unfocused: true,
                play_sounds: false,
                volume: 0.7,
            },
            ..Settings::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save(&path, &settings, &Map::new()).unwrap();
        let loaded = load(&path);
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(loaded.settings, Some(settings));
    }

    fn phase2_non_default() -> Settings {
        let mut settings = Settings::default();
        settings.shortcuts.insert("toggleSidebar".into(), Some("ctrl-alt-s".into()));
        settings.shortcuts.insert("splitRightClaude".into(), None);
        settings.claude_dirs = vec![
            ClaudeDir {
                name: "Personal".into(),
                path: "~/.claude".into(),
                command: "claude".into(),
                args: vec![],
                color: None,
            },
            ClaudeDir {
                name: "Work".into(),
                path: "~/.claude-work".into(),
                command: "claude-w".into(),
                args: vec!["--model".into(), "opus".into()],
                color: Some(ProjectColor::Rose),
            },
        ];
        settings.default_claude_dir = Some("Work".into());
        settings.folder_claude_dirs.insert("/home/me/work/ops".into(), "Work".into());
        settings.shell = ShellSettings { program: "/bin/zsh".into(), args: vec!["-l".into()] };
        settings.recents = RecentsSettings { per_project: 9, chips: 2 };
        settings.favorites = vec!["/a".into(), "/b".into()];
        settings.window = WindowSettings {
            restore_layout: false,
            resume_sessions: false,
            sidebar_at_start: SidebarAtStart::Compact,
            scrollback: 50000,
            right_to_left: false,
        };
        settings.last_prompt = LastPromptSettings { enabled: false, size: 24.0, color: Some(0xec8a96) };
        settings.animate_working = false;
        settings
    }

    #[test]
    fn phase2_defaults() {
        let s = Settings::default();
        assert!(
            s.shortcuts.is_empty()
                && s.claude_dirs.is_empty()
                && s.folder_claude_dirs.is_empty()
                && s.favorites.is_empty()
        );
        assert_eq!(s.default_claude_dir, None);
        assert_eq!(s.shell, ShellSettings { program: String::new(), args: vec![] });
        assert_eq!(s.recents, RecentsSettings { per_project: 5, chips: 6 });
        assert_eq!(
            s.window,
            WindowSettings {
                restore_layout: true,
                resume_sessions: true,
                sidebar_at_start: SidebarAtStart::Remember,
                scrollback: 10000,
                right_to_left: true,
            }
        );
        assert_eq!(s.last_prompt, LastPromptSettings { enabled: true, size: 18.0, color: None });
    }

    #[test]
    fn sidebar_at_start_compact_overrides_the_saved_state_only_when_asked() {
        for saved in [true, false] {
            assert_eq!(SidebarAtStart::Remember.compact(saved), saved);
            assert!(!SidebarAtStart::Expanded.compact(saved));
            assert!(SidebarAtStart::Compact.compact(saved));
        }
    }

    #[test]
    fn sidebar_at_start_reads_old_names() {
        let read = |name: &str| {
            parse(&format!(r#"{{"window": {{"sidebarAtStart": "{name}"}}}}"#)).settings.unwrap().window.sidebar_at_start
        };
        assert_eq!(read("shown"), SidebarAtStart::Expanded);
        assert_eq!(read("hidden"), SidebarAtStart::Compact);
        assert_eq!(read("expanded"), SidebarAtStart::Expanded);
        assert_eq!(read("compact"), SidebarAtStart::Compact);
    }

    #[test]
    fn sidebar_at_start_writes_new_names() {
        assert_eq!(SidebarAtStart::Expanded.name(), "expanded");
        assert_eq!(SidebarAtStart::Compact.name(), "compact");
        assert_eq!(SidebarAtStart::Remember.name(), "remember");
    }

    #[test]
    fn last_prompt_defaults_on_and_a_bad_value_falls_back_with_a_problem() {
        assert!(parse("{}").settings.unwrap().last_prompt.enabled);
        let off = parse(r#"{"lastPrompt": {"enabled": false}}"#);
        assert!(!off.settings.unwrap().last_prompt.enabled);
        let bad = parse(r#"{"lastPrompt": {"enabled": "yes"}}"#);
        assert_eq!(bad.problems.len(), 1);
        assert!(bad.settings.unwrap().last_prompt.enabled);
    }

    #[test]
    fn scrollbar_in_claude_panes_defaults_on_and_a_bad_value_falls_back_with_a_problem() {
        assert!(parse("{}").settings.unwrap().scrollbar.claude);
        let off = parse(r#"{"scrollbar": {"claude": false}}"#);
        assert!(!off.settings.unwrap().scrollbar.claude);
        let bad = parse(r#"{"scrollbar": {"claude": "no"}}"#);
        assert_eq!(bad.problems.len(), 1);
        assert!(bad.settings.unwrap().scrollbar.claude);
    }

    #[test]
    fn last_prompt_card_size_and_color() {
        let s = parse("{}").settings.unwrap().last_prompt;
        assert_eq!((s.size, s.color), (18.0, None));
        let set = parse(r##"{"lastPrompt": {"size": 22, "color": "#4fc4b4"}}"##).settings.unwrap();
        assert_eq!((set.last_prompt.size, set.last_prompt.color), (22.0, Some(0x4fc4b4)));
        let clamped = parse(r#"{"lastPrompt": {"size": 99}}"#);
        assert_eq!(clamped.settings.unwrap().last_prompt.size, 32.0);
        assert_eq!(clamped.problems.len(), 1);
        let bad = parse(r#"{"lastPrompt": {"color": "teal"}}"#);
        assert_eq!(bad.problems.len(), 1);
        assert_eq!(bad.settings.unwrap().last_prompt.color, None);
    }

    #[test]
    fn phase2_defaults_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save(&path, &Settings::default(), &Map::new()).unwrap();
        assert_eq!(read_json(&path), serde_json::json!({}));
    }

    #[test]
    fn phase2_round_trip() {
        let settings = phase2_non_default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save(&path, &settings, &Map::new()).unwrap();
        let loaded = load(&path);
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(loaded.settings, Some(settings));
    }

    #[test]
    fn a_dropped_claude_dir_survives_an_unrelated_save() {
        let text = r#"{"claudeDirs": [
            {"name": "A", "path": "/a", "comment": "mine"},
            {"name": "A", "path": "/b", "command": "wrapped"}
        ]}"#;
        let loaded = parse(text);
        assert_eq!(loaded.problems.len(), 1);
        let mut settings = loaded.settings.unwrap();
        settings.alerts.volume = 0.7;
        settings.shortcuts.clear();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save(&path, &settings, &loaded.raw).unwrap();
        let written = read_json(&path);
        assert_eq!(
            written["claudeDirs"],
            serde_json::from_str::<Value>(text).unwrap()["claudeDirs"],
            "dropped entry and the unknown field stay"
        );
        assert_eq!(written["alerts"]["volume"], 0.7);
    }

    #[test]
    fn favorites_keep_the_valid_items_with_one_problem_per_bad_one() {
        let loaded = parse(r#"{"favorites": ["/a", "/b", 3]}"#);
        assert_eq!(loaded.problems.len(), 1);
        let mut settings = loaded.settings.unwrap();
        assert_eq!(settings.favorites, [PathBuf::from("/a"), PathBuf::from("/b")]);
        settings.alerts.volume = 0.7;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save(&path, &settings, &loaded.raw).unwrap();
        assert_eq!(read_json(&path)["favorites"], serde_json::json!(["/a", "/b", 3]));
    }

    #[test]
    fn a_changed_whole_leaf_key_is_written() {
        let loaded = parse(r#"{"favorites": ["/a"], "claudeDirs": [{"name": "A", "path": "/a", "comment": 1}]}"#);
        let mut settings = loaded.settings.unwrap();
        settings.favorites.push("/b".into());
        settings.claude_dirs[0].command = "w".into();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save(&path, &settings, &loaded.raw).unwrap();
        let written = read_json(&path);
        assert_eq!(written["favorites"], serde_json::json!(["/a", "/b"]));
        assert_eq!(written["claudeDirs"][0]["command"], "w");
    }

    #[test]
    fn null_shortcut_survives_round_trip() {
        let loaded = parse(r#"{"shortcuts": {"splitRightClaude": null, "futureAction": "ctrl-x"}}"#);
        assert!(loaded.problems.is_empty());
        let settings = loaded.settings.unwrap();
        assert_eq!(settings.shortcuts.get("splitRightClaude"), Some(&None));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save(&path, &settings, &loaded.raw).unwrap();
        assert_eq!(
            read_json(&path),
            serde_json::json!({"shortcuts": {"splitRightClaude": null, "futureAction": "ctrl-x"}})
        );
    }

    #[test]
    fn claude_dirs_drop_duplicates_with_problem() {
        let loaded = parse(
            r#"{"claudeDirs": [
                {"name": "A", "path": "/a"},
                {"name": "A", "path": "/b"},
                {"name": "B", "path": "/a"},
                "nope",
                {"name": "", "path": "/c"},
                {"name": "C", "path": "/c", "command": "x", "args": ["-y"]}
            ]}"#,
        );
        let dirs = loaded.settings.unwrap().claude_dirs;
        assert_eq!(dirs.len(), 2);
        assert_eq!(
            dirs[0],
            ClaudeDir { name: "A".into(), path: "/a".into(), command: "claude".into(), args: vec![], color: None }
        );
        assert_eq!(dirs[1].args, vec!["-y".to_string()]);
        let keys: Vec<_> = loaded.problems.iter().map(|problem| problem.key.as_str()).collect();
        assert_eq!(keys, ["claudeDirs[1]", "claudeDirs[2]", "claudeDirs[3]", "claudeDirs[4]"]);
    }

    #[test]
    fn folder_memory_for_unknown_dir_is_kept_with_problem() {
        let loaded = parse(
            r#"{"claudeDirs": [{"name": "A", "path": "/a"}], "defaultClaudeDir": "Zed",
                "folderClaudeDirs": {"/x": "A", "/y": "Gone"}}"#,
        );
        let settings = loaded.settings.unwrap();
        assert_eq!(settings.folder_claude_dirs.len(), 2);
        assert_eq!(settings.folder_claude_dirs.get(Path::new("/y")).map(String::as_str), Some("Gone"));
        let keys: Vec<_> = loaded.problems.iter().map(|problem| problem.key.as_str()).collect();
        assert_eq!(keys, ["defaultClaudeDir", "folderClaudeDirs./y"]);
        assert_eq!(settings.default_claude_dir.as_deref(), Some("Zed"));
    }

    #[test]
    fn ranges_clamp() {
        let loaded = parse(
            r#"{"recents": {"perProject": 99, "chips": -3}, "window": {"scrollback": 5, "sidebarAtStart": "sideways"}}"#,
        );
        let settings = loaded.settings.unwrap();
        assert_eq!(settings.recents, RecentsSettings { per_project: 20, chips: 0 });
        assert_eq!(settings.window.scrollback, 1000);
        assert_eq!(settings.window.sidebar_at_start, SidebarAtStart::Remember);
        let keys: Vec<_> = loaded.problems.iter().map(|problem| problem.key.as_str()).collect();
        assert_eq!(keys, ["recents.perProject", "recents.chips", "window.sidebarAtStart", "window.scrollback"]);
        assert_eq!(parse(r#"{"window": {"scrollback": 1000000}}"#).settings.unwrap().window.scrollback, 100000);
    }

    #[test]
    fn project_colors_load_tolerantly() {
        let loaded = parse(
            r##"{"projectColors": {"enabled": false, "folders": {"/a": "amber", "/b": "mauve", "/c": 3, "/d": "#a1b2c3", "/e": "#12"}}}"##,
        );
        let s = loaded.settings.unwrap();
        assert!(!s.project_colors.enabled);
        assert_eq!(s.project_colors.folders.get(Path::new("/a")), Some(&Tint::Palette(ProjectColor::Amber)));
        assert_eq!(s.project_colors.folders.get(Path::new("/d")), Some(&Tint::Custom(0xa1b2c3)));
        assert_eq!(s.project_colors.folders.len(), 2);
        assert_eq!(loaded.problems.len(), 3, "{:?}", loaded.problems);
        let loaded = parse(r#"{"projectColors": {"folders": []}}"#);
        let s = loaded.settings.unwrap();
        assert!(s.project_colors.enabled && s.project_colors.folders.is_empty());
        assert_eq!(loaded.problems.len(), 1);
    }

    #[test]
    fn project_color_resolves_pick_then_auto_then_off() {
        let mut s = Settings::default();
        let root = Path::new("/r/app");
        assert_eq!(project_color(&s, root), Some(Tint::Palette(ProjectColor::auto(root))));
        s.project_colors.folders.insert(root.into(), Tint::Palette(ProjectColor::Lime));
        assert_eq!(project_color(&s, root), Some(Tint::Palette(ProjectColor::Lime)));
        s.project_colors.enabled = false;
        assert_eq!(project_color(&s, root), None);
    }

    #[test]
    fn project_colors_save_only_changes_and_keep_bad_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let loaded = parse(r#"{"projectColors": {"folders": {"/x": "mauve"}}, "uiFontSize": 14}"#);
        let mut s = loaded.settings.unwrap();
        s.ui_font_size = 15.0;
        save(&path, &s, &loaded.raw).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("mauve"), "a hand-written bad entry survives an unrelated save: {text}");
        assert!(!text.contains("enabled"), "enabled is written only when changed");
        s.project_colors.folders.insert("/y".into(), Tint::Palette(ProjectColor::Teal));
        s.project_colors.folders.insert("/z".into(), Tint::Custom(0xa1b2c3));
        save(&path, &s, &loaded.raw).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"/y\": \"teal\""), "{text}");
        assert!(text.contains("\"/z\": \"#a1b2c3\""), "{text}");
    }

    #[test]
    fn expand_home_tilde() {
        let home = Path::new("/home/me");
        assert_eq!(expand_home(Path::new("~/x"), Some(home)), PathBuf::from("/home/me/x"));
        assert_eq!(expand_home(Path::new("~"), Some(home)), PathBuf::from("/home/me"));
        assert_eq!(expand_home(Path::new("/abs/x"), Some(home)), PathBuf::from("/abs/x"));
        assert_eq!(expand_home(Path::new("~other/x"), Some(home)), PathBuf::from("~other/x"));
        assert_eq!(expand_home(Path::new("~/x"), None), PathBuf::from("~/x"));
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(parse_hex("#C1E5CD"), Some(0xc1e5cd));
        assert_eq!(hex(0xc1e5cd), "#c1e5cd");
        assert_eq!(parse_hex("c1e5cd"), None);
        assert_eq!(parse_hex("#c1e5c"), None);
    }

    #[test]
    fn ssh_hosts_load_with_the_default_command() {
        let loaded = parse(
            r#"{"sshHosts": [
                {"name": "devbox", "target": "me@devbox"},
                {"name": "lab", "target": "lab", "command": "/opt/gantry/bin/gantry"}
            ]}"#,
        );
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        let hosts = loaded.settings.unwrap().ssh_hosts;
        assert_eq!(
            hosts,
            vec![
                SshHost {
                    name: "devbox".into(),
                    target: "me@devbox".into(),
                    command: DEFAULT_REMOTE_COMMAND.into(),
                    auth: SshAuth::Automatic,
                    port: None,
                },
                SshHost {
                    name: "lab".into(),
                    target: "lab".into(),
                    command: "/opt/gantry/bin/gantry".into(),
                    auth: SshAuth::Automatic,
                    port: None,
                },
            ]
        );
    }

    #[test]
    fn bad_ssh_hosts_are_dropped_with_a_problem_and_the_others_load() {
        let loaded = parse(
            r#"{"sshHosts": [
                {"name": "ok", "target": "ok"},
                {"name": "", "target": "x"},
                {"name": "no-target"},
                {"name": "a/b", "target": "x"},
                {"name": "Local", "target": "x"},
                {"name": "ok", "target": "other"},
                {"name": "empty-command", "target": "x", "command": ""},
                "not an object"
            ]}"#,
        );
        let hosts = loaded.settings.unwrap().ssh_hosts;
        assert_eq!(hosts.iter().map(|host| host.name.as_str()).collect::<Vec<_>>(), ["ok"]);
        let keys: Vec<_> = loaded.problems.iter().map(|problem| problem.key.as_str()).collect();
        assert_eq!(
            keys,
            ["sshHosts[1]", "sshHosts[2]", "sshHosts[3]", "sshHosts[4]", "sshHosts[5]", "sshHosts[6]", "sshHosts[7]"]
        );
        let not_a_list = parse(r#"{"sshHosts": 3}"#);
        assert_eq!(not_a_list.problems[0].key, "sshHosts");
        assert!(not_a_list.settings.unwrap().ssh_hosts.is_empty());
    }

    #[test]
    fn ssh_host_auth_loads_and_a_bad_value_falls_back_with_a_problem() {
        let loaded = parse(
            r#"{"sshHosts": [
                {"name": "a", "target": "a"},
                {"name": "b", "target": "b", "auth": "password"},
                {"name": "c", "target": "c", "auth": "key"},
                {"name": "d", "target": "d", "auth": "fingerprint"},
                {"name": "e", "target": "e", "auth": 3}
            ]}"#,
        );
        let auths: Vec<SshAuth> = loaded.settings.unwrap().ssh_hosts.iter().map(|host| host.auth).collect();
        assert_eq!(
            auths,
            [SshAuth::Automatic, SshAuth::Password, SshAuth::Key, SshAuth::Automatic, SshAuth::Automatic]
        );
        let keys: Vec<&str> = loaded.problems.iter().map(|problem| problem.key.as_str()).collect();
        assert_eq!(keys, ["sshHosts[3].auth", "sshHosts[4].auth"]);
    }

    #[test]
    fn ssh_host_port_loads_and_a_bad_one_falls_back_with_a_problem() {
        let loaded = parse(
            r#"{"sshHosts": [
                {"name": "a", "target": "a"},
                {"name": "b", "target": "b", "port": 2222},
                {"name": "c", "target": "c", "port": 0},
                {"name": "d", "target": "d", "port": 70000},
                {"name": "e", "target": "e", "port": "22"}
            ]}"#,
        );
        let ports: Vec<Option<u16>> = loaded.settings.unwrap().ssh_hosts.iter().map(|host| host.port).collect();
        assert_eq!(ports, [None, Some(2222), None, None, None]);
        let keys: Vec<&str> = loaded.problems.iter().map(|problem| problem.key.as_str()).collect();
        assert_eq!(keys, ["sshHosts[2].port", "sshHosts[3].port", "sshHosts[4].port"]);
    }

    #[test]
    fn a_typed_port_is_empty_or_one_to_65535() {
        assert_eq!(parse_port(""), Ok(None));
        assert_eq!(parse_port(" 2222 "), Ok(Some(2222)));
        assert_eq!(parse_port("65535"), Ok(Some(65535)));
        for bad in ["0", "65536", "-1", "22a", "2 2"] {
            assert!(parse_port(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hand_written_ssh_hosts_survive_a_save_of_something_else() {
        let text =
            r#"{"theme": "Mist", "sshHosts": [{"name": "devbox", "target": "me@devbox", "note": 1}, {"name": ""}]}"#;
        let loaded = parse(text);
        let mut settings = loaded.settings.unwrap();
        settings.animate_working = !settings.animate_working;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save(&path, &settings, &loaded.raw).unwrap();
        let written: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["sshHosts"], serde_json::from_str::<Value>(text).unwrap()["sshHosts"]);
    }

    #[test]
    fn changed_ssh_hosts_are_written() {
        let loaded = parse(r#"{}"#);
        let mut settings = loaded.settings.unwrap();
        settings.ssh_hosts.push(SshHost {
            name: "devbox".into(),
            target: "me@devbox".into(),
            command: DEFAULT_REMOTE_COMMAND.into(),
            auth: SshAuth::Password,
            port: Some(2222),
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save(&path, &settings, &loaded.raw).unwrap();
        let written: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["sshHosts"][0]["name"], "devbox");
        assert_eq!(written["sshHosts"][0]["command"], DEFAULT_REMOTE_COMMAND);
        assert_eq!(written["sshHosts"][0]["auth"], "password");
        assert_eq!(written["sshHosts"][0]["port"], 2222);
    }

    fn host(name: &str, target: &str) -> SshHost {
        SshHost {
            name: name.into(),
            target: target.into(),
            command: DEFAULT_REMOTE_COMMAND.into(),
            auth: SshAuth::Automatic,
            port: None,
        }
    }

    #[test]
    fn add_ssh_host_rejects_clashes_and_bad_targets() {
        let mut hosts = vec![host("a", "a")];
        assert!(add_ssh_host(&mut hosts, host("b", "me@b")).is_ok());
        assert!(add_ssh_host(&mut hosts, host("a", "x")).is_err());
        assert!(add_ssh_host(&mut hosts, host("local", "x")).is_err());
        assert!(add_ssh_host(&mut hosts, host("c", "-oProxyCommand=x")).is_err());
        assert_eq!(hosts.len(), 2);
    }

    #[test]
    fn replace_ssh_host_renames_without_clashing_with_others() {
        let mut hosts = vec![host("a", "a"), host("b", "b")];
        assert!(replace_ssh_host(&mut hosts, "a", host("a", "me@a")).is_ok());
        assert_eq!(hosts[0].target, "me@a");
        assert!(replace_ssh_host(&mut hosts, "a", host("b", "x")).is_err());
        assert!(replace_ssh_host(&mut hosts, "a", host("c", "x")).is_ok());
        assert_eq!(hosts[0].name, "c");
        assert!(replace_ssh_host(&mut hosts, "zzz", host("d", "x")).is_err());
    }

    #[test]
    fn remove_ssh_host_reports_whether_it_existed() {
        let mut hosts = vec![host("a", "a")];
        assert!(remove_ssh_host(&mut hosts, "a"));
        assert!(!remove_ssh_host(&mut hosts, "a"));
        assert!(hosts.is_empty());
    }
}
