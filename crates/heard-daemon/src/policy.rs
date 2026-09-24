//! `heard/narration_policy.py` — should this agent event speak, and how? ONE
//! ordered table — plus the feature sources `_handle_event` computes for it.
//!
//! The table is evaluated top to bottom; the first rule whose predicate holds
//! decides. Every must-speak rule sits above every may-drop rule, so a new
//! gate cannot be added "before" a must-speak rule by accident: the list is
//! the order. [`decide`] is pure over a [`Features`] value. Five features have
//! side effects when computed (the prompt-watch poll, the tool-density record,
//! the duplicate-line register, the verbosity profile reads, the budget) and
//! are THUNKS, called only when their rule is reached — exactly when the
//! Python computes them.
//!
//! The feature sources are ported next to the table, each named after the
//! Python it came from:
//!
//! | here | Python |
//! |---|---|
//! | [`narration_mode`], [`volume_stop`] | `cadence.narration_mode` / `volume_stop` |
//! | [`budget_wpm`], [`Budget`] | `volume.budget_wpm` / `volume.Budget` |
//! | [`first_run_held`] | `first_run.state(cfg).held` |
//! | [`speakup_allows`] | `Daemon._speakup_allows` |
//! | [`is_critical_template_event`], [`is_focus_template_event`], [`is_focus_attention_event`], [`focus_prompt_text`], [`focus_prompt_speech`], [`should_use_fast_path`] | `harness.*` |
//!
//! The golden corpus (`fixtures/daemon/narration_policy.json`, made by the
//! fixture generator (not included) from the Python reference implementation) replays
//! (event × config × state) cases through these and [`decide`].

use std::time::Instant;

use heard_narrate::verbosity::Decision as Verbosity;
use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// Python value semantics for an untyped config dict.

/// Python truthiness of a config value.
pub fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `bool(cfg.get(key, default))`.
pub fn cfg_truthy(cfg: &Map<String, Value>, key: &str, default: bool) -> bool {
    cfg.get(key).map(py_truthy).unwrap_or(default)
}

/// `str(v)` for a JSON-shaped value (only what configs carry).
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.abs() < 1e16 {
                    format!("{f:.1}")
                } else {
                    f.to_string()
                }
            } else {
                n.to_string()
            }
        }
        other => other.to_string(),
    }
}

/// `int(v)`: `None` where Python raises (`TypeError`, `ValueError`,
/// `OverflowError`).
pub fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else {
                n.as_f64()
                    .filter(|f| f.is_finite())
                    .map(|f| f.trunc() as i64)
            }
        }
        Value::String(s) => py_int_str(s),
        _ => None,
    }
}

