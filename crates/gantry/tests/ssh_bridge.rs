//! The SSH transport end to end with a fake `ssh` that runs `gantry bridge` locally.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use app_core::host_state::FailKind;
use daemon::askpass::{AskAnswer, AskPrompt};
use daemon::client::ClientEvent;
use daemon::remote_install::{self, Timeouts};
use daemon::ssh::{
    Attempt, AttemptHandle, HANDSHAKE_TIMEOUT, Share, SshHostSpec, connect as connect_with, ssh_args_for,
};
use protocol::message::{ImageFormat, SpawnRequest};

const GANTRY: &str = env!("CARGO_BIN_EXE_gantry");

/// `connect` with the real handshake timeout and nobody cancelling.
fn connect(program: &str, spec: &SshHostSpec, attempt: Attempt) -> Result<daemon::client::Connection, FailKind> {
    connect_with(program, spec, attempt, &AttemptHandle::default(), HANDSHAKE_TIMEOUT)
}

/// What the fake ssh does, besides running the bridge.
#[derive(Default)]
struct Fake {
    /// Ask this through the askpass helper first (when ssh was given one); a cancel is what real ssh prints, exit 255.
    ask: Option<&'static str>,
    /// Fail with this exit code and message instead of running the bridge.
    fail: Option<(i32, &'static str)>,
    /// Run this shell text instead of the bridge (after the asking above); `$GANTRY` is the gantry binary.
    body: Option<String>,
    /// Run the remote command text (ssh's last argument) with `sh -c` instead of the bridge, like a remote shell.
    run_remote: bool,
}

struct Env {
    dir: tempfile::TempDir,
    fake: std::path::PathBuf,
}

impl Env {
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(name)
    }
}

/// A fake ssh script. The workspace forbids `unsafe`, so the tests cannot set environment variables: the script
/// exports the isolated environment itself (runtime dir, HOME, profile) before it runs `gantry bridge`. It ignores
/// ssh's arguments, records whether it was given `SSH_ASKPASS`, and calls `gantry` the way real ssh calls
/// `SSH_ASKPASS` (the prompt as the only argument; the transport sets `SSH_ASKPASS` to the test binary, which is not
/// `gantry`, so the script names `gantry` itself).
fn setup(profile: &str, fake: Fake) -> Env {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("run")).unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let root = dir.path().display();
    let mut script = format!(
        "#!/bin/sh\nexport XDG_RUNTIME_DIR='{root}/run' HOME='{root}/home' SHELL=/bin/sh GANTRY_PROFILE='{profile}'\n\
         PATH='{root}/fakebin':\"$PATH\"\nprintf '%s' \"$SSH_ASKPASS\" > '{root}/askpass-env'\n"
    );
    if let Some(prompt) = fake.ask {
        let prompt = prompt.replace('\'', "'\\''");
        script.push_str(&format!(
            "if [ -n \"$SSH_ASKPASS\" ]; then\n  answer=$('{GANTRY}' '{prompt}') || \
             {{ echo 'Permission denied (publickey,password).' >&2; exit 255; }}\n  printf '%s' \"$answer\" > '{root}/answer'\nfi\n"
        ));
    }
    if fake.run_remote {
        // Like sshd: the remote command runs in a child of its own that outlives a killed ssh, with the same stdin pipe (through fd 3: dash gives a background job /dev/null, not its `<&0`).
        script.push_str(
            "unset GANTRY_ASKPASS_SOCKET\nfor last; do :; done\nexec 3<&0\nsh -c \"$last\" <&3 &\npid=$!\nwait $pid\nexit $?\n",
        );
    } else if let Some(body) = fake.body {
        script.push_str(&format!("GANTRY='{GANTRY}'\n{body}\n"));
    } else if let Some((code, message)) = fake.fail {
        script.push_str(&format!("echo '{message}' >&2\nexit {code}\n"));
    } else {
        // The remote side never sees the local sign-in socket.
        script.push_str(&format!("unset GANTRY_ASKPASS_SOCKET\nexec '{GANTRY}' bridge\n"));
    }
    let path = dir.path().join("fake-ssh");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    Env { dir, fake: path }
}

