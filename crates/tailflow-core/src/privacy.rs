//! Privacy controls applied before anything leaves the ingestion task.
//!
//! Redaction here is a *heuristic*. It removes common, high-confidence
//! credential shapes (provider API keys, bearer tokens, URL passwords,
//! `password=`-style assignments, PEM private keys). It does not and cannot
//! guarantee that every secret is caught — an opaque 20-character string in a
//! free-text line is indistinguishable from an ID. Documentation must say so.
//!
//! Costs are bounded: Rust's `regex` engine runs in time linear in the input,
//! inputs are already capped by [`crate::event::MAX_MESSAGE_BYTES`] before
//! storage, and a [`RegexSet`] pre-check means lines with no candidate pay for
//! one scan, not one per rule.

use crate::event::{ObservabilityEvent, RedactionInfo};
use regex::{Regex, RegexSet};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// What a stored or emitted event may contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureMode {
    /// Default. Envelope, redacted message and attributes; payload bodies are
    /// not kept at all.
    #[default]
    Metadata,
    /// As `metadata`, plus payload bodies after redaction.
    Redacted,
    /// Everything verbatim, no redaction. Explicit opt-in; stores secrets.
    Full,
}

impl CaptureMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureMode::Metadata => "metadata",
            CaptureMode::Redacted => "redacted",
            CaptureMode::Full => "full",
        }
    }

    pub fn redacts(self) -> bool {
        !matches!(self, CaptureMode::Full)
    }
}

/// A user-supplied rule from `[[privacy.redaction_rules]]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedactionRuleConfig {
    pub name: String,
    /// Regex. If it has a capture group named `secret`, only that group is
    /// replaced; otherwise the whole match is.
    pub pattern: String,
}

/// Errors building a redactor or capture policy. Always fatal at startup: a
/// rule the user wrote must never be silently skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivacyError {
    InvalidRule { name: String, reason: String },
    UnknownBuiltinRule(String),
    InvalidGlob { glob: String, reason: String },
    InvalidEventType(String),
}

impl fmt::Display for PrivacyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrivacyError::InvalidRule { name, reason } => {
                write!(f, "redaction rule {name:?} is invalid: {reason}")
            }
            PrivacyError::UnknownBuiltinRule(name) => write!(
                f,
                "disabled_rules names unknown built-in rule {name:?} (known: {})",
                BUILTIN_RULES
                    .iter()
                    .map(|(n, _)| *n)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            PrivacyError::InvalidGlob { glob, reason } => {
                write!(f, "excluded_file_globs entry {glob:?} is invalid: {reason}")
            }
            PrivacyError::InvalidEventType(t) => write!(
                f,
                "excluded_event_types entry {t:?} is not a known type or `prefix.*` pattern"
            ),
        }
    }
}

impl std::error::Error for PrivacyError {}

/// Built-in high-confidence rules. A `(?P<secret>…)` group limits the
/// replacement to the secret itself so the surrounding key stays readable.
pub const BUILTIN_RULES: &[(&str, &str)] = &[
    (
        "private_key",
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----(?P<secret>[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|$))",
    ),
    (
        "aws_access_key",
        r"\b(?P<secret>(?:AKIA|ASIA)[0-9A-Z]{16})\b",
    ),
    (
        "github_token",
        r"\b(?P<secret>gh[pousr]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{22,255})\b",
    ),
    (
        "slack_token",
        r"\b(?P<secret>xox[abposr]-[A-Za-z0-9-]{10,})",
    ),
    (
        "stripe_key",
        r"\b(?P<secret>[rs]k_(?:live|test)_[A-Za-z0-9]{16,})\b",
    ),
    ("anthropic_key", r"\b(?P<secret>sk-ant-[A-Za-z0-9_-]{20,})"),
    (
        "openai_key",
        r"\b(?P<secret>sk-(?:proj-|svcacct-)?[A-Za-z0-9_-]{32,})",
    ),
    ("google_api_key", r"\b(?P<secret>AIza[0-9A-Za-z_-]{35})"),
    (
        "jwt",
        r"\b(?P<secret>eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,})",
    ),
    (
        "authorization_header",
        r#"(?i)\b(?:proxy-)?authorization["']?\s*[:=]\s*["']?(?:(?:bearer|basic|token|digest)\s+)?(?P<secret>[^\s"',;]{4,})"#,
    ),
    (
        "bearer_token",
        r"(?i)\bbearer\s+(?P<secret>[A-Za-z0-9._~+/=-]{8,})",
    ),
    (
        "url_credentials",
        r"\b[a-zA-Z][a-zA-Z0-9+.-]*://[^\s:/@]+:(?P<secret>[^\s@/]+)@",
    ),
    (
        "secret_assignment",
        r#"(?i)\b[a-z0-9_.-]*(?:password|passwd|secret|token|api[_-]?key|apikey|access[_-]?key|private[_-]?key|credentials?)["']?\s*(?:=>|[:=])\s*["']?(?P<secret>[^\s"',;&)}\[\]]+)"#,
    ),
];

