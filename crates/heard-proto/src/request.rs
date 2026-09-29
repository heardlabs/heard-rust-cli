//! Everything the daemon accepts on its socket, keyed by `cmd`.
//!
//! Mirrors the dispatch table in `daemon.py`'s `Daemon._handle` for the
//! commands `client.py` and the menu-bar UI actually send:
//! `ping`, `status`, `pin`, `unpin`, `reload`, `stop`, `mute`, `unmute`,
//! `resume_intent`, `feedback`, `report_defect`, `mute_session`,
//! `unmute_session`, `event` — plus the implicit `speak` that a payload with
//! no `cmd` falls through to, and `hook`, which is new in the Rust port.
//!
//! Any other `cmd` is not in this closed set. [`crate::parse_frame`] hands
//! such a frame back as an [`crate::ExtensionCommand`] with its raw bytes, so
//! a daemon extension can read it with exactly the wire bytes the client
//! sent.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::event::{Agent, Binding, Event};

/// A command frame: an object carrying `cmd`.
///
/// Internally tagged on `cmd`, exactly as the daemon reads it
/// (`cmd = req.get("cmd", "speak")` then a chain of `if cmd == …`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request<'a> {
    /// `{"cmd":"ping"}` — liveness. Fire-and-forget; the daemon returns
    /// nothing, and `is_daemon_alive()` treats a successful connect+send as
    /// the answer.
    Ping,
    /// `{"cmd":"status"}` — request; replies with [`crate::StatusResponse`].
    Status,
    /// `{"cmd":"pin","session_id":…}` — pin the multi-agent router to one
    /// session. Fire-and-forget; a blank id is ignored by the daemon.
    Pin(#[serde(borrow)] Pin<'a>),
    /// `{"cmd":"unpin"}` — clear the pin. Fire-and-forget.
    Unpin,
    /// `{"cmd":"reload"}` — re-read `config.yaml`. Fire-and-forget.
    Reload,
    /// `{"cmd":"stop"}` — cancel the current utterance. Fire-and-forget.
    Stop,
    /// `{"cmd":"mute","source":…}` — pause narration. Fire-and-forget.
    Mute(#[serde(borrow)] Mute<'a>),
    /// `{"cmd":"unmute","source":…}` — resume narration. Fire-and-forget.
    Unmute(#[serde(borrow)] Unmute<'a>),
    /// `{"cmd":"resume_intent","text":…}` — the resume panel's answer.
    /// Fire-and-forget.
    ResumeIntent(#[serde(borrow)] ResumeIntent<'a>),
    /// `{"cmd":"feedback","text":…,"source":…}` — preference feedback,
    /// attached to the daemon's last utterance. Fire-and-forget.
    Feedback(#[serde(borrow)] Feedback<'a>),
    /// `{"cmd":"report_defect","category":…,"note":…,"source":…}` — the
    /// defect sidecar. Fire-and-forget.
    ReportDefect(#[serde(borrow)] ReportDefect<'a>),
    /// `{"cmd":"mute_session","session_id":…}` — request; replies with
    /// [`crate::SessionResponse`].
    MuteSession(#[serde(borrow)] MuteSession<'a>),
    /// `{"cmd":"unmute_session","session_id":…}` — request; replies with
    /// [`crate::SessionResponse`].
    UnmuteSession(#[serde(borrow)] UnmuteSession<'a>),
    /// `{"cmd":"event",…}` — the narration channel. Fire-and-forget, except
    /// for [`crate::HealthProbe`], which is answered.
    Event(#[serde(borrow)] Event<'a>),
    /// `{"cmd":"hook","agent":…,"payload":{…}[,"binding":{…}]}` — a raw
    /// agent hook payload. Fire-and-forget.
    Hook(#[serde(borrow)] Hook<'a>),
}

/// The literal `"hook"` in a [`HookFrame`]'s `cmd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookCmd {
    /// `"hook"`.
    #[serde(rename = "hook")]
    Hook,
}

/// `{"cmd":"hook","agent":…,"payload":{…}[,"binding":{…}]}` — the **whole
/// frame** the Rust hook writes, and the only type it needs.
///
/// Serialise-only, and deliberately flat rather than built through
/// [`Request`]: `payload` is a [`RawValue`], the agent's stdin held by
/// reference, and serde's tagged enums buffer through an intermediate
/// representation a `RawValue` cannot pass through. Flat means the hook does
/// one parse, one serialise, and never copies the payload's bytes.
///
/// [`Hook`] is the same message as the daemon reads it back;
/// `hook_frame_matches_the_hook_command` in this crate's tests pins the two
/// together.
#[derive(Debug, Clone, Serialize)]
pub struct HookFrame<'a> {
    /// Always [`HookCmd::Hook`].
    pub cmd: HookCmd,
    /// The agent CLI that spawned the hook (`argv[1]`).
    pub agent: Agent,
    /// The agent's hook payload, verbatim.
    pub payload: &'a RawValue,
    /// The terminal that owns this hook process, from its own environment.
    /// Top-level and not inside `payload`, which stays exactly the bytes the
    /// agent wrote. Omitted entirely when the environment names no host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<Box<Binding<'a>>>,
}

impl<'a> HookFrame<'a> {
    /// One hook invocation, ready to send.
    ///
    /// `binding` comes from [`crate::HookEnv`] — derived in the hook,
    /// because the daemon's own environment identifies no terminal.
    pub fn new(agent: Agent, payload: &'a RawValue, binding: Option<Binding<'a>>) -> Self {
        Self {
            cmd: HookCmd::Hook,
            agent,
            payload,
            binding: binding.map(Box::new),
        }
    }
}

/// `{"cmd":"hook",…}` as the **daemon** reads it.
///
/// One agent hook invocation, forwarded untouched. The Rust hook is a dumb
/// pipe, so everything `client.handle_cc_*` does today — the flush sleep,
/// the transcript read, the dedup — happens on this side of the socket, and
/// `hook_event_name` is read out of `payload` there. It is deliberately not
/// lifted to the top level: one copy cannot drift from the other.
///
/// `payload` is an owned [`serde_json::Value`] here, not a [`RawValue`]:
/// this type is reached through a tagged enum, and serde buffers those
/// through an intermediate form a `RawValue` cannot survive. The hook's
/// write path pays no such cost — see [`HookFrame`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hook<'a> {
    /// The agent CLI that spawned the hook (`argv[1]`).
    pub agent: Agent,
    /// The agent's hook payload, exactly as it arrived on the hook's stdin.
    pub payload: serde_json::Value,
    /// The terminal that owned the hook process. Absent when its
    /// environment named no known host — the daemon must not substitute its
    /// own, which would identify the menu-bar app and nothing useful.
    #[serde(borrow, default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<Box<Binding<'a>>>,
}

/// `{"cmd":"pin","session_id":"…"}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin<'a> {
    /// The session to pin. The daemon strips it and ignores a blank.
    #[serde(borrow)]
    pub session_id: Cow<'a, str>,
}

/// `{"cmd":"mute","source":"…"}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mute<'a> {
    /// Who asked: `"client"` (the `client.mute()` default), `"cli"`,
    /// `"menu"`, … The daemon defaults a missing value to `"socket"`.
    #[serde(borrow)]
    pub source: Cow<'a, str>,
}

/// `{"cmd":"unmute","source":"…"}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unmute<'a> {
    /// Who asked. See [`Mute::source`].
    #[serde(borrow)]
    pub source: Cow<'a, str>,
}

/// `{"cmd":"resume_intent","text":"…"}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeIntent<'a> {
    /// The user's answer to "catch you up, or start fresh?". `client.py`
    /// sends `""` rather than omitting it.
    #[serde(borrow)]
    pub text: Cow<'a, str>,
}

/// `{"cmd":"feedback","text":"…","source":"…"}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feedback<'a> {
    /// The feedback itself. Blank text is recorded as nothing.
    #[serde(borrow)]
    pub text: Cow<'a, str>,
    /// `"cli"` by default.
    #[serde(borrow)]
    pub source: Cow<'a, str>,
}

/// `{"cmd":"report_defect","category":"…","note":"…","source":"…"}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportDefect<'a> {
    /// A `defects` category; anything unrecognised is logged as `other`.
    #[serde(borrow)]
    pub category: Cow<'a, str>,
    /// Free-text note. `client.py` sends `""` rather than omitting it.
    #[serde(borrow)]
    pub note: Cow<'a, str>,
    /// `"cli"` by default.
    #[serde(borrow)]
    pub source: Cow<'a, str>,
}

