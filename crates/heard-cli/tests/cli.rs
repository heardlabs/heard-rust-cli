//! Every subcommand in a temp HOME: output, --json shapes, exit codes, and
//! the frames a fake daemon receives.

mod common;

use std::collections::HashMap;

use common::{fake_bytes, sha256_hex, status_reply, Env, FakeDaemon, FileServer};
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::{json, Value};

fn stdout_json(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

// ── help, version, usage ────────────────────────────────────────────────

#[test]
fn help_is_grouped_and_lists_every_visible_subcommand() {
    let env = Env::new();
    let out = env
        .cmd()
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .clone();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    for heading in [
        "Get started:",
        "How Heard sounds:",
        "Right now:",
        "Daemon and files:",
    ] {
        assert!(text.contains(heading), "missing {heading}:\n{text}");
    }
    let listed = heard_cli::cli::help_listed_commands();
    use clap::CommandFactory;
    for sub in heard_cli::cli::Cli::command().get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        assert!(
            listed.contains(&sub.get_name()),
            "`{}` is not in the grouped help",
            sub.get_name()
        );
    }
    assert!(
        !text.contains("daemon  "),
        "internal `daemon` must stay hidden"
    );
}

#[test]
fn version_and_usage_exit_codes() {
    let env = Env::new();
    env.cmd()
        .arg("--version")
        .assert()
        .code(0)
        .stdout(contains(env!("CARGO_PKG_VERSION")));
    env.cmd().arg("frobnicate").assert().code(2);
    env.cmd().args(["say"]).assert().code(2);
    env.cmd().args(["history", "--limit", "x"]).assert().code(2);
}

#[test]
fn console_without_a_terminal_is_a_usage_error() {
    let env = Env::new();
    env.cmd()
        .assert()
        .code(2)
        .stderr(contains("needs a terminal").and(contains("fix:")));
}

// ── settings ────────────────────────────────────────────────────────────

#[test]
fn mode_sets_the_preset_and_its_primitives() {
    let env = Env::new();
    env.cmd()
        .args(["mode", "focus"])
        .assert()
        .success()
        .stdout(contains("mode: focus").and(contains("daemon not running")));
    let y = env.config_yaml();
    assert!(y.contains("mode: focus"), "{y}");
    assert!(y.contains("narration_volume: 0"), "{y}");
    assert!(y.contains("send_mode: prefill"), "{y}");
    assert!(y.contains("verbosity: quiet"), "{y}");
    env.cmd().arg("mode").assert().success().stdout("focus\n");

    env.cmd().args(["mode", "co-pilot"]).assert().success();
    assert!(env.config_yaml().contains("mode: copilot"));
    env.cmd().arg("mode").assert().stdout("co-pilot\n");

    env.cmd()
        .args(["mode", "loud"])
        .assert()
        .code(2)
        .stderr(contains("unknown mode").and(contains("fix:")));
}

#[test]
fn mode_change_sends_reload_to_a_running_daemon() {
    let env = Env::new();
    let d = FakeDaemon::start(&env.socket(), status_reply(json!([])));
    env.cmd()
        .args(["mode", "companion"])
        .assert()
        .success()
        .stdout(contains("daemon reloaded"));
    assert_eq!(d.wait_frames(1), vec![json!({"cmd": "reload"})]);
}

#[test]
fn voice_set_list_and_validation() {
    let env = Env::new();
    env.cmd()
        .arg("voice")
        .assert()
        .success()
        .stdout("bm_george\n");
    env.cmd()
        .args(["voice", "af_heart"])
        .assert()
        .success()
        .stdout(contains("voice: af_heart"));
    assert!(env.config_yaml().contains("kokoro_voice: af_heart"));
    env.cmd()
        .args(["voice", "nobody"])
        .assert()
        .code(2)
        .stderr(contains("heard voice --list"));
    let out = env
        .cmd()
        .args(["voice", "--list", "--json"])
        .output()
        .unwrap();
    let v = stdout_json(&out);
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 54);
    assert!(arr
        .iter()
        .any(|x| x["id"] == "af_heart" && x["current"] == true));
}

