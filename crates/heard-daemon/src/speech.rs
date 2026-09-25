//! The [`Speech`] sink — where a would-be utterance goes.
//!
//! This is the seam for **differential running**: the Rust daemon can run
//! alongside the Python reference implementation against a copied socket,
//! with hook traffic teed to both. Divergence in what they would SPEAK is
//! logged, not spoken.
//!
//! The real speech queue (TTS, `afplay`, cancellation) is `heard-speech`. The
//! daemon reaches the exact point where `Daemon._start_speech` would queue an
//! utterance and hands it here. The implementation in this crate is
//! [`LogSpeech`], which appends one JSON line
//! per utterance so a run can be diffed line-for-line against the Python
//! daemon's `history.jsonl`.
//!
//! **This crate contains no audio path at all.** There is no `afplay`, no
//! `say`, no TTS backend and no way to reach one from here — a sink that
//! spoke would be a different type in a different crate.
//!
//! `history.jsonl` goes through the same sink, via
//! [`heard_state::history::History`], because in the Python the history
//! append is downstream of playback (`_speak` → `history.append`): it records
//! what was SAID, so it belongs on the speaking side of this trait and not in
//! the routing above it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use heard_state::history::History;
use heard_state::pyjson::PyValue;

/// The core's `history.jsonl` policy source: [`heard_config::Config::load`]
/// read fresh on every append, and `history_user_text` interpreted by
/// [`heard_state::history_policy::from_config`] (absent = record the user's
/// words; `false` = withhold them). An edition with its own consent setting
/// builds its own source instead.
pub fn config_history_policy(config: heard_config::Config) -> heard_state::HistoryPolicySource {
    std::sync::Arc::new(move || {
        let cfg = config.load(None).unwrap_or_default();
        heard_state::history_policy::from_config(&cfg)
    })
}

/// One utterance the daemon would have spoken.
///
/// Borrowed end to end: every field is already owned by the event being
/// routed, and a sink that needs to keep one says so by copying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Utterance<'a> {
    /// The words. Already shaped by whichever lane produced them.
    pub text: &'a str,
    /// The originating event's `tag` (`tool_bash`, `final_long`, …).
    pub tag: &'a str,
    /// The originating event's `kind` (`tool_pre`, `final`, …).
    pub kind: &'a str,
    /// Which agent session it came from.
    pub session_id: &'a str,
    /// Which lane produced the text: `fastpath`, `brain` or `floor` —
    /// `daemon.py`'s `history_meta["via"]`, with `harness` spelled `brain`
    /// here because the harness itself is not ported (see [`crate::brain`]).
    pub via: &'a str,
    /// `repo_name` for the session, when the router knows one. Carried so the
    /// differential diff can be grouped by project.
    pub project: &'a str,
}

/// Where an utterance goes once the daemon has decided to speak it.
pub trait Speech: Send + Sync {
    /// Speak — or, in this lane, record. Must not block for long: it is
    /// called from a per-session hook task that other events of the same
    /// session are queued behind.
    fn speak(&self, utterance: &Utterance<'_>);

    /// `_cancel_only` — silence what is playing and drop what is queued.
    /// A recording sink has nothing to cancel, hence the no-op default.
    fn cancel(&self) {}

    /// `daemon._current_proc is not None or bool(daemon._queue)` — a line is
    /// playing or waiting. A recording sink never is.
    fn is_speaking(&self) -> bool {
        false
    }

    /// `status`'s `(speaking, queued)`: a line is being synthesised or
    /// played now, and how many wait behind it.
    fn queue_state(&self) -> (bool, usize) {
        (self.is_speaking(), 0)
    }

    /// [`Speech::speak`] with explicit queue placement. The default ignores
    /// `opts`: a sink without a queue has nothing to place.
    fn speak_with(&self, utterance: &Utterance<'_>, opts: LineOptions) {
        let _ = opts;
        self.speak(utterance);
    }

    /// Take the floor away from narration (see [`Hold`]). A recording sink
    /// has no floor to give, hence the no-op default.
    fn hold(&self, hold: Hold) {
        let _ = hold;
    }

    /// Give the floor back: replay what was held, in order, unless another
    /// hold still has it.
    fn release(&self, hold: Hold) {
        let _ = hold;
    }

    /// Drop everything held for replay without speaking it (`first_run_hold`,
    /// `mute`): nothing suppressed now is replayed later.
    fn discard_held(&self) {}

    /// Force-clear the external-microphone latch and replay the backlog — the
    /// activation test and `self_diagnose` "no app is holding your mic" heal.
    fn clear_mic_latch(&self) {}

    /// Whether an external app holds the microphone (the mic latch).
    fn mic_active(&self) -> bool {
        false
    }

    /// Whether a line handed over now would be held for replay rather than
    /// queued (a hold or the mic latch is in force).
    fn is_holding(&self) -> bool {
        false
    }

    /// Drop the queued (not yet playing) lines of `session_id`
    /// (`mute_session`); returns how many.
    fn drop_session(&self, session_id: &str) -> usize {
        let _ = session_id;
        0
    }

    /// Silence `session_id` only: drop its queued lines AND cut its line if
    /// that is the one playing now; other sessions' lines are untouched (a
    /// voice preview cancelled by the next click). Returns how many queued
    /// lines were dropped. Default: [`Speech::drop_session`] (a sink with no
    /// playback has nothing to cut).
    fn cut_session(&self, session_id: &str) -> usize {
        self.drop_session(session_id)
    }

    /// The `history.jsonl` id of the last line recorded, for `feedback`.
    fn last_utterance_id(&self) -> Option<String> {
        None
    }
}

/// Queue placement for [`Speech::speak_with`] — the Python's
/// `_start_speech(priority=…, coexists=…, history_meta=…)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineOptions {
    /// Results / decisions / answers: jump routine lines, supersede this
    /// session's queued routine lines.
    pub priority: bool,
    /// Do not clear other sessions' queued lines.
    pub coexists: bool,
    /// Record the line in `history.jsonl` (the Python's `if hmeta:`).
    pub history: bool,
}

