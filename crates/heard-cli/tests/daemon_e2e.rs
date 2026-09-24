//! The real `heard daemon`, end to end, in a temp `HEARD_CLI_HOME`:
//! `start` / `status` / `say` / `stop` / `restart`, the single-instance
//! guard, signals, the alerts gate, and `heard-hook` auto-starting it.
//!
//! Every daemon here records instead of speaking (`--speech log` /
//! `HEARD_DAEMON_SPEECH=log`), logs notifications to a file instead of
//! posting them (`HEARD_DAEMON_NOTIFIER=log`), and has provider keys
//! removed. Nothing plays audio, nothing runs `osascript`.

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::Env;
use serde_json::Value;

/// These tests race real processes; one at a time keeps them honest on a
/// loaded machine.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// `Env` plus the daemon's test switches.
fn daemon_env() -> Env {
    Env::new()
}

fn heard(env: &Env) -> Command {
    let mut c = env.std_command();
    c.env("HEARD_DAEMON_SPEECH", "log")
        .env("HEARD_DAEMON_TTS", "null")
        .env("HEARD_DAEMON_NOTIFIER", "log");
    c
}

fn run(env: &Env, args: &[&str]) -> (i32, String, String) {
    let out = heard(env).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn jsonl(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn would_say(env: &Env) -> Vec<Value> {
    jsonl(&env.root().join("would-say.jsonl"))
}

fn stop(env: &Env) {
    let (code, out, err) = run(env, &["stop"]);
    assert_eq!(code, 0, "{out}{err}");
}

/// Stops the daemon even when a test fails half way.
struct Guard<'a>(&'a Env);
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let _ = heard(self.0).arg("stop").output();
    }
}

