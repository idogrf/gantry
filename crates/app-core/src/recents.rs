//! The recent sessions pop-up (`Ctrl+Shift+R`): closed sessions in the order the user picked (recent, name or by
//! project), narrowed by a search (title, conversation or folder) and by filters (project, host, config dir).

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use protocol::host::HostId;
use protocol::message::{SearchHit, SessionInfo, SessionState, Speaker};

use crate::session_tree::{age, display_path, labels, session_title};

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

    /// Select only the project `key`: every other filter is cleared (the sort stays).
    pub fn only_project(&mut self, key: String) {
        self.clear_all();
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
    /// The sort and the filter selections.
    pub prefs: &'a Prefs,
    /// A session's config dir name (`None` = it has none, like a session on another host).
    pub dir_name: &'a dyn Fn(&SessionInfo) -> Option<String>,
    /// The project a session belongs to: the Gantry project it was put in, else the project of its folder.
    pub project_of: &'a dyn Fn(&SessionInfo) -> PathBuf,
    /// Every Gantry project name (existing ones first, then those only closed sessions remember): the display names
    /// of `gantry://` keys.
    pub gantry_names: &'a [String],
    /// Current time, ms since the Unix epoch.
    pub now_ms: u64,
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

/// A session that passes the search and the filters, with what the search found in it.
struct Listed<'a> {
    session: &'a SessionInfo,
    /// The project it is listed under.
    project: PathBuf,
    /// Byte ranges of its title to draw in bold.
    marks: Vec<Range<usize>>,
    /// Its conversation hit.
    hit: Option<&'a SearchHit>,
}

/// The pop-up's list: the sessions that pass the search and the filters, in the order of `input.prefs`, then a note
/// on the conversation search.
pub fn items(input: &RecentsInput<'_>) -> Vec<RecentItem> {
    let mut rows = listed(input, None);
    let session_item = |row: &Listed<'_>| RecentItem::Session {
        session: Box::new(row.session.clone()),
        age: age(input.now_ms, row.session.last_active_ms),
        project: row.project.clone(),
        marks: row.marks.clone(),
        snippet: row.hit.map(Snippet::from_hit),
        open: is_open(input, row.session),
    };
    let mut items = Vec::new();
    match input.prefs.sort {
        Sort::Recent => {
            rows.sort_by(|a, b| newest(a.session, b.session));
            if input.prefs.reverse {
                rows.reverse();
            }
            items.extend(rows.iter().map(session_item));
        }
        Sort::Name => {
            rows.sort_by(|a, b| {
                session_title(a.session)
                    .to_lowercase()
                    .cmp(&session_title(b.session).to_lowercase())
                    .then_with(|| a.session.id.cmp(&b.session.id))
                    .then_with(|| a.session.host.name().cmp(b.session.host.name()))
            });
            if input.prefs.reverse {
                rows.reverse();
            }
            items.extend(rows.iter().map(session_item));
        }
        Sort::Project => {
            let mut by_project: HashMap<PathBuf, Vec<Listed<'_>>> = HashMap::new();
            for row in rows {
                by_project.entry(row.project.clone()).or_default().push(row);
            }
            let paths: Vec<PathBuf> = by_project.keys().cloned().collect();
            let labels = labels(&paths, input.home, input.gantry_names);
            let mut projects: Vec<(PathBuf, Vec<Listed<'_>>)> = by_project.into_iter().collect();
            projects.sort_by(|(a, _), (b, _)| {
                labels[a].to_lowercase().cmp(&labels[b].to_lowercase()).then_with(|| a.cmp(b))
            });
            if input.prefs.reverse {
                projects.reverse();
            }
            for (path, mut sessions) in projects {
                sessions.sort_by(|a, b| newest(a.session, b.session));
                items.push(RecentItem::Heading {
                    label: labels[&path].clone(),
                    display: display_path(&path, input.home),
                    path,
                });
                items.extend(sessions.iter().map(session_item));
            }
        }
    }
    items.extend(note(input).map(|text| RecentItem::Note { text }));
    items
}

