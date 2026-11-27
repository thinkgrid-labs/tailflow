//! Bounded, read-only Git worktree snapshots (M4-D).
//!
//! A snapshot records which paths differ from `HEAD` in a worktree (status,
//! staged/unstaged, rename source, size, mtime, added/deleted line counts)
//! at one moment, plus `HEAD` and the branch. It never records file
//! contents or diffs, and never changes the repository:
//!
//! - `--no-optional-locks` (and `GIT_OPTIONAL_LOCKS=0`): `git status` does
//!   not refresh or rewrite the index.
//! - `core.fsmonitor=false`, `core.untrackedCache=false`: no fsmonitor hook
//!   is executed and no cache is written.
//! - `--no-ext-diff --no-textconv`: no external diff or textconv program
//!   is run for numstat. (Clean filters configured by the *user* — Git LFS,
//!   for example — still run as they would for the user's own `git status`.)
//! - Inherited `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` are removed; no
//!   terminal prompts; `LC_ALL=C`.
//! - A wall-clock timeout kills the process; output is read up to a cap.
//!
//! What a pair of snapshots can say: which paths' working-tree state
//! changed between two moments. It cannot say who changed them. Paths that
//! were already dirty in the first snapshot and did not change are
//! "pre-existing", never credited to anyone.

use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Changed paths kept per snapshot, the first ones by path (the total is
/// always reported). A sorted prefix, so two snapshots can be compared up to
/// the shorter list's last path.
pub const MAX_CHANGES: usize = 2_000;
/// Paths parsed before sorting (bounds memory on a huge dirty tree).
const MAX_PARSED: usize = 20_000;
/// Bytes read from one Git command's output.
const MAX_OUTPUT: u64 = 8 * 1024 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// One path that differs from `HEAD` (or is untracked).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    /// Relative to the worktree root, `/`-separated.
    pub path: String,
    /// `modified`, `added`, `deleted`, `renamed`, `copied`, `type_changed`,
    /// `untracked`, `conflicted`.
    pub status: &'static str,
    pub staged: bool,
    pub unstaged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Modification time (ns since the epoch), from `lstat`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_ns: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_added: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_deleted: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub modified: u64,
    pub added: u64,
    pub deleted: u64,
    pub renamed: u64,
    pub untracked: u64,
    pub conflicted: u64,
    pub other: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub worktree_root: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub changes: Vec<Change>,
    pub total_changes: usize,
    /// More than [`MAX_CHANGES`] paths: the list is incomplete.
    pub truncated: bool,
    /// `git diff --numstat` was unavailable (no HEAD yet, timeout, error):
    /// line counts are missing.
    pub numstat_missing: bool,
    pub counts: Counts,
    pub took_ms: u64,
    /// Filter drivers the repository defines that were disabled for this
    /// snapshot (their clean/smudge/process programs are not run). Files
    /// they manage, Git LFS for example, may then show as modified.
    pub filters_disabled: Vec<String>,
}

/// Locate `git` on `PATH`.
pub fn find_git() -> Option<PathBuf> {
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

fn command(git: &Path, root: &Path) -> Command {
    let mut c = Command::new(git);
    c.arg("--no-optional-locks")
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "core.untrackedCache=false"])
        .args(["-c", "core.quotePath=false"])
        .args(["-c", "color.ui=false"])
        .arg("-C")
        .arg(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_EXTERNAL_DIFF")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    c
}

/// Run with a timeout and a bounded read. `Err` on timeout, spawn failure
/// or a non-zero exit.
fn run(cmd: Command, timeout: Duration) -> Result<Vec<u8>, String> {
    run_ok(cmd, timeout, &[0])
}

/// [`run`], accepting the given exit codes.
fn run_ok(mut cmd: Command, timeout: Duration, ok: &[i32]) -> Result<Vec<u8>, String> {
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("git could not start: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _partial = (&mut stdout).take(MAX_OUTPUT).read_to_end(&mut buf);
        // Drain the rest so the child is never blocked on a full pipe.
        let _rest = std::io::copy(&mut stdout, &mut std::io::sink());
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                let _killed = child.kill();
                let _reaped = child.wait();
                return Err(format!("git timed out after {timeout:?}"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("git wait failed: {e}")),
        }
    };
    let out = reader.join().map_err(|_| "reader panicked".to_string())?;
    if !status.code().is_some_and(|c| ok.contains(&c)) {
        return Err(format!("git exited with {status}"));
    }
    Ok(out)
}

