//! The [`Extension`] seam: unknown commands, claim order, the unclaimed
//! fall-through, event observation, spoken-line callbacks, verbatim kinds
//! and per-session context.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use heard_daemon::server::dispatch;
use heard_daemon::{
    testing, Daemon, DaemonBuilder, Extension, HookQueue, NarrationEvent, Server, Speech,
    SpokenLine, Utterance,
};
use serde_json::{json, Map, Value};

/// A speech sink that keeps every utterance in memory.
#[derive(Default)]
struct Sink {
    lines: Mutex<Vec<Value>>,
}

impl Sink {
    fn lines(&self) -> Vec<Value> {
        self.lines.lock().unwrap().clone()
    }
}

fn as_value(u: &Utterance<'_>) -> Value {
    json!({
        "text": u.text, "tag": u.tag, "kind": u.kind,
        "session_id": u.session_id, "via": u.via, "project": u.project,
    })
}

impl Speech for Sink {
    fn speak(&self, u: &Utterance<'_>) {
        self.lines.lock().unwrap().push(as_value(u));
    }
}

/// An extension that records everything it is shown and claims the
/// commands it was told to.
struct Recorder {
    name: &'static str,
    /// cmd → reply (`None` = claim, fire-and-forget).
    claims: Vec<(&'static str, Option<&'static str>)>,
    verbatim: &'static [&'static str],
    context: Option<&'static str>,
    commands: Mutex<Vec<(String, Vec<u8>)>>,
    events: Mutex<Vec<Value>>,
    spoken: Mutex<Vec<Value>>,
}

impl Recorder {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            claims: Vec::new(),
            verbatim: &[],
            context: None,
            commands: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            spoken: Mutex::new(Vec::new()),
        }
    }
    fn claims(mut self, cmd: &'static str, reply: Option<&'static str>) -> Self {
        self.claims.push((cmd, reply));
        self
    }
    fn verbatim(mut self, kinds: &'static [&'static str]) -> Self {
        self.verbatim = kinds;
        self
    }
    fn context(mut self, text: &'static str) -> Self {
        self.context = Some(text);
        self
    }
    fn commands(&self) -> Vec<(String, Vec<u8>)> {
        self.commands.lock().unwrap().clone()
    }
    fn events(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
    }
    fn spoken(&self) -> Vec<Value> {
        self.spoken.lock().unwrap().clone()
    }
}

impl Extension for Recorder {
    fn name(&self) -> &'static str {
        self.name
    }
    fn handle_command(&self, _d: &Arc<Daemon>, cmd: &str, raw: &[u8]) -> Option<Option<Vec<u8>>> {
        self.commands
            .lock()
            .unwrap()
            .push((cmd.to_owned(), raw.to_vec()));
        let (_, reply) = self.claims.iter().find(|(c, _)| *c == cmd)?;
        Some(reply.map(|r| r.as_bytes().to_vec()))
    }
    fn observe_event(&self, event: &Value) {
        self.events.lock().unwrap().push(event.clone());
    }
    fn on_spoken(&self, line: &SpokenLine<'_>) {
        self.spoken.lock().unwrap().push(as_value(line));
    }
    fn verbatim_kinds(&self) -> &'static [&'static str] {
        self.verbatim
    }
    fn context_for(&self, _session: &str) -> Option<String> {
        self.context.map(str::to_owned)
    }
}

struct Rig {
    daemon: Arc<Daemon>,
    hooks: Arc<HookQueue>,
    sink: Arc<Sink>,
}

fn rig(label: &str, yaml: &str, extensions: Vec<Arc<dyn Extension>>) -> Rig {
    let dir = testing::temp_dir(label);
    let paths = heard_config::Paths::under(&dir);
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(&paths.config_path, yaml).unwrap();
    let sink = Arc::new(Sink::default());
    let daemon = DaemonBuilder::new(paths)
        .speech(Arc::clone(&sink) as Arc<dyn Speech>)
        .extensions(extensions)
        .build();
    let hooks = Arc::new(HookQueue::new(Arc::clone(&daemon)));
    Rig {
        daemon,
        hooks,
        sink,
    }
}

