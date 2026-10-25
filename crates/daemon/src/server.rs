//! The daemon server: owns panes (PTY + terminal state) and serves one GUI client at a time.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use app_core::palette::Palette;
use app_core::term_state::TermState;
use claude_index::containers::{Docker, DockerCli, to_container};
use claude_index::host::LocalHost;
use claude_index::host::{Marked, in_container, processes_with_env};
use claude_index::index::{IndexState, SessionRecord};
use interprocess::local_socket::tokio::{Listener, RecvHalf, SendHalf, Stream, prelude::*};
use protocol::frame::{Frame, read_frame, write_frame};
use protocol::message::{
    ClientMessage, ContainerChoice, DaemonMessage, ImageFormat, PROTOCOL_VERSION, PaneId, PaneInfo, SessionInfo,
    SpawnRequest, TakeOverOutcome, claude_session,
};
use tokio::sync::Notify;

use crate::containers::{self, PANE_MARKER, Registry, marker};
use crate::outbox::Outbox;
use crate::paste_image;
use crate::pty::{PtyProcess, SpawnSpec, drain_batch, spawn_output_reader};
use crate::search::{Search, SearchEvent};
use crate::sessions::{ContainerView, Gone, PaneSession, diff, merge_records, resolve, spawn_indexer, still_claimed};
use crate::take_over::{self, FORCE_WAIT, GRACE};

const OUTPUT_QUEUE_CHUNKS: usize = 32;
const MAX_BATCH_BYTES: usize = 256 * 1024;
const CLIENT_DATA_BUDGET: usize = 4 << 20;
const RESYNC_TICK: Duration = Duration::from_millis(100);
const EXIT_DRAIN: Duration = Duration::from_millis(300);
/// How long a closing connection waits for its peer to read what was sent (Windows).
#[cfg(windows)]
const CLOSE_FLUSH: Duration = Duration::from_secs(2);
/// After a failed Docker lookup, new panes skip Docker for this long (a hung Docker would delay every new pane).
const DOCKER_RETRY: Duration = Duration::from_secs(30);
/// Least time between Docker lookups caused by container ids the registry does not know.
const UNKNOWN_CONTAINER_REFRESH: Duration = Duration::from_secs(10);

/// How the server runs.
pub struct ServerOptions {
    /// Environment every pane starts from (the resolved login-shell environment).
    pub base_env: Vec<(String, String)>,
    /// If set (Unix), connections from other users are rejected.
    pub owner_uid: Option<u32>,
    /// Stop if no client connects within this time after start.
    pub idle_timeout: Duration,
    /// Claude config directory to index; `None` sends no session messages.
    pub claude_dir: Option<PathBuf>,
    /// Where dev container mappings are remembered; `None` keeps them in memory only.
    pub containers_file: Option<PathBuf>,
    /// Where the conversation search index lives; `None` turns search off.
    pub search_dir: Option<PathBuf>,
    /// The `docker` program; `None` turns dev container features off.
    pub docker: Option<String>,
    /// Where pasted images are saved; `None` turns image paste off.
    pub paste_dir: Option<PathBuf>,
}

struct Pane {
    pty: PtyProcess,
    term: TermState,
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    /// Session its command line names.
    session_arg: Option<String>,
    /// Whether it runs Claude Code.
    claude: bool,
    /// Start time of its process (to recognise that process in the index after it exits).
    start: Option<String>,
    /// Host folder of the dev container it runs in.
    container: Option<PathBuf>,
    /// That container's id (when it started).
    container_id: Option<String>,
    /// The user it runs as in that container (`docker exec -u`).
    container_user: Option<String>,
    last_focused: u64,
    needs_resync: bool,
}

struct Client {
    id: u64,
    outbox: Outbox,
    attached: HashSet<PaneId>,
    /// Wakes this connection's reader so it drops the socket (takeover / shutdown).
    kicked: Arc<Notify>,
}

#[derive(Default)]
struct State {
    panes: HashMap<PaneId, Pane>,
    /// Colors the GUI draws with; panes answer color queries from them.
    palette: Palette,
    client: Option<Client>,
    next_pane: PaneId,
    next_client: u64,
    focus_seq: u64,
    ever_connected: bool,
    /// Whether session messages are sent (an index runs).
    sessions_enabled: bool,
    /// A client has sent `SetClaudeDirs` and `ClaudeDirsIndexed` has not been sent for it yet.
    dirs_pending: bool,
    /// Latest index scans of every indexed dir, merged; `None` until the first scan finishes.
    records: Option<Arc<Vec<SessionRecord>>>,
    /// Each indexed dir's latest scan.
    dir_records: HashMap<PathBuf, Arc<Vec<SessionRecord>>>,
    /// The tasks that follow each dir's indexer (aborting one stops that indexer).
    indexers: HashMap<PathBuf, tokio::task::JoinHandle<()>>,
    /// Sessions as last sent to (or computed for) the client.
    sent: Vec<SessionInfo>,
    /// Processes seen exiting that the index still lists as running.
    gone: Vec<Gone>,
    /// This daemon's instance id (the prefix of its panes' `GANTRY_PANE` markers).
    instance: String,
    /// Dev containers.
    registry: Registry,
    /// Container processes (host pid) carrying one of this daemon's pane markers.
    marked: HashMap<u32, PaneId>,
    /// Container processes already checked and found carrying no marker of ours.
    unmarked: HashSet<u32>,
    /// Containers to look inside for this daemon's markers (processes the host cannot read, e.g. root's).
    probe_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// When unknown container ids last caused a Docker lookup.
    last_unknown_refresh: Option<Instant>,
    /// Whether "Docker is unavailable" was logged already.
    docker_warned: bool,
    /// When a Docker lookup last failed.
    docker_failed_at: Option<Instant>,
}

struct Shared {
    state: Mutex<State>,
    shutdown: Notify,
    base_env: Vec<(String, String)>,
    /// Checked against the peer's uid on Unix only.
    #[cfg_attr(not(unix), allow(dead_code))]
    owner_uid: Option<u32>,
    /// Identifies this daemon process (sent in `Welcome`).
    instance: String,
    /// The `docker` program; `None` = no dev containers.
    docker: Option<String>,
    /// The daemon's default Claude config dir.
    claude_dir: Option<PathBuf>,
    /// Conversation search; `None` = off.
    search: Option<Search>,
    /// Where pasted images are saved; `None` = image paste off.
    paste_dir: Option<PathBuf>,
}

impl Shared {
    /// Lock the daemon state, recovering from poisoning: a panic in one pane's task must not
    /// wedge every other session (spec §6).
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stop_if_idle(&self, state: &State) {
        if state.client.is_none() && state.panes.is_empty() {
            self.shutdown.notify_one();
        }
    }
}

