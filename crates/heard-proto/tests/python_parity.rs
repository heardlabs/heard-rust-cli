//! Round-trip every payload against JSON captured from the real Python.
//!
//! The literals below are **not written by hand**. They were produced by
//! importing `engine/heard/client.py` under `python3 -B` with its `socket`
//! module replaced by a recorder, calling each public helper, and printing
//! the exact bytes it passed to `sendall`. `_session_from_data` and
//! `_terminal_binding_from_env` were captured the same way. Anything marked
//! "daemon-side" was instead taken verbatim from the `json.dumps` literal in
//! `daemon.py`'s `_handle`.
//!
//! Each case asserts both directions:
//!
//! * Python JSON → Rust type produces the expected value, and
//! * Rust type → JSON is *value-identical* to the Python JSON.
//!
//! Value-identical, not byte-identical: `json.dumps` defaults to `", "` /
//! `": "` separators while `serde_json` is compact. The keys, their
//! optionality and their types are what the daemon reads, and those must
//! match exactly.

use heard_proto::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

/// Assert `python` parses to `value`, and that `value` re-serialises to the
/// same JSON *value* `python` denotes.
#[track_caller]
fn round_trip<'a, T>(python: &'a str, value: &T)
where
    T: Serialize + PartialEq + std::fmt::Debug + serde::Deserialize<'a>,
{
    let parsed: T = serde_json::from_str(python).expect("parse python payload");
    assert_eq!(&parsed, value, "parsing {python}");
    let ours = serde_json::to_value(value).expect("serialise");
    let theirs: serde_json::Value = serde_json::from_str(python).expect("python as value");
    assert_eq!(ours, theirs, "serialising back to {python}");
}

/// Parse-only, for the daemon's replies (we never emit them).
#[track_caller]
fn parses_to<T: DeserializeOwned + PartialEq + std::fmt::Debug + Serialize>(
    python: &str,
    value: &T,
) {
    round_trip(python, value);
}

// --- fire-and-forget commands ----------------------------------------------