fn spec() -> SshHostSpec {
    SshHostSpec {
        name: "devbox-name".into(),
        target: "devbox".into(),
        command: "gantry".into(),
        auth: Default::default(),
        port: None,
        share: None,
    }
}

fn sh(script: &str) -> SpawnRequest {
    SpawnRequest {
        preview: false,
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

fn output_until(events: &async_channel::Receiver<ClientEvent>, needle: &str) -> String {
    let mut text = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !text.contains(needle) {
        assert!(Instant::now() < deadline, "timed out waiting for {needle:?}; got {text:?}");
        if let Ok(ClientEvent::Output { bytes, .. }) = events.recv_blocking() {
            text.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    text
}

fn stop_daemon(connection: &daemon::client::Connection) {
    connection.client.stop_all_and_wait(Duration::from_secs(3));
}

#[test]
fn a_pane_works_through_the_bridge_and_survives_the_connection() {
    let env = setup("sshbridge1", Fake::default());
    let first = connect(env.fake.to_str().unwrap(), &spec(), Attempt::Background).unwrap();
    assert!(!first.instance.is_empty());
    first.client.spawn(1, sh("printf through-ssh; sleep 30"));
    output_until(&first.events, "through-ssh");
    let instance = first.instance.clone();
    // Dropping the connection ends ssh and the bridge; the persistent daemon and its pane stay.
    drop(first);
    std::thread::sleep(Duration::from_millis(500));
    let second = connect(env.fake.to_str().unwrap(), &spec(), Attempt::Background).unwrap();
    assert_eq!(second.instance, instance, "the same daemon");
    assert_eq!(second.panes.len(), 1, "the pane is still there");
    // Attaching replays the screen: the earlier text arrives again.
    second.client.attach(second.panes[0].pane);
    output_until(&second.events, "through-ssh");
    stop_daemon(&second);
}

#[test]
fn a_remote_daemon_lists_folders() {
    let env = setup("sshbridgelist", Fake::default());
    let tree = env.path("tree");
    std::fs::create_dir_all(tree.join("alpha")).unwrap();
    let connection = connect(env.fake.to_str().unwrap(), &spec(), Attempt::Background).unwrap();
    connection.client.list_dir(1, tree.to_str().unwrap());
    connection.client.list_dir(2, "relative");
    let (mut listing, mut failure) = (None, None);
    let deadline = Instant::now() + Duration::from_secs(10);
    while listing.is_none() || failure.is_none() {
        assert!(Instant::now() < deadline, "timed out waiting for the listings");
        match connection.events.recv_blocking() {
            Ok(ClientEvent::DirListing { request: 1, path, dirs }) => listing = Some((path, dirs)),
            Ok(ClientEvent::DirListingFailed { request: 2, reason }) => failure = Some(reason),
            _ => {}
        }
    }
    let (path, dirs) = listing.unwrap();
    assert_eq!((path, dirs.len(), dirs[0].name.as_str()), (tree, 1, "alpha"));
    assert!(failure.unwrap().contains("absolute"));
    stop_daemon(&connection);
}

#[test]
fn a_remote_daemon_saves_pasted_images() {
    let env = setup("sshbridgepaste", Fake::default());
    let connection = connect(env.fake.to_str().unwrap(), &spec(), Attempt::Background).unwrap();
    assert!(connection.client.paste_image());
    connection.client.spawn(1, sh("sleep 30"));
    let deadline = Instant::now() + Duration::from_secs(10);
    let pane = loop {
        assert!(Instant::now() < deadline, "timed out waiting for the pane");
        if let Ok(ClientEvent::Spawned { pane, .. }) = connection.events.recv_blocking() {
            break pane;
        }
    };
    let bytes = vec![0x89, b'P', b'N', b'G', 0, 255];
    connection.client.save_paste_image(pane, ImageFormat::Png, bytes.clone());
    let path = loop {
        assert!(Instant::now() < deadline, "timed out waiting for the saved image");
        match connection.events.recv_blocking() {
            Ok(ClientEvent::PasteImageSaved { pane: p, path }) => {
                assert_eq!(p, pane);
                break path;
            }
            Ok(ClientEvent::PasteImageFailed { reason, .. }) => panic!("not saved: {reason}"),
            _ => {}
        }
    };
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    stop_daemon(&connection);
}

#[test]
fn an_interactive_attempt_asks_through_askpass_and_gets_the_answer() {
    let env = setup("sshbridge2", Fake { ask: Some("me@devbox's password: "), ..Fake::default() });
    let (prompts_tx, prompts_rx) = async_channel::unbounded::<AskPrompt>();
    let responder = std::thread::spawn(move || {
        let prompt = prompts_rx.recv_blocking().unwrap();
        assert_eq!(prompt.host, "devbox-name", "the dialog names the host, not the ssh target");
        assert_eq!(prompt.prompt, "me@devbox's password: ");
        prompt.reply.send_blocking(AskAnswer::Answer("hunter2".into())).unwrap();
    });
    let attempt = Attempt::Interactive { prompts: prompts_tx, runtime_dir: env.path("run") };
    let connection = connect(env.fake.to_str().unwrap(), &spec(), attempt).unwrap();
    responder.join().unwrap();
    assert_eq!(std::fs::read_to_string(env.path("answer")).unwrap(), "hunter2");
    assert!(!std::fs::read_to_string(env.path("askpass-env")).unwrap().is_empty(), "ssh was given the helper");
    stop_daemon(&connection);
}

#[test]
fn a_cancelled_sign_in_ends_the_attempt_as_needs_input() {
    let env = setup("sshbridge3", Fake { ask: Some("Password: "), ..Fake::default() });
    let (prompts_tx, prompts_rx) = async_channel::unbounded::<AskPrompt>();
    std::thread::spawn(move || {
        let prompt = prompts_rx.recv_blocking().unwrap();
        prompt.reply.send_blocking(AskAnswer::Cancel).unwrap();
    });
    let attempt = Attempt::Interactive { prompts: prompts_tx, runtime_dir: env.path("run") };
    let started = Instant::now();
    let error = connect(env.fake.to_str().unwrap(), &spec(), attempt).err().expect("the attempt fails");
    assert_eq!(error, FailKind::NeedsInput);
    assert!(started.elapsed() < Duration::from_secs(10), "no hang");
    // The sign-in socket was removed with the attempt.
    let leftovers = std::fs::read_dir(env.path("run"))
        .unwrap()
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().starts_with("askpass-"));
    assert!(!leftovers, "the askpass socket is cleaned up");
}

#[test]
fn a_background_attempt_never_gets_the_askpass_helper() {
    let fake = Fake {
        ask: Some("Password: "),
        fail: Some((255, "me@devbox: Permission denied (publickey).")),
        ..Fake::default()
    };
    let env = setup("sshbridge4", fake);
    let error = connect(env.fake.to_str().unwrap(), &spec(), Attempt::Background).err().expect("fails");
    assert_eq!(error, FailKind::NeedsInput);
    // ssh got no SSH_ASKPASS, so the fake never asked.
    assert_eq!(std::fs::read_to_string(env.path("askpass-env")).unwrap(), "");
    assert!(!env.path("answer").exists());
}

#[test]
fn a_missing_binary_is_not_installed_and_a_dead_ssh_is_a_reason() {
    let env =
        setup("sshbridge5", Fake { fail: Some((127, "bash: line 1: gantry: command not found")), ..Fake::default() });
    let error = connect(env.fake.to_str().unwrap(), &spec(), Attempt::Background).err().unwrap();
    assert_eq!(error, FailKind::NotInstalled);
    let refused =
        Fake { fail: Some((255, "ssh: connect to host devbox port 22: Connection refused")), ..Fake::default() };
    let env = setup("sshbridge5", refused);
    match connect(env.fake.to_str().unwrap(), &spec(), Attempt::Background).err().unwrap() {
        FailKind::Other { reason } => assert!(reason.contains("Connection refused"), "{reason}"),
        other => panic!("{other:?}"),
    }
    // A program that does not exist.
    match connect("/no/such/ssh", &spec(), Attempt::Background).err().unwrap() {
        FailKind::Other { reason } => assert!(reason.contains("/no/such/ssh"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_second_question_after_a_cancel_never_reaches_the_dialog_and_ssh_is_killed() {
    // Real ssh asks again after a failed helper (an empty password, up to three times) and does not exit by itself.
    let body = "\"$GANTRY\" askpass 'Password: ' >/dev/null\n\"$GANTRY\" askpass 'Password: ' >/dev/null\n\
                touch \"$(dirname \"$0\")/asked-twice\"\nsleep 60";
    let env = setup("sshbridge6", Fake { body: Some(body.into()), ..Fake::default() });
    let (prompts_tx, prompts_rx) = async_channel::unbounded::<AskPrompt>();
    let responder = std::thread::spawn(move || {
        let mut asked = 0;
        while let Ok(prompt) = prompts_rx.recv_blocking() {
            asked += 1;
            let _ = prompt.reply.send_blocking(AskAnswer::Cancel);
        }
        asked
    });
    let attempt = Attempt::Interactive { prompts: prompts_tx, runtime_dir: env.path("run") };
    let started = Instant::now();
    let error = connect(env.fake.to_str().unwrap(), &spec(), attempt).err().expect("the attempt fails");
    assert_eq!(error, FailKind::NeedsInput);
    assert!(started.elapsed() < Duration::from_secs(10), "ssh was killed instead of sleeping 60 s");
    assert_eq!(responder.join().unwrap(), 1, "only the first question was ever asked");
}

#[test]
fn a_handshake_that_never_finishes_times_out_with_a_reason() {
    let env = setup("sshbridge7", Fake { body: Some("sleep 60".into()), ..Fake::default() });
    let started = Instant::now();
    let error = connect_with(
        env.fake.to_str().unwrap(),
        &spec(),
        Attempt::Background,
        &AttemptHandle::default(),
        Duration::from_millis(500),
    )
    .err()
    .expect("times out");
    assert_eq!(error, FailKind::Other { reason: "timed out waiting for Gantry on the host".into() });
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn cancelling_a_connecting_attempt_kills_ssh_and_ends_it() {
    let env = setup("sshbridge8", Fake { body: Some("sleep 60".into()), ..Fake::default() });
    let handle = AttemptHandle::default();
    let canceller = handle.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        canceller.cancel();
    });
    let started = Instant::now();
    let error = connect_with(env.fake.to_str().unwrap(), &spec(), Attempt::Background, &handle, HANDSHAKE_TIMEOUT)
        .err()
        .unwrap();
    assert_eq!(error, FailKind::NeedsInput);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_remote_daemon_with_another_protocol_is_update_needed() {
    let body = "cat \"$(dirname \"$0\")/mismatch.bin\"\nsleep 5";
    let env = setup("sshbridge9", Fake { body: Some(body.into()), ..Fake::default() });
    let mut frame = Vec::new();
    let message = protocol::message::DaemonMessage::VersionMismatch { daemon_protocol: 99 };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(protocol::frame::write_frame(&mut frame, &protocol::frame::Frame::control(&message))).unwrap();
    std::fs::write(env.path("mismatch.bin"), frame).unwrap();
    let error = connect(env.fake.to_str().unwrap(), &spec(), Attempt::Background).err().expect("fails");
    assert_eq!(error, FailKind::UpdateNeeded { remote: Some("protocol 99".into()) });
}

#[test]
fn an_interactive_attempt_that_is_refused_without_a_question_says_authentication_failed() {
    let fake = Fake { fail: Some((255, "me@devbox: Permission denied (publickey).")), ..Fake::default() };
    let env = setup("sshbridge10", fake);
    let (prompts_tx, _prompts_rx) = async_channel::unbounded::<AskPrompt>();
    let attempt = Attempt::Interactive { prompts: prompts_tx, runtime_dir: env.path("run") };
    match connect(env.fake.to_str().unwrap(), &spec(), attempt).err().unwrap() {
        FailKind::Other { reason } => {
            assert!(reason.starts_with("Authentication failed (ssh asked for nothing)"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_handshake_timeout_waits_while_a_question_is_open() {
    let env = setup("sshbridge11", Fake { ask: Some("me@devbox's password: "), ..Fake::default() });
    let (prompts_tx, prompts_rx) = async_channel::unbounded::<AskPrompt>();
    std::thread::spawn(move || {
        let prompt = prompts_rx.recv_blocking().unwrap();
        // The user takes longer than the 1 s handshake timeout.
        std::thread::sleep(Duration::from_secs(2));
        prompt.reply.send_blocking(AskAnswer::Answer("hunter2".into())).unwrap();
    });
    let attempt = Attempt::Interactive { prompts: prompts_tx, runtime_dir: env.path("run") };
    let connection =
        connect_with(env.fake.to_str().unwrap(), &spec(), attempt, &AttemptHandle::default(), Duration::from_secs(1))
            .expect("the time spent answering does not count");
    // The sign-in socket is gone once the handshake is done.
    let leftovers = std::fs::read_dir(env.path("run"))
        .unwrap()
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().starts_with("askpass-"));
    assert!(!leftovers, "the askpass socket is removed after a successful handshake");
    stop_daemon(&connection);
}

#[test]
fn an_interactive_attempt_that_asks_nothing_still_times_out() {
    let env = setup("sshbridge12", Fake { body: Some("sleep 60".into()), ..Fake::default() });
    let (prompts_tx, _prompts_rx) = async_channel::unbounded::<AskPrompt>();
    let attempt = Attempt::Interactive { prompts: prompts_tx, runtime_dir: env.path("run") };
    let error = connect_with(
        env.fake.to_str().unwrap(),
        &spec(),
        attempt,
        &AttemptHandle::default(),
        Duration::from_millis(500),
    )
    .err()
    .expect("times out");
    assert_eq!(error, FailKind::Other { reason: "timed out waiting for Gantry on the host".into() });
}

/// A fake ssh that runs ssh's remote command text locally, like a remote shell.
fn remote_env(profile: &str, ask: Option<&'static str>) -> Env {
    setup(profile, Fake { ask, run_remote: true, ..Fake::default() })
}

fn remote_spec(env: &Env, command: &str) -> SshHostSpec {
    SshHostSpec {
        name: "devbox-name".into(),
        target: "devbox".into(),
        command: env.path(command).display().to_string(),
        auth: Default::default(),
        port: None,
        share: None,
    }
}

fn write_script(path: &std::path::Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn uname() -> String {
    let out = std::process::Command::new("uname").arg("-sm").output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn probe_reads_a_fake_remote() {
    let env = remote_env("sshinst1", None);
    write_script(&env.path("stub-gantry"), "#!/bin/sh\necho 'gantry 0.17.0 (test)'\n");
    let spec = remote_spec(&env, "stub-gantry");
    let probe =
        remote_install::probe(env.fake.to_str().unwrap(), &spec, Attempt::Background, &AttemptHandle::default())
            .unwrap();
    assert_eq!(probe.version.as_deref(), Some("0.17.0"));
    assert_eq!(probe.os_arch, uname());
    assert_eq!(probe.home, env.path("home").display().to_string());
}

#[test]
fn probe_of_a_missing_gantry_says_missing() {
    let env = remote_env("sshinst1b", None);
    let spec = remote_spec(&env, "nowhere/gantry");
    let probe =
        remote_install::probe(env.fake.to_str().unwrap(), &spec, Attempt::Background, &AttemptHandle::default())
            .unwrap();
    assert_eq!(probe.version, None);
}

fn big_file(env: &Env, name: &str, megabytes: usize) -> std::path::PathBuf {
    let path = env.path(name);
    let chunk: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let mut data = Vec::new();
    for _ in 0..megabytes {
        data.extend_from_slice(&chunk);
    }
    std::fs::write(&path, data).unwrap();
    path
}

#[test]
fn upload_writes_the_file_with_mode_755_and_removes_the_temp_name() {
    let env = remote_env("sshinst2", None);
    let local = big_file(&env, "local-binary", 3);
    let spec = remote_spec(&env, "bin/gantry");
    let mut last = 0;
    let mut progress = |sent: u64| last = sent;
    remote_install::upload(
        env.fake.to_str().unwrap(),
        &spec,
        Attempt::Background,
        &AttemptHandle::default(),
        &local,
        &mut progress,
    )
    .unwrap();
    let target = env.path("bin/gantry");
    assert_eq!(std::fs::read(&target).unwrap(), std::fs::read(&local).unwrap());
    assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o755);
    assert!(!env.path("bin/gantry.new").exists());
    assert_eq!(last, std::fs::metadata(&local).unwrap().len());
}

#[test]
fn a_cut_upload_leaves_the_old_binary_and_no_new_file() {
    let env = remote_env("sshinst3", None);
    let local = big_file(&env, "local-binary", 20);
    let spec = remote_spec(&env, "bin/gantry");
    std::fs::create_dir_all(env.path("bin")).unwrap();
    std::fs::write(env.path("bin/gantry"), "old").unwrap();
    let new_file = env.path("bin/gantry.new");
    let handle = AttemptHandle::default();
    let canceller = handle.clone();
    let mut cut = false;
    let mut progress = |_sent: u64| {
        if !cut {
            cut = true;
            // Cut only once the remote command is writing its file, like a dropped connection mid-upload.
            let deadline = Instant::now() + Duration::from_secs(10);
            while !new_file.exists() {
                assert!(Instant::now() < deadline, "the remote never created gantry.new");
                std::thread::sleep(Duration::from_millis(10));
            }
            canceller.cancel();
        }
    };
    let error =
        remote_install::upload(env.fake.to_str().unwrap(), &spec, Attempt::Background, &handle, &local, &mut progress)
            .expect_err("cancelled");
    assert_eq!(error, FailKind::NeedsInput);
    // The remote command outlives ssh (as under sshd): wait until it is done with gantry.new, whichever way it went.
    let deadline = Instant::now() + Duration::from_secs(10);
    while new_file.exists() {
        assert!(Instant::now() < deadline, "gantry.new was never removed");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(std::fs::read_to_string(env.path("bin/gantry")).unwrap(), "old", "a cut upload replaced the binary");
}

#[test]
fn a_refused_command_never_runs() {
    let env = remote_env("sshinst4", None);
    let local = big_file(&env, "local-binary", 1);
    let mut spec = remote_spec(&env, "bin/gantry");
    spec.command = "env X=1 gantry".into();
    let error = remote_install::upload(
        env.fake.to_str().unwrap(),
        &spec,
        Attempt::Background,
        &AttemptHandle::default(),
        &local,
        &mut |_| {},
    )
    .expect_err("refused");
    match error {
        FailKind::Other { reason } => assert!(reason.contains("install Gantry on that host by hand"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert!(!env.path("askpass-env").exists(), "ssh was never started");
}

#[test]
fn a_gantry_that_cannot_start_lists_every_missing_library_at_once() {
    let env = remote_env("sshinst5b", None);
    let loader = "#!/bin/sh\necho \"$0: error while loading shared libraries: libasound.so.2: cannot open shared object file\" >&2\nexit 127\n";
    write_script(&env.path("blocked-gantry"), loader);
    write_script(
        &env.path("fakebin/ldd"),
        "#!/bin/sh\nprintf '\\tlibasound.so.2 => not found\\n\\tlibxkbcommon-x11.so.0 => not found\\n'\n",
    );
    let spec = remote_spec(&env, "blocked-gantry");
    match remote_install::verify(env.fake.to_str().unwrap(), &spec, Attempt::Background, &AttemptHandle::default())
        .unwrap_err()
    {
        FailKind::Other { reason } => {
            assert!(reason.contains("libasound.so.2") && reason.contains("libxkbcommon-x11.so.0"), "{reason}");
            assert!(reason.contains("sudo apt install -y libasound2t64 libxkbcommon-x11-0"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    // ldd finds nothing to name: the first error stays as it was.
    write_script(&env.path("fakebin/ldd"), "#!/bin/sh\necho 'not a dynamic executable'\n");
    match remote_install::verify(env.fake.to_str().unwrap(), &spec, Attempt::Background, &AttemptHandle::default())
        .unwrap_err()
    {
        FailKind::Other { reason } => {
            assert!(reason.contains("libasound.so.2") && !reason.contains("libxkbcommon-x11"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn verify_returns_the_version_line_or_the_failure() {
    let env = remote_env("sshinst5", None);
    write_script(&env.path("stub-gantry"), "#!/bin/sh\necho 'gantry 0.17.0 (test)'\n");
    let spec = remote_spec(&env, "stub-gantry");
    let line =
        remote_install::verify(env.fake.to_str().unwrap(), &spec, Attempt::Background, &AttemptHandle::default())
            .unwrap();
    assert_eq!(line, "gantry 0.17.0 (test)");
    write_script(&env.path("broken-gantry"), "#!/bin/sh\necho 'GLIBC_2.39 not found' >&2\nexit 1\n");
    let spec = remote_spec(&env, "broken-gantry");
    match remote_install::verify(env.fake.to_str().unwrap(), &spec, Attempt::Background, &AttemptHandle::default())
        .unwrap_err()
    {
        FailKind::Other { reason } => assert!(reason.contains("GLIBC_2.39 not found"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn stop_remote_daemon_ends_the_daemon_over_the_bridge() {
    let env = setup("sshinst6", Fake::default());
    let first = connect(env.fake.to_str().unwrap(), &spec(), Attempt::Background).unwrap();
    let instance = first.instance.clone();
    let started = Instant::now();
    remote_install::stop_remote_daemon(
        env.fake.to_str().unwrap(),
        &spec(),
        Attempt::Background,
        &AttemptHandle::default(),
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "the bridge ends when the daemon closes the socket");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "the first connection never ended");
        if matches!(first.events.recv_blocking(), Ok(ClientEvent::Disconnected) | Err(_)) {
            break;
        }
    }
    let second = connect(env.fake.to_str().unwrap(), &spec(), Attempt::Background).unwrap();
    assert_ne!(second.instance, instance, "a fresh daemon");
    stop_daemon(&second);
}

#[test]
fn stopping_through_the_open_client_ends_the_stream_without_taken_over() {
    let env = setup("sshinst7", Fake::default());
    let first = connect(env.fake.to_str().unwrap(), &spec(), Attempt::Background).unwrap();
    assert!(first.daemon_version.is_some(), "the daemon says which version it is");
    first.client.spawn(1, sh("sleep 30"));
    // The sidebar update stops the old daemon like this, not over a second bridge (which would take this one over).
    first.client.stop_all_and_wait(Duration::from_secs(2));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "the connection never ended");
        match first.events.recv_blocking() {
            Ok(ClientEvent::TakenOver) => panic!("a stop through the open client must not look like a take-over"),
            Ok(ClientEvent::Disconnected) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

#[test]
fn enable_linger_reports_stderr() {
    let env = remote_env("sshinst7", None);
    write_script(&env.path("fakebin/loginctl"), "#!/bin/sh\necho 'Interactive authentication required.' >&2\nexit 1\n");
    let spec = remote_spec(&env, "bin/gantry");
    let error = remote_install::enable_linger(
        env.fake.to_str().unwrap(),
        &spec,
        Attempt::Background,
        &AttemptHandle::default(),
    )
    .expect_err("fails");
    assert!(error.contains("Interactive authentication required."), "{error}");
}

#[test]
fn the_answer_to_a_question_is_used_for_the_probe() {
    let env = remote_env("sshinst8", Some("Password:"));
    write_script(&env.path("stub-gantry"), "#!/bin/sh\necho 'gantry 0.17.0 (test)'\n");
    let spec = remote_spec(&env, "stub-gantry");
    let (prompts_tx, prompts_rx) = async_channel::unbounded::<AskPrompt>();
    let responder = std::thread::spawn(move || {
        let prompt = prompts_rx.recv_blocking().unwrap();
        prompt.reply.send_blocking(AskAnswer::Answer("pw".into())).unwrap();
    });
    let attempt = Attempt::Interactive { prompts: prompts_tx, runtime_dir: env.path("run") };
    let probe = remote_install::probe(env.fake.to_str().unwrap(), &spec, attempt, &AttemptHandle::default()).unwrap();
    responder.join().unwrap();
    assert_eq!(probe.version.as_deref(), Some("0.17.0"));
    assert_eq!(std::fs::read_to_string(env.path("answer")).unwrap(), "pw");
}

#[test]
fn a_remote_that_stalls_times_out() {
    let env = setup("sshinst9", Fake { body: Some("sleep 60".into()), ..Fake::default() });
    let spec = remote_spec(&env, "bin/gantry");
    let started = Instant::now();
    let error = remote_install::run_remote(
        env.fake.to_str().unwrap(),
        &spec,
        Attempt::Background,
        &AttemptHandle::default(),
        "true",
        None,
        &mut |_| {},
        Timeouts { idle: Duration::from_millis(500) },
    )
    .expect_err("times out");
    assert!(matches!(error, FailKind::Other { .. }), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn stop_remote_daemon_works_when_the_daemon_answers_version_mismatch() {
    // A daemon of another protocol answers the frozen hello with VersionMismatch, then reads until StopAll.
    let hello = protocol::message::ClientMessage::Hello { protocol: protocol::message::PROTOCOL_VERSION };
    let mut expected = Vec::new();
    let mut reply = Vec::new();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        for message in [&hello, &protocol::message::ClientMessage::StopAll] {
            protocol::frame::write_frame(&mut expected, &protocol::frame::Frame::control(message)).await.unwrap();
        }
        let mismatch = protocol::message::DaemonMessage::VersionMismatch { daemon_protocol: 6 };
        protocol::frame::write_frame(&mut reply, &protocol::frame::Frame::control(&mismatch)).await.unwrap();
    });
    let body = format!(
        "dir=\"$(dirname \"$0\")\"\ncat \"$dir/mismatch.bin\"\nhead -c {} > \"$dir/received.bin\"",
        expected.len()
    );
    let env = setup("sshinst10", Fake { body: Some(body), ..Fake::default() });
    std::fs::write(env.path("mismatch.bin"), reply).unwrap();
    remote_install::stop_remote_daemon(
        env.fake.to_str().unwrap(),
        &spec(),
        Attempt::Background,
        &AttemptHandle::default(),
    )
    .expect("a daemon of another protocol still stops");
    assert_eq!(std::fs::read(env.path("received.bin")).unwrap(), expected, "Hello, then the frozen StopAll");
}

#[test]
fn a_share_adds_exactly_the_control_master_options_before_the_target() {
    let share = Share::at("/run/user/1000/gantry-default/cm-0a1b2c3d".into());
    let mut spec = spec();
    let plain = ssh_args_for(&spec, false, "true");
    assert!(!plain.iter().any(|arg| arg.contains("Control")), "{plain:?}");
    spec.share = Some(share);
    let shared = ssh_args_for(&spec, false, "true");
    let dash = shared.iter().position(|arg| arg == "--").unwrap();
    let expected: Vec<String> = [
        "-o",
        "ControlMaster=auto",
        "-o",
        "ControlPath=/run/user/1000/gantry-default/cm-0a1b2c3d",
        "-o",
        "ControlPersist=60",
    ]
    .map(String::from)
    .into();
    let at = shared.windows(6).position(|window| window == expected.as_slice()).expect("the three options together");
    assert!(at + 6 <= dash, "before the target: {shared:?}");
    assert_eq!(shared.iter().filter(|arg| arg.starts_with("Control")).count(), 3);
    assert!(!shared.iter().any(|arg| arg == "BatchMode=yes"), "the first connection of a share may ask");
    let background = ssh_args_for(&spec, true, "true");
    assert!(background.iter().any(|arg| arg == "BatchMode=yes"), "a retry never prompts, shared or not");
}

#[test]
fn closing_a_share_asks_the_master_to_exit_or_stop_and_removes_the_socket_only_on_exit() {
    let env = setup("sshshare1", Fake::default());
    let recorder = env.path("record-ssh");
    write_script(&recorder, &format!("#!/bin/sh\necho \"$@\" >> '{}'\n", env.path("calls").display()));
    let socket = env.path("run/cm-test");
    std::fs::write(&socket, "").unwrap();
    let share = Share::at(socket.clone());
    share.close(recorder.to_str().unwrap(), "me@devbox", true);
    assert!(socket.exists(), "a stop leaves the socket to the master");
    share.close(recorder.to_str().unwrap(), "me@devbox", false);
    assert!(!socket.exists(), "an exit removes it");
    let calls = std::fs::read_to_string(env.path("calls")).unwrap();
    let path = socket.display();
    assert_eq!(
        calls,
        format!("-O stop -o ControlPath={path} -- me@devbox\n-O exit -o ControlPath={path} -- me@devbox\n")
    );
}
