//! Which preview files to read and when (through the daemon's `ReadFile`), and the GUI's byte cache.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::host::HostId;
use protocol::message::FileBody;

/// How often a shown file is read again.
pub const POLL: Duration = Duration::from_secs(2);

/// How long a request waits for its reply before it counts as lost and may be sent again.
pub const LOST_AFTER: Duration = Duration::from_secs(30);

/// The most file bytes kept in memory.
pub const BUDGET: usize = 64 * 1024 * 1024;

/// A file on a host.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileKey {
    /// The machine the file is on.
    pub host: HostId,
    /// Absolute path on that machine.
    pub path: PathBuf,
}

/// What the last reply said about a file.
#[derive(Clone, Debug, PartialEq)]
pub enum Shown {
    /// The file's contents and modification time (ms since the Unix epoch).
    Bytes {
        /// The file's contents.
        data: Arc<[u8]>,
        /// Modification time, ms since the Unix epoch.
        mtime_ms: u64,
    },
    /// The file no longer exists.
    Missing,
    /// The file is larger than the daemon sends.
    TooLarge {
        /// Its size in bytes.
        size: u64,
    },
    /// The file could not be read.
    Failed {
        /// Why, readable.
        reason: String,
    },
}

#[derive(Default)]
struct Entry {
    shown: Option<Shown>,
    /// The id of the request waiting for its reply and when it was sent.
    in_flight: Option<(u64, Instant)>,
    last_asked: Option<Instant>,
    /// Recency stamp: the larger, the more recently wanted.
    touched: u64,
}

/// The read bookkeeping and byte cache of all preview files.
#[derive(Default)]
pub struct Reads {
    entries: HashMap<FileKey, Entry>,
    /// Request id -> the file it asked for.
    pending: HashMap<u64, FileKey>,
    next_id: u64,
    clock: u64,
    /// The files wanted by the latest `due` call (and `request_now` since): never evicted.
    wanted: HashSet<FileKey>,
}

impl Reads {
    /// Requests for the wanted files that are due: nothing in flight and never asked or last asked `POLL` ago.
    ///
    /// Each is `(request id, file, known_mtime_ms)`. Marks them in flight and touches every wanted file for the cache.
    pub fn due(&mut self, wanted: &[FileKey], now: Instant) -> Vec<(u64, FileKey, Option<u64>)> {
        let mut out = Vec::new();
        self.wanted = wanted.iter().cloned().collect();
        for key in wanted {
            self.touch(key);
            self.expire(key, now);
            let entry = &self.entries[key];
            let ready = entry.in_flight.is_none() && entry.last_asked.is_none_or(|at| now.duration_since(at) >= POLL);
            if ready && let Some((id, known)) = self.start(key, now) {
                out.push((id, key.clone(), known));
            }
        }
        out
    }

    /// A request for one file whatever the poll wait; `None` while one is in flight.
    pub fn request_now(&mut self, key: &FileKey, now: Instant) -> Option<(u64, Option<u64>)> {
        self.touch(key);
        self.wanted.insert(key.clone());
        self.expire(key, now);
        self.start(key, now)
    }

    /// Takes a reply. Returns the file when what is shown for it changed.
    ///
    /// An unknown request id or another host's reply changes nothing. `Unchanged` ends the request only.
    pub fn apply(&mut self, host: &HostId, request: u64, body: FileBody) -> Option<FileKey> {
        if self.pending.get(&request).is_none_or(|key| &key.host != host) {
            return None;
        }
        let key = self.pending.remove(&request)?;
        let entry = self.entries.get_mut(&key)?;
        entry.in_flight = None;
        let new = match body {
            FileBody::Unchanged => return None,
            FileBody::Bytes { data, mtime_ms } => {
                // The cheap fields first; the bytes themselves only when those match. Nothing changed: no copy.
                if let Some(Shown::Bytes { data: old, mtime_ms: old_mtime }) = &entry.shown
                    && *old_mtime == mtime_ms
                    && old.len() == data.len()
                    && **old == data[..]
                {
                    return None;
                }
                Shown::Bytes { data: data.into(), mtime_ms }
            }
            FileBody::Missing => Shown::Missing,
            FileBody::TooLarge { size } => Shown::TooLarge { size },
            FileBody::Failed { reason } => Shown::Failed { reason },
        };
        let stores_bytes = matches!(new, Shown::Bytes { .. });
        let changed = entry.shown.as_ref() != Some(&new);
        entry.shown = Some(new);
        if stores_bytes {
            self.evict(&key);
        }
        changed.then_some(key)
    }

