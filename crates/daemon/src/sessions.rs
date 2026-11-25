//! The daemon's view of Claude sessions: the index's records plus which pane runs which session.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use claude_index::host::LocalHost;
use claude_index::index::{Index, IndexState, SessionRecord};
use claude_index::live::Activity;
use protocol::message::{PaneId, SessionInfo, SessionState, SessionStatus};

use crate::containers::Registry;

/// Full rescan interval (also catches processes that died without removing their pid file).
const RESCAN: Duration = Duration::from_secs(5);
/// Quiet time after a change notification before rescanning (writes come in bursts).
const DEBOUNCE: Duration = Duration::from_millis(200);

/// A live pane, as session matching sees it.
pub struct PaneSession {
    /// Pane id.
    pub pane: PaneId,
    /// Its process.
    pub pid: Option<u32>,
    /// Session its command line names (`claude --session-id X` / `--resume X`).
    pub session_arg: Option<String>,
}

/// A process the daemon saw exit (a pane's, or one it took over), identified by pid and start time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gone {
    /// Its pid.
    pub pid: u32,
    /// Its start time, as `/proc` reported it while it ran.
    pub start: Option<String>,
}

/// Whether the index still says `gone` runs (its pid file has not been re-checked yet).
pub fn still_claimed(gone: &Gone, records: &[SessionRecord]) -> bool {
    records.iter().any(|r| r.state == IndexState::Running { pid: gone.pid } && r.proc_start == gone.start)
}

/// What session matching knows about dev containers.
pub struct ContainerView<'a> {
    /// Known containers and mappings.
    pub registry: &'a Registry,
    /// Host pid → the pane whose `GANTRY_PANE` marker that container process carries.
    pub marked: &'a HashMap<u32, PaneId>,
    /// Pid file pid → the pane whose process it descends from (a Claude started through a wrapper).
    pub owned: &'a HashMap<u32, PaneId>,
    /// Whether a host directory exists.
    pub dir_exists: &'a dyn Fn(&Path) -> bool,
    /// Whether this OS can tell if a pid file's process still runs. Without it a running record no pane owns may be
    /// a stale file, so it is shown as closed.
    pub checks_processes: bool,
}

/// Combine index records with panes. A running record whose process is a pane's process or descends from it — or, in
/// a container, carries a pane's marker — is managed by that pane. The session a pane was started with is managed by
/// it while that pane has no running record of its own (Claude has not written its pid file yet); once it has one
/// (e.g. after `/clear`), the start session is closed. A record whose process is `gone` counts as closed: the index has
/// not noticed the exit yet. Container sessions report their host directory when it is known.
pub fn resolve(
    records: &[SessionRecord],
    panes: &[PaneSession],
    gone: &[Gone],
    view: &ContainerView,
) -> Vec<SessionInfo> {
    let index_state = |r: &SessionRecord| match r.state {
        IndexState::Running { pid } if gone.iter().any(|g| g.pid == pid && g.start == r.proc_start) => {
            IndexState::Closed
        }
        state => state,
    };
    let by_pid: HashMap<u32, PaneId> = panes.iter().filter_map(|p| Some((p.pid?, p.pane))).collect();
    let running_pane = |state: IndexState| match state {
        IndexState::Running { pid } => by_pid.get(&pid).or_else(|| view.owned.get(&pid)).copied(),
        IndexState::InContainer { pid: Some(pid) } => view.marked.get(&pid).copied(),
        _ => None,
    };
    let reporting: HashSet<PaneId> = records.iter().filter_map(|r| running_pane(index_state(r))).collect();
    let by_arg: HashMap<&str, PaneId> = panes
        .iter()
        .filter(|p| !reporting.contains(&p.pane))
        .filter_map(|p| Some((p.session_arg.as_deref()?, p.pane)))
        .collect();
    records
        .iter()
        .map(|r| {
            let state = match (index_state(r), running_pane(index_state(r))) {
                (_, Some(pane)) => SessionState::Managed { pane },
                (IndexState::Running { .. }, None) if !view.checks_processes => SessionState::Closed,
                (IndexState::Running { pid }, None) => SessionState::External { pid: Some(pid) },
                (IndexState::InContainer { pid }, None) => SessionState::External { pid },
                (IndexState::Closed, None) => match by_arg.get(r.id.as_str()) {
                    Some(pane) => SessionState::Managed { pane: *pane },
                    None => SessionState::Closed,
                },
            };
            // A gone process's last status is not the new pane's.
            let status = if index_state(r) == r.state { status(r, state) } else { None };
            let placed = view.registry.place(r, view.dir_exists);
            let (cwd, cwd_exists) = match placed.as_ref().map(|(_, host)| host) {
                Some(Some(host)) => (host.clone(), (view.dir_exists)(host)),
                Some(None) => (r.cwd.clone(), false),
                None => (r.cwd.clone(), r.cwd_exists),
            };
            SessionInfo {
                id: r.id.clone(),
                cwd,
                title: r.title.clone(),
                last_active_ms: r.last_active_ms,
                state,
                cwd_exists,
                status_since_ms: if status.is_some() { r.activity_since_ms } else { 0 },
                status,
                container: placed.map(|(place, _)| place),
                claude_dir: r.claude_dir.clone(),
                last_prompt: r.last_prompt.clone(),
                sent_files: Vec::new(),
                host: protocol::host::HostId::local(),
            }
        })
        .collect()
}

