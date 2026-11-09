# Gantry Projects Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the user create named projects in the sidebar, put any pane or session into one by dragging or launching, and have every place that shows a project (sidebar, Recents, title bar, colors, menus) treat it like a folder project.

**Architecture:** A Gantry project is identified by its name and keyed `gantry://<lowercased name>` where folder projects use a path key. Membership lives in the layout (`Leaf.project`, `Workspace.session_projects`, `Workspace.gantry_projects`), all optional fields. `project_of` learns about membership (pure `app-core`), then `ui-gpui` draws, drags and launches against it. GUI and layout only; no protocol change.

**Tech Stack:** Rust workspace (`app-core` pure logic, `ui-gpui` GPUI views), serde layout file, existing `dialogs`, `menus`, `session_tree`, `recents`, `sidebar_order`.

**Spec:** `docs/superpowers/specs/2026-11-09-gantry-projects-design.md` (calls marked **[call]** there are decisions of the author).

## Global Constraints

- Every compiling cargo command goes through `scripts/cargo` (never `cargo build/test/clippy/check/run` directly, never two at once). `cargo fmt --all` may run directly.
- Never add `gpui` outside `crates/ui-gpui`. Colors, radii and fonts come from `ui-gpui::theme`; never hard-coded.
- Public items need `///` docs; CI runs `cargo fmt --check` (width 120) and clippy with `-D warnings`.
- Layout changes are additive optional fields (`#[serde(default, skip_serializing_if = ...)]`); an old layout loads unchanged; no layout version bump.
- No protocol or daemon change; the CHANGELOG entry therefore has no "Needs a daemon restart".
- Project key format: `gantry://<name lowercased>`; names unique ignoring case; an empty (after trim) name is refused.
- Commits: no attribution lines of any kind (user rule). Do not push; the user pushes.
- Update `CLAUDE.md` and `CHANGELOG.md` (`## [Unreleased]` > Added, starting with a bold title) in the last task.

## Review Focus

1. A name with surrounding spaces or a different case ("  billing ", "BILLING") must hit the existing project, not make a second one; renaming a project to its own name in another case must work.
2. A layout saved by the previous version (no new fields) must load and re-save without a Gantry project appearing; a layout with a `Leaf.project` naming a project missing from `gantry_projects` must recreate it, not drop the pane.
3. `/clear` in a Claude pane inside "Billing": the new session id must still show under "Billing" and the old one must stay labeled in Recents.
4. Deleting a project whose pane sits in a split tab with panes from other projects must close only that project's panes and leave the rest of the tab.
5. Renaming while a closed session in Recents carries the old name: it must show the new name; a remote pane in a project keeps its host when split.

---

### Task 1: Project model and layout fields

**Files:**
- Create: `crates/app-core/src/gantry_project.rs`
- Modify: `crates/app-core/src/lib.rs` (add `pub mod gantry_project;`), `crates/app-core/src/workspace.rs` (`Leaf`, `Workspace`)
- Test: in-module tests in the same two files

**Interfaces:**
- Produces (`gantry_project`):
  - `pub const KEY_PREFIX: &str = "gantry://";`
  - `pub fn normalize(name: &str) -> Option<String>` trims; `None` when empty.
  - `pub fn key(name: &str) -> PathBuf` (`gantry://` + lowercased trimmed name).
  - `pub fn name_of_key(key: &Path) -> Option<String>` (the lowercased name; `None` for any other key).
  - `pub fn is_key(key: &Path) -> bool`.
  - `pub fn same(a: &str, b: &str) -> bool` (ignoring case and surrounding spaces).
- Produces (`Leaf`): `pub project: Option<String>` (`serde(default, skip_serializing_if = "Option::is_none")`).
- Produces (`Workspace`): `pub gantry_projects: Vec<String>` and `pub session_projects: BTreeMap<String, String>`, both `serde(default, skip_serializing_if = empty)`; methods:
  - `pub fn create_project(&mut self, name: &str) -> Result<String, ProjectError>` (adds on top; `ProjectError::{Empty, Exists}`).
  - `pub fn rename_project(&mut self, from: &str, to: &str) -> Result<String, ProjectError>` (updates `gantry_projects`, every `Leaf.project`, every `session_projects` value; case-only rename of itself allowed).
  - `pub fn set_leaf_project(&mut self, leaf: NodeId, project: Option<&str>) -> bool` (creates the project if missing; also writes `session_projects[leaf.session]` when the leaf has a session).
  - `pub fn set_tab_project(&mut self, index: usize, project: Option<&str>) -> bool` (every leaf of the tab).
  - `pub fn set_session_project(&mut self, session: &str, project: Option<&str>)` (external or closed session; creates the project if missing).
  - `pub fn project_exists(&self, name: &str) -> Option<&str>` (canonical display name).
