//! The copy of Gantry's confirmation dialogs and of the pane bar (process ended), as plain data. The window draws a
//! [`DialogSpec`]; nothing here knows about GPUI.

/// How a choice is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceStyle {
    /// The safe or main choice: the accent look.
    Primary,
    /// A choice that ends running work: a red label.
    Danger,
    /// Any other choice: the surface look.
    Plain,
    /// The way out, drawn as a text line.
    Cancel,
}

/// How the choices are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Stacked rows, each with its description.
    Rows,
    /// One row of short buttons.
    Pills,
}

/// One answer a dialog offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    /// The button text.
    pub label: String,
    /// What the choice does, under the label (rows only).
    pub description: Option<String>,
    /// How the choice is drawn.
    pub style: ChoiceStyle,
    /// Whether the keyboard focus starts here.
    pub initial_focus: bool,
}

/// Everything a dialog shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogSpec {
    /// The question.
    pub title: String,
    /// The sentence under the chips.
    pub detail: Option<String>,
    /// Count chips, such as `3 running`.
    pub chips: Vec<String>,
    /// An amber warning line.
    pub warning: Option<String>,
    /// How `actions` are drawn.
    pub layout: Layout,
    /// The choices in display order; the answer is an index into this.
    pub actions: Vec<Choice>,
    /// The index Esc picks.
    pub escape: usize,
}

/// What is running, for the chips and the consequence lines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Running Claude sessions.
    pub claude: usize,
    /// Running terminals.
    pub terminal: usize,
    /// Claude sessions working right now.
    pub working: usize,
}

impl Counts {
    /// Every running session.
    pub fn total(&self) -> usize {
        self.claude + self.terminal
    }

    /// The chips: `{n} running`, `{a} Claude`, `{b} terminal(s)`, `{w} working`, a zero left out.
    pub fn chips(&self) -> Vec<String> {
        let mut chips = Vec::new();
        if self.total() > 0 {
            chips.push(format!("{} running", self.total()));
        }
        if self.claude > 0 {
            chips.push(format!("{} Claude", self.claude));
        }
        if self.terminal > 0 {
            chips.push(format!("{} {}", self.terminal, if self.terminal == 1 { "terminal" } else { "terminals" }));
        }
        if self.working > 0 {
            chips.push(format!("{} working", self.working));
        }
        chips
    }
}

fn sessions(n: usize) -> String {
    format!("{n} {}", if n == 1 { "session" } else { "sessions" })
}

/// "a", "a and b", "a, b and c".
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn choice(label: &str, description: Option<String>, style: ChoiceStyle, initial_focus: bool) -> Choice {
    Choice { label: label.to_string(), description, style, initial_focus }
}

fn spec(title: String, detail: Option<String>, layout: Layout, actions: Vec<Choice>, escape: usize) -> DialogSpec {
    DialogSpec { title, detail, chips: Vec::new(), warning: None, layout, actions, escape }
}

/// Stacked rows: the first choice is the safe one, the second ends sessions, then Cancel.
fn keep_or_end(
    title: String,
    detail: Option<String>,
    chips: Vec<String>,
    keep: (&str, String),
    end: (&str, String),
) -> DialogSpec {
    DialogSpec {
        chips,
        ..spec(
            title,
            detail,
            Layout::Rows,
            vec![
                choice(keep.0, Some(keep.1), ChoiceStyle::Primary, true),
                choice(end.0, Some(end.1), ChoiceStyle::Danger, false),
                choice("Cancel", None, ChoiceStyle::Cancel, false),
            ],
            2,
        )
    }
}

/// Host names for the Quit dialog: those in `listed` (the `sshHosts` order) first, then the others sorted.
pub fn order_hosts(listed: &[String], running: &[String]) -> Vec<String> {
    let mut ordered: Vec<String> = listed.iter().filter(|name| running.contains(name)).cloned().collect();
    let mut others: Vec<String> = running.iter().filter(|name| !listed.contains(name)).cloned().collect();
    others.sort();
    others.dedup();
    ordered.extend(others);
    ordered
}

