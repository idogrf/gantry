//! The steps of the SSH install wizard, each one ssh command on the transport's attempt machinery: sign-in dialogs
//! through askpass, Cancel through the attempt handle, no step that hangs. Every function blocks.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use app_core::host_install::{Probe, UploadPath, parse_probe, probe_command, upload_path};
use app_core::host_state::{FailKind, classify_failure, describe_missing_libraries, parse_ldd};
use protocol::frame::{Frame, read_frame, write_frame};
use protocol::message::{ClientMessage, PROTOCOL_VERSION};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

use crate::askpass::{AskpassServer, AskpassState};
use crate::ssh::{
    Attempt, AttemptHandle, SshHostSpec, prepare, spawn_stderr_tail, ssh_args_for, timeout_without_questions,
};

/// How long a step may make no progress while no sign-in question waits.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// The idle limit; a chunk of stdin written restarts it.
    pub idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Timeouts {
        Timeouts { idle: Duration::from_secs(30) }
    }
}

const UPLOAD_IDLE: Duration = Duration::from_secs(60);
const CLEANUP_IDLE: Duration = Duration::from_secs(10);
const STOP_IDLE: Duration = Duration::from_secs(10);
const CHUNK: usize = 64 * 1024;

fn other(reason: impl Into<String>) -> FailKind {
    FailKind::Other { reason: reason.into() }
}

fn runtime() -> Result<tokio::runtime::Runtime, FailKind> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|err| other(err.to_string()))
}

/// A running ssh child with its sign-in server and its stderr tail.
struct Started {
    child: tokio::process::Child,
    state: Arc<AskpassState>,
    /// The sign-in socket lives as long as the step does.
    _askpass: Option<AskpassServer>,
    stderr: tokio::task::JoinHandle<String>,
    /// ssh may have asked a question (an interactive attempt with a working sign-in server).
    can_ask: bool,
}