#[test]
fn voice_preview_needs_the_daemon_and_speaks_through_it() {
    let env = Env::new();
    env.cmd()
        .args(["voice", "--preview"])
        .assert()
        .code(1)
        .stderr(contains("heard start"));
    let d = FakeDaemon::start(&env.socket(), status_reply(json!([])));
    env.cmd()
        .args(["voice", "bf_emma", "--preview"])
        .assert()
        .success();
    let f = d.wait_frames(2);
    assert_eq!(f[0], json!({"cmd": "reload"}));
    assert!(f[1]["text"].as_str().unwrap().contains("Emma"), "{f:?}");
    assert!(f[1].get("cmd").is_none(), "speak is the cmd-less frame");
}

#[test]
fn an_explicit_voice_beats_the_personas_default() {
    let env = Env::new();
    // No pick yet: the persona's voice applies.
    env.cmd().args(["persona", "aria"]).assert().success();
    env.cmd()
        .arg("voice")
        .assert()
        .success()
        .stdout("af_nova\n");
    let st = stdout_json(&env.cmd().args(["status", "--json"]).output().unwrap());
    assert_eq!(st["voice"], "af_nova", "{st}");
    // The user's pick wins — even bm_george, the core's old default.
    env.cmd().args(["voice", "bm_george"]).assert().success();
    assert!(
        env.config_yaml().contains("kokoro_voice: bm_george"),
        "{}",
        env.config_yaml()
    );
    env.cmd()
        .arg("voice")
        .assert()
        .success()
        .stdout("bm_george\n");
    env.cmd().args(["persona", "atlas"]).assert().success();
    env.cmd()
        .arg("voice")
        .assert()
        .success()
        .stdout("bm_george\n");
    // Clearing the pick hands the voice back to the persona.
    env.cmd()
        .args(["config", "set", "kokoro_voice", ""])
        .assert()
        .success();
    env.cmd()
        .arg("voice")
        .assert()
        .success()
        .stdout("bm_lewis\n");
}

#[test]
fn persona_by_name_file_and_rejects_unknown() {
    let env = Env::new();
    env.cmd()
        .arg("persona")
        .assert()
        .success()
        .stdout("jarvis\n");
    env.cmd().args(["persona", "aria"]).assert().success();
    assert!(env.config_yaml().contains("persona: aria"));
    env.cmd()
        .args(["persona", "ghost"])
        .assert()
        .code(2)
        .stderr(contains("jarvis"));

    let file = env.home.path().join("pirate.md");
    std::fs::write(
        &file,
        "---\nname: pirate\nkokoro_voice: am_adam\n---\nArr.\n",
    )
    .unwrap();
    env.cmd()
        .args(["persona", file.to_str().unwrap()])
        .assert()
        .success()
        .stdout(contains("persona: pirate"));
    assert!(env.root().join("personas/pirate.md").is_file());
    assert!(env.config_yaml().contains("persona: pirate"));
    // Now a known name.
    env.cmd().args(["persona", "pirate"]).assert().success();

    let bad = env.home.path().join("bad.md");
    std::fs::write(&bad, "no front matter").unwrap();
    env.cmd()
        .args(["persona", bad.to_str().unwrap()])
        .assert()
        .code(2);
}

#[test]
fn speed_range() {
    let env = Env::new();
    env.cmd()
        .args(["speed", "1.2x"])
        .assert()
        .success()
        .stdout(contains("1.2×"));
    assert!(env.config_yaml().contains("speed: 1.2"));
    env.cmd().arg("speed").assert().stdout("1.2\n");
    env.cmd()
        .args(["speed", "3"])
        .assert()
        .code(2)
        .stderr(contains("0.5"));
    env.cmd().args(["speed", "fast"]).assert().code(2);
}

