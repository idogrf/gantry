//! The SSH transport: `ssh -T … -- <target> <command> bridge` as the stream of a daemon connection.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use app_core::host_state::{FailKind, classify_failure};
use app_core::settings::SshAuth;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

use crate::askpass::{AskPrompt, AskpassServer, AskpassState};
use crate::client::{ClientEvent, Connection, Hello, Link, Outgoing, StartError, connection, run_stream};

/// Where to connect: what follows `ssh`, and the Gantry command on the remote.
#[derive(Clone, Debug)]
pub struct SshHostSpec {
    /// The host's name in `sshHosts`, for the sign-in dialogs.
    pub name: String,
    /// An alias from `~/.ssh/config` or `user@host`.
    pub target: String,
    /// The remote Gantry binary, as the remote shell will run it.
    pub command: String,
    /// How to sign in (adds ssh's options).
    pub auth: SshAuth,
    /// The ssh port (`-p`); `None` = what ssh says.
    pub port: Option<u16>,
    /// A shared ssh connection (ControlMaster) to run through, so one sign-in serves several commands.
    pub share: Option<Share>,
}

/// One ssh connection shared by the commands of a wizard run: the first command opens the master (and may ask),
/// the others ride on it. Unix only (elsewhere there is no `Share`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    path: PathBuf,
}

impl Share {
    /// A share at control socket `path`.
    pub fn at(path: PathBuf) -> Share {
        Share { path }
    }

    /// A fresh share with a short control socket path in `runtime_dir` (the profile's 0700 dir); `None` where ssh
    /// cannot multiplex (Windows).
    pub fn new(runtime_dir: &Path) -> Option<Share> {
        if !cfg!(unix) {
            return None;
        }
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let id =
            nanos ^ std::process::id().rotate_left(16) ^ NEXT_SHARE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(Share { path: runtime_dir.join(format!("cm-{id:08x}")) })
    }

