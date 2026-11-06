//! The shell a capture or recording runs in.
//!
//! The capture protocol is POSIX (`PS1`, `printf` markers), so every platform
//! needs a POSIX shell. Unix keeps the old chain (`$SHELL`, else `/bin/bash`).
//! Windows has neither, so the default searches `PATH` for `bash.exe` and
//! then the canonical Git for Windows spots; when nothing is found it fails
//! with the fix instead of a raw spawn error. An explicit `--shell` or
//! `$SHELL` always wins verbatim on every platform.

#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

#[cfg(windows)]
use crate::error::Error;
use crate::error::Result;

/// Canonical Git for Windows shells, in preference order.
#[cfg(windows)]
const GIT_BASH_CANDIDATES: &[&str] = &[
    "C:\\Program Files\\Git\\bin\\bash.exe",
    "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
];

/// The shell to spawn when the caller did not pick one.
pub fn default_shell() -> Result<String> {
    if let Ok(shell) = std::env::var("SHELL") {
        return Ok(shell);
    }
    #[cfg(windows)]
    {
        windows_default()
    }
    #[cfg(not(windows))]
    {
        Ok("/bin/bash".to_string())
    }
}

/// First `bash.exe` on `PATH`, else the first installed Git Bash.
#[cfg(windows)]
fn windows_default() -> Result<String> {
    if let Some(found) = find_on_path("bash.exe") {
        return Ok(found);
    }
    let candidates: Vec<PathBuf> = GIT_BASH_CANDIDATES.iter().map(PathBuf::from).collect();
    if let Some(found) = find_git_bash(&candidates) {
        return Ok(found);
    }
    Err(Error::Export(
        "no POSIX shell found: install Git for Windows, or pass --shell <path-to-bash> (recordings: set SHELL)"
            .to_string(),
    ))
}

/// First candidate that exists as a file, as a string.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
fn find_git_bash(candidates: &[PathBuf]) -> Option<String> {
    candidates
        .iter()
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
}

/// A `name` found in the process `PATH`, as a string.
#[cfg(windows)]
fn find_on_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
}

/// Join `dir` and `name` for tests on every platform.
#[cfg(test)]
fn join_name(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn explicit_shell_wins_verbatim() {
        let _guard = ENV_LOCK.lock().unwrap();
        let key = "DEMO_TEST_SHELL_WIN";
        let before = std::env::var("SHELL").ok();
        unsafe { std::env::set_var("SHELL", key) };
        let shell = default_shell().expect("explicit SHELL must win");
        match before {
            Some(value) => unsafe { std::env::set_var("SHELL", value) },
            None => unsafe { std::env::remove_var("SHELL") },
        }
        assert_eq!(shell, key);
    }

    #[test]
    fn finds_bash_in_candidate_dirs() {
        let dir = std::env::temp_dir().join(format!("demo-shell-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bash = join_name(&dir, "bash.exe");
        std::fs::write(&bash, "fake").unwrap();
        let missing = join_name(&dir, "missing.exe");
        assert_eq!(
            find_git_bash(&[missing.clone(), bash.clone()]),
            Some(bash.to_string_lossy().into_owned())
        );
        assert_eq!(find_git_bash(&[missing]), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(not(windows))]
    fn unix_default_is_bash() {
        let _guard = ENV_LOCK.lock().unwrap();
        let before = std::env::var("SHELL").ok();
        unsafe { std::env::remove_var("SHELL") };
        let shell = default_shell().expect("unix always has a default");
        if let Some(value) = before {
            unsafe { std::env::set_var("SHELL", value) }
        }
        assert_eq!(shell, "/bin/bash");
    }
}
