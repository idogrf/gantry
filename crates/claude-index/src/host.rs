//! Process and filesystem queries about the machine sessions run on (phase 2 adds remote hosts).

use std::path::{Path, PathBuf};

/// What the index needs to know about a host.
pub trait Host {
    /// Whether process `pid` runs and — when `start` is given — started at `start` (guards against pid
    /// reuse). `None` when this host cannot tell.
    fn is_running(&self, pid: u32, start: Option<&str>) -> Option<bool>;
    /// This host's own pid namespace in Claude's `pidDomain` format; `None` when unknown.
    fn pid_domain(&self) -> Option<String>;
    /// Whether `path` is an existing directory.
    fn dir_exists(&self, path: &Path) -> bool;
    /// The host process behind `pid` from the foreign pid namespace `domain` (verified by `start` when given):
    /// `Some(None)` when there is none (it exited), `None` when this host cannot tell.
    fn namespaced(&self, domain: &str, pid: u32, start: Option<&str>) -> Option<Option<Namespaced>> {
        let _ = (domain, pid, start);
        None
    }
}

/// A process from another pid namespace, found on this host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Namespaced {
    /// Its pid on this host.
    pub host_pid: u32,
    /// The Docker container it runs in (from its cgroup).
    pub container: Option<String>,
}

/// A process whose environment sets a given variable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marked {
    /// Host pid.
    pub pid: u32,
    /// The variable's value.
    pub value: String,
    /// Its start time (`/proc/<pid>/stat` field 22).
    pub start: Option<String>,
}

/// The machine Gantry runs on.
pub struct LocalHost {
    domain: Option<String>,
}

impl LocalHost {
    /// Query the local machine (reads its pid namespace once).
    pub fn new() -> LocalHost {
        LocalHost { domain: local_pid_domain() }
    }
}

impl Default for LocalHost {
    fn default() -> Self {
        LocalHost::new()
    }
}

impl Host for LocalHost {
    fn is_running(&self, pid: u32, start: Option<&str>) -> Option<bool> {
        #[cfg(target_os = "linux")]
        {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return Some(false) };
            // A zombie has exited; only its parent has not collected it yet.
            if proc_state(&stat) == Some("Z") {
                return Some(false);
            }
            Some(start.is_none_or(|start| proc_start_time(&stat).as_deref() == Some(start)))
        }
        #[cfg(windows)]
        {
            let Some(started) = windows::start_secs(pid) else { return Some(false) };
            Some(start.is_none_or(|start| windows::filetime_secs(start) == Some(started)))
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = (pid, start);
            None
        }
    }

    fn pid_domain(&self) -> Option<String> {
        self.domain.clone()
    }

    fn dir_exists(&self, path: &Path) -> bool {
        path.is_dir()
    }

    #[cfg(target_os = "linux")]
    fn namespaced(&self, domain: &str, pid: u32, start: Option<&str>) -> Option<Option<Namespaced>> {
        Some(find_namespaced(Path::new("/proc"), domain, pid, start))
    }
}

fn numeric_dirs(proc_root: &Path) -> impl Iterator<Item = (u32, PathBuf)> {
    std::fs::read_dir(proc_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| Some((entry.file_name().to_str()?.parse().ok()?, entry.path())))
}

/// The host process (under `proc_root`, normally `/proc`) that is `pid` in the pid namespace `domain` names
/// (`…pid:[<inode>]`) and — when given — started at `start`. Another user's process (a root container) hides its
/// `ns/pid` link; it matches by innermost pid, start time (required then) and a Docker cgroup instead.
pub fn find_namespaced(proc_root: &Path, domain: &str, pid: u32, start: Option<&str>) -> Option<Namespaced> {
    let namespace = &domain[domain.find("pid:[")?..];
    numeric_dirs(proc_root).find_map(|(host_pid, dir)| {
        let same_namespace = match std::fs::read_link(dir.join("ns/pid")) {
            Ok(link) if link.as_os_str() != namespace => return None,
            Ok(_) => true,
            Err(_) => false,
        };
        if inner_pid(&dir)? != pid {
            return None;
        }
        let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
        let started = proc_start_time(&stat);
        if start.is_some_and(|start| started.as_deref() != Some(start)) || (!same_namespace && start.is_none()) {
            return None;
        }
        let cgroup = std::fs::read_to_string(dir.join("cgroup")).ok();
        let container = cgroup.and_then(|c| crate::containers::container_from_cgroup(&c));
        (same_namespace || container.is_some()).then_some(Namespaced { host_pid, container })
    })
}

