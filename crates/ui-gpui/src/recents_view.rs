//! The recent sessions overlay (`Ctrl+Shift+R`): closed sessions in the order picked in the sort bar, narrowed by the
//! search and by the filters in the sidebar. The sort and the filters last while Gantry runs (`RecentsPrefs`).

use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use app_core::gantry_project::Membership;
use app_core::recents::{
    self, ContentState, Facet, FilterSection, FilterValue, Prefs, RecentItem, RecentsInput, Snippet, Sort,
    searches_content,
};
use app_core::session_tree::{ProjectRoots, session_title};
use app_core::text_field::TextField;
use gpui::{
    App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Global, HighlightStyle, IntoElement,
    KeyDownEvent, ListAlignment, ListState, Pixels, Point, Render, SharedString, Stateful, StyledText, Window,
    anchored, deferred, div, list, prelude::*, px, rgb, rgba, svg,
};
use protocol::host::HostId;
use protocol::message::{SearchHit, SessionInfo, Speaker};

use crate::assets::RESET_ICON;
use crate::field;
use crate::settings_store::{SettingsStore, dir_badge_for};
use crate::theme::{
    FadeKind, RADIUS, RADIUS_LARGE, RADIUS_SMALL, Theme, container_icon, dir_badge, fade_in, side_line, ssh_icon, theme,
};

const ROW_HEIGHT: f32 = 36.0;
/// Height of the list area.
const LIST_HEIGHT: f32 = 380.0;
/// Height of a conversation hit's row (two lines).
const HIT_ROW_HEIGHT: f32 = 56.0;
/// Quiet time after typing before the query is sent.
const SEARCH_DELAY: Duration = Duration::from_millis(150);
/// Most sessions asked for (the filters narrow them here).
pub const SEARCH_LIMIT: u32 = 200;
/// Request ids, unique across pop-ups (a late reply to a closed pop-up must not match a new one's).
static NEXT_SEARCH: AtomicU64 = AtomicU64::new(1);

/// What the overlay asks the workspace to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecentsEvent {
    /// Resume a session.
    Resume {
        /// The host it ran on.
        host: HostId,
        /// Session id.
        session: String,
        /// Its directory.
        cwd: PathBuf,
        /// As a split of the focused pane.
        split: bool,
    },
    /// Connect an SSH host that is not connected.
    Connect(HostId),
    /// Close without opening anything.
    Dismiss,
    /// Search the conversations for `query`; the reply goes to `set_hits` with the same `request`.
    Search {
        /// Request id.
        request: u64,
        /// The query.
        query: String,
    },
}

/// The sort and filters of the last pop-up, kept for the next one while Gantry runs.
pub struct RecentsPrefs(pub Prefs);

impl Global for RecentsPrefs {}

/// Which Gantry project each session is in, published by the workspace on every layout change.
#[derive(Default)]
pub struct ProjectMembership(pub Membership);

impl Global for ProjectMembership {}

/// The "More…" popup of one filter section.
struct More {
    facet: Facet,
    /// Narrows the popup's values by name.
    field: TextField,
    /// Where the "More…" row was clicked; the popup opens beside it.
    at: Point<Pixels>,
}

/// The recent sessions overlay.
pub struct RecentsView {
    focus_handle: FocusHandle,
    sessions: Vec<SessionInfo>,
    open: HashSet<(HostId, String)>,
    home: Option<PathBuf>,
    roots: ProjectRoots,
    query: TextField,
    prefs: Prefs,
    items: Vec<RecentItem>,
    sections: Vec<FilterSection>,
    more: Option<More>,
    counts: (usize, usize),
    /// What the list area says while no session is listed (`None` = nothing yet).
    empty: Option<(&'static str, &'static str)>,
    selected: usize,
    list_state: ListState,
    /// The last conversation search sent (0 = none).
    request: u64,
    /// Whether the reply to `request` arrived.
    answered: bool,
    hits: Vec<SearchHit>,
    /// Percent of the index built while a large pass runs.
    indexing: Option<u8>,
    /// The hosts whose conversation search does not work.
    unavailable: Vec<HostId>,
    /// SSH hosts that are not connected: each adds a "not connected" line with a Connect button.
    offline: Vec<String>,
    search_task: Option<gpui::Task<()>>,
}

impl EventEmitter<RecentsEvent> for RecentsView {}

