//! The daemon itself: shared state, the narration routing (`_handle_event`),
//! and the socket dispatch (`_handle`).
//!
//! # What is real, what logs, what is a stub
//!
//! | command | here |
//! |---|---|
//! | `ping` | real — no reply, exactly as Python |
//! | `status` | real for everything the ported crates own; the speech-queue and account fields the core does not own are reported honestly (see [`Daemon::status`]) |
//! | `stop` | real — `Speech::cancel` + drains the pending routing state; also ends the accept loop unless built with `stop_shuts_down(false)` (the installed daemon: Python's `stop` only cancels) |
//! | `cancel` | real — `Speech::cancel` only |
//! | `voice_hold` / `voice_release` | real — [`Speech::hold`]`(User)` cuts narration and holds new lines; release stamps the user-engaged clock and replays them |
//! | `tour_hold` / `tour_release` | real — [`Speech::hold`]`(Tour)`: the queue is rescued into the held buffer, replayed on release |
//! | `subscribe` | real — the event push stream ([`crate::events`]) |
//! | `inject` | refused — `{"ok": false, "error": "not_supported"}` (typing is the app's job) |
//! | `mute` / `unmute` | real — persisted with `heard-config`'s `set_value`, the same key Python writes |
//! | `reload` | real — re-reads `config.yaml` into the cached snapshot |
//! | `mute_session` / `unmute_session` | real, with the exact reply shape |
//! | `pin` / `unpin` | real — `heard-state`'s `MultiAgentRouter` |
//! | `hook` | real — the whole of `hook_processing.py` (see [`crate::hooks`]) |
//! | `event` | real — routes through [`Daemon::handle_event`] |
//! | `event` + `health_probe` | real — echoes, with the same 32-char nonce and closed agent set |
//! | `speak` (the `cmd`-less fall-through) | reaches the [`Speech`] sink directly, as `via=direct` |
//! | `resume_intent` | real — the keyword classifier, then the extensions' `classify_resume_intent` (an edition's model fallback, off the accept loop), else "fresh" (the Python's no-model floor); catch-up flush or buffer drop |
//! | `feedback` | real — `history.append_feedback` against the sink's last utterance id |
//! | `report_defect` | offered to the [`Extension`]s first (an edition keeps the defect store); unclaimed, logged |
//! | any other `cmd` | offered to the [`Extension`]s in order; unclaimed, it is logged as `cmd_unhandled` and falls through to `speak`, exactly as Python's `_handle` runs off the end of its `if cmd == …` chain |
//!
//! # Where narration stops
//!
//! At the [`Speech`] sink. Nothing in this crate synthesises or plays audio.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use heard_config::{Config, Paths};
use heard_narrate::verbosity::{self, Cfg};
use heard_state::clock::{Clock, SystemClock};
use heard_state::multi_agent::{MultiAgentRouter, ProjectSummarizer};
use heard_state::{AgentEvent, AgentStateRegistry, SessionStore, SpokenStore};
use serde_json::{Map, Value};

use crate::brain::{Brain, BrainRequest, NoBrain};
use crate::capture::{Capture, NoCapture};
use crate::dedup::Dedup;
use crate::events::{EventBus, EventLine};
use crate::extension::{Extension, SpokenLine};
use crate::filler::{self, FillerPolicy};
use crate::floor;
use crate::policy::{self, EventView, FeatureState, Features, PolicyOutcome, Thunks};
use crate::shape;
use crate::speech::{Hold, NullSpeech, Speech, Utterance, VIA_FILLER, VIA_NOTICE};

/// `(_hung_track, _hung_alerted)`: session → (armed at, tool label), and the
/// sessions already nudged.
type HungWatch = (HashMap<String, (f64, String)>, HashSet<String>);

/// The persona fields routing reads: `persona.name` (the Focus alert seed)
/// and `persona.address` (the floor's form of address, "Sir").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersonaInfo {
    /// `persona.name`.
    pub name: String,
    /// `persona.address` — `""` when the persona uses none.
    pub address: String,
}

/// `persona.load(name, config_dir)`, reduced to the fields routing reads.
/// The runtime injects the full loader (`heard_brain::persona::load`, which
/// honours user personas under `config_dir/personas`).
pub trait PersonaSource: Send + Sync {
    /// The persona called `name` (Python: unknown → `raw`).
    fn load(&self, name: &str) -> PersonaInfo;
}

/// The bundled personas only (`engine/heard/personas/*.md` frontmatter).
#[derive(Debug, Default, Clone, Copy)]
pub struct BundledPersonas;

impl PersonaSource for BundledPersonas {
    fn load(&self, name: &str) -> PersonaInfo {
        match name {
            "jarvis" => PersonaInfo {
                name: "jarvis".into(),
                address: "Sir".into(),
            },
            "aria" | "friday" | "atlas" => PersonaInfo {
                name: name.into(),
                address: String::new(),
            },
            _ => PersonaInfo {
                name: "raw".into(),
                address: String::new(),
            },
        }
    }
}

/// `Daemon._recent_edit_paths`' `maxlen`.
const RECENT_EDIT_PATHS: usize = 8;

/// `needs_you._safe`.
fn needs_you_safe(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if s.is_empty() {
        "session".into()
    } else {
        s
    }
}

/// One narration event, owned, as `_handle_event` reads it.
///
/// Owned rather than borrowed because it crosses a task boundary: the hook
/// lane produces these on a per-session task and routes them there, and a
/// borrow would tie the event to the socket buffer it came from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NarrationEvent {
    /// `tool_pre` / `tool_post` / `intermediate` / `final` / `prompt_intent`.
    pub kind: String,
    /// The persona-free text.
    pub neutral: String,
    /// The routing tag.
    pub tag: String,
    /// Template / enrichment context.
    pub ctx: Map<String, Value>,
    /// The session id, `"default"` when the payload had none.
    pub session_id: String,
    /// The session's cwd, `""` when unknown.
    pub cwd: String,
}

impl NarrationEvent {
    /// The `heard-state` view of this event, for the Layer-2 observers.
    fn as_agent_event(&self) -> AgentEvent {
        serde_json::from_value(serde_json::json!({
            "kind": self.kind,
            "tag": self.tag,
            "neutral": self.neutral,
            "ctx": Value::Object(self.ctx.clone()),
            "session": { "id": self.session_id, "cwd": self.cwd },
        }))
        .unwrap_or_default()
    }
}

/// How a narration event ended. Returned so tests (and, later, the
/// differential tee) can assert on the routing without reading a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing was said, for this reason (the Python's `_log` reason string).
    Dropped(&'static str),
    /// Accumulated into the router's project digest.
    Digested,
    /// Handed to the [`Speech`] sink, via this lane.
    Spoke(&'static str),
}

/// The daemon's shared state. One instance, wrapped in an [`Arc`], shared by
/// the accept loop and every per-session hook task.
pub struct Daemon {
    /// Config access (load, `set_value`), rooted at the injected [`Paths`].
    pub config: Config,
    /// The merged config snapshot, refreshed by `reload` and by `mute`.
    cfg: Mutex<Map<String, Value>>,
    /// Solo / Swarm / Pinned routing and the project channel scheduler.
    pub router: Arc<MultiAgentRouter>,
    /// Layer 2 — the per-agent scoreboard.
    pub agent_states: Arc<AgentStateRegistry>,
    /// Session → host identity, fed by every hook frame's `binding`.
    pub session_identities: Arc<heard_state::SessionIdentityRegistry>,
    /// `_last_user_engaged` — wall clock of the user's last engagement (a
    /// prompt submitted, push-to-talk released). "Catch me up" covers the
    /// time since.
    last_user_engaged: Mutex<f64>,
    /// Per-session tool density, for the burst→digest rule.
    pub sessions: Arc<SessionStore>,
    /// Per-session dedup of already-narrated assistant prose.
    pub spoken: Arc<SpokenStore>,
    /// Where a would-be utterance goes.
    pub speech: Arc<dyn Speech>,
    /// The narration brain. [`NoBrain`] in this lane.
    pub brain: Arc<dyn Brain>,
    /// The project-flush narrative summarizer. `TemplateOnly` in this lane.
    pub summarizer: Arc<dyn ProjectSummarizer + Send + Sync>,
    /// The capture seam's two daemon call sites. [`NoCapture`] by default.
    pub capture: Arc<dyn Capture>,
    /// The [`Extension`]s, in the order the builder received them.
    extensions: Vec<Arc<dyn Extension>>,
    /// Live `subscribe` clients: every [`Daemon::emit_event`] goes to each.
    events: EventBus,
    /// The monotonic clock the timers read (hung tool, nudges, resume).
    clock: Arc<dyn Clock>,
    /// Index picker for the canned filler lines (`random.choice`).
    picker: Mutex<Box<dyn FnMut(usize) -> usize + Send>>,
    /// `_hung_track` / `_hung_alerted` — Focus mode's hung-tool watch.
    hung: Mutex<HungWatch>,
    /// `_turn_open` / `_turn_nudged` — the companion "still thinking" nudge.
    turns: Mutex<(HashMap<String, f64>, HashSet<String>)>,
    /// `_fillers` — the one dead-air budget, and the two `_last_*` caches.
    fillers: Mutex<(FillerPolicy, Option<usize>, Option<usize>)>,
    /// `_awaiting_resume_intent` + its safety timer's deadline (monotonic).
    awaiting_resume: Mutex<Option<f64>>,
    /// Every extension's [`Extension::verbatim_kinds`], lowercased.
    verbatim_kinds: Vec<String>,
    /// What `status.backend` reports: the sink's honest name.
    backend_name: String,
    dedup: Dedup,
    muted_sessions: Mutex<HashSet<String>>,
    /// Sessions whose next `intermediate` is the turn opener.
    opener_pending: Mutex<HashSet<String>>,
    /// The prompt that opened each session's current turn.
    last_prompt: Mutex<HashMap<String, String>>,
    /// `_skills_announced` — (session, skill) pairs whose first use spoke.
    skills_announced: Mutex<HashSet<(String, String)>>,
    /// `_recent_edit_paths` — the last few edited files narrated on the fast
    /// path; a repeat edit routes to the brain instead.
    recent_edit_paths: Mutex<VecDeque<String>>,
    /// `_volume_budget` — routine words spoken in the last minute.
    volume_budget: Mutex<policy::Budget>,
    /// `_prompt_watch_announced` — session → when the voice prompt-watcher
    /// announced its spooled question. Nothing in the Rust daemon announces
    /// one yet, so this only ever holds what [`Daemon::note_prompt_announced`]
    /// records.
    prompt_watch_announced: Mutex<HashMap<String, std::time::Instant>>,
    /// `persona.load`, for the floor's address and the Focus alert seed.
    pub personas: Arc<dyn PersonaSource>,
    /// Whether `stop` also ends the accept loop (see [`Daemon::stop`]).
    stop_shuts_down: bool,
    /// Set by a shutting-down `stop`; the accept loop exits and the runtime unwinds.
    stopping: Arc<AtomicBool>,
    /// Woken by `stop`, so the accept loop does not have to wait for one more
    /// connection before it notices.
    shutdown: Arc<tokio::sync::Notify>,
    /// `--differential <python-sock-path>`, parsed and carried. The tee
    /// itself lives in the hook path and is the next lane.
    pub differential: Option<PathBuf>,
    /// What `history.jsonl` may keep of the user's words (the `feedback`
    /// command's text), read per append.
    history_policy: heard_state::HistoryPolicySource,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("paths", self.config.paths())
            .field("differential", &self.differential)
            .finish_non_exhaustive()
    }
}

