# Click to Update Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A "Download and install" button (release card and Settings > Window > Updates) that downloads the newest release for this platform, checks its SHA-256, installs it in place, and ends at the existing Restart and update.

**Architecture:** Pure decisions (asset pick, `SHA256SUMS`, can-this-Gantry-update-itself, run states and their copy) live in a new `app-core::self_update`. A GPUI-free engine in `ui-gpui::self_update` downloads, checks and installs on a background thread (Linux: the release's own `install.sh`; macOS: mount the dmg, `ditto`, two renames). `workspace_view/updates.rs` owns the one run, publishes it as a global for the Settings page, and on success sets `installed`, which turns the existing install notice on.

**Tech Stack:** Rust, `ureq` 3 (already in `ui-gpui`), `sha2` 0.10 (new in `ui-gpui`, already in `daemon`), system `tar`, `hdiutil`, `ditto`, `codesign`, `open`.

**Spec:** `docs/superpowers/specs/2026-11-15-gantry-click-to-update-design.md`

## Global Constraints

- Every compiling cargo command goes through `scripts/cargo` (from this worktree: `/home/idogr/Private/Repos/gantry/scripts/cargo` if the branch lacks it); never two at once, never in the background.
- Before each commit: `cargo fmt --all`; at the end: `scripts/cargo test --workspace`, `scripts/cargo clippy --workspace --all-targets -- -D warnings`, `cargo deny check` (a dependency is added).
- Public items need `///` docs; width 120; no `gpui` outside `ui-gpui`; colors/radii/fonts from `theme`.
- No protocol, daemon or `claude-index` change.
- Asset names: `gantry-<version>-x86_64-linux.tar.gz`, `gantry-<version>-aarch64-macos.dmg`, `SHA256SUMS` (lines `<hex>  <name>`). Allowed asset URL prefix: `https://github.com/idogrf/gantry/releases/download/`.
- Linux can update only when the running binary is `~/.local/bin/gantry` and that folder is writable. macOS only when the binary is `<dir>/Gantry.app/Contents/MacOS/gantry`, not under `/AppTranslocation/` or `/Volumes/`, and `<dir>` is writable. Release builds always; debug builds only with `GANTRY_UPDATE_URL` set.
- Nothing downloads without a click; nothing restarts without Restart and update; the running install is touched only after the download and hash check succeed.
- Copy (exact):
  - Linux elsewhere: `Gantry is installed at <path>, which it can't update itself.`
  - Translocated: `Move Gantry to Applications with Finder first.`
  - On a disk image: `Copy Gantry to Applications first.`
  - Permission refused: `macOS did not let Gantry replace itself in /Applications. Allow Gantry under System Settings > Privacy & Security > App Management, or move Gantry to ~/Applications.`
  - Downloading: `Downloading Gantry <version>… <n>%`, pill `↓ <n>%`; installing: `Installing…`.
- Branch `feat/click-to-update`, PR to `release/0.15.0`.

## Review Focus

1. A connection that stalls or drops mid-download must end in Failed (download) within the timeouts, with the old install untouched and the temp folder gone — test in Task 3.
2. A release missing the platform file, `SHA256SUMS`, or this file's line in it must not be offered (no file) or must fail at the hash step (no line) — tests in Tasks 1 and 3.
3. Pressing Download and install in the card and in Settings, or twice, must start one run — test in Task 2 (`Run::busy`) and the guard in Task 5.
4. Cancel during the download must return to the offer and leave no temp folder — test in Task 3.
5. Leftover `.Gantry.app.new` / `.Gantry.app.old` from a crashed run must not break the next run — test in Task 4.

---

### Task 1: Release assets and `SHA256SUMS`

**Files:**
- Modify: `crates/app-core/src/release.rs` (`Release`, `parse_release`, tests)

**Interfaces:**
- Produces:
  - `pub struct Asset { pub name: String, pub url: String, pub size: u64 }` (`Clone, Debug, PartialEq, Eq`)
  - `Release.assets: Vec<Asset>` (every asset with a name and a `browser_download_url`; no URL filtering here)
  - `pub fn sum_for(sums: &str, name: &str) -> Option<String>`: the lowercase 64-hex digest on the line whose name (after two spaces, or `" *"`) equals `name`; `None` when absent or malformed.

- [ ] **Step 1: Write the failing tests** in `release.rs`'s test module:
  - `parses_assets`: a releases JSON entry with `"assets": [{"name":"gantry-0.15.0-x86_64-linux.tar.gz","browser_download_url":"https://github.com/idogrf/gantry/releases/download/v0.15.0/gantry-0.15.0-x86_64-linux.tar.gz","size":123}, {"name":"SHA256SUMS", ...}]` gives `assets.len() == 2` and the first equals that `Asset`; an asset without `browser_download_url` is skipped; an entry without `assets` gives an empty vec.
  - `sum_for_finds_the_line`: for `"<64×a>  gantry-0.15.0-x86_64-linux.tar.gz\n<64×b>  gantry-0.15.0-aarch64-macos.dmg\n"`, `sum_for(.., "gantry-0.15.0-aarch64-macos.dmg") == Some("b"*64)`; a missing name, a 63-char digest, and uppercase input normalized to lowercase are covered.
- [ ] **Step 2:** `scripts/cargo test -p app-core release` → FAIL (no `assets`, no `sum_for`).
- [ ] **Step 3:** Add `Asset`, `Release.assets`, parse in `parse_release`; add `sum_for`. Fix every `Release { .. }` literal in the workspace (tests in `ui-gpui` too) with `assets: Vec::new()`.
- [ ] **Step 4:** `scripts/cargo test -p app-core release` → PASS; `scripts/cargo check -p ui-gpui --tests` → compiles.
- [ ] **Step 5:** Commit `feat(release): read release assets and SHA256SUMS`.

### Task 2: `app-core::self_update` — where, what, and the run's states

**Files:**
- Create: `crates/app-core/src/self_update.rs`
- Modify: `crates/app-core/src/lib.rs` (`pub mod self_update;`)

**Interfaces:**
- Consumes: `release::{Release, Asset}`, `version::Version`.
- Produces:
  - `pub enum Platform { Linux, Mac }` + `pub fn current() -> Option<Platform>` (Linux x86_64, macOS aarch64; else `None`).
  - `pub const DOWNLOAD_PREFIX: &str = "https://github.com/idogrf/gantry/releases/download/";`
  - `pub fn asset_name(platform: Platform, version: Version) -> String`
  - `pub fn origin_of(url: &str) -> Option<&str>` (`scheme://host[:port]`)
  - `pub struct Pick { pub file: Asset, pub sums: Asset }` + `pub fn pick(release: &Release, platform: Platform, extra_origin: Option<&str>) -> Option<Pick>`: both assets present and each URL starts with `DOWNLOAD_PREFIX` or with `extra_origin` + `/`.
  - `pub enum Target { Linux { home: PathBuf }, Mac { app: PathBuf } }` with `pub fn installed_binary(&self) -> PathBuf` (`<home>/.local/bin/gantry`, `<app>/Contents/MacOS/gantry`).
  - `pub enum NotHere { Elsewhere(PathBuf), Translocated, DiskImage, Unsupported }` with `pub fn line(&self) -> Option<String>` (the spec copy; `Unsupported` → `None`).
  - `pub fn install_target(platform: Platform, exe: &Path, home: &Path, writable: impl Fn(&Path) -> bool) -> Result<Target, NotHere>`: `exe` is already canonical; strip a trailing `" (deleted)"` from its file name first (Linux after a reinstall). Linux: `exe == home/.local/bin/gantry` and `writable(home/.local/bin)`. Mac: path ends `Gantry.app/Contents/MacOS/gantry`; `/AppTranslocation/` → `Translocated`; starts with `/Volumes/` → `DiskImage`; parent of the `.app` not writable → `Elsewhere(app path)`.
  - `pub enum Step { Download, Hash, Install, Version, Permission }` with `pub fn line(&self) -> &'static str` (`"The download failed."`, `"The download did not match its checksum."`, `"Installing failed."`, `"The installed Gantry did not report the new version."`, and the Permission copy from Global Constraints).
  - `pub enum Run { Idle, Downloading { version: Version, percent: u8 }, Installing { version: Version }, Failed { step: Step, detail: String }, Done(Version) }` (`Clone, Debug, PartialEq, Eq, Default = Idle`) with `pub fn busy(&self) -> bool` (Downloading/Installing), `pub fn line(&self) -> Option<String>` (`Downloading Gantry 0.15.0… 42%`, `Installing…`, the step line; `None` for Idle/Done) and `pub fn pill(&self) -> Option<String>` (`↓ 42%` while downloading, else `None`).
  - `pub fn percent(done: u64, total: Option<u64>) -> u8` (0 with no total, capped at 100).
  - `Pick`, `Target`, `NotHere`, `Step`, `Run` all derive `Clone, Debug, PartialEq, Eq`.
  - `#[derive(Clone, Debug, Default, PartialEq, Eq)] pub enum Offer { #[default] None, Install(Target, Pick), NotHere(Option<String>) }` and `pub fn offer(target: Result<Target, NotHere>, pick: Option<Pick>) -> Offer`: a `NotHere` gives `NotHere(its line)`; a target without a pick gives `NotHere(None)`; both give `Install`.

- [ ] **Step 1: Write the failing tests** in the new module:
  - `asset_names`: Linux 0.15.0 → `gantry-0.15.0-x86_64-linux.tar.gz`; Mac → `gantry-0.15.0-aarch64-macos.dmg`.
  - `pick_needs_both_files_on_github`: a release with the Linux file and `SHA256SUMS` under the prefix → `Some`; without `SHA256SUMS` → `None`; with a URL on `https://evil.example/` → `None`; with `http://127.0.0.1:8765/x` and `extra_origin = Some("http://127.0.0.1:8765")` → `Some`.
  - `linux_target`: `/home/u/.local/bin/gantry` with writable → `Linux { home: /home/u }`; `/home/u/.local/bin/gantry (deleted)` → same; `/usr/bin/gantry` → `Elsewhere`, line `Gantry is installed at /usr/bin/gantry, which it can't update itself.`; not writable → `Elsewhere`.
  - `mac_target`: `/Applications/Gantry.app/Contents/MacOS/gantry` → `Mac { app: /Applications/Gantry.app }`; `/private/var/folders/x/AppTranslocation/y/d/Gantry.app/Contents/MacOS/gantry` → `Translocated`; `/Volumes/Gantry/Gantry.app/Contents/MacOS/gantry` → `DiskImage`; unwritable parent → `Elsewhere`.
  - `run_texts`: `Downloading{0.15.0, 42}` → line `Downloading Gantry 0.15.0… 42%`, pill `↓ 42%`, `busy()`; `Installing` busy, line `Installing…`; `Failed{Permission}` line equals the Permission copy; `Done` and `Idle` not busy, no line.
  - `percent_bounds`: `(0, None) → 0`, `(50, Some(200)) → 25`, `(300, Some(200)) → 100`.
  - `offer_rules`: `Err(Translocated)` + a pick → `NotHere(Some("Move Gantry to Applications with Finder first."))`; `Err(Unsupported)` → `NotHere(None)`; `Ok(target)` + `None` → `NotHere(None)`; `Ok` + `Some(pick)` → `Install(..)`.
- [ ] **Step 2:** `scripts/cargo test -p app-core self_update` → FAIL.
- [ ] **Step 3:** Implement the module as specified above.
- [ ] **Step 4:** `scripts/cargo test -p app-core self_update` → PASS.
- [ ] **Step 5:** Commit `feat(self-update): decide where and what to install`.

### Task 3: The engine — download, check, Linux install, confirm

**Files:**
- Create: `crates/ui-gpui/src/self_update.rs` (GPUI-free: no `gpui` imports in this file)
- Modify: `crates/ui-gpui/src/lib.rs` (module), `crates/ui-gpui/Cargo.toml` (`sha2 = "0.10"`)

**Interfaces:**
- Consumes: Task 2's `Pick`, `Target`, `Step`, `percent`; `update_watch::probe` and `PROBE_TIMEOUT`.
- Produces:
  - `pub(crate) fn run(pick: &Pick, target: &Target, version: Version, cancel: &AtomicBool, progress: &mut dyn FnMut(u8)) -> Result<Version, Failure>`
  - `pub(crate) enum Failure { Cancelled, At(Step, String) }`
  - Timeouts (constants): connect 20 s, response 30 s, whole download 15 min.

The run: fresh temp dir (`tempfile`-style unique dir under `std::env::temp_dir()`, removed on every exit path by a drop guard) → download `pick.file` streaming into it, calling `progress(percent(..))` at most once per whole-percent change and checking `cancel` between reads (→ `Failure::Cancelled`) → download `pick.sums` → SHA-256 of the file vs `sum_for` (missing line or mismatch → `At(Hash, ..)`) → install (Linux below; Mac is Task 4, `At(Install, "not on this platform")` until then) → `probe(target.installed_binary(), PROBE_TIMEOUT)` must equal `version`, else `At(Version, ..)`.

Linux install: `tar -xzf <file> -C <tmp>` then `sh <tmp>/gantry-<version>/install.sh` with `HOME=<home>`; a non-zero exit is `At(Install, <stderr tail>)`.

- [ ] **Step 1: Write the failing tests** (`#[cfg(test)]`, a tiny local HTTP server on `TcpListener` like `release_check`'s tests; fixtures built in the test: a stub `gantry` script `#!/bin/sh\necho "gantry 9.9.9 (test)"`, a stub `gantryd`, the repo's real `packaging/linux/install.sh`, desktop file and SVG, packed with `tar -czf` as `gantry-9.9.9/` into `gantry-9.9.9-x86_64-linux.tar.gz`, plus a matching `SHA256SUMS`; target `Linux { home: <tempdir> }` with an old stub in `<home>/.local/bin/gantry` printing 0.1.0). `#[cfg(target_os = "linux")]` on these, with a comment.
  - `installs_a_checked_tarball`: `run` → `Ok(9.9.9)`; `<home>/.local/bin/gantry --version` prints 9.9.9; `gantryd` present; progress reached 100.
  - `wrong_checksum_touches_nothing`: `SHA256SUMS` with another digest → `At(Hash, _)`; the old stub still prints 0.1.0.
  - `missing_sum_line_fails_at_hash`: `SHA256SUMS` without the file's line → `At(Hash, _)`.
  - `cut_download_fails`: the server sends `Content-Length: 1000000`, 10 bytes, then closes → `At(Download, _)`; old stub untouched; no `gantry-update-*` dir left in the temp dir used by the test.
  - `cancel_stops_and_cleans`: `cancel` set before the first read → `Failure::Cancelled`; old stub untouched; temp dir gone.
  - `version_must_match`: the tarball's stub prints 9.9.8 while `version` is 9.9.9 → `At(Version, _)`.
- [ ] **Step 2:** `scripts/cargo test -p ui-gpui self_update` → FAIL.
- [ ] **Step 3:** Implement `run` and its helpers as described.
- [ ] **Step 4:** `scripts/cargo test -p ui-gpui self_update` → PASS; `cargo deny check` → ok.
- [ ] **Step 5:** Commit `feat(self-update): download, check and install on Linux`.

### Task 4: macOS install and restart

**Files:**
- Modify: `crates/ui-gpui/src/self_update.rs` (Mac install), `crates/ui-gpui/src/update_watch.rs` (`restart_command`)

**Interfaces:**
- Consumes: Task 3's `run` / `Failure`.
- Produces: the Mac branch of the install step; `restart_command(path, resume)` on macOS returns `open -n <app> --args [--resume-sessions]` where `<app>` is the `.app` three levels above `path`.

Mac install, in order: remove leftover `<dir>/.Gantry.app.new` and `.Gantry.app.old`; `hdiutil attach -nobrowse -readonly -noautoopen -mountpoint <tmp>/mnt <dmg>`; `ditto <tmp>/mnt/Gantry.app <dir>/.Gantry.app.new`; `hdiutil detach <tmp>/mnt` (also on failure); `codesign --verify --deep --strict <new>`; rename `Gantry.app` → `.Gantry.app.old` and `.Gantry.app.new` → `Gantry.app` (second rename failing renames the old one back); remove `.Gantry.app.old` (failure only logged). A rename error of kind `PermissionDenied` (or raw EPERM) is `At(Permission, ..)`; others `At(Install, ..)`.

- [ ] **Step 1: Write the failing tests** (`#[cfg(target_os = "macos")]`, with a comment: they run in CI's macOS job on `release/*` PRs and by hand): the fixture builds `Gantry.app` with a `Contents/MacOS/gantry` script printing a version and an `Info.plist`, signs it `codesign --force --deep --sign -`, and makes `gantry-9.9.9-aarch64-macos.dmg` with `hdiutil create -srcfolder`.
  - `swaps_the_app`: old app (prints 0.1.0) in `<tmp>/Applications/Gantry.app`, target `Mac { app }` → `Ok(9.9.9)`; the app prints 9.9.9; no `.Gantry.app.new` / `.old` left; the dmg is detached (`hdiutil info` does not list the mount point).
  - `leftovers_are_cleared`: pre-create `.Gantry.app.new` and `.Gantry.app.old` with junk → still `Ok(9.9.9)`.
  - `unwritable_folder_keeps_the_old_app`: `chmod 555` the folder → `Err(At(Permission | Install, _))`, old app still prints 0.1.0 (restore permissions in the test's cleanup).
  - `restart_command_opens_the_bundle` (pure, any macOS): program `open`, args `-n <app> --args --resume-sessions`.
- [ ] **Step 2:** On Linux these are compiled out: run `scripts/cargo check -p ui-gpui --tests --target aarch64-apple-darwin` only if the target is installed; otherwise rely on CI. Note in the commit message that the macOS tests run in CI.
- [ ] **Step 3:** Implement the Mac branch and the macOS `restart_command`.
- [ ] **Step 4:** `scripts/cargo test -p ui-gpui self_update update_watch` → PASS on Linux (Linux tests unchanged). The macOS run happens in Task 7's CI dispatch.
- [ ] **Step 5:** Commit `feat(self-update): install the Mac app by swapping bundles`.

### Task 5: The workspace owns the run; the release card shows it

**Files:**
- Create: `crates/ui-gpui/src/workspace_view/updates.rs`
- Modify: `crates/ui-gpui/src/workspace_view/mod.rs` (fields, `mod updates;`), `crates/ui-gpui/src/workspace_view/title_bar.rs` (`render_release_card`, the pill), `crates/ui-gpui/src/self_update.rs` (the global)

**Interfaces:**
- Consumes: Tasks 2–4.
- Produces:
  - In `self_update.rs`: `#[derive(Clone, Debug, Default, PartialEq, Eq)] pub(crate) struct UpdateRow { pub latest: Option<Version>, pub offer: Offer, pub run: Run, pub restart_ready: bool }` (with `app_core::self_update::Offer`) and `impl Global`.
  - On `WorkspaceView` (in `updates.rs`): `pub(super) fn start_self_update(&mut self, cx)`, `pub(super) fn cancel_self_update(&mut self, cx)`, `pub(super) fn update_offer(&self) -> Offer`, `pub(super) fn publish_update_row(&mut self, cx)` (call it wherever `releases`, `installed` or the run change).
  - Fields: `self_update: Run`, `self_update_cancel: Option<Arc<AtomicBool>>`.

Behavior: `update_offer` = `Offer::None` with no newer release (`online_notice()` is `None`); `NotHere(None)` without `Platform::current()` or in a debug build without `GANTRY_UPDATE_URL`; else `app_core::self_update::offer(install_target(..), pick(..))` with `current_exe().canonicalize()`, the home dir the crate already uses elsewhere, `writable` = create-and-remove a probe file in that folder, and `pick(newest release, platform, extra_origin)` where `extra_origin` is `origin_of(&release_check::endpoint())` only in debug builds with the override set. `start_self_update` does nothing while `self_update.busy()`, re-checks the offer, sets `Downloading{latest, 0}`, runs `self_update::run` on the background executor with progress sent back through a channel (the UI updates `percent`), and on the answer: `Ok(v)` → `installed = Some(v)`, `Run::Done(v)` (the existing install notice now wins); `Cancelled` → `Idle`; `At(step, detail)` → `Failed`. A Mac `installed` is set here directly (the Linux watcher may also report it; same value).

Card (`render_release_card`): under the notes, by `(offer, run)`: Install+Idle → **Download and install** (accent) and **Open release page**; NotHere(Some(line)) → that line in place of "Download it from the release page and install as before." + Open release page; Downloading → `run.line()`, a thin progress bar (accent fill on `theme.tokens.line`) and **Cancel**; Installing → `Installing…`; Failed → the step line, `detail` in `text_xs` muted, **Retry** (= start) and Open release page. Closing the card does not cancel. The title-bar pill shows `run.pill()` while downloading, else today's text.

The decisions here are tested in Task 2 (`offer_rules`, `run_texts`); this task is drawing and wiring, verified by clippy and the live check.

- [ ] **Step 1:** Add the fields and `updates.rs` (offer, start, cancel, publish).
- [ ] **Step 2:** Implement the global (`cx.set_global(UpdateRow::default())` at startup next to `CheckRow`), the card states and the pill.
- [ ] **Step 3:** `scripts/cargo clippy -p ui-gpui --all-targets -- -D warnings` → clean; `scripts/cargo test -p ui-gpui` → PASS.
- [ ] **Step 4: Live check (Linux)** in the isolated headless GNOME setup (CLAUDE.md "Live check"), scratch `HOME` with the debug `gantry` copied to `<HOME>/.local/bin/gantry` and started from there, `GANTRY_UPDATE_URL` pointing at a local server that serves a releases list (version 9.9.9, assets on the server's own address) plus Task 3's fixture tarball and `SHA256SUMS`. Screenshot each: the card with Download and install; Downloading with a percentage (throttle the server); the Restart and update card after it finishes; Failed after swapping in a bad `SHA256SUMS`.
- [ ] **Step 5:** Commit `feat(self-update): download and install from the release card`.

### Task 6: Settings > Window > Updates

**Files:**
- Modify: `crates/ui-gpui/src/settings_view/window.rs` (the Updates row), `crates/ui-gpui/src/settings_view/mod.rs` (`SettingsEvent`, observe `UpdateRow`), `crates/ui-gpui/src/workspace_view/overlays.rs` (event handling)

**Interfaces:**
- Consumes: Task 5's `UpdateRow`, `start_self_update`, `cancel_self_update`, `restart_and_update`.
- Produces: `SettingsEvent::{DownloadUpdate, CancelUpdate, RestartToUpdate}`.

The row: when `restart_ready` → status line `Gantry <v> is installed.` and **Restart and update**; else when `run` is not Idle → `run.line()` with Cancel (Downloading) or Retry (Failed); else when `latest` is newer than running (from either the manual or the scheduled check) and `offer == Install` → `Gantry <v> is available.` with **Download and install** and **Open release page**; `NotHere(Some(line))` → the line + Open release page; otherwise today's Check now row unchanged. Check now stays available in every state except while a download runs.

- [ ] **Step 1:** Add the events and the observer (`_update_row: cx.observe_global::<UpdateRow>(|_, cx| cx.notify())`).
- [ ] **Step 2:** Draw the row states with the shared row grammar (`row`, `pressable`).
- [ ] **Step 3:** Handle the events in `overlays.rs` beside `SettingsEvent::CheckNow`.
- [ ] **Step 4:** `scripts/cargo clippy -p ui-gpui --all-targets -- -D warnings` and `scripts/cargo test -p ui-gpui` → clean/PASS. Live check as in Task 5: start the download from Settings, see the card's pill show `↓ n%` at the same time, then Restart and update in the row.
- [ ] **Step 5:** Commit `feat(self-update): update from Settings > Window > Updates`.

### Task 7: Docs, full checks, macOS CI run

**Files:**
- Modify: `CHANGELOG.md` (Unreleased > Added), `CLAUDE.md` (`ui-gpui` and `app-core` module lists; the Release check and Update notice bullets), `README.md` (the update section)

- [ ] **Step 1:** CHANGELOG: `- **Update from the title bar** - When a new release is out, Download and install fetches it, checks it and installs it; Restart and update then finishes the job. Also in Settings > Window > Updates.` with sub-bullets for Linux (`~/.local/bin` installs) and Mac (`Gantry.app`; the first install still needs Open Anyway).
- [ ] **Step 2:** CLAUDE.md and README as listed.
- [ ] **Step 3:** `cargo fmt --all`, `scripts/cargo test --workspace`, `scripts/cargo clippy --workspace --all-targets -- -D warnings`, `cargo deny check` → all pass (long output to a file, read the tail).
- [ ] **Step 4:** With the user's OK to push: run CI by hand on the branch with `linux` off (macOS + Windows) so Task 4's macOS tests run; expected: `self_update` tests pass on macOS.
- [ ] **Step 5:** Commit `docs: click to update`.

### Task 8: The real-Mac check (with the Gantry Mac user, before the 0.15.0 release)

Not code. Using the spike's `Spike.app` builds (branch `spike/mac-self-update`, run artifacts), the user's Mac user runs the three situations in the spec's "The real-Mac check". Outcome:
- No prompt anywhere → nothing to change.
- Prompt in `/Applications` only → nothing to change (the Permission path from Task 4/5 handles it); check the message reads right on their Mac.
- Blocked everywhere → in `install_target`, return `NotHere::Unsupported` for `Platform::Mac` (one line + its test), CHANGELOG sub-bullet updated.
Then delete `spike/mac-self-update` and its workflow runs (with the user's OK).
