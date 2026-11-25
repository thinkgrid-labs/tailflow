//! The agent event spool: the hand-off between short-lived hook processes
//! and whichever daemon is (or will be) running.
//!
//! Why a spool and not a socket or an HTTP endpoint:
//! - **Trust boundary = filesystem permissions.** The directory is `0700`
//!   and files `0600` on Unix; on Windows it lives under the per-user
//!   `%LOCALAPPDATA%`, whose default ACL admits only that user, SYSTEM and
//!   administrators. No listening socket, no port, no token to leak, and no
//!   unauthenticated mutation endpoint (M1's API stays read-only).
//! - **Daemon optional.** A hook succeeds whether or not a daemon runs; the
//!   next daemon start ingests what accumulated. Nothing pretends to be
//!   captured: events are "spooled" until a daemon commits them, and status
//!   says so.
//! - **One file per hook call, written to a temp name and renamed.** Rename
//!   is atomic on every supported platform, so a reader never sees half a
//!   record, and concurrent hooks never interleave.
//!
//! Bounds: at most [`MAX_SPOOL_FILES`] files and [`MAX_RECORD_BYTES`] per
//! file. A hook that finds the spool full writes nothing but bumps the
//! lock-protected `overflow.json` counter, which the daemon turns into a
//! recorded `agent_spool_overflow` gap — loss is counted, never silent.
//!
//! Ownership: a daemon claims a file by renaming it into `claimed/<run>/`.
//! Exactly one rename succeeds. It deletes the file only after storage has
//! committed its events; a daemon that dies holding claims leaves them for
//! [`Spool::recover_stale_claims`], and storage's stable-id deduplication
//! makes the replay harmless.

use super::AgentKind;
use crate::event::ObservabilityEvent;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Spool record format version.
pub const SPOOL_FORMAT: u16 = 1;
/// Files kept waiting before new hook calls overflow.
pub const MAX_SPOOL_FILES: usize = 20_000;
/// Largest record a hook writes or a daemon reads.
pub const MAX_RECORD_BYTES: usize = 2 * 1024 * 1024;
/// Records older than this are still ingested, but flagged as late.
pub const LATE_AFTER: Duration = Duration::from_secs(24 * 3600);

const OVERFLOW_FILE: &str = "overflow.json";
const CLAIMED_DIR: &str = "claimed";
const RECORD_EXT: &str = "json";

/// Why a hook input produced no stored events, recorded as loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RejectReason {
    /// Stdin exceeded the input bound.
    OversizedInput,
    /// Not JSON, or not a hook payload.
    MalformedInput,
    /// A normalized event failed schema validation after sanitizing.
    InvalidEvent,
    /// The per-user agent capture config is invalid: nothing is recorded
    /// under a policy the user did not intend.
    InvalidConfig,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectReason::OversizedInput => "oversized_input",
            RejectReason::MalformedInput => "malformed_input",
            RejectReason::InvalidEvent => "invalid_event",
            RejectReason::InvalidConfig => "invalid_config",
        }
    }
}

/// One rejected input (or event) inside a record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rejection {
    pub reason: RejectReason,
    pub count: u32,
}

/// One hook invocation's output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpoolRecord {
    pub format: u16,
    pub agent: AgentKind,
    pub written_at: DateTime<Utc>,
    /// Already admitted, sanitized and validated by the hook.
    #[serde(default)]
    pub events: Vec<ObservabilityEvent>,
    /// Events the capture policy refused (deliberate, not loss).
    #[serde(default)]
    pub excluded: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<Rejection>,
}

impl SpoolRecord {
    pub fn new(agent: AgentKind) -> Self {
        Self {
            format: SPOOL_FORMAT,
            agent,
            written_at: Utc::now(),
            events: Vec::new(),
            excluded: 0,
            rejected: Vec::new(),
        }
    }

    pub fn reject(&mut self, reason: RejectReason, count: u32) {
        if count == 0 {
            return;
        }
        match self.rejected.iter_mut().find(|r| r.reason == reason) {
            Some(r) => r.count = r.count.saturating_add(count),
            None => self.rejected.push(Rejection { reason, count }),
        }
    }

    /// Nothing worth writing.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.excluded == 0 && self.rejected.is_empty()
    }
}

/// Lost hook calls that never became a record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overflow {
    pub count: u64,
    pub first_at: Option<DateTime<Utc>>,
    pub last_at: Option<DateTime<Utc>>,
}

