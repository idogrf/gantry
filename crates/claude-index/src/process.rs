//! Starting helper programs from a process without a console (Windows): no console windows, and `.cmd` shims found.

use std::path::PathBuf;

/// Keep `command` from opening a console window of its own. Gantry's window and daemon have no console on Windows, so
/// every console program they start (git, docker, ssh, …) would otherwise flash a new one. Does nothing elsewhere.
pub fn hide_window(command: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// The program to run for `name`. On Windows a bare name is looked up on `PATH` with every `PATHEXT` extension, since
/// `std::process::Command` only tries `.exe` and tools such as VS Code's `code` are `.cmd` scripts. Elsewhere, or when
/// nothing is found, `name` itself.
pub fn resolve_program(name: &str) -> PathBuf {
    find_program(name).unwrap_or_else(|| PathBuf::from(name))
}

/// Where the bare program `name` is on `PATH`, trying every `PATHEXT` extension (Windows). `None` when it is not
/// found, and always on other systems.
pub fn find_program(name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    return find_on_path(name, std::env::var_os("PATH"), std::env::var_os("PATHEXT"));
    #[cfg(not(windows))]
    {
        let _ = name;
        None
    }
}

#[cfg(any(windows, test))]
fn find_on_path(name: &str, path: Option<std::ffi::OsString>, pathext: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if name.contains(['/', '\\']) || std::path::Path::new(name).extension().is_some() {
        return None;
    }
    let pathext = pathext.and_then(|ext| ext.into_string().ok()).unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
    let extensions: Vec<&str> = pathext.split(';').filter(|ext| !ext.is_empty()).collect();
    std::env::split_paths(&path?).find_map(|dir| {
        extensions.iter().map(|ext| dir.join(format!("{name}{ext}"))).find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_cmd_shim_by_pathext() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("code.cmd"), "@echo off").unwrap();
        let path = std::env::join_paths([tmp.path()]).unwrap();
        let found = find_on_path("code", Some(path), Some(".exe;.cmd".into()));
        assert_eq!(found, Some(tmp.path().join("code.cmd")));
    }

    #[test]
    fn earlier_extensions_win_and_paths_are_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("tool.exe"), "").unwrap();
        std::fs::write(tmp.path().join("tool.cmd"), "").unwrap();
        let path = || Some(std::env::join_paths([tmp.path()]).unwrap());
        assert_eq!(find_on_path("tool", path(), Some(".exe;.cmd".into())), Some(tmp.path().join("tool.exe")));
        assert_eq!(find_on_path("tool.cmd", path(), Some(".exe;.cmd".into())), None);
        assert_eq!(find_on_path("bin/tool", path(), Some(".exe;.cmd".into())), None);
        assert_eq!(find_on_path("missing", path(), Some(".exe;.cmd".into())), None);
    }
}