#[test]
fn alerts_persist_in_config_and_leave_speak_up_alone() {
    let env = Env::new();
    env.cmd().arg("alerts").assert().stdout("both\n");
    env.cmd().args(["alerts", "voice"]).assert().success();
    env.cmd().arg("alerts").assert().stdout("voice\n");
    let y = env.config_yaml();
    assert!(y.contains("alerts: voice"), "{y}");
    // `notify.*` are the core's "Speak up on" switches: alerts must never
    // flip them (that would silence failures, questions and finals).
    assert!(!y.contains("notify."), "{y}");
    assert!(!env.root().join("cli-settings.json").exists());
    env.cmd().args(["alerts", "notify"]).assert().success();
    assert!(env.config_yaml().contains("alerts: notify"));
    env.cmd()
        .args(["config", "get", "alerts"])
        .assert()
        .stdout(contains("notify"));
    // `off` survives YAML (unquoted it would load as a boolean).
    env.cmd().args(["alerts", "off"]).assert().success();
    env.cmd().arg("alerts").assert().stdout("off\n");
    // The default is not written.
    env.cmd().args(["alerts", "both"]).assert().success();
    assert!(
        !env.config_yaml().contains("alerts:"),
        "{}",
        env.config_yaml()
    );
    env.cmd().args(["alerts", "loud"]).assert().code(2);
    env.cmd()
        .args(["alerts", "--help"])
        .assert()
        .success()
        .stdout(contains("notify  post a notification instead of saying it"))
        .stdout(contains("off     neither"));
}

// ── pause / mute / say ──────────────────────────────────────────────────

#[test]
fn pause_resume_without_daemon_write_config() {
    let env = Env::new();
    env.cmd()
        .arg("pause")
        .assert()
        .success()
        .stdout(contains("paused"));
    assert!(env.config_yaml().contains("muted: true"));
    env.cmd().arg("resume").assert().success();
    assert!(!env.config_yaml().contains("muted: true"));
}

#[test]
fn pause_resume_with_daemon_send_mute_frames() {
    let env = Env::new();
    let d = FakeDaemon::start(&env.socket(), status_reply(json!([])));
    env.cmd().arg("pause").assert().success();
    env.cmd().arg("resume").assert().success();
    assert_eq!(
        d.wait_frames(2),
        vec![
            json!({"cmd": "mute", "source": "cli"}),
            json!({"cmd": "unmute", "source": "cli"})
        ]
    );
    // The daemon persists `muted`; the CLI must not also write it.
    assert!(!env.config_yaml().contains("muted"));
}

#[test]
fn mute_all_and_one_session() {
    let env = Env::new();
    env.cmd().arg("mute").assert().success();
    assert!(env.config_yaml().contains("audio_off: true"));
    env.cmd().arg("unmute").assert().success();
    assert!(!env.config_yaml().contains("audio_off: true"));

    env.cmd()
        .args(["mute", "--session", "abc"])
        .assert()
        .code(1)
        .stderr(contains("heard start"));

    let sessions = json!([{"session_id": "abcdef0123456789", "repo_name": "api",
                           "last_event_ago_s": 4.0, "pinned": false}]);
    let d = FakeDaemon::start(&env.socket(), status_reply(sessions));
    env.cmd()
        .args(["mute", "--session", "abcdef"])
        .assert()
        .success()
        .stdout(contains("abcdef0123456789"));
    env.cmd()
        .args(["unmute", "--session", "api"])
        .assert()
        .success();
    assert_eq!(
        d.wait_frames(2),
        vec![
            json!({"cmd": "mute_session", "session_id": "abcdef0123456789"}),
            json!({"cmd": "unmute_session", "session_id": "abcdef0123456789"})
        ]
    );
}

#[test]
fn say_speaks_through_the_daemon() {
    let env = Env::new();
    env.cmd()
        .args(["say", "hello"])
        .assert()
        .code(1)
        .stderr(contains("not running").and(contains("heard start")));
    let d = FakeDaemon::start(&env.socket(), status_reply(json!([])));
    env.cmd().args(["say", "hello", "world"]).assert().success();
    assert_eq!(d.wait_frames(1), vec![json!({"text": "hello world"})]);
}

// ── agents / history / status ───────────────────────────────────────────

