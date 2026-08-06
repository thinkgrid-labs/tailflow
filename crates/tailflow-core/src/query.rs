//! Agent-facing read layer over the log buffer.
//!
//! The TUI and web dashboard consume the live broadcast bus — a human watches
//! records scroll past. An agent cannot. It asks bounded questions after the
//! fact ("what broke in the last two minutes?") and pays for every token of
//! the answer, so this module optimises for a different shape:
//!
//! - **Retrospective** — [`LogStore`] keeps a ring buffer that can be queried.
//! - **Deduplicated** — [`LogStore::summarize`] collapses a 400-line error
//!   flood into the handful of *distinct* failures behind it.
//! - **Bounded** — every result carries a limit and reports whether it
//!   truncated, so a caller never blows its context on one query.

use crate::{processor::Filter, LogLevel, LogRecord};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// Records retained for retrospective queries.
///
/// Larger than the 500 the web dashboard renders: an agent asking "what
/// happened since I edited that file" may be looking several minutes back
/// through a chatty stack.
pub const DEFAULT_CAPACITY: usize = 5_000;

/// Default cap on a single record's payload before it is elided. Protects the
/// caller's context window from a single 50 KB JSON blob.
pub const DEFAULT_MAX_PAYLOAD_CHARS: usize = 2_000;

/// Trailing continuation lines captured after a representative error.
pub const DEFAULT_CONTEXT_LINES: usize = 8;

// ── Wire types ────────────────────────────────────────────────────────────────

/// A record plus its store sequence number.
///
/// `seq` is monotonic per store and is what makes incremental polling exact:
/// pass the previous response's `next_cursor` back as `cursor` and you get
/// precisely the records that arrived since, with no timestamp-collision
/// double-reads or gaps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeqRecord {
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    pub source: String,
    pub level: LogLevel,
    pub payload: String,
    /// Present only when `payload` was elided; the original character count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_truncated_from: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub records: Vec<SeqRecord>,
    /// Total records matching the filter, before `limit` was applied.
    pub total_matching: usize,
    /// `true` when `total_matching` exceeded `limit` and older matches were
    /// dropped. Narrow the filter or raise the limit.
    pub truncated: bool,
    /// Pass back as `cursor` to fetch only what arrives after this call.
    pub next_cursor: u64,
}

/// One distinct failure, with every occurrence folded into it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorGroup {
    /// Normalised template that identifies this group — variable parts
    /// (numbers, UUIDs, hex, quoted strings, paths) replaced by placeholders.
    pub fingerprint: String,
    /// How many records collapsed into this group.
    pub count: usize,
    /// Sources this failure appeared on, most frequent first.
    pub sources: Vec<String>,
    /// Highest severity seen in the group.
    pub level: LogLevel,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// Verbatim payload of the most recent occurrence.
    pub sample: String,
    /// Continuation lines that followed the most recent occurrence on the same
    /// source — typically the stack trace. Empty when the log is single-line.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    /// Distinct failures, most recently seen first.
    pub groups: Vec<ErrorGroup>,
    /// Records matching the filter across all groups.
    pub total_matching: usize,
    /// Distinct groups found, before `limit` was applied to `groups`.
    pub distinct: usize,
    pub truncated: bool,
    /// Oldest record held by the store — the horizon this answer can see back to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer_starts_at: Option<DateTime<Utc>>,
    pub next_cursor: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceStat {
    pub name: String,
    pub total: usize,
    pub errors: usize,
    pub warns: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
    /// Most recent line, elided to a single short preview. Lets a caller tell
    /// "compiled successfully" from "waiting for postgres" without a second query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_line: Option<String>,
}

// ── Query ─────────────────────────────────────────────────────────────────────

pub struct Query {
    pub filter: Filter,
    /// Maximum records (or groups) returned. Newest are kept when truncating.
    pub limit: usize,
    /// Return only records with `seq > cursor`.
    pub cursor: Option<u64>,
    pub max_payload_chars: usize,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            filter: Filter::none(),
            limit: 100,
            cursor: None,
            max_payload_chars: DEFAULT_MAX_PAYLOAD_CHARS,
        }
    }
}

