//! What `history.jsonl` may keep of the user's own words.
//!
//! `history.jsonl` is mostly Heard's output: the lines it spoke. Two kinds of
//! record carry the USER's words instead:
//!
//! * a feedback line (`{"type": "feedback", …, "text": …}`) is the user's
//!   free text, verbatim;
//! * a spoken line of a kind that quotes the user — the core's
//!   `prompt_intent` is the user's prompt, restated. An edition that speaks
//!   more such kinds (an answer that echoes a question, a confirmation that
//!   names what the user picked) adds them with
//!   [`HistoryPolicy::with_user_text_kinds`].
//!
//! A [`HistoryPolicy`] with `record_user_text: false` keeps every record's
//! SHAPE (`id`, `ts`, `kind`, `tag`, `session_id`, …: feedback refs and the
//! usage clock still work) and blanks the words: the feedback `text` and the
//! quoting line's `spoken` become `""`, and the record gains
//! `"redacted": true` so a reader can tell "withheld" from "said nothing".
//!
//! The policy is read through a [`HistoryPolicySource`] on EVERY append,
//! never cached: a user who turns recording off mid-session is honoured on
//! the next line, not after a restart.
//!
//! ## Defaults
//!
//! [`HistoryPolicy::default`] records everything — the behaviour before this
//! policy existed. The core reads one config key, [`CONFIG_KEY`]
//! (`history_user_text`), through [`from_config`]:
//!
//! | value | user text |
//! |---|---|
//! | absent | recorded (the default) |
//! | `true` | recorded |
//! | `false` (YAML `false`, `off`, `no`) | withheld |
//! | anything else (a string, a number, null) | withheld — a value we cannot read is not permission |

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::pyjson::PyValue;

/// The core config key: `history_user_text: false` withholds the user's
/// words from `history.jsonl`. Absent = recorded.
pub const CONFIG_KEY: &str = "history_user_text";

/// Spoken kinds the core itself emits whose line restates the user's words.
/// `prompt_intent` narrates the prompt the user just submitted.
pub const CORE_USER_TEXT_KINDS: &[&str] = &["prompt_intent"];

/// The marker a redacted record carries.
pub const REDACTED_KEY: &str = "redacted";

/// What `history.jsonl` may keep of the user's words. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPolicy {
    /// `false`: blank the user's words (feedback text, quoting lines).
    pub record_user_text: bool,
    /// Spoken kinds whose line may quote the user, matched against a
    /// record's `kind` OR its `tag`.
    pub user_text_kinds: Vec<String>,
}

impl Default for HistoryPolicy {
    fn default() -> Self {
        Self {
            record_user_text: true,
            user_text_kinds: CORE_USER_TEXT_KINDS.iter().map(|k| (*k).into()).collect(),
        }
    }
}

/// Where a history writer reads its policy from, per append.
pub type HistoryPolicySource = Arc<dyn Fn() -> HistoryPolicy + Send + Sync>;

impl HistoryPolicy {
    /// The default policy with user text withheld.
    #[must_use]
    pub fn withholding() -> Self {
        Self {
            record_user_text: false,
            ..Self::default()
        }
    }

    /// Also treat `kinds` as quoting the user.
    #[must_use]
    pub fn with_user_text_kinds(mut self, kinds: &[&str]) -> Self {
        for k in kinds {
            if !self.user_text_kinds.iter().any(|have| have == k) {
                self.user_text_kinds.push((*k).into());
            }
        }
        self
    }

    /// A source that always answers `self`.
    #[must_use]
    pub fn fixed(self) -> HistoryPolicySource {
        Arc::new(move || self.clone())
    }

