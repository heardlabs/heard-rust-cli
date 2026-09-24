//! Bounded-memory line readers for append-only JSONL files (agent
//! transcripts, mostly).
//!
//! A Claude Code transcript for a long session is tens of megabytes and the
//! hook path touches it on every PreToolUse and Stop. Reading it with
//! `fs::read_to_string` put a whole copy of the file on the heap per event;
//! on the e2e stream (a 36–38 MB transcript, 401 events) that took the
//! daemon's RSS to 1.16 GB, because the allocator did not hand the freed
//! multi-megabyte buffers back between events (an unbounded-memory bug, now fixed).
//!
//! These readers never hold more than one line (plus one [`CHUNK`]) at a time:
//!
//! * [`for_each_line_from`] streams forward from a byte offset — Python's
//!   `f.seek(offset); for line in f`.
//! * [`rfind_line`] walks backwards from end of file and stops at the first
//!   line (counting from the end) a predicate accepts — the cheap way to
//!   answer "the LAST line that matches", which the forward Python loop
//!   computes by scanning every line.
//!
//! A line longer than `max_line` bytes is skipped without being buffered, so
//! one pathological line (a multi-megabyte tool result) cannot blow the bound
//! either. Lines are handed over as raw bytes; the trailing `\n` (and a `\r`
//! before it) is stripped.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::ops::ControlFlow;
use std::path::Path;

/// Read granularity, forward and backward.
pub const CHUNK: usize = 64 * 1024;

/// The default per-line cap. Far above any assistant-prose line a transcript
/// carries; a line over it is skipped, not buffered.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

fn trim_eol(mut line: &[u8]) -> &[u8] {
    if let [rest @ .., b'\n'] = line {
        line = rest;
    }
    if let [rest @ .., b'\r'] = line {
        line = rest;
    }
    line
}

/// Stream the lines of `path` from byte `start` to end of file, calling `f`
/// with each one (a final line without a newline included, as Python's line
/// iterator does). Lines over `max_line` bytes are skipped unseen.
///
/// Returns the offset just past the last byte consumed — end of file unless
/// `f` broke early — which is what Python's `f.tell()` reports after the loop.
pub fn for_each_line_from<F>(path: &Path, start: u64, max_line: usize, mut f: F) -> io::Result<u64>
where
    F: FnMut(&[u8]) -> ControlFlow<()>,
{
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::with_capacity(CHUNK, file);
    let mut offset = start;
    let mut line: Vec<u8> = Vec::new();
    let mut oversize = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let (take, complete) = match memchr(b'\n', buf) {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        if !oversize {
            if line.len() + take > max_line.saturating_add(2) {
                oversize = true;
                line = Vec::new();
            } else {
                line.extend_from_slice(&buf[..take]);
            }
        }
        reader.consume(take);
        offset += take as u64;
        if complete {
            let skip = std::mem::replace(&mut oversize, false);
            if !skip {
                let flow = f(trim_eol(&line));
                line.clear();
                if flow.is_break() {
                    return Ok(offset);
                }
            }
            // Give back a buffer an unusually long line grew, so one big
            // line does not pin its capacity for the rest of the read.
            if line.capacity() > 4 * CHUNK {
                line = Vec::new();
            }
        }
    }
    if !oversize && !line.is_empty() {
        let _ = f(trim_eol(&line));
    }
    Ok(offset)
}

/// Walk `path` backwards from end of file, handing each line (last line
/// first) to `f` until it returns `Some`. Lines over `max_line` bytes are
/// skipped unseen. `Ok(None)` when no line matched.
pub fn rfind_line<T, F>(path: &Path, max_line: usize, mut f: F) -> io::Result<Option<T>>
where
    F: FnMut(&[u8]) -> Option<T>,
{
    let mut file = File::open(path)?;
    let mut pos = file.metadata()?.len();
    // The line currently being assembled, as chunks in REVERSE file order
    // (the piece nearest end of file first), so prepending never copies.
    let mut pieces: Vec<Vec<u8>> = Vec::new();
    let mut pieces_len = 0usize;
    let mut oversize = false;
    let mut chunk = vec![0u8; CHUNK];

    // Assemble `head` + pieces into one line and test it.
    let mut finish =
        |head: &[u8], pieces: &mut Vec<Vec<u8>>, pieces_len: &mut usize, oversize: &mut bool| {
            let skip = std::mem::replace(oversize, false)
                || head.len() + *pieces_len > max_line.saturating_add(2);
            let hit = if skip {
                None
            } else if pieces.is_empty() {
                f(trim_eol(head))
            } else {
                let mut line = Vec::with_capacity(head.len() + *pieces_len);
                line.extend_from_slice(head);
                for p in pieces.iter().rev() {
                    line.extend_from_slice(p);
                }
                f(trim_eol(&line))
            };
            pieces.clear();
            *pieces_len = 0;
            hit
        };

    // A file that ends in "\n" has an empty final segment; Python's iterator
    // (or `str::lines`) yields no line for it, and neither does this.
    let nonempty = pos > 0;
    let mut first_segment = true;
    while pos > 0 {
        let n = (pos as usize).min(CHUNK);
        pos -= n as u64;
        file.seek(SeekFrom::Start(pos))?;
        let buf = &mut chunk[..n];
        file.read_exact(buf)?;
        let mut end = n;
        while let Some(i) = memrchr(b'\n', &buf[..end]) {
            // buf[i+1..end] is the head of the line whose tail is `pieces`.
            let head = &buf[i + 1..end];
            let trailing = first_segment && head.is_empty() && pieces_len == 0 && !oversize;
            first_segment = false;
            if !trailing {
                if let Some(hit) = finish(head, &mut pieces, &mut pieces_len, &mut oversize) {
                    return Ok(Some(hit));
                }
            }
            end = i;
        }
        // buf[..end] continues into the previous chunk.
        if !oversize && end > 0 {
            if pieces_len + end > max_line.saturating_add(2) {
                oversize = true;
                pieces.clear();
                pieces_len = 0;
            } else {
                pieces.push(buf[..end].to_vec());
                pieces_len += end;
            }
        }
    }
    // The file's first line: everything before its first newline.
    let trailing = first_segment && pieces_len == 0 && !oversize;
    if nonempty && !trailing {
        return Ok(finish(&[], &mut pieces, &mut pieces_len, &mut oversize));
    }
    Ok(None)
}

