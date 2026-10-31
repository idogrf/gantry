//! The state of one SSH host's connection: what the cards say, when to retry, and how a failed attempt is read.

use std::time::Duration;

/// Retries after a dropped connection before the host shows Reconnect.
pub const RETRY_LIMIT: u8 = 4;

/// How long to wait before retry `attempt` (1 to 4): 1, 2, 5, 10 seconds. `None` once the retries are used up.
pub fn reconnect_delay(attempt: u8) -> Option<Duration> {
    let seconds = match attempt {
        1 => 1,
        2 => 2,
        3 => 5,
        4 => 10,
        _ => return None,
    };
    Some(Duration::from_secs(seconds))
}

/// Where one SSH host's connection stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostState {
    /// Nothing tried yet (hosts connect on first use).
    NotConnected,
    /// An interactive attempt is running.
    Connecting,
    /// The daemon is attached.
    Connected,
    /// The connection dropped; retry `attempt` (1 to 4) is waiting or running, without prompts.
    Reconnecting {
        /// Which retry.
        attempt: u8,
    },
    /// All retries failed; waits for the user's Reconnect.
    Disconnected,
    /// ssh needs a password, passphrase or host-key answer.
    NeedsSignIn,
    /// The remote daemon speaks another protocol version.
    UpdateNeeded {
        /// What the remote runs, for the card (for example `protocol 6`).
        remote: Option<String>,
    },
    /// The remote has no Gantry at the configured command.
    NotInstalled,
    /// Any other failure, with ssh's last words.
    Failed {
        /// Why.
        reason: String,
    },
    /// Another Gantry took this host over; no retry (two windows would fight), waits for the user's Reconnect.
    TakenOver,
}

/// A small status dot for a host in the New menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostDot {
    /// Connected.
    Ok,
    /// Working on it (connecting, reconnecting).
    Busy,
    /// Needs the user (sign-in, update, opened elsewhere).
    Warn,
    /// Not usable (lost, not installed, failed).
    Bad,
    /// Nothing to say (not connected yet).
    None,
}

/// Why an attempt failed, as read from ssh's exit and the handshake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailKind {
    /// ssh needed input (credentials, a host-key answer) that a background attempt cannot get, or the user cancelled.
    NeedsInput,
    /// The remote daemon speaks another protocol version.
    UpdateNeeded {
        /// What the remote runs, for the card.
        remote: Option<String>,
    },
    /// The remote command was not found.
    NotInstalled,
    /// Anything else.
    Other {
        /// Why.
        reason: String,
    },
}

/// What the collapsed SSH hosts line shows: how many hosts are connected and one status dot for all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostsSummary {
    /// Green when a host is connected, else amber when one is connecting or reconnecting, else none.
    pub dot: HostDot,
    /// How many hosts are connected.
    pub connected: usize,
    /// How many hosts are saved.
    pub total: usize,
}

/// Summarizes the dots of every saved host for the collapsed SSH hosts line.
pub fn hosts_summary(dots: &[HostDot]) -> HostsSummary {
    let connected = dots.iter().filter(|dot| **dot == HostDot::Ok).count();
    let dot = if connected > 0 {
        HostDot::Ok
    } else if dots.contains(&HostDot::Busy) {
        HostDot::Busy
    } else {
        HostDot::None
    };
    HostsSummary { dot, connected, total: dots.len() }
}

/// What the X of a host in the sidebar's SSH hosts block does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectPlan {
    /// Stop trying to connect or reconnect; nothing to ask.
    Cancel,
    /// Nothing runs on the host: stop its daemon and disconnect, without asking.
    StopNow,
    /// Sessions run there: ask Detach, Close all sessions or Cancel.
    Ask,
}

/// Something that happened to a host's connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostEvent {
    /// The user asked to connect (a button, the New menu, Continue on a dead leaf).
    Connect,
    /// An attempt got through the handshake.
    Established,
    /// An established connection ended.
    Dropped,
    /// An attempt failed.
    AttemptFailed(FailKind),
    /// Another Gantry took the host's daemon over while connected.
    TakenOver,
    /// The user cancelled the attempt that was connecting.
    Cancel,
}

/// What picking a host in the launcher's host step does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostPick {
    /// The host is connected: go on to its folders.
    Open,
    /// Connect it and wait.
    Connect,
    /// A background retry is running: only wait.
    Wait,
    /// An attempt is running: the row offers Cancel.
    Cancel,
    /// The last attempt failed: the row offers Retry.
    Retry,
    /// The remote has no Gantry: the row offers Install….
    Install,
    /// The remote runs another version: the row offers Update and restart.
    Update,
}

