//! The Window section: what Gantry restores on start, the sidebar at start and the update check.

use app_core::release::CheckStatus;
use app_core::self_update::{Offer, Run};
use app_core::settings::{Settings, SidebarAtStart};
use app_core::version::{Update, Version};
use gpui::{Context, Div, Window, div};

use super::*;
use crate::self_update::UpdateRow;
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

/// What the Check now row shows of the update, by priority.
#[derive(Debug, PartialEq, Eq)]
enum UpdateState {
    /// Something is older than the newest version installed: Restart and update.
    Restart(Update),
    /// The update run's line; Cancel while downloading, Retry after a failure that can be tried again.
    Running {
        /// The run's status line.
        line: String,
        /// The failure's detail, if it failed.
        detail: Option<String>,
        /// Offer Cancel.
        cancel: bool,
        /// Offer Retry.
        retry: bool,
    },
    /// A newer release this Gantry can install: Download and install.
    Available(Version),
    /// A newer release this Gantry cannot install, and why.
    NotHere(String),
    /// Nothing about an update: the plain Check now row.
    Check,
}

/// The state the row draws for `row` in a window running `running`.
fn update_state(row: &UpdateRow, running: Version) -> UpdateState {
    if let Some(update) = row.restart {
        return UpdateState::Restart(update);
    }
    if let Some(line) = row.run.line() {
        let failed = matches!(row.run, Run::Failed { .. });
        return UpdateState::Running {
            line,
            detail: match &row.run {
                Run::Failed { detail, .. } => Some(detail.clone()),
                _ => None,
            },
            cancel: matches!(row.run, Run::Downloading { .. }),
            retry: failed && matches!(row.offer, Offer::Install(..)),
        };
    }
    match (&row.offer, row.latest) {
        (Offer::Install(..), Some(latest)) if latest > running => UpdateState::Available(latest),
        (Offer::NotHere(Some(line)), _) => UpdateState::NotHere(line.clone()),
        _ => UpdateState::Check,
    }
}

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

        let check_now = self.check_now_row(theme, window, cx);

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
                    check_now,
                ],
            ))
    }
}