    /// The control socket.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Ask the master of `target` to stop (`keep_sessions`: it takes no new commands and goes when the running ones,
    /// such as a daemon connection made through it, end) or to exit now (and remove the socket). Best effort: errors
    /// are ignored, and it gives up on ssh after 5 s. Blocks.
    pub fn close(&self, program: &str, target: &str, keep_sessions: bool) {
        let action = if keep_sessions { "stop" } else { "exit" };
        let child = claude_index::process::hide_window(&mut std::process::Command::new(program))
            .args(["-O", action, "-o"])
            .arg(format!("ControlPath={}", self.path.display()))
            .args(["--", target])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while matches!(child.try_wait(), Ok(None)) {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if !keep_sessions {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

static NEXT_SHARE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// How long the handshake (`Welcome` and `Panes`) may take once ssh runs.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Lets the caller end an attempt that is still connecting.
#[derive(Clone, Default)]
pub struct AttemptHandle {
    pub(crate) cancel: Arc<Notify>,
}

impl AttemptHandle {
    /// Kill the attempt's ssh; `connect` then fails with `NeedsInput`. No effect once the handshake is done.
    pub fn cancel(&self) {
        self.cancel.notify_one();
    }
}

/// How an attempt may talk to the user.
pub enum Attempt {
    /// The user asked: ssh may prompt, through dialogs.
    Interactive {
        /// Where the questions go.
        prompts: async_channel::Sender<AskPrompt>,
        /// The profile's runtime dir (0700), where the sign-in socket is made.
        runtime_dir: PathBuf,
    },
    /// A reconnect retry: ssh must never prompt (it fails instead).
    Background,
}

/// The arguments of `ssh` for `spec` running the Gantry bridge. The target comes after `--` so it can never be read
/// as an option.
pub fn ssh_args(spec: &SshHostSpec, background: bool) -> Vec<String> {
    ssh_args_for(spec, background, &format!("{} bridge", spec.command))
}

/// The arguments of `ssh` for `spec` running `remote_command` (text for the remote shell).
pub fn ssh_args_for(spec: &SshHostSpec, background: bool, remote_command: &str) -> Vec<String> {
    let mut args: Vec<String> =
        ["-T", "-o", "ConnectTimeout=20", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"]
            .into_iter()
            .map(String::from)
            .collect();
    if background {
        args.extend(["-o".into(), "BatchMode=yes".into()]);
    }
    args.extend(spec.auth.ssh_options().iter().map(|option| option.to_string()));
    if let Some(port) = spec.port {
        args.extend(["-p".into(), port.to_string()]);
    }
    if let Some(share) = &spec.share {
        args.extend([
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}", share.path.display()),
            "-o".into(),
            "ControlPersist=60".into(),
        ]);
    }
    args.extend(["--".into(), spec.target.clone(), remote_command.to_string()]);
    args
}

/// Connect to the remote daemon through `program` (normally `ssh`). Blocks until the handshake is done or failed;
/// the returned connection keeps running on its own thread. `handle.cancel()` ends the attempt; a handshake that
/// takes longer than `handshake_timeout` fails it.
pub fn connect(
    program: &str,
    spec: &SshHostSpec,
    attempt: Attempt,
    handle: &AttemptHandle,
    handshake_timeout: Duration,
) -> Result<Connection, FailKind> {
    let Link { client, outgoing, events_tx, events } = Link::new();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Hello, FailKind>>();
    let (program, spec, handle) = (program.to_string(), spec.clone(), handle.clone());
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(err) => {
                let _ = ready_tx.send(Err(FailKind::Other { reason: err.to_string() }));
                return;
            }
        };
        runtime.block_on(run(program, spec, attempt, handle, handshake_timeout, ready_tx, outgoing, events_tx));
    });
    match ready_rx.recv() {
        Ok(Ok(hello)) => Ok(connection(client, hello, events)),
        Ok(Err(kind)) => Err(kind),
        Err(_) => Err(FailKind::Other { reason: "the ssh thread ended".into() }),
    }
}

/// Resolves after `total` of time during which no sign-in question waits for the user.
pub(crate) async fn timeout_without_questions(state: &AskpassState, total: Duration) {
    let mut left = total;
    loop {
        if state.waiting() {
            state.wait_changed().await;
            continue;
        }
        let started = tokio::time::Instant::now();
        tokio::select! {
            () = tokio::time::sleep(left) => return,
            () = state.wait_changed() => left = left.saturating_sub(started.elapsed()),
        }
    }
}

/// Set up `command`'s environment for `attempt`: for an interactive one start the sign-in server and point
/// `SSH_ASKPASS` at this binary; otherwise (or when that is impossible) remove `SSH_ASKPASS`. Returns the server (the
/// sign-in socket lives as long as it does), the questions' state and whether ssh must run in batch mode.
pub(crate) async fn prepare(
    command: &mut tokio::process::Command,
    spec: &SshHostSpec,
    attempt: &Attempt,
) -> (Option<AskpassServer>, Arc<AskpassState>, bool) {
    let mut background = !matches!(attempt, Attempt::Interactive { .. });
    let state = Arc::new(AskpassState::default());
    let mut askpass: Option<AskpassServer> = None;
    if let Attempt::Interactive { prompts, runtime_dir } = attempt {
        match AskpassServer::start(runtime_dir, &spec.name, prompts.clone(), state.clone()).await {
            Ok(server) => {
                if let Ok(exe) = std::env::current_exe() {
                    command
                        .env("SSH_ASKPASS", exe)
                        .env("SSH_ASKPASS_REQUIRE", "force")
                        .env("GANTRY_ASKPASS_SOCKET", server.path());
                    askpass = Some(server);
                } else {
                    background = true;
                }
            }
            // No sign-in dialogs without a local socket: keys and the agent only.
            Err(_) => background = true,
        }
    }
    if background {
        command.env_remove("SSH_ASKPASS").env_remove("SSH_ASKPASS_REQUIRE");
    }
    (askpass, state, background)
}

/// Collect what ssh prints on stderr in the background; only the tail (the last words of ssh) is kept.
pub(crate) fn spawn_stderr_tail(mut stderr: tokio::process::ChildStderr) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        let mut keep = text.len().saturating_sub(4096);
        while !text.is_char_boundary(keep) {
            keep += 1;
        }
        text[keep..].to_string()
    })
}

/// Why an attempt was ended from outside, before its handshake finished.
enum Abort {
    Cancelled,
    TimedOut,
}