/// `true` only when `line` is certainly a JSON record whose `"type"` is not
/// `want` — decided without building a DOM (every other field is skipped, not
/// allocated). Anything this cheap pass cannot vouch for (invalid JSON, a
/// non-string or duplicated `type`, Python-only literals like `NaN`) answers
/// `false`, so the caller falls back to its full parse and the result is
/// exactly what the full parse alone would have produced.
///
/// This is what keeps a multi-megabyte tool-result line (`"type":"user"`)
/// from being expanded into a `Value` tree just to be thrown away.
pub fn definitely_not_type(line: &str, want: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Head<'a> {
        #[serde(rename = "type", borrow, default)]
        kind: Option<std::borrow::Cow<'a, str>>,
    }
    match serde_json::from_str::<Head>(line) {
        Ok(head) => head.kind.as_deref() != Some(want),
        Err(_) => false,
    }
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

fn memrchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().rposition(|&b| b == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp(tag: &str, body: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("heard-jsonl-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("f.jsonl");
        fs::write(&path, body).expect("write");
        path
    }

    fn forward(path: &Path, start: u64, cap: usize) -> (Vec<String>, u64) {
        let mut out = Vec::new();
        let end = for_each_line_from(path, start, cap, |l| {
            out.push(String::from_utf8_lossy(l).into_owned());
            ControlFlow::Continue(())
        })
        .expect("read");
        (out, end)
    }

    fn backward(path: &Path, cap: usize) -> Vec<String> {
        let mut out = Vec::new();
        let none: Option<()> = rfind_line(path, cap, |l| {
            out.push(String::from_utf8_lossy(l).into_owned());
            None
        })
        .expect("read");
        assert!(none.is_none());
        out.reverse();
        out
    }

    fn std_lines(body: &str) -> Vec<String> {
        body.lines().map(str::to_owned).collect()
    }

    #[test]
    fn forward_and_backward_agree_with_str_lines() {
        let long = "x".repeat(CHUNK * 3 + 17);
        let bodies = [
            String::new(),
            "a".into(),
            "a\n".into(),
            "a\nb".into(),
            "a\r\nb\r\n".into(),
            "\n\n".into(),
            format!("one\n{long}\nthree"),
            format!("{long}\n{long}\n"),
        ];
        for (i, body) in bodies.iter().enumerate() {
            let path = temp(&format!("agree{i}"), body.as_bytes());
            let want = std_lines(body);
            let (got, end) = forward(&path, 0, MAX_LINE_BYTES);
            assert_eq!(got, want, "forward {i}");
            assert_eq!(end, body.len() as u64, "end {i}");
            assert_eq!(backward(&path, MAX_LINE_BYTES), want, "backward {i}");
        }
    }

    #[test]
    fn forward_starts_at_the_offset() {
        let path = temp("offset", b"first\nsecond\nthird\n");
        assert_eq!(
            forward(&path, 6, MAX_LINE_BYTES),
            (vec!["second".into(), "third".into()], 19)
        );
        assert_eq!(forward(&path, 19, MAX_LINE_BYTES), (vec![], 19));
    }

    #[test]
    fn oversize_lines_are_skipped_both_ways() {
        let big = "y".repeat(CHUNK * 2);
        let body = format!("a\n{big}\nb\n");
        let path = temp("oversize", body.as_bytes());
        let (got, end) = forward(&path, 0, 1000);
        assert_eq!(got, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(end, body.len() as u64);
        assert_eq!(
            backward(&path, 1000),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn the_type_prefilter_only_vouches_for_clean_records() {
        assert!(definitely_not_type(
            r#"{"type":"user","x":[1,2,{"a":"b"}]}"#,
            "assistant"
        ));
        assert!(definitely_not_type(r#"{"x":1}"#, "assistant"));
        assert!(!definitely_not_type(r#"{"type":"assistant"}"#, "assistant"));
        assert!(!definitely_not_type(r#"{"type":"assistant"}"#, "assistant"));
        assert!(!definitely_not_type("not json", "assistant"));
        assert!(!definitely_not_type(
            r#"{"type":"user","type":"assistant"}"#,
            "assistant"
        ));
        assert!(!definitely_not_type(r#"{"type":5}"#, "assistant"));
        assert!(!definitely_not_type(
            r#"{"type":"user","n":NaN}"#,
            "assistant"
        ));
    }

    #[test]
    fn rfind_stops_at_the_last_match() {
        let path = temp("rfind", b"k1\nv\nk2\nv\n");
        let hit = rfind_line(&path, MAX_LINE_BYTES, |l| {
            l.starts_with(b"k").then(|| l.to_vec())
        })
        .expect("read");
        assert_eq!(hit.as_deref(), Some(&b"k2"[..]));
    }
}
