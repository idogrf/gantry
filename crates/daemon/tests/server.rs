//! End-to-end tests of the daemon server over a real local socket. On Windows `sh` and `yes` come from Git for Windows
//! (on PATH on CI runners).

use std::time::Duration;

use daemon::paths::Paths;
use daemon::server::{ServerOptions, serve};
use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::tokio::{RecvHalf, SendHalf, Stream, prelude::*};
use protocol::frame::{Frame, read_frame, write_frame};
use protocol::message::*;
use tokio::time::timeout;

struct Harness {
    paths: Paths,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    _dir: tempfile::TempDir,
}

async fn start_server(idle_timeout: Duration) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path(), "test");
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    let listener = ListenerOptions::new().name(paths.socket_name().unwrap()).create_tokio().unwrap();
    let options = ServerOptions {
        base_env: std::env::vars().collect(),
        owner_uid: None,
        idle_timeout,
        claude_dir: None,
        containers_file: None,
        search_dir: None,
        docker: None,
        paste_dir: Some(paths.paste_dir.clone()),
    };
    let server = tokio::spawn(serve(listener, options));
    Harness { paths, server, _dir: dir }
}

struct TestClient {
    recv: Option<RecvHalf>,
    send: Option<SendHalf>,
    instance: String,
}

impl TestClient {
    async fn connect(paths: &Paths) -> TestClient {
        let stream = Stream::connect(paths.socket_name().unwrap()).await.unwrap();
        let (recv, send) = stream.split();
        let mut client = TestClient { recv: Some(recv), send: Some(send), instance: String::new() };
        client.send(&ClientMessage::Hello { protocol: PROTOCOL_VERSION }).await;
        match client.control().await {
            DaemonMessage::Welcome { protocol: PROTOCOL_VERSION, instance, .. } => client.instance = instance,
            other => panic!("unexpected handshake reply {other:?}"),
        }
        client
    }

    async fn send(&mut self, message: &ClientMessage) {
        write_frame(self.send.as_mut().unwrap(), &Frame::control(message)).await.unwrap();
    }

    async fn input(&mut self, pane: PaneId, bytes: &[u8]) {
        write_frame(self.send.as_mut().unwrap(), &Frame::Data { pane, bytes: bytes.to_vec() }).await.unwrap();
    }

    async fn frame(&mut self) -> Option<Frame> {
        timeout(Duration::from_secs(5), read_frame(self.recv.as_mut().unwrap()))
            .await
            .expect("timed out waiting for a frame")
            .unwrap()
    }

    /// Next control message, skipping data frames.
    async fn control(&mut self) -> DaemonMessage {
        loop {
            match self.frame().await.expect("connection closed") {
                frame @ Frame::Control(_) => return frame.parse().unwrap().unwrap(),
                Frame::Data { .. } => {}
            }
        }
    }

    /// Collect data for `pane` until it contains `needle`; returns the collected text.
    async fn output_until(&mut self, pane: PaneId, needle: &str) -> String {
        let mut text = String::new();
        while !text.contains(needle) {
            match self.frame().await.expect("connection closed") {
                Frame::Data { pane: p, bytes } if p == pane => text.push_str(&String::from_utf8_lossy(&bytes)),
                _ => {}
            }
        }
        text
    }

    async fn spawn_request(&mut self, spec: SpawnRequest) -> (PaneId, std::path::PathBuf) {
        self.send(&ClientMessage::Spawn { request: 1, spec }).await;
        loop {
            match self.control().await {
                DaemonMessage::Spawned { request: 1, pane, cwd } => return (pane, cwd),
                DaemonMessage::SpawnFailed { error, .. } => panic!("spawn failed: {error}"),
                _ => {}
            }
        }
    }

    async fn spawn(&mut self, script: &str) -> PaneId {
        self.spawn_request(sh(script, None)).await.0
    }
}

