//! What the daemon writes back on a request exchange.
//!
//! Only `status`, `mute_session` / `unmute_session` and the health probe
//! reply at all among the core commands; every other command in this crate is
//! fire-and-forget and the daemon returns `None`.
//!
//! `client.request()` swallows every failure and returns `{}`, so a caller
//! must treat "no reply" and "unparseable reply" as *daemon unreachable*,
//! never as an error to surface.
//!
//! These are owned types: a reply is read into a fresh buffer that the
//! caller usually wants to outlive, and the status snapshot is mostly
//! numbers and open-ended sub-objects owned by subsystems other crates in
//! this workspace will model.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The reply to `{"cmd":"status"}` — the menu bar's whole picture.
///
/// The `Value` fields are snapshots owned by daemon subsystems (the router,
/// the agent scoreboard, and fields only an extended daemon fills) that are
/// modelled in their own crates; they are passed through unmodelled here so
/// this crate stays the wire contract and nothing more. The core daemon
/// reports the fields it does not own as empty/`null`, and every field stays
/// on the wire so a client of either edition reads the same shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResponse {
    /// Always `true` — reaching this reply is the liveness proof.
    pub alive: bool,
    /// The TTS backend's name (`ElevenLabsTTS`, `KokoroTTS`, `NullTTS`, or a
    /// registered backend's).
    pub backend: String,
    /// The active persona's name.
    pub persona: String,
    /// `narrate_tools` from the merged config.
    pub narrate_tools: bool,
    /// `muted` from the merged config.
    pub muted: bool,
    /// The last error the daemon recorded, or `null`.
    pub last_error: Option<String>,
    /// An account usage snapshot. Always `null` in the core daemon.
    pub account_usage: Option<Value>,
    /// Something is playing right now.
    pub speaking: bool,
    /// How many utterances are queued behind it.
    pub queued: i64,
    /// `router.list_active()` — recently-active sessions for the menu.
    pub active_sessions: Value,
    /// `router.mode().value` — `solo` / `swarm` / `pinned`.
    pub router_mode: String,
    /// Layer 2 per-agent scoreboard.
    pub agent_states: Value,
    /// Run summaries from an extension. Always empty in the core daemon.
    pub langgraph_runs: Value,
    /// Rolling "what's going on right now" prose. Empty in the core daemon.
    pub recap: String,
    /// Wider (20-minute) window of agents, for the Mission Control cards.
    pub mission_agents: Value,
    /// How much narration the router is holding back.
    pub pending_count: i64,
    /// The daemon is waiting for a resume-panel answer.
    pub awaiting_resume_intent: bool,
    /// An update the daemon has staged, or `null`.
    pub pending_update: Option<PendingUpdate>,
}

/// The `pending_update` sub-object of [`StatusResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingUpdate {
    /// Version string of the staged release.
    pub version: String,
    /// The GitHub release tag.
    pub tag: String,
    /// The release page URL.
    pub url: String,
    /// Direct URL of the downloadable zip; `null` for a release that ships
    /// none (the client falls back to the release page).
    pub zip_url: Option<String>,
    /// Size of that zip in bytes; `null` when unknown.
    pub zip_size: Option<i64>,
}

/// The reply to `mute_session` / `unmute_session`.
///
/// `{"ok":true,"session_id":"…"}`, or `{"ok":false,"error":"missing_session_id"}`
/// — note the failure carries no `session_id` key at all.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionResponse {
    /// Whether the session was (un)muted.
    pub ok: bool,
    /// Echo of the session id. Absent on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Why not. Absent on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The reply to a [`crate::HealthProbe`]: `{"nonce":"…","agent":"…"}`.
///
/// A probe the daemon rejects (nonce not 32 chars, or an unknown agent)
/// comes back as the literal `{}`, which fails to deserialise here — and
/// that failure *is* the "probe rejected" answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthProbeResponse {
    /// The nonce, echoed verbatim.
    pub nonce: String,
    /// The agent, echoed verbatim.
    pub agent: String,
}

/// The bare `{"ok":…}` / `{"ok":false,"error":"…"}` shape used by the
/// commands outside this crate's core set (`first_run_hold`, `inject`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OkResponse {
    /// Whether it worked.
    pub ok: bool,
    /// Why not. Absent on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
