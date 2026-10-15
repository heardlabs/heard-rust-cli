//! The dead-air budget ([`FillerPolicy`], `sequencer.FillerPolicy` in the
//! Python): at most one canned filler line per agent turn, only into real
//! silence.
//!
//! The daemon's own fillers (Companion's instant "On it." on submit and its
//! "still thinking" nudge) and Focus's hung-tool line went with the modes
//! (retired 2026-10-14); the budget stays for an edition that speaks fillers
//! of its own.

use std::collections::{HashMap, HashSet};

/// `FILLER_MIN_SILENCE_S` — nothing at all spoken for this long.
pub const FILLER_MIN_SILENCE_S: f64 = 20.0;
/// `FILLER_MIN_SINCE_REAL_S` — and not this close behind a real line.
pub const FILLER_MIN_SINCE_REAL_S: f64 = 10.0;
/// At most one filler per agent turn, only into real silence.
///
/// * one filler per (session, turn);
/// * nothing at all spoken for `min_silence_s`;
/// * no real line within `min_since_real_s`;
/// * never two fillers in a row, across sessions.
///
/// Times are monotonic seconds, passed in so this stays pure.
#[derive(Debug, Clone)]
pub struct FillerPolicy {
    /// `min_silence_s`.
    pub min_silence_s: f64,
    /// `min_since_real_s`.
    pub min_since_real_s: f64,
    turn_no: HashMap<String, u64>,
    filled: HashSet<(String, u64)>,
    last_spoken_at: Option<f64>,
    last_real_at: Option<f64>,
    last_was_filler: bool,
}

impl Default for FillerPolicy {
    fn default() -> Self {
        Self::new(FILLER_MIN_SILENCE_S, FILLER_MIN_SINCE_REAL_S)
    }
}

impl FillerPolicy {
    /// With explicit thresholds.
    #[must_use]
    pub fn new(min_silence_s: f64, min_since_real_s: f64) -> Self {
        Self {
            min_silence_s,
            min_since_real_s,
            turn_no: HashMap::new(),
            filled: HashSet::new(),
            last_spoken_at: None,
            last_real_at: None,
            last_was_filler: false,
        }
    }

    /// A prompt was submitted: a fresh filler budget for this session.
    pub fn open_turn(&mut self, session_id: &str) -> u64 {
        let turn = self.turn_no.get(session_id).copied().unwrap_or(0) + 1;
        self.turn_no.insert(session_id.to_owned(), turn);
        self.filled.remove(&(session_id.to_owned(), turn - 1));
        turn
    }

    /// `(may we speak a filler, reason)`; the reason is the log reason on a
    /// refusal and `"ok"` on an allow.
    #[must_use]
    pub fn allow(&self, session_id: &str, now: f64) -> (bool, &'static str) {
        let Some(turn) = self.turn_no.get(session_id).copied() else {
            return (false, "filler_no_open_turn");
        };
        if self.filled.contains(&(session_id.to_owned(), turn)) {
            return (false, "filler_already_this_turn");
        }
        if self.last_was_filler {
            return (false, "filler_back_to_back");
        }
        if self
            .last_spoken_at
            .is_some_and(|t| now - t < self.min_silence_s)
        {
            return (false, "filler_silence_too_short");
        }
        if self
            .last_real_at
            .is_some_and(|t| now - t < self.min_since_real_s)
        {
            return (false, "filler_near_real_line");
        }
        (true, "ok")
    }

    /// Record a line that reached the speech queue.
    pub fn note_line(&mut self, session_id: &str, now: f64, is_filler: bool) {
        self.last_spoken_at = Some(now);
        self.last_was_filler = is_filler;
        if is_filler {
            if let Some(turn) = self.turn_no.get(session_id).copied() {
                self.filled.insert((session_id.to_owned(), turn));
            }
        } else {
            self.last_real_at = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_filler_per_turn_only_into_silence() {
        let mut p = FillerPolicy::default();
        assert_eq!(p.allow("s", 100.0), (false, "filler_no_open_turn"));
        p.open_turn("s");
        assert_eq!(p.allow("s", 100.0), (true, "ok"));
        p.note_line("s", 100.0, true);
        assert_eq!(p.allow("s", 200.0), (false, "filler_already_this_turn"));
        p.open_turn("s");
        assert_eq!(p.allow("s", 200.0), (false, "filler_back_to_back"));
        p.note_line("s", 200.0, false);
        assert_eq!(p.allow("s", 205.0), (false, "filler_silence_too_short"));
        assert_eq!(p.allow("s", 221.0), (true, "ok"));
        let mut q = FillerPolicy::new(0.0, 10.0);
        q.open_turn("t");
        q.note_line("t", 50.0, false);
        assert_eq!(q.allow("t", 55.0), (false, "filler_near_real_line"));
    }
}
