//! The installed-version check: what the daemon runs, the binary this window started from, and the footer lines.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use app_core::version::{Version, parse_version_output};
use gpui::Global;
#[cfg(target_os = "linux")]
use gpui::{Context, Task};
#[cfg(target_os = "linux")]
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

#[cfg(target_os = "linux")]
use crate::workspace_view::WorkspaceView;

/// The connected daemon's build, as its `Welcome` said.
#[derive(Clone, Debug, Default)]
pub(crate) struct DaemonBuild {
    /// App version, if the daemon sent one.
    pub version: Option<String>,
    /// Git commit, if the daemon sent one.
    pub commit: Option<String>,
}

impl Global for DaemonBuild {}

/// The Settings footer: this window's build, then the daemon's.
pub(crate) fn footer_lines(daemon: Option<&DaemonBuild>) -> [String; 2] {
    let app = format!("Gantry {} ({})", protocol::build::VERSION, protocol::build::COMMIT);
    let daemon = match daemon.and_then(|daemon| daemon.version.as_deref().map(|version| (version, daemon))) {
        Some((version, daemon)) => format!("daemon {version} ({})", daemon.commit.as_deref().unwrap_or("unknown")),
        None => "daemon version unknown".into(),
    };
    [app, daemon]
}

/// Quiet time after the last change before probing: installers replace the file in several steps.
#[cfg(target_os = "linux")]
pub(crate) const SETTLE: Duration = Duration::from_millis(500);

/// How long `--version` may take.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The version `path --version` prints, or `None` (not runnable, no answer within `timeout`, not a version).
pub(crate) fn probe(path: &Path, timeout: Duration) -> Option<Version> {
    let mut child = claude_index::process::hide_window(&mut Command::new(path))
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    parse_version_output(&output)
}

/// Polls until the daemon's lock file can be locked (the daemon holds it for as long as it runs), up to `timeout`.
pub(crate) fn wait_for_lock_free(path: &Path, timeout: Duration) -> bool {
    let started = Instant::now();
    loop {
        // Read access too: Windows locks only through a handle that may read or write, which append alone is not.
        let free = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
            .is_ok_and(|file| file.try_lock().is_ok());
        if free {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether a file event means the file changed: running `--version` on it opens and closes it for reading, which must
/// not count (it would probe again forever).
#[cfg(target_os = "linux")]
fn is_change(kind: &notify::EventKind) -> bool {
    use notify::EventKind;
    use notify::event::{AccessKind, AccessMode};
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(_)
            | EventKind::Remove(_)
            | EventKind::Access(AccessKind::Close(AccessMode::Write))
    )
}

/// Size and modification time: what tells one install from the next.
#[cfg(target_os = "linux")]
fn stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// Watch `path`'s directory; after changes to `path` settle, probe it off the UI thread and report the version.
#[cfg(target_os = "linux")]
pub(crate) fn watch(path: &Path, cx: &mut Context<WorkspaceView>) -> Option<(RecommendedWatcher, Task<()>)> {
    let dir = path.parent()?;
    let name = path.file_name()?.to_owned();
    let (events, changes) = async_channel::unbounded::<()>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event
            && is_change(&event.kind)
            && event.paths.iter().any(|path| path.file_name() == Some(name.as_os_str()))
        {
            let _ = events.try_send(());
        }
    })
    .inspect_err(|error| tracing::warn!("installed binary not watched: {error}"))
    .ok()?;
    watcher
        .watch(dir, RecursiveMode::NonRecursive)
        .inspect_err(|error| tracing::warn!("installed binary not watched: {error}"))
        .ok()?;
    let path = path.to_owned();
    let task = cx.spawn(async move |this, cx| {
        let mut last = None;
        while changes.recv().await.is_ok() {
            loop {
                cx.background_executor().timer(SETTLE).await;
                let mut more = false;
                while changes.try_recv().is_ok() {
                    more = true;
                }
                if !more {
                    break;
                }
            }
            // One install probes once: the same size and time as the last good probe is the same file.
            let current = stamp(&path);
            if current.is_none() || current == last {
                continue;
            }
            let probed = path.clone();
            let found = cx.background_executor().spawn(async move { probe(&probed, PROBE_TIMEOUT) }).await;
            if let Some(version) = found {
                last = current;
                if this.update(cx, |view, cx| view.installed_version(version, cx)).is_err() {
                    return;
                }
            }
        }
    });
    Some((watcher, task))
}