/// Builder for a [`Daemon`]. Every seam is injected so a test can drive the
/// whole pipeline without a config file, an LLM or a socket.
pub struct DaemonBuilder {
    paths: Paths,
    speech: Arc<dyn Speech>,
    brain: Arc<dyn Brain>,
    summarizer: Arc<dyn ProjectSummarizer + Send + Sync>,
    capture: Arc<dyn Capture>,
    extensions: Vec<Arc<dyn Extension>>,
    agent_states: Option<Arc<AgentStateRegistry>>,
    session_identities: Option<Arc<heard_state::SessionIdentityRegistry>>,
    backend_name: String,
    stop_shuts_down: bool,
    differential: Option<PathBuf>,
    personas: Arc<dyn PersonaSource>,
    events: EventBus,
    clock: Option<Arc<dyn Clock>>,
    picker: Option<Box<dyn FnMut(usize) -> usize + Send>>,
    history_policy: Option<heard_state::HistoryPolicySource>,
}

impl DaemonBuilder {
    /// Start from a resolved [`Paths`].
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            speech: Arc::new(NullSpeech),
            brain: Arc::new(NoBrain),
            summarizer: Arc::new(heard_state::multi_agent::TemplateOnly),
            capture: Arc::new(NoCapture),
            extensions: Vec::new(),
            agent_states: None,
            session_identities: None,
            backend_name: "LogSpeech".into(),
            stop_shuts_down: true,
            differential: None,
            personas: Arc::new(BundledPersonas),
            events: EventBus::new(),
            clock: None,
            picker: None,
            history_policy: None,
        }
    }

    /// What `history.jsonl` may keep of the user's words when the daemon
    /// itself appends (`feedback`). Default:
    /// [`crate::speech::config_history_policy`] over this daemon's config.
    /// Hand the SAME source to the speech sink's history.
    pub fn history_policy(mut self, policy: heard_state::HistoryPolicySource) -> Self {
        self.history_policy = Some(policy);
        self
    }

    /// The event bus `subscribe` streams (default: a fresh one). Share one
    /// with the speech queue so its `speech_started` / `speech_finished`
    /// reach the same subscribers.
    pub fn events(mut self, events: EventBus) -> Self {
        self.events = events;
        self
    }

    /// The monotonic clock for the daemon's timers (default: the system's).
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// The index picker for the canned filler lines: `pick(n)` → `0..n`
    /// (default: a time-seeded xorshift). Tests script it.
    pub fn picker(mut self, pick: impl FnMut(usize) -> usize + Send + 'static) -> Self {
        self.picker = Some(Box::new(pick));
        self
    }

    /// `persona.load` (default [`BundledPersonas`]).
    pub fn personas(mut self, personas: Arc<dyn PersonaSource>) -> Self {
        self.personas = personas;
        self
    }

    /// The capture seam (default [`NoCapture`]).
    pub fn capture(mut self, capture: Arc<dyn Capture>) -> Self {
        self.capture = capture;
        self
    }

    /// Append one [`Extension`]. Extensions are consulted in the order they
    /// were added (see [`crate::extension`]).
    pub fn extension(mut self, extension: Arc<dyn Extension>) -> Self {
        self.extensions.push(extension);
        self
    }

    /// Replace the whole ordered set of [`Extension`]s (default: none).
    pub fn extensions(mut self, extensions: Vec<Arc<dyn Extension>>) -> Self {
        self.extensions = extensions;
        self
    }

    /// Layer 2's registry (default: a fresh `AgentStateRegistry::new()`).
    /// A composition root builds it first when something wired before the
    /// daemon exists (an extension) must read the SAME registry
    /// `handle_event` updates.
    pub fn agent_states(mut self, registry: Arc<AgentStateRegistry>) -> Self {
        self.agent_states = Some(registry);
        self
    }

    /// The session-identity registry (default: a fresh one on the real
    /// clock). Shared the same way as [`DaemonBuilder::agent_states`].
    pub fn session_identities(
        mut self,
        registry: Arc<heard_state::SessionIdentityRegistry>,
    ) -> Self {
        self.session_identities = Some(registry);
        self
    }

    /// The project-flush summarizer (default `TemplateOnly`).
    pub fn summarizer(mut self, summarizer: Arc<dyn ProjectSummarizer + Send + Sync>) -> Self {
        self.summarizer = summarizer;
        self
    }

    /// What `status.backend` says (default `LogSpeech`).
    pub fn backend_name(mut self, name: impl Into<String>) -> Self {
        self.backend_name = name.into();
        self
    }

    /// Whether `stop` also shuts the daemon down (default `true`). The
    /// installed daemon passes `false`: Python's `stop` only cancels speech,
    /// and the voice serve sends it on every push-to-talk press.
    pub fn stop_shuts_down(mut self, yes: bool) -> Self {
        self.stop_shuts_down = yes;
        self
    }

    /// Where would-be utterances go.
    pub fn speech(mut self, speech: Arc<dyn Speech>) -> Self {
        self.speech = speech;
        self
    }

    /// The narration brain.
    pub fn brain(mut self, brain: Arc<dyn Brain>) -> Self {
        self.brain = brain;
        self
    }

    /// The Python daemon's socket, for the differential tee (stub in this lane).
    pub fn differential(mut self, path: Option<PathBuf>) -> Self {
        self.differential = path;
        self
    }

    /// Build it, loading the config snapshot once.
    pub fn build(self) -> Arc<Daemon> {
        let config = Config::new(self.paths);
        let cfg = config.load(None).unwrap_or_else(|e| {
            crate::dlog!("config_load_failed", err = e.to_string());
            Map::new()
        });
        let config_dir = config.paths().config_dir.clone();
        let mut verbatim_kinds: Vec<String> = Vec::new();
        for ext in &self.extensions {
            for k in ext.verbatim_kinds() {
                let k = k.to_lowercase();
                if !verbatim_kinds.contains(&k) {
                    verbatim_kinds.push(k);
                }
            }
        }
        let history_policy = self
            .history_policy
            .unwrap_or_else(|| crate::speech::config_history_policy(config.clone()));
        Arc::new(Daemon {
            history_policy,
            config,
            cfg: Mutex::new(cfg),
            router: Arc::new(MultiAgentRouter::new()),
            agent_states: self
                .agent_states
                .unwrap_or_else(|| Arc::new(AgentStateRegistry::new())),
            session_identities: self.session_identities.unwrap_or_else(|| {
                Arc::new(heard_state::SessionIdentityRegistry::new(Arc::new(
                    heard_state::SystemClock::new(),
                )))
            }),
            last_user_engaged: Mutex::new(crate::log::now_epoch()),
            sessions: Arc::new(SessionStore::new()),
            spoken: Arc::new(SpokenStore::new(config_dir)),
            speech: self.speech,
            brain: self.brain,
            summarizer: self.summarizer,
            capture: self.capture,
            extensions: self.extensions,
            events: self.events,
            clock: self.clock.unwrap_or_else(|| Arc::new(SystemClock::new())),
            picker: Mutex::new(self.picker.unwrap_or_else(|| Box::new(xorshift_picker()))),
            hung: Mutex::new((HashMap::new(), HashSet::new())),
            turns: Mutex::new((HashMap::new(), HashSet::new())),
            fillers: Mutex::new((FillerPolicy::default(), None, None)),
            awaiting_resume: Mutex::new(None),
            verbatim_kinds,
            backend_name: self.backend_name,
            stop_shuts_down: self.stop_shuts_down,
            dedup: Dedup::new(),
            muted_sessions: Mutex::new(HashSet::new()),
            opener_pending: Mutex::new(HashSet::new()),
            last_prompt: Mutex::new(HashMap::new()),
            skills_announced: Mutex::new(HashSet::new()),
            recent_edit_paths: Mutex::new(VecDeque::new()),
            volume_budget: Mutex::new(policy::Budget::default()),
            prompt_watch_announced: Mutex::new(HashMap::new()),
            personas: self.personas,
            stopping: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            differential: self.differential,
        })
    }
}

impl Daemon {
    /// The `history.jsonl` policy source this daemon was built with — the
    /// one question "may the user's words be kept?", for extensions with
    /// stores of their own.
    pub fn history_policy(&self) -> heard_state::HistoryPolicySource {
        Arc::clone(&self.history_policy)
    }

