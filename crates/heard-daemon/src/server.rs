//! The Unix-socket server: one accept loop, which reads and dispatches each
//! frame IN ACCEPT ORDER (see [`Server::serve`]), and `Daemon._handle`'s
//! dispatch table.
//!
//! # The wire, unchanged
//!
//! `heard-proto` is the contract, and this is the other end of it:
//!
//! * **fire-and-forget** — the client writes JSON and closes. The read loops
//!   to EOF, so the close IS the frame. No reply is written.
//! * **request** — the client writes JSON and half-closes. The read still
//!   loops to EOF; the half-close is what ends it. One JSON object goes back
//!   and the connection closes.
//!
//! There is no framing, no newline and no length prefix in either direction,
//! and adding one would break every existing Python client.
//!
//! # The socket this binds
//!
//! Whatever [`heard_proto::transport::socket_path`] resolves, which honours
//! `HEARD_DAEMON_SOCKET` first. Every test and every development run of this
//! binary sets that variable to a path under a temp directory. The Python
//! daemon owns the real `~/Library/Application Support/heard/daemon.sock` and
//! this binary must never take it: two daemons on one socket is not a
//! differential run, it is a coin flip over which one gets each hook.

use std::path::Path;
use std::sync::Arc;

use heard_proto::{Event, Session};
use heard_proto::{ExtensionCommand, Frame, Message, Request};
use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

use crate::daemon::{Daemon, NarrationEvent};
use crate::hooks::HookQueue;

/// A bound, listening daemon. Dropping it unlinks the socket.
pub struct Server {
    daemon: Arc<Daemon>,
    hooks: Arc<HookQueue>,
    listener: UnixListener,
    path: std::path::PathBuf,
}

impl Server {
    /// Bind `path`, replacing a stale socket left by a crashed daemon.
    ///
    /// A LIVE daemon's socket is not stale, and this cannot tell the
    /// difference — the caller is responsible for the pid file / single
    /// instance check that `daemon.py` does before it gets here. In this lane
    /// the caller is a test or a developer with an explicit
    /// `HEARD_DAEMON_SOCKET`.
    pub async fn bind(daemon: Arc<Daemon>, path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // `UnixListener::bind` fails with EADDRINUSE on an existing path even
        // when nothing is listening, which is the normal state after a crash.
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        let hooks = Arc::new(HookQueue::new(Arc::clone(&daemon)));
        crate::dlog!("daemon_listening", sock = path.display().to_string());
        Ok(Self {
            daemon,
            hooks,
            listener,
            path: path.to_path_buf(),
        })
    }

    /// The daemon this server serves.
    pub fn daemon(&self) -> &Arc<Daemon> {
        &self.daemon
    }

    /// The hook lane, so a test can wait for it to drain.
    pub fn hooks(&self) -> &Arc<HookQueue> {
        &self.hooks
    }