    /// What the last reply said about a file.
    pub fn shown(&self, key: &FileKey) -> Option<&Shown> {
        self.entries.get(key)?.shown.as_ref()
    }

    /// Whether a request for the file is waiting for its reply.
    pub fn in_flight(&self, key: &FileKey) -> bool {
        self.entries.get(key).is_some_and(|entry| entry.in_flight.is_some())
    }

    /// Drops a host's requests in flight (their replies are ignored) and its last-asked times; keeps what is shown.
    pub fn forget_host(&mut self, host: &HostId) {
        self.pending.retain(|_, key| &key.host != host);
        for (key, entry) in &mut self.entries {
            if &key.host == host {
                entry.in_flight = None;
                entry.last_asked = None;
            }
        }
    }

    fn touch(&mut self, key: &FileKey) {
        self.clock += 1;
        self.entries.entry(key.clone()).or_default().touched = self.clock;
    }

    /// Treats a request sent `LOST_AFTER` ago or more as lost: its late reply will be ignored.
    fn expire(&mut self, key: &FileKey, now: Instant) {
        let Some(entry) = self.entries.get_mut(key) else { return };
        if let Some((id, sent)) = entry.in_flight
            && now.duration_since(sent) >= LOST_AFTER
        {
            entry.in_flight = None;
            self.pending.remove(&id);
        }
    }

    fn start(&mut self, key: &FileKey, now: Instant) -> Option<(u64, Option<u64>)> {
        let entry = self.entries.get_mut(key)?;
        if entry.in_flight.is_some() {
            return None;
        }
        self.next_id += 1;
        let id = self.next_id;
        entry.in_flight = Some((id, now));
        entry.last_asked = Some(now);
        self.pending.insert(id, key.clone());
        let known = match &entry.shown {
            Some(Shown::Bytes { mtime_ms, .. }) => Some(*mtime_ms),
            _ => None,
        };
        Some((id, known))
    }

