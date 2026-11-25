//! Control messages exchanged as JSON.
//!
//! `ClientMessage::Hello` and `ClientMessage::StopAll` are frozen: their JSON shape must never
//! change, so a GUI can always stop a daemon from another version.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::host::HostId;

/// Protocol version; bump on any incompatible change to non-frozen messages.
pub const PROTOCOL_VERSION: u32 = 11;

/// Daemon-assigned identifier of a pane (one PTY session).
pub type PaneId = u64;

/// The image formats `SavePasteImage` carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    /// PNG.
    Png,
    /// JPEG.
    Jpeg,
    /// GIF.
    Gif,
    /// WebP.
    Webp,
}

impl ImageFormat {
    /// The file extension for this format, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Gif => "gif",
            ImageFormat::Webp => "webp",
        }
    }
}

/// Serde adapter: bytes as one standard base64 string.
mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

/// What to run in a new pane. The daemon supplies the environment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnRequest {
    /// Program to run.
    pub program: String,
    /// Program arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Initial columns.
    pub cols: u16,
    /// Initial rows.
    pub rows: u16,
    /// Start in this live pane's current directory instead of `cwd`, when the daemon can tell.
    #[serde(default)]
    pub cwd_from: Option<PaneId>,
    /// Run it in the dev container serving this host folder (`None` = on this machine).
    #[serde(default)]
    pub container: Option<PathBuf>,
    /// The program is Claude Code (session tracking reads its arguments whatever the program is named).
    #[serde(default)]
    pub claude: bool,
    /// Extra environment variables for the child, on top of the daemon's base environment (host panes only).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Scrollback lines for this pane (`None` = the default, 10,000).
    #[serde(default)]
    pub scrollback: Option<u32>,
    /// The pane is a Claude pane with the preview tool on (`show_in_preview`).
    #[serde(default)]
    pub preview: bool,
}

/// A live pane as listed by the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    /// Pane id.
    pub pane: PaneId,
    /// Program the pane was started with.
    pub program: String,
    /// Arguments the program was started with.
    pub args: Vec<String>,
    /// Working directory it was started in.
    pub cwd: PathBuf,
    /// Focus sequence number; higher = focused more recently (0 = never).
    pub last_focused: u64,
    /// Host folder of the dev container it runs in (`None` = this machine).
    #[serde(default)]
    pub container: Option<PathBuf>,
    /// Whether it runs Claude Code.
    #[serde(default)]
    pub claude: bool,
}

/// Where a container session runs (ran).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerPlace {
    /// The dev container's host folder (its key for launching); `None` when Gantry never saw that container.
    pub host_folder: Option<PathBuf>,
    /// Its directory inside the container.
    pub path: PathBuf,
}

/// A dev container that can run sessions for a folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerChoice {
    /// The host folder it serves (its key for launching).
    pub host_folder: PathBuf,
    /// Container name.
    pub name: String,
    /// Whether it runs (a stopped one is started on launch).
    pub running: bool,
}

/// Where a Claude session runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionState {
    /// In this daemon's pane.
    Managed {
        /// That pane.
        pane: PaneId,
    },
    /// Outside Gantry: another terminal on this host (`pid`), or a container (`pid: None`).
    External {
        /// Host process id, when it runs on this host.
        pid: Option<u32>,
    },
    /// Nothing runs it; it can be resumed.
    Closed,
}

/// What a live Claude session is doing (from its pid file).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionStatus {
    /// Working on a turn.
    Busy,
    /// Waiting for the next prompt.
    Idle,
    /// Blocked on the user (a permission prompt, a dialog, a question).
    NeedsInput {
        /// Claude's reason, e.g. `input needed`, `dialog open`.
        reason: Option<String>,
    },
}

/// How a `TakeOver` ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TakeOverOutcome {
    /// The process is gone; the session can be resumed.
    Exited,
    /// It was asked to stop but still runs after the grace period (offer Force quit).
    StillRunning,
    /// Nothing was signalled (it cannot or must not be stopped), or a forced stop failed.
    Failed {
        /// Human-readable reason.
        error: String,
    },
}

/// A file Claude sent to the preview with `show_in_preview`, read from the session's transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentFile {
    /// `"<tool_use_id>:<index in the call>"`.
    pub id: String,
    /// The file's path as Claude gave it.
    pub path: PathBuf,
    /// When Claude sent it, ms since the Unix epoch.
    pub sent_ms: u64,
    /// Claude's caption, when it gave one.
    pub caption: Option<String>,
}

