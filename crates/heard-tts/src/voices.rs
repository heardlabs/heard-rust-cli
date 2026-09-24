//! Voice-id resolution and speed clamping for ElevenLabs-voiced backends.
//!
//! `elevenlabs.py` carries `_VOICE_ID_RE`, `_VOICE_ALIASES`,
//! `_resolve_voice_id` and `_clamp_speed`. They are public here so any other
//! backend that forwards to ElevenLabs voices (a registered one, see
//! [`crate::select`]) resolves the same names to the same ids instead of
//! carrying a second table that can drift.
//! [`tests::the_rust_table_matches_the_python`] pins the table against the
//! Python source when that source is checked out alongside.

use once_cell::sync::Lazy;
use regex::Regex;

/// `JBFqnCBsd6RMkjVDRZzb` — George, male British. `DEFAULT_VOICE_ID` in the
/// Python.
pub const DEFAULT_VOICE_ID: &str = "JBFqnCBsd6RMkjVDRZzb";

/// ElevenLabs voice ids are 20 alphanumeric characters. Anything else gets
/// mapped through [`ALIASES`] or defaulted. `_VOICE_ID_RE` in the Python.
static VOICE_ID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z0-9]{20}$").expect("static regex"));

/// Friendly aliases, so config can carry semantic names instead of opaque ids.
///
/// The first four are the product personas behind the "Narration voice"
/// setting; the Python names `heard-website/scripts/generate-audio.mjs` as the
/// source of truth for their ids.
pub const ALIASES: &[(&str, &str)] = &[
    ("jarvis", "Fahco4VZzobUeiPqni1S"),    // formal British butler
    ("aria", "21m00Tcm4TlvDq8ikWAM"),      // Rachel — calm pair-programmer
    ("friday", "g6xIsTj2HwM6VR4iXFCw"),    // bright & breezy
    ("atlas", "sBObXMSU6qeIkKldMgv0"),     // cinematic narrator
    ("george", "JBFqnCBsd6RMkjVDRZzb"),    // male British (Jarvis-style)
    ("rachel", "21m00Tcm4TlvDq8ikWAM"),    // female US
    ("adam", "pNInz6obpgDQGcFmaJgB"),      // male US
    ("charlotte", "XB0fDUnXU5powFXDhCwa"), // female English
    ("daniel", "onwK4e9ZLuTAKqWW03F9"),    // male British
    ("lily", "pFZP5JQG7iQjIQuC4Bku"),      // female British
    ("bill", "pqHfZKP75CvOlQylNhV4"),      // male American (older)
];

/// Resolve a config `voice` value to an ElevenLabs voice id.
///
/// `_resolve_voice_id` in `elevenlabs.py`: empty → default, a well-formed
/// id → itself, a known alias (case-insensitively) → its id, anything else →
/// default. Note the last clause is a silent fallback, not an error — a typo in
/// `voice` gets you George, which is exactly the bug the persona ids were added
/// to fix. Ported as-is; changing it is a product decision, not a port one.
#[must_use]
pub fn resolve_voice_id(voice: &str) -> &str {
    let v = voice.trim();
    if v.is_empty() {
        return DEFAULT_VOICE_ID;
    }
    if VOICE_ID_RE.is_match(v) {
        return v;
    }
    let lower = v.to_lowercase();
    ALIASES
        .iter()
        .find(|(name, _)| *name == lower)
        .map_or(DEFAULT_VOICE_ID, |(_, id)| *id)
}

/// Clamp to ElevenLabs' `voice_settings.speed` range of `[0.7, 1.2]`.
///
/// `_clamp_speed` in `elevenlabs.py`: out-of-range silently rounds to the
/// bound rather than erroring, so existing config written for Kokoro's wider
/// range keeps working. A non-finite value takes the Python's
/// `except: return 1.0` path — `float("nan")` does not raise there, but it
/// would propagate into the JSON body as `NaN`, which is not valid JSON, so
/// treating it as "unparseable" is the faithful outcome rather than a
/// divergence.
#[must_use]
pub fn clamp_speed(speed: f64) -> f64 {
    if !speed.is_finite() {
        return 1.0;
    }
    speed.clamp(0.7, 1.2)
}

