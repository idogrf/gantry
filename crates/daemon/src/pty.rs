//! Processes running inside a pseudo-terminal (Unix PTY / Windows ConPTY).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Everything needed to start a process in a PTY.
#[derive(Clone, Debug)]
pub struct SpawnSpec {
    /// Program to run (looked up on `PATH`).
    pub program: String,
    /// Arguments passed to the program.
    pub args: Vec<String>,
    /// Working directory of the process.
    pub cwd: PathBuf,
    /// Extra environment variables, applied after the terminal defaults.
    pub env: Vec<(String, String)>,
    /// Complete environment to start from; `None` inherits the current process environment.
    pub base_env: Option<Vec<(String, String)>>,
    /// Initial terminal width in columns.
    pub cols: u16,
    /// Initial terminal height in rows.
    pub rows: u16,
}

/// The user's default interactive shell: `$SHELL` (or `/bin/sh`) on Unix. On Windows PowerShell (`pwsh`, else the
/// built-in `powershell`), as Windows Terminal does, else `%COMSPEC%` (or `cmd.exe`).
pub fn default_shell() -> String {
    #[cfg(unix)]
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    #[cfg(windows)]
    let shell = ["pwsh", "powershell"]
        .into_iter()
        .find(|name| claude_index::process::find_program(name).is_some())
        .map(|name| format!("{name}.exe"))
        .unwrap_or_else(|| std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()));
    shell
}

/// A running process attached to a PTY. Dropping it kills the process.
pub struct PtyProcess {
    master: Box<dyn MasterPty + Send>,
    input: mpsc::Sender<Vec<u8>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    exit: async_channel::Receiver<u32>,
    pid: Option<u32>,
}

impl PtyProcess {
    /// Start `spec` in a new PTY (an empty program runs the default shell). Returns the process and a blocking reader of its output.
    ///
    /// `TERM=xterm-256color` and `COLORTERM=truecolor` are set before `spec.env`.
    pub fn spawn(spec: &SpawnSpec) -> anyhow::Result<(PtyProcess, Box<dyn Read + Send>)> {
        let pair = native_pty_system().openpty(pty_size(spec.cols, spec.rows))?;
        let program = if spec.program.is_empty() { default_shell() } else { spec.program.clone() };
        let mut cmd = CommandBuilder::new(program);
        cmd.args(&spec.args);
        cmd.cwd(&spec.cwd);
        if let Some(env) = &spec.base_env {
            cmd.env_clear();
            for (key, value) in env {
                cmd.env(key, value);
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        let mut child = pair.slave.spawn_command(cmd)?;
        let pid = child.process_id();
        drop(pair.slave);
        let killer = child.clone_killer();
        // The waiter reaps the child and reports its exit even when the output stream stays open
        // (Windows ConPTY, or a background job still holding the terminal).
        let (exit_tx, exit) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let code = child.wait().map(|status| status.exit_code()).unwrap_or(1);
            let _ = exit_tx.send_blocking(code);
        });
        let reader = pair.master.try_clone_reader()?;
        let input = spawn_input_writer(pair.master.take_writer()?);
        Ok((PtyProcess { master: pair.master, input, killer, exit, pid }, reader))
    }

    /// Queue bytes for the process's input. Never blocks: a writer thread delivers them as the
    /// process reads, so a program that is not reading cannot stall the caller.
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.input
            .send(bytes.to_vec())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty input closed"))
    }

    /// Receiver that yields the process's exit code once it exits.
    pub fn exit_signal(&self) -> async_channel::Receiver<u32> {
        self.exit.clone()
    }

    /// Tell the PTY (and so the process) about a new terminal size.
    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.master.resize(pty_size(cols, rows))
    }

    /// Process id of the program, when the platform reports one.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Current working directory of the process, where the platform exposes it (Linux).
    pub fn current_dir(&self) -> Option<PathBuf> {
        let pid = self.pid?;
        if cfg!(target_os = "linux") { std::fs::read_link(format!("/proc/{pid}/cwd")).ok() } else { None }
    }

    /// Terminate the process.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.killer.kill()
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

fn spawn_input_writer(mut writer: Box<dyn Write + Send>) -> mpsc::Sender<Vec<u8>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        for bytes in rx {
            if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
                break;
            }
        }
    });
    tx
}

