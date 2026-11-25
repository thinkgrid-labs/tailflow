//! Project and Git association.
//!
//! Read-only and bounded: TailFlow never runs `git`, never stages, resets,
//! checks out or locks anything, and reads at most a few small files
//! (`.git`, `HEAD`, one ref, `packed-refs` up to a size cap). It runs only on
//! session start and end, never per tool call, and never scans the tree.
//!
//! What this can and cannot say:
//! - It records *where* a session ran (repository, worktree, branch, HEAD).
//! - It does **not** attribute changes: a file in `git diff` was not
//!   necessarily changed by the agent, and a HEAD that moved during a session
//!   may have been moved by the developer in another terminal. A HEAD change
//!   is recorded as `inferred.head_changed` with `inferred.confidence = low`.

use crate::event::ObservabilityEvent;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Directory levels walked up from the working directory looking for `.git`.
const MAX_WALK: usize = 64;
/// Cap on `.git` files, `HEAD`, `commondir` and loose refs.
const SMALL_FILE: u64 = 4 * 1024;
/// Cap on `packed-refs`; larger repositories simply report no HEAD commit.
const PACKED_REFS_MAX: u64 = 4 * 1024 * 1024;

/// Where a working directory sits in Git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInfo {
    /// Top of the working tree containing the directory (the project).
    pub worktree_root: PathBuf,
    /// The main repository's working tree when this is a linked worktree.
    pub repository_root: Option<PathBuf>,
    /// Linked worktree name (`.git/worktrees/<name>`), `None` for the main one.
    pub worktree: Option<String>,
    pub branch: Option<String>,
    /// Commit id of HEAD (hex), when resolvable within the bounds.
    pub head: Option<String>,
    pub detached: bool,
}

fn read_small(path: &Path, cap: u64) -> Option<String> {
    let f = fs::File::open(path).ok()?;
    let mut s = String::new();
    f.take(cap.saturating_add(1)).read_to_string(&mut s).ok()?;
    (u64::try_from(s.len()).ok()? <= cap).then_some(s)
}

fn is_hex_id(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A ref name that is safe to join onto the git dir (no `..`, no absolute
/// paths, only `refs/…`).
fn safe_ref(name: &str) -> bool {
    name.starts_with("refs/")
        && name.len() <= 255
        && !name.contains("..")
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.ends_with('/')
}

/// Discover Git facts for `cwd`. `None` when `cwd` is not inside a working
/// tree (or the metadata is unreadable or unexpected).
pub fn discover(cwd: &Path) -> Option<GitInfo> {
    let mut dir = cwd.to_path_buf();
    for _ in 0..MAX_WALK {
        let dot_git = dir.join(".git");
        if let Ok(meta) = fs::symlink_metadata(&dot_git) {
            return if meta.is_dir() {
                Some(describe(dir, &dot_git, None))
            } else if meta.is_file() {
                linked(dir, &dot_git)
            } else {
                None
            };
        }
        if !dir.pop() {
            return None;
        }
    }
    None
}

/// A `.git` *file*: a linked worktree or submodule (`gitdir: <path>`).
fn linked(root: PathBuf, dot_git: &Path) -> Option<GitInfo> {
    let text = read_small(dot_git, SMALL_FILE)?;
    let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let git_dir = {
        let p = Path::new(target);
        if p.has_root() {
            p.to_path_buf()
        } else {
            root.join(p)
        }
    };
    // `<common>/worktrees/<name>` for worktrees; modules for submodules.
    let worktree = git_dir
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "worktrees"))
        .and_then(|_| git_dir.file_name())
        .map(|n| n.to_string_lossy().into_owned());
    let common = read_small(&git_dir.join("commondir"), SMALL_FILE)
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .map(|c| {
            let p = Path::new(&c);
            if p.has_root() {
                p.to_path_buf()
            } else {
                git_dir.join(p)
            }
        });
    let mut info = describe(root, &git_dir, common.as_deref());
    info.worktree = worktree;
    info.repository_root = common
        .as_deref()
        .and_then(|c| super::normalize_path(&c.display().to_string(), None))
        .and_then(|c| c.parent().map(Path::to_path_buf));
    Some(info)
}

fn describe(root: PathBuf, git_dir: &Path, common_dir: Option<&Path>) -> GitInfo {
    let common = common_dir.unwrap_or(git_dir);
    let head_text = read_small(&git_dir.join("HEAD"), SMALL_FILE);
    let head_text = head_text.as_deref().map(str::trim).unwrap_or("");
    let (branch, head, detached) = if let Some(r) = head_text.strip_prefix("ref:") {
        let r = r.trim();
        let branch = r.strip_prefix("refs/heads/").map(str::to_string);
        let head = safe_ref(r)
            .then(|| resolve_ref(git_dir, common, r))
            .flatten();
        (branch, head, false)
    } else if is_hex_id(head_text) {
        (None, Some(head_text.to_string()), true)
    } else {
        (None, None, false)
    };
    GitInfo {
        worktree_root: root,
        repository_root: None,
        worktree: None,
        branch,
        head,
        detached,
    }
}

