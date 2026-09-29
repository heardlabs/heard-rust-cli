//! The `event` command — the narration channel, and the busiest message on
//! the socket.
//!
//! Two shapes travel under `{"cmd": "event"}`:
//!
//! * [`NarrationEvent`] — what `client.send_event()` builds today:
//!   `kind` / `neutral` / `tag` / `ctx` / `session`.
//! * [`HealthProbe`] — `hook.py --health-probe`: `kind="health_probe"` plus
//!   `agent` and a 32-char `nonce`. Answered, not fire-and-forget.
//!
//! The raw agent hook payload is **not** one of them: it is its own
//! top-level command, [`crate::Hook`] under `{"cmd": "hook"}`.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

/// `kind` values used by [`NarrationEvent`], as `client.py` emits them.
pub mod kind {
    /// A tool is about to run (`PreToolUse`).
    pub const TOOL_PRE: &str = "tool_pre";
    /// A tool finished (`PostToolUse`).
    pub const TOOL_POST: &str = "tool_post";
    /// Assistant prose that is not the last block of a turn.
    pub const INTERMEDIATE: &str = "intermediate";
    /// The last assistant block of a turn.
    pub const FINAL: &str = "final";
    /// The user just submitted a prompt (`UserPromptSubmit`).
    pub const PROMPT_INTENT: &str = "prompt_intent";
}

/// Which agent CLI a hook message came from.
///
/// The closed set `hook.py`'s `AGENTS` table accepts, and the set
/// `daemon._handle` validates a health probe against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Agent {
    /// Claude Code (`~/.claude/settings.json` hooks).
    #[serde(rename = "claude-code")]
    ClaudeCode,
    /// Codex (`~/.codex/hooks.json` hooks).
    #[serde(rename = "codex")]
    Codex,
}

impl Agent {
    /// Parse the `argv[1]` an agent CLI spawns the hook with.
    pub fn from_argv(name: &str) -> Option<Self> {
        match name {
            "claude-code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// The wire name (`"claude-code"` / `"codex"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }
}

/// The literal `"health_probe"` in a [`HealthProbe`]'s `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthProbeKind {
    /// `"health_probe"`.
    #[serde(rename = "health_probe")]
    HealthProbe,
}

/// `{"cmd":"event","kind":"health_probe","agent":…,"nonce":…}`
///
/// Sent as a *request* (the daemon echoes `{"nonce":…,"agent":…}` back) by
/// `connection_health`'s nonce-bound probe. The daemon rejects any nonce that
/// is not exactly 32 characters, and any agent outside [`Agent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthProbe<'a> {
    /// Always [`HealthProbeKind::HealthProbe`].
    pub kind: HealthProbeKind,
    /// Which agent's hook is probing.
    pub agent: Agent,
    /// A 32-character nonce the daemon must echo.
    #[serde(borrow)]
    pub nonce: Cow<'a, str>,
}

impl<'a> HealthProbe<'a> {
    /// A probe for `agent` carrying `nonce`.
    pub fn new(agent: Agent, nonce: &'a str) -> Self {
        Self {
            kind: HealthProbeKind::HealthProbe,
            agent,
            nonce: Cow::Borrowed(nonce),
        }
    }
}

/// `{"cmd":"event","kind":…,"neutral":…,"tag":…,"ctx":{…},"session":{…}}`
///
/// The shape `client.send_event()` produces. All five keys are always
/// present; `ctx` and `session` are `{}` when the caller passed nothing
/// (`ctx or {}` / `session or {}` in `send_event`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NarrationEvent<'a> {
    /// One of the [`kind`] constants.
    #[serde(borrow)]
    pub kind: Cow<'a, str>,
    /// The persona-free text — what happened, plainly.
    #[serde(borrow)]
    pub neutral: Cow<'a, str>,
    /// Routing / verbosity tag (`final_long`, `intermediate_short`, …).
    #[serde(borrow)]
    pub tag: Cow<'a, str>,
    /// Open-ended context map (`length`, `recent_intent`, template ctx…).
    #[serde(default)]
    pub ctx: serde_json::Map<String, serde_json::Value>,
    /// Which session this came from.
    #[serde(borrow, default)]
    pub session: Session<'a>,
}