#[test]
fn ping() {
    round_trip(r#"{"cmd": "ping"}"#, &Request::Ping);
}

#[test]
fn status_request() {
    round_trip(r#"{"cmd": "status"}"#, &Request::Status);
}

#[test]
fn pin_and_unpin() {
    round_trip(
        r#"{"cmd": "pin", "session_id": "s1"}"#,
        &Request::Pin(Pin { session_id: "s1" }),
    );
    round_trip(r#"{"cmd": "unpin"}"#, &Request::Unpin);
}

#[test]
fn reload_and_stop() {
    round_trip(r#"{"cmd": "reload"}"#, &Request::Reload);
    round_trip(r#"{"cmd": "stop"}"#, &Request::Stop);
}

#[test]
fn mute_and_unmute() {
    // client.mute("cli")
    round_trip(
        r#"{"cmd": "mute", "source": "cli"}"#,
        &Request::Mute(Mute { source: "cli" }),
    );
    // client.mute() — the default source is "client", not "socket".
    round_trip(
        r#"{"cmd": "mute", "source": "client"}"#,
        &Request::Mute(Mute { source: "client" }),
    );
    round_trip(
        r#"{"cmd": "unmute", "source": "menu"}"#,
        &Request::Unmute(Unmute { source: "menu" }),
    );
}

#[test]
fn resume_intent() {
    round_trip(
        r#"{"cmd": "resume_intent", "text": "keep going"}"#,
        &Request::ResumeIntent(ResumeIntent { text: "keep going" }),
    );
}

#[test]
fn feedback() {
    round_trip(
        r#"{"cmd": "feedback", "text": "too chatty", "source": "cli"}"#,
        &Request::Feedback(Feedback {
            text: "too chatty",
            source: "cli",
        }),
    );
}

#[test]
fn report_defect() {
    round_trip(
        r#"{"cmd": "report_defect", "category": "wrong_voice", "note": "note", "source": "menu"}"#,
        &Request::ReportDefect(ReportDefect {
            category: "wrong_voice",
            note: "note",
            source: "menu",
        }),
    );
}

// --- request/response commands ---------------------------------------------

#[test]
fn session_mute_commands() {
    round_trip(
        r#"{"cmd": "mute_session", "session_id": "sess-1"}"#,
        &Request::MuteSession(MuteSession {
            session_id: "sess-1",
        }),
    );
    round_trip(
        r#"{"cmd": "unmute_session", "session_id": "sess-1"}"#,
        &Request::UnmuteSession(UnmuteSession {
            session_id: "sess-1",
        }),
    );
}

// --- the cmd-less speak fall-through ---------------------------------------

#[test]
fn speak_has_no_cmd_key() {
    // client.speak("hello there")
    round_trip(
        r#"{"text": "hello there"}"#,
        &Speak {
            text: "hello there",
            priority: false,
        },
    );
    round_trip(
        r#"{"text": "urgent", "priority": true}"#,
        &Speak {
            text: "urgent",
            priority: true,
        },
    );
}

#[test]
fn message_prefers_a_known_command_and_falls_through_to_speak() {
    let m: Message = serde_json::from_str(r#"{"cmd": "ping"}"#).unwrap();
    assert_eq!(m, Message::Command(Request::Ping));

    let m: Message = serde_json::from_str(r#"{"text": "hello there"}"#).unwrap();
    assert_eq!(
        m,
        Message::Speak(Speak {
            text: "hello there",
            priority: false
        })
    );

    // daemon.py's `_handle` runs off the end of its `if cmd == …` chain for
    // an unrecognised cmd and speaks `req.get("text") or ""`. The untagged
    // fall-through reproduces that.
    let m: Message = serde_json::from_str(r#"{"cmd": "nonsense", "text": "hi"}"#).unwrap();
    assert_eq!(
        m,
        Message::Speak(Speak {
            text: "hi",
            priority: false
        })
    );
}

// --- events ----------------------------------------------------------------

#[test]
fn narration_event_full() {
    // client.send_event(kind="tool_pre", neutral=…, tag=…, ctx=…, session=…)
    let python = r#"{"cmd": "event", "kind": "tool_pre", "neutral": "Editing auth.py", "tag": "edit", "ctx": {"length": 12}, "session": {"id": "s1", "cwd": "/tmp/p", "transcript_path": "/tmp/t.jsonl"}}"#;
    let mut ctx = serde_json::Map::new();
    ctx.insert("length".into(), json!(12));
    round_trip(
        python,
        &Request::Event(Event::Narration(NarrationEvent {
            kind: kind::TOOL_PRE,
            neutral: "Editing auth.py",
            tag: "edit",
            ctx,
            session: Session::Known(SessionInfo {
                id: "s1",
                cwd: Some("/tmp/p"),
                transcript_path: Some("/tmp/t.jsonl"),
                binding: None,
            }),
        })),
    );
}

#[test]
fn narration_event_with_empty_ctx_and_session() {
    // client.send_event(kind="final", neutral="done", tag="final_short")
    // — `ctx or {}` / `session or {}`: both keys are PRESENT and empty.
    let python = r#"{"cmd": "event", "kind": "final", "neutral": "done", "tag": "final_short", "ctx": {}, "session": {}}"#;
    round_trip(
        python,
        &Request::Event(Event::Narration(NarrationEvent {
            kind: kind::FINAL,
            neutral: "done",
            tag: "final_short",
            ctx: serde_json::Map::new(),
            session: Session::Empty(EmptySession {}),
        })),
    );
}

#[test]
fn session_carries_null_cwd_when_the_payload_had_none() {
    // _session_from_data({}) — id defaults to "default" and the two path
    // keys are present as null.
    let python = r#"{"id": "default", "cwd": null, "transcript_path": null}"#;
    round_trip(
        python,
        &Session::Known(SessionInfo {
            id: "default",
            cwd: None,
            transcript_path: None,
            binding: None,
        }),
    );
}

#[test]
fn binding_vscode() {
    // _terminal_binding_from_env({"TERM_PROGRAM": "vscode",
    //                             "__CFBundleIdentifier": "com.microsoft.VSCode"})
    let python = r#"{"host_name": "VS Code", "host_type": "editor_terminal", "provenance": "env_terminal_binding", "confidence": 0.9, "pid": 11973, "process_started_at": null}"#;
    round_trip(
        python,
        &Binding {
            host_name: "VS Code",
            host_type: "editor_terminal",
            provenance: "env_terminal_binding",
            confidence: 0.9,
            pid: 11973,
            process_started_at: None,
            herdr_pane_id: None,
            herdr_tab_id: None,
            herdr_workspace_id: None,
            herdr_session: None,
            herdr_socket_path: None,
            herdr_bin_path: None,
        },
    );
}

#[test]
fn binding_herdr_adds_six_keys() {
    // _terminal_binding_from_env with HERDR_ENV=1 and the six HERDR_* vars.
    let python = r#"{"host_name": "Herdr", "host_type": "terminal", "provenance": "env_terminal_binding", "confidence": 0.9, "pid": 11973, "process_started_at": null, "herdr_pane_id": "p1", "herdr_tab_id": "t1", "herdr_workspace_id": "w1", "herdr_session": "", "herdr_socket_path": "/s", "herdr_bin_path": "/b"}"#;
    round_trip(
        python,
        &Binding {
            host_name: "Herdr",
            host_type: "terminal",
            provenance: "env_terminal_binding",
            confidence: 0.9,
            pid: 11973,
            process_started_at: None,
            herdr_pane_id: Some("p1"),
            herdr_tab_id: Some("t1"),
            herdr_workspace_id: Some("w1"),
            herdr_session: Some(""),
            herdr_socket_path: Some("/s"),
            herdr_bin_path: Some("/b"),
        },
    );
}

#[test]
fn session_with_binding() {
    // _session_from_data({"session_id": "s1", …}) inside VS Code.
    let python = r#"{"id": "s1", "cwd": "/p", "transcript_path": "/t.jsonl", "binding": {"host_name": "VS Code", "host_type": "editor_terminal", "provenance": "env_terminal_binding", "confidence": 0.9, "pid": 11973, "process_started_at": null}}"#;
    let parsed: Session = serde_json::from_str(python).unwrap();
    let Session::Known(info) = &parsed else {
        panic!("expected a known session, got {parsed:?}");
    };
    assert_eq!(info.id, "s1");
    assert_eq!(info.binding.as_ref().unwrap().host_name, "VS Code");
    assert_eq!(
        serde_json::to_value(&parsed).unwrap(),
        serde_json::from_str::<serde_json::Value>(python).unwrap()
    );
}

#[test]
fn health_probe() {
    // hook.py --health-probe, via client.request(...)
    let python = r#"{"cmd": "event", "kind": "health_probe", "agent": "claude-code", "nonce": "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"}"#;
    round_trip(
        python,
        &Request::Event(Event::HealthProbe(HealthProbe::new(
            Agent::ClaudeCode,
            "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
        ))),
    );
}

const HOOK_PAYLOAD: &str = r#"{"hook_event_name":"PreToolUse","session_id":"abc","cwd":"/repo","tool_name":"Edit","tool_input":{"file_path":"/repo/a.py"}}"#;

#[test]
fn hook_frame_forwards_the_payload_verbatim() {
    let payload = serde_json::value::RawValue::from_string(HOOK_PAYLOAD.to_string()).unwrap();
    let wire = serde_json::to_string(&HookFrame::new(Agent::ClaudeCode, &payload, None)).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&wire).unwrap(),
        json!({
            "cmd": "hook",
            "agent": "claude-code",
            "payload": {
                "hook_event_name": "PreToolUse",
                "session_id": "abc",
                "cwd": "/repo",
                "tool_name": "Edit",
                "tool_input": {"file_path": "/repo/a.py"},
            },
        })
    );
    // The payload's bytes survive untouched, not just its value.
    assert!(wire.contains(HOOK_PAYLOAD), "payload was reshaped: {wire}");
    // `hook_event_name` lives in the payload and NOWHERE else — a second
    // copy on the frame could only drift from the first.
    assert_eq!(
        wire.matches("hook_event_name").count(),
        1,
        "hook_event_name is duplicated: {wire}"
    );
    // No binding evidence → no `binding` key at all, not a null.
    assert!(!wire.contains("binding"), "binding was emitted: {wire}");
}

#[test]
fn hook_frame_matches_the_hook_command() {
    // The hook writes a HookFrame; the daemon reads a Request::Hook. These
    // must be the same message — this test is the seam between the two lanes.
    let payload = serde_json::value::RawValue::from_string(HOOK_PAYLOAD.to_string()).unwrap();
    let env = iterm_env();
    let wire = serde_json::to_string(&HookFrame::new(Agent::Codex, &payload, env.binding()))
        .expect("serialise");

    let back: Request = serde_json::from_str(&wire).expect("daemon parses the hook frame");
    assert_eq!(
        back,
        Request::Hook(Hook {
            agent: Agent::Codex,
            payload: serde_json::from_str(HOOK_PAYLOAD).unwrap(),
            binding: Some(Box::new(env.binding().unwrap())),
        })
    );
}

#[test]
fn hook_carries_a_payload_with_no_event_name_unchanged() {
    // Whether the payload names an event is the daemon's problem now; the
    // hook forwards it either way.
    let payload = serde_json::value::RawValue::from_string(r#"{"session_id":"x"}"#.into()).unwrap();
    let wire = serde_json::to_string(&HookFrame::new(Agent::ClaudeCode, &payload, None)).unwrap();
    let back: Request = serde_json::from_str(&wire).unwrap();
    let Request::Hook(got) = back else {
        panic!("expected a hook command");
    };
    assert_eq!(got.payload, json!({"session_id": "x"}));
    assert!(got.binding.is_none());
}

#[test]
fn hook_frame_carries_the_binding_at_the_top_level() {
    let payload = serde_json::value::RawValue::from_string(HOOK_PAYLOAD.to_string()).unwrap();
    let env = iterm_env();
    let wire =
        serde_json::to_string(&HookFrame::new(Agent::ClaudeCode, &payload, env.binding())).unwrap();
    let v: serde_json::Value = serde_json::from_str(&wire).unwrap();

    assert_eq!(v["binding"]["host_name"], "iTerm");
    assert_eq!(v["binding"]["provenance"], "env_terminal_binding");
    // …and the payload is still exactly the agent's bytes.
    assert!(v["payload"].get("binding").is_none());
    assert!(wire.contains(HOOK_PAYLOAD), "payload was reshaped: {wire}");
}

// --- _terminal_binding_from_env, case by case -------------------------------

/// An environment built from a list of pairs, plus a pinned parent pid so
/// the captured Python literals are stable fixtures.
fn env_of(pairs: &[(&str, &str)]) -> HookEnv {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    HookEnv::from_lookup(
        move |key| {
            owned
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.to_string())
        },
        4242,
    )
}

fn iterm_env() -> HookEnv {
    env_of(&[
        ("TERM_PROGRAM", "iTerm.app"),
        ("ITERM_SESSION_ID", "w0t0p0:ABC"),
    ])
}

/// Assert the binding this env yields is exactly what Python printed.
///
/// Every `python` literal here came from running
/// `client._terminal_binding_from_env(<the same env>)` under `python3 -B`
/// with `os.getppid` pinned to 4242.
#[track_caller]
fn binding_is(pairs: &[(&str, &str)], python: &str) {
    let env = env_of(pairs);
    let got = env.binding();
    let expected: serde_json::Value = serde_json::from_str(python).expect("python literal");
    assert_eq!(
        serde_json::to_value(&got).unwrap(),
        expected,
        "for env {pairs:?}"
    );
}

#[test]
fn an_empty_environment_names_no_host() {
    binding_is(&[], "null");
}

#[test]
fn iterm_by_term_program_or_by_session_id() {
    let iterm = r#"{"host_name": "iTerm", "host_type": "terminal", "provenance": "env_terminal_binding", "confidence": 0.9, "pid": 4242, "process_started_at": null}"#;
    binding_is(
        &[
            ("TERM_PROGRAM", "iTerm.app"),
            ("ITERM_SESSION_ID", "w0t0p0:ABC"),
        ],
        iterm,
    );
    binding_is(&[("ITERM_SESSION_ID", "w0t0p0:ABC")], iterm);
}

#[test]
fn apple_terminal() {
    let terminal = r#"{"host_name": "Terminal", "host_type": "terminal", "provenance": "env_terminal_binding", "confidence": 0.9, "pid": 4242, "process_started_at": null}"#;
    binding_is(&[("TERM_PROGRAM", "Apple_Terminal")], terminal);
    // TERM_PROGRAM is stripped before comparison.
    binding_is(&[("TERM_PROGRAM", "  Apple_Terminal  ")], terminal);
    // A Terminal.app BUNDLE ID alone is NOT evidence — Python returns None,
    // because `com.apple.Terminal` is not in the editor set and nothing else
    // matches. Surprising, and load-bearing.
    binding_is(&[("__CFBundleIdentifier", "com.apple.Terminal")], "null");
}

#[test]
fn ghostty() {
    binding_is(
        &[
            (
                "GHOSTTY_RESOURCES_DIR",
                "/Applications/Ghostty.app/Contents/Resources",
            ),
            ("TERM_PROGRAM", "ghostty"),
        ],
        r#"{"host_name": "Ghostty", "host_type": "terminal", "provenance": "env_terminal_binding", "confidence": 0.9, "pid": 4242, "process_started_at": null}"#,
    );
    // An EMPTY value is not evidence (Python takes `bool(env.get(...))`).
    binding_is(&[("GHOSTTY_RESOURCES_DIR", "")], "null");
}

#[test]
fn the_vscode_family() {
    let vscode = r#"{"host_name": "VS Code", "host_type": "editor_terminal", "provenance": "env_terminal_binding", "confidence": 0.9, "pid": 4242, "process_started_at": null}"#;
    binding_is(
        &[
            ("TERM_PROGRAM", "vscode"),
            ("__CFBundleIdentifier", "com.microsoft.VSCode"),
        ],
        vscode,
    );
    // The extension host spawns a session with NO shell, so a
    // bundle id on its own has to be enough.
    binding_is(&[("__CFBundleIdentifier", "com.microsoft.VSCode")], vscode);

    let cursor = r#"{"host_name": "Cursor", "host_type": "editor_terminal", "provenance": "env_terminal_binding", "confidence": 0.9, "pid": 4242, "process_started_at": null}"#;
    binding_is(
        &[
            ("TERM_PROGRAM", "vscode"),
            ("__CFBundleIdentifier", "com.todesktop.230313mzl4w4u92"),
        ],
        cursor,
    );
    binding_is(
        &[
            ("TERM_PROGRAM", "vscode"),
            ("CURSOR_TRACE_ID", "t1"),
            ("__CFBundleIdentifier", "com.microsoft.VSCode"),
        ],
        cursor,
    );
    binding_is(
        &[
            ("TERM_PROGRAM", "vscode"),
            ("__CFBundleIdentifier", "com.exafunction.windsurf"),
        ],
        r#"{"host_name": "Windsurf", "host_type": "editor_terminal", "provenance": "env_terminal_binding", "confidence": 0.9, "pid": 4242, "process_started_at": null}"#,
    );
}

#[test]
fn herdr_owns_the_pty_when_it_says_so() {
    // HERDR_ENV=1 plus a pane id beats every weaker signal —
    // note TERM_PROGRAM says Apple_Terminal here and loses.
    binding_is(
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
        r#"{"host_name": "Herdr", "host_type": "terminal", "provenance": "env_terminal_binding", "confidence": 0.9, "pid": 4242, "process_started_at": null, "herdr_pane_id": "p1", "herdr_tab_id": "t1", "herdr_workspace_id": "w1", "herdr_session": "", "herdr_socket_path": "/s", "herdr_bin_path": "/b"}"#,
    );
    // HERDR_ENV=1 with no pane id: the LABEL comes from the terminal
    // evidence, but the six keys ride along empty anyway
    // (`binding.update(herdr)` is unconditional).
    binding_is(
        &[("HERDR_ENV", "1"), ("TERM_PROGRAM", "iTerm.app")],
        r#"{"host_name": "iTerm", "host_type": "terminal", "provenance": "env_terminal_binding", "confidence": 0.9, "pid": 4242, "process_started_at": null, "herdr_pane_id": "", "herdr_tab_id": "", "herdr_workspace_id": "", "herdr_session": "", "herdr_socket_path": "", "herdr_bin_path": ""}"#,
    );
    // A stale pane id inherited by something Herdr did NOT launch claims
    // nothing, and contributes no keys.
    binding_is(
        &[("HERDR_PANE_ID", "p1"), ("TERM_PROGRAM", "Apple_Terminal")],
        r#"{"host_name": "Terminal", "host_type": "terminal", "provenance": "env_terminal_binding", "confidence": 0.9, "pid": 4242, "process_started_at": null}"#,
    );
}

#[test]
fn agent_names_match_the_hook_argv() {
    assert_eq!(Agent::from_argv("claude-code"), Some(Agent::ClaudeCode));
    assert_eq!(Agent::from_argv("codex"), Some(Agent::Codex));
    assert_eq!(Agent::from_argv("gemini"), None);
    assert_eq!(Agent::ClaudeCode.as_str(), "claude-code");
    assert_eq!(Agent::Codex.as_str(), "codex");
}

// --- daemon replies --------------------------------------------------------

#[test]
fn status_reply() {
    // Shape taken from daemon.py `_handle`'s `cmd == "status"` payload.
    let python = r#"{"alive": true, "backend": "KokoroTTS", "persona": "jarvis",
      "narrate_tools": true, "muted": false, "last_error": null,
      "account_usage": null, "speaking": false, "queued": 0,
      "active_sessions": [], "router_mode": "solo", "agent_states": {},
      "langgraph_runs": {}, "recap": "", "mission_agents": [],
      "pending_count": 0, "awaiting_resume_intent": false,
      "pending_update": null}"#;
    let parsed: StatusResponse = serde_json::from_str(python).unwrap();
    assert!(parsed.alive);
    assert_eq!(parsed.backend, "KokoroTTS");
    assert_eq!(parsed.router_mode, "solo");
    assert!(parsed.pending_update.is_none());
    assert_eq!(
        serde_json::to_value(&parsed).unwrap(),
        serde_json::from_str::<serde_json::Value>(python).unwrap()
    );
}

#[test]
fn status_reply_with_a_pending_update() {
    let python = r#"{"alive": true, "backend": "NullTTS", "persona": "aria",
      "narrate_tools": false, "muted": true, "last_error": "tts timeout",
      "account_usage": {"plan": "pro"}, "speaking": true, "queued": 3,
      "active_sessions": ["s1"], "router_mode": "swarm",
      "agent_states": {"s1": {}}, "langgraph_runs": {}, "recap": "building",
      "mission_agents": [{"id": "s1"}], "pending_count": 2,
      "awaiting_resume_intent": true,
      "pending_update": {"version": "1.2.40", "tag": "v1.2.40",
        "url": "https://example.invalid/r", "zip_url": "https://example.invalid/r.zip",
        "zip_size": 1234}}"#;
    let parsed: StatusResponse = serde_json::from_str(python).unwrap();
    assert_eq!(parsed.pending_update.as_ref().unwrap().version, "1.2.40");
    assert_eq!(parsed.last_error.as_deref(), Some("tts timeout"));
    assert_eq!(
        serde_json::to_value(&parsed).unwrap(),
        serde_json::from_str::<serde_json::Value>(python).unwrap()
    );
}

#[test]
fn session_mute_replies() {
    parses_to(
        r#"{"ok": true, "session_id": "s1"}"#,
        &SessionResponse {
            ok: true,
            session_id: Some("s1".into()),
            error: None,
        },
    );
    // The failure carries NO session_id key at all.
    parses_to(
        r#"{"ok": false, "error": "missing_session_id"}"#,
        &SessionResponse {
            ok: false,
            session_id: None,
            error: Some("missing_session_id".into()),
        },
    );
}

#[test]
fn health_probe_reply() {
    parses_to(
        r#"{"nonce": "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn", "agent": "codex"}"#,
        &HealthProbeResponse {
            nonce: "n".repeat(32),
            agent: "codex".into(),
        },
    );
    // A rejected probe comes back as `{}` — which is exactly a parse failure.
    assert!(serde_json::from_str::<HealthProbeResponse>("{}").is_err());
}

#[test]
fn first_run_hold_reply() {
    parses_to(
        r#"{"ok": false, "error": "first_run_hold"}"#,
        &OkResponse {
            ok: false,
            error: Some("first_run_hold".into()),
        },
    );
}
