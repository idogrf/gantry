//! The applied settings (a GPUI global): loading `settings.json`, watching it, saving changes, and rebuilding
//! the [`Theme`] from them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use app_core::settings::{self, Loaded, Problem, Settings};
use app_core::shortcuts::{self, Check, Platform};
use app_core::theme_colors::{resolve, tokens, usable_fill};
use app_core::workspace::{ClaudeDirs, DirBadge, ResolvedDir};
use gpui::{App, Global, Task, WindowAppearance};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use protocol::host::HostId;
use serde_json::{Map, Value};

use crate::theme::Theme;

/// Interface font size that draws at scale 1.0.
const BASE_UI_FONT_SIZE: f32 = 14.0;
/// Quiet time after the last file event before the file is read.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// What to do with a freshly read settings file.
#[derive(Debug, PartialEq)]
enum Reload {
    /// Replace the applied settings.
    Apply(Box<Settings>),
    /// Leave them alone (unchanged, or the file is not JSON).
    Keep,
}

fn reload_decision(applied: &Settings, loaded: &Loaded) -> Reload {
    match &loaded.settings {
        Some(settings) if settings != applied => Reload::Apply(Box::new(settings.clone())),
        _ => Reload::Keep,
    }
}

/// Whether a reload must re-apply: the settings changed, or the file's problems did (Gantry's own write changes neither).
fn reload_needs_apply(decision: &Reload, old: &[Problem], new: &[Problem]) -> bool {
    matches!(decision, Reload::Apply(_)) || old != new
}

/// Save a UI change unless the file is broken (a save would overwrite the user's hand edits); say so instead.
fn save_or_explain(broken: bool, problems: &mut Vec<Problem>, save: impl FnOnce() -> std::io::Result<()>) {
    let problem = if broken {
        Problem { key: String::new(), message: "Not saved: fix settings.json first".into() }
    } else {
        match save() {
            Ok(()) => return,
            Err(error) => Problem { key: String::new(), message: format!("could not save: {error}") },
        }
    };
    if !broken || !problems.contains(&problem) {
        problems.push(problem);
    }
}

/// The applied settings, the raw JSON they came from, what was wrong with the file, and the file watcher.
pub(crate) struct SettingsStore {
    settings: Settings,
    raw: Map<String, Value>,
    /// The last read of the file was not JSON: changes apply in memory only, so hand edits are not overwritten.
    broken: bool,
    /// Problems found reading (or writing) the file.
    file_problems: Vec<Problem>,
    /// `file_problems` plus the status fills the theme cannot draw readably.
    problems: Vec<Problem>,
    path: PathBuf,
    /// The `shortcuts` the key bindings were last built from.
    bound: Option<BTreeMap<String, Option<String>>>,
    _watcher: Option<RecommendedWatcher>,
    _reload: Option<Task<()>>,
}

impl Global for SettingsStore {}

impl SettingsStore {
    /// Load `path` and install both globals (the theme resolved with the current system appearance).
    pub fn init(path: PathBuf, cx: &mut App) {
        let loaded = settings::load(&path);
        let (watcher, reload) = watch(&path, cx).unzip();
        cx.set_global(SettingsStore {
            broken: loaded.settings.is_none(),
            settings: loaded.settings.unwrap_or_default(),
            raw: loaded.raw,
            file_problems: loaded.problems,
            problems: Vec::new(),
            path,
            bound: None,
            _watcher: watcher,
            _reload: reload,
        });
        apply(cx);
    }

    /// The applied settings.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Everything wrong with the file or the settings, for the Settings tab's banner.
    pub fn problems(&self) -> &[Problem] {
        &self.problems
    }

    /// Where the settings are stored.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The last read of the file was not JSON, so changes are not saved.
    pub fn broken(&self) -> bool {
        self.broken
    }

    /// Change the settings from the UI: apply, save (skipped if unchanged), refresh every window.
    pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) {
        let store = cx.global_mut::<SettingsStore>();
        let mut new = store.settings.clone();
        change(&mut new);
        if new == store.settings {
            return;
        }
        let (path, raw) = (&store.path, &store.raw);
        save_or_explain(store.broken, &mut store.file_problems, || settings::save(path, &new, raw));
        store.settings = new;
        apply(cx);
    }

    /// Re-resolve the theme (the system appearance changed).
    pub fn reapply(cx: &mut App) {
        apply(cx);
    }

    fn reload(cx: &mut App) {
        let store = cx.global_mut::<SettingsStore>();
        let loaded = settings::load(&store.path);
        let decision = reload_decision(&store.settings, &loaded);
        // A broken file reads as an empty object: keep the last good one so a later save keeps unknown keys.
        if loaded.settings.is_some() {
            store.raw = loaded.raw;
        }
        store.broken = loaded.settings.is_none();
        let reapply = reload_needs_apply(&decision, &store.file_problems, &loaded.problems);
        store.file_problems = loaded.problems;
        if let Reload::Apply(settings) = decision {
            store.settings = *settings;
        }
        if reapply {
            apply(cx);
        }
    }
}

