//! Read-only git for the git view (`GitStatus`, `GitDiff`): runs the `git` CLI, never writes to a repo.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use protocol::message::{GitCommit, GitCommitDetail, GitDiffBody, GitFile, GitFileStatus, GitProblem, GitState};

/// Most changed files one answer lists.
const MAX_FILES: usize = 2000;
/// Most commits one answer lists.
const MAX_COMMITS: usize = 30;
/// A diff is cut after this many bytes or lines (`full` raises them).
const DIFF_BYTES: usize = 1024 * 1024;
const DIFF_LINES: usize = 20_000;
const DIFF_BYTES_FULL: usize = 8 * 1024 * 1024;
const DIFF_LINES_FULL: usize = 100_000;
/// A file bigger than this is "too large" until the user asks for it anyway; above `TOO_LARGE_ALWAYS` even then.
const TOO_LARGE: u64 = 4 * 1024 * 1024;
const TOO_LARGE_ALWAYS: u64 = 64 * 1024 * 1024;
/// The empty tree, which a repo with no commits is diffed against.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A `git` command in `folder` that cannot write to the repo or run what the repo's config names: no optional locks
/// (so Claude's own git never meets our index lock), no file system monitor daemon, no hooks, no signature checks (no
/// `gpg.program`), no colors, paths taken literally (a file called `[id].tsx` is not a glob), none of the environment
/// that points git elsewhere, and every filter the repo configures replaced by `cat` (`neutralized` sets them).
fn git(folder: &Path) -> Command {
    let mut command = base(folder);
    NEUTRAL_FILTERS.with(|names| {
        for name in names.borrow().iter() {
            for (key, value) in [("clean", "cat"), ("smudge", "cat"), ("process", ""), ("required", "false")] {
                command.arg("-c").arg(format!("filter.{name}.{key}={value}"));
            }
        }
    });
    command
}

/// `git()` without the filter override.
fn base(folder: &Path) -> Command {
    let mut command = Command::new("git");
    claude_index::process::hide_window(&mut command);
    command.arg("-C").arg(folder).args(["--no-optional-locks", "--literal-pathspecs"]).args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "gpg.program=false",
        "-c",
        "log.showSignature=false",
        "-c",
        "color.ui=never",
    ]);
    command.env("GIT_OPTIONAL_LOCKS", "0").env("LC_ALL", "C");
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_EXTERNAL_DIFF", "GIT_PAGER"] {
        command.env_remove(name);
    }
    command
}

