//! Two daemons side by side: the GUI's multi-host case, with one client attached to each.
// On Windows `sh` comes from Git for Windows (on PATH on CI runners).

use std::time::{Duration, Instant};

use daemon::client::{ClientEvent, Connection, DaemonClient};
use daemon::launch::{DaemonOptions, run_daemon};
use daemon::paths::Paths;
use protocol::message::SpawnRequest;

fn sh(script: &str) -> SpawnRequest {
    SpawnRequest {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: std::env::temp_dir(),
        cols: 80,
        rows: 24,
        cwd_from: None,
        container: None,
        claude: false,
        env: Vec::new(),
        scrollback: None,
    }
}

fn connect(paths: &Paths) -> Connection {
    let paths = paths.clone();
    let spawner = {
        let paths = paths.clone();
        move || {
            let claude_dir = Some(paths.runtime_dir.with_file_name("claude"));
            let options = DaemonOptions {
                paths,
                base_env: Some(std::env::vars().collect()),
                idle_timeout: Duration::from_secs(5),
                claude_dir,
                docker: None,
            };
            std::thread::spawn(move || run_daemon(options));
            Ok(())
        }
    };
    DaemonClient::connect(&paths, spawner).unwrap()
}

fn output_until(events: &async_channel::Receiver<ClientEvent>, needle: &str) -> String {
    let mut text = String::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !text.contains(needle) {
        assert!(Instant::now() < deadline, "timed out waiting for {needle:?}; got {text:?}");
        if let Ok(ClientEvent::Output { bytes, .. }) = events.recv_blocking() {
            text.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    text
}

#[test]
fn two_daemons_run_side_by_side_with_their_own_instances_and_panes() {
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (Paths::in_dir(dir_a.path(), "a"), Paths::in_dir(dir_b.path(), "b"));
    let (conn_a, conn_b) = (connect(&a), connect(&b));
    assert_ne!(conn_a.instance, conn_b.instance, "each daemon has its own instance id");
    conn_a.client.spawn(1, sh("printf from-a; sleep 5"));
    conn_b.client.spawn(1, sh("printf from-b; sleep 5"));
    let (text_a, text_b) = (output_until(&conn_a.events, "from-a"), output_until(&conn_b.events, "from-b"));
    assert!(!text_a.contains("from-b") && !text_b.contains("from-a"), "output stays with its own daemon");
}