#[allow(clippy::too_many_arguments)]
async fn run(
    program: String,
    spec: SshHostSpec,
    attempt: Attempt,
    handle: AttemptHandle,
    handshake_timeout: Duration,
    ready: std::sync::mpsc::Sender<Result<Hello, FailKind>>,
    outgoing: tokio::sync::mpsc::UnboundedReceiver<Outgoing>,
    events: async_channel::Sender<ClientEvent>,
) {
    let fail = |kind: FailKind| {
        let _ = ready.send(Err(kind));
    };
    let interactive = matches!(attempt, Attempt::Interactive { .. });
    let mut command = tokio::process::Command::new(&program);
    claude_index::process::hide_window(command.as_std_mut());
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    // Dropping the server removes the sign-in socket: it goes right after the handshake.
    let (askpass, state, background) = prepare(&mut command, &spec, &attempt).await;
    command.args(ssh_args(&spec, background));
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return fail(FailKind::Other { reason: format!("could not run {program}: {err}") }),
    };
    let (stdin, stdout, stderr) = match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
        (Some(stdin), Some(stdout), Some(stderr)) => (stdin, stdout, stderr),
        _ => return fail(FailKind::Other { reason: "ssh has no pipes".into() }),
    };
    let stderr_text = spawn_stderr_tail(stderr);
    let handshaken = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hello_ready = ready.clone();
    let done = handshaken.clone();
    let outcome = {
        let stream = run_stream(stdout, stdin, outgoing, events, move |hello| {
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            // The sign-in socket is only for the handshake.
            drop(askpass);
            let _ = hello_ready.send(Ok(hello));
        });
        // Only the handshake can be cancelled or time out; once it is done this never resolves.
        let abort = async {
            let reason = tokio::select! {
                () = handle.cancel.notified() => Abort::Cancelled,
                () = state.wait_cancelled() => Abort::Cancelled,
                () = timeout_without_questions(&state, handshake_timeout) => Abort::TimedOut,
            };
            if handshaken.load(std::sync::atomic::Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            reason
        };
        tokio::select! {
            outcome = stream => Ok(outcome),
            reason = abort => Err(reason),
        }
    };
    match outcome {
        Err(reason) => {
            let _ = child.start_kill();
            fail(match reason {
                Abort::Cancelled => FailKind::NeedsInput,
                Abort::TimedOut => FailKind::Other { reason: "timed out waiting for Gantry on the host".into() },
            });
        }
        Ok(Ok(())) => {
            let _ = child.start_kill();
        }
        Ok(Err(StartError::VersionMismatch { daemon_protocol })) => {
            let _ = child.start_kill();
            fail(classify_failure(None, "", Some(daemon_protocol), false, None));
        }
        Ok(Err(StartError::Io(error))) => {
            if state.cancelled() {
                let _ = child.start_kill();
                return fail(FailKind::NeedsInput);
            }
            // Let ssh say why it ended (its exit code and last words), but do not wait for long.
            let code = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                Ok(Ok(status)) => status.code(),
                _ => {
                    let _ = child.start_kill();
                    None
                }
            };
            let stderr = tokio::time::timeout(Duration::from_secs(2), stderr_text).await;
            let stderr = stderr.ok().and_then(Result::ok).unwrap_or_default();
            let unasked = interactive && !background && !state.asked();
            fail(classify_failure(code, &stderr, None, unasked, Some(&format!("{error:#}"))));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_is_passed_to_ssh_before_the_target() {
        let args = ssh_args(&SshHostSpec { port: Some(2222), ..spec() }, false);
        let at = args.iter().position(|arg| arg == "-p").expect("-p");
        assert_eq!(args[at + 1], "2222");
        assert!(at < args.iter().position(|arg| arg == "--").unwrap());
        assert!(!ssh_args(&spec(), false).contains(&"-p".to_string()));
    }

    #[test]
    fn sign_in_methods_add_their_options_before_the_target() {
        let with = |auth| ssh_args(&SshHostSpec { auth, ..spec() }, false);
        assert_eq!(with(SshAuth::Automatic), ssh_args(&spec(), false));
        assert!(!with(SshAuth::Automatic).iter().any(|arg| arg.contains("PreferredAuthentications")));
        let password = with(SshAuth::Password);
        assert!(password.contains(&"PreferredAuthentications=password,keyboard-interactive".to_string()));
        assert!(password.contains(&"IdentitiesOnly=yes".to_string()));
        let key = with(SshAuth::Key);
        assert!(key.contains(&"PreferredAuthentications=publickey".to_string()));
        assert!(!key.contains(&"IdentitiesOnly=yes".to_string()));
        for args in [password, key] {
            let dashes = args.iter().position(|arg| arg == "--").unwrap();
            assert!(args.iter().position(|arg| arg.starts_with("PreferredAuthentications")).unwrap() < dashes);
        }
    }

    fn spec() -> SshHostSpec {
        SshHostSpec {
            name: "devbox".into(),
            target: "me@devbox".into(),
            command: "~/.local/bin/gantry".into(),
            auth: SshAuth::Automatic,
            port: None,
            share: None,
        }
    }

    #[test]
    fn the_command_line_keeps_the_target_after_the_double_dash_and_batch_mode_for_retries() {
        let interactive = ssh_args(&spec(), false);
        assert_eq!(
            interactive,
            [
                "-T",
                "-o",
                "ConnectTimeout=20",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "--",
                "me@devbox",
                "~/.local/bin/gantry bridge"
            ]
        );
        let background = ssh_args(&spec(), true);
        assert!(background.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]), "{background:?}");
        let dash = background.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(&background[dash + 1..], ["me@devbox", "~/.local/bin/gantry bridge"]);
    }
}
