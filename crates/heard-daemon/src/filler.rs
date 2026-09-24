//! Dead-air lines: the instant submit acknowledgement, the companion "still
//! thinking" nudge, and the one budget they share ([`FillerPolicy`],
//! `sequencer.FillerPolicy` in the Python).
//!
//! Both are fixed strings, spoken in the persona's voice but never restyled
//! by a brain: they must land before the first token, filling silence the
//! turn opener cannot (a long pure-thinking gap emits no intermediate).

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::policy;

/// `FILLER_MIN_SILENCE_S` — nothing at all spoken for this long.
pub const FILLER_MIN_SILENCE_S: f64 = 20.0;
/// `FILLER_MIN_SINCE_REAL_S` — and not this close behind a real line.
pub const FILLER_MIN_SINCE_REAL_S: f64 = 10.0;
/// `HUNG_TOOL_S` — a tool that started and produced no further event for
/// this long is treated as hung (Focus mode's one "still running" line).
pub const HUNG_TOOL_S: f64 = 120.0;

/// `_PROMPT_FILLER_LINES`.
pub const PROMPT_FILLER_LINES: &[&str] = &[
    "On it.",
    "On it now.",
    "Right, on it.",
    "Got it.",
    "Got it \u{2014} looking now.",
    "Okay, looking into that.",
    "Let me take a look.",
    "Let me dig in.",
    "Let me get into it.",
    "Sure, one moment.",
    "Looking now.",
    "Checking that now.",
    "Alright, digging in.",
    "Let me have a look at this.",
    "On it \u{2014} give me a sec.",
];

/// `_THINKING_NUDGE_LINES`.
pub const THINKING_NUDGE_LINES: &[&str] = &[
    "Still looking at this.",
    "Still on it.",
    "Still working through this.",
    "Still digging \u{2014} bear with me.",
    "Give me another moment on this.",
    "Still on it, almost there in thought.",
];

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

/// `_should_prompt_filler(cfg)` — companion only, under the prompt-intent
/// master switch, never while muted or paused.
#[must_use]
pub fn should_prompt_filler(cfg: &Map<String, Value>) -> bool {
    policy::cfg_truthy(cfg, "narrate_prompt_intent", true)
        && policy::narration_mode(cfg) == "companion"
        && !policy::cfg_truthy(cfg, "muted", false)
        && !policy::cfg_truthy(cfg, "paused", false)
}

/// `_should_thinking_nudge(cfg)`.
#[must_use]
pub fn should_thinking_nudge(cfg: &Map<String, Value>) -> bool {
    policy::cfg_truthy(cfg, "companion_thinking_nudge", true)
        && policy::narration_mode(cfg) == "companion"
        && !policy::cfg_truthy(cfg, "muted", false)
        && !policy::cfg_truthy(cfg, "paused", false)
}

/// `_thinking_nudge_after_s(cfg)` — `max(1, float(v or 30))`, 30 on a value
/// that is not a number.
#[must_use]
pub fn thinking_nudge_after_s(cfg: &Map<String, Value>) -> f64 {
    let v = cfg.get("companion_thinking_nudge_seconds");
    let f = match v {
        None | Some(Value::Null) => Some(30.0),
        Some(v) if !policy::py_truthy(v) => Some(30.0),
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::Bool(true)) => Some(1.0),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        Some(_) => None,
    };
    match f {
        Some(f) if f.is_finite() => f.max(1.0),
        Some(_) => 30.0,
        None => 30.0,
    }
}

/// `_pick_varied(pool, last)` — a line that is not the one just used.
/// `pick(n)` returns an index in `0..n` (random in production, scripted in
/// tests).
pub fn pick_varied(
    pool: &[&'static str],
    last: &mut Option<usize>,
    pick: &mut dyn FnMut(usize) -> usize,
) -> &'static str {
    let mut i = pick(pool.len()) % pool.len();
    if pool.len() > 1 {
        if let Some(prev) = *last {
            let mut guard = 0;
            while i == prev && guard < 64 {
                i = pick(pool.len()) % pool.len();
                guard += 1;
            }
            if i == prev {
                i = (prev + 1) % pool.len();
            }
        }
    }
    *last = Some(i);
    pool[i]
}

/// The hung-tool line: `f"Still going after two minutes — {clean}."`, with
/// `clean = (label or "A tool").rstrip(".")`.
#[must_use]
pub fn hung_tool_line(label: &str) -> String {
    let label = if label.is_empty() { "A tool" } else { label };
    format!(
        "Still going after two minutes \u{2014} {}.",
        label.trim_end_matches('.')
    )
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

    #[test]
    fn nudge_threshold_reads_like_python() {
        let cfg = |v: Value| {
            let mut m = Map::new();
            m.insert("companion_thinking_nudge_seconds".into(), v);
            m
        };
        assert_eq!(thinking_nudge_after_s(&Map::new()), 30.0);
        assert_eq!(thinking_nudge_after_s(&cfg(Value::from(0))), 30.0);
        assert_eq!(thinking_nudge_after_s(&cfg(Value::from(0.2))), 1.0);
        assert_eq!(thinking_nudge_after_s(&cfg(Value::from("7"))), 7.0);
        assert_eq!(thinking_nudge_after_s(&cfg(Value::from("x"))), 30.0);
    }

    #[test]
    fn varied_pick_never_repeats() {
        let mut last = None;
        let mut script = [0usize, 0, 0, 3].into_iter();
        let mut pick = |_n: usize| script.next().unwrap_or(1);
        assert_eq!(
            pick_varied(PROMPT_FILLER_LINES, &mut last, &mut pick),
            "On it."
        );
        assert_eq!(
            pick_varied(PROMPT_FILLER_LINES, &mut last, &mut pick),
            "Got it."
        );
        assert_eq!(
            hung_tool_line("Running the tests."),
            "Still going after two minutes \u{2014} Running the tests."
        );
        assert_eq!(
            hung_tool_line(""),
            "Still going after two minutes \u{2014} A tool."
        );
    }
}
