//! Canonical, provenanced session → host identity
//! (`engine/heard/session_identity.py`).
//!
//! Identity is separate from activity: which terminal/editor a session lives
//! in comes ONLY from the session's own evidence (the hook process's inherited
//! environment → the frame's `binding`), never from "whatever app is in front".
//! Anything that needs "which host is that session in" reads it
//! from here.
//!
//! Ported whole: known-host filter, the app-installed check for weak
//! signals, prior-field carry-over, PID-reuse reset, 6 h eviction, the
//! `display_name` confidence floor.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::clock::Clock;

/// `_KNOWN_HOSTS`.
pub const KNOWN_HOSTS: [&str; 6] = ["Ghostty", "iTerm", "Terminal", "VS Code", "Cursor", "Codex"];
/// `_EVICT_AFTER_S`.
pub const EVICT_AFTER_S: f64 = 6.0 * 3600.0;
/// `UNKNOWN_LABEL`.
pub const UNKNOWN_LABEL: &str = "Terminal unknown";
const APP_SEARCH_DIRS: [&str; 3] = [
    "/Applications",
    "/System/Applications",
    "/System/Applications/Utilities",
];

/// `_APP_BUNDLE_NAMES`.
#[must_use]
pub fn bundle_name(host: &str) -> &str {
    match host {
        "VS Code" => "Visual Studio Code",
        other => other,
    }
}

/// One `SessionIdentity`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionIdentity {
    pub session_id: String,
    pub cwd: Option<String>,
    pub host_name: String,
    pub host_type: String,
    pub provenance: String,
    pub confidence: f64,
    pub process_id: Option<i64>,
    pub process_started_at: Option<f64>,
    pub read_only: bool,
    /// monotonic
    pub updated_at: f64,
}

impl SessionIdentity {
    /// The one string every consumer shows.
    #[must_use]
    pub fn display_name(&self) -> &str {
        if !self.host_name.is_empty() && self.confidence >= 0.8 {
            &self.host_name
        } else {
            UNKNOWN_LABEL
        }
    }
}

/// `process_started_at`: omitted vs an explicit value (possibly `None`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StartedAt {
    Omitted,
    Given(Option<f64>),
}

/// One `register(...)` call's arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Registration {
    pub cwd: Option<String>,
    pub host_name: String,
    pub host_type: String,
    pub provenance: String,
    pub confidence: f64,
    pub process_id: Option<i64>,
    pub process_started_at: StartedAt,
    pub read_only: bool,
}

impl Default for Registration {
    fn default() -> Self {
        Self {
            cwd: None,
            host_name: String::new(),
            host_type: "terminal".into(),
            provenance: "unknown".into(),
            confidence: 0.0,
            process_id: None,
            process_started_at: StartedAt::Omitted,
            read_only: false,
        }
    }
}

type AppAvailable = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// `SessionIdentityRegistry`.
pub struct SessionIdentityRegistry {
    items: Mutex<HashMap<String, SessionIdentity>>,
    app_available: AppAvailable,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for SessionIdentityRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionIdentityRegistry")
            .field("len", &self.all().len())
            .finish_non_exhaustive()
    }
}

/// `_default_app_available(name)`.
#[must_use]
pub fn default_app_available(name: &str) -> bool {
    if name == "Terminal" {
        return true;
    }
    let bundle = bundle_name(name);
    APP_SEARCH_DIRS
        .iter()
        .any(|d| std::path::Path::new(&format!("{d}/{bundle}.app")).exists())
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(_) => true,
    }
}

