//! Default on-disk locations, shared by the daemon, the storage layer and
//! the agent hook bridge.

use std::path::PathBuf;

/// Environment override for the data directory (tests, CI, power users).
pub const DATA_DIR_ENV: &str = "TAILFLOW_DATA_DIR";

/// The per-user TailFlow data directory:
///
/// | OS | Path |
/// |---|---|
/// | macOS | `~/Library/Application Support/tailflow` |
/// | Linux | `$XDG_DATA_HOME/tailflow` or `~/.local/share/tailflow` |
/// | Windows | `%LOCALAPPDATA%\tailflow` |
///
/// `$TAILFLOW_DATA_DIR` overrides all of them. Returns `None` only when no
/// home directory can be determined.
pub fn default_data_dir() -> Option<PathBuf> {
    if let Some(dir) = non_empty_env(DATA_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    platform_data_dir().map(|d| d.join("tailflow"))
}

/// `<data dir>/events.db`.
pub fn default_db_path() -> Option<PathBuf> {
    default_data_dir().map(|d| d.join("events.db"))
}

/// `<data dir>/agents`: the agent spool, integration state and the per-user
/// agent capture configuration.
pub fn default_agents_dir() -> Option<PathBuf> {
    default_data_dir().map(|d| d.join("agents"))
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
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

/// Create `dir` (and parents) readable only by the current user on Unix.
///
/// # Errors
/// The directory cannot be created.
pub fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}
