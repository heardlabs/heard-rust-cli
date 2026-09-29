//! The [`Brain`] seam — `heard/harness.py`, which is **not ported yet**.
//!
//! In the Python reference implementation the harness is an optional LLM
//! narrator: prose and finals may go to one model call and tools never do. It
//! is network-bound, so it is not part of this crate — there is no provider,
//! no persona dispatch and no HTTP client here.
//!
//! What IS ported is the shape of the three outcomes `_handle_event` reads,
//! because the no-LLM floor below the harness is only reachable through them:
//!
//! | `narrate` returns | `daemon.py` does | here |
//! |---|---|---|
//! | `None` (punt / raised) | the no-LLM floor | [`crate::floor`] |
//! | `Some(speak: false)` | suppress — except a `final` or the turn opener, which override to the floor unless the brain explicitly owns attention | same |
//! | `Some(speak: true)` | enqueue `decision.text` | hand it to the [`Speech`](crate::speech::Speech) sink |
//!
//! [`NoBrain`] is the default and returns `None` for everything, so a daemon
//! wired with it runs the floor for every prose and final event. That is
//! deliberately the *interesting* path for a differential run: it is exactly
//! what the Python daemon does when the LLM is unreachable, so the two sides
//! are comparable without either of them making a network call.

/// One event, as the brain would see it.
///
/// A subset of `harness.narrate`'s arguments: the event itself plus the two
/// pieces of daemon context the floor also reads. The Layer-2 scoreboard and
/// Layer-3 working memory that the real harness takes are absent because
/// nothing in this lane can produce the prompt they feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrainRequest<'a> {
    /// `tool_pre` / `tool_post` / `intermediate` / `final` / …
    pub kind: &'a str,
    /// The narrower routing tag.
    pub tag: &'a str,
    /// The persona-free text of what happened.
    pub neutral: &'a str,
    /// Which agent session.
    pub session_id: &'a str,
    /// The session's cwd, when it has one.
    pub cwd: &'a str,
    /// The router's `repo_name` for this session, or `""`.
    pub project: &'a str,
    /// True for the first `intermediate` after a user prompt. `daemon.py`
    /// force-speaks this one, and overrides a brain that skipped it.
    pub is_opener: bool,
    /// On a `final`, the prompt that opened the turn, so the first
    /// spoken sentence can answer it. `""` when unknown.
    pub last_prompt: &'a str,
}

/// What the brain decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrainDecision {
    /// `False` means "chose silence" — NOT an error, and not the floor.
    pub speak: bool,
    /// The words to speak. Only read when `speak` is true.
    pub text: String,
    /// The silent reasoning stream. `daemon.py` logs it as `harness_think`
    /// and never speaks it; so does this crate.
    pub think: Option<String>,
}

impl BrainDecision {
    /// A decision to speak `text`.
    pub fn speak(text: impl Into<String>) -> Self {
        Self {
            speak: true,
            text: text.into(),
            think: None,
        }
    }

    /// A deliberate silence.
    pub fn silence() -> Self {
        Self {
            speak: false,
            text: String::new(),
            think: None,
        }
    }
}

/// The narration brain.
///
/// The default implementation returns `None` for every event, which is a punt
/// — so a `Brain` that is not wired to anything runs the no-LLM floor, and
/// `impl Brain for MyThing {}` is a complete implementation.
pub trait Brain: Send + Sync {
    /// `harness.narrate`. `None` is a punt: the caller runs the floor.
    ///
    /// An implementation that reaches a provider must do its own error
    /// handling and return `None` rather than panicking — `daemon.py` catches
    /// the exception and treats it as a punt, and there is no `catch_unwind`
    /// on this path.
    fn narrate(&self, request: &BrainRequest<'_>) -> Option<BrainDecision> {
        let _ = request;
        None
    }

    /// Whether this brain owns the speak-versus-silence decision for this
    /// request. The default preserves the original Heard behavior: a silent
    /// final or turn opener is rescued by the deterministic floor. A bounded
    /// attention policy may return true so its deliberate silence remains
    /// authoritative. This does not bypass mute, safety or routing policy.
    fn silence_is_authoritative(&self, request: &BrainRequest<'_>) -> bool {
        let _ = request;
        false
    }

    /// `harness.is_enabled(cfg)`. When false, `_handle_event` skips the model
    /// branch entirely and the event is dropped — NOT floored.
    fn is_enabled(&self) -> bool {
        true
    }
}

/// The no-LLM brain: punts on everything, so every prose event takes the
/// floor. The only implementation in this lane.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoBrain;

impl Brain for NoBrain {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_impl_punts() {
        let request = BrainRequest {
            kind: "final",
            tag: "final_short",
            neutral: "done",
            session_id: "s1",
            cwd: "",
            project: "",
            is_opener: false,
            last_prompt: "",
        };
        assert!(NoBrain.narrate(&request).is_none());
        assert!(!NoBrain.silence_is_authoritative(&request));
        assert!(NoBrain.is_enabled());
    }
}