/// Close without waiting for the server to read what is unread. On Windows a dropped pipe with unread data waits in
/// interprocess's process-wide linger queue until its peer reads it; when a test ends, its server is dropped too, both
/// ends wait for each other, and every later close in this test process waits behind them.
impl Drop for TestClient {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let (Some(recv), Some(send)) = (self.recv.take(), self.send.take())
            && let Ok(Stream::NamedPipe(stream)) = Stream::reunite(recv, send)
        {
            stream.inner().assume_flushed();
        }
    }
}

fn sh(script: &str, cwd_from: Option<PaneId>) -> SpawnRequest {
    SpawnRequest {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: std::env::temp_dir(),
        cols: 80,
        rows: 24,
        cwd_from,
        container: None,
        claude: false,
        env: Vec::new(),
        scrollback: None,
    }
}

#[tokio::test]
async fn spawned_pane_streams_output_and_takes_input() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut c = TestClient::connect(&h.paths).await;
    let pane = c.spawn("printf ready; read line; printf \"got:$line\"; sleep 5").await;
    c.output_until(pane, "ready").await;
    c.input(pane, b"abc\r").await;
    c.output_until(pane, "got:abc").await;
}

#[tokio::test]
async fn panes_are_listed() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut c = TestClient::connect(&h.paths).await;
    let pane = c.spawn("sleep 5").await;
    c.send(&ClientMessage::ListPanes).await;
    loop {
        if let DaemonMessage::Panes { panes } = c.control().await {
            assert_eq!(panes.len(), 1);
            assert_eq!((panes[0].pane, panes[0].program.as_str()), (pane, "sh"));
            assert_eq!(panes[0].args, vec!["-c".to_string(), "sleep 5".to_string()]);
            break;
        }
    }
}

#[tokio::test]
async fn exit_is_reported_after_final_output() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut c = TestClient::connect(&h.paths).await;
    let pane = c.spawn("printf 'last words'; exit 7").await;
    let mut text = String::new();
    loop {
        match c.frame().await.expect("closed") {
            Frame::Data { pane: p, bytes } if p == pane => text.push_str(&String::from_utf8_lossy(&bytes)),
            frame @ Frame::Control(_) => {
                if let DaemonMessage::Exited { pane: p, code } = frame.parse().unwrap().unwrap() {
                    assert_eq!((p, code), (pane, 7));
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(text.contains("last words"), "final output lost: {text:?}");
}

#[tokio::test]
async fn terminal_queries_are_answered_by_the_daemon() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut c = TestClient::connect(&h.paths).await;
    let pane = c
        .spawn("stty raw -echo; printf '\\033[6n'; reply=$(dd bs=1 count=6 2>/dev/null); stty sane; printf \"reply:%s\" \"$reply\" | od -c | head -2; sleep 5")
        .await;
    let text = c.output_until(pane, "R").await;
    assert!(text.contains("033   [   1   ;   1   R"), "no cursor report reached the program: {text:?}");
}

#[tokio::test]
async fn resize_reaches_the_program() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut c = TestClient::connect(&h.paths).await;
    let pane = c.spawn("sleep 0.5; stty size; sleep 5").await;
    c.send(&ClientMessage::Resize { pane, cols: 100, rows: 30, cell_width: 8, cell_height: 18 }).await;
    c.output_until(pane, "30 100").await;
}

#[tokio::test]
async fn reattach_replays_the_screen() {
    let h = start_server(Duration::from_secs(10)).await;
    let pane = {
        let mut first = TestClient::connect(&h.paths).await;
        let pane = first.spawn("printf 'hello from before'; sleep 5").await;
        first.output_until(pane, "hello from before").await;
        pane
    };
    let mut second = TestClient::connect(&h.paths).await;
    second.send(&ClientMessage::Attach { pane }).await;
    assert!(matches!(second.control().await, DaemonMessage::Resync { pane: p, cols: 80, rows: 24 } if p == pane));
    second.output_until(pane, "hello from before").await;
}

#[tokio::test]
async fn a_second_client_takes_over() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut first = TestClient::connect(&h.paths).await;
    let _second = TestClient::connect(&h.paths).await;
    assert_eq!(first.control().await, DaemonMessage::TakenOver);
    assert_eq!(first.frame().await, None, "first connection must be closed after takeover");
}

