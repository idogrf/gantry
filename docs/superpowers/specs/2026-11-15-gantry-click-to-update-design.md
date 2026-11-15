# Click to update (design)

Status: design, 2026-11-15. Builds on the release check (`2026-10-16-gantry-release-check-design.md`) and the update
notice (`2026-10-05-gantry-update-notice-design.md`).

## Why

Today Gantry knows a new release exists (the `↑ Update to x.y.z` pill and its release card), but the user has to open
the release page, download the tarball or the dmg, unpack it, run `install.sh` or drag `Gantry.app`, and only then does
Gantry offer **Restart and update** (Linux only; a Mac never gets that button). The goal is one click from "a release
exists" to "Restart and update", on Linux and on the Mac.

## Goals

- In the release card, a **Download and install** button downloads the release for this platform, checks it, and
  installs it in place of the running Gantry.
- When the install is done, the pill and card become the existing **Restart and update** (same card, same rules: Stop
  all only when the daemon is older, `--resume-sessions`, the "ends N sessions" note). Restarting stays the user's
  click.
- Linux tarball installs (`~/.local/bin`) and the Mac app (`Gantry.app`) are both covered.
- Wherever Gantry cannot install itself, it says why in one line and keeps **Open release page**, as today.

## Non-goals

- Downloading in the background, or on its own; restarting on its own.
- RPM or any install in a system folder (needs root), Windows, debug builds run from a worktree.
- Verifying build attestations or any signature beyond the SHA-256 in the release's `SHA256SUMS`.
- Delta updates, choosing an older version, rollback after the restart.
- Notarization or any Apple developer account. The first Mac install keeps its one-time "Open Anyway" step (README).
- Updating SSH hosts: they already update from the local `gantryd` after the restart (remote update pill).
- A new setting. `checkForUpdates` keeps controlling the scheduled check only.

## Anti-goals (a failure even if it works)

- Any session ends without the user clicking Restart and update.
- A failed, cut or cancelled download, a wrong hash or an error halfway through leaves a Gantry that does not start.
  The running install is replaced only by a complete, checked copy, in one rename (Mac) or by the release's own
  `install.sh` from a fully unpacked, checked tarball (Linux).
- Anything is downloaded without a click.
- A macOS permission prompt the user cannot make sense of. If macOS refuses the swap, Gantry says so plainly and
  falls back to the release page.

## What the spike showed (2026-11-14/15)

A throwaway test on GitHub's macOS 15 and 26 runners (branch `spike/mac-self-update`, kept until the real-Mac check
below is done): an ad-hoc signed `Spike.app`, opened through LaunchServices like a user opens it, downloaded a newer
dmg with `ureq`, checked its SHA-256, mounted it (`hdiutil attach -nobrowse -readonly`), copied the app beside itself
with `ditto`, swapped the two folders with two renames, and opened the new version, in both `/Applications` and
`~/Applications`, with no prompt and no error.

- The downloaded dmg and the copied app carry **no `com.apple.quarantine`**: Gantry's `Info.plist` does not set
  `LSFileQuarantineEnabled`, so files it writes are not marked, and Gatekeeper never stops the updated app.
- `spctl --assess` says "rejected" for the new app; that is normal for an ad-hoc signature and does not stop an
  unmarked app.
- A quarantined ad-hoc app (as a browser download is) is blocked at launch on the runners too, so the runners do run
  Gatekeeper. That is the existing first-install step, not an update problem.
- **Not proven:** the runners have System Integrity Protection off, so they cannot show whether a normal Mac asks for
  the App Management permission when an ad-hoc app replaces itself in `/Applications`, nor how an app the user
  approved with Open Anyway behaves. Public reports disagree.

## The real-Mac check (last, before release)

Both halves are built now, in one PR. The permission-refused path below (its message, Retry, the `~/Applications`
advice) is built whatever the check finds, since it costs little. The check runs at the end, before the train is
released, and the Mac half can be switched off in `install_target` (one line) if it comes back "blocked everywhere".

