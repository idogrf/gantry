//! The sign-in side channel of an SSH attempt. ssh runs `gantry askpass "<prompt>"` when it needs input; that helper
//! asks the GUI over a local socket (a unix socket, or a named pipe on Windows) this module serves for the length of one attempt, and prints the answer.
//! Answers are never stored or logged.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// What the user answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskAnswer {
    /// The text typed (or `yes` / `no`).
    Answer(String),
    /// The dialog was cancelled or closed.
    Cancel,
}

/// One question from ssh, waiting for the GUI.
pub struct AskPrompt {
    /// The host the attempt connects to.
    pub host: String,
    /// What ssh asked, as ssh worded it.
    pub prompt: String,
    /// Where the GUI sends the answer; dropping it without sending counts as a cancel.
    pub reply: async_channel::Sender<AskAnswer>,
}

/// What happened to the questions of one attempt, shared between the server and the transport.
#[derive(Default)]
pub struct AskpassState {
    asked: AtomicBool,
    cancelled: AtomicBool,
    wake: tokio::sync::Notify,
    /// Questions that wait for the user right now.
    pending: AtomicUsize,
    /// Signalled whenever `pending` changes.
    changed: tokio::sync::Notify,
}

impl AskpassState {
    /// Whether any question reached the GUI.
    pub fn asked(&self) -> bool {
        self.asked.load(Ordering::SeqCst)
    }

    /// Whether a question was cancelled (the dialog closed or answered Cancel).
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Whether a question waits for the user (the handshake timeout does not run meanwhile).
    pub fn waiting(&self) -> bool {
        self.pending.load(Ordering::SeqCst) > 0
    }

    /// Resolves when a question starts or ends waiting.
    pub async fn wait_changed(&self) {
        self.changed.notified().await;
    }

    #[cfg(any(unix, windows))]
    fn set_waiting(&self, waiting: bool) {
        if waiting {
            self.pending.fetch_add(1, Ordering::SeqCst);
        } else {
            self.pending.fetch_sub(1, Ordering::SeqCst);
        }
        self.changed.notify_one();
    }

    /// Resolves once a question was cancelled.
    pub async fn wait_cancelled(&self) {
        while !self.cancelled() {
            self.wake.notified().await;
        }
    }

    #[cfg(any(unix, windows))]
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Request {
    prompt: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    answer: Option<String>,
}

#[cfg(any(unix, windows))]
mod imp {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use interprocess::local_socket::tokio::{Stream as AsyncStream, prelude::*};
    use interprocess::local_socket::{ListenerOptions, Name, Stream, prelude::*};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    use super::*;

    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// Serves the questions of one attempt until dropped.
    pub struct AskpassServer {
        path: PathBuf,
        task: tokio::task::JoinHandle<()>,
    }