impl Query {
    pub fn new(filter: Filter) -> Self {
        Self {
            filter,
            ..Self::default()
        }
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub fn with_cursor(mut self, cursor: Option<u64>) -> Self {
        self.cursor = cursor;
        self
    }

    pub fn with_max_payload_chars(mut self, n: usize) -> Self {
        self.max_payload_chars = n;
        self
    }
}

/// Parse a `since` argument: either an RFC 3339 timestamp or a relative
/// duration such as `30s`, `5m`, `2h`, `1d`.
///
/// Relative forms exist because they are what a caller actually knows — an
/// agent knows it edited a file "about a minute ago", not the wall-clock
/// instant it did so.
pub fn parse_since(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    if let Ok(ts) = DateTime::parse_from_rfc3339(s) {
        return Some(ts.with_timezone(&Utc));
    }

    let (value, unit) = s.split_at(s.len() - 1);
    let n: i64 = value.parse().ok()?;
    if n < 0 {
        return None;
    }
    let delta = match unit {
        "s" => Duration::try_seconds(n)?,
        "m" => Duration::try_minutes(n)?,
        "h" => Duration::try_hours(n)?,
        "d" => Duration::try_days(n)?,
        _ => return None,
    };
    Utc::now().checked_sub_signed(delta)
}

// ── Fingerprinting ────────────────────────────────────────────────────────────

/// Collapse a log line to a template that identifies the *kind* of event,
/// discarding the parts that vary between occurrences.
///
/// This is what turns "417 error lines" into "3 distinct failures". Order is
/// deliberate: the most specific patterns are replaced first, so a UUID is not
/// half-eaten by the number rule before it is recognised.
pub fn fingerprint(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len());
    let bytes: Vec<char> = payload.chars().collect();
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes[i];

        // Quoted strings → "<S>". Differing identifiers inside quotes
        // ("module 'foo'" vs "module 'bar'") are the same failure.
        if c == '"' || c == '\'' || c == '`' {
            if let Some(end) = find_close(&bytes, i, c) {
                out.push_str("<S>");
                i = end + 1;
                continue;
            }
        }

        if c.is_ascii_digit() || (c == '0' && peek(&bytes, i + 1) == Some('x')) {
            let start = i;
            let mut j = i;
            // Consume a run that may be a timestamp, UUID, hex, version, or number.
            while j < bytes.len() && is_token_char(bytes[j]) {
                j += 1;
            }
            let token: String = bytes[start..j].iter().collect();
            out.push_str(classify_token(&token));
            i = j;
            continue;
        }

        // A hex/uuid run can also start with a letter (a3f9-..., deadbeef).
        // Only *consume* the run when it really is an id — otherwise fall
        // through one character at a time, or the `e` in `postgres:5432`
        // would swallow the port and emit it verbatim.
        if c.is_ascii_hexdigit() && c.is_ascii_alphabetic() {
            let start = i;
            let mut j = i;
            while j < bytes.len() && is_token_char(bytes[j]) {
                j += 1;
            }
            let token: String = bytes[start..j].iter().collect();
            if is_uuid(&token) {
                out.push_str("<UUID>");
                i = j;
                continue;
            }
            if is_hex_id(&token) {
                out.push_str("<HEX>");
                i = j;
                continue;
            }
        }

        // Collapse whitespace runs so indentation differences do not split groups.
        if c.is_whitespace() {
            while i < bytes.len() && bytes[i].is_whitespace() {
                i += 1;
            }
            out.push(' ');
            continue;
        }

        out.push(c);
        i += 1;
    }

    let trimmed = out.trim();
    // Cap the key so a pathological line cannot bloat the group map.
    if trimmed.chars().count() > 300 {
        trimmed.chars().take(300).collect()
    } else {
        trimmed.to_string()
    }
}

