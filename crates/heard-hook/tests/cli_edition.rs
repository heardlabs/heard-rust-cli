//! The CLI edition's two extra behaviours, against the real binary:
//! auto-starting `heard daemon` and the zero-token `/heard-*` intercept.
//!
//! The hook finds `heard` beside its own executable, so every test copies
//! the built `heard-hook` into a scratch `bin/` next to a fake `heard`
//! shell script that records how it was called. The fake daemon is BSD
//! `nc -dlkU` on the test's own socket; nothing here touches a real Heard
//! socket, config or home.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_heard-hook");

const PRE_TOOL_USE: &str = r#"{"session_id":"s1","cwd":"/tmp/r","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#;

fn prompt_payload(prompt: &str) -> String {
    serde_json::json!({
        "session_id": "s1",
        "transcript_path": "/tmp/r/s1.jsonl",
        "cwd": "/tmp/r",
        "hook_event_name": "UserPromptSubmit",
        "prompt": prompt,
    })
    .to_string()
}

/// A scratch dir with `bin/heard-hook` (a copy of the real one) and room
/// for a fake `bin/heard`. Short paths: a socket path has ~104 bytes.
struct Rig {
    dir: PathBuf,
    _serial: std::sync::MutexGuard<'static, ()>,
}

/// These tests measure wall time and race real processes; running them one
/// at a time keeps a loaded machine from turning 300 ms budgets into flakes.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Rig {
    fn new(tag: &str) -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("hh-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::copy(BIN, dir.join("bin/heard-hook")).unwrap();
        // macOS assesses a freshly copied executable on its first launch
        // (up to a second or two); pay that here, not inside a timing.
        let _ = Command::new(dir.join("bin/heard-hook"))
            .arg("--version")
            .output();
        Self {
            dir,
            _serial: serial,
        }
    }

    fn hook(&self) -> PathBuf {
        self.dir.join("bin/heard-hook")
    }
    fn sock(&self) -> PathBuf {
        self.dir.join("d.sock")
    }
    fn log(&self) -> PathBuf {
        self.dir.join("calls.log")
    }
    fn recv(&self) -> PathBuf {
        self.dir.join("recv.bin")
    }
    fn lock(&self) -> PathBuf {
        self.dir.join("d.sock.spawn.lock")
    }

    /// `bin/heard`: logs `<pid> <args>`, then runs `body`.
    fn fake_heard(&self, body: &str) {
        let p = self.dir.join("bin/heard");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\n[ \"$1\" = __warmup ] && exit 0\necho \"$$ $*\" >> \"$FAKE_LOG\"\n{body}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        // First exec of a new file is slow on macOS; not inside a timing.
        let _ = Command::new(&p).arg("__warmup").output();
    }

    /// A fake `heard` whose `daemon` subcommand becomes a listener on the
    /// socket after `delay` seconds.
    fn fake_daemon(&self, delay: &str) {
        self.fake_heard(&format!(
            "[ \"$1\" = daemon ] || exit 0\nsleep {delay}\nexec nc -dlkU \"$HEARD_DAEMON_SOCKET\" > \"$FAKE_RECV\""
        ));
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(self.hook());
        c.args(args)
            .env("HEARD_DAEMON_SOCKET", self.sock())
            .env("FAKE_LOG", self.log())
            .env("FAKE_RECV", self.recv())
            .env_remove("HEARD_HOOK_DISABLED")
            .env_remove("HEARD_HOOK_NO_AUTOSTART")
            .env_remove("HEARD_DIFFERENTIAL_SOCKET")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    fn run(&self, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Out {
        let mut c = self.cmd(args);
        for (k, v) in env {
            c.env(k, v);
        }
        run(c, stdin)
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn daemon_spawns(&self) -> usize {
        self.calls()
            .iter()
            .filter(|l| l.ends_with(" daemon"))
            .count()
    }

    /// Frames the fake daemon has received so far.
    fn frames(&self) -> Vec<serde_json::Value> {
        let bytes = std::fs::read(self.recv()).unwrap_or_default();
        serde_json::Deserializer::from_slice(&bytes)
            .into_iter::<serde_json::Value>()
            .filter_map(Result::ok)
            .collect()
    }

    fn wait_for_frames(&self, n: usize, within: Duration) -> Vec<serde_json::Value> {
        let end = Instant::now() + within;
        loop {
            let f = self.frames();
            if f.len() >= n || Instant::now() > end {
                return f;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // Stop any fake daemon (`exec nc` keeps the logged pid).
        for line in self.calls() {
            if let Some(pid) = line.strip_suffix(" daemon") {
                let _ = Command::new("kill").arg(pid).stderr(Stdio::null()).status();
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    took: Duration,
}

fn run(mut c: Command, stdin: &str) -> Out {
    let start = Instant::now();
    let mut child = c.spawn().expect("spawn heard-hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    Out {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        took: start.elapsed(),
    }
}

/// Accept connections until the listener is dropped with the thread; send
/// each connection's bytes down the channel.
fn serve(listener: UnixListener) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            if tx.send(buf).is_err() {
                break;
            }
        }
    });
    rx
}

// ---------------------------------------------------------------------------
// Auto-start

#[test]
fn no_daemon_spawns_heard_daemon_once_and_the_frame_arrives() {
    let rig = Rig::new("spawn");
    rig.fake_daemon("0.05");
    let out = rig.run(
        &["claude-code", "--edition", "heard-cli"],
        PRE_TOOL_USE,
        &[],
    );
    assert_eq!(out.code, Some(0));
    assert_eq!(out.stdout, "");
    assert_eq!(out.stderr, "");
    assert_eq!(rig.daemon_spawns(), 1, "calls: {:?}", rig.calls());
    let frames = rig.wait_for_frames(1, Duration::from_secs(2));
    assert_eq!(frames.len(), 1, "the retried send reached the new daemon");
    assert_eq!(frames[0]["cmd"], "hook");
    assert_eq!(frames[0]["payload"]["tool_name"], "Bash");
    assert!(out.took < Duration::from_millis(1500), "{:?}", out.took);

    // The daemon is up now: the next hook just sends, no second spawn.
    let out = rig.run(&["claude-code"], PRE_TOOL_USE, &[]);
    assert_eq!(out.code, Some(0));
    assert_eq!(rig.wait_for_frames(2, Duration::from_secs(2)).len(), 2);
    assert_eq!(rig.daemon_spawns(), 1);
}

#[test]
fn a_burst_of_hooks_spawns_the_daemon_once() {
    let rig = Rig::new("burst");
    rig.fake_daemon("0.1");
    let children: Vec<_> = (0..8)
        .map(|_| {
            let mut c = rig.cmd(&["claude-code"]);
            let mut child = c.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(PRE_TOOL_USE.as_bytes())
                .unwrap();
            child
        })
        .collect();
    for c in children {
        assert_eq!(c.wait_with_output().unwrap().status.code(), Some(0));
    }
    assert_eq!(rig.daemon_spawns(), 1, "calls: {:?}", rig.calls());
    // Hooks that ran out of their 300 ms before the daemon listened are
    // dropped — narration is best effort — but the daemon did come up.
    assert!(!rig.wait_for_frames(1, Duration::from_secs(2)).is_empty());
}

#[test]
fn no_heard_beside_the_hook_is_a_fast_silent_no_op() {
    let rig = Rig::new("nocli");
    let out = rig.run(&["claude-code"], PRE_TOOL_USE, &[]);
    assert_eq!(
        (out.code, out.stdout.as_str(), out.stderr.as_str()),
        (Some(0), "", "")
    );
    assert!(
        out.took < Duration::from_millis(250),
        "no CLI to spawn, so no retry wait: {:?}",
        out.took
    );
    assert!(!rig.lock().exists());
}

#[test]
fn the_opt_out_and_the_recursion_guard_never_spawn() {
    for env in [
        ("HEARD_HOOK_NO_AUTOSTART", "1"),
        ("HEARD_HOOK_DISABLED", "1"),
    ] {
        let rig = Rig::new("optout");
        rig.fake_daemon("0");
        let out = rig.run(&["claude-code"], PRE_TOOL_USE, &[env]);
        assert_eq!(out.code, Some(0));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(rig.daemon_spawns(), 0, "{env:?}");
    }
}

#[test]
fn a_health_probe_never_starts_a_daemon() {
    let rig = Rig::new("probe");
    rig.fake_daemon("0");
    let out = rig.run(
        &[
            "claude-code",
            "--health-probe",
            "0123456789abcdef0123456789abcdef",
        ],
        "",
        &[],
    );
    assert_eq!(out.stdout, "{}\n");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(rig.daemon_spawns(), 0);
}

#[test]
fn a_fresh_lock_means_another_hook_is_spawning() {
    let rig = Rig::new("freshlock");
    rig.fake_daemon("0");
    std::fs::write(rig.lock(), "12345").unwrap();
    let out = rig.run(&["claude-code"], PRE_TOOL_USE, &[]);
    assert_eq!(out.code, Some(0));
    assert_eq!(rig.daemon_spawns(), 0);
    // It waited out its retry budget for the other hook's daemon, no more.
    assert!(out.took >= Duration::from_millis(290), "{:?}", out.took);
    assert!(out.took < Duration::from_millis(1000), "{:?}", out.took);
}

#[test]
fn a_stale_lock_is_taken_over() {
    let rig = Rig::new("stalelock");
    rig.fake_daemon("0");
    let f = std::fs::File::create(rig.lock()).unwrap();
    f.set_modified(std::time::SystemTime::now() - Duration::from_secs(60))
        .unwrap();
    drop(f);
    rig.run(&["claude-code"], PRE_TOOL_USE, &[]);
    assert_eq!(rig.daemon_spawns(), 1);
    assert_eq!(rig.wait_for_frames(1, Duration::from_secs(2)).len(), 1);
}

#[test]
fn the_cli_edition_socket_is_under_heard_cli_home() {
    // HEARD_CLI_HOME is the state root; the socket is daemon.sock inside it.
    let rig = Rig::new("clihome");
    let root = rig.dir.join("st");
    std::fs::create_dir_all(&root).unwrap();
    let rx = serve(UnixListener::bind(root.join("daemon.sock")).unwrap());
    let mut c = rig.cmd(&["codex"]);
    c.env_remove("HEARD_DAEMON_SOCKET")
        .env("HEARD_CLI_HOME", &root)
        .env("HEARD_HOOK_NO_AUTOSTART", "1");
    let out = run(c, PRE_TOOL_USE);
    assert_eq!(out.code, Some(0));
    let got = rx.recv_timeout(Duration::from_secs(2)).expect("frame");
    let v: serde_json::Value = serde_json::from_slice(&got).unwrap();
    assert_eq!(v["agent"], "codex");

    // Without it, the root is ~/Library/Application Support/heard-cli. The
    // path is too long to bind in a temp dir, so read it off the spawn lock
    // the hook leaves beside the socket.
    let home = rig.dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    rig.fake_heard("exit 0");
    let mut c = rig.cmd(&["claude-code"]);
    c.env_remove("HEARD_DAEMON_SOCKET")
        .env_remove("HEARD_CLI_HOME")
        .env("HOME", &home);
    run(c, PRE_TOOL_USE);
    let want = if cfg!(target_os = "macos") {
        "Library/Application Support/heard-cli/daemon.sock.spawn.lock"
    } else {
        ".local/share/heard-cli/daemon.sock.spawn.lock"
    };
    assert!(home.join(want).exists(), "no lock at {want}");
    assert_eq!(rig.daemon_spawns(), 1);
}

// ---------------------------------------------------------------------------
// The intercept

fn block(reason: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"decision": "block", "reason": reason})
    )
}

/// Bind the socket so a forwarded frame would be seen, run, and return
/// (output, frames received).
fn run_with_listener(rig: &Rig, args: &[&str], stdin: &str) -> (Out, Vec<Vec<u8>>) {
    let rx = serve(UnixListener::bind(rig.sock()).unwrap());
    let out = rig.run(args, stdin, &[]);
    let mut got = Vec::new();
    while let Ok(b) = rx.recv_timeout(Duration::from_millis(150)) {
        got.push(b);
    }
    (out, got)
}

#[test]
fn a_heard_command_prompt_is_answered_with_a_block_and_not_forwarded() {
    for (prompt, want_args) in [
        ("/heard-mode focus", "mode focus --porcelain"),
        ("  @heard mode   focus\n", "mode focus --porcelain"),
        ("/heard-pause", "pause --porcelain"),
    ] {
        let rig = Rig::new("icept");
        rig.fake_heard("echo 'Focus mode'");
        let (out, got) = run_with_listener(&rig, &["claude-code"], &prompt_payload(prompt));
        assert_eq!(out.code, Some(0));
        assert_eq!(out.stdout, block("Heard: Focus mode"), "{prompt:?}");
        assert_eq!(out.stderr, "");
        let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(v["decision"], "block");
        assert!(got.is_empty(), "an intercepted prompt is not forwarded");
        let calls = rig.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].ends_with(&format!(" {want_args}")), "{calls:?}");
    }
}

