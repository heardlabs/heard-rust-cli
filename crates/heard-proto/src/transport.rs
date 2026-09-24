//! The two exchanges the daemon socket supports.
//!
//! Faithful to `client.py`:
//!
//! * [`send`] is `client.send()` — `connect → sendall(json) → close`, **no
//!   trailing newline**, no reply read. The daemon's reader loops until EOF,
//!   so the close is the only frame delimiter there is. Writing a newline
//!   would just become part of the JSON body.
//! * [`request`] is `client.request()` — `connect → sendall → shutdown(SHUT_WR)
//!   → read to EOF`. The daemon deliberately waits for the half-close before
//!   it answers; without the shutdown both sides block.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::ProtoError;

/// `client.send()`'s socket timeout.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// `client.request()`'s default socket timeout.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Overrides [`socket_path`]. Rust-only, and there purely so tests can point
/// a hook at a socket they own — the Python engine has no such knob, and
/// nothing in the product sets it.
pub const SOCKET_PATH_ENV: &str = "HEARD_DAEMON_SOCKET";

/// `config.SOCKET_PATH`: `~/Library/Application Support/heard/daemon.sock`.
///
/// The same as [`socket_path_for`]`("heard")`, the app name the full Heard
/// app uses.
pub fn socket_path() -> Result<PathBuf, ProtoError> {
    socket_path_for("heard")
}

/// `~/Library/Application Support/<app>/daemon.sock`, or
/// `HEARD_DAEMON_SOCKET` when set.
///
/// Python derives this from `platformdirs.user_data_dir(app)`. Heard is
/// macOS-only, so the macOS layout is the real answer; the non-macOS arm
/// follows the same crate's XDG rule so the workspace still builds and tests
/// on Linux CI. The CLI edition passes `"heard-cli"`, so it never shares a
/// socket with the app.
pub fn socket_path_for(app: &str) -> Result<PathBuf, ProtoError> {
    if let Some(p) = std::env::var_os(SOCKET_PATH_ENV) {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").ok_or(ProtoError::NoHome)?;
    let mut p = PathBuf::from(home);
    if cfg!(target_os = "macos") {
        p.push("Library/Application Support");
        p.push(app);
    } else {
        match std::env::var_os("XDG_DATA_HOME") {
            Some(x) if !x.is_empty() => p = PathBuf::from(x),
            _ => p.push(".local/share"),
        }
        p.push(app);
    }
    p.push("daemon.sock");
    Ok(p)
}

/// Connect to the daemon socket with both timeouts set.
pub fn connect(timeout: Duration) -> Result<UnixStream, ProtoError> {
    let stream = UnixStream::connect(socket_path()?)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout))?;
    Ok(stream)
}

/// Fire-and-forget: write these exact bytes and close. No newline is added.
pub fn send_bytes(body: &[u8]) -> Result<(), ProtoError> {
    let mut stream = connect(SEND_TIMEOUT)?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

/// Fire-and-forget: serialise `payload` and send it.
pub fn send<T: Serialize>(payload: &T) -> Result<(), ProtoError> {
    send_bytes(&serde_json::to_vec(payload)?)
}

/// Send `payload`, half-close, read the reply to EOF and parse it.
///
/// An empty reply is a deserialisation error here; `client.py` turns that
/// into `{}` at the call site, and callers should treat it the same way —
/// as "daemon unreachable", not as a fault to report.
pub fn request<T: Serialize, R: DeserializeOwned>(
    payload: &T,
    timeout: Duration,
) -> Result<R, ProtoError> {
    let mut stream = connect(timeout)?;
    stream.write_all(&serde_json::to_vec(payload)?)?;
    stream.flush()?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}
