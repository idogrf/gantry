//! Dev containers as Docker reports them: which host folder each serves, how its paths map to the host's, and how
//! to reach the `docker` CLI. Only labels, mounts and state are read — never a container's environment.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Label VS Code puts on a dev container: the host folder it was opened from.
pub const FOLDER_LABEL: &str = "devcontainer.local_folder";
/// `docker inspect` template: id, name, running, created, host folder, metadata, mounts and image user as
/// tab-separated JSON. It never asks for `Config.Env`.
pub const INSPECT_FORMAT: &str = "{{json .Id}}\t{{json .Name}}\t{{json .State.Running}}\t{{json .Created}}\t{{json (index .Config.Labels \"devcontainer.local_folder\")}}\t{{json (index .Config.Labels \"devcontainer.metadata\")}}\t{{json .Mounts}}\t{{json .Config.User}}";
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// A bind mount: `source` on the host appears at `destination` in the container.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// Host path.
    pub source: PathBuf,
    /// Container path.
    pub destination: PathBuf,
}

/// One dev container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    /// Full container id.
    pub id: String,
    /// Container name (without Docker's leading `/`).
    pub name: String,
    /// Whether it runs.
    pub running: bool,
    /// Creation time (RFC 3339, so newer sorts later).
    pub created: String,
    /// Host folder it serves (`devcontainer.local_folder`).
    pub host_folder: PathBuf,
    /// Bind mounts.
    pub mounts: Vec<Mount>,
    /// User sessions run as: the metadata's last `remoteUser`, else the image user, else `root`.
    pub remote_user: Option<String>,
    /// `CLAUDE_CONFIG_DIR` from the metadata's `remoteEnv`.
    pub config_dir: Option<String>,
}

fn map(mounts: &[Mount], path: &Path, from: impl Fn(&Mount) -> &Path, to: impl Fn(&Mount) -> &Path) -> Option<PathBuf> {
    mounts
        .iter()
        .filter_map(|m| path.strip_prefix(from(m)).ok().map(|rest| (from(m).components().count(), to(m).join(rest))))
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, mapped)| mapped)
}

/// `path` inside the container, as seen on the host (through the longest matching mount).
pub fn to_host(mounts: &[Mount], path: &Path) -> Option<PathBuf> {
    map(mounts, path, |m| &m.destination, |m| &m.source)
}

/// Host `path` as seen inside the container (through the longest matching mount).
pub fn to_container(mounts: &[Mount], path: &Path) -> Option<PathBuf> {
    map(mounts, path, |m| &m.source, |m| &m.destination)
}

/// A container from one line of `docker inspect --format INSPECT_FORMAT`.
pub fn parse_inspect(line: &str) -> Option<Container> {
    let fields: Vec<&str> = line.trim_end().split('\t').collect();
    let [id, name, running, created, folder, metadata, mounts, user] = fields[..] else { return None };
    let text = |raw: &str| serde_json::from_str::<Option<String>>(raw).ok().flatten();
    let (meta_user, config_dir) = metadata_settings(text(metadata).as_deref());
    let image_user = text(user).filter(|user| !user.is_empty());
    let mounts: Vec<serde_json::Value> = serde_json::from_str(mounts).ok()?;
    Some(Container {
        id: text(id)?,
        name: text(name)?.trim_start_matches('/').into(),
        running: serde_json::from_str(running).ok()?,
        created: text(created).unwrap_or_default(),
        host_folder: text(folder)?.into(),
        mounts: mounts
            .iter()
            .filter(|m| m["Type"] == "bind")
            .filter_map(|m| {
                Some(Mount { source: m["Source"].as_str()?.into(), destination: m["Destination"].as_str()?.into() })
            })
            .collect(),
        remote_user: Some(meta_user.or(image_user).unwrap_or_else(|| "root".into())),
        config_dir,
    })
}

