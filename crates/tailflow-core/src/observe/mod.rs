//! Independent filesystem observation of explicitly selected projects (M4-C).
//!
//! An agent hook saying "I edited `src/api.ts`" is a claim. This module
//! watches the project directories the user listed in `[observe] paths` and
//! records what the filesystem itself reported, as `file.*` events with
//! provenance `filesystem_observed` and source type `fs_watcher`.
//!
//! What an observation is, and is not:
//! - It says a path changed, when, and how (created, modified, deleted,
//!   renamed), and its size afterwards. It never contains file contents.
//! - It says nothing about **who** changed the file. A watcher cannot see
//!   the writing process; an agent, a human in an editor, a build or a
//!   formatter all look the same.
//! - It is not a forensic record: notifications can be coalesced by the OS,
//!   dropped on overflow (recorded as loss), or missed while the daemon is
//!   not running (no coverage). Coverage and loss are recorded so a missing
//!   observation is never read as "nothing changed" without saying so.
//!
//! Boundaries:
//! - Only the configured roots, never `/` or the home directory itself, and
//!   never TailFlow's own data directory (its database writes would feed
//!   back into the watcher).
//! - Built-in exclusions (VCS metadata, dependency and build directories),
//!   editor temporary files, `.gitignore` rules and `[observe] exclude`
//!   globs. Excluded changes are not recorded and are not loss.
//! - Symlinks are never followed when walking (Linux registers watches per
//!   directory); events are only accepted for paths under the root.
//! - On Linux (inotify: one watch per directory) at most `max_directories`
//!   are watched; the rest are counted and reported as partial coverage.
//!   macOS (FSEvents) and Windows (ReadDirectoryChangesW) watch the root
//!   recursively and filter in the observer.

pub mod git;
pub mod gitignore;

use crate::event::{ObservabilityEvent, Provenance};
use chrono::{DateTime, Utc};
use gitignore::{IgnoreFile, Verdict};
use notify::{event::ModifyKind, event::RenameMode, Event, EventKind, RecursiveMode, Watcher};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `source_type` of filesystem observations.
pub const SOURCE_TYPE: &str = "fs_watcher";

/// Directory names never watched (version control metadata, dependency and
/// build output). `.gitignore` usually covers the rest.
pub const BUILTIN_EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".jj",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    ".next",
    ".turbo",
    ".gradle",
    ".tailflow",
];

/// Notifications buffered between the OS watcher and the observer. Beyond
/// this they are dropped and counted (`fs_observer_overflow`).
const QUEUE: usize = 8_192;
/// Paths with a change waiting to be coalesced.
const MAX_PENDING: usize = 10_000;
/// Files reported from one directory that appeared at once (moved in, or
/// created and filled before its watch existed). Beyond this a rescan loss
/// is recorded rather than an unbounded walk.
const MAX_NEW_DIR_FILES: usize = 10_000;
/// Ignore rules kept over all ignore files of a root. Beyond this, further
/// nested `.gitignore` files are not applied (the observer then records
/// *more*, never less) and the status says so.
pub const MAX_TOTAL_IGNORE_RULES: usize = 20_000;
/// A path is reported at most this long after its first notification even
/// if notifications keep arriving (a file written continuously).
const MAX_HOLD: Duration = Duration::from_secs(2);
/// Loss (queue overflow, rescans) is reported to the sink at most this
/// often, and within this long of being detected.
const LOSS_REPORT_EVERY: Duration = Duration::from_secs(1);
/// After the last notification of a burst the observer checks once more,
/// this much later, for overflow the OS callback counted after the queue
/// had already drained (the drop and its count are not one atomic step).
const LOSS_SETTLE: Duration = Duration::from_secs(1);
/// Notifications handled per wakeup before deadlines are looked at again.
const DRAIN_BATCH: usize = 1_024;
/// Distinct notification kinds merged into one queued path (there are
/// fewer kinds than this; the bound only makes it explicit).
const MAX_MERGED_KINDS: usize = 16;

/// `[observe]` in `tailflow.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ObserveConfig {
    /// Project directories to observe, relative to `tailflow.toml`. Empty:
    /// observation is off (the default).
    pub paths: Vec<PathBuf>,
    /// Extra glob exclusions relative to each root (`**` crosses `/`).
    pub exclude: Vec<String>,
    pub respect_gitignore: bool,
    /// Bounded Git status snapshots for observed roots inside a repository.
    pub git: bool,
    /// Directory watch budget per root (Linux).
    pub max_directories: usize,
    /// Quiet time before a path's notifications are coalesced into one
    /// observation.
    pub coalesce_ms: u64,
    /// Minimum interval between Git snapshots of one repository triggered
    /// by file changes (boundary snapshots are not limited).
    pub git_min_interval_secs: u64,
}

impl Default for ObserveConfig {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            exclude: Vec::new(),
            respect_gitignore: true,
            git: true,
            max_directories: 20_000,
            coalesce_ms: 200,
            git_min_interval_secs: 30,
        }
    }
}

/// Why a configured root was refused.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RootError {
    #[error("{0}: not a directory")]
    NotADirectory(PathBuf),
    #[error(
        "{0}: refusing to observe a filesystem root or home directory; list project directories"
    )]
    TooBroad(PathBuf),
    #[error("{0}: inside TailFlow's data directory (its own writes would be observed)")]
    DataDirectory(PathBuf),
    #[error("invalid [observe] exclude glob {glob:?}: {reason}")]
    BadGlob { glob: String, reason: String },
    #[error("[observe] {0}")]
    Invalid(String),
}

impl ObserveConfig {
    /// # Errors
    /// Out-of-range numbers or invalid globs.
    pub fn validate(&self) -> Result<(), RootError> {
        if self.paths.len() > 64 {
            return Err(RootError::Invalid("at most 64 paths".into()));
        }
        if !(1..=1_000_000).contains(&self.max_directories) {
            return Err(RootError::Invalid(
                "max_directories must be 1..=1000000".into(),
            ));
        }
        if !(20..=10_000).contains(&self.coalesce_ms) {
            return Err(RootError::Invalid("coalesce_ms must be 20..=10000".into()));
        }
        if !(5..=86_400).contains(&self.git_min_interval_secs) {
            return Err(RootError::Invalid(
                "git_min_interval_secs must be 5..=86400".into(),
            ));
        }
        if self.exclude.len() > 256 {
            return Err(RootError::Invalid("at most 256 exclude globs".into()));
        }
        for g in &self.exclude {
            crate::privacy::glob_to_regex(g).map_err(|e| RootError::BadGlob {
                glob: g.clone(),
                reason: e.to_string(),
            })?;
        }
        Ok(())
    }
}

/// A root as the user named it (absolute, lexically normalized, symlinks
/// kept) and as the OS reports it (canonical). Observations are recorded
/// under `display`, the form agents and shells report paths in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub display: PathBuf,
    pub canonical: PathBuf,
}

impl Root {
    /// Resolve a configured path relative to `base`.
    ///
    /// # Errors
    /// Not a directory, too broad, or inside `data_dir`.
    pub fn resolve(raw: &Path, base: &Path, data_dir: Option<&Path>) -> Result<Self, RootError> {
        let joined = if raw.has_root() {
            raw.to_path_buf()
        } else {
            base.join(raw)
        };
        let display = crate::agent::normalize_path(&joined.to_string_lossy(), None)
            .ok_or_else(|| RootError::NotADirectory(joined.clone()))?;
        let canonical = std::fs::canonicalize(&display)
            .map(simplify_verbatim)
            .map_err(|_| RootError::NotADirectory(display.clone()))?;
        if !canonical.is_dir() {
            return Err(RootError::NotADirectory(display));
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .and_then(|h| std::fs::canonicalize(h).ok());
        let is_fs_root = canonical.parent().is_none()
            || canonical
                .components()
                .filter(|c| matches!(c, Component::Normal(_)))
                .count()
                == 0;
        if is_fs_root || home.as_deref() == Some(canonical.as_path()) {
            return Err(RootError::TooBroad(display));
        }
        // A project inside the data directory is refused; the data
        // directory inside a project is excluded by the observer's rules.
        // Both sides in the same (non-verbatim) form, or `starts_with`
        // never matches on Windows.
        if let Some(d) = data_dir.and_then(|d| std::fs::canonicalize(d).ok().map(simplify_verbatim))
        {
            if canonical.starts_with(&d) {
                return Err(RootError::DataDirectory(display));
            }
        }
        Ok(Self { display, canonical })
    }

    /// Map a path the OS reported (canonical or display form) to the
    /// display form, if it lies under this root.
    pub fn to_display(&self, p: &Path) -> Option<PathBuf> {
        if let Ok(rel) = p.strip_prefix(&self.canonical) {
            return Some(self.display.join(rel));
        }
        if p.starts_with(&self.display) {
            return Some(p.to_path_buf());
        }
        None
    }
}

/// On Windows `canonicalize` returns verbatim paths (`\\?\C:\…`,
/// `\\?\UNC\server\share\…`). Watch and compare in the ordinary form,
/// which is what the OS reports for a watch registered on it. Other
/// platforms are unaffected.
fn simplify_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        // Only drive paths (`C:\…`); other verbatim forms stay as they are.
        if rest.as_bytes().get(1) == Some(&b':') {
            return PathBuf::from(rest);
        }
    }
    p
}