A Gantry Mac user runs the spike's `Spike.app` (v1 from the spike run's dmg, plus the v2 dmg and its sha256 served
from a local `python3 -m http.server`) on a normal Mac with SIP on, in three situations:

1. In `/Applications`, installed from a dmg downloaded with a browser and approved with Open Anyway.
2. The same in `~/Applications`.
3. In `/Applications`, after copying it there from the Terminal (unmarked).

Recorded: whether the swap succeeds, whether macOS shows an App Management prompt or "Gantry was prevented from
modifying apps on your Mac" in Notification Center, and whether v2 starts. The outcome picks one of:

- **No prompt anywhere:** build as below.
- **Prompt in `/Applications`, none in `~/Applications`:** keep the design; when the swap fails with a permission
  error the card says "macOS did not let Gantry replace itself in /Applications. Allow Gantry under System Settings >
  Privacy & Security > App Management, or move Gantry to ~/Applications." plus Retry and Open release page.
- **Blocked everywhere:** the Mac half is switched off; Mac users keep Open release page.

## How it works

### Release assets

`app-core::release::parse_releases` also reads each release's `assets` (already in the list Gantry fetches; no new
request): name, `browser_download_url`, size. An asset counts only when its URL starts with
`https://github.com/idogrf/gantry/releases/download/` (redirects to GitHub's storage are followed). Per platform:

- Linux x86_64: `gantry-<version>-x86_64-linux.tar.gz`.
- macOS aarch64: `gantry-<version>-aarch64-macos.dmg`.
- Both: `SHA256SUMS` (lines `<hex>  <file name>`, written by `release.yml`).

A release missing the platform's file or `SHA256SUMS` cannot be installed (the card falls back). The release is always
the newest one in the card (`Online.latest`).

### Can this Gantry update itself? (`install_target`, pure, in app-core)

Decided from the running binary's path (`std::env::current_exe`, canonicalized) and checked again when the button is
pressed:

- **Linux:** the binary is `~/.local/bin/gantry` (the only place `install.sh` installs to) and that folder is
  writable. Anything else (`/usr/bin` from the RPM, a worktree's `target/`, a copy elsewhere) gives the reason "Gantry
  is installed at <path>, which it can't update itself."
- **macOS:** the binary is `<dir>/Gantry.app/Contents/MacOS/gantry`, the path is not under `/AppTranslocation/` or
  `/Volumes/`, and `<dir>` is writable. Translocated: "Move Gantry to Applications with Finder first."; on a disk image:
  "Copy Gantry to Applications first."
- Other platforms or architectures: not offered, no reason shown (today's card).
- Debug builds: not offered, except with `GANTRY_UPDATE_URL` set (the existing test seam), where the target checks
  apply as written so a test can point it at a scratch `HOME`.

### The steps

All of it runs off the UI thread; the card shows the state. One run at a time.

1. **Download** the platform file and `SHA256SUMS` with `ureq` into a fresh temp folder (the Mac copy is staged
   beside the app in step 3, so the temp folder's volume does not matter). Progress from `Content-Length`. Cancel
   stops it and removes the folder.
2. **Check** the file's SHA-256 against its `SHA256SUMS` line. A mismatch or a missing line fails the run.
3. **Install:**
   - **Linux:** unpack the tarball into the temp folder; run its `install.sh` (`sh <unpacked>/install.sh`). Using the
     release's own script keeps one source of truth for what an install writes (binaries, desktop entry, icon). Its
     exit status decides success.
   - **macOS:** attach the dmg read-only with no Finder window to a mount point in the temp folder; `ditto` its
     `Gantry.app` to `<dir>/.Gantry.app.new`; detach; `codesign --verify --deep --strict` the copy; rename the running
     `Gantry.app` to `<dir>/.Gantry.app.old`, rename `.Gantry.app.new` to `Gantry.app` (if the second rename fails,
     rename the old one back); remove `.Gantry.app.old`. Leftover `.new`/`.old` folders from an earlier run are removed
     first. The running process keeps working from the moved bundle until the restart.
4. **Confirm:** run the installed binary's `--version` (`update_watch::probe`). It must report the release's version.
   Then set `WorkspaceView.installed` to it, which makes `update_notice()` return the install notice: the pill and card
   turn into Restart and update. On Linux the file watcher would find it too; setting it directly avoids waiting on
   the watcher.
5. The temp folder is removed whatever happened.

### Restart

Unchanged on Linux (`restart_and_update`). On macOS the same code runs (the start path is the same
`.../Gantry.app/Contents/MacOS/gantry`, now the new file); whether the new window is started by spawning that binary
(as today) or with `open -n <app> --args --resume-sessions` is left to the builder: the spike used `open`, which gives a
normal app launch.

### What the user sees (release card)

- **Available, installable:** the notes as today; buttons **Download and install** (primary) and **Open release
  page**.
- **Available, not installable:** the notes; the reason line (above) instead of "Download it from the release page and
  install as before."; **Open release page**.
- **Downloading:** "Downloading Gantry x.y.z… 42%" with a progress bar and **Cancel**; the pill reads `↓ 42%`. Closing
  the card does not stop it.
- **Installing:** "Installing…", no buttons (seconds).
- **Done:** the existing Restart and update card replaces it (the install notice wins, as today).
- **Failed:** one line with what failed (download, hash, install, version check, permission refused) and the detail in
  the card's small text; **Retry** and **Open release page**.

Settings > Window > Updates (Check now) keeps its Open release page only (builder may add the same button there if it
falls out of the code for free).

## Code layout (suggested)

- `app-core::release`: assets on `Release`, `SHA256SUMS` parsing.
- `app-core::self_update` (new, pure): `install_target`, the asset pick per platform, the card state machine
  (`Available`, `Downloading { percent }`, `Installing`, `Failed { step, detail }`) and its texts. Unit tested.
- `ui-gpui::self_update` (new): the download, check and install steps on a background thread, cancel flag, progress
  reports; `workspace_view/title_bar.rs` draws the states.
- No protocol, daemon or `claude-index` change. `sha2` (already used by `daemon`) can be added to `ui-gpui`.

## Testing

- Unit tests for everything in `app-core::self_update` and the asset parsing (real `releases` JSON and `SHA256SUMS`
  samples).
- **Linux end to end (CI, every PR):** a scratch `HOME` with an old install in `~/.local/bin`, a local server with a
  GitHub-shaped releases list, a newer tarball and its `SHA256SUMS`. *(My call: the tarballs are fakes made in the
  test, a stub `gantry` script printing the version plus the repo's real `install.sh`, desktop entry and icon, so CI
  builds no release tarball. With `GANTRY_UPDATE_URL` set, download links on the test server's own address pass the
  URL check.)* run the install through a hidden
  headless entry point (e.g. `gantry self-update --check-url <url>`, debug or test builds only) and check the new
  `--version`, the desktop entry and a wrong-hash run that leaves the old install untouched.
- **macOS end to end (`workflow_dispatch`, and on `release/*` PRs with the existing macOS job):** the same with
  `Gantry.app` in `/Applications` and `~/Applications`; the newer version is a second build with the version bumped.
  The runners have SIP off, so this proves the mechanics, not the permission question (step 1 covers that).
- A live check of the card states in the isolated headless GNOME setup.

## Left to the builder

- Exact copy, the progress bar's look, where Retry sits.
- Whether the macOS restart uses `open -n` or spawns the binary.
- The name and shape of the hidden headless entry point, and how the macOS CI job makes its "newer" build cheaply.
- Timeouts for the download (a stalled connection must end in Failed, not hang).

## Docs

`CHANGELOG.md` (Added: **Update from the title bar**), `CLAUDE.md` (the release check and update notice bullets,
crate lists), README's update section.