#[test]
fn start_status_say_stop_round_trip() {
    let _s = serial();
    let env = daemon_env();
    let _g = Guard(&env);
    let (code, out, err) = run(&env, &["start"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("daemon started"), "{out}");
    assert!(env.socket().exists());
    let pid: u32 = std::fs::read_to_string(env.root().join("daemon.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let (code, out, _) = run(&env, &["status", "--json"]);
    assert_eq!(code, 0);
    let st: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(st["daemon"]["running"], true, "{st}");
    assert_eq!(st["daemon"]["pid"], pid, "{st}");
    assert_eq!(st["alerts"], "both");

    // `start` again is a no-op.
    let (code, out, _) = run(&env, &["start"]);
    assert_eq!(code, 0);
    assert!(out.contains("already running"), "{out}");

    let (code, out, err) = run(&env, &["say", "Hello from the round trip."]);
    assert_eq!(code, 0, "{out}{err}");
    wait_until("the say line", Duration::from_secs(5), || {
        would_say(&env)
            .iter()
            .any(|l| l["text"] == "Hello from the round trip.")
    });
    let line = would_say(&env)
        .into_iter()
        .find(|l| l["text"] == "Hello from the round trip.")
        .unwrap();
    assert_eq!(line["kind"], "speak");
    assert_eq!(line["via"], "direct");
    for k in ["ts", "text", "tag", "kind", "session_id", "via"] {
        assert!(line.get(k).is_some(), "{k} in {line}");
    }
    // history.jsonl is written beside it.
    wait_until("history", Duration::from_secs(2), || {
        std::fs::read_to_string(env.root().join("history.jsonl"))
            .unwrap_or_default()
            .contains("Hello from the round trip.")
    });

    // restart: a new pid, still one daemon.
    let (code, out, err) = run(&env, &["restart"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(
        out.contains("daemon stopped") && out.contains("daemon started"),
        "{out}"
    );
    let pid2: u32 = std::fs::read_to_string(env.root().join("daemon.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_ne!(pid, pid2);

    stop(&env);
    assert!(!env.socket().exists(), "socket left behind");
    assert!(
        !env.root().join("daemon.pid").exists(),
        "pid file left behind"
    );
    let (_, out, _) = run(&env, &["status", "--json"]);
    let st: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(st["daemon"]["running"], false, "{st}");
    let log = std::fs::read_to_string(env.root().join("daemon.log")).unwrap();
    assert!(log.contains("ev=cli_daemon_start"), "{log}");
    assert!(log.contains("ev=cli_daemon_exit"), "{log}");
}

#[test]
fn a_second_daemon_exits_zero_and_leaves_the_first_alone() {
    let _s = serial();
    let env = daemon_env();
    let _g = Guard(&env);
    assert_eq!(run(&env, &["start"]).0, 0);
    let pid = std::fs::read_to_string(env.root().join("daemon.pid")).unwrap();
    let t0 = Instant::now();
    let status = heard(&env)
        .args(["daemon", "--foreground"])
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(t0.elapsed() < Duration::from_secs(5));
    // Detached too (the hook's way in).
    let status = heard(&env).arg("daemon").status().unwrap();
    assert!(status.success());
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        std::fs::read_to_string(env.root().join("daemon.pid")).unwrap(),
        pid
    );
    assert!(env.socket().exists());
    stop(&env);
}

#[test]
fn sigint_and_sigterm_stop_it_cleanly() {
    let _s = serial();
    for sig in [rustix::process::Signal::INT, rustix::process::Signal::TERM] {
        let env = daemon_env();
        let mut child = heard(&env)
            .args(["daemon", "--foreground", "--speech", "log", "--tts", "null"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_until("the socket", Duration::from_secs(10), || {
            env.socket().exists()
        });
        let pid = rustix::process::Pid::from_child(&child);
        rustix::process::kill_process(pid, sig).unwrap();
        let t0 = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "{sig:?} did not stop it"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "{sig:?}: {status}");
        assert!(!env.socket().exists(), "{sig:?}: socket left");
        assert!(!env.root().join("daemon.pid").exists(), "{sig:?}: pid left");
    }
}

// ── the hook ────────────────────────────────────────────────────────────

/// The built `heard-hook`, beside `heard` (auto-start runs its sibling).
fn hook_bin() -> PathBuf {
    let heard = PathBuf::from(assert_cmd::cargo::cargo_bin!("heard"));
    let hook = heard.with_file_name("heard-hook");
    if !hook.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let ok = Command::new(cargo)
            .args(["build", "-p", "heard-hook"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok && hook.exists(), "cannot build heard-hook");
    }
    hook
}

fn pipe_hook(env: &Env, payload: &Value) -> std::process::ExitStatus {
    let mut c = Command::new(hook_bin());
    env.apply(&mut c);
    c.arg("claude-code")
        .env("HEARD_DAEMON_SPEECH", "log")
        .env("HEARD_DAEMON_TTS", "null")
        .env("HEARD_DAEMON_NOTIFIER", "log")
        .env_remove("HEARD_DAEMON_SOCKET")
        .env_remove("HEARD_HOOK_NO_AUTOSTART")
        .env_remove("HEARD_HOOK_DISABLED")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = c.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child.wait().unwrap()
}

fn failure_payload(env: &Env, session: &str, error: &str) -> Value {
    serde_json::json!({
        "session_id": session,
        "transcript_path": env.root().join("no-transcript.jsonl"),
        "cwd": "/tmp/heard-e2e-repo",
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "cargo test"},
        "tool_response": {"error": error},
    })
}

#[test]
fn the_hook_auto_starts_the_daemon_and_a_fresh_install_is_not_held() {
    let _s = serial();
    let env = daemon_env();
    let _g = Guard(&env);
    // A fresh install: no config.yaml at all, `heard setup` never ran.
    assert!(!env.root().join("config.yaml").exists());
    assert!(!env.socket().exists());
    let status = pipe_hook(&env, &failure_payload(&env, "hook-e2e-1", "disk is full"));
    assert!(status.success());
    wait_until("auto-start", Duration::from_secs(10), || {
        env.socket().exists()
    });
    wait_until("the would-say line", Duration::from_secs(10), || {
        would_say(&env)
            .iter()
            .any(|l| l["tag"] == "tool_post_failure")
    });
    let line = would_say(&env)
        .into_iter()
        .find(|l| l["tag"] == "tool_post_failure")
        .unwrap();
    assert_eq!(line["text"], "Error: disk is full.");
    assert_eq!(line["kind"], "tool_post");
    assert_eq!(line["session_id"], "hook-e2e-1");
    // Needs-you → a notification (to the log, never osascript).
    wait_until("the notification", Duration::from_secs(5), || {
        env.root().join("notifications.jsonl").exists()
    });
    let n = jsonl(&env.root().join("notifications.jsonl"));
    assert_eq!(n[0]["title"], "Heard");
    assert_eq!(n[0]["body"], "Failed: Error: disk is full.");
    // The auto-started daemon is detached: its own session.
    let pid: i32 = std::fs::read_to_string(env.root().join("daemon.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    assert_eq!(rustix::process::getsid(Some(pid)).unwrap(), pid);
    stop(&env);
}

#[test]
fn alerts_notify_keeps_needs_you_lines_quiet_but_notifies() {
    let _s = serial();
    let env = daemon_env();
    let _g = Guard(&env);
    assert_eq!(run(&env, &["start"]).0, 0);
    let (code, out, _) = run(&env, &["alerts", "notify"]);
    assert_eq!(code, 0);
    assert!(out.contains("daemon reloaded"), "{out}");
    assert!(pipe_hook(&env, &failure_payload(&env, "alerts-1", "quota exceeded")).success());
    wait_until("the notification", Duration::from_secs(10), || {
        jsonl(&env.root().join("notifications.jsonl"))
            .iter()
            .any(|n| n["body"] == "Failed: Error: quota exceeded.")
    });
    // …and nothing was spoken for it.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !would_say(&env)
            .iter()
            .any(|l| l["tag"] == "tool_post_failure"),
        "{:?}",
        would_say(&env)
    );
    // `voice`: spoken again, no new notification.
    assert_eq!(run(&env, &["alerts", "voice"]).0, 0);
    assert!(pipe_hook(&env, &failure_payload(&env, "alerts-2", "network down")).success());
    wait_until("the spoken line", Duration::from_secs(10), || {
        would_say(&env)
            .iter()
            .any(|l| l["text"] == "Error: network down.")
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(jsonl(&env.root().join("notifications.jsonl")).len(), 1);
    stop(&env);
}

#[test]
fn the_hook_and_the_cli_agree_on_the_socket() {
    let _s = serial();
    let env = daemon_env();
    // The socket the CLI (and `heard daemon`) resolve under HEARD_CLI_HOME…
    let paths = heard_cli::paths::for_root(env.root());
    assert_eq!(paths.socket_path, env.socket());
    let listener = std::os::unix::net::UnixListener::bind(&paths.socket_path).unwrap();
    listener.set_nonblocking(true).unwrap();
    // …is the one the real hook writes to.
    let mut c = Command::new(hook_bin());
    env.apply(&mut c);
    c.arg("claude-code")
        .env("HEARD_HOOK_NO_AUTOSTART", "1")
        .env_remove("HEARD_DAEMON_SOCKET")
        .stdin(Stdio::piped());
    let mut child = c.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"agree","hook_event_name":"Stop"}"#)
        .unwrap();
    assert!(child.wait().unwrap().success());
    let t0 = Instant::now();
    let mut stream = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(_) if t0.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => panic!("the hook never connected: {e}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut stream, &mut buf).unwrap();
    let frame: Value = serde_json::from_slice(&buf).unwrap();
    assert_eq!(frame["cmd"], "hook");
    assert_eq!(frame["payload"]["session_id"], "agree");
    let hook = heard_hook::HookConfig::CLI_EDITION;
    assert_eq!(hook.app_dir, heard_cli::paths::APP_DIR);
    assert_eq!(hook.root_env, Some(heard_cli::paths::HOME_ENV));
}
