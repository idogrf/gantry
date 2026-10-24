//! Summaries of Claude transcripts (`projects/<folder>/<session id>.jsonl`), read from the file's head
//! (directory, first prompt) and tail (latest titles) only — transcripts can be hundreds of megabytes.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Bytes read from the start of a transcript looking for its directory and first prompt.
const HEAD_LIMIT: u64 = 1 << 20;
/// Bytes read from the end looking for titles; Claude re-appends its title records as it writes.
const TAIL_LIMIT: u64 = 256 << 10;
/// Longest title kept, in characters.
const MAX_TITLE_CHARS: usize = 120;
/// Longest last prompt kept, in characters.
const MAX_PROMPT_CHARS: usize = 400;

/// What Gantry shows for one transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptSummary {
    /// Session id (the file name without `.jsonl`).
    pub session_id: String,
    /// Directory the session ran in, read from inside the transcript.
    pub cwd: PathBuf,
    /// Name the user gave the session (newest `custom-title` / `agent-name`).
    pub name: Option<String>,
    /// Title Claude generated (newest `ai-title`).
    pub ai_title: Option<String>,
    /// First line of the first prompt the user typed.
    pub first_prompt: Option<String>,
    /// Your newest prompt, on one line: the newest prompt in the tail, else the newest `last-prompt` record there
    /// (Claude writes those on its own schedule, so one can be several prompts old).
    pub last_prompt: Option<String>,
}

impl TranscriptSummary {
    /// Name, else Claude's title, else the first prompt.
    pub fn title(&self) -> String {
        self.name.clone().or_else(|| self.ai_title.clone()).or_else(|| self.first_prompt.clone()).unwrap_or_default()
    }
}

/// Name session `session` the way Claude's `/rename` does: a `custom-title` record appended to its transcript under
/// `claude_dir`. `Ok(false)` when there is no such transcript.
pub fn set_name(claude_dir: &Path, session: &str, name: &str) -> io::Result<bool> {
    let file_name = format!("{session}.jsonl");
    let Some(path) = std::fs::read_dir(claude_dir.join("projects"))?
        .flatten()
        .map(|folder| folder.path().join(&file_name))
        .find(|path| path.is_file())
    else {
        return Ok(false);
    };
    let record = serde_json::json!({"type": "custom-title", "customTitle": name, "sessionId": session});
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    std::io::Write::write_all(&mut file, format!("{record}\n").as_bytes())?;
    Ok(true)
}

/// Summarize the transcript at `path`. `Ok(None)` for transcripts without a directory or with nothing to
/// show (no prompt and no title) — sessions that never started.
pub fn summarize(path: &Path) -> io::Result<Option<TranscriptSummary>> {
    let Some(session_id) = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()) else { return Ok(None) };
    let mut file = File::open(path)?;
    let (cwd, first_prompt) = read_head(&mut file)?;
    let (name, ai_title, last_prompt) = read_tail(&mut file)?;
    let Some(cwd) = cwd else { return Ok(None) };
    if first_prompt.is_none() && name.is_none() && ai_title.is_none() {
        return Ok(None);
    }
    Ok(Some(TranscriptSummary { session_id, cwd, name, ai_title, first_prompt, last_prompt }))
}

/// The first `cwd` and the first user prompt within `HEAD_LIMIT` bytes. Unparseable lines are skipped.
fn read_head(file: &mut File) -> io::Result<(Option<PathBuf>, Option<String>)> {
    let mut reader = BufReader::new(file.by_ref().take(HEAD_LIMIT));
    let (mut cwd, mut prompt) = (None, None);
    let mut line = Vec::new();
    while cwd.is_none() || prompt.is_none() {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else { continue };
        if cwd.is_none() {
            cwd = record.get("cwd").and_then(Value::as_str).filter(|cwd| !cwd.is_empty()).map(PathBuf::from);
        }
        if prompt.is_none() {
            prompt = user_prompt(&record);
        }
    }
    Ok((cwd, prompt))
}