    /// Drops the least recently wanted bytes until the total is under `BUDGET`; never `keep` or a wanted file.
    fn evict(&mut self, keep: &FileKey) {
        let size = |entry: &Entry| match &entry.shown {
            Some(Shown::Bytes { data, .. }) => data.len(),
            _ => 0,
        };
        let mut total: usize = self.entries.values().map(size).sum();
        while total >= BUDGET {
            let oldest = self
                .entries
                .iter()
                .filter(|(key, entry)| *key != keep && !self.wanted.contains(*key) && size(entry) > 0)
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else { break };
            let entry = self.entries.get_mut(&oldest).expect("listed above");
            total -= size(entry);
            entry.shown = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(host: &str, path: &str) -> FileKey {
        FileKey { host: HostId::named(host), path: PathBuf::from(path) }
    }

    fn bytes(len: usize, mtime_ms: u64) -> FileBody {
        FileBody::Bytes { data: vec![7; len], mtime_ms }
    }

    fn local() -> HostId {
        HostId::local()
    }

    fn lkey(path: &str) -> FileKey {
        FileKey { host: local(), path: PathBuf::from(path) }
    }

    #[test]
    fn asks_once_then_every_poll() {
        let mut reads = Reads::default();
        let k = lkey("/a.md");
        let t0 = Instant::now();
        let first = reads.due(std::slice::from_ref(&k), t0);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].2, None);
        assert!(reads.due(std::slice::from_ref(&k), t0 + Duration::from_secs(1)).is_empty());
        reads.apply(&local(), first[0].0, bytes(3, 42));
        assert!(reads.due(std::slice::from_ref(&k), t0 + Duration::from_secs(1)).is_empty());
        let second = reads.due(std::slice::from_ref(&k), t0 + Duration::from_secs(2));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].2, Some(42));
        assert_ne!(second[0].0, first[0].0);
    }

    #[test]
    fn one_request_in_flight_per_file() {
        let mut reads = Reads::default();
        let k = lkey("/a.md");
        let t0 = Instant::now();
        assert_eq!(reads.due(std::slice::from_ref(&k), t0).len(), 1);
        assert!(reads.in_flight(&k));
        assert!(reads.due(std::slice::from_ref(&k), t0 + Duration::from_secs(10)).is_empty());
        assert!(reads.request_now(&k, t0 + Duration::from_secs(10)).is_none());
    }

    #[test]
    fn unchanged_changes_nothing() {
        let mut reads = Reads::default();
        let k = lkey("/a.md");
        let t0 = Instant::now();
        let id = reads.due(std::slice::from_ref(&k), t0)[0].0;
        assert_eq!(reads.apply(&local(), id, bytes(3, 1)), Some(k.clone()));
        let id = reads.due(std::slice::from_ref(&k), t0 + POLL)[0].0;
        assert_eq!(reads.apply(&local(), id, FileBody::Unchanged), None);
        assert!(!reads.in_flight(&k));
        assert!(matches!(reads.shown(&k), Some(Shown::Bytes { mtime_ms: 1, .. })));
    }

    #[test]
    fn a_deleted_file_shows_missing_and_comes_back() {
        let mut reads = Reads::default();
        let k = lkey("/a.md");
        let t0 = Instant::now();
        let id = reads.request_now(&k, t0).unwrap().0;
        assert_eq!(reads.apply(&local(), id, bytes(3, 1)), Some(k.clone()));
        let id = reads.request_now(&k, t0).unwrap().0;
        assert_eq!(reads.apply(&local(), id, FileBody::Missing), Some(k.clone()));
        assert_eq!(reads.shown(&k), Some(&Shown::Missing));
        let (id, known) = reads.request_now(&k, t0).unwrap();
        assert_eq!(known, None);
        assert_eq!(reads.apply(&local(), id, bytes(3, 1)), Some(k.clone()));
        let id = reads.request_now(&k, t0).unwrap().0;
        assert_eq!(reads.apply(&local(), id, bytes(3, 1)), None);
    }

    #[test]
    fn too_large_and_failed_are_shown() {
        let mut reads = Reads::default();
        let (a, b) = (lkey("/a"), lkey("/b"));
        let t0 = Instant::now();
        let due = reads.due(&[a.clone(), b.clone()], t0);
        assert_eq!(reads.apply(&local(), due[0].0, FileBody::TooLarge { size: 9 }), Some(a.clone()));
        assert_eq!(reads.apply(&local(), due[1].0, FileBody::Failed { reason: "denied".into() }), Some(b.clone()));
        assert_eq!(reads.shown(&a), Some(&Shown::TooLarge { size: 9 }));
        assert_eq!(reads.shown(&b), Some(&Shown::Failed { reason: "denied".into() }));
    }

    #[test]
    fn a_reply_from_another_host_or_an_old_id_is_ignored() {
        let mut reads = Reads::default();
        let k = key("box", "/a.md");
        let id = reads.request_now(&k, Instant::now()).unwrap().0;
        assert_eq!(reads.apply(&local(), id, bytes(1, 1)), None);
        assert!(reads.in_flight(&k));
        assert_eq!(reads.apply(&k.host, id + 100, bytes(1, 1)), None);
        assert_eq!(reads.shown(&k), None);
        assert_eq!(reads.apply(&k.host, id, bytes(1, 1)), Some(k.clone()));
        assert_eq!(reads.apply(&k.host, id, bytes(2, 2)), None);
    }

    #[test]
    fn forgetting_a_host_drops_its_requests() {
        let mut reads = Reads::default();
        let k = key("box", "/a.md");
        let t0 = Instant::now();
        let id = reads.due(std::slice::from_ref(&k), t0)[0].0;
        reads.forget_host(&k.host);
        assert!(!reads.in_flight(&k));
        assert_eq!(reads.apply(&k.host, id, bytes(1, 1)), None);
        assert_eq!(reads.due(std::slice::from_ref(&k), t0 + Duration::from_millis(1)).len(), 1);
    }

    #[test]
    fn the_cache_stays_under_budget() {
        let mut reads = Reads::default();
        let keys = [lkey("/1"), lkey("/2"), lkey("/3")];
        let t0 = Instant::now();
        for k in &keys {
            let id = reads.due(std::slice::from_ref(k), t0)[0].0;
            reads.apply(&local(), id, bytes(30 * 1024 * 1024, 1));
        }
        assert_eq!(reads.shown(&keys[0]), None);
        assert!(reads.shown(&keys[1]).is_some());
        assert!(reads.shown(&keys[2]).is_some());
    }

    #[test]
    fn request_now_ignores_the_poll_wait() {
        let mut reads = Reads::default();
        let k = lkey("/a.md");
        let t0 = Instant::now();
        let id = reads.due(std::slice::from_ref(&k), t0)[0].0;
        reads.apply(&local(), id, bytes(1, 5));
        assert!(reads.due(std::slice::from_ref(&k), t0).is_empty());
        let (_, known) = reads.request_now(&k, t0).unwrap();
        assert_eq!(known, Some(5));
    }

    #[test]
    fn a_lost_request_expires_and_its_late_reply_is_ignored() {
        let mut reads = Reads::default();
        let k = lkey("/a.md");
        let t0 = Instant::now();
        let old = reads.due(std::slice::from_ref(&k), t0)[0].0;
        assert!(reads.due(std::slice::from_ref(&k), t0 + LOST_AFTER - Duration::from_secs(1)).is_empty());
        let again = reads.due(std::slice::from_ref(&k), t0 + LOST_AFTER);
        assert_eq!(again.len(), 1);
        assert_ne!(again[0].0, old);
        assert_eq!(reads.apply(&local(), old, bytes(1, 1)), None);
        assert_eq!(reads.shown(&k), None);
        assert!(reads.in_flight(&k));
        assert!(reads.request_now(&k, t0 + LOST_AFTER * 2).is_some());
    }

    #[test]
    fn a_reply_with_the_same_bytes_changes_nothing() {
        let mut reads = Reads::default();
        let k = lkey("/a.md");
        let reply = |reads: &mut Reads, body| {
            let id = reads.request_now(&k, Instant::now()).unwrap().0;
            reads.apply(&local(), id, body)
        };
        assert_eq!(reply(&mut reads, bytes(3, 1)), Some(k.clone()));
        let shown = reads.shown(&k).cloned();
        assert_eq!(reply(&mut reads, bytes(3, 1)), None);
        assert_eq!(reads.shown(&k), shown.as_ref());
        // A new mtime, a new length, or the same length with other bytes is a change.
        assert_eq!(reply(&mut reads, bytes(3, 2)), Some(k.clone()));
        assert_eq!(reply(&mut reads, bytes(4, 2)), Some(k.clone()));
        let other = FileBody::Bytes { data: vec![8; 4], mtime_ms: 2 };
        assert_eq!(reply(&mut reads, other), Some(k.clone()));
    }

    #[test]
    fn forgetting_a_host_keeps_what_is_shown() {
        let mut reads = Reads::default();
        let k = key("box", "/a.md");
        let id = reads.request_now(&k, Instant::now()).unwrap().0;
        reads.apply(&k.host, id, bytes(2, 3));
        reads.forget_host(&k.host);
        assert!(matches!(reads.shown(&k), Some(Shown::Bytes { mtime_ms: 3, .. })));
    }

    #[test]
    fn the_key_just_stored_is_never_evicted() {
        let mut reads = Reads::default();
        let keys = [lkey("/1"), lkey("/2"), lkey("/3")];
        let t0 = Instant::now();
        let ids: Vec<u64> = keys.iter().map(|k| reads.request_now(k, t0).unwrap().0).collect();
        reads.due(&[], t0);
        reads.apply(&local(), ids[1], bytes(30 * 1024 * 1024, 1));
        reads.apply(&local(), ids[2], bytes(30 * 1024 * 1024, 1));
        reads.apply(&local(), ids[0], bytes(30 * 1024 * 1024, 1));
        assert!(reads.shown(&keys[0]).is_some());
        assert_eq!(reads.shown(&keys[1]), None);
        assert!(reads.shown(&keys[2]).is_some());
    }

    #[test]
    fn wanted_files_stay_cached_over_budget() {
        let mut reads = Reads::default();
        let keys = [lkey("/1"), lkey("/2"), lkey("/3"), lkey("/4")];
        let t0 = Instant::now();
        let due = reads.due(&keys, t0);
        for (id, _, _) in due {
            reads.apply(&local(), id, bytes(25 * 1024 * 1024, 1));
        }
        assert!(keys.iter().all(|k| reads.shown(k).is_some()));
        let due = reads.due(&keys[1..], t0 + POLL);
        assert_eq!(due.len(), 3);
        reads.apply(&local(), due[0].0, bytes(25 * 1024 * 1024, 2));
        assert_eq!(reads.shown(&keys[0]), None);
        assert!(keys[1..].iter().all(|k| reads.shown(k).is_some()));
    }
}
