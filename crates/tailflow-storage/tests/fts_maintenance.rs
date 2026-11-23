//! Full-text index under continuous size eviction (M1.5-F).
//!
//! Retention deletes the oldest rows; their FTS5 entries become tombstones
//! that only a segment merge removes. These tests pin two properties:
//! search stays exactly correct (only live rows, every live match), and the
//! index stays compact instead of taking the size cap away from history.

mod common;

use common::*;
use rusqlite::Connection;
use std::collections::BTreeSet;
use tailflow_core::config::StorageConfig;
use tailflow_core::privacy::PrivacyConfig;
use tailflow_core::LogLevel;
use tailflow_storage::{EventQuery, Storage};

const WORDS: &[&str] = &[
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo", "sierra", "tango",
];

/// Deterministic message: three vocabulary words, a unique id and filler, so
/// segments hold many distinct tokens (like real logs).
fn message(i: u64) -> String {
    let w = |k: u64| WORDS[usize::try_from((i / k) % WORDS.len() as u64).unwrap_or(0)];
    format!(
        "{} {} {} request id=r{i:08} took {}ms user=u{} path=/api/v{}/items/{}",
        w(1),
        w(7),
        w(31),
        i % 977,
        i % 4099,
        i % 3,
        i % 1013
    )
}

fn fts_bytes(path: &std::path::Path) -> (i64, i64) {
    let c = Connection::open(path).unwrap();
    c.query_row(
        "SELECT (SELECT coalesce(sum(length(block)), 0) FROM events_fts_data),
                (SELECT count(*) FROM events)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

/// Live seqs whose message holds `word` as a token, by scanning the table.
fn scan(path: &std::path::Path, word: &str) -> BTreeSet<u64> {
    let c = Connection::open(path).unwrap();
    let mut stmt = c
        .prepare("SELECT seq, message FROM events WHERE message LIKE '%' || ?1 || '%'")
        .unwrap();
    let rows = stmt
        .query_map([word], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .unwrap();
    rows.filter_map(Result::ok)
        .filter(|(_, m)| m.split_whitespace().any(|t| t == word))
        .map(|(s, _)| u64::try_from(s).unwrap())
        .collect()
}

fn search(s: &Storage, word: &str) -> BTreeSet<u64> {
    walk(
        s,
        &EventQuery {
            text: Some(word.into()),
            ..Default::default()
        },
        500,
    )
    .into_iter()
    .collect()
}

#[test]
fn search_stays_exact_and_index_compact_under_continuous_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    let cfg = StorageConfig {
        max_size_mb: 16,
        cleanup_interval_secs: 86_400,
        ..config(&path)
    };
    let s = Storage::open(&cfg, opts(&PrivacyConfig::default())).unwrap();

    // Fill to the cap, then stream ~3 more caps' worth through it, running
    // retention after every chunk as the writer does under load.
    let mut i = 0u64;
    let mut evicted = 0u64;
    let mut compactions = 0u32;
    let mut worst_bytes_per_event = 0f64;
    let mut samples = Vec::new();
    while i < 160_000 {
        for _ in 0..8_000 {
            let _ = s.submit(log_event(i, "api", LogLevel::Info, &message(i)));
            i += 1;
        }
        s.flush(T).unwrap();
        loop {
            let r = s.run_retention(T).unwrap();
            evicted += r.evicted_by_size;
            if r.fts_compacted {
                assert!(!r.more_work, "compaction runs only when a cycle completes");
                compactions += 1;
            }
            if !r.more_work {
                break;
            }
        }
        if evicted > 0 {
            let (bytes, live) = fts_bytes(&path);
            #[allow(clippy::cast_precision_loss)]
            let per = bytes as f64 / live.max(1) as f64;
            samples.push(per.round());
            worst_bytes_per_event = worst_bytes_per_event.max(per);
        }
    }
    assert!(evicted > 60_000, "the cap must evict heavily: {evicted}");
    eprintln!("FTS bytes per live event after each evicting chunk: {samples:?}");
    assert!(
        compactions > 0,
        "evicting cycles compact the full-text index"
    );
    let h = s.health();
    assert_eq!(h.events_dropped_total, 0, "{h:?}");

    // Exactness: every live match and nothing else (no tombstoned row).
    for word in ["alpha", "kilo", "tango", "november"] {
        let found = search(&s, word);
        let expected = scan(&path, word);
        assert!(!expected.is_empty(), "{word} must still have live matches");
        assert_eq!(
            found, expected,
            "FTS results for {word:?} differ from a scan of live rows"
        );
    }
    assert!(search(&s, "zulu").is_empty());

    // Compactness: a fresh index for these messages is ~70–80 B per row.
    // Without cross-level merging the tombstoned oldest segments were never
    // rewritten and this ratio swung several-fold (and took the history the
    // cap could hold with it).
    assert!(
        worst_bytes_per_event < 150.0,
        "FTS index bytes per live event grew to {worst_bytes_per_event:.0} (samples {samples:?})"
    );
    s.shutdown(T).unwrap();
}