const PLACEHOLDER_PREFIX: &str = "[REDACTED";

struct CompiledRule {
    name: String,
    regex: Regex,
    has_secret_group: bool,
}

/// A stateless, shareable set of compiled rules.
pub struct Redactor {
    rules: Vec<CompiledRule>,
    set: RegexSet,
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redactor")
            .field("rules", &self.rule_names())
            .finish()
    }
}

/// Compiled regex size cap for user rules: keeps a pathological pattern from
/// consuming unbounded memory at startup.
const USER_RULE_SIZE_LIMIT: usize = 1 << 20;

impl Redactor {
    /// Built-in rules minus `disabled`, plus `custom`.
    ///
    /// # Errors
    /// An unknown name in `disabled`, or a custom rule that does not compile.
    pub fn new(disabled: &[String], custom: &[RedactionRuleConfig]) -> Result<Self, PrivacyError> {
        for name in disabled {
            if !BUILTIN_RULES.iter().any(|(n, _)| n == name) {
                return Err(PrivacyError::UnknownBuiltinRule(name.clone()));
            }
        }
        let mut rules = Vec::new();
        for (name, pattern) in BUILTIN_RULES {
            if disabled.iter().any(|d| d == name) {
                continue;
            }
            let regex = Regex::new(pattern).map_err(|e| PrivacyError::InvalidRule {
                name: (*name).to_string(),
                reason: e.to_string(),
            })?;
            rules.push(CompiledRule {
                name: (*name).to_string(),
                has_secret_group: regex.capture_names().any(|n| n == Some("secret")),
                regex,
            });
        }
        for rule in custom {
            if rule.name.is_empty()
                || !rule
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(PrivacyError::InvalidRule {
                    name: rule.name.clone(),
                    reason: "name must be non-empty [A-Za-z0-9_-]".into(),
                });
            }
            let regex = regex::RegexBuilder::new(&rule.pattern)
                .size_limit(USER_RULE_SIZE_LIMIT)
                .build()
                .map_err(|e| PrivacyError::InvalidRule {
                    name: rule.name.clone(),
                    reason: e.to_string(),
                })?;
            if regex.is_match("") {
                return Err(PrivacyError::InvalidRule {
                    name: rule.name.clone(),
                    reason: "pattern matches the empty string and would redact everything".into(),
                });
            }
            rules.push(CompiledRule {
                name: rule.name.clone(),
                has_secret_group: regex.capture_names().any(|n| n == Some("secret")),
                regex,
            });
        }
        let set = RegexSet::new(rules.iter().map(|r| r.regex.as_str())).map_err(|e| {
            PrivacyError::InvalidRule {
                name: "<set>".into(),
                reason: e.to_string(),
            }
        })?;
        Ok(Self { rules, set })
    }

    /// The default rule set.
    pub fn builtin() -> Self {
        Self::new(&[], &[]).expect("built-in redaction rules are covered by tests and compile")
    }

    pub fn rule_names(&self) -> Vec<&str> {
        self.rules.iter().map(|r| r.name.as_str()).collect()
    }

    /// Redact `text`. Returns `None` when nothing matched (the common case,
    /// with no allocation).
    pub fn redact(&self, text: &str) -> Option<(String, RedactionInfo)> {
        let hits = self.set.matches(text);
        if !hits.matched_any() {
            return None;
        }
        let mut out = text.to_string();
        let mut info = RedactionInfo::default();
        for idx in hits.iter() {
            let Some(rule) = self.rules.get(idx) else {
                continue;
            };
            let (next, count) = apply_rule(rule, &out);
            if count > 0 {
                info.merge_rule(&rule.name, count);
                out = next;
            }
        }
        (info.replacements > 0).then_some((out, info))
    }
}

