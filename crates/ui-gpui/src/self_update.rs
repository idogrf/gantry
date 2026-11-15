//! Click to update: downloads a release file, checks its SHA-256, installs it and confirms the new version.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use app_core::release::{Asset, sum_for};
use app_core::self_update::{Offer, Pick, Run, Step, Target, percent};
use app_core::version::Version;
use gpui::Global;
use sha2::{Digest, Sha256};

use crate::update_watch::{PROBE_TIMEOUT, probe};

/// How long connecting to the server may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the server may take to answer a request.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a whole download may take.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// What the update UI shows, published by the workspace for the release card and Settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UpdateRow {
    /// The newest published release known.
    pub latest: Option<Version>,
    /// Whether this Gantry can install it.
    pub offer: Offer,
    /// The update run.
    pub run: Run,
    /// The newest release's page, while one newer than this Gantry is offered.
    pub page: Option<String>,
    /// The update notice, while Restart and update is offered.
    pub restart: Option<app_core::version::Update>,
}

impl Global for UpdateRow {}

/// Why an update run stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The user cancelled it.
    Cancelled,
    /// A step failed, with the detail.
    At(Step, String),
}

/// Downloads `pick`, checks it, installs it over `target` and confirms that the install reports `version`.
pub(crate) fn run(
    pick: &Pick,
    target: &Target,
    version: Version,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u8),
) -> Result<Version, Failure> {
    run_in(&std::env::temp_dir(), &[], pick, target, version, cancel, progress)
}

/// [`run`] with its working folder made under `tmp_parent` and `env` added to the installer's environment.
pub(crate) fn run_in(
    tmp_parent: &Path,
    env: &[(&str, &Path)],
    pick: &Pick,
    target: &Target,
    version: Version,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u8),
) -> Result<Version, Failure> {
    let tmp = TempDir::new(tmp_parent).map_err(|error| Failure::At(Step::Download, error.to_string()))?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .build()
        .into();
    let file = tmp.0.join(&pick.file.name);
    let digest = download(&agent, &pick.file, &file, cancel, progress)?;
    let sums = download_text(&agent, &pick.sums)?;
    let Some(expected) = sum_for(&sums, &pick.file.name) else {
        return Err(Failure::At(Step::Hash, format!("{} lists no checksum for {}", pick.sums.name, pick.file.name)));
    };
    if expected != digest {
        return Err(Failure::At(Step::Hash, format!("expected {expected}, got {digest}")));
    }
    match target {
        Target::Linux { home } => install_linux(&file, &tmp.0, home, env, version)?,
        Target::Mac { app } => install_mac(&file, &tmp.0, app)?,
    }
    match probe(&target.installed_binary(), PROBE_TIMEOUT) {
        Some(installed) if installed == version => Ok(installed),
        Some(installed) => Err(Failure::At(Step::Version, format!("it reports {installed}"))),
        None => Err(Failure::At(Step::Version, "it did not report a version".into())),
    }
}

/// A fresh folder, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(parent: &Path) -> std::io::Result<TempDir> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
        let mut attempt = 0u32;
        loop {
            let path = parent.join(format!("gantry-update-{}-{nanos}-{attempt}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(TempDir(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt < 100 => attempt += 1,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn download_failed(error: impl std::fmt::Display) -> Failure {
    Failure::At(Step::Download, error.to_string())
}

/// Streams `asset` into `path`, reporting each new whole percent; returns the file's SHA-256 in lowercase hex.
fn download(
    agent: &ureq::Agent,
    asset: &Asset,
    path: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u8),
) -> Result<String, Failure> {
    let mut response = agent.get(&asset.url).call().map_err(download_failed)?;
    let total = response.body().content_length().or(Some(asset.size));
    let mut reader = response.body_mut().as_reader();
    let mut out = File::create(path).map_err(download_failed)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    let mut shown = None;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Failure::Cancelled);
        }
        let read = reader.read(&mut buffer).map_err(download_failed)?;
        if read == 0 {
            break;
        }
        out.write_all(&buffer[..read]).map_err(download_failed)?;
        hasher.update(&buffer[..read]);
        done += read as u64;
        let now = percent(done, total);
        if shown != Some(now) {
            shown = Some(now);
            progress(now);
        }
    }
    // The whole file is in even when its size was unknown or wrong: the run moves on to checking and installing.
    if shown.is_none_or(|shown| shown < 100) {
        progress(100);
    }
    out.sync_all().map_err(download_failed)?;
    Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Fetches a small text file such as `SHA256SUMS`.
