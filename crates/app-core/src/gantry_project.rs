//! Gantry projects: projects the user makes by hand, identified by their name (unique ignoring case), keyed
//! `gantry://<lowercased name>` where a folder project is keyed by its path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::split_tree::NodeId;

/// The start of a Gantry project's key.
pub const KEY_PREFIX: &str = "gantry://";

/// What is dropped on a project heading to put it in (or take it out of) a Gantry project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Assign {
    /// A pane (a split tab's pane is pulled out into a tab of its own).
    Leaf(NodeId),
    /// A split tab, by the first leaf of its panes: every pane goes.
    Tab(NodeId),
}

/// Why a project name was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectError {
    /// Nothing but spaces.
    Empty,
    /// Another project already has this name (ignoring case).
    Exists,
    /// The name contains `/`, which a project key (a path) cannot tell apart from a separator.
    Slash,
}

impl ProjectError {
    /// The line shown under the name field; `None` for an empty name, which just does nothing.
    pub fn message(self) -> Option<&'static str> {
        match self {
            ProjectError::Empty => None,
            ProjectError::Exists => Some("A project with this name already exists"),
            ProjectError::Slash => Some("A project name cannot contain /"),
        }
    }
}

/// `name` without surrounding spaces; `None` when nothing is left.
pub fn normalize(name: &str) -> Option<String> {
    Some(name.trim().to_string()).filter(|name| !name.is_empty())
}

/// The project key of `name`: `gantry://` and the trimmed, lowercased name.
pub fn key(name: &str) -> PathBuf {
    PathBuf::from(format!("{KEY_PREFIX}{}", name.trim().to_lowercase()))
}

/// The lowercased name inside a Gantry project key; `None` for any other key.
pub fn name_of_key(key: &Path) -> Option<String> {
    key.to_str()?.strip_prefix(KEY_PREFIX).map(String::from)
}

/// Whether `key` is a Gantry project's.
pub fn is_key(key: &Path) -> bool {
    name_of_key(key).is_some()
}

/// Whether two names are the same project: equal ignoring case and surrounding spaces.
pub fn same(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// The display name of the project `key` names: the one of `existing` with that key, else `fallback` (the name
/// stored with a session, for a deleted project), else the lowercased name from the key.
pub fn display_name(key: &Path, existing: &[String], fallback: Option<&str>) -> Option<String> {
    let lowered = name_of_key(key)?;
    existing
        .iter()
        .find(|name| same(name, &lowered))
        .map(String::as_str)
        .or(fallback)
        .map(String::from)
        .or(Some(lowered))
}

/// Which Gantry project each session is in, as the workspace holds it, for the lists that show sessions without a pane
/// (Recents).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Membership {
    /// The projects that exist (display names).
    pub projects: Vec<String>,
    /// Session id -> the project (display name) it was put in, deleted projects included.
    pub sessions: BTreeMap<String, String>,
}

impl Membership {
    /// The project key of session `id`: its Gantry project when it has one (even a deleted one), else `folder()`.
    pub fn key_of(&self, id: &str, folder: impl FnOnce() -> PathBuf) -> PathBuf {
        self.sessions.get(id).map_or_else(folder, |name| key(name))
    }

    /// Every name a `gantry://` key may need to be shown as: the existing projects, then those only sessions keep.
    pub fn names(&self) -> Vec<String> {
        self.projects.iter().chain(self.sessions.values()).cloned().collect()
    }
}

/// The name to use for a project called `name` among `existing`: trimmed. `own` is the project being renamed, which
/// may keep its name (also in another case). `Empty` or `Exists` otherwise.
pub fn validate_name(existing: &[String], name: &str, own: Option<&str>) -> Result<String, ProjectError> {
    let name = normalize(name).ok_or(ProjectError::Empty)?;
    if name.contains('/') {
        return Err(ProjectError::Slash);
    }
    let taken = existing.iter().any(|other| same(other, &name) && !own.is_some_and(|own| same(own, other)));
    if taken { Err(ProjectError::Exists) } else { Ok(name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_trims_and_rejects_empty() {
        assert_eq!(normalize("  Billing "), Some("Billing".into()));
        assert_eq!(normalize("   "), None);
    }

    #[test]
    fn key_lowercases_and_round_trips_name() {
        let k = key("  Billing ");
        assert_eq!(k, PathBuf::from("gantry://billing"));
        assert_eq!(name_of_key(&k), Some("billing".into()));
        assert!(is_key(&k));
        assert!(!is_key(Path::new("/home/me/billing")));
        assert!(!is_key(Path::new("ssh://host/billing")));
    }

    #[test]
    fn display_name_prefers_the_saved_list() {
        let list = vec!["Billing".to_string()];
        assert_eq!(display_name(&key("billing"), &list, Some("BILLING")), Some("Billing".into()));
        assert_eq!(display_name(&key("gone"), &list, Some("Gone")), Some("Gone".into()));
        assert_eq!(display_name(&key("gone"), &list, None), Some("gone".into()));
        assert_eq!(display_name(Path::new("/a"), &list, None), None);
    }

    #[test]
    fn membership_keys_sessions_and_lists_every_name() {
        let membership = Membership {
            projects: vec!["Billing".into()],
            sessions: BTreeMap::from([("a".to_string(), "Gone".to_string())]),
        };
        assert_eq!(membership.key_of("a", || PathBuf::from("/r")), PathBuf::from("gantry://gone"));
        assert_eq!(membership.key_of("b", || PathBuf::from("/r")), PathBuf::from("/r"));
        assert_eq!(membership.names(), vec!["Billing".to_string(), "Gone".to_string()]);
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(validate_name(&[], "  ", None), Err(ProjectError::Empty));
    }

    #[test]
    fn rejects_existing_ignoring_case_and_spaces() {
        let list = vec!["Billing".to_string()];
        assert_eq!(validate_name(&list, "  BILLING ", None), Err(ProjectError::Exists));
        assert_eq!(validate_name(&list, "Other", None), Ok("Other".into()));
    }

    #[test]
    fn rejects_slashes() {
        assert_eq!(validate_name(&[], "a/b", None), Err(ProjectError::Slash));
        assert_eq!(validate_name(&[], "foo/", None), Err(ProjectError::Slash));
        assert!(ProjectError::Slash.message().is_some() && ProjectError::Empty.message().is_none());
    }

    #[test]
    fn allows_own_name_when_renaming() {
        let list = vec!["Billing".to_string(), "Docs".to_string()];
        assert_eq!(validate_name(&list, "BILLING", Some("Billing")), Ok("BILLING".into()));
        assert_eq!(validate_name(&list, "docs", Some("Billing")), Err(ProjectError::Exists));
    }
}
