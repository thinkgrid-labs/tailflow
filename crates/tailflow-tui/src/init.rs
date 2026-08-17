use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::Value;
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

const COMPOSE_FILES: &[&str] = &[
    "compose.yml",
    "compose.yaml",
    "docker-compose.yml",
    "docker-compose.yaml",
];
const LOG_DIRS: &[&str] = &["logs", "log", "var/log"];
const MAX_LOG_FILES: usize = 20;

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Project directory to inspect
    #[arg(long, value_name = "PATH", default_value = ".")]
    dir: PathBuf,

    /// Accept recommended detected sources without prompting
    #[arg(short = 'y', long)]
    yes: bool,

    /// Replace an existing tailflow.toml
    #[arg(short = 'f', long)]
    force: bool,

    /// Include local Docker container discovery
    #[arg(long)]
    docker: bool,

    /// Add a process as LABEL=COMMAND (repeatable)
    #[arg(long, value_name = "LABEL=COMMAND")]
    process: Vec<String>,

    /// Add a log file (repeatable)
    #[arg(long, value_name = "PATH")]
    file: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceKind {
    Docker {
        discovered_from: Option<PathBuf>,
    },
    Process {
        label: String,
        command: String,
        discovered_from: Option<String>,
    },
    File {
        path: PathBuf,
        label: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    source: SourceKind,
    recommended: bool,
}

impl Candidate {
    fn description(&self) -> String {
        match &self.source {
            SourceKind::Docker { discovered_from } => discovered_from.as_ref().map_or_else(
                || "Docker containers (requested by --docker)".to_string(),
                |path| format!("Docker containers ({})", path.display()),
            ),
            SourceKind::Process {
                label,
                command,
                discovered_from,
            } => discovered_from.as_ref().map_or_else(
                || format!("process {label}: {command}"),
                |origin| format!("process {label}: {command} ({origin})"),
            ),
            SourceKind::File { path, label } => {
                format!("file {label}: {}", path.display())
            }
        }
    }
}

pub fn run(args: InitArgs) -> Result<()> {
    let root = args
        .dir
        .canonicalize()
        .with_context(|| format!("cannot open project directory {}", args.dir.display()))?;
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }

    println!("TailFlow v{}\n", env!("CARGO_PKG_VERSION"));

    let config_path = root.join("tailflow.toml");
    if config_path.exists() && !args.force {
        bail!(
            "{} already exists; inspect it or rerun with --force to replace it",
            config_path.display()
        );
    }

    let mut candidates = discover(&root)?;
    add_explicit_sources(&root, &args, &mut candidates)?;

    if candidates.is_empty() {
        bail!(
            "no supported sources detected in {}\n\
             Add a package.json development script, a Compose file, or logs/*.log.\n\
             You can also pass --docker, --process LABEL=COMMAND, or --file PATH.",
            root.display()
        );
    }

    let selected = if args.yes {
        recommended_indices(&candidates)
    } else {
        if !io::stdin().is_terminal() {
            bail!("stdin is not interactive; rerun with --yes to accept recommended sources");
        }
        prompt_for_selection(&candidates)?
    };

    if selected.is_empty() {
        bail!("no sources selected; tailflow.toml was not written");
    }

    let config = render_config(&candidates, &selected);
    // Validate our output against the same parser used at runtime before the
    // filesystem is changed.
    toml::from_str::<tailflow_core::config::Config>(&config)
        .context("generated configuration did not validate")?;
    write_config(&config_path, config.as_bytes(), args.force)?;

    println!("\nCreated {}", config_path.display());
    println!("Enabled:");
    for index in &selected {
        println!("  • {}", candidates[*index].description());
    }
    println!("\nNext:");
    if std::env::current_dir().ok().as_deref() != Some(root.as_path()) {
        println!("  cd {}", root.display());
    }
    println!("  tailflow-daemon");
    println!("  claude mcp add tailflow -- tailflow-mcp");

    Ok(())
}

fn discover(root: &Path) -> Result<Vec<Candidate>> {
    let mut candidates = discover_package_scripts(root)?;

    if let Some(path) = COMPOSE_FILES
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file())
    {
        let relative = relative_to(root, &path);
        candidates.push(Candidate {
            source: SourceKind::Docker {
                discovered_from: Some(relative),
            },
            recommended: true,
        });
    }

    for path in discover_log_files(root)? {
        let label = label_from_path(&path);
        candidates.push(Candidate {
            source: SourceKind::File { path, label },
            recommended: true,
        });
    }

    // `--yes` must always produce a useful config when anything was found.
    // A package script that delegates to Compose is normally optional because
    // the Docker source is better, but the compose file may live elsewhere.
    if !candidates.is_empty() && !candidates.iter().any(|candidate| candidate.recommended) {
        candidates[0].recommended = true;
    }

    Ok(candidates)
}