#[test]
fn agents_json_with_and_without_daemon() {
    let env = Env::new();
    let out = env.cmd().args(["agents", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        stdout_json(&out),
        json!({"daemon_running": false, "agents": []})
    );

    let sessions = json!([{"session_id": "s1", "repo_name": "heard", "last_event_ago_s": 2.5, "pinned": true}]);
    let _d = FakeDaemon::start(&env.socket(), status_reply(sessions));
    let out = env.cmd().args(["agents", "--json"]).output().unwrap();
    let v = stdout_json(&out);
    assert_eq!(v["daemon_running"], true);
    assert_eq!(v["agents"][0]["session_id"], "s1");
    assert_eq!(v["agents"][0]["repo_name"], "heard");
    assert_eq!(v["agents"][0]["pinned"], true);
    env.cmd()
        .arg("agents")
        .assert()
        .success()
        .stdout(contains("heard ▸ s1"));
}

#[test]
fn history_filters_and_json() {
    let env = Env::new();
    env.cmd()
        .arg("history")
        .assert()
        .success()
        .stdout(contains("nothing yet"));
    let now = heard_cli::history::now();
    let iso = |t: i64| {
        // Reuse the crate's own formatting inverse via a round trip.
        let days = t.div_euclid(86_400);
        let sod = t.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
            sod / 3600,
            (sod % 3600) / 60,
            sod % 60
        )
    };
    env.write_history(&[
        json!({"ts": iso(now - 7200), "repo_name": "old", "session_id": "s0", "spoken": "Two hours ago."}),
        json!({"type": "feedback", "ref": "x", "text": "nice"}),
        json!({"ts": iso(now - 60), "repo_name": "api", "session_id": "s1", "spoken": "Tests pass."}),
        json!({"ts": iso(now - 5), "repo_name": "", "session_id": "abcdef012345", "spoken": "Needs approval."}),
    ]);
    let out = env.cmd().args(["history", "--json"]).output().unwrap();
    assert_eq!(stdout_json(&out).as_array().unwrap().len(), 3);
    let out = env
        .cmd()
        .args(["history", "--since", "1h", "--json"])
        .output()
        .unwrap();
    let v = stdout_json(&out);
    assert_eq!(v.as_array().unwrap().len(), 2);
    assert_eq!(v[0]["spoken"], "Tests pass.");
    let out = env
        .cmd()
        .args(["history", "-n", "1", "--json"])
        .output()
        .unwrap();
    assert_eq!(stdout_json(&out)[0]["spoken"], "Needs approval.");
    env.cmd().arg("history").assert().success().stdout(
        contains("api")
            .and(contains("Tests pass."))
            .and(contains("abcdef01")),
    );
    env.cmd()
        .args(["history", "--since", "soon"])
        .assert()
        .code(2)
        .stderr(contains("5m"));
}

