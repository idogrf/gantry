//! Click to update: where Gantry can replace itself, what to download, and the update run's states and wording.

use std::path::{Path, PathBuf};

use crate::release::{Asset, Release};
use crate::version::Version;

/// Where release files are downloaded from.
pub const DOWNLOAD_PREFIX: &str = "https://github.com/idogrf/gantry/releases/download/";

/// A platform Gantry can update itself on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// Linux x86_64 (the tarball).
    Linux,
    /// macOS aarch64 (the disk image).
    Mac,
}

impl Platform {
    /// The platform this build runs on, or `None` where Gantry has no release file.
    pub fn current() -> Option<Platform> {
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some(Platform::Linux)
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some(Platform::Mac)
        } else {
            None
        }
    }
}

/// The release file name for a platform and version.
pub fn asset_name(platform: Platform, version: Version) -> String {
    match platform {
        Platform::Linux => format!("gantry-{version}-x86_64-linux.tar.gz"),
        Platform::Mac => format!("gantry-{version}-aarch64-macos.dmg"),
    }
}

/// The `scheme://host[:port]` part of a URL.
pub fn origin_of(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let end = rest.find('/').unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(&url[..scheme.len() + 3 + end])
}

/// The two files an update downloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pick {
    /// The tarball or disk image.
    pub file: Asset,
    /// The `SHA256SUMS` file.
    pub sums: Asset,
}

/// Picks the release file and its checksums, only when both are on GitHub (or on `extra_origin`).
pub fn pick(release: &Release, platform: Platform, extra_origin: Option<&str>) -> Option<Pick> {
    let allowed = |a: &Asset| {
        a.url.starts_with(DOWNLOAD_PREFIX) || extra_origin.is_some_and(|o| a.url.starts_with(&format!("{o}/")))
    };
    let find = |name: &str| release.assets.iter().find(|a| a.name == name && allowed(a)).cloned();
    Some(Pick { file: find(&asset_name(platform, release.version))?, sums: find("SHA256SUMS")? })
}

/// The install an update replaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// `~/.local/bin/gantry` of this home.
    Linux {
        /// The home directory.
        home: PathBuf,
    },
    /// A `Gantry.app` bundle.
    Mac {
        /// The bundle's path.
        app: PathBuf,
    },
}

impl Target {
    /// The installed `gantry` binary.
    pub fn installed_binary(&self) -> PathBuf {
        match self {
            Target::Linux { home } => home.join(".local/bin/gantry"),
            Target::Mac { app } => app.join("Contents/MacOS/gantry"),
        }
    }
}

/// Why this Gantry cannot update itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotHere {
    /// Installed somewhere it cannot replace (the path of the binary or bundle).
    Elsewhere(PathBuf),
    /// Running from macOS's randomized quarantine location.
    Translocated,
    /// Running from a mounted disk image.
    DiskImage,
    /// No release file for this platform, or no known install.
    Unsupported,
}

impl NotHere {
    /// The sentence shown to the user; `None` when there is nothing useful to say.
    pub fn line(&self) -> Option<String> {
        match self {
            NotHere::Elsewhere(path) => {
                Some(format!("Gantry is installed at {}, which it can't update itself.", path.display()))
            }
            NotHere::Translocated => Some("Move Gantry to Applications with Finder first.".to_string()),
            NotHere::DiskImage => Some("Copy Gantry to Applications first.".to_string()),
            NotHere::Unsupported => None,
        }
    }
}

/// Decides whether the running binary can be replaced.
///
/// `exe` and `home` are already canonical; a trailing " (deleted)" on the file name is dropped first.
pub fn install_target(
    platform: Platform,
    exe: &Path,
    home: &Path,
    writable: impl Fn(&Path) -> bool,
) -> Result<Target, NotHere> {
    let text = exe.to_string_lossy();
    let exe = PathBuf::from(text.strip_suffix(" (deleted)").unwrap_or(&text));
    match platform {
        Platform::Linux => {
            let bin_dir = home.join(".local/bin");
            if exe == bin_dir.join("gantry") && writable(&bin_dir) {
                Ok(Target::Linux { home: home.to_path_buf() })
            } else {
                Err(NotHere::Elsewhere(exe))
            }
        }
        Platform::Mac => {
            let app = exe.parent().and_then(Path::parent).and_then(Path::parent).filter(|app| {
                app.file_name().is_some_and(|n| n == "Gantry.app") && app.join("Contents/MacOS/gantry") == exe
            });
            let Some(app) = app else {
                return Err(NotHere::Elsewhere(exe));
            };
            let shown = exe.to_string_lossy();
            if shown.contains("/AppTranslocation/") {
                return Err(NotHere::Translocated);
            }
            if shown.starts_with("/Volumes/") {
                return Err(NotHere::DiskImage);
            }
            match app.parent() {
                Some(dir) if writable(dir) => Ok(Target::Mac { app: app.to_path_buf() }),
                _ => Err(NotHere::Elsewhere(app.to_path_buf())),
            }
        }
    }
}

