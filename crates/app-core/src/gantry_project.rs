//! Gantry projects: projects the user makes by hand, identified by their name (unique ignoring case), keyed
//! `gantry://<lowercased name>` where a folder project is keyed by its path.

use std::path::{Path, PathBuf};

/// The start of a Gantry project's key.
pub const KEY_PREFIX: &str = "gantry://";

/// Why a project name was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectError {
    /// Nothing but spaces.
    Empty,
    /// Another project already has this name (ignoring case).
    Exists,
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

/// The name to use for a project called `name` among `existing`: trimmed. `own` is the project being renamed, which
/// may keep its name (also in another case). `Empty` or `Exists` otherwise.
pub fn validate_name(existing: &[String], name: &str, own: Option<&str>) -> Result<String, ProjectError> {
    let name = normalize(name).ok_or(ProjectError::Empty)?;
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
    fn allows_own_name_when_renaming() {
        let list = vec!["Billing".to_string(), "Docs".to_string()];
        assert_eq!(validate_name(&list, "BILLING", Some("Billing")), Ok("BILLING".into()));
        assert_eq!(validate_name(&list, "docs", Some("Billing")), Err(ProjectError::Exists));
    }
}
