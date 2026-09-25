//! The floor (voice/tour holds), the `subscribe` push stream over a real
//! socket, `inject` refusal, `feedback`, the resume panel, the dead-air
//! fillers and Focus's hung-tool line. Every test runs under its own temp
//! directory; no real socket, no audio.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use heard_daemon::server::dispatch;
use heard_daemon::{
    testing, Daemon, DaemonBuilder, Extension, Hold, HookQueue, LineOptions, NarrationEvent,
    Server, Speech, Utterance,
};
use heard_state::clock::ManualClock;
use serde_json::{json, Map, Value};

/// `(text, session, via, placement)`.
type Said = (String, String, String, Option<LineOptions>);

/// A sink that records lines and floor calls.
#[derive(Default)]
struct Sink {
    lines: Mutex<Vec<Said>>,
    floor: Mutex<Vec<String>>,
    holding: Mutex<bool>,
}

impl Sink {
    fn texts(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .map(|l| l.0.clone())
            .collect()
    }
    fn floor(&self) -> Vec<String> {
        self.floor.lock().unwrap().clone()
    }
}

impl Speech for Sink {
    fn speak(&self, u: &Utterance<'_>) {
        self.lines
            .lock()
            .unwrap()
            .push((u.text.into(), u.session_id.into(), u.via.into(), None));
    }
    fn speak_with(&self, u: &Utterance<'_>, o: LineOptions) {
        self.lines.lock().unwrap().push((
            u.text.into(),
            u.session_id.into(),
            u.via.into(),
            Some(o),
        ));
    }
    fn hold(&self, h: Hold) {
        self.floor.lock().unwrap().push(format!("hold:{h:?}"));
        *self.holding.lock().unwrap() = true;
    }
    fn release(&self, h: Hold) {
        self.floor.lock().unwrap().push(format!("release:{h:?}"));
        *self.holding.lock().unwrap() = false;
    }
    fn is_holding(&self) -> bool {
        *self.holding.lock().unwrap()
    }
    fn cancel(&self) {
        self.floor.lock().unwrap().push("cancel".into());
    }
    fn discard_held(&self) {
        self.floor.lock().unwrap().push("discard".into());
    }
    fn drop_session(&self, s: &str) -> usize {
        self.floor.lock().unwrap().push(format!("drop:{s}"));
        2
    }
    fn last_utterance_id(&self) -> Option<String> {
        Some("utt-1".into())
    }
}

struct Hello;
impl Extension for Hello {
    fn name(&self) -> &'static str {
        "hello"
    }
    fn subscribe_hello(&self, hello: &mut Map<String, Value>) {
        hello.insert("conversing".into(), Value::Bool(false));
        hello.insert("phase".into(), Value::from("idle"));
    }
}

struct Rig {
    daemon: Arc<Daemon>,
    hooks: Arc<HookQueue>,
    sink: Arc<Sink>,
    clock: Arc<ManualClock>,
    root: std::path::PathBuf,
}

fn rig(label: &str, yaml: &str) -> Rig {
    let root = testing::temp_dir(label);
    let paths = heard_config::Paths::under(&root);
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(&paths.config_path, yaml).unwrap();
    let sink = Arc::new(Sink::default());
    let clock = Arc::new(ManualClock::new(100.0));
    let daemon = DaemonBuilder::new(paths)
        .speech(sink.clone())
        .clock(clock.clone())
        .picker(|_n| 0)
        .extension(Arc::new(Hello))
        .build();
    let hooks = Arc::new(HookQueue::new(Arc::clone(&daemon)));
    Rig {
        daemon,
        hooks,
        sink,
        clock,
        root,
    }
}

fn send(r: &Rig, frame: Value) -> Option<Value> {
    let raw = serde_json::to_vec(&frame).unwrap();
    dispatch(&r.daemon, &r.hooks, &raw).map(|b| serde_json::from_slice(&b).unwrap())
}

fn event(kind: &str, tag: &str, neutral: &str, sid: &str) -> NarrationEvent {
    NarrationEvent {
        kind: kind.into(),
        tag: tag.into(),
        neutral: neutral.into(),
        session_id: sid.into(),
        ..NarrationEvent::default()
    }
}