fn peek(chars: &[char], i: usize) -> Option<char> {
    chars.get(i).copied()
}

fn find_close(chars: &[char], open: usize, quote: char) -> Option<usize> {
    // Bounded scan: an unterminated quote must not swallow the whole line.
    let limit = (open + 200).min(chars.len());
    (open + 1..limit).find(|&j| chars[j] == quote)
}

/// Characters that can appear inside a single "variable" token — digits,
/// hex letters, and the separators used by timestamps, UUIDs, versions and
/// addresses.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | ':' | '.' | '_' | '+')
}

fn is_uuid(token: &str) -> bool {
    let t = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
    let parts: Vec<&str> = t.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12] == core::array::from_fn::<usize, 5, _>(|i| parts[i].len())
        && t.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn is_hex_id(token: &str) -> bool {
    let t = token.trim_start_matches("0x");
    t.len() >= 8 && t.chars().all(|c| c.is_ascii_hexdigit())
}

fn classify_token(token: &str) -> &'static str {
    if is_uuid(token) {
        "<UUID>"
    } else if is_hex_id(token) {
        "<HEX>"
    } else if token.contains(':') && token.matches(':').count() >= 2 {
        // 10:23:45.123 or 127.0.0.1:5432:x — time or address
        "<TIME>"
    } else if token.contains('T') && token.contains(':') {
        "<TS>"
    } else if token.matches('.').count() >= 2 || token.contains(':') {
        // 1.2.3 version, 127.0.0.1, host:port
        "<ADDR>"
    } else {
        "<N>"
    }
}

// ── Store ─────────────────────────────────────────────────────────────────────

struct Entry {
    seq: u64,
    record: LogRecord,
}

/// Ring buffer of recent records, queryable after the fact.
///
/// Cloneable handles are not provided — wrap in an `Arc`. All methods take
/// `&self` and lock internally; a poisoned lock is recovered rather than
/// propagated, since a panicked writer must not take the read path down with it.
pub struct LogStore {
    ring: Mutex<VecDeque<Entry>>,
    capacity: usize,
    next_seq: Mutex<u64>,
}