impl State {
    /// Store `palette` and apply it to every pane.
    fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
        for pane in self.panes.values_mut() {
            pane.term.set_palette(palette);
        }
    }

    fn control(&self, message: &DaemonMessage) {
        if let Some(client) = &self.client {
            client.outbox.push_control(Frame::control(message));
        }
    }

    /// Send `pane`'s live output to the client, falling back to a resync when it lags.
    fn deliver(&mut self, pane_id: PaneId, bytes: Vec<u8>) {
        let Some(client) = &self.client else { return };
        if !client.attached.contains(&pane_id) {
            return;
        }
        let outbox = client.outbox.clone();
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        if pane.needs_resync || !outbox.try_push_data(Frame::Data { pane: pane_id, bytes }) {
            pane.needs_resync = true;
            self.resync_if_room(pane_id);
        }
    }

    fn resync_if_room(&mut self, pane_id: PaneId) {
        let Some(client) = &self.client else { return };
        if !client.outbox.has_room() {
            return;
        }
        let outbox = client.outbox.clone();
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            Self::send_resync(&outbox, pane_id, pane);
        }
    }

    /// Send the session list (`full`) or what changed since the last one. Without a client only the
    /// baseline is updated; a connecting client always gets a full list.
    fn publish_sessions(&mut self, full: bool) {
        let Some(records) = self.records.clone().filter(|_| self.sessions_enabled) else { return };
        self.refresh_marks(&records);
        let panes: Vec<PaneSession> = self
            .panes
            .iter()
            .map(|(id, pane)| PaneSession { pane: *id, pid: pane.pty.pid(), session_arg: pane.session_arg.clone() })
            .collect();
        let view = ContainerView {
            registry: &self.registry,
            marked: &self.marked,
            dir_exists: &|dir: &Path| dir.is_dir(),
            checks_processes: cfg!(any(target_os = "linux", windows)),
        };
        let sessions = resolve(&records, &panes, &self.gone, &view);
        if full {
            self.control(&DaemonMessage::Sessions { sessions: sessions.clone() });
        } else {
            let (upserted, removed) = diff(&self.sent, &sessions);
            if !upserted.is_empty() || !removed.is_empty() {
                self.control(&DaemonMessage::SessionsChanged { upserted, removed });
            }
        }
        self.sent = sessions;
    }

    /// Tell the client its `SetClaudeDirs` is fully indexed, once every indexed dir has its first scan.
    fn announce_dirs_indexed(&mut self) {
        if self.dirs_pending && self.indexers.keys().all(|dir| self.dir_records.contains_key(dir)) {
            self.dirs_pending = false;
            self.control(&DaemonMessage::ClaudeDirsIndexed);
        }
    }

    /// Find which container processes carry this daemon's pane markers. `/proc` is scanned only while some
    /// container pane exists and the index reports a container process not checked yet.
    fn refresh_marks(&mut self, records: &[SessionRecord]) {
        let live: HashSet<u32> = records
            .iter()
            .filter_map(|r| match r.state {
                IndexState::InContainer { pid } => pid,
                _ => None,
            })
            .collect();
        let panes = &self.panes;
        self.marked.retain(|pid, pane| live.contains(pid) && panes.contains_key(pane));
        self.unmarked.retain(|pid| live.contains(pid));
        let container_panes: Vec<PaneId> =
            panes.iter().filter(|(_, p)| p.container.is_some()).map(|(id, _)| *id).collect();
        if container_panes.is_empty()
            || live.iter().all(|pid| self.marked.contains_key(pid) || self.unmarked.contains(pid))
        {
            return;
        }
        let found = pane_marks(&processes_with_env(Path::new("/proc"), PANE_MARKER), &self.instance, &container_panes);
        let mut probe: HashSet<String> = HashSet::new();
        for pid in live {
            match found.get(&pid) {
                Some(pane) => {
                    self.marked.insert(pid, *pane);
                }
                None => {
                    if !self.marked.contains_key(&pid) && self.unmarked.insert(pid) {
                        // Not readable here (another user's process): look inside its container, if one of our panes
                        // runs there.
                        let container = records
                            .iter()
                            .find(|r| r.state == IndexState::InContainer { pid: Some(pid) })
                            .and_then(|r| r.container_id.clone());
                        let ours = |id: &str| {
                            panes
                                .values()
                                .filter_map(|p| p.container_id.as_deref())
                                .any(|c| c.starts_with(id) || id.starts_with(c))
                        };
                        probe.extend(container.filter(|id| ours(id)));
                    }
                }
            }
        }
        if let Some(tx) = &self.probe_tx {
            for id in probe {
                let _ = tx.send(id);
            }
        }
    }

    fn send_resync(outbox: &Outbox, pane_id: PaneId, pane: &mut Pane) {
        let snapshot = pane.term.snapshot();
        outbox.push_control(Frame::control(&DaemonMessage::Resync {
            pane: pane_id,
            cols: snapshot.cols as u16,
            rows: snapshot.rows.len() as u16,
        }));
        outbox.force_push_data(Frame::Data { pane: pane_id, bytes: pane.term.replay_bytes() });
        pane.needs_resync = false;
    }
}

