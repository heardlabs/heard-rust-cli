//! The CLI edition's config layer: the few defaults that differ from the
//! core's because this edition has no app around it.
//!
//! Registered with [`heard_config::register_layer`] once per process, before
//! the first config load or save ([`register`] is idempotent, and
//! [`crate::settings::Ctx::new`] and the daemon both call it), so every
//! `heard` process — subcommand, console, or daemon — reads and writes the
//! same keys.
//!
//! | key | core default | CLI edition | why |
//! |---|---|---|---|
//! | `onboarded` | `false` | `true` | the first-run rule, below |
//! | `alerts` | — | `"both"` | how needs-you lines reach you (see [`crate::notify`]) |
//! | `kokoro_voice` | `"bm_george"` | `""` | "not chosen": the persona's voice applies (see [`crate::persona`]) |
//!
//! # The first-run rule
//!
//! The core daemon holds every line until `onboarded` is true
//! (`policy::first_run_held`). In the Heard app that flag belongs to the
//! app's onboarding window, which ends by setting it. **The CLI edition has
//! no onboarding window, so it treats an install as onboarded from the
//! start**: the layer's default is `true`. A user who never ran
//! `heard setup` but ran `heard install` (or let an agent's hook auto-start
//! the daemon) hears narration straight away. `heard setup` and
//! `heard install` also write `onboarded: true` explicitly, so the file
//! itself says so and any reader without this layer agrees.
//!
//! The rest of the first-run hold still applies: an explicit
//! `onboarded: false`, or a `first_run_generation` that was never completed,
//! still holds narration — those can only come from a hand edit or the Heard
//! app, and they mean what they say.
//!
//! # `kokoro_voice: ""`
//!
//! The strict save drops a value equal to its default, so with the core's
//! `bm_george` default a user who picks `bm_george` would have no record of
//! the pick, and a persona's voice would override it. With an empty default
//! every pick is written, and "empty" unambiguously means "the user has not
//! chosen a voice".

use std::sync::OnceLock;

use serde_json::{json, Map, Value};

/// The layer's keys.
pub fn layer() -> &'static Map<String, Value> {
    static LAYER: OnceLock<Map<String, Value>> = OnceLock::new();
    LAYER.get_or_init(|| {
        let mut m = Map::new();
        m.insert("onboarded".into(), json!(true));
        m.insert("alerts".into(), json!("both"));
        m.insert("kokoro_voice".into(), json!(""));
        m
    })
}

/// Register the layer (once per process; later calls do nothing).
pub fn register() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| heard_config::register_layer(layer()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layer_lifts_the_first_run_hold_and_declares_alerts() {
        register();
        register();
        let d = heard_config::all_defaults();
        assert_eq!(d.get("onboarded"), Some(&json!(true)));
        assert_eq!(d.get("alerts"), Some(&json!("both")));
        assert_eq!(d.get("kokoro_voice"), Some(&json!("")));
        // The core's defaults are untouched: the layer is edition-only.
        assert_eq!(
            heard_config::defaults().get("onboarded"),
            Some(&json!(false))
        );
        let merged: Map<String, Value> = (*d).clone();
        assert!(!heard_daemon::policy::first_run_held(&merged));
    }
}
