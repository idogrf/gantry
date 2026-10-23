//! Gantry — a terminal for Claude Code sessions.
// Release builds open no console window on Windows (and closing one would end Gantry); `gantryd` keeps the console
// for `--version` in a terminal.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod args;

mod headless;

use std::process::ExitCode;

use args::LaunchArgs;

fn main() -> ExitCode {
    let mut argv = std::env::args().skip(1).peekable();
    // ssh runs `$SSH_ASKPASS "<prompt>"`: only an attempt started by Gantry sets the socket, and then this is the helper.
    if std::env::var_os("GANTRY_ASKPASS_SOCKET").is_some() && argv.peek().map(String::as_str) != Some("askpass") {
        return askpass_main(std::iter::once(String::new()).chain(argv));
    }
    if argv.peek().map(String::as_str) == Some("askpass") {
        return askpass_main(argv);
    }
    if let Some(code) = headless::dispatch(argv.peek().map(String::as_str)) {
        return code;
    }
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let parsed = LaunchArgs::parse(argv);
    let resume_sessions = parsed.as_ref().is_ok_and(|args| args.resume_sessions);
    let launch = match parsed.and_then(|a| a.into_launch().map_err(|e| e.to_string())) {
        Ok(launch) => launch,
        Err(message) => {
            eprintln!("gantry: {message}");
            return ExitCode::from(2);
        }
    };
    ui_gpui::run(launch, resume_sessions);
    ExitCode::SUCCESS
}

/// `gantry <prompt>` with `GANTRY_ASKPASS_SOCKET` set (or `gantry askpass <prompt>`): ssh's `SSH_ASKPASS` helper. Asks the GUI that started this attempt and prints the
/// answer; the answer is never logged.
fn askpass_main(mut argv: impl Iterator<Item = String>) -> ExitCode {
    let _ = argv.next();
    let prompt = argv.next().unwrap_or_default();
    let Some(socket) = std::env::var_os("GANTRY_ASKPASS_SOCKET") else {
        eprintln!("gantry askpass: only ssh attempts started by Gantry can ask");
        return ExitCode::FAILURE;
    };
    match daemon::askpass::run_helper(std::path::Path::new(&socket), &prompt) {
        Ok(Some(answer)) => {
            println!("{answer}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("gantry askpass: {err}");
            ExitCode::FAILURE
        }
    }
}
