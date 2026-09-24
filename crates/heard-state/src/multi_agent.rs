//! Multi-agent routing.
//!
//! When more than one agent session fires events into the daemon concurrently —
//! say three Claude Code instances running in three Ghostty tabs — naive
//! per-event narration becomes incoherent: the listener hears half-sentences
//! from each agent, bouncing.
//!
//! This module classifies each event into one of three actions:
//!
//! ```text
//!   speak           — go through the queue normally
//!   drop            — silent (routine narration from a non-focus agent)
//!   defer_to_digest — accumulate for a periodic summary
//! ```
//!
//! Three modes, picked automatically:
//!
//! ```text
//!   SOLO    — only one session active in the last SESSION_ACTIVE_S.
//!             Everything plays. Today's behaviour.
//!   SWARM   — 2+ active sessions. Most-recently-active gets full
//!             narration; others' routine events drop, but failures and
//!             wait-state questions pierce with an "Agent <name>:"
//!             prefix so the user can hear who.
//!   PINNED  — user explicitly picked one session to follow. Only that
//!             session's events narrate; others drop, except again
//!             failures/questions pierce with prefix.
//! ```
//!
//! The router is a pure state machine. The daemon owns one instance and calls
//! into it from `_handle_event`. Delete this file plus the daemon's ~5-line
//! glue and the rest of the product is unchanged.
//!
//! ## Where the LLM sits
//!
//! Nothing in this module calls a model. The daemon's 1-second tick prefers an
//! LLM narrative for a project flush ("On the API project, edited the auth flow
//! across three files; tests passed") and falls back to
//! [`format_project_summary`] when no provider is reachable or every one
//! returned `None`. That seam is [`ProjectSummarizer`], and
//! [`project_flush_text`] is the daemon's ladder with the template fallback
//! ported exactly. [`TemplateOnly`] is the summarizer for a build with no LLM
//! at all — which is also the no-LLM floor the product promises.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::clock::{Clock, SystemClock};
use crate::project_name::{GitProjectNamer, ProjectNamer};
use crate::sha1::sha1_hex;

/// Curated pool of distinguishable voices for auto-assignment to non-focus
/// agents in swarm mode. Mix of male/female + US/British so the listener can
/// tell who's speaking on first syllable. Same repo_name → same voice across
/// runs (deterministic SHA-1 hash), so the user's mental "api is Rachel"
/// mapping survives restarts.
pub const AUTO_VOICE_POOL: [&str; 6] = [
    "21m00Tcm4TlvDq8ikWAM", // Rachel — female US
    "pNInz6obpgDQGcFmaJgB", // Adam — male US
    "XB0fDUnXU5powFXDhCwa", // Charlotte — female English
    "onwK4e9ZLuTAKqWW03F9", // Daniel — male British
    "pFZP5JQG7iQjIQuC4Bku", // Lily — female British
    "pqHfZKP75CvOlQylNhV4", // Bill — male older
];

/// Deterministic per-repo voice from the pool.
///
/// SHA-1 — Python's builtin `hash()` is salted per-process, which would give
/// the same repo a different voice every time the daemon restarts. Bad.
pub fn auto_voice_for(repo_name: &str) -> &'static str {
    if repo_name.is_empty() {
        return AUTO_VOICE_POOL[0];
    }
    // `int(digest, 16) % 6` over a 160-bit value. Reducing it byte by byte is
    // the same arithmetic without a bignum.
    let digest = sha1_hex(repo_name.as_bytes());
    let mut remainder: u64 = 0;
    for ch in digest.chars() {
        let nibble = ch.to_digit(16).expect("hexdigest is hex") as u64;
        remainder = (remainder * 16 + nibble) % AUTO_VOICE_POOL.len() as u64;
    }
    AUTO_VOICE_POOL[remainder as usize]
}

/// How long after the last event a session counts as "active". Used both for
/// the SOLO/SWARM mode decision and for the menu's active-sessions list. 30 s
/// is roughly "I just ran a thing, the agent's still cooking".
pub const SESSION_ACTIVE_S: f64 = 30.0;

/// Per-project channel scheduler (SWARM behaviour). When ≥2 sessions are active
/// concurrently, routine narration is no longer "whichever session fired last
/// speaks live, the rest defer". Instead every non-pierce event lands in its
/// session's pending pile, and the daemon's 1 s scheduler drains each *project*
/// (grouped by repo_name) as one narrative summary when the project's been
/// quiet for [`CHANNEL_IDLE_FLUSH_S`] (natural turn boundary) or its total
/// pending count hits [`CHANNEL_MAX_PENDING`] (backpressure cap so a busy agent
/// doesn't hold its update hostage). Same-project agents collapse into one
/// summary stream; different-project agents drain as their own streams in
/// distinct voices.
pub const CHANNEL_IDLE_FLUSH_S: f64 = 2.0;
/// See [`CHANNEL_IDLE_FLUSH_S`].
pub const CHANNEL_MAX_PENDING: usize = 5;

/// Show a session in `list_active()` for this long after its last event, even
/// if it's no longer "active" for mode purposes. Lets the user pin a session
/// that just went idle for a moment.
pub const SESSION_VISIBLE_S: f64 = 600.0;

/// Files / directories whose presence in a folder marks it as a "real project"
/// (vs. an arbitrary working directory like `~/` or `~/Downloads`). The
/// session-to-project inference walks up from each edited file path looking for
/// any of these; the directory containing the first hit is treated as the
/// session's project root.
pub const PROJECT_MARKERS: [&str; 12] = [
    ".git",
    "pyproject.toml",
    "package.json",
    "Cargo.toml",
    "go.mod",
    "build.gradle",
    "build.gradle.kts",
    "pom.xml",
    "Gemfile",
    "composer.json",
    "Makefile",
    ".heard.yaml",
];

const PROJECT_ROOT_CACHE_MAX: usize = 512;

/// Tags that always pierce regardless of mode/focus — the "name across the
/// room" signal. Failures and wait-state questions are events the user must
/// hear even from background agents. `prompt_intent` joins the list because
/// batching "the user just told agent X to do Y" into a project flush that
/// fires 2 seconds later is useless — by then the agent has already started
/// replying.
pub const PIERCE_TAGS: [&str; 4] = [
    "tool_post_failure",
    "tool_post_command_failed",
    "tool_question",
    "prompt_intent",
];

