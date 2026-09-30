//! `client.extract_assistant_texts_from` and `client.extract_last_assistant_text`.
//!
//! The agent's transcript is a JSONL file the CLI appends to. The hook
//! processing reads it INCREMENTALLY — from the byte offset the last hook for
//! that session left off at — so a fifty-tool-call session does not re-parse
//! the whole file fifty times.
//!
//! Every failure here is swallowed and reported as "no new text", exactly as
//! the Python does: a transcript that is missing, truncated, rotated or half
//! written must never be the reason the daemon stops narrating.

use std::fs;
use std::ops::ControlFlow;
use std::path::Path;

use heard_state::jsonl;
use serde_json::Value;

/// `extract_assistant_texts_from(path, start_offset) -> (texts, end_offset)`.
///
/// A `start_offset` past the current end of file means the transcript was
/// rotated or truncated, and the read restarts from 0 — which is what makes
/// the `spoken` hash set (not the offset) the real dedup authority.
pub fn extract_assistant_texts_from(path: &Path, start_offset: u64) -> (Vec<String>, u64) {
    let Ok(meta) = fs::metadata(path) else {
        return (Vec::new(), start_offset);
    };
    let size = meta.len();
    let start = if start_offset > size { 0 } else { start_offset };

    // Streamed from `start`, one line in memory at a time — never the whole
    // file (a 38 MB transcript copied onto the heap per
    // hook event).
    let mut out = Vec::new();
    let mut undecodable = false;
    let read = jsonl::for_each_line_from(path, start, jsonl::MAX_LINE_BYTES, |raw| {
        // Python seeks by BYTES and then decodes. A start offset that lands
        // mid-character (or any undecodable byte) makes CPython raise inside
        // the decoder and the read return `([], start_offset)`.
        let Ok(line) = std::str::from_utf8(raw) else {
            undecodable = true;
            return ControlFlow::Break(());
        };
        if let Some(message) = assistant_message(line) {
            for block in content_blocks(&message) {
                if block.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if !text.is_empty() {
                    out.push(text.to_owned());
                }
            }
        }
        ControlFlow::Continue(())
    });
    match read {
        // `f.tell()` after the iterator is exhausted: end of file.
        Ok(end) if !undecodable => (out, end),
        _ => (Vec::new(), start_offset),
    }
}

/// `extract_last_assistant_text(path)` — the last assistant message's text
/// blocks, space-joined. `""` when there is none.
///
/// Python scans the whole file forward and keeps the last hit; the same
/// answer is found here by walking BACKWARDS from end of file and stopping at
/// the first hit, so a PreToolUse on a 38 MB transcript reads a few kilobytes
/// instead of the file. A line that is not UTF-8 is skipped.
pub fn extract_last_assistant_text(path: &Path) -> String {
    let found = jsonl::rfind_line(path, jsonl::MAX_LINE_BYTES, |raw| {
        let message = assistant_message(std::str::from_utf8(raw).ok()?)?;
        let joined = content_blocks(&message)
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let joined = joined.trim();
        (!joined.is_empty()).then(|| joined.to_owned())
    });
    found.ok().flatten().unwrap_or_default()
}

/// `json.loads(line)` + `msg.get("type") == "assistant"`, with the cheap
/// type pre-check so a large non-assistant line is never expanded into a
/// `Value` tree.
fn assistant_message(line: &str) -> Option<Value> {
    if jsonl::definitely_not_type(line, "assistant") {
        return None;
    }
    let message = serde_json::from_str::<Value>(line).ok()?;
    (message.get("type").and_then(Value::as_str) == Some("assistant")).then_some(message)
}

/// `msg.get("message", {}).get("content", [])`.
fn content_blocks(message: &Value) -> Vec<&Value> {
    message
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().collect())
        .unwrap_or_default()
}

/// The transcript's size on disk, for the muted offset advance. `None` when
/// it cannot be stat'd — `os.path.getsize`'s `OSError`.
pub fn size_of(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|m| m.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, lines: &[&str]) -> std::path::PathBuf {
        let path = dir.join("transcript.jsonl");
        fs::write(&path, lines.join("\n") + "\n").expect("write");
        path
    }

    #[test]
    fn only_assistant_text_blocks_come_back() {
        let dir = crate::testing::temp_dir("transcript-basic");
        let path = write(
            &dir,
            &[
                r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":" one "},{"type":"tool_use"}]}}"#,
                "not json at all",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"two"}]}}"#,
            ],
        );
        let (texts, end) = extract_assistant_texts_from(&path, 0);
        assert_eq!(texts, vec!["one".to_string(), "two".to_string()]);
        assert_eq!(end, fs::metadata(&path).expect("stat").len());
        assert_eq!(extract_last_assistant_text(&path), "two");
    }

    #[test]
    fn an_incremental_read_only_sees_the_tail() {
        let dir = crate::testing::temp_dir("transcript-incremental");
        let first = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"one"}]}}"#;
        let path = write(&dir, &[first]);
        let (_, end) = extract_assistant_texts_from(&path, 0);
        fs::write(
            &path,
            format!(
                "{first}\n{}\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"two"}]}}"#
            ),
        )
        .expect("append");
        let (texts, _) = extract_assistant_texts_from(&path, end);
        assert_eq!(texts, vec!["two".to_string()]);
    }

    #[test]
    fn an_offset_past_eof_rereads_from_the_start() {
        let dir = crate::testing::temp_dir("transcript-rotated");
        let path = write(
            &dir,
            &[r#"{"type":"assistant","message":{"content":[{"type":"text","text":"one"}]}}"#],
        );
        let (texts, _) = extract_assistant_texts_from(&path, 99_999);
        assert_eq!(texts, vec!["one".to_string()]);
    }

    #[test]
    fn a_missing_transcript_is_silent() {
        let dir = crate::testing::temp_dir("transcript-missing");
        let path = dir.join("nope.jsonl");
        assert_eq!(extract_assistant_texts_from(&path, 7), (Vec::new(), 7));
        assert_eq!(extract_last_assistant_text(&path), "");
        assert_eq!(size_of(&path), None);
    }
}
