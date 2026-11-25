//! The subcommands that run on a host without a window: `--version`, `daemon`, `bridge`, `preview-mcp` and `internal-print-env`.
//! Shared by `gantry` and the window-less `gantryd` that the SSH wizard installs on remote hosts.

use std::process::ExitCode;
use std::time::Duration;

use daemon::launch::{DaemonOptions, DaemonOutcome, detach_from_terminal, run_daemon};
use daemon::paths::{Paths, current_profile};

/// Run `command` when it is one of the window-less subcommands; `None` for anything else.
pub fn dispatch(command: Option<&str>) -> Option<ExitCode> {
    match command {
        Some("--version") => {
            println!("{}", protocol::build::version_line());
            Some(ExitCode::SUCCESS)
        }
        Some("daemon") => Some(daemon_main()),
        Some("bridge") => Some(bridge_main()),
        Some("preview-mcp") => Some(preview_mcp_main()),
        Some("internal-print-env") => {
            println!("{}", daemon::shell_env::print_env_json());
            Some(ExitCode::SUCCESS)
        }
        _ => None,
    }
}

/// `gantry bridge`: what ssh runs on a remote machine. Joins stdin/stdout to this machine's daemon socket.
/// Stdout carries only protocol bytes, so nothing here logs.
fn bridge_main() -> ExitCode {
    let paths = Paths::for_profile(&current_profile());
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("gantry bridge: {err}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(daemon::bridge::run_bridge(
        &paths,
        daemon::launch::spawn_detached_daemon,
        tokio::io::stdin(),
        tokio::io::stdout(),
    ));
    // Leave now: dropping the runtime would wait for the blocked read of stdin, which keeps the bridge (and its ssh
    // session) alive after the daemon closed the socket.
    match result {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("gantry bridge: {err}");
            std::process::exit(1)
        }
    }
}

/// `gantry preview-mcp`: the MCP tool server Claude Code runs for `show_in_preview`. Stdout carries protocol messages
/// only, so nothing here prints to it.
fn preview_mcp_main() -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => {
            eprintln!("gantry preview-mcp: {err}");
            return ExitCode::FAILURE;
        }
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match daemon::preview_mcp::run(stdin.lock(), stdout.lock(), &cwd, std::env::home_dir().as_deref()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("gantry preview-mcp: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Set on a daemon re-launched by `daemon_main`, so it never re-launches again.
const RESPAWNED_ENV: &str = "GANTRY_DAEMON_RESPAWNED";

fn daemon_main() -> ExitCode {
    match detach_from_terminal() {
        Ok(true) => {}
        // A process group leader (e.g. a shell job) cannot start a session: re-launch as a child that can.
        Ok(false) if std::env::var_os(RESPAWNED_ENV).is_none() => {
            let relaunched = std::env::current_exe().and_then(|exe| {
                std::process::Command::new(exe)
                    .arg("daemon")
                    .env(RESPAWNED_ENV, "1")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
            });
            return match relaunched {
                Ok(_) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("gantry daemon: failed to re-launch detached: {err}");
                    ExitCode::FAILURE
                }
            };
        }
        Ok(false) => {}
        Err(err) => eprintln!("gantry daemon: could not detach from the terminal: {err}"),
    }
    let paths = Paths::for_profile(&current_profile());
    let appender = tracing_appender::rolling::daily(&paths.log_dir, "daemon.log");
    tracing_subscriber::fmt()
        // Tantivy logs every commit and merge at info: keep its warnings only.
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tantivy=warn".into()),
        )
        .with_writer(appender)
        .with_ansi(false)
        .init();
    match run_daemon(DaemonOptions {
        paths,
        base_env: None,
        idle_timeout: Duration::from_secs(10),
        claude_dir: None,
        docker: Some("docker".into()),
    }) {
        Ok(DaemonOutcome::Stopped | DaemonOutcome::AlreadyRunning) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("daemon failed: {err:#}");
            ExitCode::FAILURE
        }
    }
}
