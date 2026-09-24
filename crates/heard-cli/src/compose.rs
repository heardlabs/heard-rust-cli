//! `heard daemon` — the CLI edition's composition root.
//!
//! ```text
//!  paths::resolve()  (one root: config.yaml, daemon.sock, daemon.pid,
//!        │            daemon.log, history.jsonl, models/, personas/)
//!        ▼
//!  edition layer ──▶ build() ──▶ heard_daemon::Daemon
//!                                  ├─ Speech    AlertsGate ─▶ QueuedSpeech(Tts, afplay)
//!                                  │                          │   Tts = Kokoro | ElevenLabs (own key) | Null
//!                                  │            or ─▶ LogSpeech(would-say.jsonl)   (--speech log)
//!                                  ├─ Brain     NoBrain (templates + the no-LLM floor)
//!                                  ├─ Personas  CliPersonas (bundled + <root>/personas/*.md)
//!                                  └─ Extension notify (macOS notifications for needs-you lines)
//!  background: project-digest drain (1 s), speech-settings refresh on
//!  reload, config.yaml mtime watch (2 s), SIGTERM/SIGINT → clean stop,
//!  SIGHUP → reload.
//! ```
//!
//! Only the core is wired: no LLM narrator, no memory, no hosted voice.
//! [`run`] is the process around it: detach, single instance, pid file,
//! signals, cleanup.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use heard_config::{Config, Paths};
use heard_daemon::{Daemon, DaemonBuilder, LogSpeech, NoBrain, Server, Speech};
use heard_speech::{AfplayPlayer, Player, QueuedSpeech, SpeechSettings};
use heard_tts::{NullTts, Tts};
use serde_json::{Map, Value};

use crate::notify::{AlertsGate, DaemonLink, LogNotifier, Notifier, NotifyExtension};
use crate::paths;
use crate::persona::{self, CliPersonas};
use crate::ui::{CliError, CliResult};

/// Environment variables that carry a provider credential, named
/// explicitly. The daemon never reads a key from the environment (a key
/// comes from `config.yaml`), so these — and every other variable
/// [`is_secret_env`] matches — are removed from its process, and so from
/// `afplay` and `osascript`, whether or not the user configured one.
pub const SECRET_ENV: &[&str] = &["ANTHROPIC_API_KEY", "ELEVENLABS_API_KEY", "OPENAI_API_KEY"];

/// True for a variable the daemon must not inherit: one of [`SECRET_ENV`],
/// or any name ending in `_API_KEY` or `_TOKEN`.
pub fn is_secret_env(name: &str) -> bool {
    SECRET_ENV.contains(&name) || name.ends_with("_API_KEY") || name.ends_with("_TOKEN")
}

/// Set on the re-spawned child when the daemon has to leave its process
/// group to `setsid` (see [`detach`]).
pub const DETACHED_ENV: &str = "HEARD_DAEMON_DETACHED";

/// Where lines go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpeechKind {
    /// The real queue: TTS, then `afplay`.
    #[default]
    Queued,
    /// Record would-say lines to `<root>/would-say.jsonl` (one JSON object
    /// per line) and `history.jsonl`. Never audio.
    Log,
}

/// Which voice backend the queue uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TtsKind {
    /// Kokoro when the model is installed, ElevenLabs only with the user's
    /// own key, else silence.
    #[default]
    Auto,
    /// Silence: the queue runs, nothing is synthesised.
    Null,
}

/// Where needs-you notifications go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotifierKind {
    /// macOS notifications via `osascript`.
    #[default]
    Osascript,
    /// One JSON line each in `<root>/notifications.jsonl`, nothing shown.
    Log,
}

/// What `heard daemon` was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    /// `--foreground`: stay in this terminal's session, log to stdout.
    pub foreground: bool,
    /// `--speech`.
    pub speech: SpeechKind,
    /// `--tts`.
    pub tts: TtsKind,
    /// `--notifier`.
    pub notifier: NotifierKind,
}

