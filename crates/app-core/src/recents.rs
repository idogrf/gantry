//! The recent sessions pop-up (`Ctrl+Shift+R`): closed sessions by project, newest first, a few per project
//! unless looking at one project; while searching, one list of the sessions matching by title, conversation or folder;
//! chips for the most recently used projects.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use protocol::host::HostId;
use protocol::message::{SearchHit, SessionInfo, SessionState, Speaker};

use crate::fuzzy::Fuzzy;
use crate::session_tree::{age, display_path, keep, keep_text, labels, newest_first, session_title};

/// How the pop-up orders its sessions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    /// Newest first.
    #[default]
    Recent,
    /// By title, A to Z.
    Name,
    /// Under project headings.
    Project,
}

impl Sort {
    /// Every sort, in the order the sort bar shows them.
    pub const ALL: [Sort; 3] = [Sort::Recent, Sort::Name, Sort::Project];

    /// The sort bar's text.
    pub fn label(self) -> &'static str {
        match self {
            Sort::Recent => "Recent",
            Sort::Name => "Name",
            Sort::Project => "Project",
        }
    }

    /// The next (or previous) sort, wrapping around.
    pub fn cycle(self, forward: bool) -> Sort {
        let at = Sort::ALL.iter().position(|&sort| sort == self).unwrap_or(0);
        let len = Sort::ALL.len();
        Sort::ALL[if forward { (at + 1) % len } else { (at + len - 1) % len }]
    }
}

/// A group of values the sidebar filters by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Facet {
    /// The session's project.
    Project,
    /// The host it ran on.
    Host,
    /// The Claude config dir it belongs to.
    Dir,
}

impl Facet {
    /// Every facet, in the order the sidebar shows them.
    pub const ALL: [Facet; 3] = [Facet::Project, Facet::Host, Facet::Dir];

    /// The section's heading.
    pub fn title(self) -> &'static str {
        match self {
            Facet::Project => "Project",
            Facet::Host => "Host",
            Facet::Dir => "Config dir",
        }
    }
}

/// What lasts between openings of the pop-up while Gantry runs: the sort and the filter selections. Selections are
/// keys: a project's key (as text), a host's name, a config dir's name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prefs {
    /// The order of the list.
    pub sort: Sort,
    /// Whether the order is reversed.
    pub reverse: bool,
    projects: HashSet<String>,
    hosts: HashSet<String>,
    dirs: HashSet<String>,
}

impl Prefs {
    /// The keys selected in `facet` (none selected = the facet does not filter).
    pub fn selected(&self, facet: Facet) -> &HashSet<String> {
        match facet {
            Facet::Project => &self.projects,
            Facet::Host => &self.hosts,
            Facet::Dir => &self.dirs,
        }
    }

    fn selected_mut(&mut self, facet: Facet) -> &mut HashSet<String> {
        match facet {
            Facet::Project => &mut self.projects,
            Facet::Host => &mut self.hosts,
            Facet::Dir => &mut self.dirs,
        }
    }

    /// Select `key` in `facet`, or unselect it when it is selected.
    pub fn toggle(&mut self, facet: Facet, key: &str) {
        let set = self.selected_mut(facet);
        if !set.remove(key) {
            set.insert(key.to_string());
        }
    }

    /// Unselect everything in `facet`.
    pub fn clear(&mut self, facet: Facet) {
        self.selected_mut(facet).clear();
    }

    /// Unselect everything; the sort stays.
    pub fn clear_all(&mut self) {
        Facet::ALL.into_iter().for_each(|facet| self.clear(facet));
    }

    /// Whether any filter narrows the list.
    pub fn is_filtering(&self) -> bool {
        Facet::ALL.into_iter().any(|facet| !self.selected(facet).is_empty())
    }

    /// Pick a sort; the order is no longer reversed.
    pub fn set_sort(&mut self, sort: Sort) {
        self.sort = sort;
        self.reverse = false;
    }

    /// Select only the project `key` (the other facets stay as they are).
    pub fn only_project(&mut self, key: String) {
        self.projects = HashSet::from([key]);
    }
}

/// Everything the pop-up is built from.
pub struct RecentsInput<'a> {
    /// Claude sessions.
    pub sessions: &'a [&'a SessionInfo],
    /// Sessions a leaf of the layout shows, as (host, session id) (they are in the Active list, not here).
    pub open: &'a HashSet<(HostId, String)>,
    /// Home directory, shown as `~`.
    pub home: Option<&'a Path>,
    /// Search text (blank = none).
    pub query: &'a str,
    /// The picked project (`None` = all).
    pub project: Option<&'a Path>,
    /// Projects whose "Show N more" was used.
    pub expanded: &'a HashSet<PathBuf>,
    /// The project a directory belongs to.
    pub project_of: &'a dyn Fn(&HostId, &Path) -> PathBuf,
    /// Current time, ms since the Unix epoch.
    pub now_ms: u64,
    /// Sessions shown per project before "Show N more" (no search, all projects).
    pub per_project: usize,
    /// Project chips shown before "More…".
    pub chips: usize,
    /// Conversation search hits for `query`, best first (the latest reply; may be for a slightly older query).
    pub hits: &'a [SearchHit],
    /// Where conversation search stands.
    pub content: ContentState,
}

