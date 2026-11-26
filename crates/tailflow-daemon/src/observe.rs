//! Filesystem and Git observation of the projects listed in `[observe]`
//! (M4-C, M4-D): one observer thread per root, one Git snapshot thread.
//!
//! - A root is observed by one live daemon at a time (a claim in
//!   `fs_coverage`); a daemon that loses the claim retries every minute.
//! - Observations are submitted to storage like any event (privacy
//!   boundaries and redaction apply). Observer loss is recorded as a gap.
//! - Git snapshots of each repository containing an observed root are
//!   taken at startup, at agent session boundaries in that repository,
//!   and after file changes settle (at most every `git_min_interval_secs`).
//!   A HEAD that moved between two snapshots is recorded as `git.commit`,
//!   `git.checkout` or `git.head.moved`.
//! - Idle costs no periodic wakeups beyond a few bounded maintenance
//!   timers: observers and the Git thread block until there is work, and
//!   the supervisor sleeps until its next deadline (claim retry or check,
//!   the foreign-commit poll) or a wake (own session boundary, stop).

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tailflow_core::event::{ObservabilityEvent, Provenance};
use tailflow_core::observe::{self, git, ObserveConfig, Root, Rules};
use tailflow_storage::{CoverageClaim, EventQuery, LossReason, Order, Storage};
use tokio_util::sync::CancellationToken;

