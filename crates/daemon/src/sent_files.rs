//! Watching the transcripts of preview sessions for the files Claude sent to the preview pane.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use claude_index::sent_files::SentFilesReader;
use protocol::message::{PaneId, SentFile, SessionInfo, SessionState};

/// The session ids to read transcripts of: those Managed by a pane that has the preview plugin, plus the
/// `requested` ones (the GUI's own list) that are in `sessions`. An unlisted id is not watched: its list would be
/// dropped with the session, and a reader that already consumed the transcript would never report it again.
pub fn watched(
    sessions: &[SessionInfo],
    preview_panes: &HashSet<PaneId>,
    requested: &HashSet<String>,
) -> HashSet<String> {
    let managed = sessions.iter().filter_map(|session| match session.state {
        SessionState::Managed { pane } if preview_panes.contains(&pane) => Some(session.id.clone()),
        _ => None,
    });
    let asked = sessions.iter().filter(|session| requested.contains(&session.id)).map(|session| session.id.clone());
    managed.chain(asked).collect()
}

/// The newest `<dir>/projects/*/<session>.jsonl` over `dirs`.
pub fn transcript_path(dirs: &[PathBuf], session: &str) -> Option<PathBuf> {
    let name = format!("{session}.jsonl");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for dir in dirs {
        let Ok(projects) = std::fs::read_dir(dir.join("projects")) else { continue };
        for project in projects.flatten() {
            let candidate = project.path().join(&name);
            let Ok(modified) = std::fs::metadata(&candidate).and_then(|meta| meta.modified()) else { continue };
            if newest.as_ref().is_none_or(|(best, _)| modified > *best) {
                newest = Some((modified, candidate));
            }
        }
    }
    newest.map(|(_, path)| path)
}

/// One transcript reader per watched session, with the transcript it reads.
#[derive(Debug, Default)]
pub struct SentFilesWatch {
    readers: HashMap<String, (PathBuf, SentFilesReader)>,
}

impl SentFilesWatch {
    /// Read what the transcripts of `ids` gained since the last call; returns the lists that changed. Readers of ids
    /// no longer watched are dropped.
    pub fn tick(&mut self, ids: &HashSet<String>, dirs: &[PathBuf]) -> Vec<(String, Vec<SentFile>)> {
        self.readers.retain(|id, _| ids.contains(id));
        let mut changes = Vec::new();
        for id in ids {
            let Some(path) = transcript_path(dirs, id) else { continue };
            let (read_path, reader) = self.readers.entry(id.clone()).or_default();
            if *read_path != path {
                *read_path = path.clone();
                *reader = SentFilesReader::new();
            }
            match reader.read_new(&path) {
                Ok(true) => {
                    let files = reader
                        .files()
                        .iter()
                        .map(|file| SentFile {
                            id: file.id.clone(),
                            path: file.path.clone(),
                            sent_ms: file.sent_ms,
                            caption: file.caption.clone(),
                        })
                        .collect();
                    changes.push((id.clone(), files));
                }
                Ok(false) => {}
                Err(err) => tracing::debug!("could not read {}: {err}", path.display()),
            }
        }
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_index::sent_files::TOOL_NAME;
    use serde_json::json;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::Path;

    fn session(id: &str, state: SessionState) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            cwd: "/w".into(),
            title: String::new(),
            last_active_ms: 0,
            state,
            cwd_exists: true,
            status: None,
            status_since_ms: 0,
            container: None,
            claude_dir: None,
            last_prompt: None,
            sent_files: Vec::new(),
            host: protocol::host::HostId::local(),
        }
    }

    /// Transcript lines for one `show_in_preview` call and its result.
    fn sent(id: &str, path: &str) -> String {
        let call = json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": id, "name": TOOL_NAME, "input": {"files": [{"path": path}]}}]}});
        let result = json!({"type": "user", "timestamp": "2026-11-25T10:00:00.250Z", "message": {"content": [
            {"type": "tool_result", "tool_use_id": id,
             "content": [{"type": "text", "text": format!("Shown in preview:\n- {path}")}]}]}});
        format!("{call}\n{result}\n")
    }

    fn append(path: &Path, data: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = OpenOptions::new().create(true).append(true).open(path).unwrap();
        file.write_all(data.as_bytes()).unwrap();
    }

    fn ids(list: &[&str]) -> HashSet<String> {
        list.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn watched_takes_preview_panes_and_requests() {
        let sessions = [
            session("a", SessionState::Managed { pane: 1 }),
            session("b", SessionState::Managed { pane: 2 }),
            session("c", SessionState::Closed),
        ];
        let got = watched(&sessions, &HashSet::from([1]), &ids(&["c", "unlisted"]));
        assert_eq!(got, ids(&["a", "c"]));
    }

    #[test]
    fn transcript_path_finds_the_session_in_any_dir() {
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let wanted = two.path().join("projects/p/s1.jsonl");
        append(&wanted, "{}\n");
        append(&one.path().join("projects/p/other.jsonl"), "{}\n");
        let dirs = [one.path().to_path_buf(), two.path().to_path_buf()];
        assert_eq!(transcript_path(&dirs, "s1"), Some(wanted));
        assert_eq!(transcript_path(&dirs, "missing"), None);
    }

    #[test]
    fn a_session_listed_late_still_reports_its_files() {
        let dir = tempfile::tempdir().unwrap();
        append(&dir.path().join("projects/p/s1.jsonl"), &sent("toolu_1", "/w/a.md"));
        let dirs = [dir.path().to_path_buf()];
        let mut watch = SentFilesWatch::default();
        let requested = ids(&["s1"]);
        // Not listed yet: not watched, so nothing is consumed.
        assert!(watched(&[], &HashSet::new(), &requested).is_empty());
        assert!(watch.tick(&HashSet::new(), &dirs).is_empty());
        let listed = [session("s1", SessionState::Closed)];
        let now = watched(&listed, &HashSet::new(), &requested);
        assert_eq!(watch.tick(&now, &dirs).len(), 1);
    }

    #[test]
    fn a_new_transcript_path_starts_the_reader_over() {
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        append(&one.path().join("projects/p/s1.jsonl"), &sent("toolu_1", "/w/a.md"));
        let mut watch = SentFilesWatch::default();
        let want = ids(&["s1"]);
        assert_eq!(watch.tick(&want, &[one.path().to_path_buf()]).len(), 1);
        append(&two.path().join("projects/p/s1.jsonl"), &sent("toolu_2", "/w/b.md"));
        let changes = watch.tick(&want, &[two.path().to_path_buf()]);
        assert_eq!(changes[0].1.len(), 1);
        assert_eq!(changes[0].1[0].id, "toolu_2:0");
    }

    #[test]
    fn tick_reports_only_changes() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("projects/p/s1.jsonl");
        append(&transcript, &sent("toolu_1", "/w/a.md"));
        let dirs = [dir.path().to_path_buf()];
        let mut watch = SentFilesWatch::default();
        let first = watch.tick(&ids(&["s1"]), &dirs);
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].0.as_str(), first[0].1.len()), ("s1", 1));
        assert_eq!(first[0].1[0].path, Path::new("/w/a.md"));
        assert!(watch.tick(&ids(&["s1"]), &dirs).is_empty());
        append(&transcript, &sent("toolu_2", "/w/b.md"));
        let third = watch.tick(&ids(&["s1"]), &dirs);
        assert_eq!(third[0].1.len(), 2);
        assert!(watch.tick(&ids(&["unknown"]), &dirs).is_empty());
    }
}