fn newest(a: &SessionInfo, b: &SessionInfo) -> std::cmp::Ordering {
    b.last_active_ms.cmp(&a.last_active_ms).then_with(|| a.id.cmp(&b.id)).then_with(|| a.host.name().cmp(b.host.name()))
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

/// Byte ranges of the title to draw in bold when the typed words match the title (or only the folder: then none);
/// `None` when they match neither.
fn title_marks(input: &RecentsInput<'_>, words: &[String], session: &SessionInfo) -> Option<Vec<Range<usize>>> {
    let title = session_title(session);
    if let Some(marks) = word_starts(words, title) {
        return Some(marks);
    }
    let marks = word_starts(words, &format!("{title} {}", display_path(&session.cwd, input.home)))?;
    Some(marks.into_iter().filter(|r| r.end <= title.len()).collect())
}

/// The project key of `session`, as text (what a project filter selects).
pub fn project_key_of(input: &RecentsInput<'_>, session: &SessionInfo) -> String {
    (input.project_of)(session).to_string_lossy().into_owned()
}

/// The key of `session` in `facet`, or `None` when it has none.
fn key_of(input: &RecentsInput<'_>, facet: Facet, session: &SessionInfo) -> Option<String> {
    match facet {
        Facet::Project => Some(project_key_of(input, session)),
        Facet::Host => Some(session.host.name().to_string()),
        Facet::Dir => (input.dir_name)(session),
    }
}

/// Whether `session` passes every facet's selection except `skip`'s (a facet with nothing selected passes all).
fn passes(input: &RecentsInput<'_>, session: &SessionInfo, skip: Option<Facet>) -> bool {
    Facet::ALL.into_iter().filter(|&facet| Some(facet) != skip).all(|facet| {
        let selected = input.prefs.selected(facet);
        selected.is_empty() || key_of(input, facet, session).is_some_and(|key| selected.contains(&key))
    })
}

/// Closed sessions no leaf shows: what the pop-up can list.
fn closed_sessions<'a>(input: &RecentsInput<'a>) -> Vec<&'a SessionInfo> {
    input.sessions.iter().copied().filter(|s| s.state == SessionState::Closed && !is_open(input, s)).collect()
}

/// Whether the conversation search runs for this query (the words are not just punctuation).
fn searching_content(input: &RecentsInput<'_>) -> bool {
    !typed_words(input.query).is_empty() && searches_content(input.query)
}

/// The sessions that pass the search and every filter but `skip`'s, unordered: with no search every closed session;
/// with one, those matching by title, folder or conversation (plus open sessions found in their conversation).
fn listed<'a>(input: &RecentsInput<'a>, skip: Option<Facet>) -> Vec<Listed<'a>> {
    let words = typed_words(input.query);
    let hits: &[SearchHit] = if searching_content(input) { input.hits } else { &[] };
    let mut hit_of: HashMap<(&HostId, &str), &SearchHit> = HashMap::new();
    for hit in hits {
        hit_of.entry((&hit.host, hit.session_id.as_str())).or_insert(hit);
    }
    let open_hits =
        input.sessions.iter().copied().filter(|s| hit_of.contains_key(&(&s.host, s.id.as_str())) && is_open(input, s));
    let mut seen: HashSet<(&HostId, &str)> = HashSet::new();
    let mut rows = Vec::new();
    for session in closed_sessions(input).into_iter().chain(open_hits) {
        if !seen.insert((&session.host, session.id.as_str())) || !passes(input, session, skip) {
            continue;
        }
        let hit = hit_of.get(&(&session.host, session.id.as_str())).copied();
        let marks = if words.is_empty() {
            Vec::new()
        } else {
            match title_marks(input, &words, session) {
                Some(marks) => marks,
                None if hit.is_some() => Vec::new(),
                None => continue,
            }
        };
        rows.push(Listed { session, project: (input.project_of)(session), marks, hit });
    }
    rows
}

/// The line about the conversation search under the list, when it has something to say.
fn note(input: &RecentsInput<'_>) -> Option<String> {
    match input.content {
        _ if !searching_content(input) => None,
        ContentState::Indexing(percent) => Some(format!("Indexing conversations… {percent}%")),
        ContentState::Unavailable => Some("Conversation search is unavailable — see the daemon log".to_string()),
        ContentState::Ready if input.hits.is_empty() => Some("No conversations mention this".to_string()),
        _ => None,
    }
}

fn is_open(input: &RecentsInput<'_>, session: &SessionInfo) -> bool {
    input.open.contains(&(session.host.clone(), session.id.clone()))
}

