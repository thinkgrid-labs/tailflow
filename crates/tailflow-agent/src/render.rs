//! Renders daemon JSON into compact text.
//!
//! Both binaries feed an LLM context window, where JSON is a poor encoding:
//! braces, quotes and repeated keys can outweigh the log content itself. These
//! renderers keep the same information in roughly a third of the tokens.
//!
//! Everything here works on `serde_json::Value` rather than importing the core
//! types. That keeps `tailflow-mcp` and `tailflow-logs` free of the ingestion
//! dependency tree (Docker, file watching, regex) — they are network clients,
//! not log collectors — and pins them to the same public JSON contract any
//! third-party consumer would use.

use serde_json::Value;

/// `2026-08-06T13:39:02.643Z` → `13:39:02`. Full dates are noise when every
/// record in a buffer is minutes old; the ISO timestamps stay in `--json`.
fn short_time(v: Option<&Value>) -> String {
    let s = match v.and_then(Value::as_str) {
        Some(s) => s,
        None => return "?".into(),
    };
    match s.split_once('T') {
        Some((_, time)) => time
            .split(['.', '+', 'Z'])
            .next()
            .unwrap_or(time)
            .to_string(),
        None => s.to_string(),
    }
}

fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Describes what the buffer can and cannot see.
///
/// An empty result is ambiguous — "nothing failed" and "nothing is running"
/// look identical — and an agent that guesses wrong reports a green build for
/// a stack that never started. Every renderer states which one it is.
fn horizon(v: &Value) -> String {
    let buffered = v
        .get("buffered")
        .and_then(Value::as_u64)
        .or_else(|| v.get("total_matching").and_then(Value::as_u64));
    let starts = v.get("buffer_starts_at");
    match (buffered, starts) {
        (_, Some(t)) if !t.is_null() => {
            format!("buffer reaches back to {}", short_time(Some(t)))
        }
        (Some(0), _) => "buffer is empty".to_string(),
        _ => String::new(),
    }
}

// ── /api/errors ───────────────────────────────────────────────────────────────

pub fn errors(v: &Value) -> String {
    let groups = v.get("groups").and_then(Value::as_array);
    let distinct = n(v, "distinct");
    let total = n(v, "total_matching");
    let cursor = n(v, "next_cursor");

    let Some(groups) = groups else {
        return "daemon returned no `groups` field — is it running an older version?".into();
    };

    if groups.is_empty() {
        let h = horizon(v);
        let suffix = if h.is_empty() {
            String::new()
        } else {
            format!(" ({h})")
        };
        let gap = cursor_gap_note(v);
        return format!(
            "No matching errors{suffix}.\n\
             If you expected output, check `list_log_sources` — a source that never \
             started produces no logs to fail on.\n{gap}cursor {cursor}"
        );
    }

    let mut out = String::new();
    out.push_str(&format!(
        "{distinct} distinct failure{} across {total} record{}",
        plural(distinct),
        plural(total),
    ));
    let h = horizon(v);
    if !h.is_empty() {
        out.push_str(&format!(" · {h}"));
    }
    out.push_str(&format!(" · cursor {cursor}\n"));
    out.push_str(&cursor_gap_note(v));

    if v.get("truncated").and_then(Value::as_bool) == Some(true) {
        out.push_str(&format!(
            "(showing {} of {distinct} groups — raise `limit` for the rest)\n",
            groups.len()
        ));
    }

    for (i, g) in groups.iter().enumerate() {
        let count = n(g, "count");
        let sources = g
            .get("sources")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();

        out.push_str(&format!(
            "\n[{}] x{count} {} {} first {} last {}\n",
            i + 1,
            s(g, "level"),
            sources,
            short_time(g.get("first_seen")),
            short_time(g.get("last_seen")),
        ));
        out.push_str(&format!("    {}\n", s(g, "sample").replace('\n', "\n    ")));

        if let Some(ctx) = g.get("context").and_then(Value::as_array) {
            for line in ctx.iter().filter_map(Value::as_str) {
                // Re-indent uniformly: the source's own leading whitespace
                // varies by runtime and carries no information here.
                out.push_str(&format!("      {}\n", line.trim()));
            }
        }
    }

    out
}