/// The innermost pid (`NSpid`'s last entry) of the process in `dir`.
fn inner_pid(dir: &Path) -> Option<u32> {
    let status = std::fs::read_to_string(dir.join("status")).ok()?;
    status.lines().find_map(|line| line.strip_prefix("NSpid:"))?.split_whitespace().last()?.parse().ok()
}

/// The host pid and start time of the process that is `pid` inside the Docker container `container_id` (a full id
/// or a prefix of one). Works for any user's processes.
pub fn in_container(proc_root: &Path, container_id: &str, pid: u32) -> Option<(u32, Option<String>)> {
    numeric_dirs(proc_root).find_map(|(host_pid, dir)| {
        if inner_pid(&dir)? != pid {
            return None;
        }
        let cgroup = std::fs::read_to_string(dir.join("cgroup")).ok()?;
        let id = crate::containers::container_from_cgroup(&cgroup)?;
        if !(id.starts_with(container_id) || container_id.starts_with(&id)) {
            return None;
        }
        let start = std::fs::read_to_string(dir.join("stat")).ok().and_then(|stat| proc_start_time(&stat));
        Some((host_pid, start))
    })
}

/// Processes (under `proc_root`) whose environment sets `key`, with its value. Only that variable is kept.
pub fn processes_with_env(proc_root: &Path, key: &str) -> Vec<Marked> {
    let prefix = format!("{key}=");
    numeric_dirs(proc_root)
        .filter_map(|(pid, dir)| {
            let environ = std::fs::read(dir.join("environ")).ok()?;
            let value = environ.split(|byte| *byte == 0).find_map(|var| var.strip_prefix(prefix.as_bytes()))?;
            let value = String::from_utf8(value.to_vec()).ok()?;
            let start = std::fs::read_to_string(dir.join("stat")).ok().and_then(|stat| proc_start_time(&stat));
            Some(Marked { pid, value, start })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn local_pid_domain() -> Option<String> {
    let machine = std::fs::read_to_string("/etc/machine-id").ok()?;
    let namespace = std::fs::read_link("/proc/self/ns/pid").ok()?;
    Some(format!("linux:{}:{}", machine.trim(), namespace.display()))
}

#[cfg(not(target_os = "linux"))]
fn local_pid_domain() -> Option<String> {
    None
}

/// Field 22 (start time) of a `/proc/<pid>/stat` line. The command name (field 2) may contain spaces and
/// parentheses, so fields are counted after its last `)`.
pub fn proc_start_time(stat: &str) -> Option<String> {
    // After the command name come fields 3, 4, …; field 22 is the 20th of them.
    stat.rsplit_once(')')?.1.split_whitespace().nth(19).map(Into::into)
}

/// Field 3 (state: `R`, `S`, `Z`, …) of a `/proc/<pid>/stat` line.
#[cfg(target_os = "linux")]
fn proc_state(stat: &str) -> Option<&str> {
    stat.rsplit_once(')')?.1.split_whitespace().next()
}

/// Start time of process `pid` in Claude's `procStart` format (`/proc/<pid>/stat` field 22); `None` when it is gone or
/// the platform has no `/proc`.
#[cfg(target_os = "linux")]
pub fn process_start(pid: u32) -> Option<String> {
    proc_start_time(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Start time of process `pid`; this platform cannot tell.
#[cfg(not(target_os = "linux"))]
pub fn process_start(pid: u32) -> Option<String> {
    let _ = pid;
    None
}

/// Process queries on Windows, where Claude's `procStart` is the process creation time as a FILETIME (100 ns steps
/// since 1601). It is compared to the second, which is as precise as the safe process API gets.
#[cfg(windows)]
mod windows {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    /// Seconds from 1601-01-01 (FILETIME's epoch) to 1970-01-01.
    const UNIX_EPOCH_IN_FILETIME_SECS: u64 = 11_644_473_600;

    /// A `procStart` FILETIME in Unix seconds.
    pub fn filetime_secs(start: &str) -> Option<u64> {
        (start.parse::<u64>().ok()? / 10_000_000).checked_sub(UNIX_EPOCH_IN_FILETIME_SECS)
    }

    fn with_process<T>(pid: u32, f: impl FnOnce(&sysinfo::Process) -> T) -> Option<T> {
        let pid = Pid::from_u32(pid);
        let mut system = System::new();
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, ProcessRefreshKind::nothing());
        system.process(pid).map(f)
    }

    /// When process `pid` started (Unix seconds); `None` when it is gone.
    pub fn start_secs(pid: u32) -> Option<u64> {
        with_process(pid, |process| process.start_time())
    }

    /// End process `pid` (`TerminateProcess`).
    pub fn terminate(pid: u32) -> Result<(), String> {
        match with_process(pid, |process| process.kill()) {
            Some(true) => Ok(()),
            Some(false) => Err("could not end it".into()),
            None => Err("it is gone".into()),
        }
    }
}

/// End process `pid` at once. Windows has no polite stop for a console program in another console window, so a
/// take over ends it as a force quit would.
#[cfg(windows)]
pub fn terminate(pid: u32) -> Result<(), String> {
    windows::terminate(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_checks_a_process_by_pid_and_filetime_start() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let filetime = |secs: u64| ((secs + 11_644_473_600) * 10_000_000 + 1234).to_string();
        assert_eq!(windows::filetime_secs(&filetime(now)), Some(now));
        assert_eq!(windows::filetime_secs("not a number"), None);
        let host = LocalHost::new();
        let me = std::process::id();
        let started = windows::start_secs(me).unwrap();
        assert!(started <= now && now - started < 600, "started {started}, now {now}");
        assert_eq!(host.is_running(me, None), Some(true));
        assert_eq!(host.is_running(me, Some(&filetime(started))), Some(true));
        assert_eq!(host.is_running(me, Some(&filetime(started - 5))), Some(false));
        let mut child = std::process::Command::new("cmd.exe").args(["/c", "exit"]).spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert_eq!(host.is_running(pid, None), Some(false));
    }

    #[cfg(windows)]
    #[test]
    fn windows_terminate_ends_a_process() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/c", "pause"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        terminate(child.id()).unwrap();
        assert!(!child.wait().unwrap().success());
    }

    #[test]
    fn start_time_is_field_22_even_with_odd_command_names() {
        let fields: Vec<String> = (3..=25).map(|n| n.to_string()).collect();
        let stat = format!("1234 (we ird) (name) {}", fields.join(" "));
        assert_eq!(proc_start_time(&stat).as_deref(), Some("22"));
        assert_eq!(proc_start_time("1234 (short) S 1"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn local_host_checks_pid_and_start_time() {
        let host = LocalHost::new();
        let me = std::process::id();
        let start = proc_start_time(&std::fs::read_to_string("/proc/self/stat").unwrap()).unwrap();
        assert_eq!(host.is_running(me, Some(&start)), Some(true));
        assert_eq!(host.is_running(me, None), Some(true));
        assert_eq!(host.is_running(me, Some("1")), Some(false));
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let gone = child.id();
        child.wait().unwrap();
        assert_eq!(host.is_running(gone, None), Some(false));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_start_reads_a_live_process_and_none_for_a_gone_one() {
        let me = std::process::id();
        let expected = proc_start_time(&std::fs::read_to_string("/proc/self/stat").unwrap());
        assert_eq!(process_start(me), expected);
        assert_eq!(process_start(u32::MAX), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_exited_but_unreaped_process_is_not_running() {
        let host = LocalHost::new();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        // Not waited for yet: it stays a zombie (state Z) in /proc.
        while !std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap().contains(") Z ") {
            assert!(std::time::Instant::now() < deadline, "the child never exited");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(host.is_running(pid, None), Some(false));
        child.wait().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn local_pid_domain_has_claudes_format() {
        let domain = LocalHost::new().pid_domain().unwrap();
        assert!(domain.starts_with("linux:") && domain.contains(":pid:["), "{domain}");
    }

    #[cfg(unix)]
    fn fake_process(root: &Path, host_pid: u32, ns: &str, nspid: &[u32], start: &str, cgroup: &str, environ: &[&str]) {
        let dir = root.join(host_pid.to_string());
        std::fs::create_dir_all(dir.join("ns")).unwrap();
        std::os::unix::fs::symlink(ns, dir.join("ns/pid")).unwrap();
        let ids: Vec<String> = nspid.iter().map(u32::to_string).collect();
        std::fs::write(dir.join("status"), format!("Name:\tclaude\nNSpid:\t{}\n", ids.join("\t"))).unwrap();
        let fields: Vec<String> = (3..=25).map(|n| if n == 22 { start.to_string() } else { "0".into() }).collect();
        std::fs::write(dir.join("stat"), format!("{host_pid} (claude) {}", fields.join(" "))).unwrap();
        std::fs::write(dir.join("cgroup"), cgroup).unwrap();
        std::fs::write(dir.join("environ"), environ.join("\0")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_container_pid_is_found_on_the_host_by_namespace_pid_and_start() {
        let root = tempfile::tempdir().unwrap();
        let cgroup = "0::/system.slice/docker-4a31e6.scope\n";
        fake_process(root.path(), 3299035, "pid:[4026533220]", &[3299035, 129384], "62933926", cgroup, &[]);
        fake_process(root.path(), 500, "pid:[4026531836]", &[500], "1", "0::/user.slice\n", &[]);
        let found = find_namespaced(root.path(), "linux::pid:[4026533220]", 129384, Some("62933926")).unwrap();
        assert_eq!((found.host_pid, found.container.as_deref()), (3299035, Some("4a31e6")));
        // Dead (no such pid in that namespace), and the pid reused with another start time.
        assert!(find_namespaced(root.path(), "linux::pid:[4026533223]", 365644, Some("1")).is_none());
        assert!(find_namespaced(root.path(), "linux::pid:[4026533220]", 129384, Some("999")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_processes_are_skipped() {
        // A process of another user: its ns link, status and environ cannot be read (simulated: no files).
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("77")).unwrap();
        std::fs::write(root.path().join("self"), "").unwrap();
        assert!(find_namespaced(root.path(), "linux::pid:[1]", 77, None).is_none());
        assert!(processes_with_env(root.path(), "GANTRY_PANE").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn processes_carrying_a_variable_are_found_with_its_value() {
        let root = tempfile::tempdir().unwrap();
        fake_process(root.path(), 10, "pid:[1]", &[10, 1], "5", "", &["A=1", "GANTRY_PANE=i:3", "B=2"]);
        fake_process(root.path(), 11, "pid:[1]", &[11, 2], "6", "", &["GANTRY_PANES=no"]);
        let marked = processes_with_env(root.path(), "GANTRY_PANE");
        assert_eq!(marked, vec![Marked { pid: 10, value: "i:3".into(), start: Some("5".into()) }]);
    }

    #[cfg(unix)]
    #[test]
    fn a_root_container_process_is_found_by_nspid_start_and_a_docker_cgroup() {
        // Another user's process: `ns/pid` and `environ` cannot be read, but status, stat and cgroup can.
        let root = tempfile::tempdir().unwrap();
        fake_process(root.path(), 900, "pid:[1]", &[900, 42], "77", "0::/system.slice/docker-beef.scope\n", &[]);
        std::fs::remove_file(root.path().join("900/ns/pid")).unwrap();
        fake_process(root.path(), 901, "pid:[1]", &[901, 43], "78", "0::/user.slice\n", &[]);
        std::fs::remove_file(root.path().join("901/ns/pid")).unwrap();
        let found = find_namespaced(root.path(), "linux::pid:[4026533220]", 42, Some("77")).unwrap();
        assert_eq!((found.host_pid, found.container.as_deref()), (900, Some("beef")));
        // Without a start time to check, or outside a container, nothing matches.
        assert!(find_namespaced(root.path(), "linux::pid:[4026533220]", 42, None).is_none());
        assert!(find_namespaced(root.path(), "linux::pid:[4026533220]", 43, Some("78")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_containers_process_is_found_by_its_inner_pid() {
        let root = tempfile::tempdir().unwrap();
        fake_process(root.path(), 900, "pid:[1]", &[900, 42], "77", "0::/system.slice/docker-beef01.scope\n", &[]);
        fake_process(root.path(), 901, "pid:[1]", &[901, 42], "78", "0::/system.slice/docker-cafe.scope\n", &[]);
        assert_eq!(in_container(root.path(), "beef01", 42), Some((900, Some("77".into()))));
        assert_eq!(in_container(root.path(), "beef", 42), Some((900, Some("77".into()))));
        assert_eq!(in_container(root.path(), "beef01", 7), None);
    }
}