/// Map of common event tags to short verbs for the digest summary. Anything not
/// listed groups under "operation" — generic but better than dropping the
/// count.
const TAG_TO_VERB: [(&str, &str); 29] = [
    ("tool_edit", "edit"),
    ("tool_write", "write"),
    ("tool_bash_test", "test run"),
    ("tool_bash_build", "build"),
    ("tool_bash_install", "install"),
    ("tool_bash_commit", "commit"),
    ("tool_bash_push", "push"),
    ("tool_bash_sync", "git sync"),
    ("tool_bash_grep_cmd", "search"),
    ("tool_grep", "search"),
    ("tool_bash_find", "search"),
    ("tool_glob", "search"),
    ("tool_bash_read", "read"),
    ("tool_bash_remove", "removal"),
    ("tool_bash_copy", "copy"),
    ("tool_bash_move", "move"),
    ("tool_bash_curl", "fetch"),
    ("tool_bash_git_inspect", "git check"),
    ("tool_skill", "skill"),
    ("tool_task_create", "task"),
    ("tool_send_message", "message"),
    ("tool_agent", "delegation"),
    ("tool_webfetch", "fetch"),
    ("tool_websearch", "web search"),
    ("intermediate_short", "comment"),
    ("intermediate_long", "comment"),
    ("final_short", "wrap-up"),
    ("final_long", "wrap-up"),
    ("tool_bash_grep", "operation"),
];

fn verb_for(tag: &str) -> &'static str {
    TAG_TO_VERB
        .iter()
        .find(|(k, _)| *k == tag)
        .map(|(_, v)| *v)
        .unwrap_or("operation")
}

/// The router's three modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Solo,
    Swarm,
    Pinned,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Solo => "solo",
            Mode::Swarm => "swarm",
            Mode::Pinned => "pinned",
        }
    }
}

/// One pass of project attribution: a derived name plus a confidence tier.
/// Higher confidence values overwrite lower ones in [`MultiAgentRouter::note_event`];
/// ties leave the existing value untouched. Tiers:
///
/// * 2 — found a project marker (.git, package.json, …) while walking up from
///   either the file path or the cwd. Solid signal.
/// * 1 — cwd basename was usable (cwd was provided and isn't a stop dir like
///   `~/` or `/`). Weak fallback for sessions running outside any recognised
///   project — keeps current behaviour for "random non-project folder" without
///   re-triggering the home-folder-as-project bug.
/// * 0 — nothing usable. Session sits in its own bucket keyed by session id and
///   won't trigger fake SWARM mode by sharing a generic name with another
///   stop-dir session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoInference {
    name: String,
    confidence: u8,
}

/// One tracked session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    pub repo_name: String,
    pub repo_confidence: u8,
    pub last_event: f64,
    /// Monotonic counter, bumped on every `note_event`. Used to break ties when
    /// two sessions share a `last_event` timestamp (back-to-back events on a
    /// fast machine) so "most recent" is deterministic.
    pub event_seq: u64,
    pub pending_digest: Vec<DigestEvent>,
}

/// One stashed event awaiting a digest / project flush.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DigestEvent {
    pub kind: String,
    pub tag: String,
    pub neutral: String,
    #[serde(default)]
    pub ctx: serde_json::Value,
    pub ts: f64,
}

/// Tri-state routing outcome.
///
/// `action` is the only required field. `label_prefix` is set on pierces from
/// non-focus sessions ("Agent api: tests failed"); the daemon prepends it to
/// the rewritten text. `voice_override` is set by the per-agent voice map;
/// callers that don't know about it ignore the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingDecision {
    pub action: Action,
    pub label_prefix: String,
    pub voice_override: Option<String>,
}

/// What the daemon should do with one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Speak,
    Drop,
    DeferToDigest,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Speak => "speak",
            Action::Drop => "drop",
            Action::DeferToDigest => "defer_to_digest",
        }
    }
}

impl RoutingDecision {
    fn new(action: Action) -> Self {
        Self {
            action,
            label_prefix: String::new(),
            voice_override: None,
        }
    }
}

/// One project's worth of pending events, ready to drain as a single attributed
/// summary utterance. Channels are by project (repo_name), not session —
/// same-project agents collapse into one summary stream so the listener gets
/// project-level insight rather than alternating per-agent blurbs.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectFlush {
    /// repo_name, or session_id fallback.
    pub project_key: String,
    /// Spoken-friendly name ("api").
    pub label: String,
    /// Union of pending across the project's sessions, ts-ordered.
    pub events: Vec<DigestEvent>,
    /// Which sessions contributed.
    pub member_session_ids: Vec<String>,
    /// Most-recently-active session in the project.
    pub speaker_session_id: String,
    /// Auto-pool voice keyed by repo_name; `None` = persona.
    pub voice_override: Option<String>,
    /// This is the most-recently-active project globally.
    pub is_primary: bool,
}

/// `str.capitalize()`: first character upper, everything after it lower.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
    }
}

/// `f"{body[:1].upper()}{body[1:]}"` — only the first character changes.
fn upper_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().chain(chars).collect(),
    }
}

fn count_word(n: usize) -> String {
    match n {
        2 => "two".to_string(),
        3 => "three".to_string(),
        4 => "four".to_string(),
        5 => "five".to_string(),
        other => other.to_string(),
    }
}

/// "a" or "an" for the single-count digest parts.
///
/// `format!("a {verb}")` gave "A edit." / "A operation." — which the user
/// HEARS, every burst, in `drain_session_summary(include_label = false)`.
///
/// The rule is initial vowel SOUND, not vowel letter, kept deliberately small
/// because the verb vocabulary is a closed table (`TAG_TO_VERB`) plus the
/// "operation" fallback: vowel letter -> "an", except the handful of prefixes
/// spelled with a vowel and pronounced with a consonant ("a user", "a unique
/// fix", "a one-off"). Mirrors `_article_for` in `heard/multi_agent.py`.
const CONSONANT_SOUND_PREFIXES: [&str; 5] = ["use", "uni", "uti", "eu", "one"];

fn article_for(word: &str) -> &'static str {
    let lowered = word.trim_start().to_lowercase();
    if CONSONANT_SOUND_PREFIXES
        .iter()
        .any(|prefix| lowered.starts_with(prefix))
    {
        return "a";
    }
    match lowered.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "an",
        _ => "a",
    }
}

/// The verb tally both summary formatters share: "2 edits, a test run".
///
/// Sorted by descending count, then verb name — so the biggest thing the agent
/// did leads, and equal counts read in a stable order rather than whichever way
/// a hash map happened to fall.
fn verb_parts(events: &[DigestEvent]) -> Vec<String> {
    let mut by_verb: Vec<(&str, usize)> = Vec::new();
    for event in events {
        let verb = verb_for(&event.tag);
        match by_verb.iter_mut().find(|(v, _)| *v == verb) {
            Some(entry) => entry.1 += 1,
            None => by_verb.push((verb, 1)),
        }
    }
    by_verb.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    by_verb
        .into_iter()
        .map(|(verb, count)| {
            if count == 1 {
                format!("{} {verb}", article_for(verb))
            } else {
                format!("{count} {verb}s")
            }
        })
        .collect()
}

