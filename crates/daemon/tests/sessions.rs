//! Session index → daemon → client, with a temp Claude config directory.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use daemon::client::{ClientEvent, Connection, DaemonClient};
use daemon::launch::{DaemonOptions, run_daemon};
use daemon::paths::Paths;
use protocol::message::{PaneId, SessionInfo, SessionState, SessionStatus, SpawnRequest, TakeOverOutcome};
use std::process::{Child, Command};

struct Setup {
    _dir: tempfile::TempDir,
    config: PathBuf,
    work: PathBuf,
    conn: Connection,
}

/// A daemon indexing a temp config dir; `prepare(config, work)` runs before it starts.
fn start(prepare: impl FnOnce(&Path, &Path)) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let (config, work) = (dir.path().join("claude"), dir.path().join("work"));
    std::fs::create_dir_all(&work).unwrap();
    prepare(&config, &work);
    let paths = Paths::in_dir(dir.path(), "t");
    let (spawn_paths, claude_dir) = (paths.clone(), config.clone());
    let conn = DaemonClient::connect(&paths, move || {
        let options = DaemonOptions {
            paths: spawn_paths,
            base_env: Some(std::env::vars().collect()),
            idle_timeout: Duration::from_secs(5),
            claude_dir: Some(claude_dir),
            docker: None,
        };
        std::thread::spawn(move || run_daemon(options));
        Ok(())
    })
    .unwrap();
    Setup { _dir: dir, config, work, conn }
}

fn transcript(config: &Path, id: &str, cwd: &Path, prompt: &str) {
    let folder = config.join("projects").join("-work");
    std::fs::create_dir_all(&folder).unwrap();
    let line = serde_json::json!({"type": "user", "cwd": cwd, "message": {"content": prompt}});
    std::fs::write(folder.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();
}

/// Wait until a session event carries session `id` satisfying `check`.
fn wait_for(conn: &Connection, id: &str, check: impl Fn(&SessionInfo) -> bool) -> SessionInfo {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for session {id}");
        let sessions = match conn.events.try_recv() {
            Ok(ClientEvent::Sessions { sessions }) => sessions,
            Ok(ClientEvent::SessionsChanged { upserted, .. }) => upserted,
            Ok(_) => continue,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        if let Some(found) = sessions.into_iter().find(|s| s.id == id && check(s)) {
            return found;
        }
    }
}

/// Spawn `program args` in `cwd`; returns the pane and the output seen until `needle`.
fn spawn(conn: &Connection, program: &str, args: &[&str], cwd: &Path, needle: &str) -> (PaneId, String) {
    let spec = SpawnRequest {
        preview: false,
        program: program.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: cwd.into(),
        cols: 80,
        rows: 24,
        cwd_from: None,
        container: None,
        claude: false,
        env: Vec::new(),
        scrollback: None,
    };
    spawn_spec(conn, spec, needle)
}

/// Spawn `spec`; returns the pane and the output seen until `needle`.
fn spawn_spec(conn: &Connection, spec: SpawnRequest, needle: &str) -> (PaneId, String) {
    conn.client.spawn(1, spec);
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut pane, mut text) = (None, String::new());
    while pane.is_none() || !text.contains(needle) {
        assert!(Instant::now() < deadline, "timed out; output {text:?}");
        match conn.events.try_recv() {
            Ok(ClientEvent::Spawned { pane: p, .. }) => pane = Some(p),
            Ok(ClientEvent::SpawnFailed { error, .. }) => panic!("spawn failed: {error}"),
            Ok(ClientEvent::Output { bytes, .. }) => text.push_str(&String::from_utf8_lossy(&bytes)),
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    (pane.unwrap(), text)
}

#[test]
fn closed_sessions_are_listed_on_connect() {
    let s = start(|config, work| transcript(config, "s1", work, "hello there"));
    let found = wait_for(&s.conn, "s1", |s| s.state == SessionState::Closed);
    assert_eq!((found.title.as_str(), found.cwd.as_path(), found.cwd_exists), ("hello there", s.work.as_path(), true));
}

#[test]
fn new_transcripts_arrive_as_changes() {
    let s = start(|config, work| transcript(config, "s1", work, "first"));
    wait_for(&s.conn, "s1", |_| true);
    transcript(&s.config, "s2", &s.work, "second");
    wait_for(&s.conn, "s2", |s| s.title == "second");
}

/// Transcript lines for one `show_in_preview` call and its result.
fn sent_lines(id: &str, path: &str) -> String {
    let call = serde_json::json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": id, "name": claude_index::sent_files::TOOL_NAME,
         "input": {"files": [{"path": path}]}}]}});
    let result = serde_json::json!({"type": "user", "timestamp": "2026-11-25T10:00:00.250Z", "message": {"content": [
        {"type": "tool_result", "tool_use_id": id,
         "content": [{"type": "text", "text": format!("Shown in preview:\n- {path}")}]}]}});
    format!("{call}\n{result}\n")
}

