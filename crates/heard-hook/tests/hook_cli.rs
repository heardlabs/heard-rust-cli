//! Run the real binary as a subprocess against a fake daemon socket.
//!
//! The hook's contract is "exactly these bytes, or silence" — so these tests
//! spawn the built binary the way an agent CLI does, feed it a canned
//! payload on stdin, and assert on the bytes a listening socket actually
//! received.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;

const BIN: &str = env!("CARGO_BIN_EXE_heard-hook");

/// A Claude Code `PreToolUse` payload, in the shape `hook.py` reads today.
const PRE_TOOL_USE: &str = r#"{"session_id":"7f3c","transcript_path":"/Users/x/.claude/projects/p/7f3c.jsonl","cwd":"/Users/x/repo","hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"file_path":"/Users/x/repo/auth.py","old_string":"a","new_string":"b"}}"#;

/// A socket path nothing else will collide with, in a directory we own.
/// Kept short — a Unix socket path has ~104 bytes to work with.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("heard-hook-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self { dir }
    }

    fn sock(&self) -> PathBuf {
        self.dir.join("d.sock")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Accept exactly one connection and hand back everything it wrote.
fn serve_once(listener: UnixListener) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = Vec::new();
            let _ = stream.read_to_end(&mut buf);
            let _ = tx.send(buf);
        }
    });
    rx
}

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// Every variable `_terminal_binding_from_env` reads. Cleared before each
/// run so a test inherits no host from whatever terminal is running the
/// suite — otherwise "no binding" would pass or fail by accident.
const BINDING_VARS: [&str; 12] = [
    "TERM_PROGRAM",
    "GHOSTTY_RESOURCES_DIR",
    "__CFBundleIdentifier",
    "CURSOR_TRACE_ID",
    "ITERM_SESSION_ID",
    "HERDR_ENV",
    "HERDR_PANE_ID",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
    "HERDR_SESSION",
    "HERDR_SOCKET_PATH",
    "HERDR_BIN_PATH",
];

fn run_hook(sock: &PathBuf, args: &[&str], stdin: &[u8]) -> Run {
    run_hook_env(sock, args, stdin, &[])
}

fn run_hook_env(sock: &PathBuf, args: &[&str], stdin: &[u8], env: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(BIN);
    cmd.args(args)
        .env("HEARD_DAEMON_SOCKET", sock)
        // A `heard` binary may sit beside this one in target/ once heard-cli
        // is built; these tests must never spawn a real daemon.
        .env("HEARD_HOOK_NO_AUTOSTART", "1")
        .env_remove("HEARD_HOOK_DISABLED");
    for var in BINDING_VARS {
        cmd.env_remove(var);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn heard-hook");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    Run {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn sends_exactly_one_hook_frame_with_no_trailing_newline() {
    let scratch = Scratch::new("happy");
    let listener = UnixListener::bind(scratch.sock()).expect("bind");
    let rx = serve_once(listener);

    let run = run_hook(&scratch.sock(), &["claude-code"], PRE_TOOL_USE.as_bytes());
    assert!(run.status.success(), "exit was {:?}", run.status.code());
    assert_eq!(run.stdout, "");
    assert_eq!(run.stderr, "");

    let got = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the daemon socket received nothing");
    let text = String::from_utf8(got).expect("utf-8 frame");

    // No framing byte of any kind: the close IS the frame.
    assert!(
        !text.ends_with('\n') && !text.ends_with('\0'),
        "frame is delimited: {text:?}"
    );

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).expect("frame is JSON"),
        serde_json::json!({
            "cmd": "hook",
            "agent": "claude-code",
            "payload": serde_json::from_str::<serde_json::Value>(PRE_TOOL_USE).unwrap(),
        })
    );

    // The payload crossed verbatim, byte for byte.
    assert!(text.contains(PRE_TOOL_USE), "payload was reshaped: {text}");
    // And `hook_event_name` was not lifted to the top level alongside it.
    assert_eq!(
        text.matches("hook_event_name").count(),
        1,
        "hook_event_name is duplicated: {text}"
    );
}