/// `int(str)`: surrounding whitespace, an optional sign, digits with single
/// underscores between them.
fn py_int_str(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    let n: i64 = clean.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// `x == 1` in Python for a config value: `1`, `1.0` and `True` all equal 1.
fn py_eq_one(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() == Some(1.0),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// volume.py / cadence.py

/// `volume.NEEDS_ME`.
pub const NEEDS_ME: i64 = 0;
/// `volume.LOW`.
pub const LOW: i64 = 1;
/// `volume.NORMAL`.
pub const NORMAL: i64 = 2;
/// `volume.HIGH`.
pub const HIGH: i64 = 3;

/// `volume.stop(value)` → the stop index, `None` when unset / out of range.
pub fn volume_stop_of(value: &Value) -> Option<i64> {
    py_int(value).filter(|i| (0..4).contains(i))
}

/// `volume.budget_wpm(value)` — routine words per minute at this stop;
/// `None` = unlimited (or unset).
pub fn budget_wpm(value: &Value) -> Option<usize> {
    match volume_stop_of(value)? {
        0 | 1 => Some(100),
        2 => Some(200),
        _ => None,
    }
}

/// `volume.Budget` — a rolling one-minute count of routine words spoken.
#[derive(Debug, Default)]
pub struct Budget {
    events: Vec<(Instant, usize)>,
}

impl Budget {
    /// `Budget.WINDOW_S`.
    pub const WINDOW_S: f64 = 60.0;

    /// `note(words, now)`.
    pub fn note(&mut self, words: usize, now: Instant) {
        self.events.push((now, words));
        self.trim(now);
    }

    /// `spent(now)`.
    pub fn spent(&mut self, now: Instant) -> usize {
        self.trim(now);
        self.events.iter().map(|(_, n)| n).sum()
    }

    /// `exhausted(limit, now)`.
    pub fn exhausted(&mut self, limit: Option<usize>, now: Instant) -> bool {
        match limit {
            Some(l) => self.spent(now) >= l,
            None => false,
        }
    }

    fn trim(&mut self, now: Instant) {
        self.events
            .retain(|(t, _)| now.saturating_duration_since(*t).as_secs_f64() <= Self::WINDOW_S);
    }
}

/// `str(cfg.get(key) or "").strip().lower()`.
fn norm(cfg: &Map<String, Value>, key: &str) -> String {
    match cfg.get(key) {
        Some(v) if py_truthy(v) => py_str(v).trim().to_lowercase(),
        _ => String::new(),
    }
}

/// `cadence.legacy_mode` — the stored `mode`, fail-closed to copilot.
pub fn legacy_mode(cfg: &Map<String, Value>) -> String {
    let m = norm(cfg, "mode");
    match m.as_str() {
        "copilot" | "companion" | "focus" => m,
        "custom" => m,
        _ => "copilot".into(),
    }
}

/// `volume.stop_for_legacy`.
pub fn stop_for_legacy(narrate_routine: bool, verbosity: &str) -> i64 {
    let v = if verbosity.is_empty() {
        "normal".to_string()
    } else {
        verbosity.to_lowercase()
    };
    if narrate_routine {
        return HIGH;
    }
    if v == "quiet" || v == "brief" {
        LOW
    } else {
        NORMAL
    }
}

/// `cadence.volume_stop`.
pub fn volume_stop(cfg: &Map<String, Value>) -> i64 {
    if let Some(s) = cfg.get("narration_volume").and_then(volume_stop_of) {
        return s;
    }
    match legacy_mode(cfg).as_str() {
        "focus" => return NEEDS_ME,
        "companion" => return HIGH,
        _ => {}
    }
    let verbosity = match cfg.get("verbosity") {
        Some(v) if py_truthy(v) => py_str(v),
        _ => "normal".into(),
    };
    stop_for_legacy(cfg_truthy(cfg, "narrate_routine", false), &verbosity)
}

/// `cadence.narration_mode` — WHAT is said follows the volume dial.
pub fn narration_mode(cfg: &Map<String, Value>) -> &'static str {
    match volume_stop(cfg) {
        NEEDS_ME => "focus",
        HIGH => "companion",
        _ => "copilot",
    }
}

// ---------------------------------------------------------------------------
// first_run.py

/// `first_run.State`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstRunState {
    /// The installation-wide hold is on.
    pub held: bool,
    /// `first_run_generation`.
    pub generation: i64,
    /// `first_run_completed_generation`.
    pub completed_generation: i64,
}

/// `first_run.state(cfg)`.
pub fn first_run_state(cfg: &Map<String, Value>) -> FirstRunState {
    let mut malformed = false;
    let mut generation = |key: &str| -> i64 {
        let v = match cfg.get(key) {
            Some(v) if py_truthy(v) => v.clone(),
            _ => Value::from(0),
        };
        match py_int(&v) {
            Some(i) => i.max(0),
            None => {
                malformed = true;
                0
            }
        }
    };
    let gen = generation("first_run_generation");
    let completed = generation("first_run_completed_generation");
    FirstRunState {
        held: malformed || !cfg_truthy(cfg, "onboarded", false) || (gen > 0 && completed != gen),
        generation: gen,
        completed_generation: completed,
    }
}