/// Serve clients until `StopAll` or until idle (no panes and no client).
pub async fn serve(listener: Listener, options: ServerOptions) -> anyhow::Result<()> {
    let claude_dir = options.claude_dir;
    let instance = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    );
    let state = State {
        next_pane: 1,
        sessions_enabled: claude_dir.is_some(),
        instance: instance.clone(),
        registry: Registry::load(options.containers_file),
        ..State::default()
    };
    let (search_events, mut search_rx) = tokio::sync::mpsc::unbounded_channel();
    let search = options.search_dir.map(|dir| Search::spawn(dir, search_events));
    let shared = Arc::new(Shared {
        state: Mutex::new(state),
        shutdown: Notify::new(),
        base_env: options.base_env,
        owner_uid: options.owner_uid,
        instance,
        docker: options.docker,
        claude_dir: claude_dir.clone(),
        search,
        paste_dir: options.paste_dir,
    });
    refresh_containers(&shared);
    let probes = shared.docker.is_some().then(|| {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        shared.lock().probe_tx = Some(tx);
        let shared = shared.clone();
        tokio::spawn(async move {
            while let Some(container) = rx.recv().await {
                probe_container(&shared, container);
            }
        })
    });
    let idle = {
        let shared = shared.clone();
        tokio::spawn(async move {
            tokio::time::sleep(options.idle_timeout).await;
            let state = shared.lock();
            if !state.ever_connected {
                shared.stop_if_idle(&state);
            }
        })
    };
    let ticker = {
        let shared = shared.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RESYNC_TICK).await;
                let mut state = shared.lock();
                let lagging: Vec<PaneId> =
                    state.panes.iter().filter(|(_, pane)| pane.needs_resync).map(|(id, _)| *id).collect();
                for pane in lagging {
                    state.resync_if_room(pane);
                }
            }
        })
    };
    let search_task = {
        let shared = shared.clone();
        tokio::spawn(async move {
            while let Some(event) = search_rx.recv().await {
                let message = match event {
                    SearchEvent::Indexing { done, total } => DaemonMessage::SearchIndexing { done, total },
                    SearchEvent::Unavailable { reason } => DaemonMessage::SearchUnavailable { reason },
                };
                shared.lock().control(&message);
            }
        })
    };
    set_claude_dirs(&shared, Vec::new());
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(stream) => { tokio::spawn(handle_connection(shared.clone(), stream)); }
                Err(err) => tracing::warn!("accept failed: {err}"),
            },
            _ = shared.shutdown.notified() => break,
        }
    }
    idle.abort();
    ticker.abort();
    search_task.abort();
    for task in shared.lock().indexers.values() {
        task.abort();
    }
    if let Some(task) = probes {
        task.abort();
    }
    let mut state = shared.lock();
    if let Some(client) = state.client.take() {
        client.outbox.close();
        client.kicked.notify_one();
    }
    state.panes.clear();
    Ok(())
}

async fn handle_connection(shared: Arc<Shared>, stream: Stream) {
    #[cfg(unix)]
    if let Some(owner) = shared.owner_uid {
        let peer = stream.peer_creds().ok().and_then(|creds| creds.euid());
        if peer != Some(owner) {
            tracing::warn!("rejected connection from uid {peer:?}");
            return;
        }
    }
    let (mut recv, mut send) = stream.split();
    let Ok(Some(first)) = read_frame(&mut recv).await else { return };
    let Some(Ok(ClientMessage::Hello { protocol })) = first.parse::<ClientMessage>() else { return };
    if protocol != PROTOCOL_VERSION {
        let _ = write_frame(
            &mut send,
            &Frame::control(&DaemonMessage::VersionMismatch { daemon_protocol: PROTOCOL_VERSION }),
        )
        .await;
        while let Ok(Some(frame)) = read_frame(&mut recv).await {
            if let Some(Ok(ClientMessage::StopAll)) = frame.parse::<ClientMessage>() {
                stop_all(&shared);
                return;
            }
        }
        return;
    }
    let outbox = Outbox::new(CLIENT_DATA_BUDGET);
    let kicked = Arc::new(Notify::new());
    let client_id = {
        let mut state = shared.lock();
        state.next_client += 1;
        state.ever_connected = true;
        let id = state.next_client;
        if let Some(old) = state.client.take() {
            old.outbox.push_control(Frame::control(&DaemonMessage::TakenOver));
            old.outbox.close();
            old.kicked.notify_one();
        }
        outbox.push_control(Frame::control(&DaemonMessage::Welcome {
            protocol: PROTOCOL_VERSION,
            instance: shared.instance.clone(),
            claude_dir: shared.claude_dir.clone(),
            app_version: Some(protocol::build::VERSION.into()),
            app_commit: Some(protocol::build::COMMIT.into()),
            paste_image: shared.paste_dir.is_some(),
        }));
        state.client = Some(Client { id, outbox: outbox.clone(), attached: HashSet::new(), kicked: kicked.clone() });
        id
    };
    let writer = tokio::spawn(async move {
        while let Some(frame) = outbox.next().await {
            if write_frame(&mut send, &frame).await.is_err() {
                break;
            }
        }
        send
    });
    loop {
        let frame = tokio::select! {
            read = read_frame(&mut recv) => match read {
                Ok(Some(frame)) => frame,
                _ => break,
            },
            _ = kicked.notified() => break,
        };
        match frame {
            Frame::Data { pane, bytes } => {
                let mut state = shared.lock();
                if let Some(pane) = state.panes.get_mut(&pane) {
                    let _ = pane.pty.write(&bytes);
                }
            }
            control => match control.parse::<ClientMessage>() {
                Some(Ok(message)) => handle_message(&shared, client_id, message),
                _ => tracing::warn!("ignoring malformed control frame"),
            },
        }
    }
    {
        let mut state = shared.lock();
        if state.client.as_ref().is_some_and(|client| client.id == client_id) {
            if let Some(client) = state.client.take() {
                client.outbox.close();
            }
            shared.stop_if_idle(&state);
        }
    }
    if let Ok(send) = writer.await {
        close(recv, send).await;
    }
}

/// Close a connection. On Windows interprocess hands a dropped pipe with unread data to one shared thread that waits
/// for each peer in turn to read it, so a peer that stopped reading (a stuck window that was taken over) would keep
/// every later closed connection open, and their peers would never see the close. Wait a little here instead, then
/// close regardless.
async fn close(recv: RecvHalf, send: SendHalf) {
    #[cfg(windows)]
    {
        use tokio::io::AsyncWriteExt;
        let mut send = send;
        let flushed = matches!(tokio::time::timeout(CLOSE_FLUSH, send.flush()).await, Ok(Ok(())));
        if !flushed && let Ok(Stream::NamedPipe(stream)) = Stream::reunite(recv, send) {
            stream.inner().assume_flushed();
        }
    }
    #[cfg(not(windows))]
    drop((recv, send));
}

fn stop_all(shared: &Shared) {
    let mut state = shared.lock();
    let inside: Vec<(String, String)> = state
        .panes
        .iter()
        .filter(|(_, p)| p.container.is_some())
        .filter_map(|(id, pane)| stop_container_processes(shared, *id, pane))
        .collect();
    state.panes.clear();
    drop(state);
    // The daemon exits next: stop the rest from inside their containers before it does.
    for (container, marker) in inside {
        stop_inside(shared, &container, &marker);
    }
    shared.shutdown.notify_one();
}

/// Look up dev containers (off the async threads) and republish sessions with what was found.
fn refresh_containers(shared: &Arc<Shared>) {
    let Some(program) = shared.docker.clone() else { return };
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let mut registry = shared.lock().registry.clone();
        let result = registry.refresh(&DockerCli { program });
        let mut state = shared.lock();
        match result {
            Ok(()) => {
                state.registry = registry;
                state.publish_sessions(false);
            }
            Err(err) => {
                state.docker_failed_at = Some(Instant::now());
                if !state.docker_warned {
                    state.docker_warned = true;
                    tracing::info!("dev containers are unavailable: {err}");
                }
            }
        }
    });
}