#[test]
fn the_binding_comes_from_the_hook_processs_own_environment() {
    let scratch = Scratch::new("binding");
    let listener = UnixListener::bind(scratch.sock()).expect("bind");
    let rx = serve_once(listener);

    let run = run_hook_env(
        &scratch.sock(),
        &["claude-code"],
        PRE_TOOL_USE.as_bytes(),
        &[
            ("TERM_PROGRAM", "vscode"),
            ("__CFBundleIdentifier", "com.todesktop.230313mzl4w4u92"),
        ],
    );
    assert!(run.status.success());
    assert_eq!(run.stderr, "");

    let text = String::from_utf8(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap())
        .expect("utf-8");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();

    // Top level, beside the payload — not inside it.
    assert_eq!(v["binding"]["host_name"], "Cursor");
    assert_eq!(v["binding"]["host_type"], "editor_terminal");
    assert_eq!(v["binding"]["provenance"], "env_terminal_binding");
    assert_eq!(v["binding"]["confidence"], 0.9);
    assert_eq!(v["binding"]["process_started_at"], serde_json::Value::Null);
    // The parent pid is this test process, and it is a real pid.
    assert_eq!(v["binding"]["pid"], std::process::id());
    assert!(v["payload"].get("binding").is_none());
    assert!(text.contains(PRE_TOOL_USE), "payload was reshaped: {text}");
}

#[test]
fn a_herdr_pane_carries_its_six_keys() {
    let scratch = Scratch::new("herdr");
    let listener = UnixListener::bind(scratch.sock()).expect("bind");
    let rx = serve_once(listener);

    let run = run_hook_env(
        &scratch.sock(),
        &["claude-code"],
        PRE_TOOL_USE.as_bytes(),
        &[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "p1"),
            ("HERDR_TAB_ID", "t1"),
            ("HERDR_WORKSPACE_ID", "w1"),
            ("HERDR_SESSION", ""),
            ("HERDR_SOCKET_PATH", "/s"),
            ("HERDR_BIN_PATH", "/b"),
            ("TERM_PROGRAM", "Apple_Terminal"),
        ],
    );
    assert!(run.status.success());

    let text = String::from_utf8(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap())
        .expect("utf-8");
    let b = &serde_json::from_str::<serde_json::Value>(&text).unwrap()["binding"];
    // Herdr owns the pty, so it beats TERM_PROGRAM=Apple_Terminal.
    assert_eq!(b["host_name"], "Herdr");
    assert_eq!(b["herdr_pane_id"], "p1");
    assert_eq!(b["herdr_session"], "");
    assert_eq!(b["herdr_bin_path"], "/b");
}

#[test]
fn an_environment_that_names_no_host_omits_the_binding_key() {
    let scratch = Scratch::new("nobinding");
    let listener = UnixListener::bind(scratch.sock()).expect("bind");
    let rx = serve_once(listener);

    // BINDING_VARS are cleared, so nothing identifies a terminal.
    let run = run_hook(&scratch.sock(), &["claude-code"], PRE_TOOL_USE.as_bytes());
    assert!(run.status.success());

    let text = String::from_utf8(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap())
        .expect("utf-8");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    // Omitted entirely — not null.
    assert!(
        v.get("binding").is_none(),
        "binding should be absent: {text}"
    );
    assert_eq!(
        v,
        serde_json::json!({
            "cmd": "hook",
            "agent": "claude-code",
            "payload": serde_json::from_str::<serde_json::Value>(PRE_TOOL_USE).unwrap(),
        })
    );
}

#[test]
fn codex_is_named_on_the_frame() {
    let scratch = Scratch::new("codex");
    let listener = UnixListener::bind(scratch.sock()).expect("bind");
    let rx = serve_once(listener);

    let payload = r#"{"hook_event_name":"Stop","last_assistant_message":"Done.","cwd":"/r"}"#;
    let run = run_hook(&scratch.sock(), &["codex"], payload.as_bytes());
    assert!(run.status.success());

    let text = String::from_utf8(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap())
        .expect("utf-8");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["cmd"], "hook");
    assert_eq!(v["agent"], "codex");
    assert_eq!(v["payload"]["hook_event_name"], "Stop");
}

