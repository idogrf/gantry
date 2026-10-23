![Gantry: Every Claude session. One frame.](docs/images/banner.png)

**A fast, cross-platform terminal for running and watching many [Claude Code](https://claude.com/claude-code) sessions at once.**

Gantry is a GPU-rendered terminal that knows what Claude is doing. Every pane
that runs Claude shows its status, so you can run as many sessions as you like and still know which one needs you.

Independent project, not affiliated with or endorsed by Anthropic.

https://github.com/user-attachments/assets/9a0f61cd-c2bd-46a1-966f-f141ee1364b0


![Gantry: a sidebar of sessions colored by status, and the focused Claude session](docs/images/ui-overview.png)

## See it all

Every session, by project, colored by status.

### Sessions at a glance

A sidebar lists every active session by project. A session's status colors its row, its title-bar chip and its pane
header: working, idle, needs input, done. Working rows glow.

<p align="center"><img src="docs/images/sidebar-status.png" alt="The sidebar: sessions grouped by project, colored by status" width="560"></p>

### Tabs and splits

Split any pane right or down, with Claude or a plain shell. Layouts are saved and restored.

Drag a tab from the sidebar onto a pane to split it, or drag a pane by its header onto another to swap them.

![Dragging tabs into a view as panes, then swapping two panes](docs/images/pane-drag.gif)

### Recent sessions and search

Find a past session by title, or search inside conversations (your prompts and Claude's replies).

![Searching inside conversations from the recent sessions list](docs/images/search-overlay.png)

### And also

- **Last prompt card.** Each Claude pane shows its newest prompt above the terminal, so you remember what you asked.
- **Mark as unread.** Flag a session to come back to it later. It stays flagged until Claude's status changes or you
  mark it as read.
- **Project colors.** Each project gets its own color, automatic or chosen by you.

## Never miss it

A notification and a sound when Claude needs input or finishes.

### Alerts

A native notification and a chime when a session finishes or needs input while you are looking elsewhere. Per-event
badge, notification, sound, tone and volume are configurable.

### Jump to what needs you

A "need input" pill in the title bar jumps to the next session waiting for you.

![A session that needs input, and the title-bar pill that jumps to it](docs/images/need-input.png)

### And also

- **Unseen badges.** Sessions that finished while you were away stay marked until you look at them.

## Keep it running

Close the window and your sessions continue, on this machine or on any SSH host.

- **Sessions that outlive the window.** A per-profile daemon owns every PTY, so closing the GUI does not end your
  sessions. Reopen it and it reattaches. Finished panes offer Continue (`claude --resume`) or Restart.
- **SSH hosts.** Connect to remote SSH hosts (currently Linux). Their sessions sit in the same sidebar, with the same
  status colors, search and recent sessions.
- **Dev containers.** Sessions that run in a dev container are listed under their host folder and resumed inside it.
- **Take over.** Sessions running outside Gantry show up too, and can be taken over into a pane.
- **Several Claude config dirs.** Work with more than one `CLAUDE_CONFIG_DIR` (personal, work, ...), each with its own
  badge and color. A first-run wizard finds them for you.

## Make it yours

### Everything rebindable

Shortcuts, theme (Mist, Mist light, follow system), fonts, status colors and project colors live in a Settings tab and
in a hand-editable `settings.json` that applies live.

![The same window in the dark and light themes](docs/images/theme-pair.png)

### And also

- **Right-to-left text.** Hebrew and Arabic display in the right order, as a display-only layer (selection, copy and
  links stay logical).

## Requirements and limits

- Linux x86_64 (glibc 2.35 or newer: Ubuntu 22.04+, Fedora), macOS on Apple Silicon, and Windows 10 or 11 (x86_64).
- Gantry is GPU-rendered, so it needs working graphics drivers.
- Claude Code must be installed. Gantry runs your own `claude` and does not replace it.
- SSH hosts must be Linux x86_64.
- The macOS build is not notarized (see Install).

## Privacy

Gantry reads Claude's session files on your machine, read-only, to show status and search. It has no telemetry and
sends nothing about you or your sessions anywhere. The only network call it makes itself is a daily check of this
project's GitHub releases for a newer version; turn it off in Settings > Window > Updates. SSH connections go only to
the hosts you add.

## Install

### Linux

1. Install the libraries Gantry needs.

   Ubuntu 22.04 or newer (on 24.04, use `libasound2t64` instead of `libasound2`):

   ```sh
   sudo apt install libasound2 libxkbcommon0 libxkbcommon-x11-0 libxcb1
   ```

   Fedora (most desktops already have these):

   ```sh
   sudo dnf install alsa-lib libxkbcommon libxkbcommon-x11 libxcb
   ```

2. Download `gantry-<version>-x86_64-linux.tar.gz` from the [releases page](https://github.com/idogrf/gantry/releases).

3. Unpack it and run the installer. It installs into `~/.local`:

   ```sh
   tar xf gantry-<version>-x86_64-linux.tar.gz
   cd gantry-<version>
   ./install.sh
   ```

4. Start Gantry from your application menu, or run `gantry` in a terminal. If the command is not found, add
   `~/.local/bin` to your `PATH`.

To remove it, run `./install.sh --uninstall` from the same folder.

Gantry needs glibc 2.35 or newer (Ubuntu 22.04+, Fedora).

### macOS

Apple Silicon only. Download `gantry-<version>-aarch64-macos.dmg` from the
[releases page](https://github.com/idogrf/gantry/releases), open it and drag `Gantry.app` into Applications (don't run
it from inside the DMG). The app is ad-hoc signed and not notarized, so macOS blocks it the first time. Remove the
quarantine flag once, then open it as usual:

```sh
xattr -dr com.apple.quarantine /Applications/Gantry.app
```

#### Permission prompts

macOS may ask whether Gantry can access files in Desktop, Documents, Downloads, removable volumes or other apps' data.
Gantry itself does not read those folders: the shells and `claude` it starts do, and macOS attributes their access to
the app that launched them. Allow it once per folder, or change it later in System Settings > Privacy & Security >
Files and Folders. Granting Full Disk Access is not needed.

### Windows

Windows 10 or 11, x86_64. Download `gantry-<version>-x86_64-windows.msi` from the
[releases page](https://github.com/idogrf/gantry/releases) and run it. It installs for your user only (no administrator
rights) into `%LOCALAPPDATA%\Programs\Gantry`, adds Gantry to the Start menu and its folder to your `PATH`. The installer
is not signed, so SmartScreen may warn the first time: choose More info, then Run anyway. Remove it from Settings > Apps.

For a portable copy, unpack `gantry-<version>-x86_64-windows.zip` anywhere and run `gantry.exe`.

New terminals open PowerShell (`pwsh` if installed, else Windows PowerShell); pick another shell in Settings. Not yet
on Windows:

- New panes start in the folder their tab was opened in, not the one the shell has since `cd`-ed to.
- The SSH wizard cannot upload `gantryd` to a host; install it there by hand.
- Gantry does not notice when it is reinstalled while running; restart it after an update.
- Take over ends the outside Claude at once (Windows has no polite stop for a program in another console window).

## Usage

Open Gantry from your application menu or its icon, or run `gantry` in a terminal.

Default shortcuts. All of them can be changed in Settings.

| Action | Linux and Windows | macOS |
| --- | --- | --- |
| New Claude session (same folder) | `Ctrl+Shift+T` | `Cmd+T` |
| New Claude session… (launcher) | `Ctrl+Shift+O` | `Cmd+O` |
| Find session | `Ctrl+Shift+F` | `Cmd+F` |
| Recent sessions | `Ctrl+Shift+R` | `Cmd+R` |
| Split right (Claude) | `Ctrl+Shift+D` | `Cmd+D` |
| Split down (Claude) | `Ctrl+Shift+E` | `Cmd+E` |
| Split with a terminal | add `Alt` | add `Option` |
| New terminal (same folder) | `Ctrl+Shift+Alt+T` | `Cmd+Option+T` |
| Move focus between panes | `Ctrl+Shift+Arrows` | `Cmd+Arrows` |
| Next / previous tab | `Ctrl+Tab` / `Ctrl+Shift+Tab` | `Cmd+Shift+]` / `Cmd+Shift+[` |
| Close pane | `Ctrl+Shift+W` | `Cmd+W` |
| Toggle sidebar | `Ctrl+Shift+B` | `Cmd+B` |
| Settings | `Ctrl+,` | `Cmd+,` |
| Zoom in / out / reset | `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | `Cmd+=` / `Cmd+-` / `Cmd+0` |

## Configuration

Everything you can change lives in the Settings panel: theme, fonts, colors, shortcuts, alerts and more. Open it with
`Ctrl+,` (`Cmd+,` on macOS) or with the gear button in the title bar. Changes apply as soon as you make them.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option. Third-party
notices are in [NOTICE](NOTICE).

## AI disclosure

Gantry is built with substantial help from AI coding tools. A human directs the work, reviews and tests every change,
and is responsible for the result.