    /// The merged config snapshot, as a JSON object (what
    /// [`heard_narrate::verbosity::Cfg`] reads).
    pub fn cfg_value(&self) -> Value {
        Value::Object(self.cfg.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }

    /// One key out of the snapshot.
    pub fn cfg_get(&self, key: &str) -> Option<Value> {
        self.cfg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    fn cfg_bool(&self, key: &str, default: bool) -> bool {
        match self.cfg_get(key) {
            None => default,
            Some(Value::Null) => false,
            Some(Value::Bool(b)) => b,
            Some(Value::Number(n)) => n.as_f64() != Some(0.0),
            Some(Value::String(s)) => !s.is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
        }
    }

    /// `hook_processing.is_muted()` — the persisted "Pause Heard" flag, read
    /// from DISK, not from the snapshot.
    ///
    /// It has to be the disk value: the flag is flipped by the menu bar and
    /// the hotkey in another process, and a snapshot taken at startup would
    /// let a paused Heard keep narrating. Python re-reads `config.load()` on
    /// every hook for exactly this reason.
    pub fn is_muted(&self) -> bool {
        self.config
            .load(None)
            .map(|cfg| matches!(cfg.get("muted"), Some(Value::Bool(true))))
            .unwrap_or(false)
    }

    /// `cfg["flush_delay_ms"]` — how long to wait for the agent to flush its
    /// transcript before reading it.
    pub fn flush_delay(&self) -> std::time::Duration {
        let ms = self
            .cfg_get("flush_delay_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(800);
        std::time::Duration::from_millis(ms)
    }

    /// `cfg["skip_under_chars"]`.
    pub fn skip_under_chars(&self) -> usize {
        self.cfg_get("skip_under_chars")
            .and_then(|v| v.as_u64())
            .unwrap_or(30) as usize
    }

    /// `_reload_config` — re-read `config.yaml` into the snapshot.
    pub fn reload(&self) {
        match self.config.load(None) {
            Ok(cfg) => {
                *self.cfg.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
                crate::dlog!("config_reloaded");
            }
            Err(e) => crate::dlog!("config_reload_failed", err = e.to_string()),
        }
    }

    /// `_do_mute(source=…)`. Persists `muted=true` through
    /// `heard-config`'s `set_value`, exactly as Python's `config.set_value`
    /// does — same key, same cross-process lock, same swallowed failure.
    pub fn mute(&self, source: &str) {
        self.cfg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("muted".into(), Value::Bool(true));
        if let Err(e) = self.config.set_value("muted", Value::Bool(true)) {
            crate::dlog!("mute_persist_failed", err = e.to_string());
        }
        // "Pausing means quiet" — silence what is playing (`_do_mute` cancels
        // first) and drop what the sink was holding to replay after
        // dictation, so unpausing later does not dump stale lines. The
        // router's digest buffer is KEPT: it is what the resume panel offers
        // to catch the user up on ("catch you up, or start fresh?").
        self.speech.cancel();
        self.speech.discard_held();
        self.clear_awaiting_resume();
        crate::dlog!("muted", source = source);
    }

    /// `_do_unmute(source=…)`.
    pub fn unmute(&self, source: &str) {
        self.cfg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("muted".into(), Value::Bool(false));
        if let Err(e) = self.config.set_value("muted", Value::Bool(false)) {
            crate::dlog!("unmute_persist_failed", err = e.to_string());
        }
        crate::dlog!("unmuted", source = source);
        // Arm the resume-intent flow when anything is buffered: the UI sees
        // `awaiting_resume_intent` + `pending_count` in `status` and asks.
        let pending = self.router.pending_count();
        if pending == 0 {
            return;
        }
        *self
            .awaiting_resume
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(self.now() + RESUME_INTENT_TIMEOUT_S);
        let plural = if pending != 1 { "s" } else { "" };
        crate::dlog!("resume_welcome_spoken", pending = pending);
        self.say_line(&Line {
            coexists: true,
            history: false,
            ..Line::new(
                &format!(
                    "Welcome back. While you were away, I queued up {pending} thing{plural}. \
                     Catch you up, or start fresh?"
                ),
                "",
                "",
                "__resume__",
                VIA_NOTICE,
            )
        });
        crate::dlog!("resume_intent_armed", pending = pending);
    }

    /// `cmd == "mute_session"`.
    pub fn mute_session(&self, session_id: &str) {
        self.muted_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_owned());
        // Flush anything already queued from this session so it goes quiet
        // immediately, not after the backlog drains.
        let flushed = self.speech.drop_session(session_id);
        crate::dlog!(
            "session_muted",
            session = head8(session_id),
            flushed = flushed
        );
    }

    /// `cmd == "unmute_session"`.
    pub fn unmute_session(&self, session_id: &str) {
        self.muted_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        crate::dlog!("session_unmuted", session = head8(session_id));
    }

    /// `cmd == "stop"` — `_cancel_only`: silence the speech sink, drop the
    /// router's held narration, and — unless this daemon was built with
    /// `stop_shuts_down(false)` — ask the accept loop to finish.
    ///
    /// In Python `stop` ONLY cancels (the voice serve sends it on every
    /// push-to-talk press to barge in over narration); shutting down is the
    /// CLI's `heard stop`, which also kills the pid. A standalone Rust daemon
    /// keeps the shutdown (it is how a terminal run is ended cleanly), and the
    /// installed one turns it off so a key press
    /// never takes narration down.
    pub fn stop(&self) {
        self.speech.cancel();
        let dropped = self.router.clear_pending();
        if self.stop_shuts_down {
            self.stopping.store(true, Ordering::SeqCst);
            self.shutdown.notify_waiters();
        }
        crate::dlog!("stop", dropped = dropped, shutdown = self.stop_shuts_down);
    }

    /// `cmd == "cancel"` — the notch's Mute/Pause cut: stop what is playing
    /// now, change no state.
    pub fn cancel(&self) {
        self.speech.cancel();
        crate::dlog!("cancel");
    }

    /// True once `stop` has been received.
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// The shutdown signal, for the accept loop.
    pub(crate) fn shutdown_signal(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.shutdown)
    }

    /// `cmd == "status"` — the menu bar's whole picture.
    ///
    /// Every field the ported crates can answer is real. The rest are
    /// reported as what they actually are in this lane rather than invented:
    /// `backend` is `LogSpeech` (the sink's name, which is the truth — there
    /// is no TTS), `speaking`/`queued` are `false`/`0` because there is no
    /// speech queue yet, and `account_usage` / `pending_update` are `null`
    /// because `heard_api.py` and `updater.py` are not ported.
    pub fn status(&self) -> heard_proto::StatusResponse {
        let active: Vec<Value> = self
            .router
            .list_active()
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "session_id": s.session_id,
                    "repo_name": s.repo_name,
                    "last_event_ago_s": s.last_event_ago_s,
                    "pinned": s.pinned,
                })
            })
            .collect();
        let (speaking, queued) = self.speech.queue_state();
        let mut status = heard_proto::StatusResponse {
            alive: true,
            backend: self.backend_name.clone(),
            persona: self
                .cfg_get("persona")
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            narrate_tools: self.cfg_bool("narrate_tools", true),
            muted: self.cfg_bool("muted", false),
            last_error: None,
            account_usage: None,
            speaking,
            queued: queued as i64,
            active_sessions: Value::Array(active),
            router_mode: self.router.mode().as_str().to_owned(),
            agent_states: serde_json::to_value(self.agent_states.summary()).unwrap_or(Value::Null),
            langgraph_runs: Value::Array(Vec::new()),
            recap: String::new(),
            mission_agents: Value::Array(Vec::new()),
            pending_count: self.router.pending_count() as i64,
            awaiting_resume_intent: self.is_awaiting_resume(),
            pending_update: None,
        };
        for ext in &self.extensions {
            ext.status_fields(&mut status);
        }
        status
    }

    /// Silence one session: drop its queued lines and cut its line if it is
    /// the one playing ([`Speech::cut_session`]). Returns how many queued
    /// lines were dropped.
    pub fn cut_session(&self, session_id: &str) -> usize {
        self.speech.cut_session(session_id)
    }

    // ---- narration routing -------------------------------------------------

    /// `Daemon._handle_event`: the gates before the policy, the ported
    /// `narration_policy` table ([`policy::decide`]) over the same features
    /// the Python computes, and the outcome it picks.
    ///
    /// Order, as in `daemon.py`:
    ///
    /// 1. duplicate-event suppression;
    /// 2. True Pause (`paused`) — drop before ANY observation;
    /// 3. Layer-2 observation (the scoreboard) and every extension's
    ///    `observe_event`;
    /// 4. the first-run hold (`first_run_hold`);
    /// 5. per-session mute (`/quiet`);
    /// 6. `needs_you` record / clear;
    /// 7. the policy table; `speakup_off` returns before the session is
    ///    touched, every other outcome touches it and notes the router;
    /// 8. drop / retire the prompt / digest / Focus alert / template / model.
    ///    The model lane is the [`Brain`]; its punt takes the [`floor`], with
    ///    the persona's form of address.
    ///
    /// Not ported (no Rust owner yet): the Focus hung-tool timer, the
    /// companion "still thinking" nudge and submit filler, the brain lane's
    /// anti-repeat / per-project cooldown for intermediates.
    pub fn handle_event(&self, event: &NarrationEvent) -> Outcome {
        // D0.1d: the raw agent-event stream, UNCONDITIONALLY, before any
        // drop/dedup/pause gate — the ledger is ground truth for what the
        // daemon RECEIVED; the decision below is a separate candidate row.
        let agent_kind = crate::capture::is_agent_event_kind(&event.kind);
        if agent_kind {
            self.capture.event_received(&event.kind);
        }
        let outcome = self.route_event(event);
        if agent_kind {
            match &outcome {
                Outcome::Spoke(_) => self.capture.decision(&event.kind, true, None),
                Outcome::Dropped(reason) => self.capture.decision(&event.kind, false, Some(reason)),
                // `event_deferred` is not `event_drop` in `_log`, so Python
                // writes no candidate for a digested event either.
                Outcome::Digested => {}
            }
        }
        outcome
    }

    fn route_event(&self, event: &NarrationEvent) -> Outcome {
        let kind = event.kind.as_str();
        let tag = event.tag.as_str();
        let neutral = event.neutral.trim();
        let session_id = event.session_id.as_str();

        if self
            .dedup
            .is_duplicate_event(session_id, kind, tag, neutral)
        {
            crate::dlog!(
                "event_drop",
                kind = kind,
                tag = tag,
                reason = "duplicate_event"
            );
            return Outcome::Dropped("duplicate_event");
        }

        // True Pause: drop before any observation — no state, no brain, no
        // cost. Distinct from `muted`, which still observes.
        if self.cfg_bool("paused", false) {
            crate::dlog!("event_drop", kind = kind, tag = tag, reason = "paused");
            return Outcome::Dropped("paused");
        }

        // Focus-mode hung-tool watch (`HUNG_TOOL_S`): a `tool_pre` arms the
        // timer; ANY later event from the session means it is progressing.
        // Tracked in every mode (cheap), only VOICED in focus (see
        // [`Daemon::tick`]); before the narration gates so a focus-dropped
        // `tool_pre` is still watched.
        {
            let mut hung = self.hung.lock().unwrap_or_else(|e| e.into_inner());
            if kind == "tool_pre" {
                let label = if neutral.is_empty() {
                    "a tool"
                } else {
                    neutral
                };
                hung.0
                    .insert(session_id.to_owned(), (self.now(), label.trim().to_owned()));
            } else {
                hung.0.remove(session_id);
                hung.1.remove(session_id);
            }
        }

        // Layer 2 — always-on, deterministic, never an LLM. Before every
        // narration gate: the scoreboard reflects what the agent DID, not
        // what we chose to narrate. Extensions observe at the same point.
        let agent_event = event.as_agent_event();
        self.agent_states.observe(&agent_event);
        if !self.extensions.is_empty() {
            let observed = serde_json::json!({
                "kind": event.kind,
                "tag": event.tag,
                "neutral": event.neutral,
                "session": { "id": event.session_id, "cwd": event.cwd },
            });
            for ext in &self.extensions {
                ext.observe_event(&observed);
            }
        }

        let cfg_value = self.cfg_value();
        let empty = Map::new();
        let cfg_map = cfg_value.as_object().unwrap_or(&empty);

        // The installation-wide first-run hold — stop before
        // routing, brain work, timers or narration.
        if policy::first_run_held(cfg_map) {
            crate::dlog!(
                "event_drop",
                kind = kind,
                tag = tag,
                reason = "first_run_hold"
            );
            return Outcome::Dropped("first_run_hold");
        }

        if self
            .muted_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(session_id)
        {
            crate::dlog!(
                "event_drop",
                kind = kind,
                tag = tag,
                reason = "session_muted",
                session = head8(session_id)
            );
            return Outcome::Dropped("session_muted");
        }

        // Blocked on something only the user can do: record it BEFORE any
        // narration gate, so the notch's NEEDS YOU list shows it even when
        // the category is muted.
        if tag == "tool_post_needs_you" {
            let agent = self
                .router
                .session_info(session_id)
                .map(|i| i.repo_name)
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "an agent".into());
            self.needs_you_record(session_id, &agent, neutral);
        } else if kind == "final" {
            self.needs_you_clear(session_id);
        }

        // ---- the narration policy (heard.narration_policy) ------------------
        let cfg = Cfg(&cfg_value);
        let persona = self.persona_for(cfg_map);
        let mode = policy::narration_mode(cfg_map);
        let focus_mode = mode == "focus";
        let abs_path = event
            .ctx
            .get("abs_path")
            .and_then(Value::as_str)
            .unwrap_or("");
        let view = EventView {
            kind,
            tag,
            neutral: &event.neutral,
            session_id,
            abs_path,
        };
        let skill_key = (
            session_id.to_owned(),
            match event.ctx.get("skill") {
                Some(v) if policy::py_truthy(v) => policy::py_str(v),
                _ => String::new(),
            },
        );
        let narrate_routine = policy::cfg_truthy(cfg_map, "narrate_routine", false);
        let critical = policy::is_critical_template_event(&view);
        let recent_edits: Vec<String> = self
            .recent_edit_paths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        let first_skill = tag == "tool_skill"
            && !self
                .skills_announced
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&skill_key);
        let feats = Features::build(
            &view,
            cfg_map,
            FeatureState {
                persona_name: &persona.name,
                first_skill,
                multi_agent_active: self.router.list_active().len() > 1,
                recent_edit_paths: &recent_edits,
                harness_enabled: self.brain.is_enabled(),
            },
            Thunks {
                prompt_watch_owns: Box::new(|| self.prompt_watch_owns(cfg_map, session_id)),
                verbosity_pre: Box::new(|| {
                    let density = self.sessions.tool_density(session_id) as i64;
                    self.sessions.record_tool_event(session_id);
                    verbosity::classify_pre(&cfg, tag, density)
                }),
                verbosity_post: Box::new(|| verbosity::classify_post(&cfg, tag)),
                verbosity_prose: Box::new(|| verbosity::classify_prose(&cfg)),
                duplicate_tool_line: Box::new(|| {
                    self.dedup.is_duplicate_tool_line(session_id, neutral)
                }),
                budget_exhausted: Box::new(|| {
                    let limit = policy::budget_wpm(
                        cfg_map.get("narration_volume").unwrap_or(&Value::from(-1)),
                    );
                    self.volume_budget
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .exhausted(limit, std::time::Instant::now())
                }),
            },
        );
        let decision = policy::decide(&feats);
        drop(feats);

        // "Speak up on" drops before the session is even marked active.
        if decision.rule == "speakup_off" {
            crate::dlog!("event_drop", kind = kind, tag = tag, reason = "speakup_off");
            return Outcome::Dropped("speakup_off");
        }
        self.sessions.touch(
            session_id,
            Some(event.cwd.as_str()).filter(|c| !c.is_empty()),
        );
        // `abs_path` is the load-bearing signal for project attribution.
        let path_hint = Some(abs_path).filter(|p| !p.is_empty());
        self.router.note_event(session_id, &event.cwd, path_hint);

        let project = || {
            self.router
                .session_info(session_id)
                .map(|info| info.repo_name)
                .unwrap_or_default()
        };

        match decision.outcome {
            PolicyOutcome::Drop => {
                crate::dlog!("event_drop", kind = kind, tag = tag, reason = decision.rule);
                return Outcome::Dropped(decision.rule);
            }
            PolicyOutcome::RetirePrompt => {
                // A new user prompt opens a turn: arm the opener so the first
                // intermediate that follows is force-spoken. The event itself
                // stays retired.
                if !session_id.is_empty() {
                    self.opener_pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(session_id.to_owned());
                    // Arm the companion "still thinking" nudge for this turn,
                    // and open a fresh (single) filler budget.
                    {
                        let mut turns = self.turns.lock().unwrap_or_else(|e| e.into_inner());
                        turns.0.insert(session_id.to_owned(), self.now());
                        turns.1.remove(session_id);
                    }
                    self.fillers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                        .open_turn(session_id);
                    if !neutral.is_empty() {
                        self.last_prompt
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(session_id.to_owned(), neutral.to_owned());
                        self.note_user_engaged();
                    }
                }
                // The instant canned filler ("On it."): companion only, and
                // only into real silence (the shared dead-air budget).
                if !session_id.is_empty() && filler::should_prompt_filler(cfg_map) {
                    let (ok, why) = self
                        .fillers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                        .allow(session_id, self.now());
                    if ok {
                        let line = self.pick_filler(false);
                        self.say_filler(line, session_id);
                    } else {
                        crate::dlog!(
                            "filler_suppressed",
                            session = session_id,
                            reason = why,
                            at = "prompt_submit"
                        );
                    }
                }
                crate::dlog!("event_drop", kind = kind, reason = "prompt_intent_retired");
                return Outcome::Dropped("prompt_intent_retired");
            }
            PolicyOutcome::Digest => {
                self.router.add_to_digest(
                    session_id,
                    kind,
                    tag,
                    neutral,
                    Some(Value::Object(event.ctx.clone())),
                );
                crate::dlog!(
                    "event_deferred",
                    kind = kind,
                    tag = tag,
                    reason = decision.rule
                );
                return Outcome::Digested;
            }
            PolicyOutcome::SpeakFocusAlert => {
                let alert = policy::focus_prompt_speech(&view, &persona.name);
                return self.emit(event, &alert, &project(), "focus_alert");
            }
            PolicyOutcome::SpeakTemplate => {
                let spoken = if focus_mode && policy::is_focus_template_event(&view) {
                    policy::focus_prompt_speech(&view, &persona.name)
                } else {
                    neutral.to_owned()
                };
                if decision.rule == "first_skill_use_speaks" {
                    self.skills_announced
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(skill_key);
                }
                // Track this edit's path so the NEXT edit to the same file
                // routes through the brain ("Editing X" × 3 is noise).
                if matches!(tag, "tool_edit" | "tool_write" | "tool_notebook_edit")
                    && !abs_path.is_empty()
                {
                    let mut recent = self
                        .recent_edit_paths
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if recent.len() == RECENT_EDIT_PATHS {
                        recent.pop_front();
                    }
                    recent.push_back(abs_path.to_owned());
                }
                return self.emit(event, &spoken, &project(), "fastpath");
            }
            PolicyOutcome::Model => {}
        }
        let project = project();

        // ---- the model lane: the brain ----------------------------------------
        let is_opener = kind == "intermediate"
            && self
                .opener_pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(session_id);
        let last_prompt = if kind == "final" {
            self.last_prompt
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(session_id)
                .cloned()
                .unwrap_or_default()
        } else {
            String::new()
        };

        if self.brain.is_enabled() {
            let request = BrainRequest {
                kind,
                tag,
                neutral,
                session_id,
                cwd: &event.cwd,
                project: &project,
                is_opener,
                last_prompt: &last_prompt,
            };
            match self.brain.narrate(&request) {
                Some(decision) => {
                    if let Some(think) = &decision.think {
                        crate::dlog!(
                            "harness_think",
                            kind = kind,
                            tag = tag,
                            text = think.as_str()
                        );
                    }
                    if decision.speak {
                        let text = if focus_mode {
                            let lead = floor::final_lead(&decision.text, 140);
                            if lead.is_empty() {
                                decision.text.clone()
                            } else {
                                lead
                            }
                        } else {
                            decision.text.clone()
                        };
                        return self.emit(event, &text, &project, "brain");
                    }
                    // Legacy brains do not own human attention, so preserve
                    // the original final/opener floor. A bounded attention
                    // brain can explicitly make deliberate silence final.
                    let authoritative_silence = self.brain.silence_is_authoritative(&request);
                    if kind == "final" && !focus_mode && !authoritative_silence {
                        crate::dlog!(
                            "harness_skip_override",
                            kind = kind,
                            tag = tag,
                            reason = "final_always_speaks"
                        );
                        let text = floor::floor_text(kind, neutral, &persona.address, &project);
                        if !text.is_empty() {
                            return self.emit(event, &text, &project, "floor");
                        }
                    }
                    if is_opener && !focus_mode && !authoritative_silence {
                        let lead = floor::final_lead(neutral, 160);
                        if !lead.is_empty() {
                            crate::dlog!(
                                "harness_skip_override",
                                kind = kind,
                                tag = tag,
                                reason = "opener_always_speaks"
                            );
                            return self.emit(event, &lead, &project, "floor");
                        }
                    }
                    crate::dlog!(
                        "event_drop",
                        kind = kind,
                        tag = tag,
                        reason = "harness_skip"
                    );
                    return Outcome::Dropped("harness_skip");
                }
                None => crate::dlog!("event_harness_punt", kind = kind, tag = tag),
            }
        } else {
            crate::dlog!(
                "event_drop",
                kind = kind,
                tag = tag,
                reason = "harness_disabled"
            );
            return Outcome::Dropped("harness_disabled");
        }

        // ---- the no-LLM floor -------------------------------------------------
        if focus_mode {
            crate::dlog!(
                "event_drop",
                kind = kind,
                tag = tag,
                reason = "focus_harness_punt"
            );
            return Outcome::Dropped("focus_harness_punt");
        }
        // Co-pilot suppresses the low-signal tool tier here too: with 2+
        // agents a tool event skips the fast path and lands here, and the
        // floor would otherwise read its raw template.
        if mode == "copilot"
            && !narrate_routine
            && (kind == "tool_pre" || kind == "tool_post")
            && !critical
        {
            crate::dlog!(
                "event_drop",
                kind = kind,
                tag = tag,
                reason = "copilot_tool_tier_suppressed"
            );
            return Outcome::Dropped("copilot_tool_tier_suppressed");
        }
        let text = floor::floor_text(kind, neutral, &persona.address, &project);
        if text.is_empty() {
            crate::dlog!("event_drop", kind = kind, tag = tag, reason = "floor_drop");
            return Outcome::Dropped("floor_drop");
        }
        self.emit(event, &text, &project, "floor")
    }

    /// `_persona_for(cfg)` — `cfg.get("persona", "raw")`, loaded.
    fn persona_for(&self, cfg: &Map<String, Value>) -> PersonaInfo {
        let name = match cfg.get("persona") {
            None => "raw".to_string(),
            Some(v) => policy::py_str(v),
        };
        self.personas.load(&name)
    }

    /// The Python's `_prompt_watch_owns` thunk: the voice prompt-watcher
    /// announces SPOOLED questions with a richer, answerable line, so this
    /// transcript copy must not speak them twice. The `tool_pre` event and
    /// the question hook's spool write race, so poll briefly (8 × 100 ms).
    fn prompt_watch_owns(&self, cfg: &Map<String, Value>, session_id: &str) -> bool {
        if !policy::cfg_truthy(cfg, "voice_prompt_announce", true) {
            return false;
        }
        let qsid = if session_id.is_empty() || session_id == "default" {
            ""
        } else {
            session_id
        };
        let spool = (!qsid.is_empty()).then(|| {
            self.config
                .paths()
                .heard_dir
                .join("pending-questions")
                .join(format!("{qsid}.json"))
        });
        let owned = || -> bool {
            if !qsid.is_empty() {
                let announced = self
                    .prompt_watch_announced
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(qsid)
                    .copied();
                if announced.is_some_and(|t| t.elapsed().as_secs_f64() < 240.0) {
                    return true;
                }
            }
            spool.as_ref().is_some_and(|p| p.exists())
        };
        for _ in 0..8 {
            if owned() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    }

    /// `_prompt_watch_announced[sid] = monotonic()` — the voice prompt-watcher
    /// announced this session's spooled question.
    pub fn note_prompt_announced(&self, session_id: &str) {
        self.prompt_watch_announced
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_owned(), std::time::Instant::now());
    }

    /// `needs_you.record` — `~/.heard/needs-you/<session>.json`.
    fn needs_you_record(&self, session_id: &str, agent: &str, context: &str) {
        let context = context.split_whitespace().collect::<Vec<_>>().join(" ");
        if context.is_empty() {
            return;
        }
        let dir = self.config.paths().heard_dir.join("needs-you");
        let safe = needs_you_safe(session_id);
        let body = serde_json::json!({
            "id": safe,
            "session_id": session_id,
            "agent": if agent.is_empty() { "an agent" } else { agent },
            "context": context,
            "kind": "do",
            "ts": crate::log::now_epoch(),
        });
        let path = dir.join(format!("{safe}.json"));
        let tmp = dir.join(format!("{safe}.json.tmp"));
        let _ = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(&tmp, body.to_string()))
            .and_then(|()| std::fs::rename(&tmp, &path));
    }

    /// `needs_you.clear` — the session moved on.
    fn needs_you_clear(&self, session_id: &str) {
        let path = self
            .config
            .paths()
            .heard_dir
            .join("needs-you")
            .join(format!("{}.json", needs_you_safe(session_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The project channel scheduler's drain — `daemon.py`'s one-second tick
    /// calling `router.collect_project_flushes` and speaking each summary.
    ///
    /// A channel is ready when its most recent event was ≥ 2 s ago (a natural
    /// turn boundary) or it has ≥ 5 pending events (backpressure). Each ready
    /// project produces ONE attributed line, through
    /// [`heard_state::multi_agent::project_flush_text`]: the injected
    /// [`ProjectSummarizer`]'s narrative if it has one, else the deterministic
    /// tag-count template. `TemplateOnly` is what this lane injects, so every
    /// flush here takes the template path — the same thing the Python does
    /// when `persona.summarize_project` returns nothing.
    ///
    /// Returns how many lines reached the sink. Not on a timer in this lane:
    /// the daemon has no tick loop (the speech queue owns one), so
    /// the caller decides when to drain. Digested events therefore accumulate
    /// until someone asks, which is visible in `status.pending_count`.
    pub fn drain_project_digests(&self, auto_voices: bool) -> usize {
        let mut spoken = 0;
        let flushes = self.router.collect_project_flushes(auto_voices, None);
        let solo_fleet = self.router.list_active().len() <= 1;
        for flush in flushes {
            let solo = solo_fleet && flush.member_session_ids.len() <= 1;
            let Some(text) = heard_state::multi_agent::project_flush_text(
                self.summarizer.as_ref(),
                &flush,
                solo,
            ) else {
                continue;
            };
            let event = NarrationEvent {
                kind: "digest".into(),
                neutral: text.clone(),
                tag: "project_flush".into(),
                ctx: Map::new(),
                session_id: flush.speaker_session_id.clone(),
                cwd: String::new(),
            };
            if matches!(
                self.emit(&event, &text, &flush.label, "digest"),
                Outcome::Spoke(_)
            ) {
                self.router.note_flush_spoken(&flush.speaker_session_id);
                spoken += 1;
            }
        }
        spoken
    }

    /// `_start_speech`'s shaping, ONCE, for every spoken line, in the
    /// Python's order: register (tone) → the style's length cap (template
    /// lines only) → the routine-word budget note → the first-run hold →
    /// `_sanitize_spoken` (never read a file path verbatim). `None` when the
    /// first-run hold swallows the line.
    fn shape_for_speech(&self, text: &str, kind: &str, tag: &str, via: &str) -> Option<String> {
        self.shape_for_speech_with(text, kind, tag, via, false)
    }

    fn shape_for_speech_with(
        &self,
        text: &str,
        kind: &str,
        tag: &str,
        via: &str,
        first_run_ok: bool,
    ) -> Option<String> {
        let cfg_value = self.cfg_value();
        let empty = Map::new();
        let cfg = cfg_value.as_object().unwrap_or(&empty);
        let mut text = shape::register_apply_with(
            text,
            cfg.get("narration_register"),
            kind,
            tag,
            &self.verbatim_kinds,
        );
        if via == "fastpath" {
            text = shape::style_line(&text, cfg.get("narration_skill"), tag);
        }
        if matches!(kind, "tool_pre" | "tool_post" | "intermediate") {
            self.volume_budget
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .note(text.split_whitespace().count(), std::time::Instant::now());
        }
        if !first_run_ok && policy::first_run_held(cfg) {
            crate::dlog!("speech_skipped", reason = "first_run_hold");
            return None;
        }
        Some(shape::sanitize_spoken(&text))
    }

    /// `_start_speech`: the shaping pass, the muted gate, then the sink.
    fn emit(
        &self,
        event: &NarrationEvent,
        text: &str,
        project: &str,
        via: &'static str,
    ) -> Outcome {
        let text = text.trim();
        if text.is_empty() {
            crate::dlog!("speech_skipped", reason = "empty_text");
            return Outcome::Dropped("empty_text");
        }
        let Some(shaped) = self.shape_for_speech(text, &event.kind, &event.tag, via) else {
            return Outcome::Dropped("first_run_hold");
        };
        let text = shaped.trim();
        // "Pause Heard" — don't even queue. The hook lane already gates on
        // this, but `event` messages from other producers arrive here
        // directly, and Python's check is at `_start_speech` for that reason.
        if self.is_muted() {
            crate::dlog!(
                "speech_skipped",
                reason = "muted",
                session = head8(&event.session_id)
            );
            return Outcome::Dropped("muted");
        }
        crate::dlog!(
            "event_speak",
            kind = event.kind.as_str(),
            tag = event.tag.as_str(),
            chars = text.chars().count(),
            via = via
        );
        self.hand_to_speech(&Utterance {
            text,
            tag: &event.tag,
            kind: &event.kind,
            session_id: &event.session_id,
            via,
            project,
        });
        Outcome::Spoke(via)
    }

    /// `_last_user_engaged` (wall clock).
    pub fn last_user_engaged(&self) -> f64 {
        *self
            .last_user_engaged
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Stamp `_last_user_engaged = time.time()` — a prompt submitted, a
    /// push-to-talk released.
    pub fn note_user_engaged(&self) {
        *self
            .last_user_engaged
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = crate::log::now_epoch();
    }

    /// Is speech playing or queued?
    pub fn is_speaking(&self) -> bool {
        self.speech.is_speaking()
    }

    /// The extensions, in order.
    pub fn extensions(&self) -> &[Arc<dyn Extension>] {
        &self.extensions
    }

    /// Every extension's verbatim kinds, lowercased (see
    /// [`Extension::verbatim_kinds`]).
    pub fn verbatim_kinds(&self) -> &[String] {
        &self.verbatim_kinds
    }

    fn is_verbatim(&self, kind: &str) -> bool {
        !self.verbatim_kinds.is_empty() && self.verbatim_kinds.contains(&kind.to_lowercase())
    }

    /// Push one event to every subscriber: `{"ev": name, ...fields}` as one
    /// JSON line (see [`crate::events`]). `fields` must be a JSON object
    /// (anything else is sent as `{"ev": name, "value": fields}`). Never
    /// blocks; a subscriber that went away is dropped.
    pub fn emit_event(&self, name: &str, fields: Value) {
        self.events.emit(name, fields);
    }

    /// A new receiver for every event line emitted from now on (the socket
    /// `subscribe` stream is built on this).
    pub fn subscribe_events(&self) -> std::sync::mpsc::Receiver<EventLine> {
        self.events.subscribe()
    }

    /// The event bus itself (to hand to something built after the daemon).
    pub fn events(&self) -> &EventBus {
        &self.events
    }

    /// The `hello` line a new subscriber gets first: `{"ev": "hello", …}`
    /// with every extension's [`Extension::subscribe_hello`] fields.
    pub fn hello_line(&self) -> EventLine {
        let mut fields = Map::new();
        for ext in &self.extensions {
            ext.subscribe_hello(&mut fields);
        }
        crate::events::wire_line("hello", Value::Object(fields))
    }

    /// The first non-blank [`Extension::context_for`] answer for `session`,
    /// trimmed, in extension order. `None` when no extension has any.
    pub fn context_for(&self, session: &str) -> Option<String> {
        self.extensions.iter().find_map(|ext| {
            let text = ext.context_for(session)?;
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_owned())
        })
    }

    /// Hand one line to the speech sink, then tell every extension.
    fn hand_to_speech(&self, line: &SpokenLine<'_>) {
        self.hand_to_speech_with(line, None);
    }

    fn hand_to_speech_with(&self, line: &SpokenLine<'_>, opts: Option<crate::speech::LineOptions>) {
        // `_enqueue_speech`'s bookkeeping, for a line that will reach the
        // queue now (not one the sink holds for replay, not a speaker-off
        // line): it spends the dead-air budget, and a REAL line ends the
        // turn's silence (disarms the "still thinking" nudge).
        let is_filler = line.via == VIA_FILLER;
        if !self.speech.is_holding() && !self.cfg_bool("audio_off", false) {
            self.fillers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .0
                .note_line(line.session_id, self.now(), is_filler);
            if !line.session_id.is_empty() && !is_filler {
                let mut turns = self.turns.lock().unwrap_or_else(|e| e.into_inner());
                turns.0.remove(line.session_id);
                turns.1.remove(line.session_id);
            }
        }
        match opts {
            Some(o) => self.speech.speak_with(line, o),
            None => self.speech.speak(line),
        }
        for ext in &self.extensions {
            ext.on_spoken(line);
        }
    }

    /// Speak a line that did not come from an agent event (an extension's
    /// answer, say), through the same shaping, mute gate and sink.
    ///
    /// A `tag` that is one of the [`Daemon::verbatim_kinds`] is also the kind
    /// the shaping sees, so a line tagged with a verbatim kind is spoken
    /// verbatim whatever its history `kind`.
    pub fn say(&self, text: &str, kind: &str, tag: &str, session_id: &str, via: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let shape_kind = if self.is_verbatim(tag) { tag } else { kind };
        let Some(shaped) = self.shape_for_speech(text, shape_kind, tag, via) else {
            return;
        };
        let text = shaped.trim();
        if text.is_empty() {
            return;
        }
        if self.is_muted() {
            crate::dlog!("speech_skipped", reason = "muted", kind = kind);
            return;
        }
        crate::dlog!(
            "event_speak",
            kind = kind,
            tag = tag,
            chars = text.chars().count(),
            via = via
        );
        self.hand_to_speech(&Utterance {
            text,
            tag,
            kind,
            session_id,
            via,
            project: "",
        });
    }

    /// The `cmd`-less `speak` fall-through: literal text, persona bypassed.
    pub fn speak_direct(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.is_muted() {
            crate::dlog!("speech_skipped", reason = "muted");
            return;
        }
        crate::dlog!("event_speak", chars = text.chars().count(), via = "direct");
        self.hand_to_speech(&Utterance {
            text,
            tag: "",
            kind: "speak",
            session_id: "",
            via: "direct",
            project: "",
        });
    }
}

/// `_RESUME_INTENT_TIMEOUT_S` — an unanswered resume panel defaults to
/// "fresh" after this long.
pub const RESUME_INTENT_TIMEOUT_S: f64 = 30.0;

/// `_RESUME_INTENT_CATCH_UP_TOKENS`.
const RESUME_CATCH_UP_TOKENS: &[&str] = &[
    "catch",
    "continue",
    "recap",
    "summary",
    "summarize",
    "summarise",
    "where",
    "left",
    "yes",
    "yep",
    "yeah",
    "yup",
    "please",
    "sure",
    "go",
    "ok",
    "okay",
    "do",
];
/// `_RESUME_INTENT_FRESH_TOKENS`.
const RESUME_FRESH_TOKENS: &[&str] = &[
    "fresh",
    "skip",
    "new",
    "start over",
    "starting over",
    "scratch",
    "no",
    "nope",
    "nah",
    "drop",
    "forget",
    "nothing",
    "later",
    "don't",
    "dont",
    "cancel",
];

/// `persona._keyword_classify_resume_intent` — `None` when neither keyword
/// set decides (the Python then asks a model).
pub fn keyword_resume_intent(text: &str) -> Option<&'static str> {
    let lowered = text.to_lowercase();
    for phrase in ["start over", "starting over", "from scratch"] {
        if lowered.contains(phrase) {
            return Some("fresh");
        }
    }
    const STRIP: &[char] = &[
        '.', ',', '!', '?', ';', ':', '\'', '"', '(', ')', '[', ']', '{', '}',
    ];
    let tokens: HashSet<String> = lowered
        .split_whitespace()
        .map(|t| t.trim_matches(STRIP).to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    let catch = RESUME_CATCH_UP_TOKENS.iter().any(|t| tokens.contains(*t));
    let fresh = RESUME_FRESH_TOKENS.iter().any(|t| tokens.contains(*t));
    match (catch, fresh) {
        (true, false) => Some("catch_up"),
        (false, true) => Some("fresh"),
        _ => None,
    }
}

/// `persona.classify_resume_intent` without a model: empty → fresh, the
/// keyword sets, and an ambiguous answer lands on "fresh" (the Python's
/// floor when no model answers).
pub fn classify_resume_intent(text: &str) -> &'static str {
    let stripped = text.trim();
    if stripped.is_empty() {
        return "fresh";
    }
    keyword_resume_intent(stripped).unwrap_or("fresh")
}

/// One line for [`Daemon::say_line`]: [`Daemon::say`] with the Python's
/// `_start_speech` keywords spelled out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line<'a> {
    /// The words.
    pub text: &'a str,
    /// `history_meta["kind"]`.
    pub kind: &'a str,
    /// `history_meta["tag"]`.
    pub tag: &'a str,
    /// Owning session.
    pub session_id: &'a str,
    /// `history_meta["via"]`.
    pub via: &'a str,
    /// `priority=` (default: `kind == "final"`).
    pub priority: bool,
    /// `coexists=` (default: `via == "digest"`).
    pub coexists: bool,
    /// `history_meta` given (default `true`).
    pub history: bool,
    /// `setup_capability` + `first_run_generation`: `Some(g)` lets the line
    /// through the first-run hold when `g` is the CURRENT held generation
    /// (the onboarding sample). `None` = held like every other line.
    pub first_run_generation: Option<i64>,
}

impl<'a> Line<'a> {
    /// A line with the defaults.
    #[must_use]
    pub fn new(
        text: &'a str,
        kind: &'a str,
        tag: &'a str,
        session_id: &'a str,
        via: &'a str,
    ) -> Self {
        Self {
            text,
            kind,
            tag,
            session_id,
            via,
            priority: kind == "final",
            coexists: via == "digest",
            history: true,
            first_run_generation: None,
        }
    }
}

/// A time-seeded xorshift for `random.choice` over the filler pools.
fn xorshift_picker() -> impl FnMut(usize) -> usize + Send {
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    move |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        if n == 0 {
            0
        } else {
            (state % n as u64) as usize
        }
    }
}

