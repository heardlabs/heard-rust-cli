//! `engine/heard/hook_processing.py`, ported — the work that used to run IN
//! the hook process and now runs on the daemon's per-session lane.
//!
//! The hook is a dumb pipe: it reads stdin and writes one fire-and-forget
//! `{"cmd":"hook","agent":…,"payload":<raw>,"binding":…}` message. Everything
//! that used to be on the agent's critical path lives here:
//!
//! * the `flush_delay_ms` wait for the agent to flush its assistant prose to
//!   the transcript (800 ms by default);
//! * the incremental transcript read;
//! * the flock'd `spoken` dedup read-modify-write;
//! * markdown stripping and template rendering;
//! * the AskUserQuestion suppression and the "spoke prose → skip the tool
//!   line" rule;
//! * the "Pause Heard" gate, checked BEFORE the handler lookup so a paused
//!   Heard costs no transcript read, no dedup write and no brain call.
//!
//! # Ordering
//!
//! [`HookQueue`] gives each session key (`<agent>:<session_id>`) one FIFO
//! lane and one task, so a PostToolUse can never overtake its PreToolUse even
//! though the PreToolUse sleeps for the flush delay. Different sessions get
//! different tasks and run concurrently — serialising them globally would let
//! one agent's 800 ms wait stall every other agent's narration.
//!
//! Python spends a THREAD per session on this. Here a lane is a `tokio` task,
//! so an idle one costs a `VecDeque` and a `Notify` rather than a stack; the
//! 120-second idle retirement is kept anyway, because a machine that ran
//! fifty sessions yesterday should not be holding fifty lanes today.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use heard_narrate::{markdown, templates};
use serde_json::{Map, Value};
use tokio::sync::Notify;

use crate::daemon::{Daemon, NarrationEvent};
use crate::transcript;

/// `hook_processing._WORKER_IDLE_TIMEOUT_S`.
pub const WORKER_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// `hook_processing._MAX_PENDING_PER_SESSION`. A session that outruns its
/// lane drops the OLDEST pending hook, never the newest: narration is
/// ambient, and a stale event is worth less than a fresh one.
pub const MAX_PENDING_PER_SESSION: usize = 256;

/// `hook_processing._PROMPT_INTENT_MIN_CHARS`. Quick confirmations ("yes",
/// "go", "do it") finish faster than a spoken summary of them would.
const PROMPT_INTENT_MIN_CHARS: usize = 20;

/// `handle_cc_pre_tool`'s cap on the `recent_intent` it enriches ctx with.
const RECENT_INTENT_CAP: usize = 400;

/// The closed set of `(agent, hook_event_name)` pairs the daemon acts on.
/// Anything else is a silent no-op — the hook is allowed to be newer than the
/// daemon.
pub const AGENTS: [(&str, &[&str]); 2] = [
    (
        "claude-code",
        &["Stop", "PreToolUse", "PostToolUse", "UserPromptSubmit"],
    ),
    ("codex", &["Stop", "PreToolUse", "PostToolUse"]),
];

/// `hook_processing.session_key` — the ordering key. One agent session is one
/// FIFO lane.
pub fn session_key(agent: &str, payload: &Value) -> String {
    let sid = payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("default");
    format!("{agent}:{sid}")
}

/// Is this an `(agent, event)` pair we handle?
fn is_known(agent: &str) -> bool {
    AGENTS.iter().any(|(name, _)| *name == agent)
}

fn handles(agent: &str, event: &str) -> bool {
    AGENTS
        .iter()
        .find(|(name, _)| *name == agent)
        .is_some_and(|(_, events)| events.contains(&event))
}

/// The five config values the hook path reads, snapshotted once per payload —
/// exactly one `config.load()` per handler, as in the Python.
#[derive(Debug, Clone, Copy)]
struct HookCfg {
    flush_delay: Duration,
    skip_under_chars: usize,
    narrate_tools: bool,
    narrate_tool_results: bool,
    narrate_prompt_intent: bool,
}

