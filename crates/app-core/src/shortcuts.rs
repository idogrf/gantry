//! Rebindable shortcuts: the action table with its defaults, resolving the user's overrides, checking a new
//! keystroke, and display labels. Keystrokes use GPUI's syntax (`ctrl-shift-d`).

use std::collections::BTreeMap;

/// Where an action is listed on the Shortcuts page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// Opening, switching and moving tabs.
    Tabs,
    /// Splitting and closing panes.
    Panes,
    /// Moving focus between panes.
    Focus,
    /// Overlays and the window.
    Overlays,
    /// The terminal itself: clipboard and zoom.
    Terminal,
}

impl Group {
    /// The heading shown on the Shortcuts page.
    pub fn title(self) -> &'static str {
        match self {
            Group::Tabs => "Tabs",
            Group::Panes => "Panes",
            Group::Focus => "Focus",
            Group::Overlays => "Overlays and window",
            Group::Terminal => "Terminal",
        }
    }
}

/// One rebindable action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Action {
    /// Key in `settings.json`.
    pub id: &'static str,
    /// Page group.
    pub group: Group,
    /// Name shown on the page.
    pub label: &'static str,
    /// Default on Linux and Windows; `None` = unbound.
    pub linux: Option<&'static str>,
    /// Default on macOS.
    pub macos: Option<&'static str>,
}

const fn action(
    id: &'static str,
    group: Group,
    label: &'static str,
    linux: &'static str,
    macos: &'static str,
) -> Action {
    Action { id, group, label, linux: Some(linux), macos: Some(macos) }
}

/// Every action, in page order.
pub const ACTIONS: &[Action] = &[
    action("newClaudeTab", Group::Tabs, "New Claude session (same folder)", "ctrl-shift-t", "cmd-t"),
    action("newShellTab", Group::Tabs, "New terminal (same folder)", "ctrl-shift-alt-t", "cmd-alt-t"),
    action("nextTab", Group::Tabs, "Next tab", "ctrl-tab", "cmd-shift-]"),
    action("previousTab", Group::Tabs, "Previous tab", "ctrl-shift-tab", "cmd-shift-["),
    action("historyBack", Group::Tabs, "Previous tab in history", "ctrl-shift-pageup", "cmd-["),
    action("historyForward", Group::Tabs, "Next tab in history", "ctrl-shift-pagedown", "cmd-]"),
    action("splitRightClaude", Group::Panes, "Split right (Claude)", "ctrl-shift-d", "cmd-d"),
    action("splitRightShell", Group::Panes, "Split right (terminal)", "ctrl-shift-alt-d", "cmd-alt-d"),
    action("splitDownClaude", Group::Panes, "Split down (Claude)", "ctrl-shift-e", "cmd-e"),
    action("splitDownShell", Group::Panes, "Split down (terminal)", "ctrl-shift-alt-e", "cmd-alt-e"),
    action("closePane", Group::Panes, "Close pane", "ctrl-shift-w", "cmd-w"),
    action("focusLeft", Group::Focus, "Focus left", "ctrl-shift-left", "cmd-alt-left"),
    action("focusRight", Group::Focus, "Focus right", "ctrl-shift-right", "cmd-alt-right"),
    action("focusUp", Group::Focus, "Focus up", "ctrl-shift-up", "cmd-alt-up"),
    action("focusDown", Group::Focus, "Focus down", "ctrl-shift-down", "cmd-alt-down"),
    action("openLauncher", Group::Overlays, "New Claude session…", "ctrl-shift-o", "cmd-o"),
    action("findSession", Group::Overlays, "Find session", "ctrl-shift-f", "cmd-f"),
    action("openRecents", Group::Overlays, "Recent sessions", "ctrl-shift-r", "cmd-r"),
    action("toggleSidebar", Group::Overlays, "Toggle sidebar", "ctrl-shift-b", "cmd-b"),
    action("toggleGitPanel", Group::Overlays, "Git panel", "ctrl-shift-g", "cmd-shift-g"),
    action("openSettings", Group::Overlays, "Settings", "ctrl-,", "cmd-,"),
    action("copy", Group::Terminal, "Copy", "ctrl-shift-c", "cmd-c"),
    action("paste", Group::Terminal, "Paste", "ctrl-shift-v", "cmd-v"),
    action("zoomIn", Group::Terminal, "Zoom in", "ctrl-=", "cmd-="),
    action("zoomOut", Group::Terminal, "Zoom out", "ctrl--", "cmd--"),
    action("zoomReset", Group::Terminal, "Reset zoom", "ctrl-0", "cmd-0"),
];