/// The last `remoteUser` and the last `remoteEnv.CLAUDE_CONFIG_DIR` of a `devcontainer.metadata` array, as VS Code
/// merges them. Every other variable is dropped.
fn metadata_settings(metadata: Option<&str>) -> (Option<String>, Option<String>) {
    let entries: Vec<serde_json::Value> = metadata.and_then(|m| serde_json::from_str(m).ok()).unwrap_or_default();
    let last = |pick: &dyn Fn(&serde_json::Value) -> Option<String>| entries.iter().rev().find_map(pick);
    (
        last(&|entry| entry["remoteUser"].as_str().map(Into::into)),
        last(&|entry| entry["remoteEnv"]["CLAUDE_CONFIG_DIR"].as_str().map(Into::into)),
    )
}

/// The Docker container id in a `/proc/<pid>/cgroup` file (cgroup v2 `docker-<id>.scope`, v1 `/docker/<id>`).
pub fn container_from_cgroup(cgroup: &str) -> Option<String> {
    cgroup.lines().find_map(|line| {
        let path = line.splitn(3, ':').nth(2)?;
        let last = path.rsplit('/').next()?;
        if let Some(id) = last.strip_prefix("docker-").and_then(|rest| rest.strip_suffix(".scope")) {
            return (!id.is_empty()).then(|| id.into());
        }
        let (_, rest) = path.split_once("/docker/")?;
        rest.split('/').next().filter(|id| !id.is_empty()).map(Into::into)
    })
}

/// What the daemon asks of Docker.
pub trait Docker {
    /// Every dev container, running or not.
    fn list(&self) -> Result<Vec<Container>, String>;
    /// Start a stopped container and wait until it runs.
    fn start(&self, id: &str) -> Result<(), String>;
    /// Run the shell `script` as root inside the container `id`; its output.
    fn run_as_root(&self, id: &str, script: &str) -> Result<String, String> {
        let _ = (id, script);
        Err("not supported".into())
    }
}

/// The `docker` command-line tool.
pub struct DockerCli {
    /// Program to run.
    pub program: String,
}

impl Default for DockerCli {
    fn default() -> Self {
        DockerCli { program: "docker".into() }
    }
}

impl DockerCli {
    /// Run `docker args`, killing it after `timeout`; its output, or its error message.
    fn run(&self, args: &[&str], timeout: Duration) -> Result<String, String> {
        let mut child = crate::process::hide_window(&mut Command::new(&self.program))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("could not run {}: {err}", self.program))?;
        // Read on threads so a large output cannot fill the pipe and stall the wait below.
        let drain = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut text = String::new();
                if let Some(mut pipe) = pipe {
                    let _ = pipe.read_to_string(&mut text);
                }
                text
            })
        };
        let stdout = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
        let stderr = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("docker {} did not answer within {} s", args[0], timeout.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let (out, err) = (stdout.join().unwrap_or_default(), stderr.join().unwrap_or_default());
        if status.success() { Ok(out) } else { Err(err.trim().to_string()) }
    }
}

impl Docker for DockerCli {
    fn list(&self) -> Result<Vec<Container>, String> {
        let filter = format!("label={FOLDER_LABEL}");
        let ids = self.run(&["ps", "-a", "-q", "--no-trunc", "--filter", &filter], LOOKUP_TIMEOUT)?;
        let ids: Vec<&str> = ids.split_whitespace().collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec!["inspect", "--format", INSPECT_FORMAT];
        args.extend(ids);
        Ok(self.run(&args, LOOKUP_TIMEOUT)?.lines().filter_map(parse_inspect).collect())
    }

    fn start(&self, id: &str) -> Result<(), String> {
        self.run(&["start", id], START_TIMEOUT).map(|_| ())
    }