impl HookCfg {
    fn from_map(cfg: &Map<String, Value>) -> Self {
        let truthy = |key: &str, default: bool| match cfg.get(key) {
            None => default,
            Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64() != Some(0.0),
            Some(Value::String(s)) => !s.is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
        };
        Self {
            flush_delay: Duration::from_millis(
                cfg.get("flush_delay_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(800),
            ),
            skip_under_chars: cfg
                .get("skip_under_chars")
                .and_then(Value::as_u64)
                .unwrap_or(30) as usize,
            narrate_tools: truthy("narrate_tools", true),
            narrate_tool_results: truthy("narrate_tool_results", true),
            narrate_prompt_intent: truthy("narrate_prompt_intent", true),
        }
    }
}

/// `_session_from_data` — the session descriptor for one hook payload.
///
/// The `binding` the hook derived is read by [`process`] into the daemon's
/// `session_identities` (the Herdr sidebar reporter is not ported). Never
/// invented from the DAEMON's environment — that is exactly the
/// mislabelling the split exists to prevent.
fn session_from(payload: &Value) -> (String, String) {
    let id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_owned();
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    (id, cwd)
}

fn transcript_path(payload: &Value) -> Option<PathBuf> {
    payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

/// `advance_offset_while_muted` — the load-bearing half of the
/// resume-without-replay fix.
///
/// While paused, bump the session's spoken-offset to the transcript's current
/// end of file. Without it the first hook after a resume reads every line the
/// agent wrote during the pause — potentially hours of prose — and floods the
/// speech path with stale narration in one burst. Best-effort throughout: any
/// I/O failure falls through to the pre-fix behaviour, which is no worse than
/// today. Codex transcripts have a different shape and are left alone.
pub fn advance_offset_while_muted(daemon: &Daemon, payload: &Value) {
    let Some(path) = transcript_path(payload) else {
        return;
    };
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .get("session")
                .and_then(|s| s.get("id"))
                .and_then(Value::as_str)
        })
        .filter(|s| !s.is_empty());
    let Some(session_id) = session_id else {
        return;
    };
    let Some(size) = transcript::size_of(&path) else {
        return;
    };
    daemon.spoken.set_offset(session_id, size);
}

/// `hook_processing.process` — run the hook processing for one payload.
///
/// An unknown agent or event name is a silent no-op.
pub async fn process(daemon: &Daemon, agent: &str, payload: &Value, binding: Option<&Value>) {
    if !is_known(agent) || !payload.is_object() {
        return;
    }
    // `session_identities.observe_event(req)` — every hook frame, with the
    // binding the hook process derived from its own environment (or none).
    if let Some(sid) = payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let cwd = payload.get("cwd").and_then(Value::as_str);
        daemon.session_identities.observe(sid, cwd, binding);
    }
    // "Pause Heard" — checked HERE, before the handler lookup, so a paused
    // Heard costs nothing downstream: no transcript read, no dedup write, no
    // event, and above all no brain call (the routing's own muted check is at
    // the sink, which is after the brain has already run).
    if daemon.is_muted() {
        if agent == "claude-code" {
            advance_offset_while_muted(daemon, payload);
        }
        return;
    }
    let event_name = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !handles(agent, event_name) {
        return;
    }
    match (agent, event_name) {
        ("claude-code", "Stop") => cc_stop(daemon, payload).await,
        ("claude-code", "PreToolUse") => cc_pre_tool(daemon, payload).await,
        ("claude-code", "PostToolUse") => cc_post_tool(daemon, payload),
        ("claude-code", "UserPromptSubmit") => cc_user_prompt_submit(daemon, payload),
        ("codex", "Stop") => codex_stop(daemon, payload).await,
        ("codex", "PreToolUse") => codex_pre_tool(daemon, payload).await,
        ("codex", "PostToolUse") => codex_post_tool(daemon, payload),
        _ => {}
    }
}

/// `config.load()` for Claude Code (no cwd) — the daemon's snapshot.
fn cc_cfg(daemon: &Daemon) -> HookCfg {
    match daemon.cfg_value() {
        Value::Object(map) => HookCfg::from_map(&map),
        _ => HookCfg::from_map(&Map::new()),
    }
}

/// `config.load(cwd=data.get("cwd"))` — Codex loads the PROJECT config, so a
/// repo's `.heard.yaml` can quiet just that checkout.
fn codex_cfg(daemon: &Daemon, cwd: &str) -> HookCfg {
    let cwd = (!cwd.is_empty()).then(|| Path::new(cwd));
    match daemon.config.load(cwd) {
        Ok(map) => HookCfg::from_map(&map),
        Err(_) => cc_cfg(daemon),
    }
}

