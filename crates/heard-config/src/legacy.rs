//! Reading an OLD `config.yaml`.
//!
//! The rule: legacy config values map to current ones at load time (e.g.
//! `verbosity: low/high` → `quiet/verbose`), and an existing `config.yaml`
//! never breaks without a migration path.
//!
//! The surprise when porting: **exactly one of those mappings lives in
//! `config.py` itself** — the `voice` → `persona` derivation at the bottom of
//! `load()`, which is implemented in [`crate::Config::load`] because it changes
//! what `load()` returns. Every other one is applied by the READER, in the
//! module that cares, on a dict `load()` has already handed back. They are
//! collected here because they are all "an old config.yaml must keep working",
//! and the port needs them in Rust; each carries the Python file it
//! came from so the two can be diffed.
//!
//! Not ported here, and named so nobody assumes they were:
//!
//! * `cadence.send_mode` / `cadence.volume_stop` / `cadence.preset`
//!   (`engine/heard/cadence.py`) derive the new `send_mode` + `narration_volume`
//!   dials from the legacy `mode` + `verbosity` + `narrate_routine` when the
//!   new keys were never set. They reach into `heard/volume.py`, which is
//!   narration, not config — they belong to `heard-narrate`.
//! * feature-local key fallbacks and `settings_modules.py`'s retained
//!   `frosty` id are feature-local compatibility, not config-file
//!   compatibility.

use serde_json::{Map, Value};

/// Verbosity profile names (`engine/heard/profile.py::_LEGACY`).
///
/// Pre-v0.4 installs wrote `low` / `high`; `profile.load()` normalises them so
/// an older `config.yaml` still resolves a bundled profile. `heard/tune.py`
/// carries a second, identical copy of the same table.
///
/// Note what this does NOT do: `config.load()` leaves `verbosity: low` as
/// `"low"` in the dict it returns. The normalisation happens when a profile is
/// looked up, which is why the golden corpus shows `low` surviving `load()`.
pub fn normalize_verbosity(name: Option<&str>) -> String {
    let n = name.unwrap_or("normal").trim().to_ascii_lowercase();
    let n = if n.is_empty() {
        "normal".to_string()
    } else {
        n
    };
    match n.as_str() {
        "low" => "quiet".to_string(),
        "high" => "verbose".to_string(),
        _ => n,
    }
}

/// Listening-mode presets, fail-closed (`engine/heard/cadence.py::legacy_mode`).
///
/// An unknown or missing `mode` must never upgrade itself into a mode that
/// changes where speech goes, so anything unrecognised reads as `copilot`.
/// `custom` is the one non-preset value that is preserved.
pub fn legacy_mode(cfg: &Map<String, Value>) -> String {
    let m = cfg
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match m.as_str() {
        "copilot" | "companion" | "focus" => m,
        "custom" => "custom".to_string(),
        _ => "copilot".to_string(),
    }
}

/// A settings-panel key, read dotted first and then under its legacy underscore
/// name (`engine/heard/daemon.py::_speakup_allows`,
/// `engine/heard/home_window.py`).
///
/// The Settings panel writes `notify.errors`; a config from before the panel
/// existed has `notify_errors`. The idiom in Python is literally
/// `cfg.get("notify.errors", cfg.get("notify_errors", True))`, so a dotted key
/// present with ANY value (including `false`) wins outright, and the default
/// only applies when neither name exists.
pub fn dotted_or_underscore<'a>(
    cfg: &'a Map<String, Value>,
    dotted: &str,
    default: &'a Value,
) -> &'a Value {
    if let Some(v) = cfg.get(dotted) {
        return v;
    }
    let underscored = dotted.replacen('.', "_", 1);
    cfg.get(&underscored).unwrap_or(default)
}

/// The four persona names a `voice` value may carry, which `load()` folds into
/// `persona` (`engine/heard/config.py::load`).
///
/// The "Narration voice" picker writes `voice`, but everything that SPEAKS
/// reads `persona`; deriving it in `load()` is what stops the picker setting a
/// voice while the speak path stays on the default Jarvis.
pub const PERSONA_VOICES: [&str; 4] = ["jarvis", "aria", "friday", "atlas"];
