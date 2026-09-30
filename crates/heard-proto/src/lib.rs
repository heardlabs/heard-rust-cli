//! heard-proto — the Heard daemon's Unix-socket wire protocol, as Rust types.
//!
//! This is the Rust half of the contract implemented today by
//! `engine/heard/client.py` (the sender) and `engine/heard/daemon.py`'s
//! `Daemon._handle` (the receiver), in the Python reference implementation.
//!
//! # The wire
//!
//! The daemon listens on a Unix **stream** socket at
//! `~/Library/Application Support/heard/daemon.sock`
//! (`config.SOCKET_PATH`, i.e. platformdirs' `user_data_dir("heard")`).
//!
//! There are exactly two exchange shapes:
//!
//! * **fire-and-forget** — `connect → sendall(json) → close`. **No trailing
//!   newline**, no framing, no reply. The daemon reads until EOF, so the
//!   close *is* the frame.
//! * **request** — `connect → sendall(json) → shutdown(SHUT_WR) → read to EOF`.
//!   The half-close is what tells the daemon the request is complete; it then
//!   writes one JSON object back and closes.
//!
//! [`transport`] implements both.
//!
//! # Fidelity to the Python
//!
//! Every payload here serialises to the same **keys**, with the same
//! **optionality** and the same **types** as the dict `json.dumps`'d by
//! `client.py` today. The literals in this crate's tests were captured by
//! running the real `heard.client` helpers with their socket module replaced
//! by a recorder (`python3 -B`), not written from memory.
//!
//! Two deliberate, documented differences, neither of which the daemon can
//! observe (it reads every field with `dict.get`):
//!
//! * **Whitespace.** Python's `json.dumps` defaults to `", "` / `": "`
//!   separators; `serde_json` emits compact JSON. Tests therefore compare
//!   parsed [`serde_json::Value`]s, not raw bytes.
//! * **Key order** is not guaranteed to match, though in practice the struct
//!   field order here mirrors the Python dict literal order.
//!
//! # Borrowing
//!
//! Every text field of a request is a `Cow<'a, str>`: borrowed from the
//! input buffer when the wire bytes are the string itself, owned when they
//! are not. Never a bare `&'a str` — a borrowed `&str` cannot represent a
//! JSON string with escapes (`\/`, which Swift's `JSONEncoder` writes for
//! every `/`, `\"`, `\n`, `\uXXXX`), and a frame carrying one would fail to
//! parse and fall through to a blank `speak`. The two exceptions to typed
//! fields are `ctx` and the daemon's `status` reply, which are genuinely
//! open-ended maps and are modelled as [`serde_json::Value`].

#![forbid(unsafe_code)]

mod env;
mod event;
mod frame;
mod request;
mod response;
pub mod transport;

pub use env::HookEnv;

pub use event::{
    kind, Agent, Binding, EmptySession, Event, HealthProbe, HealthProbeKind, NarrationEvent,
    Session, SessionInfo,
};
pub use frame::{is_core_cmd, parse_frame, ExtensionCommand, Frame, CORE_CMDS};
pub use request::{
    Feedback, Hook, HookCmd, HookFrame, Message, Mute, MuteSession, Pin, ReportDefect, Request,
    ResumeIntent, Speak, Unmute, UnmuteSession,
};
pub use response::{
    HealthProbeResponse, OkResponse, PendingUpdate, SessionResponse, StatusResponse,
};

/// Errors from encoding, decoding or delivering a daemon message.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    /// The payload was not valid JSON, or did not match the protocol.
    #[error("protocol json: {0}")]
    Json(#[from] serde_json::Error),
    /// The socket could not be reached, written to, or read from.
    #[error("daemon socket: {0}")]
    Io(#[from] std::io::Error),
    /// `$HOME` was unset, so the socket path could not be resolved.
    #[error("cannot resolve the daemon socket path (no home directory)")]
    NoHome,
}