impl Focusable for RecentsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl RecentsView {
    /// An overlay over `sessions`; `open` = (host, session) a leaf of the layout shows; `project` = the project to
    /// filter by from the start; `offline` = names of the SSH hosts that are not connected. The sort and filters are
    /// those of the last overlay.
    pub fn new(
        sessions: Vec<SessionInfo>,
        open: HashSet<(HostId, String)>,
        project: Option<PathBuf>,
        offline: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let home = std::env::home_dir();
        let mut prefs = cx.try_global::<RecentsPrefs>().map(|last| last.0.clone()).unwrap_or_default();
        if let Some(project) = project {
            prefs.only_project(project.to_string_lossy().into_owned());
        }
        let mut view = RecentsView {
            focus_handle: cx.focus_handle(),
            sessions,
            open,
            roots: ProjectRoots::new(home.clone()),
            home,
            query: TextField::default(),
            prefs,
            items: Vec::new(),
            sections: Vec::new(),
            more: None,
            counts: (0, 0),
            empty: None,
            selected: 0,
            list_state: ListState::new(0, ListAlignment::Top, px(200.)),
            request: 0,
            answered: false,
            hits: Vec::new(),
            indexing: None,
            unavailable: Vec::new(),
            offline,
            search_task: None,
        };
        view.refresh(cx);
        view
    }

    /// Rebuild the list after the user changed what it shows (query, sort, filter): back to the top, first row selected.
    fn refresh(&mut self, cx: &mut App) {
        self.rebuild(cx);
        self.selected = self.first_selectable();
        self.list_state.reset(self.len());
    }

    fn rebuild(&mut self, cx: &mut App) {
        let sessions: Vec<&SessionInfo> = self.sessions.iter().collect();
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let roots = &self.roots;
        let membership = cx.try_global::<ProjectMembership>().map(|global| global.0.clone()).unwrap_or_default();
        let names = membership.names();
        let project_of =
            |session: &SessionInfo| membership.key_of(&session.id, || roots.of(&session.host, &session.cwd));
        let settings = cx.global::<SettingsStore>().settings();
        let dir_name = |session: &SessionInfo| {
            dir_badge_for(settings, session.claude_dir.as_deref(), &session.host).map(|badge| badge.name)
        };
        let mut prefs = self.prefs.clone();
        // A selection the sidebar no longer offers is dropped first, so no filter narrows the list unseen.
        let (items, sections, counts, empty) = loop {
            let input = RecentsInput {
                sessions: &sessions,
                open: &self.open,
                home: self.home.as_deref(),
                query: self.query.text(),
                prefs: &prefs,
                dir_name: &dir_name,
                project_of: &project_of,
                gantry_names: &names,
                now_ms,
                hits: &self.hits,
                content: self.content_state(),
            };
            let pruned = recents::prune(&input);
            if pruned == prefs {
                break (
                    recents::items(&input),
                    recents::filter_sections(&input),
                    recents::counts(&input),
                    recents::empty_state(&input),
                );
            }
            prefs = pruned;
        };
        self.prefs = prefs;
        self.items = items;
        self.sections = sections;
        self.counts = counts;
        self.empty = empty;
        // A "More…" popup whose section no longer has hidden values has nothing left to show.
        if let Some(more) = &self.more
            && !self.sections.iter().any(|section| section.facet == more.facet && !section.more.is_empty())
        {
            self.more = None;
        }
        cx.set_global(RecentsPrefs(self.prefs.clone()));
    }

    fn content_state(&self) -> ContentState {
        if self.unavailable.iter().any(HostId::is_local) {
            ContentState::Unavailable
        } else if let Some(percent) = self.indexing {
            ContentState::Indexing(percent)
        } else if self.answered {
            ContentState::Ready
        } else {
            ContentState::Waiting
        }
    }

