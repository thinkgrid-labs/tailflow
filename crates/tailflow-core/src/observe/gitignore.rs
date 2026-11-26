//! A bounded subset of `.gitignore` matching, for deciding what the
//! filesystem observer ignores.
//!
//! Supported: comments (`#`), blank lines, negation (`!`), directory-only
//! patterns (trailing `/`), anchored patterns (a leading or inner `/`),
//! `*`, `?`, `**`, and character classes (`[abc]`, `[a-z]`, `[!x]`).
//! Escapes (`\#`, `\!`, `\ `) are honoured. Trailing spaces are trimmed.
//!
//! Not supported (treated as literal or skipped, never as a crash): the
//! rarely used `[[:class:]]` forms. Files larger than [`MAX_FILE_BYTES`] and
//! more than [`MAX_RULES`] rules per file are cut off, which can only make
//! the observer watch *more*, never less.

use regex::Regex;
use std::path::{Path, PathBuf};

pub const MAX_FILE_BYTES: u64 = 256 * 1024;
pub const MAX_RULES: usize = 2_000;

#[derive(Debug)]
struct Rule {
    re: Regex,
    negate: bool,
    dir_only: bool,
}

/// The rules of one ignore file, relative to the directory it applies to.
#[derive(Debug)]
pub struct IgnoreFile {
    /// Directory the patterns are relative to.
    base: PathBuf,
    rules: Vec<Rule>,
}

/// Outcome for one path in one file: the last matching rule wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ignored,
    Included,
    NoMatch,
}

impl IgnoreFile {
    /// Parse `text` as rules relative to `base`.
    pub fn parse(base: &Path, text: &str) -> Self {
        let mut rules = Vec::new();
        for raw in text.lines().take(MAX_RULES.saturating_mul(4)) {
            if rules.len() >= MAX_RULES {
                break;
            }
            if let Some(rule) = parse_line(raw) {
                rules.push(rule);
            }
        }
        Self {
            base: base.to_path_buf(),
            rules,
        }
    }

