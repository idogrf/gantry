//! Stopping a Claude that runs outside Gantry, so its session can be resumed in a pane (spec §4.5).

use std::time::{Duration, Instant};

use claude_index::host::Host;
use claude_index::index::SessionRecord;
use protocol::message::{SessionInfo, SessionState};

/// How long a process asked to stop (SIGTERM) gets before the user is offered Force quit.
pub const GRACE: Duration = Duration::from_secs(5);
/// How long a killed process (SIGKILL) gets to disappear.
pub const FORCE_WAIT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(100);

/// The process to stop: its pid and the start time that proves it is still that process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// Process id.
    pub pid: u32,
    /// Start time from its pid file (`procStart`).
    pub start: String,
}

/// The process running `session` outside Gantry. Refused (with the reason) for sessions in containers, in Gantry's own
/// panes, not running, unknown, or without a recorded start time.
pub fn target(sessions: &[SessionInfo], records: &[SessionRecord], session: &str) -> Result<Target, String> {
    let info = sessions.iter().find(|s| s.id == session).ok_or("unknown session")?;
    let pid = match info.state {
        SessionState::External { pid: Some(pid) } => pid,
        SessionState::External { pid: None } => return Err("it runs in a container; stop it there".into()),
        SessionState::Managed { .. } => return Err("it already runs in Gantry".into()),
        SessionState::Closed => return Err("it is not running".into()),
    };
    let start = records
        .iter()
        .find(|r| r.id == session)
        .and_then(|r| r.proc_start.clone())
        .ok_or("no start time; cannot verify it")?;
    Ok(Target { pid, start })
}

/// Ask `target` to stop (SIGTERM), or kill it (`force`: SIGKILL), after checking it is still that process.
/// `Ok(false)` when it is already gone (nothing was signalled).
pub fn signal(target: &Target, force: bool, host: &impl Host) -> Result<bool, String> {
    match host.is_running(target.pid, Some(&target.start)) {
        None => Err("cannot verify processes on this OS yet".into()),
        Some(false) => Ok(false),
        Some(true) => kill(target.pid, force).map(|()| true),
    }
}

#[cfg(unix)]
fn kill(pid: u32, force: bool) -> Result<(), String> {
    use rustix::process::{Pid, Signal, kill_process};
    let pid = i32::try_from(pid).ok().and_then(Pid::from_raw).ok_or_else(|| format!("invalid pid {pid}"))?;
    kill_process(pid, if force { Signal::KILL } else { Signal::TERM })
        .map_err(|err| format!("could not signal it: {err}"))
}

#[cfg(windows)]
fn kill(pid: u32, _force: bool) -> Result<(), String> {
    claude_index::host::terminate(pid).map_err(|err| format!("could not stop it: {err}"))
}

/// Wait until `target` is gone (exited, or its pid reused by another process); false if it still runs after `timeout`.
pub async fn wait_for_exit(target: &Target, host: &impl Host, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if host.is_running(target.pid, Some(&target.start)) != Some(true) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use claude_index::index::IndexState;
    use protocol::message::SessionState;

    /// A host that answers every liveness question the same way and never touches real processes.
    struct FakeHost(Option<bool>);

    impl Host for FakeHost {
        fn is_running(&self, _pid: u32, _start: Option<&str>) -> Option<bool> {
            self.0
        }
        fn pid_domain(&self) -> Option<String> {
            None
        }
        fn dir_exists(&self, _path: &Path) -> bool {
            true
        }
    }

    fn info(id: &str, state: SessionState) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            cwd: "/r".into(),
            title: String::new(),
            last_active_ms: 0,
            state,
            cwd_exists: true,
            status: None,
            status_since_ms: 0,
            container: None,
            claude_dir: None,
            last_prompt: None,
            host: protocol::host::HostId::local(),
        }
    }

    fn record(id: &str, start: Option<&str>) -> SessionRecord {
        SessionRecord {
            id: id.into(),
            cwd: "/r".into(),
            title: String::new(),
            last_active_ms: 0,
            state: IndexState::Running { pid: 42 },
            cwd_exists: true,
            activity: None,
            activity_since_ms: 0,
            proc_start: start.map(Into::into),
            container_id: None,
            claude_dir: None,
            last_prompt: None,
        }
    }

    #[test]
    fn target_is_the_outside_process_with_its_start_time() {
        let sessions = [info("e", SessionState::External { pid: Some(42) })];
        assert_eq!(target(&sessions, &[record("e", Some("9"))], "e"), Ok(Target { pid: 42, start: "9".into() }));
    }

    #[test]
    fn target_refuses_what_it_cannot_or_must_not_stop() {
        let sessions = [
            info("box", SessionState::External { pid: None }),
            info("mine", SessionState::Managed { pane: 3 }),
            info("old", SessionState::Closed),
            info("nostart", SessionState::External { pid: Some(42) }),
        ];
        let records =
            [record("box", Some("1")), record("mine", Some("1")), record("old", Some("1")), record("nostart", None)];
        let refused = |id: &str| target(&sessions, &records, id).unwrap_err();
        assert!(refused("box").contains("container"), "{}", refused("box"));
        assert!(refused("mine").contains("already runs in Gantry"), "{}", refused("mine"));
        assert!(refused("old").contains("not running"), "{}", refused("old"));
        assert!(refused("nostart").contains("no start time"), "{}", refused("nostart"));
        assert!(refused("unknown").contains("unknown session"), "{}", refused("unknown"));
        // Shown in a one-line dialog: keep every reason short.
        for id in ["box", "mine", "old", "nostart", "unknown"] {
            assert!(refused(id).chars().count() <= 38, "too long for the dialog: {}", refused(id));
        }
    }

    #[test]
    fn signal_never_touches_a_process_it_cannot_verify() {
        let target = Target { pid: 42, start: "9".into() };
        // This host cannot check processes (macOS/Windows until M7): refuse.
        let error = signal(&target, false, &FakeHost(None)).unwrap_err();
        assert!(error.contains("cannot verify") && error.chars().count() <= 38, "{error}");
        // Gone, or pid 42 now belongs to a process with another start time: nothing to stop.
        assert_eq!(signal(&target, true, &FakeHost(Some(false))), Ok(false));
    }

    #[tokio::test]
    async fn waiting_ends_when_the_process_is_gone_or_the_time_is_up() {
        let target = Target { pid: 42, start: "9".into() };
        assert!(wait_for_exit(&target, &FakeHost(Some(false)), Duration::from_secs(5)).await);
        assert!(!wait_for_exit(&target, &FakeHost(Some(true)), Duration::from_millis(250)).await);
    }
}
