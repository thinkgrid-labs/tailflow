/// Integration tests for Config::find_and_load — exercises real filesystem I/O.
use std::fs;
use tailflow_core::config::Config;
use tempfile::TempDir;

/// Helper: write a `tailflow.toml` inside a temp dir and return it.
fn write_config(dir: &TempDir, content: &str) -> std::path::PathBuf {
    let path = dir.path().join("tailflow.toml");
    fs::write(&path, content).unwrap();
    path
}

#[test]
fn load_minimal_config_from_path() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, "[sources]\ndocker = true");

    let cfg = Config::load(&dir.path().join("tailflow.toml")).unwrap();
    assert!(cfg.sources.docker);
}

#[test]
fn find_and_load_discovers_config_in_current_dir() {
    let dir = TempDir::new().unwrap();
    write_config(
        &dir,
        r#"
[[sources.process]]
label = "api"
cmd   = "echo hello"
"#,
    );

    let cfg = Config::find_and_load(dir.path()).unwrap().unwrap();
    assert_eq!(cfg.sources.process.len(), 1);
    assert_eq!(cfg.sources.process[0].label, "api");
}

#[test]
fn find_and_load_discovers_config_in_parent_dir() {
    let dir = TempDir::new().unwrap();
    let sub_dir = dir.path().join("packages").join("web");
    fs::create_dir_all(&sub_dir).unwrap();
    write_config(&dir, "[sources]\ndocker = true");

    // Start the search from the sub-directory — should walk up and find the file
    let cfg = Config::find_and_load(&sub_dir).unwrap().unwrap();
    assert!(cfg.sources.docker);
}

#[test]
fn find_and_load_returns_none_when_absent() {
    let dir = TempDir::new().unwrap();
    // No tailflow.toml written
    let result = Config::find_and_load(dir.path()).unwrap();
    assert!(result.is_none());
}

#[test]
fn load_returns_error_for_malformed_toml() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tailflow.toml");
    fs::write(&path, "[[[[bad toml").unwrap();

    assert!(Config::load(&path).is_err());
}
