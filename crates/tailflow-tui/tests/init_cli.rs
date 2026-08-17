use std::{fs, process::Command};

fn tailflow() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tailflow"))
}

#[test]
fn init_yes_creates_detected_configuration() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("package.json"),
        r#"{"name":"demo","scripts":{"dev":"vite"}}"#,
    )
    .unwrap();

    let output = tailflow()
        .args(["init", "--dir"])
        .arg(directory.path())
        .arg("--yes")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = fs::read_to_string(directory.path().join("tailflow.toml")).unwrap();
    assert!(config.contains("label = \"demo\""));
    assert!(config.contains("cmd = \"npm run dev\""));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(concat!("TailFlow v", env!("CARGO_PKG_VERSION"))));
    assert!(stdout.contains("claude mcp add"));
}

#[test]
fn init_protects_existing_configuration_and_force_replaces_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tailflow.toml");
    fs::write(&path, "original").unwrap();
    fs::write(directory.path().join("compose.yml"), "services: {}").unwrap();

    let refused = tailflow()
        .args(["init", "--dir"])
        .arg(directory.path())
        .arg("--yes")
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert_eq!(fs::read_to_string(&path).unwrap(), "original");

    let replaced = tailflow()
        .args(["init", "--dir"])
        .arg(directory.path())
        .args(["--yes", "--force"])
        .output()
        .unwrap();
    assert!(replaced.status.success());
    assert!(fs::read_to_string(path).unwrap().contains("docker = true"));
}

#[test]
fn init_without_detected_or_explicit_sources_does_not_write_a_file() {
    let directory = tempfile::tempdir().unwrap();
    let output = tailflow()
        .args(["init", "--dir"])
        .arg(directory.path())
        .arg("--yes")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(!directory.path().join("tailflow.toml").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no supported sources detected"));
}

#[test]
fn init_accepts_explicit_sources_when_detection_finds_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let output = tailflow()
        .args(["init", "--dir"])
        .arg(directory.path())
        .args([
            "--yes",
            "--docker",
            "--process",
            "api=go run ./cmd/api",
            "--file",
            "logs/future.log",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = fs::read_to_string(directory.path().join("tailflow.toml")).unwrap();
    assert!(config.contains("docker = true"));
    assert!(config.contains("label = \"api\""));
    assert!(config.contains("cmd = \"go run ./cmd/api\""));
    assert!(config.contains("path = \"logs/future.log\""));
}
