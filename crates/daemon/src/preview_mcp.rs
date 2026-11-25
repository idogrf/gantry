//! The `preview-mcp` tool server: a stdio MCP server giving Claude Code a `show_in_preview` tool.
//!
//! Newline-delimited JSON-RPC 2.0 on stdin/stdout. Stdout carries protocol messages only, so nothing here logs.
//! The tool result text is read back out of the transcript, so its format is a contract: `Shown in preview:`, then
//! lines `- <absolute path>`, then optionally `Rejected:` and its lines.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The one tool's description, as Claude sees it.
const TOOL_DESCRIPTION: &str = "Show one or more files to the user in Gantry's preview pane beside the terminal.";

/// The most paths one call may show.
const MAX_PATHS: usize = 20;

/// Answer one JSON-RPC message; `None` for a notification (no `id`).
pub fn handle(message: &Value, cwd: &Path, home: Option<&Path>) -> Option<Value> {
    let id = message.get("id")?.clone();
    let result = match message.get("method").and_then(Value::as_str) {
        Some("initialize") => json!({
            "protocolVersion": message["params"]["protocolVersion"].clone(),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "gantry-preview", "version": protocol::build::VERSION},
        }),
        Some("tools/list") => json!({"tools": [tool()]}),
        Some("tools/call") => call(&message["params"], cwd, home),
        Some("ping") => json!({}),
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "Method not found"},
            }));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Read newline-delimited JSON-RPC messages from `input` until EOF and write each answer to `output`.
pub fn run(input: impl BufRead, mut output: impl Write, cwd: &Path, home: Option<&Path>) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(response) = handle(&message, cwd, home) {
            writeln!(output, "{response}")?;
            output.flush()?;
        }
    }
    Ok(())
}

fn tool() -> Value {
    json!({
        "name": "show_in_preview",
        "description": TOOL_DESCRIPTION,
        "inputSchema": {
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 1,
                    "maxItems": MAX_PATHS,
                    "description": "File paths, absolute or relative to the session's working directory.",
                },
                "caption": {"type": "string", "description": "Optional short caption."},
            },
            "required": ["paths"],
        },
    })
}

/// The paths a call asks for: `paths` when it is a valid list, else a string `path`, else a string `file_path`.
fn requested_paths(arguments: &Value) -> Option<Vec<&str>> {
    if let Some(list) = arguments.get("paths").and_then(Value::as_array) {
        let strings: Option<Vec<&str>> = list.iter().map(Value::as_str).collect();
        if let Some(strings) = strings.filter(|strings| (1..=MAX_PATHS).contains(&strings.len())) {
            return Some(strings);
        }
    }
    ["path", "file_path"].iter().find_map(|key| arguments.get(*key).and_then(Value::as_str)).map(|path| vec![path])
}

/// `given` as an absolute path: `~` through `home`, a relative path against `cwd`. Nothing is canonicalized.
fn resolve(given: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let expanded = match (home, given.strip_prefix('~')) {
        (Some(home), Some("")) => home.to_path_buf(),
        (Some(home), Some(rest)) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(given),
    };
    cwd.join(expanded)
}

fn call(params: &Value, cwd: &Path, home: Option<&Path>) -> Value {
    let Some(requested) = requested_paths(&params["arguments"]) else {
        return tool_result("paths must be a list of 1-20 strings", true);
    };
    let mut shown = Vec::new();
    let mut rejected = Vec::new();
    for given in requested {
        let path = resolve(given, cwd, home);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => shown.push(path),
            Ok(_) => rejected.push(format!("- {given}: not a file ({})", path.display())),
            Err(_) => rejected.push(format!("- {given}: not found ({})", path.display())),
        }
    }
    let mut sections = Vec::new();
    if !shown.is_empty() {
        let lines: Vec<String> = shown.iter().map(|path| format!("- {}", path.display())).collect();
        sections.push(format!("Shown in preview:\n{}", lines.join("\n")));
    }
    if !rejected.is_empty() {
        sections.push(format!("Rejected:\n{}", rejected.join("\n")));
    }
    tool_result(&sections.join("\n"), shown.is_empty())
}