    /// How long one connection's frame may take to arrive before the loop
    /// gives up on it. Every real client writes its whole message and then
    /// closes or half-closes immediately, so this only bounds a client that
    /// connected and went away.
    const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    /// Accept until `stop` arrives.
    ///
    /// **The read and the dispatch happen IN the loop, in accept order.** That
    /// is the whole reason the loop is shaped this way: a `hook` frame's only
    /// synchronous work is putting it on its session's lane, and if two
    /// connections raced to do that, a PostToolUse could take the lane ahead
    /// of the PreToolUse that preceded it on the wire — the per-session FIFO
    /// below would then faithfully preserve the WRONG order. Ordering has to
    /// be established where the frames are still sequenced, which is here.
    ///
    /// What is NOT done in the loop is writing the reply back, which depends
    /// on a client that may not be reading, and the hook processing itself
    /// (the flush wait, the transcript read), which is the lane's job.
    ///
    /// The cost is that a connection that stalls mid-frame holds the loop for
    /// up to [`Server::READ_TIMEOUT`]. That is bounded, logged, and cheaper
    /// than the alternative: narration that is occasionally out of order is
    /// exactly the bug this ordering exists to prevent.
    pub async fn serve(self) {
        let shutdown = self.daemon.shutdown_signal();
        loop {
            // Checked FIRST, not only via the notify: `stop` usually arrives
            // as a frame this very loop just dispatched, and by then
            // `notify_waiters` has already fired with nobody waiting (it
            // stores no permit). The flag is what makes that case terminate.
            if self.daemon.is_stopping() {
                break;
            }
            let accepted = tokio::select! {
                biased;
                _ = shutdown.notified() => break,
                accepted = self.listener.accept() => accepted,
            };
            if self.daemon.is_stopping() {
                break;
            }
            let mut stream = match accepted {
                Ok((stream, _)) => stream,
                Err(e) => {
                    // A failed accept is not a reason to stop listening; the
                    // Python loop logs and continues too.
                    crate::dlog!("accept_failed", err = e.kind().to_string());
                    continue;
                }
            };
            let mut buf = Vec::new();
            match tokio::time::timeout(Self::READ_TIMEOUT, stream.read_to_end(&mut buf)).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    crate::dlog!("request_read_failed", err = e.kind().to_string());
                    continue;
                }
                Err(_) => {
                    crate::dlog!("request_read_timeout", bytes = buf.len());
                    continue;
                }
            }
            if is_subscribe(&buf) {
                spawn_subscriber(&self.daemon, stream);
                continue;
            }
            // Dispatch stays in the loop (accept order is the event order), but
            // it can block: a narration event runs the brain's model call
            // synchronously for seconds. Tell the runtime, so the tasks queued
            // on this worker (a Parrot reply reader, a wake read) move to
            // another one instead of waiting the call out.
            let reply = run_blocking(|| dispatch(&self.daemon, &self.hooks, &buf));
            if let Some(bytes) = reply {
                tokio::spawn(async move {
                    if let Err(e) = stream.write_all(&bytes).await {
                        crate::dlog!("reply_write_failed", err = e.kind().to_string());
                        return;
                    }
                    let _ = stream.shutdown().await;
                });
            }
        }
        self.hooks.stop();
        let _ = std::fs::remove_file(&self.path);
        crate::dlog!("daemon_stopped");
    }
}

