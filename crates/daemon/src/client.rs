//! GUI side of the daemon connection. Runs its own thread and tokio runtime; the GUI talks to it
//! through non-blocking methods and a bounded event channel.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use interprocess::local_socket::tokio::{Stream, prelude::*};
use protocol::frame::{Frame, read_frame, write_frame};
use protocol::message::{
    ClientMessage, ContainerChoice, DaemonMessage, DirEntry, ImageFormat, PROTOCOL_VERSION, PaletteColors, PaneId,
    PaneInfo, SearchHit, SessionInfo, SpawnRequest, TakeOverOutcome,
};
use tokio::sync::mpsc;

use crate::launch::{connect_or_spawn, connect_trusted};
use crate::paths::Paths;

/// Events delivered to the GUI, in daemon order.
const EVENT_QUEUE: usize = 64;

/// Something the daemon told the GUI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientEvent {
    /// Program output (or, right after `Resync`, the replay).
    Output {
        /// Pane.
        pane: PaneId,
        /// Bytes.
        bytes: Vec<u8>,
    },
    /// Reset the pane's terminal to this size before feeding the next output.
    Resync {
        /// Pane.
        pane: PaneId,
        /// Columns.
        cols: u16,
        /// Rows.
        rows: u16,
    },
    /// The pane's process exited.
    Exited {
        /// Pane.
        pane: PaneId,
        /// Exit code.
        code: u32,
    },
    /// A `spawn` succeeded; the client is attached to the new pane.
    Spawned {
        /// Echoed request id.
        request: u64,
        /// New pane.
        pane: PaneId,
        /// Directory it started in.
        cwd: PathBuf,
    },
    /// A `spawn` failed.
    SpawnFailed {
        /// Echoed request id.
        request: u64,
        /// Human-readable reason.
        error: String,
    },
    /// Every Claude session the daemon knows, replacing the previous list.
    Sessions {
        /// Sessions, most recently active first.
        sessions: Vec<SessionInfo>,
    },
    /// The daemon has sent the sessions of every dir given in the last `SetClaudeDirs`.
    ClaudeDirsIndexed,
    /// Sessions that appeared or changed, and ids that disappeared.
    SessionsChanged {
        /// New or changed sessions.
        upserted: Vec<SessionInfo>,
        /// Ids no longer known.
        removed: Vec<String>,
    },
    /// How a `take_over` request ended.
    TakeOverResult {
        /// Session id.
        session: String,
        /// What happened.
        outcome: TakeOverOutcome,
    },
    /// The answer to `find_container`.
    ContainerFound {
        /// Echoed request id.
        request: u64,
        /// The dev container that mounts the directory, if any.
        container: Option<ContainerChoice>,
    },
    /// The answer to `search`.
    SearchResults {
        /// Echoed request id.
        request: u64,
        /// One hit per session, best first.
        hits: Vec<SearchHit>,
    },
    /// The conversation index is being built.
    SearchIndexing {
        /// Files done.
        done: u64,
        /// Files in this pass.
        total: u64,
    },
    /// Conversation search does not work in this daemon run.
    SearchUnavailable {
        /// Why.
        reason: String,
    },
    /// The answer to `list_dir`.
    DirListing {
        /// Echoed request id.
        request: u64,
        /// The expanded absolute path.
        path: PathBuf,
        /// Its folders.
        dirs: Vec<DirEntry>,
    },
    /// `list_dir` failed.
    DirListingFailed {
        /// Echoed request id.
        request: u64,
        /// Why.
        reason: String,
    },
    /// The answer to `git_status`.
    GitStatus {
        /// Echoed request id.
        request: u64,
        /// The repo's state.
        state: protocol::message::GitState,
    },
    /// The answer to `git_diff`.
    GitDiff {
        /// Echoed request id.
        request: u64,
        /// The file asked for.
        path: String,
        /// Its diff.
        body: protocol::message::GitDiffBody,
    },
    /// The answer to `git_commit`.
    GitCommit {
        /// Echoed request id.
        request: u64,
        /// The commit.
        detail: protocol::message::GitCommitDetail,
    },
    /// A git request could not be answered.
    GitUnavailable {
        /// Echoed request id.
        request: u64,
        /// Why.
        problem: protocol::message::GitProblem,
    },
    /// The answer to `save_paste_image`: the image is at `path`, as the pane's Claude sees it.
    PasteImageSaved {
        /// The pane that asked.
        pane: PaneId,
        /// Absolute path of the saved file.
        path: String,
    },
    /// `save_paste_image` failed.
    PasteImageFailed {
        /// The pane that asked.
        pane: PaneId,
        /// Why.
        reason: String,
    },
    /// Another GUI took over the daemon.
    TakenOver,
    /// The connection to the daemon was lost.
    Disconnected,
}