#[test]
fn an_ordinary_prompt_is_forwarded_unchanged() {
    let rig = Rig::new("plain");
    rig.fake_heard("echo nope");
    let payload = prompt_payload("please fix /heard-mode in the docs");
    let (out, got) = run_with_listener(&rig, &["claude-code"], &payload);
    assert_eq!((out.code, out.stdout.as_str()), (Some(0), ""));
    assert_eq!(got.len(), 1);
    let v: serde_json::Value = serde_json::from_slice(&got[0]).unwrap();
    assert_eq!(v["payload"]["prompt"], "please fix /heard-mode in the docs");
    assert!(rig.calls().is_empty(), "heard was not run");
}

#[test]
fn unknown_verbs_and_codex_prompts_are_forwarded() {
    for (agent, prompt) in [
        ("claude-code", "/heard-frobnicate now"),
        ("codex", "/heard-mode focus"),
    ] {
        let rig = Rig::new("fwd");
        rig.fake_heard("echo nope");
        let (out, got) = run_with_listener(&rig, &[agent], &prompt_payload(prompt));
        assert_eq!(out.stdout, "", "{agent} {prompt}");
        assert_eq!(got.len(), 1, "{agent} {prompt}");
        assert!(rig.calls().is_empty());
    }
}

#[test]
fn the_reply_comes_from_the_cli_whatever_it_does() {
    for (body, want) in [
        // Only the first non-empty line is used.
        (
            "printf '\\nVoice: bm_george\\nextra\\n'",
            "Heard: Voice: bm_george",
        ),
        // A failure's stderr becomes the reason; `heard:` is normalised.
        (
            "echo 'heard: daemon not running, run heard start' >&2; exit 1",
            "Heard: daemon not running, run heard start",
        ),
        ("exit 0", "Heard: done"),
        ("exit 3", "Heard: `heard mode` failed"),
        // Quotes and unicode survive the JSON encoding.
        (
            "echo 'Persona \"Jarvis\" — ready'",
            "Heard: Persona \"Jarvis\" — ready",
        ),
    ] {
        let rig = Rig::new("reply");
        rig.fake_heard(body);
        let (out, got) =
            run_with_listener(&rig, &["claude-code"], &prompt_payload("/heard-mode x"));
        assert_eq!(out.stdout, block(want), "{body}");
        assert!(got.is_empty());
    }
}

