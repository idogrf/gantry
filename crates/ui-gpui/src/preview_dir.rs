//! The private folder that holds the copies of remote files opened in the system viewer.
//!
//! A shared temp folder is not safe for this: another user could create it first, or plant symlinks in it, and read
//! or redirect the copies. So the folder is in the user's own runtime dir (`$XDG_RUNTIME_DIR`) or cache dir, is made
//! owner-only, and is never used when it is not a plain folder the user owns with no access for others.

use std::io;
use std::path::{Path, PathBuf};

/// Where the copies go: `<runtime dir>/gantry-preview` when the runtime dir is set, else `<cache dir>/preview` (the
/// platform's per-user cache folder of Gantry); `None` when neither is known.
fn choose(runtime: Option<&Path>, cache: Option<&Path>) -> Option<PathBuf> {
    match (runtime.filter(|dir| !dir.as_os_str().is_empty()), cache) {
        (Some(runtime), _) => Some(runtime.join("gantry-preview")),
        (None, Some(cache)) => Some(cache.join("preview")),
        (None, None) => None,
    }
}

/// The private folder for this user, from the environment.
pub fn root() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let cache = directories::ProjectDirs::from("", "", "gantry").map(|dirs| dirs.cache_dir().to_path_buf());
    choose(runtime.as_deref(), cache.as_deref())
}

/// Whether an existing `root` may be used: a real folder (not a symlink) that `me` owns and nobody else can enter.
#[cfg(unix)]
fn trusted(is_dir: bool, is_symlink: bool, owner: u32, mode: u32, me: u32) -> bool {
    is_dir && !is_symlink && owner == me && mode & 0o077 == 0
}

/// Make sure `root` exists as a private folder (mode 0700 on Unix), or fail without touching what is there.
fn prepare(root: &Path) -> io::Result<()> {
    if let Some(parent) = root.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        match std::fs::DirBuilder::new().mode(0o700).create(root) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() != io::ErrorKind::AlreadyExists => return Err(err),
            Err(_) => {}
        }
        let meta = std::fs::symlink_metadata(root)?;
        let me = rustix::process::getuid().as_raw();
        if trusted(meta.is_dir(), meta.is_symlink(), meta.uid(), meta.mode(), me) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is not a private folder of this user", root.display()),
            ))
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(root)
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

/// Remove what an earlier run left in `root`, when it is a private folder of this user (anything else is left alone,
/// and `write_private` will refuse it). Called once at start.
pub fn clear(root: &Path) {
    if prepare(root).is_ok()
        && let Err(err) = std::fs::remove_dir_all(root)
    {
        tracing::warn!("clearing the preview copies failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_dir_wins_then_the_cache_dir() {
        let (run, cache) = (Path::new("/run/user/1000"), Path::new("/home/u/.cache/gantry"));
        assert_eq!(choose(Some(run), Some(cache)), Some(PathBuf::from("/run/user/1000/gantry-preview")));
        assert_eq!(choose(None, Some(cache)), Some(PathBuf::from("/home/u/.cache/gantry/preview")));
        assert_eq!(choose(Some(Path::new("")), Some(cache)), Some(PathBuf::from("/home/u/.cache/gantry/preview")));
        assert_eq!(choose(None, None), None);
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
        let root = temp.path().join("gantry-preview");
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
        let link = temp.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(write_private(&link, &link.join("a"), b"x").is_err());
        assert!(!target.join("a").exists());
    }

    #[cfg(unix)]
    #[test]
    fn clearing_removes_old_copies_but_leaves_a_planted_folder() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("gantry-preview");
        write_private(&root, &root.join("a"), b"x").unwrap();
        clear(&root);
        assert!(!root.exists());
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(root.join("keep"), b"x").unwrap();
        clear(&root);
        assert!(root.join("keep").exists());
    }
}