/// Why the client could not start.
#[derive(Debug)]
pub enum StartError {
    /// The running daemon speaks a different protocol version.
    VersionMismatch {
        /// Its version.
        daemon_protocol: u32,
    },
    /// Connection or protocol failure.
    Io(anyhow::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::VersionMismatch { daemon_protocol } => {
                write!(f, "the running daemon uses protocol {daemon_protocol}, this Gantry uses {PROTOCOL_VERSION}")
            }
            StartError::Io(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for StartError {}

pub(crate) enum Outgoing {
    Frame(Frame),
    Flush(std::sync::mpsc::Sender<()>),
}

/// Handle to the daemon connection; cheap to clone.
#[derive(Clone)]
pub struct DaemonClient {
    outgoing: mpsc::UnboundedSender<Outgoing>,
    /// Whether the daemon's `Welcome` said it saves pasted images (set once the handshake is done).
    paste_image: bool,
}

/// What the handshake returned.
pub(crate) struct Hello {
    pub(crate) instance: String,
    pub(crate) claude_dir: Option<PathBuf>,
    pub(crate) daemon_version: Option<String>,
    pub(crate) daemon_commit: Option<String>,
    pub(crate) paste_image: bool,
    pub(crate) panes: Vec<PaneInfo>,
}

/// A fresh daemon connection.
pub struct Connection {
    /// Handle for sending requests.
    pub client: DaemonClient,
    /// Daemon instance id (pane ids are only meaningful within it).
    pub instance: String,
    /// The daemon's default Claude config dir.
    pub claude_dir: Option<PathBuf>,
    /// The daemon's app version, if it sent one.
    pub daemon_version: Option<String>,
    /// The daemon's git commit, if it sent one.
    pub daemon_commit: Option<String>,
    /// Live panes at connect time, most recently focused first.
    pub panes: Vec<PaneInfo>,
    /// Daemon events, in order.
    pub events: async_channel::Receiver<ClientEvent>,
}

impl DaemonClient {
    /// Connect (starting the daemon via `spawn` if needed) and begin streaming events.
    pub fn connect(
        paths: &Paths,
        spawn: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> Result<Connection, StartError> {
        let paths = paths.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let Link { client, outgoing: outgoing_rx, events_tx, events } = Link::new();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = ready_tx.send(Err(StartError::Io(err.into())));
                    return;
                }
            };
            runtime.block_on(run_connection(paths, spawn, ready_tx, outgoing_rx, events_tx));
        });
        let hello = ready_rx.recv().map_err(|_| StartError::Io(anyhow::anyhow!("client thread ended")))??;
        Ok(connection(client, hello, events))
    }

    /// Start a pane; answered by `Spawned`/`SpawnFailed` with the same `request`.
    pub fn spawn(&self, request: u64, spec: SpawnRequest) {
        self.send(&ClientMessage::Spawn { request, spec });
    }

    /// Start receiving a live pane's output (a `Resync` and replay come first).
    pub fn attach(&self, pane: PaneId) {
        self.send(&ClientMessage::Attach { pane });
    }

