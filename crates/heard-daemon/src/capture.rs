//! The [`Capture`] seam — `heard/ledger/capture.py`'s two daemon call sites.
//!
//! `daemon.py` writes the D0.1d ledger from exactly two places in the
//! narration path, and this trait is those two places:
//!
//! 1. **`_handle_event`, before any gate** — `_ledger.record_event(kind)` for
//!    the five agent-event kinds, UNCONDITIONALLY, so the ledger is ground
//!    truth for "what the daemon received" regardless of what narration later
//!    decided. → [`Capture::event_received`].
//! 2. **`_log("event_speak" | "event_drop", …)`** — a `candidate` row with
//!    `would_speak`, `score` 1.0/0.0, `policy_version="daemon-narration-v1"`
//!    and the drop reason. → [`Capture::decision`].
//!
//! The default, [`NoCapture`], writes nothing. A real implementation lives
//! outside this crate (an edition that keeps a ledger owns its writer thread)
//! so this crate stays free of SQLite.

/// `daemon._AGENT_EVENT_KINDS` — the kinds the ledger records.
pub const AGENT_EVENT_KINDS: [&str; 5] = [
    "tool_pre",
    "tool_post",
    "final",
    "intermediate",
    "prompt_intent",
];

/// `daemon._NARRATION_POLICY_VERSION`.
pub const NARRATION_POLICY_VERSION: &str = "daemon-narration-v1";

/// Is `kind` one the ledger records?
pub fn is_agent_event_kind(kind: &str) -> bool {
    AGENT_EVENT_KINDS.contains(&kind)
}

/// Where the ledger rows go. Both methods must be non-blocking and must never
/// panic — `capture.py`'s contract is "never raises past this point".
pub trait Capture: Send + Sync {
    /// An agent event arrived (only called for [`AGENT_EVENT_KINDS`]).
    fn event_received(&self, kind: &str) {
        let _ = kind;
    }

    /// The narration decision for one agent event. `reason` is `None` when it
    /// spoke unremarkably, and the drop reason otherwise.
    fn decision(&self, kind: &str, would_speak: bool, reason: Option<&str>) {
        let _ = (kind, would_speak, reason);
    }
}

/// No ledger.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCapture;

impl Capture for NoCapture {}
