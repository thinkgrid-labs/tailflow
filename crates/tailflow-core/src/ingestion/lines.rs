//! Bounded, lossy line framing shared by the line-oriented collectors.
//!
//! `tokio::io::Lines` and `BufRead::read_line` have two properties a
//! collector must not inherit:
//!
//! - **Unbounded lines.** A process that writes 1 GB without a newline makes
//!   them buffer 1 GB. Every copy of a line downstream (bus, ring, SSE
//!   subscribers) multiplies that.
//! - **UTF-8 errors end the stream.** One invalid byte returns `InvalidData`;
//!   the M1 collectors treated that as end-of-output (process, stdin) or as a
//!   collector failure (file), so capture silently stopped.
//!
//! [`BoundedLines`] and [`read_bounded_line`] keep at most [`MAX_LINE_BYTES`]
//! of a line, count (and discard) the rest up to the newline, decode lossily
//! (invalid sequences become U+FFFD), and mark a cut line with
//! [`truncation_marker`] so the loss is visible in the record itself.

use std::io::{self, BufRead};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Bytes of one line kept before the rest is discarded.
///
/// Large enough for any realistic single log line, including big JSON
/// records; storage separately truncates persisted messages at
/// [`crate::event::MAX_MESSAGE_BYTES`].
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Appended to a line that exceeded the cap; `total` is the original length
/// in bytes, excluding the newline.
pub fn truncation_marker(total: usize) -> String {
    format!(" … [tailflow: line truncated from {total} bytes]")
}

/// One line being assembled: the kept prefix plus the full length seen.
#[derive(Default)]
struct LineBuf {
    kept: Vec<u8>,
    total: usize,
    /// Last byte discarded past the cap, if any.
    last_dropped: Option<u8>,
}

impl LineBuf {
    fn extend(&mut self, chunk: &[u8], cap: usize) {
        let room = cap.saturating_sub(self.kept.len());
        let keep = chunk.len().min(room);
        if let Some(kept) = chunk.get(..keep) {
            self.kept.extend_from_slice(kept);
        }
        if keep < chunk.len() {
            self.last_dropped = chunk.last().copied();
        }
        self.total = self.total.saturating_add(chunk.len());
    }

    fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Decode and reset. A `terminated` line (a `\n` followed it) loses one
    /// trailing `\r` (CRLF input), exactly as `tokio::io::Lines` does; a final
    /// unterminated line keeps it.
    fn finish(&mut self, terminated: bool) -> String {
        let mut bytes = std::mem::take(&mut self.kept);
        let total = std::mem::take(&mut self.total);
        let last_dropped = self.last_dropped.take();
        let dropped = total.saturating_sub(bytes.len());
        // A line exactly at the cap followed by `\r\n` lost only its CR,
        // which a terminated line drops anyway: not a truncation.
        let truncated =
            dropped > 1 || (dropped == 1 && !(terminated && last_dropped == Some(b'\r')));
        if truncated {
            // The cut happens before redaction, so it must not leave a
            // fragment of a secret that a fixed-shape rule can no longer
            // recognise (`AKIA` + 10 of its 16 characters). Drop the token
            // the cut landed in, back to the last delimiter.
            drop_partial_token(&mut bytes);
            // The cut may split a multi-byte character; drop the partial tail
            // rather than render it as a replacement character.
            if let Err(e) = std::str::from_utf8(&bytes) {
                if e.error_len().is_none() {
                    bytes.truncate(e.valid_up_to());
                }
            }
        } else if terminated && dropped == 0 && bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let mut line = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        };
        if truncated {
            line.push_str(&truncation_marker(total));
        }
        line
    }
}

/// How far back from a cut to look for a token boundary.
const TOKEN_BACKOFF: usize = 512;

/// Truncate `bytes` after the last delimiter in its final [`TOKEN_BACKOFF`]
/// bytes, or drop that whole window when it holds no delimiter — so no
/// fragment of any token up to [`TOKEN_BACKOFF`] bytes survives a cut. The
/// delimiter set excludes characters that occur *inside* secrets (base64
/// `/+=`, JWT `.`, URL `:?@`), which would otherwise leave a prefix behind.
fn drop_partial_token(bytes: &mut Vec<u8>) {
    let from = bytes.len().saturating_sub(TOKEN_BACKOFF);
    let tail = bytes.get(from..).unwrap_or_default();
    let boundary = tail.iter().rposition(|b| {
        matches!(
            b,
            b' ' | b'\t'
                | b','
                | b';'
                | b'"'
                | b'\''
                | b'&'
                | b'('
                | b')'
                | b'['
                | b']'
                | b'{'
                | b'}'
        )
    });
    match boundary {
        Some(i) => bytes.truncate(from.saturating_add(i).saturating_add(1)),
        None => bytes.truncate(from),
    }
}

