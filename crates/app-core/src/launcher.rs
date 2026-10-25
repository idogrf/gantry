//! The directory launcher's list: one list of folders across this machine and the SSH hosts (open now, favorites,
//! then recent, newest first), fuzzy filtered, and the typed text that opens a path instead.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use protocol::host::{HostId, project_key, split_project_key};
use protocol::message::SessionInfo;

use crate::fuzzy::Fuzzy;
use crate::session_tree::{Tone, display_path};

/// A pane of the workspace, as the launcher sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenPane {
    /// The host it runs on.
    pub host: HostId,
    /// Directory it was started in.
    pub cwd: PathBuf,
    /// It runs Claude Code (else a shell).
    pub claude: bool,
    /// Tone of its session; ignored for shells.
    pub tone: Tone,
}

/// A folder with live panes in the workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenFolder {
    /// The host it is on.
    pub host: HostId,
    /// The folder.
    pub path: PathBuf,
    /// The most urgent tone among its Claude panes; `None` when it has only shells.
    pub tone: Option<Tone>,
}

fn urgency(tone: Tone) -> u8 {
    match tone {
        Tone::NeedsInput => 0,
        Tone::Working => 1,
        Tone::Done | Tone::Unread => 2,
        _ => 3,
    }
}

/// One entry per folder on a host, in the order they first appear in `panes`.
pub fn open_folders(panes: &[OpenPane]) -> Vec<OpenFolder> {
    let mut folders: Vec<OpenFolder> = Vec::new();
    for pane in panes {
        let at = match folders.iter().position(|folder| folder.host == pane.host && folder.path == pane.cwd) {
            Some(at) => at,
            None => {
                folders.push(OpenFolder { host: pane.host.clone(), path: pane.cwd.clone(), tone: None });
                folders.len() - 1
            }
        };
        let folder = &mut folders[at];
        if pane.claude && folder.tone.is_none_or(|tone| urgency(pane.tone) < urgency(tone)) {
            folder.tone = Some(pane.tone);
        }
    }
    folders
}

/// One launcher entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LauncherItem {
    /// A folder with panes open now.
    Open(OpenFolder),
    /// A directory to open.
    Dir {
        /// The host it is on.
        host: HostId,
        /// The directory.
        path: PathBuf,
        /// Pinned in the settings (drawn with a star).
        favorite: bool,
        /// Whether it exists; a favorite of this machine whose folder is gone cannot be opened.
        exists: bool,
    },
    /// "<host>: type a path on this host"; picking it fills the field with `<host>:~/`.
    HostHint(HostId),
}

impl LauncherItem {
    /// The host the entry is about.
    pub fn host(&self) -> &HostId {
        match self {
            LauncherItem::Open(folder) => &folder.host,
            LauncherItem::Dir { host, .. } | LauncherItem::HostHint(host) => host,
        }
    }

    /// The folder it opens; none for a host hint.
    pub fn folder(&self) -> Option<&PathBuf> {
        match self {
            LauncherItem::Open(folder) => Some(&folder.path),
            LauncherItem::Dir { path, .. } => Some(path),
            LauncherItem::HostHint(_) => None,
        }
    }
}

/// The entry below (`down`) or above `from`; `from` itself at the end of the list.
pub fn step(items: &[LauncherItem], from: usize, down: bool) -> usize {
    if down { (from + 1).min(items.len().saturating_sub(1)) } else { from.saturating_sub(1) }
}

/// The key a favorite on `host` is saved under in the settings: the folder itself for this machine,
/// `ssh://<host>/<folder>` otherwise.
pub fn favorite_key(host: &HostId, dir: &Path) -> PathBuf {
    project_key(host, dir)
}

/// A folder of a host's recent sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentFolder {
    /// The host it is on.
    pub host: HostId,
    /// The folder.
    pub path: PathBuf,
    /// When a session there was last active.
    pub last_active_ms: u64,
}

/// A recent folder of an SSH host, as the layout remembers it while the host is away.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFolder {
    /// The folder.
    pub path: PathBuf,
    /// When a session there was last active.
    pub last_active_ms: u64,
}