    /// The query changed: send it after `SEARCH_DELAY` when it is long enough (the last hits stay until the reply,
    /// so the section does not flicker while typing).
    fn query_changed(&mut self, cx: &mut Context<Self>) {
        self.search_task = None;
        self.request = 0;
        self.answered = false;
        if !searches_content(self.query.text()) {
            self.hits.clear();
            return;
        }
        let query = self.query.text().to_string();
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DELAY).await;
            let _ = this.update(cx, |this, cx| this.send_search(query, cx));
        }));
    }

    fn send_search(&mut self, query: String, cx: &mut Context<Self>) {
        self.request = NEXT_SEARCH.fetch_add(1, Ordering::Relaxed);
        cx.emit(RecentsEvent::Search { request: self.request, query });
    }

    /// Hits for `request`; replies to older requests are ignored. `pending`: a queried host has not answered yet, so
    /// an empty list does not mean "no conversations" yet.
    pub fn set_hits(&mut self, request: u64, hits: Vec<SearchHit>, pending: bool, cx: &mut Context<Self>) {
        if request == 0 || request != self.request {
            return;
        }
        self.hits = hits;
        self.answered = !pending;
        self.refresh_keeping_selection(cx);
        cx.notify();
    }

    /// The index's state: `indexing` = percent while a large pass runs. When indexing ends, the query is sent again.
    pub fn set_index_state(&mut self, indexing: Option<u8>, cx: &mut Context<Self>) {
        let finished = self.indexing.is_some() && indexing.is_none();
        self.indexing = indexing;
        if finished && searches_content(self.query.text()) {
            self.send_search(self.query.text().to_string(), cx);
        }
        self.refresh_keeping_selection(cx);
        cx.notify();
    }

    /// The hosts whose conversation search does not work: this machine's shows in the list, the others as a line
    /// under it.
    pub fn set_search_unavailable(&mut self, hosts: Vec<HostId>, cx: &mut Context<Self>) {
        self.unavailable = hosts;
        self.refresh_keeping_selection(cx);
        cx.notify();
    }

    /// New sessions (a host connected) and the hosts still not connected.
    pub fn set_sessions(&mut self, sessions: Vec<SessionInfo>, offline: Vec<String>, cx: &mut Context<Self>) {
        self.sessions = sessions;
        self.offline = offline;
        self.refresh_keeping_selection(cx);
        cx.notify();
    }

    /// Rebuild after a change the user didn't make (hits, the index's state): only the rows that changed are
    /// re-measured, so the scroll position stays and the selected row can still be revealed.
    fn refresh_keeping_selection(&mut self, cx: &mut App) {
        let keep = self.selected;
        let old = std::mem::take(&mut self.items);
        self.items = old.clone();
        self.rebuild(cx);
        let (same_start, same_end) = unchanged_ends(&old, &self.items);
        self.list_state.splice(same_start..old.len() - same_end, self.len() - same_start - same_end);
        self.selected = if self.selectable(keep) { keep } else { self.first_selectable() };
    }

    fn len(&self) -> usize {
        self.items.len()
    }

    fn selectable(&self, index: usize) -> bool {
        self.items.get(index).is_some_and(RecentItem::selectable)
    }

    fn first_selectable(&self) -> usize {
        (0..self.len()).find(|&i| self.selectable(i)).unwrap_or(0)
    }

    fn move_selection(&mut self, down: bool) {
        let candidates: Vec<usize> = (0..self.len()).filter(|&i| self.selectable(i)).collect();
        let next = if down {
            candidates.iter().copied().find(|&i| i > self.selected)
        } else {
            candidates.iter().rev().copied().find(|&i| i < self.selected)
        };
        if let Some(next) = next {
            self.selected = next;
        }
    }

    fn choose(&mut self, index: usize, split: bool, cx: &mut Context<Self>) {
        if let Some(RecentItem::Session { session, .. }) = self.items.get(index)
            && session.cwd_exists
        {
            cx.emit(RecentsEvent::Resume {
                host: session.host.clone(),
                session: session.id.clone(),
                cwd: session.cwd.clone(),
                split,
            });
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if let Some(more) = self.more.as_mut() {
            // The "More…" popup has the keyboard: typing narrows its values, Enter and Esc close it.
            if matches!(keystroke.key.as_str(), "escape" | "enter") {
                self.more = None;
            } else {
                field::edit(&mut more.field, keystroke, cx);
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        match keystroke.key.as_str() {
            "up" => self.move_selection(false),
            "down" => self.move_selection(true),
            "enter" => self.choose(self.selected, keystroke.modifiers.secondary(), cx),
            "tab" => {
                self.prefs.set_sort(self.prefs.sort.cycle(!keystroke.modifiers.shift));
                self.refresh(cx);
            }
            "escape" => cx.emit(RecentsEvent::Dismiss),
            _ => {
                let before = self.query.text().to_string();
                if !field::edit(&mut self.query, keystroke, cx) {
                    return;
                }
                // Cursor moves and copying change nothing to search for.
                if self.query.text() != before {
                    self.query_changed(cx);
                    self.refresh(cx);
                }
            }
        }
        self.list_state.scroll_to_reveal_item(self.selected);
        cx.stop_propagation();
        cx.notify();
    }

    fn render_item(&self, index: usize, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let selected = index == self.selected;
        let theme = theme(cx).clone();
        let row = div().id(index).w_full().h(theme.ui(ROW_HEIGHT)).px_2().py(px(2.));
        let pill = || {
            div()
                .size_full()
                .px_3()
                .flex()
                .items_center()
                .gap_2()
                .rounded(px(RADIUS))
                .border_2()
                .border_color(if selected { rgb(theme.tokens.accent) } else { gpui::rgba(0) })
                .when(selected, |pill| pill.bg(rgb(theme.tokens.raised)))
                .cursor_pointer()
                .overflow_hidden()
                .whitespace_nowrap()
        };
        let row = match &self.items[index] {
            RecentItem::Heading { path, label, display } => {
                let color = theme.project(path);
                return row.child(
                    div()
                        .size_full()
                        .px_3()
                        .flex()
                        .items_end()
                        .gap_2()
                        .pb(theme.ui(4.))
                        .children(color.map(|color| {
                            div().flex_none().mb(theme.ui(5.)).size(theme.ui(8.)).rounded(px(3.)).bg(rgb(color))
                        }))
                        .child(
                            div()
                                .flex_none()
                                .font_weight(FontWeight::SEMIBOLD)
                                .when_some(color, |label, color| label.text_color(rgb(color)))
                                .child(label.clone()),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(rgb(theme.tokens.muted))
                                .child(display.clone()),
                        ),
                );
            }
            // A flat list has no project headings, so each row names its project.
            RecentItem::Session { session, age, project, marks, snippet: None, .. } => row.child(
                pill()
                    .when_some(theme.project(project), |pill, color| {
                        pill.relative().pl(theme.ui(18.)).child(side_line(color, &theme))
                    })
                    .child(
                        div()
                            .flex_none()
                            .size(theme.ui(8.))
                            .rounded_full()
                            .border_1()
                            .border_color(rgb(theme.tokens.muted)),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(title_text(session, marks)))
                    .when(session.container.is_some(), |pill| pill.child(container_icon(theme.tokens.text, &theme)))
                    .when(!session.host.is_local(), |pill| pill.child(ssh_icon(theme.tokens.text, &theme)))
                    .children(
                        dir_badge_for(
                            cx.global::<SettingsStore>().settings(),
                            session.claude_dir.as_deref(),
                            &session.host,
                        )
                        .map(|badge| dir_badge(&badge, &theme)),
                    )
                    .when(self.prefs.sort != Sort::Project, |pill| {
                        let name =
                            project.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                        let name = match session.host.remote_name() {
                            Some(host) => format!("{host} - {name}"),
                            None => name,
                        };
                        pill.child(div().flex_none().text_xs().text_color(rgb(theme.tokens.muted)).child(name))
                    })
                    .child(
                        div().flex_none().text_xs().text_color(rgb(theme.tokens.muted)).child(if session.cwd_exists {
                            age.clone()
                        } else {
                            "Folder missing".into()
                        }),
                    ),
            ),
            RecentItem::Session { session, age, project, marks, snippet: Some(snippet), open } => {
                let project_name =
                    project.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                let project_name = match session.host.remote_name() {
                    Some(host) => format!("{host} - {project_name}"),
                    None => project_name,
                };
                row.h(theme.ui(HIT_ROW_HEIGHT)).child(
                    pill()
                        .flex_col()
                        .items_start()
                        .justify_center()
                        .gap(px(2.))
                        .when_some(theme.project(project), |pill, color| {
                            pill.relative().pl(theme.ui(18.)).child(side_line(color, &theme))
                        })
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().flex_1().min_w_0().truncate().child(title_text(session, marks)))
                                .when(*open, |line| {
                                    line.child(
                                        div().flex_none().text_xs().text_color(rgb(theme.tokens.accent)).child("open"),
                                    )
                                })
                                .when(session.container.is_some(), |line| {
                                    line.child(container_icon(theme.tokens.text, &theme))
                                })
                                .when(!session.host.is_local(), |line| line.child(ssh_icon(theme.tokens.text, &theme)))
                                .children(
                                    dir_badge_for(
                                        cx.global::<SettingsStore>().settings(),
                                        session.claude_dir.as_deref(),
                                        &session.host,
                                    )
                                    .map(|badge| dir_badge(&badge, &theme)),
                                )
                                .child(
                                    div().flex_none().text_xs().text_color(rgb(theme.tokens.muted)).child(project_name),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(rgb(theme.tokens.muted))
                                        .child(if session.cwd_exists { age.clone() } else { "Folder missing".into() }),
                                ),
                        )
                        .child(
                            div()
                                .w_full()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(rgb(theme.tokens.muted))
                                .child(snippet_text(snippet, &theme)),
                        ),
                )
            }
            RecentItem::Note { text } => {
                return row.child(
                    div()
                        .size_full()
                        .px_3()
                        .flex()
                        .items_center()
                        .text_xs()
                        .text_color(rgb(theme.tokens.muted))
                        .child(text.clone()),
                );
            }
        };
        row.on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
            this.selected = index;
            this.choose(index, event.modifiers().secondary(), cx);
            cx.notify();
        }))
    }

    /// Under the list: a muted line per host whose conversation search is unavailable, and one with a Connect button
    /// per SSH host that is not connected.
    fn render_host_lines(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let theme = theme(cx).clone();
        let line = || {
            div()
                .h(theme.ui(30.))
                .px_4()
                .flex()
                .items_center()
                .gap_2()
                .text_xs()
                .text_color(rgb(theme.tokens.muted))
                .border_t_1()
                .border_color(rgb(theme.tokens.line))
        };
        let searching = searches_content(self.query.text());
        let unavailable = self
            .unavailable
            .iter()
            .filter(|host| !host.is_local() && searching)
            .map(|host| line().child(format!("{}: search unavailable", host.name())).into_any_element());
        let offline = self.offline.iter().enumerate().map(|(index, name)| {
            let host = HostId::named(name);
            line()
                .child(div().size(theme.ui(8.)).rounded_full().border_1().border_color(rgb(theme.tokens.muted)))
                .child(div().flex_1().child(format!("{name} is not connected")))
                .child(
                    div()
                        .id(("connect", index))
                        .h(theme.ui(22.))
                        .px(theme.ui(10.))
                        .flex()
                        .items_center()
                        .rounded(px(RADIUS_SMALL))
                        .border_1()
                        .border_color(rgb(theme.tokens.line))
                        .bg(rgb(theme.tokens.raised))
                        .text_color(rgb(theme.tokens.text))
                        .hover(|style| style.border_color(rgb(theme.tokens.accent)))
                        .cursor_pointer()
                        .child("Connect")
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(RecentsEvent::Connect(host.clone())))),
                )
                .into_any_element()
        });
        unavailable.chain(offline).collect()
    }

    /// One value of a filter: a checkbox, the project's color, its name and how many sessions it would leave.
    fn render_value(&self, facet: Facet, value: &FilterValue, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let theme = theme(cx).clone();
        let key = value.key.clone();
        let color = value.project.as_deref().and_then(|project| theme.project(project));
        div()
            .id(SharedString::from(format!("filter-{}-{}", facet.title(), value.key)))
            .w_full()
            .h(theme.ui(28.))
            .px_2()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .rounded(px(RADIUS_SMALL))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme.tokens.raised)))
            .child(
                div()
                    .flex_none()
                    .size(theme.ui(14.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.))
                    .border_1()
                    .border_color(rgb(if value.selected { theme.tokens.accent } else { theme.tokens.muted }))
                    .when(value.selected, |box_| {
                        box_.bg(rgb(theme.tokens.accent)).text_color(rgb(theme.tokens.on_accent)).text_xs().child("✓")
                    }),
            )
            .children(color.map(|color| div().flex_none().size(theme.ui(8.)).rounded_full().bg(rgb(color))))
            .child(div().flex_1().min_w_0().truncate().child(value.label.clone()))
            .child(div().flex_none().text_xs().text_color(rgb(theme.tokens.muted)).child(value.count.to_string()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.prefs.toggle(facet, &key);
                this.refresh(cx);
                cx.notify();
            }))
    }

    /// The sidebar: the reset button and one block per filter section.
    fn render_filters(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let active = self.prefs.is_filtering();
        let reset = div()
            .id("reset-filters")
            .flex_none()
            .size(theme.ui(26.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(RADIUS_SMALL))
            .when(active, |button| {
                button.cursor_pointer().hover(|style| style.bg(rgb(theme.tokens.raised))).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.prefs.clear_all();
                        this.refresh(cx);
                        cx.notify();
                    },
                ))
            })
            .child(svg().path(RESET_ICON).size(theme.ui(15.)).text_color(if active {
                rgb(theme.tokens.accent)
            } else {
                rgba((theme.tokens.muted << 8) | 0x66)
            }));
        let sections: Vec<_> = self
            .sections
            .iter()
            .map(|section| {
                let facet = section.facet;
                let chosen = section.more.iter().filter(|value| value.selected).count();
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(theme.ui(26.))
                            .px_2()
                            .flex()
                            .items_center()
                            .text_xs()
                            .child(
                                div()
                                    .flex_1()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme.tokens.muted))
                                    .child(facet.title()),
                            )
                            .when(!self.prefs.selected(facet).is_empty(), |header| {
                                header.child(
                                    div()
                                        .id(SharedString::from(format!("clear-{}", facet.title())))
                                        .text_color(rgb(theme.tokens.accent))
                                        .cursor_pointer()
                                        .child("Clear")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.prefs.clear(facet);
                                            this.refresh(cx);
                                            cx.notify();
                                        })),
                                )
                            }),
                    )
                    .children(section.shown.iter().map(|value| self.render_value(facet, value, cx)))
                    .when(!section.more.is_empty(), |block| {
                        let hidden = section.more.len();
                        block.child(
                            div()
                                .id(SharedString::from(format!("more-{}", facet.title())))
                                .w_full()
                                .h(theme.ui(28.))
                                .pl(theme.ui(30.))
                                .pr_2()
                                .flex()
                                .items_center()
                                .gap_2()
                                .rounded(px(RADIUS_SMALL))
                                .text_color(rgb(theme.tokens.accent))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(theme.tokens.raised)))
                                .child(div().flex_1().child("More…"))
                                .child(
                                    div()
                                        .flex_none()
                                        .px(theme.ui(7.))
                                        .rounded_full()
                                        .border_1()
                                        .text_xs()
                                        .when(chosen > 0, |pill| {
                                            pill.bg(rgb(theme.tokens.accent))
                                                .border_color(rgb(theme.tokens.accent))
                                                .text_color(rgb(theme.tokens.on_accent))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(format!("{chosen} selected"))
                                        })
                                        .when(chosen == 0, |pill| {
                                            pill.border_color(rgb(theme.tokens.line))
                                                .text_color(rgb(theme.tokens.muted))
                                                .child(format!("+{hidden}"))
                                        }),
                                )
                                .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                                    this.more = Some(More { facet, field: TextField::default(), at: event.position() });
                                    cx.notify();
                                })),
                        )
                    })
            })
            .collect();
        div()
            .w(theme.ui(210.))
            .flex_none()
            .flex()
            .flex_col()
            .p_2()
            .gap_1()
            .border_r_1()
            .border_color(rgb(theme.tokens.line))
            .child(
                div()
                    .h(theme.ui(28.))
                    .pl_2()
                    .flex()
                    .items_center()
                    .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child("Filters"))
                    .child(reset),
            )
            .child(
                div()
                    .id("recents-filters")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .overflow_y_scroll()
                    .children(sections),
            )
            .into_any_element()
    }

    /// The "More…" popup: the section's other values, narrowed by its field.
    fn render_more(&self, more: &More, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let facet = more.facet;
        let plural = match facet {
            Facet::Project => "projects",
            Facet::Host => "hosts",
            Facet::Dir => "config dirs",
        };
        let typed = more.field.text().trim().to_lowercase();
        let values: Vec<&FilterValue> = self
            .sections
            .iter()
            .filter(|section| section.facet == facet)
            .flat_map(|section| &section.more)
            .filter(|value| value.label.to_lowercase().contains(&typed))
            .collect();
        let picked = self.prefs.selected(facet).len();
        let rows: Vec<_> = values.iter().map(|value| self.render_value(facet, value, cx)).collect();
        let card = div()
            .id("recents-more")
            .occlude()
            .w(theme.ui(260.))
            .max_h(theme.ui(340.))
            .flex()
            .flex_col()
            .bg(rgb(theme.tokens.raised))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .rounded(px(RADIUS))
            .shadow_lg()
            .text_sm()
            .text_color(rgb(theme.tokens.text))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.more = None;
                cx.notify();
            }))
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme.tokens.muted))
                    .child(format!("More {plural}")),
            )
            .child(
                div()
                    .mx_2()
                    .mb_1()
                    .h(theme.ui(30.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded(px(RADIUS_SMALL))
                    .bg(rgb(theme.tokens.surface))
                    .border_1()
                    .border_color(rgb(theme.tokens.accent))
                    .child(field::render(
                        &more.field,
                        format!("Filter {plural}"),
                        true,
                        &theme,
                        field::on_press(cx, |this, offset, clicks, _, cx| {
                            if let Some(more) = this.more.as_mut() {
                                more.field.press(offset, clicks);
                            }
                            cx.notify();
                        }),
                    )),
            )
            .child(
                div()
                    .id("recents-more-values")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .px_1()
                    .overflow_y_scroll()
                    .children(rows)
                    .when(values.is_empty(), |list| {
                        list.child(
                            div()
                                .p_3()
                                .text_xs()
                                .text_color(rgb(theme.tokens.muted))
                                .child(format!("No {plural} named \u{201c}{}\u{201d}.", more.field.text())),
                        )
                    }),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .border_t_1()
                    .border_color(rgb(theme.tokens.line))
                    .text_xs()
                    .text_color(rgb(theme.tokens.muted))
                    .child(div().flex_1().child(if picked == 0 {
                        "None selected".to_string()
                    } else {
                        format!("{picked} selected")
                    }))
                    .child(
                        div()
                            .id("recents-more-done")
                            .px(theme.ui(12.))
                            .py(px(3.))
                            .rounded(px(RADIUS_SMALL))
                            .bg(rgb(theme.tokens.accent))
                            .text_color(rgb(theme.tokens.on_accent))
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .child("Done")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.more = None;
                                cx.notify();
                            })),
                    ),
            );
        deferred(
            anchored()
                .position(Point { x: more.at.x + theme.ui(14.), y: more.at.y - theme.ui(16.) })
                .snap_to_window()
                .child(card),
        )
        .with_priority(2)
        .into_any_element()
    }

    /// What the list area shows when no session is listed: the reason and, with filters on, a way out.
    fn render_empty(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let notes: Vec<_> = self
            .items
            .iter()
            .filter_map(|item| match item {
                RecentItem::Note { text } => Some(div().child(text.clone())),
                _ => None,
            })
            .collect();
        div()
            .h(theme.ui(LIST_HEIGHT))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .text_color(rgb(theme.tokens.muted))
            .children(self.empty.map(|(title, hint)| {
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(div().text_color(rgb(theme.tokens.text)).font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(div().text_xs().child(hint))
            }))
            .children(notes)
            .when(self.prefs.is_filtering(), |empty| {
                empty.child(
                    div()
                        .id("empty-reset")
                        .mt_1()
                        .px(theme.ui(12.))
                        .py(px(4.))
                        .rounded(px(RADIUS_SMALL))
                        .border_1()
                        .border_color(rgb(theme.tokens.line))
                        .text_color(rgb(theme.tokens.accent))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(theme.tokens.raised)))
                        .child("Reset filters")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.prefs.clear_all();
                            this.refresh(cx);
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }

    /// The sort bar: a segmented control over the sorts and the reverse button.
    fn render_sort(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let sort = self.prefs.sort;
        let reverse = self.prefs.reverse;
        let segments: Vec<_> = Sort::ALL
            .into_iter()
            .map(|option| {
                let on = option == sort;
                div()
                    .id(SharedString::from(format!("sort-{}", option.label())))
                    .px(theme.ui(11.))
                    .h(theme.ui(26.))
                    .flex()
                    .items_center()
                    .rounded(px(RADIUS_SMALL))
                    .cursor_pointer()
                    .when(on, |segment| {
                        segment
                            .bg(rgb(theme.tokens.raised))
                            .text_color(rgb(theme.tokens.accent))
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(!on, |segment| {
                        segment
                            .text_color(rgb(theme.tokens.muted))
                            .hover(|style| style.text_color(rgb(theme.tokens.text)))
                    })
                    .child(option.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.prefs.set_sort(option);
                        this.refresh(cx);
                        cx.notify();
                    }))
            })
            .collect();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .child(div().text_xs().text_color(rgb(theme.tokens.muted)).child("Sort"))
            .child(
                div()
                    .flex()
                    .p(px(2.))
                    .gap(px(2.))
                    .rounded(px(RADIUS))
                    .border_1()
                    .border_color(rgb(theme.tokens.line))
                    .children(segments),
            )
            .child(
                div()
                    .id("sort-reverse")
                    .size(theme.ui(30.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(RADIUS))
                    .border_1()
                    .border_color(rgb(theme.tokens.line))
                    .cursor_pointer()
                    .hover(|style| style.border_color(rgb(theme.tokens.accent)))
                    .child(if reverse { "↑" } else { "↓" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.prefs.reverse = !this.prefs.reverse;
                        this.refresh(cx);
                        cx.notify();
                    })),
            )
            .into_any_element()
    }
}

impl Render for RecentsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        let (matching, total) = self.counts;
        let count = if self.query.text().trim().is_empty() && !self.prefs.is_filtering() {
            format!("{total} closed sessions")
        } else {
            format!("{matching} of {total}")
        };
        let hint = |key: &'static str, what: &'static str| {
            div()
                .flex()
                .gap_1()
                .child(div().text_color(rgb(theme.tokens.text)).font_weight(FontWeight::SEMIBOLD).child(key))
                .child(what)
        };
        let any_session = self.items.iter().any(|item| matches!(item, RecentItem::Session { .. }));
        let sort_bar = self.render_sort(cx);
        let filters = self.render_filters(cx);
        let list_area = if any_session {
            let view = cx.entity().downgrade();
            list(self.list_state.clone(), move |index, _window, cx| {
                view.update(cx, |this, cx| this.render_item(index, cx).into_any_element())
                    .unwrap_or_else(|_| div().into_any_element())
            })
            .h(theme.ui(LIST_HEIGHT))
            .into_any_element()
        } else {
            self.render_empty(cx)
        };
        let popup = self.more.as_ref().map(|more| self.render_more(more, cx));
        let card = div()
            .key_context("Recents")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                // The "More…" popup floats over the card: a press on it is a press outside the card too.
                if this.more.is_none() {
                    cx.emit(RecentsEvent::Dismiss);
                }
            }))
            .w(theme.ui(900.))
            .max_w_full()
            .flex()
            .flex_col()
            .bg(rgb(theme.tokens.surface))
            .border_1()
            .border_color(rgb(theme.tokens.line))
            .rounded(px(RADIUS_LARGE))
            .overflow_hidden()
            .shadow_lg()
            .text_sm()
            .text_color(rgb(theme.tokens.text))
            .child(
                div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child("Recent sessions"))
                            .child(div().text_xs().text_color(rgb(theme.tokens.muted)).child(count)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(theme.ui(240.))
                                    .h(theme.ui(38.))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .rounded(px(RADIUS))
                                    .bg(rgb(theme.tokens.raised))
                                    .border_1()
                                    .border_color(rgb(theme.tokens.accent))
                                    .child(field::render(
                                        &self.query,
                                        "Search sessions and conversations",
                                        self.more.is_none(),
                                        &theme,
                                        field::on_press(cx, |this, offset, clicks, _, cx| {
                                            this.query.press(offset, clicks);
                                            cx.notify();
                                        }),
                                    )),
                            )
                            .child(sort_bar),
                    ),
            )
            .child(div().flex().border_t_1().border_color(rgb(theme.tokens.line)).child(filters).child(
                div().flex_1().min_w_0().flex().flex_col().child(list_area).children(self.render_host_lines(cx)),
            ))
            .child(
                div()
                    .h(theme.ui(34.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_4()
                    .text_xs()
                    .text_color(rgb(theme.tokens.muted))
                    .border_t_1()
                    .border_color(rgb(theme.tokens.line))
                    .child(hint("Enter", "resume"))
                    .child(hint(SPLIT_KEY, "resume as split"))
                    .child(hint("↑↓", "move"))
                    .child(hint("Tab", "next sort"))
                    .child(div().flex_1())
                    .child(hint("Esc", "close")),
            );
        div()
            .max_w_full()
            .child(fade_in(card, "fade-recents", FadeKind::Card, cx.global::<SettingsStore>().settings().animate_ui))
            .children(popup)
    }
}

