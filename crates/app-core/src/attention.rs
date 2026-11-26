//! Attention: which status changes of Gantry's own sessions deserve the user's notice (spec §5.7).

use std::collections::{HashMap, HashSet};

use protocol::host::{HostId, PaneKey};
use protocol::message::{SentFile, SessionInfo, SessionState, SessionStatus};

use crate::preview::fresh_sends;
use crate::settings::{AlertSettings, AlertTone};

/// A turn counts as finished only after this much busy time unless the settings say otherwise.
const DEFAULT_FINISHED_AFTER_MS: u64 = 1_000;

/// Why a session wants attention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlertKind {
    /// It is blocked on the user.
    NeedsInput {
        /// Claude's reason, if it gave one.
        reason: Option<String>,
    },
    /// It finished a turn.
    Finished,
    /// Claude sent files to the preview.
    FileSent {
        /// The new files, oldest first; never empty.
        files: Vec<SentFile>,
    },
}

/// One status change worth telling the user about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    /// Session id.
    pub session: String,
    /// The pane running it.
    pub pane: PaneKey,
    /// What happened.
    pub kind: AlertKind,
}

/// The last status seen per session, to tell transitions from rewrites.
pub struct AttentionTracker {
    seen: HashMap<(HostId, String), Seen>,
    finished_after_ms: u64,
}

impl Default for AttentionTracker {
    fn default() -> Self {
        Self { seen: HashMap::new(), finished_after_ms: DEFAULT_FINISHED_AFTER_MS }
    }
}

struct Seen {
    status: Option<SessionStatus>,
    /// When the current busy run started (first Busy seen), while busy.
    busy_since_ms: Option<u64>,
    /// Ids of every file the session had sent.
    files: HashSet<String>,
}

impl AttentionTracker {
    /// Turns shorter than this are not "finished" (from `alerts.finishedAfterSeconds`); quicker turns and
    /// Busy↔Idle flapping are ignored.
    pub fn set_finished_after_ms(&mut self, ms: u64) {
        self.finished_after_ms = ms;
    }

    /// Start over from a full list of `host`. Nothing in it alerts: it is the baseline. Other hosts keep theirs.
    pub fn reset(&mut self, host: &HostId, sessions: &[SessionInfo]) {
        self.seen.retain(|(seen_host, _), _| seen_host != host);
        for session in sessions {
            self.observe(host, session);
        }
    }

    /// Apply a change of `host` and return the alerts it causes (managed sessions only).
    pub fn update(&mut self, host: &HostId, upserted: &[SessionInfo], removed: &[String]) -> Vec<Alert> {
        for id in removed {
            self.seen.remove(&(host.clone(), id.clone()));
        }
        let mut alerts = Vec::new();
        for session in upserted {
            let before = self.seen.get(&(host.clone(), session.id.clone()));
            if let Some(alert) = alert(host, before, session, self.finished_after_ms) {
                alerts.push(alert);
            }
            if let SessionState::Managed { pane } = session.state {
                let empty = HashSet::new();
                let known = before.map_or(&empty, |seen| &seen.files);
                let files: Vec<SentFile> = fresh_sends(known, &session.sent_files).into_iter().cloned().collect();
                if !files.is_empty() {
                    alerts.push(Alert {
                        session: session.id.clone(),
                        pane: PaneKey::new(host.clone(), pane),
                        kind: AlertKind::FileSent { files },
                    });
                }
            }
            self.observe(host, session);
        }
        alerts
    }

    fn observe(&mut self, host: &HostId, session: &SessionInfo) {
        let key = (host.clone(), session.id.clone());
        let still_busy = self.seen.get(&key).filter(|seen| seen.status == Some(SessionStatus::Busy));
        let busy_since_ms = match session.status {
            Some(SessionStatus::Busy) => {
                Some(still_busy.and_then(|seen| seen.busy_since_ms).unwrap_or(session.status_since_ms))
            }
            _ => None,
        };
        self.seen.insert(
            key,
            Seen {
                status: session.status.clone(),
                busy_since_ms,
                files: session.sent_files.iter().map(|f| f.id.clone()).collect(),
            },
        );
    }
}