    /// Terminate a pane's process.
    pub fn kill(&self, pane: PaneId) {
        self.send(&ClientMessage::Kill { pane });
    }

    /// Stop the process running `session` outside Gantry (SIGKILL when `force`); answered by `TakeOverResult`.
    pub fn take_over(&self, session: &str, force: bool) {
        self.send(&ClientMessage::TakeOver { session: session.into(), force });
    }

    /// Ask which dev container (if any) mounts `dir`; answered by `ContainerFound` with the same `request`.
    pub fn find_container(&self, request: u64, dir: &Path) {
        self.send(&ClientMessage::FindContainer { request, dir: dir.into() });
    }

    /// Search the conversations' text; answered by `SearchResults` with the same `request`.
    pub fn search(&self, request: u64, query: &str, limit: u32) {
        self.send(&ClientMessage::SearchConversations { request, query: query.into(), limit });
    }

    /// List the folders in `path` (absolute or starting with `~`); answered by `DirListing` or `DirListingFailed`.
    pub fn list_dir(&self, request: u64, path: &str) {
        self.send(&ClientMessage::ListDir { request, path: path.into() });
    }

    /// Ask for the git state of the repo containing `folder` (absolute or starting with `~`); answered by `GitStatus`
    /// or `GitUnavailable`. `known_head` is the `HEAD` whose commits the caller already has.
    pub fn git_status(&self, request: u64, folder: &str, known_head: Option<&str>) {
        self.send(&ClientMessage::GitStatus {
            request,
            folder: folder.into(),
            known_head: known_head.map(String::from),
        });
    }

    /// Ask for one file's diff against `HEAD` in the repo containing `folder`; answered by `GitDiff` or
    /// `GitUnavailable`. `old_path` is where a renamed file came from; `full` lifts the "too large" limit; `commit`
    /// asks for the file as that commit changed it instead.
    pub fn git_diff(
        &self,
        request: u64,
        folder: &str,
        path: &str,
        old_path: Option<&str>,
        full: bool,
        commit: Option<&str>,
    ) {
        self.send(&ClientMessage::GitDiff {
            request,
            folder: folder.into(),
            path: path.into(),
            old_path: old_path.map(String::from),
            full,
            commit: commit.map(String::from),
        });
    }

    /// Ask for commit `hash` of the repo containing `folder`; answered by `GitCommit` or `GitUnavailable`.
    pub fn git_commit(&self, request: u64, folder: &str, hash: &str) {
        self.send(&ClientMessage::GitCommit { request, folder: folder.into(), hash: hash.into() });
    }

    /// Whether this daemon saves pasted images (`save_paste_image`); false for a daemon from before the feature.
    pub fn paste_image(&self) -> bool {
        self.paste_image
    }

    /// Save an image where `pane`'s Claude can read it; answered by `PasteImageSaved` or `PasteImageFailed`. Only for
    /// a daemon whose `paste_image` is true.
    pub fn save_paste_image(&self, pane: PaneId, format: ImageFormat, data: Vec<u8>) {
        self.send(&ClientMessage::SavePasteImage { pane, format, data });
    }

    /// Send keyboard/paste input to a pane.
    pub fn input(&self, pane: PaneId, bytes: &[u8]) {
        let _ = self.outgoing.send(Outgoing::Frame(Frame::Data { pane, bytes: bytes.to_vec() }));
    }

    /// Report the pane's new grid and cell size.
    pub fn resize(&self, pane: PaneId, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        self.send(&ClientMessage::Resize { pane, cols, rows, cell_width, cell_height });
    }

    /// Tell the daemon the terminal colors in use, so it answers color queries with them.
    pub fn set_palette(&self, colors: PaletteColors) {
        self.send(&ClientMessage::SetPalette(colors));
    }

