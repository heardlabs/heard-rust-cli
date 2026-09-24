//! In-memory per-session state kept by the daemon.
//!
//! Keyed by the agent's session_id. Tracks:
//!   - `repo_name`: derived from cwd basename
//!   - `failure_count`: how many tool failures have happened recently
//!   - `last_topic`: a breadcrumb of the last thing we narrated (for the
//!     persona to avoid repetition)
//!   - `last_seen`: timestamp for eviction
//!
//! This is intentionally tiny and ephemeral. The daemon holds it in RAM; a
//! restart clears everything. That's fine — CC sessions are also ephemeral.
//!
//! Note the deliberate difference from [`crate::agent_state`]: this store takes
//! the **cwd basename** directly, not `canonical_project_name`. No git, no
//! remote slug. The two are additive rather than
//! duplicated, and that asymmetry is part of it — this one is the cheap
//! bookkeeping the router leans on, so it never shells out.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::clock::{Clock, SystemClock};

/// Six hours of inactivity.
pub const EVICT_AFTER_S: f64 = 6.0 * 3600.0;
/// The window `tool_density` counts over.
pub const DENSITY_WINDOW_S: f64 = 30.0;

/// The publicly visible half of a session record — Python returns
/// `{k: v for k, v in sess.items() if not k.startswith("_")}`, i.e. everything
/// except the `_events` deque.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionView {
    pub repo_name: Option<String>,
    pub failure_count: u64,
    pub last_topic: Option<String>,
    /// `None` for a session that does not exist — `get` on an unknown id
    /// returns `{}`, which has no `last_seen` either.
    pub last_seen: Option<f64>,
}

#[derive(Debug, Clone)]
struct SessionRecord {
    repo_name: Option<String>,
    failure_count: u64,
    last_topic: Option<String>,
    last_seen: f64,
    events: VecDeque<f64>,
}

impl SessionRecord {
    fn view(&self) -> SessionView {
        SessionView {
            repo_name: self.repo_name.clone(),
            failure_count: self.failure_count,
            last_topic: self.last_topic.clone(),
            last_seen: Some(self.last_seen),
        }
    }
}

