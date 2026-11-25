//! Reading one file for the preview pane (`ReadFile`).

use std::path::Path;

use protocol::message::FileBody;

/// Largest file the daemon sends.
pub const MAX_BYTES: u64 = 25 * 1024 * 1024;

/// Read `path` (absolute, or starting with `~` which `home` replaces). `known_mtime_ms` is the modification time the
/// caller already has: a file with the same time answers `Unchanged`.
pub fn read_file(path: &str, known_mtime_ms: Option<u64>, home: Option<&Path>) -> FileBody {
    let expanded = app_core::settings::expand_home(Path::new(path), home);
    let meta = match std::fs::metadata(&expanded) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return FileBody::Missing,
        Err(error) => return FileBody::Failed { reason: reason(&error) },
    };
    // Checked before opening: opening a FIFO would block the thread until someone writes to it.
    if !meta.is_file() {
        return FileBody::Failed { reason: "not a file".into() };
    }
    if meta.len() > MAX_BYTES {
        return FileBody::TooLarge { size: meta.len() };
    }
    // 0 = the platform gives no time; it never matches `known_mtime_ms`.
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_millis() as u64);
    if mtime_ms != 0 && known_mtime_ms == Some(mtime_ms) {
        return FileBody::Unchanged;
    }
    match std::fs::read(&expanded) {
        Ok(data) => FileBody::Bytes { data, mtime_ms },
        Err(error) => FileBody::Failed { reason: reason(&error) },
    }
}

/// The error as text without its OS code ("Permission denied", not "Permission denied (os error 13)").
fn reason(error: &std::io::Error) -> String {
    let text = error.to_string();
    text.split(" (os error").next().unwrap_or(&text).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(body: FileBody) -> (Vec<u8>, u64) {
        match body {
            FileBody::Bytes { data, mtime_ms } => (data, mtime_ms),
            other => panic!("expected bytes, got {other:?}"),
        }
    }

    #[test]
    fn reads_bytes_and_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.md");
        std::fs::write(&file, b"hello").unwrap();
        let expected = std::fs::metadata(&file).unwrap().modified().unwrap();
        let expected = expected.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let (data, mtime_ms) = bytes(read_file(file.to_str().unwrap(), None, None));
        assert_eq!(data, b"hello");
        assert_eq!(mtime_ms, expected);
    }

    #[test]
    fn home_is_expanded() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.md"), b"x").unwrap();
        assert_eq!(bytes(read_file("~/a.md", None, Some(tmp.path()))).0, b"x");
    }

    #[test]
    fn missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope");
        assert_eq!(read_file(path.to_str().unwrap(), None, None), FileBody::Missing);
    }

    #[test]
    fn a_folder_fails_without_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let body = read_file(tmp.path().to_str().unwrap(), None, None);
        assert_eq!(body, FileBody::Failed { reason: "not a file".into() });
    }

    // `mkfifo` through the command: `libc` is not a dependency of this crate.
    #[cfg(unix)]
    #[test]
    fn a_fifo_fails_without_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("pipe");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let body = read_file(fifo.to_str().unwrap(), None, None);
        assert_eq!(body, FileBody::Failed { reason: "not a file".into() });
    }

    #[test]
    fn too_large() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("big");
        std::fs::File::create(&file).unwrap().set_len(MAX_BYTES + 1).unwrap();
        let body = read_file(file.to_str().unwrap(), None, None);
        assert_eq!(body, FileBody::TooLarge { size: MAX_BYTES + 1 });
    }

    #[test]
    fn unchanged_when_mtime_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.md");
        std::fs::write(&file, b"hello").unwrap();
        let path = file.to_str().unwrap();
        let (_, mtime_ms) = bytes(read_file(path, None, None));
        assert_eq!(read_file(path, Some(mtime_ms), None), FileBody::Unchanged);
        assert!(matches!(read_file(path, Some(mtime_ms + 1), None), FileBody::Bytes { .. }));
    }

    #[test]
    fn unicode_path() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("דוח ü.md");
        std::fs::write(&file, "שלום").unwrap();
        assert_eq!(bytes(read_file(file.to_str().unwrap(), None, None)).0, "שלום".as_bytes());
    }
}