/// Run `f`, which may block, from the serve loop. On a multi-thread runtime
/// it is [`tokio::task::block_in_place`]: this worker's queued tasks are handed
/// to another worker for the duration. Anywhere else (a current-thread runtime,
/// no runtime) it simply runs, as before.
fn run_blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// `Daemon._handle(raw)` — the `cmd` dispatch.
///
/// Returns the reply bytes for the commands that answer, `None` for the
/// fire-and-forget ones. A payload that is not JSON at all is logged and
/// ignored, exactly as Python's bare `except: return None` does — a malformed
/// frame must never take the daemon down.
///
/// A frame whose `cmd` the core does not know goes to the daemon's
/// [`crate::Extension`]s in order ([`dispatch_extension`]).
pub fn dispatch(daemon: &Arc<Daemon>, hooks: &Arc<HookQueue>, raw: &[u8]) -> Option<Vec<u8>> {
    // `cancel` and `voice_release` are core commands with no typed
    // `Request` variant, so they are peeled off before the typed parse. Only
    // a frame that contains one of the two names pays for the extra parse.
    if let Some(reply) = dispatch_untyped(daemon, raw) {
        return reply;
    }
    let message: Message = match heard_proto::parse_frame(raw) {
        Ok(Frame::Core(m)) => m,
        Ok(Frame::Extension(cmd)) => return dispatch_extension(daemon, &cmd),
        Err(e) => {
            crate::dlog!(
                "request_rejected",
                reason = "malformed",
                err = e.to_string(),
                bytes = raw.len()
            );
            return None;
        }
    };

    let request = match message {
        Message::Speak(speak) => {
            // Rejected before any work while first-run holds the install,
            // with a reply (the Python answers `speak` here, and only here).
            if daemon.first_run_state().held {
                crate::dlog!("speech_skipped", reason = "first_run_hold", via = "direct");
                return reply(&serde_json::json!({"ok": false, "error": "first_run_hold"}));
            }
            daemon.speak_direct(&speak.text);
            return None;
        }
        Message::Command(request) => request,
    };

    match request {
        // Liveness. A successful connect+send IS the answer; the Python
        // returns nothing and `is_daemon_alive()` reads the absence.
        Request::Ping => None,

        Request::Status => serde_json::to_vec(&daemon.status()).ok(),

        Request::Pin(pin) => {
            let sid = pin.session_id.trim();
            if !sid.is_empty() {
                let ok = daemon.router.pin(sid);
                crate::dlog!("router_pin", session = sid, ok = ok);
            }
            None
        }
        Request::Unpin => {
            daemon.router.unpin();
            crate::dlog!("router_unpin");
            None
        }
        Request::Reload => {
            daemon.reload();
            None
        }
        Request::Stop => {
            daemon.stop();
            None
        }
        Request::Mute(mute) => {
            daemon.mute(blank_to_socket(&mute.source));
            None
        }
        Request::Unmute(unmute) => {
            daemon.unmute(blank_to_socket(&unmute.source));
            None
        }

        Request::MuteSession(cmd) => {
            let sid = cmd.session_id.trim();
            if sid.is_empty() {
                return reply(&heard_proto::SessionResponse {
                    ok: false,
                    session_id: None,
                    error: Some("missing_session_id".into()),
                });
            }
            daemon.mute_session(sid);
            reply(&heard_proto::SessionResponse {
                ok: true,
                session_id: Some(sid.to_owned()),
                error: None,
            })
        }
        Request::UnmuteSession(cmd) => {
            // Python does NOT reject a blank here — `discard("")` is a no-op
            // and it answers ok with the empty id. Kept, divergence-free.
            let sid = cmd.session_id.trim();
            daemon.unmute_session(sid);
            reply(&heard_proto::SessionResponse {
                ok: true,
                session_id: Some(sid.to_owned()),
                error: None,
            })
        }

        Request::Event(Event::HealthProbe(probe)) => {
            // The daemon rejects any nonce that is not exactly 32 characters;
            // the agent set is closed by the type.
            if probe.nonce.chars().count() != 32 {
                crate::dlog!("health_probe_rejected", reason = "bad_nonce");
                return Some(b"{}".to_vec());
            }
            reply(&heard_proto::HealthProbeResponse {
                nonce: probe.nonce.into_owned(),
                agent: probe.agent.as_str().to_owned(),
            })
        }
        Request::Event(Event::Narration(event)) => {
            daemon.handle_event(&narration_from_wire(&event));
            None
        }

        Request::Hook(hook) => {
            if !hook.payload.is_object() {
                crate::dlog!("hook_rejected", reason = "malformed");
                return None;
            }
            let binding = hook
                .binding
                .as_ref()
                .and_then(|b| serde_json::to_value(b).ok());
            hooks.submit(hook.agent.as_str(), hook.payload, binding);
            None
        }

        // Normally answered by `dispatch_untyped` from the raw fields (the
        // Python reads each with `req.get(…) or default`); a typed parse only
        // lands here if that path could not read the frame.
        Request::ResumeIntent(cmd) => {
            daemon.resume_intent_from_socket(&cmd.text);
            None
        }
        Request::Feedback(cmd) => {
            daemon.feedback(&cmd.text, &cmd.source);
            None
        }
        Request::ReportDefect(cmd) => {
            crate::dlog!(
                "report_defect_unhandled",
                category = cmd.category.as_ref(),
                source = cmd.source.as_ref()
            );
            None
        }
    }
}

