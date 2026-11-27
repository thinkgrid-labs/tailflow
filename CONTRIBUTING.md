# Contributing to TailFlow

Thank you for your interest in contributing! This document covers how to get started, what to work on, and how to submit changes.

---

## Table of Contents

- [Getting Started](#getting-started)
- [Development Workflow](#development-workflow)
- [Project Structure](#project-structure)
- [Submitting Changes](#submitting-changes)
- [Reporting Bugs](#reporting-bugs)
- [Requesting Features](#requesting-features)

---

## Getting Started

### Prerequisites

- **Rust 1.89+** — install via [rustup](https://rustup.rs)
- **Node.js 20+** — only needed if you are working on the web dashboard
- **Docker** (optional) — for testing Docker log ingestion

### Clone and build

```bash
git clone https://github.com/thinkgrid-labs/tailflow.git
cd tailflow

# Build the web dashboard (required for the daemon binary)
cd web && npm ci && npm run build && cd ..

# Build all Rust crates
cargo build --all
```

### Run the quality gate locally

Before opening a pull request, make sure all checks pass:

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

If you change documentation, also run the documentation check. With built
binaries it also checks every documented CLI command and flag:

```bash
python3 scripts/docs/check_docs.py --bin target/debug
```

`npm run build` deletes the tracked `web/dist/.gitkeep`; restore it with
`git checkout -- web/dist/.gitkeep` before committing.

---

## Development Workflow

1. **Open an issue first** for anything non-trivial (new features, architectural changes, breaking API changes). This avoids duplicated effort and lets us align before you write code.
2. **Fork** the repository and create a branch from `dev`:
   ```bash
   git checkout -b feat/your-feature dev
   ```
3. Make your changes. Keep commits focused — one logical change per commit.
4. Run the quality gate (see above).
5. Open a pull request targeting the `dev` branch.

---

## Project Structure

```
tailflow/
├── crates/
│   ├── tailflow-core/     # Ingestion, event model, agent adapters, observation — no UI
│   ├── tailflow-storage/  # SQLite event store, retention, queries
│   ├── tailflow-tui/      # `tailflow` binary: TUI, init, agent hooks and CLI
│   ├── tailflow-daemon/   # axum HTTP daemon binary
│   └── tailflow-agent/    # MCP and shell client binaries
├── web/                   # Preact web dashboard (Vite)
├── npm/                   # npm distribution packages
├── docs/                  # User guides and design documentation
└── scripts/               # Packaging, benchmarks, smoke tests, docs check
```

New log sources belong in `crates/tailflow-core/src/ingestion/`. New UI features belong in either `crates/tailflow-tui/` or `web/src/`.

---

## Submitting Changes

- **Target branch**: always `dev`, not `main`. The `main` branch is for releases.
- **PR title**: use conventional commits style — `feat:`, `fix:`, `docs:`, `chore:`, `test:`, `refactor:`.
- **Tests**: add unit or integration tests for new behavior. The test suite lives in `crates/*/tests/` and `crates/*/src/` (`#[cfg(test)]` modules).
- **Breaking changes**: note them explicitly in the PR description and prefix the commit with `feat!:` or `fix!:`.

---

## Reporting Bugs

Use the **Bug Report** issue template. Include:

- TailFlow version (`tailflow --version`)
- OS and architecture
- Steps to reproduce
- What you expected vs. what happened
- Relevant log output or error message

---

## Requesting Features

Use the **Feature Request** issue template. Describe the problem you are trying to solve, not just the solution. This helps us understand the use case and suggest alternatives if a simpler approach exists.