- Consumes: existing `Workspace`, `Leaf`, `NodeId`.

- [ ] **Step 1: Write the failing tests** in `gantry_project.rs` and `workspace.rs`: `normalize_trims_and_rejects_empty`, `key_lowercases_and_round_trips_name`, `create_project_refuses_duplicates_ignoring_case`, `create_puts_new_project_first`, `rename_updates_leaves_and_session_map`, `rename_to_own_name_in_other_case_is_allowed`, `rename_onto_another_project_is_refused`, `set_leaf_project_creates_missing_project_and_records_session`, `set_tab_project_labels_every_leaf`, `layout_without_new_fields_loads_and_resaves_unchanged` (parse an old JSON fixture, serialize, assert no new keys appear), `leaf_project_missing_from_list_is_recreated_on_load` (Review Focus 2).
- [ ] **Step 2: Run** `scripts/cargo test -p app-core gantry_project workspace` - expect FAIL (items undefined).
- [ ] **Step 3: Implement** the module and the three `Workspace` fields/methods above; `Workspace::saveable` keeps the new fields; on load (`restore` or a `Workspace::normalize_projects`) add any `Leaf.project`/`session_projects` name absent from `gantry_projects` **only for leaves** (Review Focus 2), not for stale session-map names (those are deleted projects).
- [ ] **Step 4: Run** the same command - expect PASS; then `scripts/cargo clippy -p app-core --all-targets -- -D warnings`.
- [ ] **Step 5: Commit** `feat(projects): Gantry project model and layout fields`.

### Task 2: `project_of` knows membership (session tree and Recents)

**Files:**
- Modify: `crates/app-core/src/session_tree.rs` (`PaneEntry`, `ActiveInput`, `ProjectRoots`, groups at ~line 749, `ProjectOrder::to_save`), `crates/app-core/src/recents.rs` (`RecentsInput`, `project_key_of`, `filter_sections`, `listed`)
- Test: in-module tests in both files

**Interfaces:**
- Produces: `PaneEntry.project: Option<String>`; `ActiveInput.session_projects: &BTreeMap<String, String>` and `ActiveInput.gantry_projects: &[String]` (so empty projects get a heading); `ProjectRoots::of_member(&self, host, cwd, gantry: Option<&str>) -> PathBuf` (returns `gantry_project::key(name)` when `Some`, else `of`); `RecentsInput.project_of` becomes `&dyn Fn(&SessionInfo) -> PathBuf` (callers pass a closure that checks `session_projects[session.id]` then `ProjectRoots::of`).
- Consumes: Task 1 `gantry_project::key`.

- [ ] **Step 1: Write failing tests**: `pane_with_project_is_listed_under_its_gantry_heading_only`, `empty_gantry_project_gets_a_heading`, `split_tab_sits_under_first_panes_project`, `external_session_follows_session_projects`, `to_save_keeps_gantry_keys_even_when_empty`, and in `recents.rs`: `closed_session_with_gantry_name_is_keyed_gantry`, `deleted_project_name_still_filters` (name only in `session_projects`), `project_filter_label_is_the_name_not_a_path`.
- [ ] **Step 2: Run** `scripts/cargo test -p app-core session_tree recents` - expect FAIL / compile errors.
- [ ] **Step 3: Implement** the interface changes; update every call site in `app-core` (tests included) and the three `ui-gpui` closures (`sidebar_view.rs:693`, `recents_view.rs:176`, `panes.rs::leaf_project_color`) minimally to compile (membership passed from `Workspace`); `Facet::Project` labels for `gantry://` keys use the display name (look it up from `gantry_projects`, else title-case the stored name from `session_projects`).
- [ ] **Step 4: Run** `scripts/cargo test --workspace`; expect PASS.
- [ ] **Step 5: Commit** `feat(projects): project_of takes membership, Recents keys Gantry projects`.

