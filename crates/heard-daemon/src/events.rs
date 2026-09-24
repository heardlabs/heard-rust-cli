//! The event push channel: `{"cmd":"subscribe"}` and [`EventBus`].
//!
//! A client that sends `subscribe` keeps its connection open and receives one
//! JSON object per line, PUSHED as things happen (speech lifecycle, output
//! device changes, and whatever an edition's extensions emit), instead of
//! polling `status`.
//!
//! # The wire
//!
//! Exactly the Python daemon's (`_add_subscriber` / `emit_event`):
//!
//! * the client writes `{"cmd":"subscribe"}` and half-closes its write side
//!   (the daemon reads to EOF, like every other frame);
//! * the daemon answers with a `hello` line straight away, so a late joiner
//!   starts from the current state rather than from silence:
//!   `{"ev": "hello", …}` — the fields come from every extension's
//!   [`crate::Extension::subscribe_hello`];
//! * then one line per event: `{"ev": "<name>", <fields…>}` + `"\n"`,
//!   spelled the way CPython's `json.dumps` spells it (`", "` / `": "`
//!   separators, `ensure_ascii`), `ev` first;
//! * the connection stays open until the client goes away. A subscriber
//!   whose socket fails to take a line is dropped (`event_subscribers_pruned`).
//!
//! Events the core itself emits: `speech_started` / `speech_finished`
//! (`{"kind": <history kind>}`, from the speech queue through
//! [`EventBus`]). An edition emits its own through [`EventBus::emit`] or
//! [`crate::Daemon::emit_event`].
//!
//! # Why a separate bus
//!
//! The speech queue is built BEFORE the daemon (the daemon takes it as its
//! sink), yet it is what knows when a line starts and stops playing. The bus
//! is therefore its own cheap, cloneable value: a composition root makes one,
//! hands a clone to the speech queue and the same bus to
//! [`crate::DaemonBuilder::events`].

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use heard_state::pyjson::{value_from_fixture, PyValue};
use serde_json::{Map, Value};

/// One event, already rendered as its wire line (newline included).
pub type EventLine = String;

/// The subscriber registry. Clones share it.
#[derive(Clone, Default)]
pub struct EventBus {
    subs: Arc<Mutex<Vec<Sender<EventLine>>>>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("subscribers", &self.subscriber_count())
            .finish()
    }
}

impl EventBus {
    /// A bus with no subscribers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push `{"ev": name, …fields}` to every subscriber. `fields` must be a
    /// JSON object (`null` = no fields; anything else is sent as
    /// `{"value": fields}`). Never blocks: the channel is unbounded and each
    /// subscriber's socket is written by its own thread. With no subscriber
    /// this is one lock and nothing else.
    pub fn emit(&self, name: &str, fields: Value) {
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        if subs.is_empty() {
            return;
        }
        let line = wire_line(name, fields);
        subs.retain(|tx| tx.send(line.clone()).is_ok());
    }

    /// A receiver of every line emitted from now on. Dropping it
    /// unsubscribes (the next emit prunes the sender).
    #[must_use]
    pub fn subscribe(&self) -> Receiver<EventLine> {
        let (tx, rx) = channel();
        self.subs.lock().unwrap_or_else(|e| e.into_inner()).push(tx);
        rx
    }

    /// How many subscribers are registered (a dead one counts until the next
    /// emit prunes it).
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.subs.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// `json.dumps({"ev": name, **fields}) + "\n"`.
#[must_use]
pub fn wire_line(name: &str, fields: Value) -> EventLine {
    let fields: Map<String, Value> = match fields {
        Value::Object(m) => m,
        Value::Null => Map::new(),
        other => {
            let mut m = Map::new();
            m.insert("value".into(), other);
            m
        }
    };
    let mut pairs: Vec<(String, PyValue)> = Vec::with_capacity(fields.len() + 1);
    pairs.push(("ev".into(), PyValue::Str(name.to_owned())));
    for (k, v) in &fields {
        if k == "ev" {
            continue; // `{"ev": ev, **fields}` — the kwarg cannot be `ev`
        }
        pairs.push((k.clone(), value_from_fixture(v)));
    }
    let mut line = ascii_escape(&PyValue::Dict(pairs).dumps());
    line.push('\n');
    line
}

/// `ensure_ascii=True` over a `ensure_ascii=False` rendering: every
/// non-ASCII character (which can only occur inside a string) becomes
/// `\uXXXX`, astral characters as a surrogate pair — CPython's spelling.
fn ascii_escape(s: &str) -> String {
    if s.is_ascii() {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_python_json_dumps_with_ev_first() {
        assert_eq!(
            wire_line("speech_started", serde_json::json!({"kind": "final"})),
            "{\"ev\": \"speech_started\", \"kind\": \"final\"}\n"
        );
        assert_eq!(wire_line("x", Value::Null), "{\"ev\": \"x\"}\n");
        assert_eq!(
            wire_line("x", serde_json::json!(3)),
            "{\"ev\": \"x\", \"value\": 3}\n"
        );
        assert_eq!(
            wire_line("x", serde_json::json!({"on": false, "n": 1.5})),
            "{\"ev\": \"x\", \"n\": 1.5, \"on\": false}\n"
        );
    }

    #[test]
    fn non_ascii_is_escaped_like_ensure_ascii() {
        assert_eq!(
            wire_line("d", serde_json::json!({"device": "Andy’s AirPods 🎧"})),
            "{\"ev\": \"d\", \"device\": \"Andy\\u2019s AirPods \\ud83c\\udfa7\"}\n"
        );
    }

    #[test]
    fn a_dropped_receiver_is_pruned_on_the_next_emit() {
        let bus = EventBus::new();
        let rx = bus.subscribe();
        let gone = bus.subscribe();
        drop(gone);
        assert_eq!(bus.subscriber_count(), 2);
        bus.emit("a", Value::Null);
        assert_eq!(bus.subscriber_count(), 1);
        assert_eq!(rx.recv().unwrap(), "{\"ev\": \"a\"}\n");
        // With nobody listening, emit is a no-op.
        let empty = EventBus::new();
        empty.emit("a", Value::Null);
    }
}