impl Options {
    /// The argv that reproduces these options (for the re-spawn).
    pub fn to_args(self) -> Vec<&'static str> {
        let mut v = vec!["daemon"];
        if self.foreground {
            v.push("--foreground");
        }
        if self.speech == SpeechKind::Log {
            v.extend(["--speech", "log"]);
        }
        if self.tts == TtsKind::Null {
            v.extend(["--tts", "null"]);
        }
        if self.notifier == NotifierKind::Log {
            v.extend(["--notifier", "log"]);
        }
        v
    }
}

/// Test seams. `Default` is production.
#[derive(Default)]
pub struct Overrides {
    /// The TTS (default: by [`TtsKind`]).
    pub tts: Option<Arc<dyn Tts>>,
    /// The player (default: `afplay`).
    pub player: Option<Arc<dyn Player>>,
    /// The notifier (default: `osascript`).
    pub notifier: Option<Arc<dyn Notifier>>,
}

/// The built daemon and what it was built from.
pub struct Wired {
    /// The daemon.
    pub daemon: Arc<Daemon>,
    /// The queue, with [`SpeechKind::Queued`].
    pub queued: Option<QueuedSpeech>,
    /// Whether the queue speaks Kokoro voice ids.
    pub kokoro: bool,
    /// The TTS's name, for the startup line.
    pub tts_name: &'static str,
    /// `<root>/would-say.jsonl`, with [`SpeechKind::Log`].
    pub would_say: Option<PathBuf>,
    /// The paths.
    pub paths: Paths,
}

/// `<root>/notifications.jsonl` (`--notifier log`).
pub fn notifications_path(p: &Paths) -> PathBuf {
    p.data_dir.join("notifications.jsonl")
}

/// `<root>/would-say.jsonl`.
pub fn would_say_path(p: &Paths) -> PathBuf {
    p.data_dir.join("would-say.jsonl")
}

fn cfg_bool(cfg: &Map<String, Value>, key: &str) -> bool {
    matches!(cfg.get(key), Some(Value::Bool(true)))
}

/// The queue's settings from the merged config, with the CLI edition's
/// voice precedence ([`crate::persona`]).
pub fn speech_settings(
    cfg: &Map<String, Value>,
    personas_dir: &Path,
    kokoro: bool,
) -> SpeechSettings {
    SpeechSettings {
        muted: cfg_bool(cfg, "muted"),
        audio_off: cfg_bool(cfg, "audio_off"),
        speed: cfg.get("speed").and_then(Value::as_f64).unwrap_or(1.0),
        voice: persona::elevenlabs_voice(cfg, personas_dir),
        kokoro_voice: persona::kokoro_voice(cfg, personas_dir),
        use_kokoro_voice: kokoro,
        lang: Config::tts_lang_for(cfg),
        persona: persona::active_name(cfg),
        voice_mode: cfg
            .get("voice_mode")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }
}

/// Kokoro, loaded on first use (on the queue's blocking pool) so the socket
/// opens before the 300 MB model is read. [`LazyKokoro::warm`] starts the
/// load right away on its own thread.
#[cfg(feature = "kokoro")]
struct LazyKokoro {
    dir: PathBuf,
    cell: OnceLock<Result<heard_tts::kokoro::KokoroTts, String>>,
}

#[cfg(feature = "kokoro")]
impl LazyKokoro {
    fn get(&self) -> &Result<heard_tts::kokoro::KokoroTts, String> {
        self.cell.get_or_init(|| {
            let r = heard_tts::kokoro::KokoroTts::new(&self.dir).map_err(|e| e.to_string());
            match &r {
                Ok(_) => heard_daemon::dlog!("kokoro_loaded"),
                Err(e) => heard_daemon::dlog!("kokoro_load_failed", err = e.as_str()),
            }
            r
        })
    }

    fn warm(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("heard-kokoro-load".into())
            .spawn(move || {
                let _ = me.get();
            });
    }
}