/// The core commands with no typed [`Request`] variant (or whose typed
/// parse is stricter than Python's `req.get(…) or default`). Each is quoted,
/// so a frame is only parsed twice when it mentions one.
const UNTYPED: &[&[u8]] = &[
    b"\"cancel\"",
    b"\"voice_release\"",
    b"\"voice_hold\"",
    b"\"tour_hold\"",
    b"\"tour_release\"",
    b"\"inject\"",
    b"\"feedback\"",
    b"\"report_defect\"",
    b"\"resume_intent\"",
    b"\"subscribe\"",
];

fn mentions(raw: &[u8], needle: &[u8]) -> bool {
    raw.windows(needle.len()).any(|w| w == needle)
}

/// Is this frame `{"cmd":"subscribe"}`?
pub fn is_subscribe(raw: &[u8]) -> bool {
    mentions(raw, b"\"subscribe\"")
        && serde_json::from_slice::<Value>(raw)
            .ok()
            .and_then(|v| {
                v.get("cmd")
                    .and_then(Value::as_str)
                    .map(|c| c == "subscribe")
            })
            .unwrap_or(false)
}

/// `req.get(key) or ""`, stripped.
fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

/// The untyped core commands. `None` = none of them; `Some(reply)` =
/// handled, with `reply` the bytes to send back, if any.
fn dispatch_untyped(daemon: &Arc<Daemon>, raw: &[u8]) -> Option<Option<Vec<u8>>> {
    if !UNTYPED.iter().any(|n| mentions(raw, n)) {
        return None;
    }
    let value: Value = serde_json::from_slice(raw).ok()?;
    let cmd = value.get("cmd").and_then(Value::as_str)?;
    match cmd {
        // The Mute/Pause cut: stop what is playing now, change no state.
        "cancel" => {
            daemon.cancel();
            Some(None)
        }
        // Push-to-talk: the user started / stopped speaking.
        "voice_hold" => {
            daemon.voice_hold();
            Some(None)
        }
        "voice_release" => {
            daemon.voice_release();
            Some(None)
        }
        // The guided tour.
        "tour_hold" => {
            daemon.tour_hold();
            Some(None)
        }
        "tour_release" => {
            daemon.tour_release();
            Some(None)
        }
        // Reached only when a caller dispatches a subscribe frame without the
        // accept loop (which streams it); there is nothing to answer.
        "subscribe" => Some(None),
        // Typing into the frontmost app is the app's own job now (native, in
        // Swift, with its own Accessibility grant); the daemon refuses
        // explicitly rather than falling through to `speak`.
        "inject" => {
            crate::dlog!("inject_refused", reason = "not_supported");
            Some(reply(
                &serde_json::json!({"ok": false, "error": "not_supported"}),
            ))
        }
        "feedback" => {
            daemon.feedback(field(&value, "text"), field(&value, "source"));
            Some(None)
        }
        "resume_intent" => {
            daemon.resume_intent_from_socket(field(&value, "text"));
            Some(None)
        }
        // The defect store belongs to an edition: offered to the extensions
        // first; unclaimed, recorded in the log only.
        "report_defect" => {
            for ext in daemon.extensions() {
                if let Some(r) = ext.handle_command(daemon, cmd, raw) {
                    return Some(r);
                }
            }
            crate::dlog!(
                "report_defect_unhandled",
                category = field(&value, "category"),
                source = field(&value, "source")
            );
            Some(None)
        }
        _ => None,
    }
}