/// What picking a host in the state `state` does.
pub fn host_pick(state: &HostState) -> HostPick {
    match state {
        HostState::Connected => HostPick::Open,
        HostState::NotConnected | HostState::Disconnected | HostState::NeedsSignIn | HostState::TakenOver => {
            HostPick::Connect
        }
        HostState::Reconnecting { .. } => HostPick::Wait,
        HostState::Connecting => HostPick::Cancel,
        HostState::Failed { .. } => HostPick::Retry,
        HostState::NotInstalled => HostPick::Install,
        HostState::UpdateNeeded { .. } => HostPick::Update,
    }
}

/// The message card of a host that is not usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCard {
    /// Bold first line.
    pub title: String,
    /// Explanation under it.
    pub detail: String,
    /// The one button, if the user can do something about it.
    pub button: Option<&'static str>,
}

impl HostState {
    /// Whether a user's Connect starts an attempt from this state.
    pub fn can_connect(&self) -> bool {
        !matches!(self, HostState::Connecting | HostState::Connected | HostState::Reconnecting { .. })
    }

    /// Whether the host waits for the user to act on why it could not connect (sign in, install, update, a failure):
    /// the visible tab of the host stays on screen with its card, though the sidebar does not list it.
    pub fn asks_user(&self) -> bool {
        matches!(
            self,
            HostState::NeedsSignIn
                | HostState::UpdateNeeded { .. }
                | HostState::NotInstalled
                | HostState::Failed { .. }
        )
    }

    /// Whether the host is connected or on its way: the sidebar block gives it an X.
    pub fn in_connected_list(&self) -> bool {
        matches!(self, HostState::Connecting | HostState::Connected | HostState::Reconnecting { .. })
    }

    /// Whether the sidebar lists the host's panes after the host moved into this state, given whether it did before:
    /// they appear when the host first connects and stay through every later state (a drop, a retry, a sign-in, a
    /// take-over), until the host is not connected, or unusable (failed, not installed, needs an update).
    pub fn shows_rows(&self, before: bool) -> bool {
        match self {
            HostState::Connected => true,
            HostState::NotConnected
            | HostState::Failed { .. }
            | HostState::NotInstalled
            | HostState::UpdateNeeded { .. } => false,
            _ => before,
        }
    }

    /// What the X of this host does when `running` of its panes have a live process; `None` when the host is not in
    /// the sidebar block.
    pub fn disconnect_plan(&self, running: usize) -> Option<DisconnectPlan> {
        match self {
            HostState::Connecting | HostState::Reconnecting { .. } => Some(DisconnectPlan::Cancel),
            HostState::Connected if running == 0 => Some(DisconnectPlan::StopNow),
            HostState::Connected => Some(DisconnectPlan::Ask),
            _ => None,
        }
    }

    /// The state after `event`; an event that does not apply leaves the state as it is.
    pub fn next(&self, event: HostEvent) -> HostState {
        match (self, event) {
            (state, HostEvent::Connect) if state.can_connect() => HostState::Connecting,
            (HostState::Connecting | HostState::Reconnecting { .. }, HostEvent::Established) => HostState::Connected,
            (HostState::Connected, HostEvent::Dropped) => HostState::Reconnecting { attempt: 1 },
            (HostState::Connected, HostEvent::TakenOver) => HostState::TakenOver,
            (HostState::Connecting, HostEvent::Cancel) => HostState::NotConnected,
            (HostState::Connecting | HostState::Reconnecting { .. }, HostEvent::AttemptFailed(kind)) => match kind {
                FailKind::NeedsInput => HostState::NeedsSignIn,
                FailKind::UpdateNeeded { remote } => HostState::UpdateNeeded { remote },
                FailKind::NotInstalled => HostState::NotInstalled,
                FailKind::Other { reason } => match self {
                    HostState::Reconnecting { attempt } if *attempt < RETRY_LIMIT => {
                        HostState::Reconnecting { attempt: attempt + 1 }
                    }
                    HostState::Reconnecting { .. } => HostState::Disconnected,
                    _ => HostState::Failed { reason },
                },
            },
            (state, _) => state.clone(),
        }
    }

    /// The short state shown beside the host's name in the New menu.
    pub fn label(&self) -> String {
        match self {
            HostState::NotConnected => "not connected".into(),
            HostState::Connecting => "connecting…".into(),
            HostState::Connected => "connected".into(),
            HostState::Reconnecting { attempt } => format!("reconnecting {attempt}/{RETRY_LIMIT}"),
            HostState::Disconnected => "disconnected".into(),
            HostState::NeedsSignIn => "needs sign-in".into(),
            HostState::UpdateNeeded { .. } => "update needed".into(),
            HostState::NotInstalled => "not installed".into(),
            HostState::Failed { .. } => "failed".into(),
            HostState::TakenOver => "opened elsewhere".into(),
        }
    }