impl SettingsView {
    /// The Check now row: the update run's state when there is one (the same run as the release card's), else the
    /// manual check's status.
    fn check_now_row(&mut self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> Div {
        let check_row = cx.try_global::<crate::release_check::CheckRow>().cloned().unwrap_or_default();
        let update = cx.try_global::<UpdateRow>().cloned().unwrap_or_default();
        let Some(running) = Version::parse(protocol::build::VERSION) else {
            return row_shell(theme, row_label("Check now", div()), self.check_now_button(window, cx), div());
        };
        let status = |color: u32, line: String| div().mt(theme.ui(2.)).text_color(rgb(color)).child(line);
        let page = update.page.clone();
        let open_page = |this: &mut Self, cx: &mut Context<Self>| {
            page.clone().map(|url| {
                this.button(
                    "open-release-page",
                    0,
                    "Open release page",
                    window,
                    cx,
                    handler(move |_, _, cx| cx.open_url(&url)),
                )
            })
        };
        let (label, buttons) = match update_state(&update, running) {
            UpdateState::Restart(update) => (
                status(theme.tokens.accent, update.title())
                    .child(muted(theme, update.detail_text()))
                    .child(muted(theme, update.note_text())),
                vec![
                    self.primary_button(
                        "restart-to-update",
                        "Restart and update",
                        SettingsEvent::RestartToUpdate,
                        window,
                        cx,
                    ),
                    self.check_now_button(window, cx),
                ],
            ),
            UpdateState::Running { line, detail, cancel, retry } => {
                let color = if detail.is_some() { theme.tokens.danger } else { theme.tokens.text };
                let label = status(color, line).children(detail.map(|detail| muted(theme, detail)));
                let mut buttons = Vec::new();
                if cancel {
                    buttons.push(self.button(
                        "cancel-update",
                        0,
                        "Cancel",
                        window,
                        cx,
                        handler(|_, _, cx| cx.emit(SettingsEvent::CancelUpdate)),
                    ));
                } else {
                    if retry {
                        buttons.push(self.primary_button(
                            "retry-update",
                            "Retry",
                            SettingsEvent::DownloadUpdate,
                            window,
                            cx,
                        ));
                    }
                    if !retry {
                        buttons.extend(open_page(self, cx));
                    }
                    buttons.push(self.check_now_button(window, cx));
                }
                (label, buttons)
            }
            UpdateState::Available(latest) => {
                let mut buttons = vec![self.primary_button(
                    "download-and-install",
                    "Download and install",
                    SettingsEvent::DownloadUpdate,
                    window,
                    cx,
                )];
                buttons.extend(open_page(self, cx));
                buttons.push(self.check_now_button(window, cx));
                (status(theme.tokens.accent, format!("Gantry {latest} is available.")), buttons)
            }
            UpdateState::NotHere(line) => {
                let mut buttons: Vec<_> = open_page(self, cx).into_iter().collect();
                buttons.push(self.check_now_button(window, cx));
                (status(theme.tokens.muted, line), buttons)
            }
            UpdateState::Check => {
                let color = match check_row.status {
                    CheckStatus::Idle => theme.tokens.muted,
                    CheckStatus::Checking | CheckStatus::Available(_) => theme.tokens.accent,
                    CheckStatus::UpToDate => theme.tokens.text,
                    CheckStatus::Failed => theme.tokens.danger,
                };
                let button = match check_row.url.filter(|_| matches!(check_row.status, CheckStatus::Available(_))) {
                    Some(url) => self.button(
                        "open-release-page",
                        0,
                        "Open release page",
                        window,
                        cx,
                        handler(move |_, _, cx| cx.open_url(&url)),
                    ),
                    None => self.check_now_button(window, cx),
                };
                (status(color, check_row.status.line(running)), vec![button])
            }
        };
        let control = div().flex().flex_wrap().items_center().gap(theme.ui(8.)).children(buttons);
        row_shell(theme, row_label("Check now", label), control, div())
    }

    fn check_now_button(&mut self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        self.button("check-now", 0, "Check now", window, cx, handler(|_, _, cx| cx.emit(SettingsEvent::CheckNow)))
    }

    /// An accent button that sends `event`.
    fn primary_button(
        &mut self,
        name: &'static str,
        label: &'static str,
        event: SettingsEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = theme(cx).clone();
        self.pressable(name, 0, None, window, cx, handler(move |_, _, cx| cx.emit(event.clone())))
            .h(theme.ui(34.))
            .px(theme.ui(14.))
            .flex()
            .items_center()
            .rounded(px(RADIUS_SMALL))
            .bg(rgb(theme.tokens.accent))
            .text_color(rgb(theme.tokens.on_accent))
            .font_weight(FontWeight::SEMIBOLD)
            .text_sm()
            .child(label)
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

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn install() -> Offer {
        let asset = |name: &str| app_core::release::Asset { name: name.into(), url: String::new(), size: 1 };
        Offer::Install(
            app_core::self_update::Target::Linux { home: "/home/u".into() },
            app_core::self_update::Pick { file: asset("gantry.tar.gz"), sums: asset("SHA256SUMS") },
        )
    }

    #[test]
    fn a_newer_release_this_gantry_can_install_offers_download() {
        let row = UpdateRow { latest: Some(v("0.15.0")), offer: install(), ..Default::default() };
        assert_eq!(update_state(&row, v("0.14.0")), UpdateState::Available(v("0.15.0")));
        assert_eq!(update_state(&row, v("0.15.0")), UpdateState::Check);
        let unknown = UpdateRow { latest: None, offer: install(), ..Default::default() };
        assert_eq!(update_state(&unknown, v("0.14.0")), UpdateState::Check);
    }

    #[test]
    fn a_release_it_cannot_install_shows_why_or_the_plain_row() {
        let why = UpdateRow {
            latest: Some(v("0.15.0")),
            offer: Offer::NotHere(Some("Copy Gantry to Applications first.".into())),
            ..Default::default()
        };
        assert_eq!(update_state(&why, v("0.14.0")), UpdateState::NotHere("Copy Gantry to Applications first.".into()));
        let silent = UpdateRow { latest: Some(v("0.15.0")), offer: Offer::NotHere(None), ..Default::default() };
        assert_eq!(update_state(&silent, v("0.14.0")), UpdateState::Check);
        assert_eq!(update_state(&UpdateRow::default(), v("0.14.0")), UpdateState::Check);
    }

    #[test]
    fn the_run_shows_its_line_with_cancel_or_retry() {
        let downloading = UpdateRow {
            latest: Some(v("0.15.0")),
            offer: install(),
            run: Run::Downloading { version: v("0.15.0"), percent: 42 },
            ..Default::default()
        };
        assert_eq!(
            update_state(&downloading, v("0.14.0")),
            UpdateState::Running {
                line: "Downloading Gantry 0.15.0… 42%".into(),
                detail: None,
                cancel: true,
                retry: false
            }
        );
        let installing = UpdateRow { run: Run::Installing { version: v("0.15.0") }, ..downloading.clone() };
        assert_eq!(
            update_state(&installing, v("0.14.0")),
            UpdateState::Running { line: "Installing…".into(), detail: None, cancel: false, retry: false }
        );
        let failed = UpdateRow {
            run: Run::Failed { step: app_core::self_update::Step::Hash, detail: "expected a, got b".into() },
            ..downloading.clone()
        };
        let failed_line = "The download did not match its checksum.".to_string();
        assert_eq!(
            update_state(&failed, v("0.14.0")),
            UpdateState::Running {
                line: failed_line.clone(),
                detail: Some("expected a, got b".into()),
                cancel: false,
                retry: true
            }
        );
        let stuck = UpdateRow { offer: Offer::NotHere(None), ..failed };
        assert_eq!(
            update_state(&stuck, v("0.14.0")),
            UpdateState::Running {
                line: failed_line,
                detail: Some("expected a, got b".into()),
                cancel: false,
                retry: false
            }
        );
    }

    #[test]
    fn an_installed_update_offers_restart_before_anything_else() {
        let update = app_core::version::update(v("0.14.0"), Some(v("0.14.0")), Some(v("0.15.0")), 2).unwrap();
        let row = UpdateRow {
            latest: Some(v("0.15.0")),
            offer: install(),
            run: Run::Done(v("0.15.0")),
            restart: Some(update),
            ..Default::default()
        };
        let UpdateState::Restart(shown) = update_state(&row, v("0.14.0")) else { panic!("not the restart state") };
        assert_eq!(shown.title(), "Restart to finish updating to 0.15.0");
        assert_eq!(shown.detail_text(), "Gantry 0.15.0 is installed. This window runs 0.14.0.");
        assert!(shown.note_text().starts_with("Restarting closes your 2 running sessions and then resumes them."));
    }

    #[test]
    fn an_outdated_daemon_alone_offers_restart_with_its_own_wording() {
        let update = app_core::version::update(v("0.14.0"), Some(v("0.13.0")), None, 0).unwrap();
        let row = UpdateRow { restart: Some(update), ..Default::default() };
        let UpdateState::Restart(shown) = update_state(&row, v("0.14.0")) else { panic!("not the restart state") };
        assert_eq!(shown.title(), "Restart to finish updating to 0.14.0");
        assert_eq!(shown.detail_text(), "Your running sessions still use 0.13.0.");
        assert_eq!(shown.note_text(), "Your running sessions stay open.");
    }
}