/// What the observer ignores under one root.
#[derive(Debug)]
pub struct Rules {
    root: PathBuf,
    globs: Vec<Regex>,
    /// Ignore files by directory (root `.gitignore`, `.git/info/exclude`,
    /// nested `.gitignore` found while walking). Deepest applies last.
    ignores: Vec<IgnoreFile>,
    /// Extra absolute directories never observed (TailFlow's own data).
    excluded_dirs: Vec<PathBuf>,
    /// Directories whose `.gitignore` was already considered.
    ignore_dirs: HashSet<PathBuf>,
    ignore_rules: usize,
    /// Nested ignore files skipped because of [`MAX_TOTAL_IGNORE_RULES`].
    pub ignore_files_skipped: u64,
}

impl Rules {
    /// Rules for `root` (display form).
    pub fn new(root: &Path, cfg: &ObserveConfig, excluded_dirs: Vec<PathBuf>) -> Self {
        let globs = cfg
            .exclude
            .iter()
            .filter_map(|g| crate::privacy::glob_to_regex(g).ok())
            .collect();
        let mut ignores = Vec::new();
        if cfg.respect_gitignore {
            if let Some(f) = IgnoreFile::load(root, &root.join(".git/info/exclude")) {
                ignores.push(f);
            }
            if let Some(f) = IgnoreFile::load(root, &root.join(".gitignore")) {
                ignores.push(f);
            }
        }
        let ignore_rules = ignores.iter().map(IgnoreFile::len).sum();
        Self {
            root: root.to_path_buf(),
            globs,
            ignores,
            excluded_dirs,
            ignore_dirs: HashSet::from([root.to_path_buf()]),
            ignore_rules,
            ignore_files_skipped: 0,
        }
    }

    /// Add a nested `.gitignore` (found while walking `dir`).
    /// Bounded in total ([`MAX_TOTAL_IGNORE_RULES`]) and once per directory.
    pub fn add_ignore_file(&mut self, dir: &Path) {
        if !self.ignore_dirs.insert(dir.to_path_buf()) {
            return;
        }
        if let Some(f) = IgnoreFile::load(dir, &dir.join(".gitignore")) {
            if f.is_empty() {
                return;
            }
            if self.ignore_rules.saturating_add(f.len()) > MAX_TOTAL_IGNORE_RULES {
                self.ignore_files_skipped += 1;
                return;
            }
            self.ignore_rules += f.len();
            self.ignores.push(f);
        }
    }

    /// Whether `path` (display form, under the root) is excluded.
    pub fn excluded(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return true;
        };
        if self.excluded_dirs.iter().any(|d| path.starts_with(d)) {
            return true;
        }
        let comps: Vec<String> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        if comps
            .iter()
            .any(|c| BUILTIN_EXCLUDED_DIRS.contains(&c.as_str()))
        {
            return true;
        }
        if let Some(name) = comps.last() {
            if !is_dir && is_editor_temp(name) {
                return true;
            }
        }
        let rel_s = rel.to_string_lossy().replace('\\', "/");
        if self.globs.iter().any(|g| g.is_match(&rel_s)) {
            return true;
        }
        // An ignored ancestor directory ignores everything below it.
        let mut cur = self.root.clone();
        for (i, c) in comps.iter().enumerate() {
            cur.push(c);
            let last = i + 1 == comps.len();
            let dir = !last || is_dir;
            let mut verdict = Verdict::NoMatch;
            for f in &self.ignores {
                if cur.starts_with(f.base()) {
                    match f.verdict(&cur, dir) {
                        Verdict::NoMatch => {}
                        v => verdict = v,
                    }
                }
            }
            if verdict == Verdict::Ignored {
                return true;
            }
        }
        false
    }
}

/// Swap, backup and lock files editors write next to the real file.
pub fn is_editor_temp(name: &str) -> bool {
    name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swo")
        || name.ends_with(".swx")
        || name.ends_with(".tmp")
        || name.starts_with(".#")
        || (name.starts_with('#') && name.ends_with('#'))
        || name == "4913"
        || name == ".DS_Store"
        || name.contains(".tmp.")
        || name.ends_with(".crswap")
}

/// Where observations and loss go (the daemon writes them to storage).
pub trait Sink: Send + 'static {
    fn observed(&self, event: ObservabilityEvent);
    /// `rescan`: the OS watcher reported lost notifications (count is a
    /// lower bound); otherwise TailFlow's queue overflowed (exact count).
    fn lost(&self, rescan: bool, count: u64, first: DateTime<Utc>, last: DateTime<Utc>);
    /// Directory counts changed (Linux watch budget).
    fn coverage(&self, watched: u64, unwatched: u64);
}

/// Live counters, readable while the observer runs.
#[derive(Debug, Default)]
pub struct Stats {
    pub notifications: AtomicU64,
    pub observations: AtomicU64,
    pub excluded: AtomicU64,
    pub overflowed: AtomicU64,
    pub rescans: AtomicU64,
    pub watched_dirs: AtomicU64,
    pub unwatched_dirs: AtomicU64,
    pub pending_paths: AtomicU64,
    pub errors: AtomicU64,
    /// When the queue first overflowed since the last loss report (µs).
    pub first_overflow_us: std::sync::atomic::AtomicI64,
    /// Nested ignore files not applied because of the rule budget.
    pub ignore_files_skipped: AtomicU64,
    /// Times the observer thread woke up (a notification batch or a
    /// deadline). Idle observation should leave this flat.
    pub loop_wakeups: AtomicU64,
    /// Notifications merged into a path already waiting in the queue (they
    /// used no queue slot and cannot overflow it). Counted in
    /// `fs.notifications` of the resulting observation.
    pub coalesced: AtomicU64,
    /// Notifications that never need the queue: access and attribute-only
    /// changes, which are not content changes.
    pub filtered: AtomicU64,
    /// Entries in the queue now, and the most there have been.
    pub queue_depth: AtomicU64,
    pub queue_peak: AtomicU64,
    /// OS callbacks running now. The loss re-check after a burst waits
    /// for this to reach 0, so an overflow counted late by a descheduled
    /// callback is still reported promptly.
    pub callbacks_in_flight: AtomicU64,
}

/// Decrements [`Stats::callbacks_in_flight`] when a callback returns.
struct InFlight<'a>(&'a AtomicU64);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        // SeqCst: the loop reads 0 here only after every counter update
        // this callback made (overflow) is visible to it.
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct Options {
    pub coalesce: Duration,
    pub max_directories: usize,
    /// Prefix for event ids (unique per daemon run).
    pub id_prefix: String,
    /// Notifications buffered between the OS watcher and the observer
    /// thread; beyond this they are dropped and counted as loss. `0` means
    /// the default (8,192). Small values are for fault-injection tests.
    pub queue_capacity: usize,
}

/// A running observer. Dropping it without [`Observer::stop`] leaves the
/// thread to end when the process exits.
pub struct Observer {
    tx: SyncSender<Msg>,
    thread: std::thread::JoinHandle<()>,
    pub stats: Arc<Stats>,
    pub backend: &'static str,
}

enum Msg {
    Fs(Event),
    /// Single-path notifications merged in the callback: the details are in
    /// the [`Inbox`] under this path.
    Path(PathBuf),
    Rescan,
    Stop,
}

/// What the callback merged for one queued path.
struct Merged {
    /// Distinct kinds, in arrival order (at most [`MAX_MERGED_KINDS`]).
    kinds: Vec<EventKind>,
    /// Notifications merged (at least 1).
    count: u64,
}

