//! Layer 2 — Agent State.
//!
//! The "scoreboard." Tracks one record per active agent (CC / Codex session)
//! with facts + cheap heuristic hints derived deterministically from observed
//! events. Updated on every event. Always queryable.
//!
//! **Boundary rule (must be enforced — see `.local/architecture-v2.md`):**
//! This module reports facts and *labeled-as-hint* classifications. It NEVER
//! calls an LLM. It NEVER makes decisions. The rule of thumb is: if it can be
//! computed by a function from raw event data, it's Layer 2; if it requires
//! judgment or context beyond the current event, it belongs to Layer 5 (the
//! harness).
//!
//! Heuristic hints (`response_shape`, `salience`) are pre-computed for two
//! reasons:
//!
//! 1. **Cost** — if N agents are active, redoing N classifications on every
//!    harness call is wasteful. Heuristics save harness reasoning.
//! 2. **Always-on availability** — `heard status` (and any other layer above)
//!    can read the hint without waking Layer 5.
//!
//! The hints are *suggestions*. Layer 5 can override them when richer context
//! warrants. This module never claims authority over the decision.
//!
//! Sibling to [`crate::session`], which keeps the smaller failure-count /
//! tool-density bookkeeping the multi-agent router relies on. Eventually the
//! two could consolidate; for now they're additive.
//!
//! ## One deliberate departure from the Python
//!
//! * The `.heard.yaml` feature label is resolved through [`LabelResolver`]
//!   rather than by calling `config.project_label` directly — that YAML walk
//!   belongs to `heard-config`, which this crate must not reach into yet.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::clock::{Clock, SystemClock};
use crate::event::{AgentEvent, EventKind};
use crate::project_name::{GitProjectNamer, ProjectNamer};

/// How long without an event before an agent is considered idle (for the
/// salience hint + active-list filtering). Doesn't evict — the record stays
/// around for inspection.
pub const IDLE_AFTER_S: f64 = 30.0;

/// Eviction window — agents with no event in this long are removed from the
/// registry to keep memory bounded. ~CC session lifetimes, generous enough that
/// the daemon never accidentally forgets a long-running but quiet agent.
pub const EVICT_AFTER_S: f64 = 6.0 * 3600.0;

/// Rolling window of recent output sizes — drives the response-shape hint.
/// Last K entries; older drop off.
pub const RECENT_OUTPUTS_KEEP: usize = 8;

/// Token-count thresholds for the response-shape hint. Approximated as ~4 chars
/// per token (close enough for a heuristic; the hint is overridable
/// downstream).
const SHORT_TOKENS: usize = 80;
const LONG_TOKENS: usize = 400;

/// Rough token estimate from char count. Doesn't need to match a real
/// tokenizer — we use it only for short/long bucketing.
///
/// Note this counts **characters**, not bytes: Python's `len(text)` on a `str`
/// is a code-point count, so `len("café") == 4`.
pub fn approx_tokens(text: &str) -> usize {
    (text.chars().count() / 4).max(1)
}

/// The response-shape hint. Values are the strings the socket payload and
/// `heard status` already carry, so they are part of the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseShape {
    ShortExecution,
    LongDeliberation,
    Mixed,
}

impl ResponseShape {
    pub fn as_str(self) -> &'static str {
        match self {
            ResponseShape::ShortExecution => "short-execution",
            ResponseShape::LongDeliberation => "long-deliberation",
            ResponseShape::Mixed => "mixed",
        }
    }
}

/// The salience hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Salience {
    ActiveDecision,
    Routine,
    Blocked,
}

impl Salience {
    pub fn as_str(self) -> &'static str {
        match self {
            Salience::ActiveDecision => "active-decision",
            Salience::Routine => "routine",
            Salience::Blocked => "blocked",
        }
    }
}

/// Resolves the human feature/area label from an agent's `.heard.yaml`
/// `label:` key.
///
/// The Python reads it through `config.project_label(cwd)`, which walks up for
/// the nearest `.heard.yaml` and returns its `label:`. That YAML walk is
/// `heard-config`'s job, so the seam is a trait and this
/// crate ships the honest default: no label, and `resolved_area()` falls back
/// to the dominant touched directory exactly as it does today when the key is
/// unset.
pub trait LabelResolver: Send + Sync {
    fn project_label(&self, cwd: &str) -> Option<String>;
}

/// No `.heard.yaml` label. Every agent falls through to the derived area.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoLabel;

