//! The private folder that holds the copies of remote files opened in the system viewer.
//!
//! A shared temp folder is not safe for this: another user could create it first, or plant symlinks in it, and read
//! or redirect the copies. So the folder is in the user's own runtime dir (`$XDG_RUNTIME_DIR`) or cache dir, is made
//! owner-only, and is never used when it is not a plain folder the user owns with no access for others.

use std::io;
use std::path::{Path, PathBuf};

/// Whether `name` is one plain path component (`[A-Za-z0-9._-]+`, not `.` or `..`), so joining it stays inside its
/// base.
fn plain_name(name: &str) -> bool {
    name != "."
        && name != ".."
        && !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Where the copies of `profile` go: `<runtime dir>/gantry-preview/<profile>` when the runtime dir is set, else
/// `<cache dir>/preview/<profile>` (the platform's per-user cache folder of Gantry); `None` when neither is known or
/// the profile name is not a plain name.
fn choose(runtime: Option<&Path>, cache: Option<&Path>, profile: &str) -> Option<PathBuf> {
    if !plain_name(profile) {
        return None;
    }
    match (runtime.filter(|dir| !dir.as_os_str().is_empty()), cache) {
        (Some(runtime), _) => Some(runtime.join("gantry-preview").join(profile)),
        (None, Some(cache)) => Some(cache.join("preview").join(profile)),
        (None, None) => None,
    }
}

/// The private folder of `profile` for this user, from the environment.
pub fn root(profile: &str) -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let cache = directories::ProjectDirs::from("", "", "gantry").map(|dirs| dirs.cache_dir().to_path_buf());
    choose(runtime.as_deref(), cache.as_deref(), profile)
}

/// Whether an existing `root` may be used: a real folder (not a symlink) that `me` owns and nobody else can enter.
#[cfg(unix)]
fn trusted(is_dir: bool, is_symlink: bool, owner: u32, mode: u32, me: u32) -> bool {
    is_dir && !is_symlink && owner == me && mode & 0o077 == 0
}

/// Make sure the private folder `dir` exists (mode 0700 on Unix), or fail without touching what is there.
fn private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() != io::ErrorKind::AlreadyExists => return Err(err),
            Err(_) => {}
        }
        let meta = std::fs::symlink_metadata(dir)?;
        let me = rustix::process::getuid().as_raw();
        if trusted(meta.is_dir(), meta.is_symlink(), meta.uid(), meta.mode(), me) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is not a private folder of this user", dir.display()),
            ))
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// Make sure `root` and its parent exist as private folders, or fail without touching what is there.
fn prepare(root: &Path) -> io::Result<()> {
    if let Some(parent) = root.parent() {
        if let Some(grandparent) = parent.parent() {
            std::fs::create_dir_all(grandparent)?;
        }
        private_dir(parent)?;
    }
    private_dir(root)
}

