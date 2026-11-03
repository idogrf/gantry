//! The launcher's folder browser: breadcrumbs, going up and down, and the filter over a folder's subfolders.
//! Paths are the absolute ones the daemon answers with (`~` already expanded).

use protocol::message::DirEntry;

use crate::fuzzy::Fuzzy;

/// One clickable segment of the current folder's path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Crumb {
    /// What the segment shows: `~`, `/` or a folder name.
    pub label: String,
    /// The absolute folder it lists.
    pub path: String,
}

/// The part of `path` below `home` (empty for home itself); `None` when `path` is not home or under it.
fn below_home<'a>(path: &'a str, home: Option<&str>) -> Option<&'a str> {
    let home = home?.trim_end_matches('/');
    if home.is_empty() {
        return None;
    }
    let rest = path.strip_prefix(home)?;
    if rest.is_empty() { Some("") } else { rest.strip_prefix('/') }
}

/// The segments of `path`: the home folder (or `path` equal to it) is `~` followed by the folders below it, any other
/// path starts with `/`.
pub fn crumbs(path: &str, home: Option<&str>) -> Vec<Crumb> {
    let (mut crumbs, mut at, rest) = match below_home(path, home) {
        Some(rest) => {
            let home = home.unwrap_or_default().trim_end_matches('/').to_string();
            (vec![Crumb { label: "~".into(), path: home.clone() }], home, rest)
        }
        None => (vec![Crumb { label: "/".into(), path: "/".into() }], String::new(), path),
    };
    for segment in rest.split('/').filter(|segment| !segment.is_empty()) {
        at = format!("{at}/{segment}");
        crumbs.push(Crumb { label: segment.to_string(), path: at.clone() });
    }
    crumbs
}

/// `path` as shown to the user: `~`, `~/a/b` under home, else the absolute path.
pub fn shown(path: &str, home: Option<&str>) -> String {
    match below_home(path, home) {
        Some("") => "~".to_string(),
        Some(rest) => format!("~/{rest}"),
        None => path.to_string(),
    }
}

/// The folder above `path`; `None` at `/`.
pub fn parent(path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return None;
    }
    let at = path.rfind('/')?;
    Some(if at == 0 { "/".to_string() } else { path[..at].to_string() })
}

/// The path of the folder `name` inside `path`.
pub fn child(path: &str, name: &str) -> String {
    format!("{}/{name}", path.trim_end_matches('/'))
}

/// A subfolder that passed the filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    /// Its position in the listing.
    pub index: usize,
    /// A dot folder: drawn dimmed.
    pub dimmed: bool,
    /// The character indices of its name the query matched.
    pub bold: Vec<u32>,
}

/// The subfolders matching `query`, in the listing's order (the daemon's: dot folders last); all of them when the
/// query is blank.
pub fn filter(dirs: &[DirEntry], query: &str) -> Vec<Shown> {
    let mut fuzzy = Fuzzy::new(query, dirs.iter().map(|dir| dir.name.as_str()));
    dirs.iter()
        .enumerate()
        .filter_map(|(index, dir)| {
            let bold = match &mut fuzzy {
                None => Vec::new(),
                Some(fuzzy) => fuzzy.indices(&dir.name)?,
            };
            Some(Shown { index, dimmed: dir.name.starts_with('.'), bold })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(crumbs: &[Crumb]) -> Vec<&str> {
        crumbs.iter().map(|crumb| crumb.label.as_str()).collect()
    }

    fn paths(crumbs: &[Crumb]) -> Vec<&str> {
        crumbs.iter().map(|crumb| crumb.path.as_str()).collect()
    }

    #[test]
    fn crumbs_under_home() {
        let found = crumbs("/home/u/work/inference", Some("/home/u"));
        assert_eq!(labels(&found), ["~", "work", "inference"]);
        assert_eq!(paths(&found), ["/home/u", "/home/u/work", "/home/u/work/inference"]);
    }

    #[test]
    fn crumbs_outside_home() {
        let found = crumbs("/srv/data", Some("/home/u"));
        assert_eq!(labels(&found), ["/", "srv", "data"]);
        assert_eq!(paths(&found), ["/", "/srv", "/srv/data"]);
    }

    #[test]
    fn crumbs_at_home_and_root() {
        assert_eq!(labels(&crumbs("/home/u", Some("/home/u"))), ["~"]);
        assert_eq!(labels(&crumbs("/", Some("/home/u"))), ["/"]);
        assert_eq!(labels(&crumbs("/root", Some("/root"))), ["~"]);
        assert_eq!(labels(&crumbs("/home/uv", Some("/home/u")))[0], "/");
        assert_eq!(labels(&crumbs("/srv", None)), ["/", "srv"]);
    }

    #[test]
    fn shown_uses_tilde() {
        assert_eq!(shown("/home/u/x", Some("/home/u")), "~/x");
        assert_eq!(shown("/home/u", Some("/home/u")), "~");
        assert_eq!(shown("/srv", Some("/home/u")), "/srv");
    }

    #[test]
    fn parent_and_child() {
        assert_eq!(parent("/"), None);
        assert_eq!(parent("/a"), Some("/".to_string()));
        assert_eq!(parent("/a/b"), Some("/a".to_string()));
        assert_eq!(child("/", "x"), "/x");
        assert_eq!(child("/a", "x"), "/a/x");
    }

    #[test]
    fn filter_keeps_order_dims_dot_folders_and_bolds() {
        let dirs: Vec<DirEntry> = ["serving", "services", "model_sets", ".secrets", "docs"]
            .into_iter()
            .map(|name| DirEntry { name: name.into(), symlink: false })
            .collect();
        let found = filter(&dirs, "se");
        assert_eq!(found.iter().map(|shown| shown.index).collect::<Vec<_>>(), [0, 1, 2, 3]);
        assert_eq!(found.iter().map(|shown| shown.dimmed).collect::<Vec<_>>(), [false, false, false, true]);
        assert_eq!(found[0].bold, vec![0, 1]);
        assert_eq!(filter(&dirs, "").len(), 5);
    }
}