/// Paths that have a [`Msg::Path`] in the queue, with what has been merged
/// into it since. Holds at most one entry per queued message, so it is
/// bounded by the queue capacity. The callback inserts an entry only while
/// holding the lock across a successful send, and the loop removes it
/// under the same lock after receiving the message, so an entry never
/// outlives its message and a later notification is never merged into one
/// already processed.
type Inbox = Arc<std::sync::Mutex<HashMap<PathBuf, Merged>>>;

fn lock_inbox(inbox: &Inbox) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Merged>> {
    // A panic while holding the lock leaves a consistent map (single
    // insert/remove/update operations).
    inbox
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Notifications the observer never acts on (see `Loop::on_event`).
fn ignorable(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Access(_) | EventKind::Other | EventKind::Modify(ModifyKind::Metadata(_))
    )
}

/// A single-path notification whose handling depends only on its kind and
/// the path's state when it is processed, so several for one path can
/// share a queue entry. Renames are not merged (they pair paths).
fn mergeable(ev: &Event) -> Option<&Path> {
    let [path] = ev.paths.as_slice() else {
        return None;
    };
    match ev.kind {
        EventKind::Create(_)
        | EventKind::Remove(_)
        | EventKind::Any
        | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any | ModifyKind::Other) => {
            Some(path.as_path())
        }
        _ => None,
    }
}

/// Whether every path of `ev` lies under a built-in excluded directory of
/// `root` (`target/`, `node_modules/`, `.git/`, …). Such notifications are
/// never recorded; dropping them in the callback keeps a build or a package
/// install from filling the queue that real changes need.
fn under_builtin_exclusion(root: &Root, ev: &Event) -> bool {
    !ev.paths.is_empty()
        && ev.paths.iter().all(|p| {
            let rel = p
                .strip_prefix(&root.canonical)
                .or_else(|_| p.strip_prefix(&root.display));
            rel.is_ok_and(|rel| {
                rel.components().any(|c| {
                    matches!(c, Component::Normal(s)
                        if s.to_str().is_some_and(|s| BUILTIN_EXCLUDED_DIRS.contains(&s)))
                })
            })
        })
}

impl Observer {
    /// Start observing `root`. Returns once the watches are registered.
    ///
    /// # Errors
    /// The OS watcher could not be created or the root could not be
    /// watched.
    pub fn spawn(root: Root, rules: Rules, opts: Options, sink: impl Sink) -> notify::Result<Self> {
        let capacity = if opts.queue_capacity == 0 {
            QUEUE
        } else {
            opts.queue_capacity
        };
        let (tx, rx) = mpsc::sync_channel::<Msg>(capacity);
        let stats = Arc::new(Stats::default());
        let cb_tx = tx.clone();
        let cb_stats = stats.clone();
        let inbox: Inbox = Arc::default();
        let cb_inbox = inbox.clone();
        let cb_root = root.clone();
        let cap = u64::try_from(capacity).unwrap_or(u64::MAX);
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let st = &cb_stats;
            st.callbacks_in_flight.fetch_add(1, Ordering::SeqCst);
            let _in_flight = InFlight(&st.callbacks_in_flight);
            st.notifications.fetch_add(1, Ordering::Relaxed);
            // Queue a message; a full queue is counted as loss (exact).
            let send = |msg: Msg| -> bool {
                let depth = st.queue_depth.fetch_add(1, Ordering::Relaxed) + 1;
                match cb_tx.try_send(msg) {
                    Ok(()) => {
                        // The loop decrements after receiving, so `depth`
                        // can briefly read one above the capacity.
                        st.queue_peak.fetch_max(depth.min(cap), Ordering::Relaxed);
                        true
                    }
                    Err(e) => {
                        st.queue_depth.fetch_sub(1, Ordering::Relaxed);
                        if matches!(e, TrySendError::Full(_)) {
                            st.overflowed.fetch_add(1, Ordering::Relaxed);
                            let _first_wins = st.first_overflow_us.compare_exchange(
                                0,
                                Utc::now().timestamp_micros(),
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            );
                        }
                        false
                    }
                }
            };
            let ev = match res {
                Ok(ev) => ev,
                Err(_) => {
                    st.errors.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            };
            if ev.need_rescan() {
                send(Msg::Rescan);
                return;
            }
            if ignorable(ev.kind) {
                st.filtered.fetch_add(1, Ordering::Relaxed);
                return;
            }
            if under_builtin_exclusion(&cb_root, &ev) {
                st.excluded.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let Some(path) = mergeable(&ev) else {
                send(Msg::Fs(ev));
                return;
            };
            let mut inbox = lock_inbox(&cb_inbox);
            if let Some(m) = inbox.get_mut(path) {
                m.count = m.count.saturating_add(1);
                if !m.kinds.contains(&ev.kind) && m.kinds.len() < MAX_MERGED_KINDS {
                    m.kinds.push(ev.kind);
                }
                st.coalesced.fetch_add(1, Ordering::Relaxed);
                return;
            }
            // The lock is held across the send so the loop cannot receive
            // the message and look for the entry before it exists.
            if send(Msg::Path(path.to_path_buf())) {
                inbox.insert(
                    path.to_path_buf(),
                    Merged {
                        kinds: vec![ev.kind],
                        count: 1,
                    },
                );
            }
        })?;
        let mut rules = rules;
        let backend;
        let mut watched: HashSet<PathBuf> = HashSet::new();
        if cfg!(target_os = "linux") {
            backend = "inotify (per directory)";
            let (w, u) = walk_and_watch(
                &mut watcher,
                &root,
                &root.canonical,
                &mut rules,
                &mut watched,
                opts.max_directories,
            );
            stats.watched_dirs.store(w, Ordering::Relaxed);
            stats.unwatched_dirs.store(u, Ordering::Relaxed);
        } else {
            backend = if cfg!(target_os = "macos") {
                "fsevents (recursive)"
            } else if cfg!(windows) {
                "readdirectorychanges (recursive)"
            } else {
                "poll (recursive)"
            };
            watcher.watch(&root.canonical, RecursiveMode::Recursive)?;
            // Nested ignore files still apply when filtering.
            collect_nested_ignores(&root, &mut rules, opts.max_directories);
            stats.watched_dirs.store(1, Ordering::Relaxed);
        }
        stats
            .ignore_files_skipped
            .store(rules.ignore_files_skipped, Ordering::Relaxed);
        let thread_stats = stats.clone();
        let thread = std::thread::Builder::new()
            .name("tailflow-observe".into())
            .spawn(move || {
                let mut state = Loop {
                    root,
                    rules,
                    opts,
                    sink,
                    stats: thread_stats,
                    watcher,
                    watched,
                    pending: HashMap::new(),
                    order: VecDeque::new(),
                    next_id: 0,
                    overflow_reported: 0,
                    rescans_pending: 0,
                    loss_first: None,
                    next_flush: None,
                    inbox,
                };
                state.run(&rx);
            })
            .map_err(notify::Error::io)?;
        Ok(Self {
            tx,
            thread,
            stats,
            backend,
        })
    }

    /// Stop: pending observations are flushed first. Blocking.
    pub fn stop(self) {
        // A full queue is drained by the loop before it sees Stop.
        let _sent = self.tx.send(Msg::Stop);
        if self.thread.join().is_err() {
            tracing::warn!("filesystem observer thread panicked");
        }
    }
}

/// Register a watch for every directory under `start` (Linux), bounded.
/// Returns (watched, over budget).
fn walk_and_watch(
    watcher: &mut notify::RecommendedWatcher,
    root: &Root,
    start: &Path,
    rules: &mut Rules,
    watched: &mut HashSet<PathBuf>,
    budget: usize,
) -> (u64, u64) {
    let mut queue = VecDeque::from([start.to_path_buf()]);
    let mut unwatched = 0u64;
    while let Some(dir) = queue.pop_front() {
        let Some(display) = root.to_display(&dir) else {
            continue;
        };
        if display != root.display && rules.excluded(&display, true) {
            continue;
        }
        if watched.len() >= budget {
            unwatched = unwatched.saturating_add(1);
            continue;
        }
        if watched.contains(&dir) {
            continue;
        }
        if watcher.watch(&dir, RecursiveMode::NonRecursive).is_err() {
            unwatched = unwatched.saturating_add(1);
            continue;
        }
        watched.insert(dir.clone());
        rules.add_ignore_file(&display);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // `file_type` does not follow symlinks: a link to a directory
            // (possibly outside the project) is never descended into.
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                queue.push_back(entry.path());
            }
        }
    }
    (watched.len() as u64, unwatched)
}

