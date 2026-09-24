//! Talking to the daemon: heard-proto frames over heard-cli's own socket.
//!
//! `heard_proto::transport` is hard-wired to the Heard app's socket path, so
//! the two exchanges are re-done here against an explicit path. They are the
//! same shapes (docs: `heard_proto::transport`):
//!
//! * fire-and-forget: connect → write JSON → close. No newline.
//! * request: connect → write JSON → `shutdown(Write)` → read to EOF.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use heard_proto::{Message, Mute, MuteSession, Request, Speak, Unmute, UnmuteSession};
use serde_json::Value;

use crate::ui::{CliError, CliResult};

/// Socket timeout for both exchanges.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// Where the "daemon down" fixes point.
pub const START_FIX: &str = "start it with `heard start` (or open the console: `heard`)";

/// A handle on the daemon socket. Cheap; connects per call.
#[derive(Debug, Clone)]
pub struct Daemon {
    socket: PathBuf,
}

impl Daemon {
    /// Bind to a socket path.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Daemon {
            socket: socket.into(),
        }
    }

    /// The socket this handle talks to.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    fn connect(&self) -> std::io::Result<UnixStream> {
        let s = UnixStream::connect(&self.socket)?;
        s.set_read_timeout(Some(TIMEOUT))?;
        s.set_write_timeout(Some(TIMEOUT))?;
        Ok(s)
    }

    /// Something is accepting on the socket.
    pub fn is_up(&self) -> bool {
        UnixStream::connect(&self.socket).is_ok()
    }

    /// Fire-and-forget one frame.
    pub fn send(&self, msg: &Message<'_>) -> std::io::Result<()> {
        let body = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
        let mut s = self.connect()?;
        s.write_all(&body)?;
        s.flush()
    }

    /// Request/reply. A reply that is empty or not JSON is an error.
    pub fn request(&self, msg: &Message<'_>) -> std::io::Result<Value> {
        let body = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
        let mut s = self.connect()?;
        s.write_all(&body)?;
        s.shutdown(Shutdown::Write)?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf)?;
        serde_json::from_slice(&buf).map_err(std::io::Error::other)
    }

    /// `{"cmd":"status"}`, or `None` when the daemon is down or mute.
    pub fn status(&self) -> Option<Value> {
        self.request(&Request::Status.into()).ok()
    }

    /// `{"cmd":"reload"}`, best effort. `true` when delivered.
    pub fn reload(&self) -> bool {
        self.send(&Request::Reload.into()).is_ok()
    }

    /// `{"cmd":"mute","source":"cli"}` — pause narration.
    pub fn mute(&self) -> std::io::Result<()> {
        self.send(&Request::Mute(Mute { source: "cli" }).into())
    }

    /// `{"cmd":"unmute","source":"cli"}` — resume narration.
    pub fn unmute(&self) -> std::io::Result<()> {
        self.send(&Request::Unmute(Unmute { source: "cli" }).into())
    }

    /// `{"text":…}` — speak one line.
    pub fn speak(&self, text: &str) -> CliResult<()> {
        self.send(
            &Speak {
                text,
                priority: false,
            }
            .into(),
        )
        .map_err(|e| down_error("speak", &e))
    }

    /// `{"cmd":"mute_session"|"unmute_session","session_id":…}`.
    pub fn session_mute(&self, session_id: &str, mute: bool) -> CliResult<Value> {
        let msg: Message<'_> = if mute {
            Request::MuteSession(MuteSession { session_id }).into()
        } else {
            Request::UnmuteSession(UnmuteSession { session_id }).into()
        };
        self.request(&msg)
            .map_err(|e| down_error("change a session's mute", &e))
    }
}

/// The error for "needed the daemon, could not reach it".
pub fn down_error(what: &str, e: &std::io::Error) -> CliError {
    CliError::failure(
        format!("cannot {what}: the Heard daemon is not running ({e})"),
        START_FIX,
    )
}

/// `active_sessions` from a status reply, as `(session_id, repo_name, ago_s)`.
pub fn active_sessions(status: &Value) -> Vec<Agent> {
    status
        .get("active_sessions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|s| Agent {
                    session_id: str_field(s, "session_id"),
                    repo_name: str_field(s, "repo_name"),
                    last_event_ago_s: s.get("last_event_ago_s").and_then(Value::as_f64),
                    pinned: s.get("pinned").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn str_field(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// One agent session the daemon is following.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Agent {
    /// The session id.
    pub session_id: String,
    /// The repo (cwd basename), or "".
    pub repo_name: String,
    /// Seconds since its last event.
    pub last_event_ago_s: Option<f64>,
    /// The router is pinned to it.
    pub pinned: bool,
}

impl Agent {
    /// `repo ▸ 1a2b3c4d` or just the short id.
    pub fn label(&self) -> String {
        let short: String = self.session_id.chars().take(8).collect();
        if self.repo_name.is_empty() {
            short
        } else {
            format!("{} ▸ {short}", self.repo_name)
        }
    }
}