    /// Tell the daemon which Claude config dirs to index (empty = its own default dir).
    pub fn set_claude_dirs(&self, dirs: Vec<PathBuf>) {
        self.send(&ClientMessage::SetClaudeDirs { dirs });
    }

    /// Ask the daemon to watch these sessions' transcripts for files sent to the preview, replacing the earlier list;
    /// the files arrive in `SessionInfo.sent_files`.
    pub fn watch_sent_files(&self, sessions: &[String]) {
        self.send(&ClientMessage::WatchSentFiles { sessions: sessions.to_vec() });
    }

    /// Mark the pane as most recently focused.
    pub fn focus(&self, pane: PaneId) {
        self.send(&ClientMessage::Focus { pane });
    }

    /// Ask the daemon to stop everything; returns once the request is written (or after `timeout`).
    pub fn stop_all_and_wait(&self, timeout: Duration) {
        self.send(&ClientMessage::StopAll);
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        if self.outgoing.send(Outgoing::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(timeout);
        }
    }

    fn send(&self, message: &ClientMessage) {
        let _ = self.outgoing.send(Outgoing::Frame(Frame::control(message)));
    }
}

/// The channels between a [`DaemonClient`] handle and the thread that runs its connection.
pub(crate) struct Link {
    pub(crate) client: DaemonClient,
    pub(crate) outgoing: mpsc::UnboundedReceiver<Outgoing>,
    pub(crate) events_tx: async_channel::Sender<ClientEvent>,
    pub(crate) events: async_channel::Receiver<ClientEvent>,
}

impl Link {
    pub(crate) fn new() -> Link {
        let (outgoing_tx, outgoing) = mpsc::unbounded_channel();
        let (events_tx, events) = async_channel::bounded(EVENT_QUEUE);
        Link { client: DaemonClient { outgoing: outgoing_tx, paste_image: false }, outgoing, events_tx, events }
    }
}

/// A connection from its handshake and its handle.
pub(crate) fn connection(
    mut client: DaemonClient,
    hello: Hello,
    events: async_channel::Receiver<ClientEvent>,
) -> Connection {
    let Hello { instance, claude_dir, daemon_version, daemon_commit, paste_image, panes } = hello;
    client.paste_image = paste_image;
    Connection { client, instance, claude_dir, daemon_version, daemon_commit, panes, events }
}