/// Result of [`Spool::write`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    Written(PathBuf),
    /// The spool was full (or the record too large); counted in overflow.
    Overflowed,
    Empty,
}

#[derive(Debug)]
pub enum SpoolError {
    Io(io::Error),
    Corrupt(String),
}

impl std::fmt::Display for SpoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpoolError::Io(e) => write!(f, "spool I/O error: {e}"),
            SpoolError::Corrupt(m) => write!(f, "spool record is corrupt: {m}"),
        }
    }
}

impl std::error::Error for SpoolError {}

impl From<io::Error> for SpoolError {
    fn from(e: io::Error) -> Self {
        SpoolError::Io(e)
    }
}

/// Status of the waiting spool, for `agent status` and `/api/v1/agents/status`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SpoolStats {
    pub pending_files: usize,
    /// Counting stopped at the cap.
    pub pending_capped: bool,
    pub oldest_pending_at: Option<DateTime<Utc>>,
    pub claimed_files: usize,
    pub overflow: Overflow,
}

/// A spool directory.
#[derive(Debug, Clone)]
pub struct Spool {
    dir: PathBuf,
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    // No fsync: a hook must return in milliseconds. Rename is atomic for
    // visibility; a power loss can drop the newest records, exactly as it
    // can drop the newest WAL frames under `synchronous = NORMAL`.
    Ok(())
}

fn is_record(name: &str) -> bool {
    !name.starts_with('.')
        && Path::new(name).extension().is_some_and(|e| e == RECORD_EXT)
        && name != OVERFLOW_FILE
}

/// The record time encoded at the start of a file name.
fn name_time(name: &str) -> Option<DateTime<Utc>> {
    let micros = name.split('-').next()?;
    let us = i64::from_str_radix(micros, 16).ok()?;
    DateTime::from_timestamp_micros(us)
}

impl Spool {
    /// The default spool: `<data dir>/agents/spool`.
    pub fn default_location() -> Option<Self> {
        crate::paths::default_agents_dir().map(|d| Self::at(d.join("spool")))
    }

    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn ensure(&self) -> io::Result<()> {
        crate::paths::create_private_dir(&self.dir)
    }