impl LogStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            ring: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
            capacity,
            next_seq: Mutex::new(1),
        }
    }

    pub fn push(&self, record: LogRecord) {
        let seq = {
            let mut n = self.next_seq.lock().unwrap_or_else(|p| p.into_inner());
            let seq = *n;
            *n += 1;
            seq
        };
        let mut ring = self.ring.lock().unwrap_or_else(|p| p.into_inner());
        if ring.len() >= self.capacity {
            ring.pop_front();
        }
        ring.push_back(Entry { seq, record });
    }

    pub fn len(&self) -> usize {
        self.ring.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Highest sequence issued so far — the cursor a caller should start from
    /// if it only wants records from now on.
    pub fn cursor(&self) -> u64 {
        self.next_seq
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .saturating_sub(1)
    }

    /// Most recent `limit` records, oldest-first (reading order).
    pub fn records(&self, q: &Query) -> QueryResult {
        let ring = self.ring.lock().unwrap_or_else(|p| p.into_inner());
        let matched: Vec<&Entry> = ring
            .iter()
            .filter(|e| q.cursor.is_none_or(|c| e.seq > c))
            .filter(|e| q.filter.matches(&e.record))
            .collect();

        let total_matching = matched.len();
        let skip = total_matching.saturating_sub(q.limit);
        let records: Vec<SeqRecord> = matched
            .iter()
            .skip(skip)
            .map(|e| to_seq_record(e, q.max_payload_chars))
            .collect();

        QueryResult {
            records,
            total_matching,
            truncated: skip > 0,
            next_cursor: ring.back().map(|e| e.seq).unwrap_or(0),
        }
    }

    /// Fold matching records into distinct groups, most recently seen first.
    ///
    /// `context_lines` trailing continuation lines are attached to each group's
    /// representative occurrence. A continuation line is an `Unknown`-level,
    /// non-empty record from the same source immediately following it — which
    /// is exactly the shape of an indented stack frame, and why stack traces
    /// survive a `level >= error` filter that would otherwise drop them.
    pub fn summarize(&self, q: &Query, context_lines: usize) -> Summary {
        let ring = self.ring.lock().unwrap_or_else(|p| p.into_inner());
        let entries: Vec<&Entry> = ring.iter().collect();

        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, GroupAcc> = HashMap::new();
        let mut total_matching = 0usize;

        for (idx, e) in entries.iter().enumerate() {
            if q.cursor.is_some_and(|c| e.seq <= c) || !q.filter.matches(&e.record) {
                continue;
            }
            total_matching += 1;

            let key = fingerprint(&e.record.payload);
            let acc = groups.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                GroupAcc::new(&e.record)
            });
            acc.absorb(&e.record, idx);
        }

        let distinct = order.len();
        let mut built: Vec<ErrorGroup> = order
            .into_iter()
            .filter_map(|k| {
                groups
                    .remove(&k)
                    .map(|acc| acc.finish(k, &entries, context_lines, q.max_payload_chars))
            })
            .collect();

        // Most recent first: an agent asking "what just broke" reads top-down
        // and can stop early.
        built.sort_by_key(|g| std::cmp::Reverse(g.last_seen));
        let truncated = built.len() > q.limit;
        built.truncate(q.limit);

        Summary {
            groups: built,
            total_matching,
            distinct,
            truncated,
            buffer_starts_at: entries.first().map(|e| e.record.timestamp),
            next_cursor: entries.last().map(|e| e.seq).unwrap_or(0),
        }
    }

    /// Per-source counters — the "what is running and is it healthy" view.
    pub fn sources(&self) -> Vec<SourceStat> {
        let ring = self.ring.lock().unwrap_or_else(|p| p.into_inner());
        let mut order: Vec<String> = Vec::new();
        let mut stats: HashMap<String, SourceStat> = HashMap::new();

        for e in ring.iter() {
            let s = stats.entry(e.record.source.clone()).or_insert_with(|| {
                order.push(e.record.source.clone());
                SourceStat {
                    name: e.record.source.clone(),
                    total: 0,
                    errors: 0,
                    warns: 0,
                    last_seen: None,
                    last_line: None,
                }
            });
            s.total += 1;
            match e.record.level {
                LogLevel::Error => s.errors += 1,
                LogLevel::Warn => s.warns += 1,
                _ => {}
            }
            s.last_seen = Some(e.record.timestamp);
            s.last_line = Some(elide(&e.record.payload, 200).0);
        }

        let mut out: Vec<SourceStat> = order.into_iter().filter_map(|n| stats.remove(&n)).collect();
        // Noisiest-in-errors first — the source a caller most likely wants.
        out.sort_by(|a, b| b.errors.cmp(&a.errors).then(b.total.cmp(&a.total)));
        out
    }
}

impl Default for LogStore {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

// ── Group accumulation ────────────────────────────────────────────────────────

struct GroupAcc {
    count: usize,
    level: LogLevel,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    sample: String,
    /// Ring index of the most recent occurrence, used to walk forward for context.
    last_idx: usize,
    source_counts: HashMap<String, usize>,
}

impl GroupAcc {
    fn new(r: &LogRecord) -> Self {
        Self {
            count: 0,
            level: r.level,
            first_seen: r.timestamp,
            last_seen: r.timestamp,
            sample: String::new(),
            last_idx: 0,
            source_counts: HashMap::new(),
        }
    }

    fn absorb(&mut self, r: &LogRecord, idx: usize) {
        self.count += 1;
        if r.level.severity() > self.level.severity() {
            self.level = r.level;
        }
        if r.timestamp < self.first_seen {
            self.first_seen = r.timestamp;
        }
        if r.timestamp >= self.last_seen {
            self.last_seen = r.timestamp;
        }
        // Entries arrive in ring order, so the last write is the newest.
        self.sample = r.payload.clone();
        self.last_idx = idx;
        *self.source_counts.entry(r.source.clone()).or_insert(0) += 1;
    }