impl LabelResolver for NoLabel {
    fn project_label(&self, _cwd: &str) -> Option<String> {
        None
    }
}

/// Per-agent record. All fields are facts or heuristic hints — no LLM-derived
/// state. Times are monotonic seconds; `last_event_wall` is also stored as
/// wall-clock for human-readable display.
#[derive(Debug, Clone)]
pub struct AgentState {
    // --- identity / lifecycle ---
    /// The session id.
    pub id: String,
    pub cwd: Option<String>,
    pub repo_name: Option<String>,
    /// Human feature/area label from the agent's `.heard.yaml` `label:`
    /// (resolved once when cwd is first seen). Lets the brain disambiguate
    /// concurrent agents in the same repo by feature ("Heard analytics" vs "the
    /// frontend"). `None` when unset — [`AgentState::resolved_area`] then
    /// derives one from touched files. LOCAL / narration only; never enters an
    /// analytics payload.
    pub label: Option<String>,
    pub started_at: f64,
    pub last_event_at: f64,
    pub last_event_wall: f64,

    // --- current activity ---
    pub current_tool: Option<String>,
    pub current_tool_started_at: Option<f64>,

    // --- history facts ---
    pub last_tool: Option<String>,
    pub last_tool_duration_s: Option<f64>,
    /// Files this agent touched, INSERTION-ORDERED, oldest first; touching a
    /// file again moves it to the end. Python used a `set` here, whose
    /// iteration order is salted per process — which made `_derived_area`'s
    /// `max()` tie-break vary run to run and made `files_touched_recent` an
    /// arbitrary five paths rather than the five most recent. Both sides now
    /// keep touch order, which is what the name promises.
    pub files_touched: Vec<String>,
    pub error_count: u64,
    pub last_user_input_at: Option<f64>,
    pub last_user_input_wall: Option<f64>,
    pub event_count: u64,

    /// Rolling window of recent assistant-output token counts (for the
    /// response-shape hint). Newest at the back.
    pub recent_output_tokens: VecDeque<usize>,

    // --- heuristic hints (labeled as hints — not gospel) ---
    pub response_shape_hint: ResponseShape,
    pub salience_hint: Salience,
}

impl AgentState {
    pub fn new(id: impl Into<String>, now_mono: f64, now_wall: f64) -> Self {
        Self {
            id: id.into(),
            cwd: None,
            repo_name: None,
            label: None,
            started_at: now_mono,
            last_event_at: now_mono,
            last_event_wall: now_wall,
            current_tool: None,
            current_tool_started_at: None,
            last_tool: None,
            last_tool_duration_s: None,
            files_touched: Vec::new(),
            error_count: 0,
            last_user_input_at: None,
            last_user_input_wall: None,
            event_count: 0,
            recent_output_tokens: VecDeque::new(),
            response_shape_hint: ResponseShape::Mixed,
            salience_hint: Salience::Routine,
        }
    }

    /// Fallback area when no `.heard.yaml` label is set: the dominant
    /// immediate-parent directory of touched files ("components",
    /// "analytics"). Heuristic — a stable label beats it, but it degrades
    /// gracefully to SOMETHING finer than the repo name with zero config.
    fn derived_area(&self) -> Option<String> {
        if self.files_touched.is_empty() {
            return None;
        }
        // Insertion-ordered counts over an insertion-ordered file list, so the
        // `max()` tie-break is "the directory touched first" — the same rule
        // Python's `max()` over its insertion-ordered dict applies.
        let mut dirs: Vec<(String, usize)> = Vec::new();
        for path in &self.files_touched {
            let trimmed = path.trim_end_matches('/');
            let parent = std::path::Path::new(trimmed).parent();
            let name = parent
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if name.is_empty() {
                continue;
            }
            match dirs.iter_mut().find(|(d, _)| d == name) {
                Some(entry) => entry.1 += 1,
                None => dirs.push((name.to_string(), 1)),
            }
        }
        dirs.into_iter()
            .fold(None::<(String, usize)>, |best, candidate| match best {
                Some(best) if best.1 >= candidate.1 => Some(best),
                _ => Some(candidate),
            })
            .map(|(name, _)| name)
    }

    /// Feature/area label finer than the repo: the explicit `.heard.yaml`
    /// `label:` if set, else the dominant touched directory. `None` when
    /// neither is available — the brain then disambiguates by `repo_name`
    /// alone.
    pub fn resolved_area(&self) -> Option<String> {
        self.label.clone().or_else(|| self.derived_area())
    }