    /// The dot shown beside the host's name in the New menu.
    pub fn dot(&self) -> HostDot {
        match self {
            HostState::Connected => HostDot::Ok,
            HostState::Connecting | HostState::Reconnecting { .. } => HostDot::Busy,
            HostState::NeedsSignIn | HostState::UpdateNeeded { .. } | HostState::TakenOver => HostDot::Warn,
            HostState::Disconnected | HostState::NotInstalled | HostState::Failed { .. } => HostDot::Bad,
            HostState::NotConnected => HostDot::None,
        }
    }

    /// The card the panes of host `host` show in this state; `None` when connected.
    pub fn card(&self, host: &str) -> Option<HostCard> {
        let card = |title: String, detail: String, button| Some(HostCard { title, detail, button });
        match self {
            HostState::Connected => None,
            HostState::NotConnected => {
                card("Not connected".into(), format!("{host} is not connected yet."), Some("Connect"))
            }
            HostState::Connecting => card(
                format!("Connecting to {host}…"),
                "Starting ssh. A sign-in dialog opens here if it asks for anything.".into(),
                Some("Cancel"),
            ),
            HostState::Reconnecting { attempt } => card(
                format!("Reconnecting to {host}…"),
                format!("Attempt {attempt} of {RETRY_LIMIT}. Your sessions keep running there."),
                None,
            ),
            HostState::Disconnected => card(
                "Connection lost".into(),
                format!("{host} did not answer after {RETRY_LIMIT} tries. Your sessions keep running there."),
                Some("Reconnect"),
            ),
            HostState::NeedsSignIn => card(
                format!("Sign in to {host}"),
                format!("{host} needs a password or a key passphrase."),
                Some("Sign in"),
            ),
            HostState::UpdateNeeded { remote } => card(
                format!("Update Gantry on {host}"),
                match remote {
                    Some(remote) => {
                        format!("{host} runs a different Gantry ({remote}). Update it from this Gantry.")
                    }
                    None => {
                        format!("{host} runs a different Gantry version. Update it from this Gantry.")
                    }
                },
                Some("Update and restart"),
            ),
            HostState::NotInstalled => card(
                format!("Gantry is not installed on {host}"),
                "Gantry was not found there. Install it from this Gantry.".into(),
                Some("Install…"),
            ),
            HostState::Failed { reason } => {
                card(format!("Could not connect to {host}"), reason.clone(), Some("Reconnect"))
            }
            HostState::TakenOver => card(
                "Opened from another Gantry".into(),
                format!("Another Gantry took over {host}. Your sessions keep running there."),
                Some("Reconnect"),
            ),
        }
    }
}

/// The Ubuntu/Debian package that provides a library Gantry links.
fn package_for(library: &str) -> Option<&'static str> {
    match library {
        "libasound.so.2" => Some("libasound2t64"),
        "libxkbcommon-x11.so.0" => Some("libxkbcommon-x11-0"),
        "libxkbcommon.so.0" => Some("libxkbcommon0"),
        "libxcb.so.1" => Some("libxcb1"),
        _ => None,
    }
}

/// What the wizard says when Gantry is on the host but cannot start for want of `libraries`: all of them, with the one
/// command that installs the packages we know.
pub fn describe_missing_libraries(libraries: &[String]) -> String {
    let mut text = format!("Gantry cannot start on the host: {} missing.", libraries.join(", "));
    let packages: Vec<&str> = libraries.iter().filter_map(|library| package_for(library)).collect();
    if !packages.is_empty() {
        text.push_str(&format!(" Install with: sudo apt install -y {}", packages.join(" ")));
        if packages.contains(&"libasound2t64") {
            text.push_str(" (libasound2 on Ubuntu 22.04)");
        }
        text.push('.');
    }
    let unknown: Vec<&str> =
        libraries.iter().map(String::as_str).filter(|library| package_for(library).is_none()).collect();
    if !unknown.is_empty() {
        text.push_str(&format!(" no known package for {}.", unknown.join(", ")));
    }
    text
}

/// The libraries `ldd` reports as `not found`, each once, in the order listed.
pub fn parse_ldd(stdout: &str) -> Vec<String> {
    let mut libraries: Vec<String> = Vec::new();
    for line in stdout.lines().filter(|line| line.contains("=> not found")) {
        if let Some(library) = line.split_whitespace().next()
            && !libraries.iter().any(|known| known == library)
        {
            libraries.push(library.to_string());
        }
    }
    libraries
}

/// The loader's "error while loading shared libraries" line in `stderr` (it names only the first missing library):
/// Gantry is there but cannot start (the loader exits 127 too).
fn missing_library(stderr: &str) -> Option<String> {
    let line = stderr.lines().find(|line| line.contains("error while loading shared libraries"))?;
    let library = line.split("shared libraries:").nth(1)?.trim().split(':').next()?.trim();
    Some(describe_missing_libraries(&[library.to_string()]))
}

