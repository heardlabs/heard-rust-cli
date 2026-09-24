//! The event payload the daemon feeds the state layer.
//!
//! `agent_state.observe`'s docstring is the wire format, and it is the only
//! written-down definition there is:
//!
//! ```text
//! Event payload shape (from CC/Codex hooks, see daemon.py):
//!   - kind: "tool_pre" | "tool_post" | "prompt_intent" |
//!           "intermediate" | "final" | other
//!   - tag: more specific (e.g. "tool_bash", "tool_post_failure")
//!   - neutral: the plain text (used for output-size approximation)
//!   - ctx: dict with optional `abs_path` (file the tool touched)
//!   - session: { id, cwd }
//! ```
//!
//! Every field is optional on the wire — the Python reads each one with
//! `event.get(...) or <default>`, and `tests/test_agent_state.py` feeds it
//! `{}` on purpose. So [`AgentEvent`] defaults everything rather than failing
//! to deserialise: a malformed hook payload must never be the reason the daemon
//! stops narrating.
//!
//! [`EventKind`] is the closed part. It is spelled as an enum rather than a
//! string because the five named kinds are the whole of `observe`'s dispatch
//! and the router's, and `Other` keeps the "or other" case from the docstring
//! honest instead of silently matching nothing.

use serde::de::{Deserializer, Error as _};
use serde::{Deserialize, Serialize, Serializer};

/// Which branch of `observe` an event takes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EventKind {
    /// A tool is about to run. Sets `current_tool`.
    ToolPre,
    /// A tool finished. Clears `current_tool`, counts failures, records the
    /// touched file.
    ToolPost,
    /// The user typed something at the agent.
    PromptIntent,
    /// Assistant prose mid-turn. Feeds the output-size window.
    Intermediate,
    /// The agent's closing message. Also feeds the output-size window.
    Final,
    /// Anything else — counted, but it only bumps the event counter.
    #[default]
    Other,
    /// An unrecognised kind that still carries its name, so a log line or a
    /// future rung can see what actually arrived.
    Named(String),
}

impl EventKind {
    pub fn as_str(&self) -> &str {
        match self {
            EventKind::ToolPre => "tool_pre",
            EventKind::ToolPost => "tool_post",
            EventKind::PromptIntent => "prompt_intent",
            EventKind::Intermediate => "intermediate",
            EventKind::Final => "final",
            EventKind::Other => "",
            EventKind::Named(name) => name,
        }
    }
}

impl From<&str> for EventKind {
    fn from(value: &str) -> Self {
        match value {
            "tool_pre" => EventKind::ToolPre,
            "tool_post" => EventKind::ToolPost,
            "prompt_intent" => EventKind::PromptIntent,
            "intermediate" => EventKind::Intermediate,
            "final" => EventKind::Final,
            "" => EventKind::Other,
            other => EventKind::Named(other.to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for EventKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // `kind` is `event.get("kind") or ""` on the Python side, so a missing
        // key, an explicit null and an empty string are all the same thing.
        let raw = Option::<String>::deserialize(deserializer).map_err(D::Error::custom)?;
        Ok(EventKind::from(raw.unwrap_or_default().as_str()))
    }
}

impl Serialize for EventKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// `session: { id, cwd }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionRef {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// `ctx`, of which the state layer reads exactly one key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventCtx {
    /// Absolute path of the file a tool touched. `Edit` / `Write` /
    /// `NotebookEdit` set it; everything else leaves it out.
    #[serde(default)]
    pub abs_path: Option<String>,
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

/// One event as `_handle_event` receives it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentEvent {
    #[serde(default)]
    pub session: Option<SessionRef>,
    #[serde(default)]
    pub kind: EventKind,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub neutral: Option<String>,
    #[serde(default)]
    pub ctx: Option<EventCtx>,
}

impl AgentEvent {
    /// `sess.get("id") or "default"` — the fallback keeps a hook that forgot
    /// its session id from creating a new agent record on every event.
    pub fn session_id(&self) -> &str {
        self.session
            .as_ref()
            .and_then(|s| s.id.as_deref())
            .filter(|id| !id.is_empty())
            .unwrap_or("default")
    }

    pub fn cwd(&self) -> Option<&str> {
        self.session
            .as_ref()
            .and_then(|s| s.cwd.as_deref())
            .filter(|cwd| !cwd.is_empty())
    }

    /// `event.get("tag") or ""`.
    pub fn tag(&self) -> &str {
        self.tag.as_deref().unwrap_or("")
    }

    /// `event.get("neutral") or ""`.
    pub fn neutral(&self) -> &str {
        self.neutral.as_deref().unwrap_or("")
    }

    pub fn abs_path(&self) -> Option<&str> {
        self.ctx
            .as_ref()
            .and_then(|c| c.abs_path.as_deref())
            .filter(|p| !p.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_payload_still_parses() {
        // tests/test_agent_state.py::test_empty_event_dict_creates_default_agent
        let event: AgentEvent = serde_json::from_str("{}").expect("empty object parses");
        assert_eq!(event.session_id(), "default");
        assert_eq!(event.kind, EventKind::Other);
        assert_eq!(event.tag(), "");
        assert_eq!(event.neutral(), "");
        assert!(event.abs_path().is_none());
    }

    #[test]
    fn the_five_named_kinds_round_trip() {
        for name in [
            "tool_pre",
            "tool_post",
            "prompt_intent",
            "intermediate",
            "final",
        ] {
            let kind = EventKind::from(name);
            assert_eq!(kind.as_str(), name);
            assert!(!matches!(kind, EventKind::Named(_)));
        }
    }

    #[test]
    fn an_unknown_kind_keeps_its_name() {
        let event: AgentEvent =
            serde_json::from_str(r#"{"kind": "something_else"}"#).expect("parses");
        assert_eq!(event.kind, EventKind::Named("something_else".into()));
    }

    #[test]
    fn a_full_payload_reads_every_field() {
        let event: AgentEvent = serde_json::from_str(
            r#"{"session": {"id": "s1", "cwd": "/x/api"}, "kind": "tool_post",
                "tag": "tool_post_edit", "neutral": "done",
                "ctx": {"abs_path": "/x/y/auth.py", "extra": 1}}"#,
        )
        .expect("parses");
        assert_eq!(event.session_id(), "s1");
        assert_eq!(event.cwd(), Some("/x/api"));
        assert_eq!(event.kind, EventKind::ToolPost);
        assert_eq!(event.tag(), "tool_post_edit");
        assert_eq!(event.abs_path(), Some("/x/y/auth.py"));
        assert!(event.ctx.expect("ctx").rest.contains_key("extra"));
    }

    #[test]
    fn a_null_session_id_falls_back_to_default() {
        let event: AgentEvent =
            serde_json::from_str(r#"{"session": {"id": null, "cwd": null}}"#).expect("parses");
        assert_eq!(event.session_id(), "default");
        assert!(event.cwd().is_none());
    }
}