#[cfg(feature = "kokoro")]
impl Tts for LazyKokoro {
    fn audio_ext(&self) -> &'static str {
        ".wav"
    }
    fn max_native_speed(&self) -> f64 {
        4.0
    }
    fn list_voices(&self) -> Vec<String> {
        self.get()
            .as_ref()
            .map(|k| k.list_voices())
            .unwrap_or_default()
    }
    fn synth(
        &self,
        text: &str,
        voice: &str,
        speed: f64,
        lang: &str,
    ) -> Result<heard_tts::Audio, heard_tts::TtsError> {
        match self.get() {
            Ok(k) => k.synth(text, voice, speed, lang),
            Err(e) => Err(heard_tts::TtsError::Backend {
                name: "kokoro",
                source: e.clone().into(),
            }),
        }
    }
}

/// The free ladder over the edition's config: the user's ElevenLabs key,
/// else Kokoro when both model files are present, else silence.
fn select_tts(cfg: &Map<String, Value>, paths: &Paths) -> (Arc<dyn Tts>, &'static str, bool) {
    let key = cfg
        .get("elevenlabs_api_key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let view = heard_tts::ConfigView {
        elevenlabs_api_key: &key,
        kokoro_downloaded: heard_tts::select::kokoro_is_downloaded(&paths.models_dir),
        config: Some(cfg),
    };
    match heard_tts::select::decide(&view) {
        #[cfg(feature = "kokoro")]
        heard_tts::Backend::Kokoro => {
            let k = Arc::new(LazyKokoro {
                dir: paths.models_dir.clone(),
                cell: OnceLock::new(),
            });
            k.warm();
            (k, "kokoro", true)
        }
        heard_tts::Backend::Null => (Arc::new(NullTts), "null", false),
        b => (Arc::from(heard_tts::select_backend(&view)), b.name(), false),
    }
}

