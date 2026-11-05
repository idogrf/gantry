//! The Window section: what Gantry restores on start, the sidebar at start and the update check.

use app_core::release::CheckStatus;
use app_core::settings::{Settings, SidebarAtStart};
use app_core::version::Version;
use gpui::{Context, Div, Window, div};

use super::*;
use crate::theme::Theme;

/// The "Tabs at start" choices: whether `restoreLayout` is on, its label and what it does.
const RESTORE: [(bool, &str, &str); 2] = [
    (true, "Restore", "Reattach running sessions; finished ones offer Resume."),
    (
        false,
        "Running only",
        "Running sessions stay as tabs; finished ones are not reopened. They are not saved either, so switching back to Restore does not bring them back.",
    ),
];

const SIDEBAR: [(SidebarAtStart, &str, &str); 3] = [
    (SidebarAtStart::Remember, "Remember", "As you left it. The sidebar shortcut or button switches it any time."),
    (SidebarAtStart::Expanded, "Expanded", "Always the full sidebar when Gantry starts."),
    (SidebarAtStart::Compact, "Compact", "Always the thin icon column when Gantry starts."),
];

impl SettingsView {
    pub(super) fn window_page(
        &mut self,
        theme: &Theme,
        settings: &Settings,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let restore =
            RESTORE.iter().position(|(restore, _, _)| *restore == settings.window.restore_layout).unwrap_or(0);
        let restore_labels = RESTORE.map(|(_, label, _)| label);
        let restore_bar = self.segmented(
            "restore-layout",
            &restore_labels,
            restore,
            |index| {
                let restore = RESTORE[index].0;
                change(move |settings| settings.window.restore_layout = restore)
            },
            window,
            cx,
        );
        let restore_reset = self.reset_slot(
            "reset-restore-layout",
            0,
            settings,
            |s| s.window.restore_layout,
            |s, v| s.window.restore_layout = v,
            window,
            cx,
        );

        let resume = settings.window.resume_sessions;
        let resume_switch = self.switch(
            "resume-sessions",
            0,
            resume,
            window,
            cx,
            change(move |settings| settings.window.resume_sessions = !resume),
        );
        let resume_reset = self.reset_slot(
            "reset-resume-sessions",
            0,
            settings,
            |s| s.window.resume_sessions,
            |s, v| s.window.resume_sessions = v,
            window,
            cx,
        );

        let sidebar =
            SIDEBAR.iter().position(|(choice, _, _)| *choice == settings.window.sidebar_at_start).unwrap_or(0);
        let sidebar_labels = SIDEBAR.map(|(_, label, _)| label);
        let sidebar_bar = self.segmented(
            "sidebar-at-start",
            &sidebar_labels,
            sidebar,
            |index| {
                let choice = SIDEBAR[index].0;
                change(move |settings| settings.window.sidebar_at_start = choice)
            },
            window,
            cx,
        );
        let sidebar_reset = self.reset_slot(
            "reset-sidebar-at-start",
            0,
            settings,
            |s| s.window.sidebar_at_start,
            |s, v| s.window.sidebar_at_start = v,
            window,
            cx,
        );

        let check = settings.check_for_updates;
        let check_switch = self.switch(
            "check-for-updates",
            0,
            check,
            window,
            cx,
            change(move |settings| settings.check_for_updates = !check),
        );
        let check_reset = self.reset_slot(
            "reset-check-for-updates",
            0,
            settings,
            |s| s.check_for_updates,
            |s, v| s.check_for_updates = v,
            window,
            cx,
        );

        let check_row = cx.try_global::<crate::release_check::CheckRow>().cloned().unwrap_or_default();
        let running = Version::parse(protocol::build::VERSION);
        let status_color = match check_row.status {
            CheckStatus::Idle => theme.tokens.muted,
            CheckStatus::Checking | CheckStatus::Available(_) => theme.tokens.accent,
            CheckStatus::UpToDate => theme.tokens.text,
            CheckStatus::Failed => theme.tokens.danger,
        };
        let status_line = div()
            .mt(theme.ui(2.))
            .text_color(rgb(status_color))
            .child(running.map_or_else(String::new, |running| check_row.status.line(running)));
        let check_button = match check_row.url.filter(|_| matches!(check_row.status, CheckStatus::Available(_))) {
            Some(url) => self.button(
                "open-release-page",
                0,
                "Open release page",
                window,
                cx,
                handler(move |_, _, cx| cx.open_url(&url)),
            ),
            None => self.button(
                "check-now",
                0,
                "Check now",
                window,
                cx,
                handler(|_, _, cx| cx.emit(SettingsEvent::CheckNow)),
            ),
        };

        div()
            .flex()
            .flex_col()
            .gap(theme.ui(26.))
            .child(group(
                theme,
                "Start",
                None,
                vec![
                    row(theme, "Tabs at start", RESTORE[restore].2, restore_bar, restore_reset),
                    row(
                        theme,
                        "Resume sessions",
                        "Run finished sessions again on start (Claude with --resume) instead of waiting for Resume.",
                        resume_switch,
                        resume_reset,
                    ),
                    row(theme, "Sidebar at start", SIDEBAR[sidebar].2, sidebar_bar, sidebar_reset),
                ],
            ))
            .child(group(
                theme,
                "Updates",
                None,
                vec![
                    row(
                        theme,
                        "Check for updates",
                        "Asks GitHub when Gantry opens and every few hours whether a newer Gantry exists. Nothing else is sent.",
                        check_switch,
                        check_reset,
                    ),
                    row_shell(theme, row_label("Check now", status_line), check_button, div()),
                ],
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_restore_copy_says_what_the_spec_says() {
        assert_eq!(RESTORE[1].1, "Running only");
        assert!(RESTORE[1].2.starts_with("Running sessions stay as tabs; finished ones are not reopened."));
    }

    #[test]
    fn every_choice_has_its_own_description() {
        assert_eq!(SIDEBAR.map(|(_, label, _)| label), ["Remember", "Expanded", "Compact"]);
        assert_eq!(SIDEBAR[2].2, "Always the thin icon column when Gantry starts.");
        assert_eq!(RESTORE[0].1, "Restore");
    }
}