/// Starts the GUI at `path`, detached; `--resume-sessions` when the sessions were stopped for the update.
pub(crate) fn restart_command(path: &Path, resume_sessions: bool) -> Command {
    let mut command = Command::new(path);
    if resume_sessions {
        command.arg("--resume-sessions");
    }
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_names_both_versions() {
        let daemon = DaemonBuild { version: Some("0.2.0".into()), commit: Some("fc3d94a".into()) };
        let [app, daemon_line] = footer_lines(Some(&daemon));
        assert_eq!(app, format!("Gantry {} ({})", protocol::build::VERSION, protocol::build::COMMIT));
        assert_eq!(daemon_line, "daemon 0.2.0 (fc3d94a)");
    }

    #[test]
    fn footer_without_a_daemon_version() {
        let none = DaemonBuild { version: None, commit: None };
        assert_eq!(footer_lines(Some(&none))[1], "daemon version unknown");
        assert_eq!(footer_lines(None)[1], "daemon version unknown");
        let no_commit = DaemonBuild { version: Some("0.3.0".into()), commit: None };
        assert_eq!(footer_lines(Some(&no_commit))[1], "daemon 0.3.0 (unknown)");
    }

    #[test]
    fn lock_wait_ends_when_the_holder_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        let holder = std::fs::File::create(&path).unwrap();
        holder.lock().unwrap();
        assert!(!wait_for_lock_free(&path, Duration::from_millis(150)));
        drop(holder);
        assert!(wait_for_lock_free(&path, Duration::from_millis(150)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_changes_count_not_opens_or_reads() {
        use notify::EventKind::{Access, Create, Modify, Remove};
        use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode};
        assert!(!is_change(&Access(AccessKind::Open(AccessMode::Any))));
        assert!(!is_change(&Access(AccessKind::Close(AccessMode::Read))));
        assert!(is_change(&Create(CreateKind::File)));
        assert!(is_change(&Modify(ModifyKind::Data(DataChange::Any))));
        assert!(is_change(&Modify(ModifyKind::Name(RenameMode::Both))));
        assert!(is_change(&Remove(RemoveKind::File)));
        assert!(is_change(&Access(AccessKind::Close(AccessMode::Write))));
    }

    #[cfg(unix)]
    fn script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn probe_reads_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path(), "gantry", "echo 'gantry 9.9.9 (abc1234)'");
        assert_eq!(probe(&path, PROBE_TIMEOUT), app_core::version::Version::parse("9.9.9"));
    }

    #[cfg(unix)]
    #[test]
    fn probe_ignores_garbage_missing_files_and_hangs() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = script(dir.path(), "garbage", "echo 'not a version'");
        assert_eq!(probe(&garbage, PROBE_TIMEOUT), None);
        assert_eq!(probe(&dir.path().join("missing"), PROBE_TIMEOUT), None);
        let hang = script(dir.path(), "hang", "sleep 30");
        let started = std::time::Instant::now();
        assert_eq!(probe(&hang, std::time::Duration::from_millis(300)), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }

    #[test]
    fn restart_command_passes_the_resume_flag_only_when_asked() {
        let path = std::path::Path::new("/opt/gantry");
        let args = |resume| restart_command(path, resume).get_args().map(|a| a.to_owned()).collect::<Vec<_>>();
        assert_eq!(args(true), vec![std::ffi::OsString::from("--resume-sessions")]);
        assert!(args(false).is_empty());
        assert_eq!(restart_command(path, false).get_program(), path.as_os_str());
    }
}
