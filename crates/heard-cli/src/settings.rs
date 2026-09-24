//! The settings the CLI changes, their vocabularies, and how each is
//! written. Shared by the subcommands and the console.
//!
//! Writing a value = `heard-config` save under its lock, then a `reload`
//! frame if the daemon is up. A down daemon is fine: it reads config at start.
//!
//! Vocabularies come from `heard-config` and the Python reference
//! (`engine/heard/cli.py::_validate`, `cadence.py`, `volume.py`,
//! `engine_api.py`), not from memory:
//!
//! * `mode` — `copilot | companion | focus` (`cadence.PRESET_NAMES`), stored
//!   lowercase. `custom` is read-only (the two primitives match no preset).
//!   Picking a preset writes the preset AND both primitives it stands for
//!   (`send_mode`, `narration_volume`) plus the two legacy knobs the volume
//!   stop implies (`narrate_routine`, `verbosity`), exactly as
//!   `engine_api.py` does, so a stale primitive can never override the pick.
//! * `persona` — the four bundled personas, `raw`, or a user persona file in
//!   `<config_dir>/personas/`.
//! * `kokoro_voice` — one of the 54 ids in `voices-v1.0.bin`.
//! * `speed` — a float in 0.5–2.0 (`cli.py::_validate`).
//! * pause = the daemon's `mute` command, which persists `muted`; mute (all)
//!   = `audio_off`; mute one session = `mute_session` (in-daemon only).

use std::fs;
use std::path::Path;

use heard_config::{legacy, Config, Paths};
use serde_json::{json, Map, Value};

use crate::client::Daemon;
use crate::paths;
use crate::ui::{CliError, CliResult};
use crate::voices;

/// Everything a command needs: paths, the config module, the daemon handle.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// Resolved heard-cli paths.
    pub paths: Paths,
    /// `heard-config` bound to them.
    pub config: Config,
    /// The daemon socket.
    pub daemon: Daemon,
}

impl Ctx {
    /// Resolve from the environment.
    pub fn from_env() -> CliResult<Self> {
        Ok(Self::new(paths::resolve()?))
    }

    /// Bind to explicit paths. Registers the CLI edition's config layer
    /// first, so every load and save sees its keys.
    pub fn new(paths: Paths) -> Self {
        crate::edition::register();
        Ctx {
            config: Config::new(paths.clone()),
            daemon: Daemon::new(paths.socket_path.clone()),
            paths,
        }
    }

    /// The merged global config (no project layer).
    pub fn load(&self) -> CliResult<Map<String, Value>> {
        Ok(self.config.load(None)?)
    }

    /// Tell the daemon to re-read config; describe what happened.
    pub fn reload_note(&self) -> &'static str {
        if self.daemon.reload() {
            "daemon reloaded"
        } else {
            "daemon not running; applies when it starts"
        }
    }
}

// ── mode ────────────────────────────────────────────────────────────────

/// A listening-mode preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// At the screen: turn results, normal detail.
    Copilot,
    /// Away from it: everything, including the agent's prose.
    Companion,
    /// Heads-down: only failures, questions and approvals.
    Focus,
}

impl Mode {
    /// All presets, in picker order.
    pub const ALL: [Mode; 3] = [Mode::Copilot, Mode::Companion, Mode::Focus];

    /// The stored value.
    pub fn key(self) -> &'static str {
        match self {
            Mode::Copilot => "copilot",
            Mode::Companion => "companion",
            Mode::Focus => "focus",
        }
    }

    /// How it is shown.
    pub fn display(self) -> &'static str {
        match self {
            Mode::Copilot => "co-pilot",
            Mode::Companion => "companion",
            Mode::Focus => "focus",
        }
    }

    /// One line for pickers and help.
    pub fn describe(self) -> &'static str {
        match self {
            Mode::Copilot => "at the screen: turn results and anything that needs you",
            Mode::Companion => "away from it: the agent's prose and steps too",
            Mode::Focus => "heads-down: only failures, questions and approvals",
        }
    }

    /// Accepts the stored names plus the spellings people type.
    pub fn parse(s: &str) -> Option<Mode> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace(['_', ' '], "-")
            .as_str()
        {
            "copilot" | "co-pilot" | "pilot" => Some(Mode::Copilot),
            "companion" => Some(Mode::Companion),
            "focus" | "focused" => Some(Mode::Focus),
            _ => None,
        }
    }

    /// `(send_mode, narration_volume)` — `cadence.PRESETS`.
    fn primitives(self) -> (&'static str, i64) {
        match self {
            Mode::Copilot => ("prefill", 2),
            Mode::Companion => ("auto", 3),
            Mode::Focus => ("prefill", 0),
        }
    }

    /// The full preset map `engine_api.py` writes for a mode pick.
    pub fn preset(self) -> Map<String, Value> {
        let (send, stop) = self.primitives();
        let (routine, verbosity) = volume_derived(stop);
        let mut m = Map::new();
        m.insert("send_mode".into(), json!(send));
        m.insert("narration_volume".into(), json!(stop));
        m.insert("narrate_routine".into(), json!(routine));
        m.insert("verbosity".into(), json!(verbosity));
        m.insert("mode".into(), json!(self.key()));
        m
    }
}

