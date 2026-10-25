//! Finds Claude config dirs on this machine (read-only), for the first-run wizard: `$CLAUDE_CONFIG_DIR`, `~/.claude`
//! and sibling folders named like one. A folder counts when it holds a `projects/` or a `sessions/` directory.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::index::transcripts;

/// A dir whose newest session is older than this (30 days, in milliseconds) is stale.
pub const STALE_AFTER_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Where a candidate was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `$CLAUDE_CONFIG_DIR`.
    ConfigDirEnv,
    /// `~/.claude`.
    HomeDefault,
    /// A folder next to it (`~/.claude*`, `~/.config/claude*`), or one picked by hand.
    Sibling,
}

/// A folder that looks like a Claude config dir.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The folder, as found (a symlink is kept as written, not resolved).
    pub path: PathBuf,
    /// A display name derived from the folder name, made unique among the candidates.
    pub name: String,
    /// Where it was found.
    pub source: Source,
    /// Transcript files under `projects/`.
    pub sessions: usize,
    /// Project folders under `projects/`.
    pub projects: usize,
    /// Modification time of the newest transcript, ms since the Unix epoch.
    pub last_used_ms: Option<u64>,
    /// No sessions, or none for [`STALE_AFTER_MS`].
    pub stale: bool,
}

/// The folder has neither a `projects/` nor a `sessions/` directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotAClaudeDir;

/// Claude config dirs reachable from `env` (`CLAUDE_CONFIG_DIR`, and the home dir from `HOME`, or `USERPROFILE` on
/// Windows): the env dir, then `~/.claude`, then the siblings newest first. One level only; symlinks to a dir already
/// found are dropped.
pub fn discover(env: &[(String, String)], now_ms: u64) -> Vec<Candidate> {
    let mut roots: Vec<(PathBuf, Source)> = Vec::new();
    if let Some(dir) = var(env, "CLAUDE_CONFIG_DIR") {
        roots.push((PathBuf::from(dir), Source::ConfigDirEnv));
    }
    if let Some(home) = home(env) {
        roots.push((home.join(".claude"), Source::HomeDefault));
        roots.extend(children(&home, ".claude").into_iter().map(|path| (path, Source::Sibling)));
        roots.extend(children(&home.join(".config"), "claude").into_iter().map(|path| (path, Source::Sibling)));
    }
    let mut seen = HashSet::new();
    let mut found: Vec<Candidate> = roots
        .into_iter()
        .filter(|(path, _)| path.canonicalize().is_ok_and(|real| seen.insert(real)))
        .filter_map(|(path, source)| inspect_as(&path, source, now_ms).ok())
        .collect();
    found.sort_by_key(|candidate| {
        let rank = match candidate.source {
            Source::ConfigDirEnv => 0,
            Source::HomeDefault => 1,
            Source::Sibling => 2,
        };
        (rank, std::cmp::Reverse(candidate.last_used_ms))
    });
    for index in 0..found.len() {
        let before: Vec<String> = found[..index].iter().map(|candidate| candidate.name.clone()).collect();
        found[index].name = unique_name(&found[index].name, &|name| before.iter().any(|taken| taken == name));
    }
    found
}

/// `path` as a candidate (a folder picked by hand), or why it is not one.
pub fn inspect(path: &Path, now_ms: u64) -> Result<Candidate, NotAClaudeDir> {
    inspect_as(path, Source::Sibling, now_ms)
}

/// The index of the candidate to make the default: the env dir, else `~/.claude`, else the first fresh one.
pub fn default_pick(candidates: &[Candidate]) -> Option<usize> {
    let of = |source| candidates.iter().position(|candidate| candidate.source == source);
    of(Source::ConfigDirEnv)
        .or_else(|| of(Source::HomeDefault))
        .or_else(|| candidates.iter().position(|candidate| !candidate.stale))
}

/// `base`, or `base 2`, `base 3`, … for the first one `taken` does not refuse.
pub fn unique_name(base: &str, taken: &dyn Fn(&str) -> bool) -> String {
    let mut name = base.to_string();
    let mut number = 2;
    while taken(&name) {
        name = format!("{base} {number}");
        number += 1;
    }
    name
}

fn inspect_as(path: &Path, source: Source, now_ms: u64) -> Result<Candidate, NotAClaudeDir> {
    let projects = path.join("projects");
    if !path.is_dir() || !(projects.is_dir() || path.join("sessions").is_dir()) {
        return Err(NotAClaudeDir);
    }
    let files = transcripts(&projects);
    let project_count = std::fs::read_dir(&projects)
        .map(|folders| folders.flatten().filter(|entry| entry.path().is_dir()).count())
        .unwrap_or(0);
    let last_used_ms = files.iter().map(|(_, modified, _)| millis(*modified)).max();
    let stale = last_used_ms.is_none_or(|last| now_ms.saturating_sub(last) > STALE_AFTER_MS);
    Ok(Candidate {
        path: path.to_path_buf(),
        name: derive_name(path),
        source,
        sessions: files.len(),
        projects: project_count,
        last_used_ms,
        stale,
    })
}