/// Per-session line for the digest. "Api: 5 edits, ran the tests." Returns
/// `None` if no events count toward the summary.
///
/// `include_label` prefixes the repo label ("Api: …") — needed in swarm to
/// disambiguate which agent the summary is about, but pure noise in a solo
/// session where there's only one agent. Solo callers pass `false` so the
/// listener hears "3 reads, a search." not "Heard: 3 reads, a search." every
/// burst.
pub fn format_session_summary(
    info: &SessionInfo,
    events: &[DigestEvent],
    include_label: bool,
) -> Option<String> {
    let parts = verb_parts(events);
    if parts.is_empty() {
        return None;
    }
    let body = parts.join(", ");
    if !include_label {
        return Some(format!("{}.", upper_first(&body)));
    }
    Some(format!("{}: {}.", capitalize(&label_for(info)), body))
}

/// Aggregated tag-count summary for a project's drain — pools events from every
/// session in the project so multiple agents working in `~/api` produce one
/// line instead of N indistinguishable "Api: …" blurbs. Robotic but
/// informative; a narrative form can be layered on top by an optional LLM
/// narrator (see [`project_flush_text`]).
///
/// Output shape: `"Api: 3 edits, a search, ran a test."` Bumps to `"Api: 3
/// edits, a search across two agents."` when ≥2 member sessions contributed, so
/// the listener knows the events are pooled.
pub fn format_project_summary(
    label: &str,
    events: &[DigestEvent],
    member_count: usize,
    include_label: bool,
) -> Option<String> {
    if events.is_empty() {
        return None;
    }
    let parts = verb_parts(events);
    if parts.is_empty() {
        return None;
    }
    let tail = if member_count >= 2 {
        format!(" across {} agents", count_word(member_count))
    } else {
        String::new()
    };
    let body = parts.join(", ");
    if !include_label {
        return Some(format!("{}{}.", upper_first(&body), tail));
    }
    Some(format!("{}: {}{}.", capitalize(label), body, tail))
}

/// Spoken-friendly agent label. Falls back to a short session_id chunk if cwd /
/// repo_name aren't available — better than no label.
fn label_for(info: &SessionInfo) -> String {
    if !info.repo_name.is_empty() {
        return info.repo_name.clone();
    }
    if info.session_id.is_empty() {
        "agent".to_string()
    } else {
        info.session_id.chars().take(8).collect()
    }
}

// ---------------------------------------------------------------------------
// project-root inference
// ---------------------------------------------------------------------------

/// Walks the filesystem looking for project markers, with the same bounded
/// cache the Python keeps.
///
/// Cache of resolved project roots, keyed by the directory we started the walk
/// from. Walking up the filesystem hits `exists` once per candidate; cheap, but
/// a session firing dozens of events from the same directory shouldn't repeat
/// the same walk forever. Bounded so a fork-bomb of distinct dirs can't grow
/// the cache without limit.
#[derive(Debug, Default)]
pub struct ProjectRootFinder {
    cache: Mutex<HashMap<String, Option<String>>>,
}

impl ProjectRootFinder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Directories where the project-root walk should stop without matching.
    ///
    /// We don't want to claim `~/` or `/` as a project even if some user
    /// dropped a .git in their home dir — that'd attribute every session to
    /// "home" and defeat the whole point.
    ///
    /// Python resolves this once per process; here it is read per call, because
    /// `HOME` is exactly what the tests move and a cached value would make the
    /// home-dir rule untestable.
    fn stop_dirs() -> Vec<String> {
        let mut stops = vec!["/".to_string()];
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                stops.push(home.clone());
                // Realpath defends against symlinked homes (some corp setups
                // put ~/ on a network drive symlinked into /Users/<name>).
                if let Ok(real) = std::fs::canonicalize(&home) {
                    if let Some(real) = real.to_str() {
                        stops.push(real.to_string());
                    }
                }
            }
        }
        stops
    }

    /// Walk up from `path` looking for a folder that contains any of the
    /// project-marker files. Returns the marker-containing folder's absolute
    /// path, or `None` if we walk all the way to the user's home directory (or
    /// root) without finding one.
    ///
    /// A `path` that points to a file uses its parent directory as the walk
    /// start. A `path` that's already a directory is the start itself. Empty /
    /// nonexistent paths return `None`.
    ///
    /// The walk stops at the user's home dir on purpose. We don't want a stray
    /// `~/.git` to make every session look like one big project, and we don't
    /// want random subdirs of `~` (Downloads, Desktop, …) inheriting a parent's
    /// marker. Hitting home = "no real project".
    pub fn find(&self, path: &str) -> Option<String> {
        if path.is_empty() {
            return None;
        }
        {
            let cache = self.cache.lock().expect("project root cache poisoned");
            if let Some(hit) = cache.get(path) {
                return hit.clone();
            }
        }

        let absolute = abspath(path);
        let mut current = if Path::new(&absolute).is_dir() {
            absolute
        } else {
            Path::new(&absolute)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .to_string()
        };

        let stops = Self::stop_dirs();
        let mut seen: Vec<String> = Vec::new();
        let mut result: Option<String> = None;
        while !current.is_empty() && !seen.contains(&current) && !stops.contains(&current) {
            seen.push(current.clone());
            for marker in PROJECT_MARKERS {
                if Path::new(&current).join(marker).exists() {
                    result = Some(current.clone());
                    break;
                }
            }
            if result.is_some() {
                break;
            }
            let parent = Path::new(&current)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("")
                .to_string();
            if parent == current {
                break;
            }
            current = parent;
        }

        let mut cache = self.cache.lock().expect("project root cache poisoned");
        if cache.len() >= PROJECT_ROOT_CACHE_MAX {
            cache.clear();
        }
        cache.insert(path.to_string(), result.clone());
        result
    }

    /// Drop every cached project-root lookup. Test-only — production code never
    /// invalidates because filesystems rarely lose a `.git` mid-session, and on
    /// the rare cases they do, a daemon restart is cheap.
    pub fn clear_cache(&self) {
        self.cache
            .lock()
            .expect("project root cache poisoned")
            .clear();
    }
}

/// `os.path.abspath`: make absolute against the process cwd, then collapse
/// `.` and `..` lexically without touching the filesystem.
fn abspath(path: &str) -> String {
    let raw = Path::new(path);
    let joined: PathBuf = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(raw)
    };
    let mut out = PathBuf::from("/");
    for component in joined.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out.to_str().unwrap_or("/").to_string()
}

// ---------------------------------------------------------------------------
// the LLM seam
// ---------------------------------------------------------------------------

/// The narrative summarizer the daemon injects.
///
/// `daemon.py`'s 1-second tick calls `persona.summarize_project(...)` and takes
/// the deterministic template only when that returns nothing. Keeping it a
/// trait keeps this whole module free of the LLM and keeps the fallback
/// testable without one.
pub trait ProjectSummarizer {
    /// `None` means "no narrative available" — the caller falls back to
    /// [`format_project_summary`].
    fn summarize_project(
        &self,
        label: &str,
        events: &[DigestEvent],
        member_count: usize,
        solo: bool,
    ) -> Option<String>;
}

/// No LLM at all. Every flush takes the template path.
#[derive(Debug, Default, Clone, Copy)]
pub struct TemplateOnly;