/// Which defaults and labels apply. Windows uses `Linux`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// Linux and Windows.
    Linux,
    /// macOS.
    MacOs,
}

impl Platform {
    /// The platform this build runs on.
    pub fn current() -> Platform {
        if cfg!(target_os = "macos") { Platform::MacOs } else { Platform::Linux }
    }
}

/// A keystroke split into modifiers and key, normalised (`super`/`win` are `cmd`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Parsed<'a> {
    ctrl: bool,
    alt: bool,
    shift: bool,
    cmd: bool,
    /// Empty for a modifier set.
    key: &'a str,
}

fn parse(keystroke: &str) -> Option<Parsed<'_>> {
    let mut parsed = Parsed { ctrl: false, alt: false, shift: false, cmd: false, key: "" };
    let mut rest = keystroke;
    loop {
        let (word, tail) = match rest.split_once('-') {
            Some((word, tail)) if !tail.is_empty() => (word, Some(tail)),
            _ => (rest, None),
        };
        let flag = match word {
            "ctrl" => &mut parsed.ctrl,
            "alt" => &mut parsed.alt,
            "shift" => &mut parsed.shift,
            "cmd" | "super" | "win" => &mut parsed.cmd,
            _ => break,
        };
        if *flag {
            return None;
        }
        *flag = true;
        match tail {
            Some(tail) => rest = tail,
            None => return Some(parsed),
        }
    }
    (valid_key(rest)).then_some(Parsed { key: rest, ..parsed })
}

fn valid_key(key: &str) -> bool {
    const NAMED: &[&str] = &[
        "tab",
        "enter",
        "escape",
        "backspace",
        "delete",
        "space",
        "up",
        "down",
        "left",
        "right",
        "home",
        "end",
        "pageup",
        "pagedown",
        "insert",
    ];
    if NAMED.contains(&key) {
        return true;
    }
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return c.is_ascii_graphic();
    }
    key.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()).is_some_and(|n| (1..=35).contains(&n))
}

impl Parsed<'_> {
    fn canonical(&self) -> String {
        let mut out = String::new();
        for (on, name) in [(self.ctrl, "ctrl"), (self.alt, "alt"), (self.shift, "shift"), (self.cmd, "cmd")] {
            if on {
                out.push_str(name);
                out.push('-');
            }
        }
        out.push_str(self.key);
        if self.key.is_empty() {
            out.pop();
        }
        out
    }
}

fn canonical(keystroke: &str) -> String {
    parse(keystroke).map_or_else(|| keystroke.to_string(), |parsed| parsed.canonical())
}

fn default_of(action: &Action, platform: Platform) -> Option<&'static str> {
    match platform {
        Platform::Linux => action.linux,
        Platform::MacOs => action.macos,
    }
}

/// The keystroke `action` is on: the user's override if it is usable, else the default. `None` = unbound (or an
/// unknown action). An override that `check` refuses as invalid or reserved is ignored.
pub fn current(action: &str, overrides: &BTreeMap<String, Option<String>>, platform: Platform) -> Option<String> {
    let entry = ACTIONS.iter().find(|entry| entry.id == action)?;
    match overrides.get(action) {
        Some(None) => None,
        Some(Some(keystroke)) if !matches!(check(keystroke, action, &[]), Check::Invalid) => Some(canonical(keystroke)),
        _ => default_of(entry, platform).map(String::from),
    }
}