    impl AskpassServer {
        /// Listen on a fresh socket file in `dir` (the profile's 0700 runtime dir; on Windows an owner-only named pipe
        /// with an unguessable name instead) and forward questions to `prompts`,
        /// on behalf of the host called `host`. After one question is cancelled, `state` makes every later one of this
        /// attempt answer Cancel at once, without asking.
        pub async fn start(
            dir: &Path,
            host: &str,
            prompts: async_channel::Sender<AskPrompt>,
            state: Arc<AskpassState>,
        ) -> io::Result<AskpassServer> {
            let id = format!("askpass-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
            #[cfg(unix)]
            let (path, options) = {
                std::fs::create_dir_all(dir)?;
                let path = dir.join(format!("{id}.sock"));
                let _ = std::fs::remove_file(&path);
                (path, ListenerOptions::new())
            };
            #[cfg(windows)]
            let (path, options) = {
                use interprocess::os::windows::local_socket::ListenerOptionsExt;
                use interprocess::os::windows::security_descriptor::SecurityDescriptor;
                let _ = dir;
                // Only the current user may connect.
                let owner_only = SecurityDescriptor::deserialize(widestring::u16cstr!("D:P(A;;GA;;;OW)"))?;
                (
                    PathBuf::from(format!("gantry-{id}-{}", uuid::Uuid::new_v4())),
                    ListenerOptions::new().security_descriptor(owner_only),
                )
            };
            let listener = options.name(socket_name(&path)?).create_tokio()?;
            let host = host.to_string();
            let task = tokio::spawn(async move {
                while let Ok(stream) = listener.accept().await {
                    tokio::spawn(serve(stream, host.clone(), prompts.clone(), state.clone()));
                }
            });
            Ok(AskpassServer { path, task })
        }

        /// The socket file (the pipe name on Windows) ssh's helper connects to.
        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for AskpassServer {
        fn drop(&mut self) {
            self.task.abort();
            #[cfg(unix)]
            let _ = std::fs::remove_file(&self.path);
        }
    }

    async fn serve(
        stream: AsyncStream,
        host: String,
        prompts: async_channel::Sender<AskPrompt>,
        state: Arc<AskpassState>,
    ) {
        let (recv, mut send) = stream.split();
        let mut line = String::new();
        if tokio::io::BufReader::new(recv).read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let Ok(request) = serde_json::from_str::<Request>(&line) else { return };
        let (reply, answers) = async_channel::bounded(1);
        let answer = if state.cancelled() {
            None
        } else {
            state.asked.store(true, Ordering::SeqCst);
            state.set_waiting(true);
            let answer = if prompts.send(AskPrompt { host, prompt: request.prompt, reply }).await.is_ok() {
                match answers.recv().await {
                    Ok(AskAnswer::Answer(text)) => Some(text),
                    _ => None,
                }
            } else {
                None
            };
            state.set_waiting(false);
            if answer.is_none() {
                state.cancel();
            }
            answer
        };
        let mut text = serde_json::to_string(&Reply { answer }).unwrap_or_default();
        text.push('\n');
        let _ = send.write_all(text.as_bytes()).await;
    }

    /// The local socket `path` names: a socket file, or a named pipe's name on Windows.
    fn socket_name(path: &Path) -> io::Result<Name<'static>> {
        #[cfg(unix)]
        return path.to_path_buf().to_fs_name::<interprocess::local_socket::GenericFilePath>();
        #[cfg(windows)]
        return path.to_string_lossy().into_owned().to_ns_name::<interprocess::local_socket::GenericNamespaced>();
    }

    /// Ask the GUI's server at `socket`; blocks until the user answers. `None` = cancelled.
    pub fn run_helper(socket: &Path, prompt: &str) -> io::Result<Option<String>> {
        let stream = Stream::connect(socket_name(socket)?)?;
        let mut stream = BufReader::new(stream);
        let mut request = serde_json::to_string(&Request { prompt: prompt.to_string() }).map_err(io::Error::other)?;
        request.push('\n');
        stream.get_mut().write_all(request.as_bytes())?;
        let mut line = String::new();
        stream.read_line(&mut line)?;
        let reply: Reply = serde_json::from_str(&line).map_err(io::Error::other)?;
        Ok(reply.answer)
    }
}

#[cfg(any(unix, windows))]
pub use imp::{AskpassServer, run_helper};

/// Sign-in dialogs need a local socket: this platform has none, so ssh attempts use keys and the agent only.
#[cfg(not(any(unix, windows)))]
pub struct AskpassServer;

#[cfg(not(any(unix, windows)))]
impl AskpassServer {
    /// Always `Unsupported` here.
    pub async fn start(
        _dir: &Path,
        _host: &str,
        _prompts: async_channel::Sender<AskPrompt>,
        _state: std::sync::Arc<AskpassState>,
    ) -> io::Result<AskpassServer> {
        Err(io::ErrorKind::Unsupported.into())
    }

    /// Never reached: `start` fails first.
    pub fn path(&self) -> &Path {
        Path::new("")
    }
}

/// Always `Unsupported` here.
#[cfg(not(any(unix, windows)))]
pub fn run_helper(_socket: &Path, _prompt: &str) -> io::Result<Option<String>> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
    }