/// Quit Gantry with a local session running; `remote_hosts` are the hosts with sessions that keep running.
pub fn quit(counts: &Counts, remote_hosts: &[String]) -> DialogSpec {
    let detail = (!remote_hosts.is_empty())
        .then(|| format!("Sessions on {} keep running either way.", join_names(remote_hosts)));
    keep_or_end(
        "Quit Gantry?".into(),
        detail,
        counts.chips(),
        ("Keep sessions running", "Closes the window. Reopen Gantry to return.".into()),
        ("Stop all and quit", format!("Ends {}. You can resume Claude conversations.", sessions(counts.total()))),
    )
}

/// Disconnect from an SSH host that has sessions.
pub fn disconnect(host: &str, counts: &Counts) -> DialogSpec {
    keep_or_end(
        format!("Disconnect from {host}?"),
        None,
        counts.chips(),
        ("Disconnect only", format!("Sessions keep running on {host}.")),
        (
            "End sessions and disconnect",
            format!("Ends {}. You can resume Claude conversations.", sessions(counts.total())),
        ),
    )
}

/// Close a pane with a running process.
pub fn close_pane(is_claude: bool) -> DialogSpec {
    let detail = if is_claude {
        "Closing stops Claude. You can resume the conversation later."
    } else {
        "A command is still running. Closing stops it."
    };
    spec(
        "Close this pane?".into(),
        Some(detail.into()),
        Layout::Pills,
        vec![
            choice("Close pane", None, ChoiceStyle::Danger, false),
            choice("Keep open", None, ChoiceStyle::Primary, true),
        ],
        1,
    )
}

/// Close a tab with running processes.
pub fn close_tab(running: usize) -> DialogSpec {
    spec(
        "Close this tab?".into(),
        Some(format!(
            "Closing stops {running} running {}. You can resume Claude conversations later.",
            if running == 1 { "session" } else { "sessions" }
        )),
        Layout::Pills,
        vec![
            choice("Close tab", None, ChoiceStyle::Danger, false),
            choice("Keep open", None, ChoiceStyle::Primary, true),
        ],
        1,
    )
}

/// Delete a Gantry project that has `running` sessions: they are stopped and their panes closed.
pub fn delete_project(name: &str, running: usize) -> DialogSpec {
    spec(
        format!("Delete project {name}?"),
        Some(format!(
            "{running} {} running in it will be stopped and closed. You can resume Claude conversations later from Recent sessions.",
            if running == 1 { "session is" } else { "sessions are" }
        )),
        Layout::Pills,
        vec![
            choice("Stop and delete", None, ChoiceStyle::Danger, false),
            choice("Cancel", None, ChoiceStyle::Primary, true),
        ],
        1,
    )
}

/// Take over a session running outside Gantry (on `host` when it is remote).
pub fn take_over(host: Option<&str>) -> DialogSpec {
    let title = match host {
        Some(host) => format!("Take over this session on {host}?"),
        None => "Take over this session?".into(),
    };
    spec(
        title,
        Some(
            "It's running outside Gantry. Gantry stops it and resumes it here. Text typed there but not sent is lost."
                .into(),
        ),
        Layout::Pills,
        vec![
            choice("Stop and take over", None, ChoiceStyle::Primary, false),
            choice("Cancel", None, ChoiceStyle::Cancel, true),
        ],
        1,
    )
}

/// The session ignored the stop request for 5 seconds.
pub fn force_quit() -> DialogSpec {
    spec(
        "Claude didn't stop".into(),
        Some("It ignored the stop request for 5 seconds. Force quit ends it now and may lose unsaved work.".into()),
        Layout::Pills,
        vec![
            choice("Force quit", None, ChoiceStyle::Danger, false),
            choice("Cancel take over", None, ChoiceStyle::Plain, true),
        ],
        1,
    )
}

/// Take over while the session's host is not connected.
pub fn cannot_take_over(host: &str) -> DialogSpec {
    notice("Can't take over this session", &format!("Connect to {host} first, then try again."))
}