/// Bounded replacement for `tokio::io::Lines`.
///
/// `next_line` is cancel-safe: a partially read line is kept in `self` and
/// bytes are consumed from the reader only after they have been copied, so it
/// can be used as a `tokio::select!` branch.
pub struct BoundedLines<R> {
    reader: R,
    line: LineBuf,
    cap: usize,
}

impl<R: AsyncBufRead + Unpin> BoundedLines<R> {
    pub fn new(reader: R) -> Self {
        Self::with_cap(reader, MAX_LINE_BYTES)
    }

    pub fn with_cap(reader: R, cap: usize) -> Self {
        Self {
            reader,
            line: LineBuf::default(),
            cap: cap.max(1),
        }
    }

    /// The next line without its terminator, `None` at end of stream. A final
    /// line without a trailing newline is returned, as `Lines` does.
    ///
    /// # Errors
    /// Only I/O errors from the reader; invalid UTF-8 is decoded lossily.
    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                return Ok((!self.line.is_empty()).then(|| self.line.finish(false)));
            }
            if let Some(i) = available.iter().position(|&b| b == b'\n') {
                self.line
                    .extend(available.get(..i).unwrap_or_default(), self.cap);
                self.reader.consume(i.saturating_add(1));
                return Ok(Some(self.line.finish(true)));
            }
            let n = available.len();
            self.line.extend(available, self.cap);
            self.reader.consume(n);
        }
    }
}

/// Result of [`read_bounded_line`].
#[derive(Debug, PartialEq, Eq)]
pub enum LineRead {
    /// A complete newline-terminated line (terminator removed).
    Line(String),
    /// End of input reached in the middle of a line; `consumed` bytes of it
    /// were read and should be re-read once the writer finishes the line.
    Partial { consumed: usize },
    /// End of input at a line boundary.
    Eof,
}

/// Synchronous bounded line read for file tailing, where an unterminated
/// final line must be retried later rather than emitted.
///
/// # Errors
/// Only I/O errors from the reader.
pub fn read_bounded_line<R: BufRead>(reader: &mut R, cap: usize) -> io::Result<LineRead> {
    let mut line = LineBuf::default();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if line.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Partial {
                    consumed: line.total,
                }
            });
        }
        if let Some(i) = available.iter().position(|&b| b == b'\n') {
            line.extend(available.get(..i).unwrap_or_default(), cap.max(1));
            reader.consume(i.saturating_add(1));
            return Ok(LineRead::Line(line.finish(true)));
        }
        let n = available.len();
        line.extend(available, cap.max(1));
        reader.consume(n);
    }
}