/// `volume.derived_settings(stop)` — `(narrate_routine, verbosity)`.
fn volume_derived(stop: i64) -> (bool, &'static str) {
    match stop {
        0 | 1 => (false, "quiet"),
        3 => (true, "verbose"),
        _ => (false, "normal"),
    }
}

/// `cadence.preset(cfg)`: which preset the primitives equal, or `custom`.
/// Returned as the display name.
pub fn current_mode(cfg: &Map<String, Value>) -> String {
    let send = cfg
        .get("send_mode")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| s == "auto" || s == "prefill")
        .unwrap_or_else(|| {
            if legacy::legacy_mode(cfg) == "companion" {
                "auto".into()
            } else {
                "prefill".into()
            }
        });
    let stop = cfg
        .get("narration_volume")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .filter(|i| (0..=3).contains(i))
        .unwrap_or_else(|| match legacy::legacy_mode(cfg).as_str() {
            "focus" => 0,
            "companion" => 3,
            _ => {
                let routine = matches!(cfg.get("narrate_routine"), Some(Value::Bool(true)));
                let verb = cfg
                    .get("verbosity")
                    .and_then(Value::as_str)
                    .unwrap_or("normal")
                    .to_ascii_lowercase();
                if routine {
                    3
                } else if verb == "quiet" || verb == "brief" {
                    1
                } else {
                    2
                }
            }
        });
    for m in Mode::ALL {
        let (s, v) = m.primitives();
        if s == send && v == stop {
            return m.display().to_string();
        }
    }
    "custom".to_string()
}

/// Parse a mode argument or explain the choices.
pub fn parse_mode(s: &str) -> CliResult<Mode> {
    Mode::parse(s).ok_or_else(|| {
        CliError::usage(
            format!("unknown mode {s:?}"),
            "use one of: copilot (co-pilot), companion, focus",
        )
    })
}

/// Write a mode.
pub fn set_mode(ctx: &Ctx, m: Mode) -> CliResult<String> {
    ctx.config.apply_preset(&m.preset())?;
    Ok(format!("mode: {}  ({})", m.display(), ctx.reload_note()))
}

// ── voice ───────────────────────────────────────────────────────────────

/// The configured Kokoro voice, the user's own pick or the default (see
/// [`effective_voice`] for what the daemon actually speaks with).
pub fn current_voice(cfg: &Map<String, Value>) -> String {
    cfg.get("kokoro_voice")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(voices::DEFAULT_VOICE)
        .to_string()
}

/// The voice the daemon speaks with: the user's pick, else the persona's,
/// else `bm_george` ([`crate::persona`]).
pub fn effective_voice(p: &Paths, cfg: &Map<String, Value>) -> String {
    crate::persona::kokoro_voice(cfg, &paths::personas_dir(p))
}

/// Validate a voice id.
pub fn parse_voice(s: &str) -> CliResult<&'static str> {
    let id = s.trim().to_ascii_lowercase();
    voices::KOKORO_VOICES
        .iter()
        .copied()
        .find(|v| *v == id)
        .ok_or_else(|| {
            CliError::usage(
                format!("unknown voice {s:?}"),
                "run `heard voice --list` to see the 54 voices (e.g. bm_george, af_heart)",
            )
        })
}