/// Run one connection over `recv`/`send`: the handshake (its result goes to `on_hello`), then the writer and the
/// event loop until the stream ends. `Err` only when the handshake failed.
pub(crate) async fn run_stream<R, W>(
    mut recv: R,
    mut send: W,
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
    events: async_channel::Sender<ClientEvent>,
    on_hello: impl FnOnce(Hello),
) -> Result<(), StartError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    on_hello(hello(&mut recv, &mut send).await?);
    let writer = tokio::spawn(async move {
        while let Some(item) = outgoing.recv().await {
            match item {
                Outgoing::Frame(frame) => {
                    if write_frame(&mut send, &frame).await.is_err() {
                        break;
                    }
                }
                Outgoing::Flush(ack) => {
                    let _ = ack.send(());
                }
            }
        }
    });
    loop {
        let event = match read_frame(&mut recv).await {
            Ok(Some(Frame::Data { pane, bytes })) => ClientEvent::Output { pane, bytes },
            Ok(Some(frame)) => match frame.parse::<DaemonMessage>() {
                Some(Ok(DaemonMessage::Resync { pane, cols, rows })) => ClientEvent::Resync { pane, cols, rows },
                Some(Ok(DaemonMessage::Exited { pane, code })) => ClientEvent::Exited { pane, code },
                Some(Ok(DaemonMessage::Spawned { request, pane, cwd })) => ClientEvent::Spawned { request, pane, cwd },
                Some(Ok(DaemonMessage::SpawnFailed { request, error })) => ClientEvent::SpawnFailed { request, error },
                Some(Ok(DaemonMessage::Sessions { sessions })) => ClientEvent::Sessions { sessions },
                Some(Ok(DaemonMessage::ClaudeDirsIndexed)) => ClientEvent::ClaudeDirsIndexed,
                Some(Ok(DaemonMessage::SessionsChanged { upserted, removed })) => {
                    ClientEvent::SessionsChanged { upserted, removed }
                }
                Some(Ok(DaemonMessage::TakeOverResult { session, outcome })) => {
                    ClientEvent::TakeOverResult { session, outcome }
                }
                Some(Ok(DaemonMessage::ContainerFound { request, container })) => {
                    ClientEvent::ContainerFound { request, container }
                }
                Some(Ok(DaemonMessage::SearchResults { request, hits })) => {
                    ClientEvent::SearchResults { request, hits }
                }
                Some(Ok(DaemonMessage::SearchIndexing { done, total })) => ClientEvent::SearchIndexing { done, total },
                Some(Ok(DaemonMessage::SearchUnavailable { reason })) => ClientEvent::SearchUnavailable { reason },
                Some(Ok(DaemonMessage::DirListing { request, path, dirs })) => {
                    ClientEvent::DirListing { request, path: PathBuf::from(path), dirs }
                }
                Some(Ok(DaemonMessage::DirListingFailed { request, reason })) => {
                    ClientEvent::DirListingFailed { request, reason }
                }
                Some(Ok(DaemonMessage::GitStatus { request, state })) => ClientEvent::GitStatus { request, state },
                Some(Ok(DaemonMessage::GitDiff { request, path, body })) => {
                    ClientEvent::GitDiff { request, path, body }
                }
                Some(Ok(DaemonMessage::GitCommit { request, detail })) => ClientEvent::GitCommit { request, detail },
                Some(Ok(DaemonMessage::GitUnavailable { request, problem })) => {
                    ClientEvent::GitUnavailable { request, problem }
                }
                Some(Ok(DaemonMessage::PasteImageSaved { pane, path })) => ClientEvent::PasteImageSaved { pane, path },
                Some(Ok(DaemonMessage::PasteImageFailed { pane, reason })) => {
                    ClientEvent::PasteImageFailed { pane, reason }
                }
                Some(Ok(DaemonMessage::TakenOver)) => ClientEvent::TakenOver,
                Some(Ok(other)) => {
                    tracing::debug!("ignoring daemon message {other:?}");
                    continue;
                }
                _ => continue,
            },
            Ok(None) | Err(_) => ClientEvent::Disconnected,
        };
        let last = matches!(event, ClientEvent::Disconnected | ClientEvent::TakenOver);
        if events.send(event).await.is_err() || last {
            break;
        }
    }
    writer.abort();
    Ok(())
}

async fn run_connection(
    paths: Paths,
    spawn: impl FnOnce() -> io::Result<()>,
    ready: std::sync::mpsc::Sender<Result<Hello, StartError>>,
    outgoing: mpsc::UnboundedReceiver<Outgoing>,
    events: async_channel::Sender<ClientEvent>,
) {
    let stream = match connect_or_spawn(&paths, spawn).await {
        Ok(stream) => stream,
        Err(err) => {
            let _ = ready.send(Err(StartError::Io(err.into())));
            return;
        }
    };
    let (recv, send) = stream.split();
    let hello_ready = ready.clone();
    let result = run_stream(recv, send, outgoing, events, move |hello| {
        let _ = hello_ready.send(Ok(hello));
    })
    .await;
    if let Err(err) = result {
        let _ = ready.send(Err(err));
    }
}

