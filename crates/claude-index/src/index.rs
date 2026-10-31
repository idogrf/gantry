//! The session index: every transcript's summary (cached by path, mtime and size) combined with the
//! live pid files, checked against the host.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::host::Host;
use crate::live::{self, Activity, LiveRecord};
use crate::transcript::{FolderScan, TranscriptSummary, summarize_with};

/// Whether a session runs, as far as the index can tell (the daemon adds which pane runs it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexState {
    /// A host process runs it.
    Running {
        /// That process.
        pid: u32,
    },
    /// A process in another pid namespace (a container) runs it.
    InContainer {
        /// Its pid on this host; `None` where processes cannot be checked (then its pid file is the only sign).
        pid: Option<u32>,
    },
    /// Nothing runs it.
    Closed,
}

/// One session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecord {
    /// Session id.
    pub id: String,
    /// Directory it runs (ran) in.
    pub cwd: PathBuf,
    /// Title by the transcript's own rule; empty for a live session without a transcript yet.
    pub title: String,
    /// Last activity, ms since the Unix epoch (transcript mtime, else the pid file's `updatedAt`).
    pub last_active_ms: u64,
    /// Running or not.
    pub state: IndexState,
    /// Whether `cwd` exists (resume needs it).
    pub cwd_exists: bool,
    /// What the running Claude reports doing; `None` when closed or unknown.
    pub activity: Option<Activity>,
    /// When `activity` was last written, ms since the Unix epoch (0 = unknown).
    pub activity_since_ms: u64,
    /// Start time of the process running it (from its pid file), for verifying that process later.
    pub proc_start: Option<String>,
    /// Docker container its process runs in (from the process's cgroup).
    pub container_id: Option<String>,
    /// The config directory it was found in.
    pub claude_dir: Option<PathBuf>,
    /// Your newest prompt, from the transcript's `last-prompt` record; `None` when there is none.
    pub last_prompt: Option<String>,
}

struct Cached {
    modified: SystemTime,
    len: u64,
    summary: Option<TranscriptSummary>,
    /// What was learned about the transcript's folder, for the next time it changes.
    scan: FolderScan,
}

/// Scans one config directory; keeps summaries of unchanged transcripts between scans.
pub struct Index<H> {
    config: PathBuf,
    host: H,
    cache: HashMap<PathBuf, Cached>,
}

impl<H: Host> Index<H> {
    /// Index `config` (a Claude config directory) on `host`.
    pub fn new(config: PathBuf, host: H) -> Index<H> {
        Index { config, host, cache: HashMap::new() }
    }

    /// Re-read what changed and return every session, most recently active first.
    pub fn scan(&mut self) -> Vec<SessionRecord> {
        let mut records: HashMap<String, SessionRecord> = HashMap::new();
        let mut seen = HashSet::new();
        for (path, modified, len) in transcripts(&self.config.join("projects")) {
            seen.insert(path.clone());
            let fresh = self.cache.get(&path).is_some_and(|cached| cached.modified == modified && cached.len == len);
            if !fresh {
                let mut scan =
                    self.cache.get_mut(&path).map(|cached| std::mem::take(&mut cached.scan)).unwrap_or_default();
                let mut summary = summarize_with(&path, &mut scan).unwrap_or_else(|err| {
                    tracing::debug!("skipping transcript {}: {err}", path.display());
                    None
                });
                // The tail window can lose the last prompt in a long turn; the file only grows, so keep the old one.
                let earlier = self.cache.get(&path).and_then(|cached| cached.summary.as_ref()?.last_prompt.clone());
                if let Some(summary) = summary.as_mut() {
                    summary.last_prompt = summary.last_prompt.take().or(earlier);
                }
                self.cache.insert(path.clone(), Cached { modified, len, summary, scan });
            }
            let Some(summary) = &self.cache[&path].summary else { continue };
            let record = SessionRecord {
                id: summary.session_id.clone(),
                cwd: summary.cwd.clone(),
                title: summary.title(),
                last_active_ms: millis(modified),
                state: IndexState::Closed,
                cwd_exists: false,
                activity: None,
                activity_since_ms: 0,
                proc_start: None,
                container_id: None,
                claude_dir: Some(self.config.clone()),
                last_prompt: summary.last_prompt.clone(),
            };
            // The same id in two folders: keep the newer.
            if records.get(&record.id).is_none_or(|old| old.last_active_ms < record.last_active_ms) {
                records.insert(record.id.clone(), record);
            }
        }
        self.cache.retain(|path, _| seen.contains(path));
        for live in live::read_all(&self.config.join("sessions")) {
            let Some((state, container_id)) = liveness(&live, &self.host) else { continue };
            let record = records.entry(live.session_id.clone()).or_insert_with(|| SessionRecord {
                id: live.session_id.clone(),
                cwd: live.cwd.clone(),
                title: String::new(),
                last_active_ms: live.updated_at_ms.unwrap_or(0),
                state: IndexState::Closed,
                cwd_exists: false,
                activity: None,
                activity_since_ms: 0,
                proc_start: None,
                container_id: None,
                claude_dir: Some(self.config.clone()),
                last_prompt: None,
            });
            // A host process beats a container entry for the same session.
            if !matches!(record.state, IndexState::Running { .. }) {
                record.state = state;
                record.activity = live.activity;
                record.activity_since_ms = live.activity_since_ms.unwrap_or(0);
                record.proc_start = live.proc_start;
                record.container_id = container_id;
            }
        }
        let mut exists: HashMap<PathBuf, bool> = HashMap::new();
        let host = &self.host;
        let mut list: Vec<SessionRecord> = records
            .into_values()
            .map(|mut record| {
                record.cwd_exists = *exists.entry(record.cwd.clone()).or_insert_with(|| host.dir_exists(&record.cwd));
                record
            })
            .collect();
        list.sort_by(|a, b| b.last_active_ms.cmp(&a.last_active_ms).then_with(|| a.id.cmp(&b.id)));
        list
    }
}