fn apply_rule(rule: &CompiledRule, text: &str) -> (String, u32) {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut count: u32 = 0;
    for caps in rule.regex.captures_iter(text) {
        let target = if rule.has_secret_group {
            caps.name("secret")
        } else {
            caps.get(0)
        };
        let Some(m) = target else { continue };
        // Never re-redact a placeholder an earlier rule wrote.
        if m.is_empty() || m.as_str().starts_with(PLACEHOLDER_PREFIX) {
            continue;
        }
        let (Some(before), Some(_)) = (text.get(last..m.start()), text.get(m.start()..m.end()))
        else {
            continue;
        };
        out.push_str(before);
        out.push_str(PLACEHOLDER_PREFIX);
        out.push(':');
        out.push_str(&rule.name);
        out.push(']');
        last = m.end();
        count = count.saturating_add(1);
    }
    if count == 0 {
        return (text.to_string(), 0);
    }
    out.push_str(text.get(last..).unwrap_or_default());
    (out, count)
}

/// Lines a PEM block may span before the tracker gives up and resumes normal
/// processing — bounds the damage of a BEGIN line with no END.
const PEM_MAX_LINES: u32 = 200;

/// Per-stream redaction state for line-oriented sources.
///
/// A private key printed to a log spans many lines, and only the first and
/// last carry a recognisable marker; the base64 body in between looks like any
/// other text. This tracker redacts every line of a source between a `BEGIN …
/// PRIVATE KEY` line and its `END` line.
pub struct LineRedactor<'a> {
    redactor: &'a Redactor,
    open_blocks: HashMap<String, u32>,
}

impl<'a> LineRedactor<'a> {
    pub fn new(redactor: &'a Redactor) -> Self {
        Self {
            redactor,
            open_blocks: HashMap::new(),
        }
    }

    /// Redact one line from `source`.
    pub fn redact_line(&mut self, source: &str, line: &str) -> Option<(String, RedactionInfo)> {
        if let Some(remaining) = self.open_blocks.get_mut(source) {
            *remaining = remaining.saturating_sub(1);
            let ends = line.contains("-----END ") && line.contains("PRIVATE KEY-----");
            if ends || *remaining == 0 {
                self.open_blocks.remove(source);
            }
            let mut info = RedactionInfo::default();
            info.merge_rule("private_key", 1);
            let text = if ends {
                format!("{PLACEHOLDER_PREFIX}:private_key]-----END PRIVATE KEY-----")
            } else {
                format!("{PLACEHOLDER_PREFIX}:private_key]")
            };
            return Some((text, info));
        }
        let begins = line.contains("-----BEGIN ") && line.contains("PRIVATE KEY-----");
        let ends_same_line = line.contains("-----END ");
        if begins && !ends_same_line {
            self.open_blocks.insert(source.to_string(), PEM_MAX_LINES);
        }
        // Bound the map: a flood of distinct sources that each open a block
        // cannot grow it without limit.
        if self.open_blocks.len() > 1024 {
            self.open_blocks.clear();
        }
        self.redactor.redact(line)
    }
}

/// Why the policy refused to record an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    CaptureDisabled,
    EventType,
    Agent,
    PathOutsideAllowedDirs,
    PathExcluded,
}

impl Exclusion {
    pub fn as_str(self) -> &'static str {
        match self {
            Exclusion::CaptureDisabled => "capture_disabled",
            Exclusion::EventType => "event_type_excluded",
            Exclusion::Agent => "agent_excluded",
            Exclusion::PathOutsideAllowedDirs => "path_outside_allowed_dirs",
            Exclusion::PathExcluded => "path_excluded",
        }
    }
}

/// `[privacy]` section of `tailflow.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PrivacyConfig {
    /// Master switch for durable capture. `false` keeps the live view only.
    pub capture_enabled: bool,
    pub capture: CaptureMode,
    /// Built-in rule names to turn off (redaction exclusions).
    pub disabled_rules: Vec<String>,
    pub redaction_rules: Vec<RedactionRuleConfig>,
    /// When non-empty, path-bearing events outside these directories are not
    /// recorded. Prepared for M2 agent/file collectors.
    pub allowed_project_dirs: Vec<PathBuf>,
    /// Globs (`**` crosses `/`, `*` does not) for paths never recorded.
    pub excluded_file_globs: Vec<String>,
    /// Exact types or `prefix.*` patterns never recorded. `system.events.dropped`
    /// cannot be excluded: hiding loss markers would hide loss.
    pub excluded_event_types: Vec<String>,
    /// Agent integrations whose events are never recorded.
    pub excluded_agents: Vec<String>,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            capture_enabled: true,
            capture: CaptureMode::Metadata,
            disabled_rules: Vec::new(),
            redaction_rules: Vec::new(),
            allowed_project_dirs: Vec::new(),
            excluded_file_globs: Vec::new(),
            excluded_event_types: Vec::new(),
            excluded_agents: Vec::new(),
        }
    }
}