/// `Hello` → `Welcome`, then `ListPanes` → `Panes`. Returns the daemon instance and its live panes.
async fn hello<R, W>(recv: &mut R, send: &mut W) -> Result<Hello, StartError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let io = |err: io::Error| StartError::Io(err.into());
    write_frame(send, &Frame::control(&ClientMessage::Hello { protocol: PROTOCOL_VERSION })).await.map_err(io)?;
    let (instance, claude_dir, daemon_version, daemon_commit, paste_image) = match next_message(recv).await? {
        DaemonMessage::Welcome { instance, claude_dir, app_version, app_commit, paste_image, .. } => {
            (instance, claude_dir, app_version, app_commit, paste_image)
        }
        DaemonMessage::VersionMismatch { daemon_protocol } => {
            return Err(StartError::VersionMismatch { daemon_protocol });
        }
        other => return Err(StartError::Io(anyhow::anyhow!("unexpected handshake reply {other:?}"))),
    };
    write_frame(send, &Frame::control(&ClientMessage::ListPanes)).await.map_err(io)?;
    loop {
        if let DaemonMessage::Panes { panes } = next_message(recv).await? {
            return Ok(Hello { instance, claude_dir, daemon_version, daemon_commit, paste_image, panes });
        }
    }
}

async fn next_message<R: tokio::io::AsyncRead + Unpin>(recv: &mut R) -> Result<DaemonMessage, StartError> {
    loop {
        match read_frame(recv).await {
            Ok(Some(frame)) => {
                if let Some(parsed) = frame.parse::<DaemonMessage>() {
                    return parsed.map_err(|err| StartError::Io(err.into()));
                }
            }
            Ok(None) => return Err(StartError::Io(anyhow::anyhow!("daemon closed the connection"))),
            Err(err) => return Err(StartError::Io(err.into())),
        }
    }
}

/// Stop the profile's daemon (any protocol version) and wait until it no longer accepts connections.
pub fn restart_daemon(paths: &Paths) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let stream = connect_trusted(paths).await?;
        let (mut recv, mut send) = stream.split();
        write_frame(&mut send, &Frame::control(&ClientMessage::Hello { protocol: PROTOCOL_VERSION })).await?;
        let _ = read_frame(&mut recv).await?;
        write_frame(&mut send, &Frame::control(&ClientMessage::StopAll)).await?;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if Stream::connect(paths.socket_name()?).await.is_err() {
                return Ok(());
            }
        }
        anyhow::bail!("daemon did not stop")
    })
}

/// Merge consecutive `Output` events of one pane (up to `max_bytes`); stop after the first other event.
pub fn batch_events(
    first: ClientEvent,
    rx: &async_channel::Receiver<ClientEvent>,
    max_bytes: usize,
) -> Vec<ClientEvent> {
    let mut batch = vec![first];
    loop {
        let Some(ClientEvent::Output { pane, bytes }) = batch.last_mut() else { return batch };
        if bytes.len() >= max_bytes {
            return batch;
        }
        let pane = *pane;
        match rx.try_recv() {
            Ok(ClientEvent::Output { pane: next, bytes: more }) if next == pane => bytes.extend_from_slice(&more),
            Ok(other) => {
                batch.push(other);
                return batch;
            }
            Err(_) => return batch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_output_is_batched_up_to_the_limit() {
        let (tx, rx) = async_channel::unbounded();
        for chunk in [b"b".to_vec(), b"c".to_vec(), b"d".to_vec()] {
            tx.send_blocking(ClientEvent::Output { pane: 1, bytes: chunk }).unwrap();
        }
        tx.send_blocking(ClientEvent::Exited { pane: 1, code: 0 }).unwrap();
        let batch = batch_events(ClientEvent::Output { pane: 1, bytes: b"a".to_vec() }, &rx, 3);
        assert_eq!(batch, vec![ClientEvent::Output { pane: 1, bytes: b"abc".to_vec() }]);
        let rest = batch_events(rx.try_recv().unwrap(), &rx, 100);
        assert_eq!(
            rest,
            vec![ClientEvent::Output { pane: 1, bytes: b"d".to_vec() }, ClientEvent::Exited { pane: 1, code: 0 }]
        );
    }
}