#[tokio::test]
// Through ConPTY `yes` cannot fill the pipe and the outbox within 2 s while the other tests run; it passes alone.
#[cfg_attr(windows, ignore = "too slow to flood beside the other tests on Windows; run alone with --ignored")]
async fn lagging_client_gets_a_resync_instead_of_unbounded_buffering() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut c = TestClient::connect(&h.paths).await;
    let pane = c.spawn("yes 'flood flood flood flood flood'").await;
    tokio::time::sleep(Duration::from_secs(2)).await; // not reading: socket + outbox fill up
    let mut saw_resync = false;
    for _ in 0..20_000 {
        if let Some(frame @ Frame::Control(_)) = c.frame().await
            && let DaemonMessage::Resync { pane: p, .. } = frame.parse().unwrap().unwrap()
        {
            assert_eq!(p, pane);
            saw_resync = true;
            break;
        }
    }
    assert!(saw_resync, "a client that fell behind must be resynced");
    c.send(&ClientMessage::Kill { pane }).await;
}

#[tokio::test]
async fn stop_all_ends_panes_and_the_server() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut c = TestClient::connect(&h.paths).await;
    c.spawn("sleep 30").await;
    c.send(&ClientMessage::StopAll).await;
    timeout(Duration::from_secs(5), h.server).await.expect("server did not stop").unwrap().unwrap();
}

#[tokio::test]
async fn server_stops_when_last_pane_exits_without_a_client() {
    let h = start_server(Duration::from_secs(10)).await;
    {
        let mut c = TestClient::connect(&h.paths).await;
        c.spawn("sleep 1").await;
    }
    timeout(Duration::from_secs(5), h.server).await.expect("server kept running").unwrap().unwrap();
}

#[tokio::test]
async fn server_stops_if_nobody_ever_connects() {
    let h = start_server(Duration::from_millis(200)).await;
    timeout(Duration::from_secs(3), h.server).await.expect("idle server kept running").unwrap().unwrap();
}

#[tokio::test]
async fn version_mismatch_still_accepts_stop_all() {
    let h = start_server(Duration::from_secs(10)).await;
    let stream = Stream::connect(h.paths.socket_name().unwrap()).await.unwrap();
    let (mut recv, mut send) = stream.split();
    write_frame(&mut send, &Frame::control(&ClientMessage::Hello { protocol: 999 })).await.unwrap();
    let reply: DaemonMessage = read_frame(&mut recv).await.unwrap().unwrap().parse().unwrap().unwrap();
    assert_eq!(reply, DaemonMessage::VersionMismatch { daemon_protocol: PROTOCOL_VERSION });
    write_frame(&mut send, &Frame::control(&ClientMessage::StopAll)).await.unwrap();
    timeout(Duration::from_secs(5), h.server).await.expect("server did not stop").unwrap().unwrap();
}

#[tokio::test]
async fn final_output_survives_exit_while_the_client_lags() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut c = TestClient::connect(&h.paths).await;
    let pane = c.spawn("head -c 40000000 /dev/zero | tr '\\0' x; printf '\\r\\nFINAL-MARKER'").await;
    tokio::time::sleep(Duration::from_secs(4)).await; // lag while the flood runs and the program exits
    let mut since_resync = String::new();
    loop {
        match c.frame().await.expect("connection closed") {
            Frame::Data { pane: p, bytes } if p == pane => since_resync.push_str(&String::from_utf8_lossy(&bytes)),
            frame @ Frame::Control(_) => match frame.parse().unwrap().unwrap() {
                DaemonMessage::Resync { pane: p, .. } if p == pane => since_resync.clear(),
                DaemonMessage::Exited { pane: p, .. } if p == pane => break,
                _ => {}
            },
            _ => {}
        }
    }
    assert!(since_resync.contains("FINAL-MARKER"), "the program's last output never reached the client");
}