/// Keep a `subscribe` connection open: the `hello` line first, then every
/// event line as it is emitted, on a thread of its own (a slow reader never
/// holds the accept loop). The thread ends when the client goes away.
fn spawn_subscriber(daemon: &Arc<Daemon>, stream: tokio::net::UnixStream) {
    let std_stream = match stream.into_std() {
        Ok(s) => s,
        Err(e) => {
            crate::dlog!("event_subscriber_failed", err = e.kind().to_string());
            return;
        }
    };
    let _ = std_stream.set_nonblocking(false);
    // Registered BEFORE the hello is written, so nothing emitted in between
    // is lost.
    let rx = daemon.subscribe_events();
    let hello = daemon.hello_line();
    crate::dlog!(
        "event_subscriber_added",
        count = daemon.events().subscriber_count()
    );
    let spawned = std::thread::Builder::new()
        .name("event-subscriber".into())
        .spawn(move || {
            use std::io::Write as _;
            let mut out = std_stream;
            if out.write_all(hello.as_bytes()).is_err() {
                return;
            }
            for line in rx {
                if out.write_all(line.as_bytes()).is_err() {
                    crate::dlog!("event_subscribers_pruned", dropped = 1usize);
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        crate::dlog!("event_subscriber_failed", err = e.to_string());
    }
}

/// A frame whose `cmd` the core does not know: offered to each extension in
/// order; the first to claim it answers. Unclaimed, it is what Python's
/// `_handle` does with an unknown `cmd` — it runs off the end of the
/// `if cmd == …` chain into `_start_speech(req.get("text") or "")`.
pub fn dispatch_extension(daemon: &Arc<Daemon>, cmd: &ExtensionCommand<'_>) -> Option<Vec<u8>> {
    for ext in daemon.extensions() {
        if let Some(reply) = ext.handle_command(daemon, &cmd.cmd, cmd.raw) {
            crate::dlog!(
                "cmd_extension",
                cmd = cmd.cmd.as_ref(),
                extension = ext.name(),
                replied = reply.is_some()
            );
            return reply;
        }
    }
    crate::dlog!("cmd_unhandled", cmd = cmd.cmd.as_ref());
    daemon.speak_direct(&cmd.fallback.text);
    None
}

fn reply<T: serde::Serialize>(value: &T) -> Option<Vec<u8>> {
    serde_json::to_vec(value).ok()
}

/// `req.get("source") or "socket"`.
fn blank_to_socket(source: &str) -> &str {
    if source.is_empty() {
        "socket"
    } else {
        source
    }
}

/// The wire `event` shape → the routing's own event.
fn narration_from_wire(event: &heard_proto::NarrationEvent<'_>) -> NarrationEvent {
    let (session_id, cwd) = match &event.session {
        Session::Known(info) => (
            if info.id.is_empty() {
                "default"
            } else {
                info.id.as_ref()
            }
            .to_owned(),
            info.cwd.as_deref().unwrap_or("").to_owned(),
        ),
        Session::Empty(_) => ("default".to_owned(), String::new()),
    };
    NarrationEvent {
        kind: event.kind.clone().into_owned(),
        neutral: event.neutral.clone().into_owned(),
        tag: event.tag.clone().into_owned(),
        ctx: event.ctx.clone(),
        session_id,
        cwd,
    }
}

/// An empty `ctx`, for callers building an event by hand.
pub fn empty_ctx() -> Map<String, Value> {
    Map::new()
}

#[cfg(test)]
mod blocking_dispatch_tests {
    use std::time::{Duration, Instant};

    /// A task woken while the serve loop blocks in dispatch (Parrot's reply
    /// reader) must run on another worker, not wait the blocking call out:
    /// a brain call on the loop starved the wake listener's 2 s read.
    #[test]
    fn a_blocking_dispatch_does_not_strand_tasks_on_its_worker() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let waited = rt.block_on(async {
            tokio::spawn(async {
                let started = Instant::now();
                let (tx, rx) = tokio::sync::oneshot::channel();
                // Woken on THIS worker, so it would sit in its LIFO slot.
                tokio::spawn(async move {
                    let _ = tx.send(started.elapsed());
                });
                super::run_blocking(|| std::thread::sleep(Duration::from_millis(1500)));
                rx.await.unwrap()
            })
            .await
            .unwrap()
        });
        assert!(waited < Duration::from_millis(500), "{waited:?}");
    }

    #[test]
    fn run_blocking_runs_inline_without_a_multi_thread_runtime() {
        assert_eq!(super::run_blocking(|| 7), 7);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert_eq!(rt.block_on(async { super::run_blocking(|| 8) }), 8);
    }
}