fn discover_package_scripts(root: &Path) -> Result<Vec<Candidate>> {
    let package_path = root.join("package.json");
    if !package_path.is_file() {
        return Ok(Vec::new());
    }

    let package_text = fs::read_to_string(&package_path)
        .with_context(|| format!("cannot read {}", package_path.display()))?;
    let package: Value = serde_json::from_str(&package_text)
        .with_context(|| format!("invalid JSON in {}", package_path.display()))?;
    let Some(scripts) = package.get("scripts").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };

    let package_label = package
        .get("name")
        .and_then(Value::as_str)
        .map(normalize_label)
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| "app".to_string());
    let manager = package_manager(root, package.get("packageManager").and_then(Value::as_str));

    let mut names: Vec<&str> = scripts
        .keys()
        .map(String::as_str)
        .filter(|name| is_development_script(name))
        .collect();
    names.sort_by_key(|name| script_priority(name));

    let has_dev = names.contains(&"dev");
    let has_dev_family = names.iter().any(|name| name.starts_with("dev:"));
    let mut labels = HashSet::new();
    let mut candidates = Vec::new();

    for name in names {
        let script_body = scripts
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut label = script_label(name, &package_label);
        label = unique_label(label, &mut labels);
        let command = package_command(manager, name);
        let delegates_to_compose =
            script_body.contains("docker compose") || script_body.contains("docker-compose");
        let recommended = if name == "dev" {
            !delegates_to_compose
        } else if name.starts_with("dev:") {
            !has_dev
        } else {
            !has_dev && !has_dev_family && candidates.is_empty()
        };

        candidates.push(Candidate {
            source: SourceKind::Process {
                label,
                command,
                discovered_from: Some(format!("package.json script {name}")),
            },
            recommended,
        });
    }

    Ok(candidates)
}

fn add_explicit_sources(
    root: &Path,
    args: &InitArgs,
    candidates: &mut Vec<Candidate>,
) -> Result<()> {
    if args.docker
        && !candidates
            .iter()
            .any(|candidate| matches!(candidate.source, SourceKind::Docker { .. }))
    {
        candidates.push(Candidate {
            source: SourceKind::Docker {
                discovered_from: None,
            },
            recommended: true,
        });
    }

    let mut labels: HashSet<String> = candidates
        .iter()
        .filter_map(|candidate| match &candidate.source {
            SourceKind::Process { label, .. } | SourceKind::File { label, .. } => {
                Some(label.clone())
            }
            SourceKind::Docker { .. } => None,
        })
        .collect();

    for value in &args.process {
        let Some((raw_label, command)) = value.split_once('=') else {
            bail!("invalid --process {value:?}; expected LABEL=COMMAND");
        };
        let label = normalize_label(raw_label);
        if label.is_empty() || command.trim().is_empty() {
            bail!("invalid --process {value:?}; label and command must not be empty");
        }
        let label = unique_label(label, &mut labels);
        candidates.push(Candidate {
            source: SourceKind::Process {
                label,
                command: command.trim().to_string(),
                discovered_from: None,
            },
            recommended: true,
        });
    }

    for supplied in &args.file {
        let absolute = if supplied.is_absolute() {
            supplied.clone()
        } else {
            root.join(supplied)
        };
        let path = relative_to(root, &absolute);
        let label = unique_label(label_from_path(&path), &mut labels);
        candidates.push(Candidate {
            source: SourceKind::File { path, label },
            recommended: true,
        });
    }

    Ok(())
}

fn package_manager(root: &Path, declared: Option<&str>) -> &'static str {
    if declared.is_some_and(|value| value.starts_with("pnpm@")) {
        "pnpm"
    } else if declared.is_some_and(|value| value.starts_with("yarn@")) {
        "yarn"
    } else if declared.is_some_and(|value| value.starts_with("bun@")) {
        "bun"
    } else if declared.is_some_and(|value| value.starts_with("npm@")) {
        "npm"
    } else if root.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if root.join("yarn.lock").is_file() {
        "yarn"
    } else if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
        "bun"
    } else {
        "npm"
    }
}