impl ProjectSummarizer for TemplateOnly {
    fn summarize_project(
        &self,
        _label: &str,
        _events: &[DigestEvent],
        _member_count: usize,
        _solo: bool,
    ) -> Option<String> {
        None
    }
}

/// The daemon's ladder for one project flush: prefer the LLM narrative, fall
/// back to the deterministic tag-count formatter, and say nothing if neither
/// produced text.
///
/// `solo` is the daemon's `solo_fleet and len(pf.member_session_ids) <= 1` —
/// when the whole fleet is one agent on one project the summary skips the repo
/// label ("Read through the auth flow, tests passed", not "Heard: …").
///
/// The daemon also runs the LLM's answer through `text_shape.shape_spoken_text`
/// before speaking it. That lives in `heard-narrate`, so the caller
/// applies it to this function's output rather than this function reaching
/// across the crate boundary.
pub fn project_flush_text(
    summarizer: &dyn ProjectSummarizer,
    flush: &ProjectFlush,
    solo: bool,
) -> Option<String> {
    let member_count = flush.member_session_ids.len();
    let narrative = summarizer.summarize_project(&flush.label, &flush.events, member_count, solo);
    if let Some(text) = narrative {
        if !text.is_empty() {
            return Some(text);
        }
    }
    format_project_summary(&flush.label, &flush.events, member_count, !solo)
}

// ---------------------------------------------------------------------------
// the router
// ---------------------------------------------------------------------------

struct RouterState {
    /// Insertion-ordered, because the order matters and a hash map has none to
    /// give. Python's dict preserves insertion order and three places lean on
    /// it: the primary-project scan (`>` keeps the FIRST maximum), the
    /// project grouping that feeds a stable sort, and `collect_digest`.
    sessions: Vec<SessionInfo>,
    pinned: Option<String>,
    /// Monotonic; assigned to `SessionInfo::event_seq`.
    event_counter: u64,
    /// Last session whose narration we let through. Used so the "Agent <name>:"
    /// prefix (one-voice mode) is spoken only when the speaker *changes* —
    /// narrating ten lines in a row from the agent you're driving shouldn't
    /// read its name ten times.
    last_narrated_session: Option<String>,
}

impl RouterState {
    fn get(&self, session_id: &str) -> Option<&SessionInfo> {
        self.sessions.iter().find(|s| s.session_id == session_id)
    }

    fn get_mut(&mut self, session_id: &str) -> Option<&mut SessionInfo> {
        self.sessions
            .iter_mut()
            .find(|s| s.session_id == session_id)
    }

    fn active_count(&self, now: f64) -> usize {
        let cutoff = now - SESSION_ACTIVE_S;
        self.sessions
            .iter()
            .filter(|s| s.last_event >= cutoff)
            .count()
    }
}

/// Solo / Swarm / Pinned routing plus the project-keyed channel scheduler.
pub struct MultiAgentRouter {
    state: Mutex<RouterState>,
    clock: Arc<dyn Clock>,
    namer: Arc<dyn ProjectNamer>,
    roots: ProjectRootFinder,
}

impl MultiAgentRouter {
    pub fn new() -> Self {
        Self::with(
            Arc::new(SystemClock::new()),
            Arc::new(GitProjectNamer::new()),
        )
    }

    pub fn with(clock: Arc<dyn Clock>, namer: Arc<dyn ProjectNamer>) -> Self {
        Self {
            state: Mutex::new(RouterState {
                sessions: Vec::new(),
                pinned: None,
                event_counter: 0,
                last_narrated_session: None,
            }),
            clock,
            namer,
            roots: ProjectRootFinder::new(),
        }
    }

    /// The shared project-root cache, for tests that need to invalidate it.
    pub fn roots(&self) -> &ProjectRootFinder {
        &self.roots
    }

    // --- session tracking --------------------------------------------------

    /// Record that `session_id` just fired an event.
    ///
    /// Project attribution (sets `repo_name`, which drives voice routing and
    /// SOLO/SWARM grouping):
    ///
    /// 1. `path_hint` (an edited / read file path) walks up to a project
    ///    marker → that folder's basename is the project. High-confidence,
    ///    file-driven inference.
    /// 2. Else if `cwd` itself contains a project marker → cwd basename is the
    ///    project. Medium-confidence fallback for sessions whose first event
    ///    has no path (Stop hooks, generic bash commands).
    /// 3. Else `repo_name` stays empty → the project key falls back to the
    ///    session id, so this session sits in its own bucket and doesn't
    ///    trigger fake SWARM mode by sharing a generic name like "christian"
    ///    (the user's home dir).
    ///
    /// Repo-name upgrades: a session created without a strong signal (rule 2 or
    /// 3) can be upgraded to rule 1 on a later event that carries a path_hint.
    /// Once rule 1 has fired, subsequent events don't downgrade it — the
    /// project this session is "really on" doesn't usually change mid-turn.
    pub fn note_event(&self, session_id: &str, cwd: &str, path_hint: Option<&str>) {
        if session_id.is_empty() {
            return;
        }
        let derived = self.infer_repo_name(cwd, path_hint);
        let now = self.clock.wall();
        let mut state = self.state.lock().expect("router poisoned");
        if state.get(session_id).is_none() {
            state.sessions.push(SessionInfo {
                session_id: session_id.to_string(),
                cwd: cwd.to_string(),
                repo_name: derived.name.clone(),
                repo_confidence: derived.confidence,
                last_event: 0.0,
                event_seq: 0,
                pending_digest: Vec::new(),
            });
        } else {
            let info = state.get_mut(session_id).expect("checked above");
            // Only upgrade — never overwrite a stronger inference with a
            // weaker one.
            if derived.confidence > info.repo_confidence {
                info.repo_name = derived.name.clone();
                info.repo_confidence = derived.confidence;
            }
            // Keep cwd up to date if a later event carries one; the first
            // event might not have had it (e.g. a Stop hook without a
            // tool_input).
            if !cwd.is_empty() && info.cwd.is_empty() {
                info.cwd = cwd.to_string();
            }
        }
        state.event_counter += 1;
        let seq = state.event_counter;
        let info = state.get_mut(session_id).expect("present");
        info.last_event = now;
        info.event_seq = seq;
    }