/// Rebuild the [`Theme`] from the settings and redraw everything. Each window follows through its observer.
fn apply(cx: &mut App) {
    let dark = matches!(cx.window_appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark);
    let store = cx.global_mut::<SettingsStore>();
    let settings = &store.settings;
    let preset = resolve(settings.theme, dark);
    let tokens = tokens(preset);
    let mut fills = settings.status_fills;
    let mut problems = store.file_problems.clone();
    for (name, fill) in [
        ("needsInput", &mut fills.needs_input),
        ("working", &mut fills.working),
        ("done", &mut fills.done),
        ("idle", &mut fills.idle),
        ("unread", &mut fills.unread),
    ] {
        if fill.is_some_and(|color| !usable_fill(color, &tokens)) {
            *fill = None;
            problems.push(Problem { key: format!("statusColors.{name}"), message: "too low contrast".into() });
        }
    }
    problems.extend(shortcut_problems(&settings.shortcuts));
    let ui_scale = settings.ui_font_size / BASE_UI_FONT_SIZE;
    let project_colors = settings.project_colors.clone();
    let selection = settings.selection;
    store.problems = problems;
    let rebind = store.bound.as_ref() != Some(&store.settings.shortcuts);
    if rebind {
        store.bound = Some(store.settings.shortcuts.clone());
        let bindings = crate::workspace_view::all_bindings(&store.settings, Platform::current());
        cx.clear_key_bindings();
        cx.bind_keys(bindings);
    }
    cx.set_global(Theme { preset, tokens, fills, selection, ui_scale, project_colors });
    cx.refresh_windows();
}

/// A problem for each override that is not a usable keystroke (it is ignored; the action keeps its default).
fn shortcut_problems(overrides: &BTreeMap<String, Option<String>>) -> Vec<Problem> {
    overrides
        .iter()
        .filter(|(id, _)| shortcuts::ACTIONS.iter().any(|action| action.id == id.as_str()))
        .filter_map(|(id, keystroke)| match shortcuts::check(keystroke.as_deref()?, id, &[]) {
            Check::Invalid => {
                Some(Problem { key: format!("shortcuts.{id}"), message: "not a keystroke; the default is used".into() })
            }
            Check::Ok | Check::Reserved(_) | Check::Conflict(_) => None,
        })
        .collect()
}

/// The label of `action`'s keystroke as the settings have it ("Not set" when unbound).
pub(crate) fn shortcut_label(settings: &Settings, action: &str) -> String {
    let platform = Platform::current();
    shortcuts::label(shortcuts::current(action, &settings.shortcuts, platform).as_deref(), platform)
}

/// The badge of the config dir at `claude_dir`, shown only with two or more config dirs and never for an SSH host
/// (a remote session always runs the remote's default `claude`).
pub(crate) fn dir_badge_for(settings: &Settings, claude_dir: Option<&Path>, host: &HostId) -> Option<DirBadge> {
    if settings.claude_dirs.len() < 2 || !host.is_local() {
        return None;
    }
    let dirs = ClaudeDirs::from_settings(settings, None, std::env::home_dir().as_deref());
    dirs.by_path(claude_dir?).map(ResolvedDir::badge)
}