fn package_command(manager: &str, script: &str) -> String {
    match manager {
        "yarn" => format!("yarn run {script}"),
        "pnpm" => format!("pnpm run {script}"),
        "bun" => format!("bun run {script}"),
        _ => format!("npm run {script}"),
    }
}

fn is_development_script(name: &str) -> bool {
    matches!(name, "dev" | "develop" | "serve" | "start")
        || name.starts_with("dev:")
        || name.starts_with("serve:")
        || name.starts_with("start:")
}

fn script_priority(name: &str) -> (u8, &str) {
    let rank = if name == "dev" {
        0
    } else if name.starts_with("dev:") {
        1
    } else if name == "develop" {
        2
    } else if name == "serve" {
        3
    } else if name.starts_with("serve:") {
        4
    } else if name == "start" {
        5
    } else {
        6
    };
    (rank, name)
}

fn script_label(script: &str, package_label: &str) -> String {
    script
        .split_once(':')
        .map(|(_, suffix)| normalize_label(suffix))
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| package_label.to_string())
}

fn normalize_label(value: &str) -> String {
    let leaf = value
        .trim()
        .trim_start_matches('@')
        .rsplit('/')
        .next()
        .unwrap_or(value);
    let mut normalized = String::new();
    let mut previous_dash = false;
    for ch in leaf.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            normalized.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            normalized.push('-');
            previous_dash = true;
        }
    }
    normalized.trim_matches('-').to_string()
}

fn unique_label(mut label: String, used: &mut HashSet<String>) -> String {
    if used.insert(label.clone()) {
        return label;
    }
    let base = label.clone();
    let mut suffix = 2;
    loop {
        label = format!("{base}-{suffix}");
        if used.insert(label.clone()) {
            return label;
        }
        suffix += 1;
    }
}

fn discover_log_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for relative in LOG_DIRS {
        let directory = root.join(relative);
        if directory.is_dir() {
            collect_log_files(root, &directory, 2, &mut files)?;
        }
    }
    files.sort();
    files.dedup();
    files.truncate(MAX_LOG_FILES);
    Ok(files)
}

fn collect_log_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    output: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("cannot inspect {}", directory.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_file()
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("log"))
        {
            output.push(relative_to(root, &path));
        } else if file_type.is_dir() && depth > 0 {
            collect_log_files(root, &path, depth - 1, output)?;
        }
    }
    Ok(())
}

fn label_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(normalize_label)
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| "log".to_string())
}

fn relative_to(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

fn recommended_indices(candidates: &[Candidate]) -> Vec<usize> {
    candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| candidate.recommended.then_some(index))
        .collect()
}

fn prompt_for_selection(candidates: &[Candidate]) -> Result<Vec<usize>> {
    println!("TailFlow found:\n");
    for (index, candidate) in candidates.iter().enumerate() {
        let marker = if candidate.recommended {
            "recommended"
        } else {
            "optional"
        };
        println!("  {}. {} [{}]", index + 1, candidate.description(), marker);
    }

    let defaults = recommended_indices(candidates)
        .into_iter()
        .map(|index| (index + 1).to_string())
        .collect::<Vec<_>>()
        .join(",");
    loop {
        print!("\nSelect sources (Enter for {defaults}, numbers, 'all', or 'none'): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        match parse_selection(&input, candidates.len(), &defaults) {
            Ok(selection) => return Ok(selection),
            Err(error) => eprintln!("Invalid selection: {error}"),
        }
    }
}

fn parse_selection(input: &str, count: usize, defaults: &str) -> Result<Vec<usize>> {
    let input = input.trim();
    let input = if input.is_empty() { defaults } else { input };
    if input.eq_ignore_ascii_case("all") {
        return Ok((0..count).collect());
    }
    if input.eq_ignore_ascii_case("none") || input.is_empty() {
        return Ok(Vec::new());
    }

    let mut selection = Vec::new();
    for token in input.split(|ch: char| ch == ',' || ch.is_ascii_whitespace()) {
        if token.is_empty() {
            continue;
        }
        let number: usize = token
            .parse()
            .with_context(|| format!("{token:?} is not a source number"))?;
        if number == 0 || number > count {
            bail!("source {number} is outside 1..={count}");
        }
        let index = number - 1;
        if !selection.contains(&index) {
            selection.push(index);
        }
    }
    selection.sort_unstable();
    Ok(selection)
}