#[test]
fn a_payload_with_no_event_name_still_goes_through() {
    let scratch = Scratch::new("noname");
    let listener = UnixListener::bind(scratch.sock()).expect("bind");
    let rx = serve_once(listener);

    let run = run_hook(&scratch.sock(), &["claude-code"], br#"{"session_id":"x"}"#);
    assert!(run.status.success());

    let text = String::from_utf8(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap())
        .expect("utf-8");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    // Whether the payload names an event is the daemon's problem now; the
    // hook forwards it either way and invents no top-level copy.
    assert!(v.get("hook_event_name").is_none());
    assert_eq!(v["payload"]["session_id"], "x");
}

#[test]
fn no_socket_exits_zero_and_silent() {
    let scratch = Scratch::new("nosock");
    // Nothing is bound at this path.
    let run = run_hook(&scratch.sock(), &["claude-code"], PRE_TOOL_USE.as_bytes());
    assert!(run.status.success(), "exit was {:?}", run.status.code());
    assert_eq!(run.stdout, "");
    assert_eq!(run.stderr, "");
}

#[test]
fn bad_json_exits_zero_and_sends_nothing() {
    let scratch = Scratch::new("badjson");
    let listener = UnixListener::bind(scratch.sock()).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    for stdin in [&b"not json at all"[..], b"", b"{\"a\":", b"[1,2,3]junk"] {
        let run = run_hook(&scratch.sock(), &["claude-code"], stdin);
        assert!(run.status.success(), "exit was {:?}", run.status.code());
        assert_eq!(run.stdout, "");
        assert_eq!(run.stderr, "");
        assert!(
            listener.accept().is_err(),
            "the hook connected on input {stdin:?}"
        );
    }
}

#[test]
fn an_unknown_agent_exits_zero_and_sends_nothing() {
    let scratch = Scratch::new("agent");
    let listener = UnixListener::bind(scratch.sock()).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    for args in [&["gemini"][..], &[]] {
        let run = run_hook(&scratch.sock(), args, PRE_TOOL_USE.as_bytes());
        assert!(run.status.success());
        assert_eq!(run.stdout, "");
        assert_eq!(run.stderr, "");
        assert!(
            listener.accept().is_err(),
            "the hook connected for {args:?}"
        );
    }
}

#[test]
fn heard_hook_disabled_suppresses_the_send() {
    let scratch = Scratch::new("disabled");
    let listener = UnixListener::bind(scratch.sock()).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");

    let mut child = Command::new(BIN)
        .arg("claude-code")
        .env("HEARD_DAEMON_SOCKET", scratch.sock())
        .env("HEARD_HOOK_DISABLED", "1")
        .env("HEARD_HOOK_NO_AUTOSTART", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(PRE_TOOL_USE.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    assert!(
        listener.accept().is_err(),
        "the hook connected while disabled"
    );
}

#[test]
fn version_flag() {
    let scratch = Scratch::new("version");
    let run = run_hook(&scratch.sock(), &["--version"], b"");
    assert!(run.status.success());
    assert_eq!(
        run.stdout.trim(),
        format!("heard-hook {}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(run.stderr, "");
}

#[test]
fn health_probe_prints_empty_object_when_the_daemon_is_down() {
    let scratch = Scratch::new("probe");
    let run = run_hook(
        &scratch.sock(),
        &["claude-code", "--health-probe", &"n".repeat(32)],
        b"",
    );
    assert!(run.status.success());
    assert_eq!(run.stdout.trim(), "{}");
    assert_eq!(run.stderr, "");
}

#[test]
fn health_probe_round_trips_the_nonce() {
    let scratch = Scratch::new("probe2");
    let listener = UnixListener::bind(scratch.sock()).expect("bind");
    let nonce = "a".repeat(32);

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buf = Vec::new();
        // The client half-closes after writing, so this returns.
        let _ = stream.read_to_end(&mut buf);
        let _ = tx.send(buf);
        let _ = stream
            .write_all(br#"{"nonce":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","agent":"claude-code"}"#);
    });

    let run = run_hook(
        &scratch.sock(),
        &["claude-code", "--health-probe", &nonce],
        b"",
    );
    assert!(run.status.success());

    let sent = String::from_utf8(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap())
        .expect("utf-8");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&sent).unwrap(),
        serde_json::json!({
            "cmd": "event",
            "kind": "health_probe",
            "agent": "claude-code",
            "nonce": nonce,
        })
    );

    let reply: serde_json::Value = serde_json::from_str(run.stdout.trim()).expect("reply is JSON");
    assert_eq!(reply["nonce"], nonce);
    assert_eq!(reply["agent"], "claude-code");
}

// ---------------------------------------------------------------------------
// the differential tee
// ---------------------------------------------------------------------------

fn diff_sock(scratch: &Scratch) -> PathBuf {
    scratch.dir.join("r.sock")
}

#[test]
fn the_tee_delivers_the_byte_identical_frame_to_the_second_socket() {
    let scratch = Scratch::new("tee");
    let primary = serve_once(UnixListener::bind(scratch.sock()).expect("bind primary"));
    let second = serve_once(UnixListener::bind(diff_sock(&scratch)).expect("bind tee"));

    let tee = diff_sock(&scratch);
    let run = run_hook_env(
        &scratch.sock(),
        &["claude-code"],
        PRE_TOOL_USE.as_bytes(),
        &[("HEARD_DIFFERENTIAL_SOCKET", tee.to_str().unwrap())],
    );
    assert!(run.status.success());
    assert_eq!((run.stdout.as_str(), run.stderr.as_str()), ("", ""));

    let a = primary
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("primary got nothing");
    let b = second
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("tee got nothing");
    assert_eq!(a, b, "the two daemons must see exactly the same bytes");
    assert!(String::from_utf8(a).unwrap().contains(PRE_TOOL_USE));
}

#[test]
fn a_dead_tee_socket_changes_nothing_for_the_primary() {
    let scratch = Scratch::new("tee-dead");
    let primary = serve_once(UnixListener::bind(scratch.sock()).expect("bind primary"));
    let missing = scratch.dir.join("nobody-home.sock");

    let started = std::time::Instant::now();
    let run = run_hook_env(
        &scratch.sock(),
        &["claude-code"],
        PRE_TOOL_USE.as_bytes(),
        &[("HEARD_DIFFERENTIAL_SOCKET", missing.to_str().unwrap())],
    );
    assert!(run.status.success());
    assert_eq!(run.stderr, "");
    let got = primary
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("primary got nothing");
    assert!(String::from_utf8(got).unwrap().contains(PRE_TOOL_USE));
    // A missing socket fails the connect immediately; nothing waits on it.
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn the_tee_still_runs_when_the_primary_daemon_is_down() {
    let scratch = Scratch::new("tee-primary-down");
    let second = serve_once(UnixListener::bind(diff_sock(&scratch)).expect("bind tee"));
    let tee = diff_sock(&scratch);
    let run = run_hook_env(
        &scratch.sock(),
        &["codex"],
        PRE_TOOL_USE.as_bytes(),
        &[("HEARD_DIFFERENTIAL_SOCKET", tee.to_str().unwrap())],
    );
    assert!(run.status.success());
    assert_eq!(run.stderr, "");
    let got = second
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("tee got nothing");
    let v: serde_json::Value = serde_json::from_slice(&got).unwrap();
    assert_eq!(v["agent"], "codex");
}

#[test]
fn a_tee_pointing_at_the_primary_socket_is_ignored() {
    let scratch = Scratch::new("tee-same");
    let listener = UnixListener::bind(scratch.sock()).expect("bind");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // Count every connection for a short window.
        listener.set_nonblocking(true).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        let mut frames = 0;
        while std::time::Instant::now() < until {
            if let Ok((mut s, _)) = listener.accept() {
                s.set_nonblocking(false).unwrap();
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf);
                frames += 1;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        let _ = tx.send(frames);
    });
    let same = scratch.sock();
    let run = run_hook_env(
        &scratch.sock(),
        &["claude-code"],
        PRE_TOOL_USE.as_bytes(),
        &[("HEARD_DIFFERENTIAL_SOCKET", same.to_str().unwrap())],
    );
    assert!(run.status.success());
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
        1
    );
}

#[test]
fn a_disabled_hook_tees_nothing_either() {
    let scratch = Scratch::new("tee-disabled");
    let listener = UnixListener::bind(diff_sock(&scratch)).expect("bind tee");
    listener.set_nonblocking(true).unwrap();
    let tee = diff_sock(&scratch);
    let run = run_hook_env(
        &scratch.sock(),
        &["claude-code"],
        PRE_TOOL_USE.as_bytes(),
        &[
            ("HEARD_DIFFERENTIAL_SOCKET", tee.to_str().unwrap()),
            ("HEARD_HOOK_DISABLED", "1"),
        ],
    );
    assert!(run.status.success());
    assert!(listener.accept().is_err(), "a disabled hook must not tee");
}
