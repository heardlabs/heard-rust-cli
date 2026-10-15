# Changelog

## 0.2.6 — 2026-10-15

- **Modes retired.** Heard has one behaviour: what the old defaults did
  (Co-pilot, `narration_volume` unset, `narrate_routine` off, `verbosity`
  normal). Questions, approvals, failures and turn results are spoken; tool
  progress (`routine_narration_off`) and the agent's prose between tools
  (`copilot_prose_off`) are not. A default config narrates exactly as before;
  a config that still sets a mode now gets the one behaviour.
- **heard-config:** `mode`, `send_mode`, `narration_volume`,
  `narrate_routine`, `verbosity`, `narrate_routine_before_focus`,
  `companion_thinking_nudge` and `companion_thinking_nudge_seconds` stay in
  the defaults with their old values, so existing config files stay valid,
  but nothing reads them. New `RETIRED_KEYS` lists them.
- **heard-daemon:** the policy no longer reads any of them. Gone:
  `policy::{volume_stop, volume_stop_of, narration_mode, legacy_mode,
  stop_for_legacy, budget_wpm, Budget, NEEDS_ME, LOW, NORMAL, HIGH,
  focus_prompt_speech, is_focus_attention_event, crc32}`,
  `PolicyOutcome::{SpeakFocusAlert, Digest}`, the `mode`, `narrate_routine`,
  `focus_*`, `verbosity_*` and `budget_exhausted` fields of `Features` /
  `Thunks`, `FeatureState::persona_name`, `DaemonBuilder::picker`, and
  Focus's hung-tool line and Companion's fillers (`filler::{HUNG_TOOL_S,
  PROMPT_FILLER_LINES, THINKING_NUDGE_LINES, should_prompt_filler,
  should_thinking_nudge, thinking_nudge_after_s, pick_varied,
  hung_tool_line}`). `is_focus_template_event` is private. Rules removed
  from `RULES`: `volume_budget`, `focus_attention_drop`, `focus_alert`,
  `focus_no_prompt`, `focus_fastpath_drop`, `copilot_tool_tier_suppressed`,
  `fastpath_verbosity_drop`, `fastpath_verbosity_digest`; the floor's
  `focus_harness_punt` drop is gone. Every other rule name is unchanged.
- **heard-narrate:** the solo verbosity gates always use the normal profile
  (`swarm_verbosity` is unchanged).
- **heard-cli:** `heard mode`, `/mode`, the setup step and the status `mode`
  field are gone; `heard config set` refuses the retired keys.
  `/heard-mode` is no longer intercepted, and install / uninstall remove the
  `heard-mode.md` command file an earlier install wrote.

## 0.2.4 — 2026-10-02

- **heard-daemon: a Claude Code Stop speaks the message it carries.** Claude
  Code hands the turn's final message over as `last_assistant_message`. The
  Stop handler read only the transcript, and when a session's transcript stops
  being written (seen with a resumed session: nothing after 18:05 while Stop
  kept firing) it found no new text and said nothing for hours. It now speaks
  `last_assistant_message` when the transcript has no new prose, falling back
  to the transcript's last assistant text, as the Codex handler already does.

## 0.2.3 — 2026-10-02

- **heard-daemon: blocking work no longer strands async tasks.** Narration
  routing can call the brain synchronously (a blocking model request, often
  seconds), and both the serve loop and the hook lanes reach it from tokio
  workers. On a small multi-thread runtime that held a worker and stranded
  the tasks queued on it: in Heard's product build a Parrot wake read missed
  its 2 s reply on ~42% of brain-narrated lines, and other socket requests
  waited behind the call. `Daemon::handle_event` and the serve loop's
  dispatch now run under `tokio::task::block_in_place` on a multi-thread
  runtime (inline elsewhere, as before), so the worker's queue moves to
  another worker for the duration. Event order is unchanged.

## 0.2.2 — 2026-09-29

- **heard-proto: socket frame text is owned when it must be.** Every text
  field a frame carries (`Speak.text`, the `pin` / `mute` / `unmute` /
  `resume_intent` / `feedback` / `report_defect` / `mute_session` /
  `unmute_session` fields, the narration event's `kind` / `neutral` / `tag`
  and session fields, the health-probe nonce, the hook binding) is now
  `Cow<'a, str>` instead of `&'a str`. A frame containing any JSON escape —
  `\/` (Swift's `JSONEncoder` default for every `/`), `\"`, `\n`, `\uXXXX` —
  used to fail to parse and fall through to a blank `speak`; it now parses
  to the unescaped text. Unescaped strings are still borrowed.
  *API:* code that builds these types passes `"…".into()` where it passed
  a `&str`.
- **heard-daemon: authoritative attention silence.**
  `Brain::silence_is_authoritative` (default `false`) lets a bounded
  attention brain keep a deliberate silence on a final or turn opener
  instead of the floor rescuing it. Existing brains keep the old behaviour.
- **heard-daemon: explicit supervisor shutdown.** `Daemon::shutdown()` ends
  the service generation regardless of the `stop_shuts_down` policy, for
  supervisors that must retire a poisoned generation. It is not exposed as
  the socket `stop` command.
- **heard-cli: no signal window at daemon start.** `heard daemon` installs
  its SIGTERM/SIGINT handlers before binding `daemon.sock`, so a signal
  sent the moment the socket appears stops it cleanly instead of killing
  it and leaving `daemon.sock` / `daemon.pid` behind.

## 0.2.1

- `Extension::classify_resume_intent`: an extension can classify an
  ambiguous "catch up or start fresh?" answer; it runs off the accept loop,
  keyword answers stay inline, and an unclassifiable answer is logged.

## 0.2.0

- History policy: `history_user_text` withholds the user's own words from
  `history.jsonl`.
- Kokoro: downloader in heard-tts, long-text chunking (>510 phonemes), voice
  library fetch.
- Speech: per-utterance settings, push-to-talk and tour hold with replay,
  `cut_session`.
- Daemon: subscribe event stream, filler/nudge timers, feedback and resume
  intent, extension status fields.
- Installer: edition-aware hook merging.

## 0.1.0

- First release.