/// Write `data` to `path` (inside `root`) readable by this user only, making `root` and the folders between private.
pub fn write_private(root: &Path, path: &Path, data: &[u8]) -> io::Result<()> {
    prepare(root)?;
    if let Some(parent) = path.parent() {
        let mut dir = std::fs::DirBuilder::new();
        dir.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
        dir.create(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    io::Write::write_all(&mut options.open(path)?, data)
}

/// Remove what an earlier run of this profile left in `root`, when it and its parent are private folders of this user
/// (anything else is left alone, and `write_private` will refuse it), and the plain files an older version left
/// directly in the parent. Never follows a symlink. Called once at start.
pub fn clear(root: &Path) {
    if prepare(root).is_err() {
        return;
    }
    if let Err(err) = std::fs::remove_dir_all(root) {
        tracing::warn!("clearing the preview copies failed: {err}");
    }
    if let Some(entries) = root.parent().and_then(|parent| std::fs::read_dir(parent).ok()) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_dir_wins_then_the_cache_dir() {
        let (run, cache) = (Path::new("/run/user/1000"), Path::new("/home/u/.cache/gantry"));
        assert_eq!(choose(Some(run), Some(cache), "dev"), Some(PathBuf::from("/run/user/1000/gantry-preview/dev")));
        assert_eq!(choose(None, Some(cache), "dev"), Some(PathBuf::from("/home/u/.cache/gantry/preview/dev")));
        let empty = Some(Path::new(""));
        assert_eq!(choose(empty, Some(cache), "dev"), Some(PathBuf::from("/home/u/.cache/gantry/preview/dev")));
        assert_eq!(choose(None, None, "dev"), None);
    }

    #[test]
    fn each_profile_has_its_own_folder() {
        let (run, cache) = (Path::new("/run/user/1000"), Path::new("/home/u/.cache/gantry"));
        assert_eq!(choose(Some(run), Some(cache), "dev"), Some(PathBuf::from("/run/user/1000/gantry-preview/dev")));
        assert_eq!(
            choose(Some(run), Some(cache), "default"),
            Some(PathBuf::from("/run/user/1000/gantry-preview/default"))
        );
        assert_eq!(choose(None, Some(cache), "dev"), Some(PathBuf::from("/home/u/.cache/gantry/preview/dev")));
    }

    #[cfg(unix)]
    #[test]
    fn clearing_one_profile_leaves_the_other() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("gantry-preview");
        let (dev, default) = (base.join("dev"), base.join("default"));
        write_private(&dev, &dev.join("a"), b"x").unwrap();
        write_private(&default, &default.join("b"), b"x").unwrap();
        clear(&dev);
        assert!(!dev.exists());
        assert!(default.join("b").exists());
    }

    // The owner and mode checks are Unix concepts.
    #[cfg(unix)]
    #[test]
    fn only_a_private_folder_of_this_user_is_trusted() {
        assert!(trusted(true, false, 1000, 0o40700, 1000));
        assert!(!trusted(true, false, 0, 0o40700, 1000), "someone else's");
        assert!(!trusted(true, false, 1000, 0o40755, 1000), "others may enter");
        assert!(!trusted(true, false, 1000, 0o40770, 1000), "the group may enter");
        assert!(!trusted(true, true, 1000, 0o40700, 1000), "a symlink");
        assert!(!trusted(false, false, 1000, 0o100600, 1000), "a file");
    }

    #[cfg(unix)]
    #[test]
    fn copies_are_written_owner_only_and_a_planted_folder_is_refused() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("gantry-preview").join("dev");
        let file = root.join("box").join("id").join("a.png");
        write_private(&root, &file, b"one").unwrap();
        write_private(&root, &file, b"two").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"two");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().mode() & 0o777;
        assert_eq!((mode(&root), mode(&root.join("box")), mode(&file)), (0o700, 0o700, 0o600));
        // A folder others can enter is not used.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(write_private(&root, &file, b"x").is_err());
        // Nor is a symlink, even to a private folder.
        let target = temp.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = temp.path().join("gantry-preview").join("link");
        symlink(&target, &link).unwrap();
        assert!(write_private(&link, &link.join("a"), b"x").is_err());
        assert!(!target.join("a").exists());
    }

    #[cfg(unix)]
    #[test]
    fn clearing_removes_old_copies_but_leaves_a_planted_folder() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("gantry-preview").join("dev");
        write_private(&root, &root.join("a"), b"x").unwrap();
        clear(&root);
        assert!(!root.exists());
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(root.join("keep"), b"x").unwrap();
        clear(&root);
        assert!(root.join("keep").exists());
    }

    #[test]
    fn only_a_plain_profile_name_is_used() {
        let (run, cache) = (Path::new("/run/user/1000"), Path::new("/home/u/.cache/gantry"));
        for bad in ["..", ".", "a/b", "/abs", "a/../..", "", "a b"] {
            assert_eq!(choose(Some(run), Some(cache), bad), None, "{bad:?}");
            assert_eq!(choose(None, Some(cache), bad), None, "{bad:?}");
        }
        assert!(choose(Some(run), None, "my-profile_1.x").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_parent_others_can_enter_or_a_symlink_parent_is_refused() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("gantry-preview");
        let root = base.join("dev");
        write_private(&root, &root.join("a"), b"x").unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(write_private(&root, &root.join("a"), b"x").is_err());
        clear(&root);
        assert!(root.join("a").exists(), "an untrusted parent is not cleared");
        let target = temp.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = temp.path().join("linked");
        symlink(&target, &link).unwrap();
        assert!(write_private(&link.join("dev"), &link.join("dev").join("a"), b"x").is_err());
        assert!(!target.join("dev").exists());
    }

    #[cfg(unix)]
    #[test]
    fn clearing_removes_plain_files_in_the_parent_but_not_other_profiles_or_symlink_targets() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("gantry-preview");
        let (dev, default) = (base.join("dev"), base.join("default"));
        write_private(&dev, &dev.join("a"), b"x").unwrap();
        write_private(&default, &default.join("b"), b"x").unwrap();
        std::fs::write(base.join("old.png"), b"x").unwrap();
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"x").unwrap();
        symlink(&outside, base.join("link")).unwrap();
        clear(&dev);
        assert!(!base.join("old.png").exists());
        assert!(default.join("b").exists());
        assert!(outside.exists());
    }
}