/// `first_run.state(cfg).held` — the installation-wide first-run hold.
pub fn first_run_held(cfg: &Map<String, Value>) -> bool {
    let mut malformed = false;
    let mut generation = |key: &str| -> i64 {
        let v = match cfg.get(key) {
            Some(v) if py_truthy(v) => v.clone(),
            _ => Value::from(0),
        };
        match py_int(&v) {
            Some(i) => i.max(0),
            None => {
                malformed = true;
                0
            }
        }
    };
    let gen = generation("first_run_generation");
    let completed = generation("first_run_completed_generation");
    malformed || !cfg_truthy(cfg, "onboarded", false) || (gen > 0 && completed != gen)
}

// ---------------------------------------------------------------------------
// Daemon._speakup_allows

const SPEAKUP_DECISION_HINTS: &[&str] = &[
    "approv",
    "review",
    "decid",
    "confirm",
    "should i",
    "want me to",
    "sign off",
    "your call",
    "go ahead",
];

/// `Daemon._speakup_allows` — Settings → "Speak up on". False ONLY when the
/// event's salient category is switched off.
pub fn speakup_allows(cfg: &Map<String, Value>, kind: &str, tag: &str, neutral: &str) -> bool {
    let notify_on = |name: &str| -> bool {
        match cfg.get(&format!("notify.{name}")) {
            Some(v) => py_truthy(v),
            None => cfg
                .get(&format!("notify_{name}"))
                .map(py_truthy)
                .unwrap_or(true),
        }
    };
    let tl = tag.to_lowercase();
    if tl.contains("failure") || tl.contains("failed") {
        return notify_on("errors");
    }
    if tl == "tool_question" {
        let low = neutral.to_lowercase();
        if SPEAKUP_DECISION_HINTS.iter().any(|h| low.contains(h)) {
            return notify_on("blocked");
        }
        return true;
    }
    if kind == "final" {
        return notify_on("completions");
    }
    true
}

// ---------------------------------------------------------------------------
// harness.py gates, over the event fields the daemon has.

/// The event fields the harness gates read.
#[derive(Debug, Clone, Copy)]
pub struct EventView<'a> {
    /// `event["kind"]`.
    pub kind: &'a str,
    /// `event["tag"]`.
    pub tag: &'a str,
    /// `event["neutral"]` (raw, as the gates see it).
    pub neutral: &'a str,
    /// `event["session"]["id"]`.
    pub session_id: &'a str,
    /// `event["ctx"]["abs_path"]`, `""` when absent.
    pub abs_path: &'a str,
}

/// `harness.is_critical_template_event`.
pub fn is_critical_template_event(ev: &EventView) -> bool {
    let tag = ev.tag.to_lowercase();
    if tag.is_empty() {
        return false;
    }
    tag == "tool_question"
        || tag == "tool_post_needs_you"
        || tag.contains("failure")
        || tag.contains("failed")
}

/// `harness.is_focus_template_event`.
pub fn is_focus_template_event(ev: &EventView) -> bool {
    matches!(
        ev.tag.to_lowercase().as_str(),
        "tool_question" | "tool_post_needs_you"
    )
}

const WAKE_TAGS: &[&str] = &[
    "tool_bash_test",
    "tool_bash_build",
    "tool_bash_install",
    "tool_bash_push",
    "tool_bash_sync",
    "tool_agent",
];

/// `harness.should_use_fast_path`.
pub fn should_use_fast_path(
    ev: &EventView,
    multi_agent_active: bool,
    recent_edit_paths: &[String],
) -> bool {
    if is_critical_template_event(ev) {
        return true;
    }
    if ev.kind == "intermediate" || multi_agent_active {
        return false;
    }
    if WAKE_TAGS.contains(&ev.tag) || ev.kind == "final" {
        return false;
    }
    if matches!(ev.tag, "tool_edit" | "tool_write" | "tool_notebook_edit")
        && !ev.abs_path.is_empty()
        && recent_edit_paths.iter().any(|p| p == ev.abs_path)
    {
        return false;
    }
    ev.kind == "tool_pre" || ev.kind == "tool_post"
}