/// Whether `keystroke` is `action`'s default on `platform` (spelled in any modifier order).
pub fn is_default(action: &str, keystroke: &str, platform: Platform) -> bool {
    ACTIONS
        .iter()
        .find(|entry| entry.id == action)
        .and_then(|entry| default_of(entry, platform))
        .is_some_and(|default| canonical(default) == canonical(keystroke))
}

/// The alias that stays bound while Zoom in is on its default: `ctrl-+` is what Ctrl+Shift+= reports.
fn zoom_alias(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => "ctrl-+",
        Platform::MacOs => "cmd-+",
    }
}

/// The resolved bindings as (action id, keystroke). Fixed
/// aliases of defaults are included: `ctrl-+` for Zoom in, and on macOS `ctrl-tab` / `ctrl-shift-tab` for the tab
/// keys. Actions the user set to `null` have none.
pub fn bindings(overrides: &BTreeMap<String, Option<String>>, platform: Platform) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for entry in ACTIONS {
        let Some(keystroke) = current(entry.id, overrides, platform) else { continue };
        let on_default = default_of(entry, platform) == Some(keystroke.as_str());
        out.push((entry.id, keystroke));
        let alias = match entry.id {
            "zoomIn" => Some(zoom_alias(platform)),
            "nextTab" if platform == Platform::MacOs => Some("ctrl-tab"),
            "previousTab" if platform == Platform::MacOs => Some("ctrl-shift-tab"),
            _ => None,
        };
        if let Some(alias) = alias.filter(|_| on_default) {
            out.push((entry.id, alias.to_string()));
        }
    }
    out
}