#[test]
fn a_hung_cli_is_cut_off_at_two_seconds() {
    let rig = Rig::new("hung");
    rig.fake_heard("exec sleep 10");
    let (out, got) = run_with_listener(&rig, &["claude-code"], &prompt_payload("/heard-status"));
    assert_eq!(out.code, Some(0));
    assert_eq!(out.stdout, block("Heard: `heard status` timed out"));
    assert!(got.is_empty());
    assert!(out.took >= Duration::from_millis(1900), "{:?}", out.took);
    assert!(out.took < Duration::from_millis(4000), "{:?}", out.took);
}

#[test]
fn a_missing_cli_still_blocks_with_a_useful_reason() {
    let rig = Rig::new("icept-nocli");
    let (out, got) =
        run_with_listener(&rig, &["claude-code"], &prompt_payload("/heard-mode focus"));
    assert_eq!(out.code, Some(0));
    let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["decision"], "block");
    assert!(v["reason"].as_str().unwrap().contains("install.sh"));
    assert!(got.is_empty());
}

// ---------------------------------------------------------------------------
// Latency

/// p95 wall time per invocation with a live daemon. The target is < 20 ms
/// for a release build (measured ~2–4 ms, most of it fork/exec). Debug
/// builds and a loaded CI box get a documented 2.5× tolerance — the number
/// being guarded is "no retry, no spawn, no sleep on the happy path", which
/// would show up as ≥ 300 ms, not as a few ms of debug overhead.
#[test]
fn p95_latency_with_a_live_daemon() {
    let rig = Rig::new("lat");
    rig.fake_heard("echo should-not-run; exit 1");
    let rx = serve(UnixListener::bind(rig.sock()).unwrap());
    let payloads = [
        PRE_TOOL_USE.to_owned(),
        prompt_payload("run the tests please"),
    ];
    let n = 120;
    let mut times: Vec<Duration> = (0..n)
        .map(|i| rig.run(&["claude-code"], &payloads[i % 2], &[]).took)
        .collect();
    times.sort();
    let p95 = times[n * 95 / 100];
    let limit = if cfg!(debug_assertions) {
        Duration::from_millis(50)
    } else {
        Duration::from_millis(20)
    };
    eprintln!(
        "heard-hook p50 {:?} p95 {:?} max {:?} (limit {limit:?})",
        times[n / 2],
        p95,
        times[n - 1]
    );
    assert!(p95 < limit, "p95 {p95:?} >= {limit:?}");
    let mut received = 0;
    while rx.recv_timeout(Duration::from_millis(200)).is_ok() {
        received += 1;
    }
    assert_eq!(received, n, "every hook delivered exactly one frame");
    assert!(rig.calls().is_empty());
}