const DECISION_PHRASES: &[&str] = &[
    "approval",
    "approve",
    "can i",
    "choose",
    "confirm",
    "decide",
    "do you want me to",
    "how should",
    "may i",
    "pick",
    "should i",
    "want me to",
    "what should i",
];

const ROUTINE_PHRASES: &[&str] = &[
    "anything else",
    "call those done",
    "call this done",
    "can i help",
    "dig into anything else",
    "move on",
    "ready for whatever comes next",
    "shall we wrap",
    "want to call",
    "want to tweak",
    "what do you want to do next",
    "what would you like to do next",
    "what's next",
];

const PERMISSION_PREFIXES: &[&str] = &[
    "Sir, need your attention.",
    "Sir, I need permission:",
    "Sir, permission needed:",
    "Sir, when you have a second,",
    "Sir, I need your approval:",
];

const DECISION_PREFIXES: &[&str] = &[
    "Sir, need your attention.",
    "Sir, I need your call:",
    "Sir, when you have a second,",
    "Sir, this is waiting on you:",
    "Sir, I need you to decide:",
];

/// Python's `str.isspace` for one character (close enough: Unicode White_Space
/// plus the ASCII separators `split()` treats as whitespace).
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\x1c'..='\x1f')
}

/// `" ".join(text.split())`.
fn collapse_ws(text: &str) -> String {
    text.split(is_py_space)
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `harness._clean_focus_prompt`.
fn clean_focus_prompt(text: &str) -> String {
    const MAX: usize = 220;
    let collapsed = collapse_ws(text);
    let t = collapsed.trim_matches([' ', '-', ':']);
    if t.chars().count() <= MAX {
        return t.to_string();
    }
    let head: String = t.chars().take(MAX).collect();
    let cut = match head.rsplit_once(' ') {
        Some((left, _)) => left.to_string(),
        None => head,
    };
    cut.trim_end_matches([' ', ',', ';', ':']).to_string()
}

/// `re.split(r"(?<=[.!])\s+", q)[-1]`.
fn after_last_sentence_break(q: &str) -> &str {
    let mut last_end = 0;
    let mut prev: Option<char> = None;
    let mut iter = q.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        if is_py_space(c) && matches!(prev, Some('.' | '!')) {
            let mut end = i + c.len_utf8();
            while let Some(&(j, d)) = iter.peek() {
                if is_py_space(d) {
                    end = j + d.len_utf8();
                    iter.next();
                } else {
                    break;
                }
            }
            last_end = end;
            prev = None;
            continue;
        }
        prev = Some(c);
    }
    &q[last_end..]
}

/// `harness.focus_prompt_text` — the exact user-action prompt Focus speaks,
/// `""` when the event is not something the user must answer now.
pub fn focus_prompt_text(ev: &EventView) -> String {
    let text = collapse_ws(ev.neutral);
    if text.is_empty() {
        return String::new();
    }
    if is_focus_template_event(ev) {
        return clean_focus_prompt(&text);
    }
    let mut questions = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if c == '?' {
            questions.push(&text[start..=i]);
            start = i + 1;
        }
    }
    for q in questions.iter().rev() {
        let q = after_last_sentence_break(q.trim_matches(is_py_space));
        let lower = q.to_lowercase();
        if ROUTINE_PHRASES.iter().any(|p| lower.contains(p)) {
            continue;
        }
        if DECISION_PHRASES.iter().any(|p| lower.contains(p)) {
            return clean_focus_prompt(q);
        }
    }
    String::new()
}

/// `harness.focus_prompt_speech`.
pub fn focus_prompt_speech(ev: &EventView, persona_name: &str) -> String {
    let prompt = focus_prompt_text(ev);
    if prompt.is_empty() {
        return String::new();
    }
    let lower = prompt.to_lowercase();
    let permission = ["allow", "approve", "approval", "access", "permission"]
        .iter()
        .any(|p| lower.contains(p));
    let prefixes = if permission {
        PERMISSION_PREFIXES
    } else {
        DECISION_PREFIXES
    };
    let seed = format!("{prompt}|{}|{persona_name}", ev.session_id);
    let idx = crc32(seed.as_bytes()) as usize % prefixes.len();
    format!("{} {prompt}", prefixes[idx]).trim().to_string()
}

