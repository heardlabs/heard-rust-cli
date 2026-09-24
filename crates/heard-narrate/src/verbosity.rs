//! Per-event verbosity decisions, profile-driven.
//!
//! Port of `engine/heard/verbosity.py`. Each event passes through three gates:
//!
//! ```text
//!   classify_pre(cfg, tag, density)  → speak | drop | digest
//!   classify_post(cfg, tag)          → speak | drop
//!   classify_prose(cfg)              → speak | drop
//! ```
//!
//! Three-way on the pre gate so the daemon can route each outcome
//! appropriately — silent drop, accumulate-for-digest, or send through the
//! queue.
//!
//! The actual decisions come from profiles ([`crate::profile`]). The config key
//! `verbosity` names the profile; solo / focus events use it, and swarm
//! non-focus events use `swarm_verbosity` (default "brief") — that selection is
//! the router's job (`multi_agent.py`, a later stage), not this module's.

use serde_json::Value;

use crate::profile::{self, Profile};

/// Window the `SessionStore.tool_density` count covers — used by callers, kept
/// here because that is where Python keeps it.
pub const DENSITY_WINDOW_S: u64 = 30;

/// Legacy: long-running tool tags that always pierce even on quiet settings.
/// Tests, builds, installs, push/sync and agent delegation are user-relevant
/// beats, not micro-operations; quiet and digest profiles are about cutting the
/// noise, not the milestones.
const ALWAYS_NARRATE_PRE: &[&str] = &[
    "tool_bash_test",
    "tool_bash_build",
    "tool_bash_install",
    "tool_bash_push",
    "tool_bash_sync",
    "tool_agent",
    "tool_question",
];

const FAILURE_TAGS: &[&str] = &["tool_post_failure", "tool_post_command_failed"];

/// Blocked on the USER (install an extension, sign in). Pierces the profile for
/// the same reason a failure does: the worst outcome is silence. The owner's
/// "install the Claude Chrome extension" was created as an event and then
/// dropped here by the default profile's `post_success: silent` (2026-09-11).
const NEEDS_YOU_TAGS: &[&str] = &["tool_post_needs_you"];

/// Routine per-file operations. Voicing each one is a repetitive, low-signal
/// stream ("Editing X." "Editing Y." "Searching the codebase.") that tells the
/// listener nothing actionable — the milestones (tests/builds/errors/questions/
/// completions) are what matter. So these are NOT narrated individually except
/// in Verbose, the explicit play-by-play mode. They still count toward burst
/// density and still route to the digest under a digest profile (Brief).
const LOW_SIGNAL_PRE: &[&str] = &["tool_edit", "tool_write", "tool_glob", "tool_grep"];

/// What to do with one event. Returned as a value (not a string) because the
/// daemon routes on it; [`Decision::as_str`] is the wire form Python returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Speak,
    Drop,
    Digest,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Speak => "speak",
            Decision::Drop => "drop",
            Decision::Digest => "digest",
        }
    }
}

/// The daemon config, as loaded from `config.yaml`. Borrowed, not copied: this
/// only ever reads a handful of keys out of it.
///
/// Python reads a plain dict and leans on Python truthiness for the toggles
/// (`if not cfg.get("narrate_tools", True)`), so a `0`, an empty string or a
/// `null` in someone's config.yaml all mean "off". [`Cfg::truthy`] reproduces
/// that exactly rather than quietly treating a non-bool as absent.
#[derive(Debug, Clone, Copy)]
pub struct Cfg<'a>(pub &'a Value);

impl Cfg<'_> {
    fn truthy(&self, key: &str, default: bool) -> bool {
        match self.0.get(key) {
            None => default,
            Some(v) => py_truthy(v),
        }
    }

    fn verbosity(&self) -> Option<&str> {
        self.0.get("verbosity").and_then(Value::as_str)
    }

    /// Resolve the active profile for the focus / solo event path.
    fn profile(&self) -> Profile {
        profile::load(self.verbosity())
    }
}

/// Python's notion of truth, for config values that were never type-checked.
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Active profile name (after legacy normalisation). Used by tests and the
/// menu's checkmark logic.
pub fn level(cfg: &Cfg) -> String {
    cfg.profile().name
}

/// Three-way pre-tool decision. The master `narrate_tools` toggle still
/// short-circuits everything to drop. Wait-state questions always speak
/// regardless of profile.
pub fn classify_pre(cfg: &Cfg, tag: &str, density: i64) -> Decision {
    if !cfg.truthy("narrate_tools", true) {
        return Decision::Drop;
    }
    if tag == "tool_question" {
        return Decision::Speak;
    }
    classify_pre_with_profile(&cfg.profile(), tag, density)
}