    /// Read `path` (bounded). Missing or unreadable files have no rules.
    pub fn load(base: &Path, path: &Path) -> Option<Self> {
        use std::io::Read;
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() {
            return None;
        }
        let f = std::fs::File::open(path).ok()?;
        let mut buf = Vec::new();
        f.take(MAX_FILE_BYTES).read_to_end(&mut buf).ok()?;
        Some(Self::parse(base, &String::from_utf8_lossy(&buf)))
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Match `path` (absolute, under `base`).
    pub fn verdict(&self, path: &Path, is_dir: bool) -> Verdict {
        let Ok(rel) = path.strip_prefix(&self.base) else {
            return Verdict::NoMatch;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() {
            return Verdict::NoMatch;
        }
        let mut out = Verdict::NoMatch;
        for r in &self.rules {
            if r.dir_only && !is_dir {
                continue;
            }
            if r.re.is_match(&rel) {
                out = if r.negate {
                    Verdict::Included
                } else {
                    Verdict::Ignored
                };
            }
        }
        out
    }
}

fn parse_line(raw: &str) -> Option<Rule> {
    let line = raw.strip_suffix('\r').unwrap_or(raw);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    // Trailing unescaped spaces are not part of the pattern.
    let mut line = line.to_string();
    while line.ends_with(' ') && !line.ends_with("\\ ") {
        line.pop();
    }
    if line.is_empty() {
        return None;
    }
    let (negate, body) = match line.strip_prefix('!') {
        Some(b) => (true, b.to_string()),
        None => (false, line),
    };
    let body = body
        .strip_prefix("\\!")
        .map(|b| format!("!{b}"))
        .or_else(|| body.strip_prefix("\\#").map(|b| format!("#{b}")))
        .unwrap_or(body);
    let (dir_only, body) = match body.strip_suffix('/') {
        Some(b) => (true, b.to_string()),
        None => (false, body),
    };
    if body.is_empty() || body.len() > 1024 {
        return None;
    }
    // A slash anywhere except at the end anchors the pattern to `base`;
    // otherwise it matches a name at any depth.
    let anchored = body.contains('/');
    let body = body.strip_prefix('/').unwrap_or(&body).to_string();
    let core = glob_body(&body)?;
    let re = if anchored {
        format!("^{core}(?:/.*)?$")
    } else {
        format!("^(?:.*/)?{core}(?:/.*)?$")
    };
    Regex::new(&re).ok().map(|re| Rule {
        re,
        negate,
        dir_only,
    })
}

/// Translate one glob into a regex fragment (no anchors).
fn glob_body(glob: &str) -> Option<String> {
    let chars: Vec<char> = glob.chars().collect();
    let mut re = String::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        match c {
            '*' if chars.get(i + 1) == Some(&'*') => {
                let at_start = i == 0 || chars.get(i.wrapping_sub(1)) == Some(&'/');
                let slash_after = chars.get(i + 2) == Some(&'/');
                if at_start && slash_after {
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
            '[' => {
                // A class up to the next `]` (a `]` right after `[` or `[!`
                // is literal).
                let mut j = i + 1;
                let mut class = String::from("[");
                if matches!(chars.get(j), Some('!' | '^')) {
                    class.push('^');
                    j += 1;
                }
                if chars.get(j) == Some(&']') {
                    class.push_str("\\]");
                    j += 1;
                }
                let mut closed = false;
                while let Some(&cc) = chars.get(j) {
                    if cc == ']' {
                        closed = true;
                        break;
                    }
                    if cc == '\\' || cc == '[' {
                        class.push('\\');
                    }
                    class.push(cc);
                    j += 1;
                }
                if closed {
                    class.push(']');
                    re.push_str(&class);
                    i = j + 1;
                    continue;
                }
                re.push_str("\\[");
            }
            '\\' => {
                if let Some(&n) = chars.get(i + 1) {
                    re.push_str(&regex::escape(&n.to_string()));
                    i += 2;
                    continue;
                }
            }
            other => re.push_str(&regex::escape(&other.to_string())),
        }
        i += 1;
    }
    Some(re)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(text: &str) -> IgnoreFile {
        IgnoreFile::parse(Path::new("/r"), text)
    }

    fn ig(file: &IgnoreFile, p: &str, dir: bool) -> Verdict {
        file.verdict(&Path::new("/r").join(p), dir)
    }

    #[test]
    fn names_match_at_any_depth_and_anchors_do_not() {
        let file = f("*.log\n/build\ndocs/*.tmp\n");
        assert_eq!(ig(&file, "a.log", false), Verdict::Ignored);
        assert_eq!(ig(&file, "x/y/a.log", false), Verdict::Ignored);
        assert_eq!(ig(&file, "build", true), Verdict::Ignored);
        assert_eq!(
            ig(&file, "build/out.o", false),
            Verdict::Ignored,
            "inside an ignored dir"
        );
        assert_eq!(ig(&file, "src/build", true), Verdict::NoMatch, "anchored");
        assert_eq!(ig(&file, "docs/a.tmp", false), Verdict::Ignored);
        assert_eq!(ig(&file, "x/docs/a.tmp", false), Verdict::NoMatch);
        assert_eq!(ig(&file, "a.rs", false), Verdict::NoMatch);
    }

    #[test]
    fn negation_dir_only_and_double_star() {
        let file = f("*.env\n!example.env\ncache/\n**/gen/**\n# comment\n\n");
        assert_eq!(ig(&file, "prod.env", false), Verdict::Ignored);
        assert_eq!(ig(&file, "example.env", false), Verdict::Included);
        assert_eq!(ig(&file, "cache", true), Verdict::Ignored);
        assert_eq!(ig(&file, "cache", false), Verdict::NoMatch, "dir-only");
        assert_eq!(ig(&file, "a/gen/b.rs", false), Verdict::Ignored);
        assert_eq!(ig(&file, "gen/b.rs", false), Verdict::Ignored);
    }

    #[test]
    fn classes_escapes_and_hostile_input() {
        let file = f("file[0-9].txt\n[!a]x\n\\#hash\n\\!bang\nweird[\n");
        assert_eq!(ig(&file, "file3.txt", false), Verdict::Ignored);
        assert_eq!(ig(&file, "filex.txt", false), Verdict::NoMatch);
        assert_eq!(ig(&file, "bx", false), Verdict::Ignored);
        assert_eq!(ig(&file, "ax", false), Verdict::NoMatch);
        assert_eq!(ig(&file, "#hash", false), Verdict::Ignored);
        assert_eq!(ig(&file, "!bang", false), Verdict::Ignored);
        assert_eq!(
            ig(&file, "weird[", false),
            Verdict::Ignored,
            "unclosed class is literal"
        );
        // Huge or regex-hostile input never panics and stays bounded.
        let big = "(.*)+\n".repeat(10_000);
        let file = f(&big);
        assert!(file.rules.len() <= MAX_RULES);
    }

    #[test]
    fn paths_outside_the_base_do_not_match() {
        let file = f("*\n");
        assert_eq!(
            file.verdict(Path::new("/elsewhere/a"), false),
            Verdict::NoMatch
        );
    }
}
