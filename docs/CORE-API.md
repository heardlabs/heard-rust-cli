# Core API for editions and extensions

The seams the core exposes so a composition root (the CLI's `heard daemon`,
or an app embedding the core) can add behaviour without the core knowing its
name. Everything here is public API of the crates in `crates/`. An edition is
any program that builds a daemon from these crates and plugs its own pieces
into the seams below.

## 1. Daemon extensions — `heard_daemon::Extension`

```rust
pub trait Extension: Send + Sync {
    fn name(&self) -> &'static str;
    fn handle_command(&self, daemon: &Arc<Daemon>, cmd: &str, raw: &[u8])
        -> Option<Option<Vec<u8>>> { None }
    fn observe_event(&self, event: &serde_json::Value) {}
    fn on_spoken(&self, line: &SpokenLine<'_>) {}          // SpokenLine = Utterance
    fn verbatim_kinds(&self) -> &'static [&'static str] { &[] }
    fn context_for(&self, session: &str) -> Option<String> { None }
    fn subscribe_hello(&self, hello: &mut serde_json::Map<String, Value>) {}
}
```

Object-safe; the daemon holds `Vec<Arc<dyn Extension>>` in builder order.

| Hook | Called | Contract |
|---|---|---|
| `handle_command` | on the accept loop, for a frame whose `cmd` is not in `heard_proto::CORE_CMDS` | `None` = pass to the next extension. `Some(None)` = claimed, no reply. `Some(Some(bytes))` = claimed, `bytes` written back verbatim. `raw` is the frame exactly as the client sent it. First claim wins. Unclaimed: logged `cmd_unhandled`, then the old `speak` fall-through (`req.get("text") or ""`). Must not block: spawn long work |
| `observe_event` | for every narration event that passed duplicate suppression and True Pause, before first-run hold, session mute and the policy | the value is `{"kind","tag","neutral","session":{"id","cwd"}}` |
| `on_spoken` | after every line handed to the `Speech` sink: routed event lines, `Daemon::say`, and the direct `speak` | the same `Utterance` the sink got. Nothing is called for a line dropped by mute or first-run hold |
| `verbatim_kinds` | once, in `DaemonBuilder::build` | lowercased and merged across extensions (`Daemon::verbatim_kinds()`). The casual register never prepends an opener to those kinds. In `Daemon::say`, a `tag` that is a verbatim kind is also the shaping kind |
| `context_for` | `Daemon::context_for(session)` | first non-blank answer, trimmed, in extension order |
| `subscribe_hello` | a `subscribe` client connects | insert the fields of its first `{"ev": "hello", …}` line (later extension wins a key) |

Core commands never reach an extension: `speak ping status pin unpin reload
stop mute unmute resume_intent feedback mute_session unmute_session event
hook cancel voice_hold voice_release tour_hold tour_release subscribe
inject` — except `report_defect`, which is offered to the extensions first
(an edition keeps the defect store) and only logged when unclaimed. A frame with one of those
`cmd`s whose fields fail to parse keeps the core's old reading (a blank
`speak`).

Helpers an extension uses: `Daemon::say(text, kind, tag, session_id, via)`
(shaping + mute gate + sink + `on_spoken`), `Daemon::note_user_engaged()`,
`Daemon::cfg_get(key)`, `Daemon::agent_states`, `Daemon::router`,
`heard_daemon::log::{utc_offset_seconds, now_epoch}`, `heard_daemon::dlog!`.
Do not store the `Arc<Daemon>` (the daemon owns its extensions); keep a
`Weak` if you must.

### Example

```rust
use std::sync::Arc;
use heard_daemon::{Daemon, DaemonBuilder, Extension, SpokenLine};

/// Answers `{"cmd":"ask","question":…}` with a small JSON reply, and speaks
/// its answers verbatim.
struct Ask;

#[derive(serde::Deserialize)]
struct AskFrame {
    question: String, // owned: the client may send escapes
    #[serde(default)]
    speak: bool,
}

impl Extension for Ask {
    fn name(&self) -> &'static str {
        "ask"
    }

    fn handle_command(&self, d: &Arc<Daemon>, cmd: &str, raw: &[u8]) -> Option<Option<Vec<u8>>> {
        if cmd != "ask" {
            return None; // not ours: the next extension, then the fall-through
        }
        let reply = match serde_json::from_slice::<AskFrame>(raw) {
            Ok(f) if !f.question.trim().is_empty() => {
                let answer = format!("You asked: {}", f.question.trim());
                if f.speak {
                    d.say(&answer, "answer", "answer", "__ask__", "ask");
                }
                serde_json::json!({"ok": true, "answer": answer})
            }
            _ => serde_json::json!({"ok": false, "answer": "", "error": "missing_question"}),
        };
        Some(serde_json::to_vec(&reply).ok())
    }

    fn verbatim_kinds(&self) -> &'static [&'static str] {
        &["answer"]
    }

    fn on_spoken(&self, line: &SpokenLine<'_>) {
        heard_daemon::dlog!("ask_saw_line", kind = line.kind, chars = line.text.chars().count());
    }
}

let daemon = DaemonBuilder::new(paths)
    .speech(speech)                 // Arc<dyn Speech>
    .extension(Arc::new(Ask))       // appended; order = claim order
    .build();
```