/// The verified processes of a container pane: those carrying its marker whose start time is known.
fn marked_targets(marked: &[Marked], marker: &str) -> Vec<take_over::Target> {
    marked
        .iter()
        .filter(|m| m.value == marker)
        .filter_map(|m| Some(take_over::Target { pid: m.pid, start: m.start.clone()? }))
        .collect()
}

/// Host pid → pane for processes carrying a marker of `instance` that names one of `panes`.
fn pane_marks(marked: &[Marked], instance: &str, panes: &[PaneId]) -> HashMap<u32, PaneId> {
    marked
        .iter()
        .filter_map(|m| {
            let (owner, pane) = m.value.rsplit_once(':')?;
            let pane: PaneId = pane.parse().ok()?;
            (owner == instance && panes.contains(&pane)).then_some((m.pid, pane))
        })
        .collect()
}

/// Ask the processes a container pane started (inside the container) to stop; killing the `docker exec` client alone
/// does not end them. Processes this user can see are signalled from the host. When none are (another user's, e.g.
/// root's), returns the container and marker to stop them from inside (`stop_inside`).
fn stop_container_processes(shared: &Shared, pane_id: PaneId, pane: &Pane) -> Option<(String, String)> {
    let marker = marker(&shared.instance, pane_id);
    let host = LocalHost::new();
    let targets = marked_targets(&processes_with_env(Path::new("/proc"), PANE_MARKER), &marker);
    for target in &targets {
        if let Err(err) = take_over::signal(target, false, &host) {
            tracing::warn!("could not stop container process {}: {err}", target.pid);
        }
    }
    targets.is_empty().then(|| pane.container_id.clone().map(|id| (id, marker))).flatten()
}

/// Ask every process carrying `marker` inside `container` to stop, as root inside it (Docker, up to 3 s).
fn stop_inside(shared: &Shared, container: &str, marker: &str) {
    let Some(program) = shared.docker.clone() else { return };
    if let Err(err) = (DockerCli { program }).run_as_root(container, &containers::stop_script(marker)) {
        tracing::warn!("could not stop the pane's processes in container {container}: {err}");
    }
}

/// Look inside `container` for processes carrying this daemon's markers (the host cannot read them), and mark them.
fn probe_container(shared: &Arc<Shared>, container: String) {
    let Some(program) = shared.docker.clone() else { return };
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let output = match (DockerCli { program }).run_as_root(&container, &containers::marked_script()) {
            Ok(output) => output,
            Err(err) => return tracing::info!("could not look inside container {container}: {err}"),
        };
        let marked: Vec<Marked> = containers::parse_marked(&output)
            .into_iter()
            .filter_map(|(inner, value)| {
                let (pid, start) = in_container(Path::new("/proc"), &container, inner)?;
                Some(Marked { pid, value, start })
            })
            .collect();
        let mut state = shared.lock();
        let panes: Vec<PaneId> = state.panes.iter().filter(|(_, p)| p.container.is_some()).map(|(id, _)| *id).collect();
        let found = pane_marks(&marked, &state.instance, &panes);
        if found.is_empty() {
            return;
        }
        for (pid, pane) in found {
            state.unmarked.remove(&pid);
            state.marked.insert(pid, pane);
        }
        state.publish_sessions(false);
    });
}

/// Stop the outside process running `session`, then report. On success, the session is published as closed first
/// (its exit is remembered until the index notices), so the GUI's resume finds it closed.
fn take_over_session(shared: &Arc<Shared>, session: String, force: bool) {
    let target = {
        let state = shared.lock();
        let records = state.records.as_deref().map_or(&[][..], Vec::as_slice);
        take_over::target(&state.sent, records, &session)
    };
    let shared = shared.clone();
    tokio::spawn(async move {
        let host = LocalHost::new();
        let outcome = match target.and_then(|target| take_over::signal(&target, force, &host).map(|_| target)) {
            Err(error) => TakeOverOutcome::Failed { error },
            Ok(target) => {
                if take_over::wait_for_exit(&target, &host, if force { FORCE_WAIT } else { GRACE }).await {
                    let mut state = shared.lock();
                    state.gone.push(Gone { pid: target.pid, start: Some(target.start) });
                    state.publish_sessions(false);
                    TakeOverOutcome::Exited
                } else if force {
                    TakeOverOutcome::Failed { error: "it did not exit after SIGKILL".into() }
                } else {
                    TakeOverOutcome::StillRunning
                }
            }
        };
        shared.lock().control(&DaemonMessage::TakeOverResult { session, outcome });
    });
}

fn handle_message(shared: &Arc<Shared>, client_id: u64, message: ClientMessage) {
    match message {
        ClientMessage::Hello { .. } => {}
        ClientMessage::StopAll => stop_all(shared),
        ClientMessage::ListPanes => {
            let mut state = shared.lock();
            let mut panes: Vec<PaneInfo> = state
                .panes
                .iter()
                .map(|(id, pane)| PaneInfo {
                    pane: *id,
                    program: pane.program.clone(),
                    args: pane.args.clone(),
                    cwd: pane.cwd.clone(),
                    last_focused: pane.last_focused,
                    container: pane.container.clone(),
                    claude: pane.claude,
                })
                .collect();
            panes.sort_by(|a, b| b.last_focused.cmp(&a.last_focused).then(b.pane.cmp(&a.pane)));
            state.control(&DaemonMessage::Panes { panes });
            state.publish_sessions(true);
        }
        ClientMessage::SetClaudeDirs { dirs } => {
            set_claude_dirs(shared, dirs);
            let mut state = shared.lock();
            state.dirs_pending = true;
            state.announce_dirs_indexed();
        }
        ClientMessage::Spawn { request, spec } => spawn_pane(shared, client_id, request, spec),
        ClientMessage::Attach { pane } => {
            let mut state = shared.lock();
            let Some(client) = state.client.as_mut() else { return };
            client.attached.insert(pane);
            let outbox = client.outbox.clone();
            match state.panes.get_mut(&pane) {
                Some(p) => State::send_resync(&outbox, pane, p),
                None => state.control(&DaemonMessage::Error { message: format!("no pane {pane}") }),
            }
        }
        ClientMessage::Resize { pane, cols, rows, cell_width, cell_height } => {
            let mut state = shared.lock();
            if let Some(p) = state.panes.get_mut(&pane) {
                p.term.resize(cols, rows);
                p.term.set_cell_size(cell_width, cell_height);
                if let Err(err) = p.pty.resize(cols, rows) {
                    tracing::warn!("resize failed: {err:#}");
                }
            }
        }
        ClientMessage::SetPalette(colors) => shared.lock().set_palette(colors.into()),
        ClientMessage::Focus { pane } => {
            let mut state = shared.lock();
            state.focus_seq += 1;
            let seq = state.focus_seq;
            if let Some(p) = state.panes.get_mut(&pane) {
                p.last_focused = seq;
            }
        }
        ClientMessage::TakeOver { session, force } => take_over_session(shared, session, force),
        ClientMessage::FindContainer { request, dir } => find_container(shared, request, dir),
        ClientMessage::SearchConversations { request, query, limit } => {
            search_conversations(shared, request, query, limit)
        }
        ClientMessage::ListDir { request, path } => list_dir_request(shared, request, path),
        ClientMessage::SavePasteImage { pane, format, data } => paste_image_request(shared, pane, format, data),
        ClientMessage::GitStatus { request, folder, known_head } => {
            git_status_request(shared, request, folder, known_head)
        }
        ClientMessage::GitDiff { request, folder, path, old_path, full, commit } => {
            git_diff_request(shared, request, folder, path, old_path, full, commit)
        }
        ClientMessage::GitCommit { request, folder, hash } => git_commit_request(shared, request, folder, hash),
        ClientMessage::Kill { pane } => {
            let mut state = shared.lock();
            let inside = state
                .panes
                .get(&pane)
                .filter(|p| p.container.is_some())
                .and_then(|p| stop_container_processes(shared, pane, p));
            if let Some((container, marker)) = inside {
                let shared = shared.clone();
                tokio::task::spawn_blocking(move || stop_inside(&shared, &container, &marker));
            }
            if let Some(p) = state.panes.get_mut(&pane) {
                let _ = p.pty.kill();
            }
        }
    }
}