thread_local! {
    /// The clean/smudge filters `git()` replaces by `cat` on this thread; set by `neutralized` for the length of a call.
    static NEUTRAL_FILTERS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The names of the clean/smudge/process filters the repo's own config sets (reads the config, runs nothing).
fn filter_names(folder: &Path) -> Vec<String> {
    let mut command = base(folder);
    command.args(["config", "--show-scope", "--get-regexp", r"^filter\..*\.(clean|smudge|process)$"]);
    let Ok(output) = run_allowing(command, &[0, 1]) else { return Vec::new() };
    repo_filter_names(&text(&output))
}

/// The filter names in a `git config --show-scope --get-regexp` listing, leaving out the system and global ones: those
/// are the user's own (git-lfs installs one), not the repo's, so they run as usual.
fn repo_filter_names(listing: &str) -> Vec<String> {
    let mut names: Vec<String> = listing
        .lines()
        .filter_map(|line| {
            let (scope, line) = line.split_once('\t')?;
            if scope == "system" || scope == "global" {
                return None;
            }
            let key = line.split_once(' ').map_or(line, |(key, _)| key);
            let (name, _) = key.strip_prefix("filter.")?.rsplit_once('.')?;
            Some(name.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Run `attempt` with every filter the repo configures (nbstripout, git-lfs, ...) replaced by `cat`, so a repo's own
/// config never runs a program here, and report whether there were any to skip. Read-only: nothing is written.
fn neutralized<T>(folder: &Path, attempt: impl Fn() -> Result<T, GitProblem>) -> Result<(T, bool), GitProblem> {
    let names = filter_names(folder);
    let skipped = !names.is_empty();
    NEUTRAL_FILTERS.with(|cell| *cell.borrow_mut() = names);
    let result = attempt();
    NEUTRAL_FILTERS.with(|cell| cell.borrow_mut().clear());
    result.map(|value| (value, skipped))
}

/// How long one git call may run before it is killed. Shorter than the GUI's 30 s wait for an answer, so a hung call
/// never makes the GUI ask again while the first still runs.
const GIT_TIMEOUT: Duration = Duration::from_secs(20);
/// The most output one git call may produce (the largest diff plus headroom).
const MAX_OUTPUT: usize = 16 * 1024 * 1024;

/// Run `command` and return its stdout, or why it failed.
fn run(command: Command) -> Result<Vec<u8>, GitProblem> {
    run_allowing(command, &[0])
}

/// Run `command` and return its stdout when it exits with one of `ok_codes`, else why it failed.
fn run_allowing(command: Command, ok_codes: &[i32]) -> Result<Vec<u8>, GitProblem> {
    run_limited(command, ok_codes, GIT_TIMEOUT, MAX_OUTPUT)
}

/// Like `run_allowing`, killing the command after `timeout` and refusing more than `max_output` bytes of stdout.
fn run_limited(
    mut command: Command,
    ok_codes: &[i32],
    timeout: Duration,
    max_output: usize,
) -> Result<Vec<u8>, GitProblem> {
    use std::io::Read;
    use std::process::Stdio;
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => GitProblem::NoGit,
        _ => GitProblem::Failed { reason: error.to_string() },
    })?;
    // Read both pipes on threads so a full pipe never blocks the child; stdout past the limit is dropped, which closes
    // the pipe and ends the child.
    let read = |pipe: Option<Box<dyn Read + Send>>, limit: usize| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(pipe) = pipe {
                let _ = pipe.take(limit as u64 + 1).read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = read(child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>), max_output);
    let stderr = read(child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>), 64 * 1024);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitProblem::Failed { reason: "git took too long".into() });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => return Err(GitProblem::Failed { reason: error.to_string() }),
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if stdout.len() > max_output {
        return Err(GitProblem::Failed { reason: "git output too large".into() });
    }
    if status.code().is_some_and(|code| ok_codes.contains(&code)) {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&stderr);
    if stderr.contains("not a git repository") {
        return Err(GitProblem::NotRepo);
    }
    Err(GitProblem::Failed { reason: stderr.lines().next().unwrap_or("git failed").trim().to_string() })
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// What `git status --porcelain=v2 -z --branch` said.
#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedStatus {
    /// Full hash of `HEAD`; `None` before the first commit.
    oid: Option<String>,
    /// The branch; `None` when detached.
    branch: Option<String>,
    upstream: Option<String>,
    ahead: u32,
    behind: u32,
    /// Path, old path, status.
    entries: Vec<(String, Option<String>, GitFileStatus)>,
}

/// The status of a tracked entry from its two-letter `XY` code: a deletion on either side wins, then an add.
fn tracked_status(xy: &[u8]) -> GitFileStatus {
    if xy.contains(&b'D') {
        GitFileStatus::Deleted
    } else if xy.first() == Some(&b'A') {
        GitFileStatus::Added
    } else {
        GitFileStatus::Modified
    }
}

/// Parse the NUL-separated records of `git status --porcelain=v2 -z --branch`.
fn parse_status(output: &[u8]) -> ParsedStatus {
    let mut parsed = ParsedStatus::default();
    let mut records = output.split(|&byte| byte == 0).filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        let line = text(record);
        if let Some(header) = line.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" if value != "(initial)" => parsed.oid = Some(value.to_string()),
                "branch.head" if value != "(detached)" => parsed.branch = Some(value.to_string()),
                "branch.upstream" => parsed.upstream = Some(value.to_string()),
                "branch.ab" => {
                    for part in value.split(' ') {
                        let count = part.get(1..).and_then(|digits| digits.parse().ok()).unwrap_or(0);
                        match part.as_bytes().first() {
                            Some(b'+') => parsed.ahead = count,
                            Some(b'-') => parsed.behind = count,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        // Entries: the path is the last space-separated field and may itself hold spaces.
        let fields_before_path = match record.first() {
            Some(b'1') => 8,
            Some(b'2') => 9,
            Some(b'u') => 10,
            Some(b'?') => 1,
            _ => continue,
        };
        let mut parts = line.splitn(fields_before_path + 1, ' ');
        let kind = parts.next().unwrap_or("");
        // An untracked record (`? path`) has no `XY` code.
        let xy = if kind == "?" { Vec::new() } else { parts.next().unwrap_or("").as_bytes().to_vec() };
        let Some(path) = parts.last() else { continue };
        let path = path.to_string();
        match kind {
            "1" => parsed.entries.push((path, None, tracked_status(&xy))),
            "2" => {
                let old = records.next().map(text);
                parsed.entries.push((path, old, GitFileStatus::Renamed));
            }
            "u" => parsed.entries.push((path, None, GitFileStatus::Conflicted)),
            _ => parsed.entries.push((path, None, GitFileStatus::Untracked)),
        }
    }
    parsed
}

/// Lines added and removed per path from `git diff --numstat -z`; `None` for a binary file.
fn parse_numstat(output: &[u8]) -> HashMap<String, (Option<u32>, Option<u32>)> {
    let mut counts = HashMap::new();
    let mut records = output.split(|&byte| byte == 0).filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        let line = text(record);
        let mut fields = line.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (fields.next(), fields.next(), fields.next()) else { continue };
        // A rename has an empty path here, then the old and the new path as records of their own.
        let path = if path.is_empty() {
            let _old = records.next();
            records.next().map(text).unwrap_or_default()
        } else {
            path.to_string()
        };
        counts.insert(path, (added.parse().ok(), removed.parse().ok()));
    }
    counts
}

/// `git log` for the latest commits; a repo's `log.showSignature` must not make it run gpg.
fn log_command(folder: &Path) -> Command {
    let mut log = git(folder);
    log.args(["log", "--no-show-signature", "-n", &MAX_COMMITS.to_string(), "-z", "--format=%H%x1f%an%x1f%ct%x1f%s"]);
    log
}

/// The default branch to measure a branch against: `origin/HEAD`, else the first of `origin/main`, `origin/master`,
/// `main` and `master` that exists.
fn default_branch(folder: &Path) -> Option<String> {
    let mut head = git(folder);
    head.args(["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"]);
    if let Some(name) =
        run_allowing(head, &[0, 1]).ok().map(|out| text(&out).trim().to_string()).filter(|n| !n.is_empty())
    {
        return Some(name);
    }
    ["origin/main", "origin/master", "main", "master"].into_iter().find_map(|name| {
        let mut probe = git(folder);
        probe.args(["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")]);
        run_allowing(probe, &[0, 1]).ok().filter(|out| !out.is_empty()).map(|_| name.to_string())
    })
}

/// The commits this branch has beyond the default branch (`log base..HEAD`), else the latest 30 of `HEAD`; whether they
/// are the branch's own comes with them. Marks those not on the upstream, or on no remote when there is no upstream.
fn commits(folder: &Path, has_upstream: bool) -> Result<(Vec<GitCommit>, bool), GitProblem> {
    let own = default_branch(folder).and_then(|base| {
        let mut log = log_command(folder);
        log.arg(format!("{base}..HEAD"));
        run(log).ok().filter(|out| !out.is_empty())
    });
    let (output, own) = match own {
        Some(output) => (output, true),
        None => (run(log_command(folder))?, false),
    };
    let unpushed: HashSet<String> = {
        let mut list = git(folder);
        list.args(["rev-list", "-n", &MAX_COMMITS.to_string()]);
        if has_upstream {
            list.arg("@{upstream}..HEAD");
        } else {
            list.args(["HEAD", "--not", "--remotes"]);
        }
        text(&run(list)?).lines().map(String::from).collect()
    };
    let mut commits = Vec::new();
    for record in output.split(|&byte| byte == 0).filter(|record| !record.is_empty()) {
        let line = text(record);
        let mut fields = line.splitn(4, '\x1f');
        let (Some(hash), Some(author), Some(time), Some(subject)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        commits.push(GitCommit {
            hash: hash.chars().take(7).collect(),
            subject: subject.to_string(),
            author: author.to_string(),
            time: time.parse().unwrap_or(0),
            unpushed: unpushed.contains(hash),
        });
    }
    Ok((commits, own))
}

/// The expanded absolute folder, or why it cannot be one.
fn expand(folder: &str, home: Option<&Path>) -> Result<PathBuf, GitProblem> {
    let expanded = app_core::settings::expand_home(Path::new(folder), home);
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Err(GitProblem::Failed { reason: "give an absolute path or start with ~".into() })
    }
}

/// One file's diff against `HEAD` (staged and unstaged changes together), or the whole file as added when it is not
/// tracked. `old_path` is where a renamed file came from; `full` lifts the "too large" limit. With `commit`, the
/// file as that commit changed it, against its first parent.
pub fn diff(
    folder: &str,
    home: Option<&Path>,
    path: &str,
    old_path: Option<&str>,
    full: bool,
    commit: Option<&str>,
) -> Result<GitDiffBody, GitProblem> {
    let dir = expand(folder, home)?;
    neutralized(&dir, || diff_once(folder, home, path, old_path, full, commit)).map(|(body, _)| body)
}

fn diff_once(
    folder: &str,
    home: Option<&Path>,
    path: &str,
    old_path: Option<&str>,
    full: bool,
    commit: Option<&str>,
) -> Result<GitDiffBody, GitProblem> {
    let folder = expand(folder, home)?;
    let relative = |name: &str| {
        let ok = !name.is_empty()
            && Path::new(name).components().all(|part| matches!(part, std::path::Component::Normal(_)));
        if ok { Ok(()) } else { Err(GitProblem::Failed { reason: format!("not a path inside the repo: {name}") }) }
    };
    relative(path)?;
    if let Some(old) = old_path {
        relative(old)?;
    }
    let mut top = git(&folder);
    top.args(["rev-parse", "--show-toplevel"]);
    let root = PathBuf::from(toplevel(&run(top)?));

    if let Some(hash) = commit {
        return commit_diff(&root, hash, path, old_path, full);
    }
    let mut head = git(&root);
    head.args(["rev-parse", "--verify", "-q", "HEAD"]);
    let base = if run_allowing(head, &[0, 1]).is_ok_and(|out| !out.is_empty()) { "HEAD" } else { EMPTY_TREE };

    let mut others = git(&root);
    others.args(["ls-files", "--others", "--exclude-standard", "-z", "--", path]);
    let untracked = !run(others)?.is_empty();
    let on_disk = std::fs::metadata(root.join(path)).ok();
    let in_base = {
        let mut probe = git(&root);
        probe.args(["cat-file", "-e", &format!("{base}:{path}")]);
        run(probe).is_ok()
    };
    if on_disk.is_none() && !in_base {
        return Err(GitProblem::Failed { reason: format!("no such file: {path}") });
    }

    // The diff holds both sides, so the bigger of the file now and the file in `HEAD` counts.
    let in_head = if in_base {
        let mut size = git(&root);
        size.args(["cat-file", "-s", &format!("{base}:{path}")]);
        text(&run(size)?).trim().parse().unwrap_or(0)
    } else {
        0
    };
    let size = on_disk.as_ref().map_or(0, |meta| meta.len()).max(in_head);
    if size > TOO_LARGE_ALWAYS || (size > TOO_LARGE && !full) {
        return Ok(GitDiffBody::TooLarge { bytes: size });
    }

    let mut command = git(&root);
    command.args(["diff", "--no-ext-diff", "--no-textconv", "-M"]);
    let output = if untracked {
        command.args(["--no-index", "--", "/dev/null", path]);
        run_allowing(command, &[0, 1])?
    } else {
        command.arg(base).arg("--");
        if let Some(old) = old_path {
            command.arg(old);
        }
        command.arg(path);
        run(command)?
    };
    Ok(body_of(text(&output), full))
}

/// A diff's text as the answer: binary and submodule files as such, the rest cut at the caps.
fn body_of(body: String, full: bool) -> GitDiffBody {
    if body.lines().any(|line| line.starts_with("Binary files ") || line == "GIT binary patch") {
        return GitDiffBody::Binary;
    }
    if body.lines().any(|line| line.starts_with("+Subproject commit") || line.starts_with("-Subproject commit")) {
        return GitDiffBody::Submodule;
    }
    let (max_bytes, max_lines) = if full { (DIFF_BYTES_FULL, DIFF_LINES_FULL) } else { (DIFF_BYTES, DIFF_LINES) };
    cut(body, max_bytes, max_lines)
}

/// Whether `hash` looks like a commit hash (and so cannot be taken for an option).
fn is_hash(hash: &str) -> bool {
    (4..=64).contains(&hash.len()) && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// One file's diff in commit `hash`, against its first parent (the whole file for a root commit).
fn commit_diff(
    root: &Path,
    hash: &str,
    path: &str,
    old_path: Option<&str>,
    full: bool,
) -> Result<GitDiffBody, GitProblem> {
    if !is_hash(hash) {
        return Err(GitProblem::Failed { reason: format!("not a commit: {hash}") });
    }
    let blob = |spec: String| {
        let mut size = git(root);
        size.args(["cat-file", "-s", &spec]);
        run(size).ok().and_then(|out| text(&out).trim().parse::<u64>().ok()).unwrap_or(0)
    };
    let size = blob(format!("{hash}:{path}")).max(blob(format!("{hash}^:{}", old_path.unwrap_or(path))));
    if size > TOO_LARGE_ALWAYS || (size > TOO_LARGE && !full) {
        return Ok(GitDiffBody::TooLarge { bytes: size });
    }
    let mut command = git(root);
    command.args(["show", "--format=", "--first-parent", "-m", "-M", "--no-ext-diff", "--no-textconv", hash, "--"]);
    if let Some(old) = old_path {
        command.arg(old);
    }
    command.arg(path);
    Ok(body_of(text(&run(command)?), full))
}

/// The status of a `--name-status` letter.
fn letter_status(letter: u8) -> GitFileStatus {
    match letter {
        b'A' => GitFileStatus::Added,
        b'D' => GitFileStatus::Deleted,
        b'R' | b'C' => GitFileStatus::Renamed,
        b'U' => GitFileStatus::Conflicted,
        _ => GitFileStatus::Modified,
    }
}

/// Path, old path and status per file from `git show --name-status -z`.
fn parse_name_status(output: &[u8]) -> Vec<(String, Option<String>, GitFileStatus)> {
    let mut records = output.split(|&byte| byte == 0).filter(|record| !record.is_empty());
    let mut entries = Vec::new();
    while let Some(code) = records.next() {
        let status = letter_status(code[0]);
        let Some(first) = records.next().map(text) else { break };
        if status == GitFileStatus::Renamed {
            let Some(new) = records.next().map(text) else { break };
            entries.push((new, Some(first), status));
        } else {
            entries.push((first, None, status));
        }
    }
    entries
}

/// Commit `hash` of the repo containing `folder`: its message and the files it changed against its first parent.
pub fn commit(folder: &str, home: Option<&Path>, hash: &str) -> Result<GitCommitDetail, GitProblem> {
    let dir = expand(folder, home)?;
    if !is_hash(hash) {
        return Err(GitProblem::Failed { reason: format!("not a commit: {hash}") });
    }
    neutralized(&dir, || commit_once(&dir, hash)).map(|(detail, _)| detail)
}

fn commit_once(dir: &Path, hash: &str) -> Result<GitCommitDetail, GitProblem> {
    let mut head = git(dir);
    head.args(["show", "-s", "--no-show-signature", "--format=%H%x00%P%x00%an%x00%ct%x00%B", hash, "--"]);
    let out = text(&run(head)?);
    let mut parts = out.splitn(5, '\0');
    let (Some(full), Some(parents), Some(author), Some(time), Some(message)) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(GitProblem::Failed { reason: "unreadable commit".into() });
    };
    let shown = ["--format=", "--first-parent", "-m", "-M", "--no-ext-diff", "--no-textconv", "-z"];
    let mut names = git(dir);
    names.arg("show").args(shown).args(["--name-status", hash, "--"]);
    let entries = parse_name_status(&run(names)?);
    let mut numstat = git(dir);
    numstat.arg("show").args(shown).args(["--numstat", hash, "--"]);
    let counts = parse_numstat(&run(numstat)?);
    let more = entries.len().saturating_sub(MAX_FILES) as u32;
    let files = entries
        .into_iter()
        .take(MAX_FILES)
        .map(|(path, old_path, status)| {
            let (added, removed) = counts.get(&path).copied().unwrap_or((None, None));
            GitFile { path, old_path, status, added, removed }
        })
        .collect();
    Ok(GitCommitDetail {
        hash: full.to_string(),
        message: message.trim_end().to_string(),
        author: author.to_string(),
        time: time.trim().parse().unwrap_or(0),
        parents: parents.split_whitespace().map(|parent| parent.chars().take(7).collect()).collect(),
        files,
        more,
    })
}

/// `body` cut after whole lines so that it holds at most `max_bytes` and `max_lines`.
fn cut(body: String, max_bytes: usize, max_lines: usize) -> GitDiffBody {
    if body.len() <= max_bytes && body.lines().count() <= max_lines {
        return GitDiffBody::Text { text: body, truncated: false };
    }
    let mut kept = String::new();
    for (count, line) in body.split_inclusive('\n').enumerate() {
        if count >= max_lines || kept.len() + line.len() > max_bytes {
            break;
        }
        kept.push_str(line);
    }
    GitDiffBody::Text { text: kept, truncated: true }
}

/// The state of the repo containing `folder` (absolute, or starting with `~` which `home` replaces). `commits` are
/// left out when `known_head` is still `HEAD`.
pub fn status(folder: &str, home: Option<&Path>, known_head: Option<&str>) -> Result<GitState, GitProblem> {
    let started = Instant::now();
    let dir = expand(folder, home)?;
    let (mut state, filters_skipped) = neutralized(&dir, || status_once(&dir, known_head))?;
    state.filters_skipped = filters_skipped;
    state.took_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
    Ok(state)
}

/// The repository root from `git rev-parse --show-toplevel` output, with Windows separators on Windows (git prints
/// `C:/…`).
fn toplevel(output: &[u8]) -> String {
    let root = text(output).trim_end_matches('\n').to_string();
    if cfg!(windows) { root.replace('/', "\\") } else { root }
}

fn status_once(folder: &Path, known_head: Option<&str>) -> Result<GitState, GitProblem> {
    let folder = folder.to_path_buf();
    let mut top = git(&folder);
    top.args(["rev-parse", "--show-toplevel"]);
    let root = toplevel(&run(top)?);

    let mut command = git(&folder);
    command.args(["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all"]);
    let parsed = parse_status(&run(command)?);
    let head: Option<String> = parsed.oid.as_ref().map(|oid| oid.chars().take(7).collect());

    let counts = if head.is_some() {
        let mut numstat = git(&folder);
        numstat.args(["diff", "HEAD", "--numstat", "-z"]);
        parse_numstat(&run(numstat)?)
    } else {
        HashMap::new()
    };
    let total = parsed.entries.len();
    let files = parsed
        .entries
        .into_iter()
        .take(MAX_FILES)
        .map(|(path, old_path, status)| {
            let (added, removed) = counts.get(&path).copied().unwrap_or((None, None));
            GitFile { path, old_path, status, added, removed }
        })
        .collect();

    let (commits, own_commits) = match &head {
        None => (Some(Vec::new()), false),
        Some(head) if known_head == Some(head.as_str()) => (None, false),
        Some(_) => {
            let (list, own) = commits(&folder, parsed.upstream.is_some())?;
            (Some(list), own)
        }
    };
    let last_commit = match &commits {
        Some(list) => list.first().map(|commit| commit.time),
        None => {
            let mut latest = git(&folder);
            latest.args(["log", "-1", "--format=%ct"]);
            text(&run(latest)?).trim().parse().ok()
        }
    };
    Ok(GitState {
        root,
        branch: parsed.branch,
        head,
        upstream: parsed.upstream,
        ahead: parsed.ahead,
        behind: parsed.behind,
        files,
        more: total.saturating_sub(MAX_FILES) as u32,
        commits,
        took_ms: 0,
        last_commit,
        filters_skipped: false,
        own_commits,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use protocol::message::{GitFile, GitFileStatus, GitProblem};

    use super::*;

    /// Run git in `dir` with an identity and no global config; panics when it fails.
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A repo on `main` with one commit holding `a.txt` ("one\n") and `c.txt`.
    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-b", "main"]);
        std::fs::write(tmp.path().join("a.txt"), "one\n").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "gone soon\n").unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-m", "first"]);
        tmp
    }

    fn state(dir: &Path) -> GitState {
        status(dir.to_str().unwrap(), None, None).unwrap()
    }

    fn file<'a>(state: &'a GitState, path: &str) -> &'a GitFile {
        state.files.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("{path} in {:?}", state.files))
    }

    /// A repo whose `*.txt` files go through a clean filter that cannot run, like a notebook filter whose python moved.
    fn repo_with_broken_filter() -> tempfile::TempDir {
        let tmp = repo();
        std::fs::write(tmp.path().join(".gitattributes"), "*.txt filter=broken\n").unwrap();
        git(tmp.path(), &["config", "filter.broken.clean", "/nonexistent/clean"]);
        git(tmp.path(), &["config", "filter.broken.required", "true"]);
        std::fs::write(tmp.path().join("a.txt"), "one\ntwo\n").unwrap();
        tmp
    }

    #[test]
    fn a_broken_filter_is_skipped_and_said_so() {
        let tmp = repo_with_broken_filter();
        let state = state(tmp.path());
        assert!(state.filters_skipped);
        assert_eq!(file(&state, "a.txt").status, GitFileStatus::Modified);
        assert!(!super::NEUTRAL_FILTERS.with(|names| !names.borrow().is_empty()), "the override is cleared again");
        let body = diff_of(tmp.path(), "a.txt", None, false).unwrap();
        assert!(matches!(body, GitDiffBody::Text { ref text, .. } if text.contains("+two")), "{body:?}");
    }

    /// A repo with a tracked file modified under a filter whose clean command creates `marker`.
    fn repo_with_marker_filter(marker: &Path) -> tempfile::TempDir {
        let tmp = repo();
        std::fs::write(tmp.path().join(".gitattributes"), "*.txt filter=spy\n").unwrap();
        git(tmp.path(), &["config", "filter.spy.clean", &format!("sh -c 'touch {}; cat'", marker.display())]);
        std::fs::write(tmp.path().join("a.txt"), "one\ntwo\n").unwrap();
        tmp
    }

    #[test]
    fn a_repos_filter_never_runs() {
        let out = tempfile::tempdir().unwrap();
        let marker = out.path().join("ran");
        let tmp = repo_with_marker_filter(&marker);
        let state = state(tmp.path());
        diff_of(tmp.path(), "a.txt", None, false).unwrap();
        assert!(!marker.exists(), "the clean filter ran");
        assert!(state.filters_skipped);
    }

    #[test]
    fn a_repos_gpg_program_never_runs() {
        let out = tempfile::tempdir().unwrap();
        let marker = out.path().join("ran");
        let tmp = repo();
        let script = out.path().join("gpg.sh");
        std::fs::write(&script, format!("#!/bin/sh\ntouch {}\nexit 1\n", marker.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // A commit carrying a (fake) signature header, which `log.showSignature` would verify with `gpg.program`.
        let head = head_of(tmp.path());
        let raw = Command::new("git").arg("-C").arg(tmp.path()).args(["cat-file", "commit", &head]).output().unwrap();
        let raw = String::from_utf8(raw.stdout).unwrap();
        let (headers, message) = raw.split_once("\n\n").unwrap();
        let signed = format!(
            "{headers}\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n x\n -----END PGP SIGNATURE-----\n\n{message}"
        );
        let signed_file = out.path().join("signed");
        std::fs::write(&signed_file, signed).unwrap();
        let hash = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["hash-object", "-t", "commit", "-w", "--literally", "--stdin"])
            .stdin(std::fs::File::open(&signed_file).unwrap())
            .output()
            .unwrap();
        let hash = String::from_utf8(hash.stdout).unwrap().trim().to_string();
        git(tmp.path(), &["update-ref", "HEAD", &hash]);
        git(tmp.path(), &["config", "log.showSignature", "true"]);
        git(tmp.path(), &["config", "gpg.program", script.to_str().unwrap()]);
        state(tmp.path());
        commit(tmp.path().to_str().unwrap(), None, &hash).unwrap();
        assert!(!marker.exists(), "gpg.program ran");
    }

    #[test]
    fn a_healthy_repo_does_not_skip_filters() {
        let tmp = repo();
        assert!(!state(tmp.path()).filters_skipped);
    }

    #[test]
    fn only_the_repos_own_filters_count() {
        let listing = "system\tfilter.lfs.clean git-lfs clean -- %f\n\
                       global\tfilter.lfs.process git-lfs filter-process\n\
                       local\tfilter.spy.clean sh -c 'touch x'\n\
                       worktree\tfilter.wt.smudge cat\n\
                       command\tfilter.cmd.clean cat\n";
        assert_eq!(repo_filter_names(listing), ["cmd", "spy", "wt"]);
    }

    /// The full hash of `HEAD`.
    fn head_of(dir: &Path) -> String {
        let out = Command::new("git").arg("-C").arg(dir).args(["rev-parse", "HEAD"]).output().unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn commit_has_message_files_and_counts() {
        let tmp = repo();
        std::fs::write(tmp.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(tmp.path().join("new.txt"), "n\n").unwrap();
        git(tmp.path(), &["rm", "-q", "c.txt"]);
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-m", "second thing", "-m", "the body\nsecond line"]);
        let hash = head_of(tmp.path());
        let detail = commit(tmp.path().to_str().unwrap(), None, &hash[..7]).unwrap();
        assert_eq!(detail.hash, hash);
        assert_eq!(detail.message, "second thing\n\nthe body\nsecond line");
        assert_eq!(detail.author, "T");
        assert_eq!(detail.parents.len(), 1);
        let a = detail.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!((a.status, a.added, a.removed), (GitFileStatus::Modified, Some(1), Some(0)));
        assert_eq!(detail.files.iter().find(|f| f.path == "new.txt").unwrap().status, GitFileStatus::Added);
        assert_eq!(detail.files.iter().find(|f| f.path == "c.txt").unwrap().status, GitFileStatus::Deleted);
    }

    #[test]
    fn commit_of_the_first_commit_and_a_rename() {
        let tmp = repo();
        let first = commit(tmp.path().to_str().unwrap(), None, &head_of(tmp.path())).unwrap();
        assert!(first.parents.is_empty());
        assert_eq!(first.files.len(), 2);
        git(tmp.path(), &["mv", "a.txt", "b.txt"]);
        git(tmp.path(), &["commit", "-m", "rename"]);
        let renamed = commit(tmp.path().to_str().unwrap(), None, &head_of(tmp.path())).unwrap();
        assert_eq!(renamed.files.len(), 1);
        assert_eq!(renamed.files[0].path, "b.txt");
        assert_eq!(renamed.files[0].old_path.as_deref(), Some("a.txt"));
        assert_eq!(renamed.files[0].status, GitFileStatus::Renamed);
    }

    #[test]
    fn commit_refuses_what_is_not_a_hash() {
        let tmp = repo();
        for bad in ["--output=/tmp/x", "HEAD", "zz", ""] {
            assert!(matches!(commit(tmp.path().to_str().unwrap(), None, bad), Err(GitProblem::Failed { .. })), "{bad}");
        }
    }

    #[test]
    fn a_commits_file_diff_is_against_its_parent_not_the_working_tree() {
        let tmp = repo();
        std::fs::write(tmp.path().join("a.txt"), "one\ntwo\n").unwrap();
        git(tmp.path(), &["commit", "-qam", "two"]);
        let hash = head_of(tmp.path());
        std::fs::write(tmp.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let body = diff(tmp.path().to_str().unwrap(), None, "a.txt", None, false, Some(&hash)).unwrap();
        let GitDiffBody::Text { text, .. } = body else { panic!("{body:?}") };
        assert!(text.contains("+two") && !text.contains("three"), "{text}");
        let first = head_of_first(tmp.path());
        let body = diff(tmp.path().to_str().unwrap(), None, "a.txt", None, false, Some(&first)).unwrap();
        let GitDiffBody::Text { text, .. } = body else { panic!("{body:?}") };
        assert!(text.contains("+one"), "a root commit shows the whole file: {text}");
    }

    fn head_of_first(dir: &Path) -> String {
        let out =
            Command::new("git").arg("-C").arg(dir).args(["rev-list", "--max-parents=0", "HEAD"]).output().unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn clean_repo_has_branch_and_no_files() {
        let tmp = repo();
        let state = state(tmp.path());
        assert_eq!(state.branch.as_deref(), Some("main"));
        assert_eq!(state.head.as_ref().map(String::len), Some(7));
        assert!(state.files.is_empty());
        assert_eq!(state.more, 0);
        assert_eq!(PathBuf::from(&state.root).canonicalize().unwrap(), tmp.path().canonicalize().unwrap());
        assert!(!cfg!(windows) || !state.root.contains('/'), "{}", state.root);
        assert!(state.last_commit.is_some());
    }

    #[test]
    fn modified_added_deleted_untracked_are_classified() {
        let tmp = repo();
        std::fs::write(tmp.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "new\n").unwrap();
        git(tmp.path(), &["add", "b.txt"]);
        std::fs::remove_file(tmp.path().join("c.txt")).unwrap();
        std::fs::write(tmp.path().join("d.txt"), "loose\n").unwrap();
        let state = state(tmp.path());
        let a = file(&state, "a.txt");
        assert_eq!((a.status, a.added, a.removed), (GitFileStatus::Modified, Some(1), Some(0)));
        assert_eq!(file(&state, "b.txt").status, GitFileStatus::Added);
        assert_eq!(file(&state, "c.txt").status, GitFileStatus::Deleted);
        let d = file(&state, "d.txt");
        assert_eq!((d.status, d.added, d.removed), (GitFileStatus::Untracked, None, None));
    }

    #[test]
    fn rename_keeps_old_path() {
        let tmp = repo();
        git(tmp.path(), &["mv", "a.txt", "moved.txt"]);
        let state = state(tmp.path());
        let moved = file(&state, "moved.txt");
        assert_eq!((moved.status, moved.old_path.as_deref()), (GitFileStatus::Renamed, Some("a.txt")));
    }

    #[test]
    fn odd_file_names_survive() {
        let tmp = repo();
        // Windows file names cannot hold a line break.
        let names: &[&str] = if cfg!(windows) {
            &["a b.txt", "héllo wörld.txt"]
        } else {
            &["a b.txt", "héllo wörld.txt", "line\nbreak.txt"]
        };
        for name in names {
            std::fs::write(tmp.path().join(name), "x\n").unwrap();
        }
        let state = state(tmp.path());
        for name in names {
            assert_eq!(file(&state, name).status, GitFileStatus::Untracked);
        }
    }

    #[test]
    fn no_commits_yet() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-b", "main"]);
        std::fs::write(tmp.path().join("x.txt"), "x\n").unwrap();
        let state = state(tmp.path());
        assert_eq!((state.branch.as_deref(), state.head.clone()), (Some("main"), None));
        assert_eq!(state.commits, Some(vec![]));
        assert_eq!(file(&state, "x.txt").status, GitFileStatus::Untracked);
        assert_eq!(state.last_commit, None);
    }

    #[test]
    fn detached_head_has_no_branch() {
        let tmp = repo();
        git(tmp.path(), &["checkout", "--detach"]);
        let state = state(tmp.path());
        assert_eq!(state.branch, None);
        assert!(state.head.is_some());
    }

    #[test]
    fn upstream_ahead_behind_and_unpushed() {
        let tmp = repo();
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "--bare", "-b", "main"]);
        let remote_url = remote.path().to_str().unwrap();
        git(tmp.path(), &["remote", "add", "origin", remote_url]);
        git(tmp.path(), &["push", "-u", "origin", "main"]);
        // Another clone pushes one commit; this repo fetches it and adds one of its own.
        let other = tempfile::tempdir().unwrap();
        git(other.path(), &["clone", remote_url, "."]);
        std::fs::write(other.path().join("o.txt"), "o\n").unwrap();
        git(other.path(), &["add", "."]);
        git(other.path(), &["commit", "-m", "theirs"]);
        git(other.path(), &["push"]);
        git(tmp.path(), &["fetch"]);
        std::fs::write(tmp.path().join("m.txt"), "m\n").unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-m", "mine"]);
        let state = state(tmp.path());
        assert_eq!(state.upstream.as_deref(), Some("origin/main"));
        assert_eq!((state.ahead, state.behind), (1, 1));
        // `main` is measured against `origin/main`: only what it has beyond it is listed.
        assert!(state.own_commits);
        let commits = state.commits.unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].subject, "mine");
        assert!(commits[0].unpushed);
    }

    #[test]
    fn a_feature_branch_lists_only_its_own_commits() {
        let tmp = repo();
        git(tmp.path(), &["checkout", "-b", "feat/x"]);
        for name in ["one", "two"] {
            std::fs::write(tmp.path().join(format!("{name}.txt")), name).unwrap();
            git(tmp.path(), &["add", "."]);
            git(tmp.path(), &["commit", "-m", name]);
        }
        let state = state(tmp.path());
        assert!(state.own_commits);
        let subjects: Vec<&str> = state.commits.as_ref().unwrap().iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["two", "one"]);
        assert!(state.commits.unwrap().iter().all(|c| c.unpushed), "on no remote yet");
    }

    #[test]
    fn the_default_branch_itself_falls_back_to_its_latest_commits_and_pushed_ones_are_not_unpushed() {
        let tmp = repo();
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "--bare", "-b", "main"]);
        git(tmp.path(), &["remote", "add", "origin", remote.path().to_str().unwrap()]);
        git(tmp.path(), &["push", "origin", "main"]);
        git(tmp.path(), &["checkout", "-b", "feat/y"]);
        let state = state(tmp.path());
        assert!(!state.own_commits, "feat/y has nothing of its own yet");
        let commits = state.commits.unwrap();
        assert_eq!(commits.len(), 1);
        assert!(!commits[0].unpushed, "the commit is on origin/main");
    }

    #[test]
    fn without_an_upstream_every_commit_is_unpushed() {
        let tmp = repo();
        let state = state(tmp.path());
        assert!(!state.own_commits, "main has no default branch to be measured against beyond itself");
        let commits = state.commits.unwrap();
        assert_eq!(commits.len(), 1);
        assert!(commits[0].unpushed);
        assert_eq!((commits[0].subject.as_str(), commits[0].author.as_str()), ("first", "T"));
    }

    #[test]
    fn not_a_repo_is_not_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(status(tmp.path().to_str().unwrap(), None, None), Err(GitProblem::NotRepo));
    }

    #[test]
    fn relative_path_is_refused() {
        assert!(matches!(status("relative/dir", None, None), Err(GitProblem::Failed { .. })));
    }

    #[test]
    fn file_list_is_capped_with_more() {
        let tmp = repo();
        for i in 0..2005 {
            std::fs::write(tmp.path().join(format!("f{i:04}.txt")), "x\n").unwrap();
        }
        let state = state(tmp.path());
        assert_eq!((state.files.len(), state.more), (2000, 5));
    }

    #[test]
    fn status_works_with_index_lock_present_and_leaves_it() {
        let tmp = repo();
        let lock = tmp.path().join(".git/index.lock");
        std::fs::write(&lock, "").unwrap();
        std::fs::write(tmp.path().join("a.txt"), "changed\n").unwrap();
        let state = state(tmp.path());
        assert_eq!(file(&state, "a.txt").status, GitFileStatus::Modified);
        assert!(lock.exists());
    }

    #[test]
    fn commits_only_when_head_changed() {
        let tmp = repo();
        let head = state(tmp.path()).head.unwrap();
        let same = status(tmp.path().to_str().unwrap(), None, Some(&head)).unwrap();
        assert_eq!(same.commits, None);
        let other = status(tmp.path().to_str().unwrap(), None, Some("0000000")).unwrap();
        assert_eq!(other.commits.map(|c| c.len()), Some(1));
    }

    #[test]
    fn tilde_is_expanded() {
        let tmp = repo();
        let state = status("~", Some(tmp.path()), None).unwrap();
        assert_eq!(state.branch.as_deref(), Some("main"));
    }

    fn diff_of(dir: &Path, path: &str, old_path: Option<&str>, full: bool) -> Result<GitDiffBody, GitProblem> {
        diff(dir.to_str().unwrap(), None, path, old_path, full, None)
    }

    fn text_of(body: GitDiffBody) -> (String, bool) {
        match body {
            GitDiffBody::Text { text, truncated } => (text, truncated),
            other => panic!("not a text diff: {other:?}"),
        }
    }

    #[test]
    fn modified_file_diff_is_combined_staged_and_unstaged() {
        let tmp = repo();
        std::fs::write(tmp.path().join("a.txt"), "one\ntwo\n").unwrap();
        git(tmp.path(), &["add", "a.txt"]);
        std::fs::write(tmp.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let (text, truncated) = text_of(diff_of(tmp.path(), "a.txt", None, false).unwrap());
        assert!(text.contains("+two") && text.contains("+three"), "{text}");
        assert!(!truncated);
    }

    #[test]
    fn untracked_file_is_all_added() {
        let tmp = repo();
        std::fs::write(tmp.path().join("loose.txt"), "l1\nl2\n").unwrap();
        let (text, _) = text_of(diff_of(tmp.path(), "loose.txt", None, false).unwrap());
        assert!(text.contains("+l1") && text.contains("+l2"), "{text}");
    }

    #[test]
    fn deleted_file_diff() {
        let tmp = repo();
        std::fs::remove_file(tmp.path().join("c.txt")).unwrap();
        let (text, _) = text_of(diff_of(tmp.path(), "c.txt", None, false).unwrap());
        assert!(text.contains("-gone soon"), "{text}");
    }

    #[test]
    fn rename_diff_uses_both_paths() {
        let tmp = repo();
        git(tmp.path(), &["mv", "a.txt", "moved.txt"]);
        let (text, _) = text_of(diff_of(tmp.path(), "moved.txt", Some("a.txt"), false).unwrap());
        assert!(text.contains("rename from a.txt") && text.contains("rename to moved.txt"), "{text}");
    }

    #[test]
    fn binary_file_is_binary() {
        let tmp = repo();
        std::fs::write(tmp.path().join("pic.bin"), [0u8, 159, 146, 150, 0, 1]).unwrap();
        assert_eq!(diff_of(tmp.path(), "pic.bin", None, false), Ok(GitDiffBody::Binary));
        git(tmp.path(), &["add", "pic.bin"]);
        std::fs::write(tmp.path().join("pic.bin"), [0u8, 1, 2, 3]).unwrap();
        assert_eq!(diff_of(tmp.path(), "pic.bin", None, false), Ok(GitDiffBody::Binary));
    }

    #[test]
    fn no_commits_diff_against_empty_tree() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-b", "main"]);
        std::fs::write(tmp.path().join("x.txt"), "x\n").unwrap();
        git(tmp.path(), &["add", "x.txt"]);
        let (text, _) = text_of(diff_of(tmp.path(), "x.txt", None, false).unwrap());
        assert!(text.contains("+x"), "{text}");
        std::fs::write(tmp.path().join("y.txt"), "y\n").unwrap();
        let (text, _) = text_of(diff_of(tmp.path(), "y.txt", None, false).unwrap());
        assert!(text.contains("+y"), "{text}");
    }

    #[test]
    fn diff_over_caps_is_truncated_at_a_line_boundary() {
        let tmp = repo();
        let big: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
        assert!(big.len() > 2 * 1024 * 1024);
        std::fs::write(tmp.path().join("a.txt"), big).unwrap();
        let (text, truncated) = text_of(diff_of(tmp.path(), "a.txt", None, false).unwrap());
        assert!(truncated);
        assert!(text.ends_with('\n'));
        assert!(text.len() <= 1024 * 1024 && text.lines().count() <= 20_000, "{} bytes", text.len());
    }

    #[test]
    fn large_file_is_too_large_until_full() {
        let tmp = repo();
        let big = "x".repeat(5 * 1024 * 1024);
        std::fs::write(tmp.path().join("a.txt"), big).unwrap();
        assert!(
            matches!(diff_of(tmp.path(), "a.txt", None, false), Ok(GitDiffBody::TooLarge { bytes }) if bytes > 4 * 1024 * 1024)
        );
        assert!(matches!(diff_of(tmp.path(), "a.txt", None, true), Ok(GitDiffBody::Text { .. })));
    }

    #[test]
    fn non_utf8_bytes_do_not_fail() {
        let tmp = repo();
        std::fs::write(tmp.path().join("latin.txt"), b"caf\xe9\n").unwrap();
        let (text, _) = text_of(diff_of(tmp.path(), "latin.txt", None, false).unwrap());
        assert!(text.contains("caf"), "{text}");
    }

    #[test]
    fn missing_path_is_failed_not_panic() {
        let tmp = repo();
        assert!(matches!(diff_of(tmp.path(), "nope.txt", None, false), Err(GitProblem::Failed { .. })));
        assert!(matches!(diff_of(tmp.path(), "../escape", None, false), Err(GitProblem::Failed { .. })));
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(diff_of(plain.path(), "a.txt", None, false), Err(GitProblem::NotRepo));
    }

    #[test]
    fn pathspec_characters_in_a_file_name_are_literal() {
        let tmp = repo();
        std::fs::write(tmp.path().join("i.txt"), "one\n").unwrap();
        std::fs::write(tmp.path().join("[id].txt"), "bracket\n").unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-m", "two files"]);
        std::fs::write(tmp.path().join("i.txt"), "one\nchanged i\n").unwrap();
        std::fs::write(tmp.path().join("[id].txt"), "bracket\nchanged bracket\n").unwrap();
        let (text, _) = text_of(diff_of(tmp.path(), "[id].txt", None, false).unwrap());
        assert!(text.contains("+changed bracket"), "{text}");
        assert!(!text.contains("changed i\n") && !text.contains("i.txt"), "{text}");
    }

    #[test]
    fn a_new_folder_lists_its_files_and_each_has_a_diff() {
        let tmp = repo();
        std::fs::create_dir(tmp.path().join("newdir")).unwrap();
        std::fs::write(tmp.path().join("newdir/a.txt"), "aa\n").unwrap();
        std::fs::write(tmp.path().join("newdir/b.txt"), "bb\n").unwrap();
        let state = state(tmp.path());
        let paths: Vec<&str> = state.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["newdir/a.txt", "newdir/b.txt"]);
        let (text, _) = text_of(diff_of(tmp.path(), "newdir/a.txt", None, false).unwrap());
        assert!(text.contains("+aa"), "{text}");
    }

    #[test]
    fn git_commands_are_hardened_against_repo_config_and_the_environment() {
        let command = super::git(Path::new("/tmp"));
        let args: Vec<String> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        for wanted in ["--literal-pathspecs", "--no-optional-locks", "core.fsmonitor=false", "core.hooksPath=/dev/null"]
        {
            assert!(args.iter().any(|a| a == wanted), "{wanted} in {args:?}");
        }
        let removed: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for wanted in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_EXTERNAL_DIFF", "GIT_PAGER"] {
            assert!(removed.iter().any(|r| r == wanted), "{wanted} removed in {removed:?}");
        }
        let log: Vec<String> =
            log_command(Path::new("/tmp")).get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(log.iter().any(|a| a == "--no-show-signature"), "{log:?}");
    }

    #[test]
    fn a_hung_command_is_killed_after_the_timeout() {
        let started = Instant::now();
        let mut sleeper = Command::new("sleep");
        sleeper.arg("5");
        let result = run_limited(sleeper, &[0], std::time::Duration::from_millis(200), 1024);
        assert!(matches!(&result, Err(GitProblem::Failed { reason }) if reason.contains("too long")), "{result:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }

    #[test]
    fn output_over_the_limit_is_refused_and_small_output_passes() {
        let mut big = Command::new("sh");
        big.args(["-c", "yes | head -c 200000"]);
        let result = run_limited(big, &[0], std::time::Duration::from_secs(5), 1000);
        assert!(matches!(&result, Err(GitProblem::Failed { reason }) if reason.contains("too large")), "{result:?}");
        let mut small = Command::new("sh");
        small.args(["-c", "printf hi"]);
        assert_eq!(run_limited(small, &[0], std::time::Duration::from_secs(5), 1000), Ok(b"hi".to_vec()));
    }

    #[test]
    fn a_small_file_that_was_huge_in_head_is_too_large_until_full() {
        let tmp = repo();
        std::fs::write(tmp.path().join("a.txt"), "x".repeat(5 * 1024 * 1024)).unwrap();
        git(tmp.path(), &["commit", "-am", "huge"]);
        std::fs::write(tmp.path().join("a.txt"), "tiny\n").unwrap();
        assert!(
            matches!(diff_of(tmp.path(), "a.txt", None, false), Ok(GitDiffBody::TooLarge { bytes }) if bytes > 4 * 1024 * 1024)
        );
        assert!(matches!(diff_of(tmp.path(), "a.txt", None, true), Ok(GitDiffBody::Text { .. })));
    }
}