## 2. The wire — `heard_proto::parse_frame`

```rust
pub fn parse_frame(raw: &[u8]) -> Result<Frame<'_>, serde_json::Error>;
pub enum Frame<'a> { Core(Message<'a>), Extension(ExtensionCommand<'a>) }
pub struct ExtensionCommand<'a> { pub cmd: Cow<'a, str>, pub raw: &'a [u8], pub fallback: Speak<'a> }
pub const CORE_CMDS: &[&str];  pub fn is_core_cmd(cmd: &str) -> bool;
```

`Message` parses a recognised `cmd`, else the `speak` fall-through. Any
other command (for example `{"cmd":"ask",…}` or `{"cmd":"recap",…}`) arrives
as `Frame::Extension` with the client's bytes; the extension that claims it
defines its own request and response types. `StatusResponse` keeps its full
wire shape (the core reports the fields it does not own as empty/`null`). New: `transport::socket_path_for(app)`;
`socket_path()` is `socket_path_for("heard")`.

## 3. Speech observers — `heard_speech::SpeechObserver`

```rust
pub trait SpeechObserver: Send + Sync {
    fn on_spoken(&self, item: &SpeechItem, delivery: Delivery) {}  // every line the worker finished
    fn on_expired(&self, item: &SpeechItem) {}                     // held while the user spoke, aged out unspoken
}
QueuedSpeech::builder(tts, player, handle).observer(Arc::new(my_observer))  // repeatable, in order
```

Default: none. `on_spoken` runs on the worker after the `history.jsonl`
decision, for played, cancelled, skipped and failed lines alike. An observer
that keeps its own record of unspoken lines can map `on_expired(item)` to an
event such as
`{"kind":"unspoken","tag":"held_expired","neutral":item.text,"session":{"id":item.session_id,"cwd":""}}`.

The Python-compatible string helpers the queue needs live in `heard_state::py`
(`strip`, `is_space`, `rstrip`, `split_ws`, `len`, `head`, `truthy`, `int_of`,
`float_repr`, `dumps`, …).

## 4. TTS backend registry — `heard_tts::select`

```rust
pub trait BackendFactory: Send + Sync {
    fn name(&self) -> &'static str;
    fn claim(&self, cfg: &ConfigView<'_>) -> Claim;   // No | Preferred | Fallback
    fn build(&self, cfg: &ConfigView<'_>) -> Box<dyn Tts>;
}
pub fn register_backend(f: Arc<dyn BackendFactory>);          // process-wide; same name replaces
pub struct BackendRegistry;  // the same as a value: new, register, names, decide, select
pub struct ConfigView<'a> { pub elevenlabs_api_key: &'a str, pub kokoro_downloaded: bool,
                            pub config: Option<&'a Map<String, Value>> }
pub enum Backend { ElevenLabs, Registered(&'static str), Kokoro, Null }
```

Ladder: first `Preferred` registered backend → the user's ElevenLabs key →
first `Fallback` registered backend → Kokoro if downloaded → Null. With
nothing registered it is the built-in ladder (key, Kokoro, silence). A
registered backend reads its own keys from `ConfigView::config` (pass the
merged config) and reports errors as `TtsError::Backend { name, source }`.

A hosted voice service, for example, fits this exactly: `claim` = `No`
unless the service is usable with the current config; `Fallback` when the
user prefers their own key (the key wins); otherwise `Preferred`.

## 5. Config layers — `heard_config`

```rust
pub fn register_layer(defaults: &'static Map<String, Value>);
pub fn register_critical_keys(keys: &'static [&'static str]);
pub type SaveHook = fn(&Config, &Map<String, Value>, &str);  // (config, written map, YAML body)
pub fn register_save_hook(hook: SaveHook);
pub fn all_defaults() -> Arc<Map<String, Value>>;   // core defaults() + every layer
pub fn critical_keys() -> Vec<&'static str>;        // CRITICAL_KEYS + registered
pub fn read_mapping_file(path: &Path) -> Map<String, Value>;
pub fn write_atomic(path: &Path, body: &str) -> Result<()>;
```