/// Index `dirs` (empty = the daemon's default dir): stop the indexers of dirs no longer listed, start those of new
/// ones, and publish the merged sessions.
fn set_claude_dirs(shared: &Arc<Shared>, dirs: Vec<PathBuf>) {
    let mut wanted: Vec<PathBuf> = Vec::new();
    for dir in if dirs.is_empty() { shared.claude_dir.iter().cloned().collect() } else { dirs } {
        if !wanted.contains(&dir) {
            wanted.push(dir);
        }
    }
    if let Some(search) = &shared.search {
        search.set_dirs(wanted.clone());
    }
    let mut state = shared.lock();
    state.sessions_enabled = !wanted.is_empty();
    let stale: Vec<PathBuf> = state.indexers.keys().filter(|dir| !wanted.contains(dir)).cloned().collect();
    for dir in &stale {
        if let Some(task) = state.indexers.remove(dir) {
            task.abort();
        }
        state.dir_records.remove(dir);
    }
    let changed = !stale.is_empty() && state.records.is_some();
    for dir in wanted {
        if state.indexers.contains_key(&dir) {
            continue;
        }
        let (tx, mut rx) = tokio::sync::watch::channel(None);
        spawn_indexer(dir.clone(), tx);
        let task_shared = shared.clone();
        let task_dir = dir.clone();
        let task = tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let Some(records) = rx.borrow_and_update().clone() else { continue };
                let mut state = task_shared.lock();
                state.dir_records.insert(task_dir.clone(), records);
                on_records(&task_shared, &mut state);
            }
        });
        state.indexers.insert(dir, task);
    }
    if changed {
        on_records(shared, &mut state);
    }
}

/// The indexed dirs' scans changed: merge them and publish what changed.
fn on_records(shared: &Arc<Shared>, state: &mut State) {
    let merged = Arc::new(merge_records(state.dir_records.values()));
    state.gone.retain(|gone| still_claimed(gone, &merged));
    let unknown = merged.iter().filter_map(|r| r.container_id.as_deref()).any(|id| !state.registry.knows(id));
    if unknown && state.last_unknown_refresh.is_none_or(|at| at.elapsed() >= UNKNOWN_CONTAINER_REFRESH) {
        state.last_unknown_refresh = Some(Instant::now());
        refresh_containers(shared);
    }
    // Sessions seen in a container reopen there after they close.
    for record in merged.iter() {
        state.registry.remember(record);
    }
    // The first scan is the first list a connected client gets.
    let first = state.records.is_none();
    state.records = Some(merged);
    state.publish_sessions(first);
    state.announce_dirs_indexed();
}

fn spawn_pane(shared: &Arc<Shared>, client_id: u64, request: u64, spec: SpawnRequest) {
    if let Some(folder) = spec.container.clone() {
        spawn_in_container(shared.clone(), client_id, request, spec, folder);
        return;
    }
    let cwd = spec
        .cwd_from
        .and_then(|from| shared.lock().panes.get(&from).and_then(|pane| pane.pty.current_dir()))
        .unwrap_or_else(|| spec.cwd.clone());
    let cwd = app_core::settings::expand_home(&cwd, std::env::home_dir().as_deref());
    let command = (spec.program.clone(), spec.args.clone());
    start_pane(shared, client_id, request, spec, cwd, |_| command, None);
}

/// The answer to `FindContainer` without asking Docker, when there is one: the container the last lookup found for
/// `dir` (a launch looks it up again anyway), or none for a while after Docker failed. `None` = ask Docker.
fn cached_answer(
    registry: &Registry,
    dir: &Path,
    failed_at: Option<Instant>,
    now: Instant,
) -> Option<Option<ContainerChoice>> {
    if let Some(c) = containers::for_dir(registry.containers(), dir) {
        return Some(Some(ContainerChoice {
            host_folder: c.host_folder.clone(),
            name: c.name.clone(),
            running: c.running,
        }));
    }
    failed_at.filter(|at| now.duration_since(*at) < DOCKER_RETRY).map(|_| None)
}

/// Answer `SearchConversations` off the async threads: hits for sessions the index lists, then the indexing
/// progress while a large pass runs (a GUI that connected mid-build learns it this way).
fn search_conversations(shared: &Arc<Shared>, request: u64, query: String, limit: u32) {
    let Some(search) = shared.search.clone() else {
        shared.lock().control(&DaemonMessage::SearchUnavailable { reason: "conversation search is off".into() });
        return;
    };
    let records = shared.lock().records.clone();
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let known: HashSet<&str> = records.iter().flat_map(|records| records.iter()).map(|r| r.id.as_str()).collect();
        let reply = match search.search(&query, limit as usize, &|id| known.contains(id)) {
            Ok(hits) => DaemonMessage::SearchResults { request, hits },
            Err(reason) => DaemonMessage::SearchUnavailable { reason },
        };
        let state = shared.lock();
        state.control(&reply);
        if let Some((done, total)) = search.indexing() {
            state.control(&DaemonMessage::SearchIndexing { done, total });
        }
    });
}