impl Daemon {
    /// The daemon's monotonic clock.
    pub fn now(&self) -> f64 {
        self.clock.monotonic()
    }

    // ---- the floor: push-to-talk and the guided tour ---------------------------

    /// `cmd == "voice_hold"` — CHANNEL B: the user started speaking. Cut what
    /// is playing, drop the queue, hold new narration until they finish.
    pub fn voice_hold(&self) {
        self.speech.hold(Hold::User);
        crate::dlog!("voice_hold");
    }

    /// `cmd == "voice_release"` — the user stopped: stamp engagement HERE
    /// (not at hold: the utterance that asks "catch me up" must not zero its
    /// own away window), then replay what was held.
    pub fn voice_release(&self) {
        self.note_user_engaged();
        self.speech.release(Hold::User);
        crate::dlog!("voice_release", engaged = "stamped");
    }

    /// `cmd == "tour_hold"` — the guided tour is on screen: rescue the
    /// unplayed queue into the held buffer, cut, and hold new lines.
    pub fn tour_hold(&self) {
        self.speech.hold(Hold::Tour);
        crate::dlog!("tour_hold");
    }

    /// `cmd == "tour_release"`.
    pub fn tour_release(&self) {
        crate::dlog!("tour_release");
        self.speech.release(Hold::Tour);
    }