#[test]
fn requested_session_reports_its_sent_files() {
    use std::io::Write;
    let s = start(|config, work| transcript(config, "s1", work, "hello"));
    let file = s.config.join("projects/-work/s1.jsonl");
    let append = |data: String| {
        let mut file = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
        file.write_all(data.as_bytes()).unwrap();
    };
    append(sent_lines("toolu_1", "/w/a.md"));
    wait_for(&s.conn, "s1", |_| true);
    s.conn.client.watch_sent_files(&["s1".to_string()]);
    let found = wait_for(&s.conn, "s1", |info| info.sent_files.len() == 1);
    assert_eq!(found.sent_files[0].path, Path::new("/w/a.md"));
    append(sent_lines("toolu_2", "/w/b.md"));
    wait_for(&s.conn, "s1", |info| info.sent_files.len() == 2);
}

#[test]
fn a_pane_whose_process_wrote_a_pid_file_is_managed() {
    let s = start(|config, work| transcript(config, "s1", work, "first"));
    let (pane, output) = spawn(&s.conn, "sh", &["-c", "echo PID:$$; sleep 30"], &s.work, "PID:");
    let digits: String = output.split("PID:").nth(1).unwrap().chars().take_while(char::is_ascii_digit).collect();
    let pid: u32 = digits.parse().unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start = claude_index::host::proc_start_time(&stat).unwrap();
    std::fs::create_dir_all(s.config.join("sessions")).unwrap();
    let pid_file = serde_json::json!({"pid": pid, "sessionId": "s3", "cwd": s.work, "procStart": start, "updatedAt": 1,
        "status": "waiting", "waitingFor": "input needed", "statusUpdatedAt": 7});
    std::fs::write(s.config.join("sessions").join(format!("{pid}.json")), pid_file.to_string()).unwrap();
    let found = wait_for(&s.conn, "s3", |s| s.state == SessionState::Managed { pane });
    assert_eq!(
        (found.status, found.status_since_ms),
        (Some(SessionStatus::NeedsInput { reason: Some("input needed".into()) }), 7)
    );
}

