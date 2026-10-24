# Changelog

User-facing changes to Gantry. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the version follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html) (`0.MINOR.PATCH`).

## [Unreleased]

### Added
- **Rename Claude panes** - Rename now works on Claude panes, not only terminals: from the sidebar, or in place on a split pane's header. On this machine the name also becomes the Claude session's name, as `/rename` would, so Claude and `claude --resume` show it too.

## [0.10.0] - 2026-10-22

### Added
- **Welcome screen** - With no tabs open, Gantry shows what a session is and your recent folders from every host; one click starts Claude in one, or continue a past conversation, or open a terminal.
- **Update remote Gantry** - A connected SSH host that runs an older Gantry than this one shows `↑ <version>` in the SSH hosts list; one click uploads the new `gantryd` and restarts the host's daemon, after asking when sessions are running there.
- **Paste images on SSH hosts and in dev containers** - Ctrl+V with an image on the clipboard now works in a Claude pane on an SSH host or in a dev container, as it does on this machine. Needs a daemon restart; SSH hosts need the new `gantryd`.

### Changed
- **Terminal, not shell** - Buttons, menus and shortcut names say "terminal"; every sidebar row with New Claude here also has New terminal here, and the `>_` button says what it does.
- **One launcher for every host** - The launcher lists your folders from this machine and every SSH host in one list, with the host's name on remote folders; type a path (`~/…`, `/…`, or `devbox:~/…` for a host) to open any folder, even one you never used; New Claude session and the terminal button open it straight away.
  - The Claude config dir is a small badge on the highlighted folder (Tab / Shift+Tab or a click changes it), shown only with two or more config dirs.
- **"One new tab" is now "Running only"** - Same behavior, a name that says it.

### Removed
- **Title-bar +** - Ctrl+Shift+T still opens a new session in the current folder.

### Fixed
- **New tab from an SSH pane** - Ctrl+Shift+T on a pane of an SSH host opens the new session on that host, not on this machine.
- **`gantry -- claude`** - A Claude started from the command line is a tracked session with Continue, like one started in Gantry.
- **Continue that can't continue** - Says why instead of doing nothing.

## [0.9.1] - 2026-10-19

### Fixed
- **Git filters in untrusted repos** - A repo's own git filters (and a signed commit's `gpg.program`) can no longer run a program when Gantry reads its status, diffs or commits. Every filter the repo's own config sets is skipped on every call, and the panel notes that filters are not run in a repo that has them; filters you set up yourself, like git-lfs, still run. Needs a daemon restart.
- **SSH sign-in prompts** - Every sign-in question except the host-key one is now masked, so codes and passwords in other languages (Passwort, Mot de passe, one-time codes) are never shown while typed.

## [0.9.0] - 2026-10-19

First public release.

### Added
- **License notices** - Every download now includes `NOTICE` and `THIRD_PARTY_LICENSES.html`, the license texts of everything Gantry is built from.
- **Checksums** - Each release lists a `SHA256SUMS` file for its downloads.

### Changed
- **macOS install** - The README opens the app with one `xattr` command instead of the long Privacy & Security steps.
- **Empty start** - When there is nothing to restore (the first start, or after every pane was closed), Gantry opens on its empty screen with the New Claude session and New shell buttons, not a shell in the folder it was started from. `gantry --cwd DIR` and `gantry -- program` still open their tab.
- **Sessions outside Gantry on macOS** - On macOS and Windows a Claude running outside Gantry shows as a closed session, not as "running elsewhere", since those systems cannot yet check whether its process still runs. Take over is not offered there. Needs a daemon restart.

## [0.8.0] - 2026-10-19