    /// Resolve `(repo_name, confidence)` from the available signals. Higher
    /// confidence wins on conflict; ties keep the existing value. See
    /// [`MultiAgentRouter::note_event`] and [`RepoInference`] for the
    /// precedence rules and the rationale for each tier.
    fn infer_repo_name(&self, cwd: &str, path_hint: Option<&str>) -> RepoInference {
        // Tier 2: file path walks up to a real project root. This is the
        // load-bearing case for "Claude launched from home but editing files
        // inside ~/Desktop/Projects/heard/" — the path-derived inference beats
        // the home-dir cwd.
        if let Some(hint) = path_hint.filter(|h| !h.is_empty()) {
            if let Some(root) = self.roots.find(hint) {
                let name = self.namer.canonical_project_name(&root);
                let name = if name.is_empty() { root } else { name };
                return RepoInference {
                    name,
                    confidence: 2,
                };
            }
        }
        // Tier 2: cwd itself walks up to a real project root.
        if !cwd.is_empty() {
            if let Some(root) = self.roots.find(cwd) {
                let name = self.namer.canonical_project_name(&root);
                let name = if name.is_empty() { root } else { name };
                return RepoInference {
                    name,
                    confidence: 2,
                };
            }
        }
        // Tier 1: cwd basename. Backwards compatible with sessions in
        // non-project folders that still have a meaningful name — but
        // explicitly skip the stop dirs so home (~/) doesn't get attributed as
        // a project called e.g. "christian".
        if !cwd.is_empty() {
            let resolved = abspath(cwd);
            if !resolved.is_empty() && !ProjectRootFinder::stop_dirs().contains(&resolved) {
                let trimmed = cwd.trim_end_matches('/');
                let base = Path::new(trimmed)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                let name = if base.is_empty() {
                    cwd.to_string()
                } else {
                    base.to_string()
                };
                return RepoInference {
                    name,
                    confidence: 1,
                };
            }
        }
        // Tier 0: nothing usable. `project_key` falls back to the session id
        // and this session sits alone.
        RepoInference {
            name: String::new(),
            confidence: 0,
        }
    }

    pub fn mode(&self) -> Mode {
        let now = self.clock.wall();
        let state = self.state.lock().expect("router poisoned");
        if let Some(pinned) = &state.pinned {
            if state.get(pinned).is_some() {
                return Mode::Pinned;
            }
        }
        if state.active_count(now) >= 2 {
            Mode::Swarm
        } else {
            Mode::Solo
        }
    }

    // --- routing -----------------------------------------------------------

    /// Route one event.
    ///
    /// `kind` is accepted and unused, exactly as in the Python: the decision
    /// reads the tag and the session, never the kind. Kept in the signature so
    /// the call site stays honest about what it is handing over.
    pub fn classify(
        &self,
        kind: &str,
        tag: &str,
        session_id: &str,
        agent_voices: Option<&HashMap<String, String>>,
        auto_voices: bool,
    ) -> RoutingDecision {
        let _ = kind;
        let empty = HashMap::new();
        let agent_voices = agent_voices.unwrap_or(&empty);
        let now = self.clock.wall();
        let mut state = self.state.lock().expect("router poisoned");

        // Pinned mode: user has explicitly committed to one session.
        let pinned_active = state.pinned.clone().filter(|p| state.get(p).is_some());
        if let Some(pinned) = pinned_active {
            if session_id == pinned {
                let voice = voice_for(&state, session_id, agent_voices, auto_voices, true);
                let prefix = focus_label_prefix(&state, session_id, agent_voices, auto_voices, now);
                state.last_narrated_session = Some(session_id.to_string());
                return RoutingDecision {
                    action: Action::Speak,
                    label_prefix: prefix,
                    voice_override: voice,
                };
            }
            if PIERCE_TAGS.contains(&tag) {
                let voice = voice_for(&state, session_id, agent_voices, auto_voices, false);
                let decision = pierced(&state, session_id, voice);
                state.last_narrated_session = Some(session_id.to_string());
                return decision;
            }
            return RoutingDecision::new(Action::Drop);
        }

        // Solo: <2 active sessions, today's behaviour, everything plays.
        if state.active_count(now) < 2 {
            let voice = voice_for(&state, session_id, agent_voices, auto_voices, true);
            state.last_narrated_session = Some(session_id.to_string());
            return RoutingDecision {
                action: Action::Speak,
                label_prefix: String::new(),
                voice_override: voice,
            };
        }

        // Swarm: ≥2 active. Per-project channel scheduler — every non-pierce
        // event lands in its session's pending pile; the daemon drains each
        // *project* (grouped by repo_name) on idle / backpressure as one
        // attributed summary. Pierces (failures, questions) still cut through
        // immediately with the agent's name.
        if PIERCE_TAGS.contains(&tag) {
            let voice = voice_for(&state, session_id, agent_voices, auto_voices, false);
            let decision = pierced(&state, session_id, voice);
            state.last_narrated_session = Some(session_id.to_string());
            return decision;
        }
        RoutingDecision::new(Action::DeferToDigest)
    }

    // --- project channel scheduler ----------------------------------------

    /// Atomically pop pending events from every project whose channel is ready
    /// to drain. A channel is ready when its most recent event in any member
    /// session was ≥ [`CHANNEL_IDLE_FLUSH_S`] ago (natural turn boundary) OR its
    /// total pending count is ≥ [`CHANNEL_MAX_PENDING`] (backpressure). Member
    /// sessions' piles are cleared as part of the call so the next event starts
    /// a fresh accumulation. Largest pile first so the worst backlog gets
    /// spoken first when several flush at once.
    ///
    /// Voice routing: the project whose most-recently-active session is the
    /// global newest gets `voice_override=None` (= persona); every other
    /// project gets a deterministic auto-pool voice keyed by its `repo_name`
    /// (so "api" always sounds the same way) iff `auto_voices` is true. With
    /// `auto_voices=false` ("one voice" mode) every project uses the persona
    /// voice — the listener distinguishes them by the project name baked into
    /// the summary text instead.
    pub fn collect_project_flushes(
        &self,
        auto_voices: bool,
        now: Option<f64>,
    ) -> Vec<ProjectFlush> {
        let now_ts = now.unwrap_or_else(|| self.clock.wall());
        let mut state = self.state.lock().expect("router poisoned");
        drain(&mut state, auto_voices, Some(now_ts))
    }

    /// Same shape as [`MultiAgentRouter::collect_project_flushes`] but bypasses
    /// the idle / backpressure gates — every session with pending events
    /// contributes to a flush, regardless of how recent the last event was.
    /// Used by the resume-with-catch-up path so a user who just unmuted gets a
    /// single recap of *everything* that was buffered, not just the channels
    /// the tick happened to consider ready.
    ///
    /// Atomically clears every session's pending_digest as part of the call
    /// (same lock + drain pattern as the tick path).
    pub fn force_flush_all(&self, auto_voices: bool, now: Option<f64>) -> Vec<ProjectFlush> {
        // `now` is accepted and ignored, keeping the signature parallel with
        // `collect_project_flushes` so callers can swap one for the other.
        let _ = now;
        let mut state = self.state.lock().expect("router poisoned");
        drain(&mut state, auto_voices, None)
    }

    /// Update `last_narrated_session` after the daemon speaks a project flush,
    /// so a same-project pierce arriving right after doesn't redundantly
    /// re-announce the agent name.
    pub fn note_flush_spoken(&self, speaker_session_id: &str) {
        let mut state = self.state.lock().expect("router poisoned");
        state.last_narrated_session = Some(speaker_session_id.to_string());
    }