    /// Write a record. Never blocks on another process except for the brief
    /// overflow-file lock. Bounded: counts at most [`MAX_SPOOL_FILES`]
    /// directory entries.
    ///
    /// # Errors
    /// The directory or file cannot be created.
    pub fn write(&self, rec: &SpoolRecord) -> Result<WriteOutcome, SpoolError> {
        if rec.is_empty() {
            return Ok(WriteOutcome::Empty);
        }
        self.ensure()?;
        let bytes = serde_json::to_vec(rec).map_err(|e| SpoolError::Corrupt(e.to_string()))?;
        if bytes.len() > MAX_RECORD_BYTES || self.count_pending(MAX_SPOOL_FILES)? >= MAX_SPOOL_FILES
        {
            self.add_overflow(1, rec.written_at)?;
            return Ok(WriteOutcome::Overflowed);
        }
        let stem = format!(
            "{:x}-{}-{}",
            rec.written_at.timestamp_micros().max(0),
            rec.agent.as_str(),
            super::unique_nonce()
        );
        let tmp = self.dir.join(format!(".tmp-{stem}"));
        let path = self.dir.join(format!("{stem}.{RECORD_EXT}"));
        if let Err(e) = write_private(&tmp, &bytes) {
            let _cleanup = fs::remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = fs::rename(&tmp, &path) {
            let _cleanup = fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(WriteOutcome::Written(path))
    }

    fn count_pending(&self, cap: usize) -> io::Result<usize> {
        let mut n = 0usize;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_name().to_str().is_some_and(is_record) {
                n += 1;
                if n >= cap {
                    break;
                }
            }
        }
        Ok(n)
    }

    fn open_overflow(&self) -> io::Result<fs::File> {
        let mut opts = fs::OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(self.dir.join(OVERFLOW_FILE))
    }

    fn read_overflow(f: &mut fs::File) -> Overflow {
        let mut s = String::new();
        if f.rewind().is_err() {
            // Cannot read it: loss happened, the amount is unknown.
            return Overflow {
                count: 1,
                first_at: None,
                last_at: None,
            };
        }
        if f.take(4096).read_to_string(&mut s).is_err() || s.trim().is_empty() {
            return Overflow::default();
        }
        // A damaged counter still means loss happened: never return zero.
        serde_json::from_str(&s).unwrap_or(Overflow {
            count: 1,
            first_at: None,
            last_at: None,
        })
    }

    fn store_overflow(f: &mut fs::File, o: &Overflow) -> io::Result<()> {
        f.set_len(0)?;
        f.rewind()?;
        if o.count > 0 {
            let bytes = serde_json::to_vec(o).map_err(io::Error::other)?;
            f.write_all(&bytes)?;
        }
        Ok(())
    }

    /// Count `n` lost hook calls at `at`.
    ///
    /// # Errors
    /// The counter file cannot be opened or locked.
    pub fn add_overflow(&self, n: u64, at: DateTime<Utc>) -> io::Result<()> {
        self.ensure()?;
        let mut f = self.open_overflow()?;
        f.lock()?;
        let mut o = Self::read_overflow(&mut f);
        o.count = o.count.saturating_add(n);
        o.first_at = Some(o.first_at.map_or(at, |f| f.min(at)));
        o.last_at = Some(o.last_at.map_or(at, |l| l.max(at)));
        let r = Self::store_overflow(&mut f, &o);
        let _unlock = f.unlock();
        r
    }

    /// Take (and reset) the overflow counter. The caller must record it.
    ///
    /// # Errors
    /// The counter file cannot be opened or locked.
    pub fn take_overflow(&self) -> io::Result<Option<Overflow>> {
        if !self.dir.join(OVERFLOW_FILE).exists() {
            return Ok(None);
        }
        let mut f = self.open_overflow()?;
        f.lock()?;
        let o = Self::read_overflow(&mut f);
        let r = Self::store_overflow(&mut f, &Overflow::default());
        let _unlock = f.unlock();
        r?;
        Ok((o.count > 0).then_some(o))
    }

    /// Put back an overflow that could not be recorded.
    ///
    /// # Errors
    /// The counter file cannot be opened or locked.
    pub fn restore_overflow(&self, o: &Overflow) -> io::Result<()> {
        let mut f = self.open_overflow()?;
        f.lock()?;
        let mut cur = Self::read_overflow(&mut f);
        cur.count = cur.count.saturating_add(o.count);
        cur.first_at = match (cur.first_at, o.first_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        cur.last_at = match (cur.last_at, o.last_at) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let r = Self::store_overflow(&mut f, &cur);
        let _unlock = f.unlock();
        r
    }

    /// Up to `limit` waiting record names, oldest first.
    ///
    /// # Errors
    /// The directory cannot be read (a missing directory is empty).
    pub fn pending(&self, limit: usize) -> io::Result<Vec<PathBuf>> {
        let rd = match fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut names: Vec<String> = Vec::new();
        for entry in rd {
            let entry = entry?;
            if let Some(n) = entry.file_name().to_str().filter(|n| is_record(n)) {
                names.push(n.to_string());
                // Bound memory for a huge spool: keep a window, sort later.
                if names.len() > limit.saturating_mul(4).max(4096) {
                    names.sort_unstable();
                    names.truncate(limit);
                }
            }
        }
        names.sort_unstable();
        names.truncate(limit);
        Ok(names.into_iter().map(|n| self.dir.join(n)).collect())
    }

    /// Records this owner claimed earlier and has not completed (after a
    /// failed flush, for example), oldest first.
    ///
    /// # Errors
    /// The claim directory cannot be read (missing = none).
    pub fn own_claims(&self, owner: &str) -> io::Result<Vec<PathBuf>> {
        let dir = self.claim_dir(owner);
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut v: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(is_record)
            })
            .collect();
        v.sort();
        Ok(v)
    }

    /// Return this owner's unfinished claims to the inbox (clean shutdown),
    /// so the next daemon does not wait for them to go stale.
    ///
    /// # Errors
    /// The claim directory cannot be read.
    pub fn release_claims(&self, owner: &str) -> io::Result<usize> {
        let mut n = 0usize;
        for p in self.own_claims(owner)? {
            if let Some(name) = p.file_name() {
                if fs::rename(&p, self.dir.join(name)).is_ok() {
                    n += 1;
                }
            }
        }
        let _empty_dir = fs::remove_dir(self.claim_dir(owner));
        Ok(n)
    }

    fn claim_dir(&self, owner: &str) -> PathBuf {
        self.dir.join(CLAIMED_DIR).join(owner)
    }

