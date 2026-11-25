use super::{
    ClosePane, FindSession, FocusDown, FocusLeft, FocusRight, FocusUp, HistoryBack, HistoryForward, NewClaudeTab,
    NewShellTab, NextTab, OpenLauncher, OpenPreview, OpenRecents, OpenSettings, PreviousTab, SplitDownClaude,
    SplitDownShell, SplitRightClaude, SplitRightShell, ToggleGitPanel, ToggleSidebar, WorkspaceView,
};

use app_core::split_tree::{Axis, Direction, NodeId};
use gpui::{Context, Window};

use app_core::settings::Settings;
use app_core::shortcuts::{self, Platform};

use crate::launcher_view::LauncherBrowse;
use crate::terminal_view::terminal_binding;

impl WorkspaceView {
    pub(super) fn new_claude_tab(&mut self, _: &NewClaudeTab, window: &mut Window, cx: &mut Context<Self>) {
        self.open(true, None, window, cx)
    }

    pub(super) fn new_shell_tab(&mut self, _: &NewShellTab, window: &mut Window, cx: &mut Context<Self>) {
        self.open(false, None, window, cx)
    }

    pub(super) fn split_right_claude(&mut self, _: &SplitRightClaude, window: &mut Window, cx: &mut Context<Self>) {
        self.open(true, Some(Axis::Horizontal), window, cx)
    }

    pub(super) fn split_right_shell(&mut self, _: &SplitRightShell, window: &mut Window, cx: &mut Context<Self>) {
        self.open(false, Some(Axis::Horizontal), window, cx)
    }

    pub(super) fn split_down_claude(&mut self, _: &SplitDownClaude, window: &mut Window, cx: &mut Context<Self>) {
        self.open(true, Some(Axis::Vertical), window, cx)
    }

    pub(super) fn split_down_shell(&mut self, _: &SplitDownShell, window: &mut Window, cx: &mut Context<Self>) {
        self.open(false, Some(Axis::Vertical), window, cx)
    }

    pub(super) fn close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace.active_tab().is_some_and(|tab| tab.settings) {
            self.close_settings(window, cx);
        } else if let Some(leaf) = self.workspace.focused() {
            self.confirm_close(vec![leaf], window, cx);
        }
    }

    /// Open the preview pane for the focused Claude session.
    pub(super) fn open_preview_action(&mut self, _: &OpenPreview, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(leaf) = self.workspace.focused() {
            self.open_preview_for(leaf, window, cx);
        }
    }

    /// Open (or show) the preview pane that follows the Claude pane `claude` (Task 8 fills this in).
    pub(super) fn open_preview_for(&mut self, _claude: NodeId, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn focus_dir(&mut self, direction: Direction, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace.focus_direction(direction).is_some() {
            self.focus_current(window, cx);
            self.save(cx);
        }
    }

    pub(super) fn focus_left(&mut self, _: &FocusLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_dir(Direction::Left, window, cx)
    }

    pub(super) fn focus_right(&mut self, _: &FocusRight, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_dir(Direction::Right, window, cx)
    }

    pub(super) fn focus_up(&mut self, _: &FocusUp, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_dir(Direction::Up, window, cx)
    }

    pub(super) fn focus_down(&mut self, _: &FocusDown, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_dir(Direction::Down, window, cx)
    }

    fn cycle(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let order = self.sidebar.read(cx).leaf_order();
        let away = self.away_leaves(cx);
        self.workspace.cycle_tab_in(&order, delta, |leaf| away.contains(&leaf));
        self.focus_current(window, cx);
        self.save(cx);
    }

    pub(super) fn previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle(-1, window, cx)
    }

    pub(super) fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle(1, window, cx)
    }

    /// Show the tab `delta` places away in the Back / Forward history (-1 Back, 1 Forward).
    pub(super) fn history(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let away = self.away_leaves(cx);
        if self.workspace.history_step(delta, |leaf| away.contains(&leaf)) {
            self.focus_current(window, cx);
            self.save(cx);
        }
    }

    pub(super) fn history_back(&mut self, _: &HistoryBack, window: &mut Window, cx: &mut Context<Self>) {
        self.history(-1, window, cx)
    }

    pub(super) fn history_forward(&mut self, _: &HistoryForward, window: &mut Window, cx: &mut Context<Self>) {
        self.history(1, window, cx)
    }
}