/// Sessions in the list (after the search and the filters), and all the pop-up could list: the closed sessions and the
/// open ones found in their conversation.
pub fn counts(input: &RecentsInput<'_>) -> (usize, usize) {
    let hits: &[SearchHit] = if searching_content(input) { input.hits } else { &[] };
    let found: HashSet<(&HostId, &str)> = hits.iter().map(|hit| (&hit.host, hit.session_id.as_str())).collect();
    let open_hits: HashSet<(&HostId, &str)> = input
        .sessions
        .iter()
        .filter(|s| is_open(input, s) && found.contains(&(&s.host, s.id.as_str())))
        .map(|s| (&s.host, s.id.as_str()))
        .collect();
    (listed(input, None).len(), closed_sessions(input).len() + open_hits.len())
}

/// What the list area says when no session is listed: a title and a line, or `None` while the conversation search
/// has not answered yet.
pub fn empty_state(input: &RecentsInput<'_>) -> Option<(&'static str, &'static str)> {
    if closed_sessions(input).is_empty() && input.query.trim().is_empty() && !input.prefs.is_filtering() {
        return Some(("No closed sessions yet.", "Sessions you close show up here."));
    }
    if searching_content(input) && input.content == ContentState::Waiting {
        return None;
    }
    Some(("No sessions match.", "Clear a filter or change the search."))
}

/// How many values a filter section shows before "More…".
pub const SHOWN_PER_FILTER: usize = 5;

/// One value of a filter: a project, a host or a config dir.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterValue {
    /// What `Prefs` stores when it is selected.
    pub key: String,
    /// Its text.
    pub label: String,
    /// The project it stands for (`None` for a host or a config dir), for the project's color.
    pub project: Option<PathBuf>,
    /// Sessions that have this value and pass the search and the other sections' selections.
    pub count: usize,
    /// Whether it is selected.
    pub selected: bool,
}

/// One section of the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterSection {
    /// What it filters by.
    pub facet: Facet,
    /// The first [`SHOWN_PER_FILTER`] values, those with the newest sessions first.
    pub shown: Vec<FilterValue>,
    /// The others, in the same order (under "More…").
    pub more: Vec<FilterValue>,
}

/// The sidebar's sections: every facet that has at least two values among the closed sessions. Values are ordered by
/// their newest session (ties by label), so the order does not move while searching or ticking.
pub fn filter_sections(input: &RecentsInput<'_>) -> Vec<FilterSection> {
    let closed = closed_sessions(input);
    Facet::ALL
        .into_iter()
        .filter_map(|facet| {
            // key -> (newest session's time, the project)
            let mut found: HashMap<String, (u64, PathBuf)> = HashMap::new();
            for session in &closed {
                if let Some(key) = key_of(input, facet, session) {
                    let project = (input.project_of)(session);
                    let entry = found.entry(key).or_insert((0, project));
                    entry.0 = entry.0.max(session.last_active_ms);
                }
            }
            if found.len() < 2 {
                return None;
            }
            let paths: Vec<PathBuf> = found.values().map(|(_, project)| project.clone()).collect();
            let project_labels = labels(&paths, input.home, input.gantry_names);
            let mut counted: HashMap<String, usize> = HashMap::new();
            for row in listed(input, Some(facet)) {
                if let Some(key) = key_of(input, facet, row.session) {
                    *counted.entry(key).or_default() += 1;
                }
            }
            let mut values: Vec<(u64, FilterValue)> = found
                .into_iter()
                .map(|(key, (newest, project))| {
                    let label = match facet {
                        Facet::Project => project_labels[&project].clone(),
                        Facet::Host if key == HostId::local().name() => "This machine".to_string(),
                        Facet::Host | Facet::Dir => key.clone(),
                    };
                    let value = FilterValue {
                        count: counted.get(&key).copied().unwrap_or(0),
                        selected: input.prefs.selected(facet).contains(&key),
                        project: (facet == Facet::Project).then_some(project),
                        label,
                        key,
                    };
                    (newest, value)
                })
                .collect();
            values.sort_by(|(a_newest, a), (b_newest, b)| b_newest.cmp(a_newest).then_with(|| a.label.cmp(&b.label)));
            let mut shown: Vec<FilterValue> = values.into_iter().map(|(_, value)| value).collect();
            let more = shown.split_off(SHOWN_PER_FILTER.min(shown.len()));
            Some(FilterSection { facet, shown, more })
        })
        .collect()
}

