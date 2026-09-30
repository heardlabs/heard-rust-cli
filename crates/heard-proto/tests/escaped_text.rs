//! Frame text that arrives JSON-escaped must parse, and must parse to the
//! unescaped string.
//!
//! A borrowed `&str` field can only be read when the wire bytes ARE the
//! string, so any escape — `\/` (which Swift's `JSONEncoder` emits for every
//! `/` by default), `\"`, `\n`, `\uXXXX` — made the whole frame fail and fall
//! through to a blank `speak`. Every text field is a `Cow<str>` now: borrowed
//! when the bytes allow it, owned when they need unescaping. These tests pin
//! that for every frame shape that carries free text, and through both
//! entry points: plain `serde_json::from_slice::<Message>` and
//! [`heard_proto::parse_frame`].

use std::borrow::Cow;

use heard_proto::{
    parse_frame, Agent, Event, Feedback, Frame, Message, Mute, MuteSession, Pin, ReportDefect,
    Request, ResumeIntent, Session, Speak, Unmute, UnmuteSession,
};

/// Every awkward string, the way a client puts it on the wire (escaped JSON
/// string body) and the way the daemon must read it back.
const CASES: &[(&str, &str)] = &[
    // Swift's default `/` escape.
    (r"src\/main.rs", "src/main.rs"),
    (r#"a \"quoted\" word"#, "a \"quoted\" word"),
    (r"line one\nline two", "line one\nline two"),
    (r"tab\there\\back", "tab\there\\back"),
    // Raw UTF-8 non-ASCII and emoji, no escapes at all.
    (
        "caf\u{e9} na\u{ef}ve \u{1f680} done",
        "caf\u{e9} na\u{ef}ve \u{1f680} done",
    ),
    // The same, `\u`-escaped (Python's `json.dumps` default, `ensure_ascii`),
    // emoji as a surrogate pair.
    (
        r"caf\u00e9 \ud83d\ude80 \u4f60\u597d",
        "caf\u{e9} \u{1f680} \u{4f60}\u{597d}",
    ),
    // Everything at once.
    (
        r#"Edited \"src\/lib.rs\"\n\u2014 caf\u00e9 \ud83c\udf89"#,
        "Edited \"src/lib.rs\"\n\u{2014} caf\u{e9} \u{1f389}",
    ),
];

fn message(raw: &str) -> Message<'_> {
    let m: Message<'_> =
        serde_json::from_slice(raw.as_bytes()).unwrap_or_else(|e| panic!("{raw}: {e}"));
    // `parse_frame` is what the daemon actually calls; it must agree.
    assert_eq!(
        parse_frame(raw.as_bytes()).expect("parse_frame"),
        Frame::Core(m.clone()),
        "{raw}"
    );
    m
}

