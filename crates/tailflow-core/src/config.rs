use crate::ingestion::{
    docker::DockerAllSource, file::FileSource, process::ProcessSource, stdin::StdinSource, Source,
};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Top-level `tailflow.toml` structure.
///
/// Unknown top-level keys are tolerated (older binaries must keep reading
/// newer files). The `[storage]` and `[privacy]` sections are strict: a typo
/// such as `retention_day` is an error, not a silently ignored setting.
#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub sources: SourcesConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub privacy: crate::privacy::PrivacyConfig,
}

// ── [storage] ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackend {
    #[default]
    Sqlite,
}

/// `retention_days = 30` or `retention_days = "unlimited"`. Unlimited age
/// retention never means unlimited disk: `max_size_mb` always applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    Days(u32),
    Unlimited,
}

impl Retention {
    pub fn days(self) -> Option<u32> {
        match self {
            Retention::Days(d) => Some(d),
            Retention::Unlimited => None,
        }
    }
}

impl<'de> Deserialize<'de> for Retention {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Days(i64),
            Word(String),
        }
        match Raw::deserialize(d)? {
            Raw::Days(n) if (1..=36_500).contains(&n) => Ok(Retention::Days(
                u32::try_from(n).map_err(serde::de::Error::custom)?,
            )),
            Raw::Days(n) => Err(serde::de::Error::custom(format!(
                "retention_days must be 1..=36500 or \"unlimited\", got {n}"
            ))),
            Raw::Word(w) if w == "unlimited" => Ok(Retention::Unlimited),
            Raw::Word(w) => Err(serde::de::Error::custom(format!(
                "retention_days must be a number of days or \"unlimited\", got {w:?}"
            ))),
        }
    }
}

/// SQLite `synchronous` level for the WAL. See STORAGE durability docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    /// Committed transactions survive a process crash; the last few may be
    /// lost on OS crash or power failure.
    #[default]
    Normal,
    /// Every commit is fsynced. Survives power loss; slower writes.
    Full,
}

/// `[storage]` section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StorageConfig {
    pub enabled: bool,
    pub backend: StorageBackend,
    /// Database file. Default: `<user data dir>/tailflow/events.db`.
    pub path: Option<PathBuf>,
    pub retention_days: Retention,
    /// Hard cap on live database pages. Oldest events are evicted first.
    pub max_size_mb: u64,
    /// Shorter retention for optional payload bodies.
    pub payload_retention_days: u32,
    pub cleanup_interval_secs: u64,
    pub full_text_search: bool,
    pub synchronous: SyncMode,
    /// Bounded hand-off queue between ingestion and the writer.
    pub queue_capacity: usize,
    pub batch_max_events: usize,
    pub batch_max_delay_ms: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: StorageBackend::Sqlite,
            path: None,
            retention_days: Retention::Days(30),
            max_size_mb: 2048,
            payload_retention_days: 7,
            cleanup_interval_secs: 300,
            full_text_search: true,
            synchronous: SyncMode::Normal,
            queue_capacity: 16_384,
            batch_max_events: 512,
            batch_max_delay_ms: 100,
        }
    }
}

impl StorageConfig {
    /// Range-check every numeric setting.
    ///
    /// # Errors
    /// The first out-of-range field, by name.
    pub fn validate(&self) -> Result<()> {
        fn check(ok: bool, msg: &str) -> Result<()> {
            if ok {
                Ok(())
            } else {
                Err(anyhow::anyhow!("[storage] {msg}"))
            }
        }
        check(
            (16..=1_048_576).contains(&self.max_size_mb),
            "max_size_mb must be 16..=1048576",
        )?;
        check(
            (1..=36_500).contains(&self.payload_retention_days),
            "payload_retention_days must be 1..=36500",
        )?;
        check(
            (10..=86_400).contains(&self.cleanup_interval_secs),
            "cleanup_interval_secs must be 10..=86400",
        )?;
        check(
            (256..=1_000_000).contains(&self.queue_capacity),
            "queue_capacity must be 256..=1000000",
        )?;
        check(
            (1..=10_000).contains(&self.batch_max_events),
            "batch_max_events must be 1..=10000",
        )?;
        check(
            (1..=10_000).contains(&self.batch_max_delay_ms),
            "batch_max_delay_ms must be 1..=10000",
        )?;
        if let Some(path) = &self.path {
            check(!path.as_os_str().is_empty(), "path must not be empty")?;
        }
        Ok(())
    }
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
            sources.push(Box::new(DockerAllSource::new()));
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
    fn storage_and_privacy_default_when_absent() {
        let cfg = parse("[sources]\ndocker = true");
        assert!(cfg.storage.enabled);
        assert_eq!(cfg.storage.retention_days, Retention::Days(30));
        assert_eq!(cfg.storage.max_size_mb, 2048);
        assert_eq!(cfg.privacy.capture, crate::privacy::CaptureMode::Metadata);
        cfg.storage.validate().unwrap();
    }

    #[test]
    fn storage_section_parses_documented_contract() {
        let cfg = parse(
            r#"
[storage]
enabled = true
backend = "sqlite"
retention_days = 90
max_size_mb = 512

[privacy]
capture = "redacted"
excluded_event_types = ["git.*"]
"#,
        );
        assert_eq!(cfg.storage.retention_days, Retention::Days(90));
        assert_eq!(cfg.storage.max_size_mb, 512);
        assert_eq!(cfg.privacy.capture, crate::privacy::CaptureMode::Redacted);
    }

    #[test]
    fn unlimited_retention_keeps_the_size_cap() {
        let cfg = parse("[storage]\nretention_days = \"unlimited\"");
        assert_eq!(cfg.storage.retention_days, Retention::Unlimited);
        assert_eq!(cfg.storage.max_size_mb, 2048, "size cap still applies");
    }

    #[test]
    fn storage_typos_and_bad_values_are_errors() {
        for bad in [
            "[storage]\nretention_day = 30",
            "[storage]\nretention_days = 0",
            "[storage]\nretention_days = \"forever\"",
            "[storage]\nbackend = \"postgres\"",
            "[privacy]\ncapture = \"everything\"",
            "[privacy]\nunknown = 1",
        ] {
            assert!(toml::from_str::<Config>(bad).is_err(), "accepted: {bad}");
        }
        let cfg = parse("[storage]\nmax_size_mb = 1");
        assert!(cfg.storage.validate().is_err());
    }

    #[test]
    fn shipped_example_config_is_valid() {
        let cfg = parse(include_str!("../../../tailflow.example.toml"));
        cfg.storage.validate().unwrap();
        crate::privacy::CapturePolicy::from_config(&cfg.privacy).unwrap();
        assert_eq!(cfg.storage.retention_days, Retention::Days(30));
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
