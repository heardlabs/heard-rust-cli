//! Pty smoke test of the console: `/mode focus` changes config, and a line
//! appended to history.jsonl is printed above the prompt by the live feed.
//!
//! The terminal emulator is played by the test: crossterm asks for the cursor
//! position (`ESC[6n`) and this answers `ESC[1;1R`.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::Env;
use rexpect::reader::{Options, ReadUntil};
use rexpect::session::{spawn_with_options, PtySession};

const CPR: &str = "\x1b[6n";

/// Read until `needle`, answering cursor-position queries on the way.
fn expect(p: &mut PtySession, needle: &str) -> String {
    let mut seen = String::new();
    loop {
        let (before, found) = p
            .exp_any(vec![
                ReadUntil::String(CPR.into()),
                ReadUntil::String(needle.into()),
            ])
            .unwrap_or_else(|e| panic!("waiting for {needle:?}: {e}\nseen so far: {seen:?}"));
        seen.push_str(&before);
        if found == CPR {
            p.send("\x1b[1;1R").unwrap();
            p.flush().unwrap();
            continue;
        }
        return seen;
    }
}

/// Answer queries for a moment so the editor is idle at its prompt.
/// Consumes (and discards) whatever is printed meanwhile.
fn settle(p: &mut PtySession, ms: u64) {
    let t0 = Instant::now();
    let mut buf = String::new();
    while t0.elapsed() < Duration::from_millis(ms) {
        while let Some(c) = p.try_read() {
            buf.push(c);
            if buf.ends_with(CPR) {
                p.send("\x1b[1;1R").unwrap();
                p.flush().unwrap();
                buf.clear();
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn console_mode_command_and_live_feed() {
    let env = Env::new();
    let bin = assert_cmd::cargo::cargo_bin!("heard");
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg("stty cols 100 rows 40; exec \"$0\"")
        .arg(bin);
    env.apply(&mut cmd);
    cmd.env("TERM", "xterm-256color");
    let mut p = spawn_with_options(
        cmd,
        Options::new()
            .timeout_ms(Some(15_000))
            .strip_ansi_escape_codes(false),
    )
    .unwrap();

    // Header, then the prompt.
    let head = expect(&mut p, "type / for commands");
    assert!(head.contains("heard"), "{head:?}");
    assert!(head.contains("co-pilot"), "{head:?}");
    expect(&mut p, "> ");
    settle(&mut p, 300);

    p.send("/mode focus").unwrap();
    p.flush().unwrap();
    settle(&mut p, 300);
    // Close any completion menu, then submit.
    p.send("\x1b").unwrap();
    p.flush().unwrap();
    settle(&mut p, 100);
    p.send("\r").unwrap();
    p.flush().unwrap();
    expect(&mut p, "mode: focus");

    let t0 = Instant::now();
    while !env.config_yaml().contains("mode: focus") {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "config never changed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(env.config_yaml().contains("narration_volume: 0"));

    // The live feed: append a spoken line; it appears while the prompt waits.
    settle(&mut p, 300);
    let rec = serde_json::json!({
        "ts": "2026-09-23T14:02:11Z", "kind": "final", "tag": "stop", "via": "template",
        "repo_name": "demo-repo", "id": "x", "session_id": "s1",
        "spoken": "Tests pass. Pushing the branch.", "voice": "bm_george", "persona": "jarvis"
    });
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(env.root().join("history.jsonl"))
        .unwrap();
    writeln!(f, "{rec}").unwrap();
    drop(f);
    let seen = expect(&mut p, "Tests pass. Pushing the branch.");
    let _ = seen;
    // The prompt comes back after the printed line.
    expect(&mut p, "> ");

    settle(&mut p, 200);
    p.send("/quit").unwrap();
    p.flush().unwrap();
    settle(&mut p, 200);
    p.send("\x1b").unwrap();
    p.flush().unwrap();
    settle(&mut p, 100);
    p.send("\r").unwrap();
    p.flush().unwrap();
    let t0 = Instant::now();
    loop {
        match p.process().status() {
            Some(rexpect::process::WaitStatus::StillAlive) | None => {
                assert!(
                    t0.elapsed() < Duration::from_secs(10),
                    "console did not quit"
                );
                settle(&mut p, 100);
            }
            Some(rexpect::process::WaitStatus::Exited(_, code)) => {
                assert_eq!(code, 0);
                break;
            }
            Some(other) => panic!("console ended oddly: {other:?}"),
        }
    }
}

/// The console starts the real daemon (autostart on), and plain text typed
/// at the prompt is spoken through it. The daemon records instead of
/// speaking (`HEARD_DAEMON_SPEECH=log`) and logs notifications to a file.
#[test]
fn console_autostarts_the_real_daemon_and_speaks_plain_text() {
    let env = Env::new();
    let bin = assert_cmd::cargo::cargo_bin!("heard");
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg("stty cols 100 rows 40; exec \"$0\"")
        .arg(bin);
    env.apply(&mut cmd);
    cmd.env_remove("HEARD_NO_AUTOSTART")
        .env("HEARD_DAEMON_SPEECH", "log")
        .env("HEARD_DAEMON_TTS", "null")
        .env("HEARD_DAEMON_NOTIFIER", "log")
        .env("TERM", "xterm-256color");
    let mut p = spawn_with_options(
        cmd,
        Options::new()
            .timeout_ms(Some(15_000))
            .strip_ansi_escape_codes(false),
    )
    .unwrap();
    expect(&mut p, "type / for commands");
    expect(&mut p, "> ");
    assert!(
        env.socket().exists(),
        "the console did not start the daemon"
    );
    settle(&mut p, 300);
    p.send("hello from the console").unwrap();
    p.flush().unwrap();
    settle(&mut p, 200);
    p.send("\r").unwrap();
    p.flush().unwrap();
    let would_say = env.root().join("would-say.jsonl");
    let t0 = Instant::now();
    while !std::fs::read_to_string(&would_say)
        .unwrap_or_default()
        .contains("hello from the console")
    {
        assert!(t0.elapsed() < Duration::from_secs(10), "never spoken");
        settle(&mut p, 50);
    }
    // Leave the daemon behind the console, then stop it.
    p.send("\x04").unwrap();
    p.flush().unwrap();
    settle(&mut p, 300);
    let mut stop = Command::new(bin);
    env.apply(&mut stop);
    let out = stop.arg("stop").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(!env.socket().exists());
}