#[tokio::test]
async fn each_daemon_has_its_own_instance_id() {
    let (h1, h2) = (start_server(Duration::from_secs(10)).await, start_server(Duration::from_secs(10)).await);
    let (a, b) = (TestClient::connect(&h1.paths).await, TestClient::connect(&h2.paths).await);
    assert!(!a.instance.is_empty());
    assert_ne!(a.instance, b.instance);
    assert_eq!(TestClient::connect(&h1.paths).await.instance, a.instance);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn spawn_can_start_in_another_panes_current_directory() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut c = TestClient::connect(&h.paths).await;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().canonicalize().unwrap();
    let first = c.spawn(&format!("cd '{}'; printf moved; sleep 5", target.display())).await;
    c.output_until(first, "moved").await;
    let (second, cwd) = c.spawn_request(sh("pwd; sleep 5", Some(first))).await;
    assert_eq!(cwd, target);
    c.output_until(second, &target.display().to_string()).await;
}

#[tokio::test]
async fn spawn_from_an_unknown_pane_falls_back_to_cwd() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut c = TestClient::connect(&h.paths).await;
    let (_, cwd) = c.spawn_request(sh("sleep 5", Some(999))).await;
    assert_eq!(cwd, std::env::temp_dir());
}

#[tokio::test]
async fn without_docker_container_launches_fail_and_lookups_find_nothing() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut c = TestClient::connect(&h.paths).await;
    let spec = SpawnRequest { container: Some("/home/u/app".into()), ..sh("true", None) };
    c.send(&ClientMessage::Spawn { request: 7, spec }).await;
    loop {
        match c.control().await {
            DaemonMessage::SpawnFailed { request: 7, error } => {
                assert!(error.contains("Docker"), "{error}");
                break;
            }
            DaemonMessage::Spawned { .. } => panic!("spawned without Docker"),
            _ => {}
        }
    }
    c.send(&ClientMessage::FindContainer { request: 8, dir: "/home/u/app".into() }).await;
    loop {
        if let DaemonMessage::ContainerFound { request, container } = c.control().await {
            assert_eq!((request, container), (8, None));
            break;
        }
    }
}

async fn start_search_server(claude: &std::path::Path, data: &std::path::Path) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path(), "test");
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    let listener = ListenerOptions::new().name(paths.socket_name().unwrap()).create_tokio().unwrap();
    let options = ServerOptions {
        base_env: std::env::vars().collect(),
        owner_uid: None,
        idle_timeout: Duration::from_secs(30),
        claude_dir: Some(claude.to_path_buf()),
        containers_file: None,
        docker: None,
        search_dir: Some(data.join("search")),
        paste_dir: None,
    };
    let server = tokio::spawn(serve(listener, options));
    Harness { paths, server, _dir: dir }
}