/// One Claude session, from the daemon's index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Session id (`claude --resume <id>`).
    pub id: String,
    /// Directory it runs (ran) in.
    pub cwd: PathBuf,
    /// Title; empty while a live session has no transcript yet.
    pub title: String,
    /// Last activity, ms since the Unix epoch.
    pub last_active_ms: u64,
    /// Where it runs.
    pub state: SessionState,
    /// Whether `cwd` still exists.
    pub cwd_exists: bool,
    /// What it is doing; `None` when closed, or when Claude has not said.
    pub status: Option<SessionStatus>,
    /// When `status` was last written, ms since the Unix epoch (0 = unknown).
    pub status_since_ms: u64,
    /// Where it runs (ran) when it is a dev container session; `cwd` is then its directory on this host when known.
    #[serde(default)]
    pub container: Option<ContainerPlace>,
    /// The Claude config dir it was found in (`None` from an older daemon).
    #[serde(default)]
    pub claude_dir: Option<PathBuf>,
    /// Your newest prompt in this session, on one line; `None` when unknown (or from an older daemon).
    #[serde(default)]
    pub last_prompt: Option<String>,
    /// Files Claude sent to the preview in this session, oldest first.
    #[serde(default)]
    pub sent_files: Vec<SentFile>,
    /// The host whose daemon reported it. The GUI sets this when the session arrives; it never travels on the wire.
    #[serde(skip)]
    pub host: HostId,
}

/// The session a Claude command line names: the value after `--session-id`, `--resume` or `-r`. With `claude`
/// set the arguments are read whatever the program is called; otherwise only when `program` is named `claude`
/// (layouts and clients from before the flag).
pub fn claude_session<'a>(claude: bool, program: &str, args: &'a [String]) -> Option<&'a str> {
    if !claude && std::path::Path::new(program).file_stem().is_none_or(|stem| stem != "claude") {
        return None;
    }
    args.windows(2)
        .find(|pair| matches!(pair[0].as_str(), "--session-id" | "--resume" | "-r"))
        .map(|pair| pair[1].as_str())
}

/// Terminal colors the GUI draws with, 0xRRGGBB (the daemon answers color queries from them).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaletteColors {
    /// Default text color.
    pub foreground: u32,
    /// Default background color.
    pub background: u32,
    /// Cursor color.
    pub cursor: u32,
    /// Background of selected cells.
    pub selection: u32,
    /// The 16 ANSI colors: 0-7 normal, 8-15 bright.
    pub ansi: [u32; 16],
}

/// The result of a `ReadFile`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileBody {
    /// The file's bytes (a base64 string on the wire) and modification time.
    Bytes {
        /// The file's contents.
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        /// Modification time, ms since the Unix epoch.
        mtime_ms: u64,
    },
    /// The file still has the `known_mtime_ms` the client sent.
    Unchanged,
    /// The file is larger than the daemon sends.
    TooLarge {
        /// Its size in bytes.
        size: u64,
    },
    /// No such file.
    Missing,
    /// It could not be read.
    Failed {
        /// Why, readable.
        reason: String,
    },
}

/// GUI → daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// First message of every connection (frozen).
    Hello {
        /// Client protocol version.
        protocol: u32,
    },
    /// Ask for `Panes`.
    ListPanes,
    /// The Claude config dirs to index; an empty list means the daemon's own default dir.
    SetClaudeDirs {
        /// Directories, already expanded.
        dirs: Vec<PathBuf>,
    },
    /// Start a new pane; the client is attached to it automatically.
    Spawn {
        /// Client-chosen id echoed in the reply.
        request: u64,
        /// What to run.
        spec: SpawnRequest,
    },
    /// Start receiving a pane's output; the daemon first sends `Resync` + a replay.
    Attach {
        /// Pane to attach to.
        pane: PaneId,
    },
    /// The pane's visible grid changed size.
    Resize {
        /// Pane.
        pane: PaneId,
        /// Columns.
        cols: u16,
        /// Rows.
        rows: u16,
        /// Cell width in pixels (for size queries).
        cell_width: u16,
        /// Cell height in pixels (for size queries).
        cell_height: u16,
    },
    /// The terminal colors the GUI uses now; the daemon answers color queries from them.
    SetPalette(PaletteColors),
    /// The user focused this pane.
    Focus {
        /// Pane.
        pane: PaneId,
    },
    /// Terminate this pane's process.
    Kill {
        /// Pane.
        pane: PaneId,
    },
    /// Stop the process that runs `session` outside Gantry (SIGTERM, or SIGKILL when `force`), after verifying it.
    TakeOver {
        /// Session id.
        session: String,
        /// Kill immediately instead of asking it to stop.
        force: bool,
    },
    /// Which dev container (if any) mounts `dir`; answered by `ContainerFound`.
    FindContainer {
        /// Client-chosen id echoed in the reply.
        request: u64,
        /// Directory a new session would start in.
        dir: PathBuf,
    },
    /// Search the conversations' text; answered by `SearchResults` (or `SearchUnavailable`).
    SearchConversations {
        /// Client-chosen id echoed in the reply.
        request: u64,
        /// What the user typed.
        query: String,
        /// Most sessions to return.
        limit: u32,
    },
    /// List the folders in `path` (absolute, or starting with `~`); answered by `DirListing` or `DirListingFailed`.
    ListDir {
        /// Client-chosen id echoed in the reply.
        request: u64,
        /// The folder to list.
        path: String,
    },
    /// The daemon also watches these sessions' transcripts for sent files; each message replaces the set.
    WatchSentFiles {
        /// Session ids to watch.
        sessions: Vec<String>,
    },
    /// Read a file on the daemon's host; answered by `FileContents`.
    ReadFile {
        /// Client-chosen id echoed in the reply.
        request: u64,
        /// Absolute path, or starting with `~`.
        path: String,
        /// The modification time the client already has; the reply is `Unchanged` when it still matches.
        known_mtime_ms: Option<u64>,
    },
    /// Save a pasted image where `pane`'s Claude can read it; answered by `PasteImageSaved` or `PasteImageFailed`.
    /// Only sent to a daemon whose `Welcome.paste_image` is true (no protocol bump: older daemons never see it).
    SavePasteImage {
        /// The pane the image is pasted into.
        pane: PaneId,
        /// The image's format.
        format: ImageFormat,
        /// The image file's bytes (a base64 string on the wire).
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    /// Git state of the repo containing `folder` (absolute, or starting with `~`); answered by `GitStatus` or
    /// `GitUnavailable`.
    GitStatus {
        /// Client-chosen id echoed in the reply.
        request: u64,
        /// A folder inside the repo.
        folder: String,
        /// The `HEAD` the client already has the commits for; the reply skips them when it is unchanged.
        known_head: Option<String>,
    },
    /// One file's diff against `HEAD` (staged and unstaged together); answered by `GitDiff` or `GitUnavailable`.
    GitDiff {
        /// Client-chosen id echoed in the reply.
        request: u64,
        /// A folder inside the repo.
        folder: String,
        /// The file, relative to the repo root.
        path: String,
        /// Where a renamed file came from.
        old_path: Option<String>,
        /// Lift the "too large" limit (the line and byte caps stay, raised).
        full: bool,
        /// Diff the file as that commit changed it (against its first parent) instead of against `HEAD`.
        #[serde(default)]
        commit: Option<String>,
    },
    /// One commit's message and changed files; answered by `GitCommit` or `GitUnavailable`.
    GitCommit {
        /// Client-chosen id echoed in the reply.
        request: u64,
        /// A folder inside the repo.
        folder: String,
        /// The commit (a hash as listed in `GitState.commits`).
        hash: String,
    },
    /// Terminate every pane and stop the daemon (frozen).
    StopAll,
}

