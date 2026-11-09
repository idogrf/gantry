# Gantry projects - design

Status: draft for approval, 2026-11-09. Target branch: `release/0.14.0` (worktree `.claude/worktrees/gantry-projects`, branch `feat/gantry-projects`, started from the merge of #55).
Mock-ups (private): https://claude.ai/artifact/J4LxYG28TpSiJnqk225x5V

Marked **[call]** = decided by the author, not by the user; check these first.

## Why

Projects in Gantry are derived from the folder a session ran in. People who run everything from one folder get a single project and lose the grouped view. A *Gantry project* is a project the user makes by hand and puts sessions into, independent of any folder.

## Goals

- The user can create, rename, color and delete named projects, and put any pane or session into one by dragging or by launching into it.
- A Gantry project behaves like any other project wherever projects appear: sidebar, compact sidebar, title bar, pane headers, hover cards, project colors, Recents (sort/filter pop-up from #55).
- Gantry remembers membership, including for closed sessions.

## Non-goals

- Folder projects change in no way. A Gantry project has no folder, no default folder and no git state.
- No protocol or daemon change (GUI and layout only; no "Needs a daemon restart").
- No favorites, VS Code, config-dir or "Copy folder path" entries for a Gantry project.
- No auto-assignment, rules or templates.

## Model

- A Gantry project is identified by its **name**. Names are unique ignoring case; the display keeps the case typed. A new project with a taken name is refused ("already exists", the field stays open); an empty name does nothing.
- Project key (what `project_of` returns today as a path): `gantry://<name>` **[call]**, like `ssh://<host>/<folder>`. The name part is the lowercased name; the display name is looked up from the saved list. Colors, saved order and menus use this key.
- Membership is saved in two places (both additive in `layout.json`, no layout version bump, old files load unchanged):
  - `Leaf.project: Option<String>` - the display name; applies to every pane, Claude or shell, and follows the leaf across `/clear` and resume **[call]**.
  - `Workspace.session_projects: map session id -> name` - written whenever a leaf in a project reports its session, and when a session (including an external one) is dragged into a project. It is what labels closed sessions in Recents and what a resume reads. Entries are never pruned **[call]** (tiny).
- `Workspace.gantry_projects: Vec<String>` - the projects that exist (the user's created list). A project "exists" in the sidebar even with no sessions; names that appear only in `session_projects` (deleted ones) exist only as Recents labels.
- `project_of` gains the session: today `(host, cwd) -> key`; it becomes `(host, cwd, leaf project or session id) -> key`. The Gantry membership wins over the folder. Callers: `session_tree` (rows, split-tab groups), `recents` (`RecentsInput.project_of`, `project_key_of`, filter sections and labels), `sidebar_view`, `recents_view`, `workspace_view::panes::leaf_project_color`.
- A project can mix hosts and folders freely (it is not host-bound).

## Behavior

### Creating

A button beside the "Filter active sessions" box (icon: layers, with a small accent plus badge at the bottom-right) adds an empty heading at the top of the list with an inline name field (same pattern as Rename on a shell row): Enter creates, Esc or clicking elsewhere cancels. Also shown in the compact sidebar, as an icon tile at the top of the 56 px column (the filter is hidden there).

### Heading

- Marker: the layers icon before the name (plain, no plus), plus the usual project dot with its status/aura behavior (the dot shows the most urgent session status as for folder projects). Compact sidebar: the layers icon takes the place of a folder project's square dot between tiles.
- A Gantry project with no sessions stays in the list. A hover x (like a session row's x, in the dot's place) deletes it. Folder headings never get this x.
- New projects go on top, as new folder projects do; the user can drag a heading to reorder, saved via `ProjectOrder`. `to_save` keeps `gantry://` keys even when empty, like `ssh://` ones **[call]**.
- Empty state under the heading: a muted line "Empty. Drag a session here, or New Claude here." **[call]** wording.

### Putting things in a project

- Drag a session/pane row onto a project heading (or between its rows) to assign it. This works for Claude panes, shell panes and external sessions. The session leaves its folder heading and shows only under the Gantry project.
- Dragging one pane of a split tab (pane header or its row under the tab heading) pulls it out into a tab of its own in the target project (existing `pull_out` plus a project label).
- Dragging a split tab's own heading moves the whole tab: every pane gets the project.
- A split tab sits under its first pane's project (unchanged rule); a tab may mix projects.
- Dragging a row out to a folder project's heading removes the Gantry membership (it returns to its folder project) **[call]** - needs a drop target on folder headings; confirm in review.
- A split from a pane in a project inherits the project and, as today, the pane's host.
- "New Claude here" on a Gantry heading opens the launcher with the project preset, highlighting the folder that project's sessions most recently ran in. The launcher shows the project name as a removable chip **[call]**. Normal launches have no project.
- "New terminal here" on a Gantry heading opens a shell at once on this machine in the user's home directory (`~`; Linux and macOS), with the project set. No launcher, never a remote host.

### Heading menu

New Claude here; New terminal here; Color; Recent sessions here; Rename...; Delete project. Not offered: Claude with (config dir), Open folder, Open in VS Code, Copy folder path (a Gantry project has no folder). (Pin as favorite no longer exists in the code; ignore older docs.)

### Rename

Inline field. Updates `gantry_projects`, every `Leaf.project`, every `session_projects` entry with the old name (so closed sessions follow), the `projectColors` key and the saved order key. Same uniqueness rule; renaming to its own name with a different case is allowed.

### Delete

- No running sessions: deletes at once, no dialog **[call]**.
- Running sessions: a Gantry dialog (`app-core::dialogs`, new `delete_project` builder) says N sessions are running and will be stopped; OK stops them and closes their panes and removes the project. Cancel does nothing.
- Dead leaves (pane bar) in the project are closed too **[call]**.
- Closed sessions keep their name in `session_projects`. The project's color and saved position are dropped.
- Resuming a closed session (Recents, pane bar) whose name has no project recreates it fresh (name only, top of the list); if one with that name exists, the session joins it. Typing the same name later has the same effect: the name is the identity.

### Recents (PR #55, `app-core::recents`, `recents_view`)

- `RecentsInput.project_of` takes the session (see Model). A closed session with a Gantry name is listed under it whether or not the project still exists: a deleted project is treated exactly like a folder project with no open sessions.
- The Project filter section lists Gantry names (with the layers icon, labeled by name, not by `labels(paths)`); counts, ordering and the "at least two values" rule are unchanged. Row labels and the project pill/colour follow the key.
- "Recent sessions here" on a Gantry heading filters by that key; Open ("open" mark, Enter focuses) unchanged.

### Other surfaces

- Title-bar chip `project > session`, pane header and hover cards show the name; hover card gets no Folder line for the project, only the session's own folder.
- Sidebar filter and search: a row's project label is the Gantry name.
- Project colors: stored under `projectColors.folders` with the `gantry://` key; the auto color derives from the key. Settings > Appearance > project colors lists them like folders.
- Compact sidebar: heading = layers icon (no tile letter); split-tab and tile rules unchanged.

## Constraints

- GUI-only: `app-core` (key, model, `session_tree`, `recents`, `menus`, `dialogs`, `workspace`/`layout_store`) and `ui-gpui` (sidebar, workspace_view, recents_view, launcher_view). No `gpui` outside `ui-gpui`. Colors/radii from `Theme`; Public items need docs; fmt and clippy clean; `scripts/cargo` only.
- Layout additions are optional fields: an old file loads unchanged, an old Gantry ignores them.
- Update `CLAUDE.md` (project model, drag rules, layout keys) and `CHANGELOG.md` `## [Unreleased]` (Added: **Gantry projects**).

## Left to the builder

Exact drop-target visuals, the inline field details, the dialog wording, the launcher chip, and the test plan (pure functions first: key, rename, delete, resume-recreate, project_of with membership, reorder across projects).