/// Fewest non-space characters that also search inside conversations.
pub const CONTENT_MIN_CHARS: usize = 3;

/// Whether `query` also searches inside conversations.
pub fn searches_content(query: &str) -> bool {
    query.trim().chars().count() >= CONTENT_MIN_CHARS
}

/// Where conversation search stands for the pop-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentState {
    /// No reply for the current query yet.
    Waiting,
    /// The reply for the current query arrived.
    Ready,
    /// The daemon is still building the index (percent done).
    Indexing(u8),
    /// The daemon cannot search.
    Unavailable,
}

/// The matching message a conversation hit shows under its title.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    /// Who wrote it.
    pub speaker: Speaker,
    /// One line of it.
    pub text: String,
    /// Byte ranges of `text` to draw in bold: on character boundaries, in order, not overlapping.
    pub highlights: Vec<Range<usize>>,
}

impl Snippet {
    /// The snippet of `hit`, keeping only highlight ranges that can be drawn (a bad range would panic the renderer).
    pub fn from_hit(hit: &SearchHit) -> Snippet {
        let text = hit.snippet.clone();
        let mut ranges: Vec<Range<usize>> = hit
            .highlights
            .iter()
            .map(|&(start, end)| start as usize..end as usize)
            .filter(|r| r.start < r.end && r.end <= text.len())
            .filter(|r| text.is_char_boundary(r.start) && text.is_char_boundary(r.end))
            .collect();
        ranges.sort_by_key(|r| r.start);
        let mut highlights: Vec<Range<usize>> = Vec::new();
        for range in ranges {
            match highlights.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => highlights.push(range),
            }
        }
        Snippet { speaker: hit.speaker, text, highlights }
    }
}

/// One row of the pop-up's list.
#[derive(Clone, Debug, PartialEq)]
pub enum RecentItem {
    /// A project heading.
    Heading {
        /// Project directory.
        path: PathBuf,
        /// Displayed name.
        label: String,
        /// Its directory for display (`~/…`).
        display: String,
    },
    /// A closed session.
    Session {
        /// The session (boxed: it is far bigger than the other variants).
        session: Box<SessionInfo>,
        /// Time since last activity.
        age: String,
        /// The project it is listed under.
        project: PathBuf,
        /// Byte ranges of its title to draw in bold (the typed word starts), in order.
        marks: Vec<Range<usize>>,
        /// The matching message, for a conversation hit (`None` = not found in its conversation).
        snippet: Option<Snippet>,
        /// Whether a leaf of the layout shows it (Enter focuses it).
        open: bool,
    },
    /// "Show N more" under a project.
    More {
        /// The project.
        project: PathBuf,
        /// Sessions not shown.
        hidden: usize,
    },
    /// A line about the conversation search's state.
    Note {
        /// Its text.
        text: String,
    },
}

impl RecentItem {
    /// Whether the selection can land here (headings are skipped).
    pub fn selectable(&self) -> bool {
        !matches!(self, RecentItem::Heading { .. } | RecentItem::Note { .. })
    }
}

/// What a project chip selects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChipTarget {
    /// Every project.
    All,
    /// One project.
    Project(PathBuf),
    /// Choose among all projects.
    More,
}

/// A project chip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chip {
    /// What it selects.
    pub target: ChipTarget,
    /// Its text.
    pub label: String,
    /// Sessions in it (0 for More…).
    pub count: usize,
}

/// A project in the "More…" chooser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectItem {
    /// Project directory.
    pub path: PathBuf,
    /// Displayed name.
    pub label: String,
    /// Its directory for display.
    pub display: String,
    /// Its closed sessions.
    pub count: usize,
}

/// Closed sessions not shown by a leaf, by project: projects by their newest session, sessions newest first.
fn grouped<'a>(input: &RecentsInput<'a>) -> Vec<(PathBuf, Vec<&'a SessionInfo>)> {
    let mut by_project: HashMap<PathBuf, Vec<&'a SessionInfo>> = HashMap::new();
    for session in input.sessions.iter().copied() {
        if session.state == SessionState::Closed && !is_open(input, session) {
            by_project.entry((input.project_of)(&session.host, &session.cwd)).or_default().push(session);
        }
    }
    let mut projects: Vec<(PathBuf, Vec<&'a SessionInfo>)> = by_project.into_iter().collect();
    for (_, sessions) in &mut projects {
        sessions.sort_by(newest_first);
    }
    projects.sort_by(|(a_path, a), (b_path, b)| {
        b[0].last_active_ms.cmp(&a[0].last_active_ms).then_with(|| a_path.cmp(b_path))
    });
    projects
}

fn in_scope(input: &RecentsInput<'_>, path: &Path) -> bool {
    input.project.is_none_or(|picked| picked == path)
}

/// The pop-up's list: by project without a search; with one, one list in tiers (see `matches`).
pub fn items(input: &RecentsInput<'_>) -> Vec<RecentItem> {
    let words = typed_words(input.query);
    if words.is_empty() { by_project(input) } else { matches(input, &words) }
}

/// The search's words: lowercase runs of letters and digits ("rust_terminal" is two).
fn typed_words(query: &str) -> Vec<String> {
    query.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()).map(str::to_lowercase).collect()
}