### Added
- **Animations** - Splitting a pane slides the new pane in, and overlays, menus and cards fade in; Settings > Appearance > Interface turns it off.
- **Drag panes** - Drag panes to rearrange them. Drag a pane by its header (in a split tab), or by its row in the sidebar, onto another pane: the middle swaps the two, an edge splits that pane on that side, and a preview shows what will happen. A tab you drag from the sidebar joins the pane as a new pane (dropped on the middle, the pane it lands on becomes a tab of its own). An edge drop that would make a fifth pane in a tab is refused and the preview turns red. Drag a pane of a split tab into the sidebar to make it a tab of its own. Local and SSH panes can share a tab, and the pane you drop gets focus. Esc, or letting go anywhere else, cancels the drag.
- **Pane scrollbar** - Panes have a scrollbar at their right edge. It shows where the view sits in the pane's scrollback; drag it, or click the track to jump. It turns blue while the view scrolls and greys out when idle. A "Jump to bottom" chip appears when you scroll up, fades out a second and a half after you stop, and scrolls back to the live output in a quarter of a second. Shell panes always have it; for Claude panes it is a switch in Settings > Appearance > Panes (on by default), and it shows something only while Claude runs in its classic renderer (`/tui default`), since in fullscreen mode Claude scrolls inside the app. It is hidden for programs on the alternate screen (vim, less).
- **Tab history** - Back and Forward through the tabs you have visited, like a browser: the mouse side buttons (anywhere in the window, over a terminal pane too) or the new "Previous tab in history" / "Next tab in history" shortcuts (`Ctrl+Shift+PageUp` / `Ctrl+Shift+PageDown`, `Cmd+[` / `Cmd+]` on macOS, rebindable in Settings > Shortcuts). Showing a tab any other way drops the forward history; tabs you closed are skipped. The history is not saved between runs.
- **Git view** - Every session whose folder is in a git repo shows a branch chip (branch, a dot and the number of changed files, ahead/behind) in the title bar and in the header of each pane of a split tab, with a card on hover. Gantry only reads: it never stages, commits or switches branches. Needs a daemon restart.
  - Click the chip, or press `Ctrl+Shift+G` (`Cmd+Shift+G` on macOS, rebindable), to slide in a panel with the branch, the changed files (+/- counts) and the commits of the branch (those it has beyond the default branch; the latest ones on the default branch itself). An × closes the panel.
  - Click a commit to see its message and the files it changed, and a file to see its diff on a page of its own (a red × closes it). The diff marks changed words, has line numbers, scrollbars like the panes' and sideways scrolling for long lines, a "Show anyway" button for very large files, and shows Hebrew or Arabic lines in display order when right-to-left display is on. Select rows with a click (Shift extends) and copy them with `Ctrl+C`.
  - A repo where git fails shows a red "git error" chip with git's message, and a broken filter (such as nbstripout pointing at a missing python) is skipped so the view still works.
  - It works on this machine and on SSH hosts (they need the updated `gantryd`). It asks git every 5 seconds while Claude is busy in the repo or the panel is open, and once a minute otherwise.