    // --- pin control -------------------------------------------------------

    /// Returns `true` if the session was found and pinned, else `false`.
    pub fn pin(&self, session_id: &str) -> bool {
        let mut state = self.state.lock().expect("router poisoned");
        if state.get(session_id).is_some() {
            state.pinned = Some(session_id.to_string());
            true
        } else {
            false
        }
    }

    pub fn unpin(&self) {
        let mut state = self.state.lock().expect("router poisoned");
        state.pinned = None;
    }

    // --- digest -----------------------------------------------------------

    /// Stash an event for the periodic digest summary. No-op if `session_id` is
    /// unknown.
    pub fn add_to_digest(
        &self,
        session_id: &str,
        kind: &str,
        tag: &str,
        neutral: &str,
        ctx: Option<serde_json::Value>,
    ) {
        let now = self.clock.wall();
        let mut state = self.state.lock().expect("router poisoned");
        let Some(info) = state.get_mut(session_id) else {
            return;
        };
        info.pending_digest.push(DigestEvent {
            kind: kind.to_string(),
            tag: tag.to_string(),
            neutral: neutral.to_string(),
            ctx: ctx.unwrap_or_else(|| serde_json::Value::Object(Default::default())),
            ts: now,
        });
    }

    /// Drain pending digest events. Returns `[(session_info, events)]` for any
    /// session with non-empty pending events.
    pub fn collect_digest(&self) -> Vec<(SessionInfo, Vec<DigestEvent>)> {
        let mut state = self.state.lock().expect("router poisoned");
        let mut out = Vec::new();
        for info in state.sessions.iter_mut() {
            if !info.pending_digest.is_empty() {
                let events = std::mem::take(&mut info.pending_digest);
                out.push((info.clone(), events));
            }
        }
        out
    }

    /// Drain ONE session's pending digest and format it as a spoken summary.
    ///
    /// Used by the daemon when intermediate prose arrives — we play the tool
    /// summary first ("3 edits, ran the tests"), then the prose ("OK, all
    /// green"), so the user gets a coherent narrative instead of a wall of
    /// "editing X.py. editing Y.py..." preceding the prose.
    ///
    /// `include_label` is forwarded to [`format_session_summary`] — solo
    /// callers pass `false` to drop the redundant repo prefix.
    pub fn drain_session_summary(&self, session_id: &str, include_label: bool) -> Option<String> {
        let (info, events) = {
            let mut state = self.state.lock().expect("router poisoned");
            let info = state.get_mut(session_id)?;
            if info.pending_digest.is_empty() {
                return None;
            }
            let events = std::mem::take(&mut info.pending_digest);
            (info.clone(), events)
        };
        // Formatting is pure and touches no shared state, so it happens
        // outside the lock — same reasoning as the Python.
        format_session_summary(&info, &events, include_label)
    }

    // --- resume-from-pause helpers ---------------------------------------
    //
    // The "Pause Heard" toggle clears the speech queue but leaves the router's
    // per-session pending_digest piles untouched. Resuming would normally let
    // the 1-second digest tick drain those stale piles, replaying audio from
    // before the pause. The three helpers below let the daemon take explicit
    // control on resume: ask the user whether to catch them up or start fresh,
    // then either flush the buffer through the existing project-flush summary
    // path or drop it on the floor.

    /// Total events currently buffered for the digest summary across every
    /// session, ignoring the channel thresholds the 1-second tick uses.
    /// Surfaced in the daemon's status payload so the UI can decide whether the
    /// resume prompt is worth showing (zero pending → silent resume).
    pub fn pending_count(&self) -> usize {
        let state = self.state.lock().expect("router poisoned");
        state.sessions.iter().map(|s| s.pending_digest.len()).sum()
    }

    /// Drop every session's pending_digest events. Returns the number of events
    /// thrown away (for logging / status). Used by the resume-with-fresh-start
    /// path: user said "don't catch me up", so the buffer goes to /dev/null and
    /// the next event narrates as if pause never accumulated anything.
    pub fn clear_pending(&self) -> usize {
        let mut state = self.state.lock().expect("router poisoned");
        let mut cleared = 0;
        for info in state.sessions.iter_mut() {
            cleared += info.pending_digest.len();
            info.pending_digest.clear();
        }
        cleared
    }

    // --- introspection for menu UI ----------------------------------------

    /// Roll the per-session pending events into a single spoken line. Returns
    /// `None` when there's nothing to say (no events accumulated since the last
    /// drain).
    ///
    /// `drained` is the output of [`MultiAgentRouter::collect_digest`]; passed
    /// in explicitly so the daemon controls when accumulation resets.
    pub fn format_digest(
        &self,
        drained: Option<Vec<(SessionInfo, Vec<DigestEvent>)>>,
    ) -> Option<String> {
        let drained = drained.unwrap_or_else(|| self.collect_digest());
        let parts: Vec<String> = drained
            .iter()
            .filter_map(|(info, events)| format_session_summary(info, events, true))
            .collect();
        if parts.is_empty() {
            return None;
        }
        Some(format!("Background update. {}", parts.join(" ")))
    }