    /// Whether `record` carries the user's words: a feedback line, or a line
    /// whose `kind` or `tag` is one of [`HistoryPolicy::user_text_kinds`].
    #[must_use]
    pub fn carries_user_text(&self, record: &PyValue) -> bool {
        let field = |k: &str| record.get(k).and_then(PyValue::as_str).unwrap_or("");
        if field("type") == "feedback" {
            return true;
        }
        let (kind, tag) = (field("kind"), field("tag"));
        self.user_text_kinds
            .iter()
            .any(|k| (!kind.is_empty() && k == kind) || (!tag.is_empty() && k == tag))
    }

    /// Apply the policy to one record in place. Returns whether it was
    /// redacted.
    pub fn apply(&self, record: &mut PyValue) -> bool {
        if self.record_user_text || !self.carries_user_text(record) {
            return false;
        }
        let field = if record.get("type").and_then(PyValue::as_str) == Some("feedback") {
            "text"
        } else {
            "spoken"
        };
        if record.get(field).is_some() {
            record.set(field, PyValue::Str(String::new()));
        }
        record.set(REDACTED_KEY, PyValue::Bool(true));
        true
    }
}

/// The core's reading of [`CONFIG_KEY`] in a freshly-loaded config (see the
/// module docs for the table).
#[must_use]
pub fn record_user_text_from_config(cfg: &Map<String, Value>) -> bool {
    match cfg.get(CONFIG_KEY) {
        None => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => false,
    }
}

/// [`HistoryPolicy::default`] with `record_user_text` from [`CONFIG_KEY`].
#[must_use]
pub fn from_config(cfg: &Map<String, Value>) -> HistoryPolicy {
    HistoryPolicy {
        record_user_text: record_user_text_from_config(cfg),
        ..HistoryPolicy::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(pairs: &[(&str, &str)]) -> PyValue {
        PyValue::Dict(
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), PyValue::Str((*v).into())))
                .collect(),
        )
    }

    #[test]
    fn the_default_records_everything() {
        let p = HistoryPolicy::default();
        let mut r = rec(&[("kind", "prompt_intent"), ("spoken", "fix the resolver")]);
        assert!(!p.apply(&mut r));
        assert_eq!(
            r.get("spoken").and_then(PyValue::as_str),
            Some("fix the resolver")
        );
    }

    #[test]
    fn withholding_blanks_feedback_text_and_quoting_lines_only() {
        let p = HistoryPolicy::withholding().with_user_text_kinds(&["echo"]);
        let mut fb = rec(&[("type", "feedback"), ("ref", "abc"), ("text", "too chatty")]);
        assert!(p.apply(&mut fb));
        assert_eq!(fb.get("text").and_then(PyValue::as_str), Some(""));
        assert_eq!(fb.get("ref").and_then(PyValue::as_str), Some("abc"));
        assert_eq!(fb.get(REDACTED_KEY), Some(&PyValue::Bool(true)));

        let mut quoting = rec(&[("kind", "x"), ("tag", "echo"), ("spoken", "you said hi")]);
        assert!(p.apply(&mut quoting));
        assert_eq!(quoting.get("spoken").and_then(PyValue::as_str), Some(""));

        let mut plain = rec(&[
            ("kind", "final"),
            ("tag", "final"),
            ("spoken", "All green."),
        ]);
        assert!(!p.apply(&mut plain));
        assert_eq!(
            plain.get("spoken").and_then(PyValue::as_str),
            Some("All green.")
        );
        assert!(plain.get(REDACTED_KEY).is_none());
    }

    #[test]
    fn the_config_key_reads_absent_as_on_and_garbage_as_off() {
        let cfg = |v: Value| {
            let mut m = Map::new();
            m.insert(CONFIG_KEY.into(), v);
            m
        };
        assert!(record_user_text_from_config(&Map::new()));
        assert!(record_user_text_from_config(&cfg(json!(true))));
        assert!(!record_user_text_from_config(&cfg(json!(false))));
        assert!(!record_user_text_from_config(&cfg(json!("yes"))));
        assert!(!record_user_text_from_config(&cfg(Value::Null)));
        assert!(!from_config(&cfg(json!(false))).record_user_text);
    }
}
