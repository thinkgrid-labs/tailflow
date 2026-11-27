//! Connection setup and schema migrations.
//!
//! Safety rules:
//! - A file that is not SQLite, not ours, or from a newer schema is reported
//!   and **never modified**. There is no code path that deletes or recreates a
//!   database because opening it failed.
//! - Each migration runs in one transaction together with its
//!   `PRAGMA user_version` bump. SQLite DDL is transactional, so an
//!   interrupted migration leaves the previous version intact.

use crate::error::{Result, StorageError};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::path::Path;
use std::time::Duration;
use tailflow_core::config::SyncMode;

/// `PRAGMA application_id` stamped on every TailFlow database ("TFLW").
pub const APPLICATION_ID: i32 = 0x5446_4c57;

/// Highest schema version this build understands.
pub const SCHEMA_VERSION: i32 = 4;

/// How long a connection waits on a lock held by another connection or
/// process before reporting `SQLITE_BUSY`.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a process waits for the write lock while a schema migration is
/// pending. Another process may be applying the same migration (an index
/// build over millions of rows takes seconds to minutes); failing after
/// [`BUSY_TIMEOUT`] would turn a slow upgrade into a failed start.
pub const MIGRATION_BUSY_TIMEOUT: Duration = Duration::from_secs(300);

pub struct Migration {
    pub version: i32,
    pub name: &'static str,
    pub sql: &'static str,
}

/// Times are integer microseconds since the Unix epoch. `seq` is the
/// persistent cursor: `AUTOINCREMENT` guarantees a value is never reused, even
/// after the newest rows are deleted.
const V1: &str = r"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE runs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    run_key       TEXT NOT NULL UNIQUE,
    started_at    INTEGER NOT NULL,
    heartbeat_at  INTEGER NOT NULL,
    ended_at      INTEGER,
    clean         INTEGER NOT NULL DEFAULT 0,
    last_seq      INTEGER NOT NULL DEFAULT 0,
    version       TEXT NOT NULL,
    pid           INTEGER NOT NULL,
    project_id    TEXT
);

CREATE TABLE events (
    seq                 INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id            TEXT    NOT NULL,
    dedup_key           TEXT,
    schema_version      INTEGER NOT NULL,
    ts                  INTEGER NOT NULL,
    observed_at         INTEGER NOT NULL,
    event_type          TEXT    NOT NULL,
    provenance          TEXT    NOT NULL,
    source_type         TEXT    NOT NULL,
    source_id           TEXT    NOT NULL,
    project_id          TEXT,
    session_id          TEXT,
    operation_id        TEXT,
    parent_operation_id TEXT,
    agent_name          TEXT,
    severity            INTEGER NOT NULL,
    status              TEXT,
    duration_ms         INTEGER,
    path                TEXT,
    message             TEXT,
    attributes          TEXT,
    payload             TEXT,
    redaction           TEXT,
    run_id              INTEGER NOT NULL
);

CREATE UNIQUE INDEX events_dedup   ON events(dedup_key)   WHERE dedup_key  IS NOT NULL;
CREATE INDEX events_observed       ON events(observed_at);
CREATE INDEX events_ts             ON events(ts);
CREATE INDEX events_type           ON events(event_type);
CREATE INDEX events_source         ON events(source_id);
CREATE INDEX events_session        ON events(session_id)  WHERE session_id IS NOT NULL;
CREATE INDEX events_agent          ON events(agent_name)  WHERE agent_name IS NOT NULL;
CREATE INDEX events_project        ON events(project_id)  WHERE project_id IS NOT NULL;

-- A gap is a seq interval in which events may be missing. Loss gaps have
-- from_seq = to_seq = the last seq committed before the loss was detected.
CREATE TABLE gaps (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    reason    TEXT    NOT NULL,
    count     INTEGER,
    first_at  INTEGER NOT NULL,
    last_at   INTEGER NOT NULL,
    from_seq  INTEGER NOT NULL,
    to_seq    INTEGER NOT NULL,
    run_id    INTEGER,
    detail    TEXT
);
CREATE INDEX gaps_to_seq ON gaps(to_seq);