impl SessionIdentityRegistry {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self::with_app_check(clock, Arc::new(default_app_available))
    }

    pub fn with_app_check(clock: Arc<dyn Clock>, app_available: AppAvailable) -> Self {
        Self {
            items: Mutex::new(HashMap::new()),
            app_available,
            clock,
        }
    }

    fn evict(&self, items: &mut HashMap<String, SessionIdentity>) {
        let now = self.clock.monotonic();
        items.retain(|_, i| now - i.updated_at <= EVICT_AFTER_S);
    }

    /// `register(session_id, …)`.
    pub fn register(&self, session_id: &str, r: Registration) -> SessionIdentity {
        let now = self.clock.monotonic();
        let mut host_name = if KNOWN_HOSTS.contains(&r.host_name.as_str()) {
            r.host_name.clone()
        } else {
            String::new()
        };
        let (mut confidence, mut provenance) = (r.confidence, r.provenance.clone());
        if !host_name.is_empty() && confidence < 1.0 && !(self.app_available)(&host_name) {
            host_name = String::new();
            confidence = 0.0;
            provenance = "host_not_available".into();
        }
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.evict(&mut items);
        let mut prior = items.get(session_id).cloned();
        if let (Some(p), Some(pid), StartedAt::Given(started)) =
            (&prior, r.process_id, r.process_started_at)
        {
            if p.process_id == Some(pid) && p.process_started_at != started {
                prior = None;
            }
        }
        let started = match r.process_started_at {
            StartedAt::Omitted => prior.as_ref().and_then(|p| p.process_started_at),
            StartedAt::Given(s) => s,
        };
        let has_host = !host_name.is_empty();
        let item = SessionIdentity {
            session_id: session_id.to_owned(),
            cwd: r
                .cwd
                .clone()
                .filter(|c| !c.is_empty())
                .or_else(|| prior.as_ref().and_then(|p| p.cwd.clone())),
            host_name: if has_host {
                host_name
            } else {
                prior
                    .as_ref()
                    .map(|p| p.host_name.clone())
                    .unwrap_or_default()
            },
            host_type: if r.host_type.is_empty() {
                prior
                    .as_ref()
                    .map_or_else(|| "terminal".to_owned(), |p| p.host_type.clone())
            } else {
                r.host_type.clone()
            },
            provenance: if has_host {
                provenance.clone()
            } else {
                prior.as_ref().map_or(provenance, |p| p.provenance.clone())
            },
            confidence: if has_host {
                confidence
            } else {
                prior.as_ref().map_or(confidence, |p| p.confidence)
            },
            process_id: r
                .process_id
                .or_else(|| prior.as_ref().and_then(|p| p.process_id)),
            process_started_at: started,
            read_only: r.read_only || prior.as_ref().is_some_and(|p| p.read_only),
            updated_at: now,
        };
        items.insert(session_id.to_owned(), item.clone());
        item
    }

    /// `observe_event({"session": {"id", "cwd", "binding"}})` — the hook
    /// path: the session id, its cwd, and the binding the hook process derived.
    pub fn observe(
        &self,
        session_id: &str,
        cwd: Option<&str>,
        binding: Option<&Value>,
    ) -> Option<SessionIdentity> {
        let sid = session_id.trim();
        if sid.is_empty() {
            return None;
        }
        let empty = Value::Object(serde_json::Map::new());
        let b = binding.filter(|b| b.is_object()).unwrap_or(&empty);
        let s = |k: &str| match b.get(k) {
            Some(Value::String(x)) => x.clone(),
            Some(v) if truthy(Some(v)) => v.to_string(),
            _ => String::new(),
        };
        let started = match b.get("process_started_at") {
            None => StartedAt::Omitted,
            Some(Value::Null) => StartedAt::Given(None),
            Some(v) => StartedAt::Given(v.as_f64()),
        };
        let host_type = s("host_type");
        let provenance = s("provenance");
        Some(self.register(
            sid,
            Registration {
                cwd: cwd.map(str::to_owned),
                host_name: s("host_name"),
                host_type: if host_type.is_empty() {
                    "terminal".into()
                } else {
                    host_type
                },
                provenance: if provenance.is_empty() {
                    "unknown".into()
                } else {
                    provenance
                },
                confidence: b.get("confidence").and_then(Value::as_f64).unwrap_or(0.0),
                process_id: b.get("pid").and_then(Value::as_i64),
                process_started_at: started,
                read_only: truthy(b.get("read_only")),
            },
        ))
    }

    pub fn get(&self, session_id: &str) -> Option<SessionIdentity> {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.evict(&mut items);
        items.get(session_id).cloned()
    }

    pub fn all(&self) -> Vec<SessionIdentity> {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.evict(&mut items);
        let mut v: Vec<SessionIdentity> = items.values().cloned().collect();
        v.sort_by(|a, b| {
            a.updated_at
                .partial_cmp(&b.updated_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    }

    pub fn remove(&self, session_id: &str) {
        self.items
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use serde_json::json;

    fn reg(installed: &'static [&'static str]) -> (SessionIdentityRegistry, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(100.0));
        let r = SessionIdentityRegistry::with_app_check(
            clock.clone(),
            Arc::new(move |n| installed.contains(&n)),
        );
        (r, clock)
    }

    #[test]
    fn a_binding_names_a_known_installed_host() {
        let (r, _) = reg(&["Ghostty"]);
        let i = r.observe("s1", Some("/w"), Some(&json!({"host_name": "Ghostty", "confidence": 0.9, "provenance": "env_terminal_binding", "pid": 42}))).unwrap();
        assert_eq!(i.display_name(), "Ghostty");
        assert_eq!(i.process_id, Some(42));
        // A later event with no binding keeps what was learned.
        let i = r.observe("s1", None, None).unwrap();
        assert_eq!(i.host_name, "Ghostty");
        assert_eq!(i.cwd.as_deref(), Some("/w"));
    }

    #[test]
    fn a_weak_signal_for_an_app_that_is_not_installed_is_blanked() {
        let (r, _) = reg(&[]);
        let i = r
            .observe(
                "s1",
                None,
                Some(&json!({"host_name": "Ghostty", "confidence": 0.9})),
            )
            .unwrap();
        assert_eq!(i.host_name, "");
        assert_eq!(i.provenance, "host_not_available");
        assert_eq!(i.display_name(), UNKNOWN_LABEL);
        // Direct evidence (confidence 1.0) is trusted without the check.
        let i = r
            .observe(
                "s2",
                None,
                Some(&json!({"host_name": "Cursor", "confidence": 1.0})),
            )
            .unwrap();
        assert_eq!(i.host_name, "Cursor");
        // An unknown name is never shown.
        let i = r
            .observe(
                "s3",
                None,
                Some(&json!({"host_name": "Warp", "confidence": 1.0})),
            )
            .unwrap();
        assert_eq!(i.host_name, "");
    }

    #[test]
    fn pid_reuse_clears_the_stale_binding_and_old_records_are_evicted() {
        let (r, clock) = reg(&["Ghostty", "iTerm"]);
        r.observe("s1", Some("/a"), Some(&json!({"host_name": "Ghostty", "confidence": 0.9, "pid": 7, "process_started_at": 1.0})));
        let i = r
            .observe(
                "s1",
                None,
                Some(&json!({"pid": 7, "process_started_at": 2.0})),
            )
            .unwrap();
        assert_eq!(i.host_name, "");
        assert_eq!(i.cwd, None);
        clock.advance(EVICT_AFTER_S + 1.0);
        assert!(r.get("s1").is_none());
        assert!(r.all().is_empty());
    }
}