/// "You: …" / "Claude: …" with the matched ranges in bold.
/// A session's title with the typed word starts in bold.
fn title_text(session: &SessionInfo, marks: &[Range<usize>]) -> StyledText {
    let bold = HighlightStyle { font_weight: Some(FontWeight::BOLD), ..HighlightStyle::default() };
    StyledText::new(session_title(session).to_string()).with_highlights(marks.iter().map(|r| (r.clone(), bold)))
}

fn snippet_text(snippet: &Snippet, theme: &Theme) -> StyledText {
    let prefix = match snippet.speaker {
        Speaker::You => "You: ",
        Speaker::Claude => "Claude: ",
    };
    let bold = HighlightStyle {
        font_weight: Some(FontWeight::BOLD),
        color: Some(rgb(theme.tokens.text).into()),
        ..HighlightStyle::default()
    };
    StyledText::new(format!("{prefix}{}", snippet.text))
        .with_highlights(snippet.highlights.iter().map(|r| (r.start + prefix.len()..r.end + prefix.len(), bold)))
}

#[cfg(target_os = "macos")]
const SPLIT_KEY: &str = "⌘Enter";
#[cfg(not(target_os = "macos"))]
const SPLIT_KEY: &str = "Ctrl+Enter";

/// How many items at the start and at the end of `old` and `new` are the same (never overlapping).
fn unchanged_ends<T: PartialEq>(old: &[T], new: &[T]) -> (usize, usize) {
    let start = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - start;
    let end = old.iter().rev().zip(new.iter().rev()).take(room).take_while(|(a, b)| a == b).count();
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_ends_are_found_on_both_sides() {
        assert_eq!(unchanged_ends(&[1, 2, 3, 4], &[1, 2, 9, 4]), (2, 1));
        assert_eq!(unchanged_ends(&[1, 2], &[1, 2, 3]), (2, 0), "rows added at the end");
        assert_eq!(unchanged_ends(&[1, 2, 3], &[1, 3]), (1, 1), "a row removed in the middle");
        assert_eq!(unchanged_ends(&[1, 1], &[1, 1, 1]), (2, 0), "prefix and suffix never overlap");
        assert_eq!(unchanged_ends::<u8>(&[], &[]), (0, 0));
    }
}
