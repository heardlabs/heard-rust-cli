//! Personas as the daemon reads them: the four bundled ones plus the user's
//! `<config_dir>/personas/*.md`, reduced to the fields the CLI edition uses
//! (form of address, and the voices), and the voice precedence.
//!
//! # Voice precedence (Kokoro)
//!
//! 1. the user's own pick — a non-empty `kokoro_voice` in the config
//!    (`heard voice`, `heard config set kokoro_voice …`);
//! 2. the active persona's `kokoro_voice` front-matter;
//! 3. `bm_george`.
//!
//! This deliberately differs from the Python (`persona.kokoro_voice or
//! cfg["kokoro_voice"]`, persona first): in the CLI edition the voice is a
//! first-class choice (`heard voice`), and a persona that silently overrode it
//! would make that command look broken. The edition's config layer sets the
//! `kokoro_voice` default to `""`, so "the user chose" is simply "non-empty"
//! (see [`crate::edition`]).
//!
//! The ElevenLabs voice keeps the Python rule: the persona's `voice` if it has
//! one, else `cfg["voice"]`.

use std::path::Path;

use heard_daemon::{PersonaInfo, PersonaSource};
use serde_json::{Map, Value};

/// Kokoro's default voice when neither the user nor the persona picks one.
pub const FALLBACK_KOKORO_VOICE: &str = "bm_george";

/// The persona fields this edition reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Persona {
    /// `name` (`raw` for no persona).
    pub name: String,
    /// `address` — the floor's form of address ("Sir"), `""` for none.
    pub address: String,
    /// `kokoro_voice`, `""` when the persona has none.
    pub kokoro_voice: String,
    /// `voice` (ElevenLabs id), `""` when none.
    pub voice: String,
}

/// The bundled personas' front matter (`engine/heard/personas/*.md`).
fn bundled(name: &str) -> Option<Persona> {
    let (address, kokoro, voice) = match name {
        "jarvis" => ("Sir", "bm_george", "Fahco4VZzobUeiPqni1S"),
        "aria" => ("", "af_nova", "rachel"),
        "friday" => ("", "af_bella", "g6xIsTj2HwM6VR4iXFCw"),
        "atlas" => ("", "bm_lewis", "sBObXMSU6qeIkKldMgv0"),
        _ => return None,
    };
    Some(Persona {
        name: name.into(),
        address: address.into(),
        kokoro_voice: kokoro.into(),
        voice: voice.into(),
    })
}

fn raw() -> Persona {
    Persona {
        name: "raw".into(),
        ..Persona::default()
    }
}

/// `key: value` pairs of a `---` front-matter block, values unquoted.
pub fn front_matter(text: &str) -> Map<String, Value> {
    let mut out = Map::new();
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return out;
    }
    for line in lines {
        let line = line.trim_end();
        if line.trim() == "---" {
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() || k.starts_with('#') || line.starts_with([' ', '\t']) {
            continue;
        }
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(v);
        out.insert(k.to_string(), Value::String(v.to_string()));
    }
    out
}

/// `persona.load(name, config_dir)`: a user file first (it may shadow
/// nothing bundled — `heard persona` refuses that — but a hand-made one
/// could), then the bundled set, else `raw`.
pub fn load(name: &str, personas_dir: &Path) -> Persona {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() || name == "raw" {
        return raw();
    }
    let safe = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if safe {
        if let Ok(text) = std::fs::read_to_string(personas_dir.join(format!("{name}.md"))) {
            let fm = front_matter(&text);
            let s = |k: &str| {
                fm.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string()
            };
            let base = bundled(&name).unwrap_or_default();
            return Persona {
                name: name.clone(),
                address: fm
                    .get("address")
                    .map(|_| s("address"))
                    .unwrap_or(base.address),
                kokoro_voice: Some(s("kokoro_voice"))
                    .filter(|v| !v.is_empty())
                    .unwrap_or(base.kokoro_voice),
                voice: Some(s("voice"))
                    .filter(|v| !v.is_empty())
                    .unwrap_or(base.voice),
            };
        }
    }
    bundled(&name).unwrap_or_else(raw)
}