#[test]
fn hold_commands_reach_the_sink_and_release_stamps_engagement() {
    let r = rig("floor-cmds", "onboarded: true\n");
    let before = r.daemon.last_user_engaged();
    std::thread::sleep(Duration::from_millis(5));
    for cmd in ["voice_hold", "voice_release", "tour_hold", "tour_release"] {
        assert!(send(&r, json!({ "cmd": cmd })).is_none());
    }
    assert_eq!(
        r.sink.floor(),
        vec!["hold:User", "release:User", "hold:Tour", "release:Tour"]
    );
    assert!(r.daemon.last_user_engaged() > before);
    // None of them falls through to `speak`.
    assert!(r.sink.texts().is_empty());
}

#[test]
fn inject_is_refused_explicitly() {
    let r = rig("inject", "onboarded: true\n");
    assert_eq!(
        send(&r, json!({"cmd": "inject", "text": "hi", "submit": true})),
        Some(json!({"ok": false, "error": "not_supported"}))
    );
    assert!(r.sink.texts().is_empty());
}

#[test]
fn speak_during_first_run_is_answered_with_the_hold() {
    let r = rig("speak-first-run", "onboarded: false\n");
    assert_eq!(
        send(&r, json!({"text": "hello"})),
        Some(json!({"ok": false, "error": "first_run_hold"}))
    );
    assert!(r.sink.texts().is_empty());
}

#[test]
fn feedback_lands_in_history_against_the_last_utterance() {
    let r = rig("feedback", "onboarded: true\n");
    // No `source` at all: the Python reads `req.get("source") or "cli"`.
    send(
        &r,
        json!({"cmd": "feedback", "text": "  too chatty \u{2014} really  "}),
    );
    send(&r, json!({"cmd": "feedback", "text": "   "}));
    let path = heard_config::Paths::under(&r.root)
        .config_dir
        .join("history.jsonl");
    let body = std::fs::read_to_string(path).unwrap();
    let lines: Vec<Value> = body
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["type"], "feedback");
    assert_eq!(lines[0]["ref"], "utt-1");
    assert_eq!(lines[0]["source"], "cli");
    assert_eq!(lines[0]["kind"], "explicit");
    assert_eq!(lines[0]["text"], "too chatty \u{2014} really");
}

#[test]
fn feedback_text_is_withheld_when_the_config_says_so() {
    let r = rig(
        "feedback-withheld",
        "onboarded: true\nhistory_user_text: false\n",
    );
    send(&r, json!({"cmd": "feedback", "text": "SECRET too chatty"}));
    let path = heard_config::Paths::under(&r.root)
        .config_dir
        .join("history.jsonl");
    let body = std::fs::read_to_string(path).unwrap();
    assert!(!body.contains("SECRET"), "{body}");
    let line: Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(line["type"], "feedback");
    assert_eq!(line["ref"], "utt-1");
    assert_eq!(line["text"], "");
    assert_eq!(line["redacted"], true);
}

#[test]
fn mute_session_flushes_that_sessions_queue() {
    let r = rig("mute-session", "onboarded: true\n");
    assert_eq!(
        send(&r, json!({"cmd": "mute_session", "session_id": "s1"})),
        Some(json!({"ok": true, "session_id": "s1"}))
    );
    assert_eq!(r.sink.floor(), vec!["drop:s1"]);
}

#[test]
fn resume_keywords_follow_the_python_sets() {
    use heard_daemon::daemon::classify_resume_intent as c;
    assert_eq!(c(""), "fresh");
    assert_eq!(c("yes please!"), "catch_up");
    assert_eq!(c("catch me up"), "catch_up");
    assert_eq!(c("start over"), "fresh");
    assert_eq!(c("nah"), "fresh");
    assert_eq!(c("yes no"), "fresh", "ambiguous → the no-model floor");
    assert_eq!(c("banana"), "fresh");
}

#[test]
fn unmute_with_a_buffer_arms_the_panel_and_it_times_out_fresh() {
    let r = rig("resume", "onboarded: true\n");
    r.daemon.router.note_event("s1", "/tmp/proj", None);
    r.daemon
        .router
        .add_to_digest("s1", "tool_pre", "tool_bash", "Running tests.", None);
    assert_eq!(r.daemon.router.pending_count(), 1);
    r.daemon.mute("menu");
    assert_eq!(r.daemon.router.pending_count(), 1, "mute keeps the buffer");
    r.daemon.unmute("menu");
    assert!(r.daemon.status().awaiting_resume_intent);
    assert!(
        r.sink.texts()[0].starts_with("Welcome back. While you were away, I queued up 1 thing.")
    );
    // Waiting on the panel: the tick does not drain the buffer.
    assert_eq!(r.daemon.tick(false), 0);
    assert_eq!(r.daemon.router.pending_count(), 1);
    r.clock.advance(30.0);
    r.daemon.tick(false);
    assert!(!r.daemon.status().awaiting_resume_intent);
    assert_eq!(r.daemon.router.pending_count(), 0, "timeout = fresh start");
}