async fn start(
    program: &str,
    spec: &SshHostSpec,
    attempt: Attempt,
    remote_command: &str,
    piped_stdin: bool,
) -> Result<Started, FailKind> {
    let interactive = matches!(attempt, Attempt::Interactive { .. });
    let mut command = tokio::process::Command::new(program);
    claude_index::process::hide_window(command.as_std_mut());
    command
        .stdin(if piped_stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let (askpass, state, background) = prepare(&mut command, spec, &attempt).await;
    command.args(ssh_args_for(spec, background, remote_command));
    let mut child = command.spawn().map_err(|err| other(format!("could not run {program}: {err}")))?;
    let Some(stderr) = child.stderr.take() else { return Err(other("ssh has no pipes")) };
    Ok(Started {
        child,
        state,
        _askpass: askpass,
        stderr: spawn_stderr_tail(stderr),
        can_ask: interactive && !background,
    })
}

/// Why an attempt was ended from outside.
enum Abort {
    Cancelled,
    TimedOut,
}

impl Abort {
    fn kind(&self, idle: Duration) -> FailKind {
        match self {
            Abort::Cancelled => FailKind::NeedsInput,
            Abort::TimedOut => other(format!("timed out: no progress for {} s", idle.as_secs())),
        }
    }
}

/// What ssh's end says about a failed step: its exit code and last words.
async fn failure(started: Started, code: Option<i32>) -> FailKind {
    if started.state.cancelled() {
        return FailKind::NeedsInput;
    }
    let stderr = tokio::time::timeout(Duration::from_secs(2), started.stderr).await;
    let stderr = stderr.ok().and_then(Result::ok).unwrap_or_default();
    let unasked = started.can_ask && !started.state.asked();
    classify_failure(code, &stderr, None, unasked, None)
}

/// Run `remote_command` (text for the remote shell) through `program` (normally `ssh`) and return its stdout. `stdin`
/// is sent in 64 KiB chunks, `on_progress` gets the total after each; the step fails after `timeouts.idle` without
/// progress while no question waits, and `handle.cancel()` ends it as `NeedsInput`. A non-zero exit is classified as
/// the transport does, so `Other` carries ssh's or the remote's last words.
#[allow(clippy::too_many_arguments)]
pub fn run_remote(
    program: &str,
    spec: &SshHostSpec,
    attempt: Attempt,
    handle: &AttemptHandle,
    remote_command: &str,
    stdin: Option<Box<dyn Read + Send>>,
    on_progress: &mut dyn FnMut(u64),
    timeouts: Timeouts,
) -> Result<String, FailKind> {
    runtime()?.block_on(async {
        let mut started = start(program, spec, attempt, remote_command, stdin.is_some()).await?;
        let pipe = started.child.stdin.take();
        let Some(mut stdout) = started.child.stdout.take() else { return Err(other("ssh has no pipes")) };
        let state = started.state.clone();
        let activity = Notify::new();
        let outcome = {
            let work = async {
                let feed = async {
                    if let (Some(mut pipe), Some(mut input)) = (pipe, stdin) {
                        let mut buffer = vec![0u8; CHUNK];
                        let mut sent = 0u64;
                        loop {
                            let read = match input.read(&mut buffer) {
                                Ok(0) => break,
                                Ok(read) => read,
                                Err(err) => return Err(format!("could not read the file: {err}")),
                            };
                            // A closed pipe means the remote command ended early: its exit status says why.
                            if pipe.write_all(&buffer[..read]).await.is_err() {
                                break;
                            }
                            sent += read as u64;
                            on_progress(sent);
                            activity.notify_one();
                        }
                        let _ = pipe.shutdown().await;
                    }
                    Ok(())
                };
                let read_out = async {
                    let mut out = Vec::new();
                    let _ = stdout.read_to_end(&mut out).await;
                    out
                };
                let (fed, out) = tokio::join!(feed, read_out);
                (fed, out, started.child.wait().await)
            };
            let idle = async {
                loop {
                    tokio::select! {
                        () = timeout_without_questions(&state, timeouts.idle) => return,
                        () = activity.notified() => {}
                    }
                }
            };
            tokio::select! {
                done = work => Ok(done),
                () = handle.cancel.notified() => Err(Abort::Cancelled),
                () = state.wait_cancelled() => Err(Abort::Cancelled),
                () = idle => Err(Abort::TimedOut),
            }
        };
        match outcome {
            Err(abort) => {
                let _ = started.child.start_kill();
                Err(abort.kind(timeouts.idle))
            }
            Ok((fed, out, status)) => {
                if let Err(reason) = fed {
                    let _ = started.child.start_kill();
                    return Err(other(reason));
                }
                match status {
                    Ok(status) if status.success() => Ok(String::from_utf8_lossy(&out).into_owned()),
                    Ok(status) => Err(failure(started, status.code()).await),
                    Err(err) => Err(other(err.to_string())),
                }
            }
        }
    })
}

/// Run one short command without input or progress.
fn run_short(
    program: &str,
    spec: &SshHostSpec,
    attempt: Attempt,
    handle: &AttemptHandle,
    remote_command: &str,
) -> Result<String, FailKind> {
    run_remote(program, spec, attempt, handle, remote_command, None, &mut |_| {}, Timeouts::default())
}

/// Look at the remote: OS, home, Gantry's version and lingering.
pub fn probe(program: &str, spec: &SshHostSpec, attempt: Attempt, handle: &AttemptHandle) -> Result<Probe, FailKind> {
    let stdout = run_short(program, spec, attempt, handle, &probe_command(&spec.command))?;
    parse_probe(&stdout).map_err(other)
}

/// The file opened and rewound, its size and its sha256 (lowercase hex).
fn open_with_hash(local: &Path) -> Result<(std::fs::File, u64, String), FailKind> {
    let fail = |err: std::io::Error| other(format!("could not read {}: {err}", local.display()));
    let mut input = std::fs::File::open(local).map_err(fail)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut size = 0u64;
    loop {
        let read = input.read(&mut buffer).map_err(fail)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    input.seek(SeekFrom::Start(0)).map_err(fail)?;
    Ok((input, size, format!("{:x}", hasher.finalize())))
}

/// The remote shell text of an upload: the file goes to `<file>.new` and only replaces `file` when it has the size
/// (and, where `sha256sum` exists, the hash) sent. A cut stream (sshd closes the channel and `cat` sees a clean end)
/// fails the check and removes `<file>.new`. `size` and `hash` are digits and hex only.
fn upload_command(dir: &str, file: &str, size: u64, hash: &str) -> String {
    format!(
        "{{ mkdir -p {dir} && cat > {file}.new; }} || \
         {{ rm -f {file}.new; echo 'the file could not be written on the host' >&2; exit 1; }}; \
         [ \"$(wc -c < {file}.new)\" -eq {size} ] && \
         {{ ! command -v sha256sum >/dev/null 2>&1 || [ \"$(sha256sum < {file}.new | cut -d' ' -f1)\" = {hash} ]; }} && \
         chmod 755 {file}.new && mv -f {file}.new {file} || \
         {{ rm -f {file}.new; echo 'the upload was cut or did not match' >&2; exit 1; }}"
    )
}

/// Send the file `local` to the host's Gantry path, through `<path>.new` so a cut upload never replaces a working
/// binary; a failed or cancelled upload removes `<path>.new` (best effort). `on_progress` gets the bytes sent.
pub fn upload(
    program: &str,
    spec: &SshHostSpec,
    attempt: Attempt,
    handle: &AttemptHandle,
    local: &Path,
    on_progress: &mut dyn FnMut(u64),
) -> Result<(), FailKind> {
    let UploadPath { dir, file } = upload_path(&spec.command).map_err(other)?;
    let (input, size, hash) = open_with_hash(local)?;
    let command = upload_command(&dir, &file, size, &hash);
    let result = run_remote(
        program,
        spec,
        attempt,
        handle,
        &command,
        Some(Box::new(input)),
        on_progress,
        Timeouts { idle: UPLOAD_IDLE },
    );
    if result.is_err() {
        let _ = run_remote(
            program,
            spec,
            Attempt::Background,
            &AttemptHandle::default(),
            &format!("rm -f {file}.new"),
            None,
            &mut |_| {},
            Timeouts { idle: CLEANUP_IDLE },
        );
    }
    result.map(|_| ())
}

/// Run `<command> --version` on the host: its first line, or why it failed.
pub fn verify(program: &str, spec: &SshHostSpec, attempt: Attempt, handle: &AttemptHandle) -> Result<String, FailKind> {
    match run_short(program, spec, attempt, handle, &format!("{} --version", spec.command)) {
        Ok(stdout) => Ok(stdout.lines().next().unwrap_or_default().trim().to_string()),
        Err(error) => Err(with_every_missing_library(program, spec, handle, error)),
    }
}

/// The loader names only the first missing library: when `error` is a Gantry that cannot start, ask `ldd` (over the
/// shared connection, never prompting) for all of them. Anything `ldd` does not name leaves `error` as it was.
fn with_every_missing_library(program: &str, spec: &SshHostSpec, handle: &AttemptHandle, error: FailKind) -> FailKind {
    let FailKind::Other { reason } = &error else { return error };
    if !reason.starts_with("Gantry cannot start on the host") {
        return error;
    }
    let listed = run_short(program, spec, Attempt::Background, handle, &format!("ldd {} 2>&1; true", spec.command));
    match listed.map(|stdout| parse_ldd(&stdout)) {
        Ok(libraries) if !libraries.is_empty() => other(describe_missing_libraries(&libraries)),
        _ => error,
    }
}

/// Turn lingering on for the remote user; the error is what the remote said.
pub fn enable_linger(
    program: &str,
    spec: &SshHostSpec,
    attempt: Attempt,
    handle: &AttemptHandle,
) -> Result<(), String> {
    match run_short(program, spec, attempt, handle, "loginctl enable-linger \"$(id -un)\"") {
        Ok(_) => Ok(()),
        Err(FailKind::Other { reason }) => Err(reason),
        Err(FailKind::NotInstalled) => Err("loginctl was not found on the host".into()),
        Err(FailKind::NeedsInput) => Err("cancelled".into()),
        Err(FailKind::UpdateNeeded { .. }) => Err("unexpected answer".into()),
    }
}

/// Stop the remote daemon (and with it every session there) through `<command> bridge`: `Hello` and the frozen
/// `StopAll`, whatever protocol the daemon speaks, then wait for the bridge to end.
pub fn stop_remote_daemon(
    program: &str,
    spec: &SshHostSpec,
    attempt: Attempt,
    handle: &AttemptHandle,
) -> Result<(), FailKind> {
    runtime()?.block_on(async {
        let mut started = start(program, spec, attempt, &format!("{} bridge", spec.command), true).await?;
        let (Some(mut stdin), Some(mut stdout)) = (started.child.stdin.take(), started.child.stdout.take()) else {
            return Err(other("ssh has no pipes"));
        };
        let state = started.state.clone();
        let answered = std::cell::Cell::new(false);
        let outcome = {
            let work = async {
                // A write error means the bridge is gone: the exit status below says why.
                let _ = write_frame(&mut stdin, &Frame::control(&ClientMessage::Hello { protocol: PROTOCOL_VERSION }))
                    .await;
                let _ = write_frame(&mut stdin, &Frame::control(&ClientMessage::StopAll)).await;
                let _ = stdin.flush().await;
                // Stdin stays open: the bridge ends by itself when the daemon closes the socket.
                while let Ok(Some(_)) = read_frame(&mut stdout).await {
                    answered.set(true);
                }
                drop(stdin);
                started.child.wait().await
            };
            tokio::select! {
                done = work => Ok(done),
                () = handle.cancel.notified() => Err(Abort::Cancelled),
                () = state.wait_cancelled() => Err(Abort::Cancelled),
                () = timeout_without_questions(&state, STOP_IDLE) => Err(Abort::TimedOut),
            }
        };
        match outcome {
            // The daemon answered, so it has the frames and is going down: a slow close is not a failure.
            Err(Abort::TimedOut) if answered.get() => {
                let _ = started.child.start_kill();
                Ok(())
            }
            Err(abort) => {
                let _ = started.child.start_kill();
                Err(abort.kind(STOP_IDLE))
            }
            Ok(_) if answered.get() => Ok(()),
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(status) => Err(failure(started, status.ok().and_then(|status| status.code())).await),
        }
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::upload_command;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// Run the upload script with `sh` in `dir`, feeding `data`; the last line of its stderr.
    fn run(dir: &std::path::Path, setup: &str, data: &[u8], size: u64) -> (bool, String) {
        let file = dir.join("gantryd");
        let command = upload_command(dir.to_str().unwrap(), file.to_str().unwrap(), size, "0");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!("{setup}\n{command}"))
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(data).unwrap();
        let out = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        (out.status.success(), stderr.lines().last().unwrap_or_default().to_string())
    }

    #[test]
    fn a_write_error_is_not_reported_as_a_cut_upload() {
        let dir = tempfile::tempdir().unwrap();
        let setup = format!("mkdir {}/gantryd.new", dir.path().display());
        let (ok, last) = run(dir.path(), &setup, b"abc", 3);
        assert!(!ok);
        assert_eq!(last, "the file could not be written on the host");
    }

    #[test]
    fn a_size_mismatch_still_reads_as_a_cut_upload() {
        let dir = tempfile::tempdir().unwrap();
        let (ok, last) = run(dir.path(), ":", b"abc", 10);
        assert!(!ok);
        assert_eq!(last, "the upload was cut or did not match");
    }
}