/// The existing folders of `sessions`, newest session per (host, folder), newest first; plus the `remembered` folders of
/// the hosts none of `sessions` belongs to (they are away). Only this machine and the hosts in `listed`.
pub fn recent_folders(
    sessions: &[&SessionInfo],
    remembered: &BTreeMap<HostId, Vec<HostFolder>>,
    listed: &[HostId],
) -> Vec<RecentFolder> {
    let shown = |host: &HostId| host.is_local() || listed.contains(host);
    let mut latest: HashMap<(&HostId, &Path), u64> = HashMap::new();
    for session in sessions.iter().filter(|s| s.cwd_exists && shown(&s.host)) {
        let last = latest.entry((&session.host, session.cwd.as_path())).or_default();
        *last = (*last).max(session.last_active_ms);
    }
    let mut folders: Vec<RecentFolder> = latest
        .into_iter()
        .map(|((host, path), last_active_ms)| RecentFolder {
            host: host.clone(),
            path: path.to_path_buf(),
            last_active_ms,
        })
        .collect();
    for (host, kept) in remembered {
        if !shown(host) || sessions.iter().any(|session| session.host == *host) {
            continue;
        }
        folders.extend(kept.iter().map(|folder| RecentFolder {
            host: host.clone(),
            path: folder.path.clone(),
            last_active_ms: folder.last_active_ms,
        }));
    }
    folders.sort_by(|a, b| {
        b.last_active_ms.cmp(&a.last_active_ms).then_with(|| (&a.host, &a.path).cmp(&(&b.host, &b.path)))
    });
    folders
}

/// What the typed text means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Typed {
    /// Words that filter the list.
    Filter,
    /// A path on a host: it is completed and can be opened as typed.
    Path {
        /// The host the path is on.
        host: HostId,
        /// The path as typed (`~/` for a bare `<host>:`).
        path: String,
    },
}

/// `/…` and `~…` (and on Windows `C:\…`) are paths on this machine; `<host>:<path>` is a path on that host when it is
/// one of `hosts` (the `sshHosts` names, matched ignoring case; a path starting with neither `/` nor `~` is under `~/`,
/// like scp); anything else filters. On Windows a local path's `\` become `/`, which completion splits on.
pub fn typed(text: &str, hosts: &[HostId]) -> Typed {
    if cfg!(windows) && (text.starts_with('~') || is_drive_path(text)) {
        return Typed::Path { host: HostId::local(), path: text.replace('\\', "/") };
    }
    if text.starts_with(['/', '~']) {
        return Typed::Path { host: HostId::local(), path: text.to_string() };
    }
    let Some((name, path)) = text.split_once(':') else { return Typed::Filter };
    let Some(host) = hosts.iter().find(|host| host.name().eq_ignore_ascii_case(name)) else { return Typed::Filter };
    let path = match path {
        "" => "~/".to_string(),
        path if path.starts_with(['/', '~']) => path.to_string(),
        path => format!("~/{path}"),
    };
    Typed::Path { host: host.clone(), path }
}

/// Whether `text` starts with a Windows drive (`C:`, then nothing, `\` or `/`).
fn is_drive_path(text: &str) -> bool {
    match text.as_bytes() {
        [drive, b':'] => drive.is_ascii_alphabetic(),
        [drive, b':', b'\\' | b'/', ..] => drive.is_ascii_alphabetic(),
        _ => false,
    }
}

/// What a row is matched against: the displayed path on this machine, `<host>:<path>` on a host.
fn row_text(host: &HostId, path: &Path, home: Option<&Path>) -> String {
    if host.is_local() { display_path(path, home) } else { format!("{}:{}", host.name(), path.display()) }
}

