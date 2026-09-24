//! `DEFAULTS` — the core (free edition) subset of `engine/heard/config.py`.
//!
//! Every key the core crates read, with its value and its comment from the
//! Python dict, in the same order. The comments are the spec: they record WHY
//! a default is what it is, and they port with the code or the knowledge is
//! lost.
//!
//! Keys that only an extended edition reads are not here. That edition adds
//! them with [`crate::register_layer`] before it loads or saves anything, so
//! the merged defaults it sees are the full Python dict again.
//! `fixtures/config/defaults.json` is what stops this subset drifting.

use serde_json::{json, Map, Value};
use std::sync::OnceLock;

/// The application name handed to `platformdirs` (`config.APP`) by the full
/// Heard app. [`crate::Paths::resolve`] uses it.
pub const APP: &str = "heard";

/// The application name the CLI edition resolves its paths under, so it never
/// shares a socket, config or history with the app. See
/// [`crate::Paths::resolve_for`].
pub const CLI_APP: &str = "heard-cli";

/// Per-project override file, walked up from an event's cwd
/// (`config.PROJECT_FILE`).
pub const PROJECT_FILE: &str = ".heard.yaml";

/// The core `config.DEFAULTS` — the lowest of the three config layers,
/// before any registered layer (see [`crate::all_defaults`]).
pub fn defaults() -> &'static Map<String, Value> {
    static DEFAULTS: OnceLock<Map<String, Value>> = OnceLock::new();
    DEFAULTS.get_or_init(|| {
        let mut m = Map::new();
        // ElevenLabs voice alias (see heard.tts.elevenlabs._VOICE_ALIASES) or
        // a 20-char ElevenLabs voice_id. Defaults to George — male British,
        // fits the Jarvis persona.
        m.insert("voice".into(), json!("george"));
        // Kokoro voice ID (54 baked-in voices, format <accent_gender>_<name>).
        // Used only when the active backend is Kokoro — the persona's
        // `kokoro_voice` frontmatter wins over this when set. ElevenLabs IDs
        // don't resolve under Kokoro and vice versa, so the two values are
        // carried independently.
        m.insert("kokoro_voice".into(), json!("bm_george"));
        m.insert("speed".into(), json!(1.05));
        m.insert("lang".into(), json!("en-us"));
        m.insert("skip_under_chars".into(), json!(30));
        m.insert("flush_delay_ms".into(), json!(800));
        m.insert("narrate_tools".into(), json!(true));
        m.insert("narrate_tool_results".into(), json!(true));
        // Default to jarvis so first-launch users get the in-character
        // narration ("very good, sir." / "Three failures in auth.py.")
        // instead of the bare template ("Tests are green."). Existing
        // users who explicitly chose a different persona keep their choice
        // — config.save only persists keys whose values differ from
        // DEFAULTS, so we never overwrite an explicit selection.
        m.insert("persona".into(), json!("jarvis"));
        // Verbosity profile names (heard/profiles/<name>.yaml). Bundled:
        // quiet / brief / normal / verbose. Custom: drop your own YAML in
        // $CONFIG_DIR/profiles/<name>.yaml. swarm_verbosity applies to
        // non-focus sessions when 2+ agents are active concurrently —
        // default "brief" so background agents stay quiet without losing
        // their critical signals.
        m.insert("verbosity".into(), json!("normal"));
        m.insert("swarm_verbosity".into(), json!("brief"));
        // True "Pause Heard" (menu item). Distinct from muted/audio_off, which only silence
        // OUTPUT: paused halts the narration brain at event ingress — no LLM calls, no cost —
        // turns voice input off, and dims the notch. Resume restores everything.
        m.insert("paused".into(), json!(false));
        // Installation-wide first-run lifecycle. `onboarded` stays the
        // fail-closed reset switch; generations reject stale UI completions.
        m.insert("first_run_generation".into(), json!(0));
        m.insert("first_run_completed_generation".into(), json!(0));
        m.insert("first_run_attempt".into(), json!(""));
        // Remembered by engine_api when Focus turns "Narrate routine steps" off, so
        // leaving Focus restores the user's choice.
        m.insert("narrate_routine_before_focus".into(), json!(false));
        // Settings → Voice (cadence.py). `send_mode` is WHERE your words go ("" = derive from
        // the legacy `mode`); `narration_volume` is THE quantity dial (0..3); -1 = unset = the
        // legacy mode + knobs decide, no budget. `narration_register` is tone (0 casual, 1 neutral, 2 formal);
        // neutral = the persona's own voice, an identity transform. See volume.py, register.py.
        m.insert("send_mode".into(), json!(""));
        m.insert("narration_volume".into(), json!(-1));
        m.insert("narration_register".into(), json!(1));
        // The user's own ElevenLabs key. Empty by default: Kokoro (local) is the
        // default voice, and ElevenLabs is only used when the user sets this.
        // Stored plain-text under the user-only-readable config dir.
        m.insert("elevenlabs_api_key".into(), json!(""));
        // Multi-agent (parallel CC sessions): when 2+ are firing events
        // concurrently, non-focus events accumulate and a periodic
        // digest summarises them. On by default — when you only have one
        // session active, it's a no-op. Off if you'd rather just drop
        // background events outright.
        m.insert("multi_agent_digest_enabled".into(), json!(true));
        m.insert("multi_agent_digest_interval_s".into(), json!(60));
        // When you fan out to a new project, the new agent gets a voice
        // automatically picked (deterministically) from a curated pool —
        // no YAML editing required. Same repo_name always maps to the
        // same voice across CC restarts. Only kicks in for non-focus
        // sessions in swarm mode, so solo-mode users keep their persona
        // voice unchanged. Set to false ("one voice" mode) if you'd rather
        // every agent speak in the persona's voice — then, in multi-agent
        // situations, every spoken line is prefixed with "Agent <name>: "
        // so you still know which agent it's reporting on.
        m.insert("multi_agent_auto_voices".into(), json!(false));
        // Manual repo_name → ElevenLabs voice_id overrides. Always wins
        // over the auto-pick. Edit YAML directly:
        //   agent_voices:
        //     api: <voice_id>
        //     web: <voice_id>
        m.insert("agent_voices".into(), json!({}));
        // Set to True after the user finishes the welcome flow (or skips it),
        // so we never re-prompt them.
        m.insert("onboarded".into(), json!(false));
        // Indefinite "Pause Heard": when true, the daemon drops every
        // event and the hook subprocess short-circuits without spawning
        // the daemon, so a paused Heard stays silent even if Quit makes
        // the daemon respawn on the next agent event. Only "Resume Heard"
        // (menu or hotkey) clears it — there's no auto-timeout.
        m.insert("muted".into(), json!(false));
        // Speaker off (the notch mute button) — DISTINCT from `muted`/Pause. audio_off keeps the
        // brain running so the turn feed still records the skill-styled transcript ("see it"), it
        // only suppresses TTS. `muted` (Pause Heard) is the cost-saver: events don't flow, nothing
        // runs, nothing records. So: audio_off = see-not-hear (brain cost, no TTS); muted = full off.
        m.insert("audio_off".into(), json!(false));
        // Codex integration preference. CLI hooks are still stored in
        // ~/.codex/hooks.json, but Codex Desktop narration tails session
        // logs directly, so "enabled" must not depend solely on the hook
        // file existing. Default on so upgraded users get Codex Desktop
        // narration without also installing the CLI hook.
        m.insert("codex_enabled".into(), json!(true));
        // "Thinking summary": when the user submits a prompt, Heard
        // speaks a 6-10 word "looking into X" phrase in the persona's
        // voice, filling Claude's first-token latency with relevant
        // context. Short prompts ("yes", "go ahead") are skipped at the
        // hook layer regardless. Off-by-config disables the feature
        // entirely.
        m.insert("narrate_prompt_intent".into(), json!(true));
        // Companion "still thinking" nudge: in companion mode, if a turn has been open
        // this many seconds with nothing spoken yet (the agent is still thinking, no
        // tool call or reply), Heard voices ONE canned "still looking at this" line so
        // the away-from-keyboard listener knows it is still working. Companion only
        // (copilot/focus never fire it). Off disables it. (owner, 2026-09-15)
        m.insert("companion_thinking_nudge".into(), json!(true));
        m.insert("companion_thinking_nudge_seconds".into(), json!(30));
        // The harness path: an optional LLM narrator (persona + agent state +
        // current event into one model call), when an edition wires one. The
        // verbosity/router/template chain stays in place as the fallback for
        // any harness failure (the daemon falls through to it), so an LLM
        // hiccup never goes audibly silent. With no narrator wired (the core
        // and the CLI), this key only selects the policy's harness rules.
        // Must stay in DEFAULTS so `config.save()` persists explicit
        // overrides (save() only writes keys whose values differ from
        // DEFAULTS — so users who set this to False keep their False).
        m.insert("harness_enabled".into(), json!(true));
        // Phase 3 add-on — listening mode for the harness path. Three values:
        //   "copilot"   — default. Screen-on, daily coding. Compressed
        //                 hooks and signposts; details live in the diff
        //                 the listener can read.
        //   "companion" — eyes-off (driving, cooking, walking). Lean but
        //                 substantive: state the choice, surface decisions,
        //                 plain English over developer-speak, every turn
        //                 ends with a hook into action. Built on Karpathy's
        //                 "simplicity + surgical + goal-driven" principles.
        //   "focus"  — alert-only. Stay quiet unless the user needs to
        //                 decide, approve something, or fix a blocker.
        // Read by harness.py to pick which addendum to layer onto the
        // base instruction block. No effect when harness_enabled is False
        // (v1 path doesn't have a prompt customisation point).
        //
        // Hold-to-speak voice input (in editions that have it) is the other consumer:
        //   copilot          — drafts the words into the agent's input; the
        //                      user presses Enter (review before send).
        //   companion/focus  — types + Enter straight into the agent
        //                      (auto-send; breaking changes need a spoken
        //                      "confirm" first).
        // A pending question/permission is answered the same way in every
        // mode (voice_answer runs before the loop).
        m.insert("mode".into(), json!("copilot"));
        // Narration SKILL — the drop-in voice/constraint the server engine applies to
        // every turn (built-in: default | adhd | verbose | karpathy, or the basename of
        // a user file in ~/.heard/skills/). "default" = the natural colloquial voice.
        // Read fresh per turn (live switch).
        m.insert("narration_skill".into(), json!("default"));
        // Routine-narration MODULE (toggleable, off by default). Off = only meaningful
        // turns are voiced (finals + needs-you); the play-by-play tool/progress chatter is
        // dropped entirely (no floor, no LLM, no cost). On = full play-by-play. Independent
        // of listening mode; read fresh per event.
        m.insert("narrate_routine".into(), json!(false));
        // Think/speak streams — how the harness brain works by default. The
        // harness emits a two-stream output: a private `think` field (its
        // reasoning — logged as ev=harness_think, NEVER voiced) and a `say`
        // field (the only thing spoken). Keeps reasoning structurally out of
        // speech so rationale can't leak into TTS. Costs a little extra
        // output per meaningful event (the think); the flag stays as an
        // off-switch (`config set harness_think_say false`) if that cost
        // ever proves not worth it. No effect when harness_enabled is False.
        m.insert("harness_think_say".into(), json!(true));
        // Report agent state into Herdr's sidebar (pane report-agent) when
        // a session runs inside a Herdr pane. On by default; a no-op for everyone who
        // does not use Herdr, because the report needs `herdr_pane_id` in the
        // session's terminal binding. Off = Heard reports nothing and Herdr goes back
        // to screen-detecting the agent itself.
        m.insert("herdr_report_state".into(), json!(true));

        // Settings-panel keys (the "Speak up on" switches). Registered here so
        // config.save() persists them — save() only keeps keys present in DEFAULTS.
        m.insert("notify.errors".into(), json!(true));
        m.insert("notify.blocked".into(), json!(true));
        m.insert("notify.completions".into(), json!(true));
        // "" = follow the macOS default output (unchanged behaviour). A device NAME
        // routes narration there; heard.audio_output resolves it at playback time.
        m.insert("voice.output_device".into(), json!(""));
        m.insert("app.language".into(), json!("system"));

        m
    })
}
