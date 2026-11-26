//! The files Claude sent to Gantry's preview pane, read from a session transcript: a `show_in_preview` tool call and
//! its result, from a byte offset up to the last complete line.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

/// The name Claude Code gives the preview server's tool.
pub const TOOL_NAME: &str = "mcp__plugin_gantry_preview__show_in_preview";

/// How many sent files are kept per session: the newest.
pub const KEPT: usize = 50;

/// The first line of a `show_in_preview` result that shows files; `- <absolute path>` lines follow it.
///
/// The result text format is frozen: the daemon that reads a transcript can be older than the `preview-mcp` that
/// wrote it (a reinstall replaces the binary while the old daemon keeps running), so a change must stay readable by
/// every earlier reader.
pub const SHOWN_HEADER: &str = "Shown in preview:";

/// One file Claude sent to the preview pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentFileRecord {
    /// `<tool_use_id>:<index>`, the index being the file's place in the result's accepted list.
    pub id: String,
    /// The absolute path the server resolved and accepted (not as Claude gave it).
    pub path: PathBuf,
    /// When the result was written, in milliseconds since the Unix epoch.
    pub sent_ms: u64,
    /// The caption Claude gave the call, if any.
    pub caption: Option<String>,
    /// Found by the reader's first read of this transcript, not sent since; stays set.
    pub backlog: bool,
}

/// Reads sent files out of one transcript, keeping its place between calls.
#[derive(Debug, Default)]
pub struct SentFilesReader {
    offset: u64,
    /// Whether a read has finished since the reader was made or reset: later finds are new, earlier ones backlog.
    scanned: bool,
    /// Open calls (tool use id -> caption) waiting for their result.
    pending: HashMap<String, Option<String>>,
    files: Vec<SentFileRecord>,
}

impl SentFilesReader {
    /// A reader that has seen nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads `transcript` from the saved offset to its last complete line; returns whether the list changed. A file
    /// that shrank was rewritten, so the reader starts over.
    pub fn read_new(&mut self, transcript: &Path) -> io::Result<bool> {
        let file = File::open(transcript)?;
        let meta = file.metadata()?;
        let mut changed = false;
        if meta.len() < self.offset {
            self.offset = 0;
            self.scanned = false;
            self.pending.clear();
            changed = !self.files.is_empty();
            self.files.clear();
        }
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_millis() as u64);
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(self.offset))?;
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 || line.last() != Some(&b'\n') {
                break;
            }
            self.offset += read as u64;
            if let Ok(record) = serde_json::from_slice::<Value>(&line) {
                changed |= self.apply(&record, mtime_ms, !self.scanned);
            }
        }
        self.scanned = true;
        if self.files.len() > KEPT {
            self.files.drain(..self.files.len() - KEPT);
        }
        Ok(changed)
    }

    /// The sent files, oldest first, at most [`KEPT`].
    pub fn files(&self) -> &[SentFileRecord] {
        &self.files
    }

    /// Applies one record; returns whether files were added.
    fn apply(&mut self, record: &Value, mtime_ms: u64, backlog: bool) -> bool {
        let Some(blocks) = record.get("message").and_then(|m| m.get("content")).and_then(Value::as_array) else {
            return false;
        };
        let mut added = false;
        match record.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && block.get("name").and_then(Value::as_str) == Some(TOOL_NAME)
                        && let Some(id) = block.get("id").and_then(Value::as_str)
                    {
                        let caption = block.get("input").and_then(|i| i.get("caption")).and_then(Value::as_str);
                        self.pending.insert(id.to_string(), caption.map(str::to_string));
                    }
                }
            }
            Some("user") => {
                let sent_ms =
                    record.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_ms).unwrap_or(mtime_ms);
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else { continue };
                    let Some(caption) = self.pending.remove(id) else { continue };
                    if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                        continue;
                    }
                    for (index, path) in accepted_paths(&result_text(block)).into_iter().enumerate() {
                        self.files.push(SentFileRecord {
                            id: format!("{id}:{index}"),
                            path: PathBuf::from(path),
                            sent_ms,
                            caption: caption.clone(),
                            backlog,
                        });
                        added = true;
                    }
                }
            }
            _ => {}
        }
        added
    }
}