/// Build the daemon. Must run inside the tokio runtime (the queue takes its
/// handle). Registers the edition's config layer first.
pub fn build(paths: &Paths, opts: &Options, overrides: Overrides) -> Wired {
    crate::edition::register();
    let cfg = Config::new(paths.clone()).load(None).unwrap_or_default();
    let link = DaemonLink::new();
    let personas_dir = paths::personas_dir(paths);

    let mut queued = None;
    let mut would_say = None;
    let mut kokoro = false;
    let mut tts_name = "none";
    let events = heard_daemon::EventBus::new();
    let (sink, backend): (Arc<dyn Speech>, String) = match opts.speech {
        SpeechKind::Log => {
            let p = would_say_path(paths);
            would_say = Some(p.clone());
            (
                Arc::new(LogSpeech::new(&p).with_history(&paths.config_dir)),
                "LogSpeech".into(),
            )
        }
        SpeechKind::Queued => {
            let (tts, name, k): (Arc<dyn Tts>, &'static str, bool) =
                match (&overrides.tts, opts.tts) {
                    (Some(t), _) => (Arc::clone(t), "injected", false),
                    (None, TtsKind::Null) => (Arc::new(NullTts), "null", false),
                    (None, TtsKind::Auto) => select_tts(&cfg, paths),
                };
            tts_name = name;
            kokoro = k;
            let player = overrides
                .player
                .clone()
                .unwrap_or_else(|| Arc::new(AfplayPlayer::new()));
            let audio_dir = paths.data_dir.join("tmp");
            let _ = std::fs::create_dir_all(&audio_dir);
            let q = QueuedSpeech::builder(tts, player, tokio::runtime::Handle::current())
                .history(&paths.config_dir)
                .tmp_dir(audio_dir)
                .settings(speech_settings(&cfg, &personas_dir, kokoro))
                .events(events.clone())
                .build();
            queued = Some(q.clone());
            (Arc::new(q), format!("QueuedSpeech/{name}"))
        }
    };
    let speech: Arc<dyn Speech> = Arc::new(AlertsGate::new(sink, Arc::clone(&link)));
    let notifier: Option<Arc<dyn Notifier>> = match (overrides.notifier, opts.notifier) {
        (Some(n), _) => Some(n),
        (None, NotifierKind::Log) => Some(Arc::new(LogNotifier::new(notifications_path(paths)))),
        (None, NotifierKind::Osascript) => None,
    };
    let notify = match notifier {
        Some(n) => NotifyExtension::with(
            Arc::clone(&link),
            n,
            crate::notify::DEFAULT_GAP,
            crate::notify::DEFAULT_DEDUP,
        ),
        None => NotifyExtension::new(Arc::clone(&link)),
    };
    let daemon = DaemonBuilder::new(paths.clone())
        .events(events)
        .speech(speech)
        .brain(Arc::new(NoBrain))
        .backend_name(backend)
        .personas(Arc::new(CliPersonas {
            dir: personas_dir.clone(),
        }))
        .extension(Arc::new(notify))
        .build();
    link.attach(&daemon);
    Wired {
        daemon,
        queued,
        kokoro,
        tts_name,
        would_say,
        paths: paths.clone(),
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// The background loops: the project-digest drain (1 s), the speech
/// settings refresh (whenever the snapshot changes; the on-disk pause every
/// 5 s), and the `config.yaml` watcher (2 s, so a hand edit applies without
/// a `reload`).
pub fn spawn_background(w: &Wired) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = Vec::new();
    let daemon = Arc::clone(&w.daemon);
    tasks.push(tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let auto_voices = matches!(daemon.cfg_get("auto_voices"), Some(Value::Bool(true)));
            let d = Arc::clone(&daemon);
            let _ = tokio::task::spawn_blocking(move || d.tick(auto_voices)).await;
        }
    }));

    let daemon = Arc::clone(&w.daemon);
    let config_path = w.paths.config_path.clone();
    tasks.push(tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let mut seen = mtime(&config_path);
        loop {
            tick.tick().await;
            let now = mtime(&config_path);
            if now != seen {
                seen = now;
                let d = Arc::clone(&daemon);
                let _ = tokio::task::spawn_blocking(move || d.reload()).await;
            }
        }
    }));

    if let Some(q) = w.queued.clone() {
        let daemon = Arc::clone(&w.daemon);
        let kokoro = w.kokoro;
        let personas_dir = paths::personas_dir(&w.paths);
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            let mut last = Value::Null;
            let mut n: u64 = 0;
            loop {
                tick.tick().await;
                n += 1;
                let snap = daemon.cfg_value();
                let disk_tick = n.is_multiple_of(5);
                if snap == last && !disk_tick {
                    continue;
                }
                let cfg = snap.as_object().cloned().unwrap_or_default();
                let mut s = speech_settings(&cfg, &personas_dir, kokoro);
                if disk_tick {
                    // "Pause Heard" is read from disk (see Daemon::is_muted).
                    let d = Arc::clone(&daemon);
                    s.muted = tokio::task::spawn_blocking(move || d.is_muted())
                        .await
                        .unwrap_or(s.muted);
                }
                last = snap;
                if s != q.settings() {
                    q.set_settings(s);
                }
            }
        }));
    }
    tasks
}

// ── the process ─────────────────────────────────────────────────────────

/// Remove the provider credentials from this process's environment. Call
/// before any thread exists.
pub fn scrub_env() {
    let names: Vec<std::ffi::OsString> = std::env::vars_os()
        .map(|(k, _)| k)
        .filter(|k| k.to_str().is_some_and(is_secret_env))
        .collect();
    for name in names {
        // Edition 2021: `remove_var` is a safe fn. `run` calls this first,
        // before the runtime or any thread, which is what makes it sound.
        std::env::remove_var(name);
    }
}