/// Compiled `[privacy]` policy.
pub struct CapturePolicy {
    pub enabled: bool,
    pub mode: CaptureMode,
    redactor: Redactor,
    excluded_types: Vec<String>,
    excluded_prefixes: Vec<String>,
    excluded_agents: Vec<String>,
    allowed_dirs: Vec<PathBuf>,
    excluded_globs: Vec<Regex>,
}

impl fmt::Debug for CapturePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturePolicy")
            .field("enabled", &self.enabled)
            .field("mode", &self.mode)
            .field("redactor", &self.redactor)
            .finish_non_exhaustive()
    }
}

impl CapturePolicy {
    /// # Errors
    /// Any invalid rule, glob, or event-type pattern.
    pub fn from_config(config: &PrivacyConfig) -> Result<Self, PrivacyError> {
        let redactor = Redactor::new(&config.disabled_rules, &config.redaction_rules)?;
        let mut excluded_types = Vec::new();
        let mut excluded_prefixes = Vec::new();
        for t in &config.excluded_event_types {
            if let Some(prefix) = t.strip_suffix(".*") {
                if prefix.is_empty()
                    || !crate::event::EVENT_TYPES
                        .iter()
                        .any(|s| s.name.starts_with(&format!("{prefix}.")))
                {
                    return Err(PrivacyError::InvalidEventType(t.clone()));
                }
                excluded_prefixes.push(format!("{prefix}."));
            } else if crate::event::event_type_spec(t).is_some() {
                excluded_types.push(t.clone());
            } else {
                return Err(PrivacyError::InvalidEventType(t.clone()));
            }
        }
        let excluded_globs = config
            .excluded_file_globs
            .iter()
            .map(|g| glob_to_regex(g))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            enabled: config.capture_enabled,
            mode: config.capture,
            redactor,
            excluded_types,
            excluded_prefixes,
            excluded_agents: config.excluded_agents.clone(),
            allowed_dirs: config.allowed_project_dirs.clone(),
            excluded_globs,
        })
    }

    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    /// Decide whether `event` may be recorded at all.
    pub fn admit(&self, event: &ObservabilityEvent) -> Result<(), Exclusion> {
        // Loss markers are never excludable.
        if event.event_type == "system.events.dropped" {
            return Ok(());
        }
        if !self.enabled {
            return Err(Exclusion::CaptureDisabled);
        }
        if self.excluded_types.contains(&event.event_type)
            || self
                .excluded_prefixes
                .iter()
                .any(|p| event.event_type.starts_with(p.as_str()))
        {
            return Err(Exclusion::EventType);
        }
        if event
            .agent_name
            .as_deref()
            .is_some_and(|a| self.excluded_agents.iter().any(|x| x == a))
        {
            return Err(Exclusion::Agent);
        }
        if let Some(path) = event.path.as_deref() {
            let p = Path::new(path);
            if !self.allowed_dirs.is_empty()
                && p.is_absolute()
                && !self.allowed_dirs.iter().any(|d| p.starts_with(d))
            {
                return Err(Exclusion::PathOutsideAllowedDirs);
            }
            let normalized = path.replace('\\', "/");
            if self.excluded_globs.iter().any(|g| g.is_match(&normalized)) {
                return Err(Exclusion::PathExcluded);
            }
        }
        Ok(())
    }

    /// Apply capture mode and redaction to an event's text fields in place.
    /// Message and string attributes are redacted in `metadata` and
    /// `redacted` modes; the payload is dropped in `metadata`, redacted in
    /// `redacted`, and kept verbatim in `full`.
    pub fn sanitize(&self, event: &mut ObservabilityEvent) {
        if self.mode.redacts() {
            if let Some(message) = &event.message {
                if let Some((clean, info)) = self.redactor.redact(message) {
                    event.message = Some(clean);
                    merge_info(&mut event.redaction, &info);
                }
            }
            for value in event.attributes.values_mut() {
                if let serde_json::Value::String(s) = value {
                    if let Some((clean, info)) = self.redactor.redact(s) {
                        *s = clean;
                        merge_info(&mut event.redaction, &info);
                    }
                }
            }
        }
        match self.mode {
            CaptureMode::Metadata => {
                if event.payload.take().is_some() {
                    event.redaction.payload_dropped = true;
                }
            }
            CaptureMode::Redacted => {
                if let Some(payload) = &event.payload {
                    if let Some((clean, info)) = self.redactor.redact(payload) {
                        event.payload = Some(clean);
                        merge_info(&mut event.redaction, &info);
                    }
                }
            }
            CaptureMode::Full => {}
        }
        event.enforce_size_bounds();
    }
}

