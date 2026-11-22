#![allow(dead_code)]

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tailflow_core::config::StorageConfig;
use tailflow_core::event::{ObservabilityEvent, Provenance};
use tailflow_core::privacy::{CapturePolicy, PrivacyConfig};
use tailflow_core::{LogLevel, LogRecord};
use tailflow_storage::{EventQuery, OpenOptions, Storage};

pub const T: Duration = Duration::from_secs(20);

pub fn config(path: &Path) -> StorageConfig {
    StorageConfig {
        path: Some(path.to_path_buf()),
        batch_max_delay_ms: 5,
        ..StorageConfig::default()
    }
}

pub fn opts(privacy: &PrivacyConfig) -> OpenOptions {
    OpenOptions {
        project_id: Some("/work/app".into()),
        version: "test".into(),
        policy: Arc::new(CapturePolicy::from_config(privacy).unwrap()),
    }
}

pub fn open(path: &Path) -> Storage {
    Storage::open(&config(path), opts(&PrivacyConfig::default())).unwrap()
}

pub fn db_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("events.db")
}

pub fn log_event(n: u64, source: &str, level: LogLevel, msg: &str) -> ObservabilityEvent {
    let record = LogRecord {
        timestamp: Utc::now(),
        source: source.into(),
        level,
        payload: msg.into(),
    };
    ObservabilityEvent::from_log_record(&record, format!("run:{n}"), "process", Some(n))
}

pub fn at(e: ObservabilityEvent, when: DateTime<Utc>) -> ObservabilityEvent {
    let mut e = e;
    e.timestamp = when;
    e.observed_at = when;
    e
}

pub fn ago(days: i64) -> DateTime<Utc> {
    Utc::now() - ChronoDuration::days(days)
}

pub fn agent_event(id: &str, ty: &str, session: &str) -> ObservabilityEvent {
    let mut e = ObservabilityEvent::new(
        id,
        ty,
        Provenance::AgentReported,
        "hook",
        "claude",
        Utc::now(),
    );
    e.session_id = Some(session.into());
    e.agent_name = Some("claude".into());
    e.stable_id = true;
    e
}

/// Every seq in the store, via repeated pages of `page` — exercising the
/// cursor exactly as a client would.
pub fn walk(storage: &Storage, base: &EventQuery, page: usize) -> Vec<u64> {
    let mut seqs = Vec::new();
    let mut after = None;
    loop {
        let q = EventQuery {
            after_seq: after,
            limit: page,
            ..base.clone()
        };
        let p = storage.query(&q).unwrap();
        seqs.extend(p.events.iter().map(|e| e.seq));
        if !p.has_more {
            break;
        }
        after = p.next_cursor;
    }
    seqs
}

pub fn count_rows(path: &Path) -> i64 {
    let c = rusqlite::Connection::open(path).unwrap();
    c.query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap()
}

/// Raw bytes of the database and its WAL — what is actually on disk.
pub fn disk_bytes(path: &Path) -> Vec<u8> {
    let mut out = std::fs::read(path).unwrap_or_default();
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    out.extend(std::fs::read(PathBuf::from(wal)).unwrap_or_default());
    out
}

pub fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}
