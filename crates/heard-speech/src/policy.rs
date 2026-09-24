//! The speech queue's decisions, as pure functions over a list.
//!
//! Everything `daemon.py` decides about WHAT plays and in which ORDER lives
//! here, with no clock, no lock and no audio, so the golden corpus
//! (`fixtures/speech/speech.json`, generated from the live
//! `Daemon._enqueue_speech` / `_flush_deferred_while_mic` / `_split`) can pin
//! it exactly. [`crate::queue`] holds the lock and calls these.

use heard_state::py::is_space;

/// `_queue_max`.
pub const QUEUE_MAX: usize = 5;
/// `_DEFERRED_MIC_MAX`.
pub const DEFERRED_MIC_MAX: usize = 10;
/// `_DEFERRED_MAX_AGE_S`.
pub const DEFERRED_MAX_AGE_S: f64 = 300.0;
/// `sequencer.ROUTINE_KINDS`.
pub const ROUTINE_KINDS: &[&str] = &["tool_pre", "tool_post", "intermediate"];

/// What the policy needs to know about a queued line.
pub trait Queued {
    /// `item[3]` — the session id (`""` for session-less lines).
    fn session(&self) -> &str;
    /// `item[5]["kind"]`.
    fn kind(&self) -> &str;
}

/// What [`enqueue`] dropped, for the structured log lines.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dropped {
    /// `queue_drop_other_session`.
    pub other_session: usize,
    /// `queue_drop` with `final_supersedes_routine` (session priority) or
    /// `queue_drop_stale` (session-less priority).
    pub superseded: usize,
    /// `queue_drop` — the cap.
    pub cap: usize,
}

/// `_enqueue_speech`'s list surgery.
///
/// * A live line (`coexists == false`) from a session clears every queued
///   line of OTHER sessions — the freshest session wins the one speaker.
/// * A priority line drops its own session's pending ROUTINE lines (the
///   result subsumes them) and lands right after its session's last
///   remaining line, jumping other sessions but never its own earlier lines.
///   A session-less priority line drops queued `intermediate` prose and goes
///   to the front.
/// * The cap keeps the FIRST `max` after a priority insert and the LAST
///   `max` after a normal append.
pub fn enqueue<T: Queued>(
    queue: &mut Vec<T>,
    item: T,
    priority: bool,
    coexists: bool,
    max: usize,
) -> Dropped {
    let mut dropped = Dropped::default();
    let session = item.session().to_owned();
    if !session.is_empty() && !queue.is_empty() && !coexists {
        let before = queue.len();
        queue.retain(|e| e.session() == session);
        dropped.other_session = before - queue.len();
    }
    if priority {
        let before = queue.len();
        if session.is_empty() {
            queue.retain(|e| e.kind() != "intermediate");
        } else {
            queue.retain(|e| e.session() != session || !ROUTINE_KINDS.contains(&e.kind()));
        }
        dropped.superseded = before - queue.len();
        let mut at = 0;
        if !session.is_empty() {
            for (i, e) in queue.iter().enumerate() {
                if e.session() == session {
                    at = i + 1;
                }
            }
        }
        queue.insert(at, item);
        if queue.len() > max {
            dropped.cap = queue.len() - max;
            queue.truncate(max);
        }
    } else {
        queue.push(item);
        if queue.len() > max {
            dropped.cap = queue.len() - max;
            queue.drain(..dropped.cap);
        }
    }
    dropped
}

/// `_start_speech`'s held-while-dictating overflow: past the cap, drop the
/// oldest NON-priority line first (a held result must survive a chatty
/// dictation); if every held line is priority, the oldest goes.
pub fn cap_deferred<T>(deferred: &mut Vec<(T, bool, f64)>, max: usize) {
    while deferred.len() > max {
        match deferred.iter().position(|e| !e.1) {
            Some(i) => {
                deferred.remove(i);
            }
            None => {
                deferred.remove(0);
            }
        }
    }
}