    /// `cmd == "first_run_hold"`'s effects once its generation checked out:
    /// stop current output, discard the held backlog and the digest, forget
    /// the hung-tool watch and the resume panel. Nothing suppressed during
    /// setup is replayed after Finish.
    pub fn first_run_reset(&self) {
        self.speech.cancel();
        self.speech.discard_held();
        let _ = self.router.collect_project_flushes(false, None);
        {
            let mut hung = self.hung.lock().unwrap_or_else(|e| e.into_inner());
            hung.0.clear();
            hung.1.clear();
        }
        self.clear_awaiting_resume();
    }

    /// `first_run.state()` from DISK (the Python reads `config.load()`; the
    /// app flips it from another process).
    pub fn first_run_state(&self) -> policy::FirstRunState {
        let cfg = self.config.load(None).unwrap_or_default();
        policy::first_run_state(&cfg)
    }

    // ---- feedback and the resume panel --------------------------------------------

    /// `cmd == "feedback"` — `history.append_feedback` against the last
    /// utterance the sink recorded. Blank text records nothing.
    pub fn feedback(&self, text: &str, source: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let source = if source.is_empty() { "cli" } else { source };
        let last = self.speech.last_utterance_id();
        heard_state::history::History::new(&self.config.paths().config_dir)
            .with_policy(Arc::clone(&self.history_policy))
            .append_feedback(last.as_deref().unwrap_or(""), source, text, "explicit");
        crate::dlog!(
            "feedback_recorded",
            source = source,
            has_ref = last.is_some()
        );
    }