#[test]
fn a_catch_up_answer_speaks_the_buffered_projects() {
    let r = rig("catch-up", "onboarded: true\n");
    r.daemon.router.note_event("s1", "/tmp/proj", None);
    r.daemon
        .router
        .add_to_digest("s1", "tool_pre", "tool_bash", "Running tests.", None);
    r.daemon.unmute("menu");
    send(&r, json!({"cmd": "resume_intent", "text": "yes"}));
    assert!(!r.daemon.status().awaiting_resume_intent);
    assert_eq!(r.sink.texts().len(), 2, "{:?}", r.sink.texts());
    assert_eq!(r.daemon.router.pending_count(), 0);
}

const COMPANION: &str = "onboarded: true\nmode: companion\nnarration_volume: companion\n";

#[test]
fn a_submitted_prompt_gets_one_filler_and_a_silent_turn_one_nudge() {
    let r = rig("fillers", COMPANION);
    r.daemon
        .handle_event(&event("prompt_intent", "prompt", "fix the build", "s1"));
    let lines = r.sink.lines.lock().unwrap().clone();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].0, "On it.");
    assert_eq!(lines[0].2, "filler");
    assert_eq!(
        lines[0].3,
        Some(LineOptions {
            priority: false,
            coexists: true,
            history: false
        })
    );
    // 30 s later the nudge is due, but this turn already had its filler.
    r.clock.advance(31.0);
    r.daemon.tick(false);
    assert_eq!(r.sink.texts().len(), 1, "never two fillers in a row");
    // A real line, then a new turn: no submit filler (not silent enough),
    // and 30 s of silence later its one nudge.
    r.daemon
        .say("Working on it.", "intermediate", "x", "s2", "brain");
    r.daemon
        .handle_event(&event("prompt_intent", "prompt", "and the docs", "s2"));
    r.clock.advance(31.0);
    r.daemon.tick(false);
    assert_eq!(r.sink.texts().last().unwrap(), "Still looking at this.");
    r.clock.advance(60.0);
    r.daemon.tick(false);
    assert_eq!(
        r.sink
            .texts()
            .iter()
            .filter(|t| t.starts_with("Still"))
            .count(),
        1,
        "at most one nudge per turn"
    );
}