    /// Record a touched file, newest last.
    ///
    /// Dedups by moving an already-seen path to the end rather than keeping
    /// its first position: `files_touched_recent` promises the five most
    /// RECENTLY touched files, and a file edited three times over an hour is
    /// recent, not stale.
    fn touch_file(&mut self, path: &str) {
        if path.is_empty() {
            return;
        }
        if let Some(idx) = self.files_touched.iter().position(|p| p == path) {
            self.files_touched.remove(idx);
        }
        self.files_touched.push(path.to_string());
    }

    pub fn idle_seconds(&self, now: f64) -> f64 {
        (now - self.last_event_at).max(0.0)
    }

    pub fn is_active(&self, idle_after_s: f64, now: f64) -> bool {
        self.idle_seconds(now) <= idle_after_s
    }

    /// The deterministic half of Python's `to_dict()`.
    ///
    /// `to_dict` also carries `idle_seconds`, `last_event_wall`,
    /// `last_user_input_wall`, `files_touched_count` and
    /// `files_touched_recent`. The three clock fields are machine facts and the
    /// two file fields are derivable from `files_touched`, so the snapshot the
    /// corpus pins records presence rather than value for the clocks and the
    /// full touch-ordered list for the files.
    pub fn snapshot(&self) -> AgentSnapshot {
        AgentSnapshot {
            id: self.id.clone(),
            cwd: self.cwd.clone(),
            repo_name: self.repo_name.clone(),
            area: self.resolved_area(),
            current_tool: self.current_tool.clone(),
            current_tool_running: self.current_tool_started_at.is_some(),
            last_tool: self.last_tool.clone(),
            has_last_tool_duration: self.last_tool_duration_s.is_some(),
            files_touched: self.files_touched.to_vec(),
            error_count: self.error_count,
            event_count: self.event_count,
            has_last_user_input: self.last_user_input_at.is_some(),
            recent_output_tokens: self.recent_output_tokens.iter().copied().collect(),
            response_shape_hint: self.response_shape_hint.as_str().to_string(),
            salience_hint: self.salience_hint.as_str().to_string(),
        }
    }
}

/// Serializable per-agent snapshot, matching `fixtures/state/agent_state.json`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentSnapshot {
    pub id: String,
    pub cwd: Option<String>,
    pub repo_name: Option<String>,
    pub area: Option<String>,
    pub current_tool: Option<String>,
    pub current_tool_running: bool,
    pub last_tool: Option<String>,
    pub has_last_tool_duration: bool,
    pub files_touched: Vec<String>,
    pub error_count: u64,
    pub event_count: u64,
    pub has_last_user_input: bool,
    pub recent_output_tokens: Vec<usize>,
    pub response_shape_hint: String,
    pub salience_hint: String,
}

/// Pull the tool name out of an event tag like `tool_bash` or `tool_post_bash`.
///
/// Returns `None` when the tag is a generic outcome marker
/// (`tool_post_failure`, `tool_post_command_failed`) or for non-tool tags
/// (prose, etc.) — those don't carry a tool name, and without the guard the
/// caller ends up with `last_tool = "failure"`.
pub fn tool_name_from_tag(tag: &str) -> Option<String> {
    if tag.is_empty() {
        return None;
    }
    let mut parts = tag.split('_');
    if parts.next() != Some("tool") {
        return None;
    }
    let mut rest: Vec<&str> = parts.collect();
    if matches!(rest.first(), Some(&"pre") | Some(&"post")) {
        rest.remove(0);
    }
    let first = rest.first()?;
    // Generic outcome markers (failure / success / command_failed).
    if matches!(*first, "failure" | "success" | "command") {
        return None;
    }
    Some(rest.join("_"))
}

/// Rule-based: average over the rolling output window.
///
/// `short-execution` if all recent outputs sit below the short threshold;
/// `long-deliberation` if any cross the long threshold; `mixed` otherwise.
/// Empty window → `mixed` (no signal yet).
pub fn compute_response_shape_hint(window: &VecDeque<usize>) -> ResponseShape {
    if window.is_empty() {
        return ResponseShape::Mixed;
    }
    if window.iter().any(|t| *t >= LONG_TOKENS) {
        if window.iter().all(|t| *t >= SHORT_TOKENS) {
            return ResponseShape::LongDeliberation;
        }
        return ResponseShape::Mixed;
    }
    if window.iter().all(|t| *t < SHORT_TOKENS) {
        return ResponseShape::ShortExecution;
    }
    ResponseShape::Mixed
}