/// The status a session shows: what its process reports, while something runs it.
fn status(record: &SessionRecord, state: SessionState) -> Option<SessionStatus> {
    if state == SessionState::Closed {
        return None;
    }
    record.activity.as_ref().map(|activity| match activity {
        Activity::Busy => SessionStatus::Busy,
        Activity::Idle => SessionStatus::Idle,
        Activity::Waiting { reason } => SessionStatus::NeedsInput { reason: reason.clone() },
    })
}

/// What changed from `old` to `new`: sessions added or changed, and ids that disappeared.
pub fn diff(old: &[SessionInfo], new: &[SessionInfo]) -> (Vec<SessionInfo>, Vec<String>) {
    let before: HashMap<&str, &SessionInfo> = old.iter().map(|s| (s.id.as_str(), s)).collect();
    let after: HashSet<&str> = new.iter().map(|s| s.id.as_str()).collect();
    let upserted = new.iter().filter(|s| before.get(s.id.as_str()).copied() != Some(*s)).cloned().collect();
    let removed = old.iter().filter(|s| !after.contains(s.id.as_str())).map(|s| s.id.clone()).collect();
    (upserted, removed)
}

/// One list from the scans of several config dirs: a session found in more than one keeps the record that was
/// active most recently; most recently active first.
pub fn merge_records<'a>(scans: impl IntoIterator<Item = &'a Arc<Vec<SessionRecord>>>) -> Vec<SessionRecord> {
    let mut by_id: HashMap<&str, &SessionRecord> = HashMap::new();
    for record in scans.into_iter().flat_map(|scan| scan.iter()) {
        if by_id.get(record.id.as_str()).is_none_or(|old| old.last_active_ms < record.last_active_ms) {
            by_id.insert(record.id.as_str(), record);
        }
    }
    let mut list: Vec<SessionRecord> = by_id.into_values().cloned().collect();
    list.sort_by(|a, b| b.last_active_ms.cmp(&a.last_active_ms).then_with(|| a.id.cmp(&b.id)));
    list
}