#[test]
fn focus_speaks_once_for_a_hung_tool() {
    let r = rig(
        "hung",
        "onboarded: true\nmode: focus\nnarration_volume: focus\n",
    );
    r.daemon
        .handle_event(&event("tool_pre", "tool_bash", "Running the tests.", "s1"));
    let before = r.sink.texts().len();
    r.clock.advance(119.0);
    r.daemon.tick(false);
    assert_eq!(r.sink.texts().len(), before);
    r.clock.advance(2.0);
    r.daemon.tick(false);
    r.daemon.tick(false);
    let hung: Vec<String> = r
        .sink
        .texts()
        .into_iter()
        .filter(|t| t.starts_with("Still going"))
        .collect();
    assert_eq!(
        hung,
        vec!["Still going after two minutes \u{2014} Running the tests."]
    );
    // A later event disarms the watch.
    r.daemon
        .handle_event(&event("tool_post", "tool_bash", "", "s1"));
    r.clock.advance(500.0);
    r.daemon.tick(false);
    assert_eq!(
        r.sink
            .texts()
            .iter()
            .filter(|t| t.starts_with("Still going"))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_streams_hello_then_emitted_events_over_a_real_socket() {
    let r = rig("subscribe", "onboarded: true\n");
    let socket = testing::socket_path(&r.root);
    let server = Server::bind(Arc::clone(&r.daemon), &socket).await.unwrap();
    let serving = tokio::spawn(server.serve());
    let path = socket.clone();
    let lines = tokio::task::spawn_blocking(move || {
        let mut s = UnixStream::connect(&path).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(br#"{"cmd":"subscribe"}"#).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reader = BufReader::new(s);
        let mut hello = String::new();
        reader.read_line(&mut hello).unwrap();
        hello
    })
    .await
    .unwrap();
    assert_eq!(
        lines,
        "{\"ev\": \"hello\", \"conversing\": false, \"phase\": \"idle\"}\n"
    );

    // A second subscriber receives what is emitted after it joined.
    let path = socket.clone();
    let reader = tokio::task::spawn_blocking(move || {
        let mut s = UnixStream::connect(&path).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(br#"{"cmd": "subscribe"}"#).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reader = BufReader::new(s);
        let mut out = Vec::new();
        for _ in 0..2 {
            let mut l = String::new();
            reader.read_line(&mut l).unwrap();
            out.push(l);
        }
        out
    });
    for _ in 0..500 {
        if r.daemon.events().subscriber_count() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    r.daemon
        .emit_event("speech_started", json!({"kind": "final"}));
    let got = reader.await.unwrap();
    assert!(got[0].starts_with("{\"ev\": \"hello\""));
    assert_eq!(
        got[1],
        "{\"ev\": \"speech_started\", \"kind\": \"final\"}\n"
    );
    r.daemon.stop();
    let _ = UnixStream::connect(&socket);
    let _ = tokio::time::timeout(Duration::from_secs(5), serving).await;
}

/// An edition's resume classifier: answers every ambiguous reply with a
/// fixed label and records what it was asked.
struct Classifier(&'static str, Mutex<Vec<String>>);

impl Extension for Classifier {
    fn name(&self) -> &'static str {
        "classifier"
    }
    fn classify_resume_intent(&self, text: &str) -> Option<&'static str> {
        self.1.lock().unwrap().push(text.to_owned());
        Some(self.0)
    }
}

fn classifier_rig(label: &str, answer: &'static str) -> (Rig, Arc<Classifier>) {
    let root = testing::temp_dir(label);
    let paths = heard_config::Paths::under(&root);
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(&paths.config_path, "onboarded: true\n").unwrap();
    let sink = Arc::new(Sink::default());
    let clock = Arc::new(ManualClock::new(100.0));
    let classifier = Arc::new(Classifier(answer, Mutex::new(Vec::new())));
    let daemon = DaemonBuilder::new(paths)
        .speech(sink.clone())
        .clock(clock.clone())
        .picker(|_n| 0)
        .extension(classifier.clone())
        .build();
    let hooks = Arc::new(HookQueue::new(Arc::clone(&daemon)));
    (
        Rig {
            daemon,
            hooks,
            sink,
            clock,
            root,
        },
        classifier,
    )
}

fn buffer_one(r: &Rig) {
    r.daemon.router.note_event("s1", "/tmp/proj", None);
    r.daemon
        .router
        .add_to_digest("s1", "tool_pre", "tool_bash", "Running tests.", None);
    r.daemon.unmute("menu");
}

#[test]
fn an_ambiguous_resume_answer_goes_to_the_edition_classifier() {
    let (r, c) = classifier_rig("resume-classifier", "catch_up");
    buffer_one(&r);
    send(
        &r,
        json!({"cmd": "resume_intent", "text": "  hmm what happened  "}),
    );
    assert!(!r.daemon.status().awaiting_resume_intent, "cleared at once");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while r.daemon.router.pending_count() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(r.daemon.router.pending_count(), 0);
    assert_eq!(
        r.sink.texts().len(),
        2,
        "the catch-up spoke: {:?}",
        r.sink.texts()
    );
    assert_eq!(*c.1.lock().unwrap(), vec!["hmm what happened".to_string()]);
}

#[test]
fn keywords_and_empty_answers_never_reach_the_classifier() {
    let (r, c) = classifier_rig("resume-keywords", "catch_up");
    assert_eq!(r.daemon.resume_intent("", false), "fresh");
    assert_eq!(r.daemon.resume_intent("nope", false), "fresh");
    assert_eq!(r.daemon.resume_intent("yes", false), "catch_up");
    assert!(c.1.lock().unwrap().is_empty());
    // "other" drops the buffer like "fresh".
    let (r2, _) = classifier_rig("resume-other", "other");
    buffer_one(&r2);
    assert_eq!(r2.daemon.resume_intent("tell me a joke", false), "other");
    assert_eq!(r2.daemon.router.pending_count(), 0);
    assert_eq!(r2.sink.texts().len(), 1, "no catch-up line");
}