/// Write a voice.
pub fn set_voice(ctx: &Ctx, id: &str) -> CliResult<String> {
    ctx.config.set_value("kokoro_voice", json!(id))?;
    Ok(format!(
        "voice: {id} — {}  ({})",
        voices::label(id),
        ctx.reload_note()
    ))
}

/// Forget the user's pick, so the persona's voice applies again
/// (`heard config set kokoro_voice ""`).
pub fn clear_voice(ctx: &Ctx) -> CliResult<String> {
    ctx.config.set_value("kokoro_voice", json!(""))?;
    let cfg = ctx.load()?;
    Ok(format!(
        "voice: {} (the persona's)  ({})",
        effective_voice(&ctx.paths, &cfg),
        ctx.reload_note()
    ))
}

/// Ask the daemon to speak a short sample in the current voice.
pub fn preview_voice(ctx: &Ctx, id: &str) -> CliResult<String> {
    if !ctx.daemon.is_up() {
        return Err(CliError::failure(
            "no spoken preview: the Heard daemon is not running",
            "start it with `heard start`, then run `heard voice --preview`",
        ));
    }
    let name = voices::label(id);
    let name = name.split(" · ").next().unwrap_or(id);
    ctx.daemon
        .speak(&format!("Hello, I'm {name}. This is how Heard will sound."))?;
    Ok(format!("previewing {id}…"))
}

// ── persona ─────────────────────────────────────────────────────────────

/// Bundled personas plus `raw` (`cli.py::_validate` accepts `raw`).
pub const BUNDLED_PERSONAS: [&str; 5] = ["jarvis", "aria", "friday", "atlas", "raw"];

/// One line each, from the persona files' opening sentences.
pub fn persona_describe(name: &str) -> &'static str {
    match name {
        "jarvis" => "impeccable British butler, quiet wit (bm_george)",
        "aria" => "senior pair-programmer, bottom-line first (af_nova)",
        "friday" => "sharp, breezy right-hand, three steps ahead (af_bella)",
        "atlas" => "cinematic narrator, movie-trailer cadence (bm_lewis)",
        "raw" => "no persona: the plain narration lines",
        _ => "your persona file",
    }
}

/// Every persona name that resolves: bundled, raw, then user files.
pub fn persona_names(p: &Paths) -> Vec<String> {
    let mut v: Vec<String> = BUNDLED_PERSONAS.iter().map(|s| s.to_string()).collect();
    if let Ok(rd) = fs::read_dir(paths::personas_dir(p)) {
        let mut user: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .filter(|s| !v.contains(s))
            .collect();
        user.sort();
        v.extend(user);
    }
    v
}

/// The active persona name.
pub fn current_persona(cfg: &Map<String, Value>) -> String {
    cfg.get("persona")
        .and_then(Value::as_str)
        .unwrap_or("raw")
        .to_string()
}

/// Set a persona by name, or install a persona `.md` file and select it.
pub fn set_persona(ctx: &Ctx, arg: &str) -> CliResult<String> {
    let path = Path::new(arg);
    let name = if arg.ends_with(".md") || path.is_file() {
        install_persona_file(ctx, path)?
    } else {
        let n = arg.trim().to_ascii_lowercase();
        if !persona_names(&ctx.paths).contains(&n) {
            return Err(CliError::usage(
                format!("unknown persona {arg:?}"),
                format!(
                    "use one of: {} — or pass a persona .md file",
                    persona_names(&ctx.paths).join(", ")
                ),
            ));
        }
        n
    };
    // `load()` derives persona from `voice` when voice names a persona, so a
    // stale `voice: aria` would override this pick. Keep the two in step.
    let mut preset = Map::new();
    preset.insert("persona".into(), json!(name));
    let cfg = ctx.load()?;
    let voice = cfg
        .get("voice")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if legacy::PERSONA_VOICES.contains(&voice.as_str()) && voice != name {
        let v = if legacy::PERSONA_VOICES.contains(&name.as_str()) {
            name.clone()
        } else {
            "george".to_string()
        };
        preset.insert("voice".into(), json!(v));
    }
    ctx.config.apply_preset(&preset)?;
    Ok(format!("persona: {name}  ({})", ctx.reload_note()))
}