/// One folder in a `DirListing`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    /// The folder's name (no path).
    pub name: String,
    /// It is a symlink to a folder.
    pub symlink: bool,
}

/// How a file differs from `HEAD` in a `GitState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitFileStatus {
    /// Content changed.
    Modified,
    /// New and tracked (staged).
    Added,
    /// Removed.
    Deleted,
    /// Moved; `GitFile.old_path` is where from.
    Renamed,
    /// Not tracked.
    Untracked,
    /// Unmerged paths of a conflict.
    Conflicted,
}

/// One changed file in a `GitState`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFile {
    /// Path relative to the repo root.
    pub path: String,
    /// The old path of a rename.
    pub old_path: Option<String>,
    /// How it changed.
    pub status: GitFileStatus,
    /// Lines added; `None` for an untracked or binary file.
    pub added: Option<u32>,
    /// Lines removed; `None` for an untracked or binary file.
    pub removed: Option<u32>,
}

/// One commit in a `GitState`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCommit {
    /// Abbreviated hash (7 characters).
    pub hash: String,
    /// First line of the message.
    pub subject: String,
    /// Author name.
    pub author: String,
    /// Commit time, seconds since the Unix epoch.
    pub time: i64,
    /// Not on the upstream yet (every commit when there is no upstream).
    pub unpushed: bool,
}

/// One commit in full, in a `GitCommit` answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCommitDetail {
    /// The full hash.
    pub hash: String,
    /// The whole message, subject and body.
    pub message: String,
    /// Author name.
    pub author: String,
    /// Commit time, seconds since the Unix epoch.
    pub time: i64,
    /// Abbreviated hashes of the parents (two or more for a merge).
    pub parents: Vec<String>,
    /// Files the commit changed (against its first parent), at most 2,000.
    pub files: Vec<GitFile>,
    /// Changed files beyond `files`.
    pub more: u32,
}

/// What `git status` found for one repo.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitState {
    /// The repo's top folder.
    pub root: String,
    /// The branch; `None` on a detached HEAD.
    pub branch: Option<String>,
    /// Abbreviated hash of `HEAD`; `None` before the first commit.
    pub head: Option<String>,
    /// The upstream branch, if any.
    pub upstream: Option<String>,
    /// Commits ahead of the upstream.
    pub ahead: u32,
    /// Commits behind the upstream.
    pub behind: u32,
    /// Changed files, at most 2,000.
    pub files: Vec<GitFile>,
    /// Changed files beyond `files`.
    pub more: u32,
    /// The latest commits; `None` when `known_head` in the request still is `HEAD`.
    pub commits: Option<Vec<GitCommit>>,
    /// How long the daemon's git calls took, in milliseconds.
    pub took_ms: u32,
    /// Time of the latest commit, seconds since the Unix epoch.
    pub last_commit: Option<i64>,
    /// The repo configures git filters (nbstripout, git-lfs, ...); they are never run, so `files` was read with them skipped.
    #[serde(default)]
    pub filters_skipped: bool,
    /// `commits` are the ones this branch has beyond the default branch; false = the latest commits of `HEAD`.
    #[serde(default)]
    pub own_commits: bool,
}