// ── /api/query ────────────────────────────────────────────────────────────────

pub fn records(v: &Value) -> String {
    let Some(recs) = v.get("records").and_then(Value::as_array) else {
        return "daemon returned no `records` field — is it running an older version?".into();
    };
    let total = n(v, "total_matching");
    let cursor = n(v, "next_cursor");

    if recs.is_empty() {
        return format!("No matching records. {}cursor {cursor}", cursor_gap_note(v));
    }

    let mut out = String::new();
    out.push_str(&cursor_gap_note(v));
    if v.get("truncated").and_then(Value::as_bool) == Some(true) {
        out.push_str(&format!(
            "{} of {total} matching records (newest kept — narrow the filter or raise `limit`) · cursor {cursor}\n",
            recs.len()
        ));
    } else {
        out.push_str(&format!(
            "{} matching record{} · cursor {cursor}\n",
            recs.len(),
            plural(recs.len() as u64)
        ));
    }

    // Align the source column so the eye (and the model) can group by service.
    let width = recs
        .iter()
        .filter_map(|r| r.get("source").and_then(Value::as_str))
        .map(str::len)
        .max()
        .unwrap_or(0)
        .min(20);

    for r in recs {
        out.push_str(&format!(
            "{} {:<5} {:<width$} {}\n",
            short_time(r.get("timestamp")),
            s(r, "level"),
            s(r, "source"),
            s(r, "payload").replace('\n', "\n    "),
            width = width,
        ));
    }

    out
}

// ── /api/sources ──────────────────────────────────────────────────────────────

pub fn sources(v: &Value) -> String {
    let Some(list) = v.get("sources").and_then(Value::as_array) else {
        return "daemon returned no `sources` field — is it running an older version?".into();
    };
    let buffered = n(v, "buffered");
    let cursor = n(v, "cursor");

    if list.is_empty() {
        return "No sources have produced any output yet.\n\
                The daemon is running but every source is silent — the processes in \
                tailflow.toml may have failed to start, or no Docker container is running."
            .into();
    }

    let mut out = format!(
        "{} source{} · {buffered} records buffered · cursor {cursor}\n",
        list.len(),
        plural(list.len() as u64)
    );

    let width = list
        .iter()
        .filter_map(|x| x.get("name").and_then(Value::as_str))
        .map(str::len)
        .max()
        .unwrap_or(0)
        .min(24);

    for src in list {
        let errors = n(src, "errors");
        let warns = n(src, "warns");
        out.push_str(&format!(
            "{:<width$}  {:<8} {:>5} records  {:>4} err  {:>4} warn  last {}  {}{}\n",
            s(src, "name"),
            s(src, "status"),
            n(src, "total"),
            errors,
            warns,
            short_time(src.get("last_seen")),
            truncate(s(src, "last_line"), 80),
            src.get("detail")
                .and_then(Value::as_str)
                .map(|d| format!(" · {}", truncate(d, 80)))
                .unwrap_or_default(),
            width = width,
        ));
    }

    out
}

// ── /api/wait ─────────────────────────────────────────────────────────────────