/// Rule-based:
///   - blocked: error in the last 2 events (a fresh failure sticking out)
///   - active-decision: agent currently running a tool OR very recent long
///     output (likely a decision moment for the user)
///   - routine: default
pub fn compute_salience_hint(state: &AgentState, now: f64) -> Salience {
    if state.error_count > 0 && state.event_count <= 2 {
        // Fresh failure in a short-lived agent — almost certainly something the
        // user wants to see.
        return Salience::Blocked;
    }
    // Recent failures within the last few events?
    if state.error_count > 0 && state.idle_seconds(now) < IDLE_AFTER_S {
        // Failure recent enough to still be relevant.
        return Salience::Blocked;
    }
    if state.current_tool.is_some() {
        return Salience::ActiveDecision;
    }
    if let Some(last) = state.recent_output_tokens.back() {
        if *last >= LONG_TOKENS {
            // Most recent output was long — likely a deliberation moment the
            // user should attend to.
            return Salience::ActiveDecision;
        }
    }
    Salience::Routine
}

/// Thread-safe per-agent registry. The daemon owns one instance and calls
/// [`AgentStateRegistry::observe`] from `_handle_event` on every incoming agent
/// event.
pub struct AgentStateRegistry {
    agents: Mutex<HashMap<String, AgentState>>,
    clock: Arc<dyn Clock>,
    namer: Arc<dyn ProjectNamer>,
    labels: Arc<dyn LabelResolver>,
}

impl AgentStateRegistry {
    /// The production wiring: real clock, git-backed project names, no
    /// `.heard.yaml` labels until `heard-config` lands.
    pub fn new() -> Self {
        Self::with(
            Arc::new(SystemClock::new()),
            Arc::new(GitProjectNamer::new()),
            Arc::new(NoLabel),
        )
    }

    pub fn with(
        clock: Arc<dyn Clock>,
        namer: Arc<dyn ProjectNamer>,
        labels: Arc<dyn LabelResolver>,
    ) -> Self {
        Self {
            agents: Mutex::new(HashMap::new()),
            clock,
            namer,
            labels,
        }
    }

    /// Update the agent's state from one event payload. Returns the updated
    /// state.
    ///
    /// Python's signature says "or None for malformed events", but no branch
    /// ever returns `None`: an empty dict lands under the `"default"` session
    /// and is counted. `tests/test_agent_state.py` pins that, so the port
    /// returns the state unconditionally rather than an `Option` nothing fills.
    pub fn observe(&self, event: &AgentEvent) -> AgentState {
        let sid = event.session_id().to_string();
        let cwd = event.cwd().map(str::to_string);
        let now_mono = self.clock.monotonic();
        let now_wall = self.clock.wall();

        let mut agents = self.agents.lock().expect("agent registry poisoned");
        agents.retain(|_, a| now_mono - a.last_event_at <= EVICT_AFTER_S);

        let state = agents
            .entry(sid.clone())
            .or_insert_with(|| AgentState::new(sid.clone(), now_mono, now_wall));

        state.last_event_at = now_mono;
        state.last_event_wall = now_wall;
        state.event_count += 1;
        if let Some(cwd) = cwd {
            if state.cwd.is_none() {
                // Canonical name = git remote slug → folder basename (see
                // project_name.rs). A git subprocess, but cached per path —
                // still a deterministic fact, not a judgment, so the Layer-2
                // boundary holds.
                let name = self.namer.canonical_project_name(&cwd);
                state.repo_name = (!name.is_empty()).then_some(name);
                // Resolve the `.heard.yaml` feature label once, at the cwd we
                // first see for this agent. Cheap (one file walk); local-only.
                state.label = self.labels.project_label(&cwd);
                state.cwd = Some(cwd);
            }
        }

        let tool_name = tool_name_from_tag(event.tag());

        match event.kind {
            EventKind::ToolPre => {
                state.current_tool = tool_name;
                state.current_tool_started_at = Some(now_mono);
            }
            EventKind::ToolPost => {
                if matches!(
                    event.tag(),
                    "tool_post_failure" | "tool_post_command_failed"
                ) {
                    state.error_count += 1;
                }
                // Clear current_tool — duration is current_tool_started_at → now.
                if let Some(started) = state.current_tool_started_at {
                    state.last_tool_duration_s = Some((now_mono - started).max(0.0));
                }
                if let Some(name) = tool_name {
                    state.last_tool = Some(name);
                }
                state.current_tool = None;
                state.current_tool_started_at = None;
                // File touch — Edit/Write/NotebookEdit set abs_path.
                if let Some(path) = event.abs_path() {
                    state.touch_file(path);
                }
            }
            EventKind::PromptIntent => {
                state.last_user_input_at = Some(now_mono);
                state.last_user_input_wall = Some(now_wall);
            }
            EventKind::Intermediate | EventKind::Final => {
                // Rolling window of approximate output sizes.
                if state.recent_output_tokens.len() == RECENT_OUTPUTS_KEEP {
                    state.recent_output_tokens.pop_front();
                }
                state
                    .recent_output_tokens
                    .push_back(approx_tokens(event.neutral()));
            }
            EventKind::Other | EventKind::Named(_) => {}
        }

        // Recompute hints. Cheap; one pass over a small window.
        state.response_shape_hint = compute_response_shape_hint(&state.recent_output_tokens);
        state.salience_hint = compute_salience_hint(state, now_mono);
        state.clone()
    }