    fn run_as_root(&self, id: &str, script: &str) -> Result<String, String> {
        self.run(&["exec", "-u", "root", id, "sh", "-c", script], LOOKUP_TIMEOUT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mounts() -> Vec<Mount> {
        vec![
            Mount { source: "/home/u/code/app".into(), destination: "/workspaces/app".into() },
            Mount { source: "/home/u/.claude".into(), destination: "/home/u/.claude".into() },
            Mount { source: "/home/u/common/ruff.toml".into(), destination: "/workspaces/ruff.toml".into() },
        ]
    }

    #[test]
    fn container_paths_map_to_the_host_through_the_longest_mount() {
        let m = mounts();
        assert_eq!(to_host(&m, Path::new("/workspaces/app")), Some("/home/u/code/app".into()));
        assert_eq!(
            to_host(&m, Path::new("/workspaces/app/.claude/worktrees/x")),
            Some("/home/u/code/app/.claude/worktrees/x".into())
        );
        assert_eq!(to_host(&m, Path::new("/workspaces/apple")), None);
        assert_eq!(to_host(&m, Path::new("/etc")), None);
        assert_eq!(to_container(&m, Path::new("/home/u/code/app/src")), Some("/workspaces/app/src".into()));
        assert_eq!(to_container(&m, Path::new("/home/u/elsewhere")), None);
    }

    /// One line as `docker inspect --format INSPECT_FORMAT` prints it (hand-written; no real values).
    const LINE: &str = r#""abc123def4567890"	"/lucid_carver"	true	"2026-09-30T08:00:00Z"	"/home/u/code/app"	"[{\"id\":\"feature\"},{\"remoteUser\":\"dev\",\"remoteEnv\":{\"CLAUDE_CONFIG_DIR\":\"/home/u/.claude\",\"SECRET\":\"x\"}},{\"remoteUser\":\"u\"}]"	[{"Type":"bind","Source":"/home/u/code/app","Destination":"/workspaces/app"},{"Type":"volume","Source":"/var/lib/docker/volumes/v/_data","Destination":"/vscode"}]	"root""#;
    const METADATA: &str = r#""[{\"id\":\"feature\"},{\"remoteUser\":\"dev\",\"remoteEnv\":{\"CLAUDE_CONFIG_DIR\":\"/home/u/.claude\",\"SECRET\":\"x\"}},{\"remoteUser\":\"u\"}]""#;

    #[test]
    fn inspect_lines_parse_with_the_last_metadata_entry_winning() {
        let c = parse_inspect(LINE).unwrap();
        assert_eq!((c.id.as_str(), c.name.as_str(), c.running), ("abc123def4567890", "lucid_carver", true));
        assert_eq!(c.host_folder, PathBuf::from("/home/u/code/app"));
        assert_eq!((c.remote_user.as_deref(), c.config_dir.as_deref()), (Some("u"), Some("/home/u/.claude")));
        // Only bind mounts map paths.
        assert_eq!(c.mounts, vec![Mount { source: "/home/u/code/app".into(), destination: "/workspaces/app".into() }]);
        assert!(parse_inspect("garbage").is_none());
    }

    #[test]
    fn without_a_remote_user_the_image_user_then_root_is_used() {
        let no_meta = LINE.replace(METADATA, "null");
        assert_eq!(parse_inspect(&no_meta).unwrap().remote_user.as_deref(), Some("root"));
        let with_user = no_meta.replace(r#""root""#, r#""vscode""#);
        assert_eq!(parse_inspect(&with_user).unwrap().remote_user.as_deref(), Some("vscode"));
        let empty = no_meta.replace(r#""root""#, r#""""#);
        assert_eq!(parse_inspect(&empty).unwrap().remote_user.as_deref(), Some("root"));
    }

    #[test]
    fn cgroups_name_their_docker_container() {
        assert_eq!(container_from_cgroup("0::/system.slice/docker-4a31e6.scope\n").as_deref(), Some("4a31e6"));
        assert_eq!(
            container_from_cgroup("12:pids:/docker/4a31e6\n1:name=systemd:/docker/4a31e6\n").as_deref(),
            Some("4a31e6")
        );
        assert_eq!(container_from_cgroup("0::/user.slice/user-1000.slice/session-2.scope\n"), None);
        assert_eq!(container_from_cgroup("0::/system.slice/docker-.scope\n"), None);
    }

    #[test]
    fn a_missing_docker_binary_is_an_error_not_a_panic() {
        let docker = DockerCli { program: "/nonexistent/docker".into() };
        assert!(docker.list().is_err());
    }
}