/// Who holds the floor, for [`Speech::hold`] / [`Speech::release`].
///
/// | hold | wire | on hold | while held | on release |
/// |---|---|---|---|---|
/// | [`Hold::User`] | `voice_hold` / `voice_release` | cut what is playing and drop the queue (the user started talking) | new lines are held (priority-aware cap, age limit) | replay the held lines in order |
/// | [`Hold::Tour`] | `tour_hold` / `tour_release` | RESCUE the unplayed queue into the held buffer, then cut | new lines are held, except for the sink's exempt sessions (lines the user asked for directly); a tour hold older than 30 minutes lapses | replay the held lines in order |
///
/// Replay waits until NO hold (and no mic latch) remains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hold {
    /// The user is speaking (push-to-talk key down, dictation capture).
    User,
    /// The app's guided tour is on screen.
    Tour,
}

/// A canned dead-air line (the submit ack, the "still thinking" nudge): it
/// coexists with what is queued, carries no history meta, and does not end
/// the turn's silence. See [`crate::Daemon::tick`].
pub const VIA_FILLER: &str = "filler";
/// A timer-driven notice (the Focus hung-tool line): coexists, no history
/// meta.
pub const VIA_NOTICE: &str = "notice";

/// The differential-run sink: one JSON line per would-be utterance.
///
/// Each line is `{ts, text, tag, kind, session_id, via}` — `ts` being seconds
/// since the epoch, so two runs can be aligned by time without either side
/// agreeing on a timezone.
pub struct LogSpeech {
    path: PathBuf,
    history: Option<History>,
    /// What `history.jsonl` may keep of the user's words.
    history_policy: Option<heard_state::HistoryPolicySource>,
    /// Serialises the append so two session tasks can't interleave a line.
    /// The write itself is one `write_all` of a whole line, which the OS
    /// already keeps atomic below `PIPE_BUF`; the lock is what keeps the
    /// history append and the JSONL line in the same order.
    lock: Mutex<()>,
    /// The id of the last `history.jsonl` record written, for `feedback`.
    last_id: Mutex<Option<String>>,
    /// Injected so tests get deterministic lines.
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
}

impl std::fmt::Debug for LogSpeech {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogSpeech")
            .field("path", &self.path)
            .field("history", &self.history.is_some())
            .finish()
    }
}