/// Why a git request has no answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitProblem {
    /// The folder is not inside a git repo.
    NotRepo,
    /// `git` is not installed on that host.
    NoGit,
    /// Anything else.
    Failed {
        /// Git's first line of complaint, readable.
        reason: String,
    },
}

/// A file's diff in a `GitDiff` answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitDiffBody {
    /// Unified diff text.
    Text {
        /// The diff.
        text: String,
        /// Cut at the size caps.
        truncated: bool,
    },
    /// A binary file.
    Binary,
    /// A submodule pointer changed.
    Submodule,
    /// Bigger than the cap; ask again with `full`.
    TooLarge {
        /// The file's size in bytes.
        bytes: u64,
    },
}

/// Who wrote the message a search hit shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    /// The user.
    You,
    /// Claude.
    Claude,
}

/// A session whose conversation matches a search, shown by its best-matching message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// Session id.
    pub session_id: String,
    /// Claude config dir the transcript is in.
    pub claude_dir: PathBuf,
    /// Who wrote the matching message.
    pub speaker: Speaker,
    /// About 160 characters of it around the match, on one line.
    pub snippet: String,
    /// Byte ranges of `snippet` that matched (start inclusive, end exclusive), in order.
    pub highlights: Vec<(u32, u32)>,
    /// The host whose daemon found it. The GUI sets this; it never travels on the wire.
    #[serde(skip)]
    pub host: HostId,
}