#[test]
fn status_json_shape() {
    let env = Env::new();
    let out = env.cmd().args(["status", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = stdout_json(&out);
    for k in [
        "version",
        "daemon",
        "mode",
        "persona",
        "voice",
        "speed",
        "alerts",
        "paused",
        "audio_off",
        "model",
        "agents",
        "state_dir",
    ] {
        assert!(v.get(k).is_some(), "missing {k}: {v}");
    }
    assert_eq!(v["daemon"]["running"], false);
    assert_eq!(v["mode"], "co-pilot");
    assert_eq!(v["persona"], "jarvis");
    assert_eq!(v["voice"], "bm_george");
    assert_eq!(v["speed"], 1.05);
    assert_eq!(v["alerts"], "both");
    assert_eq!(v["model"]["installed"], false);

    let sessions =
        json!([{"session_id": "s9", "repo_name": "web", "last_event_ago_s": 1.0, "pinned": false}]);
    let _d = FakeDaemon::start(&env.socket(), status_reply(sessions));
    let v = stdout_json(&env.cmd().args(["status", "--json"]).output().unwrap());
    assert_eq!(v["daemon"]["running"], true);
    assert_eq!(v["agents"][0]["repo_name"], "web");
    env.cmd()
        .arg("status")
        .assert()
        .success()
        .stdout(contains("running").and(contains("co-pilot")));
}

// ── lifecycle and stubs ─────────────────────────────────────────────────

#[test]
fn start_reports_a_daemon_that_is_already_up() {
    // The real lifecycle (start → status → say → stop) is in daemon_e2e.rs.
    let env = Env::new();
    env.cmd()
        .arg("stop")
        .assert()
        .success()
        .stdout(contains("not running"));
    let _d = FakeDaemon::start(&env.socket(), status_reply(json!([])));
    env.cmd()
        .arg("start")
        .assert()
        .success()
        .stdout(contains("already running"));
    // Up but no pid file: stop says how to do it by hand.
    env.cmd()
        .arg("stop")
        .assert()
        .code(1)
        .stderr(contains("pkill"));
}

#[test]
fn install_and_uninstall_round_trip_in_a_temp_home() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let hook = env.root().join("heard-hook");
    std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let run = |args: &[&str]| env.cmd().env("HEARD_HOOK_BIN", &hook).args(args).assert();

    assert!(!env.config_yaml().contains("onboarded"));
    run(&["install", "claude-code"])
        .success()
        .stdout(contains("hooks installed"));
    // The first-run rule: wiring the hooks marks the install onboarded.
    assert!(
        env.config_yaml().contains("onboarded: true"),
        "{}",
        env.config_yaml()
    );
    let settings = env.home.path().join(".claude/settings.json");
    let text = std::fs::read_to_string(&settings).unwrap();
    assert!(text.contains("--edition heard-cli"), "{text}");
    assert!(env
        .home
        .path()
        .join(".claude/commands/heard-mode.md")
        .exists());
    run(&["install", "claude-code"])
        .success()
        .stdout(contains("already installed"));

    run(&["install", "codex"]).success();
    assert!(env.home.path().join(".codex/hooks.json").exists());

    run(&["uninstall", "all"])
        .success()
        .stdout(contains("removed"));
    let text = std::fs::read_to_string(&settings).unwrap();
    assert!(!text.contains("heard-cli"), "{text}");
    assert!(!env
        .home
        .path()
        .join(".claude/commands/heard-mode.md")
        .exists());

    env.cmd().args(["install", "vim"]).assert().code(2);
}

#[test]
fn install_refuses_a_missing_hook_binary() {
    let env = Env::new();
    env.cmd()
        .env("HEARD_HOOK_BIN", env.root().join("nope"))
        .args(["install", "claude-code"])
        .assert()
        .code(1)
        .stderr(contains("heard-hook was not found"));
    assert!(!env.home.path().join(".claude/settings.json").exists());
}

#[test]
fn daemon_rejects_unknown_test_switches() {
    let env = Env::new();
    env.cmd()
        .args(["daemon", "--foreground", "--speech", "loud"])
        .assert()
        .code(2)
        .stderr(contains("queued or log"));
    env.cmd()
        .args(["daemon", "--foreground", "--tts", "piper"])
        .assert()
        .code(2);
    env.cmd()
        .args(["daemon", "--foreground", "--notifier", "growl"])
        .assert()
        .code(2);
}

#[test]
fn completions_for_each_shell() {
    let env = Env::new();
    for sh in ["zsh", "bash", "fish"] {
        env.cmd()
            .args(["completions", sh])
            .assert()
            .success()
            .stdout(contains("heard").and(contains("mode")));
    }
    env.cmd().args(["completions", "tcsh"]).assert().code(2);
}

#[test]
fn setup_without_a_terminal_prints_the_commands() {
    let env = Env::new();
    env.cmd()
        .arg("setup")
        .env("PATH", "/usr/bin:/bin")
        .assert()
        .success()
        .stdout(
            contains("heard models download")
                .and(contains("heard mode"))
                .and(contains("heard install")),
        );
    assert!(
        env.config_yaml().contains("onboarded: true"),
        "{}",
        env.config_yaml()
    );
}

// ── config ──────────────────────────────────────────────────────────────

#[test]
fn config_get_set_list_path() {
    let env = Env::new();
    env.cmd()
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(format!("{}\n", env.root().join("config.yaml").display()));
    env.cmd()
        .args(["config", "get", "speed"])
        .assert()
        .stdout("1.05\n");
    env.cmd()
        .args(["config", "set", "narrate_tools", "off"])
        .assert()
        .success();
    env.cmd()
        .args(["config", "get", "narrate_tools", "--json"])
        .assert()
        .stdout("false\n");
    env.cmd()
        .args(["config", "set", "narrate_tools", "maybe"])
        .assert()
        .code(2);
    env.cmd()
        .args(["config", "set", "no_such_key", "1"])
        .assert()
        .code(2)
        .stderr(contains("heard config list"));
    env.cmd()
        .args(["config", "set", "mode", "focus"])
        .assert()
        .success();
    assert!(env.config_yaml().contains("narration_volume: 0"));
    env.cmd().args(["config", "get", "nope"]).assert().code(1);

    // Secrets are redacted in list (a fake value, in a temp dir).
    env.cmd()
        .args(["config", "set", "elevenlabs_api_key", "fake-value-wxyz"])
        .assert()
        .success();
    let v = stdout_json(
        &env.cmd()
            .args(["config", "list", "--json"])
            .output()
            .unwrap(),
    );
    let red = v["elevenlabs_api_key"].as_str().unwrap();
    assert!(
        red.starts_with("<redacted") && red.ends_with("wxyz>"),
        "{red}"
    );
    assert_eq!(v["alerts"], "both");
    env.cmd()
        .args(["config", "list", "--show-secrets"])
        .assert()
        .stdout(contains("fake-value-wxyz"));
}

// ── models ──────────────────────────────────────────────────────────────

struct Models {
    server: FileServer,
    manifest: std::path::PathBuf,
    a: Vec<u8>,
    b: Vec<u8>,
}

fn models_fixture(env: &Env, honor_range: bool, bad_hash: bool) -> Models {
    let a = fake_bytes(300_000, 7);
    let b = fake_bytes(40_000, 11);
    let mut files = HashMap::new();
    files.insert("model.onnx".to_string(), a.clone());
    files.insert("voices.bin".to_string(), b.clone());
    let server = FileServer::start(files, honor_range);
    let sha_a = if bad_hash {
        "0".repeat(64)
    } else {
        sha256_hex(&a)
    };
    let manifest = env.home.path().join("manifest.json");
    std::fs::write(
        &manifest,
        json!([
            {"name": "model.onnx", "size": a.len(), "sha256": sha_a},
            {"name": "voices.bin", "size": b.len(), "sha256": sha256_hex(&b)},
        ])
        .to_string(),
    )
    .unwrap();
    Models {
        server,
        manifest,
        a,
        b,
    }
}

fn models_cmd(env: &Env, m: &Models) -> assert_cmd::Command {
    let mut c = env.cmd();
    c.env("HEARD_CLI_MODELS_URL", &m.server.base_url)
        .env("HEARD_CLI_MODELS_MANIFEST", &m.manifest);
    c
}

#[test]
fn models_status_download_verify_remove() {
    let env = Env::new();
    let m = models_fixture(&env, true, false);
    let out = models_cmd(&env, &m)
        .args(["models", "status", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v = stdout_json(&out);
    assert_eq!(v["installed"], false);
    assert_eq!(v["files"][0]["present"], false);

    models_cmd(&env, &m)
        .args(["models", "download"])
        .assert()
        .success()
        .stdout(contains("model.onnx: downloaded").and(contains("verified")));
    let dir = env.root().join("models");
    assert_eq!(std::fs::read(dir.join("model.onnx")).unwrap(), m.a);
    assert_eq!(std::fs::read(dir.join("voices.bin")).unwrap(), m.b);
    assert!(!dir.join("model.onnx.part").exists());

    let v = stdout_json(
        &models_cmd(&env, &m)
            .args(["models", "status", "--verify", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(v["installed"], true);
    assert_eq!(v["verified"], true);

    // A second run downloads nothing.
    let before = m.server.ranges.lock().unwrap().len();
    models_cmd(&env, &m)
        .args(["models", "download"])
        .assert()
        .success()
        .stdout(contains("already installed"));
    assert_eq!(m.server.ranges.lock().unwrap().len(), before);

    models_cmd(&env, &m)
        .args(["models", "remove"])
        .assert()
        .success()
        .stdout(contains("removed"));
    assert!(!dir.join("model.onnx").exists());
}

#[test]
fn models_download_resumes_a_partial_file() {
    let env = Env::new();
    let m = models_fixture(&env, true, false);
    let dir = env.root().join("models");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("model.onnx.part"), &m.a[..123_456]).unwrap();
    models_cmd(&env, &m)
        .args(["models", "download"])
        .assert()
        .success()
        .stdout(contains("resumed"));
    assert_eq!(m.server.ranges.lock().unwrap()[0], "bytes=123456-");
    assert_eq!(std::fs::read(dir.join("model.onnx")).unwrap(), m.a);
}

#[test]
fn models_download_restarts_when_the_server_ignores_range() {
    let env = Env::new();
    let m = models_fixture(&env, false, false);
    let dir = env.root().join("models");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("model.onnx.part"), &m.a[..1000]).unwrap();
    models_cmd(&env, &m)
        .args(["models", "download"])
        .assert()
        .success();
    assert_eq!(std::fs::read(dir.join("model.onnx")).unwrap(), m.a);
}

#[test]
fn models_download_rejects_a_bad_hash() {
    let env = Env::new();
    let m = models_fixture(&env, true, true);
    models_cmd(&env, &m)
        .args(["models", "download"])
        .assert()
        .code(1)
        .stderr(contains("checksum mismatch").and(contains("fix:")));
    let dir = env.root().join("models");
    assert!(!dir.join("model.onnx").exists());
    assert!(
        !dir.join("model.onnx.part").exists(),
        "a bad file must not linger"
    );
}

#[test]
fn models_download_reports_an_unreachable_server() {
    let env = Env::new();
    let m = models_fixture(&env, true, false);
    let mut c = env.cmd();
    c.env("HEARD_CLI_MODELS_URL", "http://127.0.0.1:9/nothing/")
        .env("HEARD_CLI_MODELS_MANIFEST", &m.manifest)
        .args(["models", "download"])
        .assert()
        .code(1)
        .stderr(contains("resumes"));
}

#[test]
fn pinned_manifest_matches_heard_tts_sizes() {
    let p = heard_cli::models::pinned();
    assert_eq!(p[0].name, "kokoro-v1.0.onnx");
    assert_eq!(p[0].size, 325_532_387);
    assert_eq!(p[1].name, "voices-v1.0.bin");
    assert_eq!(p[1].size, 28_214_398);
    assert!(p.iter().all(|f| f.sha256.len() == 64));
}

// ── doctor ──────────────────────────────────────────────────────────────

#[test]
fn doctor_json_reports_failures_with_fixes() {
    let env = Env::new();
    let out = env
        .cmd()
        .args(["doctor", "--json"])
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v = stdout_json(&out);
    assert_eq!(v["ok"], false);
    let checks = v["checks"].as_array().unwrap();
    let get = |n: &str| checks.iter().find(|c| c["name"] == n).cloned().unwrap();
    assert_eq!(get("model")["status"], "fail");
    assert!(get("model")["fix"]
        .as_str()
        .unwrap()
        .contains("heard models download"));
    assert_eq!(get("daemon")["status"], "warn");
    assert_eq!(get("config")["status"], "ok");
    assert!(checks.iter().any(|c| c["name"] == "hooks:claude-code"));
    if cfg!(target_os = "macos") {
        assert_eq!(get("afplay")["status"], "ok");
    }
    for c in checks {
        if c["status"] != "ok" {
            assert!(c["fix"].is_string(), "no fix for {c}");
        }
    }
}

#[test]
fn doctor_flags_a_shadowing_heard_on_path() {
    let env = Env::new();
    let bin = env.home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("heard");
    std::fs::write(&fake, "#!/bin/sh\necho old heard\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = env
        .cmd()
        .args(["doctor", "--json"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .output()
        .unwrap();
    let v = stdout_json(&out);
    let path = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "path")
        .cloned()
        .unwrap();
    assert_eq!(path["status"], "fail", "{path}");
    assert!(path["detail"]
        .as_str()
        .unwrap()
        .contains(fake.to_str().unwrap()));
}

#[test]
fn doctor_passes_the_model_check_with_verified_files() {
    let env = Env::new();
    let m = models_fixture(&env, true, false);
    models_cmd(&env, &m)
        .args(["models", "download"])
        .assert()
        .success();
    let v = stdout_json(
        &models_cmd(&env, &m)
            .args(["doctor", "--json"])
            .output()
            .unwrap(),
    );
    let model = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "model")
        .cloned()
        .unwrap();
    assert_eq!(model["status"], "ok", "{model}");
}

#[test]
fn no_color_output_has_no_escapes() {
    let env = Env::new();
    let out = env.cmd().arg("status").output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains('\x1b'));
    let out = env.cmd().args(["mode", "bogus"]).output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stderr).contains('\x1b'));
}

// ── --porcelain (the hook's contract) ───────────────────────────────────

fn one_plain_line(out: &std::process::Output) -> String {
    let s = String::from_utf8(out.stdout.clone()).unwrap();
    assert!(
        out.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(s.matches('\n').count(), 1, "not one line: {s:?}");
    assert!(!s.contains('\x1b'));
    s.trim_end().to_string()
}

#[test]
fn porcelain_prints_exactly_one_line_for_every_forwarded_verb() {
    let env = Env::new();
    let cases: Vec<(Vec<&str>, i32, &str)> = vec![
        (vec!["mode", "--porcelain", "focus"], 0, "mode: focus"),
        (vec!["mode", "--porcelain"], 0, "mode: focus"),
        (
            vec!["mode", "--porcelain", "loud"],
            2,
            "error: unknown mode",
        ),
        (
            vec!["voice", "--porcelain", "af_heart"],
            0,
            "voice: af_heart",
        ),
        (vec!["voice", "--porcelain"], 0, "voice: af_heart"),
        (vec!["persona", "--porcelain", "aria"], 0, "persona: aria"),
        (vec!["speed", "--porcelain", "1.1"], 0, "speed: 1.1×"),
        (vec!["alerts", "--porcelain", "notify"], 0, "alerts: notify"),
        (vec!["alerts", "--porcelain"], 0, "alerts: notify"),
        (vec!["pause", "--porcelain"], 0, "paused"),
        (vec!["resume", "--porcelain"], 0, "resumed"),
        (vec!["mute", "--porcelain"], 0, "muted"),
        (vec!["unmute", "--porcelain"], 0, "unmuted"),
        (
            vec!["status", "--porcelain"],
            0,
            "daemon stopped · focus · aria (af_heart)",
        ),
        (
            vec!["say", "--porcelain", "hi", "there"],
            1,
            "error: cannot speak",
        ),
        (vec!["mode", "--porcelain", "--bogus"], 2, "error:"),
    ];
    for (args, code, prefix) in cases {
        let out = env.cmd().args(&args).output().unwrap();
        let line = one_plain_line(&out);
        assert_eq!(out.status.code(), Some(code), "{args:?}: {line}");
        assert!(line.starts_with(prefix), "{args:?}: {line:?}");
    }
}

#[test]
fn porcelain_say_and_persona_join_trailing_words() {
    let env = Env::new();
    let d = FakeDaemon::start(&env.socket(), status_reply(json!([])));
    let out = env
        .cmd()
        .args(["say", "--porcelain", "rm", "-rf", "build", "done"])
        .output()
        .unwrap();
    assert_eq!(one_plain_line(&out), "said: rm -rf build done");
    // --porcelain after the text also works.
    let out = env
        .cmd()
        .args(["say", "hello", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(one_plain_line(&out), "said: hello");
    assert_eq!(
        d.wait_frames(2),
        vec![
            json!({"text": "rm -rf build done"}),
            json!({"text": "hello"})
        ]
    );

    let dir = env.home.path().join("My Personas");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("sea dog.md");
    std::fs::write(&file, "---\nname: x\n---\nArr.\n").unwrap();
    // A file name with spaces is a usage error only because of its name;
    // the words reach the persona code joined.
    let parts: Vec<String> = file
        .to_str()
        .unwrap()
        .split(' ')
        .map(String::from)
        .collect();
    let mut args = vec!["persona".to_string(), "--porcelain".to_string()];
    args.extend(parts);
    let out = env.cmd().args(&args).output().unwrap();
    let line = one_plain_line(&out);
    assert!(line.contains("sea dog"), "{line}");

    let file = dir.join("seadog.md");
    std::fs::write(&file, "---\nname: x\n---\nArr.\n").unwrap();
    let parts: Vec<String> = file
        .to_str()
        .unwrap()
        .split(' ')
        .map(String::from)
        .collect();
    let mut args = vec!["persona".to_string(), "--porcelain".to_string()];
    args.extend(parts);
    let out = env.cmd().args(&args).output().unwrap();
    assert_eq!(one_plain_line(&out), "persona: seadog");
}