fn install_persona_file(ctx: &Ctx, src: &Path) -> CliResult<String> {
    let text = fs::read_to_string(src).map_err(|e| {
        CliError::failure(
            format!("cannot read persona file {}: {e}", src.display()),
            "pass the path of a readable persona .md file",
        )
    })?;
    if !text.trim_start().starts_with("---") {
        return Err(CliError::usage(
            format!("{} has no front matter", src.display()),
            "a persona file starts with a `---` block (name, kokoro_voice, speed …); copy a bundled one as a template",
        ));
    }
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .filter(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
        .ok_or_else(|| {
            CliError::usage(
                format!("persona file name {:?} is not usable", src.display()),
                "name the file like `my-narrator.md` (letters, digits, - and _)",
            )
        })?;
    if BUNDLED_PERSONAS.contains(&stem.as_str()) {
        return Err(CliError::usage(
            format!("{stem} is a bundled persona name"),
            "rename your file so it does not shadow a bundled persona",
        ));
    }
    let dir = paths::personas_dir(&ctx.paths);
    fs::create_dir_all(&dir).map_err(|e| io_fail(&dir, e))?;
    let dest = dir.join(format!("{stem}.md"));
    fs::write(&dest, text).map_err(|e| io_fail(&dest, e))?;
    Ok(stem)
}

// ── speed ───────────────────────────────────────────────────────────────

/// The speaking rate.
pub fn current_speed(cfg: &Map<String, Value>) -> f64 {
    cfg.get("speed").and_then(Value::as_f64).unwrap_or(1.0)
}

/// Parse 0.5–2.0 (`1.2`, `1.2x`, `1.2×`).
pub fn parse_speed(s: &str) -> CliResult<f64> {
    let t = s.trim().trim_end_matches(['x', 'X', '×']);
    let f: f64 = t.parse().map_err(|_| {
        CliError::usage(
            format!("speed must be a number; got {s:?}"),
            "use a value from 0.5 to 2.0, e.g. `heard speed 1.1`",
        )
    })?;
    if !(0.5..=2.0).contains(&f) || !f.is_finite() {
        return Err(CliError::usage(
            format!("speed out of range: {f}"),
            "use a value from 0.5 to 2.0",
        ));
    }
    Ok(f)
}

/// Write a speed.
pub fn set_speed(ctx: &Ctx, f: f64) -> CliResult<String> {
    ctx.config.set_value("speed", json!(f))?;
    Ok(format!("speed: {}  ({})", fmt_speed(f), ctx.reload_note()))
}

/// `1.05×`.
pub fn fmt_speed(f: f64) -> String {
    let s = format!("{f:.2}");
    let s = s.trim_end_matches('0');
    let s = if s.ends_with('.') {
        format!("{s}0")
    } else {
        s.to_string()
    };
    format!("{s}×")
}

// ── alerts ──────────────────────────────────────────────────────────────

/// How needs-you moments (an approval or permission, a question, a failure,
/// a turn that ends waiting on you) reach you. Routine narration follows the
/// mode whatever this says.
///
/// | value | needs-you line spoken | macOS notification |
/// |---|---|---|
/// | `both` (default) | yes | yes |
/// | `voice` | yes | no |
/// | `notify` | no | yes |
/// | `off` | no | no |
///
/// Stored as `alerts` in `config.yaml`, a key the CLI edition's config layer
/// declares ([`crate::edition`]). The daemon's `notify` extension and its
/// speech gate read it from the config snapshot ([`crate::notify`]), so a
/// change applies on the `reload` every write sends.
///
/// It is NOT mirrored into the core's `notify.errors` / `notify.blocked` /
/// `notify.completions`: despite the name, those are the core's "Speak up
/// on" switches, and turning them off silences failures, questions and
/// every final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alerts {
    /// Spoken only.
    Voice,
    /// macOS notification only.
    Notify,
    /// Spoken and a notification.
    Both,
    /// Neither.
    Off,
}

impl Alerts {
    /// Picker order.
    pub const ALL: [Alerts; 4] = [Alerts::Both, Alerts::Voice, Alerts::Notify, Alerts::Off];