pub fn wait(v: &Value) -> String {
    let matched = v.get("matched").and_then(Value::as_bool).unwrap_or(false);
    let waited = n(v, "waited_ms");
    if !matched {
        return format!(
            "Nothing matched within {waited}ms — the condition did not occur.\ncursor {}",
            n(v, "next_cursor")
        );
    }
    format!("Matched after {waited}ms.\n{}", records(v))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn plural(n: u64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn cursor_gap_note(v: &Value) -> String {
    if v.get("cursor_gap").and_then(Value::as_bool) == Some(true) {
        let start = n(v, "buffer_start_cursor");
        format!("WARNING: cursor history has a gap; oldest retained cursor is {start}.\n")
    } else {
        String::new()
    }
}

fn truncate(s: &str, max: usize) -> String {
    let clean = s.replace(['\n', '\r'], " ");
    if clean.chars().count() <= max {
        clean
    } else {
        clean.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn short_time_extracts_clock_time() {
        assert_eq!(
            short_time(Some(&json!("2026-08-06T13:39:02.643842Z"))),
            "13:39:02"
        );
        assert_eq!(
            short_time(Some(&json!("2026-08-06T13:39:02+00:00"))),
            "13:39:02"
        );
        assert_eq!(short_time(None), "?");
    }

    #[test]
    fn errors_renders_groups_with_counts_and_context() {
        let v = json!({
            "groups": [{
                "fingerprint": "ERROR connection refused: postgres:<N>",
                "count": 200,
                "sources": ["api"],
                "level": "error",
                "first_seen": "2026-08-06T13:37:02Z",
                "last_seen": "2026-08-06T13:39:01Z",
                "sample": "ERROR connection refused: postgres:5432",
                "context": ["    at Foo.bar (a.js:1:2)"]
            }],
            "total_matching": 200,
            "distinct": 1,
            "truncated": false,
            "buffer_starts_at": "2026-08-06T13:22:01Z",
            "next_cursor": 4210
        });
        let out = errors(&v);
        assert!(out.contains("1 distinct failure across 200 records"));
        assert!(out.contains("x200 error api"));
        assert!(out.contains("connection refused: postgres:5432"));
        assert!(out.contains("at Foo.bar"));
        assert!(out.contains("cursor 4210"));
    }

    #[test]
    fn errors_distinguishes_no_failures_from_no_logs() {
        let quiet = errors(&json!({
            "groups": [], "distinct": 0, "total_matching": 0,
            "buffer_starts_at": "2026-08-06T13:22:01Z", "next_cursor": 12
        }));
        assert!(quiet.contains("No matching errors"));
        assert!(quiet.contains("buffer reaches back to 13:22:01"));

        let empty = errors(&json!({
            "groups": [], "distinct": 0, "total_matching": 0,
            "buffered": 0, "buffer_starts_at": Value::Null, "next_cursor": 0
        }));
        assert!(empty.contains("buffer is empty"));
    }

    #[test]
    fn errors_flags_group_truncation() {
        let out = errors(&json!({
            "groups": [{"count": 1, "sources": ["a"], "level": "error",
                        "first_seen": "2026-08-06T13:00:00Z",
                        "last_seen": "2026-08-06T13:00:00Z",
                        "sample": "boom", "fingerprint": "boom"}],
            "distinct": 9, "total_matching": 9, "truncated": true, "next_cursor": 1
        }));
        assert!(out.contains("showing 1 of 9 groups"));
    }

    #[test]
    fn records_reports_truncation_and_aligns_sources() {
        let out = records(&json!({
            "records": [
                {"seq": 1, "timestamp": "2026-08-06T13:39:01Z", "source": "api",
                 "level": "error", "payload": "boom"},
                {"seq": 2, "timestamp": "2026-08-06T13:39:02Z", "source": "worker",
                 "level": "info", "payload": "ok"}
            ],
            "total_matching": 340, "truncated": true, "next_cursor": 400
        }));
        assert!(out.contains("2 of 340 matching records"));
        assert!(out.contains("13:39:01 error api"));
        assert!(out.contains("cursor 400"));
    }

    #[test]
    fn sources_explains_a_silent_stack() {
        let out = sources(&json!({"sources": [], "buffered": 0, "cursor": 0}));
        assert!(out.contains("No sources have produced any output"));
        assert!(out.contains("failed to start"));
    }

    #[test]
    fn sources_renders_counters() {
        let out = sources(&json!({
            "sources": [{"name": "api", "total": 812, "errors": 17, "warns": 3,
                         "last_seen": "2026-08-06T13:39:02Z", "last_line": "GET /health 200"}],
            "buffered": 812, "cursor": 812
        }));
        assert!(out.contains("1 source ·"));
        assert!(out.contains("812 records"));
        assert!(out.contains("17 err"));
        assert!(out.contains("GET /health 200"));
    }

    #[test]
    fn wait_reports_timeout_without_pretending_success() {
        let out = wait(&json!({"matched": false, "waited_ms": 30000, "next_cursor": 9}));
        assert!(out.contains("Nothing matched within 30000ms"));
        assert!(out.contains("did not occur"));
    }

    #[test]
    fn truncate_collapses_newlines() {
        assert_eq!(truncate("a\nb", 10), "a b");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }
}