-- Contentless: the index stores tokens only, the text lives in `events`.
CREATE VIRTUAL TABLE events_fts USING fts5(
    message, content = '', contentless_delete = 1, tokenize = 'unicode61'
);

INSERT INTO meta(key, value) VALUES ('event_count', '0');
INSERT INTO meta(key, value) VALUES ('evicted_through_seq', '0');
INSERT INTO meta(key, value) VALUES ('payload_expired_through_seq', '0');
";

/// Schema v2 (M2): agent sessions, and the `events_ts` index dropped.
///
/// - `events_ts` was never chosen by the planner (M1.5 SQLITE-OPTIMIZATION
///   §6: every time filter is translated into a seq window first); dropping
///   it saves 5–8 % WAL bytes per event. `DROP INDEX` only frees pages, so
///   the migration needs no extra disk space.
/// - `agent_sessions` is a per-session summary maintained by the writer in
///   the same transaction as each agent event insert (duplicates excluded),
///   so listing sessions never scans `events`. Session keys are
///   `<agent>:<source id>`. Rows outlive partial expiry of their events
///   (readers compare `first_seq` with `evicted_through_seq`) and are
///   deleted when every event they summarize has expired or was purged.
///
/// Rollback: a build that only knows v1 refuses a v2 file (`NewerSchema`)
/// and runs memory-only; it never modifies it. A v1 daemon that keeps a
/// database open while a v2 daemon migrates it continues to append events
/// (it does not ingest agent events, so `agent_sessions` stays correct; its
/// retention leaves expired session rows behind, which readers filter out).
const V2: &str = r"
DROP INDEX IF EXISTS events_ts;

CREATE TABLE agent_sessions (
    session_key       TEXT PRIMARY KEY,
    agent_name        TEXT    NOT NULL,
    project_id        TEXT,
    working_directory TEXT,
    first_seq         INTEGER NOT NULL,
    last_seq          INTEGER NOT NULL,
    first_event_at    INTEGER NOT NULL,
    last_event_at     INTEGER NOT NULL,
    event_count       INTEGER NOT NULL DEFAULT 0,
    started_at        INTEGER,
    last_started_at   INTEGER,
    start_count       INTEGER NOT NULL DEFAULT 0,
    start_source      TEXT,
    ended_at          INTEGER,
    end_count         INTEGER NOT NULL DEFAULT 0,
    end_reason        TEXT,
    tool_calls        INTEGER NOT NULL DEFAULT 0,
    tool_failures     INTEGER NOT NULL DEFAULT 0,
    commands          INTEGER NOT NULL DEFAULT 0,
    command_failures  INTEGER NOT NULL DEFAULT 0,
    file_events       INTEGER NOT NULL DEFAULT 0,
    subagents         INTEGER NOT NULL DEFAULT 0,
    turns             INTEGER NOT NULL DEFAULT 0,
    tokens_total      INTEGER,
    git_root          TEXT,
    git_branch        TEXT,
    git_head_start    TEXT,
    git_head_end      TEXT,
    git_worktree      TEXT,
    model             TEXT
) WITHOUT ROWID;

CREATE INDEX agent_sessions_recent   ON agent_sessions(last_event_at);
CREATE INDEX agent_sessions_last_seq ON agent_sessions(last_seq);
";

/// Schema v3 (M3): a partial covering index over agent events, for the
/// dashboard aggregates (`/api/v1/stats`, `/api/v1/files`).
///
/// Counting agent activity by event time over days of history otherwise
/// costs one random table lookup per agent event (`ts`, `agent_name`,
/// `project_id` are not in `events_type`): ~140k lookups for a 7-day range on
/// a 2M-event store, which blew the 3 s query budget on a cold cache. With
/// `(event_type, ts, agent_name, project_id, session_id, path)` the overview
/// counts and the per-path file aggregation are index-only.
///
/// Partial (`agent_name IS NOT NULL`): runtime logs — the bulk of the table —
/// are not in it, so ingestion of runtime lines is unaffected; each agent
/// event adds one index entry. Building it scans the table once, at upgrade.
///
/// Rollback: a v2 build refuses a v3 file (`NewerSchema`) and runs
/// memory-only without modifying it.
const V3: &str = r"
CREATE INDEX IF NOT EXISTS events_agent_activity
    ON events(event_type, ts, agent_name, project_id, session_id, path)
    WHERE agent_name IS NOT NULL;