/// The profile half of [`classify_pre`], split out exactly as Python splits it
/// so the router can classify against a profile it already has.
pub fn classify_pre_with_profile(prof: &Profile, tag: &str, density: i64) -> Decision {
    let is_long_running = ALWAYS_NARRATE_PRE.contains(&tag);
    match prof.pre_tool.as_str() {
        "silent" => {
            return if is_long_running {
                Decision::Speak
            } else {
                Decision::Drop
            };
        }
        "digest" => {
            return if is_long_running {
                Decision::Speak
            } else {
                Decision::Digest
            };
        }
        _ => {}
    }
    // per_tool: speak each meaningful tool. Routine per-file operations
    // (edits/writes/searches) are dropped here — voicing every one is the
    // repetitive "Editing X." stream that doesn't help. Verbose keeps them
    // (it's the explicit play-by-play mode).
    if LOW_SIGNAL_PRE.contains(&tag) && prof.name != "verbose" {
        return Decision::Drop;
    }
    // burst overflow routed to digest.
    if density > prof.burst_threshold && !is_long_running {
        return Decision::Digest;
    }
    Decision::Speak
}

/// Failures always pierce — even at the quietest verbosity, hearing "command
/// failed" beats silently missing a regression. Use the master `narrate_tools`
/// toggle to mute Heard entirely. Successes follow the profile's `post_success`
/// switch.
pub fn classify_post(cfg: &Cfg, tag: &str) -> Decision {
    if FAILURE_TAGS.contains(&tag) || NEEDS_YOU_TAGS.contains(&tag) {
        return Decision::Speak;
    }
    if !cfg.truthy("narrate_tools", true) {
        return Decision::Drop;
    }
    if !cfg.truthy("narrate_tool_results", true) {
        return Decision::Drop;
    }
    if cfg.profile().post_success == "speak" {
        Decision::Speak
    } else {
        Decision::Drop
    }
}

/// Intermediate / final prose follow the prose dimension. A silent profile
/// (Quiet) drops them; everything else speaks.
pub fn classify_prose(cfg: &Cfg) -> Decision {
    if cfg.profile().prose == "speak" {
        Decision::Speak
    } else {
        Decision::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(v: &Value) -> Cfg<'_> {
        Cfg(v)
    }

    #[test]
    fn the_python_verbosity_tests_hold() {
        // engine/tests/test_verbosity.py
        let low = json!({"narrate_tools": true, "verbosity": "low"});
        assert_eq!(
            classify_pre(&cfg(&low), "tool_bash_test", 0),
            Decision::Speak
        );
        assert_eq!(classify_pre(&cfg(&low), "tool_edit", 0), Decision::Drop);
        assert_eq!(
            classify_pre(&cfg(&low), "tool_question", 0),
            Decision::Speak
        );

        let normal = json!({"narrate_tools": true, "verbosity": "normal"});
        assert_eq!(
            classify_pre(&cfg(&normal), "tool_bash_generic", 3),
            Decision::Speak
        );
        assert_eq!(
            classify_pre(&cfg(&normal), "tool_bash_generic", 10),
            Decision::Digest
        );
        assert_eq!(
            classify_pre(&cfg(&normal), "tool_bash_test", 10),
            Decision::Speak
        );
        for tag in ["tool_edit", "tool_write", "tool_glob", "tool_grep"] {
            assert_eq!(classify_pre(&cfg(&normal), tag, 0), Decision::Drop, "{tag}");
        }

        let verbose = json!({"narrate_tools": true, "verbosity": "verbose"});
        for tag in ["tool_edit", "tool_write", "tool_glob", "tool_grep"] {
            assert_eq!(
                classify_pre(&cfg(&verbose), tag, 0),
                Decision::Speak,
                "{tag}"
            );
        }

        let brief = json!({"narrate_tools": true, "verbosity": "brief"});
        assert_eq!(classify_pre(&cfg(&brief), "tool_edit", 0), Decision::Digest);
        assert_eq!(
            classify_pre(&cfg(&brief), "tool_bash_test", 0),
            Decision::Speak
        );

        // Failures and needs-you pierce everything, including the master mute.
        let muted = json!({"narrate_tools": false, "verbosity": "high"});
        assert_eq!(
            classify_pre(&cfg(&muted), "tool_bash_test", 0),
            Decision::Drop
        );
        assert_eq!(
            classify_post(&cfg(&muted), "tool_post_failure"),
            Decision::Speak
        );
        assert_eq!(
            classify_post(&cfg(&muted), "tool_post_command_failed"),
            Decision::Speak
        );
        assert_eq!(
            classify_post(&cfg(&muted), "tool_post_needs_you"),
            Decision::Speak
        );

        let empty = json!({});
        assert_eq!(
            classify_post(&cfg(&empty), "tool_post_success"),
            Decision::Drop
        );
        assert_eq!(
            classify_prose(&cfg(&json!({"verbosity": "quiet"}))),
            Decision::Drop
        );
        assert_eq!(level(&cfg(&json!({"verbosity": "bogus"}))), "normal");
    }
}