/// Where each typed word starts a word of `text` (case ignored; a word starts after a non-alphanumeric character or
/// at a lowercase-to-uppercase step), as merged byte ranges in order; `None` unless every word does.
fn word_starts(words: &[String], text: &str) -> Option<Vec<Range<usize>>> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let starts_word = |i: usize| {
        chars[i].1.is_alphanumeric()
            && (i == 0
                || !chars[i - 1].1.is_alphanumeric()
                || (chars[i - 1].1.is_lowercase() && chars[i].1.is_uppercase()))
    };
    let mut ranges = Vec::new();
    for word in words {
        let found = (0..chars.len()).filter(|&i| starts_word(i)).find_map(|i| {
            let mut rest = chars[i..].iter();
            for wanted in word.chars() {
                let &(_, have) = rest.next()?;
                if !have.to_lowercase().eq(wanted.to_lowercase()) {
                    return None;
                }
            }
            let end = rest.next().map_or(text.len(), |&(at, _)| at);
            Some(chars[i].0..end)
        })?;
        ranges.push(found);
    }
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    Some(merged)
}

/// How a session matched: by title or only by its folder, and the title marks.
fn title_match(input: &RecentsInput<'_>, words: &[String], session: &SessionInfo) -> Option<(bool, Vec<Range<usize>>)> {
    let title = session_title(session);
    if let Some(marks) = word_starts(words, title) {
        return Some((true, marks));
    }
    let marks = word_starts(words, &format!("{title} {}", display_path(&session.cwd, input.home)))?;
    Some((false, marks.into_iter().filter(|r| r.end <= title.len()).collect()))
}

/// Where a row of the search's list goes: tier, conversation rank, newest first.
type Order = (u8, usize, Reverse<u64>);

/// The search's list, without headings: sessions matching by title and in their conversation (conversation order),
/// by title only (newest first), in their conversation only (conversation order), then by folder only (newest first);
/// then a note on the conversation search. Closed sessions, plus open ones found in their conversation (sessions
/// running outside Gantry are in the Active list); all in the picked project.
fn matches(input: &RecentsInput<'_>, words: &[String]) -> Vec<RecentItem> {
    let content = searches_content(input.query);
    let hits: &[SearchHit] = if content { input.hits } else { &[] };
    let mut hit_of: HashMap<(&HostId, &str), (usize, &SearchHit)> = HashMap::new();
    for (rank, hit) in hits.iter().enumerate() {
        hit_of.entry((&hit.host, hit.session_id.as_str())).or_insert((rank, hit));
    }
    let mut candidates: Vec<&SessionInfo> = Vec::new();
    let mut seen: HashSet<(&HostId, &str)> = HashSet::new();
    let open_hits =
        input.sessions.iter().filter(|s| hit_of.contains_key(&(&s.host, s.id.as_str())) && is_open(input, s));
    for session in grouped(input).into_iter().flat_map(|(_, sessions)| sessions).chain(open_hits.copied()) {
        if seen.insert((&session.host, session.id.as_str())) {
            candidates.push(session);
        }
    }
    let mut rows: Vec<(Order, RecentItem)> = Vec::new();
    for session in candidates {
        let project = (input.project_of)(&session.host, &session.cwd);
        if !in_scope(input, &project) {
            continue;
        }
        let hit = hit_of.get(&(&session.host, session.id.as_str()));
        let (by_title, marks) = match title_match(input, words, session) {
            Some(found) => found,
            None if hit.is_some() => (false, Vec::new()),
            None => continue,
        };
        let newest = Reverse(session.last_active_ms);
        let key = match (by_title, hit) {
            (true, Some(&(rank, _))) => (0, rank, newest),
            (true, None) => (1, 0, newest),
            (false, Some(&(rank, _))) => (2, rank, newest),
            (false, None) => (3, 0, newest),
        };
        rows.push((
            key,
            RecentItem::Session {
                session: Box::new(session.clone()),
                age: age(input.now_ms, session.last_active_ms),
                project,
                marks,
                snippet: hit.map(|&(_, hit)| Snippet::from_hit(hit)),
                open: is_open(input, session),
            },
        ));
    }
    rows.sort_by(|(a, a_item), (b, b_item)| a.cmp(b).then_with(|| item_id(a_item).cmp(item_id(b_item))));
    let mut items: Vec<RecentItem> = rows.into_iter().map(|(_, item)| item).collect();
    let note = match input.content {
        _ if !content => None,
        ContentState::Indexing(percent) => Some(format!("Indexing conversations… {percent}%")),
        ContentState::Unavailable => Some("Conversation search is unavailable — see the daemon log".to_string()),
        ContentState::Ready if input.hits.is_empty() => Some("No conversations mention this".to_string()),
        _ => None,
    };
    items.extend(note.map(|text| RecentItem::Note { text }));
    items
}

fn is_open(input: &RecentsInput<'_>, session: &SessionInfo) -> bool {
    input.open.contains(&(session.host.clone(), session.id.clone()))
}

fn item_id(item: &RecentItem) -> &str {
    match item {
        RecentItem::Session { session, .. } => &session.id,
        _ => "",
    }
}