/// `_flush_deferred_while_mic`'s split of the held batch: `(fresh, expired)`
/// in the order they were held. A line exactly `max_age` old is still fresh.
pub fn split_deferred<T>(
    deferred: Vec<(T, bool, f64)>,
    now: f64,
    max_age: f64,
) -> (Vec<T>, Vec<T>) {
    let mut fresh = Vec::new();
    let mut expired = Vec::new();
    for (item, _priority, ts) in deferred {
        if now - ts <= max_age {
            fresh.push(item);
        } else {
            expired.push(item);
        }
    }
    (fresh, expired)
}

/// Split at every whitespace run that follows one of `marks`, the way
/// `re.split(r"(?<=[marks])\s+", text)` does.
fn split_after(text: &str, marks: &[char]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev: Option<char> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if is_space(c) && prev.is_some_and(|p| marks.contains(&p)) {
            while chars.peek().copied().is_some_and(is_space) {
                chars.next();
            }
            out.push(std::mem::take(&mut cur));
            prev = None;
            continue;
        }
        cur.push(c);
        prev = Some(c);
    }
    out.push(cur);
    out
}

/// `_split(text)` — one chunk up to 800 characters; beyond that sentence
/// splits, and comma/semicolon/colon splits inside a sentence still too long.
#[must_use]
pub fn split(text: &str) -> Vec<String> {
    let text = heard_state::py::strip(text);
    if text.is_empty() {
        return Vec::new();
    }
    if text.chars().count() <= 800 {
        return vec![text.to_owned()];
    }
    let mut out = Vec::new();
    for p in split_after(text, &['.', '!', '?']) {
        if p.chars().count() <= 800 {
            out.push(p);
        } else {
            out.extend(split_after(&p, &[',', ';', ':']));
        }
    }
    out.retain(|s| !heard_state::py::strip(s).is_empty());
    out
}

/// The `afplay -r` rate for a requested speed: backends render up to their
/// `MAX_NATIVE_SPEED` themselves, and `afplay` makes up the rest, capped at
/// its own 2.0 upper bound.
#[must_use]
pub fn play_rate(speed: f64, max_native: f64) -> f64 {
    if speed > max_native && max_native > 0.0 {
        (speed / max_native).min(2.0)
    } else {
        1.0
    }
}

/// `_drain_queue`'s `history.append` rule: a line is written to
/// `history.jsonl` whenever it was NOT cancelled and it carries history meta
/// (`if not cancel.is_set(): if hmeta: history.append(...)`).
///
/// It does NOT depend on whether the audio played. A line whose synthesis or
/// playback failed, or that `_speak` skipped (muted / audio-off / mic active /
/// no voice configured), still returns with its cancel event clear, so it is
/// still recorded — only a silence, a mute's cancel or a barge-in (which set
/// the event) keeps it out. The corpus's `history` section pins this against
/// the live `Daemon._drain_queue`.
#[must_use]
pub fn appends_history(cancelled: bool, has_meta: bool) -> bool {
    !cancelled && has_meta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_layers_afplay_only_above_native() {
        assert_eq!(play_rate(1.0, 1.2), 1.0);
        assert_eq!(play_rate(1.2, 1.2), 1.0);
        assert!((play_rate(1.8, 1.2) - 1.5).abs() < 1e-12);
        assert_eq!(play_rate(4.0, 1.2), 2.0);
        assert_eq!(play_rate(3.0, 4.0), 1.0);
        assert_eq!(play_rate(3.0, 0.0), 1.0);
    }

    #[test]
    fn deferred_cap_prefers_dropping_routine() {
        let mut d = vec![
            ("a", true, 0.0),
            ("b", false, 0.0),
            ("c", true, 0.0),
            ("d", false, 0.0),
        ];
        cap_deferred(&mut d, 2);
        assert_eq!(d.iter().map(|e| e.0).collect::<Vec<_>>(), vec!["a", "c"]);
        cap_deferred(&mut d, 1);
        assert_eq!(d.iter().map(|e| e.0).collect::<Vec<_>>(), vec!["c"]);
    }
}