/// The daemon's per-session store.
pub struct SessionStore {
    sessions: Mutex<Vec<(String, SessionRecord)>>,
    clock: Arc<dyn Clock>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock::new()))
    }

    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            sessions: Mutex::new(Vec::new()),
            clock,
        }
    }

    fn evict(sessions: &mut Vec<(String, SessionRecord)>, now: f64) {
        sessions.retain(|(_, s)| now - s.last_seen <= EVICT_AFTER_S);
    }

    /// Create-or-refresh. Returns the visible half of the record.
    ///
    /// `repo_name` is sticky: the first cwd that yields one wins, so a later
    /// event from a different directory cannot rename a live session.
    pub fn touch(&self, session_id: &str, cwd: Option<&str>) -> SessionView {
        let now = self.clock.wall();
        let mut sessions = self.sessions.lock().expect("session store poisoned");
        Self::evict(&mut sessions, now);
        if !sessions.iter().any(|(id, _)| id == session_id) {
            sessions.push((
                session_id.to_string(),
                SessionRecord {
                    repo_name: None,
                    failure_count: 0,
                    last_topic: None,
                    last_seen: now,
                    events: VecDeque::new(),
                },
            ));
        }
        let record = sessions
            .iter_mut()
            .find(|(id, _)| id == session_id)
            .map(|(_, s)| s)
            .expect("just inserted");
        record.last_seen = now;
        if let Some(cwd) = cwd {
            if !cwd.is_empty() && record.repo_name.is_none() {
                // `os.path.basename(cwd.rstrip("/")) or cwd` — a cwd of "/"
                // has no basename, so the raw value is kept.
                let trimmed = cwd.trim_end_matches('/');
                let base = std::path::Path::new(trimmed)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                record.repo_name = Some(if base.is_empty() {
                    cwd.to_string()
                } else {
                    base.to_string()
                });
            }
        }
        record.view()
    }

    /// Note that a tool event happened, for the density window. No-op for an
    /// unknown session.
    pub fn record_tool_event(&self, session_id: &str) {
        let now = self.clock.wall();
        let mut sessions = self.sessions.lock().expect("session store poisoned");
        let Some((_, record)) = sessions.iter_mut().find(|(id, _)| id == session_id) else {
            return;
        };
        record.events.push_back(now);
        let cutoff = now - DENSITY_WINDOW_S;
        while record.events.front().is_some_and(|t| *t < cutoff) {
            record.events.pop_front();
        }
    }

    /// How many tool events landed in the last [`DENSITY_WINDOW_S`] seconds.
    /// Prunes as it reads, exactly as the Python does.
    pub fn tool_density(&self, session_id: &str) -> usize {
        let now = self.clock.wall();
        let mut sessions = self.sessions.lock().expect("session store poisoned");
        let Some((_, record)) = sessions.iter_mut().find(|(id, _)| id == session_id) else {
            return 0;
        };
        let cutoff = now - DENSITY_WINDOW_S;
        while record.events.front().is_some_and(|t| *t < cutoff) {
            record.events.pop_front();
        }
        record.events.len()
    }

    /// The visible half of a record, or an all-default view for an unknown id
    /// (Python returns `{}`; `last_seen: None` is how that reads here).
    pub fn get(&self, session_id: &str) -> SessionView {
        self.sessions
            .lock()
            .expect("session store poisoned")
            .iter()
            .find(|(id, _)| id == session_id)
            .map(|(_, s)| s.view())
            .unwrap_or_default()
    }

    /// Bump the failure counter. No-op for an unknown session — note this does
    /// NOT create one, so ordering against `touch` matters.
    pub fn note_failure(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().expect("session store poisoned");
        if let Some((_, record)) = sessions.iter_mut().find(|(id, _)| id == session_id) {
            record.failure_count += 1;
        }
    }

    /// Remember the last thing we narrated for this session. No-op for an
    /// unknown session.
    pub fn note_topic(&self, session_id: &str, topic: &str) {
        let mut sessions = self.sessions.lock().expect("session store poisoned");
        if let Some((_, record)) = sessions.iter_mut().find(|(id, _)| id == session_id) {
            record.last_topic = Some(topic.to_string());
        }
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    fn store(clock: Arc<ManualClock>) -> SessionStore {
        SessionStore::with_clock(clock)
    }

    #[test]
    fn touch_creates_session_with_repo_name() {
        // tests/test_session.py::test_touch_creates_session_with_repo_name
        let s = store(Arc::new(ManualClock::new(100.0)));
        let view = s.touch("abc", Some("/Users/me/my-repo"));
        assert_eq!(view.repo_name.as_deref(), Some("my-repo"));
        assert_eq!(view.failure_count, 0);
    }

    #[test]
    fn touch_is_idempotent_and_does_not_reset_failures() {
        // tests/test_session.py::test_touch_idempotent_does_not_reset_failures
        let s = store(Arc::new(ManualClock::new(100.0)));
        s.touch("abc", Some("/x/y/repo"));
        s.note_failure("abc");
        s.note_failure("abc");
        s.touch("abc", Some("/x/y/repo"));
        assert_eq!(s.get("abc").failure_count, 2);
    }

    #[test]
    fn unknown_session_reads_as_empty_and_writes_are_dropped() {
        // tests/test_session.py::test_get_unknown_session_returns_empty_dict
        let s = store(Arc::new(ManualClock::new(100.0)));
        assert_eq!(s.get("missing"), SessionView::default());
        s.note_failure("missing");
        s.note_topic("missing", "t");
        assert_eq!(s.get("missing"), SessionView::default());
        assert_eq!(s.tool_density("missing"), 0);
    }

    #[test]
    fn tool_density_forgets_events_older_than_the_window() {
        let clock = Arc::new(ManualClock::new(1000.0));
        let s = store(clock.clone());
        s.touch("a", Some("/x/repo"));
        s.record_tool_event("a");
        clock.advance(10.0);
        s.record_tool_event("a");
        assert_eq!(s.tool_density("a"), 2);
        clock.advance(21.0);
        assert_eq!(s.tool_density("a"), 1);
        clock.advance(100.0);
        assert_eq!(s.tool_density("a"), 0);
    }

    #[test]
    fn a_session_quiet_for_six_hours_is_evicted_on_the_next_touch() {
        let clock = Arc::new(ManualClock::new(0.0));
        let s = store(clock.clone());
        s.touch("old", Some("/a/old"));
        clock.advance(EVICT_AFTER_S + 1.0);
        s.touch("new", Some("/a/new"));
        assert_eq!(s.get("old"), SessionView::default());
        assert_eq!(s.get("new").repo_name.as_deref(), Some("new"));
    }
}