/// No search: closed sessions by project, a few per project unless expanded or looking at one project.
fn by_project(input: &RecentsInput<'_>) -> Vec<RecentItem> {
    let grouped = grouped(input);
    let paths: Vec<PathBuf> = grouped.iter().map(|(path, _)| path.clone()).collect();
    let labels = labels(&paths, input.home);
    let mut items = Vec::new();
    for (path, matching) in grouped {
        if !in_scope(input, &path) {
            continue;
        }
        let cap =
            if input.project.is_none() && !input.expanded.contains(&path) { input.per_project } else { usize::MAX };
        let display = display_path(&path, input.home);
        items.push(RecentItem::Heading { label: labels[&path].clone(), display, path: path.clone() });
        for session in matching.iter().take(cap) {
            items.push(RecentItem::Session {
                session: Box::new((*session).clone()),
                age: age(input.now_ms, session.last_active_ms),
                project: path.clone(),
                marks: Vec::new(),
                snippet: None,
                open: false,
            });
        }
        if matching.len() > cap {
            items.push(RecentItem::More { project: path, hidden: matching.len() - cap });
        }
    }
    items
}

/// The project chips: All, the `input.chips` projects with the newest sessions (plus the picked one), then More….
pub fn chips(input: &RecentsInput<'_>) -> Vec<Chip> {
    let grouped = grouped(input);
    let mut paths: Vec<PathBuf> = grouped.iter().map(|(path, _)| path.clone()).collect();
    if let Some(picked) = input.project
        && !paths.iter().any(|path| path == picked)
    {
        paths.push(picked.to_path_buf());
    }
    let labels = labels(&paths, input.home);
    let count = |path: &Path| grouped.iter().find(|(p, _)| p == path).map_or(0, |(_, sessions)| sessions.len());
    let total = grouped.iter().map(|(_, sessions)| sessions.len()).sum();
    let mut chips = vec![Chip { target: ChipTarget::All, label: "All projects".into(), count: total }];
    let mut shown: Vec<&PathBuf> = paths.iter().take(input.chips).collect();
    if let Some(picked) = input.project
        && !shown.iter().any(|path| path.as_path() == picked)
    {
        shown.push(paths.iter().find(|path| path.as_path() == picked).expect("added above"));
    }
    for path in shown {
        chips.push(Chip { target: ChipTarget::Project(path.clone()), label: labels[path].clone(), count: count(path) });
    }
    if grouped.len() > input.chips {
        chips.push(Chip { target: ChipTarget::More, label: "More…".into(), count: 0 });
    }
    chips
}

/// Every project, for "More…", filtered by the search (name and directory).
pub fn projects(input: &RecentsInput<'_>) -> Vec<ProjectItem> {
    let grouped = grouped(input);
    let paths: Vec<PathBuf> = grouped.iter().map(|(path, _)| path.clone()).collect();
    let labels = labels(&paths, input.home);
    let mut fuzzy = Fuzzy::new(input.query, paths.iter().map(|path| keep_text(&labels[path], path, input.home)));
    grouped
        .into_iter()
        .filter(|(path, _)| keep(&mut fuzzy, &labels[path], path, input.home))
        .map(|(path, sessions)| ProjectItem {
            label: labels[&path].clone(),
            display: display_path(&path, input.home),
            count: sessions.len(),
            path,
        })
        .collect()
}

/// Sessions matching the search, and sessions in scope (the picked project, else all).
pub fn counts(input: &RecentsInput<'_>) -> (usize, usize) {
    let words = typed_words(input.query);
    let scoped: Vec<&SessionInfo> = grouped(input)
        .into_iter()
        .filter(|(path, _)| in_scope(input, path))
        .flat_map(|(_, sessions)| sessions)
        .collect();
    let matching = scoped.iter().filter(|s| words.is_empty() || title_match(input, &words, s).is_some()).count();
    (matching, scoped.len())
}

/// The sessions the pop-up lists: those of this machine and of the hosts `connected` says are.
pub fn listed_sessions<'a>(
    sessions: impl Iterator<Item = &'a SessionInfo>,
    connected: impl Fn(&HostId) -> bool,
) -> Vec<SessionInfo> {
    sessions.filter(|session| session.host.is_local() || connected(&session.host)).cloned().collect()
}