    /// Stored value.
    pub fn key(self) -> &'static str {
        match self {
            Alerts::Voice => "voice",
            Alerts::Notify => "notify",
            Alerts::Both => "both",
            Alerts::Off => "off",
        }
    }

    /// Header form.
    pub fn display(self) -> &'static str {
        match self {
            Alerts::Voice => "voice",
            Alerts::Notify => "notify",
            Alerts::Both => "voice+notify",
            Alerts::Off => "off",
        }
    }

    /// One line.
    pub fn describe(self) -> &'static str {
        match self {
            Alerts::Voice => "say it out loud, no notification",
            Alerts::Notify => "post a macOS notification instead of saying it",
            Alerts::Both => "say it and post a notification",
            Alerts::Off => "neither: needs-you lines are not spoken or notified",
        }
    }

    /// Parse.
    pub fn parse(s: &str) -> Option<Alerts> {
        match s.trim().to_ascii_lowercase().as_str() {
            "voice" | "speak" | "spoken" => Some(Alerts::Voice),
            "notify" | "notification" | "notifications" => Some(Alerts::Notify),
            "both" | "voice+notify" | "all" | "on" => Some(Alerts::Both),
            "off" | "none" => Some(Alerts::Off),
            _ => None,
        }
    }

    /// Posts a notification.
    pub fn notifies(self) -> bool {
        matches!(self, Alerts::Notify | Alerts::Both)
    }

    /// Speaks the needs-you line.
    pub fn speaks(self) -> bool {
        matches!(self, Alerts::Voice | Alerts::Both)
    }
}

impl Alerts {
    /// A config value: a known string, or a YAML 1.1 boolean — a hand-written
    /// `alerts: off` loads as `false` (and `on` as `true`). Anything else is
    /// the default.
    pub fn from_value(v: Option<&Value>) -> Alerts {
        match v {
            Some(Value::String(s)) => Alerts::parse(s).unwrap_or(Alerts::Both),
            Some(Value::Bool(false)) => Alerts::Off,
            _ => Alerts::Both,
        }
    }
}

/// `alerts` out of a loaded config (default: both).
pub fn alerts_of(cfg: &Map<String, Value>) -> Alerts {
    Alerts::from_value(cfg.get("alerts"))
}

/// The alerts setting (default: both).
pub fn current_alerts(p: &Paths) -> Alerts {
    crate::edition::register();
    Config::new(p.clone())
        .load(None)
        .map(|c| alerts_of(&c))
        .unwrap_or(Alerts::Both)
}

/// Parse an alerts argument.
pub fn parse_alerts(s: &str) -> CliResult<Alerts> {
    Alerts::parse(s).ok_or_else(|| {
        CliError::usage(
            format!("unknown alerts value {s:?}"),
            "use one of: voice, notify, both, off",
        )
    })
}

/// Write alerts.
pub fn set_alerts(ctx: &Ctx, a: Alerts) -> CliResult<String> {
    ctx.config.set_value("alerts", json!(a.key()))?;
    Ok(format!("alerts: {}  ({})", a.display(), ctx.reload_note()))
}

// ── pause / mute ────────────────────────────────────────────────────────

/// `muted` — the daemon's persisted pause.
pub fn is_paused(cfg: &Map<String, Value>) -> bool {
    matches!(cfg.get("muted"), Some(Value::Bool(true)))
}

/// `audio_off` — speech silenced, everything else running.
pub fn is_audio_off(cfg: &Map<String, Value>) -> bool {
    matches!(cfg.get("audio_off"), Some(Value::Bool(true)))
}

/// Pause (true) or resume (false) narration. With the daemon up this is its
/// `mute`/`unmute` command, which also cancels speech and persists `muted`;
/// with it down the flag is written directly.
pub fn set_paused(ctx: &Ctx, paused: bool) -> CliResult<String> {
    let sent = if paused {
        ctx.daemon.mute()
    } else {
        ctx.daemon.unmute()
    };
    let note = match sent {
        Ok(()) => "daemon told",
        Err(_) => {
            ctx.config.set_value("muted", json!(paused))?;
            "daemon not running; applies when it starts"
        }
    };
    Ok(if paused {
        format!("paused — Heard is quiet until `heard resume`  ({note})")
    } else {
        format!("resumed — Heard is narrating again  ({note})")
    })
}