fn tool_result(text: &str, is_error: bool) -> Value {
    if is_error {
        json!({"content": [{"type": "text", "text": text}], "isError": true})
    } else {
        json!({"content": [{"type": "text", "text": text}]})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Env {
        cwd: tempfile::TempDir,
        home: tempfile::TempDir,
    }

    fn env() -> Env {
        let cwd = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        fs::write(cwd.path().join("a.md"), "a").unwrap();
        fs::create_dir(cwd.path().join("sub dir")).unwrap();
        fs::write(cwd.path().join("sub dir/ü.png"), "p").unwrap();
        fs::create_dir(cwd.path().join("d")).unwrap();
        fs::write(home.path().join("x.md"), "x").unwrap();
        Env { cwd, home }
    }

    fn call(env: &Env, arguments: Value) -> Value {
        let message = json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name":"show_in_preview","arguments":arguments}});
        handle(&message, env.cwd.path(), Some(env.home.path())).unwrap()
    }

    fn text(response: &Value) -> String {
        response["result"]["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn is_error(response: &Value) -> bool {
        response["result"]["isError"].as_bool().unwrap_or(false)
    }

    #[test]
    fn initialize_echoes_the_protocol_version() {
        let env = env();
        let message = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}});
        let response = handle(&message, env.cwd.path(), None).unwrap();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(response["result"]["capabilities"], json!({"tools": {}}));
        assert_eq!(response["result"]["serverInfo"]["name"], "gantry-preview");
        assert_eq!(response["result"]["serverInfo"]["version"], protocol::build::VERSION);
    }

    #[test]
    fn tools_list_has_the_one_tool() {
        let env = env();
        let message = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
        let response = handle(&message, env.cwd.path(), None).unwrap();
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "show_in_preview");
        assert_eq!(
            tools[0]["description"],
            "Show one or more files to the user in Gantry's preview pane beside the terminal."
        );
        assert_eq!(tools[0]["inputSchema"]["required"], json!(["paths"]));
        assert_eq!(tools[0]["inputSchema"]["properties"]["paths"]["maxItems"], 20);
    }

    #[test]
    fn relative_and_home_paths_resolve() {
        let env = env();
        let response = call(&env, json!({"paths": ["a.md", "~/x.md"]}));
        assert!(!is_error(&response));
        let expected = format!(
            "Shown in preview:\n- {}\n- {}",
            env.cwd.path().join("a.md").display(),
            env.home.path().join("x.md").display()
        );
        assert_eq!(text(&response), expected);
    }

    #[test]
    fn missing_and_folder_paths_are_rejected_with_reasons() {
        let env = env();
        let response = call(&env, json!({"paths": ["a.md", "nope.md", "d"]}));
        assert!(!is_error(&response));
        let cwd = env.cwd.path().display();
        let expected = format!(
            "Shown in preview:\n- {cwd}/a.md\nRejected:\n- nope.md: not found ({cwd}/nope.md)\n- d: not a file ({cwd}/d)"
        );
        assert_eq!(text(&response), expected);
    }

    #[test]
    fn all_missing_is_an_error() {
        let env = env();
        let response = call(&env, json!({"paths": ["nope.md"]}));
        assert!(is_error(&response));
        assert!(text(&response).contains("Rejected:\n- nope.md: not found"));
        assert!(!text(&response).contains("Shown in preview:"));
    }

    #[test]
    fn path_and_file_path_aliases_are_accepted() {
        let env = env();
        let response = call(&env, json!({"path": "a.md"}));
        assert!(!is_error(&response));
        assert!(text(&response).contains(&format!("- {}", env.cwd.path().join("a.md").display())));
        let abs = env.cwd.path().join("sub dir/ü.png");
        let response = call(&env, json!({"file_path": abs.to_str().unwrap()}));
        assert!(!is_error(&response));
        assert!(text(&response).contains(&format!("- {}", abs.display())));
    }

    #[test]
    fn bad_paths_argument_is_an_error() {
        let env = env();
        for arguments in [json!({"paths": []}), json!({"paths": "a.md"}), json!({})] {
            let response = call(&env, arguments);
            assert!(is_error(&response));
            assert_eq!(text(&response), "paths must be a list of 1-20 strings");
        }
    }

    #[test]
    fn notifications_get_no_answer() {
        let env = env();
        let message = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert!(handle(&message, env.cwd.path(), None).is_none());
    }

    #[test]
    fn unknown_method_with_an_id_is_an_error() {
        let env = env();
        let message = json!({"jsonrpc":"2.0","id":3,"method":"nope/nope"});
        let response = handle(&message, env.cwd.path(), None).unwrap();
        assert_eq!(response["error"]["code"], -32601);
        assert_eq!(response["id"], 3);
    }

    #[test]
    fn run_answers_each_line() {
        let env = env();
        let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n\
                     {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\
                     {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"x\"}}\n";
        let mut output = Vec::new();
        run(input.as_bytes(), &mut output, env.cwd.path(), None).unwrap();
        let lines: Vec<Value> =
            String::from_utf8(output).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["id"], 1);
        assert_eq!(lines[1]["id"], 2);
    }
}