/// How [`detach`] went.
#[derive(Debug, PartialEq, Eq)]
pub enum Detach {
    /// This process is now its own session leader (or stays attached with
    /// `--foreground`): carry on.
    Continue,
    /// A detached copy was started; this process should exit 0.
    HandedOff,
}

/// Leave the controlling terminal's session.
///
/// `setsid()` fails for a process-group leader, which is exactly what
/// `heard-hook`'s auto-start spawns (`process_group(0)`). Then a copy of
/// this daemon is started as a plain child (not a group leader, so its own
/// `setsid()` succeeds) and this one hands off. `heard start` spawns a plain
/// child, which detaches directly.
pub fn detach(opts: &Options, paths: &Paths) -> CliResult<Detach> {
    if opts.foreground {
        return Ok(Detach::Continue);
    }
    match rustix::process::setsid() {
        Ok(_) => Ok(Detach::Continue),
        Err(e) if std::env::var_os(DETACHED_ENV).is_some() => {
            // Already the re-spawned copy: never loop. Stay in the group.
            heard_daemon::dlog!("setsid_failed", err = e.to_string());
            Ok(Detach::Continue)
        }
        Err(_) => {
            let exe = std::env::current_exe().map_err(|e| {
                CliError::failure(
                    format!("cannot find the heard binary: {e}"),
                    "reinstall heard",
                )
            })?;
            let log = open_log(paths)?;
            let err = log.try_clone().map_err(|e| {
                CliError::failure(format!("log handle: {e}"), "retry `heard start`")
            })?;
            std::process::Command::new(exe)
                .args(opts.to_args())
                .env(DETACHED_ENV, "1")
                .stdin(std::process::Stdio::null())
                .stdout(log)
                .stderr(err)
                .spawn()
                .map_err(|e| {
                    CliError::failure(
                        format!("cannot start the detached daemon: {e}"),
                        "run `heard daemon --foreground` to see why",
                    )
                })?;
            Ok(Detach::HandedOff)
        }
    }
}

fn open_log(paths: &Paths) -> CliResult<File> {
    std::fs::create_dir_all(&paths.data_dir).ok();
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log_path)
        .map_err(|e| {
            CliError::failure(
                format!("cannot open {}: {e}", paths.log_path.display()),
                "check you can write to Heard's state directory",
            )
        })
}

/// Point stdout and stderr at `daemon.log` (the hook spawns the daemon with
/// both on /dev/null, and the daemon logs with `println!`).
fn redirect_stdio(paths: &Paths) -> CliResult<()> {
    let log = open_log(paths)?;
    rustix::stdio::dup2_stdout(&log)
        .and_then(|()| rustix::stdio::dup2_stderr(&log))
        .map_err(|e| {
            CliError::failure(
                format!("cannot log to {}: {e}", paths.log_path.display()),
                "check you can write to Heard's state directory",
            )
        })
}

/// The single-instance lock, held for the daemon's life.
pub fn lock_path(p: &Paths) -> PathBuf {
    p.data_dir.join("daemon.lock")
}

/// Take the lock, or `None` when another daemon holds it.
fn take_lock(paths: &Paths) -> CliResult<Option<File>> {
    std::fs::create_dir_all(&paths.data_dir)
        .map_err(|e| crate::settings::io_fail(&paths.data_dir, e))?;
    let path = lock_path(paths);
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| crate::settings::io_fail(&path, e))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(crate::settings::io_fail(&path, e)),
    }
}

fn write_pid(paths: &Paths) -> CliResult<()> {
    let tmp = paths.pid_path.with_extension("pid.tmp");
    std::fs::write(&tmp, format!("{}\n", std::process::id()))
        .and_then(|()| std::fs::rename(&tmp, &paths.pid_path))
        .map_err(|e| crate::settings::io_fail(&paths.pid_path, e))
}