/// `_mark_transcript_prose_spoken` — advance the offset and mark all pending
/// prose spoken WITHOUT sending it. Used to suppress narration we know would
/// land too late to be useful.
fn mark_transcript_prose_spoken(daemon: &Daemon, path: &Path, session_id: &str) {
    // First encounter with this session (fresh install / wiped state):
    // initialise at EOF instead of replaying the whole transcript as
    // "pending prose to suppress".
    if !daemon.spoken.has_offset(session_id) {
        daemon.spoken.initialize_at_eof(session_id, path, &[]);
        return;
    }
    let start = daemon.spoken.get_offset(session_id);
    let (fresh, end) = transcript::extract_assistant_texts_from(path, start);
    if end != start {
        daemon.spoken.set_offset(session_id, end);
    }
    for text in fresh {
        daemon.spoken.mark_spoken(session_id, &text);
    }
}

/// `_speak_unspoken_texts` — narrate any assistant text blocks not yet spoken
/// for this session. Returns how many were emitted.
///
/// `finalize=true` marks the LAST block `final` (the Stop hook);
/// `finalize=false` marks them all `intermediate` (PreToolUse).
fn speak_unspoken_texts(
    daemon: &Daemon,
    path: &Path,
    session_id: &str,
    cwd: &str,
    cfg: HookCfg,
    finalize: bool,
) -> usize {
    // First encounter: without this guard `get_offset` returns 0 and every
    // historical assistant message is dumped into the speech path. Initialise
    // at EOF and seed the dedup hashes instead.
    if !daemon.spoken.has_offset(session_id) {
        daemon.spoken.initialize_at_eof(session_id, path, &[]);
        return 0;
    }
    let start = daemon.spoken.get_offset(session_id);
    let (fresh, end) = transcript::extract_assistant_texts_from(path, start);
    if end != start {
        daemon.spoken.set_offset(session_id, end);
    }
    let new_texts = daemon.spoken.filter_unspoken(session_id, &fresh);
    if new_texts.is_empty() {
        return 0;
    }

    let mut speakable: Vec<(String, String)> = Vec::new();
    for raw in new_texts {
        let clean = markdown::strip(&raw).into_owned();
        if clean.chars().count() < cfg.skip_under_chars {
            daemon.spoken.mark_spoken(session_id, &raw);
            continue;
        }
        speakable.push((raw, clean));
    }

    let last = speakable.len().saturating_sub(1);
    for (i, (raw, clean)) in speakable.iter().enumerate() {
        let is_last = finalize && i == last;
        let long = clean.chars().count() > 400;
        let (kind, tag) = match (is_last, long) {
            (true, true) => ("final", "final_long"),
            (true, false) => ("final", "final_short"),
            (false, true) => ("intermediate", "intermediate_long"),
            (false, false) => ("intermediate", "intermediate_short"),
        };
        let mut ctx = Map::new();
        ctx.insert("length".into(), Value::from(clean.chars().count()));
        daemon.handle_event(&NarrationEvent {
            kind: kind.into(),
            neutral: clean.clone(),
            tag: tag.into(),
            ctx,
            session_id: session_id.to_owned(),
            cwd: cwd.to_owned(),
        });
        daemon.spoken.mark_spoken(session_id, raw);
    }
    speakable.len()
}

// --- Claude Code handlers ---------------------------------------------------

async fn cc_stop(daemon: &Daemon, payload: &Value) {
    let cfg = cc_cfg(daemon);
    let Some(path) = transcript_path(payload) else {
        return;
    };
    tokio::time::sleep(cfg.flush_delay).await;
    let (session_id, cwd) = session_from(payload);
    let spoke = speak_unspoken_texts(daemon, &path, &session_id, &cwd, cfg, true);
    if spoke > 0 {
        return;
    }
    // No new text — fall back to the legacy "last assistant text" path so we
    // never go silent on edge-case transcripts.
    let text = transcript::extract_last_assistant_text(&path);
    let clean = markdown::strip(&text).into_owned();
    if clean.chars().count() >= cfg.skip_under_chars && !daemon.spoken.is_spoken(&session_id, &text)
    {
        emit_prose(daemon, &session_id, &cwd, &clean, true);
        daemon.spoken.mark_spoken(&session_id, &text);
    }
}