/// One list for every host: open folders (in the given order), then favorites (settings order), then recent folders
/// (by match score, then recency), each (host, folder) once and the first of those groups it is in wins. The query
/// filters by `row_text`. While it is a bare prefix of a host name (no `:` or `/`), a hint row per such host leads.
/// `hosts` are the `sshHosts` names; rows of any other remote host are left out.
pub fn items(
    open: &[OpenFolder],
    recent: &[RecentFolder],
    query: &str,
    home: Option<&Path>,
    favorites: &[PathBuf],
    hosts: &[HostId],
) -> Vec<LauncherItem> {
    let shown = |host: &HostId| host.is_local() || hosts.contains(host);
    let open: Vec<&OpenFolder> = open.iter().filter(|folder| shown(&folder.host)).collect();
    let favorites: Vec<(HostId, PathBuf)> =
        favorites.iter().map(|favorite| split_project_key(favorite)).filter(|(host, _)| shown(host)).collect();
    let recent: Vec<&RecentFolder> = recent.iter().filter(|folder| shown(&folder.host)).collect();
    let is_open = |host: &HostId, path: &Path| open.iter().any(|folder| folder.host == *host && folder.path == path);
    let is_favorite = |host: &HostId, path: &Path| favorites.iter().any(|(h, p)| h == host && p == path);
    let texts = open
        .iter()
        .map(|folder| row_text(&folder.host, &folder.path, home))
        .chain(favorites.iter().map(|(host, path)| row_text(host, path, home)))
        .chain(recent.iter().map(|folder| row_text(&folder.host, &folder.path, home)));
    let mut fuzzy = Fuzzy::new(query, texts);
    let mut score = |host: &HostId, path: &Path| match &mut fuzzy {
        None => Some(0),
        Some(fuzzy) => fuzzy.score(&row_text(host, path, home)),
    };
    let mut list: Vec<LauncherItem> = Vec::new();
    if !query.is_empty() && !query.contains([':', '/']) {
        let query = query.to_lowercase();
        list.extend(
            hosts
                .iter()
                .filter(|host| host.name().to_lowercase().starts_with(&query))
                .map(|host| LauncherItem::HostHint(host.clone())),
        );
    }
    list.extend(
        open.iter()
            .filter(|folder| score(&folder.host, &folder.path).is_some())
            .map(|folder| LauncherItem::Open((*folder).clone())),
    );
    let mut seen: Vec<(&HostId, &Path)> = Vec::new();
    for (host, path) in &favorites {
        if is_open(host, path) || seen.contains(&(host, path.as_path())) || score(host, path).is_none() {
            continue;
        }
        seen.push((host, path));
        let exists = !host.is_local() || path.is_dir();
        list.push(LauncherItem::Dir { host: host.clone(), path: path.clone(), favorite: true, exists });
    }
    let mut ranked: Vec<(&RecentFolder, u32)> = recent
        .into_iter()
        .filter(|folder| !is_open(&folder.host, &folder.path) && !is_favorite(&folder.host, &folder.path))
        .filter_map(|folder| Some((folder, score(&folder.host, &folder.path)?)))
        .collect();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then(b.0.last_active_ms.cmp(&a.0.last_active_ms))
            .then_with(|| (&a.0.host, &a.0.path).cmp(&(&b.0.host, &b.0.path)))
    });
    list.extend(ranked.into_iter().map(|(folder, _)| LauncherItem::Dir {
        host: folder.host.clone(),
        path: folder.path.clone(),
        favorite: false,
        exists: true,
    }));
    list
}

/// How many folders the welcome screen lists.
pub const WELCOME_ROWS: usize = 5;