/// Index `config` on a thread of its own: rescan on change notifications (debounced) and every
/// `RESCAN`, publishing the first list and then each changed one to `tx`. Stops once `tx` has no receivers.
pub fn spawn_indexer(config: PathBuf, tx: tokio::sync::watch::Sender<Option<Arc<Vec<SessionRecord>>>>) {
    let spawned = std::thread::Builder::new().name("claude-index".into()).spawn(move || {
        let (wake_tx, wake) = std::sync::mpsc::channel();
        let _watcher = claude_index::watch::watch(&config, move || {
            let _ = wake_tx.send(());
        })
        .map_err(|err| tracing::warn!("watching {} failed: {err}", config.display()))
        .ok();
        let mut index = Index::new(config, LocalHost::new());
        loop {
            let records = index.scan();
            let changed = tx.borrow().as_deref().is_none_or(|old| *old != records);
            if changed && tx.send(Some(Arc::new(records))).is_err() {
                return;
            }
            if tx.is_closed() {
                return;
            }
            match wake.recv_timeout(RESCAN) {
                Ok(()) => {
                    std::thread::sleep(DEBOUNCE);
                    while wake.try_recv().is_ok() {}
                }
                Err(RecvTimeoutError::Timeout) => {}
                // No watcher: rescan on the timer only.
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(RESCAN),
            }
        }
    });
    if let Err(err) = spawned {
        tracing::warn!("could not start the session index: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::message::ContainerPlace;

    /// `resolve` with no containers known and no container processes marked.
    fn plain(records: &[SessionRecord], panes: &[PaneSession], gone: &[Gone]) -> Vec<SessionInfo> {
        let (registry, marked) = (Registry::default(), HashMap::new());
        resolve(
            records,
            panes,
            gone,
            &ContainerView {
                registry: &registry,
                marked: &marked,
                owned: &HashMap::new(),
                dir_exists: &|_| true,
                checks_processes: true,
            },
        )
    }

    fn record(id: &str, state: IndexState) -> SessionRecord {
        SessionRecord {
            id: id.into(),
            cwd: "/r".into(),
            title: id.into(),
            last_active_ms: 1,
            state,
            cwd_exists: true,
            activity: None,
            activity_since_ms: 0,
            proc_start: None,
            container_id: None,
            claude_dir: None,
            last_prompt: None,
        }
    }

    #[test]
    fn resolve_passes_the_last_prompt_on() {
        let mut r = record("a", IndexState::Closed);
        r.last_prompt = Some("what next".into());
        assert_eq!(plain(&[r], &[], &[])[0].last_prompt.as_deref(), Some("what next"));
    }

    fn pane(pane: PaneId, pid: u32, session_arg: Option<&str>) -> PaneSession {
        PaneSession { pane, pid: Some(pid), session_arg: session_arg.map(Into::into) }
    }

    fn states(sessions: &[SessionInfo]) -> Vec<(&str, SessionState)> {
        sessions.iter().map(|s| (s.id.as_str(), s.state)).collect()
    }

    #[test]
    fn a_pane_process_makes_its_session_managed_and_others_external() {
        let records = [
            record("mine", IndexState::Running { pid: 100 }),
            record("other", IndexState::Running { pid: 200 }),
            record("boxed", IndexState::InContainer { pid: None }),
            record("old", IndexState::Closed),
        ];
        let sessions = plain(&records, &[pane(7, 100, None)], &[]);
        assert_eq!(
            states(&sessions),
            vec![
                ("mine", SessionState::Managed { pane: 7 }),
                ("other", SessionState::External { pid: Some(200) }),
                ("boxed", SessionState::External { pid: None }),
                ("old", SessionState::Closed),
            ]
        );
        assert_eq!((sessions[0].title.as_str(), sessions[0].cwd_exists), ("mine", true));
    }

    #[test]
    fn a_pane_is_managed_by_its_launch_session_until_its_process_reports() {
        let records = [record("launched", IndexState::Closed)];
        assert_eq!(
            states(&plain(&records, &[pane(7, 100, Some("launched"))], &[])),
            vec![("launched", SessionState::Managed { pane: 7 })]
        );
    }

    #[test]
    fn after_clear_the_launch_session_is_closed() {
        let records = [record("launched", IndexState::Closed), record("after-clear", IndexState::Running { pid: 100 })];
        assert_eq!(
            states(&plain(&records, &[pane(7, 100, Some("launched"))], &[])),
            vec![("launched", SessionState::Closed), ("after-clear", SessionState::Managed { pane: 7 }),]
        );
    }

    #[test]
    fn live_sessions_carry_their_status_and_closed_ones_none() {
        let mut busy = record("busy", IndexState::Running { pid: 100 });
        (busy.activity, busy.activity_since_ms) = (Some(Activity::Busy), 50);
        let mut asks = record("asks", IndexState::InContainer { pid: None });
        (asks.activity, asks.activity_since_ms) = (Some(Activity::Waiting { reason: Some("dialog open".into()) }), 60);
        let mut stale = record("old", IndexState::Closed);
        (stale.activity, stale.activity_since_ms) = (Some(Activity::Idle), 70);
        let sessions = plain(&[busy, asks, stale], &[pane(7, 100, None)], &[]);
        let status: Vec<(&str, Option<SessionStatus>, u64)> =
            sessions.iter().map(|s| (s.id.as_str(), s.status.clone(), s.status_since_ms)).collect();
        assert_eq!(
            status,
            vec![
                ("busy", Some(SessionStatus::Busy), 50),
                ("asks", Some(SessionStatus::NeedsInput { reason: Some("dialog open".into()) }), 60),
                ("old", None, 0),
            ]
        );
    }

    #[test]
    fn without_process_checks_an_outside_running_record_is_closed_but_a_pane_keeps_its_own() {
        let records =
            [record("outside", IndexState::Running { pid: 100 }), record("mine", IndexState::Running { pid: 7 })];
        let panes = [PaneSession { pane: 1, pid: Some(7), session_arg: None }];
        let registry = Registry::default();
        let marked = HashMap::new();
        let view = ContainerView {
            registry: &registry,
            marked: &marked,
            owned: &HashMap::new(),
            dir_exists: &|_| true,
            checks_processes: false,
        };
        assert_eq!(
            states(&resolve(&records, &panes, &[], &view)),
            vec![("outside", SessionState::Closed), ("mine", SessionState::Managed { pane: 1 })]
        );
    }

    /// `resolve` where pid 77028 and 76638 descend from panes 1 (pid 77017) and 2 (pid 76623).
    fn wrapped(records: &[SessionRecord], panes: &[PaneSession], checks_processes: bool) -> Vec<SessionInfo> {
        let (registry, marked) = (Registry::default(), HashMap::new());
        let owned = HashMap::from([(77028, 1), (76638, 2)]);
        let view = ContainerView {
            registry: &registry,
            marked: &marked,
            owned: &owned,
            dir_exists: &|_| true,
            checks_processes,
        };
        resolve(records, panes, &[], &view)
    }

    #[test]
    fn a_wrapped_claude_is_managed_by_the_pane_it_descends_from() {
        let mut busy = record("f67f", IndexState::Running { pid: 77028 });
        (busy.activity, busy.activity_since_ms) = (Some(Activity::Busy), 50);
        let panes = [pane(1, 77017, Some("f67f"))];
        for checks_processes in [true, false] {
            let sessions = wrapped(std::slice::from_ref(&busy), &panes, checks_processes);
            assert_eq!(states(&sessions), vec![("f67f", SessionState::Managed { pane: 1 })]);
            assert_eq!(sessions[0].status, Some(SessionStatus::Busy));
        }
    }

    #[test]
    fn a_wrapped_claude_that_changed_session_closes_the_launch_session() {
        let records = [record("8cb3", IndexState::Closed), record("ecfb", IndexState::Running { pid: 76638 })];
        let panes = [pane(2, 76623, Some("8cb3"))];
        assert_eq!(
            states(&wrapped(&records, &panes, true)),
            vec![("8cb3", SessionState::Closed), ("ecfb", SessionState::Managed { pane: 2 })]
        );
    }

    #[test]
    fn a_claude_started_elsewhere_stays_outside() {
        let records = [record("warp", IndexState::Running { pid: 31833 })];
        let panes = [pane(1, 77017, None)];
        assert_eq!(
            states(&wrapped(&records, &panes, true)),
            vec![("warp", SessionState::External { pid: Some(31833) })]
        );
        assert_eq!(states(&wrapped(&records, &panes, false)), vec![("warp", SessionState::Closed)]);
    }

    #[test]
    fn a_process_seen_exiting_is_closed_until_the_index_catches_up() {
        let mut running = record("s", IndexState::Running { pid: 100 });
        running.proc_start = Some("5".into());
        let records = [running];
        let gone = |start: &str| vec![Gone { pid: 100, start: Some(start.into()) }];
        assert_eq!(states(&plain(&records, &[], &gone("5"))), vec![("s", SessionState::Closed)]);
        // Same pid, other start time: a new process that reused the pid; not hidden.
        assert_eq!(states(&plain(&records, &[], &gone("6"))), vec![("s", SessionState::External { pid: Some(100) })]);
        assert!(still_claimed(&gone("5")[0], &records));
        assert!(!still_claimed(&gone("6")[0], &records));
    }

    #[test]
    fn a_closed_by_exit_session_goes_to_a_pane_started_with_it() {
        let mut old = record("s", IndexState::Running { pid: 100 });
        old.proc_start = Some("5".into());
        let gone = [Gone { pid: 100, start: Some("5".into()) }];
        // After a take over, the new pane runs `claude --resume s` before Claude rewrites its pid file.
        let sessions = plain(&[old], &[pane(9, 300, Some("s"))], &gone);
        assert_eq!(states(&sessions), vec![("s", SessionState::Managed { pane: 9 })]);
        assert_eq!(sessions[0].status, None);
    }

    #[test]
    fn diff_reports_new_changed_and_removed_sessions() {
        let infos = |records: &[SessionRecord]| plain(records, &[], &[]);
        let old = infos(&[
            record("same", IndexState::Closed),
            record("changes", IndexState::Closed),
            record("goes", IndexState::Closed),
        ]);
        let new = infos(&[
            record("same", IndexState::Closed),
            record("changes", IndexState::Running { pid: 5 }),
            record("arrives", IndexState::Closed),
        ]);
        let (upserted, removed) = diff(&old, &new);
        assert_eq!(upserted.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["changes", "arrives"]);
        assert_eq!(removed, vec!["goes".to_string()]);
        assert_eq!(diff(&new, &new), (vec![], vec![]));
    }

    #[test]
    fn a_container_process_carrying_a_panes_marker_is_managed_by_it() {
        let records = [
            record("boxed", IndexState::InContainer { pid: Some(9007) }),
            record("vscode", IndexState::InContainer { pid: Some(9008) }),
            record("launched", IndexState::Closed),
        ];
        let (registry, marked) = (Registry::default(), HashMap::from([(9007, 4)]));
        let view = ContainerView {
            registry: &registry,
            marked: &marked,
            owned: &HashMap::new(),
            dir_exists: &|_| true,
            checks_processes: true,
        };
        // Pane 4 was started as `launched`, then `/clear`ed into `boxed`: the launch session is closed.
        let sessions = resolve(&records, &[pane(4, 100, Some("launched"))], &[], &view);
        assert_eq!(
            states(&sessions),
            vec![
                ("boxed", SessionState::Managed { pane: 4 }),
                ("vscode", SessionState::External { pid: Some(9008) }),
                ("launched", SessionState::Closed),
            ]
        );
    }

    #[test]
    fn container_sessions_report_their_host_directory_and_place() {
        use claude_index::containers::{Container, Docker, Mount};
        struct OneContainer;
        impl Docker for OneContainer {
            fn list(&self) -> Result<Vec<Container>, String> {
                Ok(vec![Container {
                    id: "c1".into(),
                    name: "n".into(),
                    running: true,
                    created: "1".into(),
                    host_folder: "/home/u/app".into(),
                    mounts: vec![Mount { source: "/home/u/app".into(), destination: "/workspaces/app".into() }],
                    remote_user: None,
                    config_dir: None,
                }])
            }
            fn start(&self, _id: &str) -> Result<(), String> {
                Ok(())
            }
        }
        let mut registry = Registry::default();
        registry.refresh(&OneContainer).unwrap();
        let mut boxed = record("boxed", IndexState::Closed);
        boxed.cwd = "/workspaces/app/src".into();
        let marked = HashMap::new();
        let exists = |path: &std::path::Path| path == std::path::Path::new("/home/u/app/src");
        let info = resolve(
            &[boxed],
            &[],
            &[],
            &ContainerView {
                registry: &registry,
                marked: &marked,
                owned: &HashMap::new(),
                dir_exists: &exists,
                checks_processes: true,
            },
        )
        .remove(0);
        assert_eq!((info.cwd.as_path(), info.cwd_exists), (std::path::Path::new("/home/u/app/src"), true));
        assert_eq!(
            info.container,
            Some(ContainerPlace { host_folder: Some("/home/u/app".into()), path: "/workspaces/app/src".into() })
        );
    }

    #[test]
    fn merging_dirs_keeps_the_most_recently_active_duplicate() {
        let (mut a, mut b) = (record("dup", IndexState::Closed), record("dup", IndexState::Closed));
        (a.last_active_ms, a.claude_dir) = (5, Some("/a".into()));
        (b.last_active_ms, b.claude_dir) = (9, Some("/b".into()));
        let only = record("only", IndexState::Closed);
        let merged = merge_records([&Arc::new(vec![a, only]), &Arc::new(vec![b])]);
        assert_eq!(
            merged.iter().map(|r| (r.id.as_str(), r.last_active_ms)).collect::<Vec<_>>(),
            vec![("dup", 9), ("only", 1)]
        );
        assert_eq!(merged[0].claude_dir.as_deref(), Some(Path::new("/b")));
    }
}