impl Rig {
    fn send(&self, raw: &[u8]) -> Option<Vec<u8>> {
        dispatch(&self.daemon, &self.hooks, raw)
    }
}

fn event(kind: &str, tag: &str, neutral: &str, session: &str) -> NarrationEvent {
    NarrationEvent {
        kind: kind.into(),
        tag: tag.into(),
        neutral: neutral.into(),
        ctx: Map::new(),
        session_id: session.into(),
        cwd: "/tmp/proj".into(),
    }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_cmd_goes_to_the_extensions_in_order_and_the_first_claim_wins() {
    let a = Arc::new(Recorder::new("a").claims("alpha", Some(r#"{"from":"a"}"#)));
    let b = Arc::new(
        Recorder::new("b")
            .claims("alpha", Some(r#"{"from":"b"}"#))
            .claims("beta", None),
    );
    let r = rig("ext-order", "onboarded: true\n", vec![a.clone(), b.clone()]);

    // `alpha`: both would claim it, the first in order answers.
    let raw: &[u8] = br#"{"cmd": "alpha", "x": 1}"#;
    assert_eq!(r.send(raw), Some(br#"{"from":"a"}"#.to_vec()));
    assert_eq!(a.commands(), vec![("alpha".to_string(), raw.to_vec())]);
    assert!(
        b.commands().is_empty(),
        "a later extension must not see a claimed cmd"
    );

    // `beta`: a passes, b claims it fire-and-forget — no reply.
    let raw: &[u8] = br#"{"cmd":"beta"}"#;
    assert_eq!(r.send(raw), None);
    assert_eq!(a.commands().last().unwrap().0, "beta");
    assert_eq!(b.commands(), vec![("beta".to_string(), raw.to_vec())]);

    // A claimed command never falls through to speech.
    assert!(r.sink.lines().is_empty());
}

#[tokio::test]
async fn an_extension_command_arrives_with_its_exact_wire_bytes() {
    let ext = Arc::new(Recorder::new("wire").claims("ask", Some(r#"{"ok":true}"#)));
    let r = rig("ext-wire", "onboarded: true\n", vec![ext.clone()]);
    // Python's `json.dumps` spacing, an escape, and a key order serde would
    // not produce: the extension must see these bytes, not a re-encoding.
    let raw: &[u8] =
        br#"{"cmd": "ask", "question": "what changed in \u00e9t\u00e9?", "speak": true, "cwd": "/tmp/p"}"#;
    assert_eq!(r.send(raw), Some(br#"{"ok":true}"#.to_vec()));
    assert_eq!(ext.commands(), vec![("ask".to_string(), raw.to_vec())]);
}

#[tokio::test]
async fn an_unclaimed_cmd_falls_through_to_speak_as_before() {
    let ext = Arc::new(Recorder::new("nothing"));
    let r = rig("ext-unclaimed", "onboarded: true\n", vec![ext.clone()]);

    // With a `text`, the fall-through speaks it, as `_handle` always did.
    assert_eq!(r.send(br#"{"cmd":"gamma","text":"Hello there"}"#), None);
    let lines = r.sink.lines();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["text"], "Hello there");
    assert_eq!(lines[0]["via"], "direct");

    // Without one it says nothing and answers nothing — including the two
    // request commands that left the core, when nothing claims them.
    assert_eq!(
        r.send(br#"{"cmd": "ask", "question": "what changed?", "speak": true}"#),
        None
    );
    assert_eq!(r.send(br#"{"cmd": "recap", "speak": true}"#), None);
    assert_eq!(r.sink.lines().len(), 1);
    let seen: Vec<String> = ext.commands().into_iter().map(|(c, _)| c).collect();
    assert_eq!(seen, ["gamma", "ask", "recap"]);
}

#[tokio::test]
async fn with_no_extensions_an_unknown_cmd_is_the_old_fall_through() {
    let r = rig("ext-none", "onboarded: true\n", Vec::new());
    assert_eq!(
        r.send(br#"{"cmd":"whatever","text":"spoken anyway"}"#),
        None
    );
    assert_eq!(r.sink.lines()[0]["text"], "spoken anyway");
    assert_eq!(
        r.send(br#"{"cmd": "ask", "question": "q", "speak": false}"#),
        None
    );
    assert_eq!(r.sink.lines().len(), 1);
}

#[tokio::test]
async fn core_commands_never_reach_an_extension() {
    let greedy = Arc::new(
        Recorder::new("greedy")
            .claims("status", Some("{}"))
            .claims("ping", None)
            .claims("cancel", None)
            .claims("voice_release", None)
            .claims("pin", None)
            .claims("speak", None),
    );
    let r = rig("ext-core", "onboarded: true\n", vec![greedy.clone()]);
    let status = r.send(br#"{"cmd":"status"}"#).expect("status answers");
    let status: Value = serde_json::from_slice(&status).unwrap();
    assert_eq!(status["alive"], true);
    assert_eq!(r.send(br#"{"cmd":"ping"}"#), None);
    assert_eq!(r.send(br#"{"cmd":"cancel"}"#), None);
    assert_eq!(r.send(br#"{"cmd":"voice_release"}"#), None);
    // A known cmd whose fields do not parse is still the core's (a blank
    // speak), never an extension command.
    assert_eq!(r.send(br#"{"cmd":"pin"}"#), None);
    assert_eq!(r.send(br#"{"cmd":"speak","text":"literal"}"#), None);
    assert!(greedy.commands().is_empty(), "{:?}", greedy.commands());
    assert_eq!(r.sink.lines().len(), 1);
    assert_eq!(r.sink.lines()[0]["text"], "literal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_extension_replies_over_the_real_socket() {
    let ext =
        Arc::new(Recorder::new("sock").claims("ask", Some(r#"{"ok": true, "answer": "yes"}"#)));
    let root = testing::temp_dir("ext-socket");
    let socket = testing::socket_path(&root);
    let paths = heard_config::Paths::under(&root);
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(&paths.config_path, "onboarded: true\n").unwrap();
    let daemon = DaemonBuilder::new(paths).extension(ext.clone()).build();
    let server = Server::bind(Arc::clone(&daemon), &socket).await.unwrap();
    tokio::spawn(server.serve());

    let body: &[u8] = br#"{"cmd": "ask", "question": "done?", "speak": false}"#;
    let reply = tokio::task::spawn_blocking(move || {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(body).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        buf
    })
    .await
    .unwrap();
    assert_eq!(reply, br#"{"ok": true, "answer": "yes"}"#.to_vec());
    assert_eq!(ext.commands(), vec![("ask".to_string(), body.to_vec())]);
}

// ---------------------------------------------------------------------------
// observation and callbacks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_extension_observes_each_event_before_the_narration_gates() {
    let (a, b) = (Arc::new(Recorder::new("a")), Arc::new(Recorder::new("b")));
    // Session-muted: nothing is SAID, but the event is still observed.
    let r = rig(
        "ext-observe",
        "onboarded: true\n",
        vec![a.clone(), b.clone()],
    );
    r.daemon.mute_session("s1");
    r.daemon
        .handle_event(&event("final", "final_short", "All tests pass.", "s1"));
    let want = vec![json!({
        "kind": "final", "tag": "final_short", "neutral": "All tests pass.",
        "session": {"id": "s1", "cwd": "/tmp/proj"},
    })];
    assert_eq!(a.events(), want);
    assert_eq!(b.events(), want);
    assert!(r.sink.lines().is_empty());

    // A duplicate is suppressed before observation...
    r.daemon
        .handle_event(&event("final", "final_short", "All tests pass.", "s1"));
    assert_eq!(a.events().len(), 1);
}

#[tokio::test]
async fn a_paused_daemon_observes_nothing() {
    let ext = Arc::new(Recorder::new("paused"));
    let r = rig(
        "ext-paused",
        "onboarded: true\npaused: true\n",
        vec![ext.clone()],
    );
    r.daemon
        .handle_event(&event("final", "final_short", "All tests pass.", "s1"));
    assert!(ext.events().is_empty());
}

#[tokio::test]
async fn every_spoken_line_reaches_on_spoken_exactly_as_the_sink_got_it() {
    let (a, b) = (Arc::new(Recorder::new("a")), Arc::new(Recorder::new("b")));
    let r = rig(
        "ext-spoken",
        "onboarded: true\n",
        vec![a.clone(), b.clone()],
    );
    assert_eq!(r.send(br#"{"text":"Direct line."}"#), None);
    r.daemon.say(
        "Said by an extension.",
        "ext_kind",
        "ext_tag",
        "__ext__",
        "ext",
    );
    r.daemon
        .handle_event(&event("final", "final_short", "All tests pass.", "s2"));
    let sink = r.sink.lines();
    assert_eq!(sink.len(), 3, "{sink:#?}");
    assert_eq!(a.spoken(), sink);
    assert_eq!(b.spoken(), sink);
    assert_eq!(sink[1]["session_id"], "__ext__");
    assert_eq!(sink[1]["via"], "ext");
}

#[tokio::test]
async fn nothing_reaches_on_spoken_while_muted() {
    let ext = Arc::new(Recorder::new("muted"));
    let r = rig(
        "ext-muted",
        "onboarded: true\nmuted: true\n",
        vec![ext.clone()],
    );
    r.daemon
        .say("Quiet please.", "ext_kind", "ext_tag", "", "ext");
    assert!(r.sink.lines().is_empty());
    assert!(ext.spoken().is_empty());
}

// ---------------------------------------------------------------------------
// verbatim kinds and context
// ---------------------------------------------------------------------------

const LONG: &str = "the build finished and every test in the suite passed";

#[tokio::test]
async fn a_verbatim_kind_is_spoken_without_the_casual_opener() {
    let casual = "onboarded: true\nnarration_register: 0\n";
    let plain = rig("ext-no-verbatim", casual, Vec::new());
    plain
        .daemon
        .say(LONG, "ext_answer", "ext_answer", "", "ext");
    let dressed = plain.sink.lines()[0]["text"].as_str().unwrap().to_owned();
    assert_ne!(
        dressed, LONG,
        "the core dresses an unclaimed kind with an opener"
    );
    assert!(dressed.ends_with(LONG), "{dressed:?}");

    let ext = Arc::new(Recorder::new("verbatim").verbatim(&["EXT_ANSWER", "ext_filler"]));
    let r = rig("ext-verbatim", casual, vec![ext]);
    assert_eq!(r.daemon.verbatim_kinds(), ["ext_answer", "ext_filler"]);
    r.daemon.say(LONG, "ext_answer", "ext_answer", "", "ext");
    // The TAG decides the shaping kind when it is verbatim.
    r.daemon.say(LONG, "ext_answer", "ext_filler", "", "ext");
    // Any other kind is shaped exactly as the core shapes it.
    r.daemon.say(LONG, "ext_other", "ext_other", "", "ext");
    let got: Vec<String> = r
        .sink
        .lines()
        .iter()
        .map(|l| l["text"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(got, [LONG.to_owned(), LONG.to_owned(), dressed]);
}

#[tokio::test]
async fn context_for_is_the_first_non_blank_answer_trimmed() {
    let blank = Arc::new(Recorder::new("blank").context("   "));
    let first = Arc::new(Recorder::new("first").context("  two agents building \n"));
    let second = Arc::new(Recorder::new("second").context("ignored"));
    let r = rig(
        "ext-context",
        "onboarded: true\n",
        vec![blank, first, second],
    );
    assert_eq!(
        r.daemon.context_for("s1").as_deref(),
        Some("two agents building")
    );

    let none = rig("ext-context-none", "onboarded: true\n", Vec::new());
    assert_eq!(none.daemon.context_for("s1"), None);
}

#[test]
fn emitted_events_reach_every_live_subscriber() {
    let dir = testing::temp_dir("events");
    let d = heard_daemon::DaemonBuilder::new(heard_config::Paths::under(&dir)).build();
    let a = d.subscribe_events();
    let b = d.subscribe_events();
    d.emit_event("phase", serde_json::json!({"phase": "thinking"}));
    for rx in [&a, &b] {
        let line = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(line, "{\"ev\": \"phase\", \"phase\": \"thinking\"}\n");
    }
    drop(b);
    d.emit_event("hello", serde_json::Value::Null);
    assert_eq!(
        a.recv_timeout(std::time::Duration::from_secs(1)).unwrap(),
        "{\"ev\": \"hello\"}\n"
    );
}