/// Resume a session running elsewhere that cannot be taken over.
pub fn cannot_resume() -> DialogSpec {
    notice("Can't resume this conversation", "It's running outside Gantry, and Gantry can't take it over.")
}

/// A new session in a folder a dev container mounts.
pub fn dev_container(project: &str, container: &str, running: bool) -> DialogSpec {
    let detail = if running {
        format!("{container} is running.")
    } else {
        format!("{container} is stopped. Running Claude in it starts it.")
    };
    spec(
        format!("Open {project} in a dev container?"),
        Some(detail),
        Layout::Rows,
        vec![
            choice("In dev container", Some(format!("Runs Claude inside {container}.")), ChoiceStyle::Primary, true),
            choice("On this machine", Some("Runs Claude directly on your computer.".into()), ChoiceStyle::Plain, false),
            choice("Cancel", None, ChoiceStyle::Cancel, false),
        ],
        2,
    )
}

/// The daemon runs another version; restarting ends every session. Focus starts on Quit, the safe choice.
pub fn daemon_mismatch() -> DialogSpec {
    spec(
        "Background service is out of date".into(),
        Some("Restarting it ends every running session. You can resume Claude conversations later.".into()),
        Layout::Rows,
        vec![
            choice(
                "Restart and end sessions",
                Some("Ends all sessions, then reopens Gantry.".into()),
                ChoiceStyle::Danger,
                false,
            ),
            choice("Quit", Some("Leave everything as it is.".into()), ChoiceStyle::Primary, true),
        ],
        1,
    )
}

/// A failure report with a single OK.
pub fn notice(title: &str, detail: &str) -> DialogSpec {
    spec(title.into(), Some(detail.into()), Layout::Pills, vec![choice("OK", None, ChoiceStyle::Primary, true)], 0)
}

/// The focus after Tab / Down (`forward`) or Shift+Tab / Up from `current`, wrapping around `len` choices.
pub fn next_focus(current: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        0
    } else if forward {
        (current + 1) % len
    } else {
        (current + len - 1) % len
    }
}

/// What a pane that is not running shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BarState {
    /// The process ended, with its exit code when it is known.
    Exited(Option<u32>),
    /// The process could not start, with the error text.
    Failed(String),
    /// The connection to the background service was lost.
    Disconnected,
}

/// The status dot of the pane bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarTone {
    /// A normal exit: green.
    Ok,
    /// A failure: red.
    Error,
    /// Nothing is known: muted.
    Neutral,
}

/// The pane bar's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneBar {
    /// The bold title.
    pub title: String,
    /// The status line (one line).
    pub status: String,
    /// The whole text when `status` is a cut-down version of it, for the hover card.
    pub detail: Option<String>,
    /// The status dot.
    pub tone: BarTone,
    /// The label of the button that starts the pane again, if there is one.
    pub rerun: Option<String>,
    /// Whether a Close pane button is shown.
    pub close: bool,
}