#[test]
fn speak_text_is_unescaped() {
    for (wire, want) in CASES {
        let raw = format!(r#"{{"text":"{wire}"}}"#);
        assert_eq!(
            message(&raw),
            Message::Speak(Speak {
                text: Cow::Borrowed(want),
                priority: false,
            }),
            "{raw}"
        );
        let raw = format!(r#"{{"text":"{wire}","priority":true}}"#);
        let Message::Speak(speak) = message(&raw) else {
            panic!("{raw}: expected speak");
        };
        assert_eq!(speak.text, *want);
        assert!(speak.priority);
    }
}

#[test]
fn an_unescaped_speak_still_borrows() {
    let raw = br#"{"text":"plain words"}"#;
    let Message::Speak(speak) = serde_json::from_slice(raw).expect("speak") else {
        panic!("expected speak");
    };
    assert!(matches!(speak.text, Cow::Borrowed("plain words")));
}

#[test]
fn command_text_fields_are_unescaped() {
    for (wire, want) in CASES {
        let want = Cow::Borrowed(*want);
        let checks: Vec<(String, Request<'static>)> = vec![
            (
                format!(r#"{{"cmd":"pin","session_id":"{wire}"}}"#),
                Request::Pin(Pin {
                    session_id: want.clone(),
                }),
            ),
            (
                format!(r#"{{"cmd":"mute","source":"{wire}"}}"#),
                Request::Mute(Mute {
                    source: want.clone(),
                }),
            ),
            (
                format!(r#"{{"cmd":"unmute","source":"{wire}"}}"#),
                Request::Unmute(Unmute {
                    source: want.clone(),
                }),
            ),
            (
                format!(r#"{{"cmd":"resume_intent","text":"{wire}"}}"#),
                Request::ResumeIntent(ResumeIntent { text: want.clone() }),
            ),
            (
                format!(r#"{{"cmd":"feedback","text":"{wire}","source":"{wire}"}}"#),
                Request::Feedback(Feedback {
                    text: want.clone(),
                    source: want.clone(),
                }),
            ),
            (
                format!(
                    r#"{{"cmd":"report_defect","category":"{wire}","note":"{wire}","source":"{wire}"}}"#
                ),
                Request::ReportDefect(ReportDefect {
                    category: want.clone(),
                    note: want.clone(),
                    source: want.clone(),
                }),
            ),
            (
                format!(r#"{{"cmd":"mute_session","session_id":"{wire}"}}"#),
                Request::MuteSession(MuteSession {
                    session_id: want.clone(),
                }),
            ),
            (
                format!(r#"{{"cmd":"unmute_session","session_id":"{wire}"}}"#),
                Request::UnmuteSession(UnmuteSession {
                    session_id: want.clone(),
                }),
            ),
        ];
        for (raw, request) in checks {
            assert_eq!(message(&raw), Message::Command(request), "{raw}");
        }
    }
}

#[test]
fn narration_event_fields_are_unescaped() {
    for (wire, want) in CASES {
        let raw = format!(
            r#"{{"cmd":"event","kind":"{wire}","neutral":"{wire}","tag":"{wire}","ctx":{{"k":"{wire}"}},"session":{{"id":"{wire}","cwd":"{wire}","transcript_path":"{wire}"}}}}"#
        );
        let Message::Command(Request::Event(Event::Narration(event))) = message(&raw) else {
            panic!("{raw}: expected a narration event");
        };
        assert_eq!(event.kind, *want);
        assert_eq!(event.neutral, *want);
        assert_eq!(event.tag, *want);
        assert_eq!(event.ctx["k"], *want);
        let Session::Known(session) = event.session else {
            panic!("{raw}: expected a known session");
        };
        assert_eq!(session.id, *want);
        assert_eq!(session.cwd.as_deref(), Some(*want));
        assert_eq!(session.transcript_path.as_deref(), Some(*want));
    }
}

#[test]
fn a_health_probe_nonce_is_unescaped() {
    // 32 characters once unescaped; 33 bytes on the wire.
    let raw = r#"{"cmd":"event","kind":"health_probe","agent":"codex","nonce":"abcdefghijklmno\/pqrstuvwxyz01234"}"#;
    let Message::Command(Request::Event(Event::HealthProbe(probe))) = message(raw) else {
        panic!("expected a health probe");
    };
    assert_eq!(probe.agent, Agent::Codex);
    assert_eq!(probe.nonce, "abcdefghijklmno/pqrstuvwxyz01234");
    assert_eq!(probe.nonce.chars().count(), 32);
}

#[test]
fn a_hook_binding_is_unescaped() {
    for (wire, want) in CASES {
        let raw = format!(
            r#"{{"cmd":"hook","agent":"claude-code","payload":{{"prompt":"{wire}"}},"binding":{{"host_name":"{wire}","host_type":"terminal","provenance":"env_terminal_binding","confidence":0.9,"pid":7,"process_started_at":null,"herdr_pane_id":"{wire}","herdr_socket_path":"{wire}"}}}}"#
        );
        let Message::Command(Request::Hook(hook)) = message(&raw) else {
            panic!("{raw}: expected a hook");
        };
        assert_eq!(hook.payload["prompt"], *want);
        let binding = hook.binding.expect("binding");
        assert_eq!(binding.host_name, *want);
        assert_eq!(binding.herdr_pane_id.as_deref(), Some(*want));
        assert_eq!(binding.herdr_socket_path.as_deref(), Some(*want));
        assert!(binding.herdr_tab_id.is_none());
    }
}

#[test]
fn an_extension_frame_keeps_escaped_text_in_its_fallback() {
    for (wire, want) in CASES {
        let raw = format!(r#"{{"cmd":"ask\/v2","text":"{wire}"}}"#);
        let Frame::Extension(ext) = parse_frame(raw.as_bytes()).expect("frame") else {
            panic!("{raw}: expected an extension command");
        };
        assert_eq!(ext.cmd, "ask/v2");
        assert_eq!(ext.fallback.text, *want);
        assert_eq!(ext.raw, raw.as_bytes());
    }
}

#[test]
fn escaped_frames_round_trip_through_serialisation() {
    for (_, want) in CASES {
        let speak = Message::Speak(Speak {
            text: Cow::Borrowed(want),
            priority: false,
        });
        let bytes = serde_json::to_vec(&speak).expect("encode");
        assert_eq!(
            serde_json::from_slice::<Message<'_>>(&bytes).expect("decode"),
            speak
        );

        let pin = Message::Command(Request::Pin(Pin {
            session_id: Cow::Borrowed(want),
        }));
        let bytes = serde_json::to_vec(&pin).expect("encode");
        assert_eq!(
            serde_json::from_slice::<Message<'_>>(&bytes).expect("decode"),
            pin
        );
    }
}