    /// Claim `path` for `owner` (a daemon run key). `None` when another
    /// process claimed it first.
    ///
    /// # Errors
    /// Any rename failure other than "already gone".
    pub fn claim(&self, path: &Path, owner: &str) -> io::Result<Option<PathBuf>> {
        let dir = self.claim_dir(owner);
        crate::paths::create_private_dir(&dir)?;
        let Some(name) = path.file_name() else {
            return Ok(None);
        };
        let dest = dir.join(name);
        match fs::rename(path, &dest) {
            Ok(()) => {
                // Stamp the claim time: staleness is judged from it, and a
                // record written long ago (daemon was down) must not look
                // abandoned the moment it is claimed.
                if let Ok(f) = fs::OpenOptions::new().write(true).open(&dest) {
                    let _best_effort = f.set_modified(SystemTime::now());
                }
                Ok(Some(dest))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Read a claimed record, bounded.
    ///
    /// # Errors
    /// Unreadable, oversized, or not a valid record of a known format.
    pub fn read(path: &Path) -> Result<SpoolRecord, SpoolError> {
        let f = fs::File::open(path)?;
        let mut bytes = Vec::new();
        f.take(
            u64::try_from(MAX_RECORD_BYTES)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(SpoolError::Corrupt("record exceeds the size bound".into()));
        }
        let rec: SpoolRecord =
            serde_json::from_slice(&bytes).map_err(|e| SpoolError::Corrupt(e.to_string()))?;
        if rec.format != SPOOL_FORMAT {
            return Err(SpoolError::Corrupt(format!(
                "unsupported spool format {}",
                rec.format
            )));
        }
        Ok(rec)
    }

    /// Remove a claimed record after its events were committed.
    ///
    /// # Errors
    /// The file cannot be removed.
    pub fn complete(path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    /// Move claims older than `older_than` (of any owner other than `keep`)
    /// back into the inbox. Returns how many were recovered.
    ///
    /// # Errors
    /// The claimed directory cannot be read.
    pub fn recover_stale_claims(&self, keep: &str, older_than: Duration) -> io::Result<usize> {
        let root = self.dir.join(CLAIMED_DIR);
        let owners = match fs::read_dir(&root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let now = SystemTime::now();
        let mut recovered = 0usize;
        for owner in owners {
            let owner = owner?;
            if owner.file_name() == keep {
                continue;
            }
            let dir = owner.path();
            let Ok(files) = fs::read_dir(&dir) else {
                continue;
            };
            let mut left = 0usize;
            for f in files.flatten() {
                let age = f
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| now.duration_since(m).ok())
                    .unwrap_or_default();
                if age < older_than {
                    left += 1;
                    continue;
                }
                if fs::rename(f.path(), self.dir.join(f.file_name())).is_ok() {
                    recovered += 1;
                } else {
                    left += 1;
                }
            }
            if left == 0 {
                let _empty_dir_may_race_a_new_claim = fs::remove_dir(&dir);
            }
        }
        Ok(recovered)
    }

    /// A cheap summary for status displays (bounded by [`MAX_SPOOL_FILES`]).
    pub fn stats(&self) -> SpoolStats {
        let mut s = SpoolStats::default();
        if let Ok(rd) = fs::read_dir(&self.dir) {
            for entry in rd.flatten() {
                let name = entry.file_name();
                let Some(n) = name.to_str().filter(|n| is_record(n)) else {
                    continue;
                };
                s.pending_files += 1;
                if let Some(t) = name_time(n) {
                    s.oldest_pending_at = Some(s.oldest_pending_at.map_or(t, |o| o.min(t)));
                }
                if s.pending_files >= MAX_SPOOL_FILES {
                    s.pending_capped = true;
                    break;
                }
            }
        }
        if let Ok(owners) = fs::read_dir(self.dir.join(CLAIMED_DIR)) {
            for o in owners.flatten() {
                if let Ok(files) = fs::read_dir(o.path()) {
                    s.claimed_files += files.count();
                }
            }
        }
        if let Ok(mut f) = fs::File::open(self.dir.join(OVERFLOW_FILE)) {
            s.overflow = Self::read_overflow(&mut f);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Provenance;

    fn spool() -> (tempfile::TempDir, Spool) {
        let t = tempfile::tempdir().unwrap();
        let s = Spool::at(t.path().join("spool"));
        (t, s)
    }

    fn record(n: usize) -> SpoolRecord {
        let mut r = SpoolRecord::new(AgentKind::ClaudeCode);
        for i in 0..n {
            let mut e = ObservabilityEvent::new(
                format!("agent:claude-code:x:{i}"),
                "agent.tool.started",
                Provenance::AgentReported,
                "agent_hook",
                "claude-code",
                Utc::now(),
            );
            e.stable_id = true;
            r.events.push(e);
        }
        r
    }

    #[test]
    fn write_claim_read_complete_round_trip() {
        let (_t, s) = spool();
        let rec = record(2);
        let WriteOutcome::Written(path) = s.write(&rec).unwrap() else {
            panic!("expected a write")
        };
        let pending = s.pending(10).unwrap();
        assert_eq!(pending, vec![path.clone()]);
        let claimed = s.claim(&path, "runA").unwrap().unwrap();
        assert!(
            s.claim(&path, "runB").unwrap().is_none(),
            "exactly one claim wins"
        );
        assert_eq!(Spool::read(&claimed).unwrap(), rec);
        Spool::complete(&claimed).unwrap();
        assert!(s.pending(10).unwrap().is_empty());
        assert_eq!(s.stats().claimed_files, 0);
    }

    #[cfg(unix)]
    #[test]
    fn files_and_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_t, s) = spool();
        let WriteOutcome::Written(path) = s.write(&record(1)).unwrap() else {
            panic!()
        };
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(s.dir()), 0o700);
    }

    #[test]
    fn empty_records_are_not_written() {
        let (_t, s) = spool();
        assert_eq!(
            s.write(&SpoolRecord::new(AgentKind::Codex)).unwrap(),
            WriteOutcome::Empty
        );
    }

    #[test]
    fn overflow_counts_and_resets() {
        let (_t, s) = spool();
        let now = Utc::now();
        s.add_overflow(2, now).unwrap();
        s.add_overflow(3, now).unwrap();
        assert_eq!(s.stats().overflow.count, 5);
        let o = s.take_overflow().unwrap().unwrap();
        assert_eq!(o.count, 5);
        assert!(s.take_overflow().unwrap().is_none());
        s.restore_overflow(&o).unwrap();
        assert_eq!(s.take_overflow().unwrap().unwrap().count, 5);
    }

    #[test]
    fn oversized_records_overflow_instead_of_writing() {
        let (_t, s) = spool();
        let mut rec = record(1);
        rec.events[0].message = Some("x".repeat(MAX_RECORD_BYTES + 1));
        assert_eq!(s.write(&rec).unwrap(), WriteOutcome::Overflowed);
        assert_eq!(s.stats().overflow.count, 1);
        assert_eq!(s.stats().pending_files, 0);
    }

    #[test]
    fn corrupt_and_foreign_records_are_errors() {
        let (_t, s) = spool();
        s.ensure().unwrap();
        let bad = s.dir().join("1-claude-code-x.json");
        fs::write(&bad, b"{not json").unwrap();
        assert!(matches!(Spool::read(&bad), Err(SpoolError::Corrupt(_))));
        let mut v = serde_json::to_value(record(1)).unwrap();
        v["format"] = 99.into();
        fs::write(&bad, serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(matches!(Spool::read(&bad), Err(SpoolError::Corrupt(_))));
    }

    #[test]
    fn stale_claims_are_recovered_for_replay() {
        let (_t, s) = spool();
        let WriteOutcome::Written(path) = s.write(&record(1)).unwrap() else {
            panic!()
        };
        s.claim(&path, "dead-run").unwrap().unwrap();
        assert_eq!(
            s.recover_stale_claims("me", Duration::from_secs(3600))
                .unwrap(),
            0
        );
        assert_eq!(
            s.recover_stale_claims("dead-run", Duration::ZERO).unwrap(),
            0,
            "own claims kept"
        );
        assert_eq!(s.recover_stale_claims("me", Duration::ZERO).unwrap(), 1);
        assert_eq!(s.pending(10).unwrap().len(), 1);
    }

    #[test]
    fn pending_is_ordered_and_limited() {
        let (_t, s) = spool();
        for _ in 0..5 {
            s.write(&record(1)).unwrap();
        }
        let p = s.pending(3).unwrap();
        assert_eq!(p.len(), 3);
        let mut sorted = p.clone();
        sorted.sort();
        assert_eq!(p, sorted);
        assert_eq!(s.stats().pending_files, 5);
        assert!(s.stats().oldest_pending_at.is_some());
    }

    #[test]
    fn concurrent_writers_never_collide() {
        let (_t, s) = spool();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = s.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        assert!(matches!(
                            s.write(&record(1)).unwrap(),
                            WriteOutcome::Written(_)
                        ));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(s.pending(1000).unwrap().len(), 200);
    }
}