/// Human-friendly voice names, sorted. `list_voices` in `elevenlabs.py` —
/// no network call just to list voices.
#[must_use]
pub fn alias_names() -> Vec<String> {
    let mut names: Vec<String> = ALIASES.iter().map(|(n, _)| (*n).to_string()).collect();
    names.sort_unstable();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_whitespace_get_the_default() {
        assert_eq!(resolve_voice_id(""), DEFAULT_VOICE_ID);
        assert_eq!(resolve_voice_id("   "), DEFAULT_VOICE_ID);
    }

    #[test]
    fn a_well_formed_id_passes_through_untouched() {
        assert_eq!(
            resolve_voice_id("Fahco4VZzobUeiPqni1S"),
            "Fahco4VZzobUeiPqni1S"
        );
        // Trimmed first, exactly as the Python strips before matching.
        assert_eq!(
            resolve_voice_id("  Fahco4VZzobUeiPqni1S  "),
            "Fahco4VZzobUeiPqni1S"
        );
    }

    #[test]
    fn aliases_resolve_case_insensitively() {
        assert_eq!(resolve_voice_id("jarvis"), "Fahco4VZzobUeiPqni1S");
        assert_eq!(resolve_voice_id("JARVIS"), "Fahco4VZzobUeiPqni1S");
        assert_eq!(resolve_voice_id("Aria"), "21m00Tcm4TlvDq8ikWAM");
        // aria and rachel are the same voice.
        assert_eq!(resolve_voice_id("aria"), resolve_voice_id("rachel"));
    }

    #[test]
    fn an_unknown_name_falls_back_to_george() {
        assert_eq!(resolve_voice_id("nonesuch"), DEFAULT_VOICE_ID);
        // 19 and 21 characters both fail the id shape.
        assert_eq!(resolve_voice_id("Fahco4VZzobUeiPqni1"), DEFAULT_VOICE_ID);
        assert_eq!(resolve_voice_id("Fahco4VZzobUeiPqni1SS"), DEFAULT_VOICE_ID);
        // 20 characters but not alphanumeric.
        assert_eq!(resolve_voice_id("Fahco4VZzobUeiPqni1-"), DEFAULT_VOICE_ID);
    }

    #[test]
    fn speed_clamps_to_the_elevenlabs_range() {
        // Exact equality, not an epsilon: these values go onto the wire as
        // JSON, and 1.2000000476837158 (what an f32 round-trip produces) is a
        // different request body from 1.2. That is the whole reason `speed` is
        // f64 through this crate — Python's float is f64, and the wire diff
        // against a captured request has to be empty.
        assert_eq!(clamp_speed(1.0), 1.0);
        assert_eq!(clamp_speed(0.1), 0.7);
        assert_eq!(clamp_speed(1.7), 1.2);
        assert_eq!(clamp_speed(0.7), 0.7);
        assert_eq!(clamp_speed(1.2), 1.2);
        assert_eq!(clamp_speed(0.9), 0.9);
        assert_eq!(clamp_speed(f64::NAN), 1.0);
        assert_eq!(clamp_speed(f64::INFINITY), 1.0);
        assert_eq!(clamp_speed(f64::NEG_INFINITY), 1.0);
    }

    #[test]
    fn alias_names_are_sorted_and_complete() {
        let names = alias_names();
        assert_eq!(names.len(), ALIASES.len());
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert!(names.contains(&"jarvis".to_string()));
        assert!(names.contains(&"bill".to_string()));
    }

    /// The Rust alias table is `elevenlabs.py`'s, parsed out of the Python
    /// source rather than asserted from memory. Skips itself when the Python
    /// engine is not checked out alongside.
    #[test]
    fn the_rust_table_matches_the_python() {
        let engine = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .map(|r| r.join("engine/heard/tts"));
        let Some(engine) = engine else { return };
        if !engine.join("elevenlabs.py").exists() {
            return; // Rust-only checkout; nothing to pin against.
        }

        // Pull `"name": "id",` pairs out of the `_VOICE_ALIASES` block.
        let block = |src: &str| -> Vec<(String, String)> {
            let start = src.find("_VOICE_ALIASES = {").expect("alias table");
            let rest = &src[start..];
            let end = rest.find("\n}").expect("table end");
            let re = Regex::new(r#""([a-z]+)":\s*"([A-Za-z0-9]{20})""#).unwrap();
            re.captures_iter(&rest[..end])
                .map(|c| (c[1].to_string(), c[2].to_string()))
                .collect()
        };

        let el = block(&std::fs::read_to_string(engine.join("elevenlabs.py")).unwrap());

        let ours: Vec<(String, String)> = ALIASES
            .iter()
            .map(|(n, i)| ((*n).to_string(), (*i).to_string()))
            .collect();
        assert_eq!(ours, el, "the Rust alias table has drifted from Python");
    }
}