";

/// Schema v4 (M4): filesystem observation coverage and a path index for
/// evidence correlation.
///
/// - `fs_coverage`: one row per observed root per daemon run (started,
///   stopped, watch backend, directories watched and over budget). It says
///   whether a path *could* have been observed at a time, which separates
///   "reported but not observed" from "not verifiable". Rows of runs that
///   died are closed at their last heartbeat by dead-run recovery.
/// - `events_path_ts`: partial index over filesystem observations and agent
///   hook events that carry a path, for "what happened to this path near
///   this time" lookups. Runtime log lines have no path and are not in it,
///   so runtime ingestion is unaffected. Building it scans the table once.
/// - `events_fs_ts`: partial index over filesystem observations by time
///   (observations are not in the agent-activity index).
///
/// Rollback: a v3 build refuses a v4 file (`NewerSchema`) and runs
/// memory-only without modifying it.
const V4: &str = r"
CREATE TABLE fs_coverage (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    root           TEXT    NOT NULL,
    run_id         INTEGER NOT NULL,
    started_at     INTEGER NOT NULL,
    stopped_at     INTEGER,
    backend        TEXT    NOT NULL,
    watched_dirs   INTEGER NOT NULL DEFAULT 0,
    unwatched_dirs INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX fs_coverage_root ON fs_coverage(root, started_at);

CREATE INDEX IF NOT EXISTS events_path_ts
    ON events(path, ts)
    WHERE path IS NOT NULL AND source_type IN ('fs_watcher', 'agent_hook');

CREATE INDEX IF NOT EXISTS events_fs_ts
    ON events(ts)
    WHERE source_type = 'fs_watcher';
";

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial event store",
        sql: V1,
    },
    Migration {
        version: 2,
        name: "agent sessions; drop unused events_ts index",
        sql: V2,
    },
    Migration {
        version: 3,
        name: "agent activity index for dashboard aggregates",
        sql: V3,
    },
    Migration {
        version: 4,
        name: "filesystem coverage and path evidence index",
        sql: V4,
    },
];

/// Open (creating if needed) a read-write connection and bring the schema
/// up to date.
pub fn open_writer(path: &Path, sync: SyncMode) -> Result<Connection> {
    open_writer_with(path, sync, MIGRATIONS)
}

pub(crate) fn open_writer_with(
    path: &Path,
    sync: SyncMode,
    migrations: &[Migration],
) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        crate::paths::create_private_dir(parent).map_err(|e| StorageError::CannotOpen {
            path: path.into(),
            detail: e.to_string(),
        })?;
    }
    if path.is_dir() {
        return Err(StorageError::CannotOpen {
            path: path.into(),
            detail: "path is a directory".into(),
        });
    }
    create_private_file(path).map_err(|e| StorageError::CannotOpen {
        path: path.into(),
        detail: e.to_string(),
    })?;
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| StorageError::from_open(path, e))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| StorageError::from_open(path, e))?;

    // First real read of the file: a non-SQLite file fails here.
    //
    // One statement, so one read snapshot. Another process may be creating
    // this file right now: it writes page 1 (auto_vacuum), then stamps the
    // application id, then creates the tables. Separate reads could see the
    // unstamped page 1 and then the new tables, and refuse our own database
    // as foreign. In a single snapshot, tables are only ever visible
    // together with the stamp.
    let (page_count, app_id, user_version, has_tables): (i64, i32, i32, i64) = conn
        .query_row(
            "SELECT (SELECT page_count FROM pragma_page_count()), \
                    (SELECT application_id FROM pragma_application_id()), \
                    (SELECT user_version FROM pragma_user_version()), \
                    (SELECT count(*) FROM sqlite_master)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|e| StorageError::from_open(path, e))?;

    if page_count == 0 {
        // Brand-new file. auto_vacuum must be chosen before the first table.
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.pragma_update(None, "application_id", APPLICATION_ID)?;
    } else if app_id != APPLICATION_ID {
        if app_id != 0 || has_tables > 0 || user_version != 0 {
            return Err(StorageError::ForeignDatabase {
                path: path.into(),
                application_id: app_id,
            });
        }
        conn.pragma_update(None, "application_id", APPLICATION_ID)?;
    }

    let latest = migrations.iter().map(|m| m.version).max().unwrap_or(0);
    if user_version > latest {
        return Err(StorageError::NewerSchema {
            found: user_version,
            supported: latest,
        });
    }

    let mode = enable_wal(&conn).map_err(|e| StorageError::from_open(path, e))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(journal_mode = %mode, "SQLite refused WAL mode; continuing with {mode}");
    }
    conn.pragma_update(
        None,
        "synchronous",
        match sync {
            SyncMode::Normal => "NORMAL",
            SyncMode::Full => "FULL",
        },
    )?;
    // Truncate the WAL back to this size after checkpoints.
    conn.pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -16_000)?; // ~16 MB
    conn.set_prepared_statement_cache_capacity(32);

    migrate(&mut conn, migrations)?;
    Ok(conn)
}