    /// Snapshot for the menu's Active Sessions submenu. Includes sessions
    /// visible within [`SESSION_VISIBLE_S`] even if they're no longer 'active'
    /// for mode-decision purposes.
    pub fn list_active(&self) -> Vec<ActiveSession> {
        let now = self.clock.wall();
        let state = self.state.lock().expect("router poisoned");
        let cutoff_visible = now - SESSION_VISIBLE_S;
        let mut out: Vec<ActiveSession> = state
            .sessions
            .iter()
            .filter(|s| s.last_event >= cutoff_visible)
            .map(|s| ActiveSession {
                session_id: s.session_id.clone(),
                repo_name: if s.repo_name.is_empty() {
                    label_for(s)
                } else {
                    s.repo_name.clone()
                },
                last_event_ago_s: round1(now - s.last_event),
                pinned: state.pinned.as_deref() == Some(s.session_id.as_str()),
            })
            .collect();
        // Python's `sorted` is stable, so equal ages keep insertion order.
        out.sort_by(|a, b| {
            a.last_event_ago_s
                .partial_cmp(&b.last_event_ago_s)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }

    /// Read one session's record. Mirrors the Python tests reaching into
    /// `router._sessions[...]`.
    pub fn session_info(&self, session_id: &str) -> Option<SessionInfo> {
        self.state
            .lock()
            .expect("router poisoned")
            .get(session_id)
            .cloned()
    }

    /// Backdate a session's `last_event`, as the Python tests do directly, to
    /// simulate a session that has gone quiet. Test-only.
    pub fn backdate_for_test(&self, session_id: &str, last_event: f64) {
        let mut state = self.state.lock().expect("router poisoned");
        if let Some(info) = state.get_mut(session_id) {
            info.last_event = last_event;
        }
    }
}

impl Default for MultiAgentRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// One row of the Active Sessions menu.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ActiveSession {
    pub session_id: String,
    pub repo_name: String,
    pub last_event_ago_s: f64,
    pub pinned: bool,
}

/// `round(x, 1)` — Python rounds half to even, which only shows up on exact
/// halves and is cheap to match.
fn round1(value: f64) -> f64 {
    let scaled = value * 10.0;
    let rounded = if (scaled - scaled.trunc()).abs() == 0.5 {
        let floor = scaled.floor();
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    } else {
        scaled.round()
    };
    rounded / 10.0
}

/// Key for grouping sessions into channels. repo_name when we have one;
/// session_id fallback so an unknown-cwd session still gets its own channel
/// rather than colliding under "".
fn project_key(info: &SessionInfo) -> String {
    if info.repo_name.is_empty() {
        info.session_id.clone()
    } else {
        info.repo_name.clone()
    }
}

/// The body shared by `collect_project_flushes` and `force_flush_all`. `now`
/// is `Some` for the gated tick path and `None` for the forced one.
fn drain(state: &mut RouterState, auto_voices: bool, now: Option<f64>) -> Vec<ProjectFlush> {
    // Group sessions by project, keeping first-seen order.
    let mut by_project: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, info) in state.sessions.iter().enumerate() {
        let key = project_key(info);
        match by_project.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1.push(index),
            None => by_project.push((key, vec![index])),
        }
    }

    // Primary project = the one containing the globally most-recently-active
    // session. That project speaks in the persona's voice so the listener's
    // "main" channel sounds familiar; the rest get auto-pool voices.
    //
    // Python's `>` on the `(last_event, event_seq)` tuple keeps the FIRST
    // maximum, so ties resolve to whichever session was registered earlier.
    let mut primary_key: Option<String> = None;
    let mut best = (-1.0f64, -1i64);
    for info in state.sessions.iter() {
        let candidate = (info.last_event, info.event_seq as i64);
        if candidate.0 > best.0 || (candidate.0 == best.0 && candidate.1 > best.1) {
            best = candidate;
            primary_key = Some(project_key(info));
        }
    }

    let mut out: Vec<ProjectFlush> = Vec::new();
    for (key, members) in &by_project {
        let total_pending: usize = members
            .iter()
            .map(|i| state.sessions[*i].pending_digest.len())
            .sum();
        if total_pending == 0 {
            continue;
        }
        if let Some(now_ts) = now {
            let last_event = members
                .iter()
                .map(|i| state.sessions[*i].last_event)
                .fold(f64::NEG_INFINITY, f64::max);
            let idle_for = now_ts - last_event;
            if idle_for < CHANNEL_IDLE_FLUSH_S && total_pending < CHANNEL_MAX_PENDING {
                continue;
            }
        }
        // Atomic drain — copy out and clear under the lock so any event landing
        // during this call goes into the next cycle's pile, not this one.
        let mut events: Vec<DigestEvent> = Vec::new();
        let mut contributing: Vec<String> = Vec::new();
        for i in members {
            let info = &mut state.sessions[*i];
            if !info.pending_digest.is_empty() {
                contributing.push(info.session_id.clone());
                events.append(&mut info.pending_digest);
            }
        }
        events.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal));
        // Speaker session id = most recently active in the project (so the
        // daemon's speaker-change tracking treats this project as one speaker).
        // `max()` keeps the FIRST maximum, like Python's.
        let speaker_index = members
            .iter()
            .copied()
            .fold(None::<usize>, |best, candidate| match best {
                None => Some(candidate),
                Some(best_i) => {
                    let b = &state.sessions[best_i];
                    let c = &state.sessions[candidate];
                    if (c.last_event, c.event_seq) > (b.last_event, b.event_seq) {
                        Some(candidate)
                    } else {
                        Some(best_i)
                    }
                }
            })
            .expect("a project always has at least one member");
        let speaker = &state.sessions[speaker_index];
        let label = if speaker.repo_name.is_empty() {
            if speaker.session_id.is_empty() {
                "agent".to_string()
            } else {
                speaker.session_id.chars().take(8).collect()
            }
        } else {
            speaker.repo_name.clone()
        };
        let is_primary = primary_key.as_deref() == Some(key.as_str());
        let voice_override = if auto_voices && !is_primary && !speaker.repo_name.is_empty() {
            Some(auto_voice_for(&speaker.repo_name).to_string())
        } else {
            None
        };
        out.push(ProjectFlush {
            project_key: key.clone(),
            label,
            events,
            member_session_ids: contributing,
            speaker_session_id: speaker.session_id.clone(),
            voice_override,
            is_primary,
        });
    }
    // Largest pile first. Stable, so equal sizes keep grouping order.
    out.sort_by_key(|pf| std::cmp::Reverse(pf.events.len()));
    out
}

/// `"Agent <name>: "` prefix for the focused agent's narration when there's no
/// other way to tell agents apart by sound — i.e. the single-voice multi-agent
/// mode (auto_voices off, no manual voice for this repo). Empty when:
///   - only one active agent (no ambiguity),
///   - this agent has a distinct voice (auto-pool or manual map),
///   - this agent already spoke last (don't re-announce its name on every
///     consecutive line — only on a speaker change).
///
/// Must be evaluated *before* the speaker is recorded.
fn focus_label_prefix(
    state: &RouterState,
    session_id: &str,
    agent_voices: &HashMap<String, String>,
    auto_voices: bool,
    now: f64,
) -> String {
    if state.active_count(now) < 2 {
        return String::new();
    }
    if auto_voices {
        return String::new();
    }
    if let Some(info) = state.get(session_id) {
        if agent_voices
            .get(&info.repo_name)
            .is_some_and(|v| !v.is_empty())
        {
            return String::new();
        }
    }
    if state.last_narrated_session.as_deref() == Some(session_id) {
        return String::new();
    }
    let label = match state.get(session_id) {
        Some(info) => label_for(info),
        None => {
            let short: String = session_id.chars().take(8).collect();
            if short.is_empty() {
                "agent".to_string()
            } else {
                short
            }
        }
    };
    format!("Agent {label}: ")
}

fn pierced(state: &RouterState, session_id: &str, voice: Option<String>) -> RoutingDecision {
    let label = match state.get(session_id) {
        Some(info) => label_for(info),
        None => session_id.chars().take(8).collect(),
    };
    RoutingDecision {
        action: Action::Speak,
        label_prefix: format!("Agent {label}: "),
        voice_override: voice,
    }
}