    /// Whether the resume panel is waiting for an answer.
    pub fn is_awaiting_resume(&self) -> bool {
        self.awaiting_resume
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    fn clear_awaiting_resume(&self) {
        *self
            .awaiting_resume
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// `cmd == "resume_intent"` — act on the resume panel's answer:
    /// `catch_up` speaks one rolled-up line per buffered project, `fresh`
    /// (and anything unrecognised) drops the buffer.
    ///
    /// An answer neither keyword set decides goes to the extensions'
    /// [`Extension::classify_resume_intent`] (the Python's model fallback)
    /// before landing on "fresh".
    pub fn resume_intent(&self, text: &str, from_timeout: bool) -> &'static str {
        self.clear_awaiting_resume();
        let intent = self.classify_resume(text);
        self.act_on_resume_intent(text, intent, from_timeout)
    }

    /// `resume_intent` from the socket: an answer only a model can classify
    /// is classified OFF the accept loop (a model call must not stall the
    /// socket); everything else is answered inline, exactly as
    /// [`Daemon::resume_intent`] does.
    pub fn resume_intent_from_socket(self: &Arc<Self>, text: &str) {
        let stripped = text.trim();
        let needs_model = !stripped.is_empty()
            && keyword_resume_intent(stripped).is_none()
            && !self.extensions.is_empty();
        if !needs_model {
            self.resume_intent(text, false);
            return;
        }
        self.clear_awaiting_resume();
        let daemon = Arc::clone(self);
        let text = text.to_owned();
        let spawned = std::thread::Builder::new()
            .name("resume-intent".into())
            .spawn(move || {
                let intent = daemon.classify_resume(&text);
                daemon.act_on_resume_intent(&text, intent, false);
            });
        if let Err(e) = spawned {
            crate::dlog!("resume_intent_thread_failed", err = e.to_string());
        }
    }

    /// `persona.classify_resume_intent`: empty → fresh, the keyword sets,
    /// the extensions' classifiers in order, then "fresh".
    fn classify_resume(&self, text: &str) -> &'static str {
        let stripped = text.trim();
        if stripped.is_empty() {
            return "fresh";
        }
        if let Some(intent) = keyword_resume_intent(stripped) {
            return intent;
        }
        self.extensions
            .iter()
            .find_map(|ext| ext.classify_resume_intent(stripped))
            .unwrap_or("fresh")
    }

