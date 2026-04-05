/// Returns `true` if `s` looks like a JSON object or array.
/// Uses a fast prefix check before attempting a full parse.
pub fn is_json(s: &str) -> bool {
    let s = s.trim();
    (s.starts_with('{') || s.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(s).is_ok()
}

/// Parse `s` as a JSON object and return a compact `key=value` string
/// suitable for single-line TUI display.
///
/// - String values are shown unquoted: `msg=request`
/// - Numbers/bools are shown as-is: `status=200  ok=true`
/// - Nested objects/arrays are inlined as compact JSON: `meta={"host":"x"}`
/// - Returns `None` if `s` is not a valid JSON object (arrays included as
///   pretty JSON).
pub fn flatten_json(s: &str) -> Option<String> {
    let s = s.trim();
    if !s.starts_with('{') && !s.starts_with('[') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    match v {
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, val)| {
                    let formatted = match val {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Null => "null".to_string(),
                        other => other.to_string(),
                    };
                    format!("{k}={formatted}")
                })
                .collect();
            Some(parts.join("  "))
        }
        // For arrays, fall back to compact single-line JSON
        other => serde_json::to_string(&other).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_json_detects_object() {
        assert!(is_json(r#"{"level":"info","msg":"ok"}"#));
    }

    #[test]
    fn is_json_detects_array() {
        assert!(is_json(r#"[1,2,3]"#));
    }

    #[test]
    fn is_json_rejects_plain_text() {
        assert!(!is_json("server started on port 3000"));
        assert!(!is_json("ERROR: connection refused"));
    }

    #[test]
    fn is_json_rejects_invalid_json() {
        assert!(!is_json("{not valid}"));
    }

    #[test]
    fn flatten_json_produces_key_value_pairs() {
        let s = r#"{"level":"info","status":200,"ok":true}"#;
        let out = flatten_json(s).unwrap();
        assert!(out.contains("level=info"));
        assert!(out.contains("status=200"));
        assert!(out.contains("ok=true"));
    }

    #[test]
    fn flatten_json_unquotes_string_values() {
        let out = flatten_json(r#"{"msg":"hello world"}"#).unwrap();
        assert_eq!(out, "msg=hello world");
    }

    #[test]
    fn flatten_json_inlines_nested_objects() {
        let out = flatten_json(r#"{"meta":{"host":"x"}}"#).unwrap();
        assert!(out.starts_with("meta="));
        assert!(out.contains("host"));
    }

    #[test]
    fn flatten_json_returns_none_for_plain_text() {
        assert!(flatten_json("not json").is_none());
    }

    #[test]
    fn flatten_json_handles_array() {
        let out = flatten_json(r#"[1,2,3]"#).unwrap();
        assert_eq!(out, "[1,2,3]");
    }
}