    pub fn get(&self, session_id: &str) -> Option<AgentState> {
        self.agents
            .lock()
            .expect("agent registry poisoned")
            .get(session_id)
            .cloned()
    }

    /// All agents that have produced an event within `idle_after_s`.
    pub fn all_active(&self, idle_after_s: f64) -> Vec<AgentState> {
        let now = self.clock.monotonic();
        let mut out: Vec<AgentState> = self
            .agents
            .lock()
            .expect("agent registry poisoned")
            .values()
            .filter(|a| a.is_active(idle_after_s, now))
            .cloned()
            .collect();
        // Python returns dict-iteration order here. Nothing downstream depends
        // on it and a HashMap has none to offer, so sort by id: stable, and it
        // makes `summary()` reproducible.
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    pub fn all(&self) -> Vec<AgentState> {
        let mut out: Vec<AgentState> = self
            .agents
            .lock()
            .expect("agent registry poisoned")
            .values()
            .cloned()
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Serializable per-agent snapshot for `heard status` and the daemon's
    /// status socket reply. Includes only active agents to keep the payload
    /// small.
    pub fn summary(&self) -> Vec<AgentSnapshot> {
        self.all_active(IDLE_AFTER_S)
            .iter()
            .map(AgentState::snapshot)
            .collect()
    }

    /// Test helper — drop all state. Never called from prod.
    pub fn clear(&self) {
        self.agents.lock().expect("agent registry poisoned").clear();
    }

    /// Backdate an agent's last event, the way the Python tests assign
    /// `reg.get("s1").last_event_at = time.monotonic() - …` to simulate an
    /// agent going quiet. Test-only; production has no reason to rewrite a
    /// timestamp.
    pub fn backdate_for_test(&self, session_id: &str, last_event_at: f64) {
        if let Some(state) = self
            .agents
            .lock()
            .expect("agent registry poisoned")
            .get_mut(session_id)
        {
            state.last_event_at = last_event_at;
        }
    }
}

impl Default for AgentStateRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::project_name::BasenameProjectNamer;

    fn registry(clock: Arc<ManualClock>) -> AgentStateRegistry {
        AgentStateRegistry::with(clock, Arc::new(BasenameProjectNamer), Arc::new(NoLabel))
    }

    fn event(json: &str) -> AgentEvent {
        serde_json::from_str(json).expect("fixture event parses")
    }

    #[test]
    fn all_active_skips_idle_agents() {
        // tests/test_agent_state.py::test_all_active_skips_idle_agents
        let clock = Arc::new(ManualClock::new(1000.0));
        let reg = registry(clock.clone());
        reg.observe(&event(
            r#"{"session": {"id": "s1"}, "kind": "tool_pre", "tag": "tool_bash"}"#,
        ));
        reg.observe(&event(
            r#"{"session": {"id": "s2"}, "kind": "tool_pre", "tag": "tool_edit"}"#,
        ));
        reg.backdate_for_test("s1", clock.now() - (IDLE_AFTER_S + 60.0));
        let ids: Vec<String> = reg
            .all_active(IDLE_AFTER_S)
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, vec!["s2".to_string()]);
        assert_eq!(reg.summary().len(), 1);
    }

