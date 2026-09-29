//! The daemon, over a real Unix socket, driven the way `heard-proto` drives it.
//!
//! Every test binds a socket under its own temp directory. **None of them
//! touches `~/Library/Application Support/heard/daemon.sock`** — that is the
//! live Python daemon's, and `heard-proto`'s `HEARD_DAEMON_SOCKET` override
//! exists precisely so this suite never has to.
//!
//! `transport::send` / `transport::request` resolve that variable from the
//! PROCESS environment, and a Rust test binary runs every test in one
//! process, so it cannot be set per test. The helpers here are therefore
//! `transport`'s two exchanges with the path passed in rather than resolved —
//! same bytes, same half-close, same read-to-EOF. `transport_drives_the_same_daemon`
//! pins the two together by running the shipped client through the env
//! override, so the protocol is proven against what ships and not only
//! against a copy of it.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use heard_daemon::testing;
use heard_daemon::{DaemonBuilder, LogSpeech, Server};
use heard_proto::{Agent, HealthProbe, Mute, MuteSession, Pin, Request, Speak};
use serde_json::Value;

/// How long a positive assertion waits for the pipeline to produce lines.
const WAIT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

struct Harness {
    socket: PathBuf,
    speech_log: PathBuf,
    config_dir: PathBuf,
    hooks: Arc<heard_daemon::HookQueue>,
    daemon: Arc<heard_daemon::Daemon>,
}

impl Harness {
    async fn start(label: &str) -> Self {
        let root = testing::temp_dir(label);
        let socket = testing::socket_path(&root);
        let speech_log = root.join("speech.jsonl");
        let paths = heard_config::Paths::under(&root);
        let config_dir = paths.config_dir.clone();
        // An onboarded install: a fresh one is held by first run.
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(config_dir.join("config.yaml"), "onboarded: true\n").expect("config");
        let daemon = DaemonBuilder::new(paths)
            .speech(Arc::new(
                LogSpeech::new(&speech_log).with_history(&config_dir),
            ))
            .build();
        // Bound before `serve` is spawned, so a connect cannot race the
        // listener — the kernel queues on a bound socket.
        let server = Server::bind(Arc::clone(&daemon), &socket)
            .await
            .expect("bind");
        let hooks = Arc::clone(server.hooks());
        tokio::spawn(server.serve());
        Self {
            socket,
            speech_log,
            config_dir,
            hooks,
            daemon,
        }
    }

    /// `client.send()` — write and close, no reply.
    fn send<T: serde::Serialize>(&self, payload: &T) {
        send_to(&self.socket, &serde_json::to_vec(payload).expect("encode"));
    }

    fn send_raw(&self, body: &[u8]) {
        send_to(&self.socket, body);
    }

    /// `client.request()` — write, half-close, read to EOF.
    fn request<T: serde::Serialize>(&self, payload: &T) -> Value {
        let body = serde_json::to_vec(payload).expect("encode");
        serde_json::from_slice(&request_to(&self.socket, &body)).unwrap_or(Value::Null)
    }

    /// Wait for the pipeline to go quiet.
    ///
    /// A fire-and-forget send returns as soon as the bytes are written, so
    /// "nothing more is coming" is not observable from the client side. This
    /// round-trips a request (which cannot be served before the connections
    /// opened ahead of it were accepted), drains the hook lane, and then
    /// gives the spawned connection tasks a bounded grace window. Used for
    /// the assertions that expect NOTHING; positive ones use
    /// [`Harness::wait_for_lines`], which is not timing-dependent.
    async fn settle(&self) {
        let _ = self.request(&Request::Status);
        assert!(self.hooks.drain(WAIT).await, "hook lane did not drain");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(self.hooks.drain(WAIT).await, "hook lane did not drain");
    }

    /// Wait until the sink holds at least `n` lines, then return them all.
    async fn wait_for_lines(&self, n: usize) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let lines = self.spoken();
            if lines.len() >= n {
                return lines;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "only {} of {n} utterances reached the sink: {:#?}",
                lines.len(),
                lines
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn spoken(&self) -> Vec<Value> {
        read_jsonl(&self.speech_log)
    }

    fn history(&self) -> Vec<Value> {
        read_jsonl(&self.config_dir.join("history.jsonl"))
    }
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("jsonl line is json"))
        .collect()
}