/// Every key binding: the workspace and terminal actions at their resolved keystrokes (defaults, the user's overrides
/// from `settings.json`, `null` = none). The text fields and menus handle their keys themselves, so nothing else is bound.
pub(crate) fn all_bindings(settings: &Settings, platform: Platform) -> Vec<gpui::KeyBinding> {
    shortcuts::bindings(&settings.shortcuts, platform)
        .into_iter()
        .filter_map(|(id, key)| workspace_binding(id, &key).or_else(|| terminal_binding(id, &key)))
        // Browse inside the launcher; its own context wins over the Workspace binding of the same chord (Cmd+O on macOS).
        .chain([gpui::KeyBinding::new("secondary-o", LauncherBrowse, Some("Launcher"))])
        .collect()
}

fn workspace_binding(id: &str, key: &str) -> Option<gpui::KeyBinding> {
    use gpui::KeyBinding;
    let context = Some("Workspace");
    Some(match id {
        "newClaudeTab" => KeyBinding::new(key, NewClaudeTab, context),
        "newShellTab" => KeyBinding::new(key, NewShellTab, context),
        "nextTab" => KeyBinding::new(key, NextTab, context),
        "previousTab" => KeyBinding::new(key, PreviousTab, context),
        "historyBack" => KeyBinding::new(key, HistoryBack, context),
        "historyForward" => KeyBinding::new(key, HistoryForward, context),
        "splitRightClaude" => KeyBinding::new(key, SplitRightClaude, context),
        "splitRightShell" => KeyBinding::new(key, SplitRightShell, context),
        "splitDownClaude" => KeyBinding::new(key, SplitDownClaude, context),
        "splitDownShell" => KeyBinding::new(key, SplitDownShell, context),
        "closePane" => KeyBinding::new(key, ClosePane, context),
        "openPreview" => KeyBinding::new(key, OpenPreview, context),
        "focusLeft" => KeyBinding::new(key, FocusLeft, context),
        "focusRight" => KeyBinding::new(key, FocusRight, context),
        "focusUp" => KeyBinding::new(key, FocusUp, context),
        "focusDown" => KeyBinding::new(key, FocusDown, context),
        "openLauncher" => KeyBinding::new(key, OpenLauncher, context),
        "findSession" => KeyBinding::new(key, FindSession, context),
        "openRecents" => KeyBinding::new(key, OpenRecents, context),
        "toggleSidebar" => KeyBinding::new(key, ToggleSidebar, context),
        "toggleGitPanel" => KeyBinding::new(key, ToggleGitPanel, context),
        "openSettings" => KeyBinding::new(key, OpenSettings, context),
        _ => return None,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use gpui::Keystroke;

    fn action_for(keys: &str) -> Option<&'static str> {
        action_in(&all_bindings(&Settings::default(), Platform::Linux), keys)
    }

    fn action_in(bindings: &[gpui::KeyBinding], keys: &str) -> Option<&'static str> {
        let keystroke = Keystroke::parse(keys).unwrap();
        bindings
            .iter()
            .find(|binding| binding.match_keystrokes(std::slice::from_ref(&keystroke)) == Some(false))
            .map(|binding| binding.action().name())
    }

    #[test]
    fn workspace_shortcuts_match_the_spec() {
        for (keys, action) in [
            ("ctrl-shift-t", "workspace::NewClaudeTab"),
            ("ctrl-shift-alt-t", "workspace::NewShellTab"),
            ("ctrl-shift-d", "workspace::SplitRightClaude"),
            ("ctrl-shift-alt-d", "workspace::SplitRightShell"),
            ("ctrl-shift-e", "workspace::SplitDownClaude"),
            ("ctrl-shift-alt-e", "workspace::SplitDownShell"),
            ("ctrl-shift-w", "workspace::ClosePane"),
            ("ctrl-shift-left", "workspace::FocusLeft"),
            ("ctrl-shift-down", "workspace::FocusDown"),
            ("ctrl-tab", "workspace::NextTab"),
            ("ctrl-shift-tab", "workspace::PreviousTab"),
            ("ctrl-shift-b", "workspace::ToggleSidebar"),
            ("ctrl-shift-f", "workspace::FindSession"),
            ("ctrl-shift-o", "workspace::OpenLauncher"),
            ("ctrl-shift-r", "workspace::OpenRecents"),
            ("ctrl-shift-g", "workspace::ToggleGitPanel"),
            ("ctrl-,", "workspace::OpenSettings"),
        ] {
            assert_eq!(action_for(keys), Some(action), "{keys}");
        }
    }

    #[test]
    fn plain_control_keys_stay_with_the_pane() {
        for keys in ["ctrl-c", "ctrl-r", "ctrl-d", "ctrl-w", "escape", "shift-tab", "tab", "alt-1"] {
            assert_eq!(action_for(keys), None, "{keys}");
        }
    }

    #[test]
    fn ctrl_page_keys_no_longer_switch_tabs() {
        assert_eq!(action_for("ctrl-pageup"), None);
        assert_eq!(action_for("ctrl-pagedown"), None);
    }

    #[test]
    fn no_default_binding_is_reserved() {
        let bindings = shortcuts::bindings(&Default::default(), Platform::Linux);
        for (id, key) in &bindings {
            if key == "ctrl-+" {
                continue;
            }
            assert!(!matches!(shortcuts::check(key, id, &[]), shortcuts::Check::Reserved(_)), "{id} {key}");
        }
    }

    #[test]
    fn an_override_replaces_the_default_and_the_old_keystroke_is_gone() {
        let mut settings = Settings::default();
        settings.shortcuts.insert("toggleSidebar".into(), Some("ctrl-alt-s".into()));
        let bindings = all_bindings(&settings, Platform::Linux);
        assert_eq!(action_in(&bindings, "ctrl-alt-s"), Some("workspace::ToggleSidebar"));
        assert_eq!(action_in(&bindings, "ctrl-shift-b"), None);
        assert_eq!(action_in(&bindings, "ctrl-shift-f"), Some("workspace::FindSession"), "others keep theirs");
    }

    #[test]
    fn open_preview_is_unbound_until_the_user_binds_it() {
        let bindings = all_bindings(&Settings::default(), Platform::Linux);
        assert!(!bindings.iter().any(|b| b.action().name() == "workspace::OpenPreview"));
        let mut settings = Settings::default();
        settings.shortcuts.insert("openPreview".into(), Some("ctrl-alt-p".into()));
        let bindings = all_bindings(&settings, Platform::Linux);
        assert_eq!(action_in(&bindings, "ctrl-alt-p"), Some("workspace::OpenPreview"));
    }

    #[test]
    fn null_removes_the_binding() {
        let mut settings = Settings::default();
        settings.shortcuts.insert("closePane".into(), None);
        settings.shortcuts.insert("copy".into(), None);
        let bindings = all_bindings(&settings, Platform::Linux);
        assert_eq!(action_in(&bindings, "ctrl-shift-w"), None);
        assert_eq!(action_in(&bindings, "ctrl-shift-c"), None);
        assert_eq!(action_in(&bindings, "ctrl-shift-v"), Some("terminal::Paste"));
    }

    #[test]
    fn terminal_actions_are_bound() {
        let bindings = all_bindings(&Settings::default(), Platform::Linux);
        assert_eq!(action_in(&bindings, "ctrl-shift-c"), Some("terminal::Copy"));
        assert_eq!(action_in(&bindings, "ctrl-="), Some("terminal::ZoomIn"));
    }
}