fn download_text(agent: &ureq::Agent, asset: &Asset) -> Result<String, Failure> {
    let mut response = agent.get(&asset.url).call().map_err(download_failed)?;
    response.body_mut().read_to_string().map_err(download_failed)
}

/// Unpacks the tarball into `tmp` and runs its `install.sh` for `home`.
fn install_linux(file: &Path, tmp: &Path, home: &Path, env: &[(&str, &Path)], version: Version) -> Result<(), Failure> {
    let mut tar = Command::new("tar");
    tar.arg("-xzf").arg(file).arg("-C").arg(tmp);
    run_step(&mut tar)?;
    let mut install = Command::new("sh");
    install.arg(tmp.join(format!("gantry-{version}/install.sh"))).env("HOME", home);
    for (key, value) in env {
        install.env(key, value);
    }
    run_step(&mut install)
}

/// Copies `Gantry.app` out of the disk image `dmg` beside `app`, checks its signature and swaps it in.
fn install_mac(dmg: &Path, tmp: &Path, app: &Path) -> Result<(), Failure> {
    let (staged, old) = beside(app);
    let _ = std::fs::remove_dir_all(&staged);
    let _ = std::fs::remove_dir_all(&old);
    let mount = tmp.join("mnt");
    let mut attach = Command::new("hdiutil");
    attach.args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"]).arg(&mount).arg(dmg);
    run_step(&mut attach)?;
    let copied = run_step(Command::new("ditto").arg(mount.join("Gantry.app")).arg(&staged));
    if let Err(Failure::At(_, detail)) = run_step(Command::new("hdiutil").arg("detach").arg(&mount)) {
        tracing::warn!("update disk image not detached: {detail}");
    }
    copied?;
    run_step(Command::new("codesign").args(["--verify", "--deep", "--strict"]).arg(&staged))?;
    swap(app)
}

/// The staged new bundle and the old bundle's parking place, both hidden beside `app`.
fn beside(app: &Path) -> (PathBuf, PathBuf) {
    let dir = app.parent().unwrap_or(Path::new("."));
    (dir.join(".Gantry.app.new"), dir.join(".Gantry.app.old"))
}

/// Moves `app` aside and the staged bundle into its place, putting `app` back when the second move fails.
fn swap(app: &Path) -> Result<(), Failure> {
    let (staged, old) = beside(app);
    std::fs::rename(app, &old).map_err(rename_failed)?;
    if let Err(error) = std::fs::rename(&staged, app) {
        if let Err(back) = std::fs::rename(&old, app) {
            tracing::warn!("old app not moved back from {} to {}: {back}", old.display(), app.display());
        }
        return Err(rename_failed(error));
    }
    if let Err(error) = std::fs::remove_dir_all(&old) {
        tracing::warn!("old app not removed from {}: {error}", old.display());
    }
    Ok(())
}

/// A refused move is macOS's App Management protection (or a folder without write access); anything else is a failed
/// install.
fn rename_failed(error: std::io::Error) -> Failure {
    let refused = error.kind() == std::io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(1);
    Failure::At(if refused { Step::Permission } else { Step::Install }, error.to_string())
}