    fn act_on_resume_intent(
        &self,
        text: &str,
        intent: &'static str,
        from_timeout: bool,
    ) -> &'static str {
        crate::dlog!(
            "resume_intent",
            intent = intent,
            timeout = from_timeout,
            text_len = text.chars().count()
        );
        if intent == "catch_up" {
            self.drain_pending_as_summary();
            return intent;
        }
        if intent == "other" {
            let head: String = text.chars().take(160).collect();
            crate::dlog!("resume_intent_other", text = head.as_str());
        }
        let cleared = self.router.clear_pending();
        if cleared > 0 {
            crate::dlog!("resume_pending_cleared", count = cleared);
        }
        intent
    }

    /// `_drain_pending_as_summary` — the catch-up: force-flush every
    /// project's buffer through the same project-flush line the tick uses.
    fn drain_pending_as_summary(&self) -> usize {
        let auto_voices = self.cfg_bool("multi_agent_auto_voices", false);
        let flushes = self.router.force_flush_all(auto_voices, None);
        let mut spoken = 0;
        for flush in flushes {
            // The catch-up always names the project, even for one agent:
            // `_drain_pending_as_summary` calls `summarize_project` without
            // `solo`, so it is False there (unlike the tick's flush).
            let solo = false;
            let Some(text) = heard_state::multi_agent::project_flush_text(
                self.summarizer.as_ref(),
                &flush,
                solo,
            ) else {
                continue;
            };
            crate::dlog!(
                "resume_catch_up",
                project = flush.label.as_str(),
                sessions = flush.member_session_ids.len()
            );
            self.router.note_flush_spoken(&flush.speaker_session_id);
            self.say_line(&Line {
                coexists: true,
                history: false,
                ..Line::new(&text, "", "", &flush.speaker_session_id, VIA_NOTICE)
            });
            spoken += 1;
        }
        spoken
    }

    // ---- the one-second tick ---------------------------------------------------------

    /// The Python's one-second `_tick`, in its order:
    ///
    /// 1. first-run hold: collect (and discard) the digest, nothing else;
    /// 2. Focus mode's hung-tool line — once per hang, after
    ///    [`filler::HUNG_TOOL_S`] with no follow-up event;
    /// 3. companion's "still thinking" nudge — once per turn, after
    ///    `companion_thinking_nudge_seconds` with nothing spoken, under the
    ///    shared dead-air budget;
    /// 4. the resume panel's safety timeout ("fresh");
    /// 5. the project-digest drain ([`Daemon::drain_project_digests`]),
    ///    skipped entirely while muted or waiting on the resume panel (the
    ///    buffer is what the panel offers), collected silently when
    ///    `multi_agent_digest_enabled` is off.
    ///
    /// Returns how many digest lines reached the sink.
    pub fn tick(&self, auto_voices: bool) -> usize {
        let cfg_value = self.cfg_value();
        let empty = Map::new();
        let cfg = cfg_value.as_object().unwrap_or(&empty);
        if policy::first_run_held(cfg) {
            let _ = self.router.collect_project_flushes(false, None);
            return 0;
        }
        let quiet =
            policy::cfg_truthy(cfg, "muted", false) || policy::cfg_truthy(cfg, "paused", false);
        let now = self.now();

        if policy::narration_mode(cfg) == "focus" && !quiet {
            let due: Vec<(String, String, f64)> = {
                let mut hung = self.hung.lock().unwrap_or_else(|e| e.into_inner());
                let (track, alerted) = &mut *hung;
                let mut due = Vec::new();
                for (sid, (started, label)) in track.iter() {
                    if alerted.contains(sid) || now - started < filler::HUNG_TOOL_S {
                        continue;
                    }
                    due.push((sid.clone(), label.clone(), now - started));
                }
                for (sid, _, _) in &due {
                    alerted.insert(sid.clone());
                }
                due
            };
            for (sid, label, elapsed) in due {
                let text = filler::hung_tool_line(&label);
                crate::dlog!(
                    "hung_tool_alert",
                    session = sid.as_str(),
                    label = label.trim_end_matches('.'),
                    elapsed = elapsed.round() as i64
                );
                self.say_line(&Line {
                    coexists: true,
                    history: false,
                    ..Line::new(&text, "", "", &sid, VIA_NOTICE)
                });
            }
        }

        if filler::should_thinking_nudge(cfg) {
            let thr = filler::thinking_nudge_after_s(cfg);
            let open: Vec<(String, f64)> = {
                let turns = self.turns.lock().unwrap_or_else(|e| e.into_inner());
                turns
                    .0
                    .iter()
                    .filter(|(sid, opened)| !turns.1.contains(*sid) && now - **opened >= thr)
                    .map(|(sid, opened)| (sid.clone(), *opened))
                    .collect()
            };
            for (sid, opened) in open {
                let (ok, why) = self
                    .fillers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .0
                    .allow(&sid, now);
                if !ok {
                    crate::dlog!(
                        "filler_suppressed",
                        session = sid.as_str(),
                        reason = why,
                        at = "thinking_nudge"
                    );
                    continue;
                }
                self.turns
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .1
                    .insert(sid.clone());
                crate::dlog!(
                    "companion_thinking_nudge",
                    session = sid.as_str(),
                    elapsed = (now - opened).round() as i64
                );
                let line = self.pick_filler(true);
                self.say_filler(line, &sid);
            }
        }

        let timed_out = self
            .awaiting_resume
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|deadline| now >= deadline);
        if timed_out {
            self.resume_intent("", true);
        }

        if !policy::cfg_truthy(cfg, "multi_agent_digest_enabled", true) {
            let _ = self.router.collect_project_flushes(auto_voices, None);
            return 0;
        }
        if policy::cfg_truthy(cfg, "muted", false) || self.is_awaiting_resume() {
            return 0;
        }
        self.drain_project_digests(auto_voices)
    }

    fn pick_filler(&self, nudge: bool) -> &'static str {
        let mut picker = self.picker.lock().unwrap_or_else(|e| e.into_inner());
        let mut fillers = self.fillers.lock().unwrap_or_else(|e| e.into_inner());
        let (pool, last) = if nudge {
            (filler::THINKING_NUDGE_LINES, &mut fillers.2)
        } else {
            (filler::PROMPT_FILLER_LINES, &mut fillers.1)
        };
        filler::pick_varied(pool, last, &mut **picker)
    }

    fn say_filler(&self, text: &str, session_id: &str) {
        self.say_line(&Line {
            coexists: true,
            history: false,
            ..Line::new(text, "", "", session_id, VIA_FILLER)
        });
    }

    /// Speak one line through the shaping, the first-run hold, the mute gate
    /// and the sink, with explicit queue placement — `_start_speech` with
    /// its keywords. Returns whether it reached the sink.
    pub fn say_line(&self, line: &Line<'_>) -> bool {
        let text = line.text.trim();
        if text.is_empty() {
            return false;
        }
        let setup_ok = line.first_run_generation.is_some_and(|g| {
            let st = self.first_run_state();
            st.held && g == st.generation
        });
        let shape_kind = if self.is_verbatim(line.tag) {
            line.tag
        } else {
            line.kind
        };
        let Some(shaped) =
            self.shape_for_speech_with(text, shape_kind, line.tag, line.via, setup_ok)
        else {
            return false;
        };
        let text = shaped.trim();
        if text.is_empty() {
            return false;
        }
        if self.is_muted() {
            crate::dlog!("speech_skipped", reason = "muted", kind = line.kind);
            return false;
        }
        crate::dlog!(
            "event_speak",
            kind = line.kind,
            tag = line.tag,
            chars = text.chars().count(),
            via = line.via
        );
        self.hand_to_speech_with(
            &Utterance {
                text,
                tag: line.tag,
                kind: line.kind,
                session_id: line.session_id,
                via: line.via,
                project: "",
            },
            Some(crate::speech::LineOptions {
                priority: line.priority,
                coexists: line.coexists,
                history: line.history,
            }),
        );
        true
    }
}