Register once at startup, before the first `load` or `save` (a save before
registration drops the layer's keys as undeclared). `load` starts from
`all_defaults()`; the strict `save` writes non-default keys declared by the
core OR any layer, still drops undeclared keys, and the wipe guard protects
`critical_keys()`. Hooks run after each successful write. `defaults()` is
the core subset only.

## 6. Paths per edition — `heard_config::Paths`

```rust
Paths::resolve_for(app: &str, env: &PathEnv) -> Result<Paths>
Paths::from_env_for(app: &str) -> Result<Paths>
Paths::resolve(env) == Paths::resolve_for(heard_config::APP /* "heard" */, env)   // unchanged
```

The CLI uses `heard_config::CLI_APP` (`"heard-cli"`): config and data under
`~/Library/Application Support/heard-cli/` (XDG respected), `HEARD_DIR` =
`~/.heard-cli`, and `HEARD_LEDGER_PATH` ignored — nothing it resolves
overlaps the app's paths. An app edition can keep `Paths::resolve` /
`from_env`.

### 6.1 What `history.jsonl` keeps of the user's words — `heard_state::HistoryPolicy`

`history.jsonl` is mostly what Heard SAID. Two kinds of record carry what
the USER said instead: a `feedback` line's `text`, and a spoken line whose
kind quotes the user (the core's `prompt_intent` restates the prompt just
submitted). A `HistoryPolicy { record_user_text: false, .. }` keeps those
records' shape (`id`, `ts`, `kind`, `ref`, …) and blanks the words
(`text` / `spoken` → `""`, plus `"redacted": true`).

The policy is read through a `HistoryPolicySource` (`Arc<dyn Fn() ->
HistoryPolicy>`) on every append, so a config change applies to the next
line. Hand the same source to every history writer:

```rust
let policy = heard_daemon::speech::config_history_policy(Config::new(paths.clone()));
let speech = QueuedSpeech::builder(tts, player, handle)
    .history(&paths.config_dir)
    .history_policy(Arc::clone(&policy))   // or LogSpeech::with_history_policy
    .build();
let daemon = DaemonBuilder::new(paths).history_policy(policy) /* … */ .build();
```

| `history_user_text` in config | user words in `history.jsonl` |
|---|---|
| absent (default) | recorded — the behaviour before the policy existed |
| `true` | recorded |
| `false` | withheld |
| anything else | withheld (a value that cannot be read is not permission) |

An edition that speaks more quoting kinds adds them with
`HistoryPolicy::with_user_text_kinds`, and an edition with its own consent
setting builds its own source; `Daemon::history_policy()` hands the daemon's
source to extensions that keep stores of their own.

## 7. Wiring it together (a composition root)

```rust
// 1. config layers first (an edition with its own keys)
heard_config::register_layer(edition_defaults());
heard_config::register_critical_keys(EDITION_CRITICAL);
heard_config::register_save_hook(edition_backup_hook);
// 2. extra TTS backends (optional)
heard_tts::register_backend(Arc::new(MyBackendFactory::new()));
// 3. paths for this edition
let paths = heard_config::Paths::from_env_for(heard_config::CLI_APP)?;   // an app: Paths::from_env()
// 4. speech, with observers
let speech = QueuedSpeech::builder(tts, player, handle).history(&paths.config_dir)
    .observer(my_observer)          // optional
    .build();
// 5. the daemon, with extensions in claim order
let daemon = DaemonBuilder::new(paths)
    .speech(Arc::new(speech))
    .extension(notify)              // the CLI's notify extension
    .extension(my_extension)        // optional
    .build();
let server = Server::bind(daemon, &socket).await?;
server.serve().await;
```

### 7.1 The CLI edition's composition (`heard daemon`)

`crates/heard-cli/src/compose.rs` is the CLI edition's composition root —
the core crates only:

| seam | the CLI wires |
|---|---|
| paths | `heard_cli::paths::resolve()` — one root, `$HEARD_CLI_HOME` or `~/Library/Application Support/heard-cli/`: `config.yaml`, `daemon.sock`, `daemon.pid`, `daemon.log`, `daemon.lock`, `history.jsonl`, `models/`, `personas/`, and the daemon's side files (`heard_dir` = the root). `heard-hook`'s `HookConfig::CLI_EDITION` resolves the same socket |
| config layer | `heard_cli::edition::register()` — `onboarded: true`, `alerts: "both"`, `kokoro_voice: ""`, registered by every `heard` process before its first load or save |
| speech | `notify::AlertsGate` → `heard_speech::QueuedSpeech` (history in the root) over `AfplayPlayer` and a TTS picked by the built-in ladder: the user's own ElevenLabs key, else Kokoro when both model files are present (loaded lazily so the socket opens first; `heard-tts/kokoro` is a default feature of heard-cli), else `NullTts`. Hidden `--speech log` swaps the queue for `LogSpeech(<root>/would-say.jsonl)` |
| brain | `NoBrain` — templates and the no-LLM floor (the brain seam is where an optional LLM narrator would plug in) |
| personas | `persona::CliPersonas` — bundled + `<root>/personas/*.md` front matter |
| extensions | `notify::NotifyExtension` — `on_spoken` → macOS notification for needs-you lines (see `crates/heard-cli/src/notify.rs` for the classification table), coalesced on one worker thread, `osascript` with a constant `on run argv` script |
| history policy | `heard_daemon::speech::config_history_policy` — ONE source handed to the queue (`.history_policy`) and the daemon (`.history_policy`, for `feedback`). The CLI edition has no consent UI, so it records by default; `history_user_text: false` in `config.yaml` withholds the user's words (see §6.1) |
| background | project-digest drain (1 s), queue settings refresh when the snapshot changes (the on-disk pause every 5 s), `config.yaml` mtime watch (2 s → `reload`) |

The process: provider env vars are removed; `setsid()` (a process-group
leader — what the hook spawns — re-spawns itself once so the copy can);
stdout/stderr → `daemon.log`; an exclusive `flock` on `daemon.lock` makes a
second `heard daemon` exit 0; pid file written, socket bound; SIGTERM/SIGINT
→ `Daemon::stop` (a `stop` frame also shuts it down), SIGHUP → reload; on
exit the socket goes, then the lock, then the pid file (so `heard stop`
returning means a new daemon can start).

Rules the edition adds on top of the core:

- **First run.** The core holds narration until `onboarded`; the CLI has no
  onboarding window, so the layer defaults it to `true`, and `heard setup` /
  `heard install` also write it. An explicit `onboarded: false` still holds.
- **alerts** only affects needs-you lines: `both` = spoken + notification,
  `voice` = spoken, `notify` = notification instead of speech, `off` =
  neither. It is never mirrored into the core's `notify.*` keys, which are
  the "Speak up on" switches.
- **Voice precedence.** A non-empty `kokoro_voice` (the user's pick) wins over
  the persona's `kokoro_voice`, which wins over `bm_george`. The ElevenLabs
  voice keeps the Python order (`persona.voice or cfg["voice"]`).

Test switches (hidden flags, each also an env var so an auto-started daemon
inherits them): `--speech queued|log` (`HEARD_DAEMON_SPEECH`),
`--tts auto|null` (`HEARD_DAEMON_TTS`), `--notifier osascript|log`
(`HEARD_DAEMON_NOTIFIER`, `log` → `<root>/notifications.jsonl`).
`tests/e2e/replay_compare.py` replays recorded hook traffic through the real
`heard-hook` into `heard daemon --speech log` and diffs the would-say lines
against a baseline (bring your own recorded traffic; none is included).

## 8. The floor, the event stream and the timers

**Hold/replay — `Speech::hold(Hold)` / `Speech::release(Hold)`.** Every
`Speech` method below has a no-op default, so a recording sink needs none.

```rust
pub enum Hold { User, Tour }
trait Speech {
    fn speak_with(&self, u: &Utterance<'_>, opts: LineOptions);   // priority / coexists / history
    fn queue_state(&self) -> (bool, usize);                       // status: speaking, queued
    fn hold(&self, h: Hold);  fn release(&self, h: Hold);
    fn discard_held(&self);   fn clear_mic_latch(&self);  fn mic_active(&self) -> bool;
    fn is_holding(&self) -> bool;  fn drop_session(&self, sid: &str) -> usize;
    fn last_utterance_id(&self) -> Option<String>;
}
```

| wire | daemon | `QueuedSpeech` |
|---|---|---|
| `voice_hold` | `Daemon::voice_hold` | cut the playing line, DROP the queue, hold new lines |
| `voice_release` | `Daemon::voice_release` (stamps engagement) | replay the held lines in order |
| `tour_hold` | `Daemon::tour_hold` | RESCUE the queue into the front of the held buffer, cut, hold; lapses after 30 min; `hold_exempt_sessions` are never held by it |
| `tour_release` | `Daemon::tour_release` | replay |

Replay waits until no hold and no mic latch remains; the held buffer keeps
its priority-aware cap and age limit. `QueuedSpeechBuilder::events(bus)`
emits `speech_started` / `speech_finished` (`{"kind"}`) around each chunk.

**Event stream — `subscribe`.** `heard_daemon::EventBus` (clone = same bus;
`DaemonBuilder::events(bus)`), `Daemon::emit_event(name, fields)`,
`Daemon::subscribe_events() -> Receiver<String>` (wire lines),
`Daemon::hello_line()`. Wire: the client writes `{"cmd":"subscribe"}` and
half-closes; the daemon keeps the connection and writes `{"ev": "hello", …}`
then one `json.dumps`-spelled line per event, `ev` first, `ensure_ascii`.

**Timers — `Daemon::tick(auto_voices)`** (call once a second instead of
`drain_project_digests`): first-run discards the digest; Focus's hung-tool
line (`filler::HUNG_TOOL_S`); companion's "still thinking" nudge and, on
prompt submit, the "On it." filler, both under `filler::FillerPolicy`; the
resume panel's 30 s timeout; the digest drain (skipped while muted or
awaiting the resume answer). `DaemonBuilder::clock` / `picker` make them
deterministic in tests.

**Other core commands now real:** `feedback` (`history.append_feedback`
against `Speech::last_utterance_id`), `resume_intent` (keyword classifier;
ambiguous = fresh), `mute` keeps the digest for the resume panel,
`unmute` arms it, `mute_session` drops that session's queued lines, `speak`
during first run answers `{"ok": false, "error": "first_run_hold"}`,
`inject` answers `{"ok": false, "error": "not_supported"}`.
`Daemon::say_line(&Line)` speaks with explicit placement and an optional
first-run setup pass (`first_run_generation`); `Daemon::first_run_state()`
and `Daemon::first_run_reset()` serve an edition's `first_run_hold`.

## 9. TTS, speech settings and status seams

**Kokoro download — `heard_tts::download`** (no `kokoro` feature needed):
`Source { base_url, files, retry }` (`Source::pinned(retry)`,
`with_base_url`), `download(&src, dir, &mut dyn Progress) ->
Result<Vec<(name, Outcome)>, DownloadError { message, fix }>`, `status`,
`installed`, `remove`, `sha256_file`, `pinned()`, `human`. Resumable
(`Range`), pinned by size and SHA-256, atomic rename, size-capped. Nothing
downloads by itself; `heard models download` (and the app's "download the
local voice") call it. `Progress` has no-op defaults (`NoProgress`).

**Long text — `heard_tts::chunk`**: `kokoro_onnx` 0.6.1's chunker
(`split_phonemes`, `split_phonemes_max`, `pause_after`, `normalize`,
`batches`). `KokoroTts::synth_chunked` / `Tts::synth` batch at 510 phonemes
(sentence → clause → word → mid-word, balanced), trim each batch, add 0.25 s
after a sentence / 0.1 s after a clause, concatenate. `synth_pcm` stays the
raw single window.

**Kokoro as a real rung** (`kokoro` feature): `heard_tts::kokoro::KokoroFactory::new(models_dir)`
claims `Fallback` when both model files are present (the built-in rung's
position when registered after any other `Fallback` backend) and builds a
`LazyKokoro` (loaded on first synth; `warm()` loads ahead). Without it,
`select_backend` turns a Kokoro decision into `NullTts`. ONNX Runtime is
linked statically by `ort` (a prebuilt `libonnxruntime.a`; no dylib to ship).

**Voice library**: `ElevenLabsTts::fetch_voice_library() -> Vec<LibraryVoice
{ id, name, description, category }>` — `GET /v1/voices`, empty on any
failure.

**Per-utterance settings**: `QueuedSpeechBuilder::settings_source(Fn(&SpeechSettings)
-> SpeechSettings)` — called once per line just before synthesis (off the
lock); the answer is what the line is spoken with and becomes the queue's
settings; `muted` is always kept from the queue.

**Cut one session**: `Speech::cut_session(sid) -> usize` (default
`drop_session`); `QueuedSpeech` also cuts the playing line when it belongs to
`sid`. `Daemon::cut_session(sid)`.

**Status fields**: `Extension::status_fields(&mut StatusResponse)` — called in
extension order on every `status`, so an edition fills `account_usage`,
`pending_update`, … (the core leaves them empty).