/// Switch the database to WAL and return the journal mode SQLite reports.
///
/// Leaving rollback mode needs an exclusive lock. When another connection
/// holds the file (a second daemon opening the same new database), SQLite
/// returns `SQLITE_BUSY` at once without calling the busy handler, so the
/// connection's busy timeout does not apply. Retry with a short backoff for
/// up to [`BUSY_TIMEOUT`]. Once any connection has switched the file, WAL is
/// persistent and this statement no longer needs the exclusive lock.
fn enable_wal(conn: &Connection) -> rusqlite::Result<String> {
    let deadline = std::time::Instant::now() + BUSY_TIMEOUT;
    let mut pause = Duration::from_millis(1);
    loop {
        match conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)) {
            Err(e)
                if matches!(
                    e.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

/// Create a new database file readable only by its owner, so a custom path
/// in a shared directory is not world-readable under a 022 umask. SQLite
/// gives the `-wal` and `-shm` files the main file's permissions. Existing
/// files are left exactly as they are.
fn create_private_file(path: &Path) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Apply every migration newer than the database's `user_version`.
///
/// The version is re-read inside each migration's write transaction, so two
/// daemons opening a fresh shared database at the same moment cannot both
/// apply the same migration.
///
/// A newer build may migrate the same new file past this build's schema
/// after the caller's identity check. Every re-read refuses that, so this
/// build never writes into a database from a newer schema.
pub(crate) fn migrate(conn: &mut Connection, migrations: &[Migration]) -> Result<i32> {
    let latest = migrations.iter().map(|m| m.version).max().unwrap_or(0);
    let mut current: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current > latest {
        return Err(StorageError::NewerSchema {
            found: current,
            supported: latest,
        });
    }
    if migrations.iter().all(|m| m.version <= current) {
        return Ok(current);
    }
    conn.busy_timeout(MIGRATION_BUSY_TIMEOUT)?;
    let result = migrate_pending(conn, migrations, latest, &mut current);
    // Back to the normal wait on every exit path.
    conn.busy_timeout(BUSY_TIMEOUT)?;
    result.map(|()| current)
}

fn migrate_pending(
    conn: &mut Connection,
    migrations: &[Migration],
    latest: i32,
    current: &mut i32,
) -> Result<()> {
    for m in migrations {
        if m.version <= *current {
            continue;
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now: i32 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if now > latest {
            // Dropping `tx` rolls back; nothing was written.
            return Err(StorageError::NewerSchema {
                found: now,
                supported: latest,
            });
        }
        if now >= m.version {
            // Another process applied it while we waited for the lock.
            tx.commit()?;
            *current = now;
            continue;
        }
        // Index builds scan the whole table once; on a multi-gigabyte
        // database that can take tens of seconds. Say so before starting,
        // so a slow first start is not mistaken for a hang.
        let rows: i64 = tx
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'event_count'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if rows > 100_000 {
            eprintln!(
                "tailflow: upgrading the event store to schema v{} ({}) over ~{rows} events; \
                 this runs once and may take a while on a large database",
                m.version, m.name
            );
        }
        let started = std::time::Instant::now();
        tx.execute_batch(m.sql).map_err(|e| {
            StorageError::Config(format!(
                "migration {} ({}) failed and was rolled back: {e}",
                m.version, m.name
            ))
        })?;
        tx.pragma_update(None, "user_version", m.version)?;
        tx.commit()?;
        tracing::info!(
            version = m.version,
            name = m.name,
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "applied storage migration"
        );
        if rows > 100_000 {
            eprintln!(
                "tailflow: schema v{} applied in {:.1} s",
                m.version,
                started.elapsed().as_secs_f64()
            );
        }
        *current = m.version;
    }
    Ok(())
}

/// A connection for queries. Writes are refused at the SQLite level.
pub fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| StorageError::from_open(path, e))?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.pragma_update(None, "query_only", "ON")?;
    conn.pragma_update(None, "cache_size", -8_000)?;
    conn.set_prepared_statement_cache_capacity(32);
    Ok(conn)
}

/// `PRAGMA quick_check` (or `integrity_check` when `full`). Returns the list
/// of problems; empty means healthy.
pub fn integrity_problems(conn: &Connection, full: bool) -> Result<Vec<String>> {
    let pragma = if full {
        "PRAGMA integrity_check(100)"
    } else {
        "PRAGMA quick_check(100)"
    };
    let mut stmt = conn.prepare(pragma)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut problems = Vec::new();
    for row in rows {
        let line = row?;
        if line != "ok" {
            problems.push(line);
        }
    }
    Ok(problems)
}

pub(crate) fn meta_get_i64(conn: &Connection, key: &str) -> Result<i64> {
    let v: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(v.and_then(|s| s.parse().ok()).unwrap_or(0))
}

pub(crate) fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other.into()),
        })
}