fn status_of(x: u8, y: u8) -> &'static str {
    let any = |c: u8| x == c || y == c;
    if any(b'U') || (x == b'A' && y == b'A') || (x == b'D' && y == b'D') {
        "conflicted"
    } else if any(b'R') {
        "renamed"
    } else if any(b'C') {
        "copied"
    } else if any(b'A') {
        "added"
    } else if any(b'D') {
        "deleted"
    } else if any(b'T') {
        "type_changed"
    } else {
        "modified"
    }
}

/// Parse `git status --porcelain=v2 -z --branch`.
pub fn parse_status(out: &[u8]) -> (Option<String>, Option<String>, bool, Vec<Change>, usize) {
    let mut head = None;
    let mut branch = None;
    let mut detached = false;
    let mut changes = Vec::new();
    let mut total = 0usize;
    let mut records = out.split(|b| *b == 0).filter(|r| !r.is_empty());
    while let Some(rec) = records.next() {
        let line = String::from_utf8_lossy(rec);
        if let Some(rest) = line.strip_prefix("# branch.oid ") {
            if rest != "(initial)" {
                head = Some(rest.to_string());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            if rest == "(detached)" {
                detached = true;
            } else {
                branch = Some(rest.to_string());
            }
            continue;
        }
        let mut push = |c: Change| {
            total += 1;
            if changes.len() < MAX_PARSED {
                changes.push(c);
            }
        };
        let fields: Vec<&str> = line.splitn(9, ' ').collect();
        match fields.first().copied() {
            Some("1") => {
                let xy = fields.get(1).map_or(b"..".as_slice(), |s| s.as_bytes());
                let (x, y) = (
                    xy.first().copied().unwrap_or(b'.'),
                    xy.get(1).copied().unwrap_or(b'.'),
                );
                if let Some(path) = fields.get(8) {
                    push(Change {
                        path: (*path).to_string(),
                        status: status_of(x, y),
                        staged: x != b'.',
                        unstaged: y != b'.',
                        from: None,
                        size: None,
                        mtime_ns: None,
                        lines_added: None,
                        lines_deleted: None,
                    });
                }
            }
            Some("2") => {
                // `2 XY sub mH mI mW hH hI Xscore path`, then origPath.
                let parts: Vec<&str> = line.splitn(10, ' ').collect();
                let xy = parts.get(1).map_or(b"..".as_slice(), |s| s.as_bytes());
                let (x, y) = (
                    xy.first().copied().unwrap_or(b'.'),
                    xy.get(1).copied().unwrap_or(b'.'),
                );
                let from = records
                    .next()
                    .map(|r| String::from_utf8_lossy(r).into_owned());
                if let Some(path) = parts.get(9) {
                    push(Change {
                        path: (*path).to_string(),
                        status: status_of(x, y),
                        staged: x != b'.',
                        unstaged: y != b'.',
                        from,
                        size: None,
                        mtime_ns: None,
                        lines_added: None,
                        lines_deleted: None,
                    });
                }
            }
            Some("u") => {
                let parts: Vec<&str> = line.splitn(11, ' ').collect();
                if let Some(path) = parts.get(10) {
                    push(Change {
                        path: (*path).to_string(),
                        status: "conflicted",
                        staged: true,
                        unstaged: true,
                        from: None,
                        size: None,
                        mtime_ns: None,
                        lines_added: None,
                        lines_deleted: None,
                    });
                }
            }
            Some("?") => {
                if let Some(path) = line.strip_prefix("? ") {
                    push(Change {
                        path: path.to_string(),
                        status: "untracked",
                        staged: false,
                        unstaged: true,
                        from: None,
                        size: None,
                        mtime_ns: None,
                        lines_added: None,
                        lines_deleted: None,
                    });
                }
            }
            _ => {}
        }
    }
    // A deterministic, sorted prefix (git lists tracked changes before
    // untracked ones, so its order is not comparable across snapshots).
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes.truncate(MAX_CHANGES);
    (head, branch, detached, changes, total)
}

/// Parse `git diff --numstat -z`: `added\tdeleted\tpath\0` (renames:
/// `added\tdeleted\t\0from\0to\0`). Binary files report `-`.
pub fn parse_numstat(out: &[u8]) -> Vec<(String, Option<u64>, Option<u64>)> {
    let mut v = Vec::new();
    let mut records = out.split(|b| *b == 0);
    while let Some(rec) = records.next() {
        if rec.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(rec);
        let mut parts = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(p)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let path = if p.is_empty() {
            let _from = records.next();
            records
                .next()
                .map(|r| String::from_utf8_lossy(r).into_owned())
                .unwrap_or_default()
        } else {
            p.to_string()
        };
        v.push((path, a.parse().ok(), d.parse().ok()));
    }
    v
}

fn is_hex_id(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether `old` is a parent of `new` (a commit made on top of `old`).
/// Both must be full hex ids (never passed to `git` otherwise).
pub fn is_parent(git: &Path, root: &Path, new: &str, old: &str) -> bool {
    if !is_hex_id(new) || !is_hex_id(old) {
        return false;
    }
    let mut c = command(git, root);
    c.args(["rev-list", "--parents", "-n", "1", new, "--"]);
    run(c, Duration::from_secs(5)).is_ok_and(|out| {
        String::from_utf8_lossy(&out)
            .split_whitespace()
            .skip(1)
            .any(|p| p == old)
    })
}

/// Filter drivers (`filter.<name>.clean|smudge|process|required`) defined
/// in any config Git reads for this repository. `None` when they cannot be
/// listed (timeout, error) or a driver name cannot be passed safely: then
/// no snapshot is taken, because the filters could not be disabled.
fn filter_drivers(git: &Path, root: &Path, timeout: Duration) -> Option<Vec<String>> {
    let mut c = command(git, root);
    c.args([
        "config",
        "--name-only",
        "--get-regexp",
        r"^filter\..*\.(clean|smudge|process|required)$",
    ]);
    // Exit 1 means no match.
    let out = run_ok(c, timeout, &[0, 1]).ok()?;
    let mut names: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(&out).lines() {
        let Some(rest) = line.strip_prefix("filter.") else {
            continue;
        };
        let Some((name, _)) = rest.rsplit_once('.') else {
            continue;
        };
        let safe = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
        if !safe {
            return None;
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    Some(names)
}

/// Disable every filter driver for one command: the repository's config
/// must not be able to run a program in the background.
fn without_filters(c: &mut Command, names: &[String]) {
    for n in names {
        c.args(["-c", &format!("filter.{n}.clean=")])
            .args(["-c", &format!("filter.{n}.smudge=")])
            .args(["-c", &format!("filter.{n}.process=")])
            .args(["-c", &format!("filter.{n}.required=false")]);
    }
}

/// Take a snapshot of the worktree at `root` with `git`.
///
/// # Errors
/// `git status` failed or timed out (the message says why).
pub fn snapshot(git: &Path, root: &Path, timeout: Duration) -> Result<Snapshot, String> {
    let started = Instant::now();
    let filters = filter_drivers(git, root, timeout).ok_or_else(|| {
        "filter drivers could not be listed or disabled safely; snapshot skipped".to_string()
    })?;
    let timeout = timeout
        .saturating_sub(started.elapsed())
        .max(Duration::from_millis(100));
    let mut st = command(git, root);
    without_filters(&mut st, &filters);
    st.args([
        "status",
        "--porcelain=v2",
        "-z",
        "--branch",
        "--untracked-files=normal",
        "--ignore-submodules=dirty",
    ]);
    let out = run(st, timeout)?;
    let (head, branch, detached, mut changes, total) = parse_status(&out);
    let mut numstat_missing = true;
    if head.is_some() {
        let left = timeout.saturating_sub(started.elapsed());
        let mut d = command(git, root);
        without_filters(&mut d, &filters);
        d.args([
            "diff",
            "HEAD",
            "--numstat",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--ignore-submodules=dirty",
        ]);
        if let Ok(out) = run(d, left.max(Duration::from_millis(100))) {
            numstat_missing = false;
            let stats = parse_numstat(&out);
            let by_path: std::collections::HashMap<&str, (Option<u64>, Option<u64>)> = stats
                .iter()
                .map(|(p, a, d)| (p.as_str(), (*a, *d)))
                .collect();
            for c in &mut changes {
                if let Some((a, d)) = by_path.get(c.path.as_str()) {
                    c.lines_added = *a;
                    c.lines_deleted = *d;
                }
            }
        }
    }
    let mut counts = Counts::default();
    for c in &mut changes {
        let field = match c.status {
            "modified" | "type_changed" => &mut counts.modified,
            "added" => &mut counts.added,
            "deleted" => &mut counts.deleted,
            "renamed" | "copied" => &mut counts.renamed,
            "untracked" => &mut counts.untracked,
            "conflicted" => &mut counts.conflicted,
            _ => &mut counts.other,
        };
        *field += 1;
        if let Ok(m) = std::fs::symlink_metadata(root.join(&c.path)) {
            if m.is_file() {
                c.size = Some(m.len());
            }
            c.mtime_ns = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX));
        }
    }
    Ok(Snapshot {
        worktree_root: root.to_path_buf(),
        head,
        branch,
        detached,
        truncated: total > changes.len(),
        total_changes: total,
        changes,
        numstat_missing,
        counts,
        took_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        filters_disabled: filters,
    })
}

/// How one path's state moved between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Delta {
    pub path: String,
    /// `appeared` (clean before, differs now), `changed` (differed before
    /// and its state changed again), `cleared` (differed before, matches
    /// HEAD now: reverted, or committed), `unchanged` (differed before and
    /// is exactly as it was: pre-existing, not credited to anyone).
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Change>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Change>,
}

/// Compare two snapshots of the same worktree. `complete` is false when
/// either list was truncated (paths beyond the cap cannot be compared).
pub fn compare(before: &Snapshot, after: &Snapshot) -> (Vec<Delta>, bool) {
    use std::collections::BTreeMap;
    // A truncated list is a sorted prefix: beyond its last path nothing is
    // known, so paths past the shorter known prefix are not compared (a
    // pre-existing change must never look like it "appeared").
    let bound: Option<&str> = [before, after]
        .iter()
        .filter(|s| s.truncated)
        .map(|s| s.changes.last().map_or("", |c| c.path.as_str()))
        .min();
    let within = |p: &str| bound.is_none_or(|b| p <= b);
    let b: BTreeMap<&str, &Change> = before
        .changes
        .iter()
        .filter(|c| within(&c.path))
        .map(|c| (c.path.as_str(), c))
        .collect();
    let a: BTreeMap<&str, &Change> = after
        .changes
        .iter()
        .filter(|c| within(&c.path))
        .map(|c| (c.path.as_str(), c))
        .collect();
    let mut out = Vec::new();
    for (p, ca) in &a {
        match b.get(p) {
            None => out.push(Delta {
                path: (*p).to_string(),
                kind: "appeared",
                before: None,
                after: Some((*ca).clone()),
            }),
            Some(cb) => {
                let same = cb.status == ca.status
                    && cb.size == ca.size
                    && cb.mtime_ns == ca.mtime_ns
                    && cb.lines_added == ca.lines_added
                    && cb.lines_deleted == ca.lines_deleted
                    && cb.staged == ca.staged
                    && cb.unstaged == ca.unstaged;
                out.push(Delta {
                    path: (*p).to_string(),
                    kind: if same { "unchanged" } else { "changed" },
                    before: Some((*cb).clone()),
                    after: Some((*ca).clone()),
                });
            }
        }
    }
    for (p, cb) in &b {
        if !a.contains_key(p) {
            out.push(Delta {
                path: (*p).to_string(),
                kind: "cleared",
                before: Some((*cb).clone()),
                after: None,
            });
        }
    }
    (out, !before.truncated && !after.truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> Option<PathBuf> {
        let g = find_git();
        if g.is_none() {
            eprintln!("git not installed; skipped");
        }
        g
    }

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        sh(d.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(d.path().join("a.rs"), "one\n").unwrap();
        std::fs::write(d.path().join("b.rs"), "two\n").unwrap();
        sh(d.path(), &["add", "."]);
        sh(d.path(), &["commit", "-q", "-m", "init"]);
        d
    }

    #[test]
    fn parses_porcelain_v2_records() {
        let out = b"# branch.oid 1111111111111111111111111111111111111111\0# branch.head main\0\
1 .M N... 100644 100644 100644 aaa bbb src/a.rs\0\
2 R. N... 100644 100644 100644 aaa bbb R100 new name.rs\0old.rs\0\
u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
? new file.txt\0";
        let (head, branch, detached, changes, total) = parse_status(out);
        assert_eq!(
            head.as_deref(),
            Some("1111111111111111111111111111111111111111")
        );
        assert_eq!(branch.as_deref(), Some("main"));
        assert!(!detached);
        assert_eq!(total, 4);
        let by = |p: &str| changes.iter().find(|c| c.path == p).unwrap();
        assert_eq!(by("src/a.rs").status, "modified");
        assert!(by("src/a.rs").unstaged && !by("src/a.rs").staged);
        assert_eq!(by("new name.rs").status, "renamed");
        assert_eq!(by("new name.rs").from.as_deref(), Some("old.rs"));
        assert_eq!(by("conflict.rs").status, "conflicted");
        assert_eq!(by("new file.txt").status, "untracked");
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(paths, sorted, "a sorted, deterministic list");
    }

    #[test]
    fn parses_numstat_including_renames_and_binary() {
        let out = b"3\t1\tsrc/a.rs\0-\t-\timg.png\0\x30\t0\t\0old.rs\0new.rs\0";
        let v = parse_numstat(out);
        assert_eq!(v[0], ("src/a.rs".to_string(), Some(3), Some(1)));
        assert_eq!(v[1], ("img.png".to_string(), None, None));
        assert_eq!(v[2].0, "new.rs");
    }

    #[test]
    fn snapshots_a_real_worktree_without_touching_it() {
        let Some(git) = git_available() else { return };
        let d = repo();
        std::fs::write(d.path().join("a.rs"), "one\nmore\n").unwrap();
        std::fs::write(d.path().join("new.rs"), "x").unwrap();
        let index_before = std::fs::read(d.path().join(".git/index")).unwrap();
        let mtime_before = std::fs::metadata(d.path().join(".git/index"))
            .unwrap()
            .modified()
            .unwrap();
        let s = snapshot(&git, d.path(), DEFAULT_TIMEOUT).unwrap();
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert!(s.head.is_some());
        assert_eq!(s.total_changes, 2);
        let a = s.changes.iter().find(|c| c.path == "a.rs").unwrap();
        assert_eq!(a.status, "modified");
        assert_eq!(a.lines_added, Some(1));
        assert!(a.size.is_some() && a.mtime_ns.is_some());
        assert!(s
            .changes
            .iter()
            .any(|c| c.path == "new.rs" && c.status == "untracked"));
        // Read-only: the index is byte-identical and not rewritten.
        assert_eq!(
            std::fs::read(d.path().join(".git/index")).unwrap(),
            index_before
        );
        assert_eq!(
            std::fs::metadata(d.path().join(".git/index"))
                .unwrap()
                .modified()
                .unwrap(),
            mtime_before
        );
        assert!(!d.path().join(".git/index.lock").exists());
    }

    #[test]
    fn comparing_snapshots_never_credits_pre_existing_changes() {
        let Some(git) = git_available() else { return };
        let d = repo();
        // Dirty before the "session".
        std::fs::write(d.path().join("a.rs"), "pre-existing\n").unwrap();
        let before = snapshot(&git, d.path(), DEFAULT_TIMEOUT).unwrap();
        // During: b.rs changes, a.rs untouched, c.rs created.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(d.path().join("b.rs"), "two\nthree\n").unwrap();
        std::fs::write(d.path().join("c.rs"), "new").unwrap();
        let after = snapshot(&git, d.path(), DEFAULT_TIMEOUT).unwrap();
        let (deltas, complete) = compare(&before, &after);
        assert!(complete);
        let kind = |p: &str| deltas.iter().find(|x| x.path == p).map(|x| x.kind);
        assert_eq!(
            kind("a.rs"),
            Some("unchanged"),
            "pre-existing: not credited"
        );
        assert_eq!(kind("b.rs"), Some("appeared"));
        assert_eq!(kind("c.rs"), Some("appeared"));
        // Revert a.rs: it clears.
        std::fs::write(d.path().join("a.rs"), "one\n").unwrap();
        let later = snapshot(&git, d.path(), DEFAULT_TIMEOUT).unwrap();
        let (deltas, _) = compare(&after, &later);
        assert_eq!(
            deltas.iter().find(|x| x.path == "a.rs").map(|x| x.kind),
            Some("cleared")
        );
    }

    #[test]
    fn a_hostile_fsmonitor_hook_is_not_executed() {
        let Some(git) = git_available() else { return };
        let d = repo();
        let marker = d.path().join("PWNED");
        // A repository config that would run a program on `git status`.
        let hook = if cfg!(windows) {
            format!("cmd /c type nul > {}", marker.display())
        } else {
            format!("touch {}", marker.display())
        };
        sh(d.path(), &["config", "core.fsmonitor", &hook]);
        std::fs::write(d.path().join("a.rs"), "changed\n").unwrap();
        let s = snapshot(&git, d.path(), DEFAULT_TIMEOUT).unwrap();
        assert_eq!(s.total_changes, 1);
        assert!(!marker.exists(), "core.fsmonitor must not run");
    }

    #[test]
    fn parent_detection_only_accepts_hex_ids() {
        let Some(git) = git_available() else { return };
        let d = repo();
        let head = |d: &Path| {
            let out = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(d)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let first = head(d.path());
        std::fs::write(d.path().join("a.rs"), "x\n").unwrap();
        sh(d.path(), &["commit", "-q", "-am", "second"]);
        let second = head(d.path());
        assert!(is_parent(&git, d.path(), &second, &first));
        assert!(!is_parent(&git, d.path(), &first, &second));
        assert!(
            !is_parent(&git, d.path(), "--output=/tmp/x", &first),
            "options never reach git"
        );
    }

    #[test]
    fn truncated_snapshots_are_compared_only_within_their_common_prefix() {
        let ch = |p: &str| Change {
            path: p.into(),
            status: "modified",
            staged: false,
            unstaged: true,
            from: None,
            size: Some(1),
            mtime_ns: Some(1),
            lines_added: None,
            lines_deleted: None,
        };
        let snap = |paths: &[&str], truncated: bool| Snapshot {
            worktree_root: PathBuf::from("/r"),
            head: None,
            branch: None,
            detached: false,
            changes: paths.iter().map(|p| ch(p)).collect(),
            total_changes: 100,
            truncated,
            numstat_missing: false,
            counts: Counts::default(),
            took_ms: 0,
            filters_disabled: Vec::new(),
        };
        // "before" lists a..c (truncated), "after" lists a..e (truncated):
        // d and e are past what "before" knows and must not "appear".
        let (deltas, complete) = compare(
            &snap(&["a", "b", "c"], true),
            &snap(&["a", "b", "c", "d", "e"], true),
        );
        assert!(!complete);
        assert!(deltas.iter().all(|d| d.path.as_str() <= "c"), "{deltas:?}");
        assert!(deltas.iter().all(|d| d.kind == "unchanged"));
    }

    #[test]
    fn repository_filter_drivers_are_not_run() {
        let Some(git) = git_available() else { return };
        let d = repo();
        let marker = d.path().join("FILTER-RAN");
        let cmd = if cfg!(windows) {
            format!("cmd /c type nul > {} & more", marker.display())
        } else {
            format!("sh -c 'touch {}; cat'", marker.display())
        };
        sh(d.path(), &["config", "filter.evil.clean", &cmd]);
        std::fs::write(d.path().join(".gitattributes"), "*.rs filter=evil\n").unwrap();
        std::fs::write(d.path().join("a.rs"), "changed\n").unwrap();
        let s = snapshot(&git, d.path(), DEFAULT_TIMEOUT).unwrap();
        // Filters from the user's global config (git-lfs on CI runners)
        // are disabled too.
        assert!(
            s.filters_disabled.iter().any(|f| f == "evil"),
            "{:?}",
            s.filters_disabled
        );
        assert!(!marker.exists(), "the clean filter must not run");
    }

    #[test]
    fn a_hanging_git_is_killed_at_the_timeout() {
        if cfg!(windows) {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let fake = d.path().join("git");
        std::fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let t = Instant::now();
        let err = snapshot(&fake, d.path(), Duration::from_millis(300)).unwrap_err();
        // The first call (listing filter drivers) hangs: the snapshot fails
        // closed, before `git status` could run with unknown filters.
        assert!(
            err.contains("could not be listed") || err.contains("timed out"),
            "{err}"
        );
        assert!(t.elapsed() < Duration::from_secs(5));
    }
}