/// Answer `ListDir` off the async threads.
fn list_dir_request(shared: &Arc<Shared>, request: u64, path: String) {
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let reply = match crate::list_dir::list_dir(&path, std::env::home_dir().as_deref()) {
            Ok((path, dirs)) => DaemonMessage::DirListing { request, path: path.to_string_lossy().into_owned(), dirs },
            Err(reason) => DaemonMessage::DirListingFailed { request, reason },
        };
        shared.lock().control(&reply);
    });
}

/// Answer `SavePasteImage` off the async threads: a container pane's image goes into its container, any other
/// pane's into the paste folder beside the socket.
fn paste_image_request(shared: &Arc<Shared>, pane: PaneId, format: ImageFormat, data: Vec<u8>) {
    // The pane's container id and user, read while the lock is held; `None` = no such pane.
    let target = shared
        .lock()
        .panes
        .get(&pane)
        .map(|p| p.container_id.as_ref().map(|id| (id.clone(), p.container_user.clone())));
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let saved = match (target, &shared.paste_dir, &shared.docker) {
            (None, _, _) => Err("no such pane".to_string()),
            (_, None, _) => Err("image paste is off".to_string()),
            (Some(Some(_)), _, None) => Err("dev containers are off".to_string()),
            (Some(Some((id, user))), _, Some(docker)) => {
                paste_image::save_in_container(docker, &id, user.as_deref(), format, &data)
            }
            (Some(None), Some(dir), _) => paste_image::save_local(dir, format, &data, std::time::SystemTime::now())
                .map(|path| path.to_string_lossy().into_owned()),
        };
        let reply = match saved {
            Ok(path) => DaemonMessage::PasteImageSaved { pane, path },
            Err(reason) => {
                tracing::warn!(pane, "pasted image not saved: {reason}");
                DaemonMessage::PasteImageFailed { pane, reason }
            }
        };
        shared.lock().control(&reply);
    });
}

/// Answer `GitStatus` off the async threads.
fn git_status_request(shared: &Arc<Shared>, request: u64, folder: String, known_head: Option<String>) {
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let reply = match crate::git::status(&folder, std::env::home_dir().as_deref(), known_head.as_deref()) {
            Ok(state) => DaemonMessage::GitStatus { request, state },
            Err(problem) => DaemonMessage::GitUnavailable { request, problem },
        };
        shared.lock().control(&reply);
    });
}

/// Answer `GitDiff` off the async threads.
fn git_diff_request(
    shared: &Arc<Shared>,
    request: u64,
    folder: String,
    path: String,
    old_path: Option<String>,
    full: bool,
    commit: Option<String>,
) {
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let home = std::env::home_dir();
        let reply =
            match crate::git::diff(&folder, home.as_deref(), &path, old_path.as_deref(), full, commit.as_deref()) {
                Ok(body) => DaemonMessage::GitDiff { request, path, body },
                Err(problem) => DaemonMessage::GitUnavailable { request, problem },
            };
        shared.lock().control(&reply);
    });
}

/// Answer `GitCommit` off the async threads.
fn git_commit_request(shared: &Arc<Shared>, request: u64, folder: String, hash: String) {
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let reply = match crate::git::commit(&folder, std::env::home_dir().as_deref(), &hash) {
            Ok(detail) => DaemonMessage::GitCommit { request, detail },
            Err(problem) => DaemonMessage::GitUnavailable { request, problem },
        };
        shared.lock().control(&reply);
    });
}

/// Answer `FindContainer`: from the last lookup when it knows, else refresh the dev containers (off the async threads)
/// and name the one that mounts `dir`.
fn find_container(shared: &Arc<Shared>, request: u64, dir: PathBuf) {
    let Some(program) = shared.docker.clone() else {
        shared.lock().control(&DaemonMessage::ContainerFound { request, container: None });
        return;
    };
    {
        let state = shared.lock();
        if let Some(container) = cached_answer(&state.registry, &dir, state.docker_failed_at, Instant::now()) {
            state.control(&DaemonMessage::ContainerFound { request, container });
            return;
        }
    }
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let mut registry = shared.lock().registry.clone();
        let refreshed = registry.refresh(&DockerCli { program });
        let mut state = shared.lock();
        match refreshed {
            Ok(()) => state.registry = registry,
            Err(_) => state.docker_failed_at = Some(Instant::now()),
        }
        let container = refreshed
            .ok()
            .and_then(|()| containers::for_dir(state.registry.containers(), &dir))
            .map(|c| ContainerChoice { host_folder: c.host_folder.clone(), name: c.name.clone(), running: c.running });
        state.control(&DaemonMessage::ContainerFound { request, container });
    });
}

/// Start `spec` in the dev container serving `folder`: find it (starting it when stopped), then run the command
/// through `docker exec` with this pane's marker.
fn spawn_in_container(shared: Arc<Shared>, client_id: u64, request: u64, spec: SpawnRequest, folder: PathBuf) {
    tokio::spawn(async move {
        let fail =
            |shared: &Shared, error: String| shared.lock().control(&DaemonMessage::SpawnFailed { request, error });
        let Some(program) = shared.docker.clone() else {
            return fail(&shared, "Dev containers are not available: Docker is turned off".into());
        };
        let mut registry = shared.lock().registry.clone();
        let looked_up = tokio::task::spawn_blocking(move || {
            let ready = containers::ready(&DockerCli { program }, &mut registry, &folder);
            (registry, ready, folder)
        })
        .await;
        let Ok((mut registry, ready, folder)) = looked_up else {
            return fail(&shared, "the Docker lookup failed".into());
        };
        if let (Ok(container), Some(session)) = (&ready, claude_session(spec.claude, &spec.program, &spec.args)) {
            registry.remember_session(session, &container.id);
        }
        shared.lock().registry = registry;
        let container = match ready {
            Ok(container) => container,
            Err(error) => return fail(&shared, error),
        };
        let Some(inner_cwd) = to_container(&container.mounts, &spec.cwd) else {
            return fail(&shared, format!("{} is not inside the dev container {}", spec.cwd.display(), container.name));
        };
        let cwd = spec.cwd.clone();
        let (program, args) = (spec.program.clone(), spec.args.clone());
        let instance = shared.instance.clone();
        start_pane(
            &shared,
            client_id,
            request,
            spec,
            cwd,
            |id| containers::exec_command(&container, &inner_cwd, &program, &args, &marker(&instance, id)),
            Some((folder, container.id.clone(), container.remote_user.clone())),
        );
    });
}