### Fixed
- **Window size** - Gantry opens at the size (and maximized state) the window had when you closed it, instead of always 1000×700. The position is left to the window manager.
- **SSH sessions** - Sessions on SSH hosts no longer show a Claude config dir badge (they always run the remote's own `claude`); before, a pane could show this machine's default dir.

## [0.7.1] - 2026-10-16

### Changed
- The last prompt card's settings (show, text size, background) moved from Settings > Window to Settings > Appearance > Panes. In `settings.json` they are now `lastPrompt.enabled`, `lastPrompt.size` and `lastPrompt.color` instead of `window.lastPrompt`, `window.lastPromptSize` and `window.lastPromptColor`; values saved under the old keys are not carried over.

## [0.7.0] - 2026-10-16

### Added
- Gantry checks GitHub once a day for a newer release. A newer one shows as `↑ Update to x.y.z` in the title bar, with its release notes and an Open release page button; Settings > Window > Updates turns the check off and has a Check now button that asks right away and says what it found.

### Fixed
- Copying inside Claude or a shell running in a dev container or on an SSH host now reaches this machine's clipboard. Programs there copy with the terminal's clipboard escape sequence (OSC 52), which Gantry ignored.

## [0.6.2] - 2026-10-12

### Fixed
- After the daemon restarted, sessions of a Claude folder other than the daemon's own came back with "Session ID … is already in use" instead of resuming. Resuming now waits until the daemon has read every configured Claude folder. Needs a daemon restart.

## [0.6.1] - 2026-10-12

### Changed
- A listed SSH host that is not connected no longer shows its panes or sessions in the sidebar (only what is live is listed); they come back when the host connects. A host that is connecting or reconnecting keeps its rows.

## [0.6.0] - 2026-10-12

### Added
- SSH hosts can have a port: a Port field in the Add SSH host wizard (under Advanced) and in Settings > SSH hosts. Empty uses what ssh and `~/.ssh/config` say.
- An SSH hosts block in the sidebar, above Recent sessions: a card with every configured host and its state (green dot = connected). Click a host that is not connected to connect it (or to open Install or Update when it needs one); "Add SSH host…" sits at its end, so it is there even before any host exists. The × on a connected host asks Detach (leave its sessions running) or Close all sessions (ends them and stops the host's Gantry) when sessions run there, stops an idle host's Gantry at once, and cancels a connection that is still starting. With more than 4 hosts the card starts collapsed to its heading and the connected hosts; click the heading to expand it.

### Changed
- The Add SSH host wizard now installs `gantryd`, Gantry without its window, instead of the full app: it is smaller, uploads faster and needs no system libraries on the host. New hosts default to `~/.local/bin/gantryd`; hosts you added before keep their own command (an Update there replaces that file with `gantryd`). `install.sh` and the RPM install `gantryd` beside `gantry`.
- When Gantry is installed on a host but cannot start for missing system libraries, the wizard now lists every missing library at once with one install command, instead of one library per try.

## [0.5.0] - 2026-10-08

### Added
- SSH hosts have a sign-in method: Automatic, Password (only a password, keys are not tried) or SSH key (no password). Pick it in the Add SSH host wizard or from the host's row in Settings > SSH hosts. A server that hangs up after too many failed tries now says so and points at this setting.

### Fixed
- SSH hosts that need a password: the sign-in dialog now appears. Real ssh calls the helper with just the prompt, which Gantry did not recognise, so ssh gave up with "Permission denied" and no question.
- A host where Gantry is installed but cannot start because a system library is missing (for example `libasound2` or `libxkbcommon-x11-0`) now says which library and package, instead of "A command was not found on the host".

## [0.4.0] - 2026-10-08

### Added
- SSH hosts: list remote machines in `sshHosts` in settings.json (name and ssh target); the New buttons then offer Local and the hosts, connect on first use with sign-in dialogs, and reconnect by themselves. Gantry can be installed on the remote from the app (see the SSH host wizard below). A connecting host can be cancelled, a host taken over by another Gantry says so instead of reconnecting, and Stop all sessions on quit only stops this machine's sessions. The remote needs lingering (`loginctl enable-linger`) to keep sessions after the last ssh session ends.
- Add SSH host wizard: pick a host from your ssh config or type one, and Gantry installs itself there (Linux x86_64 hosts, from a Linux x86_64 Gantry), checks it, offers to turn on lingering, then saves and connects the host. The upload checks its size (and hash) before it replaces anything, so a dropped connection never leaves a broken Gantry; if turning lingering on fails the wizard shows the command to run and lets you save anyway.
- Update and restart for remote hosts, and Install on a host without Gantry, from the host's card.
- Settings > SSH hosts: see each host's state, rename it, edit its target, remove it, or add one.
- Launcher host chip with remote folders and path completion: pick an SSH host in Ctrl+Shift+O, see its open folders, favorites and recent folders, and type a path that completes from the host.
- Remote sessions in recent sessions and conversation search; hosts that are not connected get a line with a Connect button.
- Take over works on SSH hosts.

### Changed
- Remote sessions no longer offer menu actions that only work on this machine (open folder, VS Code, project color, Claude config dir choice; pinning a remote project heading as a favorite works and is saved as `ssh://<host>/<folder>`).
- Needs a daemon restart (protocol 8, also needed on every SSH host): new panes starting in `~` now open in the home folder.

## [0.3.0] - 2026-10-07

### Added
- Settings shows which version of Gantry is running, at the bottom of its menu.
- When a newer Gantry is installed while it is open, the title bar offers to restart into it; your sessions resume after the restart.
- `gantry --version`.

### Changed
- Rows marked as unread now look different from Done rows: a quieter green fill and a pin before the title, also on the title-bar chip and the pane header, and "Unread" in the hover card.

Needs a daemon restart (the daemon now reports its version).

## [0.2.0] - 2026-10-05

First versioned state: everything built up to this point.
