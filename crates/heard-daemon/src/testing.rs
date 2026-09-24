//! Test scaffolding shared by this crate's unit tests and its integration
//! tests. Public because an integration test cannot reach a `#[cfg(test)]`
//! item; nothing in the daemon's own paths calls any of it.
//!
//! No `tempfile` dependency: the workspace has none cached, and a daemon that
//! pulls a crate in to make a directory is not paying its way.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A fresh, empty directory under the system temp dir, unique to this
/// process. Never removed on drop — a failed test's evidence is worth more
/// than a clean `/tmp`, and the OS reaps it.
pub fn temp_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("heard-daemon-{label}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("temp dir");
    path
}

/// A socket path inside `dir`.
///
/// **Never** the real daemon socket. `heard-proto` resolves
/// `HEARD_DAEMON_SOCKET` first, and every test and every local run of this
/// crate sets it to one of these; the Python daemon owns
/// `~/Library/Application Support/heard/daemon.sock` and binding it would
/// take the socket out from under a live install.
///
/// The path stays short on purpose: `sockaddr_un.sun_path` is 104 bytes on
/// macOS and a long temp path silently fails to bind.
pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("d.sock")
}