impl LogSpeech {
    /// Write utterances to `path`, and nothing to `history.jsonl`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            history: None,
            history_policy: None,
            lock: Mutex::new(()),
            last_id: Mutex::new(None),
            clock: Box::new(crate::log::now_epoch),
        }
    }

    /// Also append a `history.jsonl` record under `config_dir`, exactly as
    /// `daemon.py` does after a successful play.
    pub fn with_history(mut self, config_dir: impl Into<PathBuf>) -> Self {
        let history = History::new(config_dir);
        self.history = Some(match &self.history_policy {
            Some(p) => history.with_policy(std::sync::Arc::clone(p)),
            None => history,
        });
        self
    }

    /// What `history.jsonl` may keep of the user's words, read per append
    /// (default: everything; see `heard_state::history_policy`). Order with
    /// [`LogSpeech::with_history`] does not matter.
    pub fn with_history_policy(mut self, policy: heard_state::HistoryPolicySource) -> Self {
        self.history = self
            .history
            .take()
            .map(|h| h.with_policy(std::sync::Arc::clone(&policy)));
        self.history_policy = Some(policy);
        self
    }

    /// Replace the timestamp source. Tests pin the `ts` field with this.
    pub fn with_clock(mut self, clock: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Where the JSONL lands.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Speech for LogSpeech {
    fn speak(&self, utterance: &Utterance<'_>) {
        self.record(utterance, true);
    }

    /// `history: false` (a line with no history meta) is logged but not
    /// written to `history.jsonl`, as the Python's `if hmeta:`.
    fn speak_with(&self, utterance: &Utterance<'_>, opts: LineOptions) {
        self.record(utterance, opts.history);
    }

    fn last_utterance_id(&self) -> Option<String> {
        self.last_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl LogSpeech {
    fn record(&self, utterance: &Utterance<'_>, with_history: bool) {
        use std::io::Write as _;

        let line = serde_json::json!({
            "ts": (self.clock)(),
            "text": utterance.text,
            "tag": utterance.tag,
            "kind": utterance.kind,
            "session_id": utterance.session_id,
            "via": utterance.via,
        });

        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // ONE `write_all` of the whole line, never `writeln!`: a formatting
        // write on a bare `File` issues a syscall per fragment, and a reader
        // (the differential diff, a test) would see a torn line. Best-effort
        // beyond that, like every logging path in the Python daemon: a full
        // disk must never be the reason narration stops.
        let record = format!("{line}\n");
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(record.as_bytes()));
        if let Err(e) = written {
            crate::dlog!("speech_log_failed", err = e.kind().to_string());
        }

        if let Some(history) = self.history.as_ref().filter(|_| with_history) {
            // `daemon.py`: `history.append({**hmeta, "id", "session_id",
            // "spoken", "voice", "persona"})`. `voice` and `persona` belong
            // to the TTS selection, which the speech queue owns; they are written
            // empty rather than invented.
            let id = heard_state::history::new_utterance_id();
            *self.last_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
            history.append(&vec![
                ("kind".into(), PyValue::Str(utterance.kind.into())),
                ("tag".into(), PyValue::Str(utterance.tag.into())),
                ("via".into(), PyValue::Str(utterance.via.into())),
                ("repo_name".into(), PyValue::Str(utterance.project.into())),
                ("id".into(), PyValue::Str(id)),
                (
                    "session_id".into(),
                    PyValue::Str(utterance.session_id.into()),
                ),
                ("spoken".into(), PyValue::Str(utterance.text.into())),
            ]);
        }
    }
}

/// A sink that drops everything. Used by tests that only care about routing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSpeech;

impl Speech for NullSpeech {
    fn speak(&self, _utterance: &Utterance<'_>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_carries_every_documented_key() {
        let dir = crate::testing::temp_dir("speech");
        let sink = LogSpeech::new(dir.join("speech.jsonl")).with_clock(|| 1.5);
        sink.speak(&Utterance {
            text: "Editing auth.",
            tag: "tool_edit",
            kind: "tool_pre",
            session_id: "s1",
            via: "fastpath",
            project: "heard",
        });
        let body = std::fs::read_to_string(dir.join("speech.jsonl")).expect("written");
        let value: serde_json::Value = serde_json::from_str(body.trim()).expect("json");
        assert_eq!(value["ts"], 1.5);
        assert_eq!(value["text"], "Editing auth.");
        assert_eq!(value["tag"], "tool_edit");
        assert_eq!(value["kind"], "tool_pre");
        assert_eq!(value["session_id"], "s1");
        assert_eq!(value["via"], "fastpath");
    }

    #[test]
    fn history_is_written_through_the_same_sink() {
        let dir = crate::testing::temp_dir("speech-history");
        let sink = LogSpeech::new(dir.join("speech.jsonl")).with_history(&dir);
        sink.speak(&Utterance {
            text: "Done.",
            tag: "final_short",
            kind: "final",
            session_id: "s1",
            via: "floor",
            project: "heard",
        });
        let body = std::fs::read_to_string(dir.join("history.jsonl")).expect("history written");
        assert!(body.contains("\"spoken\": \"Done.\""), "{body}");
        assert!(body.contains("\"via\": \"floor\""), "{body}");
    }

    #[test]
    fn a_withholding_policy_blanks_a_quoting_line_in_history_only() {
        let dir = crate::testing::temp_dir("speech-history-policy");
        // Policy before history: the order must not matter.
        let sink = LogSpeech::new(dir.join("speech.jsonl"))
            .with_history_policy(heard_state::HistoryPolicy::withholding().fixed())
            .with_history(&dir);
        let line = |text, kind| Utterance {
            text,
            tag: kind,
            kind,
            session_id: "s1",
            via: "brain",
            project: "heard",
        };
        sink.speak(&line("Starting on SECRET-PROMPT.", "prompt_intent"));
        sink.speak(&line("All green.", "final"));
        let body = std::fs::read_to_string(dir.join("history.jsonl")).expect("history written");
        assert!(!body.contains("SECRET"), "{body}");
        assert!(body.contains("\"spoken\": \"\""), "{body}");
        assert!(body.contains("\"spoken\": \"All green.\""), "{body}");
    }
}