/// `{"cmd":"mute_session","session_id":"…"}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MuteSession<'a> {
    /// The session to silence.
    #[serde(borrow)]
    pub session_id: Cow<'a, str>,
}

/// `{"cmd":"unmute_session","session_id":"…"}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnmuteSession<'a> {
    /// The session to un-silence.
    #[serde(borrow)]
    pub session_id: Cow<'a, str>,
}

/// `{"text":"…"}` — the `cmd`-less payload `client.speak()` sends.
///
/// The daemon's `cmd` default is `"speak"`, and its fall-through at the end
/// of `_handle` is `_start_speech(req.get("text") or "", priority=…)`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Speak<'a> {
    /// The literal text to speak (persona is bypassed).
    #[serde(borrow, default)]
    pub text: Cow<'a, str>,
    /// Jump the queue. `client.speak()` never sends this key; the daemon
    /// reads it as `bool(req.get("priority"))`, so absent means `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub priority: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Any frame the daemon will accept.
///
/// Untagged and ordered command-first, which reproduces the daemon's own
/// reading: a payload with a `cmd` it recognises is that command; anything
/// else — including an unknown `cmd` — falls through to `speak`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message<'a> {
    /// A recognised `cmd`.
    #[serde(borrow)]
    Command(Request<'a>),
    /// The `speak` fall-through.
    #[serde(borrow)]
    Speak(Speak<'a>),
}

impl<'a> From<Request<'a>> for Message<'a> {
    fn from(r: Request<'a>) -> Self {
        Self::Command(r)
    }
}

impl<'a> From<Speak<'a>> for Message<'a> {
    fn from(s: Speak<'a>) -> Self {
        Self::Speak(s)
    }
}