/// Watch the settings file's directory; a foreground task reloads after the events settle.
fn watch(path: &Path, cx: &mut App) -> Option<(RecommendedWatcher, Task<()>)> {
    let dir = path.parent()?;
    let name = path.file_name()?.to_owned();
    if let Err(error) = std::fs::create_dir_all(dir) {
        tracing::warn!("settings directory not created: {error}");
        return None;
    }
    let (events, changes) = async_channel::unbounded::<()>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event
            && event.paths.iter().any(|path| path.file_name() == Some(name.as_os_str()))
        {
            let _ = events.try_send(());
        }
    })
    .inspect_err(|error| tracing::warn!("settings file not watched: {error}"))
    .ok()?;
    watcher
        .watch(dir, RecursiveMode::NonRecursive)
        .inspect_err(|error| tracing::warn!("settings file not watched: {error}"))
        .ok()?;
    let task = cx.spawn(async move |cx| {
        while changes.recv().await.is_ok() {
            cx.background_executor().timer(DEBOUNCE).await;
            while changes.try_recv().is_ok() {}
            cx.update(SettingsStore::reload);
        }
    });
    Some((watcher, task))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(settings: Option<Settings>, problems: Vec<Problem>) -> Loaded {
        Loaded { settings, problems, raw: Map::new() }
    }

    #[test]
    fn own_write_is_not_a_change() {
        let applied = Settings::default();
        assert_eq!(reload_decision(&applied, &loaded(Some(applied.clone()), Vec::new())), Reload::Keep);
    }

    #[test]
    fn broken_file_keeps_applied() {
        let problem = Problem { key: String::new(), message: "not JSON".into() };
        let broken = loaded(None, vec![problem]);
        assert_eq!(reload_decision(&Settings::default(), &broken), Reload::Keep);
        assert_eq!(broken.problems.len(), 1);
    }

    #[test]
    fn keep_reapplies_only_when_problems_change() {
        let problem = Problem { key: "alerts.volume".into(), message: "bad".into() };
        assert!(!reload_needs_apply(&Reload::Keep, &[], &[]));
        assert!(!reload_needs_apply(&Reload::Keep, std::slice::from_ref(&problem), std::slice::from_ref(&problem)));
        assert!(reload_needs_apply(&Reload::Keep, &[], std::slice::from_ref(&problem)));
        assert!(reload_needs_apply(&Reload::Keep, std::slice::from_ref(&problem), &[]));
        assert!(reload_needs_apply(&Reload::Apply(Box::default()), &[], &[]));
    }

    #[test]
    fn broken_file_is_never_saved_over() {
        let mut problems = Vec::new();
        let mut saved = false;
        save_or_explain(true, &mut problems, || {
            saved = true;
            Ok(())
        });
        assert!(!saved);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].message.starts_with("Not saved"));
        // Repeated changes do not repeat the message.
        save_or_explain(true, &mut problems, || Ok(()));
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn good_file_is_saved_and_failure_reported() {
        let mut problems = Vec::new();
        let mut saved = false;
        save_or_explain(false, &mut problems, || {
            saved = true;
            Ok(())
        });
        assert!(saved && problems.is_empty());
        save_or_explain(false, &mut problems, || Err(std::io::Error::other("disk full")));
        assert_eq!(problems.len(), 1);
        assert!(problems[0].message.contains("disk full"));
    }

    #[test]
    fn changed_file_applies() {
        let mut changed = Settings::default();
        changed.terminal_font.size = 18.0;
        assert_eq!(
            reload_decision(&Settings::default(), &loaded(Some(changed.clone()), Vec::new())),
            Reload::Apply(Box::new(changed))
        );
    }

    #[test]
    fn only_shortcut_overrides_that_are_not_keystrokes_are_problems() {
        let overrides: BTreeMap<String, Option<String>> = [
            ("closePane", Some("ctrl-c")),
            ("copy", Some("bogus-key")),
            ("toggleSidebar", Some("ctrl-alt-s")),
            ("openRecents", None),
            ("unknownAction", Some("ctrl-c")),
        ]
        .into_iter()
        .map(|(id, key)| (id.to_string(), key.map(String::from)))
        .collect();
        let keys: Vec<String> = shortcut_problems(&overrides).into_iter().map(|problem| problem.key).collect();
        assert_eq!(keys, ["shortcuts.copy"], "a key taken from the terminal is allowed");
    }

    #[test]
    fn the_dir_badge_is_shown_only_with_two_or_more_dirs() {
        use app_core::settings::ClaudeDir;
        let dir = |name: &str, path: &str| ClaudeDir {
            name: name.into(),
            path: path.into(),
            command: "claude".into(),
            args: vec![],
            color: None,
        };
        let mut settings = Settings { claude_dirs: vec![dir("Personal", "/c/one")], ..Settings::default() };
        assert_eq!(dir_badge_for(&settings, Some(Path::new("/c/one")), &HostId::local()), None);
        settings.claude_dirs.push(dir("Work", "/c/two"));
        let badge = dir_badge_for(&settings, Some(Path::new("/c/two")), &HostId::local()).unwrap();
        assert_eq!((badge.name.as_str(), badge.letter.as_str()), ("Work", "W"));
        assert_eq!(dir_badge_for(&settings, Some(Path::new("/c/else")), &HostId::local()), None);
        assert_eq!(dir_badge_for(&settings, None, &HostId::local()), None);
        assert_eq!(dir_badge_for(&settings, Some(Path::new("/c/two")), &HostId::named("box")), None);
    }

    // Linux/Windows labels (macOS shows ⌘ symbols).
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn labels_follow_the_overrides() {
        let mut settings = Settings::default();
        assert_eq!(shortcut_label(&settings, "openRecents"), "Ctrl+Shift+R");
        settings.shortcuts.insert("openRecents".into(), Some("ctrl-alt-r".into()));
        assert_eq!(shortcut_label(&settings, "openRecents"), "Ctrl+Alt+R");
        settings.shortcuts.insert("openRecents".into(), None);
        assert_eq!(shortcut_label(&settings, "openRecents"), "Not set");
    }
}