/// Find nested `.gitignore` files (recursive backends), bounded.
fn collect_nested_ignores(root: &Root, rules: &mut Rules, budget: usize) {
    let mut queue = VecDeque::from([root.display.clone()]);
    let mut seen = 0usize;
    while let Some(dir) = queue.pop_front() {
        seen += 1;
        if seen > budget {
            break;
        }
        if dir != root.display && rules.excluded(&dir, true) {
            continue;
        }
        rules.add_ignore_file(&dir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                queue.push_back(entry.path());
            }
        }
    }
}

#[derive(Default)]
struct Pending {
    first: Option<Instant>,
    first_at: Option<DateTime<Utc>>,
    last: Option<Instant>,
    create: bool,
    modify: bool,
    remove: bool,
    rename_to: bool,
    rename_from: Option<PathBuf>,
    count: u64,
}

struct Loop<S: Sink> {
    root: Root,
    rules: Rules,
    opts: Options,
    sink: S,
    stats: Arc<Stats>,
    watcher: notify::RecommendedWatcher,
    watched: HashSet<PathBuf>,
    pending: HashMap<PathBuf, Pending>,
    order: VecDeque<PathBuf>,
    next_id: u64,
    overflow_reported: u64,
    rescans_pending: u64,
    loss_first: Option<DateTime<Utc>>,
    /// When the next flush pass is due (`None`: nothing pending).
    next_flush: Option<Instant>,
    inbox: Inbox,
}

impl<S: Sink> Loop<S> {
    /// The observer thread. Event-driven: with nothing pending it blocks
    /// until the OS reports a change or [`Observer::stop`] is called, so an
    /// idle observed project costs no wakeups. It wakes on a timer only
    /// while there is work with a deadline:
    /// - a pending path's coalescing deadline (quiet time, or
    ///   [`MAX_HOLD`]), checked no more often than every coalesce/4;
    /// - unreported loss, reported at most every [`LOSS_REPORT_EVERY`];
    /// - once, [`LOSS_SETTLE`] after the last notification, to catch a
    ///   queue overflow the callback counted just after the queue drained.
    fn run(&mut self, rx: &mpsc::Receiver<Msg>) {
        let granule =
            (self.opts.coalesce / 4).clamp(Duration::from_millis(5), Duration::from_millis(50));
        let mut last_loss_flush: Option<Instant> = None;
        let mut settle_at: Option<Instant> = None;
        loop {
            let loss_due = self
                .loss_pending()
                .then(|| last_loss_flush.map_or_else(Instant::now, |t| t + LOSS_REPORT_EVERY));
            let deadline = [self.next_flush, settle_at, loss_due]
                .into_iter()
                .flatten()
                .min();
            let got = match deadline {
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(d) => rx.recv_timeout(d.saturating_duration_since(Instant::now())),
            };
            self.stats.loop_wakeups.fetch_add(1, Ordering::Relaxed);
            let mut next = match got {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => Some(Msg::Stop),
            };
            // Drain what is already queued before looking at deadlines: one
            // flush pass per batch, not per notification.
            let mut handled = 0usize;
            while let Some(msg) = next.take() {
                if !matches!(msg, Msg::Stop) {
                    self.stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
                }
                match msg {
                    Msg::Fs(ev) => self.on_event(ev),
                    Msg::Path(path) => self.on_merged(path),
                    Msg::Rescan => {
                        self.stats.rescans.fetch_add(1, Ordering::Relaxed);
                        self.rescans_pending = self.rescans_pending.saturating_add(1);
                        self.loss_first.get_or_insert_with(Utc::now);
                    }
                    Msg::Stop => {
                        self.flush(true, granule);
                        self.flush_loss();
                        return;
                    }
                }
                handled += 1;
                if handled < DRAIN_BATCH {
                    next = rx.try_recv().ok();
                }
            }
            let now = Instant::now();
            if handled > 0 {
                settle_at = Some(now + LOSS_SETTLE);
            } else if settle_at.is_some_and(|t| now >= t) {
                // A callback still running may count an overflow after
                // this check: look again later rather than block for good.
                settle_at = (self.stats.callbacks_in_flight.load(Ordering::SeqCst) > 0)
                    .then(|| now + LOSS_SETTLE);
            }
            if self.next_flush.is_some_and(|t| now >= t) {
                self.flush(false, granule);
            }
            if self.loss_pending() && last_loss_flush.is_none_or(|t| now >= t + LOSS_REPORT_EVERY) {
                self.flush_loss();
                last_loss_flush = Some(now);
            }
        }
    }

    /// Overflow or rescans not yet reported to the sink.
    fn loss_pending(&self) -> bool {
        self.rescans_pending > 0
            || self.stats.overflowed.load(Ordering::Relaxed) > self.overflow_reported
    }

    fn flush_loss(&mut self) {
        let overflowed = self.stats.overflowed.load(Ordering::Relaxed);
        let new_overflow = overflowed.saturating_sub(self.overflow_reported);
        if new_overflow == 0 && self.rescans_pending == 0 {
            return;
        }
        let now = Utc::now();
        let overflow_at = self.stats.first_overflow_us.swap(0, Ordering::Relaxed);
        let first = [
            self.loss_first.take(),
            (overflow_at > 0)
                .then(|| DateTime::from_timestamp_micros(overflow_at))
                .flatten(),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(now);
        if new_overflow > 0 {
            self.overflow_reported = overflowed;
            self.sink.lost(false, new_overflow, first, now);
        }
        if self.rescans_pending > 0 {
            self.sink.lost(true, self.rescans_pending, first, now);
            self.rescans_pending = 0;
        }
    }

    /// A directory appeared under the root (created, moved in, renamed):
    /// watch it (Linux) and report the files already in it. Without this,
    /// files written right after `mkdir`, or a whole tree moved in, would
    /// never be observed while coverage still claimed to be complete.
    fn new_directory(&mut self, dir: &Path) {
        if cfg!(target_os = "linux") {
            let budget = self.opts.max_directories;
            let (_, unwatched) = walk_and_watch(
                &mut self.watcher,
                &self.root,
                dir,
                &mut self.rules,
                &mut self.watched,
                budget,
            );
            let w = self.watched.len() as u64;
            let u = self
                .stats
                .unwatched_dirs
                .load(Ordering::Relaxed)
                .saturating_add(unwatched);
            self.stats.watched_dirs.store(w, Ordering::Relaxed);
            self.stats.unwatched_dirs.store(u, Ordering::Relaxed);
            self.sink.coverage(w, u);
        }
        let mut queue = VecDeque::from([dir.to_path_buf()]);
        let mut files = 0usize;
        while let Some(d) = queue.pop_front() {
            let Some(display) = self.root.to_display(&d) else {
                continue;
            };
            if self.rules.excluded(&display, true) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(t) = entry.file_type() else { continue };
                if t.is_dir() {
                    queue.push_back(entry.path());
                } else if files < MAX_NEW_DIR_FILES {
                    files += 1;
                    if let Some(f) = self.root.to_display(&entry.path()) {
                        self.note(&f, |p| p.create = true);
                    }
                } else {
                    // Too many to list: say that changes may be missing.
                    self.rescans_pending = self.rescans_pending.saturating_add(1);
                    self.loss_first.get_or_insert_with(Utc::now);
                    return;
                }
            }
        }
    }

    fn sync_ignore_stats(&self) {
        self.stats
            .ignore_files_skipped
            .store(self.rules.ignore_files_skipped, Ordering::Relaxed);
    }

    /// Forget watches of a directory that was removed or renamed away.
    fn forget_directory(&mut self, dir: &Path) {
        self.watched.retain(|w| !w.starts_with(dir));
        self.stats
            .watched_dirs
            .store(self.watched.len() as u64, Ordering::Relaxed);
    }