/// Mute or unmute all speech (`audio_off`), or one session.
pub fn set_muted(ctx: &Ctx, mute: bool, session: Option<&str>) -> CliResult<String> {
    if let Some(sid) = session {
        let sid = sid.trim();
        if sid.is_empty() {
            return Err(CliError::usage(
                "empty session id",
                "pass a session id from `heard agents`",
            ));
        }
        let sid = resolve_session(ctx, sid);
        let reply = ctx.daemon.session_mute(&sid, mute)?;
        if reply.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(CliError::failure(
                format!("the daemon refused: {err}"),
                "check the id with `heard agents`",
            ));
        }
        return Ok(if mute {
            format!("muted session {sid} — other agents keep narrating")
        } else {
            format!("unmuted session {sid}")
        });
    }
    ctx.config.set_value("audio_off", json!(mute))?;
    let note = ctx.reload_note();
    Ok(if mute {
        format!("muted — no speech until `heard unmute`  ({note})")
    } else {
        format!("unmuted — speech is back on  ({note})")
    })
}

/// A short id prefix or repo name → the full session id, via `status`.
fn resolve_session(ctx: &Ctx, arg: &str) -> String {
    let Some(st) = ctx.daemon.status() else {
        return arg.to_string();
    };
    let agents = crate::client::active_sessions(&st);
    let hits: Vec<_> = agents
        .iter()
        .filter(|a| a.session_id.starts_with(arg) || a.repo_name == arg)
        .collect();
    if hits.len() == 1 {
        hits[0].session_id.clone()
    } else {
        arg.to_string()
    }
}

// ── snapshot ────────────────────────────────────────────────────────────

/// What the header and `heard status` show.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Snapshot {
    /// Display mode (`co-pilot`, `companion`, `focus`, `custom`).
    pub mode: String,
    /// Persona name.
    pub persona: String,
    /// Kokoro voice id.
    pub voice: String,
    /// Speaking rate.
    pub speed: f64,
    /// Alerts key.
    pub alerts: String,
    /// Narration paused (`muted`).
    pub paused: bool,
    /// Speech silenced (`audio_off`).
    pub audio_off: bool,
}

/// Read the snapshot.
pub fn snapshot(ctx: &Ctx) -> CliResult<Snapshot> {
    let cfg = ctx.load()?;
    Ok(Snapshot {
        mode: current_mode(&cfg),
        persona: current_persona(&cfg),
        voice: effective_voice(&ctx.paths, &cfg),
        speed: current_speed(&cfg),
        alerts: alerts_of(&cfg).key().to_string(),
        paused: is_paused(&cfg),
        audio_off: is_audio_off(&cfg),
    })
}

/// A filesystem failure, with the path.
pub fn io_fail(path: &Path, e: std::io::Error) -> CliError {
    CliError::failure(
        format!("{}: {e}", path.display()),
        "check the path exists and you can write to it",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_aliases() {
        assert_eq!(Mode::parse("co-pilot"), Some(Mode::Copilot));
        assert_eq!(Mode::parse("Co_Pilot"), Some(Mode::Copilot));
        assert_eq!(Mode::parse("FOCUS"), Some(Mode::Focus));
        assert_eq!(Mode::parse("custom"), None);
    }

    #[test]
    fn presets_round_trip_through_current_mode() {
        for m in Mode::ALL {
            let mut cfg = heard_config::defaults().clone();
            cfg.extend(m.preset());
            assert_eq!(current_mode(&cfg), m.display());
        }
        // Defaults (send_mode "", narration_volume -1, mode copilot) → co-pilot.
        assert_eq!(current_mode(heard_config::defaults()), "co-pilot");
        // A stale primitive that matches no preset → custom.
        let mut cfg = heard_config::defaults().clone();
        cfg.insert("send_mode".into(), json!("auto"));
        cfg.insert("narration_volume".into(), json!(0));
        assert_eq!(current_mode(&cfg), "custom");
    }

    #[test]
    fn speed_parsing() {
        assert_eq!(parse_speed("1.2x").unwrap(), 1.2);
        assert_eq!(parse_speed("0.5").unwrap(), 0.5);
        assert_eq!(parse_speed("2.5").unwrap_err().code, 2);
        assert_eq!(parse_speed("fast").unwrap_err().code, 2);
        assert_eq!(fmt_speed(1.05), "1.05×");
        assert_eq!(fmt_speed(1.0), "1.0×");
    }
}