/// The state a pid file proves (and the container its process runs in), or `None` if its process is gone. A pid
/// from another pid namespace is looked up on the host by namespace, pid and start time; where that cannot be
/// checked it counts as running (Claude removes its pid file on exit).
fn liveness(record: &LiveRecord, host: &impl Host) -> Option<(IndexState, Option<String>)> {
    if let (Some(theirs), Some(ours)) = (&record.pid_domain, host.pid_domain())
        && *theirs != ours
    {
        return match host.namespaced(theirs, record.pid, record.proc_start.as_deref()) {
            None => Some((IndexState::InContainer { pid: None }, None)),
            Some(None) => None,
            Some(Some(found)) => Some((IndexState::InContainer { pid: Some(found.host_pid) }, found.container)),
        };
    }
    match host.is_running(record.pid, record.proc_start.as_deref()) {
        Some(false) => None,
        // Running, or this host cannot tell.
        _ => Some((IndexState::Running { pid: record.pid }, None)),
    }
}

/// `projects/<folder>/<id>.jsonl` files with their mtime and size. Deeper files (subagent
/// transcripts) are not sessions.
pub fn transcripts(projects: &Path) -> Vec<(PathBuf, SystemTime, u64)> {
    let Ok(folders) = std::fs::read_dir(projects) else { return Vec::new() };
    folders
        .flatten()
        .filter_map(|folder| std::fs::read_dir(folder.path()).ok())
        .flat_map(|files| files.flatten())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| (entry.path(), meta.modified().unwrap_or(UNIX_EPOCH), meta.len()))
        })
        .collect()
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Namespaced;
    use crate::live::Activity;
    use std::time::Duration;

    #[derive(Default)]
    struct FakeHost {
        /// pid → start time of running processes.
        running: HashMap<u32, String>,
        domain: Option<String>,
        missing: HashSet<PathBuf>,
        /// Container pid → its host process.
        namespaced: HashMap<u32, Namespaced>,
        cannot_check_namespaces: bool,
    }

    impl Host for FakeHost {
        fn is_running(&self, pid: u32, start: Option<&str>) -> Option<bool> {
            Some(self.running.get(&pid).is_some_and(|s| start.is_none_or(|start| start == s)))
        }
        fn pid_domain(&self) -> Option<String> {
            self.domain.clone()
        }
        fn dir_exists(&self, path: &Path) -> bool {
            !self.missing.contains(path)
        }
        fn namespaced(&self, _domain: &str, pid: u32, _start: Option<&str>) -> Option<Option<Namespaced>> {
            if self.cannot_check_namespaces { None } else { Some(self.namespaced.get(&pid).cloned()) }
        }
    }

    fn transcript(config: &Path, folder: &str, id: &str, cwd: &str, prompt: &str, age_secs: u64) -> PathBuf {
        let dir = config.join("projects").join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        let line = serde_json::json!({"type": "user", "cwd": cwd, "message": {"content": prompt}});
        std::fs::write(&path, format!("{line}\n")).unwrap();
        set_age(&path, age_secs);
        path
    }

    /// Set the mtime to a fixed base minus `age_secs` (deterministic, so "the same mtime" is exact).
    fn set_age(path: &Path, age_secs: u64) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(1_800_000_000 - age_secs)).unwrap();
    }

    fn pid_file(config: &Path, pid: u32, id: &str, cwd: &str, start: &str, domain: Option<&str>) {
        let dir = config.join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let mut json =
            serde_json::json!({"pid": pid, "sessionId": id, "cwd": cwd, "procStart": start, "updatedAt": 1000});
        if let Some(domain) = domain {
            json["pidDomain"] = domain.into();
        }
        std::fs::write(dir.join(format!("{pid}.json")), json.to_string()).unwrap();
    }

    fn ids(records: &[SessionRecord]) -> Vec<&str> {
        records.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn closed_sessions_come_from_transcripts_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        transcript(dir.path(), "-a", "old", "/a", "first", 300);
        transcript(dir.path(), "-b", "new", "/b", "second", 10);
        let records = Index::new(dir.path().into(), FakeHost::default()).scan();
        assert_eq!(ids(&records), vec!["new", "old"]);
        assert_eq!(
            (records[0].title.as_str(), records[0].cwd.as_path(), records[0].state),
            ("second", Path::new("/b"), IndexState::Closed)
        );
        assert!(records[0].cwd_exists);
    }

    #[test]
    fn a_running_pid_file_makes_its_session_running() {
        let dir = tempfile::tempdir().unwrap();
        transcript(dir.path(), "-a", "s1", "/a", "hi", 5);
        pid_file(dir.path(), 42, "s1", "/a", "777", None);
        let host = FakeHost { running: HashMap::from([(42, "777".to_string())]), ..FakeHost::default() };
        assert_eq!(Index::new(dir.path().into(), host).scan()[0].state, IndexState::Running { pid: 42 });
    }

    #[test]
    fn a_running_pid_file_carries_its_status_and_start_time() {
        let dir = tempfile::tempdir().unwrap();
        transcript(dir.path(), "-a", "s1", "/a", "hi", 5);
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        let json = serde_json::json!({"pid": 42, "sessionId": "s1", "cwd": "/a", "procStart": "777",
            "status": "waiting", "waitingFor": "dialog open", "statusUpdatedAt": 1234, "updatedAt": 1300});
        std::fs::write(dir.path().join("sessions/42.json"), json.to_string()).unwrap();
        let host = FakeHost { running: HashMap::from([(42, "777".to_string())]), ..FakeHost::default() };
        let record = Index::new(dir.path().into(), host).scan().remove(0);
        assert_eq!(record.activity, Some(Activity::Waiting { reason: Some("dialog open".into()) }));
        assert_eq!((record.activity_since_ms, record.proc_start.as_deref()), (1234, Some("777")));
    }

    #[test]
    fn closed_sessions_have_no_status() {
        let dir = tempfile::tempdir().unwrap();
        transcript(dir.path(), "-a", "s1", "/a", "hi", 5);
        let record = Index::new(dir.path().into(), FakeHost::default()).scan().remove(0);
        assert_eq!((record.activity, record.activity_since_ms, record.proc_start), (None, 0, None));
    }

    #[test]
    fn a_reused_pid_is_not_live() {
        let dir = tempfile::tempdir().unwrap();
        transcript(dir.path(), "-a", "s1", "/a", "hi", 5);
        pid_file(dir.path(), 42, "s1", "/a", "777", None);
        pid_file(dir.path(), 43, "ghost", "/g", "1", None);
        // Pid 42 now belongs to a process that started at another time; 43 is gone.
        let host = FakeHost { running: HashMap::from([(42, "999".to_string())]), ..FakeHost::default() };
        let records = Index::new(dir.path().into(), host).scan();
        assert_eq!(ids(&records), vec!["s1"]);
        assert_eq!(records[0].state, IndexState::Closed);
    }

    #[test]
    fn a_container_pid_file_is_running_only_while_its_host_process_lives() {
        let dir = tempfile::tempdir().unwrap();
        pid_file(dir.path(), 7, "alive", "/workspaces/x", "5", Some("linux::pid:[4026533220]"));
        pid_file(dir.path(), 8, "stale", "/workspaces/x", "6", Some("linux::pid:[4026533223]"));
        let host = FakeHost {
            domain: Some("linux:abc:pid:[4026531836]".into()),
            namespaced: HashMap::from([(7, Namespaced { host_pid: 9007, container: Some("4a31e6".into()) })]),
            ..FakeHost::default()
        };
        let records = Index::new(dir.path().into(), host).scan();
        assert_eq!(ids(&records), vec!["alive"]);
        assert_eq!(
            (records[0].state, records[0].container_id.as_deref()),
            (IndexState::InContainer { pid: Some(9007) }, Some("4a31e6"))
        );
    }

    #[test]
    fn a_container_pid_file_counts_as_running_where_processes_cannot_be_checked() {
        let dir = tempfile::tempdir().unwrap();
        pid_file(dir.path(), 7, "box", "/workspaces/x", "5", Some("linux::pid:[4026533220]"));
        let host = FakeHost {
            domain: Some("linux:abc:pid:[4026531836]".into()),
            cannot_check_namespaces: true,
            ..FakeHost::default()
        };
        assert_eq!(Index::new(dir.path().into(), host).scan()[0].state, IndexState::InContainer { pid: None });
    }

    #[test]
    fn a_live_session_without_a_transcript_is_listed_untitled() {
        let dir = tempfile::tempdir().unwrap();
        pid_file(dir.path(), 42, "fresh", "/a", "777", None);
        let host = FakeHost { running: HashMap::from([(42, "777".to_string())]), ..FakeHost::default() };
        let record = Index::new(dir.path().into(), host).scan().remove(0);
        assert_eq!((record.id.as_str(), record.title.as_str(), record.last_active_ms), ("fresh", "", 1000));
    }

    #[test]
    fn the_last_prompt_survives_the_transcript_growing_past_the_tail_window() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), "-r", "s1", "/r", "hello", 10);
        let record = serde_json::json!({"type": "user", "cwd": "/r", "message": {"content": "what next"}});
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(format!("{record}\n").as_bytes())
            .unwrap();
        let mut index = Index::new(dir.path().into(), FakeHost::default());
        assert_eq!(index.scan()[0].last_prompt.as_deref(), Some("what next"));
        // A long turn: tool output pushes the prompt out of the tail window.
        let filler = format!(r#"{{"type":"assistant","pad":"{}"}}"#, "y".repeat(300 << 10));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(format!("{filler}\n").as_bytes())
            .unwrap();
        assert_eq!(index.scan()[0].last_prompt.as_deref(), Some("what next"));
    }

    #[test]
    fn unchanged_transcripts_are_not_reread() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript(dir.path(), "-a", "s", "/a", "aaaa", 60);
        let mut index = Index::new(dir.path().into(), FakeHost::default());
        assert_eq!(index.scan()[0].title, "aaaa");
        // Same size and mtime: the cached summary is used.
        let line = serde_json::json!({"type": "user", "cwd": "/a", "message": {"content": "bbbb"}});
        std::fs::write(&path, format!("{line}\n")).unwrap();
        set_age(&path, 60);
        assert_eq!(index.scan()[0].title, "aaaa");
        set_age(&path, 30);
        assert_eq!(index.scan()[0].title, "bbbb");
    }

    #[test]
    fn deleted_transcripts_disappear_and_missing_dirs_are_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let gone = transcript(dir.path(), "-a", "gone", "/a", "x", 5);
        transcript(dir.path(), "-b", "moved", "/deleted", "y", 6);
        let host = FakeHost { missing: HashSet::from([PathBuf::from("/deleted")]), ..FakeHost::default() };
        let mut index = Index::new(dir.path().into(), host);
        assert_eq!(index.scan().len(), 2);
        std::fs::remove_file(gone).unwrap();
        let records = index.scan();
        assert_eq!(ids(&records), vec!["moved"]);
        assert!(!records[0].cwd_exists);
    }

    #[test]
    fn subagent_transcripts_and_missing_config_dirs_yield_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("projects/-a/s1/subagents");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("agent-x.jsonl"), r#"{"type":"user","cwd":"/a","message":{"content":"hi"}}"#)
            .unwrap();
        assert!(Index::new(dir.path().into(), FakeHost::default()).scan().is_empty());
        assert!(Index::new(dir.path().join("nope"), FakeHost::default()).scan().is_empty());
    }
}