async fn cc_pre_tool(daemon: &Daemon, payload: &Value) {
    let cfg = cc_cfg(daemon);
    let (session_id, cwd) = session_from(payload);
    let transcript = transcript_path(payload);
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");

    // AskUserQuestion's popup races our async hook — preface prose would land
    // after the user answered. Mark it spoken (Stop won't replay it) and
    // narrate the question itself instead.
    if tool_name == "AskUserQuestion" {
        if let Some(path) = &transcript {
            tokio::time::sleep(cfg.flush_delay).await;
            mark_transcript_prose_spoken(daemon, path, &session_id);
        }
        if cfg.narrate_tools {
            let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
            if let Some(ev) = templates::pre_tool_event(tool_name, &input) {
                emit_template(daemon, "tool_pre", &ev, &session_id, &cwd, None);
            }
        }
        return;
    }

    // Surface any prose the agent wrote leading up to this tool call. If we
    // said something, the tool announcement is noise on top of it — skip it.
    let mut spoke_text = 0;
    if let Some(path) = &transcript {
        // Match Stop's flush wait: the agent may not have flushed the
        // assistant-prose line yet, so reading immediately misses prose
        // written right before the tool.
        tokio::time::sleep(cfg.flush_delay).await;
        spoke_text = speak_unspoken_texts(daemon, path, &session_id, &cwd, cfg, false);
    }
    if spoke_text > 0 {
        return;
    }
    if !cfg.narrate_tools {
        return;
    }
    let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let Some(ev) = templates::pre_tool_event(tool_name, &input) else {
        return;
    };
    // When the tool_pre DOES fire we enrich its ctx with the latest
    // transcript prose, so a brain downstream can produce a purposeful status
    // line instead of a bare template verb.
    let recent = transcript.as_ref().map(|p| {
        let text = transcript::extract_last_assistant_text(p);
        text.chars().take(RECENT_INTENT_CAP).collect::<String>()
    });
    emit_template(
        daemon,
        "tool_pre",
        &ev,
        &session_id,
        &cwd,
        recent.filter(|r| !r.is_empty()),
    );
}

fn cc_post_tool(daemon: &Daemon, payload: &Value) {
    let cfg = cc_cfg(daemon);
    if !cfg.narrate_tools || !cfg.narrate_tool_results {
        return;
    }
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let response = payload.get("tool_response").cloned().unwrap_or(Value::Null);
    let Some(ev) = templates::post_tool_event(tool_name, &response) else {
        return;
    };
    let (session_id, cwd) = session_from(payload);
    emit_template(daemon, "tool_post", &ev, &session_id, &cwd, None);
}

fn cc_user_prompt_submit(daemon: &Daemon, payload: &Value) {
    let cfg = cc_cfg(daemon);
    if !cfg.narrate_prompt_intent {
        return;
    }
    let prompt = payload
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if prompt.chars().count() < PROMPT_INTENT_MIN_CHARS {
        return;
    }
    let (session_id, cwd) = session_from(payload);
    let mut ctx = Map::new();
    ctx.insert("recent_intent".into(), Value::String(prompt.clone()));
    daemon.handle_event(&NarrationEvent {
        kind: "prompt_intent".into(),
        neutral: prompt,
        // `tag` doubles as the pierce key in the router, so this is spoken
        // immediately even in SWARM mode — by the time a digest drained, the
        // agent would already have replied.
        tag: "prompt_intent".into(),
        ctx,
        session_id,
        cwd,
    });
}

// --- Codex handlers ---------------------------------------------------------

async fn codex_stop(daemon: &Daemon, payload: &Value) {
    let (session_id, cwd) = session_from(payload);
    let cfg = codex_cfg(daemon, &cwd);
    // Codex hands us the assistant message directly. Use the transcript when
    // available so intermediate prose surfaces; fall back otherwise.
    let path = transcript_path(payload);
    if let Some(path) = &path {
        tokio::time::sleep(cfg.flush_delay).await;
        if speak_unspoken_texts(daemon, path, &session_id, &cwd, cfg, true) > 0 {
            return;
        }
    }
    let mut text = payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if text.is_empty() {
        if let Some(path) = &path {
            text = transcript::extract_last_assistant_text(path);
        }
    }
    let clean = markdown::strip(&text).into_owned();
    if clean.chars().count() < cfg.skip_under_chars {
        return;
    }
    if daemon.spoken.is_spoken(&session_id, &text) {
        return;
    }
    emit_prose(daemon, &session_id, &cwd, &clean, true);
    daemon.spoken.mark_spoken(&session_id, &text);
}

async fn codex_pre_tool(daemon: &Daemon, payload: &Value) {
    let (session_id, cwd) = session_from(payload);
    let cfg = codex_cfg(daemon, &cwd);
    let mut spoke_text = 0;
    if let Some(path) = transcript_path(payload) {
        tokio::time::sleep(cfg.flush_delay).await;
        spoke_text = speak_unspoken_texts(daemon, &path, &session_id, &cwd, cfg, false);
    }
    if spoke_text > 0 || !cfg.narrate_tools {
        return;
    }
    // Codex currently only emits Bash as a tool name.
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    if let Some(ev) = templates::pre_tool_event(tool_name, &input) {
        emit_template(daemon, "tool_pre", &ev, &session_id, &cwd, None);
    }
}