/// The newest name, AI title and last prompt in the last `TAIL_LIMIT` bytes. Only lines mentioning one of those
/// records or a user record are parsed; a partial first line (the window starts mid-line) and a half-written last line fail to
/// parse and are skipped.
fn read_tail(file: &mut File) -> io::Result<(Option<String>, Option<String>, Option<String>)> {
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_LIMIT)))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    let (mut name, mut ai_title, mut last_prompt, mut typed) = (None, None, None, None);
    for line in tail.split(|&byte| byte == b'\n') {
        if !mentions(line, b"-title\"")
            && !mentions(line, b"\"agent-name\"")
            && !mentions(line, b"\"last-prompt\"")
            && !mentions(line, b"\"type\":\"user\"")
        {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(line) else { continue };
        let text = |key: &str| record.get(key).and_then(Value::as_str).map(clean).filter(|text| !text.is_empty());
        match record.get("type").and_then(Value::as_str) {
            Some("custom-title") => name = text("customTitle").or(name),
            Some("agent-name") => name = text("agentName").or(name),
            Some("ai-title") => ai_title = text("aiTitle").or(ai_title),
            Some("last-prompt") => {
                let prompt = record.get("lastPrompt").and_then(Value::as_str).map(clean_prompt);
                last_prompt = prompt.filter(|prompt| !prompt.is_empty()).or(last_prompt);
            }
            Some("user") => {
                typed = prompt_text(&record).map(clean_prompt).filter(|prompt| !prompt.is_empty()).or(typed)
            }
            _ => {}
        }
    }
    Ok((name, ai_title, typed.or(last_prompt)))
}

fn mentions(line: &[u8], needle: &[u8]) -> bool {
    line.windows(needle.len()).any(|window| window == needle)
}

/// First line of a prompt the user typed.
fn user_prompt(record: &Value) -> Option<String> {
    Some(clean(prompt_text(record)?.lines().next()?)).filter(|line| !line.is_empty())
}

/// The text of a prompt the user typed. Skips meta records, sidechains, wrapped command output (text starting
/// with `<`) and the `[Image: …]` notes Claude adds for pasted images.
fn prompt_text(record: &Value) -> Option<&str> {
    if record.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    if ["isMeta", "isSidechain"].iter().any(|key| record.get(*key).and_then(Value::as_bool) == Some(true)) {
        return None;
    }
    let text = match record.get("message")?.get("content")? {
        Value::String(text) => text.as_str(),
        Value::Array(blocks) => blocks
            .iter()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("text"))?
            .get("text")?
            .as_str()?,
        _ => return None,
    };
    let text = text.trim_start();
    (!text.starts_with('<') && !text.starts_with("[Image")).then_some(text)
}

/// Trimmed, without control characters, at most `MAX_TITLE_CHARS` characters.
fn clean(text: &str) -> String {
    text.trim().chars().filter(|c| !c.is_control()).take(MAX_TITLE_CHARS).collect()
}

