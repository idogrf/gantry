//! Real Claude sends a file to the preview through the daemon (run by hand: `-- --ignored`; it runs `claude`).
#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use daemon::paths::Paths;
use daemon::preview_plugin::write_plugin;
use daemon::server::{ServerOptions, serve};
use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::tokio::{Stream, prelude::*};
use protocol::frame::{Frame, read_frame, write_frame};
use protocol::message::*;
use tokio::time::timeout;

#[tokio::test]
#[ignore = "runs a real claude (costs a request)"]
async fn real_claude_sends_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path(), "test");
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    let gantryd = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/gantryd");
    let plugin_dir = dir.path().join("claude-plugin");
    write_plugin(&plugin_dir, &gantryd.canonicalize().expect("build gantryd first")).unwrap();
    let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join(".claude"));
    println!("temp dirs: {} and {}", dir.path().display(), work.path().display());

    let listener = ListenerOptions::new().name(paths.socket_name().unwrap()).create_tokio().unwrap();
    let options = ServerOptions {
        base_env: std::env::vars().collect(),
        owner_uid: None,
        idle_timeout: Duration::from_secs(300),
        claude_dir: Some(claude_dir),
        containers_file: None,
        search_dir: None,
        docker: None,
        paste_dir: Some(paths.paste_dir.clone()),
        plugin_dir: Some(plugin_dir),
    };
    tokio::spawn(serve(listener, options));

    let stream = Stream::connect(paths.socket_name().unwrap()).await.unwrap();
    let (mut recv, mut send) = stream.split();
    write_frame(&mut send, &Frame::control(&ClientMessage::Hello { protocol: PROTOCOL_VERSION })).await.unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let spec = SpawnRequest {
        program: "claude".into(),
        args: vec![
            "--session-id".into(),
            session.clone(),
            "--permission-mode".into(),
            "acceptEdits".into(),
            "-p".into(),
            "Write a two-line note to note.md".into(),
        ],
        cwd: work.path().to_path_buf(),
        cols: 100,
        rows: 30,
        cwd_from: None,
        container: None,
        claude: true,
        env: Vec::new(),
        scrollback: None,
        preview: true,
    };
    write_frame(&mut send, &Frame::control(&ClientMessage::Spawn { request: 1, spec })).await.unwrap();

    let want = work.path().join("note.md");
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut sent: Vec<SentFile> = Vec::new();
    while Instant::now() < deadline {
        let left = deadline - Instant::now();
        let Ok(frame) = timeout(left, read_frame(&mut recv)).await else { break };
        let Some(Frame::Control(bytes)) = frame.unwrap() else { continue };
        let message: DaemonMessage = serde_json::from_slice(&bytes).unwrap();
        let sessions = match message {
            DaemonMessage::SpawnFailed { error, .. } => panic!("spawn failed: {error}"),
            DaemonMessage::Sessions { sessions } => sessions,
            DaemonMessage::SessionsChanged { upserted, .. } => upserted,
            _ => continue,
        };
        for s in sessions.iter().filter(|s| s.id == session) {
            println!("session {} state {:?} sent {:?}", s.id, s.state, s.sent_files);
            sent.clone_from(&s.sent_files);
        }
        if sent.iter().any(|f| f.path == want) {
            return;
        }
    }
    panic!("no sent file {} within 120 s; last seen {sent:?} (session {session})", want.display());
}