fn send_to(socket: &Path, body: &[u8]) {
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream.write_all(body).expect("write");
    stream.flush().expect("flush");
}

fn request_to(socket: &Path, body: &[u8]) -> Vec<u8> {
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    stream.write_all(body).expect("write");
    stream.flush().expect("flush");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("half-close");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read");
    buf
}

/// One Claude Code hook frame.
fn hook_frame(session_id: &str, mut payload: Value) -> Value {
    payload["session_id"] = Value::String(session_id.into());
    serde_json::json!({ "cmd": "hook", "agent": "claude-code", "payload": payload })
}

/// Write a `config.yaml` with no flush wait and the play-by-play profile, so
/// tool events actually speak and the lane does not sleep 800 ms per hook.
fn configure(harness: &Harness, extra: &str) {
    std::fs::create_dir_all(&harness.config_dir).expect("config dir");
    std::fs::write(
        harness.config_dir.join("config.yaml"),
        format!("onboarded: true\nflush_delay_ms: 0\nverbosity: verbose\nnarrate_routine: true\n{extra}"),
    )
    .expect("config");
    harness.daemon.reload();
}

// ---------------------------------------------------------------------------
// the commands
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_is_accepted_and_answers_nothing() {
    let harness = Harness::start("ping").await;
    harness.send(&Request::Ping);
    assert_eq!(harness.request(&Request::Status)["alive"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_round_trips_as_the_proto_type() {
    let harness = Harness::start("status").await;
    let raw = request_to(&harness.socket, br#"{"cmd":"status"}"#);
    let status: heard_proto::StatusResponse = serde_json::from_slice(&raw).expect("StatusResponse");
    assert!(status.alive);
    assert!(!status.muted);
    assert!(status.narrate_tools);
    assert_eq!(status.router_mode, "solo");
    assert_eq!(status.queued, 0);
    assert_eq!(status.pending_count, 0);
    assert!(status.last_error.is_none());
    assert!(status.pending_update.is_none());
    assert!(status.account_usage.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mute_and_unmute_persist_through_config() {
    let harness = Harness::start("mute").await;
    harness.send(&Request::Mute(Mute {
        source: "cli".into(),
    }));
    assert_eq!(harness.request(&Request::Status)["muted"], true);
    // The flag is on DISK, which is what the hook gate and the menu read.
    let body = std::fs::read_to_string(harness.config_dir.join("config.yaml")).expect("config");
    assert!(body.contains("muted: true"), "{body}");

    harness.send(&Request::Unmute(heard_proto::Unmute {
        source: "cli".into(),
    }));
    assert_eq!(harness.request(&Request::Status)["muted"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_picks_up_a_changed_config() {
    let harness = Harness::start("reload").await;
    assert_eq!(harness.request(&Request::Status)["narrate_tools"], true);
    std::fs::create_dir_all(&harness.config_dir).expect("config dir");
    std::fs::write(
        harness.config_dir.join("config.yaml"),
        "onboarded: true\nnarrate_tools: false\n",
    )
    .expect("config");
    harness.send(&Request::Reload);
    harness.settle().await;
    assert_eq!(harness.request(&Request::Status)["narrate_tools"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mute_session_and_unmute_session_have_the_documented_reply_shapes() {
    let harness = Harness::start("mute-session").await;
    let ok = harness.request(&Request::MuteSession(MuteSession {
        session_id: "s1".into(),
    }));
    assert_eq!(ok, serde_json::json!({"ok": true, "session_id": "s1"}));

    let bad = harness.request(&Request::MuteSession(MuteSession {
        session_id: "  ".into(),
    }));
    assert_eq!(
        bad,
        serde_json::json!({"ok": false, "error": "missing_session_id"})
    );
    assert!(bad.get("session_id").is_none(), "a failure carries no id");

    let ok = harness.request(&Request::UnmuteSession(heard_proto::UnmuteSession {
        session_id: "s1".into(),
    }));
    assert_eq!(ok, serde_json::json!({"ok": true, "session_id": "s1"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_muted_session_reaches_no_sink() {
    let harness = Harness::start("session-muted").await;
    configure(&harness, "");
    harness.request(&Request::MuteSession(MuteSession {
        session_id: "s1".into(),
    }));
    harness.send(&hook_frame(
        "s1",
        serde_json::json!({
            "hook_event_name": "PreToolUse", "cwd": "/tmp/proj",
            "tool_name": "Bash", "tool_input": {"command": "cargo test"},
        }),
    ));
    harness.settle().await;
    assert!(harness.spoken().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pin_and_unpin_move_the_router() {
    let harness = Harness::start("pin").await;
    // The router only pins a session it has seen; drive one event first.
    harness.send(&serde_json::json!({
        "cmd": "event", "kind": "tool_pre", "tag": "tool_bash",
        "neutral": "Running a shell command", "ctx": {},
        "session": {"id": "s1", "cwd": "/tmp/p", "transcript_path": null},
    }));
    harness.settle().await;
    harness.send(&Request::Pin(Pin {
        session_id: "s1".into(),
    }));
    harness.settle().await;
    assert_eq!(harness.request(&Request::Status)["router_mode"], "pinned");
    harness.send(&Request::Unpin);
    harness.settle().await;
    assert_eq!(harness.request(&Request::Status)["router_mode"], "solo");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_health_probe_echoes_a_valid_nonce_and_rejects_a_short_one() {
    let harness = Harness::start("probe").await;
    let nonce = "a".repeat(32);
    let reply = harness.request(&Request::Event(heard_proto::Event::HealthProbe(
        HealthProbe::new(Agent::ClaudeCode, &nonce),
    )));
    assert_eq!(reply["nonce"], nonce);
    assert_eq!(reply["agent"], "claude-code");

    let short = harness.request(&Request::Event(heard_proto::Event::HealthProbe(
        HealthProbe::new(Agent::Codex, "tooshort"),
    )));
    assert_eq!(short, serde_json::json!({}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cmdless_fallthrough_speaks_literal_text() {
    let harness = Harness::start("speak").await;
    harness.send(&Speak {
        text: "Hello there".into(),
        priority: false,
    });
    let spoken = harness.wait_for_lines(1).await;
    assert_eq!(spoken[0]["text"], "Hello there");
    assert_eq!(spoken[0]["via"], "direct");
}

/// Frames as Swift's `JSONEncoder` writes them: `/` escaped as `\/`, plus
/// quotes, newlines, `\u` escapes and raw non-ASCII. A borrowed `&str` field
/// cannot hold an unescaped string, so each of these used to fail to parse
/// and fall through to a blank `speak` — silently doing nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escaped_speak_text_is_spoken_unescaped() {
    let harness = Harness::start("speak-escaped").await;
    harness.send_raw(
        br#"{"text":"Edited \"src\/main.rs\"\nand caf\u00e9 \ud83d\ude80 \u2014 na\u00efve"}"#,
    );
    let spoken = harness.wait_for_lines(1).await;
    let text = spoken[0]["text"].as_str().expect("text");
    assert!(text.contains("\"src/main.rs\""), "{text:?}");
    assert!(text.contains("caf\u{e9} \u{1f680}"), "{text:?}");
    assert!(text.contains("na\u{ef}ve"), "{text:?}");
    assert!(
        !text.contains('\\'),
        "escapes must not reach the sink: {text:?}"
    );
    assert_eq!(spoken[0]["via"], "direct");

    // Raw UTF-8 (no escapes) is spoken as-is too.
    harness.send_raw("{\"text\":\"d\u{e9}j\u{e0} vu \u{1f389}\"}".as_bytes());
    let spoken = harness.wait_for_lines(2).await;
    assert_eq!(spoken[1]["text"], "d\u{e9}j\u{e0} vu \u{1f389}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escaped_command_fields_act_on_the_unescaped_value() {
    let harness = Harness::start("cmd-escaped").await;

    // mute_session echoes the id it acted on.
    let raw = request_to(
        &harness.socket,
        br#"{"cmd":"mute_session","session_id":"proj\/s\u00e9 \"1\""}"#,
    );
    let reply: Value = serde_json::from_slice(&raw).expect("reply");
    assert_eq!(
        reply,
        serde_json::json!({"ok": true, "session_id": "proj/s\u{e9} \"1\""})
    );

    // A muted source with escapes still mutes.
    harness.send_raw(br#"{"cmd":"mute","source":"menu\/bar"}"#);
    assert_eq!(harness.request(&Request::Status)["muted"], true);
    harness.send_raw(br#"{"cmd":"unmute","source":"menu\/bar"}"#);
    assert_eq!(harness.request(&Request::Status)["muted"], false);

    // A narration event whose session id and cwd carry escapes, then a pin
    // to that same (escaped) id: the router must see one session.
    harness.send_raw(
        br#"{"cmd":"event","kind":"tool_pre","tag":"tool_bash","neutral":"Running \"cargo\"\nnow","ctx":{},"session":{"id":"s\/1","cwd":"\/tmp\/caf\u00e9","transcript_path":null}}"#,
    );
    harness.settle().await;
    harness.send_raw(br#"{"cmd":"pin","session_id":"s\/1"}"#);
    harness.settle().await;
    assert_eq!(harness.request(&Request::Status)["router_mode"], "pinned");

    // A health probe whose nonce is 32 characters once unescaped.
    let raw = request_to(
        &harness.socket,
        br#"{"cmd":"event","kind":"health_probe","agent":"claude-code","nonce":"abcdefghijklmno\/pqrstuvwxyz01234"}"#,
    );
    let reply: Value = serde_json::from_slice(&raw).expect("reply");
    assert_eq!(reply["nonce"], "abcdefghijklmno/pqrstuvwxyz01234");
    assert_eq!(reply["agent"], "claude-code");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_shuts_the_listener_down_cleanly() {
    let harness = Harness::start("stop").await;
    harness.send(&Request::Stop);
    for _ in 0..500 {
        if !harness.socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(harness.daemon.is_stopping());
    assert!(
        !harness.socket.exists(),
        "stop must unlink the socket it bound"
    );
    assert!(
        UnixStream::connect(&harness.socket).is_err(),
        "nothing answers after stop"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_payload_is_logged_and_ignored() {
    let harness = Harness::start("malformed").await;
    harness.send_raw(b"{not json at all");
    harness.send_raw(b"");
    harness.send_raw(&[0xff, 0xfe, 0xfd]);
    harness.send_raw(b"[1,2,3]");
    harness.send_raw(br#"{"cmd":"hook","agent":"claude-code","payload":"not an object"}"#);
    harness.send_raw(br#"{"cmd":"hook"}"#);
    harness.settle().await;
    assert_eq!(harness.request(&Request::Status)["alive"], true);
    assert!(harness.spoken().is_empty());
    assert_eq!(harness.hooks.pending(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_cmd_falls_through_to_speak_exactly_as_python_does() {
    let harness = Harness::start("unknown-cmd").await;
    // `cmd = req.get("cmd", "speak")` then a chain of `if cmd == …`: an
    // unrecognised cmd reaches `_start_speech(req.get("text") or "")`, which
    // with no `text` speaks nothing.
    harness.send_raw(br#"{"cmd":"not_a_core_command","question":"hi"}"#);
    harness.settle().await;
    assert!(harness.spoken().is_empty());
}

// ---------------------------------------------------------------------------
// the hook path
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_and_post_tool_pair_reach_the_sink_in_order() {
    let harness = Harness::start("hook-order").await;
    configure(&harness, "");

    harness.send(&hook_frame(
        "s1",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/tmp/proj",
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test"},
        }),
    ));
    harness.send(&hook_frame(
        "s1",
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "cwd": "/tmp/proj",
            "tool_name": "Bash",
            "tool_response": {"exit_code": 1, "stderr": "boom"},
        }),
    ));
    let spoken = harness.wait_for_lines(2).await;
    harness.settle().await;

    assert_eq!(harness.spoken().len(), 2, "{spoken:#?}");
    assert_eq!(spoken[0]["kind"], "tool_pre");
    assert_eq!(spoken[1]["kind"], "tool_post");
    assert_eq!(spoken[0]["session_id"], "s1");
    // `cargo test` is `tool_bash_test`, a harness wake tag: it skips the
    // fast path, the brain punts and the floor reads its template (Python).
    assert_eq!(spoken[0]["via"], "floor");
    assert_eq!(spoken[1]["tag"], "tool_post_command_failed");
    assert!(spoken[0]["ts"].as_f64().expect("ts") > 0.0);
    assert!(!spoken[0]["text"].as_str().expect("text").is_empty());

    // `history.jsonl` is written through the same sink, in the same order.
    let history = harness.history();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["spoken"], spoken[0]["text"]);
    assert_eq!(history[0]["via"], "floor");
    assert_eq!(history[0]["id"].as_str().expect("id").len(), 32);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sessions_interleave_without_crossing_lanes() {
    let harness = Harness::start("hook-interleave").await;
    configure(&harness, "");

    // Interleaved on the wire: A-pre, B-pre, A-post, B-post. Each session's
    // own order must hold; the two sessions are free to race each other.
    //
    // Both tags PIERCE (`tool_question`, `tool_post_command_failed`). Two
    // active sessions put the router in SWARM, where a routine tool event is
    // correctly batched into a project digest instead of spoken — so a test
    // about ORDERING has to use events the router speaks in either mode, or
    // it is really a test about how fast the second session registered.
    for (session, event) in [
        ("sa", "PreToolUse"),
        ("sb", "PreToolUse"),
        ("sa", "PostToolUse"),
        ("sb", "PostToolUse"),
    ] {
        let payload = if event == "PreToolUse" {
            serde_json::json!({
                "hook_event_name": event, "cwd": format!("/tmp/{session}"),
                "tool_name": "AskUserQuestion",
                "tool_input": {
                    "questions": [{"question": format!("Ship {session} now?")}]
                },
            })
        } else {
            serde_json::json!({
                "hook_event_name": event, "cwd": format!("/tmp/{session}"),
                "tool_name": "Bash",
                "tool_response": {"exit_code": 1, "stderr": format!("boom {session}")},
            })
        };
        harness.send(&hook_frame(session, payload));
    }
    let spoken = harness.wait_for_lines(4).await;
    harness.settle().await;

    assert_eq!(harness.spoken().len(), 4, "{spoken:#?}");
    for session in ["sa", "sb"] {
        let kinds: Vec<&str> = spoken
            .iter()
            .filter(|l| l["session_id"] == session)
            .map(|l| l["kind"].as_str().expect("kind"))
            .collect();
        assert_eq!(kinds, vec!["tool_pre", "tool_post"], "session {session}");
    }
    assert_eq!(harness.hooks.lanes(), 2, "one FIFO lane per session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_heard_narrates_nothing_and_advances_the_offset() {
    let harness = Harness::start("hook-muted").await;
    configure(&harness, "muted: true\n");

    // Prose the daemon would otherwise replay in one burst on resume.
    let transcript = harness.config_dir.join("t.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\
         \"text\":\"A long line of assistant prose that would be spoken.\"}]}}\n",
    )
    .expect("transcript");

    harness.send(&hook_frame(
        "s1",
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "cwd": "/tmp/proj",
            "transcript_path": transcript.to_string_lossy(),
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test"},
        }),
    ));
    harness.settle().await;

    assert!(harness.spoken().is_empty(), "a paused Heard says nothing");
    let size = std::fs::metadata(&transcript).expect("stat").len();
    assert_eq!(
        harness.daemon.spoken.get_offset("s1"),
        size,
        "the muted offset advance is what stops a post-resume flood"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_prose_suppresses_the_tool_line() {
    let harness = Harness::start("hook-prose-wins").await;
    configure(&harness, "");

    let transcript = harness.config_dir.join("t.jsonl");
    std::fs::write(&transcript, "").expect("transcript");
    let path = transcript.to_string_lossy().to_string();

    let pre = |command: &str| {
        hook_frame(
            "s1",
            serde_json::json!({
                "hook_event_name": "PreToolUse", "cwd": "/tmp/proj",
                "transcript_path": path, "tool_name": "Bash",
                "tool_input": {"command": command},
            }),
        )
    };

    // 1. First hook for this session initialises the offset at EOF (so a
    //    fresh install never replays the whole transcript) and, finding no
    //    fresh prose, speaks its tool line.
    harness.send(&pre("cargo test"));
    let spoken = harness.wait_for_lines(1).await;
    assert_eq!(spoken[0]["kind"], "tool_pre");

    // 2. Still no new prose: the next tool line speaks too.
    harness.send(&pre("cargo build"));
    let spoken = harness.wait_for_lines(2).await;
    assert_eq!(spoken[1]["kind"], "tool_pre");

    // 3. The agent writes prose, THEN calls a tool. The prose is what gets
    //    handled (as an `intermediate`, which the no-LLM floor then drops —
    //    see `_floor_text`), and the tool announcement is skipped: it would
    //    only be noise on top of prose we just narrated.
    std::fs::write(
        &transcript,
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\
         \"text\":\"Now I will run the test suite and see what breaks.\"}]}}\n",
    )
    .expect("append");
    harness.send(&pre("cargo install"));
    harness.settle().await;

    let spoken = harness.spoken();
    assert_eq!(spoken.len(), 2, "no third tool line: {spoken:#?}");
    // The prose was consumed, not merely ignored: the offset moved to EOF.
    let size = std::fs::metadata(&transcript).expect("stat").len();
    assert_eq!(harness.daemon.spoken.get_offset("s1"), size);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_hook_narrates_the_final_through_the_floor() {
    let harness = Harness::start("hook-stop").await;
    configure(&harness, "");

    let transcript = harness.config_dir.join("t.jsonl");
    std::fs::write(&transcript, "").expect("transcript");
    let path = transcript.to_string_lossy().to_string();

    // Seed the offset, as the first hook of a session always does. It speaks
    // its own tool line on the way past.
    harness.send(&hook_frame(
        "s1",
        serde_json::json!({
            "hook_event_name": "PreToolUse", "cwd": "/tmp/proj",
            "transcript_path": path, "tool_name": "Bash",
            "tool_input": {"command": "cargo test"},
        }),
    ));
    harness.wait_for_lines(1).await;

    std::fs::write(
        &transcript,
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\
         \"text\":\"All twenty tests pass and the build is clean\"}]}}\n",
    )
    .expect("append");
    harness.send(&hook_frame(
        "s1",
        serde_json::json!({
            "hook_event_name": "Stop", "cwd": "/tmp/proj",
            "transcript_path": path,
        }),
    ));
    let spoken = harness.wait_for_lines(2).await;

    assert_eq!(spoken[1]["kind"], "final");
    assert_eq!(spoken[1]["via"], "floor");
    assert_eq!(spoken[1]["tag"], "final_short");
    // A short final is read as-is by the floor, with Jarvis's address.
    assert_eq!(
        spoken[1]["text"],
        "All twenty tests pass and the build is clean, Sir."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_short_prompt_is_not_worth_a_prompt_intent() {
    let harness = Harness::start("hook-prompt").await;
    configure(&harness, "");
    for prompt in ["go", "please go ahead and refactor the auth module"] {
        harness.send(&hook_frame(
            "s1",
            serde_json::json!({
                "hook_event_name": "UserPromptSubmit", "cwd": "/tmp/proj",
                "prompt": prompt,
            }),
        ));
    }
    harness.settle().await;
    // Both are retired — a `prompt_intent` is never spoken — but only the
    // long one gets as far as arming the turn. `narrate_routine` makes this
    // companion mode, so the submit gets its ONE canned filler (the second
    // would be two fillers back to back).
    let spoken = harness.spoken();
    assert_eq!(spoken.len(), 1, "{spoken:?}");
    assert_eq!(spoken[0]["via"], "filler");
    assert_eq!(spoken[0]["kind"], "");
}

// ---------------------------------------------------------------------------
// the shipped client
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_drives_the_same_daemon() {
    // The one test that uses `heard-proto::transport` itself, through the env
    // override it exists for. It sets a process-wide variable and is the only
    // test that reads one, so it cannot reach another test's socket — and it
    // can never reach the installed daemon's, because it points the variable
    // at its own temp path first.
    let harness = Harness::start("transport").await;
    std::env::set_var(heard_proto::transport::SOCKET_PATH_ENV, &harness.socket);
    assert_eq!(
        heard_proto::transport::socket_path().expect("path"),
        harness.socket
    );

    let status: heard_proto::StatusResponse = tokio::task::block_in_place(|| {
        heard_proto::transport::request(&Request::Status, Duration::from_secs(10))
    })
    .expect("status over the shipped client");
    assert!(status.alive);

    tokio::task::block_in_place(|| heard_proto::transport::send(&Request::Ping))
        .expect("ping over the shipped client");
}
