//! `Daemon._is_duplicate_event` / `Daemon._is_duplicate_tool_line`.
//!
//! Two independent gates, both per-session ring buffers of
//! `(signature, monotonic)`:
//!
//! * **raw events** — Codex Desktop can surface the same assistant result
//!   through more than one channel, and without this gate a final is spoken
//!   twice and stored twice. Window widens for prose (45 s) and finals
//!   (180 s), because those duplicates arrive further apart than a repeated
//!   tool call does.
//! * **tool lines** — the same rendered template inside 25 s is a repeat
//!   ("Running sed." four times); case- and whitespace-insensitive.
//!
//! Both record the signature whether or not they fire, so the NEXT repeat is
//! caught relative to the most recent sighting.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

/// `Daemon._TOOL_DUP_WINDOW_S`.
pub const TOOL_DUP_WINDOW_S: f64 = 25.0;
/// `Daemon._EVENT_DUP_WINDOW_S`.
pub const EVENT_DUP_WINDOW_S: f64 = 10.0;
/// `Daemon._PROSE_EVENT_DUP_WINDOW_S`.
pub const PROSE_EVENT_DUP_WINDOW_S: f64 = 45.0;
/// `Daemon._FINAL_EVENT_DUP_WINDOW_S`.
pub const FINAL_EVENT_DUP_WINDOW_S: f64 = 180.0;

const EVENT_RING: usize = 32;
const TOOL_RING: usize = 12;

/// `_event_dup_window_s(kind)`.
fn window_for(kind: &str) -> f64 {
    match kind {
        "final" => FINAL_EVENT_DUP_WINDOW_S,
        "intermediate" => PROSE_EVENT_DUP_WINDOW_S,
        _ => EVENT_DUP_WINDOW_S,
    }
}

/// `" ".join(text.lower().split())` — the whitespace- and case-insensitive
/// form both signatures are built from.
fn squash(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Both duplicate gates, together, because they share a shape and a lock.
#[derive(Debug)]
pub struct Dedup {
    events: Mutex<HashMap<String, VecDeque<(String, Instant)>>>,
    tool_lines: Mutex<HashMap<String, VecDeque<(String, Instant)>>>,
}

impl Default for Dedup {
    fn default() -> Self {
        Self::new()
    }
}

impl Dedup {
    /// Two empty rings.
    pub fn new() -> Self {
        Self {
            events: Mutex::new(HashMap::new()),
            tool_lines: Mutex::new(HashMap::new()),
        }
    }

    /// `_is_duplicate_event`. Empty text is never a duplicate — and is not
    /// recorded either, matching the Python's early `return False`.
    pub fn is_duplicate_event(&self, session_id: &str, kind: &str, tag: &str, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        // `_event_signature`: NUL-joined so no field can impersonate another.
        let signature = format!("{kind}\0{tag}\0{}", squash(text));
        let key = if session_id.is_empty() {
            "default"
        } else {
            session_id
        };
        let mut map = self.events.lock().unwrap_or_else(|e| e.into_inner());
        push_and_check(
            map.entry(key.to_owned()).or_default(),
            signature,
            window_for(kind),
            EVENT_RING,
        )
    }

    /// `_is_duplicate_tool_line`.
    pub fn is_duplicate_tool_line(&self, session_id: &str, text: &str) -> bool {
        let mut map = self.tool_lines.lock().unwrap_or_else(|e| e.into_inner());
        push_and_check(
            map.entry(session_id.to_owned()).or_default(),
            squash(text),
            TOOL_DUP_WINDOW_S,
            TOOL_RING,
        )
    }
}

fn push_and_check(
    ring: &mut VecDeque<(String, Instant)>,
    signature: String,
    window_s: f64,
    cap: usize,
) -> bool {
    let now = Instant::now();
    let is_dup = ring
        .iter()
        .any(|(s, ts)| *s == signature && now.duration_since(*ts).as_secs_f64() <= window_s);
    ring.push_back((signature, now));
    while ring.len() > cap {
        ring.pop_front();
    }
    is_dup
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_event_twice_is_a_duplicate_the_second_time() {
        let dedup = Dedup::new();
        assert!(!dedup.is_duplicate_event("s1", "final", "final_short", "All done"));
        assert!(dedup.is_duplicate_event("s1", "final", "final_short", "All done"));
    }

    #[test]
    fn sessions_do_not_share_a_ring() {
        let dedup = Dedup::new();
        assert!(!dedup.is_duplicate_event("s1", "final", "t", "x"));
        assert!(!dedup.is_duplicate_event("s2", "final", "t", "x"));
    }

    #[test]
    fn empty_text_is_never_a_duplicate() {
        let dedup = Dedup::new();
        assert!(!dedup.is_duplicate_event("s1", "final", "t", ""));
        assert!(!dedup.is_duplicate_event("s1", "final", "t", ""));
    }

    #[test]
    fn tool_lines_match_case_and_space_insensitively() {
        let dedup = Dedup::new();
        assert!(!dedup.is_duplicate_tool_line("s1", "Running  sed."));
        assert!(dedup.is_duplicate_tool_line("s1", "running sed."));
    }

    #[test]
    fn a_ring_is_bounded() {
        let dedup = Dedup::new();
        for i in 0..40 {
            dedup.is_duplicate_event("s1", "tool_pre", "t", &format!("line {i}"));
        }
        let map = dedup.events.lock().expect("lock");
        assert_eq!(map["s1"].len(), EVENT_RING);
    }

    #[test]
    fn windows_widen_for_prose_and_finals() {
        assert_eq!(window_for("final"), FINAL_EVENT_DUP_WINDOW_S);
        assert_eq!(window_for("intermediate"), PROSE_EVENT_DUP_WINDOW_S);
        assert_eq!(window_for("tool_pre"), EVENT_DUP_WINDOW_S);
    }
}