/// Changed paths listed in one `git.snapshot` event's attributes (the
/// total is always recorded).
const SNAPSHOT_ATTR_CHANGES: usize = 150;
/// Serialized budget for that list (event attributes are capped at 16 KiB).
const SNAPSHOT_ATTR_BYTES: usize = 11 * 1024;
/// File changes must be quiet this long before a change-triggered snapshot.
const GIT_QUIET: Duration = Duration::from_secs(2);
const CLAIM_RETRY: Duration = Duration::from_secs(60);
/// How often a running observer checks that its coverage claim is still
/// open (another daemon closes it when it declares this run dead).
const CLAIM_CHECK: Duration = Duration::from_secs(30);
/// Session boundaries committed by this daemon wake the supervisor at once
/// (commit hook). Another daemon sharing the database does not call our
/// hook, so its commits are found by polling this often.
const FOREIGN_SESSION_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Default)]
pub struct RootStatus {
    pub root: String,
    /// observing | waiting (held by another daemon) | failed
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub observations: u64,
    pub excluded: u64,
    pub overflowed: u64,
    pub rescans: u64,
    pub watched_dirs: u64,
    pub unwatched_dirs: u64,
    pub pending_paths: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_root: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct RepoStatus {
    pub worktree_root: String,
    pub snapshots: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_snapshot_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_took_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ObserveStatus {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    pub roots: Vec<RootStatus>,
    pub git_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_binary: Option<String>,
    pub repos: Vec<RepoStatus>,
    pub errors: Vec<String>,
    pub coalesce_ms: u64,
    pub git_min_interval_secs: u64,
    /// Live counters of each root's running observer (read when the status
    /// is requested, so nothing has to copy them on a timer).
    #[serde(skip)]
    live: Vec<Option<Arc<observe::Stats>>>,
}

impl ObserveStatus {
    /// A copy with the counters of running observers read now.
    pub fn current(&self) -> Self {
        let mut s = self.clone();
        for (r, live) in s.roots.iter_mut().zip(&self.live) {
            if let Some(st) = live {
                fill_counts(r, st);
            }
        }
        s.live = Vec::new();
        s
    }
}

fn fill_counts(r: &mut RootStatus, st: &observe::Stats) {
    let ld = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
    r.observations = ld(&st.observations);
    r.excluded = ld(&st.excluded);
    r.overflowed = ld(&st.overflowed);
    r.rescans = ld(&st.rescans);
    r.watched_dirs = ld(&st.watched_dirs);
    r.unwatched_dirs = ld(&st.unwatched_dirs);
    r.pending_paths = ld(&st.pending_paths);
}

pub type Shared = Arc<Mutex<ObserveStatus>>;

pub fn disabled(reason: &str) -> Shared {
    Arc::new(Mutex::new(ObserveStatus {
        enabled: false,
        disabled_reason: Some(reason.into()),
        ..ObserveStatus::default()
    }))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

enum GitMsg {
    /// A file under this repository changed.
    Changed(PathBuf),
    /// Take a snapshot now (startup, session boundary).
    Now {
        repo: PathBuf,
        trigger: &'static str,
        session: Option<String>,
    },
    Stop,
}

/// Messages queued for the Git thread. `Changed` is sent at most once per
/// repository until the Git thread has taken it (a per-repository flag),
/// so a burst of file changes cannot grow the queue.
const GIT_QUEUE: usize = 1024;

/// Change tracking of one repository, shared by its observers and the Git
/// thread.
#[derive(Clone)]
struct RepoFlag {
    /// Set by the first change after a snapshot (and a `Changed` is sent);
    /// cleared by the Git thread just before it takes a snapshot. Later
    /// changes only update `last_change_ms`, so a burst of file changes
    /// neither grows the queue nor wakes the Git thread per change.
    dirty: Arc<AtomicBool>,
    /// Time of the latest change, in ms since `epoch`.
    last_change_ms: Arc<AtomicU64>,
    epoch: Instant,
}

impl RepoFlag {
    fn new(epoch: Instant) -> Self {
        Self {
            dirty: Arc::new(AtomicBool::new(false)),
            last_change_ms: Arc::new(AtomicU64::new(0)),
            epoch,
        }
    }

    fn touch(&self) {
        let ms = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_change_ms.fetch_max(ms, Ordering::AcqRel);
    }

    fn last_change(&self) -> Instant {
        self.epoch + Duration::from_millis(self.last_change_ms.load(Ordering::Acquire))
    }
}

/// The Git nudge of one repository.
#[derive(Clone)]
struct GitNudge {
    repo: PathBuf,
    flag: RepoFlag,
    tx: mpsc::SyncSender<GitMsg>,
}

/// Writes observations and loss to storage; nudges the Git thread.
struct StorageSink {
    storage: Storage,
    coverage: Arc<Mutex<Option<i64>>>,
    git: Option<GitNudge>,
}

impl observe::Sink for StorageSink {
    fn observed(&self, event: ObservabilityEvent) {
        let _counted_in_storage_health = self.storage.submit(event);
        if let Some(g) = &self.git {
            // Time first, then the flag: the Git thread clears the flag
            // before it snapshots, so a change is either in that snapshot
            // or sets the flag again.
            g.flag.touch();
            if !g.flag.dirty.swap(true, Ordering::AcqRel)
                && g.tx.try_send(GitMsg::Changed(g.repo.clone())).is_err()
            {
                // Queue full or the Git thread is gone: allow a later try.
                g.flag.dirty.store(false, Ordering::Release);
            }
        }
    }

    fn lost(&self, rescan: bool, count: u64, first: DateTime<Utc>, last: DateTime<Utc>) {
        let reason = if rescan {
            LossReason::FsWatcherRescan
        } else {
            LossReason::FsObserverOverflow
        };
        self.storage
            .record_observer_loss(reason, count, first, last);
    }

    fn coverage(&self, watched: u64, unwatched: u64) {
        if let Some(id) = *lock(&self.coverage) {
            if let Err(e) = self.storage.coverage_update(id, watched, unwatched) {
                tracing::warn!(err = %e, "could not update observation coverage");
            }
        }
    }
}

pub struct Handle {
    stop: CancellationToken,
    wake: mpsc::SyncSender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Handle {
    /// Stop observers (pending observations are flushed) and the Git
    /// thread; close coverage. Blocking.
    pub fn stop(mut self) {
        self.stop.cancel();
        // Full: a wake is already queued, and the supervisor checks the
        // token after every wake.
        let _queued = self.wake.try_send(());
        if let Some(t) = self.thread.take() {
            if t.join().is_err() {
                tracing::warn!("observe supervisor panicked");
            }
        }
    }
}

/// Start observation. Problems with individual roots are reported in the
/// status (and on stderr) without stopping the daemon.
pub fn spawn(
    cfg: ObserveConfig,
    config_dir: &Path,
    storage: Storage,
    shutdown: &CancellationToken,
) -> (Shared, Option<Handle>) {
    let status: Shared = Arc::new(Mutex::new(ObserveStatus {
        enabled: true,
        coalesce_ms: cfg.coalesce_ms,
        git_min_interval_secs: cfg.git_min_interval_secs,
        ..ObserveStatus::default()
    }));
    let data_dir = tailflow_core::paths::default_data_dir();
    // Never observe our own files: the data directory and the database.
    let mut own_dirs: Vec<PathBuf> = data_dir.iter().cloned().collect();
    if let Some(parent) = storage.path().parent() {
        own_dirs.push(parent.to_path_buf());
    }
    let mut roots = Vec::new();
    for raw in &cfg.paths {
        match Root::resolve(raw, config_dir, data_dir.as_deref()) {
            Ok(r) => {
                if roots.iter().any(|x: &Root| x.canonical == r.canonical) {
                    continue;
                }
                roots.push(r);
            }
            Err(e) => {
                eprintln!("tailflow-daemon: [observe] {e}");
                lock(&status).errors.push(e.to_string());
            }
        }
    }
    if roots.is_empty() {
        let mut s = lock(&status);
        s.enabled = false;
        s.disabled_reason = Some("no usable [observe] paths".into());
        drop(s);
        return (status, None);
    }
    let stop = shutdown.child_token();
    // Wakes the supervisor: a session boundary committed by this daemon,
    // or stop. Capacity 1: wakes coalesce, a sender never blocks.
    let (wake, wake_rx) = mpsc::sync_channel::<()>(1);
    let hook_wake = wake.clone();
    storage.on_commit(move |info| {
        if info.inserted_session_boundaries > 0 {
            let _coalesced = hook_wake.try_send(());
        }
    });
    // The daemon's shutdown token is cancelled before `Handle::stop`; wake
    // the supervisor for it too, so stopping never waits for a timer.
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        let token = stop.clone();
        let w = wake.clone();
        rt.spawn(async move {
            token.cancelled().await;
            let _coalesced = w.try_send(());
        });
    }
    let st = status.clone();
    let stop_thread = stop.clone();
    let thread = std::thread::Builder::new()
        .name("tailflow-observe-supervisor".into())
        .spawn(move || supervise(cfg, roots, own_dirs, storage, &st, &stop_thread, &wake_rx))
        .ok();
    (status, Some(Handle { stop, wake, thread }))
}

struct Running {
    observer: observe::Observer,
    coverage: Arc<Mutex<Option<i64>>>,
    id: i64,
    checked: Instant,
}

#[allow(clippy::too_many_lines)]
fn supervise(
    cfg: ObserveConfig,
    roots: Vec<Root>,
    own_dirs: Vec<PathBuf>,
    storage: Storage,
    status: &Shared,
    stop: &CancellationToken,
    wake: &mpsc::Receiver<()>,
) {
    // Repositories containing the roots.
    let git_bin = if cfg.git { git::find_git() } else { None };
    let repo_of: Vec<Option<PathBuf>> = roots
        .iter()
        .map(|r| tailflow_core::agent::correlation::discover(&r.display).map(|g| g.worktree_root))
        .collect();
    {
        let mut s = lock(status);
        s.git_enabled = cfg.git && git_bin.is_some();
        s.git_binary = git_bin.as_ref().map(|p| p.display().to_string());
        if cfg.git && git_bin.is_none() {
            s.errors
                .push("git not found on PATH; Git snapshots are off".into());
        }
        s.roots = roots
            .iter()
            .zip(&repo_of)
            .map(|(r, g)| RootStatus {
                root: r.display.display().to_string(),
                state: "starting",
                git_root: g.as_ref().map(|p| p.display().to_string()),
                ..RootStatus::default()
            })
            .collect();
        s.live = roots.iter().map(|_| None).collect();
    }
    let epoch = Instant::now();
    let mut flags: HashMap<PathBuf, RepoFlag> = HashMap::new();
    let (git_tx, git_thread) = match &git_bin {
        Some(bin) => {
            let mut repos: Vec<PathBuf> = repo_of.iter().flatten().cloned().collect();
            repos.sort();
            repos.dedup();
            // Each repository's snapshot lists only paths under the roots
            // observed in it: a configured subdirectory does not expose the
            // rest of the repository.
            let scope: HashMap<PathBuf, Vec<Root>> = repos
                .iter()
                .map(|repo| {
                    let rs = roots
                        .iter()
                        .zip(&repo_of)
                        .filter(|(_, g)| g.as_deref() == Some(repo.as_path()))
                        .map(|(r, _)| r.clone())
                        .collect();
                    (repo.clone(), rs)
                })
                .collect();
            for repo in &repos {
                flags.insert(repo.clone(), RepoFlag::new(epoch));
            }
            let (tx, rx) = mpsc::sync_channel::<GitMsg>(GIT_QUEUE);
            let t = spawn_git(
                bin.clone(),
                scope,
                flags.clone(),
                &cfg,
                storage.clone(),
                status.clone(),
                rx,
            );
            for repo in &repos {
                let _sent = tx.send(GitMsg::Now {
                    repo: repo.clone(),
                    trigger: "startup",
                    session: None,
                });
            }
            (Some(tx), t)
        }
        None => (None, None),
    };

    let mut running: Vec<Option<Running>> = roots.iter().map(|_| None).collect();
    let mut next_try = vec![Instant::now(); roots.len()];
    let mut session_cursor = storage.newest_seq().unwrap_or(0);
    let mut next_session_poll = Instant::now() + FOREIGN_SESSION_POLL;
    let mut session_wake = false;
    while !stop.is_cancelled() {
        // Start (or retry) observers.
        for (i, root) in roots.iter().enumerate() {
            if running.get(i).is_some_and(Option::is_some) || Instant::now() < next_try[i] {
                continue;
            }
            next_try[i] = Instant::now() + CLAIM_RETRY;
            let root_s = root.display.display().to_string();
            let backend = if cfg!(target_os = "linux") {
                "inotify (per directory)"
            } else if cfg!(target_os = "macos") {
                "fsevents (recursive)"
            } else {
                "recursive"
            };
            let claim = storage.coverage_started(&root_s, backend, 0, 0);
            let id = match claim {
                Ok(CoverageClaim::Claimed(id)) => id,
                Ok(CoverageClaim::HeldBy { pid, .. }) => {
                    set_root(status, i, |r| {
                        r.state = "waiting";
                        r.detail = Some(format!(
                            "observed by another TailFlow daemon (pid {pid}); retrying every minute"
                        ));
                    });
                    continue;
                }
                Err(e) => {
                    set_root(status, i, |r| {
                        r.state = "failed";
                        r.detail = Some(format!("coverage could not be recorded: {e}"));
                    });
                    continue;
                }
            };
            let coverage = Arc::new(Mutex::new(Some(id)));
            let rules = Rules::new(&root.display, &cfg, own_dirs.clone());
            let sink = StorageSink {
                storage: storage.clone(),
                coverage: coverage.clone(),
                git: match (&git_tx, repo_of.get(i).cloned().flatten()) {
                    (Some(tx), Some(repo)) => flags.get(&repo).map(|f| GitNudge {
                        flag: f.clone(),
                        repo,
                        tx: tx.clone(),
                    }),
                    _ => None,
                },
            };
            let opts = observe::Options {
                coalesce: Duration::from_millis(cfg.coalesce_ms),
                max_directories: cfg.max_directories,
                id_prefix: format!("fs:{}:{i}", storage.run_key()),
                queue_capacity: 0,
            };
            match observe::Observer::spawn(root.clone(), rules, opts, sink) {
                Ok(observer) => {
                    let w = observer
                        .stats
                        .watched_dirs
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let u = observer
                        .stats
                        .unwatched_dirs
                        .load(std::sync::atomic::Ordering::Relaxed);
                    // Coverage starts once the watches are in place, not at
                    // the claim (the directory walk may take a while).
                    if let Err(e) = storage.coverage_ready(id, w, u) {
                        tracing::warn!(err = %e, "coverage update failed");
                    }
                    if u > 0 {
                        eprintln!(
                            "tailflow-daemon: [observe] {root_s}: {u} directories over the watch budget \
                             (max_directories = {}); coverage is partial",
                            cfg.max_directories
                        );
                    }
                    {
                        let mut s = lock(status);
                        if let Some(r) = s.roots.get_mut(i) {
                            r.state = "observing";
                            r.backend = Some(observer.backend);
                            r.detail = None;
                        }
                        if let Some(l) = s.live.get_mut(i) {
                            *l = Some(observer.stats.clone());
                        }
                    }
                    if let Some(slot) = running.get_mut(i) {
                        *slot = Some(Running {
                            observer,
                            coverage,
                            id,
                            checked: Instant::now(),
                        });
                    }
                }
                Err(e) => {
                    let _closed = storage.coverage_stopped(id);
                    set_root(status, i, |r| {
                        r.state = "failed";
                        r.detail = Some(format!("watcher could not start: {e}"));
                    });
                }
            }
        }
        // A claim closed by another daemon (it judged this run dead, for
        // example after a long sleep): stop, then re-claim at once.
        for (i, slot) in running.iter_mut().enumerate() {
            let Some(run) = slot.as_mut() else { continue };
            if run.checked.elapsed() < CLAIM_CHECK {
                continue;
            }
            run.checked = Instant::now();
            if !matches!(storage.coverage_is_open(run.id), Ok(false)) {
                continue;
            }
            if let Some(run) = slot.take() {
                run.observer.stop();
                freeze_counts(status, i);
                lock(&run.coverage).take();
            }
            eprintln!(
                "tailflow-daemon: [observe] {}: observation claim was closed by another daemon; re-claiming",
                roots.get(i).map_or_else(String::new, |r| r.display.display().to_string())
            );
            set_root(status, i, |r| {
                r.state = "waiting";
                r.detail =
                    Some("observation claim was closed by another daemon; re-claiming".into());
            });
            if let Some(t) = next_try.get_mut(i) {
                *t = Instant::now();
            }
        }
        // Session boundaries in observed repositories trigger snapshots:
        // at once for this daemon's own commits (the commit hook wakes us),
        // and by a bounded poll for commits of another daemon.
        if let Some(tx) = &git_tx {
            if session_wake || Instant::now() >= next_session_poll {
                session_cursor = session_boundaries(&storage, session_cursor, &repo_of, tx);
                next_session_poll = Instant::now() + FOREIGN_SESSION_POLL;
            }
        }
        // Sleep until the next deadline or a wake (session boundary, stop).
        let mut deadline: Option<Instant> = git_tx.as_ref().map(|_| next_session_poll);
        for (i, slot) in running.iter().enumerate() {
            let at = match slot {
                Some(run) => Some(run.checked + CLAIM_CHECK),
                None => next_try.get(i).copied(),
            };
            deadline = [deadline, at].into_iter().flatten().min();
        }
        if stop.is_cancelled() {
            break;
        }
        let wait = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        session_wake = match wait {
            None => match wake.recv() {
                Ok(()) => true,
                Err(_) => break,
            },
            Some(w) => match wake.recv_timeout(w) {
                Ok(()) => true,
                Err(RecvTimeoutError::Timeout) => false,
                // Every sender is gone (cannot happen while the handle
                // lives): stop rather than spin.
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
    }
    for (i, run) in running.into_iter().enumerate() {
        let Some(run) = run else { continue };
        // Stop first: stopping flushes pending observations.
        run.observer.stop();
        freeze_counts(status, i);
        let id = lock(&run.coverage).take();
        if let Some(id) = id {
            if let Err(e) = storage.coverage_stopped(id) {
                tracing::warn!(err = %e, "could not close observation coverage");
            }
        }
    }
    if let Some(tx) = git_tx {
        let _sent = tx.send(GitMsg::Stop);
    }
    if let Some(t) = git_thread {
        let _joined = t.join();
    }
}

/// Copy a root's final counters into its status and stop reading them
/// live (its observer has stopped).
fn freeze_counts(status: &Shared, i: usize) {
    let mut s = lock(status);
    let live = s.live.get_mut(i).and_then(Option::take);
    if let (Some(st), Some(r)) = (live, s.roots.get_mut(i)) {
        fill_counts(r, &st);
    }
}

fn set_root(status: &Shared, i: usize, f: impl FnOnce(&mut RootStatus)) {
    if let Some(r) = lock(status).roots.get_mut(i) {
        f(r);
    }
}

/// Snapshot repositories where an agent session started or ended since
/// `cursor`. Returns the new cursor.
fn session_boundaries(
    storage: &Storage,
    cursor: u64,
    repos: &[Option<PathBuf>],
    tx: &mpsc::SyncSender<GitMsg>,
) -> u64 {
    let q = EventQuery {
        event_types: vec!["agent.session.started".into(), "agent.session.ended".into()],
        after_seq: Some(cursor),
        order: Order::Asc,
        limit: 100,
        ..EventQuery::default()
    };
    let Ok(page) = storage.query(&q) else {
        return cursor;
    };
    for e in &page.events {
        let ev = &e.event;
        let git_root = ev
            .attributes
            .get("git.root")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        let cwd = ev
            .path
            .as_deref()
            .or(ev.project_id.as_deref())
            .map(PathBuf::from);
        let hit = repos.iter().flatten().find(|r| {
            git_root.as_deref() == Some(r.as_path())
                || cwd.as_deref().is_some_and(|c| c.starts_with(r))
        });
        if let Some(repo) = hit {
            let trigger = if ev.event_type == "agent.session.started" {
                "session_start"
            } else {
                "session_end"
            };
            let _sent = tx.send(GitMsg::Now {
                repo: repo.clone(),
                trigger,
                session: ev.session_id.clone(),
            });
        }
    }
    page.next_cursor.unwrap_or(cursor).max(cursor)
}

struct RepoState {
    last_snapshot: Option<Instant>,
    /// A `Changed` arrived and no snapshot has been taken since.
    dirty: bool,
    previous: Option<git::Snapshot>,
    seq: u64,
}

fn spawn_git(
    bin: PathBuf,
    scope: HashMap<PathBuf, Vec<Root>>,
    flags: HashMap<PathBuf, RepoFlag>,
    cfg: &ObserveConfig,
    storage: Storage,
    status: Shared,
    rx: mpsc::Receiver<GitMsg>,
) -> Option<std::thread::JoinHandle<()>> {
    let min_interval = Duration::from_secs(cfg.git_min_interval_secs);
    let mut repos: Vec<PathBuf> = scope.keys().cloned().collect();
    repos.sort();
    {
        let mut s = lock(&status);
        s.repos = repos
            .iter()
            .map(|r| RepoStatus {
                worktree_root: r.display().to_string(),
                ..RepoStatus::default()
            })
            .collect();
    }
    std::thread::Builder::new()
        .name("tailflow-git-snapshots".into())
        .spawn(move || {
            let mut state: HashMap<PathBuf, RepoState> = repos
                .iter()
                .map(|r| {
                    (
                        r.clone(),
                        RepoState {
                            last_snapshot: None,
                            dirty: false,
                            previous: None,
                            seq: 0,
                        },
                    )
                })
                .collect();
            // A change-triggered snapshot is due once changes have been
            // quiet for GIT_QUIET and the last snapshot is min_interval
            // old. With no repository dirty, the thread blocks.
            let due_at = |repo: &Path, st: &RepoState| -> Option<Instant> {
                if !st.dirty {
                    return None;
                }
                let quiet = flags.get(repo).map(|f| f.last_change() + GIT_QUIET);
                let spaced = st.last_snapshot.map(|l| l + min_interval);
                [quiet, spaced].into_iter().flatten().max()
            };
            // Clear the shared flag before snapshotting: a change after
            // this point sets it again and is snapshotted later.
            let clear = |repo: &Path, st: &mut RepoState| {
                st.dirty = false;
                if let Some(f) = flags.get(repo) {
                    f.dirty.store(false, Ordering::Release);
                }
            };
            loop {
                let deadline = state.iter().filter_map(|(r, st)| due_at(r, st)).min();
                let got = match deadline {
                    None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    Some(d) => rx.recv_timeout(d.saturating_duration_since(Instant::now())),
                };
                match got {
                    Ok(GitMsg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                    Ok(GitMsg::Changed(repo)) => {
                        if let Some(st) = state.get_mut(&repo) {
                            st.dirty = true;
                        }
                    }
                    Ok(GitMsg::Now {
                        repo,
                        trigger,
                        session,
                    }) => {
                        if let Some(st) = state.get_mut(&repo) {
                            clear(&repo, st);
                            let roots = scope.get(&repo).map_or(&[][..], Vec::as_slice);
                            take(
                                &bin,
                                &repo,
                                roots,
                                st,
                                trigger,
                                session.as_deref(),
                                &storage,
                                &status,
                            );
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                }
                let now = Instant::now();
                let due: Vec<PathBuf> = state
                    .iter()
                    .filter(|(r, st)| due_at(r, st).is_some_and(|d| now >= d))
                    .map(|(r, _)| r.clone())
                    .collect();
                for repo in due {
                    if let Some(st) = state.get_mut(&repo) {
                        clear(&repo, st);
                        let roots = scope.get(&repo).map_or(&[][..], Vec::as_slice);
                        take(&bin, &repo, roots, st, "fs_change", None, &storage, &status);
                    }
                }
            }
        })
        .ok()
}

#[allow(clippy::too_many_arguments)]
fn take(
    bin: &Path,
    repo: &Path,
    roots: &[Root],
    st: &mut RepoState,
    trigger: &'static str,
    session: Option<&str>,
    storage: &Storage,
    status: &Shared,
) {
    st.last_snapshot = Some(Instant::now());
    let result = git::snapshot(bin, repo, git::DEFAULT_TIMEOUT);
    let repo_s = repo.display().to_string();
    let snap = match result {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(repo = %repo_s, err = %e, "git snapshot failed");
            let mut s = lock(status);
            if let Some(r) = s.repos.iter_mut().find(|r| r.worktree_root == repo_s) {
                r.last_error = Some(e);
            }
            return;
        }
    };
    st.seq = st.seq.wrapping_add(1);
    let id_base = format!("git:{}:{}", storage.run_key(), repo_s);
    // HEAD transitions since the previous snapshot of this repository.
    if let Some(prev) = &st.previous {
        if let Some(e) = head_transition(bin, repo, prev, &snap, &format!("{id_base}:h{}", st.seq))
        {
            let _counted = storage.submit(e);
        }
    }
    let event = snapshot_event(
        &snap,
        roots,
        storage,
        trigger,
        session,
        &format!("{id_base}:s{}", st.seq),
    );
    let _counted = storage.submit(event);
    {
        let mut s = lock(status);
        if let Some(r) = s.repos.iter_mut().find(|r| r.worktree_root == repo_s) {
            r.snapshots += 1;
            r.last_snapshot_at = Some(Utc::now());
            r.last_took_ms = Some(snap.took_ms);
            r.last_error = None;
            r.head = snap.head.clone();
            r.branch = snap.branch.clone();
        }
    }
    st.previous = Some(snap);
}

fn snapshot_event(
    snap: &git::Snapshot,
    roots: &[Root],
    storage: &Storage,
    trigger: &str,
    session: Option<&str>,
    id: &str,
) -> ObservabilityEvent {
    let root = snap.worktree_root.display().to_string();
    let mut e = ObservabilityEvent::new(
        id,
        "git.snapshot",
        Provenance::FilesystemObserved,
        "git",
        format!("git:{root}"),
        Utc::now(),
    );
    e.path = Some(root.clone());
    e.project_id = Some(root.clone());
    e.severity = tailflow_core::LogLevel::Info;
    e.session_id = session.map(str::to_string);
    let policy = storage.policy();
    let in_scope = |p: &Path| {
        roots
            .iter()
            .any(|r| p.starts_with(&r.display) || p.starts_with(&r.canonical))
    };
    let admitted = |p: &Path| {
        let mut probe = ObservabilityEvent::new(
            "probe",
            "file.modified",
            Provenance::FilesystemObserved,
            "git",
            "git",
            Utc::now(),
        );
        probe.path = Some(p.display().to_string());
        policy.admit(&probe).is_ok()
    };
    // Changed paths under the observed roots, held to the privacy
    // boundaries. The list is a prefix of the path-sorted changes: at the
    // count or byte cap it stops (it never skips one path and lists a
    // later one), so two snapshots can be compared up to the shorter
    // prefix.
    let mut list = Vec::new();
    let mut bytes = 0usize;
    let mut withheld = 0u64;
    let mut outside = 0u64;
    let mut cut = false;
    for c in &snap.changes {
        let abs = snap.worktree_root.join(&c.path);
        if !in_scope(&abs) {
            outside += 1;
            continue;
        }
        let from_abs = c.from.as_ref().map(|f| snap.worktree_root.join(f));
        if !admitted(&abs) || from_abs.as_deref().is_some_and(|f| !admitted(f)) {
            withheld += 1;
            continue;
        }
        if list.len() >= SNAPSHOT_ATTR_CHANGES {
            cut = true;
            break;
        }
        let mut item = json!({ "p": c.path, "s": c.status });
        if c.staged {
            item["x"] = Value::Bool(true);
        }
        if c.unstaged {
            item["y"] = Value::Bool(true);
        }
        // A rename from outside the observed roots keeps its status but
        // not the source path.
        if let (Some(f), Some(fa)) = (&c.from, &from_abs) {
            if in_scope(fa) {
                item["from"] = Value::from(f.clone());
            }
        }
        if let Some(n) = c.size {
            item["size"] = Value::from(n);
        }
        if let Some(t) = c.mtime_ns {
            item["mtime_ns"] = Value::from(i64::try_from(t).unwrap_or(i64::MAX));
        }
        if let Some(n) = c.lines_added {
            item["add"] = Value::from(n);
        }
        if let Some(n) = c.lines_deleted {
            item["del"] = Value::from(n);
        }
        let len = item.to_string().len();
        if bytes.saturating_add(len) > SNAPSHOT_ATTR_BYTES {
            cut = true;
            break;
        }
        bytes = bytes.saturating_add(len);
        list.push(item);
    }
    // Paths are known only up to this one when the list is cut here or
    // the snapshot itself was truncated.
    let listed_through = (cut || snap.truncated).then(|| {
        list.last()
            .and_then(|i| i.get("p"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    });
    let listed = list.len();
    let a = &mut e.attributes;
    a.insert("git.root".into(), Value::from(root.clone()));
    if let Some(h) = &snap.head {
        a.insert("git.head".into(), Value::from(h.clone()));
    }
    if let Some(b) = &snap.branch {
        a.insert("git.branch".into(), Value::from(b.clone()));
    }
    if snap.detached {
        a.insert("git.detached".into(), Value::Bool(true));
    }
    a.insert("git.trigger".into(), Value::from(trigger));
    a.insert("git.took_ms".into(), Value::from(snap.took_ms));
    a.insert(
        "git.changes_total".into(),
        Value::from(snap.total_changes as u64),
    );
    a.insert("git.changes_listed".into(), Value::from(listed as u64));
    a.insert("git.list_format".into(), Value::from(2u64));
    a.insert(
        "git.changes_complete".into(),
        Value::Bool(listed_through.is_none() && withheld == 0),
    );
    if let Some(p) = listed_through {
        a.insert("git.changes_listed_through".into(), Value::from(p));
    }
    if outside > 0 {
        a.insert(
            "git.changes_outside_observed_roots".into(),
            Value::from(outside),
        );
    }
    if !snap.filters_disabled.is_empty() {
        a.insert(
            "git.filters_disabled".into(),
            Value::from(snap.filters_disabled.clone()),
        );
    }
    if withheld > 0 {
        a.insert(
            "git.changes_withheld_by_privacy".into(),
            Value::from(withheld),
        );
    }
    if snap.numstat_missing {
        a.insert("git.numstat_missing".into(), Value::Bool(true));
    }
    if let Ok(Value::Object(counts)) = serde_json::to_value(&snap.counts) {
        a.insert("git.counts".into(), Value::Object(counts));
    }
    a.insert("git.changes".into(), Value::Array(list));
    e.message = Some(format!(
        "{} changed path{} in {}{}",
        snap.total_changes,
        if snap.total_changes == 1 { "" } else { "s" },
        Path::new(&root)
            .file_name()
            .map_or(root.clone(), |n| n.to_string_lossy().into_owned()),
        snap.branch
            .as_deref()
            .map(|b| format!(" ({b})"))
            .unwrap_or_default()
    ));
    e
}

/// `git.commit`, `git.checkout` or `git.head.moved` when HEAD or the
/// branch differ between two snapshots.
fn head_transition(
    bin: &Path,
    repo: &Path,
    prev: &git::Snapshot,
    next: &git::Snapshot,
    id: &str,
) -> Option<ObservabilityEvent> {
    if prev.head == next.head && prev.branch == next.branch {
        return None;
    }
    let ty = if prev.branch != next.branch {
        "git.checkout"
    } else if let (Some(old), Some(new)) = (&prev.head, &next.head) {
        if git::is_parent(bin, repo, new, old) {
            "git.commit"
        } else {
            "git.head.moved"
        }
    } else {
        "git.head.moved"
    };
    let root = repo.display().to_string();
    let mut e = ObservabilityEvent::new(
        id,
        ty,
        Provenance::FilesystemObserved,
        "git",
        format!("git:{root}"),
        Utc::now(),
    );
    e.path = Some(root.clone());
    e.project_id = Some(root.clone());
    e.severity = tailflow_core::LogLevel::Info;
    let a = &mut e.attributes;
    a.insert("git.root".into(), Value::from(root));
    for (k, v) in [
        ("git.head_before", &prev.head),
        ("git.head_after", &next.head),
        ("git.branch_before", &prev.branch),
        ("git.branch_after", &next.branch),
    ] {
        if let Some(v) = v {
            a.insert(k.into(), Value::from(v.clone()));
        }
    }
    e.message = Some(match ty {
        "git.checkout" => format!(
            "switched from {} to {}",
            prev.branch.as_deref().unwrap_or("(detached)"),
            next.branch.as_deref().unwrap_or("(detached)")
        ),
        "git.commit" => format!(
            "new commit {} on {}",
            next.head
                .as_deref()
                .map_or("?", |h| h.get(..10).unwrap_or(h)),
            next.branch.as_deref().unwrap_or("(detached)")
        ),
        _ => "HEAD moved".into(),
    });
    // Observed between two snapshots: who moved HEAD is unknown.
    a.insert("git.observed_between_snapshots".into(), Value::Bool(true));
    Some(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tailflow_core::config::StorageConfig;
    use tailflow_core::privacy::{CapturePolicy, PrivacyConfig};
    use tailflow_storage::OpenOptions;

    fn storage(dir: &Path, privacy: &PrivacyConfig) -> Storage {
        Storage::open(
            &StorageConfig {
                path: Some(dir.join("events.db")),
                ..StorageConfig::default()
            },
            OpenOptions {
                project_id: None,
                version: "test".into(),
                policy: Arc::new(CapturePolicy::from_config(privacy).unwrap()),
            },
        )
        .unwrap()
    }

    fn change(path: &str, from: Option<&str>) -> git::Change {
        git::Change {
            path: path.into(),
            status: if from.is_some() {
                "renamed"
            } else {
                "modified"
            },
            staged: false,
            unstaged: true,
            from: from.map(str::to_string),
            size: None,
            mtime_ns: None,
            lines_added: None,
            lines_deleted: None,
        }
    }

    fn snap(root: &Path, changes: Vec<git::Change>) -> git::Snapshot {
        git::Snapshot {
            worktree_root: root.to_path_buf(),
            head: None,
            branch: None,
            detached: false,
            total_changes: changes.len(),
            changes,
            truncated: false,
            numstat_missing: false,
            counts: git::Counts::default(),
            took_ms: 0,
            filters_disabled: vec!["lfs".into()],
        }
    }

    fn root(p: PathBuf) -> Root {
        Root {
            canonical: p.clone(),
            display: p,
        }
    }

    #[test]
    fn snapshots_list_only_observed_roots_and_admitted_rename_sources() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let privacy = PrivacyConfig {
            excluded_file_globs: vec!["**/secret/**".into()],
            ..PrivacyConfig::default()
        };
        let st = storage(dir.path(), &privacy);
        let s = snap(
            &repo,
            vec![
                change("app/a.rs", None),
                change("app/b.rs", Some("secret/key.rs")),
                change("app/c.rs", Some("other/c.rs")),
                change("other/x.rs", None),
            ],
        );
        let e = snapshot_event(&s, &[root(repo.join("app"))], &st, "startup", None, "id");
        let a = &e.attributes;
        let listed: Vec<&str> = a["git.changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["p"].as_str().unwrap())
            .collect();
        assert_eq!(listed, ["app/a.rs", "app/c.rs"], "{a:?}");
        // A rename from outside the observed roots keeps no source path.
        assert!(a["git.changes"][1].get("from").is_none(), "{a:?}");
        assert_eq!(a["git.changes_withheld_by_privacy"], 1);
        assert_eq!(a["git.changes_outside_observed_roots"], 1);
        assert_eq!(a["git.filters_disabled"], serde_json::json!(["lfs"]));
        assert!(a.get("git.changes_listed_through").is_none());
        assert_eq!(a["git.list_format"], 2);
    }

    fn snapshots(st: &Storage) -> Vec<ObservabilityEvent> {
        let q = EventQuery {
            event_types: vec!["git.snapshot".into()],
            order: Order::Asc,
            limit: 100,
            ..EventQuery::default()
        };
        st.query(&q)
            .unwrap()
            .events
            .into_iter()
            .map(|e| e.event)
            .collect()
    }

    fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ok() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// M4-F: the event-driven supervisor and Git thread still take the
    /// startup, change-triggered and session-boundary snapshots, the
    /// status reports live counters, and stopping does not wait on a timer.
    #[test]
    fn event_driven_supervisor_keeps_every_snapshot_trigger() {
        let Some(git_bin) = git::find_git() else {
            eprintln!("git not installed; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run_git = |args: &[&str]| {
            let ok = std::process::Command::new(&git_bin)
                .args(args)
                .current_dir(&repo)
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        run_git(&["init", "-q"]);
        std::fs::write(repo.join("a.rs"), "v1\n").unwrap();
        run_git(&["add", "."]);
        run_git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "init",
        ]);
        let st = storage(&dir.path().join("data"), &PrivacyConfig::default());
        let cfg = ObserveConfig {
            paths: vec![repo.clone()],
            git_min_interval_secs: 5,
            coalesce_ms: 50,
            ..ObserveConfig::default()
        };
        let (status, handle) = spawn(cfg, dir.path(), st.clone(), &CancellationToken::new());
        let handle = handle.expect("observation starts");
        wait_until("observing and the startup snapshot", || {
            let s = lock(&status).current();
            s.roots.first().is_some_and(|r| r.state == "observing")
                && s.repos.first().is_some_and(|r| r.snapshots >= 1)
        });
        let triggers = |st: &Storage| -> Vec<String> {
            snapshots(st)
                .iter()
                .filter_map(|e| e.attributes.get("git.trigger")?.as_str().map(String::from))
                .collect()
        };
        // Submitted to storage asynchronously; committed shortly after.
        wait_until("the startup snapshot in storage", || {
            !triggers(&st).is_empty()
        });
        assert_eq!(triggers(&st), ["startup"]);

        // A file change: observed (live counter), then a snapshot once the
        // changes are quiet and the minimum interval has passed.
        std::fs::write(repo.join("a.rs"), "v2\n").unwrap();
        wait_until("a live observation count", || {
            lock(&status)
                .current()
                .roots
                .first()
                .is_some_and(|r| r.observations >= 1)
        });
        wait_until("a change-triggered snapshot", || {
            triggers(&st).iter().any(|t| t == "fs_change")
        });

        // A session boundary committed by this daemon wakes the supervisor.
        let mut start = ObservabilityEvent::new(
            "s1",
            "agent.session.started",
            Provenance::AgentReported,
            "agent",
            "agent:test",
            Utc::now(),
        );
        start.session_id = Some("claude-code:s1".into());
        start.attributes.insert(
            "git.root".into(),
            Value::from(
                tailflow_core::agent::correlation::discover(&repo)
                    .unwrap()
                    .worktree_root
                    .display()
                    .to_string(),
            ),
        );
        let _accepted = st.submit(start);
        wait_until("a session-start snapshot", || {
            triggers(&st).iter().any(|t| t == "session_start")
        });

        let t = Instant::now();
        handle.stop();
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "stop took {:?}",
            t.elapsed()
        );
        // Counters stay readable after the observer stopped.
        let s = lock(&status).current();
        assert!(
            s.roots.first().is_some_and(|r| r.observations >= 1),
            "{s:?}"
        );
    }

    #[test]
    fn a_capped_snapshot_lists_a_sorted_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let st = storage(dir.path(), &PrivacyConfig::default());
        let changes: Vec<git::Change> = (0..SNAPSHOT_ATTR_CHANGES + 5)
            .map(|i| change(&format!("f{i:05}.rs"), None))
            .collect();
        let s = snap(&repo, changes);
        let e = snapshot_event(&s, &[root(repo.clone())], &st, "startup", None, "id");
        let a = &e.attributes;
        let list = a["git.changes"].as_array().unwrap();
        assert_eq!(list.len(), SNAPSHOT_ATTR_CHANGES);
        let last = format!("f{:05}.rs", SNAPSHOT_ATTR_CHANGES - 1);
        assert_eq!(list.last().unwrap()["p"], last.as_str());
        assert_eq!(a["git.changes_listed_through"], last.as_str());
        assert_eq!(a["git.changes_complete"], false);
    }
}