    #[test]
    fn the_helper_gets_the_users_answer_and_a_cancel_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let (prompts_tx, prompts_rx) = async_channel::unbounded::<AskPrompt>();
        let server = runtime.block_on(AskpassServer::start(dir.path(), "devbox", prompts_tx, Arc::default())).unwrap();
        let socket = server.path().to_path_buf();
        #[cfg(unix)]
        assert!(socket.exists());

        let helper_socket = socket.clone();
        let first = std::thread::spawn(move || run_helper(&helper_socket, "me@devbox's password: "));
        let prompt = runtime.block_on(prompts_rx.recv()).unwrap();
        assert_eq!((prompt.host.as_str(), prompt.prompt.as_str()), ("devbox", "me@devbox's password: "));
        runtime.block_on(prompt.reply.send(AskAnswer::Answer("hunter2".into()))).unwrap();
        assert_eq!(first.join().unwrap().unwrap(), Some("hunter2".to_string()));

        let helper_socket = socket.clone();
        let second = std::thread::spawn(move || run_helper(&helper_socket, "Verification code: "));
        let prompt = runtime.block_on(prompts_rx.recv()).unwrap();
        runtime.block_on(prompt.reply.send(AskAnswer::Cancel)).unwrap();
        assert_eq!(second.join().unwrap().unwrap(), None);

        // The socket goes away with the server.
        drop(server);
        #[cfg(unix)]
        assert!(!socket.exists());
        assert!(run_helper(&socket, "Password: ").is_err());
    }

    #[test]
    fn a_dialog_that_is_dropped_answers_cancel_instead_of_hanging_ssh() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let (prompts_tx, prompts_rx) = async_channel::unbounded::<AskPrompt>();
        let server = runtime.block_on(AskpassServer::start(dir.path(), "devbox", prompts_tx, Arc::default())).unwrap();
        let socket = server.path().to_path_buf();
        let helper = std::thread::spawn(move || run_helper(&socket, "Password: "));
        let prompt = runtime.block_on(prompts_rx.recv()).unwrap();
        drop(prompt); // the reply sender goes away without an answer
        assert_eq!(helper.join().unwrap().unwrap(), None);
    }

    #[test]
    fn after_a_cancel_every_later_question_of_the_attempt_is_cancelled_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let (prompts_tx, prompts_rx) = async_channel::unbounded::<AskPrompt>();
        let state = Arc::new(AskpassState::default());
        let server = runtime.block_on(AskpassServer::start(dir.path(), "devbox", prompts_tx, state.clone())).unwrap();
        let socket = server.path().to_path_buf();
        assert!(!state.asked() && !state.cancelled());
        let first_socket = socket.clone();
        let first = std::thread::spawn(move || run_helper(&first_socket, "Password: "));
        let prompt = runtime.block_on(prompts_rx.recv()).unwrap();
        assert!(state.asked());
        runtime.block_on(prompt.reply.send(AskAnswer::Cancel)).unwrap();
        assert_eq!(first.join().unwrap().unwrap(), None);
        assert!(state.cancelled());
        // ssh asks again (it retries an empty password): the dialog never opens.
        assert_eq!(run_helper(&socket, "Password: ").unwrap(), None);
        assert!(prompts_rx.try_recv().is_err(), "no second prompt reached the GUI");
    }

    #[test]
    fn with_nobody_listening_for_prompts_the_helper_is_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime();
        let (prompts_tx, prompts_rx) = async_channel::unbounded::<AskPrompt>();
        let server = runtime.block_on(AskpassServer::start(dir.path(), "devbox", prompts_tx, Arc::default())).unwrap();
        drop(prompts_rx);
        let socket = server.path().to_path_buf();
        assert_eq!(std::thread::spawn(move || run_helper(&socket, "Password: ")).join().unwrap().unwrap(), None);
    }

    #[test]
    fn the_helper_without_a_server_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run_helper(&dir.path().join("nothing.sock"), "Password: ").is_err());
    }
}