/// The step of an update that failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Fetching a file.
    Download,
    /// Comparing the checksum.
    Hash,
    /// Replacing the install.
    Install,
    /// Running the new binary's `--version`.
    Version,
    /// macOS refused to let Gantry replace itself.
    Permission,
}

impl Step {
    /// The sentence shown for this failure.
    pub fn line(&self) -> &'static str {
        match self {
            Step::Download => "The download failed.",
            Step::Hash => "The download did not match its checksum.",
            Step::Install => "Installing failed.",
            Step::Version => "The installed Gantry did not report the new version.",
            Step::Permission => {
                "macOS did not let Gantry replace itself in /Applications. Allow Gantry under System Settings > \
                 Privacy & Security > App Management, or move Gantry to ~/Applications."
            }
        }
    }
}

/// The state of an update run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Run {
    /// Nothing started.
    #[default]
    Idle,
    /// Fetching the release file.
    Downloading {
        /// The version being fetched.
        version: Version,
        /// Progress, 0 to 100.
        percent: u8,
    },
    /// Replacing the install.
    Installing {
        /// The version being installed.
        version: Version,
    },
    /// A step failed.
    Failed {
        /// Which step.
        step: Step,
        /// The underlying reason, shown under the status line.
        detail: String,
    },
    /// The new version is installed.
    Done(Version),
}

impl Run {
    /// Whether a download or install is in flight.
    pub fn busy(&self) -> bool {
        matches!(self, Run::Downloading { .. } | Run::Installing { .. })
    }

    /// The status line, if any.
    pub fn line(&self) -> Option<String> {
        match self {
            Run::Downloading { version, percent } => Some(format!("Downloading Gantry {version}… {percent}%")),
            Run::Installing { .. } => Some("Installing…".to_string()),
            Run::Failed { step, .. } => Some(step.line().to_string()),
            Run::Idle | Run::Done(_) => None,
        }
    }

    /// The title-bar pill text while downloading.
    pub fn pill(&self) -> Option<String> {
        match self {
            Run::Downloading { percent, .. } => Some(format!("↓ {percent}%")),
            _ => None,
        }
    }
}

/// Download progress as a percentage; 0 with no known total, at most 100.
pub fn percent(done: u64, total: Option<u64>) -> u8 {
    match total {
        Some(total) if total > 0 => (done.min(total) as u128 * 100 / total as u128) as u8,
        _ => 0,
    }
}

/// What the update UI offers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Offer {
    /// Nothing to show yet.
    #[default]
    None,
    /// Click to update.
    Install(Target, Pick),
    /// Cannot update here; the optional line says why.
    NotHere(Option<String>),
}

