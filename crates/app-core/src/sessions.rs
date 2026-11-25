//! The GUI's copy of the daemons' session lists, one list per host.

use std::collections::{HashMap, HashSet};

use protocol::host::HostId;
use protocol::message::SessionInfo;

/// Sessions by host and id, kept in sync from `Sessions` / `SessionsChanged`. Every session is tagged with the host
/// it came from.
#[derive(Default)]
pub struct SessionStore {
    sessions: HashMap<HostId, HashMap<String, SessionInfo>>,
    loaded: HashSet<HostId>,
}

fn tagged(mut session: SessionInfo, host: &HostId) -> SessionInfo {
    session.host = host.clone();
    session
}

impl SessionStore {
    /// Replace everything `host` reported with `sessions`.
    pub fn replace(&mut self, host: &HostId, sessions: Vec<SessionInfo>) {
        let list = sessions.into_iter().map(|session| (session.id.clone(), tagged(session, host))).collect();
        self.sessions.insert(host.clone(), list);
        self.loaded.insert(host.clone());
    }

    /// Insert or update `upserted`, then drop `removed`, in `host`'s list.
    pub fn apply(&mut self, host: &HostId, upserted: Vec<SessionInfo>, removed: &[String]) {
        let list = self.sessions.entry(host.clone()).or_default();
        for session in upserted {
            list.insert(session.id.clone(), tagged(session, host));
        }
        for id in removed {
            list.remove(id);
        }
    }

    /// Whether `host`'s full list has arrived (before that, an unknown id proves nothing).
    pub fn loaded(&self, host: &HostId) -> bool {
        self.loaded.contains(host)
    }

    /// Session `id` of `host`.
    pub fn get(&self, host: &HostId, id: &str) -> Option<&SessionInfo> {
        self.sessions.get(host)?.get(id)
    }

    /// Every session of every host, in no particular order.
    pub fn all(&self) -> impl Iterator<Item = &SessionInfo> {
        self.sessions.values().flat_map(|list| list.values())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::message::SessionState;

    fn info(id: &str, title: &str) -> SessionInfo {
        SessionInfo {
            sent_files: Vec::new(),
            id: id.into(),
            cwd: "/".into(),
            title: title.into(),
            last_active_ms: 0,
            state: SessionState::Closed,
            cwd_exists: true,
            status: None,
            status_since_ms: 0,
            container: None,
            claude_dir: None,
            last_prompt: None,
            host: HostId::local(),
        }
    }

    fn devbox() -> HostId {
        HostId::named("devbox")
    }

    #[test]
    fn a_full_list_replaces_and_changes_apply() {
        let local = HostId::local();
        let mut store = SessionStore::default();
        store.replace(&local, vec![info("a", "A"), info("b", "B")]);
        store.replace(&local, vec![info("b", "B"), info("c", "C")]);
        assert!(store.get(&local, "a").is_none());
        store.apply(&local, vec![info("c", "C2"), info("d", "D")], &["b".to_string()]);
        let mut ids: Vec<&str> = store.all().map(|s| s.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["c", "d"]);
        assert_eq!(store.get(&local, "c").unwrap().title, "C2");
    }

    #[test]
    fn each_host_has_its_own_list_and_its_sessions_are_tagged() {
        let (local, devbox) = (HostId::local(), devbox());
        let mut store = SessionStore::default();
        store.replace(&local, vec![info("same", "mine")]);
        store.replace(&devbox, vec![info("same", "theirs")]);
        assert_eq!(store.get(&local, "same").unwrap().title, "mine");
        assert_eq!(store.get(&devbox, "same").unwrap().title, "theirs");
        assert_eq!(store.get(&devbox, "same").unwrap().host, devbox);
        assert_eq!(store.all().count(), 2);
        // A new list from one host leaves the other host's sessions alone.
        store.replace(&devbox, Vec::new());
        assert_eq!(store.all().map(|s| s.title.as_str()).collect::<Vec<_>>(), vec!["mine"]);
        store.apply(&devbox, vec![info("x", "X")], &[]);
        assert_eq!(store.get(&devbox, "x").unwrap().host, devbox);
    }

    #[test]
    fn a_host_is_loaded_once_its_full_list_arrives() {
        let (local, devbox) = (HostId::local(), devbox());
        let mut store = SessionStore::default();
        store.apply(&local, vec![info("a", "A")], &[]);
        assert!(!store.loaded(&local));
        store.replace(&local, Vec::new());
        assert!(store.loaded(&local));
        assert!(!store.loaded(&devbox), "the other host's list has not arrived");
    }
}