/// The pane bar for `state` of a Claude (`is_claude`) or shell pane.
pub fn pane_bar(state: &BarState, is_claude: bool) -> PaneBar {
    let rerun = if is_claude { "Resume conversation" } else { "Restart terminal" };
    match state {
        BarState::Exited(code) => PaneBar {
            title: if is_claude { "Claude session ended" } else { "Terminal ended" }.into(),
            status: match code {
                Some(0) => "Exited normally · code 0".into(),
                Some(code) => format!("Exited with an error · code {code}"),
                None => "Ended while Gantry was closed".into(),
            },
            detail: None,
            tone: match code {
                Some(0) => BarTone::Ok,
                Some(_) => BarTone::Error,
                None => BarTone::Neutral,
            },
            rerun: Some(rerun.into()),
            close: true,
        },
        BarState::Failed(error) => PaneBar {
            title: if is_claude { "Couldn't start Claude" } else { "Couldn't start the terminal" }.into(),
            status: error.lines().next().unwrap_or_default().to_string(),
            detail: Some(error.clone()),
            tone: BarTone::Error,
            rerun: Some("Try again".into()),
            close: true,
        },
        BarState::Disconnected => PaneBar {
            title: "Lost connection to the background service".into(),
            status: "Quit and reopen Gantry to reconnect.".into(),
            detail: None,
            tone: BarTone::Error,
            rerun: None,
            close: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_hosts_follow_the_listed_order_then_the_rest_by_name() {
        let names = |list: &[&str]| list.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        let ordered = order_hosts(&names(&["b", "a"]), &names(&["z", "a", "y", "b", "z"]));
        assert_eq!(ordered, names(&["b", "a", "y", "z"]));
    }

    fn labels(spec: &DialogSpec) -> Vec<&str> {
        spec.actions.iter().map(|a| a.label.as_str()).collect()
    }

    fn styles(spec: &DialogSpec) -> Vec<ChoiceStyle> {
        spec.actions.iter().map(|a| a.style).collect()
    }

    fn focus(spec: &DialogSpec) -> usize {
        spec.actions.iter().position(|a| a.initial_focus).unwrap()
    }

    fn three() -> Counts {
        Counts { claude: 2, terminal: 1, working: 1 }
    }

    #[test]
    fn counts_chips_omit_zero_and_pluralize() {
        assert_eq!(three().chips(), ["3 running", "2 Claude", "1 terminal", "1 working"]);
        assert_eq!(Counts { claude: 1, terminal: 0, working: 0 }.chips(), ["1 running", "1 Claude"]);
        assert_eq!(Counts { claude: 0, terminal: 2, working: 0 }.chips(), ["2 running", "2 terminals"]);
    }

    #[test]
    fn quit_copy() {
        let q = quit(&three(), &[]);
        assert_eq!(q.title, "Quit Gantry?");
        assert_eq!(labels(&q), ["Keep sessions running", "Stop all and quit", "Cancel"]);
        assert_eq!(q.actions[0].description.as_deref(), Some("Closes the window. Reopen Gantry to return."));
        assert_eq!(q.actions[1].description.as_deref(), Some("Ends 3 sessions. You can resume Claude conversations."));
        assert_eq!(styles(&q), [ChoiceStyle::Primary, ChoiceStyle::Danger, ChoiceStyle::Cancel]);
        assert_eq!(q.escape, 2);
        assert_eq!(q.detail, None);
        assert_eq!(q.chips, three().chips());
    }

    #[test]
    fn quit_lists_hosts() {
        let host = |names: &[&str]| quit(&three(), &names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).detail;
        assert_eq!(host(&["devbox"]).as_deref(), Some("Sessions on devbox keep running either way."));
        assert_eq!(
            host(&["devbox", "staging"]).as_deref(),
            Some("Sessions on devbox and staging keep running either way.")
        );
        assert_eq!(host(&["a", "b", "c"]).as_deref(), Some("Sessions on a, b and c keep running either way."));
    }

    #[test]
    fn disconnect_copy() {
        let d = disconnect("devbox", &three());
        assert_eq!(d.title, "Disconnect from devbox?");
        assert_eq!(labels(&d), ["Disconnect only", "End sessions and disconnect", "Cancel"]);
        assert_eq!(d.actions[0].description.as_deref(), Some("Sessions keep running on devbox."));
        assert_eq!(d.actions[1].description.as_deref(), Some("Ends 3 sessions. You can resume Claude conversations."));
        assert_eq!(styles(&d), [ChoiceStyle::Primary, ChoiceStyle::Danger, ChoiceStyle::Cancel]);
        assert_eq!(d.escape, 2);
        assert_eq!(d.chips, three().chips());
    }

    #[test]
    fn close_pane_copy() {
        let c = close_pane(true);
        assert_eq!(c.title, "Close this pane?");
        assert_eq!(c.detail.as_deref(), Some("Closing stops Claude. You can resume the conversation later."));
        assert_eq!(labels(&c), ["Close pane", "Keep open"]);
        assert_eq!(styles(&c), [ChoiceStyle::Danger, ChoiceStyle::Primary]);
        assert_eq!((focus(&c), c.escape, c.layout), (1, 1, Layout::Pills));
        assert_eq!(close_pane(false).detail.as_deref(), Some("A command is still running. Closing stops it."));
    }

    #[test]
    fn close_tab_copy() {
        let c = close_tab(2);
        assert_eq!(c.title, "Close this tab?");
        assert_eq!(
            c.detail.as_deref(),
            Some("Closing stops 2 running sessions. You can resume Claude conversations later.")
        );
        assert_eq!(labels(&c), ["Close tab", "Keep open"]);
        assert_eq!(styles(&c), [ChoiceStyle::Danger, ChoiceStyle::Primary]);
        assert_eq!((focus(&c), c.escape), (1, 1));
    }

    #[test]
    fn delete_dialog_copy_mentions_the_count() {
        let one = delete_project("Billing", 1);
        assert_eq!(one.title, "Delete project Billing?");
        assert!(one.detail.as_deref().unwrap().starts_with("1 session is running"));
        assert!(delete_project("Billing", 3).detail.as_deref().unwrap().starts_with("3 sessions are running"));
        assert_eq!(labels(&one), ["Stop and delete", "Cancel"]);
        assert_eq!(one.escape, 1);
    }

    #[test]
    fn take_over_copy() {
        let t = take_over(None);
        assert_eq!(t.title, "Take over this session?");
        assert_eq!(
            t.detail.as_deref(),
            Some(
                "It's running outside Gantry. Gantry stops it and resumes it here. Text typed there but not sent is lost."
            )
        );
        assert_eq!(labels(&t), ["Stop and take over", "Cancel"]);
        assert_eq!(styles(&t), [ChoiceStyle::Primary, ChoiceStyle::Cancel]);
        assert_eq!((focus(&t), t.escape), (1, 1));
        assert_eq!(take_over(Some("devbox")).title, "Take over this session on devbox?");
    }

    #[test]
    fn force_quit_copy() {
        let f = force_quit();
        assert_eq!(f.title, "Claude didn't stop");
        assert_eq!(
            f.detail.as_deref(),
            Some("It ignored the stop request for 5 seconds. Force quit ends it now and may lose unsaved work.")
        );
        assert_eq!(labels(&f), ["Force quit", "Cancel take over"]);
        assert_eq!(styles(&f), [ChoiceStyle::Danger, ChoiceStyle::Plain]);
        assert_eq!((focus(&f), f.escape), (1, 1));
    }

    #[test]
    fn notices() {
        let t = cannot_take_over("devbox");
        assert_eq!(t.title, "Can't take over this session");
        assert_eq!(t.detail.as_deref(), Some("Connect to devbox first, then try again."));
        assert_eq!(labels(&t), ["OK"]);
        assert_eq!((focus(&t), t.escape), (0, 0));
        let r = cannot_resume();
        assert_eq!(r.title, "Can't resume this conversation");
        assert_eq!(r.detail.as_deref(), Some("It's running outside Gantry, and Gantry can't take it over."));
        assert_eq!(labels(&r), ["OK"]);
        let n = notice("Cannot open this session", "The dev container is gone.");
        assert_eq!(
            (n.title.as_str(), n.detail.as_deref()),
            ("Cannot open this session", Some("The dev container is gone."))
        );
        assert_eq!(styles(&n), [ChoiceStyle::Primary]);
    }

    #[test]
    fn dev_container_copy() {
        let d = dev_container("gantry", "gantry-dev", true);
        assert_eq!(d.title, "Open gantry in a dev container?");
        assert_eq!(d.detail.as_deref(), Some("gantry-dev is running."));
        assert_eq!(labels(&d), ["In dev container", "On this machine", "Cancel"]);
        assert_eq!(d.actions[0].description.as_deref(), Some("Runs Claude inside gantry-dev."));
        assert_eq!(d.actions[1].description.as_deref(), Some("Runs Claude directly on your computer."));
        assert_eq!(styles(&d), [ChoiceStyle::Primary, ChoiceStyle::Plain, ChoiceStyle::Cancel]);
        assert_eq!((focus(&d), d.escape), (0, 2));
        assert_eq!(
            dev_container("gantry", "gantry-dev", false).detail.as_deref(),
            Some("gantry-dev is stopped. Running Claude in it starts it.")
        );
    }

    #[test]
    fn daemon_mismatch_copy() {
        let d = daemon_mismatch();
        assert_eq!(d.title, "Background service is out of date");
        assert_eq!(
            d.detail.as_deref(),
            Some("Restarting it ends every running session. You can resume Claude conversations later.")
        );
        assert_eq!(labels(&d), ["Restart and end sessions", "Quit"]);
        assert_eq!(d.actions[0].description.as_deref(), Some("Ends all sessions, then reopens Gantry."));
        assert_eq!(d.actions[1].description.as_deref(), Some("Leave everything as it is."));
        assert_eq!(styles(&d)[0], ChoiceStyle::Danger);
        assert_eq!((focus(&d), d.escape), (1, 1));
    }

    #[test]
    fn every_dialog_has_one_safe_initial_focus() {
        let all = [
            quit(&three(), &[]),
            disconnect("h", &three()),
            close_pane(true),
            close_pane(false),
            close_tab(1),
            take_over(None),
            force_quit(),
            cannot_take_over("h"),
            cannot_resume(),
            dev_container("p", "c", false),
            daemon_mismatch(),
            notice("t", "d"),
        ];
        for spec in &all {
            let focused: Vec<_> = spec.actions.iter().filter(|a| a.initial_focus).collect();
            assert_eq!(focused.len(), 1, "{}", spec.title);
            assert_ne!(focused[0].style, ChoiceStyle::Danger, "{}", spec.title);
            assert!(spec.escape < spec.actions.len(), "{}", spec.title);
        }
    }

    #[test]
    fn next_focus_wraps() {
        assert_eq!(next_focus(0, 3, false), 2);
        assert_eq!(next_focus(2, 3, true), 0);
        assert_eq!(next_focus(0, 1, true), 0);
    }

    #[test]
    fn pane_bar_exited() {
        let b = pane_bar(&BarState::Exited(Some(0)), true);
        assert_eq!((b.title.as_str(), b.status.as_str()), ("Claude session ended", "Exited normally · code 0"));
        assert_eq!(
            (b.tone, b.rerun.as_deref(), b.close, b.detail),
            (BarTone::Ok, Some("Resume conversation"), true, None)
        );
        let b = pane_bar(&BarState::Exited(Some(1)), false);
        assert_eq!((b.title.as_str(), b.status.as_str()), ("Terminal ended", "Exited with an error · code 1"));
        assert_eq!((b.tone, b.rerun.as_deref()), (BarTone::Error, Some("Restart terminal")));
        let b = pane_bar(&BarState::Exited(None), false);
        assert_eq!((b.status.as_str(), b.tone), ("Ended while Gantry was closed", BarTone::Neutral));
        assert_eq!(pane_bar(&BarState::Exited(None), true).title, "Claude session ended");
    }

    #[test]
    fn pane_bar_disconnected() {
        let b = pane_bar(&BarState::Disconnected, true);
        assert_eq!(b.title, "Lost connection to the background service");
        assert_eq!(b.status, "Quit and reopen Gantry to reconnect.");
        assert_eq!((b.tone, b.rerun, b.close), (BarTone::Error, None, false));
    }

    #[test]
    fn pane_bar_failed_keeps_full_text_and_one_line() {
        let state = BarState::Failed("spawn failed\nNo such file".into());
        let b = pane_bar(&state, true);
        assert_eq!(b.title, "Couldn't start Claude");
        assert_eq!(b.status, "spawn failed");
        assert_eq!(b.detail.as_deref(), Some("spawn failed\nNo such file"));
        assert_eq!((b.tone, b.rerun.as_deref(), b.close), (BarTone::Error, Some("Try again"), true));
        assert_eq!(pane_bar(&state, false).title, "Couldn't start the terminal");
    }
}