/// Apply the line cap to a payload that arrived already framed (Docker log
/// frames). Cuts on a character boundary and appends the marker.
pub fn cap_payload(mut payload: String, cap: usize) -> String {
    // Same rules as a cut line: char boundary, and no partial token.
    if payload.len() <= cap {
        return payload;
    }
    let total = payload.len();
    let mut cut = cap;
    while cut > 0 && !payload.is_char_boundary(cut) {
        cut -= 1;
    }
    payload.truncate(cut);
    let mut bytes = payload.into_bytes();
    drop_partial_token(&mut bytes);
    // Dropping a delimiter-free window can end inside a character.
    if let Err(e) = std::str::from_utf8(&bytes) {
        if e.error_len().is_none() {
            bytes.truncate(e.valid_up_to());
        }
    }
    let mut payload = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    payload.push_str(&truncation_marker(total));
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::BufReader;

    async fn all(input: &[u8], cap: usize, buf: usize) -> Vec<String> {
        let reader = BufReader::with_capacity(buf, input);
        let mut lines = BoundedLines::with_cap(reader, cap);
        let mut out = Vec::new();
        while let Some(l) = lines.next_line().await.expect("in-memory read") {
            out.push(l);
        }
        out
    }

    #[tokio::test]
    async fn matches_lines_for_ordinary_input() {
        let input = b"one\ntwo\r\n\nthree";
        assert_eq!(all(input, 1024, 8192).await, ["one", "two", "", "three"]);
        // Tiny buffer: lines span many fill_buf calls.
        assert_eq!(all(input, 1024, 1).await, ["one", "two", "", "three"]);
        assert!(all(b"", 1024, 8).await.is_empty());
    }

    #[tokio::test]
    async fn invalid_utf8_is_replaced_and_reading_continues() {
        let input = b"ok\nbad \xff\xfe byte\nafter\n";
        assert_eq!(
            all(input, 1024, 4).await,
            ["ok", "bad \u{fffd}\u{fffd} byte", "after"]
        );
    }

    #[tokio::test]
    async fn a_cut_never_keeps_a_fragment_of_the_token_it_split() {
        // The secret straddles the cap: none of it may survive the cut.
        let mut input = b"user=alice key=".to_vec();
        input.extend_from_slice(b"AKIAABCDEFGHIJKLMNOP");
        input.extend_from_slice(b" tail\n");
        let cap = "user=alice key=AKIAABCD".len();
        let out = all(&input, cap, 8).await;
        assert_eq!(
            out,
            [format!("user=alice {}", truncation_marker(input.len() - 1))]
        );
    }

    #[tokio::test]
    async fn oversized_line_is_cut_marked_and_the_next_line_survives() {
        let mut input = vec![b'a'; 10_000];
        input.extend_from_slice(b"\nnext\n");
        let out = all(&input, 100, 64).await;
        assert_eq!(out.len(), 2);
        // A delimiter-free run shorter than the back-off window is dropped
        // whole: no fragment of it survives the cut.
        assert_eq!(out[0], truncation_marker(10_000));
        assert_eq!(out[1], "next");
    }

    #[tokio::test]
    async fn cut_never_splits_a_multibyte_character() {
        // 800 bytes of a 2-byte character, no delimiter: the 512-byte window
        // is dropped and the cut at byte 89 would split a character.
        let mut input = "é".repeat(400).into_bytes();
        input.push(b'\n');
        let out = all(&input, 601, 64).await;
        assert_eq!(
            out,
            [format!("{}{}", "é".repeat(44), truncation_marker(800))]
        );
    }

    #[tokio::test]
    async fn a_delimiter_free_window_is_dropped_whole() {
        // A secret with no delimiter before it within 512 bytes.
        let mut input = "x".repeat(700).into_bytes();
        input.extend_from_slice(b"skABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\n");
        let out = all(&input, 710, 64).await;
        assert_eq!(out.len(), 1);
        assert!(!out[0].contains("sk"), "fragment survived: {:?}", out[0]);
        assert!(out[0].starts_with(&"x".repeat(198)));
    }

    #[tokio::test]
    async fn carriage_returns_match_tokio_lines() {
        // Exactly the cap followed by CRLF is not a truncation.
        assert_eq!(all(b"abcde\r\nnext\n", 5, 3).await, ["abcde", "next"]);
        // A final unterminated line keeps its CR, as `Lines` does.
        assert_eq!(all(b"one\r\nabc\r", 1024, 4).await, ["one", "abc\r"]);
    }

    #[tokio::test]
    async fn memory_stays_bounded_for_a_huge_unterminated_line() {
        let input = vec![b'x'; 4 * 1024 * 1024];
        let reader = BufReader::with_capacity(8192, &input[..]);
        let mut lines = BoundedLines::with_cap(reader, 1024);
        let line = lines.next_line().await.expect("read").expect("one line");
        assert!(line.len() < 1024 + 64, "kept {} bytes", line.len());
        assert!(line.ends_with(&truncation_marker(4 * 1024 * 1024)));
        assert!(lines.next_line().await.expect("read").is_none());
    }

    #[test]
    fn sync_reader_reports_partial_lines_for_retry() {
        let mut r = Cursor::new(b"done\nhalf".to_vec());
        assert_eq!(
            read_bounded_line(&mut r, 1024).expect("read"),
            LineRead::Line("done".into())
        );
        assert_eq!(
            read_bounded_line(&mut r, 1024).expect("read"),
            LineRead::Partial { consumed: 4 }
        );
        assert_eq!(
            read_bounded_line(&mut r, 1024).expect("read"),
            LineRead::Eof
        );
    }

    #[test]
    fn sync_reader_bounds_and_decodes_lossily() {
        let mut input = vec![b'z'; 5000];
        input.extend_from_slice(b"\n\xffok\n");
        let mut r = io::BufReader::with_capacity(16, Cursor::new(input));
        let LineRead::Line(first) = read_bounded_line(&mut r, 10).expect("read") else {
            panic!("expected a line");
        };
        assert_eq!(first, truncation_marker(5000));
        assert_eq!(
            read_bounded_line(&mut r, 10).expect("read"),
            LineRead::Line("\u{fffd}ok".into())
        );
    }

    #[test]
    fn cap_payload_cuts_on_char_boundary() {
        assert_eq!(cap_payload("short".into(), 10), "short");
        assert_eq!(
            cap_payload("id=1 secret=ABCDEFGH".into(), 15),
            format!("id=1 {}", truncation_marker(20))
        );
        assert_eq!(cap_payload("ééé".into(), 3), truncation_marker(6));
        let long = format!("{} {}", "a".repeat(10), "é".repeat(400));
        let capped = cap_payload(long.clone(), 601);
        assert!(capped.is_char_boundary(capped.len()));
        assert!(capped.ends_with(&truncation_marker(long.len())));
    }
}