/// Three-step voice resolution:
///
/// 1. Manual map (`agent_voices`) wins always — the user explicitly assigned
///    this repo to this voice.
/// 2. Auto-pick from the pool — but only for non-focus sessions, so the agent
///    the user is actively driving keeps the persona's voice (the "default"
///    speaker). Without this, solo-mode users would hear a hash-picked voice
///    instead of the persona they configured.
/// 3. Otherwise `None` → caller falls through to persona/cfg voice.
///
/// Keyed by repo_name (cwd basename) so the mapping survives across CC
/// restarts — session_ids change every run, but the project dir doesn't.
fn voice_for(
    state: &RouterState,
    session_id: &str,
    agent_voices: &HashMap<String, String>,
    auto_voices: bool,
    is_focus: bool,
) -> Option<String> {
    let info = state.get(session_id)?;
    if let Some(manual) = agent_voices.get(&info.repo_name) {
        if !manual.is_empty() {
            return Some(manual.clone());
        }
    }
    if auto_voices && !is_focus && !info.repo_name.is_empty() {
        return Some(auto_voice_for(&info.repo_name).to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::project_name::BasenameProjectNamer;

    fn router(clock: Arc<ManualClock>) -> MultiAgentRouter {
        MultiAgentRouter::with(clock, Arc::new(BasenameProjectNamer))
    }

    #[test]
    fn auto_voice_is_deterministic_and_inside_the_pool() {
        // tests/test_multi_agent.py::test_auto_voice_for_helper_is_deterministic
        let a = auto_voice_for("api");
        assert_eq!(a, auto_voice_for("api"));
        assert!(AUTO_VOICE_POOL.contains(&a));
        let distinct: std::collections::HashSet<&str> =
            ["api", "web", "cli", "frontend", "infra", "ml"]
                .iter()
                .map(|n| auto_voice_for(n))
                .collect();
        assert!(distinct.len() >= 4, "{distinct:?}");
    }

    #[test]
    fn article_follows_the_vowel_sound_not_the_vowel_letter() {
        assert_eq!(article_for("edit"), "an");
        assert_eq!(article_for("install"), "an");
        assert_eq!(article_for("operation"), "an");
        assert_eq!(article_for("upload"), "an");
        assert_eq!(article_for("user check"), "a");
        assert_eq!(article_for("unique fix"), "a");
        assert_eq!(article_for("one-off"), "a");
        assert_eq!(article_for("build"), "a");
        assert_eq!(article_for(""), "a");
    }

    #[test]
    fn capitalize_lowercases_the_tail_like_python() {
        assert_eq!(capitalize("API"), "Api");
        assert_eq!(capitalize("api"), "Api");
        assert_eq!(capitalize(""), "");
        assert_eq!(upper_first("2 edits"), "2 edits");
        assert_eq!(upper_first("api THING"), "Api THING");
    }

    #[test]
    fn solo_speaks_everything_without_a_prefix() {
        // tests/test_multi_agent.py::test_solo_mode_speaks_everything
        let r = router(Arc::new(ManualClock::new(1000.0)));
        r.note_event("only-session", "/Users/x/projects/api", None);
        for tag in [
            "tool_pre",
            "intermediate_short",
            "final_short",
            "tool_post_failure",
        ] {
            let d = r.classify("any", tag, "only-session", None, false);
            assert_eq!(d.action, Action::Speak);
            assert_eq!(d.label_prefix, "");
        }
        assert_eq!(r.mode(), Mode::Solo);
    }

    #[test]
    fn force_flush_ignores_the_idle_gate_and_empties_the_buffer() {
        // tests/test_multi_agent.py::test_force_flush_all_returns_every_project…
        let r = router(Arc::new(ManualClock::new(1000.0)));
        r.note_event("a", "/x/api", None);
        r.note_event("b", "/x/web", None);
        r.add_to_digest("a", "tool_pre", "tool_edit", "edit", None);
        r.add_to_digest("b", "tool_pre", "tool_edit", "edit", None);
        assert!(r.collect_project_flushes(true, None).is_empty());
        let mut labels: Vec<String> = r
            .force_flush_all(true, None)
            .into_iter()
            .map(|pf| pf.label)
            .collect();
        labels.sort();
        assert_eq!(labels, vec!["api".to_string(), "web".to_string()]);
        assert_eq!(r.pending_count(), 0);
    }

    #[test]
    fn the_template_fallback_runs_when_the_summarizer_declines() {
        let flush = ProjectFlush {
            project_key: "api".into(),
            label: "api".into(),
            events: vec![
                DigestEvent {
                    kind: "tool_pre".into(),
                    tag: "tool_edit".into(),
                    neutral: String::new(),
                    ctx: serde_json::Value::Null,
                    ts: 1.0,
                },
                DigestEvent {
                    kind: "tool_pre".into(),
                    tag: "tool_edit".into(),
                    neutral: String::new(),
                    ctx: serde_json::Value::Null,
                    ts: 2.0,
                },
            ],
            member_session_ids: vec!["a".into()],
            speaker_session_id: "a".into(),
            voice_override: None,
            is_primary: true,
        };
        assert_eq!(
            project_flush_text(&TemplateOnly, &flush, false).as_deref(),
            Some("Api: 2 edits.")
        );
        // Solo drops the label, per the dogfooding complaint about hearing the
        // repo name on every burst.
        assert_eq!(
            project_flush_text(&TemplateOnly, &flush, true).as_deref(),
            Some("2 edits.")
        );

        struct Narrative;
        impl ProjectSummarizer for Narrative {
            fn summarize_project(
                &self,
                label: &str,
                _events: &[DigestEvent],
                _member_count: usize,
                _solo: bool,
            ) -> Option<String> {
                Some(format!("On the {label} project, tightened the auth flow."))
            }
        }
        assert_eq!(
            project_flush_text(&Narrative, &flush, false).as_deref(),
            Some("On the api project, tightened the auth flow.")
        );
    }

    #[test]
    fn an_empty_narrative_falls_through_to_the_template() {
        struct Blank;
        impl ProjectSummarizer for Blank {
            fn summarize_project(
                &self,
                _l: &str,
                _e: &[DigestEvent],
                _m: usize,
                _s: bool,
            ) -> Option<String> {
                Some(String::new())
            }
        }
        let flush = ProjectFlush {
            project_key: "api".into(),
            label: "api".into(),
            events: vec![DigestEvent {
                kind: "tool_pre".into(),
                tag: "tool_bash_test".into(),
                neutral: String::new(),
                ctx: serde_json::Value::Null,
                ts: 1.0,
            }],
            member_session_ids: vec!["a".into(), "b".into()],
            speaker_session_id: "a".into(),
            voice_override: None,
            is_primary: true,
        };
        assert_eq!(
            project_flush_text(&Blank, &flush, false).as_deref(),
            Some("Api: a test run across two agents.")
        );
    }

    #[test]
    fn abspath_collapses_dot_segments_without_touching_disk() {
        assert_eq!(abspath("/a/b/../c/./d"), "/a/c/d");
        assert_eq!(abspath("/"), "/");
    }

    #[test]
    fn round1_matches_pythons_banker_rounding() {
        assert_eq!(round1(0.25), 0.2);
        assert_eq!(round1(0.35), 0.4);
        assert_eq!(round1(1.24), 1.2);
        assert_eq!(round1(0.0), 0.0);
    }
}
