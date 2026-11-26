//! Where the daemon writes its runtime file, without depending on
//! `tailflow-core` (this crate stays small for the npx binaries).
//!
//! Must agree with `tailflow_core::paths::default_data_dir`; a test in this
//! crate compares the two.

use std::path::PathBuf;

const DATA_DIR_ENV: &str = "TAILFLOW_DATA_DIR";

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// The per-user TailFlow data directory (`$TAILFLOW_DATA_DIR` first).
pub fn default_data_dir() -> Option<PathBuf> {
    if let Some(dir) = non_empty_env(DATA_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    platform_data_dir().map(|d| d.join("tailflow"))
}

/// `<data dir>/run/daemon-<port>.json`.
pub fn runtime_file_for(port: u16) -> Option<PathBuf> {
    default_data_dir().map(|d| d.join("run").join(format!("daemon-{port}.json")))
}

#[cfg(target_os = "macos")]
fn platform_data_dir() -> Option<PathBuf> {
    non_empty_env("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
}

#[cfg(windows)]
fn platform_data_dir() -> Option<PathBuf> {
    non_empty_env("LOCALAPPDATA")
        .or_else(|| non_empty_env("APPDATA"))
        .map(PathBuf::from)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_data_dir() -> Option<PathBuf> {
    non_empty_env("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| non_empty_env("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

#[cfg(not(any(unix, windows)))]
fn platform_data_dir() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn agrees_with_tailflow_core() {
        assert_eq!(
            super::default_data_dir(),
            tailflow_core::paths::default_data_dir()
        );
    }
}
