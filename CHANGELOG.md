# Changelog

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