    /// Replay what the callback merged for one path: removals first, then
    /// creations, then modifications. Every decision below depends only on
    /// the kind and on the path's state now (`lstat`), and the coalesced
    /// observation is decided by the final state, so the replay order
    /// cannot change the result.
    fn on_merged(&mut self, path: PathBuf) {
        let Some(mut m) = lock_inbox(&self.inbox).remove(&path) else {
            return;
        };
        let rank = |k: &EventKind| match k {
            EventKind::Remove(_) => 0,
            EventKind::Create(_) => 1,
            _ => 2,
        };
        m.kinds.sort_by_key(rank);
        // Each replayed kind counts once in `note`; add the rest.
        let replayed = u64::try_from(m.kinds.len()).unwrap_or(u64::MAX);
        let extra = m.count.saturating_sub(replayed);
        for kind in m.kinds {
            self.on_event(Event::new(kind).add_path(path.clone()));
        }
        // fs.notifications counts every merged notification.
        if extra > 0 {
            if let Some(p) = self
                .root
                .to_display(&path)
                .and_then(|d| self.pending.get_mut(&d))
            {
                p.count = p.count.saturating_add(extra);
            }
        }
    }

    fn on_event(&mut self, ev: Event) {
        let kind = ev.kind;
        let appeared = matches!(
            kind,
            EventKind::Create(_)
                | EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Any))
        );
        if appeared {
            let dirs: Vec<PathBuf> = ev
                .paths
                .iter()
                .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
                .cloned()
                .collect();
            for d in &dirs {
                self.new_directory(d);
            }
            self.sync_ignore_stats();
            if !dirs.is_empty() && dirs.len() == ev.paths.len() {
                return;
            }
        }
        if let EventKind::Modify(ModifyKind::Name(RenameMode::Both)) = kind {
            if let [from, to] = ev.paths.as_slice() {
                if std::fs::symlink_metadata(to).is_ok_and(|m| m.is_dir()) {
                    self.forget_directory(from);
                    if self.root.to_display(to).is_some() {
                        self.new_directory(to);
                    }
                    return;
                }
            }
        }
        if matches!(
            kind,
            EventKind::Remove(notify::event::RemoveKind::Folder)
                | EventKind::Modify(ModifyKind::Name(RenameMode::From))
        ) {
            for p in &ev.paths {
                if self.watched.contains(p) {
                    self.forget_directory(p);
                }
            }
            if matches!(kind, EventKind::Remove(notify::event::RemoveKind::Folder)) {
                return;
            }
        }
        let (create, modify, remove, rename_to, rename_from) = match kind {
            EventKind::Create(_) => (true, false, false, false, false),
            EventKind::Remove(_) => (false, false, true, false, false),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                // [from, to]
                if let [from, to] = ev.paths.as_slice() {
                    let from_d = self.root.to_display(from);
                    let to_d = self.root.to_display(to);
                    // An atomic save (an excluded temporary file renamed over
                    // the real one) is a modification of the real file.
                    let from_d = from_d.filter(|f| !self.rules.excluded(f, false));
                    if let Some(to_d) = to_d {
                        let from_known = from_d.is_some();
                        self.note(&to_d, |p| {
                            if from_known {
                                p.rename_to = true;
                                p.rename_from = from_d.clone();
                            } else {
                                p.modify = true;
                            }
                        });
                    } else if let Some(from_d) = from_d {
                        // Moved out of the root: gone from here.
                        self.note(&from_d, |p| p.remove = true);
                    }
                }
                return;
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                (false, false, false, false, true)
            }
            EventKind::Modify(ModifyKind::Name(_)) => (false, false, false, true, false),
            // Attribute-only changes (chmod, touch without writing) are not
            // content changes.
            EventKind::Modify(ModifyKind::Metadata(_)) => return,
            EventKind::Modify(_) => (false, true, false, false, false),
            EventKind::Access(_) | EventKind::Other => return,
            EventKind::Any => (false, true, false, false, false),
        };
        for p in &ev.paths {
            let Some(d) = self.root.to_display(p) else {
                continue;
            };
            self.note(&d, |e| {
                e.create |= create;
                e.modify |= modify;
                e.remove |= remove || rename_from;
                e.rename_to |= rename_to;
            });
        }
    }

    fn note(&mut self, path: &Path, f: impl FnOnce(&mut Pending)) {
        if path == self.root.display {
            return;
        }
        // A path already pending passed these checks when it was added
        // (the final `lstat` decides its type when it is reported), so a
        // burst of notifications for one path costs one `lstat` and one
        // rule check, not one per notification.
        if !self.pending.contains_key(path) {
            // Directories are not observations (their files are).
            let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
            if is_dir {
                return;
            }
            if self.rules.excluded(path, false) {
                self.stats.excluded.fetch_add(1, Ordering::Relaxed);
                return;
            }
            if self.pending.len() >= MAX_PENDING {
                // Report the oldest now rather than drop anything.
                if let Some(oldest) = self.order.pop_front() {
                    if let Some(p) = self.pending.remove(&oldest) {
                        self.emit(&oldest, p);
                    }
                }
            }
            self.order.push_back(path.to_path_buf());
        }
        let now = Instant::now();
        let quiet = self.opts.coalesce;
        let entry = self.pending.entry(path.to_path_buf()).or_default();
        let first = *entry.first.get_or_insert(now);
        entry.first_at.get_or_insert_with(Utc::now);
        entry.last = Some(now);
        entry.count = entry.count.saturating_add(1);
        f(entry);
        // This path is due no earlier than this. `next_flush` may end up
        // earlier than the true earliest deadline (a later notification
        // postponed it); that costs one flush pass that finds the exact one.
        let due = (now + quiet).min(first + MAX_HOLD);
        self.next_flush = Some(self.next_flush.map_or(due, |t| t.min(due)));
        self.stats
            .pending_paths
            .store(self.pending.len() as u64, Ordering::Relaxed);
    }

    /// Report paths whose notifications have settled (all of them when
    /// `all`), and schedule the next pass at the earliest remaining
    /// deadline, at least `granule` from now so a stream of distinct paths
    /// costs a bounded number of passes rather than one per path.
    fn flush(&mut self, all: bool, granule: Duration) {
        let now = Instant::now();
        let quiet = self.opts.coalesce;
        let mut keep = VecDeque::with_capacity(self.order.len());
        let mut earliest: Option<Instant> = None;
        while let Some(path) = self.order.pop_front() {
            let due = self.pending.get(&path).map(|p| {
                let by_quiet = p.last.map(|l| l + quiet);
                let by_hold = p.first.map(|f| f + MAX_HOLD);
                [by_quiet, by_hold].into_iter().flatten().min()
            });
            let ready = all || due.flatten().is_none_or(|d| now >= d);
            if ready {
                if let Some(p) = self.pending.remove(&path) {
                    self.emit(&path, p);
                }
            } else {
                if let Some(d) = due.flatten() {
                    earliest = Some(earliest.map_or(d, |e| e.min(d)));
                }
                keep.push_back(path);
            }
        }
        self.order = keep;
        self.next_flush = earliest.map(|e| e.max(now + granule));
        self.stats
            .pending_paths
            .store(self.pending.len() as u64, Ordering::Relaxed);
    }

    fn emit(&mut self, path: &Path, p: Pending) {
        // The final state decides the type: the notifications only say
        // something happened (FSEvents coalesces flags, renames arrive
        // unpaired). Size and kind come from `lstat`, never the contents.
        let meta = std::fs::symlink_metadata(path).ok();
        let exists = meta.is_some();
        let (ty, transient) = match (exists, p.create, p.rename_to, &p.rename_from) {
            (false, created, ..) => ("file.deleted", created),
            (true, _, true, Some(_)) => ("file.renamed", false),
            (true, true, _, _) => ("file.created", false),
            (true, _, true, None) => ("file.created", false),
            (true, false, false, _) => ("file.modified", false),
        };
        self.next_id = self.next_id.wrapping_add(1);
        let at = p.first_at.unwrap_or_else(Utc::now);
        let mut e = ObservabilityEvent::new(
            format!("{}:{}", self.opts.id_prefix, self.next_id),
            ty,
            Provenance::FilesystemObserved,
            SOURCE_TYPE,
            format!("fs:{}", self.root.display.display()),
            at,
        );
        e.path = Some(path.display().to_string());
        e.project_id = Some(self.root.display.display().to_string());
        e.severity = crate::LogLevel::Info;
        let rel = path
            .strip_prefix(&self.root.display)
            .map_or_else(|_| path.display().to_string(), |r| r.display().to_string());
        e.message = Some(match ty {
            "file.deleted" if transient => format!("{rel} created and removed"),
            "file.deleted" => format!("{rel} deleted"),
            "file.created" => format!("{rel} created"),
            "file.renamed" => format!("{rel} renamed"),
            _ => format!("{rel} modified"),
        });
        let a = &mut e.attributes;
        a.insert(
            "fs.root".into(),
            Value::from(self.root.display.display().to_string()),
        );
        a.insert("fs.notifications".into(), Value::from(p.count));
        let mut kinds = Vec::new();
        for (on, k) in [
            (p.create, "create"),
            (p.modify, "modify"),
            (p.remove, "remove"),
            (p.rename_to, "rename"),
        ] {
            if on {
                kinds.push(Value::from(k));
            }
        }
        a.insert("fs.kinds".into(), Value::Array(kinds));
        if transient {
            a.insert("fs.transient".into(), Value::Bool(true));
        }
        if let Some(m) = &meta {
            if m.is_file() {
                a.insert("fs.size".into(), Value::from(m.len()));
            }
            if m.file_type().is_symlink() {
                a.insert("fs.symlink".into(), Value::Bool(true));
            }
        }
        if let Some(from) = &p.rename_from {
            a.insert(
                "file.renamed_from".into(),
                Value::from(from.display().to_string()),
            );
        }
        self.stats.observations.fetch_add(1, Ordering::Relaxed);
        self.sink.observed(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Collect {
        events: Arc<Mutex<Vec<ObservabilityEvent>>>,
        lost: Arc<Mutex<Vec<(bool, u64)>>>,
    }

    impl Sink for Collect {
        fn observed(&self, e: ObservabilityEvent) {
            self.events.lock().unwrap().push(e);
        }
        fn lost(&self, rescan: bool, count: u64, _: DateTime<Utc>, _: DateTime<Utc>) {
            self.lost.lock().unwrap().push((rescan, count));
        }
        fn coverage(&self, _: u64, _: u64) {}
    }

    fn start(dir: &Path, cfg: &ObserveConfig) -> (Observer, Collect, Root) {
        let root = Root::resolve(dir, Path::new("/"), None).unwrap();
        let rules = Rules::new(&root.display, cfg, Vec::new());
        let sink = Collect::default();
        let obs = Observer::spawn(
            root.clone(),
            rules,
            Options {
                coalesce: Duration::from_millis(100),
                max_directories: cfg.max_directories,
                id_prefix: "t".into(),
                queue_capacity: 0,
            },
            sink.clone(),
        )
        .unwrap();
        // Let the OS watcher settle (FSEvents starts slightly delayed).
        std::thread::sleep(Duration::from_millis(400));
        (obs, sink, root)
    }

    fn wait_for(
        sink: &Collect,
        pred: impl Fn(&[ObservabilityEvent]) -> bool,
    ) -> Vec<ObservabilityEvent> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let ev = sink.events.lock().unwrap().clone();
            if pred(&ev) || Instant::now() > deadline {
                return ev;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Events whose path ends with `suffix` (`/`-separated; Windows paths
    /// are compared with `\\` turned into `/`).
    fn by_path<'a>(ev: &'a [ObservabilityEvent], suffix: &str) -> Vec<&'a ObservabilityEvent> {
        ev.iter()
            .filter(|e| {
                e.path
                    .as_deref()
                    .is_some_and(|p| p.replace('\\', "/").ends_with(suffix))
            })
            .collect()
    }

    #[test]
    fn verbatim_windows_paths_are_simplified() {
        assert_eq!(
            simplify_verbatim(PathBuf::from(r"\\?\C:\Users\a")),
            PathBuf::from(r"C:\Users\a")
        );
        assert_eq!(
            simplify_verbatim(PathBuf::from(r"\\?\UNC\srv\share\x")),
            PathBuf::from(r"\\srv\share\x")
        );
        assert_eq!(
            simplify_verbatim(PathBuf::from(r"\\?\Volume{abc}\x")),
            PathBuf::from(r"\\?\Volume{abc}\x")
        );
        assert_eq!(
            simplify_verbatim(PathBuf::from("/tmp/x")),
            PathBuf::from("/tmp/x")
        );
    }

    #[test]
    fn rules_cover_builtins_temp_files_gitignore_and_globs() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::write(r.join(".gitignore"), "*.log\n/dist/\n").unwrap();
        std::fs::create_dir_all(r.join("pkg")).unwrap();
        std::fs::write(r.join("pkg/.gitignore"), "local.txt\n").unwrap();
        let cfg = ObserveConfig {
            exclude: vec!["secrets/**".into()],
            ..ObserveConfig::default()
        };
        let root = Root::resolve(r, Path::new("/"), None).unwrap();
        let mut rules = Rules::new(&root.display, &cfg, vec![root.display.join("data")]);
        rules.add_ignore_file(&root.display.join("pkg"));
        let x = |p: &str| rules.excluded(&root.display.join(p), false);
        assert!(x("node_modules/a/b.js"));
        assert!(x(".git/HEAD"));
        assert!(x("target/debug/x"));
        assert!(x("src/.main.rs.swp"));
        assert!(x("src/main.rs~"));
        assert!(x("app.log"));
        assert!(x("dist/bundle.js"));
        assert!(x("secrets/key.pem"));
        assert!(x("pkg/local.txt"));
        assert!(x("data/events.db"), "TailFlow data dir");
        assert!(!x("src/main.rs"));
        assert!(!x("local.txt"), "nested rule applies below its dir only");
        assert!(
            rules.excluded(Path::new("/elsewhere/a"), false),
            "outside root"
        );
    }

    #[test]
    fn roots_must_be_specific_directories() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Root::resolve(Path::new("/"), Path::new("/"), None),
            Err(RootError::TooBroad(_))
        ));
        let f = dir.path().join("file");
        std::fs::write(&f, "x").unwrap();
        assert!(matches!(
            Root::resolve(&f, Path::new("/"), None),
            Err(RootError::NotADirectory(_))
        ));
        let data = dir.path().join("data");
        std::fs::create_dir_all(data.join("proj")).unwrap();
        assert!(matches!(
            Root::resolve(&data.join("proj"), Path::new("/"), Some(&data)),
            Err(RootError::DataDirectory(_))
        ));
        let ok = Root::resolve(Path::new("proj"), &data, None).unwrap();
        assert!(ok.display.ends_with(Path::new("data").join("proj")));
        assert!(
            !ok.canonical.to_string_lossy().starts_with(r"\\?\"),
            "no verbatim prefix"
        );
    }

    #[test]
    fn observes_create_modify_delete_and_coalesces_bursts() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, sink, _root) = start(dir.path(), &ObserveConfig::default());
        let f = dir.path().join("a.rs");
        std::fs::write(&f, "1").unwrap();
        let ev = wait_for(&sink, |e| !by_path(e, "/a.rs").is_empty());
        let first = by_path(&ev, "/a.rs");
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(first[0].event_type, "file.created");
        assert_eq!(first[0].provenance, Provenance::FilesystemObserved);
        assert_eq!(first[0].source_type, SOURCE_TYPE);
        assert_eq!(first[0].attributes["fs.size"], 1);
        // A burst of writes is one observation.
        std::thread::sleep(Duration::from_millis(300));
        let burst = Instant::now();
        for i in 0..50 {
            std::fs::write(&f, format!("{i}")).unwrap();
        }
        let took = burst.elapsed();
        let ev = wait_for(&sink, |e| by_path(e, "/a.rs").len() >= 2);
        std::thread::sleep(Duration::from_millis(500));
        let ev2 = sink.events.lock().unwrap().clone();
        let writes = by_path(&ev2, "/a.rs").len() - 1;
        // A burst within one quiet period is one observation (allow a
        // split at the edges). A loaded runner can stretch 50 writes over
        // seconds: then at most one observation per quiet period.
        let coalesce = ObserveConfig::default().coalesce_ms;
        let periods =
            usize::try_from(took.as_millis() / u128::from(coalesce)).unwrap_or(usize::MAX);
        let bound = periods.saturating_add(3).min(49);
        assert!(
            (1..=bound).contains(&writes),
            "burst coalesced: {writes} observations for 50 writes in {took:?} ({ev:?})"
        );
        // Contents never recorded.
        let text = serde_json::to_string(&ev2).unwrap();
        assert!(!text.contains("\"49\""), "no contents");
        std::fs::remove_file(&f).unwrap();
        let ev = wait_for(&sink, |e| {
            by_path(e, "/a.rs")
                .iter()
                .any(|x| x.event_type == "file.deleted")
        });
        assert!(by_path(&ev, "/a.rs")
            .iter()
            .any(|x| x.event_type == "file.deleted"));
        obs.stop();
    }

    #[test]
    fn excluded_and_temporary_files_are_not_observed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/x")).unwrap();
        let (obs, sink, _root) = start(dir.path(), &ObserveConfig::default());
        std::fs::write(dir.path().join("debug.log"), "x").unwrap();
        std::fs::write(dir.path().join("node_modules/x/i.js"), "x").unwrap();
        std::fs::write(dir.path().join(".main.rs.swp"), "x").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "x").unwrap();
        let ev = wait_for(&sink, |e| !by_path(e, "/kept.rs").is_empty());
        std::thread::sleep(Duration::from_millis(400));
        let ev = if ev.is_empty() {
            ev
        } else {
            sink.events.lock().unwrap().clone()
        };
        let paths: Vec<_> = ev
            .iter()
            .filter_map(|e| e.path.as_deref().map(|p| p.replace('\\', "/")))
            .collect();
        assert!(paths.iter().any(|p| p.ends_with("/kept.rs")), "{paths:?}");
        assert!(
            !paths
                .iter()
                .any(|p| p.ends_with(".log") || p.contains("node_modules") || p.ends_with(".swp")),
            "{paths:?}"
        );
        assert!(obs.stats.excluded.load(Ordering::Relaxed) > 0);
        obs.stop();
    }

    #[test]
    fn atomic_replace_is_one_change_to_the_real_path() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("config.json");
        std::fs::write(&f, "{}").unwrap();
        let (obs, sink, _root) = start(dir.path(), &ObserveConfig::default());
        // Editor-style save: write a temp file, rename over the original.
        let tmp = dir.path().join("config.json.tmp.123");
        std::fs::write(&tmp, "{\"a\":1}").unwrap();
        std::fs::rename(&tmp, &f).unwrap();
        let ev = wait_for(&sink, |e| !by_path(e, "/config.json").is_empty());
        let real = by_path(&ev, "/config.json");
        assert!(!real.is_empty(), "{ev:?}");
        assert!(
            matches!(
                real[0].event_type.as_str(),
                "file.modified" | "file.created" | "file.renamed"
            ),
            "{real:?}"
        );
        assert!(by_path(&ev, ".tmp.123").is_empty(), "temp file excluded");
        obs.stop();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_outside_the_root_are_not_followed() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let (obs, sink, _root) = start(dir.path(), &ObserveConfig::default());
        std::fs::write(outside.path().join("secret.txt"), "x").unwrap();
        std::fs::write(dir.path().join("inside.txt"), "x").unwrap();
        let ev = wait_for(&sink, |e| !by_path(e, "/inside.txt").is_empty());
        std::thread::sleep(Duration::from_millis(300));
        let ev = if ev.is_empty() {
            ev
        } else {
            sink.events.lock().unwrap().clone()
        };
        assert!(by_path(&ev, "secret.txt").is_empty(), "{ev:?}");
        obs.stop();
    }

    #[test]
    fn a_rename_is_seen_on_both_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.rs"), "x").unwrap();
        let (obs, sink, _root) = start(dir.path(), &ObserveConfig::default());
        std::fs::rename(dir.path().join("old.rs"), dir.path().join("new.rs")).unwrap();
        let ev = wait_for(&sink, |e| {
            !by_path(e, "/new.rs").is_empty() && !by_path(e, "/old.rs").is_empty()
        });
        let new = by_path(&ev, "/new.rs");
        assert!(
            new.iter()
                .any(|e| matches!(e.event_type.as_str(), "file.renamed" | "file.created")),
            "{ev:?}"
        );
        assert!(
            by_path(&ev, "/old.rs")
                .iter()
                .any(|e| e.event_type == "file.deleted"),
            "{ev:?}"
        );
        obs.stop();
    }

    #[test]
    fn removing_the_observed_directory_does_not_break_the_observer() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "x").unwrap();
        let (obs, _sink, _root) = start(&root, &ObserveConfig::default());
        std::fs::remove_dir_all(&root).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        // Stops promptly and cleanly even though the root is gone.
        let t = Instant::now();
        obs.stop();
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_directory_moved_in_is_watched_and_its_files_reported() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let src = outside.path().join("pkg");
        std::fs::create_dir_all(src.join("deep")).unwrap();
        std::fs::write(src.join("a.rs"), "x").unwrap();
        std::fs::write(src.join("deep/b.rs"), "x").unwrap();
        let (obs, sink, _root) = start(dir.path(), &ObserveConfig::default());
        std::fs::rename(&src, dir.path().join("pkg")).unwrap();
        let ev = wait_for(&sink, |e| {
            !by_path(e, "/pkg/a.rs").is_empty() && !by_path(e, "/pkg/deep/b.rs").is_empty()
        });
        assert!(!by_path(&ev, "/pkg/deep/b.rs").is_empty(), "{ev:?}");
        // And later changes inside it are observed too.
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(dir.path().join("pkg/deep/c.rs"), "x").unwrap();
        let ev = wait_for(&sink, |e| !by_path(e, "/pkg/deep/c.rs").is_empty());
        assert!(!by_path(&ev, "/pkg/deep/c.rs").is_empty(), "{ev:?}");
        obs.stop();
    }

    #[test]
    fn a_file_written_right_after_mkdir_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, sink, _root) = start(dir.path(), &ObserveConfig::default());
        std::fs::create_dir_all(dir.path().join("new/nested")).unwrap();
        std::fs::write(dir.path().join("new/nested/mod.ts"), "x").unwrap();
        let ev = wait_for(&sink, |e| !by_path(e, "/new/nested/mod.ts").is_empty());
        assert!(!by_path(&ev, "/new/nested/mod.ts").is_empty(), "{ev:?}");
        obs.stop();
    }

    #[test]
    fn ignore_rules_are_bounded_in_total() {
        let dir = tempfile::tempdir().unwrap();
        let rules_text: String = (0..1_500).map(|i| format!("gen{i}.out\n")).collect();
        for i in 0..20 {
            let d = dir.path().join(format!("d{i}"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(".gitignore"), &rules_text).unwrap();
        }
        let root = Root::resolve(dir.path(), Path::new("/"), None).unwrap();
        let mut rules = Rules::new(&root.display, &ObserveConfig::default(), Vec::new());
        for i in 0..20 {
            let d = root.display.join(format!("d{i}"));
            rules.add_ignore_file(&d);
            rules.add_ignore_file(&d); // once per directory
        }
        assert!(rules.ignore_rules <= MAX_TOTAL_IGNORE_RULES);
        assert_eq!(
            rules.ignore_files_skipped,
            20 - (MAX_TOTAL_IGNORE_RULES / 1_500) as u64
        );
    }

    /// A sink that blocks the observer thread on every observation, like
    /// a storage writer that has stalled.
    #[derive(Clone, Default)]
    struct Slow(Collect);

    impl Sink for Slow {
        fn observed(&self, e: ObservabilityEvent) {
            std::thread::sleep(Duration::from_millis(30));
            self.0.observed(e);
        }
        fn lost(&self, rescan: bool, count: u64, a: DateTime<Utc>, b: DateTime<Utc>) {
            self.0.lost(rescan, count, a, b);
        }
        fn coverage(&self, _: u64, _: u64) {}
    }

    /// A sink that blocks until released: the observer thread stalls in
    /// its first report, so everything after it waits in the queue.
    #[derive(Clone, Default)]
    struct Gate {
        inner: Collect,
        open: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    impl Gate {
        fn release(&self) {
            let (m, cv) = &*self.open;
            *m.lock().unwrap() = true;
            cv.notify_all();
        }
    }

    impl Sink for Gate {
        fn observed(&self, e: ObservabilityEvent) {
            let (m, cv) = &*self.open;
            let mut open = m.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
            drop(open);
            self.inner.observed(e);
        }
        fn lost(&self, rescan: bool, count: u64, a: DateTime<Utc>, b: DateTime<Utc>) {
            self.inner.lost(rescan, count, a, b);
        }
        fn coverage(&self, _: u64, _: u64) {}
    }

    /// Start an observer with a tiny queue and stall it in its first
    /// report (of `first.txt`).
    fn stalled(dir: &Path, capacity: usize) -> (Observer, Gate) {
        let cfg = ObserveConfig::default();
        let root = Root::resolve(dir, Path::new("/"), None).unwrap();
        let gate = Gate::default();
        let obs = Observer::spawn(
            root.clone(),
            Rules::new(&root.display, &cfg, Vec::new()),
            Options {
                coalesce: Duration::from_millis(20),
                max_directories: 100,
                id_prefix: "t".into(),
                queue_capacity: capacity,
            },
            gate.clone(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(400));
        std::fs::write(dir.join("first.txt"), "x").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        // `observations` is counted just before the (blocking) report.
        while obs.stats.observations.load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "the observer never reported");
            std::thread::sleep(Duration::from_millis(10));
        }
        (obs, gate)
    }

    /// M4-F: notifications for a path already waiting in the queue are
    /// merged into its entry. M4 queued each one, so a stalled observer
    /// lost them once the queue was full.
    #[test]
    fn notifications_for_one_path_share_a_queue_slot() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, gate) = stalled(dir.path(), 4);
        for i in 0..500 {
            std::fs::write(dir.path().join("busy.log"), format!("{i}")).unwrap();
        }
        std::thread::sleep(Duration::from_millis(500));
        let stats = obs.stats.clone();
        gate.release();
        let ev = wait_for(&gate.inner, |ev| !by_path(ev, "/busy.log").is_empty());
        obs.stop();
        assert_eq!(stats.overflowed.load(Ordering::Relaxed), 0);
        assert!(stats.coalesced.load(Ordering::Relaxed) > 0);
        assert!(stats.queue_peak.load(Ordering::Relaxed) <= 4);
        let busy = by_path(&ev, "/busy.log");
        let notes: u64 = busy
            .iter()
            .filter_map(|e| e.attributes.get("fs.notifications")?.as_u64())
            .sum();
        // Merged notifications still count in the observation.
        assert!(notes > 1, "{busy:?}");
        assert!(gate.inner.lost.lock().unwrap().is_empty());
    }

    /// M4-F: a build writing under `target/` or a package install under
    /// `node_modules/` never takes queue slots from real changes.
    #[test]
    fn builtin_excluded_directories_never_use_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        for d in ["target/debug", "node_modules/pkg"] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        let (obs, gate) = stalled(dir.path(), 4);
        for i in 0..300 {
            std::fs::write(dir.path().join(format!("target/debug/o{i}")), "x").unwrap();
            std::fs::write(dir.path().join(format!("node_modules/pkg/m{i}.js")), "x").unwrap();
        }
        std::thread::sleep(Duration::from_millis(500));
        let stats = obs.stats.clone();
        gate.release();
        std::fs::write(dir.path().join("real.rs"), "x").unwrap();
        let ev = wait_for(&gate.inner, |ev| !by_path(ev, "/real.rs").is_empty());
        obs.stop();
        assert_eq!(stats.overflowed.load(Ordering::Relaxed), 0);
        // Recursive backends report the writes and the callback drops them
        // as excluded. inotify never watches excluded directories, so on
        // Linux the writes produce no notification at all.
        if !cfg!(target_os = "linux") {
            assert!(stats.excluded.load(Ordering::Relaxed) >= 300);
        }
        assert_eq!(by_path(&ev, "/real.rs").len(), 1);
        assert!(ev.iter().all(|e| {
            let p = e.path.as_deref().unwrap_or("").replace('\\', "/");
            !p.contains("/target/") && !p.contains("/node_modules/")
        }));
    }

    /// M4-F: attribute-only changes are dropped in the callback (the
    /// observer never acted on them) and, where the OS reports them as
    /// such (inotify), produce no observation.
    #[cfg(unix)]
    #[test]
    fn attribute_changes_are_filtered_before_the_queue() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("mode.sh");
        std::fs::write(&f, "x").unwrap();
        let (obs, sink, _root) = start(dir.path(), &ObserveConfig::default());
        for i in 0..20 {
            let mode = if i % 2 == 0 { 0o755 } else { 0o644 };
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(mode)).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::write(dir.path().join("after.rs"), "x").unwrap();
        let ev = wait_for(&sink, |ev| !by_path(ev, "/after.rs").is_empty());
        let stats = obs.stats.clone();
        obs.stop();
        assert!(stats.filtered.load(Ordering::Relaxed) > 0);
        // inotify reports a chmod as an attribute change only. FSEvents
        // repeats a recently created file's sticky "created"/"modified"
        // flags on later events, so on macOS a chmod of a new file can still
        // be reported as a change (a documented limitation, as in M4).
        if cfg!(target_os = "linux") {
            assert!(by_path(&ev, "/mode.sh").is_empty(), "{ev:?}");
        }
    }

    #[test]
    fn fault_a_full_queue_is_reported_as_counted_loss() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ObserveConfig::default();
        let root = Root::resolve(dir.path(), Path::new("/"), None).unwrap();
        let sink = Slow::default();
        let obs = Observer::spawn(
            root.clone(),
            Rules::new(&root.display, &cfg, Vec::new()),
            Options {
                coalesce: Duration::from_millis(20),
                max_directories: 100,
                id_prefix: "t".into(),
                queue_capacity: 2,
            },
            sink.clone(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(400));
        // A burst while the sink is stalled: far more notifications than
        // the 2-slot queue holds.
        for round in 0..20 {
            for i in 0..100 {
                std::fs::write(dir.path().join(format!("f{i}.txt")), format!("{round}")).unwrap();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while sink.0.lost.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        // Stopping flushes the last loss report; count after it.
        let stats = obs.stats.clone();
        obs.stop();
        let dropped = stats.overflowed.load(Ordering::Relaxed);
        assert!(dropped > 0, "the queue overflowed");
        let lost = sink.0.lost.lock().unwrap().clone();
        // Reported as observer overflow (not a rescan), with the exact count:
        // loss is never silent and never double counted.
        assert!(
            lost.iter()
                .all(|(rescan, _)| !rescan || cfg!(target_os = "macos")),
            "{lost:?}"
        );
        let reported: u64 = lost.iter().filter(|(r, _)| !r).map(|(_, n)| n).sum();
        assert_eq!(reported, dropped, "{lost:?}");
    }

    #[test]
    fn stopping_flushes_pending_observations() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ObserveConfig {
            coalesce_ms: 5_000,
            ..ObserveConfig::default()
        };
        let root = Root::resolve(dir.path(), Path::new("/"), None).unwrap();
        let sink = Collect::default();
        let obs = Observer::spawn(
            root.clone(),
            Rules::new(&root.display, &cfg, Vec::new()),
            Options {
                coalesce: Duration::from_millis(5_000),
                max_directories: 100,
                id_prefix: "t".into(),
                queue_capacity: 0,
            },
            sink.clone(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(400));
        std::fs::write(dir.path().join("late.rs"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(600));
        obs.stop();
        let ev = sink.events.lock().unwrap().clone();
        assert!(
            !by_path(&ev, "/late.rs").is_empty(),
            "flushed on stop: {ev:?}"
        );
    }

    /// M4-F: an observed project with no file activity costs no periodic
    /// wakeups (M4 woke every coalesce/2, 10/s per root), a change costs a
    /// bounded few, and stop is immediate.
    #[test]
    fn an_idle_observer_sleeps_until_a_change_or_stop() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, sink, _root) = start(dir.path(), &ObserveConfig::default());
        // Startup notifications (if any) settle: coalescing + loss re-check.
        std::thread::sleep(Duration::from_millis(1_500));
        let wakeups = || obs.stats.loop_wakeups.load(Ordering::Relaxed);
        let before = wakeups();
        std::thread::sleep(Duration::from_secs(2));
        let idle = wakeups() - before;
        // A late OS notification alone accounts for 3 (it, its deadline,
        // the loss re-check); M4 measured 40 here.
        assert!(idle <= 3, "idle wakeups in 2 s: {idle}");
        std::fs::write(dir.path().join("a.rs"), "x").unwrap();
        let ev = wait_for(&sink, |ev| !by_path(ev, "/a.rs").is_empty());
        assert_eq!(by_path(&ev, "/a.rs").len(), 1, "{ev:?}");
        std::thread::sleep(Duration::from_millis(1_500));
        let busy = wakeups() - before;
        assert!(busy <= 15, "wakeups for one change: {busy}");
        let t = Instant::now();
        obs.stop();
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    }
}