/// Remove the pid file if it is still ours.
fn remove_pid(paths: &Paths) {
    let ours = std::fs::read_to_string(&paths.pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        == Some(std::process::id());
    if ours {
        let _ = std::fs::remove_file(&paths.pid_path);
    }
}

/// Run the daemon in this process until SIGTERM, SIGINT or a `stop` frame.
pub fn run(paths: &Paths, opts: Options) -> CliResult<()> {
    scrub_env();
    crate::edition::register();
    if detach(&opts, paths)? == Detach::HandedOff {
        return Ok(());
    }
    if !opts.foreground {
        redirect_stdio(paths)?;
    }
    let Some(lock) = take_lock(paths)? else {
        heard_daemon::dlog!(
            "daemon_already_running",
            pid = i64::from(std::process::id())
        );
        if opts.foreground {
            println!("heard daemon is already running");
        }
        return Ok(());
    };
    // Holding the lock, a live socket can only be a daemon that lost its
    // lock file (deleted by hand): leave it be rather than take its socket.
    if std::os::unix::net::UnixStream::connect(&paths.socket_path).is_ok() {
        heard_daemon::dlog!("daemon_already_running", reason = "socket_answers");
        return Ok(());
    }
    write_pid(paths)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_io()
        .enable_time()
        .build()
        .map_err(|e| {
            remove_pid(paths);
            CliError::failure(
                format!("cannot start the runtime: {e}"),
                "retry `heard start`",
            )
        })?;
    let result = runtime.block_on(serve(paths, opts));
    // The queue's blocking tasks (a synth, an afplay) must not hold the exit.
    runtime.shutdown_timeout(Duration::from_secs(2));
    // Lock first, pid last: once daemon.pid is gone a new daemon can start.
    drop(lock);
    remove_pid(paths);
    result
}

async fn serve(paths: &Paths, opts: Options) -> CliResult<()> {
    let wired = build(paths, &opts, Overrides::default());
    let alerts =
        crate::settings::alerts_of(wired.daemon.cfg_value().as_object().unwrap_or(&Map::new()));
    let server = Server::bind(Arc::clone(&wired.daemon), &paths.socket_path)
        .await
        .map_err(|e| {
            CliError::failure(
                format!("cannot listen on {}: {e}", paths.socket_path.display()),
                "check nothing else owns that path, then `heard start`",
            )
        })?;
    heard_daemon::dlog!(
        "cli_daemon_start",
        pid = i64::from(std::process::id()),
        speech = match opts.speech {
            SpeechKind::Queued => "queued",
            SpeechKind::Log => "log",
        },
        tts = wired.tts_name,
        alerts = alerts.key(),
        sock = paths.socket_path.display().to_string()
    );
    let tasks = spawn_background(&wired);
    let signals = spawn_signals(Arc::clone(&wired.daemon));
    server.serve().await;
    for t in tasks.into_iter().chain(signals) {
        t.abort();
    }
    let _ = std::fs::remove_file(&paths.socket_path);
    heard_daemon::dlog!("cli_daemon_exit", pid = i64::from(std::process::id()));
    Ok(())
}

/// SIGTERM / SIGINT → the daemon's own `stop` (cancel speech, end the accept
/// loop, unlink the socket); SIGHUP → reload config.
fn spawn_signals(daemon: Arc<Daemon>) -> Vec<tokio::task::JoinHandle<()>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut tasks = Vec::new();
    for (kind, name) in [
        (SignalKind::terminate(), "SIGTERM"),
        (SignalKind::interrupt(), "SIGINT"),
    ] {
        let Ok(mut s) = signal(kind) else {
            continue;
        };
        let d = Arc::clone(&daemon);
        tasks.push(tokio::spawn(async move {
            if s.recv().await.is_some() {
                heard_daemon::dlog!("signal", sig = name);
                d.stop();
            }
        }));
    }
    if let Ok(mut s) = signal(SignalKind::hangup()) {
        tasks.push(tokio::spawn(async move {
            while s.recv().await.is_some() {
                heard_daemon::dlog!("signal", sig = "SIGHUP");
                daemon.reload();
            }
        }));
    }
    tasks
}