/// Runs an install command; a failure carries the end of its error output.
fn run_step(command: &mut Command) -> Result<(), Failure> {
    let output = command.output().map_err(|error| Failure::At(Step::Install, error.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr.trim_end().lines().collect();
    let tail = lines[lines.len().saturating_sub(5)..].join("\n");
    Err(Failure::At(Step::Install, if tail.is_empty() { output.status.to_string() } else { tail }))
}

// The Linux tests run on Linux only: their fixtures are installed by the real Linux `install.sh`. The macOS tests run
// on macOS only (CI's macOS job on `release/*` PRs, and by hand): they need `hdiutil`, `ditto` and `codesign`.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;

    const NAME: &str = "gantry-9.9.9-x86_64-linux.tar.gz";

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn sha256(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn script(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A release tarball whose `gantry` reports `reported`.
    fn tarball(dir: &Path, reported: &str) -> Vec<u8> {
        let stage = dir.join("stage/gantry-9.9.9");
        script(&stage.join("gantry"), &format!("echo \"gantry {reported} (test)\""));
        script(&stage.join("gantryd"), "echo \"gantryd 9.9.9 (test)\"");
        let packaging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux");
        for name in ["install.sh", "io.github.idogrf.Gantry.desktop", "io.github.idogrf.Gantry.svg"] {
            std::fs::copy(packaging.join(name), stage.join(name)).unwrap();
        }
        let out = dir.join(NAME);
        let status = Command::new("tar")
            .arg("-C")
            .arg(dir.join("stage"))
            .arg("-czf")
            .arg(&out)
            .arg("gantry-9.9.9")
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(out).unwrap()
    }

    fn ok(body: &[u8]) -> Vec<u8> {
        let mut response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(body);
        response
    }

    /// Serves canned raw responses by request path on a local port; returns the base URL.
    fn serve(routes: Vec<(&'static str, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&chunk[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("");
                let response = routes.iter().find(|(route, _)| path == format!("/{route}")).map_or(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    |r| r.1.clone(),
                );
                let _ = stream.write_all(&response);
            }
        });
        base
    }

    struct Setup {
        _dir: tempfile::TempDir,
        home: PathBuf,
        tmp: PathBuf,
        pick: Pick,
    }

    impl Setup {
        /// A home with an old `gantry` (0.1.0) and a server answering with `file` and `sums` responses.
        fn new(file: impl FnOnce(&[u8]) -> Vec<u8>, sums: impl FnOnce(&[u8]) -> String, reported: &str) -> Setup {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            script(&home.join(".local/bin/gantry"), "echo \"gantry 0.1.0 (old)\"");
            let tmp = dir.path().join("tmp");
            std::fs::create_dir(&tmp).unwrap();
            let bytes = tarball(dir.path(), reported);
            let base = serve(vec![(NAME, file(&bytes)), ("SHA256SUMS", ok(sums(&bytes).as_bytes()))]);
            let pick = Pick {
                file: Asset { name: NAME.into(), url: format!("{base}/{NAME}"), size: bytes.len() as u64 },
                sums: Asset { name: "SHA256SUMS".into(), url: format!("{base}/SHA256SUMS"), size: 0 },
            };
            Setup { _dir: dir, home, tmp, pick }
        }

        fn run(&self, cancel: bool, progress: &mut dyn FnMut(u8)) -> Result<Version, Failure> {
            let data = self.home.join(".local/share");
            run_in(
                &self.tmp,
                &[("XDG_DATA_HOME", &data)],
                &self.pick,
                &Target::Linux { home: self.home.clone() },
                v("9.9.9"),
                &AtomicBool::new(cancel),
                progress,
            )
        }

        /// What the installed `gantry` reports. Tried a few times: a stub this process just wrote can briefly fail
        /// to start ("Text file busy") while another test thread forks with its write handle still open.
        fn installed(&self) -> Option<Version> {
            for _ in 0..20 {
                if let Some(version) = probe(&self.home.join(".local/bin/gantry"), PROBE_TIMEOUT) {
                    return Some(version);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            None
        }

        fn tmp_is_empty(&self) -> bool {
            std::fs::read_dir(&self.tmp).unwrap().next().is_none()
        }
    }

    fn good_sums(bytes: &[u8]) -> String {
        format!("{}  {NAME}\n{}  other.dmg\n", sha256(bytes), sha256(b"other"))
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs the Linux install.sh: Linux only")]
    fn installs_a_checked_tarball() {
        let setup = Setup::new(ok, good_sums, "9.9.9");
        let mut seen = Vec::new();
        assert_eq!(setup.run(false, &mut |n| seen.push(n)), Ok(v("9.9.9")));
        assert_eq!(setup.installed(), Some(v("9.9.9")));
        assert!(setup.home.join(".local/bin/gantryd").is_file());
        assert_eq!(seen.last(), Some(&100));
        assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "{seen:?}");
        assert!(setup.tmp_is_empty());
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs the Linux install.sh: Linux only")]
    fn ends_at_100_without_a_known_size() {
        let close_delimited = |bytes: &[u8]| {
            let mut response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
            response.extend_from_slice(bytes);
            response
        };
        let mut setup = Setup::new(close_delimited, good_sums, "9.9.9");
        setup.pick.file.size = 0;
        let mut seen = Vec::new();
        assert_eq!(setup.run(false, &mut |n| seen.push(n)), Ok(v("9.9.9")));
        assert_eq!(seen.last(), Some(&100), "{seen:?}");
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs the Linux install.sh: Linux only")]
    fn wrong_checksum_touches_nothing() {
        let setup = Setup::new(ok, |_| format!("{}  {NAME}\n", sha256(b"something else")), "9.9.9");
        assert!(matches!(setup.run(false, &mut |_| {}), Err(Failure::At(Step::Hash, _))));
        assert_eq!(setup.installed(), Some(v("0.1.0")));
        assert!(setup.tmp_is_empty());
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs the Linux install.sh: Linux only")]
    fn missing_sum_line_fails_at_hash() {
        let setup = Setup::new(ok, |_| format!("{}  other.dmg\n", sha256(b"other")), "9.9.9");
        assert!(matches!(setup.run(false, &mut |_| {}), Err(Failure::At(Step::Hash, _))));
        assert_eq!(setup.installed(), Some(v("0.1.0")));
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs the Linux install.sh: Linux only")]
    fn cut_download_fails() {
        let cut =
            |_: &[u8]| b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n0123456789".to_vec();
        let setup = Setup::new(cut, good_sums, "9.9.9");
        assert!(matches!(setup.run(false, &mut |_| {}), Err(Failure::At(Step::Download, _))));
        assert_eq!(setup.installed(), Some(v("0.1.0")));
        assert!(setup.tmp_is_empty());
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs the Linux install.sh: Linux only")]
    fn cancel_stops_and_cleans() {
        let setup = Setup::new(ok, good_sums, "9.9.9");
        assert_eq!(setup.run(true, &mut |_| {}), Err(Failure::Cancelled));
        assert_eq!(setup.installed(), Some(v("0.1.0")));
        assert!(setup.tmp_is_empty());
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs the Linux install.sh: Linux only")]
    fn version_must_match() {
        let setup = Setup::new(ok, good_sums, "9.9.8");
        assert!(matches!(setup.run(false, &mut |_| {}), Err(Failure::At(Step::Version, _))));
        assert!(setup.tmp_is_empty());
    }

    #[test]
    fn rename_refusals_are_permission_failures() {
        let step = |error| match rename_failed(error) {
            Failure::At(step, _) => step,
            Failure::Cancelled => unreachable!(),
        };
        assert_eq!(step(std::io::Error::from_raw_os_error(1)), Step::Permission);
        assert_eq!(step(std::io::Error::from(std::io::ErrorKind::PermissionDenied)), Step::Permission);
        assert_eq!(step(std::io::Error::from(std::io::ErrorKind::NotFound)), Step::Install);
    }

    /// A folder holding an old `Gantry.app` (0.1.0) and a staged `.Gantry.app.new` (9.9.9).
    fn bundles(dir: &Path) -> (PathBuf, PathBuf) {
        let folder = dir.join("Applications");
        let app = folder.join("Gantry.app");
        script(&app.join("Contents/MacOS/gantry"), "echo \"gantry 0.1.0 (old)\"");
        script(&folder.join(".Gantry.app.new/Contents/MacOS/gantry"), "echo \"gantry 9.9.9 (test)\"");
        (folder, app)
    }

    /// What `app`'s `gantry` reports, tried a few times (see `Setup::installed`).
    fn reports(app: &Path) -> Option<Version> {
        let binary = Target::Mac { app: app.to_owned() }.installed_binary();
        (0..20).find_map(|_| {
            let version = probe(&binary, PROBE_TIMEOUT);
            if version.is_none() {
                std::thread::sleep(Duration::from_millis(50));
            }
            version
        })
    }

    #[test]
    fn swap_puts_the_new_bundle_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let (folder, app) = bundles(dir.path());
        assert_eq!(swap(&app), Ok(()));
        assert_eq!(reports(&app), Some(v("9.9.9")));
        assert!(!folder.join(".Gantry.app.new").exists());
        assert!(!folder.join(".Gantry.app.old").exists());
    }

    #[test]
    fn swap_in_a_locked_folder_keeps_the_old_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let (folder, app) = bundles(dir.path());
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = swap(&app);
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(result, Err(Failure::At(Step::Permission, _))), "{result:?}");
        assert_eq!(reports(&app), Some(v("0.1.0")));
    }

    #[test]
    fn swap_without_a_staged_bundle_puts_the_old_one_back() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Applications/Gantry.app");
        script(&app.join("Contents/MacOS/gantry"), "echo \"gantry 0.1.0 (old)\"");
        let result = swap(&app);
        assert!(matches!(result, Err(Failure::At(Step::Install, _))), "{result:?}");
        assert_eq!(reports(&app), Some(v("0.1.0")));
        assert!(!dir.path().join("Applications/.Gantry.app.old").exists());
    }

    const DMG: &str = "gantry-9.9.9-aarch64-macos.dmg";

    fn run_command(command: &mut Command) {
        let output = command.output().unwrap();
        assert!(output.status.success(), "{command:?}: {}", String::from_utf8_lossy(&output.stderr));
    }

    /// An ad-hoc signed `Gantry.app` whose `gantry` reports 9.9.9, in a disk image.
    fn dmg(dir: &Path) -> Vec<u8> {
        let stage = dir.join("stage");
        let app = stage.join("Gantry.app");
        script(&app.join("Contents/MacOS/gantry"), "echo \"gantry 9.9.9 (test)\"");
        std::fs::write(
            app.join("Contents/Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Gantry</string>
  <key>CFBundleIdentifier</key><string>io.github.idogrf.GantryTest</string>
  <key>CFBundleExecutable</key><string>gantry</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>9.9.9</string>
  <key>CFBundleVersion</key><string>9.9.9</string>
</dict>
</plist>
"#,
        )
        .unwrap();
        run_command(Command::new("codesign").args(["--force", "--deep", "--sign", "-"]).arg(&app));
        let out = dir.join(DMG);
        run_command(
            Command::new("hdiutil")
                .args(["create", "-volname", "Gantry", "-srcfolder"])
                .arg(&stage)
                .args(["-ov", "-format", "UDZO"])
                .arg(&out),
        );
        std::fs::read(out).unwrap()
    }

    struct MacSetup {
        _dir: tempfile::TempDir,
        folder: PathBuf,
        app: PathBuf,
        tmp: PathBuf,
        pick: Pick,
    }

    impl MacSetup {
        /// An `Applications` folder with an old `Gantry.app` (0.1.0) and a server offering the new one's dmg.
        fn new() -> MacSetup {
            let dir = tempfile::tempdir().unwrap();
            let folder = dir.path().join("Applications");
            let app = folder.join("Gantry.app");
            script(&app.join("Contents/MacOS/gantry"), "echo \"gantry 0.1.0 (old)\"");
            let tmp = dir.path().join("tmp");
            std::fs::create_dir(&tmp).unwrap();
            let bytes = dmg(dir.path());
            let sums = format!("{}  {DMG}\n", sha256(&bytes));
            let base = serve(vec![(DMG, ok(&bytes)), ("SHA256SUMS", ok(sums.as_bytes()))]);
            let pick = Pick {
                file: Asset { name: DMG.into(), url: format!("{base}/{DMG}"), size: bytes.len() as u64 },
                sums: Asset { name: "SHA256SUMS".into(), url: format!("{base}/SHA256SUMS"), size: 0 },
            };
            MacSetup { _dir: dir, folder, app, tmp, pick }
        }

        fn run(&self) -> Result<Version, Failure> {
            let target = Target::Mac { app: self.app.clone() };
            run_in(&self.tmp, &[], &self.pick, &target, v("9.9.9"), &AtomicBool::new(false), &mut |_| {})
        }

        /// Whether `hdiutil info` still lists an image mounted under this setup's working folder.
        fn still_mounted(&self) -> bool {
            let output = Command::new("hdiutil").arg("info").output().unwrap();
            String::from_utf8_lossy(&output.stdout).contains(self.tmp.to_str().unwrap())
        }
    }

    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "needs hdiutil/ditto/codesign: macOS only")]
    fn swaps_the_app() {
        let setup = MacSetup::new();
        assert_eq!(setup.run(), Ok(v("9.9.9")));
        assert_eq!(reports(&setup.app), Some(v("9.9.9")));
        assert!(!setup.folder.join(".Gantry.app.new").exists());
        assert!(!setup.folder.join(".Gantry.app.old").exists());
        assert!(!setup.still_mounted());
    }

    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "needs hdiutil/ditto/codesign: macOS only")]
    fn leftovers_are_cleared() {
        let setup = MacSetup::new();
        for leftover in [".Gantry.app.new", ".Gantry.app.old"] {
            std::fs::create_dir_all(setup.folder.join(leftover).join("junk")).unwrap();
        }
        assert_eq!(setup.run(), Ok(v("9.9.9")));
        assert_eq!(reports(&setup.app), Some(v("9.9.9")));
        assert!(!setup.folder.join(".Gantry.app.new").exists());
        assert!(!setup.folder.join(".Gantry.app.old").exists());
    }

    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "needs hdiutil/ditto/codesign: macOS only")]
    fn unwritable_folder_keeps_the_old_app() {
        let setup = MacSetup::new();
        std::fs::set_permissions(&setup.folder, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = setup.run();
        std::fs::set_permissions(&setup.folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(result, Err(Failure::At(Step::Permission | Step::Install, _))), "{result:?}");
        assert_eq!(reports(&setup.app), Some(v("0.1.0")));
        assert!(!setup.still_mounted());
    }
}