/// Combines the install check and the release files into an offer.
pub fn offer(target: Result<Target, NotHere>, pick: Option<Pick>) -> Offer {
    match (target, pick) {
        (Err(why), _) => Offer::NotHere(why.line()),
        (Ok(_), None) => Offer::NotHere(None),
        (Ok(target), Some(pick)) => Offer::Install(target, pick),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn asset(name: &str, url: &str) -> Asset {
        Asset { name: name.to_string(), url: url.to_string(), size: 10 }
    }

    fn release(assets: Vec<Asset>) -> Release {
        Release { version: v("0.15.0"), date: String::new(), notes: String::new(), url: String::new(), assets }
    }

    fn gh(name: &str) -> Asset {
        asset(name, &format!("{DOWNLOAD_PREFIX}v0.15.0/{name}"))
    }

    #[test]
    fn asset_names() {
        assert_eq!(asset_name(Platform::Linux, v("0.15.0")), "gantry-0.15.0-x86_64-linux.tar.gz");
        assert_eq!(asset_name(Platform::Mac, v("0.15.0")), "gantry-0.15.0-aarch64-macos.dmg");
    }

    #[test]
    fn origins() {
        assert_eq!(origin_of("http://127.0.0.1:8765/x"), Some("http://127.0.0.1:8765"));
        assert_eq!(origin_of("https://github.com"), Some("https://github.com"));
        assert_eq!(origin_of("nope"), None);
    }

    #[test]
    fn pick_needs_both_files_on_github() {
        let file = "gantry-0.15.0-x86_64-linux.tar.gz";
        let ok = release(vec![gh(file), gh("SHA256SUMS")]);
        assert!(pick(&ok, Platform::Linux, None).is_some());
        assert!(pick(&release(vec![gh(file)]), Platform::Linux, None).is_none());
        let evil = release(vec![asset(file, "https://evil.example/a"), gh("SHA256SUMS")]);
        assert!(pick(&evil, Platform::Linux, None).is_none());
        let local =
            release(vec![asset(file, "http://127.0.0.1:8765/x"), asset("SHA256SUMS", "http://127.0.0.1:8765/sums")]);
        assert!(pick(&local, Platform::Linux, Some("http://127.0.0.1:8765")).is_some());
        assert!(pick(&local, Platform::Linux, None).is_none());
    }

    #[test]
    fn linux_target() {
        let home = Path::new("/home/u");
        let ok = install_target(Platform::Linux, Path::new("/home/u/.local/bin/gantry"), home, |_| true);
        assert_eq!(ok, Ok(Target::Linux { home: home.to_path_buf() }));
        let deleted = install_target(Platform::Linux, Path::new("/home/u/.local/bin/gantry (deleted)"), home, |_| true);
        assert_eq!(deleted, ok);
        let elsewhere = install_target(Platform::Linux, Path::new("/usr/bin/gantry"), home, |_| true).unwrap_err();
        assert_eq!(elsewhere, NotHere::Elsewhere(PathBuf::from("/usr/bin/gantry")));
        assert_eq!(
            elsewhere.line().as_deref(),
            Some("Gantry is installed at /usr/bin/gantry, which it can't update itself.")
        );
        let locked = install_target(Platform::Linux, Path::new("/home/u/.local/bin/gantry"), home, |_| false);
        assert!(matches!(locked, Err(NotHere::Elsewhere(_))));
        assert_eq!(
            Target::Linux { home: home.to_path_buf() }.installed_binary(),
            Path::new("/home/u/.local/bin/gantry")
        );
    }

    #[test]
    fn mac_target() {
        let home = Path::new("/Users/u");
        let exe = Path::new("/Applications/Gantry.app/Contents/MacOS/gantry");
        let ok = install_target(Platform::Mac, exe, home, |_| true).unwrap();
        assert_eq!(ok, Target::Mac { app: PathBuf::from("/Applications/Gantry.app") });
        assert_eq!(ok.installed_binary(), exe);
        let trans = Path::new("/private/var/folders/x/AppTranslocation/y/d/Gantry.app/Contents/MacOS/gantry");
        assert_eq!(install_target(Platform::Mac, trans, home, |_| true), Err(NotHere::Translocated));
        let dmg = Path::new("/Volumes/Gantry/Gantry.app/Contents/MacOS/gantry");
        assert_eq!(install_target(Platform::Mac, dmg, home, |_| true), Err(NotHere::DiskImage));
        assert_eq!(
            install_target(Platform::Mac, exe, home, |_| false),
            Err(NotHere::Elsewhere(PathBuf::from("/Applications/Gantry.app")))
        );
    }

    #[test]
    fn run_texts() {
        let down = Run::Downloading { version: v("0.15.0"), percent: 42 };
        assert_eq!(down.line().as_deref(), Some("Downloading Gantry 0.15.0… 42%"));
        assert_eq!(down.pill().as_deref(), Some("↓ 42%"));
        assert!(down.busy());
        let inst = Run::Installing { version: v("0.15.0") };
        assert!(inst.busy());
        assert_eq!(inst.line().as_deref(), Some("Installing…"));
        assert_eq!(inst.pill(), None);
        let failed = Run::Failed { step: Step::Permission, detail: String::new() };
        assert_eq!(failed.line().as_deref(), Some(Step::Permission.line()));
        assert!(Step::Permission.line().starts_with("macOS did not let Gantry replace itself in /Applications."));
        for run in [Run::Done(v("0.15.0")), Run::Idle] {
            assert!(!run.busy());
            assert_eq!(run.line(), None);
        }
    }

    #[test]
    fn percent_bounds() {
        assert_eq!(percent(0, None), 0);
        assert_eq!(percent(50, Some(200)), 25);
        assert_eq!(percent(300, Some(200)), 100);
        assert_eq!(percent(5, Some(0)), 0);
    }

    #[test]
    fn offer_rules() {
        let target = Target::Linux { home: PathBuf::from("/home/u") };
        let p = Pick { file: gh("a"), sums: gh("SHA256SUMS") };
        assert_eq!(
            offer(Err(NotHere::Translocated), Some(p.clone())),
            Offer::NotHere(Some("Move Gantry to Applications with Finder first.".to_string()))
        );
        assert_eq!(offer(Err(NotHere::Unsupported), None), Offer::NotHere(None));
        assert_eq!(offer(Ok(target.clone()), None), Offer::NotHere(None));
        assert_eq!(offer(Ok(target.clone()), Some(p.clone())), Offer::Install(target, p));
    }
}