fn merge_info(into: &mut RedactionInfo, from: &RedactionInfo) {
    for rule in &from.rules {
        if !into.rules.contains(rule) {
            into.rules.push(rule.clone());
        }
    }
    into.replacements = into.replacements.saturating_add(from.replacements);
}

/// Translate a path glob into an anchored regex. `**` matches across `/`,
/// `*` and `?` do not; `**/` also matches zero directories.
pub fn glob_to_regex(glob: &str) -> Result<Regex, PrivacyError> {
    if glob.is_empty() || glob.len() > 512 {
        return Err(PrivacyError::InvalidGlob {
            glob: glob.to_string(),
            reason: "must be 1..=512 bytes".into(),
        });
    }
    let mut re = String::from("^");
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        match c {
            '*' if chars.get(i + 1) == Some(&'*') => {
                if chars.get(i + 2) == Some(&'/') {
                    re.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
                continue;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            other => re.push_str(&regex::escape(&other.to_string())),
        }
        i += 1;
    }
    re.push('$');
    Regex::new(&re).map_err(|e| PrivacyError::InvalidGlob {
        glob: glob.to_string(),
        reason: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Provenance;
    use chrono::Utc;

    fn shared() -> &'static Redactor {
        static R: std::sync::OnceLock<Redactor> = std::sync::OnceLock::new();
        R.get_or_init(Redactor::builtin)
    }

    fn r(text: &str) -> String {
        shared()
            .redact(text)
            .map_or_else(|| text.to_string(), |(s, _)| s)
    }

    #[test]
    fn builtin_rules_compile() {
        assert_eq!(Redactor::builtin().rule_names().len(), BUILTIN_RULES.len());
    }

    #[test]
    fn redacts_common_credentials() {
        let cases = [
            ("key AKIAIOSFODNN7EXAMPLE used", "AKIAIOSFODNN7EXAMPLE"),
            ("token ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789", "ghp_aBcD"),
            ("Authorization: Bearer abc.def.ghijklmnop", "abc.def.ghijklmnop"),
            ("curl -H 'authorization: Basic dXNlcjpwYXNz'", "dXNlcjpwYXNz"),
            ("DATABASE_URL=postgres://app:hunter22@db:5432/app", "hunter22"),
            ("password=s3cr3t!", "s3cr3t!"),
            (r#"{"password": "s3cr3t"}"#, "s3cr3t"),
            ("GITHUB_TOKEN=abcdef123456", "abcdef123456"),
            ("client_secret: zzzyyyxxx", "zzzyyyxxx"),
            (concat!("sk_", "live_", "4eC39HqLyjWDarjtT1zdp7dc"), "4eC39HqLyjWDarjtT1zdp7dc"),
            ("sk-ant-api03-abcdefghijklmnopqrstuvwxyz", "abcdefghijklmnop"),
            ("AIzaSyA-1234567890abcdefghijklmnopqrstu", "AIzaSyA-12345"),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
                "dozjgNryP4J3",
            ),
            ("xoxb-123456789012-abcdefghijkl", "xoxb-1234"),
        ];
        for (input, secret) in cases {
            let out = r(input);
            assert!(
                !out.contains(secret),
                "{input:?} → {out:?} still contains {secret:?}"
            );
            assert!(out.contains("[REDACTED:"), "{input:?} → {out:?}");
        }
    }

    #[test]
    fn keeps_the_key_readable() {
        assert_eq!(
            r("password=hunter2"),
            "password=[REDACTED:secret_assignment]"
        );
        assert_eq!(
            r("postgres://app:pw@db/app"),
            "postgres://app:[REDACTED:url_credentials]@db/app"
        );
        let json = r(r#"{"level":"info","password":"s3cr3t","user":"a"}"#);
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("redacted JSON line stays valid JSON");
        assert_eq!(parsed["password"], "[REDACTED:secret_assignment]");
        assert_eq!(parsed["user"], "a");
    }

    #[test]
    fn does_not_destroy_ordinary_development_logs() {
        for line in [
            "GET /api/users 200 12ms",
            "compiled successfully in 412ms",
            "max_tokens=4096 temperature=0.2",
            "token_count: 1234",
            "error: connection refused: postgres:5432",
            "Listening on http://localhost:3000",
            "user logged in id=550e8400-e29b-41d4-a716-446655440000",
            "updated 3 tokens in cache",
            "commit 4f1c2d9e8b7a6f5e4d3c2b1a0f9e8d7c6b5a4f3e",
            "the password field is required",
            "warning: unused variable `secret_sauce`",
        ] {
            assert_eq!(r(line), line, "over-redacted an ordinary line");
        }
    }

    #[test]
    fn does_not_rerdact_placeholders_or_double_count() {
        let (out, info) = Redactor::builtin()
            .redact("password=postgres://u:p@h/db")
            .unwrap();
        assert!(!out.contains(":p@"), "{out}");
        assert!(info.replacements >= 1);
        let again = Redactor::builtin().redact(&out);
        assert!(
            again.is_none(),
            "redacting redacted output must be a no-op: {again:?}"
        );
    }

    #[test]
    fn multi_line_private_key_is_redacted_line_by_line() {
        let redactor = Redactor::builtin();
        let mut lines = LineRedactor::new(&redactor);
        let input = [
            "-----BEGIN RSA PRIVATE KEY-----",
            "MIIEowIBAAKCAQEA1234567890abcdef",
            "ZZZZbase64bodyline",
            "-----END RSA PRIVATE KEY-----",
            "after the key",
        ];
        let out: Vec<String> = input
            .iter()
            .map(|l| {
                lines
                    .redact_line("api", l)
                    .map_or_else(|| (*l).to_string(), |(s, _)| s)
            })
            .collect();
        assert!(out[1].starts_with("[REDACTED:private_key]"));
        assert!(out[2].starts_with("[REDACTED:private_key]"));
        assert!(!out
            .iter()
            .any(|l| l.contains("MIIEow") || l.contains("ZZZZ")));
        assert_eq!(out[4], "after the key", "state must reset after END");
    }

    #[test]
    fn pem_tracking_is_per_source() {
        let redactor = Redactor::builtin();
        let mut lines = LineRedactor::new(&redactor);
        lines.redact_line("api", "-----BEGIN PRIVATE KEY-----");
        assert!(lines.redact_line("web", "GET / 200").is_none());
        assert!(lines.redact_line("api", "MIIbody").is_some());
    }

    #[test]
    fn user_rules_and_exclusions() {
        let custom = vec![RedactionRuleConfig {
            name: "internal".into(),
            pattern: r"itk_(?P<secret>[a-z0-9]{8})".into(),
        }];
        let redactor = Redactor::new(&["secret_assignment".into()], &custom).unwrap();
        let (out, _) = redactor.redact("using itk_abcd1234").unwrap();
        assert_eq!(out, "using itk_[REDACTED:internal]");
        assert!(
            redactor.redact("password=hunter2").is_none(),
            "rule was disabled"
        );
    }

    #[test]
    fn malformed_rules_fail_loudly() {
        let bad = vec![RedactionRuleConfig {
            name: "broken".into(),
            pattern: "([unclosed".into(),
        }];
        assert!(matches!(
            Redactor::new(&[], &bad),
            Err(PrivacyError::InvalidRule { .. })
        ));
        let everything = vec![RedactionRuleConfig {
            name: "all".into(),
            pattern: ".*".into(),
        }];
        assert!(Redactor::new(&[], &everything).is_err());
        assert!(matches!(
            Redactor::new(&["nope".into()], &[]),
            Err(PrivacyError::UnknownBuiltinRule(_))
        ));
    }

    fn event(ty: &str) -> ObservabilityEvent {
        ObservabilityEvent::new("id", ty, Provenance::AgentReported, "t", "s", Utc::now())
    }

    #[test]
    fn capture_modes_control_payload_and_redaction() {
        let mut cfg = PrivacyConfig::default();
        let mut e = event("command.failed");
        e.message = Some("password=hunter2".into());
        e.payload = Some("Bearer abcdefghijkl".into());

        let mut m = e.clone();
        CapturePolicy::from_config(&cfg).unwrap().sanitize(&mut m);
        assert!(m.payload.is_none() && m.redaction.payload_dropped);
        assert!(!m.message.as_deref().unwrap().contains("hunter2"));

        cfg.capture = CaptureMode::Redacted;
        let mut rd = e.clone();
        CapturePolicy::from_config(&cfg).unwrap().sanitize(&mut rd);
        assert!(rd
            .payload
            .as_deref()
            .unwrap()
            .contains("[REDACTED:bearer_token]"));

        cfg.capture = CaptureMode::Full;
        let mut f = e.clone();
        CapturePolicy::from_config(&cfg).unwrap().sanitize(&mut f);
        assert_eq!(f.message, e.message);
        assert_eq!(f.payload, e.payload);
    }

    #[test]
    fn collection_boundaries() {
        let cfg = PrivacyConfig {
            allowed_project_dirs: vec![PathBuf::from("/work/app")],
            excluded_file_globs: vec!["**/.env".into(), "**/secrets/**".into()],
            excluded_event_types: vec!["git.*".into(), "file.read".into()],
            excluded_agents: vec!["cursor".into()],
            ..PrivacyConfig::default()
        };
        let p = CapturePolicy::from_config(&cfg).unwrap();
        let with_path = |ty: &str, path: &str| {
            let mut e = event(ty);
            e.path = Some(path.into());
            e
        };
        assert_eq!(
            p.admit(&with_path("file.modified", "/etc/passwd")),
            Err(Exclusion::PathOutsideAllowedDirs)
        );
        assert_eq!(
            p.admit(&with_path("file.modified", "/work/app/.env")),
            Err(Exclusion::PathExcluded)
        );
        assert_eq!(
            p.admit(&with_path("file.modified", ".env")),
            Err(Exclusion::PathExcluded)
        );
        assert_eq!(
            p.admit(&with_path("file.modified", "/work/app/a/secrets/k")),
            Err(Exclusion::PathExcluded)
        );
        assert_eq!(
            p.admit(&with_path("file.modified", "/work/app/src/main.rs")),
            Ok(())
        );
        assert_eq!(p.admit(&event("git.commit")), Err(Exclusion::EventType));
        assert_eq!(p.admit(&event("file.read")), Err(Exclusion::EventType));
        let mut agent = event("agent.tool.started");
        agent.agent_name = Some("cursor".into());
        assert_eq!(p.admit(&agent), Err(Exclusion::Agent));
    }

    #[test]
    fn loss_markers_cannot_be_excluded_or_disabled() {
        let cfg = PrivacyConfig {
            capture_enabled: false,
            excluded_event_types: vec!["system.*".into()],
            ..PrivacyConfig::default()
        };
        let p = CapturePolicy::from_config(&cfg).unwrap();
        assert_eq!(p.admit(&event("system.events.dropped")), Ok(()));
        assert_eq!(
            p.admit(&event("system.collector.started")),
            Err(Exclusion::CaptureDisabled)
        );
    }

    #[test]
    fn invalid_policy_entries_fail_loudly() {
        for cfg in [
            PrivacyConfig {
                excluded_event_types: vec!["nope.*".into()],
                ..Default::default()
            },
            PrivacyConfig {
                excluded_event_types: vec!["file.modifed".into()],
                ..Default::default()
            },
            PrivacyConfig {
                excluded_file_globs: vec![String::new()],
                ..Default::default()
            },
        ] {
            assert!(CapturePolicy::from_config(&cfg).is_err());
        }
    }

    #[test]
    fn glob_semantics() {
        let g = glob_to_regex("src/*.rs").unwrap();
        assert!(g.is_match("src/main.rs"));
        assert!(!g.is_match("src/a/main.rs"), "`*` must not cross `/`");
        let g = glob_to_regex("**/auth/**").unwrap();
        assert!(g.is_match("app/auth/login.ts"));
        assert!(g.is_match("auth/x"));
        let g = glob_to_regex("a+b(c).txt").unwrap();
        assert!(g.is_match("a+b(c).txt"), "regex metacharacters are literal");
    }
}
