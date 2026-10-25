//! Change notifications for a config directory's `projects/` and `sessions/`.

use std::path::Path;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Call `on_change` whenever something under `config/projects` or `config/sessions` is created, modified or
/// removed (reads do not count). Directories
/// that do not exist yet are not watched (the caller's periodic rescan covers them). Dropping the
/// returned watcher stops the notifications.
pub fn watch(config: &Path, on_change: impl Fn() + Send + 'static) -> notify::Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        // Only real changes: inotify also reports opens and reads, and the index's own scan reads
        // every watched file, so counting those would make every scan trigger the next one.
        if event
            .is_ok_and(|event| matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)))
        {
            on_change();
        }
    })?;
    for dir in ["projects", "sessions"] {
        let path = config.join(dir);
        if path.is_dir() {
            watcher.watch(&path, RecursiveMode::Recursive)?;
        }
    }
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn writing_a_transcript_notifies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projects/-a")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let _watcher = watch(dir.path(), move || {
            let _ = tx.send(());
        })
        .unwrap();
        std::fs::write(dir.path().join("projects/-a/s.jsonl"), "{}\n").unwrap();
        rx.recv_timeout(Duration::from_secs(5)).expect("no change notification");
    }

    // Windows reports a read soon after a write as a change of the folder (see the next test).
    #[cfg(not(windows))]
    #[test]
    fn reading_does_not_notify() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projects/-a")).unwrap();
        std::fs::write(dir.path().join("projects/-a/s.jsonl"), "{}\n").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let _watcher = watch(dir.path(), move || {
            let _ = tx.send(());
        })
        .unwrap();
        // What a scan does: list folders, stat and read transcripts.
        for entry in std::fs::read_dir(dir.path().join("projects/-a")).unwrap().flatten() {
            let _ = entry.metadata();
            let _ = std::fs::read(entry.path());
        }
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "a read-only scan woke the index");
    }

    /// Opening a file within about a second of a write changes its folder on Windows, so the scan after a write can
    /// wake the index once more; it must settle instead of every scan waking the next.
    #[test]
    fn scans_after_a_write_settle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projects/-a")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let _watcher = watch(dir.path(), move || {
            let _ = tx.send(());
        })
        .unwrap();
        std::fs::write(dir.path().join("projects/-a/s.jsonl"), "{}\n").unwrap();
        let mut scans = 0;
        while rx.recv_timeout(Duration::from_millis(1500)).is_ok() {
            while rx.try_recv().is_ok() {}
            scans += 1;
            assert!(scans <= 3, "every scan wakes the index again");
            for entry in std::fs::read_dir(dir.path().join("projects/-a")).unwrap().flatten() {
                let _ = entry.metadata();
                let _ = std::fs::read(entry.path());
            }
        }
        assert!(scans >= 1, "the write did not wake the index");
    }

    #[test]
    fn a_config_dir_without_subdirectories_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(watch(dir.path(), || {}).is_ok());
    }
}