fn alert(host: &HostId, before: Option<&Seen>, now: &SessionInfo, finished_after_ms: u64) -> Option<Alert> {
    let SessionState::Managed { pane } = now.state else { return None };
    let kind = match (before.and_then(|seen| seen.status.as_ref()), &now.status) {
        (Some(SessionStatus::NeedsInput { .. }), Some(SessionStatus::NeedsInput { .. })) => return None,
        (_, Some(SessionStatus::NeedsInput { reason })) => AlertKind::NeedsInput { reason: reason.clone() },
        (Some(SessionStatus::Busy), Some(SessionStatus::Idle)) => {
            let started = before.and_then(|seen| seen.busy_since_ms)?;
            if now.status_since_ms.saturating_sub(started) < finished_after_ms {
                return None;
            }
            AlertKind::Finished
        }
        _ => return None,
    };
    Some(Alert { session: now.id.clone(), pane: PaneKey::new(host.clone(), pane), kind })
}

/// Which channels an alert uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Channels {
    /// Show the unseen badge.
    pub badge: bool,
    /// Show an in-app pop-up.
    pub popup: bool,
    /// Raise a native notification.
    pub notification: bool,
    /// Play this sound.
    pub sound: Option<AlertTone>,
}

/// Decide the channels for `kind`. `visible`: the pane is in the visible tab of its window; `window_active`:
/// that window is the active one; `preview_in_view`: a following preview of the session is in the visible tab
/// (only meaningful for a file alert). A file alert is quiet while that preview is on screen in the active window.
pub fn alert_channels(
    alerts: &AlertSettings,
    kind: &AlertKind,
    visible: bool,
    window_active: bool,
    preview_in_view: bool,
) -> Channels {
    let (event, quiet) = match kind {
        AlertKind::NeedsInput { .. } => (&alerts.needs_input, false),
        AlertKind::Finished => (&alerts.finished, false),
        AlertKind::FileSent { .. } => (&alerts.file_sent, preview_in_view && window_active),
    };
    let is_file = matches!(kind, AlertKind::FileSent { .. });
    if quiet || (!is_file && !alerts.when_visible && visible && window_active) {
        return Channels::default();
    }
    if is_file && !alerts.when_visible && visible && window_active {
        // The Claude pane being on screen does not hide a file: only the preview does.
        return Channels { badge: false, popup: event.popup, notification: false, sound: None };
    }
    Channels {
        badge: event.badge,
        popup: event.popup,
        notification: event.notification && !(alerts.only_when_unfocused && window_active),
        sound: (event.sound && alerts.play_sounds).then_some(event.tone),
    }
}

fn rank(status: &SessionStatus) -> u8 {
    match status {
        SessionStatus::NeedsInput { .. } => 3,
        SessionStatus::Busy => 2,
        SessionStatus::Idle => 1,
    }
}

/// The most urgent of `statuses`: NeedsInput > Busy > Idle.
pub fn most_urgent<'a>(statuses: impl IntoIterator<Item = &'a SessionStatus>) -> Option<&'a SessionStatus> {
    statuses.into_iter().max_by_key(|status| rank(status))
}

/// `title` without the status glyph Claude puts in front of its terminal title (◐/◑ while busy, ✳ when idle);
/// Gantry shows its own status next to the title.
pub fn without_claude_glyph(title: &str) -> &str {
    title.strip_prefix(['◐', '◑', '✳']).map_or(title, str::trim_start)
}

/// Words of the last prompt a notification quotes.
const PROMPT_WORDS: usize = 5;
/// Characters of the last prompt a notification quotes, so one long word (a path) cannot fill the line.
const PROMPT_CHARS: usize = 40;

/// `prompt` collapsed to one line and cut after [`PROMPT_WORDS`] words or [`PROMPT_CHARS`] characters, at a word
/// boundary (inside the first word when that alone is too long), with `…` when something was left out.
fn prompt_snippet(prompt: &str) -> String {
    let mut snippet = String::new();
    let mut words = prompt.split_whitespace().enumerate();
    for (index, word) in words.by_ref() {
        let room = PROMPT_CHARS.saturating_sub(snippet.chars().count() + usize::from(index > 0));
        if index == PROMPT_WORDS || (index > 0 && word.chars().count() > room) {
            return format!("{snippet}…");
        }
        if index > 0 {
            snippet.push(' ');
        }
        if word.chars().count() > room {
            snippet.extend(word.chars().take(room));
            return format!("{snippet}…");
        }
        snippet.push_str(word);
    }
    snippet
}