/// `harness.is_focus_attention_event`.
pub fn is_focus_attention_event(ev: &EventView) -> bool {
    if is_focus_template_event(ev) {
        return true;
    }
    let kind = ev.kind.to_lowercase();
    if kind != "final" && kind != "intermediate" {
        return false;
    }
    !focus_prompt_text(ev).is_empty()
}

/// `zlib.crc32` (IEEE, reflected).
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

// ---------------------------------------------------------------------------
// narration_policy.py — the table.

/// A lazily-computed, possibly side-effecting feature.
pub type Thunk<'a, T> = Box<dyn Fn() -> T + 'a>;

/// `narration_policy.Features`.
pub struct Features<'a> {
    /// `kind`.
    pub kind: &'a str,
    /// `tag`.
    pub tag: &'a str,
    /// `neutral` is non-empty.
    pub has_text: bool,
    /// `copilot` | `companion` | `focus`.
    pub mode: &'a str,
    /// Settings → "Speak up on" toggles.
    pub speakup_allowed: bool,
    /// `cfg["onboarded"]`.
    pub onboarded: bool,
    /// `cfg["narrate_routine"]`.
    pub narrate_routine: bool,
    /// Failure / question / needs-you.
    pub critical: bool,
    /// A session's FIRST use of a skill.
    pub first_skill: bool,
    /// `harness.is_focus_attention_event`.
    pub focus_attention: bool,
    /// `harness.is_focus_template_event`.
    pub focus_template: bool,
    /// A Focus alert line exists for it.
    pub focus_alert_text: bool,
    /// `harness.is_enabled` (always true in Python).
    pub harness_enabled: bool,
    /// `harness.should_use_fast_path`.
    pub fast_path: bool,
    /// The voice prompt-watcher already owns this question.
    pub prompt_watch_owns: Thunk<'a, bool>,
    /// `verbosity.classify_pre` (records tool density).
    pub verbosity_pre: Thunk<'a, Verbosity>,
    /// `verbosity.classify_post`.
    pub verbosity_post: Thunk<'a, Verbosity>,
    /// `verbosity.classify_prose`.
    pub verbosity_prose: Thunk<'a, Verbosity>,
    /// `_is_duplicate_tool_line` (records the line).
    pub duplicate_tool_line: Thunk<'a, bool>,
    /// The routine words-per-minute budget is spent.
    pub budget_exhausted: Thunk<'a, bool>,
}

/// `Decision.outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyOutcome {
    /// Say nothing.
    Drop,
    /// Fold into the session's burst digest.
    Digest,
    /// A `prompt_intent`: arm the turn opener, record the "you" turn.
    RetirePrompt,
    /// Focus: the prompt-shaped alert line.
    SpeakFocusAlert,
    /// The deterministic template line (fast path).
    SpeakTemplate,
    /// Hand to the narration brain.
    Model,
}

impl PolicyOutcome {
    /// The Python outcome string.
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyOutcome::Drop => "drop",
            PolicyOutcome::Digest => "digest",
            PolicyOutcome::RetirePrompt => "retire_prompt",
            PolicyOutcome::SpeakFocusAlert => "speak_focus_alert",
            PolicyOutcome::SpeakTemplate => "speak_template",
            PolicyOutcome::Model => "model",
        }
    }
}

/// The five side-effecting features, supplied by the caller.
pub struct Thunks<'a> {
    /// See [`Features::prompt_watch_owns`].
    pub prompt_watch_owns: Thunk<'a, bool>,
    /// See [`Features::verbosity_pre`].
    pub verbosity_pre: Thunk<'a, Verbosity>,
    /// See [`Features::verbosity_post`].
    pub verbosity_post: Thunk<'a, Verbosity>,
    /// See [`Features::verbosity_prose`].
    pub verbosity_prose: Thunk<'a, Verbosity>,
    /// See [`Features::duplicate_tool_line`].
    pub duplicate_tool_line: Thunk<'a, bool>,
    /// See [`Features::budget_exhausted`].
    pub budget_exhausted: Thunk<'a, bool>,
}