fn cfg_str<'a>(cfg: &'a Map<String, Value>, key: &str) -> &'a str {
    cfg.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
}

/// The active persona's name as routing reads it (`cfg.get("persona","raw")`).
pub fn active_name(cfg: &Map<String, Value>) -> String {
    match cfg.get("persona") {
        None => "raw".into(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => "raw".into(),
    }
}

/// The Kokoro voice to speak with (see the module doc for the order).
pub fn kokoro_voice(cfg: &Map<String, Value>, personas_dir: &Path) -> String {
    let user = cfg_str(cfg, "kokoro_voice");
    if !user.is_empty() {
        return user.to_string();
    }
    let p = load(&active_name(cfg), personas_dir);
    if !p.kokoro_voice.is_empty() {
        return p.kokoro_voice;
    }
    FALLBACK_KOKORO_VOICE.to_string()
}

/// The ElevenLabs voice: `persona.voice or cfg["voice"]`.
pub fn elevenlabs_voice(cfg: &Map<String, Value>, personas_dir: &Path) -> String {
    let p = load(&active_name(cfg), personas_dir);
    if !p.voice.is_empty() {
        return p.voice;
    }
    cfg_str(cfg, "voice").to_string()
}

/// The daemon's [`PersonaSource`]: user files, then bundled.
#[derive(Debug, Clone)]
pub struct CliPersonas {
    /// `<config_dir>/personas`.
    pub dir: std::path::PathBuf,
}

impl PersonaSource for CliPersonas {
    fn load(&self, name: &str) -> PersonaInfo {
        let p = load(name, &self.dir);
        PersonaInfo {
            name: p.name,
            address: p.address,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn the_users_voice_beats_the_personas() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(json!({"persona": "aria", "kokoro_voice": "bm_george"}));
        assert_eq!(kokoro_voice(&c, dir.path()), "bm_george");
    }

    #[test]
    fn the_personas_voice_applies_when_the_user_chose_none() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(json!({"persona": "aria", "kokoro_voice": ""}));
        assert_eq!(kokoro_voice(&c, dir.path()), "af_nova");
        let c = cfg(json!({"persona": "atlas"}));
        assert_eq!(kokoro_voice(&c, dir.path()), "bm_lewis");
        let c = cfg(json!({"persona": "raw", "kokoro_voice": "  "}));
        assert_eq!(kokoro_voice(&c, dir.path()), FALLBACK_KOKORO_VOICE);
    }

    #[test]
    fn a_user_persona_file_supplies_voice_and_address() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("narrator.md"),
            "---\nname: narrator\nkokoro_voice: af_sky\naddress: \"Captain\"\n---\nBody: not: front matter\n",
        )
        .unwrap();
        let c = cfg(json!({"persona": "narrator"}));
        assert_eq!(kokoro_voice(&c, dir.path()), "af_sky");
        let info = CliPersonas {
            dir: dir.path().to_path_buf(),
        }
        .load("narrator");
        assert_eq!(info.address, "Captain");
        // The user's pick still wins over the file.
        let c = cfg(json!({"persona": "narrator", "kokoro_voice": "am_adam"}));
        assert_eq!(kokoro_voice(&c, dir.path()), "am_adam");
    }

    #[test]
    fn bundled_personas_match_the_daemons_table() {
        let dir = tempfile::tempdir().unwrap();
        let src = CliPersonas {
            dir: dir.path().to_path_buf(),
        };
        for name in [
            "jarvis", "aria", "friday", "atlas", "raw", "nobody", "../etc",
        ] {
            assert_eq!(
                src.load(name),
                heard_daemon::BundledPersonas.load(name),
                "{name}"
            );
        }
    }

    #[test]
    fn elevenlabs_keeps_the_python_order() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(json!({"persona": "aria", "voice": "custom"}));
        assert_eq!(elevenlabs_voice(&c, dir.path()), "rachel");
        let c = cfg(json!({"persona": "raw", "voice": "custom"}));
        assert_eq!(elevenlabs_voice(&c, dir.path()), "custom");
    }
}