#[tokio::test]
async fn conversations_are_searchable() {
    let root = tempfile::tempdir().unwrap();
    let claude = root.path().join("claude");
    let folder = claude.join("projects").join("-r");
    std::fs::create_dir_all(&folder).unwrap();
    let lines = [
        serde_json::json!({"type": "user", "cwd": "/r", "message": {"content": "where is the quokka config"}}),
        serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "in quokka.toml"}]}}),
    ];
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(folder.join("s-quokka.jsonl"), text).unwrap();
    let harness = start_search_server(&claude, root.path()).await;
    let mut client = TestClient::connect(&harness.paths).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    for request in 1.. {
        client.send(&ClientMessage::SearchConversations { request, query: "quokka".into(), limit: 10 }).await;
        let hits = loop {
            match client.control().await {
                DaemonMessage::SearchResults { request: r, hits } if r == request => break hits,
                _ => {}
            }
        };
        if let Some(hit) = hits.first() {
            assert_eq!(hit.session_id, "s-quokka");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no hit in time");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn without_a_search_dir_search_is_unavailable() {
    let harness = start_server(Duration::from_secs(30)).await;
    let mut client = TestClient::connect(&harness.paths).await;
    client.send(&ClientMessage::SearchConversations { request: 1, query: "x".into(), limit: 10 }).await;
    loop {
        if let DaemonMessage::SearchUnavailable { .. } = client.control().await {
            break;
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_tilde_cwd_starts_in_the_home_folder() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut c = TestClient::connect(&h.paths).await;
    let home = std::env::home_dir().unwrap();
    let spec = SpawnRequest { cwd: "~".into(), ..sh("pwd; sleep 5", None) };
    let (pane, cwd) = c.spawn_request(spec).await;
    assert_eq!(cwd, home);
    c.output_until(pane, &home.to_string_lossy()).await;
}

/// The `paste_image` flag of the `Welcome` a fresh connection gets.
async fn welcome_paste_image(paths: &Paths) -> bool {
    let stream = Stream::connect(paths.socket_name().unwrap()).await.unwrap();
    let (mut recv, mut send) = stream.split();
    write_frame(&mut send, &Frame::control(&ClientMessage::Hello { protocol: PROTOCOL_VERSION })).await.unwrap();
    match read_frame(&mut recv).await.unwrap().unwrap().parse::<DaemonMessage>().unwrap().unwrap() {
        DaemonMessage::Welcome { paste_image, .. } => paste_image,
        other => panic!("unexpected handshake reply {other:?}"),
    }
}

#[tokio::test]
async fn welcome_advertises_image_paste() {
    let h = start_server(Duration::from_secs(10)).await;
    assert!(welcome_paste_image(&h.paths).await);
}

#[tokio::test]
async fn pasted_images_are_saved_beside_the_socket() {
    let h = start_server(Duration::from_secs(10)).await;
    let mut c = TestClient::connect(&h.paths).await;
    let pane = c.spawn("sleep 5").await;
    c.send(&ClientMessage::SavePasteImage { pane, format: ImageFormat::Png, data: vec![1, 2, 3] }).await;
    loop {
        match c.control().await {
            DaemonMessage::PasteImageSaved { pane: p, path } => {
                assert_eq!(p, pane);
                let path = std::path::PathBuf::from(path);
                assert!(path.starts_with(&h.paths.paste_dir), "{path:?}");
                assert_eq!(std::fs::read(&path).unwrap(), [1, 2, 3]);
                break;
            }
            DaemonMessage::PasteImageFailed { reason, .. } => panic!("not saved: {reason}"),
            _ => {}
        }
    }
    c.send(&ClientMessage::SavePasteImage { pane: pane + 100, format: ImageFormat::Png, data: vec![1] }).await;
    loop {
        match c.control().await {
            DaemonMessage::PasteImageFailed { pane: p, reason } => {
                assert_eq!((p, reason.as_str()), (pane + 100, "no such pane"));
                break;
            }
            DaemonMessage::PasteImageSaved { .. } => panic!("saved for an unknown pane"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn image_paste_is_off_without_a_paste_dir() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path(), "test");
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    let listener = ListenerOptions::new().name(paths.socket_name().unwrap()).create_tokio().unwrap();
    let options = ServerOptions {
        base_env: std::env::vars().collect(),
        owner_uid: None,
        idle_timeout: Duration::from_secs(10),
        claude_dir: None,
        containers_file: None,
        search_dir: None,
        docker: None,
        paste_dir: None,
    };
    let _server = tokio::spawn(serve(listener, options));
    assert!(!welcome_paste_image(&paths).await);
}