/// The daemon-held state the pure features read.
#[derive(Debug, Clone, Copy)]
pub struct FeatureState<'a> {
    /// `persona.name` (the Focus alert seed).
    pub persona_name: &'a str,
    /// `tag == "tool_skill"` and this (session, skill) never spoke.
    pub first_skill: bool,
    /// `len(router.list_active()) > 1`.
    pub multi_agent_active: bool,
    /// `tuple(self._recent_edit_paths)`.
    pub recent_edit_paths: &'a [String],
    /// `harness.is_enabled(cfg)`.
    pub harness_enabled: bool,
}

impl<'a> Features<'a> {
    /// `narration_policy.Features(...)` exactly as `_handle_event` builds it.
    pub fn build(
        view: &EventView<'a>,
        cfg: &Map<String, Value>,
        state: FeatureState<'_>,
        thunks: Thunks<'a>,
    ) -> Features<'a> {
        let neutral = view.neutral.trim();
        let mode = narration_mode(cfg);
        Features {
            kind: view.kind,
            tag: view.tag,
            has_text: !neutral.is_empty(),
            mode,
            speakup_allowed: speakup_allows(cfg, view.kind, view.tag, neutral),
            onboarded: cfg_truthy(cfg, "onboarded", false),
            narrate_routine: cfg_truthy(cfg, "narrate_routine", false),
            critical: is_critical_template_event(view),
            first_skill: state.first_skill,
            focus_attention: is_focus_attention_event(view),
            focus_template: is_focus_template_event(view),
            focus_alert_text: mode == "focus"
                && !focus_prompt_speech(view, state.persona_name).is_empty(),
            harness_enabled: state.harness_enabled,
            fast_path: should_use_fast_path(
                view,
                state.multi_agent_active,
                state.recent_edit_paths,
            ),
            prompt_watch_owns: thunks.prompt_watch_owns,
            verbosity_pre: thunks.verbosity_pre,
            verbosity_post: thunks.verbosity_post,
            verbosity_prose: thunks.verbosity_prose,
            duplicate_tool_line: thunks.duplicate_tool_line,
            budget_exhausted: thunks.budget_exhausted,
        }
    }
}

/// `narration_policy.Decision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyDecision {
    /// What to do.
    pub outcome: PolicyOutcome,
    /// Which rule fired — the log reason.
    pub rule: &'static str,
}

fn drop_(rule: &'static str) -> PolicyDecision {
    PolicyDecision {
        outcome: PolicyOutcome::Drop,
        rule,
    }
}

fn out(outcome: PolicyOutcome, rule: &'static str) -> PolicyDecision {
    PolicyDecision { outcome, rule }
}

