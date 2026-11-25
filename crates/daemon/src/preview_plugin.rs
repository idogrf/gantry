//! The Gantry Claude Code plugin: the folder Claude panes load with `--plugin-dir` so they get the preview tool.
//!
//! The plugin lives under the daemon's data dir and is never written into a Claude config dir.

use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use claude_index::sent_files::TOOL_NAME;
use serde_json::json;

/// Added to the system prompt of a preview Claude pane.
pub const INSTRUCTION: &str = "This session runs inside Gantry, a terminal app with a preview pane beside this terminal. You have a tool, mcp__plugin_gantry_preview__show_in_preview (input: {\"paths\": [\"<path>\", ...], \"caption\": \"<optional>\"}), that shows files to the user in that pane. Call it, without being asked, once a file meant for the user to look at is complete: after writing a plan, spec, design note, report or other document for the user to read; after making a screenshot, chart, diagram or other image. Do not call it for source files you edit as part of a task, or for scratch files.";

/// The oldest Claude Code that takes the plugin flags.
pub const MIN_CLAUDE: (u32, u32, u32) = (2, 1, 332);

/// Write the plugin into `dir`: its manifest and the MCP server entry that runs `<exe> preview-mcp`. A file whose
/// content already matches is left alone.
pub fn write_plugin(dir: &Path, exe: &Path) -> io::Result<()> {
    let manifest = json!({
        "name": "gantry",
        "version": protocol::build::VERSION,
        "description": "Lets Claude show files in Gantry's preview pane.",
    });
    let mcp = json!({"mcpServers": {"preview": {"command": exe.to_string_lossy(), "args": ["preview-mcp"]}}});
    write_if_changed(&dir.join(".claude-plugin").join("plugin.json"), &manifest.to_string())?;
    write_if_changed(&dir.join(".mcp.json"), &mcp.to_string())
}

fn write_if_changed(path: &Path, content: &str) -> io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|current| current == content) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
}

/// The extra arguments of a preview Claude pane: load the plugin at `dir`, allow its tool, add the instruction.
pub fn preview_args(dir: &Path) -> Vec<String> {
    vec![
        "--plugin-dir".into(),
        dir.to_string_lossy().into_owned(),
        "--allowedTools".into(),
        TOOL_NAME.into(),
        "--append-system-prompt".into(),
        INSTRUCTION.into(),
    ]
}

/// The first `X.Y.Z` in `output` (`2.1.332 (Claude Code)` gives `(2, 1, 332)`).
pub fn parse_version(output: &str) -> Option<(u32, u32, u32)> {
    output.split(|c: char| !(c.is_ascii_digit() || c == '.')).find_map(|word| {
        let mut parts = word.split('.');
        let version = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
        parts.next().is_none().then_some(version)
    })
}

/// Run `<program> --version` with `env` as its whole environment and read the version it prints. `None` when it does
/// not start, prints no version or has not finished after `timeout` (then it is killed).
pub fn probe_version(program: &str, env: &[(String, String)], timeout: Duration) -> Option<(u32, u32, u32)> {
    let mut child = Command::new(program)
        .arg("--version")
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    parse_version(&output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_plugin_writes_both_files() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path(), Path::new("/opt/gantryd")).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(".claude-plugin/plugin.json")).unwrap())
                .unwrap();
        assert_eq!(
            manifest,
            json!({
                "name": "gantry",
                "version": protocol::build::VERSION,
                "description": "Lets Claude show files in Gantry's preview pane.",
            })
        );
        let mcp: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(mcp, json!({"mcpServers": {"preview": {"command": "/opt/gantryd", "args": ["preview-mcp"]}}}));
    }

    #[test]
    fn write_plugin_leaves_equal_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path(), Path::new("/opt/gantryd")).unwrap();
        let files = [dir.path().join(".claude-plugin/plugin.json"), dir.path().join(".mcp.json")];
        let before: Vec<_> = files.iter().map(|f| std::fs::metadata(f).unwrap().modified().unwrap()).collect();
        std::thread::sleep(Duration::from_millis(30));
        write_plugin(dir.path(), Path::new("/opt/gantryd")).unwrap();
        let after: Vec<_> = files.iter().map(|f| std::fs::metadata(f).unwrap().modified().unwrap()).collect();
        assert_eq!(before, after);
        write_plugin(dir.path(), Path::new("/opt/other")).unwrap();
        assert!(std::fs::read_to_string(&files[1]).unwrap().contains("/opt/other"));
    }

    #[test]
    fn preview_args_are_exact() {
        assert_eq!(
            preview_args(Path::new("/p/claude-plugin")),
            vec![
                "--plugin-dir",
                "/p/claude-plugin",
                "--allowedTools",
                "mcp__plugin_gantry_preview__show_in_preview",
                "--append-system-prompt",
                "This session runs inside Gantry, a terminal app with a preview pane beside this terminal. You have a tool, mcp__plugin_gantry_preview__show_in_preview (input: {\"paths\": [\"<path>\", ...], \"caption\": \"<optional>\"}), that shows files to the user in that pane. Call it, without being asked, once a file meant for the user to look at is complete: after writing a plan, spec, design note, report or other document for the user to read; after making a screenshot, chart, diagram or other image. Do not call it for source files you edit as part of a task, or for scratch files.",
            ]
        );
    }

    #[test]
    fn parse_version_reads_claude_output() {
        assert_eq!(parse_version("2.1.332 (Claude Code)"), Some((2, 1, 332)));
        assert_eq!(parse_version("claude 2.0.9"), Some((2, 0, 9)));
        assert_eq!(parse_version("no version"), None);
    }

    // Needs a shell script to stand in for a program that hangs.
    #[cfg(unix)]
    #[test]
    fn probe_gives_up_after_the_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("slow");
        std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let start = Instant::now();
        let env = vec![("PATH".to_string(), std::env::var("PATH").unwrap_or_default())];
        assert_eq!(probe_version(script.to_str().unwrap(), &env, Duration::from_millis(300)), None);
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