#[test]
fn a_claude_pane_is_managed_by_its_session_id_until_it_exits() {
    let s = start(|config, work| {
        transcript(config, "s4", work, "resumable");
        let script = work.join("claude");
        std::fs::write(&script, "#!/bin/sh\necho started\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    });
    wait_for(&s.conn, "s4", |s| s.state == SessionState::Closed);
    let program = s.work.join("claude").display().to_string();
    // Return at `Spawned`: the Managed change follows it directly and must not be consumed here.
    let (pane, _) = spawn(&s.conn, &program, &["--session-id", "s4"], &s.work, "");
    wait_for(&s.conn, "s4", |s| s.state == SessionState::Managed { pane });
    s.conn.client.kill(pane);
    wait_for(&s.conn, "s4", |s| s.state == SessionState::Closed);
}

#[test]
fn a_session_is_closed_as_soon_as_its_pane_exits() {
    let s = start(|config, work| transcript(config, "s6", work, "first"));
    wait_for(&s.conn, "s6", |s| s.state == SessionState::Closed);
    let (pane, output) = spawn(&s.conn, "sh", &["-c", "echo PID:$$; sleep 30"], &s.work, "PID:");
    let digits: String = output.split("PID:").nth(1).unwrap().chars().take_while(char::is_ascii_digit).collect();
    let pid: u32 = digits.parse().unwrap();
    let start = claude_index::host::process_start(pid).unwrap();
    std::fs::create_dir_all(s.config.join("sessions")).unwrap();
    let pid_file =
        serde_json::json!({"pid": pid, "sessionId": "s6", "cwd": s.work, "procStart": start, "status": "idle"});
    std::fs::write(s.config.join("sessions").join(format!("{pid}.json")), pid_file.to_string()).unwrap();
    wait_for(&s.conn, "s6", |s| s.state == SessionState::Managed { pane });
    let killed = Instant::now();
    s.conn.client.kill(pane);
    // The pid file stays (sh never removes it) and the periodic rescan is 5 s away: without the daemon remembering
    // the exit, s6 would show External (pid alive in the stale records) first.
    loop {
        assert!(killed.elapsed() < Duration::from_secs(2), "s6 did not turn Closed promptly");
        let upserted = match s.conn.events.try_recv() {
            Ok(ClientEvent::SessionsChanged { upserted, .. }) => upserted,
            Ok(_) => continue,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        let Some(s6) = upserted.into_iter().find(|s| s.id == "s6") else { continue };
        assert_ne!(s6.state, SessionState::External { pid: Some(pid) }, "flashed External after its pane exited");
        if s6.state == SessionState::Closed {
            assert_eq!(s6.status, None);
            break;
        }
    }
}

/// Start `program args` as a process outside Gantry and register it as running session `id`.
fn outside(s: &Setup, id: &str, program: &str, args: &[&str]) -> Child {
    let child = Command::new(program).args(args).spawn().unwrap();
    let pid = child.id();
    let start = claude_index::host::process_start(pid).unwrap();
    std::fs::create_dir_all(s.config.join("sessions")).unwrap();
    let pid_file =
        serde_json::json!({"pid": pid, "sessionId": id, "cwd": s.work, "procStart": start, "status": "idle"});
    std::fs::write(s.config.join("sessions").join(format!("{pid}.json")), pid_file.to_string()).unwrap();
    child
}

/// Wait for the take-over result for `id`; returns it and the session states seen on the way.
fn take_over_result(conn: &Connection, id: &str) -> (TakeOverOutcome, Vec<SessionState>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut seen = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "no take-over result for {id}");
        match conn.events.try_recv() {
            Ok(ClientEvent::TakeOverResult { session, outcome }) if session == id => return (outcome, seen),
            Ok(ClientEvent::SessionsChanged { upserted, .. }) => {
                seen.extend(upserted.into_iter().filter(|s| s.id == id).map(|s| s.state))
            }
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[test]
fn take_over_stops_the_outside_process_and_closes_its_session() {
    use std::os::unix::process::ExitStatusExt;
    let s = start(|config, work| transcript(config, "s7", work, "outside"));
    let mut child = outside(&s, "s7", "sleep", &["30"]);
    let pid = child.id();
    wait_for(&s.conn, "s7", |s| s.state == SessionState::External { pid: Some(pid) });
    s.conn.client.take_over("s7", false);
    let (outcome, seen) = take_over_result(&s.conn, "s7");
    assert_eq!(outcome, TakeOverOutcome::Exited);
    // Closed is published before the result, so the GUI's resume sees a closed session.
    assert_eq!(seen.last(), Some(&SessionState::Closed), "{seen:?}");
    assert_eq!(child.wait().unwrap().signal(), Some(15), "stopped with SIGTERM");
}

#[test]
fn a_process_ignoring_sigterm_is_still_running_until_forced() {
    use std::os::unix::process::ExitStatusExt;
    let s = start(|config, work| transcript(config, "s8", work, "stubborn"));
    let mut child = outside(&s, "s8", "sh", &["-c", "trap '' TERM; while :; do sleep 1; done"]);
    let pid = child.id();
    wait_for(&s.conn, "s8", |s| s.state == SessionState::External { pid: Some(pid) });
    s.conn.client.take_over("s8", false);
    assert_eq!(take_over_result(&s.conn, "s8").0, TakeOverOutcome::StillRunning);
    s.conn.client.take_over("s8", true);
    assert_eq!(take_over_result(&s.conn, "s8").0, TakeOverOutcome::Exited);
    assert_eq!(child.wait().unwrap().signal(), Some(9), "killed with SIGKILL");
}

#[test]
fn take_over_of_a_closed_session_is_refused() {
    let s = start(|config, work| transcript(config, "s9", work, "mine"));
    wait_for(&s.conn, "s9", |s| s.state == SessionState::Closed);
    s.conn.client.take_over("s9", false);
    assert!(matches!(take_over_result(&s.conn, "s9").0, TakeOverOutcome::Failed { .. }));
}

/// Start a daemon indexing `config` (the default dir), with a second dir and fixtures prepared.
fn two_dirs() -> (Setup, PathBuf) {
    let mut second = PathBuf::new();
    let s = start(|config, work| {
        transcript(config, "d1", work, "from the first");
        second = config.with_file_name("claude2");
        transcript(&second, "d2", work, "from the second");
    });
    (s, second)
}

#[test]
fn set_claude_dirs_indexes_and_tags_two_dirs() {
    let (s, second) = two_dirs();
    let first = wait_for(&s.conn, "d1", |_| true);
    assert_eq!(first.claude_dir.as_deref(), Some(s.config.as_path()));
    s.conn.client.set_claude_dirs(vec![s.config.clone(), second.clone()]);
    let found = wait_for(&s.conn, "d2", |_| true);
    assert_eq!(found.claude_dir.as_deref(), Some(second.as_path()));
}

#[test]
fn removing_a_dir_removes_its_sessions() {
    let (s, second) = two_dirs();
    s.conn.client.set_claude_dirs(vec![s.config.clone(), second.clone()]);
    wait_for(&s.conn, "d2", |_| true);
    s.conn.client.set_claude_dirs(vec![s.config.clone()]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for the removal");
        match s.conn.events.try_recv() {
            Ok(ClientEvent::SessionsChanged { removed, .. }) if removed.contains(&"d2".to_string()) => break,
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[test]
fn spawn_env_reaches_child() {
    let s = start(|_, _| {});
    let spec = SpawnRequest {
        preview: false,
        program: "sh".into(),
        args: vec!["-c".into(), "echo GOT:$FOO; sleep 5".into()],
        cwd: s.work.clone(),
        cols: 80,
        rows: 24,
        cwd_from: None,
        container: None,
        claude: false,
        env: vec![("FOO".into(), "bar".into())],
        scrollback: None,
    };
    let (_, output) = spawn_spec(&s.conn, spec, "GOT:bar");
    assert!(output.contains("GOT:bar"), "{output:?}");
}