/// The configured SSH hosts that are not connected, in settings order. This machine is never one of them.
pub fn offline_hosts(configured: &[String], connected: impl Fn(&HostId) -> bool) -> Vec<String> {
    configured
        .iter()
        .filter(|name| {
            let host = HostId::named(name);
            !host.is_local() && !connected(&host)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::host::project_key;

    const NOW: u64 = 1_000_000_000;
    const HOUR: u64 = 60 * 60 * 1000;

    fn closed(id: &str, cwd: &str, ago_hours: u64) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            cwd: cwd.into(),
            title: format!("title {id}"),
            last_active_ms: NOW - ago_hours * HOUR,
            state: SessionState::Closed,
            cwd_exists: true,
            status: None,
            status_since_ms: 0,
            container: None,
            claude_dir: None,
            last_prompt: None,
            host: protocol::host::HostId::local(),
        }
    }

    fn project_of(host: &HostId, cwd: &Path) -> PathBuf {
        let root = cwd.ancestors().find(|dir| dir.parent() == Some(Path::new("/r"))).unwrap_or(cwd);
        project_key(host, root)
    }

    struct Fixture {
        sessions: Vec<SessionInfo>,
        open: HashSet<(HostId, String)>,
        query: String,
        project: Option<PathBuf>,
        expanded: HashSet<PathBuf>,
        per_project: usize,
        chips: usize,
        hits: Vec<SearchHit>,
        content: ContentState,
    }

    impl Default for Fixture {
        fn default() -> Self {
            Fixture {
                sessions: Vec::new(),
                open: HashSet::new(),
                query: String::new(),
                project: None,
                expanded: HashSet::new(),
                per_project: 5,
                chips: 6,
                hits: Vec::new(),
                content: ContentState::Waiting,
            }
        }
    }

    impl Fixture {
        fn of(sessions: Vec<SessionInfo>) -> Fixture {
            Fixture { sessions, ..Fixture::default() }
        }

        fn with<T>(&self, f: impl FnOnce(&RecentsInput<'_>) -> T) -> T {
            let refs: Vec<&SessionInfo> = self.sessions.iter().collect();
            f(&RecentsInput {
                sessions: &refs,
                open: &self.open,
                home: Some(Path::new("/home/me")),
                query: &self.query,
                project: self.project.as_deref(),
                expanded: &self.expanded,
                project_of: &project_of,
                now_ms: NOW,
                per_project: self.per_project,
                chips: self.chips,
                hits: &self.hits,
                content: self.content,
            })
        }

        fn outline(&self) -> Vec<String> {
            self.with(items)
                .iter()
                .map(|item| match item {
                    RecentItem::Heading { label, .. } => format!("# {label}"),
                    RecentItem::Session { session, snippet: Some(_), open: true, .. } => {
                        format!("~{} open", session.id)
                    }
                    RecentItem::Session { session, snippet: Some(_), .. } => format!("~{}", session.id),
                    RecentItem::Note { text } => format!("! {text}"),
                    RecentItem::Session { session, .. } => session.id.clone(),
                    RecentItem::More { hidden, .. } => format!("+{hidden}"),
                })
                .collect()
        }

        fn chip_labels(&self) -> Vec<String> {
            self.with(chips).into_iter().map(|chip| format!("{} {}", chip.label, chip.count)).collect()
        }
    }

    fn many(project: &str, n: u64, start_hour: u64) -> Vec<SessionInfo> {
        (0..n).map(|i| closed(&format!("{project}{i}"), &format!("/r/{project}"), start_hour + i)).collect()
    }

    #[test]
    fn closed_sessions_by_project_newest_first_without_live_or_open_ones() {
        let mut live = closed("live", "/r/a", 0);
        live.state = SessionState::Managed { pane: 3 };
        let mut fixture = Fixture::of(vec![
            closed("a-old", "/r/a", 5),
            closed("b", "/r/b", 1),
            closed("a-new", "/r/a", 2),
            closed("tab", "/r/b", 0),
            live,
        ]);
        fixture.open.insert((HostId::local(), "tab".into()));
        assert_eq!(fixture.outline(), vec!["# b", "b", "# a", "a-new", "a-old"]);
        assert_eq!(fixture.with(counts), (3, 3));
    }

    #[test]
    fn a_session_item_names_its_project() {
        let fixture = Fixture::of(vec![closed("a", "/r/a", 1), closed("b", "/r/b", 2)]);
        let items = fixture.with(items);
        let mut heading = None;
        for item in &items {
            match item {
                RecentItem::Heading { path, .. } => heading = Some(path.clone()),
                RecentItem::Session { project, .. } => assert_eq!(Some(project), heading.as_ref()),
                RecentItem::More { .. } | RecentItem::Note { .. } => {}
            }
        }
        assert!(
            items
                .iter()
                .any(|item| matches!(item, RecentItem::Session { project, .. } if project == Path::new("/r/a")))
        );
    }

    #[test]
    fn five_per_project_with_show_more_until_expanded() {
        let mut fixture = Fixture::of(many("a", 8, 1));
        assert_eq!(fixture.outline(), vec!["# a", "a0", "a1", "a2", "a3", "a4", "+3"]);
        fixture.expanded.insert("/r/a".into());
        assert_eq!(fixture.outline().len(), 9);
    }

    #[test]
    fn searching_reaches_every_session_without_the_limit() {
        let mut sessions = many("a", 8, 1);
        sessions.push(closed("zzz", "/r/b", 1));
        let mut fixture = Fixture::of(sessions);
        fixture.query = "title a".into();
        let outline = fixture.outline();
        assert_eq!(outline.len(), 8, "{outline:?}");
        assert_eq!(fixture.with(counts), (8, 9));
    }

    fn titled(id: &str, title: &str, cwd: &str, ago_hours: u64) -> SessionInfo {
        SessionInfo { title: title.into(), ..closed(id, cwd, ago_hours) }
    }

    #[test]
    fn searching_lists_sessions_whose_title_words_start_with_the_typed_words() {
        let mut fixture = Fixture::of(vec![
            titled("boundary", "Selected tab cyan boundary", "/r/a", 1),
            titled("gantry", "Gantry sounds", "/r/a", 2),
            titled("system", "System sounds", "/r/b", 3),
            titled("other", "Unrelated", "/r/b", 4),
        ]);
        fixture.query = "sound".into();
        // One list without headings, newest first; "s…ound" in "Selected tab cyan boundary" is not the word.
        assert_eq!(fixture.outline(), vec!["gantry", "system"]);
        fixture.query = "SEL tab".into();
        assert_eq!(fixture.outline(), vec!["boundary"]);
        fixture.query = "ound".into();
        assert_eq!(fixture.outline(), Vec::<String>::new(), "a word is matched from its start");
    }

    #[test]
    fn the_typed_word_starts_are_marked_in_the_title() {
        let mut fixture = Fixture::of(vec![titled("s", "System sounds", "/r/a", 1)]);
        fixture.query = "sou sys".into();
        let marks = fixture.with(items).into_iter().find_map(|item| match item {
            RecentItem::Session { marks, .. } => Some(marks),
            _ => None,
        });
        assert_eq!(marks, Some(vec![0..3, 7..10]));
    }

    #[test]
    fn one_list_in_tiers_title_and_conversation_then_title_then_conversation_then_folder() {
        let mut fixture = Fixture::of(vec![
            titled("folder-new", "Notes", "/r/sound", 1),
            titled("title-new", "Sound check", "/r/a", 2),
            titled("both-weak", "Sound board", "/r/a", 3),
            titled("talk", "Unrelated", "/r/a", 4),
            titled("both-strong", "Sound fix", "/r/b", 5),
            titled("title-old", "Sound old", "/r/b", 6),
            titled("folder-old", "Other notes", "/r/sound", 7),
        ]);
        fixture.query = "sound".into();
        fixture.hits = vec![hit("both-strong", "x", vec![]), hit("talk", "y", vec![]), hit("both-weak", "z", vec![])];
        fixture.content = ContentState::Ready;
        assert_eq!(
            fixture.outline(),
            vec!["~both-strong", "~both-weak", "title-new", "title-old", "~talk", "folder-new", "folder-old"]
        );
    }

    #[test]
    fn folder_only_matches_come_after_title_matches() {
        let mut fixture = Fixture::of(vec![
            titled("in-folder", "Notes", "/r/sounds", 1),
            titled("in-title", "Sound check", "/r/a", 5),
        ]);
        fixture.query = "sound".into();
        assert_eq!(fixture.outline(), vec!["in-title", "in-folder"]);
    }

    #[test]
    fn a_picked_project_shows_all_its_sessions_and_scopes_the_search() {
        let mut sessions = many("a", 8, 1);
        sessions.extend(many("b", 2, 0));
        let mut fixture = Fixture::of(sessions);
        fixture.project = Some("/r/a".into());
        assert_eq!(fixture.outline().len(), 9);
        assert!(!fixture.outline().iter().any(|row| row.starts_with('+')));
        fixture.query = "b".into();
        assert_eq!(fixture.outline(), Vec::<String>::new(), "b's sessions are outside the picked project");
        assert_eq!(fixture.with(counts), (0, 8));
    }

    #[test]
    fn chips_are_all_then_the_six_newest_projects_then_more() {
        let sessions: Vec<SessionInfo> = (0..8).map(|i| closed(&format!("s{i}"), &format!("/r/p{i}"), i)).collect();
        let mut fixture = Fixture::of(sessions);
        assert_eq!(
            fixture.chip_labels(),
            vec!["All projects 8", "p0 1", "p1 1", "p2 1", "p3 1", "p4 1", "p5 1", "More… 0"]
        );
        // A picked project beyond the six still gets its chip.
        fixture.project = Some("/r/p7".into());
        assert!(fixture.chip_labels().contains(&"p7 1".to_string()));
        // With six projects or fewer there is no More….
        fixture.sessions.truncate(6);
        fixture.project = None;
        assert!(!fixture.chip_labels().iter().any(|chip| chip.starts_with("More")));
    }

    #[test]
    fn a_picked_project_without_sessions_keeps_its_chip() {
        let mut fixture = Fixture::of(vec![closed("b", "/r/b", 1)]);
        fixture.project = Some("/r/a".into());
        assert_eq!(fixture.outline(), Vec::<String>::new());
        let picked = fixture.with(chips).into_iter().find(|chip| chip.target == ChipTarget::Project("/r/a".into()));
        assert_eq!(picked.map(|chip| chip.count), Some(0));
    }

    #[test]
    fn the_project_chooser_lists_every_project_filtered_by_the_search() {
        let mut fixture =
            Fixture::of(vec![closed("x", "/r/api", 1), closed("y", "/r/web", 2), closed("z", "/r/api", 3)]);
        let names = |fixture: &Fixture| {
            fixture.with(projects).into_iter().map(|p| format!("{} {}", p.label, p.count)).collect::<Vec<_>>()
        };
        assert_eq!(names(&fixture), vec!["api 2", "web 1"]);
        fixture.query = "web".into();
        assert_eq!(names(&fixture), vec!["web 1"]);
    }

    #[test]
    fn sessions_whose_folder_is_gone_are_listed() {
        let gone = SessionInfo { cwd_exists: false, ..closed("gone", "/r/a", 1) };
        let fixture = Fixture::of(vec![gone]);
        assert_eq!(fixture.outline(), vec!["# a", "gone"]);
    }

    #[test]
    fn sizes_come_from_the_input() {
        let sessions: Vec<SessionInfo> =
            (0..4).map(|n| closed(&format!("s{n}"), "/r/a", n + 1)).chain([closed("t", "/r/b", 9)]).collect();
        let one = Fixture { per_project: 1, ..Fixture::of(sessions.clone()) };
        assert_eq!(one.outline(), vec!["# a", "s0", "+3", "# b", "t"]);
        let no_chips = Fixture { chips: 0, ..Fixture::of(sessions) };
        assert_eq!(no_chips.chip_labels(), vec!["All projects 5", "More… 0"]);
    }

    fn hit(id: &str, text: &str, highlights: Vec<(u32, u32)>) -> SearchHit {
        SearchHit {
            session_id: id.into(),
            claude_dir: "/home/me/.claude".into(),
            speaker: Speaker::You,
            snippet: text.into(),
            highlights,
            host: HostId::local(),
        }
    }

    #[test]
    fn conversation_hits_follow_the_title_matches_in_one_list() {
        let mut fixture = Fixture::of(vec![closed("a1", "/r/a", 1), closed("b1", "/r/b", 2), closed("b2", "/r/b", 3)]);
        fixture.query = "title a1".into();
        fixture.hits = vec![hit("b2", "x", vec![]), hit("a1", "y", vec![]), hit("b1", "z", vec![])];
        fixture.content = ContentState::Ready;
        assert_eq!(fixture.outline(), vec!["~a1", "~b2", "~b1"]);
    }

    #[test]
    fn no_content_section_for_short_queries() {
        let mut fixture = Fixture::of(vec![closed("a1", "/r/a", 1)]);
        fixture.query = "a".into();
        fixture.hits = vec![hit("a1", "x", vec![])];
        fixture.content = ContentState::Ready;
        assert_eq!(fixture.outline(), vec!["a1"], "no snippet and no note below 3 characters");
        assert!(!searches_content(" a "));
        assert!(!searches_content("in"), "two letters would list half the history");
        assert!(searches_content("abc"));
    }

    #[test]
    fn content_hits_respect_the_picked_project() {
        let mut fixture = Fixture::of(vec![closed("a1", "/r/a", 1), closed("b1", "/r/b", 2)]);
        fixture.query = "zzz".into();
        fixture.project = Some("/r/b".into());
        fixture.hits = vec![hit("a1", "x", vec![]), hit("b1", "y", vec![])];
        fixture.content = ContentState::Ready;
        assert_eq!(fixture.outline(), vec!["~b1"]);
    }

    #[test]
    fn open_sessions_are_hits_and_external_ones_are_not() {
        let mut tab = closed("tab", "/r/a", 1);
        tab.state = SessionState::Managed { pane: 2 };
        let mut elsewhere = closed("elsewhere", "/r/a", 1);
        elsewhere.state = SessionState::External { pid: Some(9) };
        let mut fixture = Fixture::of(vec![tab, elsewhere, closed("unknown-to-hits", "/r/a", 1)]);
        fixture.open.insert((HostId::local(), "tab".into()));
        fixture.query = "zzz".into();
        fixture.hits = vec![hit("tab", "x", vec![]), hit("elsewhere", "y", vec![]), hit("not-listed", "z", vec![])];
        fixture.content = ContentState::Ready;
        assert_eq!(fixture.outline(), vec!["~tab open"]);
    }

    #[test]
    fn a_title_match_with_a_hit_carries_its_snippet() {
        let mut fixture = Fixture::of(vec![closed("a1", "/r/a", 1)]);
        fixture.query = "title a1".into();
        fixture.hits = vec![hit("a1", "x", vec![])];
        fixture.content = ContentState::Ready;
        assert_eq!(fixture.outline(), vec!["~a1"]);
    }

    #[test]
    fn the_section_explains_its_state() {
        let mut fixture = Fixture::of(vec![closed("a1", "/r/a", 1)]);
        fixture.query = "zzz".into();
        fixture.content = ContentState::Waiting;
        assert!(fixture.outline().is_empty(), "nothing until the first reply");
        fixture.content = ContentState::Ready;
        assert_eq!(fixture.outline(), vec!["! No conversations mention this"]);
        fixture.content = ContentState::Indexing(40);
        fixture.hits = vec![hit("a1", "x", vec![])];
        assert_eq!(fixture.outline(), vec!["~a1", "! Indexing conversations… 40%"]);
        fixture.content = ContentState::Unavailable;
        fixture.hits.clear();
        assert_eq!(fixture.outline(), vec!["! Conversation search is unavailable — see the daemon log"]);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn title_matches_have_no_snippet_and_hits_carry_theirs() {
        let mut fixture = Fixture::of(vec![closed("a1", "/r/a", 1), closed("b1", "/r/b", 2)]);
        fixture.query = "title a1".into();
        fixture.hits = vec![hit("b1", "the fallback", vec![(4, 12)])];
        fixture.content = ContentState::Ready;
        let items = fixture.with(items);
        let snippets: Vec<Option<&Snippet>> = items
            .iter()
            .filter_map(|item| match item {
                RecentItem::Session { snippet, .. } => Some(snippet.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(snippets[0], None);
        let snippet = snippets[1].unwrap();
        assert_eq!(
            (snippet.speaker, snippet.text.as_str(), snippet.highlights.clone()),
            (Speaker::You, "the fallback", vec![4..12])
        );
    }

    #[test]
    fn highlights_off_char_boundaries_are_dropped() {
        // "שלום" is 8 bytes; 1..3 cuts a character, 0..8 does not; 20..30 is past the end; 6..2 is backwards.
        let snippet = Snippet::from_hit(&hit("s", "שלום fallback", vec![(1, 3), (0, 8), (20, 30), (6, 2), (9, 17)]));
        assert_eq!(snippet.highlights, vec![0..8, 9..17]);
    }

    #[test]
    fn overlapping_highlights_are_merged_in_order() {
        let snippet = Snippet::from_hit(&hit("s", "abcdefghij", vec![(5, 8), (0, 2), (1, 4)]));
        assert_eq!(snippet.highlights, vec![0..4, 5..8]);
    }

    #[test]
    fn the_same_folder_on_two_hosts_is_two_recent_projects_named_with_the_host() {
        let remote = SessionInfo { host: HostId::named("devbox"), ..closed("r", "/r/app", 2) };
        let fixture = Fixture::of(vec![closed("l", "/r/app", 1), remote]);
        assert_eq!(fixture.outline(), vec!["# app", "l", "# devbox - app", "r"]);
        assert_eq!(fixture.chip_labels()[1..3], ["app 1", "devbox - app 1"]);
    }

    #[test]
    fn a_hit_belongs_to_its_host_and_an_open_remote_session_is_marked() {
        let remote = SessionInfo { host: HostId::named("devbox"), ..closed("same", "/r/app", 2) };
        let mut fixture = Fixture::of(vec![closed("same", "/r/app", 1), remote]);
        fixture.query = "zzz".into();
        fixture.hits = vec![SearchHit { host: HostId::named("devbox"), ..hit("same", "x", vec![]) }];
        fixture.content = ContentState::Ready;
        fixture.open.insert((HostId::named("devbox"), "same".into()));
        let items = fixture.with(items);
        let found: Vec<_> = items
            .iter()
            .filter_map(|item| match item {
                RecentItem::Session { session, snippet, open, .. } => {
                    Some((session.host.name().to_string(), snippet.is_some(), *open))
                }
                _ => None,
            })
            .collect();
        assert_eq!(found, [("devbox".to_string(), true, true)]);
    }

    #[test]
    fn only_connected_hosts_sessions_are_listed_and_the_others_are_offline() {
        let session = |host: HostId, id: &str| SessionInfo { host, ..closed(id, "/r", 1) };
        let all =
            [session(HostId::local(), "l"), session(HostId::named("up"), "u"), session(HostId::named("down"), "d")];
        let up = |host: &HostId| host.name() == "up";
        let listed = listed_sessions(all.iter(), up);
        assert_eq!(listed.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["l", "u"]);
        let configured = ["down".to_string(), "up".to_string(), "lab".to_string()];
        assert_eq!(offline_hosts(&configured, up), ["down", "lab"]);
        // This machine is never offline, even when it is configured or nothing is connected.
        assert_eq!(offline_hosts(&["local".to_string(), "".to_string()], |_| false), Vec::<String>::new());
    }

    #[test]
    fn sort_cycles_and_wraps() {
        assert_eq!(Sort::Recent.cycle(true), Sort::Name);
        assert_eq!(Sort::Project.cycle(true), Sort::Recent);
        assert_eq!(Sort::Recent.cycle(false), Sort::Project);
        assert_eq!(Sort::default(), Sort::Recent);
        assert_eq!(Sort::ALL.map(Sort::label), ["Recent", "Name", "Project"]);
    }

    #[test]
    fn toggle_adds_then_removes() {
        let mut prefs = Prefs::default();
        prefs.toggle(Facet::Host, "devbox");
        assert!(prefs.selected(Facet::Host).contains("devbox"));
        prefs.toggle(Facet::Host, "devbox");
        assert!(prefs.selected(Facet::Host).is_empty());
    }

    #[test]
    fn set_sort_clears_reverse() {
        let mut prefs = Prefs { reverse: true, ..Prefs::default() };
        prefs.set_sort(Sort::Name);
        assert_eq!((prefs.sort, prefs.reverse), (Sort::Name, false));
    }

    #[test]
    fn clear_all_empties_every_facet_but_keeps_sort() {
        let mut prefs = Prefs::default();
        prefs.set_sort(Sort::Project);
        for facet in Facet::ALL {
            prefs.toggle(facet, "x");
        }
        assert!(prefs.is_filtering());
        prefs.clear_all();
        assert!(!prefs.is_filtering());
        assert_eq!(prefs.sort, Sort::Project);
        prefs.toggle(Facet::Dir, "Work");
        prefs.clear(Facet::Dir);
        assert!(!prefs.is_filtering());
    }

    #[test]
    fn only_project_replaces_the_project_selection_only() {
        let mut prefs = Prefs::default();
        prefs.toggle(Facet::Project, "/r/a");
        prefs.toggle(Facet::Host, "devbox");
        prefs.only_project("/r/b".into());
        assert_eq!(prefs.selected(Facet::Project).iter().collect::<Vec<_>>(), ["/r/b"]);
        assert!(prefs.selected(Facet::Host).contains("devbox"));
    }

    #[test]
    fn is_filtering_follows_the_selections() {
        let mut prefs = Prefs::default();
        assert!(!prefs.is_filtering());
        prefs.toggle(Facet::Project, "/r/a");
        assert!(prefs.is_filtering());
    }
}