/// The folder name without a leading dot, `claude`, `-` or `_`, capitalised; "Default" when nothing is left.
fn derive_name(path: &Path) -> String {
    let folder = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let folder = folder.trim_start_matches('.');
    let rest = folder.strip_prefix("claude").unwrap_or(folder).trim_start_matches(['-', '_']);
    let mut chars = rest.chars();
    match chars.next() {
        None => "Default".into(),
        Some(first) => first.to_uppercase().chain(chars).collect(),
    }
}

/// Folders directly inside `parent` whose name starts with `prefix`, by name.
fn children(parent: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else { return Vec::new() };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    paths
}

fn var<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter().find(|(k, v)| k == key && !v.is_empty()).map(|(_, v)| v.as_str())
}

fn home(env: &[(String, String)]) -> Option<PathBuf> {
    var(env, if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const NOW: u64 = 1_800_000_000_000;
    const DAY: u64 = 24 * 60 * 60 * 1000;
    /// The variable the home dir is read from.
    const HOME: &str = if cfg!(windows) { "USERPROFILE" } else { "HOME" };

    fn env(pairs: &[(&str, &Path)]) -> Vec<(String, String)> {
        pairs.iter().map(|(key, value)| (key.to_string(), value.to_string_lossy().into_owned())).collect()
    }

    /// A config dir at `path` with one transcript per `(project folder, session id, age in ms)`.
    fn claude_dir(path: &Path, sessions: &[(&str, &str, u64)]) {
        std::fs::create_dir_all(path.join("projects")).unwrap();
        for (project, id, age) in sessions {
            let folder = path.join("projects").join(project);
            std::fs::create_dir_all(&folder).unwrap();
            let file = std::fs::File::create(folder.join(format!("{id}.jsonl"))).unwrap();
            file.set_modified(UNIX_EPOCH + Duration::from_millis(NOW - age)).unwrap();
        }
    }

    fn names(found: &[Candidate]) -> Vec<&str> {
        found.iter().map(|candidate| candidate.name.as_str()).collect()
    }

    #[test]
    fn finds_the_default_siblings_and_config_siblings_and_ignores_the_rest() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        claude_dir(&h.join(".claude"), &[("-a", "s1", DAY)]);
        claude_dir(&h.join(".claude-work"), &[("-b", "s2", 2 * DAY), ("-b", "s3", 3 * DAY)]);
        claude_dir(&h.join(".config/claude-lab"), &[("-c", "s4", 4 * DAY)]);
        std::fs::create_dir_all(h.join(".claude-empty")).unwrap(); // no projects/ or sessions/
        std::fs::write(h.join(".claude.json"), "{}").unwrap(); // a file, not a dir
        std::fs::create_dir_all(h.join(".config/gantry/projects")).unwrap(); // not named claude*
        let found = discover(&env(&[(HOME, h)]), NOW);
        assert_eq!(names(&found), ["Default", "Work", "Lab"]);
        assert_eq!(found[0].source, Source::HomeDefault);
        assert!(found[1..].iter().all(|candidate| candidate.source == Source::Sibling));
    }

    #[test]
    fn counts_sessions_projects_and_last_use_and_skips_subagent_files() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude");
        claude_dir(&dir, &[("-a", "s1", 5 * DAY), ("-a", "s2", 2 * DAY), ("-b", "s3", 9 * DAY)]);
        std::fs::create_dir_all(dir.join("projects/-a/s1/subagents")).unwrap();
        std::fs::write(dir.join("projects/-a/s1/subagents/x.jsonl"), "{}").unwrap();
        let found = discover(&env(&[(HOME, home.path())]), NOW);
        assert_eq!((found[0].sessions, found[0].projects), (3, 2));
        assert_eq!(found[0].last_used_ms, Some(NOW - 2 * DAY));
    }

    #[test]
    fn stale_means_no_sessions_or_older_than_thirty_days() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        claude_dir(&h.join(".claude"), &[("-a", "s", STALE_AFTER_MS)]); // exactly 30 days: fresh
        claude_dir(&h.join(".claude-old"), &[("-a", "s", STALE_AFTER_MS + 1)]);
        std::fs::create_dir_all(h.join(".claude-none/sessions")).unwrap(); // no transcripts at all
        let found = discover(&env(&[(HOME, h)]), NOW);
        let stale: Vec<_> = found.iter().map(|candidate| (candidate.name.as_str(), candidate.stale)).collect();
        assert_eq!(stale, [("Default", false), ("Old", true), ("None", true)]);
    }

    #[test]
    fn the_env_dir_is_first_and_not_listed_twice_when_it_is_the_home_default() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        claude_dir(&h.join(".claude"), &[("-a", "s", DAY)]);
        claude_dir(&h.join(".claude-work"), &[("-a", "s", DAY)]);
        let found = discover(&env(&[(HOME, h), ("CLAUDE_CONFIG_DIR", &h.join(".claude-work"))]), NOW);
        assert_eq!(names(&found), ["Work", "Default"]);
        assert_eq!(found[0].source, Source::ConfigDirEnv);
        let same = discover(&env(&[(HOME, h), ("CLAUDE_CONFIG_DIR", &h.join(".claude"))]), NOW);
        assert_eq!(same.iter().filter(|candidate| candidate.name == "Default").count(), 1);
        assert_eq!(same[0].source, Source::ConfigDirEnv);
    }

    // Symlinks: unix only.
    #[cfg(unix)]
    #[test]
    fn a_symlink_to_another_candidate_is_one_row_and_keeps_the_path_as_found() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        claude_dir(&h.join("real"), &[("-a", "s", DAY)]);
        std::os::unix::fs::symlink(h.join("real"), h.join(".claude")).unwrap();
        std::os::unix::fs::symlink(h.join("real"), h.join(".claude-link")).unwrap();
        let found = discover(&env(&[(HOME, h)]), NOW);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, h.join(".claude"));
    }

    #[test]
    fn a_bad_env_dir_is_skipped() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        claude_dir(&h.join(".claude"), &[("-a", "s", DAY)]);
        let missing = h.join("nope");
        let a_file = h.join("file");
        std::fs::write(&a_file, "x").unwrap();
        let plain = h.join("plain");
        std::fs::create_dir_all(&plain).unwrap(); // exists, but holds no projects/ or sessions/
        for value in [&missing, &a_file, &plain, Path::new("")] {
            let found = discover(&env(&[(HOME, h), ("CLAUDE_CONFIG_DIR", value)]), NOW);
            assert_eq!(names(&found), ["Default"], "{value:?}");
        }
        assert!(discover(&env(&[]), NOW).is_empty(), "no HOME, no candidates");
    }

    #[test]
    fn siblings_come_newest_first_and_equal_names_get_a_number() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        claude_dir(&h.join(".claude-a"), &[("-x", "s", 9 * DAY)]);
        claude_dir(&h.join(".claude-b"), &[("-x", "s", DAY)]);
        claude_dir(&h.join(".config/claude-b"), &[("-x", "s", 3 * DAY)]);
        let found = discover(&env(&[(HOME, h)]), NOW);
        assert_eq!(names(&found), ["B", "B 2", "A"]);
    }

    #[test]
    fn names_come_from_the_folder() {
        for (folder, name) in [
            (".claude", "Default"),
            ("claude", "Default"),
            (".claude-work", "Work"),
            ("claude_lab", "Lab"),
            (".claude-private", "Private"),
            ("elsewhere", "Elsewhere"),
        ] {
            assert_eq!(derive_name(Path::new("/x").join(folder).as_path()), name, "{folder}");
        }
    }

    #[test]
    fn unique_name_numbers_from_two() {
        let taken = |name: &str| ["Work", "Work 2"].contains(&name);
        assert_eq!(unique_name("Work", &taken), "Work 3");
        assert_eq!(unique_name("Lab", &taken), "Lab");
    }

    fn candidate(source: Source, stale: bool) -> Candidate {
        Candidate { path: "/x".into(), name: "N".into(), source, sessions: 1, projects: 1, last_used_ms: None, stale }
    }

    #[test]
    fn the_default_is_the_env_dir_then_home_then_the_first_fresh_one() {
        let sibling = |stale| candidate(Source::Sibling, stale);
        assert_eq!(default_pick(&[sibling(false), candidate(Source::ConfigDirEnv, true)]), Some(1));
        assert_eq!(default_pick(&[sibling(false), candidate(Source::HomeDefault, true)]), Some(1));
        assert_eq!(default_pick(&[sibling(true), sibling(false)]), Some(1));
        assert_eq!(default_pick(&[sibling(true)]), None);
        assert_eq!(default_pick(&[]), None);
    }

    #[test]
    fn inspect_accepts_a_claude_dir_and_refuses_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        claude_dir(&d.join("ok"), &[("-a", "s", DAY)]);
        std::fs::create_dir_all(d.join("sessions-only/sessions")).unwrap();
        std::fs::create_dir_all(d.join("plain")).unwrap();
        std::fs::write(d.join("file"), "x").unwrap();
        assert_eq!(inspect(&d.join("ok"), NOW).unwrap().sessions, 1);
        assert!(inspect(&d.join("sessions-only"), NOW).is_ok());
        for bad in ["plain", "file", "missing"] {
            assert_eq!(inspect(&d.join(bad), NOW), Err(NotAClaudeDir), "{bad}");
        }
    }
}