/// Start a pane for `spec` in `cwd`, running the command `command` builds from the new pane's id.
fn start_pane(
    shared: &Arc<Shared>,
    client_id: u64,
    request: u64,
    spec: SpawnRequest,
    cwd: PathBuf,
    command: impl FnOnce(PaneId) -> (String, Vec<String>),
    container: Option<(PathBuf, String, Option<String>)>,
) {
    let session_arg = claude_session(spec.claude, &spec.program, &spec.args).map(String::from);
    let mut state = shared.lock();
    let id = state.next_pane;
    let (program, args) = command(id);
    let pty_spec = SpawnSpec {
        program,
        args,
        cwd: cwd.clone(),
        env: if container.is_some() { Vec::new() } else { spec.env.clone() },
        base_env: Some(shared.base_env.clone()),
        cols: spec.cols,
        rows: spec.rows,
    };
    let (pty, reader) = match PtyProcess::spawn(&pty_spec) {
        Ok(pair) => pair,
        Err(err) => {
            state.control(&DaemonMessage::SpawnFailed { request, error: format!("{err:#}") });
            return;
        }
    };
    state.next_pane += 1;
    state.focus_seq += 1;
    let exit = pty.exit_signal();
    let start = pty.pid().and_then(claude_index::host::process_start);
    let mut term = match spec.scrollback {
        Some(lines) => TermState::with_scrollback(spec.cols, spec.rows, lines as usize),
        None => TermState::new(spec.cols, spec.rows),
    };
    term.set_palette(state.palette);
    let pane = Pane {
        pty,
        term,
        program: spec.program,
        args: spec.args,
        cwd: cwd.clone(),
        session_arg,
        claude: spec.claude,
        start,
        container_id: container.as_ref().map(|(_, id, _)| id.clone()),
        container_user: container.as_ref().and_then(|(_, _, user)| user.clone()),
        container: container.map(|(folder, _, _)| folder),
        last_focused: state.focus_seq,
        needs_resync: false,
    };
    state.panes.insert(id, pane);
    if let Some(client) = state.client.as_mut().filter(|client| client.id == client_id) {
        client.attached.insert(id);
    }
    state.control(&DaemonMessage::Spawned { request, pane: id, cwd });
    state.publish_sessions(false);
    drop(state);

    let output_done = Arc::new(Notify::new());
    let output = spawn_output_reader(reader, OUTPUT_QUEUE_CHUNKS);
    let (reader_shared, reader_done) = (shared.clone(), output_done.clone());
    tokio::spawn(async move {
        while let Ok(first) = output.recv().await {
            let bytes = drain_batch(&output, first, MAX_BATCH_BYTES);
            let pane_alive = {
                let mut state = reader_shared.lock();
                match state.panes.get_mut(&id) {
                    Some(pane) => {
                        pane.term.feed(&bytes);
                        let replies = pane.term.take_pty_writes();
                        if !replies.is_empty() {
                            let _ = pane.pty.write(&replies);
                        }
                        state.deliver(id, bytes);
                        true
                    }
                    None => false,
                }
            };
            if !pane_alive {
                break;
            }
            // A flooding pane keeps its channel full, so `recv` never waits; yield so connection
            // writers and other panes still run.
            tokio::task::yield_now().await;
        }
        reader_done.notify_one();
    });
    let exit_shared = shared.clone();
    tokio::spawn(async move {
        let code = exit.recv().await.unwrap_or(1);
        // Let the reader deliver the program's last output before announcing the exit.
        let _ = tokio::time::timeout(EXIT_DRAIN, output_done.notified()).await;
        let mut state = exit_shared.lock();
        // A lagging client would otherwise never see the final screen: send it before `Exited`.
        let lagging_outbox = state
            .client
            .as_ref()
            .filter(|client| client.attached.contains(&id))
            .map(|client| client.outbox.clone())
            .filter(|_| state.panes.get(&id).is_some_and(|pane| pane.needs_resync));
        if let (Some(outbox), Some(pane)) = (lagging_outbox, state.panes.get_mut(&id)) {
            State::send_resync(&outbox, id, pane);
        }
        if let Some(pane) = state.panes.remove(&id)
            && let Some(pid) = pane.pty.pid()
        {
            state.gone.push(Gone { pid, start: pane.start.clone() });
        }
        state.control(&DaemonMessage::Exited { pane: id, code });
        state.publish_sessions(false);
        exit_shared.stop_if_idle(&state);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sessions_are_sent_only_once_the_first_scan_is_done() {
        use claude_index::index::IndexState;
        let outbox = Outbox::new(1 << 20);
        let mut state = State { sessions_enabled: true, ..State::default() };
        state.client =
            Some(Client { id: 1, outbox: outbox.clone(), attached: HashSet::new(), kicked: Arc::new(Notify::new()) });
        // A client connects (ListPanes) before the index has scanned: an empty list would claim no
        // session exists, so nothing is sent yet.
        state.publish_sessions(true);
        let record = SessionRecord {
            id: "s".into(),
            cwd: "/".into(),
            title: "t".into(),
            last_active_ms: 1,
            state: IndexState::Closed,
            cwd_exists: true,
            activity: None,
            activity_since_ms: 0,
            proc_start: None,
            container_id: None,
            claude_dir: None,
            last_prompt: None,
        };
        state.records = Some(Arc::new(vec![record]));
        state.publish_sessions(true);
        outbox.close();
        let mut messages = Vec::new();
        while let Some(frame) = outbox.next().await {
            messages.push(frame.parse::<DaemonMessage>().unwrap().unwrap());
        }
        assert!(matches!(&messages[..], [DaemonMessage::Sessions { sessions }] if sessions.len() == 1), "{messages:?}");
    }

    #[tokio::test]
    async fn dirs_are_announced_once_every_indexed_dir_has_scanned() {
        let outbox = Outbox::new(1 << 20);
        let mut state = State { dirs_pending: true, ..State::default() };
        state.client =
            Some(Client { id: 1, outbox: outbox.clone(), attached: HashSet::new(), kicked: Arc::new(Notify::new()) });
        let dir = PathBuf::from("/claude");
        state.indexers.insert(dir.clone(), tokio::spawn(async {}));
        state.announce_dirs_indexed();
        assert!(state.dirs_pending, "the dir has not been scanned");
        state.dir_records.insert(dir, Arc::new(Vec::new()));
        state.announce_dirs_indexed();
        state.announce_dirs_indexed();
        outbox.close();
        let mut messages = Vec::new();
        while let Some(frame) = outbox.next().await {
            messages.push(frame.parse::<DaemonMessage>().unwrap().unwrap());
        }
        assert!(matches!(&messages[..], [DaemonMessage::ClaudeDirsIndexed]), "{messages:?}");
    }

    #[tokio::test]
    async fn set_palette_applies_to_existing_and_new_panes() {
        use app_core::palette::LIGHT;
        let shared = Arc::new(Shared {
            state: Mutex::new(State { next_pane: 1, ..State::default() }),
            shutdown: Notify::new(),
            base_env: std::env::vars().collect(),
            owner_uid: None,
            instance: String::new(),
            docker: None,
            claude_dir: None,
            search: None,
            paste_dir: None,
        });
        let spec = || SpawnRequest {
            program: "sleep".into(),
            args: vec!["30".into()],
            cwd: "/".into(),
            cols: 80,
            rows: 24,
            cwd_from: None,
            container: None,
            claude: false,
            env: Vec::new(),
            scrollback: None,
        };
        handle_message(&shared, 1, ClientMessage::Spawn { request: 1, spec: spec() });
        handle_message(&shared, 1, ClientMessage::SetPalette(LIGHT.into()));
        handle_message(&shared, 1, ClientMessage::Spawn { request: 2, spec: spec() });
        let mut state = shared.lock();
        assert_eq!(state.panes.len(), 2);
        for pane in state.panes.values_mut() {
            pane.term.feed(b"\x1b]11;?\x07");
            assert_eq!(String::from_utf8(pane.term.take_pty_writes()).unwrap(), "\x1b]11;rgb:f8f8/f9f9/fcfc\x07");
        }
        let ids: Vec<PaneId> = state.panes.keys().copied().collect();
        drop(state);
        for pane in ids {
            handle_message(&shared, 1, ClientMessage::Kill { pane });
        }
    }

    #[tokio::test]
    async fn scrollback_request_sets_history() {
        let shared = Arc::new(Shared {
            state: Mutex::new(State { next_pane: 1, ..State::default() }),
            shutdown: Notify::new(),
            base_env: std::env::vars().collect(),
            owner_uid: None,
            instance: String::new(),
            docker: None,
            claude_dir: None,
            search: None,
            paste_dir: None,
        });
        let spec = SpawnRequest {
            program: "sh".into(),
            args: vec!["-c".into(), "seq 1 500; sleep 5".into()],
            cwd: "/".into(),
            cols: 80,
            rows: 24,
            cwd_from: None,
            container: None,
            claude: false,
            env: Vec::new(),
            scrollback: Some(50),
        };
        handle_message(&shared, 1, ClientMessage::Spawn { request: 1, spec });
        let deadline = Instant::now() + Duration::from_secs(10);
        let top = loop {
            assert!(Instant::now() < deadline, "timed out");
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut state = shared.lock();
            let Some(pane) = state.panes.values_mut().next() else { continue };
            pane.term.scroll_viewport(1_000_000);
            let top = pane.term.row_text(0);
            pane.term.scroll_to_bottom();
            if top == "428" {
                break top;
            }
        };
        assert_eq!(top, "428");
        let ids: Vec<PaneId> = shared.lock().panes.keys().copied().collect();
        for pane in ids {
            handle_message(&shared, 1, ClientMessage::Kill { pane });
        }
    }

    #[test]
    fn a_panic_while_holding_the_state_lock_does_not_wedge_the_daemon() {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            shutdown: Notify::new(),
            base_env: Vec::new(),
            owner_uid: None,
            instance: String::new(),
            docker: None,
            claude_dir: None,
            search: None,
            paste_dir: None,
        });
        let poisoner = shared.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock();
            panic!("simulated pane bug");
        })
        .join();
        assert!(shared.state.is_poisoned());
        shared.lock().next_pane = 7;
        assert_eq!(shared.lock().next_pane, 7);
    }

    #[test]
    fn closing_a_container_pane_signals_only_its_verified_processes() {
        use claude_index::host::Marked;
        let marked = [
            Marked { pid: 10, value: "i:3".into(), start: Some("5".into()) },
            Marked { pid: 11, value: "i:3".into(), start: None },
            Marked { pid: 12, value: "i:30".into(), start: Some("6".into()) },
            Marked { pid: 13, value: "other:3".into(), start: Some("7".into()) },
        ];
        assert_eq!(marked_targets(&marked, "i:3"), vec![take_over::Target { pid: 10, start: "5".into() }]);
    }

    #[test]
    fn marks_are_read_only_for_this_daemons_live_panes() {
        use claude_index::host::Marked;
        let marked = [
            Marked { pid: 10, value: "i:3".into(), start: None },
            Marked { pid: 11, value: "i:9".into(), start: None },
            Marked { pid: 12, value: "j:3".into(), start: None },
            Marked { pid: 13, value: "i:x".into(), start: None },
        ];
        assert_eq!(pane_marks(&marked, "i", &[3, 4]), HashMap::from([(10, 3)]));
    }

    #[test]
    fn new_panes_ask_docker_only_when_the_last_lookup_cannot_answer() {
        use claude_index::containers::{Container, Mount};
        struct One;
        impl claude_index::containers::Docker for One {
            fn list(&self) -> Result<Vec<Container>, String> {
                Ok(vec![Container {
                    id: "c1".into(),
                    name: "box".into(),
                    running: true,
                    created: "1".into(),
                    host_folder: "/h/app".into(),
                    mounts: vec![Mount { source: "/h/app".into(), destination: "/workspaces/app".into() }],
                    remote_user: None,
                    config_dir: None,
                }])
            }
            fn start(&self, _id: &str) -> Result<(), String> {
                Ok(())
            }
        }
        let mut registry = Registry::default();
        let now = Instant::now();
        // Nothing known yet: ask Docker.
        assert_eq!(cached_answer(&registry, Path::new("/h/app"), None, now), None);
        registry.refresh(&One).unwrap();
        let hit = cached_answer(&registry, Path::new("/h/app/src"), None, now).unwrap().unwrap();
        assert_eq!((hit.name.as_str(), hit.host_folder.as_path(), hit.running), ("box", Path::new("/h/app"), true));
        // A folder no known container mounts: ask Docker, unless it just failed.
        assert_eq!(cached_answer(&registry, Path::new("/elsewhere"), None, now), None);
        assert_eq!(
            cached_answer(&registry, Path::new("/elsewhere"), Some(now), now + Duration::from_secs(5)),
            Some(None)
        );
        assert_eq!(cached_answer(&registry, Path::new("/elsewhere"), Some(now), now + Duration::from_secs(60)), None);
    }
}