/// The text of a tool result: a string, or the text blocks of an array joined by newlines.
fn result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text")?.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The `- ` lines under the `Shown in preview:` line, up to the next line that is not one.
fn accepted_paths(text: &str) -> Vec<&str> {
    text.lines()
        .skip_while(|line| line.trim() != SHOWN_HEADER)
        .skip(1)
        .map_while(|line| line.strip_prefix("- "))
        .collect()
}

/// `YYYY-MM-DDTHH:MM:SS[.fff]Z` as milliseconds since the Unix epoch.
fn parse_rfc3339_ms(text: &str) -> Option<u64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut clock = clock.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let millis: i64 =
        if fraction.is_empty() { 0 } else { format!("{:0<3}", fraction.get(..3).unwrap_or(fraction)).parse().ok()? };
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = ((days * 24 + hour) * 60 + minute) * 60 + second;
    u64::try_from(secs * 1000 + millis).ok()
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;

    use serde_json::json;

    use super::*;

    fn call(id: &str, name: &str, input: Value) -> String {
        let record = json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": id, "name": name, "input": input}]}});
        format!("{record}\n")
    }

    fn result(id: &str, content: Value, is_error: bool) -> String {
        let record = json!({"type": "user", "timestamp": "2026-11-25T10:00:00.250Z", "message": {"content": [
            {"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error}]}});
        format!("{record}\n")
    }

    fn text(content: &str) -> Value {
        json!([{"type": "text", "text": content}])
    }

    fn append(path: &Path, data: &str) {
        let mut file = OpenOptions::new().create(true).append(true).open(path).unwrap();
        file.write_all(data.as_bytes()).unwrap();
    }

    fn read(data: &str) -> Vec<SentFileRecord> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, data);
        let mut reader = SentFilesReader::new();
        reader.read_new(&path).unwrap();
        reader.files().to_vec()
    }

    #[test]
    fn a_call_and_its_result_make_sent_files() {
        let data = call("toolu_1", TOOL_NAME, json!({"paths": ["a.md", "b.png"], "caption": "look"}))
            + &result("toolu_1", text("Shown in preview:\n- /w/a.md\n- /w/b.png"), false);
        let files = read(&data);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].id, "toolu_1:0");
        assert_eq!(files[0].path, PathBuf::from("/w/a.md"));
        assert_eq!(files[1].id, "toolu_1:1");
        assert_eq!(files[1].path, PathBuf::from("/w/b.png"));
        assert_eq!(files[0].caption.as_deref(), Some("look"));
        assert_eq!(files[0].sent_ms, 1_795_600_800_250);
    }

    #[test]
    fn files_of_the_first_read_are_backlog_and_later_ones_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let send =
            |id: &str| call(id, TOOL_NAME, json!({})) + &result(id, text(&format!("{SHOWN_HEADER}\n- /w/a.md")), false);
        append(&path, &send("toolu_1"));
        let mut reader = SentFilesReader::new();
        reader.read_new(&path).unwrap();
        assert!(reader.files()[0].backlog);
        append(&path, &send("toolu_2"));
        reader.read_new(&path).unwrap();
        assert!(reader.files()[0].backlog);
        assert!(!reader.files()[1].backlog);
        // A rewritten transcript is read from the start again: what it holds is backlog.
        std::fs::write(&path, send("toolu_3")).unwrap();
        reader.read_new(&path).unwrap();
        assert_eq!(reader.files().len(), 1);
        assert!(reader.files()[0].backlog);
    }

    #[test]
    fn an_empty_first_read_does_not_make_later_sends_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, "");
        let mut reader = SentFilesReader::new();
        reader.read_new(&path).unwrap();
        append(
            &path,
            &(call("toolu_1", TOOL_NAME, json!({})) + &result("toolu_1", text("Shown in preview:\n- /w/a.md"), false)),
        );
        reader.read_new(&path).unwrap();
        assert!(!reader.files()[0].backlog);
    }

    #[test]
    fn an_error_result_adds_nothing() {
        let data = call("toolu_1", TOOL_NAME, json!({"paths": ["x.md"]}))
            + &result("toolu_1", json!("Rejected:\n- x.md: not found (/w/x.md)"), true);
        assert!(read(&data).is_empty());
    }

    #[test]
    fn a_permission_denial_adds_nothing() {
        let denial = json!("Claude requested permissions to use the tool, but you haven't granted it yet.");
        let data = call("toolu_1", TOOL_NAME, json!({"paths": ["x.md"]})) + &result("toolu_1", denial, true);
        assert!(read(&data).is_empty());
    }

    #[test]
    fn a_call_without_a_result_yet_adds_nothing_until_it_arrives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, &call("toolu_1", TOOL_NAME, json!({"paths": ["a.md"]})));
        let mut reader = SentFilesReader::new();
        assert!(!reader.read_new(&path).unwrap());
        assert!(reader.files().is_empty());
        append(&path, &result("toolu_1", text("Shown in preview:\n- /w/a.md"), false));
        assert!(reader.read_new(&path).unwrap());
        assert_eq!(reader.files().len(), 1);
    }

    #[test]
    fn a_line_without_its_newline_waits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, &call("toolu_1", TOOL_NAME, json!({"paths": ["a.md"]})));
        let line = result("toolu_1", text("Shown in preview:\n- /w/a.md"), false);
        append(&path, line.trim_end());
        let mut reader = SentFilesReader::new();
        assert!(!reader.read_new(&path).unwrap());
        assert!(reader.files().is_empty());
        append(&path, "\n");
        assert!(reader.read_new(&path).unwrap());
        assert_eq!(reader.files().len(), 1);
    }

    #[test]
    fn rejected_paths_in_a_mixed_result_are_left_out() {
        let mixed = "Shown in preview:\n- /w/a.md\nRejected:\n- b.md: not found (/w/b.md)";
        let data =
            call("toolu_1", TOOL_NAME, json!({"paths": ["a.md", "b.md"]})) + &result("toolu_1", text(mixed), false);
        let files = read(&data);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, PathBuf::from("/w/a.md"));
    }

    #[test]
    fn paths_with_spaces_and_unicode_survive() {
        let data = call("toolu_1", TOOL_NAME, json!({"path": "my notes/ü.md"}))
            + &result("toolu_1", text("Shown in preview:\n- /w/my notes/ü.md"), false);
        assert_eq!(read(&data)[0].path, PathBuf::from("/w/my notes/ü.md"));
    }

    #[test]
    fn other_tools_are_ignored() {
        let data = call("toolu_1", "Write", json!({"file_path": "/w/a.md"}))
            + &result("toolu_1", text("Shown in preview:\n- /w/a.md"), false);
        assert!(read(&data).is_empty());
    }

    #[test]
    fn only_the_newest_fifty_are_kept() {
        let mut data = String::new();
        for n in 1..=55 {
            let id = format!("toolu_{n}");
            data += &call(&id, TOOL_NAME, json!({"paths": ["a.md"]}));
            data += &result(&id, text("Shown in preview:\n- /w/a.md"), false);
        }
        let files = read(&data);
        assert_eq!(files.len(), KEPT);
        assert_eq!(files[0].id, "toolu_6:0");
        assert_eq!(files[49].id, "toolu_55:0");
    }

    #[test]
    fn a_missing_timestamp_falls_back_to_the_file_time() {
        let record = json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "Shown in preview:\n- /w/a.md"}]}});
        let data = call("toolu_1", TOOL_NAME, json!({})) + &format!("{record}\n");
        assert!(read(&data)[0].sent_ms > 1_700_000_000_000);
    }

    #[test]
    fn a_shrunk_file_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let first =
            call("toolu_1", TOOL_NAME, json!({})) + &result("toolu_1", text("Shown in preview:\n- /w/a.md"), false);
        append(&path, &first);
        let mut reader = SentFilesReader::new();
        reader.read_new(&path).unwrap();
        std::fs::write(&path, "{}\n").unwrap();
        assert!(reader.read_new(&path).unwrap());
        assert!(reader.files().is_empty());
    }
}