/// Notification summary and body for `kind` on the session titled `title`. The body is what Claude waits for, or else
/// the start of the session's `last_prompt` (empty when there is neither).
pub fn describe(kind: &AlertKind, title: &str, last_prompt: Option<&str>) -> (String, String) {
    let (status, reason) = match kind {
        AlertKind::NeedsInput { reason } => ("Needs input", reason.as_deref()),
        AlertKind::Finished => ("Finished", None),
        AlertKind::FileSent { files } => {
            let body = match files.as_slice() {
                [one] => crate::preview::file_name(&one.path),
                many => format!("{} files", many.len()),
            };
            return (format!("File sent · {title}"), body);
        }
    };
    let body = reason.map(str::to_string).unwrap_or_else(|| {
        let snippet = last_prompt.map(prompt_snippet).unwrap_or_default();
        if snippet.is_empty() { snippet } else { format!("You: {snippet}") }
    });
    (format!("{status} · {title}"), body)
}

/// The pop-up heading for `count` new files.
pub fn file_sent_heading(count: usize) -> String {
    if count == 1 { "Claude sent a file".to_string() } else { format!("Claude sent {count} files") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::EventAlerts;
    use protocol::message::SessionState;

    fn at(id: &str, state: SessionState, status: Option<SessionStatus>, since: u64) -> SessionInfo {
        SessionInfo {
            sent_files: Vec::new(),
            id: id.into(),
            cwd: "/r".into(),
            title: String::new(),
            last_active_ms: 0,
            state,
            cwd_exists: true,
            status,
            status_since_ms: since,
            container: None,
            claude_dir: None,
            last_prompt: None,
            host: protocol::host::HostId::local(),
        }
    }

    fn mine(status: Option<SessionStatus>, since: u64) -> SessionInfo {
        at("s", SessionState::Managed { pane: 7 }, status, since)
    }

    fn asks() -> Option<SessionStatus> {
        Some(SessionStatus::NeedsInput { reason: Some("dialog open".into()) })
    }

    fn kinds(alerts: Vec<Alert>) -> Vec<AlertKind> {
        alerts.into_iter().map(|alert| alert.kind).collect()
    }

    #[test]
    fn needs_input_alerts_once_per_episode() {
        let mut tracker = AttentionTracker::default();
        tracker.reset(&HostId::local(), &[mine(Some(SessionStatus::Busy), 1)]);
        let alerts = tracker.update(&HostId::local(), &[mine(asks(), 2)], &[]);
        assert_eq!(
            alerts,
            vec![Alert {
                session: "s".into(),
                pane: PaneKey::local(7),
                kind: AlertKind::NeedsInput { reason: Some("dialog open".into()) }
            }]
        );
        // Claude rewrites the file while still waiting: no second alert.
        assert!(tracker.update(&HostId::local(), &[mine(asks(), 3)], &[]).is_empty());
        // Answered, then asks again: a new episode.
        tracker.update(&HostId::local(), &[mine(Some(SessionStatus::Busy), 4)], &[]);
        assert_eq!(kinds(tracker.update(&HostId::local(), &[mine(asks(), 5)], &[])).len(), 1);
    }

    #[test]
    fn finished_needs_a_busy_run_of_one_second() {
        let mut tracker = AttentionTracker::default();
        tracker.reset(&HostId::local(), &[mine(Some(SessionStatus::Idle), 0)]);
        tracker.update(&HostId::local(), &[mine(Some(SessionStatus::Busy), 10_000)], &[]);
        // Busy for 0.9 s: a quick turn, or flapping.
        assert!(tracker.update(&HostId::local(), &[mine(Some(SessionStatus::Idle), 10_900)], &[]).is_empty());
        tracker.update(&HostId::local(), &[mine(Some(SessionStatus::Busy), 20_000)], &[]);
        assert_eq!(
            kinds(tracker.update(&HostId::local(), &[mine(Some(SessionStatus::Idle), 21_000)], &[])),
            vec![AlertKind::Finished]
        );
    }

    #[test]
    fn busy_time_counts_from_the_start_of_the_run() {
        let mut tracker = AttentionTracker::default();
        tracker.reset(&HostId::local(), &[mine(Some(SessionStatus::Idle), 0)]);
        tracker.update(&HostId::local(), &[mine(Some(SessionStatus::Busy), 1_000)], &[]);
        // A rewrite while still busy moves `statusUpdatedAt`; the run still started at 1 000.
        tracker.update(&HostId::local(), &[mine(Some(SessionStatus::Busy), 3_500)], &[]);
        assert_eq!(
            kinds(tracker.update(&HostId::local(), &[mine(Some(SessionStatus::Idle), 4_200)], &[])),
            vec![AlertKind::Finished]
        );
    }

    #[test]
    fn the_initial_list_sets_a_baseline_without_alerts() {
        let mut tracker = AttentionTracker::default();
        tracker.reset(&HostId::local(), &[mine(asks(), 1)]);
        assert!(tracker.update(&HostId::local(), &[mine(asks(), 2)], &[]).is_empty());
        // A session first seen in a change (a new pane, or a new id after /clear) can alert right away.
        let fresh = at("new", SessionState::Managed { pane: 8 }, asks(), 3);
        assert_eq!(tracker.update(&HostId::local(), &[fresh], &[]).len(), 1);
    }

    #[test]
    fn sessions_outside_ccm_never_alert() {
        let mut tracker = AttentionTracker::default();
        let outside = |status, since| at("e", SessionState::External { pid: Some(9) }, status, since);
        tracker.reset(&HostId::local(), &[outside(Some(SessionStatus::Busy), 0)]);
        assert!(tracker.update(&HostId::local(), &[outside(asks(), 1)], &[]).is_empty());
        assert!(tracker.update(&HostId::local(), &[outside(Some(SessionStatus::Idle), 9_000)], &[]).is_empty());
    }

    #[test]
    fn removed_sessions_are_forgotten() {
        let mut tracker = AttentionTracker::default();
        tracker.reset(&HostId::local(), &[mine(asks(), 1)]);
        tracker.update(&HostId::local(), &[], &["s".to_string()]);
        // Back with the same status: no baseline any more, so it counts as new.
        assert_eq!(tracker.update(&HostId::local(), &[mine(asks(), 2)], &[]).len(), 1);
    }

    #[test]
    fn needs_input_outranks_busy_outranks_idle() {
        let (idle, busy, needs) =
            (SessionStatus::Idle, SessionStatus::Busy, SessionStatus::NeedsInput { reason: None });
        assert_eq!(most_urgent([&idle, &needs, &busy]), Some(&needs));
        assert_eq!(most_urgent([&idle, &busy]), Some(&busy));
        assert_eq!(most_urgent([]), None);
    }

    #[test]
    fn claudes_own_status_glyph_is_dropped_from_titles() {
        assert_eq!(without_claude_glyph("◐ Poem about terminals"), "Poem about terminals");
        assert_eq!(without_claude_glyph("◑ Poem"), "Poem");
        assert_eq!(without_claude_glyph("✳ Poem"), "Poem");
        assert_eq!(without_claude_glyph("Claude Code"), "Claude Code");
        assert_eq!(without_claude_glyph("◐"), "");
    }

    #[test]
    fn notification_text_names_the_session_and_why() {
        let needs = AlertKind::NeedsInput { reason: Some("dialog open".into()) };
        assert_eq!(
            describe(&needs, "Fix CI", Some("fix the build")),
            ("Needs input · Fix CI".into(), "dialog open".into())
        );
        assert_eq!(
            describe(&AlertKind::NeedsInput { reason: None }, "Fix CI", Some("fix the build")).1,
            "You: fix the build"
        );
        assert_eq!(
            describe(&AlertKind::Finished, "Fix CI", Some("fix the build")),
            ("Finished · Fix CI".into(), "You: fix the build".into())
        );
    }

    #[test]
    fn notification_quotes_five_words_of_the_last_prompt() {
        let body = |prompt: Option<&str>| describe(&AlertKind::Finished, "t", prompt).1;
        assert_eq!(body(Some("one two three four five six")), "You: one two three four five…");
        assert_eq!(body(Some("one two three four five")), "You: one two three four five");
        assert_eq!(body(Some("  a \n b  ")), "You: a b");
        assert_eq!(body(Some("fix the very long-running-continuous-integration job")), "You: fix the very…");
        assert_eq!(body(Some("see @/home/alice/Private/Repos/rust_terminal/docs/plan.md")), "You: see…");
        assert_eq!(
            body(Some("/home/alice/Private/Repos/rust_terminal/docs/plan.md")),
            "You: /home/alice/Private/Repos/rust_terminal/…"
        );
        assert_eq!(body(Some("   ")), "");
        assert_eq!(body(None), "");
    }

    #[test]
    fn threshold_comes_from_settings() {
        let mut tracker = AttentionTracker::default();
        tracker.set_finished_after_ms(10_000);
        tracker.reset(&HostId::local(), &[mine(Some(SessionStatus::Idle), 0)]);
        tracker.update(&HostId::local(), &[mine(Some(SessionStatus::Busy), 10_000)], &[]);
        assert!(tracker.update(&HostId::local(), &[mine(Some(SessionStatus::Idle), 15_000)], &[]).is_empty());
        tracker.update(&HostId::local(), &[mine(Some(SessionStatus::Busy), 20_000)], &[]);
        assert_eq!(
            kinds(tracker.update(&HostId::local(), &[mine(Some(SessionStatus::Idle), 31_000)], &[])),
            vec![AlertKind::Finished]
        );
    }

    fn all_on() -> EventAlerts {
        EventAlerts { badge: true, popup: false, notification: true, sound: true, tone: AlertTone::FallingBlip }
    }

    fn settings() -> AlertSettings {
        AlertSettings {
            needs_input: all_on(),
            finished: all_on(),
            file_sent: all_on(),
            finished_after_seconds: 3,
            read_after_seconds: 2,
            when_visible: false,
            only_when_unfocused: false,
            play_sounds: true,
            volume: 1.0,
        }
    }

    fn finished() -> AlertKind {
        AlertKind::Finished
    }

    #[test]
    fn visible_pane_is_quiet_by_default() {
        assert_eq!(alert_channels(&settings(), &finished(), true, true, false), Channels::default());
        // Visible tab of a window that is not active still alerts.
        assert!(alert_channels(&settings(), &finished(), true, false, false).badge);
    }

    #[test]
    fn when_visible_alerts_a_visible_pane() {
        let alerts = AlertSettings { when_visible: true, ..settings() };
        assert_eq!(
            alert_channels(&alerts, &finished(), true, true, false),
            Channels { badge: true, popup: false, notification: true, sound: Some(AlertTone::FallingBlip) }
        );
    }

    #[test]
    fn only_when_unfocused_drops_notification_but_keeps_badge_and_sound() {
        let alerts = AlertSettings { only_when_unfocused: true, ..settings() };
        assert_eq!(
            alert_channels(&alerts, &finished(), false, true, false),
            Channels { badge: true, popup: false, notification: false, sound: Some(AlertTone::FallingBlip) }
        );
        assert!(alert_channels(&alerts, &finished(), false, false, false).notification);
    }

    #[test]
    fn play_sounds_off_mutes_every_event() {
        let alerts = AlertSettings { play_sounds: false, ..settings() };
        assert_eq!(alert_channels(&alerts, &finished(), false, false, false).sound, None);
        assert_eq!(alert_channels(&alerts, &AlertKind::NeedsInput { reason: None }, false, false, false).sound, None);
    }

    #[test]
    fn grid_cells_are_independent() {
        let alerts = AlertSettings {
            needs_input: EventAlerts {
                badge: false,
                popup: false,
                notification: false,
                sound: true,
                tone: AlertTone::FallingBlip,
            },
            finished: EventAlerts {
                badge: false,
                popup: false,
                notification: true,
                sound: false,
                tone: AlertTone::ClassicBlip,
            },
            ..settings()
        };
        assert_eq!(
            alert_channels(&alerts, &AlertKind::NeedsInput { reason: None }, false, false, false),
            Channels { badge: false, popup: false, notification: false, sound: Some(AlertTone::FallingBlip) }
        );
        assert_eq!(
            alert_channels(&alerts, &finished(), false, false, false),
            Channels { badge: false, popup: false, notification: true, sound: None }
        );
    }

    #[test]
    fn two_hosts_with_the_same_session_and_pane_ids_do_not_disturb_each_other() {
        let (local, devbox) = (HostId::local(), HostId::named("devbox"));
        let mut tracker = AttentionTracker::default();
        tracker.reset(&local, &[mine(Some(SessionStatus::Busy), 1)]);
        tracker.reset(&devbox, &[mine(Some(SessionStatus::Busy), 1)]);
        // devbox's session asks: the alert names devbox's pane 7, and the local baseline is untouched.
        let alerts = tracker.update(&devbox, &[mine(asks(), 2)], &[]);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].pane, PaneKey::new(devbox.clone(), 7));
        // A new full list from devbox does not forget the local session: it can still alert on its own change.
        tracker.reset(&devbox, &[]);
        let alerts = tracker.update(&local, &[mine(asks(), 3)], &[]);
        assert_eq!(alerts[0].pane, PaneKey::local(7));
    }

    fn sent(id: &str, ms: u64, backlog: bool) -> SentFile {
        SentFile { id: id.into(), path: format!("/r/{id}.md").into(), sent_ms: ms, caption: None, backlog }
    }

    fn with_files(state: SessionState, files: Vec<SentFile>) -> SessionInfo {
        SessionInfo { sent_files: files, ..at("s", state, None, 0) }
    }

    fn managed_files(files: Vec<SentFile>) -> SessionInfo {
        with_files(SessionState::Managed { pane: 7 }, files)
    }

    #[test]
    fn twenty_files_in_one_update_are_one_alert() {
        let mut tracker = AttentionTracker::default();
        tracker.reset(&HostId::local(), &[managed_files(Vec::new())]);
        let files: Vec<_> = (0..20).rev().map(|i| sent(&format!("f{i}"), i, false)).collect();
        let alerts = tracker.update(&HostId::local(), &[managed_files(files)], &[]);
        assert_eq!(alerts.len(), 1);
        let AlertKind::FileSent { files } = &alerts[0].kind else { panic!("not a file alert") };
        assert_eq!(files.len(), 20);
        assert!(files.windows(2).all(|w| w[0].sent_ms <= w[1].sent_ms));
    }

    #[test]
    fn backlog_and_known_files_raise_nothing() {
        let mut tracker = AttentionTracker::default();
        tracker.reset(&HostId::local(), &[managed_files(Vec::new())]);
        let files = vec![sent("a", 1, true), sent("b", 2, true)];
        assert!(tracker.update(&HostId::local(), &[managed_files(files.clone())], &[]).is_empty());
        assert!(tracker.update(&HostId::local(), &[managed_files(files)], &[]).is_empty());
    }

    #[test]
    fn a_session_new_since_the_baseline_alerts_on_non_backlog_files() {
        let mut tracker = AttentionTracker::default();
        tracker.reset(&HostId::local(), &[]);
        let files = vec![sent("a", 1, true), sent("b", 2, false)];
        let alerts = tracker.update(&HostId::local(), &[managed_files(files)], &[]);
        let [alert] = alerts.as_slice() else { panic!("expected one alert") };
        let AlertKind::FileSent { files } = &alert.kind else { panic!("not a file alert") };
        assert_eq!(files.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), ["b"]);
    }

    #[test]
    fn external_sessions_never_alert_for_files() {
        let mut tracker = AttentionTracker::default();
        let outside = |files| with_files(SessionState::External { pid: Some(9) }, files);
        tracker.reset(&HostId::local(), &[outside(Vec::new())]);
        assert!(tracker.update(&HostId::local(), &[outside(vec![sent("a", 1, false)])], &[]).is_empty());
    }

    #[test]
    fn file_sent_popup_ignores_the_claude_pane_but_not_the_preview() {
        let alerts = settings();
        let kind = AlertKind::FileSent { files: vec![sent("a", 1, false)] };
        let file_alerts =
            AlertSettings { file_sent: EventAlerts { popup: true, notification: false, ..all_on() }, ..alerts };
        let c = alert_channels(&file_alerts, &kind, true, true, false);
        assert!(c.popup && !c.notification);
        assert_eq!(alert_channels(&file_alerts, &kind, false, true, true), Channels::default());
        assert!(alert_channels(&file_alerts, &kind, false, false, true).popup);
    }

    #[test]
    fn status_popups_follow_the_visibility_rule() {
        let alerts = AlertSettings { finished: EventAlerts { popup: true, ..all_on() }, ..settings() };
        assert!(!alert_channels(&alerts, &finished(), true, true, false).popup);
        assert!(alert_channels(&alerts, &finished(), false, true, false).popup);
        assert!(!alert_channels(&settings(), &finished(), false, true, false).popup);
    }

    #[test]
    fn describe_file_sent() {
        let one = AlertKind::FileSent { files: vec![SentFile { path: "/r/plan.md".into(), ..sent("a", 1, false) }] };
        assert_eq!(describe(&one, "api", None), ("File sent · api".into(), "plan.md".into()));
        let three = AlertKind::FileSent { files: vec![sent("a", 1, false), sent("b", 2, false), sent("c", 3, false)] };
        assert_eq!(describe(&three, "api", None).1, "3 files");
    }

    #[test]
    fn file_sent_heading_counts() {
        assert_eq!(file_sent_heading(1), "Claude sent a file");
        assert_eq!(file_sent_heading(4), "Claude sent 4 files");
    }
}