fn codex_post_tool(daemon: &Daemon, payload: &Value) {
    let (session_id, cwd) = session_from(payload);
    let cfg = codex_cfg(daemon, &cwd);
    if !cfg.narrate_tools || !cfg.narrate_tool_results {
        return;
    }
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let response = payload.get("tool_response").cloned().unwrap_or(Value::Null);
    if let Some(ev) = templates::post_tool_event(tool_name, &response) {
        emit_template(daemon, "tool_post", &ev, &session_id, &cwd, None);
    }
}

// --- emission ---------------------------------------------------------------

fn emit_template(
    daemon: &Daemon,
    kind: &str,
    narration: &templates::Narration<'_>,
    session_id: &str,
    cwd: &str,
    recent_intent: Option<String>,
) {
    let mut ctx = match narration.ctx_json() {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    if let Some(recent) = recent_intent {
        ctx.insert("recent_intent".into(), Value::String(recent));
    }
    daemon.handle_event(&NarrationEvent {
        kind: kind.to_owned(),
        neutral: narration.text.to_string(),
        tag: narration.tag.to_owned(),
        ctx,
        session_id: session_id.to_owned(),
        cwd: cwd.to_owned(),
    });
}

fn emit_prose(daemon: &Daemon, session_id: &str, cwd: &str, clean: &str, finalize: bool) {
    let chars = clean.chars().count();
    let (kind, tag) = match (finalize, chars > 400) {
        (true, true) => ("final", "final_long"),
        (true, false) => ("final", "final_short"),
        (false, true) => ("intermediate", "intermediate_long"),
        (false, false) => ("intermediate", "intermediate_short"),
    };
    let mut ctx = Map::new();
    ctx.insert("length".into(), Value::from(chars));
    daemon.handle_event(&NarrationEvent {
        kind: kind.into(),
        neutral: clean.to_owned(),
        tag: tag.into(),
        ctx,
        session_id: session_id.to_owned(),
        cwd: cwd.to_owned(),
    });
}

// --- per-session FIFO -------------------------------------------------------

/// One accepted hook payload, waiting for its lane.
#[derive(Debug, Clone)]
struct Job {
    agent: String,
    payload: Value,
    binding: Option<Value>,
}

#[derive(Debug, Default)]
struct Lane {
    queue: VecDeque<Job>,
    wake: Arc<Notify>,
}

#[derive(Debug, Default)]
struct Inner {
    lanes: HashMap<String, Lane>,
    /// Payloads accepted but not yet finished (queued OR mid-flight).
    /// Counted at SUBMIT, not at dequeue, so there is no window in which a
    /// payload is invisible to [`HookQueue::drain`] — an idle lane task is
    /// still alive, so liveness alone cannot tell "waiting for work" from
    /// "mid-flush-delay".
    outstanding: usize,
    stopping: bool,
    /// Payloads the backlog cap threw away. A test seam, as in the Python.
    dropped: usize,
}

/// `hook_processing.HookQueue`, on `tokio` tasks.
///
/// ORDERING CONTRACT: for any one session key, payloads are processed in the
/// order the daemon received them, on a single task — a PostToolUse can never
/// overtake its PreToolUse.
pub struct HookQueue {
    daemon: Arc<Daemon>,
    inner: Arc<Mutex<Inner>>,
    idle: Arc<Notify>,
    idle_timeout: Duration,
    max_pending: usize,
}

impl std::fmt::Debug for HookQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookQueue")
            .field("pending", &self.pending())
            .field("dropped", &self.dropped())
            .finish()
    }
}

impl HookQueue {
    /// A queue feeding `daemon`, with the Python's timings.
    pub fn new(daemon: Arc<Daemon>) -> Self {
        Self::with_limits(daemon, WORKER_IDLE_TIMEOUT, MAX_PENDING_PER_SESSION)
    }

    /// The same, with the idle timeout and backlog cap injected, so a test
    /// does not have to wait two minutes to watch a lane retire.
    pub fn with_limits(daemon: Arc<Daemon>, idle_timeout: Duration, max_pending: usize) -> Self {
        Self {
            daemon,
            inner: Arc::new(Mutex::new(Inner::default())),
            idle: Arc::new(Notify::new()),
            idle_timeout,
            max_pending,
        }
    }