/// Read a failed attempt. `exit_code`: ssh's exit status when it ended by itself; `stderr`: what it printed;
/// `version_mismatch`: the remote daemon's protocol version when the handshake said it differs;
/// `interactive_unasked`: the attempt was interactive and ssh never asked a question (so "Permission denied" is not
/// something a dialog can fix); `handshake_error`: why the handshake failed, when it did.
pub fn classify_failure(
    exit_code: Option<i32>,
    stderr: &str,
    version_mismatch: Option<u32>,
    interactive_unasked: bool,
    handshake_error: Option<&str>,
) -> FailKind {
    if let Some(protocol) = version_mismatch {
        return FailKind::UpdateNeeded { remote: Some(format!("protocol {protocol}")) };
    }
    let lower = stderr.to_lowercase();
    if exit_code == Some(127) {
        if let Some(reason) = missing_library(stderr) {
            return FailKind::Other { reason };
        }
        return FailKind::NotInstalled;
    }
    if lower.contains("remote host identification has changed") {
        return FailKind::Other { reason: "The host key of the host changed. Check ~/.ssh/known_hosts.".into() };
    }
    if lower.contains("too many authentication failures") {
        return FailKind::Other {
            reason: "The server stopped after too many failed sign-in tries. Set this host's sign-in to Password or SSH key."
                .into(),
        };
    }
    let last_line = stderr.lines().rev().find(|line| !line.trim().is_empty()).map(|line| line.trim().to_string());
    let wants_input = ["permission denied", "host key verification failed", "passphrase", "authentication failed"];
    if exit_code == Some(255) && wants_input.iter().any(|needle| lower.contains(needle)) {
        if interactive_unasked && lower.contains("permission denied") {
            let line = last_line.unwrap_or_default();
            return FailKind::Other { reason: format!("Authentication failed (ssh asked for nothing): {line}") };
        }
        return FailKind::NeedsInput;
    }
    // A plain end of stream says nothing: the exit code does better. Only another handshake error is named.
    let handshake_error = handshake_error.filter(|error| *error != "daemon closed the connection");
    let reason = match (last_line, handshake_error, exit_code) {
        (Some(line), _, _) => line,
        (None, Some(error), _) => error.to_string(),
        (None, None, Some(code)) => format!("ssh ended with exit code {code}"),
        (None, None, None) => "the connection closed".to_string(),
    };
    FailKind::Other { reason }
}

/// How a prompt from ssh is asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// Anything but a host-key question: the field shows bullets, since ssh does not say whether a prompt echoes.
    Masked,
    /// A host-key question: two buttons.
    YesNo,
}

/// Which kind of prompt `prompt` is.
pub fn prompt_kind(prompt: &str) -> PromptKind {
    let lower = prompt.to_lowercase();
    if lower.contains("(yes/no") { PromptKind::YesNo } else { PromptKind::Masked }
}

/// The dialog title for `prompt` asked on behalf of `host`.
pub fn prompt_title(host: &str, prompt: &str) -> String {
    let lower = prompt.to_lowercase();
    match prompt_kind(prompt) {
        PromptKind::YesNo => format!("Trust {host}?"),
        PromptKind::Masked if lower.contains("passphrase") => format!("Key passphrase for {host}"),
        PromptKind::Masked if lower.contains("password") => format!("Password for {host}"),
        PromptKind::Masked => format!("Sign in to {host}"),
    }
}