    fn finish(
        self,
        fingerprint: String,
        entries: &[&Entry],
        context_lines: usize,
        max_payload_chars: usize,
    ) -> ErrorGroup {
        let mut sources: Vec<(String, usize)> = self.source_counts.into_iter().collect();
        sources.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        let context = collect_context(entries, self.last_idx, context_lines, max_payload_chars);

        ErrorGroup {
            fingerprint,
            count: self.count,
            sources: sources.into_iter().map(|(n, _)| n).collect(),
            level: self.level,
            first_seen: self.first_seen,
            last_seen: self.last_seen,
            sample: elide(&self.sample, max_payload_chars).0,
            context,
        }
    }
}

/// Walk forward from `idx`, collecting continuation lines from the same source.
fn collect_context(
    entries: &[&Entry],
    idx: usize,
    max_lines: usize,
    max_payload_chars: usize,
) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    let source = match entries.get(idx) {
        Some(e) => &e.record.source,
        None => return Vec::new(),
    };

    let mut out = Vec::new();
    for e in entries.iter().skip(idx + 1) {
        if &e.record.source != source {
            continue; // interleaved output from another service — skip, don't stop
        }
        // A detected level means a new log event started; the trace has ended.
        if e.record.level != LogLevel::Unknown || e.record.payload.trim().is_empty() {
            break;
        }
        out.push(elide(&e.record.payload, max_payload_chars).0);
        if out.len() >= max_lines {
            break;
        }
    }
    out
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Truncate to `max` characters, appending a marker. Returns the original
/// character count when elision occurred.
fn elide(s: &str, max: usize) -> (String, Option<usize>) {
    let count = s.chars().count();
    if count <= max {
        return (s.to_string(), None);
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str(&format!("… [+{} chars]", count - max));
    (out, Some(count))
}

fn to_seq_record(e: &Entry, max_payload_chars: usize) -> SeqRecord {
    let (payload, truncated_from) = elide(&e.record.payload, max_payload_chars);
    SeqRecord {
        seq: e.seq,
        timestamp: e.record.timestamp,
        source: e.record.source.clone(),
        level: e.record.level,
        payload,
        payload_truncated_from: truncated_from,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(source: &str, level: LogLevel, payload: &str) -> LogRecord {
        LogRecord {
            timestamp: Utc::now(),
            source: source.into(),
            level,
            payload: payload.into(),
        }
    }

    // ── fingerprint ───────────────────────────────────────────────────────────

    #[test]
    fn fingerprint_collapses_varying_numbers() {
        let a = fingerprint("connection refused: postgres:5432");
        let b = fingerprint("connection refused: postgres:5433");
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_collapses_uuids() {
        let a = fingerprint("request 550e8400-e29b-41d4-a716-446655440000 failed");
        let b = fingerprint("request 6ba7b810-9dad-11d1-80b4-00c04fd430c8 failed");
        assert_eq!(a, b);
        assert!(a.contains("<UUID>"), "got {a}");
    }

    #[test]
    fn fingerprint_collapses_quoted_identifiers() {
        let a = fingerprint(r#"Cannot find module "foo""#);
        let b = fingerprint(r#"Cannot find module "bar""#);
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_keeps_distinct_errors_apart() {
        let a = fingerprint("connection refused: postgres:5432");
        let b = fingerprint("permission denied: /var/run/docker.sock");
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_normalises_whitespace_and_indentation() {
        assert_eq!(
            fingerprint("  at Foo.bar   (x)"),
            fingerprint("at Foo.bar (x)")
        );
    }

    #[test]
    fn fingerprint_is_bounded() {
        let long = "e".repeat(5_000);
        assert!(fingerprint(&long).chars().count() <= 300);
    }

    #[test]
    fn fingerprint_survives_unterminated_quote() {
        // Must not hang or swallow the line.
        let f = fingerprint(r#"error: unexpected " in input"#);
        assert!(!f.is_empty());
    }

    // ── parse_since ───────────────────────────────────────────────────────────

    #[test]
    fn parse_since_accepts_relative_durations() {
        let now = Utc::now();
        let five_min = parse_since("5m").unwrap();
        let delta = now - five_min;
        assert!(delta.num_seconds() >= 299 && delta.num_seconds() <= 301);
    }

    #[test]
    fn parse_since_accepts_rfc3339() {
        let t = parse_since("2026-04-04T10:23:45Z").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-04-04T10:23:45+00:00");
    }

    #[test]
    fn parse_since_rejects_garbage() {
        assert!(parse_since("yesterday").is_none());
        assert!(parse_since("-5m").is_none());
        assert!(parse_since("").is_none());
    }

    // ── store ─────────────────────────────────────────────────────────────────

    #[test]
    fn store_evicts_oldest_past_capacity() {
        let store = LogStore::new(3);
        for i in 0..5 {
            store.push(rec("api", LogLevel::Info, &format!("line {i}")));
        }
        assert_eq!(store.len(), 3);
        let out = store.records(&Query::default());
        assert_eq!(out.records[0].payload, "line 2");
        assert_eq!(out.records[2].payload, "line 4");
    }

    #[test]
    fn store_cursor_returns_only_new_records() {
        let store = LogStore::new(100);
        store.push(rec("api", LogLevel::Info, "old"));
        let cursor = store.records(&Query::default()).next_cursor;
        store.push(rec("api", LogLevel::Info, "new"));

        let out = store.records(&Query::default().with_cursor(Some(cursor)));
        assert_eq!(out.records.len(), 1);
        assert_eq!(out.records[0].payload, "new");
    }

    #[test]
    fn store_limit_keeps_newest_and_flags_truncation() {
        let store = LogStore::new(100);
        for i in 0..10 {
            store.push(rec("api", LogLevel::Info, &format!("line {i}")));
        }
        let out = store.records(&Query::default().with_limit(3));
        assert_eq!(out.records.len(), 3);
        assert_eq!(out.total_matching, 10);
        assert!(out.truncated);
        assert_eq!(out.records[0].payload, "line 7");
    }

    #[test]
    fn store_filters_by_min_level() {
        let store = LogStore::new(100);
        store.push(rec("api", LogLevel::Info, "started"));
        store.push(rec("api", LogLevel::Warn, "slow query"));
        store.push(rec("api", LogLevel::Error, "boom"));
        store.push(rec("api", LogLevel::Unknown, "plain output"));

        let q = Query::new(Filter::none().with_min_level(LogLevel::Warn));
        let out = store.records(&q);
        assert_eq!(out.records.len(), 2);
    }

    #[test]
    fn store_elides_oversized_payloads() {
        let store = LogStore::new(10);
        store.push(rec("api", LogLevel::Info, &"x".repeat(500)));
        let out = store.records(&Query::default().with_max_payload_chars(100));
        assert!(out.records[0].payload.contains("[+400 chars]"));
        assert_eq!(out.records[0].payload_truncated_from, Some(500));
    }

    // ── summarize ─────────────────────────────────────────────────────────────

    #[test]
    fn summarize_collapses_a_flood_into_distinct_failures() {
        let store = LogStore::new(1000);
        for i in 0..200 {
            store.push(rec(
                "api",
                LogLevel::Error,
                &format!("ERROR connection refused: postgres:{}", 5432 + i % 2),
            ));
        }
        for _ in 0..50 {
            store.push(rec("web", LogLevel::Error, "ERROR upstream timeout"));
        }

        let q = Query::new(Filter::none().with_min_level(LogLevel::Error));
        let s = store.summarize(&q, 0);

        assert_eq!(s.total_matching, 250);
        assert_eq!(s.distinct, 2, "groups: {:?}", s.groups);
        assert_eq!(s.groups.len(), 2);
        // Most recent first — the web timeout arrived last.
        assert_eq!(s.groups[0].count, 50);
        assert_eq!(s.groups[0].sources, vec!["web"]);
        assert_eq!(s.groups[1].count, 200);
    }

    #[test]
    fn summarize_attaches_stack_trace_as_context() {
        let store = LogStore::new(100);
        store.push(rec("api", LogLevel::Error, "Error: boom"));
        store.push(rec("api", LogLevel::Unknown, "    at Foo.bar (a.js:1:2)"));
        store.push(rec("api", LogLevel::Unknown, "    at Baz.qux (b.js:3:4)"));
        store.push(rec("api", LogLevel::Info, "request handled"));

        let q = Query::new(Filter::none().with_min_level(LogLevel::Error));
        let s = store.summarize(&q, DEFAULT_CONTEXT_LINES);

        assert_eq!(s.groups.len(), 1);
        assert_eq!(s.groups[0].context.len(), 2, "{:?}", s.groups[0].context);
        assert!(s.groups[0].context[0].contains("Foo.bar"));
    }

    #[test]
    fn summarize_context_stops_at_next_real_event() {
        let store = LogStore::new(100);
        store.push(rec("api", LogLevel::Error, "Error: boom"));
        store.push(rec("api", LogLevel::Info, "recovered"));
        store.push(rec("api", LogLevel::Unknown, "unrelated later line"));

        let q = Query::new(Filter::none().with_min_level(LogLevel::Error));
        let s = store.summarize(&q, DEFAULT_CONTEXT_LINES);
        assert!(s.groups[0].context.is_empty());
    }

    #[test]
    fn summarize_context_ignores_interleaved_other_sources() {
        let store = LogStore::new(100);
        store.push(rec("api", LogLevel::Error, "Error: boom"));
        store.push(rec("web", LogLevel::Info, "GET / 200"));
        store.push(rec("api", LogLevel::Unknown, "    at Foo.bar"));

        let q = Query::new(Filter::none().with_min_level(LogLevel::Error));
        let s = store.summarize(&q, DEFAULT_CONTEXT_LINES);
        assert_eq!(s.groups[0].context.len(), 1);
    }

    #[test]
    fn summarize_records_multiple_sources_per_group() {
        let store = LogStore::new(100);
        store.push(rec("api", LogLevel::Error, "ERROR upstream timeout"));
        store.push(rec("worker", LogLevel::Error, "ERROR upstream timeout"));
        store.push(rec("worker", LogLevel::Error, "ERROR upstream timeout"));

        let q = Query::new(Filter::none().with_min_level(LogLevel::Error));
        let s = store.summarize(&q, 0);
        assert_eq!(s.groups.len(), 1);
        // Most frequent source first.
        assert_eq!(s.groups[0].sources, vec!["worker", "api"]);
    }

    #[test]
    fn summarize_truncates_group_count() {
        let store = LogStore::new(500);
        for i in 0..50 {
            store.push(rec(
                "api",
                LogLevel::Error,
                &format!("ERROR distinct kind {}", (b'a' + (i % 26) as u8) as char),
            ));
        }
        let q = Query::new(Filter::none().with_min_level(LogLevel::Error)).with_limit(5);
        let s = store.summarize(&q, 0);
        assert_eq!(s.groups.len(), 5);
        assert!(s.truncated);
        assert_eq!(s.distinct, 26);
    }

    // ── sources ───────────────────────────────────────────────────────────────

    #[test]
    fn sources_counts_levels_and_sorts_errors_first() {
        let store = LogStore::new(100);
        store.push(rec("quiet", LogLevel::Info, "ok"));
        store.push(rec("quiet", LogLevel::Info, "ok"));
        store.push(rec("noisy", LogLevel::Error, "boom"));
        store.push(rec("noisy", LogLevel::Warn, "hmm"));

        let stats = store.sources();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].name, "noisy");
        assert_eq!(stats[0].errors, 1);
        assert_eq!(stats[0].warns, 1);
        assert_eq!(stats[1].total, 2);
        assert_eq!(stats[1].last_line.as_deref(), Some("ok"));
    }
}