/// The welcome screen's folders: favorites, then recent folders (newest first), each (host, folder) once, at most
/// `WELCOME_ROWS`; no open folders and no host hints.
pub fn welcome_items(
    recent: &[RecentFolder],
    home: Option<&Path>,
    favorites: &[PathBuf],
    hosts: &[HostId],
) -> Vec<LauncherItem> {
    let mut list = items(&[], recent, "", home, favorites, hosts);
    list.truncate(WELCOME_ROWS);
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_tree::Tone;
    use protocol::message::SessionState;

    fn info(host: &HostId, cwd: &str, last_active_ms: u64, cwd_exists: bool) -> SessionInfo {
        SessionInfo {
            id: format!("{host:?}{cwd}{last_active_ms}"),
            cwd: cwd.into(),
            title: String::new(),
            last_active_ms,
            state: SessionState::Closed,
            cwd_exists,
            status: None,
            status_since_ms: 0,
            container: None,
            claude_dir: None,
            last_prompt: None,
            host: host.clone(),
        }
    }

    fn local() -> HostId {
        HostId::local()
    }

    fn devbox() -> HostId {
        HostId::named("devbox")
    }

    fn recent(host: &HostId, path: &str, last_active_ms: u64) -> RecentFolder {
        RecentFolder { host: host.clone(), path: path.into(), last_active_ms }
    }

    fn open(host: &HostId, path: &str) -> OpenFolder {
        OpenFolder { host: host.clone(), path: path.into(), tone: None }
    }

    fn pane(cwd: &str, claude: bool, tone: Tone) -> OpenPane {
        OpenPane { host: local(), cwd: cwd.into(), claude, tone }
    }

    /// `open`, `fav`, `dir` or `hint`, then `host:path`.
    fn rows(items: &[LauncherItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                LauncherItem::Open(folder) => format!("open {}:{}", folder.host.name(), folder.path.display()),
                LauncherItem::Dir { host, path, favorite, .. } => {
                    format!("{} {}:{}", if *favorite { "fav" } else { "dir" }, host.name(), path.display())
                }
                LauncherItem::HostHint(host) => format!("hint {}", host.name()),
            })
            .collect()
    }

    #[test]
    fn one_list_open_then_favorites_then_recent_across_hosts() {
        let favorites = [PathBuf::from("ssh://devbox/work")];
        let recent = [recent(&local(), "/b", 10), recent(&devbox(), "/src/x", 20)];
        let list = items(&[open(&local(), "/a")], &recent, "", None, &favorites, &[devbox()]);
        assert_eq!(rows(&list), vec!["open local:/a", "fav devbox:/work", "dir devbox:/src/x", "dir local:/b"]);
    }

    #[test]
    fn a_folder_is_listed_once_per_host() {
        let recent = [recent(&local(), "/a", 5), recent(&devbox(), "/a", 6)];
        let list = items(&[open(&local(), "/a")], &recent, "", None, &[], &[devbox()]);
        assert_eq!(rows(&list), vec!["open local:/a", "dir devbox:/a"]);
    }

    #[test]
    fn open_beats_favorite_beats_recent() {
        let favorites = [PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/b")];
        let recent = [recent(&local(), "/a", 1), recent(&local(), "/b", 2), recent(&local(), "/c", 3)];
        let list = items(&[open(&local(), "/a")], &recent, "", None, &favorites, &[]);
        assert_eq!(rows(&list), vec!["open local:/a", "fav local:/b", "dir local:/c"]);
    }

    #[test]
    fn hosts_not_in_ssh_hosts_are_left_out() {
        let gone = HostId::named("gone");
        let recent = [recent(&gone, "/x", 1), recent(&devbox(), "/y", 2)];
        let favorites = [PathBuf::from("ssh://gone/f")];
        let list = items(&[open(&gone, "/o")], &recent, "", None, &favorites, &[devbox()]);
        assert_eq!(rows(&list), vec!["dir devbox:/y"]);
    }

    #[test]
    fn remote_rows_match_on_host_and_path() {
        let recent = [recent(&devbox(), "/src/x", 20), recent(&devbox(), "/etc", 15), recent(&local(), "/b", 10)];
        let hosts = [devbox()];
        let by_host = items(&[], &recent, "devbox", None, &[], &hosts);
        assert_eq!(rows(&by_host), vec!["hint devbox", "dir devbox:/src/x", "dir devbox:/etc"]);
        assert_eq!(rows(&items(&[], &recent, "src", None, &[], &hosts)), vec!["dir devbox:/src/x"]);
    }

    #[test]
    fn host_hint_rows_only_for_a_bare_prefix() {
        let hosts = [devbox()];
        let hinted =
            |query: &str| items(&[], &[], query, None, &[], &hosts).first() == Some(&LauncherItem::HostHint(devbox()));
        assert!(hinted("dev"));
        assert!(hinted("DEV"));
        assert!(!hinted("dev/x"));
        assert!(!hinted("dev:"));
        assert!(!hinted(""));
        assert!(!hinted("box"));
    }

    #[test]
    fn welcome_items_are_favorites_then_recent_capped() {
        let favorites = [PathBuf::from("ssh://devbox/work"), PathBuf::from("/fav")];
        let host = |n: u64| if n.is_multiple_of(2) { local() } else { devbox() };
        let recent: Vec<RecentFolder> = (0..6u64).map(|n| recent(&host(n), &format!("/r{n}"), n)).collect();
        let list = welcome_items(&recent, None, &favorites, &[devbox()]);
        assert_eq!(list.len(), WELCOME_ROWS);
        assert_eq!(
            rows(&list),
            vec!["fav devbox:/work", "fav local:/fav", "dir devbox:/r5", "dir local:/r4", "dir devbox:/r3"]
        );
    }

    #[test]
    fn typed_paths() {
        let hosts = [devbox()];
        let path = |host: HostId, path: &str| Typed::Path { host, path: path.into() };
        assert_eq!(typed("~/co", &hosts), path(local(), "~/co"));
        assert_eq!(typed("/etc", &hosts), path(local(), "/etc"));
        assert_eq!(typed("devbox:~/src/m", &hosts), path(devbox(), "~/src/m"));
        assert_eq!(typed("devbox:", &hosts), path(devbox(), "~/"));
        assert_eq!(typed("devbox:src", &hosts), path(devbox(), "~/src"));
        assert_eq!(typed("DevBox:/etc", &hosts), path(devbox(), "/etc"));
        assert_eq!(typed("nohost:~/x", &hosts), Typed::Filter);
        assert_eq!(typed("devbox:~/x", &[]), Typed::Filter);
        assert_eq!(typed(":~/x", &hosts), Typed::Filter);
        assert_eq!(typed("notes", &hosts), Typed::Filter);
    }

    #[test]
    fn drive_letters_start_a_windows_path() {
        assert!(is_drive_path("C:") && is_drive_path("c:\\Users") && is_drive_path("D:/src"));
        assert!(!is_drive_path("C") && !is_drive_path("devbox:~/x") && !is_drive_path("1:/x") && !is_drive_path("C:x"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_local_paths_complete_on_forward_slashes() {
        let hosts = [HostId::named("c")];
        let path = |path: &str| Typed::Path { host: local(), path: path.into() };
        assert_eq!(typed("C:\\Users\\me", &hosts), path("C:/Users/me"));
        assert_eq!(typed("~\\src", &hosts), path("~/src"));
        assert_eq!(typed("c:", &hosts), path("c:"));
    }

    #[test]
    fn missing_local_favorite_is_not_openable() {
        let favorites = [PathBuf::from("/gone-folder-for-sure"), PathBuf::from("ssh://devbox/gone")];
        let list = items(&[], &[], "", None, &favorites, &[devbox()]);
        let exists: Vec<bool> =
            list.iter().map(|item| matches!(item, LauncherItem::Dir { exists: true, .. })).collect();
        assert_eq!(exists, vec![false, true]);
    }

    #[test]
    fn recent_folders_fill_in_away_hosts() {
        let gpu = HostId::named("gpu");
        let sessions = [
            info(&local(), "/a", 5, true),
            info(&devbox(), "/d", 7, true),
            info(&devbox(), "/d", 9, true),
            info(&devbox(), "/gone", 99, false),
        ];
        let refs: Vec<&SessionInfo> = sessions.iter().collect();
        let folder = |path: &str, last_active_ms| HostFolder { path: path.into(), last_active_ms };
        let remembered = BTreeMap::from([(devbox(), vec![folder("/old", 1)]), (gpu.clone(), vec![folder("/g", 3)])]);
        let listed = [devbox(), gpu.clone()];
        let found = recent_folders(&refs, &remembered, &listed);
        assert_eq!(found, vec![recent(&devbox(), "/d", 9), recent(&local(), "/a", 5), recent(&gpu, "/g", 3)]);
        assert_eq!(recent_folders(&refs, &remembered, &[devbox()]).len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_query_filters_by_the_displayed_path() {
        let recent =
            [recent(&local(), "/home/me/code/devops", 50), recent(&local(), "/home/me/Private/rust_terminal", 10)];
        let home = Some(Path::new("/home/me"));
        let found = |query: &str| rows(&items(&[], &recent, query, home, &[], &[]));
        assert_eq!(found("rust"), vec!["dir local:/home/me/Private/rust_terminal"]);
        assert_eq!(found("~/code"), vec!["dir local:/home/me/code/devops"]);
        assert!(found("zzz").is_empty());
        assert!(items(&[open(&local(), "/home/me/x")], &recent, "zzz", home, &[], &[]).is_empty());
    }

    #[test]
    fn open_folders_merge_panes_per_folder_in_first_seen_order() {
        let panes = [
            pane("/a", true, Tone::Idle),
            pane("/b", false, Tone::Idle),
            pane("/a", true, Tone::NeedsInput),
            pane("/a", false, Tone::Idle),
            pane("/c", true, Tone::Working),
            pane("/c", true, Tone::Done),
        ];
        let folder = |path: &str, tone| OpenFolder { host: local(), path: path.into(), tone };
        assert_eq!(
            open_folders(&panes),
            vec![folder("/a", Some(Tone::NeedsInput)), folder("/b", None), folder("/c", Some(Tone::Working)),]
        );
    }

    #[test]
    fn open_folders_group_by_host() {
        let remote = OpenPane { host: devbox(), ..pane("/a", true, Tone::Idle) };
        let folders = open_folders(&[pane("/a", true, Tone::Idle), remote]);
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[1].host, devbox());
    }

    #[test]
    fn selection_steps_through_every_row() {
        let list = items(&[open(&local(), "/a"), open(&local(), "/b")], &[], "", None, &[], &[]);
        assert_eq!(step(&list, 0, true), 1);
        assert_eq!(step(&list, 1, true), 1);
        assert_eq!(step(&list, 1, false), 0);
        assert_eq!(step(&list, 0, false), 0);
        assert_eq!(step(&[], 0, true), 0);
    }

    #[test]
    fn favorite_keys_per_host() {
        assert_eq!(favorite_key(&devbox(), Path::new("/work")), PathBuf::from("ssh://devbox/work"));
        assert_eq!(favorite_key(&local(), Path::new("/a")), PathBuf::from("/a"));
    }
}
