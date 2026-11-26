//! What a shell command was for: build, test, lint, typecheck, format,
//! dependency management, or something else (M4-E).
//!
//! Privacy first. In the default `metadata` capture mode TailFlow records a
//! command's program, never its arguments. To classify `cargo test` versus
//! `cargo build` it additionally records an *intent*: the effective program
//! (skipping `cd …&&`, `env`, `timeout 60`, `time`, assignments), its
//! subcommand **only if it is in that tool's fixed vocabulary**, and a
//! package-script name **only if it matches** [`SCRIPT_PATTERN`]. Nothing
//! else from the argument list is kept: an unknown word becomes "not
//! recorded", never the word itself.
//!
//! Classification is a table of explainable rules over that intent. It
//! says what the command was *for*, not whether it succeeded.

use std::sync::OnceLock;

/// What a command was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Build,
    Test,
    Lint,
    Typecheck,
    Format,
    Deps,
    /// Recognized, but none of the above (`cargo run`, `npm start`).
    Other,
    /// A multi-command tool whose subcommand was not recorded (older
    /// events, an unrecognized subcommand), or no program at all.
    Unknown,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Build => "build",
            Category::Test => "test",
            Category::Lint => "lint",
            Category::Typecheck => "typecheck",
            Category::Format => "format",
            Category::Deps => "deps",
            Category::Other => "other",
            Category::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "build" => Category::Build,
            "test" => Category::Test,
            "lint" => Category::Lint,
            "typecheck" => Category::Typecheck,
            "format" => Category::Format,
            "deps" => Category::Deps,
            "other" => Category::Other,
            "unknown" => Category::Unknown,
            _ => return None,
        })
    }
}

/// The recordable intent of a command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Intent {
    /// The program that does the work (after `cd x &&`, `env`, `timeout N`).
    pub program: Option<String>,
    /// A subcommand from the program's vocabulary (`test`, `clippy`, `-m
    /// pytest` → `pytest`, `npx vitest` → `vitest`).
    pub subcommand: Option<String>,
    /// A package script name matching [`SCRIPT_PATTERN`] (`test:unit`).
    pub script: Option<String>,
}

/// Script names recorded verbatim: a known verb plus an optional short
/// suffix (`test`, `test:e2e`, `build-prod`, `lint:fix`).
pub const SCRIPT_PATTERN: &str = r"^(test|tests|build|lint|typecheck|type-check|types|check|format|fmt|e2e|ci|verify|compile|dev|start)([:-][a-z0-9-]{1,24})?$";

fn script_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(SCRIPT_PATTERN).expect("the script pattern is a valid regex")
    })
}

/// Words that precede the real program.
const WRAPPERS: &[&str] = &[
    "time", "nice", "nohup", "sudo", "command", "exec", "env", "xargs",
];
/// Commands that only set up the shell (skipped in compound commands).
const SETUP: &[&str] = &[
    "cd", "pushd", "popd", "export", "set", "source", ".", "unset", "true", "echo", "clear",
    "mkdir",
];

/// The fixed vocabulary of subcommands recorded per program.
fn vocabulary(program: &str) -> Option<&'static [&'static str]> {
    Some(match program {
        "cargo" => &[
            "test", "nextest", "build", "b", "check", "c", "clippy", "fmt", "add", "install",
            "update", "fetch", "run", "r", "bench", "doc", "publish", "remove", "rm", "tree",
            "audit", "deny", "miri",
        ],
        "npm" | "pnpm" | "yarn" | "bun" => &[
            "test",
            "t",
            "run",
            "run-script",
            "install",
            "i",
            "ci",
            "add",
            "exec",
            "x",
            "dlx",
            "build",
            "lint",
            "dev",
            "start",
            "update",
            "upgrade",
            "up",
            "remove",
            "rm",
            "uninstall",
            "audit",
            "outdated",
            "publish",
        ],
        "go" => &[
            "test", "build", "vet", "fmt", "mod", "get", "run", "install", "generate", "tool",
        ],
        "make" | "just" | "task" => &[
            "test",
            "tests",
            "build",
            "all",
            "lint",
            "check",
            "fmt",
            "format",
            "typecheck",
            "install",
            "clean",
            "ci",
        ],
        "gradle" | "gradlew" | "mvn" | "mvnw" => &[
            "test", "build", "assemble", "compile", "check", "lint", "package", "install",
            "verify", "clean",
        ],
        "dotnet" => &["test", "build", "format", "restore", "run", "publish"],
        "swift" => &["test", "build", "run", "format"],
        "deno" => &[
            "test", "lint", "fmt", "check", "task", "run", "compile", "install",
        ],
        "uv" | "poetry" | "pdm" => &[
            "run", "pip", "sync", "add", "install", "lock", "remove", "build",
        ],
        "pip" | "pip3" => &["install", "uninstall", "download"],
        "ruff" => &["check", "format"],
        "biome" => &["check", "lint", "format", "ci"],
        "bundle" => &["install", "update", "exec"],
        _ => return None,
    })
}