/// One line: whitespace runs become single spaces, control characters are dropped, at most `MAX_PROMPT_CHARS`.
fn clean_prompt(text: &str) -> String {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|word| word.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|word| !word.is_empty())
        .collect();
    words.join(" ").chars().take(MAX_PROMPT_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, lines: &[String]) -> PathBuf {
        let path = dir.join("0f3c-session.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    fn user(text: &str) -> String {
        serde_json::json!({"type": "user", "cwd": "/repo", "message": {"role": "user", "content": text}}).to_string()
    }

    fn record(kind: &str, key: &str, value: &str) -> String {
        serde_json::json!({"type": kind, key: value, "sessionId": "0f3c-session"}).to_string()
    }

    #[test]
    fn summary_reads_directory_prompt_and_titles() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            &[
                r#"{"type":"mode","mode":"normal"}"#.into(),
                serde_json::json!({"type": "attachment", "cwd": "/home/me/repo"}).to_string(),
                user("Fix the Jenkins lib\nsecond line"),
                record("ai-title", "aiTitle", "Jenkins library fix"),
            ],
        );
        let summary = summarize(&path).unwrap().unwrap();
        assert_eq!(summary.session_id, "0f3c-session");
        assert_eq!(summary.cwd, PathBuf::from("/home/me/repo"));
        assert_eq!(summary.first_prompt.as_deref(), Some("Fix the Jenkins lib"));
        assert_eq!(summary.ai_title.as_deref(), Some("Jenkins library fix"));
        assert_eq!(summary.name, None);
    }

    #[test]
    fn title_prefers_name_then_ai_title_then_prompt() {
        let mut summary = TranscriptSummary {
            session_id: "s".into(),
            cwd: "/".into(),
            name: Some("mine".into()),
            ai_title: Some("ai".into()),
            first_prompt: Some("prompt".into()),
            last_prompt: None,
        };
        assert_eq!(summary.title(), "mine");
        summary.name = None;
        assert_eq!(summary.title(), "ai");
        summary.ai_title = None;
        assert_eq!(summary.title(), "prompt");
    }

    #[test]
    fn set_name_appends_a_custom_title_that_wins() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("projects").join("-repo");
        std::fs::create_dir_all(&folder).unwrap();
        let path = write(&folder, &[user("hello"), record("custom-title", "customTitle", "Old")]);
        let id = path.file_stem().unwrap().to_string_lossy().into_owned();
        assert!(set_name(dir.path(), &id, "New name").unwrap());
        assert_eq!(summarize(&path).unwrap().unwrap().name.as_deref(), Some("New name"));
        assert!(!set_name(dir.path(), "missing", "x").unwrap());
    }

    #[test]
    fn newest_title_records_win() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            &[
                user("hello"),
                record("ai-title", "aiTitle", "old title"),
                record("custom-title", "customTitle", "First name"),
                record("ai-title", "aiTitle", "new title"),
                record("agent-name", "agentName", "Second name"),
            ],
        );
        let summary = summarize(&path).unwrap().unwrap();
        assert_eq!((summary.name.as_deref(), summary.ai_title.as_deref()), (Some("Second name"), Some("new title")));
    }

    #[test]
    fn meta_records_and_command_wrappers_are_not_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), &[
            serde_json::json!({"type": "user", "isMeta": true, "cwd": "/r", "message": {"content": "Caveat: meta"}}).to_string(),
            user("<command-name>/clear</command-name>"),
            serde_json::json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "x"}]}}).to_string(),
            serde_json::json!({"type": "user", "message": {"content": [{"type": "text", "text": "  Real prompt  "}]}}).to_string(),
        ]);
        assert_eq!(summarize(&path).unwrap().unwrap().first_prompt.as_deref(), Some("Real prompt"));
    }

    #[test]
    fn transcripts_without_a_directory_or_content_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let no_cwd =
            write(dir.path(), &[serde_json::json!({"type": "user", "message": {"content": "hi"}}).to_string()]);
        assert_eq!(summarize(&no_cwd).unwrap(), None);
        let empty = write(dir.path(), &[serde_json::json!({"type": "attachment", "cwd": "/r"}).to_string()]);
        assert_eq!(summarize(&empty).unwrap(), None);
    }

    #[test]
    fn garbage_and_a_half_written_last_line_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let huge = format!(r#"{{"type":"assistant","text":"{}"}}"#, "x".repeat(3 << 20));
        let content = [
            "not json at all".to_string(),
            "\u{0}\u{1}binary".to_string(),
            user("the prompt"),
            huge,
            record("ai-title", "aiTitle", "kept"),
            r#"{"type":"ai-title","aiTitle":"half wri"#.to_string(),
        ]
        .join("\n");
        std::fs::write(&path, content).unwrap();
        let summary = summarize(&path).unwrap().unwrap();
        assert_eq!(summary.first_prompt.as_deref(), Some("the prompt"));
        assert_eq!(summary.ai_title.as_deref(), Some("kept"));
    }

    #[test]
    fn titles_are_found_at_the_end_of_large_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let filler: Vec<String> =
            (0..2000).map(|i| format!(r#"{{"type":"assistant","n":{i},"pad":"{}"}}"#, "y".repeat(200))).collect();
        let mut lines = vec![user("start"), record("ai-title", "aiTitle", "early only")];
        lines.extend(filler);
        lines.push(record("custom-title", "customTitle", "Late name"));
        let summary = summarize(&write(dir.path(), &lines)).unwrap().unwrap();
        assert_eq!(summary.name.as_deref(), Some("Late name"));
        // A title record only in the head of a large file is outside the tail window.
        assert_eq!(summary.ai_title, None);
        assert_eq!(summary.title(), "Late name");
    }

    #[test]
    fn titles_drop_control_characters_and_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        let long = "é".repeat(500);
        let path = write(dir.path(), &[user(&format!("a\u{7}b{long}")), record("ai-title", "aiTitle", "x\u{1b}[31my")]);
        let summary = summarize(&path).unwrap().unwrap();
        assert_eq!(summary.ai_title.as_deref(), Some("x[31my"));
        assert_eq!(summary.first_prompt.unwrap().chars().count(), MAX_TITLE_CHARS);
    }

    fn last_prompt(text: &str) -> String {
        serde_json::json!({"type": "last-prompt", "lastPrompt": text, "leafUuid": "u", "sessionId": "0f3c-session"})
            .to_string()
    }

    #[test]
    fn the_newest_last_prompt_record_wins_and_records_without_text_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            &[
                record("ai-title", "aiTitle", "Title"),
                serde_json::json!({"type": "attachment", "cwd": "/repo"}).to_string(),
                last_prompt("first"),
                last_prompt("second"),
                r#"{"type":"last-prompt","leafUuid":"u","sessionId":"0f3c-session"}"#.into(),
            ],
        );
        assert_eq!(summarize(&path).unwrap().unwrap().last_prompt.as_deref(), Some("second"));
    }

    #[test]
    fn the_newest_typed_prompt_beats_an_older_last_prompt_record() {
        let dir = tempfile::tempdir().unwrap();
        let tool_result = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "tool_result", "content": "done"}]}
        })
        .to_string();
        let path = write(
            dir.path(),
            &[
                user("first"),
                last_prompt("first"),
                user("second\nline two"),
                tool_result,
                user("<command-name>/clear</command-name>"),
                user("[Image: source: /tmp/1.png]"),
            ],
        );
        assert_eq!(summarize(&path).unwrap().unwrap().last_prompt.as_deref(), Some("second line two"));
    }

    #[test]
    fn last_prompt_is_one_clean_line_of_at_most_400_characters() {
        let dir = tempfile::tempdir().unwrap();
        let long = "é".repeat(5000);
        let title = record("ai-title", "aiTitle", "Title");
        let cwd = serde_json::json!({"type": "attachment", "cwd": "/repo"}).to_string();
        let path = write(dir.path(), &[title, cwd, last_prompt(&format!("a\n\tb\u{7}c  d {long}"))]);
        let prompt = summarize(&path).unwrap().unwrap().last_prompt.unwrap();
        assert!(prompt.starts_with("a bc d é"), "{prompt}");
        assert_eq!(prompt.chars().count(), MAX_PROMPT_CHARS);
    }

    #[test]
    fn last_prompt_alone_does_not_make_a_session_worth_showing() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write(dir.path(), &[serde_json::json!({"type": "attachment", "cwd": "/r"}).to_string(), last_prompt("x")]);
        assert_eq!(summarize(&path).unwrap(), None);
    }

    #[test]
    fn a_last_prompt_outside_the_tail_window_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let filler: Vec<String> =
            (0..2000).map(|i| format!(r#"{{"type":"assistant","n":{i},"pad":"{}"}}"#, "y".repeat(200))).collect();
        let mut lines = vec![user("start"), last_prompt("early")];
        lines.extend(filler);
        assert_eq!(summarize(&write(dir.path(), &lines)).unwrap().unwrap().last_prompt, None);
    }
}