    #[test]
    fn eviction_drops_an_agent_quiet_past_the_window() {
        let clock = Arc::new(ManualClock::new(0.0));
        let reg = registry(clock.clone());
        reg.observe(&event(r#"{"session": {"id": "old"}, "kind": "tool_pre"}"#));
        clock.advance(EVICT_AFTER_S + 1.0);
        reg.observe(&event(r#"{"session": {"id": "new"}, "kind": "tool_pre"}"#));
        assert!(reg.get("old").is_none());
        assert!(reg.get("new").is_some());
    }

    #[test]
    fn last_tool_duration_uses_the_monotonic_clock() {
        let clock = Arc::new(ManualClock::new(500.0));
        let reg = registry(clock.clone());
        reg.observe(&event(
            r#"{"session": {"id": "s1"}, "kind": "tool_pre", "tag": "tool_bash"}"#,
        ));
        clock.advance(2.25);
        let state = reg.observe(&event(
            r#"{"session": {"id": "s1"}, "kind": "tool_post", "tag": "tool_post_bash"}"#,
        ));
        assert_eq!(state.last_tool_duration_s, Some(2.25));
        assert_eq!(state.last_tool.as_deref(), Some("bash"));
        assert!(state.current_tool.is_none());
    }

    #[test]
    fn derived_area_breaks_ties_by_first_touched() {
        // Equal counts resolve to the directory touched FIRST, in whichever
        // order the events arrived — the rule Python now follows too.
        let clock = Arc::new(ManualClock::new(0.0));
        let reg = registry(clock);
        for path in ["/repo/zeta/a.ts", "/repo/analytics/b.ts"] {
            reg.observe(&event(&format!(
                r#"{{"session": {{"id": "s1"}}, "kind": "tool_post",
                    "tag": "tool_post_edit", "ctx": {{"abs_path": "{path}"}}}}"#
            )));
        }
        assert_eq!(
            reg.get("s1").expect("agent").resolved_area().as_deref(),
            Some("zeta")
        );

        let reg = registry(Arc::new(ManualClock::new(0.0)));
        for path in ["/repo/analytics/b.ts", "/repo/zeta/a.ts"] {
            reg.observe(&event(&format!(
                r#"{{"session": {{"id": "s1"}}, "kind": "tool_post",
                    "tag": "tool_post_edit", "ctx": {{"abs_path": "{path}"}}}}"#
            )));
        }
        assert_eq!(
            reg.get("s1").expect("agent").resolved_area().as_deref(),
            Some("analytics")
        );
    }

    #[test]
    fn touching_a_file_again_moves_it_to_the_end() {
        let reg = registry(Arc::new(ManualClock::new(0.0)));
        for path in ["/repo/a/one.py", "/repo/a/two.py", "/repo/a/one.py"] {
            reg.observe(&event(&format!(
                r#"{{"session": {{"id": "s1"}}, "kind": "tool_post",
                    "tag": "tool_post_edit", "ctx": {{"abs_path": "{path}"}}}}"#
            )));
        }
        assert_eq!(
            reg.get("s1").expect("agent").files_touched,
            vec!["/repo/a/two.py".to_string(), "/repo/a/one.py".to_string()]
        );
    }

    #[test]
    fn an_explicit_label_beats_the_derived_area() {
        struct Fixed;
        impl LabelResolver for Fixed {
            fn project_label(&self, _cwd: &str) -> Option<String> {
                Some("Heard analytics".to_string())
            }
        }
        let reg = AgentStateRegistry::with(
            Arc::new(ManualClock::new(0.0)),
            Arc::new(BasenameProjectNamer),
            Arc::new(Fixed),
        );
        reg.observe(&event(
            r#"{"session": {"id": "s1", "cwd": "/repo"}, "kind": "tool_post",
                "tag": "tool_post_edit", "ctx": {"abs_path": "/repo/components/x.ts"}}"#,
        ));
        assert_eq!(
            reg.get("s1").expect("agent").resolved_area().as_deref(),
            Some("Heard analytics")
        );
    }

    #[test]
    fn approx_tokens_counts_characters_not_bytes() {
        assert_eq!(approx_tokens(""), 1);
        assert_eq!(approx_tokens("café"), 1);
        assert_eq!(approx_tokens(&"é".repeat(400)), 100);
    }
}