/// Read PTY output on a background thread, delivering chunks through the returned channel.
///
/// At most `capacity` chunks are buffered. When the consumer falls behind, the thread stops
/// reading, the kernel PTY buffer fills, and the program blocks on its own writes — so a flood
/// of output cannot grow memory without limit.
pub fn spawn_output_reader(mut reader: Box<dyn Read + Send>, capacity: usize) -> async_channel::Receiver<Vec<u8>> {
    let (tx, rx) = async_channel::bounded(capacity);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send_blocking(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

/// Append chunks already waiting in `rx` to `first`, stopping once the batch reaches `max_bytes`.
pub fn drain_batch(rx: &async_channel::Receiver<Vec<u8>>, mut first: Vec<u8>, max_bytes: usize) -> Vec<u8> {
    while first.len() < max_bytes
        && let Ok(more) = rx.try_recv()
    {
        first.extend_from_slice(&more);
    }
    first
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::{Duration, Instant};

    fn read_until(mut reader: Box<dyn Read + Send>, needle: &str, timeout: Duration) -> String {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let deadline = Instant::now() + timeout;
        let mut out = String::new();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => {
                    out.push_str(&String::from_utf8_lossy(&chunk));
                    if out.contains(needle) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        out
    }

    /// An endless output source that counts how many times it was read.
    struct Endless(Arc<AtomicUsize>);

    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.fetch_add(1, Ordering::SeqCst);
            buf.fill(b'y');
            Ok(buf.len())
        }
    }

    #[test]
    fn reader_stops_reading_while_consumer_lags() {
        let reads = Arc::new(AtomicUsize::new(0));
        let _rx = spawn_output_reader(Box::new(Endless(reads.clone())), 4);
        std::thread::sleep(Duration::from_millis(200));
        let count = reads.load(Ordering::SeqCst);
        assert!(count <= 6, "reader kept going while nobody consumed: {count} reads");
    }

    #[test]
    fn batches_are_capped() {
        let (tx, rx) = async_channel::unbounded();
        for _ in 0..10 {
            tx.send_blocking(vec![0u8; 100_000]).unwrap();
        }
        let batch = drain_batch(&rx, vec![0u8; 100_000], 256_000);
        assert_eq!(batch.len(), 300_000);
        assert_eq!(rx.len(), 8);
    }

    #[cfg(unix)]
    fn sh(script: &str, cwd: &std::path::Path) -> SpawnSpec {
        SpawnSpec {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: cwd.to_path_buf(),
            env: Vec::new(),
            base_env: None,
            cols: 80,
            rows: 24,
        }
    }

    #[cfg(unix)]
    #[test]
    fn output_is_readable() {
        let dir = tempfile::tempdir().unwrap();
        let (_pty, reader) = PtyProcess::spawn(&sh("printf hello-pty", dir.path())).unwrap();
        assert!(read_until(reader, "hello-pty", Duration::from_secs(5)).contains("hello-pty"));
    }

    #[cfg(unix)]
    #[test]
    fn terminal_env_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let (_pty, reader) = PtyProcess::spawn(&sh("printf \"$TERM/$COLORTERM\"", dir.path())).unwrap();
        let out = read_until(reader, "truecolor", Duration::from_secs(5));
        assert!(out.contains("xterm-256color/truecolor"), "got: {out:?}");
    }

    #[cfg(unix)]
    #[test]
    fn extra_env_and_cwd_are_applied() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = sh("printf \"$GANTRY_TEST_VAR:\"; pwd -P", dir.path());
        spec.env.push(("GANTRY_TEST_VAR".into(), "set".into()));
        let (_pty, reader) = PtyProcess::spawn(&spec).unwrap();
        let expected = dir.path().canonicalize().unwrap().display().to_string();
        let out = read_until(reader, &expected, Duration::from_secs(5));
        assert!(out.contains(&format!("set:{expected}")), "got: {out:?}");
    }

    #[cfg(unix)]
    #[test]
    fn resize_reaches_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let (pty, reader) = PtyProcess::spawn(&sh("sleep 0.5; stty size", dir.path())).unwrap();
        pty.resize(100, 30).unwrap();
        let out = read_until(reader, "30 100", Duration::from_secs(5));
        assert!(out.contains("30 100"), "got: {out:?}");
    }

    #[cfg(unix)]
    #[test]
    fn written_input_reaches_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let (mut pty, reader) = PtyProcess::spawn(&sh("read line; printf \"got:$line\"", dir.path())).unwrap();
        pty.write(b"abc\r").unwrap();
        let out = read_until(reader, "got:abc", Duration::from_secs(5));
        assert!(out.contains("got:abc"), "got: {out:?}");
    }

    #[cfg(unix)]
    #[test]
    fn writing_to_a_program_that_does_not_read_never_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let (mut pty, _reader) = PtyProcess::spawn(&sh("stty raw -echo; sleep 3", dir.path())).unwrap();
        std::thread::sleep(Duration::from_millis(300)); // let `stty raw` take effect
        let started = Instant::now();
        pty.write(&vec![b'x'; 1_000_000]).unwrap();
        assert!(started.elapsed() < Duration::from_millis(500), "write blocked for {:?}", started.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn exit_is_reported_while_a_background_job_holds_the_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let (pty, _reader) = PtyProcess::spawn(&sh("sleep 5 & exit 3", dir.path())).unwrap();
        let exit = pty.exit_signal();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(exit.recv_blocking().ok());
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(Some(3)));
    }

    #[cfg(unix)]
    #[test]
    fn base_env_replaces_the_inherited_environment() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = sh("printf \"[$GANTRY_BASE][$HOME]\"", dir.path());
        spec.base_env = Some(vec![
            ("GANTRY_BASE".into(), "from-login-shell".into()),
            ("PATH".into(), std::env::var("PATH").unwrap()),
        ]);
        let (_pty, reader) = PtyProcess::spawn(&spec).unwrap();
        let out = read_until(reader, "]", Duration::from_secs(5));
        assert!(out.contains("[from-login-shell][]"), "got: {out:?}");
    }

    #[cfg(windows)]
    #[test]
    fn output_is_readable() {
        let dir = tempfile::tempdir().unwrap();
        let spec = SpawnSpec {
            program: "cmd.exe".into(),
            args: vec!["/C".into(), "echo hello-pty".into()],
            cwd: dir.path().to_path_buf(),
            env: Vec::new(),
            base_env: None,
            cols: 80,
            rows: 24,
        };
        let (mut pty, reader) = PtyProcess::spawn(&spec).unwrap();
        // ConPTY asks where the cursor is (`ESC[6n`) and holds all output until it is told; in a pane the terminal
        // state answers.
        pty.write(b"\x1b[1;1R").unwrap();
        assert!(read_until(reader, "hello-pty", Duration::from_secs(10)).contains("hello-pty"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn current_dir_follows_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().canonicalize().unwrap();
        let script = format!("cd '{}' && sleep 5", target.display());
        let (pty, _reader) = PtyProcess::spawn(&sh(&script, &std::env::temp_dir())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pty.current_dir().as_deref() != Some(target.as_path()) {
            assert!(Instant::now() < deadline, "cwd never followed: {:?}", pty.current_dir());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