/// Tools run through `npx`/`bunx`/`pnpm exec`, `python -m`, `uv run`.
const RUNNABLE_TOOLS: &[&str] = &[
    "vitest",
    "jest",
    "mocha",
    "ava",
    "playwright",
    "cypress",
    "tsc",
    "vue-tsc",
    "eslint",
    "prettier",
    "biome",
    "next",
    "vite",
    "webpack",
    "rollup",
    "esbuild",
    "tsx",
    "pytest",
    "unittest",
    "mypy",
    "pyright",
    "black",
    "isort",
    "ruff",
    "flake8",
    "pylint",
    "pip",
    "build",
    "tox",
    "nox",
    "stylelint",
    "turbo",
    "nx",
];

fn word(w: &str) -> &str {
    let w = w.trim_matches(|c| c == '"' || c == '\'');
    w.rsplit(['/', '\\']).next().unwrap_or(w)
}

fn is_env_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(n, _)| {
        !n.is_empty()
            && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !n.starts_with(|c: char| c.is_ascii_digit())
    })
}

fn plain(w: &str) -> bool {
    !w.is_empty()
        && w.len() <= 64
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// Intent of one simple command (no `&&`).
fn simple(words: &[&str]) -> Intent {
    let mut i = 0;
    // Wrappers, assignments, and `timeout <secs>`.
    while let Some(w) = words.get(i) {
        let w = word(w);
        if is_env_assignment(w) || WRAPPERS.contains(&w) {
            i += 1;
        } else if w == "timeout" {
            i += 1;
            while words.get(i).is_some_and(|a| a.starts_with('-')) {
                i += 1;
            }
            i += 1; // the duration
        } else {
            break;
        }
    }
    let Some(program) = words.get(i).map(|w| word(w)).filter(|w| plain(w)) else {
        return Intent::default();
    };
    let program = program.to_string();
    let args: Vec<&str> = words.iter().skip(i + 1).map(|w| word(w)).collect();
    let mut out = Intent {
        program: Some(program.clone()),
        ..Intent::default()
    };
    let first_word = |skip_flags: bool| -> Option<&str> {
        args.iter()
            .copied()
            .find(|a| !(skip_flags && (a.starts_with('-') || a.starts_with('+'))))
    };
    match program.as_str() {
        "npx" | "bunx" | "pnpx" => {
            out.subcommand = first_word(true)
                .filter(|t| RUNNABLE_TOOLS.contains(t))
                .map(str::to_string);
        }
        "python" | "python3" | "py" => {
            if let Some(pos) = args.iter().position(|a| *a == "-m") {
                out.subcommand = args
                    .get(pos + 1)
                    .filter(|t| RUNNABLE_TOOLS.contains(t))
                    .map(|t| (*t).to_string());
            }
        }
        _ => {
            if let Some(vocab) = vocabulary(&program) {
                let sub = first_word(true);
                if let Some(s) = sub.filter(|s| vocab.contains(s)) {
                    out.subcommand = Some(s.to_string());
                    // `npm run test`, `pnpm exec vitest`, `uv run pytest`,
                    // `deno task build`, `bundle exec rspec`.
                    let rest: Vec<&str> = args
                        .iter()
                        .copied()
                        .skip_while(|a| *a != s)
                        .skip(1)
                        .filter(|a| !a.starts_with('-'))
                        .collect();
                    if let Some(next) = rest.first() {
                        if matches!(s, "run" | "run-script" | "task") && script_re().is_match(next)
                        {
                            out.script = Some((*next).to_string());
                        } else if matches!(s, "exec" | "x" | "dlx" | "run")
                            && RUNNABLE_TOOLS.contains(next)
                        {
                            out.subcommand = Some((*next).to_string());
                        }
                    }
                } else if matches!(
                    program.as_str(),
                    "yarn" | "pnpm" | "bun" | "make" | "just" | "task"
                ) {
                    // `yarn test:unit`, `make lint`: the script is the word.
                    if let Some(s) = sub.filter(|s| script_re().is_match(s)) {
                        out.script = Some(s.to_string());
                    }
                }
            }
        }
    }
    out
}

/// The recordable intent of a (possibly compound) command line: the first
/// segment that does real work.
pub fn intent(cmd: &str) -> Intent {
    for segment in cmd.split(['\n', ';', '|', '&']) {
        let words: Vec<&str> = segment.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        let it = simple(&words);
        match it.program.as_deref() {
            Some(p) if SETUP.contains(&p) => continue,
            Some(_) => return it,
            None => continue,
        }
    }
    Intent::default()
}

/// Classify. Returns the category and the rule that decided it, in words.
pub fn classify(
    program: Option<&str>,
    subcommand: Option<&str>,
    script: Option<&str>,
) -> (Category, String) {
    use Category::{Build, Deps, Format, Lint, Other, Test, Typecheck, Unknown};
    let Some(p) = program else {
        return (Unknown, "no program was recorded".into());
    };
    let by_verb = |v: &str| -> Option<Category> {
        let base = v.split([':', '-']).next().unwrap_or(v);
        Some(match base {
            "test" | "tests" | "e2e" | "bench" | "nextest" | "check"
                if v != "check" || p == "make" =>
            {
                Test
            }
            "check" | "typecheck" | "types" => Typecheck,
            "type" => Typecheck,
            "build" | "b" | "compile" | "assemble" | "package" | "all" | "doc" => Build,
            "lint" | "clippy" | "vet" => Lint,
            "fmt" | "format" => Format,
            "install" | "i" | "ci" | "add" | "update" | "upgrade" | "up" | "fetch" | "remove"
            | "rm" | "uninstall" | "restore" | "sync" | "get" | "mod" => Deps,
            _ => return None,
        })
    };
    // Single-purpose programs.
    let direct = match p {
        "pytest" | "jest" | "vitest" | "mocha" | "ava" | "rspec" | "phpunit" | "playwright"
        | "cypress" | "tox" | "nox" | "ctest" => Some(Test),
        "tsc" | "vue-tsc" | "mypy" | "pyright" => Some(Typecheck),
        "eslint" | "flake8" | "pylint" | "golangci-lint" | "shellcheck" | "stylelint"
        | "hadolint" | "clippy-driver" => Some(Lint),
        "prettier" | "rustfmt" | "black" | "isort" | "gofmt" | "clang-format" | "shfmt" => {
            Some(Format)
        }
        "webpack" | "rollup" | "esbuild" | "rustc" | "gcc" | "clang" | "javac" => Some(Build),
        _ => None,
    };
    if let Some(c) = direct {
        return (c, format!("`{p}` is a {} tool", c.as_str()));
    }
    if let Some(s) = script {
        if let Some(c) = by_verb(s) {
            if !matches!(s.split([':', '-']).next(), Some("dev" | "start")) {
                return (c, format!("package script `{s}`"));
            }
        }
        return (Other, format!("package script `{s}`"));
    }
    match (p, subcommand) {
        ("ruff", Some("format")) | ("biome", Some("format")) => (Format, format!("`{p} format`")),
        ("ruff", _) | ("biome", Some("lint" | "check" | "ci")) => (Lint, format!("`{p}` lints")),
        ("cargo", Some("check" | "c")) | ("deno", Some("check")) => {
            (Typecheck, format!("`{p} check` type-checks"))
        }
        ("cargo", Some("miri")) => (Test, "`cargo miri` runs tests under Miri".into()),
        ("cargo", Some("audit" | "deny" | "tree")) => (
            Deps,
            format!("`cargo {}` inspects dependencies", subcommand.unwrap_or("")),
        ),
        ("make" | "just" | "task", Some("check")) => {
            (Test, format!("`{p} check` (conventionally runs tests)"))
        }
        ("gradle" | "gradlew" | "mvn" | "mvnw", Some("check" | "verify")) => {
            (Test, format!("`{p} {}`", subcommand.unwrap_or("")))
        }
        ("npx" | "bunx" | "pnpx" | "python" | "python3" | "py", Some(tool))
        | ("npm" | "pnpm" | "yarn" | "bun" | "uv" | "poetry" | "pdm" | "bundle", Some(tool))
            if RUNNABLE_TOOLS.contains(&tool) && !matches!(tool, "build") || tool == "pip" =>
        {
            let (c, _) = classify(Some(tool), None, None);
            match (c, tool) {
                (Unknown | Other, "pip") => (Deps, format!("`{p} … pip`")),
                (Unknown | Other, "next" | "vite" | "turbo" | "nx") => (
                    Other,
                    format!("`{tool}` via `{p}` (subcommand not recorded)"),
                ),
                (Unknown | Other, _) => (Other, format!("`{tool}` via `{p}`")),
                (c, _) => (c, format!("`{tool}` via `{p}`")),
            }
        }
        (_, Some(s)) => match by_verb(s) {
            Some(c) => (c, format!("`{p} {s}`")),
            None => (Other, format!("`{p} {s}`")),
        },
        (_, None)
            if vocabulary(p).is_some()
                || matches!(p, "npx" | "bunx" | "pnpx" | "python" | "python3") =>
        {
            (
                Unknown,
                format!("`{p}` runs many kinds of work; its subcommand was not recorded"),
            )
        }
        (_, None) => (
            Other,
            format!("`{p}` is not a recognized build, test, lint, format or dependency tool"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat(cmd: &str) -> Category {
        let i = intent(cmd);
        classify(
            i.program.as_deref(),
            i.subcommand.as_deref(),
            i.script.as_deref(),
        )
        .0
    }

    #[test]
    fn classifies_common_commands() {
        use Category::*;
        for (cmd, want) in [
            ("cargo test --workspace", Test),
            ("cargo nextest run", Test),
            ("cargo build --release", Build),
            ("cargo +nightly clippy -- -D warnings", Lint),
            ("cargo check", Typecheck),
            ("cargo fmt --all", Format),
            ("cargo add serde", Deps),
            ("cargo run --bin x", Other),
            ("npm test", Test),
            ("npm run test:unit", Test),
            ("npm run build", Build),
            ("npm run lint", Lint),
            ("npm run typecheck", Typecheck),
            ("pnpm install", Deps),
            ("npm ci", Deps),
            ("yarn test", Test),
            ("bun run format", Format),
            ("npm run dev", Other),
            ("npx vitest run", Test),
            ("npx tsc --noEmit", Typecheck),
            ("npx eslint .", Lint),
            ("npx prettier --write .", Format),
            ("pytest -x tests/", Test),
            ("python -m pytest", Test),
            ("python3 -m mypy src", Typecheck),
            ("uv run pytest", Test),
            ("pip install -r requirements.txt", Deps),
            ("ruff format .", Format),
            ("ruff check .", Lint),
            ("go test ./...", Test),
            ("go vet ./...", Lint),
            ("go build ./cmd/x", Build),
            ("go mod tidy", Deps),
            ("make test", Test),
            ("make", Unknown),
            ("./gradlew build", Build),
            ("mvn verify", Test),
            ("tsc -p .", Typecheck),
            ("eslint src", Lint),
            ("prettier -c .", Format),
            ("cd web && npm test", Test),
            ("cd crates/x; cargo test -q 2>&1 | tail -5", Test),
            ("RUST_LOG=debug timeout 60 cargo test", Test),
            ("time cargo build", Build),
            ("cargo", Unknown),
            ("ls -la", Other),
            ("", Unknown),
        ] {
            assert_eq!(cat(cmd), want, "{cmd}");
        }
    }

    #[test]
    fn records_only_vocabulary_words_never_arguments() {
        let i = intent("cargo test secret_token_abc --features x");
        assert_eq!(i.subcommand.as_deref(), Some("test"));
        assert_eq!(i.script, None);
        let i = intent("npm run deploy-to-prod-with-key-123");
        assert_eq!(i.subcommand.as_deref(), Some("run"));
        assert_eq!(
            i.script, None,
            "script names outside the pattern are not recorded"
        );
        let i = intent("cargo mysecretcommand");
        assert_eq!(i.subcommand, None, "unknown subcommands are not recorded");
        let i = intent("npx some-random-package --token=abc");
        assert_eq!(i.subcommand, None);
        let i = intent("curl -H 'Authorization: Bearer x' https://example.invalid");
        assert_eq!(i.program.as_deref(), Some("curl"));
        assert_eq!(i.subcommand, None);
        let i = intent("python -m http.server 8000");
        assert_eq!(i.subcommand, None);
    }

    #[test]
    fn every_classification_explains_itself() {
        for cmd in [
            "cargo test",
            "npm run lint",
            "make",
            "ls",
            "npx vitest",
            "ruff format .",
        ] {
            let i = intent(cmd);
            let (_, rule) = classify(
                i.program.as_deref(),
                i.subcommand.as_deref(),
                i.script.as_deref(),
            );
            assert!(!rule.is_empty(), "{cmd}");
        }
        assert_eq!(Category::parse("test"), Some(Category::Test));
        assert_eq!(Category::parse("x"), None);
    }
}