fn render_config(candidates: &[Candidate], selected: &[usize]) -> String {
    let sources: Vec<&SourceKind> = selected
        .iter()
        .map(|index| &candidates[*index].source)
        .collect();
    let docker = sources
        .iter()
        .any(|source| matches!(source, SourceKind::Docker { .. }));
    let mut output = format!(
        "# Generated by `tailflow init`. Edit this file as your stack changes.\n\n\
         [sources]\n\
         docker = {docker}\n"
    );

    for source in &sources {
        if let SourceKind::Process { label, command, .. } = source {
            output.push_str("\n[[sources.process]]\n");
            output.push_str(&format!("label = {}\n", toml_string(label)));
            output.push_str(&format!("cmd = {}\n", toml_string(command)));
        }
    }
    for source in &sources {
        if let SourceKind::File { path, label } = source {
            output.push_str("\n[[sources.file]]\n");
            output.push_str(&format!(
                "path = {}\n",
                toml_string(&path.to_string_lossy())
            ));
            output.push_str(&format!("label = {}\n", toml_string(label)));
        }
    }
    output
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

fn write_config(path: &Path, contents: &[u8], force: bool) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_preferred_package_script_and_package_manager() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("package.json"),
            r#"{"name":"@scope/web-app","packageManager":"pnpm@9.0.0","scripts":{"test":"vitest","dev":"vite","dev:api":"node api.js"}}"#,
        )
        .unwrap();

        let candidates = discover_package_scripts(directory.path()).unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0].recommended);
        assert!(!candidates[1].recommended);
        assert!(matches!(
            &candidates[0].source,
            SourceKind::Process { label, command, .. }
                if label == "web-app" && command == "pnpm run dev"
        ));
        assert!(matches!(
            &candidates[1].source,
            SourceKind::Process { label, command, .. }
                if label == "api" && command == "pnpm run dev:api"
        ));
    }

    #[test]
    fn discovers_compose_and_nested_log_files() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("compose.yml"), "services: {}").unwrap();
        fs::create_dir_all(directory.path().join("logs/api")).unwrap();
        fs::write(directory.path().join("logs/api/server.log"), "ready\n").unwrap();
        fs::write(directory.path().join("logs/ignore.txt"), "ignore\n").unwrap();

        let candidates = discover(directory.path()).unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(matches!(candidates[0].source, SourceKind::Docker { .. }));
        assert!(matches!(
            &candidates[1].source,
            SourceKind::File { path, label }
                if path == Path::new("logs/api/server.log") && label == "server"
        ));
    }

    #[test]
    fn compose_delegating_script_falls_back_when_no_compose_file_is_found() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("package.json"),
            r#"{"name":"stack","scripts":{"dev":"docker compose -f infra/compose.yml up"}}"#,
        )
        .unwrap();

        let candidates = discover(directory.path()).unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].recommended);
    }

    #[test]
    fn renders_a_runtime_valid_configuration() {
        let candidates = vec![
            Candidate {
                source: SourceKind::Docker {
                    discovered_from: Some(PathBuf::from("compose.yml")),
                },
                recommended: true,
            },
            Candidate {
                source: SourceKind::Process {
                    label: "web".into(),
                    command: "node -e \"console.log('ready')\"".into(),
                    discovered_from: None,
                },
                recommended: true,
            },
            Candidate {
                source: SourceKind::File {
                    path: PathBuf::from("logs/app.log"),
                    label: "app".into(),
                },
                recommended: true,
            },
        ];
        let output = render_config(&candidates, &[0, 1, 2]);
        let config: tailflow_core::config::Config = toml::from_str(&output).unwrap();

        assert!(config.sources.docker);
        assert_eq!(config.sources.process.len(), 1);
        assert_eq!(
            config.sources.process[0].cmd,
            "node -e \"console.log('ready')\""
        );
        assert_eq!(config.sources.file.len(), 1);
        assert!(output.contains("# Generated by `tailflow init`"));
    }

    #[test]
    fn parses_interactive_selection() {
        assert_eq!(parse_selection("", 3, "1,3").unwrap(), vec![0, 2]);
        assert_eq!(parse_selection("all", 3, "1").unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_selection("3, 1, 3", 3, "2").unwrap(), vec![0, 2]);
        assert!(parse_selection("4", 3, "1").is_err());
    }

    #[test]
    fn refuses_to_replace_a_file_without_force() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tailflow.toml");
        fs::write(&path, "original").unwrap();

        assert!(write_config(&path, b"replacement", false).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        write_config(&path, b"replacement", true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
    }
}