/// The two `event` shapes, discriminated by their `kind`.
///
/// Untagged, and ordered narrowest-first: [`HealthProbe`] pins `kind` to a
/// single literal, so only [`NarrationEvent`] is open.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Event<'a> {
    /// A nonce-bound reachability probe.
    #[serde(borrow)]
    HealthProbe(HealthProbe<'a>),
    /// A fully-formed narration event (Python `client.send_event`).
    #[serde(borrow)]
    Narration(NarrationEvent<'a>),
}

/// `_session_from_data`'s output — or `{}` when `send_event` got no session.
///
/// Modelled as an enum because the empty case is genuinely `{}` on the wire,
/// whereas a real session always carries `id`, `cwd` and `transcript_path`
/// keys (the latter two as `null` when the hook payload omits them).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Session<'a> {
    /// A session the hook could identify.
    #[serde(borrow)]
    Known(SessionInfo<'a>),
    /// `{}` — `send_event` was called without a session.
    Empty(EmptySession),
}

impl Default for Session<'_> {
    fn default() -> Self {
        Self::Empty(EmptySession {})
    }
}

/// The `{}` arm of [`Session`]. Rejects unknown keys so a malformed session
/// is an error rather than silently becoming "no session".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptySession {}

/// A session identified by the hook from its own payload and environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo<'a> {
    /// `data["session_id"]`, or `"default"` when the payload has none.
    #[serde(borrow)]
    pub id: Cow<'a, str>,
    /// The agent's working directory; `null` when the payload omits it.
    #[serde(borrow)]
    pub cwd: Option<Cow<'a, str>>,
    /// Path to the agent's JSONL transcript; `null` when absent.
    #[serde(borrow)]
    pub transcript_path: Option<Cow<'a, str>>,
    /// The terminal/editor that owns the hook process, when identifiable.
    /// Omitted entirely — not `null` — when `_terminal_binding_from_env`
    /// returns `None`.
    ///
    /// Boxed: a binding is twelve fields and most sessions carry none, so
    /// inlining it would make every event frame pay for the rare case.
    #[serde(borrow, default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<Box<Binding<'a>>>,
}

/// `_terminal_binding_from_env`'s record: which terminal owns this session.
///
/// The six `herdr_*` keys appear together, and only when the hook process
/// had `HERDR_ENV=1`; they are absent otherwise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Binding<'a> {
    /// `Ghostty` / `iTerm` / `Terminal` / `VS Code` / `Cursor` / `Windsurf` / `Herdr`.
    #[serde(borrow)]
    pub host_name: Cow<'a, str>,
    /// `terminal` or `editor_terminal`.
    #[serde(borrow)]
    pub host_type: Cow<'a, str>,
    /// Always `"env_terminal_binding"` from this path.
    #[serde(borrow)]
    pub provenance: Cow<'a, str>,
    /// `0.9` from the env path.
    pub confidence: f64,
    /// The hook process's parent pid (the agent CLI).
    pub pid: i64,
    /// Always `null` today — macOS has no cheap dependency-free way to read
    /// a process start time. Kept on the wire so PID-reuse detection can
    /// start populating it without a protocol change.
    pub process_started_at: Option<f64>,
    /// `HERDR_PANE_ID`.
    #[serde(borrow, default, skip_serializing_if = "Option::is_none")]
    pub herdr_pane_id: Option<Cow<'a, str>>,
    /// `HERDR_TAB_ID`.
    #[serde(borrow, default, skip_serializing_if = "Option::is_none")]
    pub herdr_tab_id: Option<Cow<'a, str>>,
    /// `HERDR_WORKSPACE_ID`.
    #[serde(borrow, default, skip_serializing_if = "Option::is_none")]
    pub herdr_workspace_id: Option<Cow<'a, str>>,
    /// `HERDR_SESSION` — `""` for the default session.
    #[serde(borrow, default, skip_serializing_if = "Option::is_none")]
    pub herdr_session: Option<Cow<'a, str>>,
    /// `HERDR_SOCKET_PATH`.
    #[serde(borrow, default, skip_serializing_if = "Option::is_none")]
    pub herdr_socket_path: Option<Cow<'a, str>>,
    /// `HERDR_BIN_PATH`.
    #[serde(borrow, default, skip_serializing_if = "Option::is_none")]
    pub herdr_bin_path: Option<Cow<'a, str>>,
}