/// What `check` found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// Usable.
    Ok,
    /// Programs in the terminal use it: usable once the user confirms; the reason is shown to the user.
    Reserved(&'static str),
    /// Another action has it (its id).
    Conflict(&'static str),
    /// Not a keystroke.
    Invalid,
}

/// Whether `keystroke` (GPUI syntax, e.g. `ctrl-shift-d`) is usable for `action`, given the `current` resolved
/// bindings.
pub fn check(keystroke: &str, action: &str, current: &[(&'static str, String)]) -> Check {
    let Some(parsed) = parse(keystroke) else { return Check::Invalid };
    if parsed.key.is_empty() {
        return Check::Invalid;
    }
    if matches!(keystroke, "escape" | "tab" | "shift-tab" | "enter" | "backspace") {
        return Check::Reserved("belongs to the terminal");
    }
    if !(parsed.ctrl || parsed.alt || parsed.cmd) {
        return Check::Reserved("needs Ctrl, Alt or Cmd");
    }
    let letter = parsed.key.len() == 1 && parsed.key.as_bytes()[0].is_ascii_alphabetic();
    if parsed.ctrl && !parsed.alt && !parsed.shift && !parsed.cmd && letter {
        return Check::Reserved("belongs to the terminal");
    }
    match conflict(keystroke, action, current) {
        Some(id) => Check::Conflict(id),
        None => Check::Ok,
    }
}

/// The other action in `current` that has `keystroke`, if any.
pub fn conflict(keystroke: &str, action: &str, current: &[(&'static str, String)]) -> Option<&'static str> {
    let wanted = canonical(keystroke);
    current.iter().find(|(id, bound)| *id != action && canonical(bound) == wanted).map(|(id, _)| *id)
}

/// Whether binding `keystroke` takes it away from programs in the terminal.
pub fn takes_from_terminal(keystroke: &str) -> bool {
    matches!(check(keystroke, "", &[]), Check::Reserved(_))
}

/// Display text: `Ctrl+Shift+R` (Linux, Windows), `⌘⇧R` (macOS); `Not set` for `None`.
pub fn label(keystroke: Option<&str>, platform: Platform) -> String {
    let Some(keystroke) = keystroke else { return "Not set".into() };
    let Some(parsed) = parse(keystroke) else { return keystroke.to_string() };
    let key = match parsed.key {
        "" => String::new(),
        "left" => "←".into(),
        "right" => "→".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "pageup" => "PageUp".into(),
        "pagedown" => "PageDown".into(),
        key if key.chars().count() == 1 => key.to_uppercase(),
        key => {
            let mut chars = key.chars();
            chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
        }
    };
    match platform {
        Platform::MacOs => {
            let mods: String = [(parsed.ctrl, "⌃"), (parsed.alt, "⌥"), (parsed.cmd, "⌘"), (parsed.shift, "⇧")]
                .into_iter()
                .filter_map(|(on, symbol)| on.then_some(symbol))
                .collect();
            format!("{mods}{key}")
        }
        Platform::Linux => {
            let mut parts: Vec<&str> =
                [(parsed.ctrl, "Ctrl"), (parsed.shift, "Shift"), (parsed.alt, "Alt"), (parsed.cmd, "Super")]
                    .into_iter()
                    .filter_map(|(on, name)| on.then_some(name))
                    .collect();
            if !key.is_empty() {
                parts.push(&key);
            }
            parts.join("+")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_say_terminal_and_session() {
        let label = |id: &str| ACTIONS.iter().find(|a| a.id == id).unwrap().label;
        assert_eq!(label("newClaudeTab"), "New Claude session (same folder)");
        assert_eq!(label("newShellTab"), "New terminal (same folder)");
        assert_eq!(label("splitRightShell"), "Split right (terminal)");
        assert_eq!(label("splitDownShell"), "Split down (terminal)");
        assert_eq!(label("openLauncher"), "New Claude session…");
    }

    fn none() -> BTreeMap<String, Option<String>> {
        BTreeMap::new()
    }

    fn of(pairs: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
        pairs.iter().map(|(id, key)| (id.to_string(), key.map(String::from))).collect()
    }

    fn ks(list: &[(&'static str, String)], id: &str) -> Vec<String> {
        list.iter().filter(|(a, _)| *a == id).map(|(_, k)| k.clone()).collect()
    }

    #[test]
    fn defaults_match_todays_bindings() {
        // From workspace_view / terminal_view before this change, minus Ctrl+PageUp/PageDown for the tabs.
        let linux = bindings(&none(), Platform::Linux);
        let expected = [
            ("newClaudeTab", "ctrl-shift-t"),
            ("newShellTab", "ctrl-shift-alt-t"),
            ("splitRightClaude", "ctrl-shift-d"),
            ("splitRightShell", "ctrl-shift-alt-d"),
            ("splitDownClaude", "ctrl-shift-e"),
            ("splitDownShell", "ctrl-shift-alt-e"),
            ("closePane", "ctrl-shift-w"),
            ("focusLeft", "ctrl-shift-left"),
            ("focusRight", "ctrl-shift-right"),
            ("focusUp", "ctrl-shift-up"),
            ("focusDown", "ctrl-shift-down"),
            ("previousTab", "ctrl-shift-tab"),
            ("nextTab", "ctrl-tab"),
            ("historyBack", "ctrl-shift-pageup"),
            ("historyForward", "ctrl-shift-pagedown"),
            ("toggleSidebar", "ctrl-shift-b"),
            ("findSession", "ctrl-shift-f"),
            ("openLauncher", "ctrl-shift-o"),
            ("openRecents", "ctrl-shift-r"),
            ("toggleGitPanel", "ctrl-shift-g"),
            ("openSettings", "ctrl-,"),
            ("copy", "ctrl-shift-c"),
            ("paste", "ctrl-shift-v"),
            ("zoomIn", "ctrl-="),
            ("zoomOut", "ctrl--"),
            ("zoomReset", "ctrl-0"),
        ];
        for (id, key) in expected {
            assert_eq!(ks(&linux, id).first().map(String::as_str), Some(key), "{id}");
        }
        assert_eq!(linux.len(), expected.len() + 1, "26 single bindings, the zoom alias");
        let mac = bindings(&none(), Platform::MacOs);
        assert_eq!(ks(&mac, "nextTab"), ["cmd-shift-]", "ctrl-tab"]);
        assert_eq!(ks(&mac, "previousTab"), ["cmd-shift-[", "ctrl-shift-tab"]);
        assert_eq!(ks(&mac, "newClaudeTab"), ["cmd-t"]);
        assert_eq!(ks(&mac, "openSettings"), ["cmd-,"]);
        assert_eq!(ks(&mac, "toggleGitPanel"), ["cmd-shift-g"]);
    }

    #[test]
    fn is_default_ignores_modifier_order() {
        assert!(is_default("newShellTab", "ctrl-alt-shift-t", Platform::Linux));
        assert!(is_default("newShellTab", "ctrl-shift-alt-t", Platform::Linux));
        assert!(!is_default("newShellTab", "ctrl-shift-t", Platform::Linux));
        assert!(!is_default("nope", "ctrl-shift-t", Platform::Linux));
    }

    #[test]
    fn old_tab_ids_in_settings_are_ignored() {
        let overrides: BTreeMap<String, Option<String>> = [
            ("goToTab".to_string(), Some("alt".to_string())),
            ("moveTabLeft".to_string(), Some("ctrl-shift-pageup".to_string())),
            ("toggleSidebar".to_string(), Some("ctrl-alt-s".to_string())),
        ]
        .into();
        let bound = bindings(&overrides, Platform::Linux);
        assert!(bound.iter().all(|(id, _)| !matches!(*id, "goToTab" | "moveTabLeft" | "moveTabRight")));
        assert!(bound.iter().any(|(id, key)| *id == "toggleSidebar" && key == "ctrl-alt-s"));
    }

    #[test]
    fn zoom_alias_only_on_default() {
        assert_eq!(ks(&bindings(&none(), Platform::Linux), "zoomIn"), ["ctrl-=", "ctrl-+"]);
        assert_eq!(ks(&bindings(&of(&[("zoomIn", Some("ctrl-alt-z"))]), Platform::Linux), "zoomIn"), ["ctrl-alt-z"]);
        assert_eq!(ks(&bindings(&of(&[("zoomIn", Some("ctrl-="))]), Platform::Linux), "zoomIn"), ["ctrl-=", "ctrl-+"]);
    }

    #[test]
    fn override_and_null() {
        let overrides =
            of(&[("toggleSidebar", Some("ctrl-alt-s")), ("splitRightClaude", None), ("nope", Some("ctrl-alt-q"))]);
        let list = bindings(&overrides, Platform::Linux);
        assert_eq!(ks(&list, "toggleSidebar"), ["ctrl-alt-s"]);
        assert!(ks(&list, "splitRightClaude").is_empty());
        assert!(list.iter().all(|(id, _)| *id != "nope"));
        assert_eq!(current("toggleSidebar", &overrides, Platform::Linux).as_deref(), Some("ctrl-alt-s"));
        assert_eq!(current("splitRightClaude", &overrides, Platform::Linux), None);
        assert_eq!(current("closePane", &overrides, Platform::Linux).as_deref(), Some("ctrl-shift-w"));
        // A key taken from the terminal is used; a keystroke that is not one falls back to the default.
        let bad = of(&[("closePane", Some("ctrl-w")), ("copy", Some("bogus-key"))]);
        assert_eq!(current("closePane", &bad, Platform::Linux).as_deref(), Some("ctrl-w"));
        assert_eq!(current("copy", &bad, Platform::Linux).as_deref(), Some("ctrl-shift-c"));
    }

    #[test]
    fn terminal_keys_are_marked_and_still_clash() {
        assert!(takes_from_terminal("ctrl-w") && takes_from_terminal("enter") && takes_from_terminal("t"));
        assert!(!takes_from_terminal("ctrl-shift-w") && !takes_from_terminal("bogus-key"));
        let taken = bindings(&of(&[("closePane", Some("ctrl-w"))]), Platform::Linux);
        assert!(taken.iter().any(|(id, key)| *id == "closePane" && key == "ctrl-w"));
        assert_eq!(conflict("ctrl-w", "copy", &taken), Some("closePane"));
        assert_eq!(conflict("ctrl-w", "closePane", &taken), None, "its own key is no clash");
    }

    #[test]
    fn check_reserved() {
        let now = bindings(&none(), Platform::Linux);
        for key in ["ctrl-c", "ctrl-d", "ctrl-r", "ctrl-w", "ctrl-z", "ctrl-l", "ctrl-a", "ctrl-e", "ctrl-u", "ctrl-k"]
        {
            assert!(matches!(check(key, "closePane", &now), Check::Reserved(_)), "{key}");
        }
        for key in ["escape", "tab", "shift-tab", "enter", "backspace", "t", "shift-t", "f5", "shift-left"] {
            assert!(matches!(check(key, "closePane", &now), Check::Reserved(_)), "{key}");
        }
        assert!(matches!(check("alt", "newClaudeTab", &now), Check::Invalid));
    }

    #[test]
    fn check_conflict() {
        let now = bindings(&none(), Platform::Linux);
        assert_eq!(check("ctrl-shift-r", "closePane", &now), Check::Conflict("openRecents"));
        assert_eq!(check("shift-ctrl-r", "closePane", &now), Check::Conflict("openRecents"), "order does not matter");
        assert_eq!(check("ctrl-shift-r", "openRecents", &now), Check::Ok, "its own key");
        assert_eq!(check("ctrl-+", "closePane", &now), Check::Conflict("zoomIn"));
    }

    #[test]
    fn check_ok() {
        let now = bindings(&none(), Platform::Linux);
        for key in ["ctrl-alt-s", "ctrl-shift-j", "alt-x", "ctrl-f5", "ctrl-shift-home", "cmd-k", "ctrl-."] {
            assert_eq!(check(key, "toggleSidebar", &now), Check::Ok, "{key}");
        }
    }

    #[test]
    fn check_invalid() {
        let now = bindings(&none(), Platform::Linux);
        for key in ["", "ctrl-", "ctrl-banana", "ctrl-ctrl-a", "ctrl-shift", "f99"] {
            assert_eq!(check(key, "toggleSidebar", &now), Check::Invalid, "{key:?}");
        }
        assert_eq!(check("ctrl-alt", "toggleSidebar", &now), Check::Invalid, "a modifier set has no key");
    }

    #[test]
    fn labels_linux_and_macos() {
        assert_eq!(label(Some("ctrl-shift-r"), Platform::Linux), "Ctrl+Shift+R");
        assert_eq!(label(Some("ctrl-shift-alt-t"), Platform::Linux), "Ctrl+Shift+Alt+T");
        assert_eq!(label(Some("ctrl-alt-s"), Platform::Linux), "Ctrl+Alt+S");
        assert_eq!(label(Some("ctrl-shift-left"), Platform::Linux), "Ctrl+Shift+←");
        assert_eq!(label(Some("ctrl-shift-pagedown"), Platform::Linux), "Ctrl+Shift+PageDown");
        assert_eq!(label(Some("ctrl-,"), Platform::Linux), "Ctrl+,");
        assert_eq!(label(Some("ctrl--"), Platform::Linux), "Ctrl+-");
        assert_eq!(label(Some("ctrl-tab"), Platform::Linux), "Ctrl+Tab");
        assert_eq!(label(Some("alt"), Platform::Linux), "Alt");
        assert_eq!(label(Some("cmd-shift-r"), Platform::MacOs), "⌘⇧R");
        assert_eq!(label(Some("cmd-alt-t"), Platform::MacOs), "⌥⌘T");
        assert_eq!(label(Some("cmd-left"), Platform::MacOs), "⌘←");
        assert_eq!(label(None, Platform::Linux), "Not set");
        assert_eq!(label(None, Platform::MacOs), "Not set");
    }

    #[test]
    fn every_action_has_defaults_that_pass_the_checks() {
        for platform in [Platform::Linux, Platform::MacOs] {
            let list = bindings(&none(), platform);
            for entry in ACTIONS {
                let key = current(entry.id, &none(), platform).unwrap();
                assert_eq!(check(&key, entry.id, &list), Check::Ok, "{} {key}", entry.id);
            }
            let unique: std::collections::BTreeSet<_> = list.iter().map(|(_, key)| key.clone()).collect();
            assert_eq!(unique.len(), list.len(), "no key is bound twice");
        }
    }
}
