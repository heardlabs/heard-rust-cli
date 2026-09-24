//! The [`Extension`] seam — how features that are not part of the core attach
//! to the daemon without the core knowing their names.
//!
//! An extension can:
//!
//! | hook | when | what for |
//! |---|---|---|
//! | [`Extension::handle_command`] | a socket frame whose `cmd` the core does not know ([`heard_proto::Frame::Extension`]) | its own commands, with the exact wire bytes |
//! | [`Extension::observe_event`] | every narration event that passed the duplicate and pause gates, before any narration decision | memory, analytics, anything that must see what the agent DID |
//! | [`Extension::on_spoken`] | every line the daemon handed to its [`Speech`](crate::Speech) sink | a record of what was said |
//! | [`Extension::verbatim_kinds`] | once, at build | kinds whose text the register pass must not dress with a casual opener |
//! | [`Extension::context_for`] | [`Daemon::context_for`](crate::Daemon::context_for) | one piece of per-session context |
//! | [`Extension::subscribe_hello`] | a `subscribe` client connects | the fields of its first `hello` line |
//!
//! Every method has a default, so an extension implements only what it uses.
//!
//! # Order
//!
//! The daemon holds its extensions in the order the builder received them
//! ([`DaemonBuilder::extension`](crate::DaemonBuilder::extension)). A command
//! is offered to each in that order and the FIRST that claims it replies; an
//! unclaimed command gets the core's own behaviour for an unknown `cmd`,
//! which is the `speak` fall-through (`req.get("text") or ""`). Observers and
//! [`Extension::on_spoken`] run for every extension, in order.
//!
//! # Rules
//!
//! * Never panic. A panic in a hook is a panic in the daemon's accept loop or
//!   in a session's hook lane.
//! * Never block for long. [`Extension::handle_command`] runs on the accept
//!   loop, which reads frames in order; long work (a network call, an LLM
//!   turn) belongs on a thread or task the extension spawns, the reply to a
//!   fire-and-forget command being `Some(None)`.
//! * Do not keep the `Arc<Daemon>` handed to [`Extension::handle_command`]
//!   beyond the work it starts: the daemon owns its extensions, so an
//!   extension that stores the daemon keeps both alive forever. Take a
//!   [`std::sync::Weak`] if it must outlive the call.

use std::sync::Arc;

use serde_json::Value;

use crate::daemon::Daemon;
use crate::speech::Utterance;

/// A line the daemon handed to its speech sink: exactly the [`Utterance`]
/// the sink received.
pub type SpokenLine<'a> = Utterance<'a>;

/// A feature that attaches to the daemon from outside the core. Object-safe:
/// the daemon holds `Vec<Arc<dyn Extension>>`.
pub trait Extension: Send + Sync {
    /// For logs and `status`. Stable, short, lowercase.
    fn name(&self) -> &'static str;

    /// A frame whose `cmd` the core does not know.
    ///
    /// `cmd` is the frame's `cmd`, unescaped; `raw` is the whole frame
    /// byte for byte as it arrived, to parse with the extension's own types.
    ///
    /// * `None` — not mine; the next extension is asked.
    /// * `Some(None)` — claimed, fire-and-forget: nothing is written back.
    /// * `Some(Some(bytes))` — claimed; `bytes` is the reply, written back
    ///   as-is before the connection closes.
    fn handle_command(
        &self,
        daemon: &Arc<Daemon>,
        cmd: &str,
        raw: &[u8],
    ) -> Option<Option<Vec<u8>>> {
        let _ = (daemon, cmd, raw);
        None
    }

    /// One narration event, as
    /// `{"kind","tag","neutral","session":{"id","cwd"}}`.
    ///
    /// Called for every event that passed duplicate suppression and the True
    /// Pause gate, BEFORE the first-run hold, per-session mute and the
    /// narration policy — so it sees what the agent did, not what the daemon
    /// chose to say.
    fn observe_event(&self, event: &Value) {
        let _ = event;
    }

    /// A line the daemon just handed to its speech sink (an event's line, a
    /// line said with [`Daemon::say`], or a direct `speak`).
    fn on_spoken(&self, line: &SpokenLine<'_>) {
        let _ = line;
    }

    /// Kinds whose text is spoken as written: the casual register does not
    /// prepend an opener to them. Matched case-insensitively. Read once, when
    /// the daemon is built.
    fn verbatim_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    /// Fields for the `hello` line a new `subscribe` client gets first
    /// (`{"ev": "hello", …}`), so a late joiner starts from the current
    /// state. Insert into `hello`; a later extension's key wins.
    fn subscribe_hello(&self, hello: &mut serde_json::Map<String, Value>) {
        let _ = hello;
    }

    /// Context for `session`, if this extension has any. The daemon returns
    /// the first non-blank answer, trimmed, in extension order.
    fn context_for(&self, session: &str) -> Option<String> {
        let _ = session;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Bare;
    impl Extension for Bare {
        fn name(&self) -> &'static str {
            "bare"
        }
    }

    #[test]
    fn every_hook_defaults_to_doing_nothing() {
        let dir = crate::testing::temp_dir("ext-defaults");
        let daemon = crate::DaemonBuilder::new(heard_config::Paths::under(&dir)).build();
        let ext: Arc<dyn Extension> = Arc::new(Bare);
        assert_eq!(ext.name(), "bare");
        assert!(ext.handle_command(&daemon, "anything", b"{}").is_none());
        ext.observe_event(&serde_json::json!({}));
        ext.on_spoken(&Utterance {
            text: "hi",
            tag: "",
            kind: "speak",
            session_id: "",
            via: "direct",
            project: "",
        });
        assert!(ext.verbatim_kinds().is_empty());
        assert!(ext.context_for("s1").is_none());
        let mut hello = serde_json::Map::new();
        ext.subscribe_hello(&mut hello);
        assert!(hello.is_empty());
    }
}