### Task 3: Sidebar rendering and project order

**Files:**
- Modify: `crates/ui-gpui/src/sidebar_view.rs` (heading ~1276-1316, compact tiles, `ProjectOrder` use), `crates/ui-gpui/src/assets.rs` and `crates/ui-gpui/assets/icons/layers.svg` (new), `crates/ui-gpui/src/hover_card.rs` only if the heading card shows a folder
- Test: `crates/ui-gpui` unit tests where pure (icon path constants); the rest verified in Task 9

**Interfaces:**
- Produces: `assets::icons::LAYERS` (embedded `layers.svg`, stroke icon like `split-right.svg`); heading draws the layers icon before the name for `gantry_project::is_key`; compact mode draws it in place of the square dot; an empty Gantry project shows the muted line "Empty. Drag a session here, or New Claude here."; the dot keeps the status/aura rule of folder projects; `SidebarEvent::DeleteProject(String)` is emitted by a hover × (in the dot's place, only for Gantry headings with the existing `close_slot` pattern).
- Consumes: Task 2 headings for empty projects.

- [ ] **Step 1: Add `layers.svg`** matching the mock-up (three stacked layers, 16 px viewBox, stroke `currentColor`) and register it with the other icons; add a test next to the existing asset tests that every `icons/*.svg` is embedded.
- [ ] **Step 2: Draw** the Gantry heading variants (expanded, compact, empty line, hover ×) following the existing project heading code; no hard-coded colors (use `Theme`).
- [ ] **Step 3: Run** `scripts/cargo test -p ui-gpui` and clippy - expect PASS.
- [ ] **Step 4: Commit** `feat(projects): Gantry project headings in the sidebar`.

### Task 4: Create and rename (button, inline field, uniqueness)

**Files:**
- Modify: `crates/ui-gpui/src/sidebar_view.rs` (button beside the filter, compact tile, inline field), `crates/ui-gpui/src/workspace_view/sidebar_menus.rs` (event handling), `crates/app-core/src/menus.rs` (Task 6 adds the menu items; here only `MenuAction::RenameProject`)
- Test: pure checks in `app-core::gantry_project` (`validate_name(existing: &[String], name: &str, own: Option<&str>) -> Result<String, ProjectError>`)

**Interfaces:**
- Produces: `SidebarEvent::{NewProject(String), RenameProject { from: String, to: String }}`; the field shows "already exists" under it and stays open on `ProjectError::Exists`, does nothing on `Empty`; the button is the layers icon with an accent plus badge at the bottom-right (theme tokens), tooltip "New project", shown as the first tile of the compact column.
- Consumes: Task 1 `create_project`/`rename_project`.

- [ ] **Step 1: Write failing tests** for `validate_name`: `rejects_empty`, `rejects_existing_ignoring_case_and_spaces`, `allows_own_name_when_renaming`.
- [ ] **Step 2: Run** `scripts/cargo test -p app-core gantry_project` - expect FAIL.
- [ ] **Step 3: Implement** `validate_name`, the button, the inline field (reuse `field` and the shell-row rename pattern: Enter saves, Esc or click elsewhere cancels) and route both events through `WorkspaceView` to `Workspace::create_project`/`rename_project`; on rename also move the `projectColors.folders` key and the `SidebarOrder.projects` entry (old key -> new key).
- [ ] **Step 4: Run** `scripts/cargo test --workspace` - expect PASS; commit `feat(projects): create and rename Gantry projects`.

### Task 5: Drag into a project

**Files:**
- Modify: `crates/app-core/src/session_tree.rs` (`DragSource`, `DropAt`, `reorder` ~280-350), `crates/ui-gpui/src/sidebar_view.rs` (drop targets on headings ~1084, 1783), `crates/ui-gpui/src/workspace_view/sidebar_menus.rs`
- Test: `reorder`/`drop` tests in `session_tree.rs`

**Interfaces:**
- Produces: `SidebarEvent::AssignToProject { source: DragSource, project: Option<String> }` (`Some(name)` = into a Gantry project, `None` = back to its folder project); `DragSource::{Pane(NodeId), Tab(NodeId), Session(String)}` (the last for external rows). Rules: a pane of a split tab = `pull_out` then `set_leaf_project`; a tab heading = `set_tab_project`; a single pane/external session = `set_leaf_project` / `set_session_project`; a folder heading as target = membership removed. Dropping on its own project does nothing.
- Consumes: Task 1 setters, existing `Workspace::pull_out`.

- [ ] **Step 1: Write failing tests**: `drop_pane_of_split_tab_on_gantry_heading_pulls_it_out_and_labels_it`, `drop_tab_heading_labels_every_pane`, `drop_on_folder_heading_removes_membership`, `drop_on_own_project_changes_nothing`, `drop_external_session_records_session_map`.
- [ ] **Step 2: Run** `scripts/cargo test -p app-core session_tree` - expect FAIL.
- [ ] **Step 3: Implement** the new plan function (pure, in `session_tree.rs`: `assign_plan(source, project) -> Vec<WorkspaceOp>` or direct `Workspace` calls) and make heading `on_drop` handlers (for `DraggedPane`, `DraggedTab`, and the external-row drag) emit the event; the visible tab stays visible.
- [ ] **Step 4: Run** `scripts/cargo test --workspace` - expect PASS; commit `feat(projects): drag panes, tabs and sessions into a project`.

### Task 6: Menu, launching and splits

**Files:**
- Modify: `crates/app-core/src/menus.rs` (`project_menu`, `MenuAction::{NewHere, RenameProject, DeleteProject}`), `crates/app-core/src/launcher_flow.rs` and `crates/app-core/src/launcher.rs` (preset project, last folder), `crates/ui-gpui/src/launcher_view.rs` (chip), `crates/ui-gpui/src/workspace_view/{overlays.rs,sidebar_menus.rs,panes.rs}`, `crates/app-core/src/workspace.rs` (`split_on` inherit)
- Test: in-module tests

**Interfaces:**
- Produces: `project_menu` for a `gantry://` key returns exactly `New Claude here`, `New terminal here`, `Color`, `Recent sessions here`, `Rename...`, `Delete project` (no config-dir submenu, no folder entries); `Flow.project: Option<String>` and `launcher::last_folder_of(project: &str, sessions: &[&SessionInfo], session_projects: &BTreeMap<String,String>, leaves: ...) -> Option<(HostId, PathBuf)>` used to highlight the row; the launcher shows the project as a removable chip; `New terminal here` on a Gantry project opens a shell in the home directory on `HostId::local()` with `Leaf.project` set, without the launcher; `Workspace::split_on` copies `Leaf.project` from the focused leaf, host unchanged.
- Consumes: Task 1, Task 4 events.

- [ ] **Step 1: Write failing tests**: `gantry_project_menu_lists_only_project_entries`, `last_folder_of_picks_the_most_recent_session_folder`, `split_inherits_project_and_host`, `new_terminal_here_is_local_home` (pure part: the launch built for a Gantry key has `cwd == home`, `host == local`).
- [ ] **Step 2: Run** `scripts/cargo test -p app-core menus launcher workspace` - expect FAIL.
- [ ] **Step 3: Implement**; `New Claude here` opens the launcher with `Flow.project` set and the highlight on `last_folder_of`; launching from it passes the project to `new_tab_on`/the leaf (`set_leaf_project`); the chip's × clears `Flow.project`. Use `std::env`/existing `home` for `~` (Linux and macOS).
- [ ] **Step 4: Run** `scripts/cargo test --workspace` - expect PASS; commit `feat(projects): project menu, launch into a project, splits inherit`.

### Task 7: Delete, resume and recreate

**Files:**
- Modify: `crates/app-core/src/workspace.rs` (`delete_plan`, `remove_project`), `crates/app-core/src/dialogs.rs` (`delete_project` builder, `DialogAction::DeleteProject`), `crates/ui-gpui/src/workspace_view/{overlays.rs,resume.rs,panes.rs}`
- Test: in-module tests

**Interfaces:**
- Produces: `Workspace::delete_plan(&self, name: &str) -> DeletePlan` where `DeletePlan { running: Vec<NodeId>, dead: Vec<NodeId> }` is computed without mutation (`project_leaves`), and `Workspace::remove_project(&mut self, name: &str)` removes it from `gantry_projects` (and drops its saved order and color); `dialogs::delete_project(name: &str, running: usize) -> DialogSpec` (Cancel / "Stop and delete"); with `running == 0` the UI deletes without a dialog. Closing the project's panes uses the existing close path per leaf (only those leaves; a split tab keeps its other panes). Resume: `Workspace::resume_project_for(&mut self, session: &str)` recreates the project from `session_projects[session]` when it no longer exists (fresh: top of list, no color) and the leaf gets `Leaf.project`; called by Recents open/resume and the pane bar Resume.
- Consumes: Task 1.

- [ ] **Step 1: Write failing tests**: `delete_plan_lists_running_and_dead_leaves_of_the_project_only` (Review Focus 4), `remove_keeps_closed_session_names`, `resume_recreates_a_deleted_project_fresh`, `resume_joins_an_existing_project_of_that_name_ignoring_case`, `clear_keeps_the_leaf_in_its_project_and_labels_both_sessions` (Review Focus 3), `delete_dialog_copy_mentions_the_count`.
- [ ] **Step 2: Run** `scripts/cargo test -p app-core workspace dialogs` - expect FAIL.
- [ ] **Step 3: Implement**, hook `SidebarEvent::DeleteProject` and the menu action to: plan, dialog if `running > 0`, then close leaves and `remove_project`.
- [ ] **Step 4: Run** `scripts/cargo test --workspace` - expect PASS; commit `feat(projects): delete projects, resume recreates them`.

### Task 8: Recents surface and the other places

**Files:**
- Modify: `crates/ui-gpui/src/recents_view.rs` (layers icon in project pills, rows, filter values), `crates/ui-gpui/src/workspace_view/title_bar.rs`, `crates/ui-gpui/src/git_chip.rs` / hover cards only where a project folder is shown, `crates/ui-gpui/src/settings_view/appearance.rs` (project colors list shows Gantry keys by name), `crates/app-core/src/recents.rs` (`RecentsFor` with a Gantry key)
- Test: `recents.rs` tests

**Interfaces:**
- Produces: `MenuAction::RecentsFor(PathBuf)` with a `gantry://` key preselects exactly that Project filter value; project pill/label shows the name with the layers icon; the title-bar chip reads `<Name> > <session>`; the hover card shows no project Folder line for a Gantry project; the color picker saves under the `gantry://` key.
- Consumes: Tasks 2, 6.

- [ ] **Step 1: Write failing tests**: `recents_for_gantry_key_selects_that_project_value`, `renamed_project_relabels_closed_sessions` (Review Focus 5), `deleted_project_listed_like_a_folder_project_without_open_sessions`.
- [ ] **Step 2: Run** `scripts/cargo test -p app-core recents` - expect FAIL.
- [ ] **Step 3: Implement** the view changes listed; no hard-coded colors.
- [ ] **Step 4: Run** `scripts/cargo test --workspace` and clippy - expect PASS; commit `feat(projects): Gantry projects in Recents, title bar and colors`.

### Task 9: Docs, changelog and live check

**Files:**
- Modify: `CLAUDE.md` (project model, layout keys `gantryProjects`/`sessionProjects`/`Leaf.project`, drag rules, menu), `CHANGELOG.md` (`## [Unreleased]` > Added: **Gantry projects** with one sub-bullet each for create/rename/delete, drag in, launch into, Recents)
- Test: full checks and the headless live check

- [ ] **Step 1: Update** `CLAUDE.md` and `CHANGELOG.md` as above (no "Needs a daemon restart").
- [ ] **Step 2: Run** `cargo fmt --all`, `scripts/cargo test --workspace`, `scripts/cargo clippy --workspace --all-targets -- -D warnings` - all pass (send output to a file; read the tail).
- [ ] **Step 3: Live check** per `CLAUDE.md > Live check` (isolated headless GNOME, throwaway HOME and profile, fake `claude`): create "Billing" (also a duplicate name), drag a session and a split-tab heading in, a pane out of a split tab, New Claude here (launcher preset + chip), New terminal here (cwd `~`), rename, delete with a running session (dialog), resume a closed session from Recents (project recreated), compact sidebar, restart Gantry and confirm membership. Capture screenshots; fix findings.
- [ ] **Step 4: Commit** `docs(projects): CLAUDE.md and changelog`. Do not push or open the PR; report the branch target (`release/0.14.0`) to the user.