/// `narration_policy.decide` — top to bottom; the first matching rule wins.
pub fn decide(f: &Features) -> PolicyDecision {
    let tool = f.kind == "tool_pre" || f.kind == "tool_post";
    let focus = f.mode == "focus";

    // ---- 1. The user's own choices, and installation state.
    if !f.speakup_allowed {
        return drop_("speakup_off");
    }
    if !f.onboarded {
        return drop_("not_onboarded");
    }
    if f.kind == "prompt_intent" {
        return out(PolicyOutcome::RetirePrompt, "prompt_intent_retired");
    }
    if f.tag.to_lowercase() == "tool_question" && (f.prompt_watch_owns)() {
        return drop_("prompt_watch_owns_question");
    }

    // ---- 2. MUST SPEAK.
    if f.critical || (f.first_skill && !focus) {
        if !f.has_text {
            return drop_("fastpath_empty_neutral");
        }
        if tool && (f.duplicate_tool_line)() {
            return drop_("fastpath_dup_tool_line");
        }
        return out(
            PolicyOutcome::SpeakTemplate,
            if f.critical {
                "critical_always_speaks"
            } else {
                "first_skill_use_speaks"
            },
        );
    }

    // ---- 3. May drop.
    if (tool || f.kind == "intermediate") && (f.budget_exhausted)() {
        return drop_("volume_budget");
    }
    if tool && !f.narrate_routine {
        return drop_("routine_narration_off");
    }
    if f.kind == "intermediate" && f.mode == "copilot" && !f.narrate_routine {
        return drop_("copilot_prose_off");
    }
    if focus && !f.focus_attention {
        return drop_("focus_attention_drop");
    }
    if focus && !f.focus_template {
        return if f.focus_alert_text {
            out(PolicyOutcome::SpeakFocusAlert, "focus_alert")
        } else {
            drop_("focus_no_prompt")
        };
    }

    // ---- 4. Routing.
    if !f.harness_enabled || !f.fast_path {
        return out(PolicyOutcome::Model, "harness");
    }
    if focus && !f.focus_template {
        return drop_("focus_fastpath_drop");
    }
    if f.mode == "copilot" && tool && !f.narrate_routine {
        return drop_("copilot_tool_tier_suppressed");
    }
    match f.kind {
        "tool_pre" => match (f.verbosity_pre)() {
            Verbosity::Drop => return drop_("fastpath_verbosity_drop"),
            Verbosity::Digest => return out(PolicyOutcome::Digest, "fastpath_verbosity_digest"),
            Verbosity::Speak => {}
        },
        // Guards run only for their own kind: the thunks stay lazy.
        "tool_post" if (f.verbosity_post)() != Verbosity::Speak => {
            return drop_("fastpath_verbosity_drop")
        }
        "intermediate" if (f.verbosity_prose)() != Verbosity::Speak => {
            return drop_("fastpath_verbosity_drop")
        }
        _ => {}
    }
    if !f.has_text {
        return drop_("fastpath_empty_neutral");
    }
    if tool && (f.duplicate_tool_line)() {
        return drop_("fastpath_dup_tool_line");
    }
    out(PolicyOutcome::SpeakTemplate, "fastpath")
}

/// `narration_policy.RULES` — every rule name, in evaluation order.
pub const RULES: &[&str] = &[
    "speakup_off",
    "not_onboarded",
    "prompt_intent_retired",
    "prompt_watch_owns_question",
    "critical_always_speaks",
    "first_skill_use_speaks",
    "volume_budget",
    "routine_narration_off",
    "copilot_prose_off",
    "focus_attention_drop",
    "focus_alert",
    "focus_no_prompt",
    "harness",
    "focus_fastpath_drop",
    "copilot_tool_tier_suppressed",
    "fastpath_verbosity_drop",
    "fastpath_verbosity_digest",
    "fastpath_empty_neutral",
    "fastpath_dup_tool_line",
    "fastpath",
];

/// `narration_policy.MUST_SPEAK`.
pub const MUST_SPEAK: &[&str] = &["critical_always_speaks", "first_skill_use_speaks"];

/// `register.apply`'s stop test, exposed for the shaping pass: Neutral
/// (`None` / `1` / `1.0` / `True`) is the identity.
pub(crate) fn is_neutral_register(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(v) => py_eq_one(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_zlib() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"hello"), 0x3610_a686);
    }

    #[test]
    fn py_int_follows_python() {
        assert_eq!(py_int(&Value::from(" 2 ")), Some(2));
        assert_eq!(py_int(&Value::from("1_0")), Some(10));
        assert_eq!(py_int(&Value::from("2.0")), None);
        assert_eq!(py_int(&Value::from(2.9)), Some(2));
        assert_eq!(py_int(&Value::Bool(true)), Some(1));
        assert_eq!(py_int(&Value::Null), None);
    }

    #[test]
    fn default_mode_is_copilot() {
        assert_eq!(narration_mode(&Map::new()), "copilot");
    }
}