/// The launcher's line about a typed host whose daemon is not connected: a host that is simply away is opened by
/// Enter (the lazy connect); any other state is named in a few words.
pub fn away_line(host: &str, state: &HostState) -> String {
    match state {
        HostState::NotConnected | HostState::Disconnected | HostState::Failed { .. } => {
            format!("{host} is not connected: Enter opens it and connects")
        }
        other => format!("{host}: {}", other.label()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_pick_follows_the_state() {
        use super::{HostPick, HostState, host_pick};
        assert_eq!(host_pick(&HostState::Connected), HostPick::Open);
        assert_eq!(host_pick(&HostState::NotConnected), HostPick::Connect);
        assert_eq!(host_pick(&HostState::Disconnected), HostPick::Connect);
        assert_eq!(host_pick(&HostState::NeedsSignIn), HostPick::Connect);
        assert_eq!(host_pick(&HostState::TakenOver), HostPick::Connect);
        assert_eq!(host_pick(&HostState::Reconnecting { attempt: 2 }), HostPick::Wait);
        assert_eq!(host_pick(&HostState::Connecting), HostPick::Cancel);
        assert_eq!(host_pick(&HostState::Failed { reason: "x".into() }), HostPick::Retry);
        assert_eq!(host_pick(&HostState::NotInstalled), HostPick::Install);
        assert_eq!(host_pick(&HostState::UpdateNeeded { remote: None }), HostPick::Update);
    }

    #[test]
    fn an_empty_host_list_summarizes_to_no_dot_and_zero_of_zero() {
        assert_eq!(hosts_summary(&[]), HostsSummary { dot: HostDot::None, connected: 0, total: 0 });
    }

    #[test]
    fn rows_appear_on_connect_and_stay_until_the_host_is_gone_or_unusable() {
        let failed = HostState::Failed { reason: String::new() };
        let update = HostState::UpdateNeeded { remote: None };
        assert!(HostState::Connected.shows_rows(false));
        for state in [HostState::Connecting, HostState::Disconnected, HostState::NeedsSignIn, HostState::TakenOver] {
            assert!(!state.shows_rows(false), "{state:?} before a connect");
            assert!(state.shows_rows(true), "{state:?} after a connect");
        }
        assert!(HostState::Reconnecting { attempt: 3 }.shows_rows(true));
        for state in [HostState::NotConnected, HostState::NotInstalled, update, failed] {
            assert!(!state.shows_rows(true), "{state:?} hides the rows");
        }
    }

    #[test]
    fn one_connected_host_makes_the_dot_ok() {
        assert_eq!(
            hosts_summary(&[HostDot::Ok, HostDot::None]),
            HostsSummary { dot: HostDot::Ok, connected: 1, total: 2 }
        );
    }

    #[test]
    fn a_connecting_host_with_none_connected_is_busy() {
        assert_eq!(
            hosts_summary(&[HostDot::Busy, HostDot::None]),
            HostsSummary { dot: HostDot::Busy, connected: 0, total: 2 }
        );
    }

    #[test]
    fn a_host_that_needs_the_user_or_failed_does_not_count_as_up() {
        assert_eq!(
            hosts_summary(&[HostDot::Warn, HostDot::Bad]),
            HostsSummary { dot: HostDot::None, connected: 0, total: 2 }
        );
    }

    #[test]
    fn a_connected_host_beats_a_busy_one() {
        assert_eq!(
            hosts_summary(&[HostDot::Busy, HostDot::Ok]),
            HostsSummary { dot: HostDot::Ok, connected: 1, total: 2 }
        );
    }

    #[test]
    fn asks_user_only_for_what_the_user_must_answer() {
        let reason = "ssh: no route".to_string();
        for state in [
            HostState::NeedsSignIn,
            HostState::UpdateNeeded { remote: None },
            HostState::NotInstalled,
            HostState::Failed { reason },
        ] {
            assert!(state.asks_user(), "{state:?}");
        }
        for state in [HostState::NotConnected, HostState::Disconnected, HostState::TakenOver, HostState::Connected] {
            assert!(!state.asks_user(), "{state:?}");
        }
    }

    #[test]
    fn only_a_connected_or_connecting_host_has_an_x() {
        assert!(HostState::Connected.in_connected_list());
        assert!(HostState::Connecting.in_connected_list());
        assert!(HostState::Reconnecting { attempt: 2 }.in_connected_list());
        assert!(!HostState::NotConnected.in_connected_list());
        assert!(!HostState::NeedsSignIn.in_connected_list());
        assert!(!HostState::Disconnected.in_connected_list());
    }

    #[test]
    fn the_x_cancels_a_pending_host_stops_an_idle_one_and_asks_when_sessions_run() {
        assert_eq!(HostState::Connecting.disconnect_plan(0), Some(DisconnectPlan::Cancel));
        assert_eq!(HostState::Reconnecting { attempt: 1 }.disconnect_plan(3), Some(DisconnectPlan::Cancel));
        assert_eq!(HostState::Connected.disconnect_plan(0), Some(DisconnectPlan::StopNow));
        assert_eq!(HostState::Connected.disconnect_plan(2), Some(DisconnectPlan::Ask));
        assert_eq!(HostState::NotConnected.disconnect_plan(0), None);
    }

    use std::time::Duration;

    use super::*;

    #[test]
    fn the_away_line_offers_enter_only_for_a_host_that_is_simply_away() {
        let line = "devbox is not connected: Enter opens it and connects";
        assert_eq!(away_line("devbox", &HostState::NotConnected), line);
        assert_eq!(away_line("devbox", &HostState::Disconnected), line);
        assert_eq!(away_line("devbox", &HostState::Failed { reason: "x".into() }), line);
        assert_eq!(away_line("devbox", &HostState::NeedsSignIn), "devbox: needs sign-in");
        assert_eq!(away_line("devbox", &HostState::TakenOver), "devbox: opened elsewhere");
        assert_eq!(away_line("devbox", &HostState::Reconnecting { attempt: 2 }), "devbox: reconnecting 2/4");
    }

    fn other(reason: &str) -> HostEvent {
        HostEvent::AttemptFailed(FailKind::Other { reason: reason.into() })
    }

    #[test]
    fn the_retry_schedule_is_1_2_5_10_seconds_then_nothing() {
        let delays: Vec<_> = (1..=4).map(|attempt| reconnect_delay(attempt).unwrap().as_secs()).collect();
        assert_eq!(delays, [1, 2, 5, 10]);
        assert_eq!(reconnect_delay(0), None);
        assert_eq!(reconnect_delay(5), None);
    }

    #[test]
    fn connect_starts_from_every_state_that_can_connect_and_from_no_other() {
        for state in [
            HostState::NotConnected,
            HostState::Disconnected,
            HostState::NeedsSignIn,
            HostState::UpdateNeeded { remote: None },
            HostState::NotInstalled,
            HostState::Failed { reason: "x".into() },
            HostState::TakenOver,
        ] {
            assert!(state.can_connect(), "{state:?}");
            assert_eq!(state.next(HostEvent::Connect), HostState::Connecting, "{state:?}");
        }
        for state in [HostState::Connecting, HostState::Connected, HostState::Reconnecting { attempt: 2 }] {
            assert!(!state.can_connect(), "{state:?}");
            assert_eq!(state.next(HostEvent::Connect), state, "{state:?}");
        }
    }

    #[test]
    fn an_interactive_attempt_ends_in_connected_or_a_reason() {
        let connecting = HostState::Connecting;
        assert_eq!(connecting.next(HostEvent::Established), HostState::Connected);
        assert_eq!(connecting.next(HostEvent::AttemptFailed(FailKind::NeedsInput)), HostState::NeedsSignIn);
        assert_eq!(
            connecting.next(HostEvent::AttemptFailed(FailKind::UpdateNeeded { remote: Some("protocol 6".into()) })),
            HostState::UpdateNeeded { remote: Some("protocol 6".into()) }
        );
        assert_eq!(connecting.next(HostEvent::AttemptFailed(FailKind::NotInstalled)), HostState::NotInstalled);
        assert_eq!(connecting.next(other("refused")), HostState::Failed { reason: "refused".into() });
    }

    #[test]
    fn a_dropped_connection_retries_four_times_then_disconnects() {
        let mut state = HostState::Connected.next(HostEvent::Dropped);
        assert_eq!(state, HostState::Reconnecting { attempt: 1 });
        for attempt in 2..=4 {
            state = state.next(other("timeout"));
            assert_eq!(state, HostState::Reconnecting { attempt });
        }
        assert_eq!(state.next(other("timeout")), HostState::Disconnected);
    }

    #[test]
    fn a_retry_that_needs_input_never_loops_it_asks_for_sign_in_at_once() {
        let retrying = HostState::Reconnecting { attempt: 2 };
        assert_eq!(retrying.next(HostEvent::AttemptFailed(FailKind::NeedsInput)), HostState::NeedsSignIn);
        assert_eq!(retrying.next(HostEvent::AttemptFailed(FailKind::NotInstalled)), HostState::NotInstalled);
        assert_eq!(retrying.next(HostEvent::Established), HostState::Connected);
    }

    #[test]
    fn events_that_do_not_apply_change_nothing() {
        assert_eq!(HostState::NotConnected.next(HostEvent::Dropped), HostState::NotConnected);
        assert_eq!(HostState::Disconnected.next(HostEvent::Established), HostState::Disconnected);
        assert_eq!(HostState::Connected.next(other("x")), HostState::Connected);
    }

    #[test]
    fn menu_labels_and_cards_name_the_state() {
        assert_eq!(HostState::NotConnected.label(), "not connected");
        assert_eq!(HostState::Reconnecting { attempt: 2 }.label(), "reconnecting 2/4");
        assert_eq!(HostState::NeedsSignIn.label(), "needs sign-in");
        assert!(HostState::Connected.card("devbox").is_none());
        let card = HostState::Disconnected.card("devbox").unwrap();
        assert_eq!((card.title.as_str(), card.button), ("Connection lost", Some("Reconnect")));
        let card = HostState::NotConnected.card("devbox").unwrap();
        assert_eq!((card.title.as_str(), card.button), ("Not connected", Some("Connect")));
        assert_eq!(HostState::NeedsSignIn.card("devbox").unwrap().button, Some("Sign in"));
        assert_eq!(HostState::Failed { reason: "refused".into() }.card("devbox").unwrap().detail, "refused");
        assert_eq!(HostState::Reconnecting { attempt: 1 }.card("devbox").unwrap().button, None);
        assert_eq!(HostState::UpdateNeeded { remote: None }.card("devbox").unwrap().button, Some("Update and restart"));
        let card = HostState::Reconnecting { attempt: 3 }.card("devbox").unwrap();
        assert_eq!(card.title, "Reconnecting to devbox…");
        assert!(card.detail.contains("Attempt 3 of 4"), "{}", card.detail);
        let card = HostState::UpdateNeeded { remote: Some("protocol 6".into()) }.card("devbox").unwrap();
        assert!(card.detail.contains("protocol 6"), "{}", card.detail);
        assert_eq!(card.button, Some("Update and restart"));
        assert_eq!(HostState::NotInstalled.card("devbox").unwrap().button, Some("Install…"));
    }

    #[test]
    fn a_failed_attempt_is_read_from_the_exit_code_and_ssh_s_last_words() {
        let read = |code, stderr| classify_failure(code, stderr, None, false, None);
        assert_eq!(
            read(Some(127), "bash: line 1: ~/.local/bin/gantry: No such file or directory"),
            FailKind::NotInstalled
        );
        assert_eq!(
            read(Some(1), "sh: gantry: command not found"),
            FailKind::Other { reason: "sh: gantry: command not found".into() },
            "only exit code 127 means not installed"
        );
        assert_eq!(read(Some(255), "me@devbox: Permission denied (publickey,password)."), FailKind::NeedsInput);
        assert_eq!(read(Some(255), "Host key verification failed."), FailKind::NeedsInput);
        assert_eq!(
            read(Some(255), "ssh: connect to host devbox port 22: Connection refused\n"),
            FailKind::Other { reason: "ssh: connect to host devbox port 22: Connection refused".into() }
        );
        assert_eq!(read(Some(3), ""), FailKind::Other { reason: "ssh ended with exit code 3".into() });
        assert_eq!(read(None, ""), FailKind::Other { reason: "the connection closed".into() });
        assert_eq!(
            classify_failure(Some(255), "Permission denied", Some(6), false, None),
            FailKind::UpdateNeeded { remote: Some("protocol 6".into()) },
            "a version mismatch wins"
        );
    }

    #[test]
    fn a_changed_host_key_says_so_instead_of_asking_for_sign_in() {
        let stderr =
            "@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n@@@@\nHost key verification failed.";
        assert_eq!(
            classify_failure(Some(255), stderr, None, false, None),
            FailKind::Other { reason: "The host key of the host changed. Check ~/.ssh/known_hosts.".into() }
        );
    }

    #[test]
    fn a_missing_shared_library_is_named_with_its_package_not_called_not_installed() {
        let stderr = "/home/me/.local/bin/gantry: error while loading shared libraries: libxkbcommon-x11.so.0: cannot open shared object file: No such file or directory";
        let FailKind::Other { reason } = classify_failure(Some(127), stderr, None, false, None) else {
            panic!("expected Other");
        };
        assert!(reason.contains("libxkbcommon-x11.so.0"), "{reason}");
        assert!(reason.contains("libxkbcommon-x11-0"), "{reason}");
        let alsa = stderr.replace("libxkbcommon-x11.so.0", "libasound.so.2");
        let FailKind::Other { reason } = classify_failure(Some(127), &alsa, None, false, None) else {
            panic!("expected Other");
        };
        assert!(reason.contains("libasound2"), "{reason}");
        let unknown = stderr.replace("libxkbcommon-x11.so.0", "libfoo.so.1");
        let FailKind::Other { reason } = classify_failure(Some(127), &unknown, None, false, None) else {
            panic!("expected Other");
        };
        assert!(reason.contains("libfoo.so.1") && !reason.contains("install "), "{reason}");
        assert_eq!(classify_failure(Some(127), "sh: gantry: not found", None, false, None), FailKind::NotInstalled);
    }

    #[test]
    fn ldd_output_gives_every_missing_library_once() {
        let ldd = "\tlinux-vdso.so.1 (0x00007ffd)\n\tlibasound.so.2 => not found\n\tlibc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x1)\n\
                   \tlibxkbcommon-x11.so.0 => not found\n\tlibasound.so.2 => not found\n";
        assert_eq!(parse_ldd(ldd), ["libasound.so.2", "libxkbcommon-x11.so.0"]);
        assert!(parse_ldd("\tnot a dynamic executable\n").is_empty());
    }

    #[test]
    fn missing_libraries_come_with_one_install_command() {
        let both = describe_missing_libraries(&["libasound.so.2".into(), "libxkbcommon-x11.so.0".into()]);
        assert!(both.contains("libasound.so.2") && both.contains("libxkbcommon-x11.so.0"), "{both}");
        assert!(both.contains("sudo apt install -y libasound2t64 libxkbcommon-x11-0"), "{both}");
        assert!(both.contains("libasound2 on Ubuntu 22.04"), "{both}");
        let mixed = describe_missing_libraries(&["libxcb.so.1".into(), "libfoo.so.1".into()]);
        assert!(mixed.contains("sudo apt install -y libxcb1"), "{mixed}");
        assert!(mixed.contains("libfoo.so.1") && mixed.contains("no known package"), "{mixed}");
        let unknown = describe_missing_libraries(&["libfoo.so.1".into()]);
        assert!(!unknown.contains("sudo"), "{unknown}");
    }

    #[test]
    fn too_many_authentication_failures_points_at_the_sign_in_setting() {
        let stderr = "Received disconnect from 192.168.31.7 port 9162:2: Too many authentication failures\nDisconnected from 192.168.31.7 port 9162";
        for unasked in [true, false] {
            let FailKind::Other { reason } = classify_failure(Some(255), stderr, None, unasked, None) else {
                panic!("expected Other");
            };
            assert!(
                reason.contains("too many failed sign-in tries") && reason.contains("Password or SSH key"),
                "{reason}"
            );
        }
    }

    #[test]
    fn permission_denied_when_ssh_asked_nothing_is_an_authentication_failure_not_a_sign_in() {
        let stderr = "me@devbox: Permission denied (publickey).";
        assert_eq!(
            classify_failure(Some(255), stderr, None, true, None),
            FailKind::Other {
                reason: "Authentication failed (ssh asked for nothing): me@devbox: Permission denied (publickey)."
                    .into()
            }
        );
        assert_eq!(
            classify_failure(Some(255), stderr, None, false, None),
            FailKind::NeedsInput,
            "asked, or background"
        );
    }

    #[test]
    fn a_silent_ssh_exit_with_only_an_eof_handshake_error_reports_the_exit_code() {
        assert_eq!(
            classify_failure(Some(3), "", None, false, Some("daemon closed the connection")),
            FailKind::Other { reason: "ssh ended with exit code 3".into() }
        );
    }

    #[test]
    fn a_garbled_handshake_names_its_error_when_ssh_said_nothing() {
        assert_eq!(
            classify_failure(None, "", None, false, Some("unexpected handshake reply Exited")),
            FailKind::Other { reason: "unexpected handshake reply Exited".into() }
        );
        assert_eq!(
            classify_failure(None, "warning: x\n", None, false, Some("garbled")),
            FailKind::Other { reason: "warning: x".into() },
            "ssh's own words come first"
        );
    }

    #[test]
    fn a_remote_take_over_ends_in_opened_elsewhere_and_never_retries() {
        assert_eq!(HostState::Connected.next(HostEvent::TakenOver), HostState::TakenOver);
        assert_eq!(HostState::NotConnected.next(HostEvent::TakenOver), HostState::NotConnected);
        assert!(HostState::TakenOver.can_connect());
        assert_eq!(HostState::TakenOver.next(HostEvent::Connect), HostState::Connecting);
        assert_eq!(HostState::TakenOver.label(), "opened elsewhere");
        let card = HostState::TakenOver.card("devbox").unwrap();
        assert_eq!(card.title, "Opened from another Gantry");
        assert_eq!(card.detail, "Another Gantry took over devbox. Your sessions keep running there.");
        assert_eq!(card.button, Some("Reconnect"));
    }

    #[test]
    fn cancelling_a_connecting_host_returns_it_to_not_connected() {
        assert_eq!(HostState::Connecting.next(HostEvent::Cancel), HostState::NotConnected);
        assert_eq!(HostState::Connected.next(HostEvent::Cancel), HostState::Connected);
        assert_eq!(HostState::Connecting.card("devbox").unwrap().button, Some("Cancel"));
    }

    #[test]
    fn every_state_has_a_dot() {
        use HostDot::*;
        let dots = [
            (HostState::NotConnected, None),
            (HostState::Connecting, Busy),
            (HostState::Connected, Ok),
            (HostState::Reconnecting { attempt: 1 }, Busy),
            (HostState::Disconnected, Bad),
            (HostState::NeedsSignIn, Warn),
            (HostState::UpdateNeeded { remote: Option::None }, Warn),
            (HostState::NotInstalled, Bad),
            (HostState::Failed { reason: "x".into() }, Bad),
            (HostState::TakenOver, Warn),
        ];
        for (state, dot) in dots {
            assert_eq!(state.dot(), dot, "{state:?}");
        }
    }

    #[test]
    fn prompts_are_masked_asked_yes_no_or_plain() {
        assert_eq!(prompt_kind("me@devbox's password: "), PromptKind::Masked);
        assert_eq!(prompt_kind("Enter passphrase for key '/home/me/.ssh/id_ed25519':"), PromptKind::Masked);
        assert_eq!(
            prompt_kind("Are you sure you want to continue connecting (yes/no/[fingerprint])? "),
            PromptKind::YesNo
        );
        for prompt in ["Verification code: ", "Passwort: ", "Mot de passe : ", "Enter PASSCODE: "] {
            assert_eq!(prompt_kind(prompt), PromptKind::Masked, "{prompt}");
        }
        assert_eq!(prompt_title("devbox", "me@devbox's password: "), "Password for devbox");
        assert_eq!(prompt_title("devbox", "Enter passphrase for key 'k':"), "Key passphrase for devbox");
        assert_eq!(prompt_title("devbox", "continue connecting (yes/no)?"), "Trust devbox?");
        assert_eq!(prompt_title("devbox", "Verification code: "), "Sign in to devbox");
        assert_eq!(Duration::from_secs(1), reconnect_delay(1).unwrap());
    }
}
