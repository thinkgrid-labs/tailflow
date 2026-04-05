use crate::ingestion::{
    docker::DockerSource, file::FileSource, process::ProcessSource, stdin::StdinSource, Source,
};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Top-level `tailflow.toml` structure.
#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub sources: SourcesConfig,
}

#[derive(Debug, Deserialize, Default)]
pub struct SourcesConfig {
    /// `docker = true` — tail all running containers
    #[serde(default)]
    pub docker: bool,

    /// `stdin = "label"` — treat piped input as this named source
    pub stdin: Option<String>,

    /// `[[sources.file]]` entries
    #[serde(default)]
    pub file: Vec<FileEntry>,

    /// `[[sources.process]]` entries
    #[serde(default)]
    pub process: Vec<ProcessEntry>,
}

#[derive(Debug, Deserialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub label: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Never restart — default behaviour.
    #[default]
    Never,
    /// Restart after every exit, zero or non-zero.
    Always,
    /// Restart only when the process exits with a non-zero status.
    OnFailure,
}

fn default_restart_delay_ms() -> u64 {
    1_000
}

#[derive(Debug, Deserialize)]
pub struct ProcessEntry {
    pub cmd: String,
    pub label: String,
    /// Restart policy on process exit.  Defaults to `never`.
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Initial restart delay in milliseconds.  Doubles on each attempt, capped at 30 s.
    #[serde(default = "default_restart_delay_ms")]
    pub restart_delay_ms: u64,
}

impl Config {
    /// Load from `tailflow.toml` in the given directory (or any parent).
    pub fn find_and_load(start: &Path) -> Result<Option<Self>> {
        let mut dir = start.to_path_buf();
        loop {
            let candidate = dir.join("tailflow.toml");
            if candidate.exists() {
                return Ok(Some(Self::load(&candidate)?));
            }
            if !dir.pop() {
                return Ok(None);
            }
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid TOML in {}", path.display()))
    }

    /// Resolve config into a list of ingestion sources.
    /// Async because DockerSource::discover() calls the Docker API.
    pub async fn into_sources(self) -> Result<Vec<Box<dyn Source>>> {
        let mut sources: Vec<Box<dyn Source>> = Vec::new();

        if self.sources.docker {
            let containers = DockerSource::discover().await?;
            for c in containers {
                sources.push(Box::new(c));
            }
        }

        for entry in self.sources.file {
            let src = match entry.label {
                Some(label) => FileSource::with_label(entry.path, label),
                None => FileSource::new(entry.path),
            };
            sources.push(Box::new(src));
        }

        for entry in self.sources.process {
            let src = ProcessSource::new(entry.label, entry.cmd)
                .with_restart(entry.restart, entry.restart_delay_ms);
            sources.push(Box::new(src));
        }

        if let Some(label) = self.sources.stdin {
            sources.push(Box::new(StdinSource::new(label)));
        }

        Ok(sources)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Config {
        toml::from_str(toml).expect("valid TOML")
    }

    #[test]
    fn empty_config_has_sensible_defaults() {
        let cfg = parse("");
        assert!(!cfg.sources.docker);
        assert!(cfg.sources.stdin.is_none());
        assert!(cfg.sources.file.is_empty());
        assert!(cfg.sources.process.is_empty());
    }

    #[test]
    fn docker_flag_parsed() {
        let cfg = parse("[sources]\ndocker = true");
        assert!(cfg.sources.docker);
    }

    #[test]
    fn stdin_label_parsed() {
        let cfg = parse("[sources]\nstdin = \"pipe\"");
        assert_eq!(cfg.sources.stdin.as_deref(), Some("pipe"));
    }

    #[test]
    fn process_entries_parsed() {
        let cfg = parse(
            r#"
[[sources.process]]
label = "frontend"
cmd   = "npm run dev"

[[sources.process]]
label = "api"
cmd   = "go run ./cmd/api"
"#,
        );
        assert_eq!(cfg.sources.process.len(), 2);
        assert_eq!(cfg.sources.process[0].label, "frontend");
        assert_eq!(cfg.sources.process[0].cmd, "npm run dev");
        assert_eq!(cfg.sources.process[1].label, "api");
    }

    #[test]
    fn file_entry_with_label_parsed() {
        let cfg = parse(
            r#"
[[sources.file]]
path  = "/var/log/app.log"
label = "app"
"#,
        );
        assert_eq!(cfg.sources.file.len(), 1);
        assert_eq!(cfg.sources.file[0].path, PathBuf::from("/var/log/app.log"));
        assert_eq!(cfg.sources.file[0].label, Some("app".to_string()));
    }

    #[test]
    fn file_entry_without_label_parsed() {
        let cfg = parse("[[sources.file]]\npath = \"/tmp/out.log\"");
        assert!(cfg.sources.file[0].label.is_none());
    }

    #[test]
    fn process_restart_defaults_to_never() {
        let cfg = parse("[[sources.process]]\nlabel = \"api\"\ncmd = \"go run .\"");
        assert_eq!(cfg.sources.process[0].restart, RestartPolicy::Never);
        assert_eq!(cfg.sources.process[0].restart_delay_ms, 1_000);
    }

    #[test]
    fn process_restart_on_failure_parsed() {
        let cfg = parse(
            r#"
[[sources.process]]
label = "api"
cmd   = "go run ."
restart = "on-failure"
restart_delay_ms = 2000
"#,
        );
        assert_eq!(cfg.sources.process[0].restart, RestartPolicy::OnFailure);
        assert_eq!(cfg.sources.process[0].restart_delay_ms, 2_000);
    }

    #[test]
    fn process_restart_always_parsed() {
        let cfg = parse("[[sources.process]]\nlabel = \"w\"\ncmd = \"x\"\nrestart = \"always\"");
        assert_eq!(cfg.sources.process[0].restart, RestartPolicy::Always);
    }

    #[test]
    fn invalid_toml_returns_error() {
        let result: Result<Config, _> = toml::from_str("[[[[invalid toml");
        assert!(result.is_err());
    }

    #[test]
    fn config_load_missing_file_returns_error() {
        let result = Config::load(Path::new("/nonexistent/tailflow.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn find_and_load_returns_none_when_no_file() {
        // /tmp has no tailflow.toml above it
        let result = Config::find_and_load(Path::new("/tmp")).unwrap();
        assert!(result.is_none());
    }
}