    /// Enqueue one hook payload. `false` means it was refused (not an object,
    /// or the queue is stopping).
    pub fn submit(&self, agent: &str, payload: Value, binding: Option<Value>) -> bool {
        if !payload.is_object() {
            return false;
        }
        let key = session_key(agent, &payload);
        let wake = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.stopping {
                return false;
            }
            let max_pending = self.max_pending;
            let spawn_lane = !inner.lanes.contains_key(&key);
            let mut dropped = 0usize;
            let lane = inner.lanes.entry(key.clone()).or_default();
            while lane.queue.len() >= max_pending {
                if lane.queue.pop_front().is_none() {
                    break;
                }
                dropped += 1;
            }
            lane.queue.push_back(Job {
                agent: agent.to_owned(),
                payload,
                binding,
            });
            let wake = Arc::clone(&lane.wake);
            inner.outstanding += 1;
            inner.outstanding -= dropped;
            inner.dropped += dropped;
            if dropped > 0 {
                crate::dlog!("hook_backlog_dropped", session = key.as_str(), n = dropped);
            }
            if spawn_lane {
                self.spawn_lane(key.clone(), Arc::clone(&wake));
            }
            wake
        };
        wake.notify_one();
        true
    }

    fn spawn_lane(&self, key: String, wake: Arc<Notify>) {
        let daemon = Arc::clone(&self.daemon);
        let inner = Arc::clone(&self.inner);
        let idle = Arc::clone(&self.idle);
        let idle_timeout = self.idle_timeout;
        tokio::spawn(async move {
            loop {
                // Register interest BEFORE looking at the queue, so a submit
                // that lands between the peek and the await still wakes us.
                let notified = wake.notified();
                tokio::pin!(notified);

                let job = {
                    let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.stopping {
                        return;
                    }
                    guard.lanes.get_mut(&key).and_then(|l| l.queue.pop_front())
                };

                match job {
                    Some(job) => {
                        process(&daemon, &job.agent, &job.payload, job.binding.as_ref()).await;
                        {
                            let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
                            guard.outstanding = guard.outstanding.saturating_sub(1);
                        }
                        idle.notify_waiters();
                    }
                    None => {
                        // Nothing to do. Retire after the idle timeout, but
                        // re-check under the lock first: a submit may have
                        // raced in between the timeout and the lock.
                        let retire = tokio::select! {
                            _ = &mut notified => false,
                            _ = tokio::time::sleep(idle_timeout) => true,
                        };
                        if retire {
                            let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
                            let empty = guard.lanes.get(&key).is_none_or(|l| l.queue.is_empty());
                            if empty {
                                guard.lanes.remove(&key);
                                crate::dlog!("hook_lane_retired", session = key.as_str());
                                return;
                            }
                        }
                    }
                }
            }
        });
    }

    /// Payloads accepted but not yet processed, across every lane.
    pub fn pending(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .outstanding
    }

    /// How many the backlog cap threw away.
    pub fn dropped(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).dropped
    }

    /// How many lanes are alive. Test/observability helper.
    pub fn lanes(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lanes
            .len()
    }

    /// Block until nothing is queued or in flight. `false` on timeout.
    pub async fn drain(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.pending() == 0 {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            let notified = self.idle.notified();
            if self.pending() == 0 {
                return true;
            }
            let _ = tokio::time::timeout(Duration::from_millis(20), notified).await;
        }
    }

    /// Retire every lane. Safe to call more than once.
    pub fn stop(&self) {
        let wakes: Vec<Arc<Notify>> = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.stopping = true;
            inner.outstanding = 0;
            let wakes = inner.lanes.values().map(|l| Arc::clone(&l.wake)).collect();
            inner.lanes.clear();
            wakes
        };
        for wake in wakes {
            wake.notify_waiters();
            wake.notify_one();
        }
        self.idle.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_key_is_agent_and_session() {
        let payload = serde_json::json!({"session_id": "abc"});
        assert_eq!(session_key("claude-code", &payload), "claude-code:abc");
        assert_eq!(
            session_key("codex", &serde_json::json!({})),
            "codex:default"
        );
        assert_eq!(
            session_key("codex", &serde_json::json!({"session_id": ""})),
            "codex:default"
        );
    }

    #[test]
    fn the_agent_table_is_the_python_one() {
        assert!(handles("claude-code", "UserPromptSubmit"));
        assert!(!handles("codex", "UserPromptSubmit"));
        assert!(!handles("aider", "Stop"));
        assert!(is_known("codex"));
        assert!(!is_known("aider"));
    }
}