/// Daemon → GUI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMessage {
    /// Handshake accepted.
    Welcome {
        /// Daemon protocol version.
        protocol: u32,
        /// Identifies this daemon process; pane ids are only meaningful within one instance.
        instance: String,
        /// The daemon's default Claude config dir.
        #[serde(default)]
        claude_dir: Option<PathBuf>,
        /// The daemon's app version (`protocol::build::VERSION`); older daemons send none.
        #[serde(default)]
        app_version: Option<String>,
        /// The daemon's git commit (`protocol::build::COMMIT`); older daemons send none.
        #[serde(default)]
        app_commit: Option<String>,
        /// The daemon answers `SavePasteImage`; older daemons send none.
        #[serde(default)]
        paste_image: bool,
    },
    /// Handshake rejected; only `StopAll` is accepted afterwards.
    VersionMismatch {
        /// Daemon protocol version.
        daemon_protocol: u32,
    },
    /// Live panes, most recently focused first.
    Panes {
        /// Panes.
        panes: Vec<PaneInfo>,
    },
    /// A `Spawn` succeeded.
    Spawned {
        /// Echoed request id.
        request: u64,
        /// New pane.
        pane: PaneId,
        /// Directory the program started in.
        cwd: PathBuf,
    },
    /// A `Spawn` failed.
    SpawnFailed {
        /// Echoed request id.
        request: u64,
        /// Human-readable reason.
        error: String,
    },
    /// Reset this pane's terminal to `cols`×`rows`; the next data frame for it is a full replay.
    Resync {
        /// Pane.
        pane: PaneId,
        /// Columns of the replayed screen.
        cols: u16,
        /// Rows of the replayed screen.
        rows: u16,
    },
    /// The pane's process exited; the pane is gone.
    Exited {
        /// Pane.
        pane: PaneId,
        /// Exit code.
        code: u32,
    },
    /// Every known Claude session, replacing any earlier list (sent after `Panes`).
    Sessions {
        /// Sessions, most recently active first.
        sessions: Vec<SessionInfo>,
    },
    /// Sessions that appeared or changed, and ids that disappeared, since the last list.
    SessionsChanged {
        /// New or changed sessions.
        upserted: Vec<SessionInfo>,
        /// Ids no longer known.
        removed: Vec<String>,
    },
    /// Every dir of the last `SetClaudeDirs` has been scanned and its sessions sent: the list is complete for them.
    /// Not sent by older daemons.
    ClaudeDirsIndexed,
    /// How a `TakeOver` request ended.
    TakeOverResult {
        /// Session id from the request.
        session: String,
        /// What happened.
        outcome: TakeOverOutcome,
    },
    /// The answer to `FindContainer`.
    ContainerFound {
        /// Echoed request id.
        request: u64,
        /// The container that mounts the directory, if any.
        container: Option<ContainerChoice>,
    },
    /// The answer to `SearchConversations`: one hit per session, best first.
    SearchResults {
        /// Echoed request id.
        request: u64,
        /// Matching sessions.
        hits: Vec<SearchHit>,
    },
    /// The conversation index is being built: `done` of `total` files (sent about once a second, and once with
    /// `done == total` at the end).
    SearchIndexing {
        /// Files indexed so far.
        done: u64,
        /// Files in this pass.
        total: u64,
    },
    /// Conversation search does not work in this daemon run.
    SearchUnavailable {
        /// Why (also in the daemon log).
        reason: String,
    },
    /// The answer to `ListDir`: the folders in the expanded path.
    DirListing {
        /// Echoed request id.
        request: u64,
        /// The expanded absolute path.
        path: String,
        /// Its folders, sorted, at most 500.
        dirs: Vec<DirEntry>,
    },
    /// `ListDir` failed.
    DirListingFailed {
        /// Echoed request id.
        request: u64,
        /// Why, readable.
        reason: String,
    },
    /// The answer to `ReadFile`.
    FileContents {
        /// Echoed request id.
        request: u64,
        /// What was found.
        body: FileBody,
    },
    /// The answer to `SavePasteImage`: the image is saved at `path`, as the pane's Claude sees it.
    PasteImageSaved {
        /// The pane that asked.
        pane: PaneId,
        /// Absolute path of the saved file.
        path: String,
    },
    /// `SavePasteImage` failed.
    PasteImageFailed {
        /// The pane that asked.
        pane: PaneId,
        /// Why, readable.
        reason: String,
    },
    /// The answer to `GitStatus`.
    GitStatus {
        /// Echoed request id.
        request: u64,
        /// The repo's state.
        state: GitState,
    },
    /// The answer to `GitDiff`.
    GitDiff {
        /// Echoed request id.
        request: u64,
        /// The file asked for.
        path: String,
        /// Its diff.
        body: GitDiffBody,
    },
    /// The answer to `GitCommit`.
    GitCommit {
        /// Echoed request id.
        request: u64,
        /// The commit.
        detail: GitCommitDetail,
    },
    /// A git request could not be answered.
    GitUnavailable {
        /// Echoed request id.
        request: u64,
        /// Why.
        problem: GitProblem,
    },
    /// Another GUI connected; this connection is closed next.
    TakenOver,
    /// A request could not be handled.
    Error {
        /// Human-readable reason.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_messages_keep_their_wire_shape() {
        assert_eq!(
            serde_json::to_string(&ClientMessage::Hello { protocol: 7 }).unwrap(),
            r#"{"type":"hello","protocol":7}"#
        );
        assert_eq!(serde_json::to_string(&ClientMessage::StopAll).unwrap(), r#"{"type":"stop_all"}"#);
    }

    #[test]
    fn a_sessions_host_is_not_on_the_wire() {
        let info = SessionInfo {
            id: "s".into(),
            cwd: "/r".into(),
            title: String::new(),
            last_active_ms: 0,
            state: SessionState::Closed,
            cwd_exists: true,
            status: None,
            status_since_ms: 0,
            container: None,
            claude_dir: None,
            last_prompt: None,
            sent_files: Vec::new(),
            host: HostId::named("devbox"),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("devbox") && !json.contains("host"), "{json}");
        assert!(serde_json::from_str::<SessionInfo>(&json).unwrap().host.is_local());
    }

    #[test]
    fn messages_round_trip() {
        let msg = ClientMessage::Spawn {
            request: 3,
            spec: SpawnRequest {
                program: "claude".into(),
                args: vec!["--resume".into()],
                cwd: "/tmp".into(),
                cols: 80,
                rows: 24,
                cwd_from: Some(4),
                container: None,
                claude: true,
                env: vec![("A".into(), "b".into())],
                scrollback: Some(5000),
                preview: false,
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), msg);
        for reply in [
            DaemonMessage::Resync { pane: 9, cols: 100, rows: 30 },
            DaemonMessage::Welcome {
                protocol: PROTOCOL_VERSION,
                instance: "123-456".into(),
                claude_dir: Some("/home/u/.claude".into()),
                app_version: Some("0.3.0".into()),
                app_commit: Some("a1b2c3d".into()),
                paste_image: true,
            },
            DaemonMessage::Welcome {
                protocol: PROTOCOL_VERSION,
                instance: "1".into(),
                claude_dir: None,
                app_version: None,
                app_commit: None,
                paste_image: false,
            },
            DaemonMessage::Spawned { request: 3, pane: 9, cwd: "/srv".into() },
        ] {
            let json = serde_json::to_string(&reply).unwrap();
            assert_eq!(serde_json::from_str::<DaemonMessage>(&json).unwrap(), reply);
        }
    }

    #[test]
    fn set_palette_round_trips() {
        let msg = ClientMessage::SetPalette(PaletteColors {
            foreground: 0xd8dde8,
            background: 0x141821,
            cursor: 0x9ecbff,
            selection: 0x2f4a6e,
            ansi: [7; 16],
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.starts_with(r#"{"type":"set_palette","#), "{json}");
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), msg);
    }

    #[test]
    fn cwd_from_is_optional_on_the_wire() {
        let json = r#"{"program":"sh","args":[],"cwd":"/","cols":80,"rows":24}"#;
        assert_eq!(serde_json::from_str::<SpawnRequest>(json).unwrap().cwd_from, None);
    }

    #[test]
    fn session_messages_round_trip_with_a_tagged_state_and_status() {
        let session = SessionInfo {
            id: "9835b878".into(),
            cwd: "/home/me/repo".into(),
            title: "Fix the build".into(),
            last_active_ms: 1_790_840_985_757,
            state: SessionState::Managed { pane: 3 },
            cwd_exists: true,
            status: Some(SessionStatus::NeedsInput { reason: Some("dialog open".into()) }),
            status_since_ms: 1_790_840_985_000,
            container: None,
            claude_dir: Some("/home/me/.claude-work".into()),
            last_prompt: Some("Fix the build".into()),
            sent_files: Vec::new(),
            host: HostId::local(),
        };
        let json = serde_json::to_string(&session).unwrap();
        assert!(json.contains(r#""state":{"kind":"managed","pane":3}"#), "{json}");
        assert!(json.contains(r#""status":{"kind":"needs_input","reason":"dialog open"}"#), "{json}");
        for message in [
            DaemonMessage::Sessions { sessions: vec![session.clone()] },
            DaemonMessage::SessionsChanged {
                upserted: vec![SessionInfo {
                    state: SessionState::External { pid: None },
                    status: None,
                    ..session.clone()
                }],
                removed: vec!["gone".into()],
            },
        ] {
            let json = serde_json::to_string(&message).unwrap();
            assert_eq!(serde_json::from_str::<DaemonMessage>(&json).unwrap(), message);
        }
    }

    #[test]
    fn last_prompt_is_optional_on_the_wire() {
        let session = SessionInfo {
            id: "s".into(),
            cwd: "/r".into(),
            title: "t".into(),
            last_active_ms: 1,
            state: SessionState::Closed,
            cwd_exists: true,
            status: None,
            status_since_ms: 0,
            container: None,
            claude_dir: None,
            last_prompt: Some("x".into()),
            sent_files: Vec::new(),
            host: HostId::local(),
        };
        let mut json = serde_json::to_value(&session).unwrap();
        json.as_object_mut().unwrap().remove("last_prompt");
        assert_eq!(serde_json::from_value::<SessionInfo>(json).unwrap().last_prompt, None);
    }

    #[test]
    fn take_over_messages_round_trip() {
        let request = ClientMessage::TakeOver { session: "s".into(), force: true };
        assert_eq!(serde_json::to_string(&request).unwrap(), r#"{"type":"take_over","session":"s","force":true}"#);
        for outcome in
            [TakeOverOutcome::Exited, TakeOverOutcome::StillRunning, TakeOverOutcome::Failed { error: "no".into() }]
        {
            let reply = DaemonMessage::TakeOverResult { session: "s".into(), outcome };
            let json = serde_json::to_string(&reply).unwrap();
            assert_eq!(serde_json::from_str::<DaemonMessage>(&json).unwrap(), reply);
        }
    }

    #[test]
    fn claude_session_reads_the_session_flags_of_claude_only() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(claude_session(false, "claude", &args(&["--session-id", "abc"])), Some("abc"));
        assert_eq!(claude_session(false, "/usr/bin/claude", &args(&["--model", "x", "--resume", "def"])), Some("def"));
        assert_eq!(claude_session(false, "claude", &args(&["-r", "ghi"])), Some("ghi"));
        assert_eq!(claude_session(false, "claude", &args(&["--continue"])), None);
        assert_eq!(claude_session(false, "sh", &args(&["--session-id", "abc"])), None);
        assert_eq!(claude_session(false, "claude", &args(&["--resume"])), None);
    }

    #[test]
    fn claude_session_with_the_flag_ignores_the_program_name() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(
            claude_session(true, "/opt/wrap/claude-work", &args(&["--model", "x", "--session-id", "abc"])),
            Some("abc")
        );
        assert_eq!(claude_session(true, "sh", &args(&["--continue"])), None);
        assert_eq!(claude_session(false, "/opt/wrap/claude-work", &args(&["--session-id", "abc"])), None);
    }

    #[test]
    fn welcome_without_app_version_decodes() {
        let old = r#"{"type":"welcome","protocol":7,"instance":"x"}"#;
        assert_eq!(
            serde_json::from_str::<DaemonMessage>(old).unwrap(),
            DaemonMessage::Welcome {
                protocol: 7,
                instance: "x".into(),
                claude_dir: None,
                app_version: None,
                app_commit: None,
                paste_image: false
            }
        );
        let new = DaemonMessage::Welcome {
            protocol: 7,
            instance: "x".into(),
            claude_dir: None,
            app_version: Some("0.3.0".into()),
            app_commit: Some("a1b2c3d".into()),
            paste_image: true,
        };
        let json = serde_json::to_string(&new).unwrap();
        assert_eq!(serde_json::from_str::<DaemonMessage>(&json).unwrap(), new);
    }

    #[test]
    fn set_claude_dirs_and_phase2_fields_round_trip() {
        let msg = ClientMessage::SetClaudeDirs { dirs: vec!["/a".into(), "/b".into()] };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"set_claude_dirs","dirs":["/a","/b"]}"#);
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), msg);
        let old_welcome = r#"{"type":"welcome","protocol":5,"instance":"x"}"#;
        assert_eq!(
            serde_json::from_str::<DaemonMessage>(old_welcome).unwrap(),
            DaemonMessage::Welcome {
                protocol: 5,
                instance: "x".into(),
                claude_dir: None,
                app_version: None,
                app_commit: None,
                paste_image: false
            }
        );
        let spawn = r#"{"program":"sh","args":[],"cwd":"/","cols":80,"rows":24}"#;
        let spec = serde_json::from_str::<SpawnRequest>(spawn).unwrap();
        assert_eq!((spec.claude, spec.env.len(), spec.scrollback), (false, 0, None));
        let pane = r#"{"pane":1,"program":"sh","args":[],"cwd":"/","last_focused":0}"#;
        assert!(!serde_json::from_str::<PaneInfo>(pane).unwrap().claude);
        assert_eq!(PROTOCOL_VERSION, 11);
    }

    #[test]
    fn protocol_is_eleven() {
        assert_eq!(PROTOCOL_VERSION, 11);
    }

    #[test]
    fn session_without_sent_files_parses() {
        let json = r#"{"id":"a","cwd":"/","title":"t","last_active_ms":1,"state":{"kind":"closed"},"cwd_exists":true,"status":null,"status_since_ms":0}"#;
        assert_eq!(serde_json::from_str::<SessionInfo>(json).unwrap().sent_files, vec![]);
    }

    #[test]
    fn spawn_request_without_preview_is_false() {
        let spawn = r#"{"program":"sh","args":[],"cwd":"/","cols":80,"rows":24}"#;
        assert!(!serde_json::from_str::<SpawnRequest>(spawn).unwrap().preview);
    }

    #[test]
    fn read_file_round_trips() {
        let msg = ClientMessage::ReadFile { request: 3, path: "~/a b/ü.md".into(), known_mtime_ms: Some(5) };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), msg);
        for body in [
            FileBody::Bytes { data: vec![0, 255, 10], mtime_ms: 9 },
            FileBody::Unchanged,
            FileBody::TooLarge { size: 7 },
            FileBody::Missing,
            FileBody::Failed { reason: "denied".into() },
        ] {
            let reply = DaemonMessage::FileContents { request: 3, body };
            let json = serde_json::to_string(&reply).unwrap();
            assert_eq!(serde_json::from_str::<DaemonMessage>(&json).unwrap(), reply);
        }
    }

    #[test]
    fn watch_sent_files_round_trips() {
        let msg = ClientMessage::WatchSentFiles { sessions: vec!["a".into(), "b".into()] };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), msg);
    }

    #[test]
    fn save_paste_image_round_trips() {
        let msg = ClientMessage::SavePasteImage { pane: 4, format: ImageFormat::Png, data: vec![0x89, b'P', 0, 255] };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""data":"iVAA/w==""#), "{json}");
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), msg);
    }

    #[test]
    fn paste_image_answers_round_trip() {
        for reply in [
            DaemonMessage::PasteImageSaved { pane: 4, path: "/run/user/1000/gantry-dev/paste/a.png".into() },
            DaemonMessage::PasteImageFailed { pane: 4, reason: "no such pane".into() },
        ] {
            let json = serde_json::to_string(&reply).unwrap();
            assert_eq!(serde_json::from_str::<DaemonMessage>(&json).unwrap(), reply);
        }
    }

    #[test]
    fn welcome_without_paste_image_is_false() {
        let old = r#"{"type":"welcome","protocol":10,"instance":"x"}"#;
        match serde_json::from_str::<DaemonMessage>(old).unwrap() {
            DaemonMessage::Welcome { paste_image, .. } => assert!(!paste_image),
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    #[test]
    fn image_format_extensions() {
        let all = [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Gif, ImageFormat::Webp];
        assert_eq!(all.map(ImageFormat::extension), ["png", "jpg", "gif", "webp"]);
    }

    #[test]
    fn list_dir_messages_round_trip() {
        let request = ClientMessage::ListDir { request: 7, path: "~/projects".into() };
        assert_eq!(serde_json::from_str::<ClientMessage>(&serde_json::to_string(&request).unwrap()).unwrap(), request);
        let replies = [
            DaemonMessage::DirListing {
                request: 7,
                path: "/home/me/projects".into(),
                dirs: vec![
                    DirEntry { name: "app".into(), symlink: false },
                    DirEntry { name: "link".into(), symlink: true },
                ],
            },
            DaemonMessage::DirListingFailed { request: 7, reason: "No such file or directory".into() },
        ];
        for reply in replies {
            assert_eq!(serde_json::from_str::<DaemonMessage>(&serde_json::to_string(&reply).unwrap()).unwrap(), reply);
        }
    }

    #[test]
    fn git_messages_round_trip() {
        let requests = [
            ClientMessage::GitStatus { request: 3, folder: "~/work/app".into(), known_head: Some("abc1234".into()) },
            ClientMessage::GitDiff {
                request: 4,
                folder: "/home/me/app".into(),
                path: "src/new name.rs".into(),
                old_path: Some("src/old.rs".into()),
                full: true,
                commit: Some("abc1234".into()),
            },
            ClientMessage::GitCommit { request: 5, folder: "/home/me/app".into(), hash: "abc1234".into() },
        ];
        for request in requests {
            let json = serde_json::to_string(&request).unwrap();
            assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), request);
        }
        let state = GitState {
            root: "/home/me/app".into(),
            branch: Some("fix/x".into()),
            head: Some("abc1234".into()),
            upstream: Some("origin/fix/x".into()),
            ahead: 2,
            behind: 0,
            files: vec![GitFile {
                path: "src/new name.rs".into(),
                old_path: Some("src/old.rs".into()),
                status: GitFileStatus::Renamed,
                added: Some(3),
                removed: None,
            }],
            more: 0,
            commits: Some(vec![GitCommit {
                hash: "abc1234".into(),
                subject: "Add backoff".into(),
                author: "Me".into(),
                time: 1_760_000_000,
                unpushed: true,
            }]),
            took_ms: 31,
            last_commit: Some(1_760_000_000),
            filters_skipped: false,
            own_commits: false,
        };
        let replies = [
            DaemonMessage::GitStatus { request: 3, state },
            DaemonMessage::GitDiff {
                request: 4,
                path: "src/new name.rs".into(),
                body: GitDiffBody::Text { text: "@@ -1 +1 @@\n-a\n+b\n".into(), truncated: false },
            },
            DaemonMessage::GitDiff { request: 5, path: "x.png".into(), body: GitDiffBody::Binary },
            DaemonMessage::GitDiff { request: 6, path: "vendor".into(), body: GitDiffBody::Submodule },
            DaemonMessage::GitDiff {
                request: 7,
                path: "big.bin".into(),
                body: GitDiffBody::TooLarge { bytes: 9_999_999 },
            },
            DaemonMessage::GitCommit {
                request: 5,
                detail: GitCommitDetail {
                    hash: "abc1234def".into(),
                    message: "fix: a\n\nbody".into(),
                    author: "Ido".into(),
                    time: 1_760_000_000,
                    parents: vec!["1111111".into()],
                    files: vec![GitFile {
                        path: "a.rs".into(),
                        old_path: None,
                        status: GitFileStatus::Modified,
                        added: Some(1),
                        removed: None,
                    }],
                    more: 0,
                },
            },
            DaemonMessage::GitUnavailable { request: 8, problem: GitProblem::NotRepo },
            DaemonMessage::GitUnavailable { request: 9, problem: GitProblem::NoGit },
            DaemonMessage::GitUnavailable { request: 10, problem: GitProblem::Failed { reason: "boom".into() } },
        ];
        for reply in replies {
            let json = serde_json::to_string(&reply).unwrap();
            assert_eq!(serde_json::from_str::<DaemonMessage>(&json).unwrap(), reply);
        }
    }

    #[test]
    fn search_messages_round_trip() {
        let request = ClientMessage::SearchConversations { request: 4, query: "font \"fall back\"".into(), limit: 200 };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.starts_with(r#"{"type":"search_conversations","#), "{json}");
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), request);
        let hit = SearchHit {
            session_id: "s1".into(),
            claude_dir: "/home/me/.claude".into(),
            speaker: Speaker::Claude,
            snippet: "the font fallback".into(),
            highlights: vec![(4, 8), (9, 17)],
            host: HostId::local(),
        };
        for reply in [
            DaemonMessage::SearchResults { request: 4, hits: vec![hit] },
            DaemonMessage::SearchIndexing { done: 10, total: 40 },
            DaemonMessage::SearchUnavailable { reason: "disk".into() },
        ] {
            let json = serde_json::to_string(&reply).unwrap();
            assert_eq!(serde_json::from_str::<DaemonMessage>(&json).unwrap(), reply);
        }
        assert!(serde_json::to_string(&Speaker::You).unwrap() == r#""you""#);
    }

    #[test]
    fn messages_from_before_containers_still_parse() {
        let spawn = r#"{"program":"sh","args":[],"cwd":"/","cols":80,"rows":24}"#;
        assert_eq!(serde_json::from_str::<SpawnRequest>(spawn).unwrap().container, None);
        let info = r#"{"id":"s","cwd":"/","title":"","last_active_ms":0,"state":{"kind":"closed"},"cwd_exists":true,"status":null,"status_since_ms":0}"#;
        assert_eq!(serde_json::from_str::<SessionInfo>(info).unwrap().container, None);
    }
}