fn resolve_ref(git_dir: &Path, common: &Path, name: &str) -> Option<String> {
    for base in [git_dir, common] {
        if let Some(text) = read_small(&base.join(name), SMALL_FILE) {
            let id = text.trim();
            if is_hex_id(id) {
                return Some(id.to_string());
            }
        }
    }
    let packed = read_small(&common.join("packed-refs"), PACKED_REFS_MAX)?;
    packed.lines().find_map(|line| {
        let (id, r) = line.split_once(' ')?;
        (r.trim() == name && is_hex_id(id)).then(|| id.to_string())
    })
}

/// Record `git` on a session boundary event; `git.repository = false`
/// when the directory is not in a working tree.
pub(crate) fn add_git_attributes(e: &mut ObservabilityEvent, git: Option<&GitInfo>) {
    let Some(g) = git else {
        e.attributes
            .insert("git.repository".into(), Value::Bool(false));
        return;
    };
    super::put(
        &mut e.attributes,
        "git.root",
        Some(g.worktree_root.display().to_string()),
    );
    super::put(
        &mut e.attributes,
        "git.repository_root",
        g.repository_root.as_ref().map(|p| p.display().to_string()),
    );
    super::put(&mut e.attributes, "git.worktree", g.worktree.clone());
    super::put(&mut e.attributes, "git.branch", g.branch.clone());
    super::put(&mut e.attributes, "git.head", g.head.clone());
    if g.detached {
        e.attributes
            .insert("git.detached".into(), Value::Bool(true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    fn repo() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let g = t.path().join(".git");
        fs::create_dir_all(g.join("refs/heads")).unwrap();
        fs::write(g.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(g.join("refs/heads/main"), format!("{A}\n")).unwrap();
        fs::create_dir_all(t.path().join("src/deep")).unwrap();
        t
    }

    #[test]
    fn finds_root_branch_and_head_from_a_subdirectory() {
        let t = repo();
        let info = discover(&t.path().join("src/deep")).unwrap();
        assert_eq!(info.worktree_root, t.path());
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert_eq!(info.head.as_deref(), Some(A));
        assert!(!info.detached);
        assert_eq!(info.worktree, None);
    }

    #[test]
    fn packed_refs_and_detached_head() {
        let t = repo();
        let g = t.path().join(".git");
        fs::remove_file(g.join("refs/heads/main")).unwrap();
        fs::write(
            g.join("packed-refs"),
            format!("# pack-refs\n{B} refs/heads/main\n"),
        )
        .unwrap();
        assert_eq!(discover(t.path()).unwrap().head.as_deref(), Some(B));
        fs::write(g.join("HEAD"), format!("{A}\n")).unwrap();
        let info = discover(t.path()).unwrap();
        assert!(info.detached);
        assert_eq!(info.branch, None);
        assert_eq!(info.head.as_deref(), Some(A));
    }

    #[test]
    fn linked_worktree_is_identified() {
        let t = repo();
        let g = t.path().join(".git");
        let wt_git = g.join("worktrees/feature");
        fs::create_dir_all(&wt_git).unwrap();
        fs::write(wt_git.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        fs::write(g.join("refs/heads/feature"), format!("{B}\n")).unwrap();
        let wt = tempfile::tempdir().unwrap();
        fs::write(
            wt.path().join(".git"),
            format!("gitdir: {}\n", wt_git.display()),
        )
        .unwrap();
        let info = discover(wt.path()).unwrap();
        assert_eq!(info.worktree_root, wt.path());
        assert_eq!(info.worktree.as_deref(), Some("feature"));
        assert_eq!(info.branch.as_deref(), Some("feature"));
        assert_eq!(
            info.head.as_deref(),
            Some(B),
            "ref resolved in the common dir"
        );
        assert_eq!(info.repository_root.as_deref(), Some(t.path()));
    }

    #[test]
    fn hostile_refs_are_not_followed() {
        let t = repo();
        fs::write(t.path().join(".git/HEAD"), "ref: ../../../../etc/passwd\n").unwrap();
        let info = discover(t.path()).unwrap();
        assert_eq!(info.head, None);
        assert_eq!(info.branch, None);
    }

    #[test]
    fn outside_a_repository_is_none() {
        let t = tempfile::tempdir().unwrap();
        // The temp dir may itself sit inside a checkout on some hosts; only
        // assert when it does not.
        if discover(t.path()).is_some_and(|i| i.worktree_root == t.path()) {
            panic!("a bare temp dir is not a worktree root");
        }
    }
}