pub(crate) fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;
    Ok(())
}

/// Add `delta` to an integer meta value, never going below zero.
pub(crate) fn meta_add(conn: &Connection, key: &str, delta: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, max(0, ?2))
         ON CONFLICT(key) DO UPDATE SET value = CAST(max(0, CAST(value AS INTEGER) + ?2) AS TEXT)",
        rusqlite::params![key, delta],
    )?;
    Ok(())
}

/// Raise an integer meta value to at least `value`.
pub(crate) fn meta_max(conn: &Connection, key: &str, value: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = CAST(max(CAST(value AS INTEGER), ?2) AS TEXT)",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_schema_and_stamps_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/events.db");
        let conn = open_writer(&path, SyncMode::Normal).unwrap();
        let v: i32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let id: i32 = conn
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .unwrap();
        assert_eq!(id, APPLICATION_ID);
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let av: i32 = conn
            .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
            .unwrap();
        assert_eq!(av, 2, "incremental auto_vacuum");
        drop(conn);
        // Reopening is idempotent.
        open_writer(&path, SyncMode::Normal).unwrap();
    }

    #[test]
    fn garbage_file_is_reported_and_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let garbage = b"this is not a database, it is a shopping list\n".repeat(200);
        std::fs::write(&path, &garbage).unwrap();
        let err = open_writer(&path, SyncMode::Normal).unwrap_err();
        assert!(
            matches!(err, StorageError::NotADatabase { .. }),
            "got {err:?}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            garbage,
            "file must be untouched"
        );
    }

    #[test]
    fn foreign_sqlite_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE customers(id INTEGER PRIMARY KEY);")
                .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let err = open_writer(&path, SyncMode::Normal).unwrap_err();
        assert!(
            matches!(err, StorageError::ForeignDatabase { .. }),
            "{err:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn newer_schema_is_refused_without_modification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        drop(open_writer(&path, SyncMode::Normal).unwrap());
        {
            let c = Connection::open(&path).unwrap();
            c.pragma_update(None, "user_version", SCHEMA_VERSION + 5)
                .unwrap();
        }
        let err = open_writer(&path, SyncMode::Normal).unwrap_err();
        assert!(
            matches!(err, StorageError::NewerSchema { found, .. } if found == SCHEMA_VERSION + 5),
            "{err:?}"
        );
    }

    #[test]
    fn a_schema_that_becomes_newer_before_migration_is_refused() {
        // The race: this build's identity check saw an older schema, then a
        // newer build migrated the file before `migrate` ran.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        drop(open_writer_with(&path, SyncMode::Normal, &MIGRATIONS[..1]).unwrap());
        let mut c = Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let tables_before: i64 = c
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
            .unwrap();
        // Fast path (nothing pending from this build's point of view)...
        let err = migrate(&mut c, MIGRATIONS).unwrap_err();
        assert!(
            matches!(err, StorageError::NewerSchema { found, supported }
                if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION),
            "{err:?}"
        );
        // ...and the re-read inside a pending migration's transaction.
        let mut current = 1;
        let err = migrate_pending(&mut c, MIGRATIONS, SCHEMA_VERSION, &mut current).unwrap_err();
        assert!(matches!(err, StorageError::NewerSchema { .. }), "{err:?}");
        let (v, tables_after): (i32, i64) = c
            .query_row(
                "SELECT (SELECT user_version FROM pragma_user_version()), \
                        (SELECT count(*) FROM sqlite_master)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION + 1, "the newer version is left alone");
        assert_eq!(tables_after, tables_before, "no migration was applied");
    }

    #[test]
    fn interrupted_migration_rolls_back_completely() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        drop(open_writer_with(&path, SyncMode::Normal, &MIGRATIONS[..1]).unwrap());
        let broken = [
            MIGRATIONS[0].clone_ref(),
            Migration {
                version: 2,
                name: "half-applied",
                sql: "CREATE TABLE new_thing(x INTEGER); INSERT INTO nonexistent VALUES (1);",
            },
        ];
        let err = open_writer_with(&path, SyncMode::Normal, &broken).unwrap_err();
        assert!(err.to_string().contains("rolled back"), "{err}");
        let c = Connection::open(&path).unwrap();
        let v: i32 = c
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1, "version must not advance");
        let exists: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'new_thing'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 0, "partial DDL must be rolled back");
    }

    #[test]
    fn v1_database_upgrades_to_current_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        {
            let c = open_writer_with(&path, SyncMode::Normal, &MIGRATIONS[..1]).unwrap();
            c.execute(
                "INSERT INTO events (event_id, schema_version, ts, observed_at, event_type, provenance,
                 source_type, source_id, severity, run_id)
                 VALUES ('e1', 1, 1, 1, 'runtime.log', 'runtime_observed', 'p', 's', 0, 1)",
                [],
            )
            .unwrap();
        }
        let c = open_writer(&path, SyncMode::Normal).unwrap();
        let v: i32 = c
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let n: i64 = c
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "existing history is kept");
        let activity: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'events_agent_activity'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(activity, 1, "v3 agent activity index created");
        for name in ["fs_coverage", "events_path_ts", "events_fs_ts"] {
            let n: i64 = c
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                    [name],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "v4 {name} created");
        }
        let idx: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'events_ts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 0, "unused index dropped");
        let t: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'agent_sessions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(t, 1);
        assert!(integrity_problems(&c, true).unwrap().is_empty());
        // A v1-only build refuses the upgraded file without touching it.
        drop(c);
        let err = open_writer_with(&path, SyncMode::Normal, &MIGRATIONS[..1]).unwrap_err();
        assert!(
            matches!(
                err,
                StorageError::NewerSchema {
                    found: SCHEMA_VERSION,
                    supported: 1
                }
            ),
            "{err:?}"
        );
    }

    /// Every older schema upgrades to the current one with its history, and
    /// the evidence index is usable for path lookups afterwards.
    #[test]
    fn v2_and_v3_databases_upgrade_to_v4_keeping_history() {
        for from in [2usize, 3] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("events.db");
            {
                let c = open_writer_with(&path, SyncMode::Normal, &MIGRATIONS[..from]).unwrap();
                for (i, (st, p)) in [
                    ("agent_hook", Some("/w/a.rs")),
                    ("process", None),
                    ("agent_hook", None),
                ]
                .iter()
                .enumerate()
                {
                    c.execute(
                        "INSERT INTO events (event_id, schema_version, ts, observed_at, event_type, provenance,
                         source_type, source_id, severity, run_id, path)
                         VALUES (?1, 1, ?2, ?2, 'file.modified', 'agent_reported', ?3, 's', 0, 1, ?4)",
                        rusqlite::params![format!("e{i}"), 1_000 + i64::try_from(i).unwrap(), st, p],
                    )
                    .unwrap();
                }
            }
            let c = open_writer(&path, SyncMode::Normal).unwrap();
            let v: i32 = c
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .unwrap();
            assert_eq!(v, SCHEMA_VERSION, "from v{from}");
            let n: i64 = c
                .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 3, "history kept from v{from}");
            // The partial index holds only path-carrying fs/agent rows.
            let indexed: i64 = c
                .query_row(
                    "SELECT count(*) FROM events INDEXED BY events_path_ts
                     WHERE path IS NOT NULL AND source_type IN ('fs_watcher', 'agent_hook')
                       AND path = '/w/a.rs'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(indexed, 1);
            assert!(integrity_problems(&c, true).unwrap().is_empty());
            drop(c);
            let err = open_writer_with(&path, SyncMode::Normal, &MIGRATIONS[..3]).unwrap_err();
            assert!(
                matches!(
                    err,
                    StorageError::NewerSchema {
                        found: 4,
                        supported: 3
                    }
                ),
                "{err:?}"
            );
        }
    }

    #[test]
    fn directory_path_and_unwritable_parent_fail_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let err = open_writer(dir.path(), SyncMode::Normal).unwrap_err();
        assert!(matches!(err, StorageError::CannotOpen { .. }), "{err:?}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let ro = dir.path().join("ro");
            std::fs::create_dir(&ro).unwrap();
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
            // Root (e.g. a CI container) bypasses directory permissions; the
            // read-only case cannot be produced there.
            if std::fs::write(ro.join("probe"), b"").is_ok() {
                std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
                eprintln!("skipped: directory permissions are not enforced for this user");
                return;
            }
            let err = open_writer(&ro.join("events.db"), SyncMode::Normal).unwrap_err();
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(
                matches!(
                    err,
                    StorageError::CannotOpen { .. } | StorageError::ReadOnly { .. }
                ),
                "{err:?}"
            );
        }
    }

    #[test]
    fn concurrent_first_open_applies_the_migration_once() {
        // Several daemons starting together on a new data directory. Before
        // the single-snapshot identity read and the WAL retry, about 4 % of
        // rounds here had an opener fail with `ForeignDatabase` or
        // "database is locked". The barrier lines the openers up so the
        // windows are actually hit.
        for _ in 0..60 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("events.db");
            let start = std::sync::Arc::new(std::sync::Barrier::new(8));
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let p = path.clone();
                    let start = std::sync::Arc::clone(&start);
                    std::thread::spawn(move || {
                        start.wait();
                        open_writer(&p, SyncMode::Normal).map(drop)
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap().expect("every concurrent opener succeeds");
            }
            let conn = open_writer(&path, SyncMode::Normal).unwrap();
            let v: i32 = conn
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .unwrap();
            assert_eq!(v, SCHEMA_VERSION);
            let mode: String = conn
                .pragma_query_value(None, "journal_mode", |r| r.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
        }
    }

    #[cfg(unix)]
    #[test]
    fn new_database_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        drop(open_writer(&path, SyncMode::Normal).unwrap());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn meta_helpers_clamp_and_raise() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_writer(&dir.path().join("e.db"), SyncMode::Normal).unwrap();
        meta_add(&conn, "event_count", 5).unwrap();
        meta_add(&conn, "event_count", -9).unwrap();
        assert_eq!(meta_get_i64(&conn, "event_count").unwrap(), 0);
        meta_max(&conn, "evicted_through_seq", 7).unwrap();
        meta_max(&conn, "evicted_through_seq", 3).unwrap();
        assert_eq!(meta_get_i64(&conn, "evicted_through_seq").unwrap(), 7);
    }

    impl Migration {
        fn clone_ref(&self) -> Migration {
            Migration {
                version: self.version,
                name: self.name,
                sql: self.sql,
            }
        }
    }
}