/// `input.prefs` without the selections that match nothing the sidebar offers: a value that no longer exists, or a
/// whole section that is left out (fewer than two values). Without this a filter could narrow the list with no control
/// left to clear it.
pub fn prune(input: &RecentsInput<'_>) -> Prefs {
    let sections = filter_sections(input);
    let mut prefs = input.prefs.clone();
    for facet in Facet::ALL {
        let offered: HashSet<&str> = sections
            .iter()
            .filter(|section| section.facet == facet)
            .flat_map(|section| section.shown.iter().chain(&section.more))
            .map(|value| value.key.as_str())
            .collect();
        let gone: Vec<String> =
            prefs.selected(facet).iter().filter(|key| !offered.contains(key.as_str())).cloned().collect();
        for key in gone {
            prefs.toggle(facet, &key);
        }
    }
    prefs
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

    fn folder_project(host: &HostId, cwd: &Path) -> PathBuf {
        let root = cwd.ancestors().find(|dir| dir.parent() == Some(Path::new("/r"))).unwrap_or(cwd);
        project_key(host, root)
    }

    struct Fixture {
        sessions: Vec<SessionInfo>,
        open: HashSet<(HostId, String)>,
        query: String,
        prefs: Prefs,
        dirs: HashMap<PathBuf, String>,
        hits: Vec<SearchHit>,
        content: ContentState,
        /// Session id -> Gantry project name.
        gantry: HashMap<String, String>,
    }

    impl Default for Fixture {
        fn default() -> Self {
            Fixture {
                sessions: Vec::new(),
                open: HashSet::new(),
                query: String::new(),
                prefs: Prefs::default(),
                dirs: HashMap::new(),
                hits: Vec::new(),
                content: ContentState::Waiting,
                gantry: HashMap::new(),
            }
        }
    }

    impl Fixture {
        fn of(sessions: Vec<SessionInfo>) -> Fixture {
            Fixture { sessions, ..Fixture::default() }
        }

        fn with<T>(&self, f: impl FnOnce(&RecentsInput<'_>) -> T) -> T {
            let refs: Vec<&SessionInfo> = self.sessions.iter().collect();
            let dir_name =
                |session: &SessionInfo| session.claude_dir.as_ref().and_then(|dir| self.dirs.get(dir).cloned());
            let project_of = |session: &SessionInfo| match self.gantry.get(&session.id) {
                Some(name) => crate::gantry_project::key(name),
                None => folder_project(&session.host, &session.cwd),
            };
            let names: Vec<String> = self.gantry.values().cloned().collect();
            f(&RecentsInput {
                sessions: &refs,
                open: &self.open,
                home: Some(Path::new("/home/me")),
                query: &self.query,
                prefs: &self.prefs,
                dir_name: &dir_name,
                project_of: &project_of,
                gantry_names: &names,
                now_ms: NOW,
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
                })
                .collect()
        }

        fn sorted_clone(&self, sort: Sort) -> Fixture {
            Fixture {
                sessions: self.sessions.clone(),
                open: self.open.clone(),
                prefs: self.prefs.clone(),
                ..Fixture::default()
            }
            .sorted(sort)
        }

        fn sorted(mut self, sort: Sort) -> Fixture {
            self.prefs.set_sort(sort);
            self
        }
    }

    fn many(project: &str, n: u64, start_hour: u64) -> Vec<SessionInfo> {
        (0..n).map(|i| closed(&format!("{project}{i}"), &format!("/r/{project}"), start_hour + i)).collect()
    }

    #[test]
    fn closed_sessions_newest_first_without_live_or_open_ones() {
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
        assert_eq!(fixture.outline(), vec!["b", "a-new", "a-old"]);
        assert_eq!(fixture.with(counts), (3, 3));
    }

    #[test]
    fn a_session_item_names_its_project() {
        let fixture = Fixture::of(vec![closed("a", "/r/a", 1), closed("b", "/r/b", 2)]).sorted(Sort::Project);
        let items = fixture.with(items);
        let mut heading = None;
        for item in &items {
            match item {
                RecentItem::Heading { path, .. } => heading = Some(path.clone()),
                RecentItem::Session { project, .. } => assert_eq!(Some(project), heading.as_ref()),
                RecentItem::Note { .. } => {}
            }
        }
        assert!(
            items
                .iter()
                .any(|item| matches!(item, RecentItem::Session { project, .. } if project == Path::new("/r/a")))
        );
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
    fn sessions_whose_folder_is_gone_are_listed() {
        let gone = SessionInfo { cwd_exists: false, ..closed("gone", "/r/a", 1) };
        let fixture = Fixture::of(vec![gone]);
        assert_eq!(fixture.outline(), vec!["gone"]);
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
    fn conversation_hits_are_listed_in_the_chosen_order() {
        let mut fixture = Fixture::of(vec![closed("a1", "/r/a", 1), closed("b1", "/r/b", 2), closed("b2", "/r/b", 3)]);
        fixture.query = "title a1".into();
        fixture.hits = vec![hit("b2", "x", vec![]), hit("a1", "y", vec![]), hit("b1", "z", vec![])];
        fixture.content = ContentState::Ready;
        assert_eq!(fixture.outline(), vec!["~a1", "~b1", "~b2"], "newest first, whatever the search ranked");
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
    fn the_same_folder_on_two_hosts_is_two_headings_named_with_the_host() {
        let remote = SessionInfo { host: HostId::named("devbox"), ..closed("r", "/r/app", 2) };
        let fixture = Fixture::of(vec![closed("l", "/r/app", 1), remote]).sorted(Sort::Project);
        assert_eq!(fixture.outline(), vec!["# app", "l", "# devbox - app", "r"]);
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

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn recent_sort_is_newest_first_with_id_tiebreak_and_reverse_is_oldest_first() {
        let mut fixture = Fixture::of(vec![closed("b", "/r/a", 2), closed("c", "/r/a", 1), closed("a", "/r/b", 2)]);
        assert_eq!(fixture.outline(), ids(&["c", "a", "b"]));
        fixture.prefs.reverse = true;
        assert_eq!(fixture.outline(), ids(&["b", "a", "c"]));
    }

    #[test]
    fn a_loose_recent_match_comes_above_a_strong_old_match() {
        let mut fixture =
            Fixture::of(vec![titled("old", "Sound check", "/r/a", 100), titled("recent", "Unrelated", "/r/b", 1)]);
        fixture.query = "sound".into();
        fixture.hits = vec![hit("recent", "x", vec![])];
        fixture.content = ContentState::Ready;
        assert_eq!(fixture.outline(), ids(&["~recent", "old"]));
    }

    #[test]
    fn search_only_narrows() {
        let mut fixture = Fixture::of(vec![
            titled("a1", "alpha one", "/r/a", 3),
            titled("b1", "beta two", "/r/a", 2),
            titled("a2", "alpha three", "/r/b", 1),
            titled("a3", "alpha four", "/r/b", 4),
        ]);
        let everything = fixture.outline();
        fixture.query = "alpha".into();
        let narrowed: Vec<String> = everything.into_iter().filter(|id| id.starts_with('a')).collect();
        assert_eq!(fixture.outline(), narrowed);
        assert_eq!(fixture.with(counts), (3, 4));
    }

    #[test]
    fn name_sort_ignores_case_and_ties_by_id() {
        let mut fixture = Fixture::of(vec![
            titled("b", "beta", "/r/a", 1),
            titled("a2", "alpha", "/r/a", 2),
            titled("a1", "Alpha", "/r/a", 3),
        ])
        .sorted(Sort::Name);
        assert_eq!(fixture.outline(), ids(&["a1", "a2", "b"]));
        fixture.prefs.reverse = true;
        assert_eq!(fixture.outline(), ids(&["b", "a2", "a1"]));
    }

    fn project_fixture() -> Fixture {
        Fixture::of(vec![
            closed("z1", "/r/zed", 1),
            closed("a1", "/r/api", 5),
            closed("a2", "/r/api", 2),
            closed("m", "/r/Mid", 3),
        ])
        .sorted(Sort::Project)
    }

    #[test]
    fn project_sort_groups_under_headings_projects_a_to_z_sessions_newest_first() {
        assert_eq!(project_fixture().outline(), ids(&["# api", "a2", "a1", "# Mid", "m", "# zed", "z1"]));
    }

    #[test]
    fn project_reverse_flips_projects_not_the_sessions_inside() {
        let mut fixture = project_fixture();
        fixture.prefs.reverse = true;
        assert_eq!(fixture.outline(), ids(&["# zed", "z1", "# Mid", "m", "# api", "a2", "a1"]));
    }

    fn key(host: &HostId, folder: &str) -> String {
        project_key(host, Path::new(folder)).to_string_lossy().into_owned()
    }

    #[test]
    fn facet_selection_ors_inside_a_section_and_ands_across() {
        let devbox = HostId::named("devbox");
        let on = |id: &str, folder: &str, ago: u64| SessionInfo { host: devbox.clone(), ..closed(id, folder, ago) };
        let mut fixture = Fixture::of(vec![
            closed("a-local", "/r/a", 1),
            on("a-dev", "/r/a", 2),
            closed("b-local", "/r/b", 3),
            on("b-dev", "/r/b", 4),
            on("c-dev", "/r/c", 5),
        ]);
        fixture.prefs.toggle(Facet::Project, &key(&HostId::local(), "/r/a"));
        fixture.prefs.toggle(Facet::Project, &key(&devbox, "/r/b"));
        // The same folder on another host is another project, so "a-dev" is not selected.
        assert_eq!(fixture.outline(), ids(&["a-local", "b-dev"]), "a project OR the other");
        fixture.prefs.toggle(Facet::Host, "devbox");
        assert_eq!(fixture.outline(), ids(&["b-dev"]), "and on that host");
    }

    #[test]
    fn a_remote_session_matches_no_config_dir_value() {
        let remote = SessionInfo { host: HostId::named("devbox"), ..closed("remote", "/r/a", 1) };
        let local = SessionInfo { claude_dir: Some("/c/default".into()), ..closed("local", "/r/a", 2) };
        let mut fixture = Fixture::of(vec![remote, local]);
        fixture.dirs.insert("/c/default".into(), "Default".into());
        assert_eq!(fixture.outline(), ids(&["remote", "local"]));
        fixture.prefs.toggle(Facet::Dir, "Default");
        assert_eq!(fixture.outline(), ids(&["local"]));
    }

    fn section(fixture: &Fixture, facet: Facet) -> Option<FilterSection> {
        fixture.with(filter_sections).into_iter().find(|section| section.facet == facet)
    }

    fn shown(section: &FilterSection) -> Vec<String> {
        section.shown.iter().map(|value| format!("{} {}", value.label, value.count)).collect()
    }

    #[test]
    fn the_five_values_with_the_newest_sessions_are_shown_the_rest_are_more() {
        let sessions: Vec<SessionInfo> = (0..7).map(|i| closed(&format!("s{i}"), &format!("/r/p{i}"), i + 1)).collect();
        let fixture = Fixture::of(sessions);
        let projects = section(&fixture, Facet::Project).expect("seven projects");
        assert_eq!(shown(&projects), ["p0 1", "p1 1", "p2 1", "p3 1", "p4 1"]);
        assert_eq!(projects.more.iter().map(|v| v.label.as_str()).collect::<Vec<_>>(), ["p5", "p6"]);
        assert!(section(&fixture, Facet::Host).is_none(), "one host is not worth a filter");
        assert!(section(&fixture, Facet::Dir).is_none(), "no config dirs");
    }

    #[test]
    fn a_section_with_one_value_is_left_out() {
        let fixture = Fixture::of(vec![closed("a", "/r/a", 1), closed("b", "/r/a", 2)]);
        assert!(fixture.with(filter_sections).is_empty());
    }

    #[test]
    fn counts_follow_the_search_and_the_other_sections() {
        let devbox = HostId::named("devbox");
        let remote = SessionInfo { host: devbox.clone(), ..closed("b-dev", "/r/b", 3) };
        let mut fixture = Fixture::of(vec![closed("a1", "/r/a", 1), closed("a2", "/r/a", 2), remote]);
        assert_eq!(shown(&section(&fixture, Facet::Project).unwrap()), ["a 2", "devbox - b 1"]);
        fixture.prefs.toggle(Facet::Host, "devbox");
        assert_eq!(
            shown(&section(&fixture, Facet::Project).unwrap()),
            ["a 0", "devbox - b 1"],
            "the host narrows projects"
        );
        assert_eq!(
            shown(&section(&fixture, Facet::Host).unwrap()),
            ["This machine 2", "devbox 1"],
            "its own selection does not"
        );
        assert!(section(&fixture, Facet::Host).unwrap().shown[1].selected);
        fixture.query = "a1".into();
        assert_eq!(
            shown(&section(&fixture, Facet::Host).unwrap()),
            ["This machine 1", "devbox 0"],
            "the search narrows"
        );
    }

    #[test]
    fn project_values_of_two_hosts_are_separate_and_labelled_with_the_host() {
        let remote = SessionInfo { host: HostId::named("devbox"), ..closed("r", "/r/app", 2) };
        let fixture = Fixture::of(vec![closed("l", "/r/app", 1), remote]);
        assert_eq!(shown(&section(&fixture, Facet::Project).unwrap()), ["app 1", "devbox - app 1"]);
    }

    #[test]
    fn prune_drops_values_that_no_longer_exist() {
        let remote = SessionInfo { host: HostId::named("devbox"), ..closed("r", "/r/b", 2) };
        let mut fixture = Fixture::of(vec![closed("l", "/r/a", 1), remote]);
        fixture.prefs.toggle(Facet::Project, "/r/gone");
        fixture.prefs.toggle(Facet::Host, "devbox");
        let pruned = fixture.with(prune);
        assert!(pruned.selected(Facet::Project).is_empty());
        assert!(pruned.selected(Facet::Host).contains("devbox"));
    }

    #[test]
    fn prune_drops_a_selection_in_a_section_that_is_left_out() {
        let mut fixture = Fixture::of(vec![closed("a", "/r/a", 1), closed("b", "/r/b", 2)]);
        fixture.prefs.toggle(Facet::Host, "devbox");
        fixture.prefs.toggle(Facet::Dir, "Work");
        fixture.prefs.set_sort(Sort::Name);
        let pruned = fixture.with(prune);
        assert!(!pruned.is_filtering(), "no filter stays active without a control to clear it");
        assert_eq!(pruned.sort, Sort::Name);
    }

    #[test]
    fn no_match_with_filters_leaves_counts_zero_of_total() {
        let mut fixture = Fixture::of(vec![closed("a", "/r/a", 1), closed("b", "/r/b", 2)]);
        fixture.prefs.toggle(Facet::Project, &key(&HostId::local(), "/r/a"));
        fixture.query = "title b".into();
        assert!(fixture.outline().is_empty());
        assert_eq!(fixture.with(counts), (0, 2));
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
    fn only_project_clears_every_other_filter() {
        let mut prefs = Prefs::default();
        prefs.toggle(Facet::Project, "/r/a");
        prefs.toggle(Facet::Host, "devbox");
        prefs.toggle(Facet::Dir, "Work");
        prefs.only_project("/r/b".into());
        assert_eq!(prefs.selected(Facet::Project).iter().collect::<Vec<_>>(), ["/r/b"]);
        assert!(prefs.selected(Facet::Host).is_empty());
        assert!(prefs.selected(Facet::Dir).is_empty());
    }

    #[test]
    fn the_count_never_exceeds_its_total() {
        let mut open = closed("open", "/r/a", 1);
        open.state = SessionState::Managed { pane: 2 };
        let mut fixture = Fixture::of(vec![
            titled("c1", "sound one", "/r/a", 1),
            titled("c2", "sound two", "/r/a", 2),
            titled("c3", "sound three", "/r/a", 3),
            open,
        ]);
        fixture.open.insert((HostId::local(), "open".into()));
        fixture.query = "sound".into();
        fixture.hits = vec![hit("open", "sound", vec![])];
        fixture.content = ContentState::Ready;
        let (matching, total) = fixture.with(counts);
        assert_eq!(matching, 4);
        assert!(matching <= total, "{matching} of {total}");
    }

    #[test]
    fn the_same_session_on_two_hosts_keeps_one_order() {
        let remote = SessionInfo { host: HostId::named("devbox"), ..closed("same", "/r/a", 1) };
        let hosts = |fixture: &Fixture| -> Vec<String> {
            fixture
                .with(items)
                .iter()
                .filter_map(|item| match item {
                    RecentItem::Session { session, .. } => Some(session.host.name().to_string()),
                    _ => None,
                })
                .collect()
        };
        let one = Fixture::of(vec![closed("same", "/r/a", 1), remote.clone()]);
        let other = Fixture::of(vec![remote, closed("same", "/r/a", 1)]);
        for sort in [Sort::Recent, Sort::Name, Sort::Project] {
            assert_eq!(hosts(&one.sorted_clone(sort)), hosts(&other.sorted_clone(sort)), "{sort:?}");
        }
    }

    #[test]
    fn the_empty_state_says_why_nothing_is_listed() {
        let mut fixture = Fixture::of(Vec::new());
        assert_eq!(fixture.with(empty_state), Some(("No closed sessions yet.", "Sessions you close show up here.")));
        fixture.sessions = vec![closed("a1", "/r/a", 1), closed("b1", "/r/b", 2)];
        fixture.query = "zzz".into();
        fixture.content = ContentState::Ready;
        assert_eq!(fixture.with(empty_state).map(|(title, _)| title), Some("No sessions match."));
        fixture.content = ContentState::Waiting;
        fixture.query = "zzz".into();
        assert_eq!(fixture.with(empty_state), None, "nothing until the conversation search answers");
        fixture.content = ContentState::Ready;
        fixture.query = String::new();
        fixture.prefs.toggle(Facet::Project, "/r/a");
        fixture.prefs.toggle(Facet::Host, "nowhere");
        assert_eq!(fixture.with(empty_state).map(|(title, _)| title), Some("No sessions match."));
    }

    #[test]
    fn is_filtering_follows_the_selections() {
        let mut prefs = Prefs::default();
        assert!(!prefs.is_filtering());
        prefs.toggle(Facet::Project, "/r/a");
        assert!(prefs.is_filtering());
    }

    #[test]
    fn closed_session_with_gantry_name_is_keyed_gantry() {
        let mut fixture = Fixture::of(vec![closed("a", "/r/app", 1), closed("b", "/r/app", 2)]);
        fixture.gantry.insert("a".into(), "Billing".into());
        fixture.with(|input| {
            assert_eq!(project_key_of(input, input.sessions[0]), "gantry://billing");
            assert_eq!(project_key_of(input, input.sessions[1]), "/r/app");
        });
    }

    #[test]
    fn deleted_project_name_still_filters() {
        // The name lives only in the session map: no project exists, the session is still keyed and filterable.
        let mut fixture = Fixture::of(vec![closed("a", "/r/app", 1), closed("b", "/r/app", 2)]);
        fixture.gantry.insert("a".into(), "Gone".into());
        fixture.prefs.toggle(Facet::Project, "gantry://gone");
        let shown = fixture.with(|input| {
            items(input)
                .into_iter()
                .filter_map(|item| match item {
                    RecentItem::Session { session, .. } => Some(session.id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(shown, vec!["a".to_string()]);
    }

    #[test]
    fn project_filter_label_is_the_name_not_a_path() {
        let mut fixture = Fixture::of(vec![closed("a", "/r/app", 1), closed("b", "/r/app", 2)]);
        fixture.gantry.insert("a".into(), "Billing".into());
        let sections = fixture.with(filter_sections);
        let project = sections.iter().find(|section| section.facet == Facet::Project).expect("two projects");
        let labels: Vec<&str> = project.shown.iter().map(|value| value.label.as_str()).collect();
        assert!(labels.contains(&"Billing"), "{labels:?}");
        assert!(labels.contains(&"app"), "{labels:?}");
    }

    fn session_ids(fixture: &Fixture) -> Vec<String> {
        fixture.with(|input| {
            items(input)
                .into_iter()
                .filter_map(|item| match item {
                    RecentItem::Session { session, .. } => Some(session.id.clone()),
                    _ => None,
                })
                .collect()
        })
    }

    #[test]
    fn recents_for_gantry_key_selects_that_project_value() {
        let mut fixture =
            Fixture::of(vec![closed("a", "/r/app", 1), closed("b", "/r/app", 2), closed("c", "/r/app", 3)]);
        fixture.gantry.insert("a".into(), "Billing".into());
        fixture.gantry.insert("c".into(), "Other".into());
        fixture.prefs.only_project(crate::gantry_project::key("Billing").to_string_lossy().into_owned());
        assert_eq!(session_ids(&fixture), vec!["a".to_string()]);
    }

    #[test]
    fn renamed_project_relabels_closed_sessions() {
        let mut ws = crate::workspace::Workspace::default();
        ws.create_project("Billing").unwrap();
        ws.session_projects.insert("a".into(), "Billing".into());
        ws.rename_project("Billing", "Invoices").unwrap();
        let membership = ws.membership();
        let mut fixture = Fixture::of(vec![closed("a", "/r/app", 1), closed("b", "/r/other", 2)]);
        fixture.gantry = membership.sessions.clone().into_iter().collect();
        let sections = fixture.with(filter_sections);
        let project = sections.iter().find(|section| section.facet == Facet::Project).expect("two projects");
        let labels: Vec<&str> = project.shown.iter().map(|value| value.label.as_str()).collect();
        assert!(labels.contains(&"Invoices"), "{labels:?}");
        assert!(!labels.contains(&"Billing"), "{labels:?}");
    }

    #[test]
    fn deleted_project_listed_like_a_folder_project_without_open_sessions() {
        // Nothing of the project exists any more: its closed session still sits under its name, like a folder project
        // whose sessions are all closed.
        let mut fixture = Fixture::of(vec![closed("a", "/r/app", 1), closed("b", "/r/other", 2)]);
        fixture.gantry.insert("a".into(), "Gone".into());
        fixture.prefs.sort = Sort::Project;
        let outline = fixture.outline();
        assert!(outline.contains(&"# Gone".to_string()), "{outline:?}");
        assert!(outline.contains(&"# other".to_string()), "{outline:?}");
    }
}