/// `session=sid[:8]` in the Python log lines.
fn head8(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech::LogSpeech;

    /// A daemon over `config.yaml` = `yaml` (an onboarded install unless the
    /// YAML says otherwise), speaking into a JSONL sink.
    fn daemon_with(
        label: &str,
        yaml: &str,
        brain: Option<Arc<dyn Brain>>,
    ) -> (Arc<Daemon>, PathBuf) {
        let dir = crate::testing::temp_dir(label);
        let sink = dir.join("speech.jsonl");
        let paths = Paths::under(&dir);
        std::fs::create_dir_all(&paths.config_dir).expect("config dir");
        std::fs::write(&paths.config_path, yaml).expect("config");
        let mut builder = DaemonBuilder::new(paths).speech(Arc::new(LogSpeech::new(&sink)));
        if let Some(b) = brain {
            builder = builder.brain(b);
        }
        (builder.build(), sink)
    }

    fn daemon(label: &str) -> (Arc<Daemon>, PathBuf) {
        daemon_with(label, "onboarded: true\n", None)
    }

    fn lines(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect()
    }

    fn event(kind: &str, tag: &str, neutral: &str) -> NarrationEvent {
        NarrationEvent {
            kind: kind.into(),
            tag: tag.into(),
            neutral: neutral.into(),
            ctx: Map::new(),
            session_id: "s1".into(),
            cwd: "/tmp/proj".into(),
        }
    }

    #[test]
    fn a_tool_event_takes_the_fast_path() {
        let (daemon, sink) =
            daemon_with("fastpath", "onboarded: true\nnarrate_routine: true\n", None);
        assert_eq!(
            daemon.handle_event(&event("tool_pre", "tool_bash", "Running a shell command")),
            Outcome::Spoke("fastpath")
        );
        let lines = lines(&sink);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["via"], "fastpath");
        assert_eq!(lines[0]["kind"], "tool_pre");
    }

    #[test]
    fn a_final_takes_the_floor_when_the_brain_punts() {
        let (daemon, sink) = daemon("floor");
        assert_eq!(
            daemon.handle_event(&event("final", "final_short", "All tests pass")),
            Outcome::Spoke("floor")
        );
        let lines = lines(&sink);
        assert_eq!(lines[0]["via"], "floor");
        // The default persona is Jarvis: the floor appends his address.
        assert_eq!(lines[0]["text"], "All tests pass, Sir.");
    }

    #[test]
    fn at_the_defaults_routine_tools_and_copilot_prose_are_dropped() {
        let (daemon, sink) = daemon("defaults-drop");
        assert_eq!(
            daemon.handle_event(&event("tool_pre", "tool_bash", "Running a shell command")),
            Outcome::Dropped("routine_narration_off")
        );
        assert_eq!(
            daemon.handle_event(&event(
                "intermediate",
                "intermediate_short",
                "Looking at it."
            )),
            Outcome::Dropped("copilot_prose_off")
        );
        // A failure still pierces every quiet rule.
        assert_eq!(
            daemon.handle_event(&event("tool_post", "tool_post_failure", "Tests failed.")),
            Outcome::Spoke("fastpath")
        );
        assert_eq!(lines(&sink).len(), 1);
        assert_eq!(
            daemon.router.pending_count(),
            0,
            "nothing digests at the defaults"
        );
    }

    #[test]
    fn a_fresh_install_is_held_by_first_run() {
        let (daemon, sink) = daemon_with("first-run", "muted: false\n", None);
        assert_eq!(
            daemon.handle_event(&event("final", "final_short", "All tests pass")),
            Outcome::Dropped("first_run_hold")
        );
        assert!(lines(&sink).is_empty());
    }

    #[test]
    fn a_path_in_a_final_is_spoken_as_its_stem() {
        let (daemon, sink) = daemon("sanitize");
        daemon.handle_event(&event(
            "final",
            "final_short",
            "Wrote it in notes/release-checklist.csv",
        ));
        assert_eq!(
            lines(&sink)[0]["text"],
            "Wrote it in release-checklist, Sir."
        );
    }

    #[test]
    fn a_punted_intermediate_is_dropped_by_the_floor() {
        // Companion (routine on): prose reaches the model lane, and the
        // floor has nothing for a punted intermediate.
        let (daemon, sink) = daemon_with(
            "floor-intermediate",
            "onboarded: true\nnarrate_routine: true\n",
            None,
        );
        assert_eq!(
            daemon.handle_event(&event("intermediate", "intermediate_short", "thinking")),
            Outcome::Dropped("floor_drop")
        );
        assert!(lines(&sink).is_empty());
    }

    #[test]
    fn the_same_event_twice_only_speaks_once() {
        let (daemon, sink) = daemon("dup");
        let e = event("final", "final_short", "All tests pass");
        assert!(matches!(daemon.handle_event(&e), Outcome::Spoke(_)));
        assert_eq!(daemon.handle_event(&e), Outcome::Dropped("duplicate_event"));
        assert_eq!(lines(&sink).len(), 1);
    }

    #[test]
    fn a_muted_session_reaches_no_sink() {
        let (daemon, sink) = daemon("session-mute");
        daemon.mute_session("s1");
        assert_eq!(
            daemon.handle_event(&event("final", "final_short", "All tests pass")),
            Outcome::Dropped("session_muted")
        );
        assert!(lines(&sink).is_empty());
    }

    #[test]
    fn mute_persists_through_config_and_gates_the_sink() {
        let (daemon, sink) = daemon("mute");
        daemon.mute("cli");
        assert!(daemon.is_muted());
        assert_eq!(
            daemon.handle_event(&event("final", "final_short", "All tests pass")),
            Outcome::Dropped("muted")
        );
        assert!(lines(&sink).is_empty());
        daemon.unmute("cli");
        assert!(!daemon.is_muted());
    }

    #[test]
    fn a_prompt_intent_is_retired_but_arms_the_opener() {
        let (daemon, _) = daemon("opener");
        assert_eq!(
            daemon.handle_event(&event("prompt_intent", "prompt_intent", "add a test")),
            Outcome::Dropped("prompt_intent_retired")
        );
        assert!(daemon.opener_pending.lock().expect("lock").contains("s1"));
        assert_eq!(
            daemon
                .last_prompt
                .lock()
                .expect("lock")
                .get("s1")
                .map(String::as_str),
            Some("add a test")
        );
    }

    #[test]
    fn the_brain_can_speak_and_its_text_is_what_lands() {
        struct Chatty;
        impl Brain for Chatty {
            fn narrate(&self, _r: &BrainRequest<'_>) -> Option<crate::brain::BrainDecision> {
                Some(crate::brain::BrainDecision::speak(
                    "Wrapped up the auth work.",
                ))
            }
        }
        let (daemon, sink) =
            daemon_with("brain-speaks", "onboarded: true\n", Some(Arc::new(Chatty)));
        assert_eq!(
            daemon.handle_event(&event("final", "final_short", "raw")),
            Outcome::Spoke("brain")
        );
        assert_eq!(lines(&sink)[0]["text"], "Wrapped up the auth work.");
    }

    #[test]
    fn a_brain_that_skips_a_final_still_floors_it() {
        struct Silent;
        impl Brain for Silent {
            fn narrate(&self, _r: &BrainRequest<'_>) -> Option<crate::brain::BrainDecision> {
                Some(crate::brain::BrainDecision::silence())
            }
        }
        let (daemon, sink) =
            daemon_with("brain-silence", "onboarded: true\n", Some(Arc::new(Silent)));
        assert_eq!(
            daemon.handle_event(&event("final", "final_short", "All good")),
            Outcome::Spoke("floor")
        );
        assert_eq!(lines(&sink)[0]["text"], "All good, Sir.");
    }

    #[test]
    fn an_attention_brain_can_authoritatively_silence_a_final() {
        struct AttentionBrain;
        impl Brain for AttentionBrain {
            fn narrate(&self, _r: &BrainRequest<'_>) -> Option<crate::brain::BrainDecision> {
                Some(crate::brain::BrainDecision::silence())
            }

            fn silence_is_authoritative(&self, _r: &BrainRequest<'_>) -> bool {
                true
            }
        }
        let (daemon, sink) = daemon_with(
            "attention-silence",
            "onboarded: true\n",
            Some(Arc::new(AttentionBrain)),
        );
        assert_eq!(
            daemon.handle_event(&event("final", "final_short", "All good")),
            Outcome::Dropped("harness_skip")
        );
        assert!(lines(&sink).is_empty());
    }

    #[test]
    fn status_reports_what_the_ported_crates_know() {
        let (daemon, _) = daemon("status");
        daemon.handle_event(&event("tool_pre", "tool_bash", "Running a shell command"));
        let status = daemon.status();
        assert!(status.alive);
        assert_eq!(status.router_mode, "solo");
        assert_eq!(status.active_sessions.as_array().map(Vec::len), Some(1));
        assert!(!status.muted);
    }

    #[test]
    fn a_digested_event_drains_as_one_project_line() {
        let (daemon, sink) = daemon("digest");
        // Quiet drops prose outright, so use the profile that DIGESTS tools.
        std::fs::create_dir_all(&daemon.config.paths().config_dir).expect("config dir");
        std::fs::write(
            &daemon.config.paths().config_path,
            "onboarded: true\nverbosity: brief\nnarrate_tools: true\nnarrate_routine: true\n",
        )
        .expect("config");
        daemon.reload();

        // `tool_edit` is low-signal, so `brief` routes it to the digest.
        let mut event = event("tool_pre", "tool_edit", "Editing auth.py");
        event
            .ctx
            .insert("abs_path".into(), Value::String("/tmp/proj/auth.py".into()));
        assert_eq!(daemon.handle_event(&event), Outcome::Digested);
        assert_eq!(daemon.router.pending_count(), 1);
        assert!(lines(&sink).is_empty(), "a digest is not spoken on arrival");

        // Backpressure has not fired and 2 s of idle have not passed, so a
        // drain right now flushes nothing.
        assert_eq!(daemon.drain_project_digests(false), 0);
        assert_eq!(daemon.router.pending_count(), 1);
    }

    #[test]
    fn a_forced_flush_speaks_the_template_summary() {
        let (daemon, sink) = daemon("digest-forced");
        daemon.router.note_event("s1", "/tmp/proj", None);
        daemon.router.add_to_digest(
            "s1",
            "tool_pre",
            "tool_edit",
            "Editing auth.py",
            Some(Value::Object(Map::new())),
        );
        let flushes = daemon.router.force_flush_all(false, None);
        assert_eq!(flushes.len(), 1);
        // `TemplateOnly` is the injected summarizer, so this is the
        // deterministic tag-count line, not an LLM narrative.
        let text = heard_state::multi_agent::project_flush_text(
            daemon.summarizer.as_ref(),
            &flushes[0],
            true,
        )
        .expect("a template summary");
        assert!(!text.is_empty());
        assert!(lines(&sink).is_empty());
    }

    #[test]
    fn stop_is_idempotent_and_latches() {
        let (daemon, _) = daemon("stop");
        assert!(!daemon.is_stopping());
        daemon.stop();
        daemon.stop();
        assert!(daemon.is_stopping());
    }
}
