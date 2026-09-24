//! `heard daemon` and the lifecycle around it (`start`, `stop`, `restart`).
//!
//! [`run`] is the composition root in [`crate::compose`]. `start` spawns
//! `heard daemon` detached and waits for the socket; `stop` signals the pid
//! in `daemon.pid` (SIGTERM) and waits for the socket to go.

use std::fs::OpenOptions;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use heard_config::Paths;

use crate::compose;
use crate::settings::Ctx;
use crate::ui::{CliError, CliResult};

/// Run the daemon in this process until stopped, with the default speech
/// (the voice queue, TTS by the free ladder).
pub fn run(paths: &Paths, foreground: bool) -> CliResult<()> {
    run_with(
        paths,
        compose::Options {
            foreground,
            ..compose::Options::default()
        },
    )
}

/// Run the daemon with explicit options.
pub fn run_with(paths: &Paths, opts: compose::Options) -> CliResult<()> {
    compose::run(paths, opts)
}

/// Fallback for `heard daemon --speech` (tests and the e2e replay: the hook
/// auto-starts the daemon with no flags, but with the hook's environment).
pub const SPEECH_ENV: &str = "HEARD_DAEMON_SPEECH";
/// Fallback for `heard daemon --tts`.
pub const TTS_ENV: &str = "HEARD_DAEMON_TTS";
/// Fallback for `heard daemon --notifier`.
pub const NOTIFIER_ENV: &str = "HEARD_DAEMON_NOTIFIER";

/// The flag, else a non-empty `env`, else `default`.
pub fn flag_or_env(flag: Option<String>, env: &str, default: &str) -> String {
    flag.or_else(|| std::env::var(env).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| default.to_string())
}

/// `heard daemon`'s hidden `--speech` / `--tts` values.
pub fn parse_options(
    foreground: bool,
    speech: &str,
    tts: &str,
    notifier: &str,
) -> CliResult<compose::Options> {
    let speech = match speech.trim().to_ascii_lowercase().as_str() {
        "queued" | "voice" => compose::SpeechKind::Queued,
        "log" => compose::SpeechKind::Log,
        other => {
            return Err(CliError::usage(
                format!("unknown --speech {other:?}"),
                "use queued or log",
            ))
        }
    };
    let tts = match tts.trim().to_ascii_lowercase().as_str() {
        "auto" => compose::TtsKind::Auto,
        "null" | "none" => compose::TtsKind::Null,
        other => {
            return Err(CliError::usage(
                format!("unknown --tts {other:?}"),
                "use auto or null",
            ))
        }
    };
    let notifier = match notifier.trim().to_ascii_lowercase().as_str() {
        "osascript" | "macos" => compose::NotifierKind::Osascript,
        "log" => compose::NotifierKind::Log,
        other => {
            return Err(CliError::usage(
                format!("unknown --notifier {other:?}"),
                "use osascript or log",
            ))
        }
    };
    Ok(compose::Options {
        foreground,
        speech,
        tts,
        notifier,
    })
}

/// Spawn `heard daemon` detached (stdio to the log). A plain child — not a
/// process-group leader — so the daemon's own `setsid()` succeeds at once.
pub fn spawn_detached(paths: &Paths) -> CliResult<std::process::Child> {
    let exe = std::env::current_exe().map_err(|e| {
        CliError::failure(
            format!("cannot find the heard binary: {e}"),
            "run heard from its installed path",
        )
    })?;
    std::fs::create_dir_all(&paths.data_dir).ok();
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log_path)
        .map_err(|e| {
            CliError::failure(
                format!("cannot open {}: {e}", paths.log_path.display()),
                "check you can write to Heard's state directory",
            )
        })?;
    let err = log
        .try_clone()
        .map_err(|e| CliError::failure(format!("log handle: {e}"), "retry `heard start`"))?;
    Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err)
        .spawn()
        .map_err(|e| {
            CliError::failure(
                format!("cannot start the daemon: {e}"),
                "run `heard daemon --foreground` to see why",
            )
        })
}

/// Start the daemon unless it is up; wait up to `wait` for the socket.
pub fn start(ctx: &Ctx, wait: Duration) -> CliResult<String> {
    if ctx.daemon.is_up() {
        return Ok("daemon already running".into());
    }
    let mut child = spawn_detached(&ctx.paths)?;
    let t0 = Instant::now();
    while t0.elapsed() < wait {
        if ctx.daemon.is_up() {
            return Ok(format!("daemon started (pid {})", child.id()));
        }
        if let Ok(Some(status)) = child.try_wait() {
            // Exit 0 at start = it handed off to a detached copy, or found
            // one already running: keep waiting for the socket.
            if status.success() {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            return Err(CliError::failure(
                format!(
                    "the daemon exited at start ({status}): {}",
                    last_log_line(&ctx.paths)
                ),
                format!(
                    "see {} or run `heard daemon --foreground`",
                    ctx.paths.log_path.display()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(CliError::failure(
        "the daemon did not open its socket in time",
        format!(
            "see {} or run `heard daemon --foreground`",
            ctx.paths.log_path.display()
        ),
    ))
}

/// Stop the daemon via the pid in `daemon.pid`.
pub fn stop(ctx: &Ctx, wait: Duration) -> CliResult<String> {
    if !ctx.daemon.is_up() {
        return Ok("daemon not running".into());
    }
    let pid = std::fs::read_to_string(&ctx.paths.pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| {
            CliError::failure(
                format!(
                    "the daemon is running but {} has no pid",
                    ctx.paths.pid_path.display()
                ),
                "stop it by hand: `pkill -f 'heard daemon'`",
            )
        })?;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).map_err(|e| {
        CliError::failure(
            format!("cannot signal daemon pid {}: {e}", pid.as_raw_nonzero()),
            "stop it by hand: `pkill -f 'heard daemon'`",
        )
    })?;
    let t0 = Instant::now();
    while t0.elapsed() < wait {
        // Down = the socket is gone AND daemon.pid no longer names it (the
        // daemon's last act, after releasing its single-instance lock), so a
        // `start` right after cannot race the old one's exit.
        let alive = rustix::process::test_kill_process(pid).is_ok()
            && std::fs::read_to_string(&ctx.paths.pid_path)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                == Some(pid.as_raw_nonzero().get());
        if !ctx.daemon.is_up() && !alive {
            return Ok("daemon stopped".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(CliError::failure(
        "the daemon did not stop in time",
        "stop it by hand: `pkill -f 'heard daemon'`",
    ))
}

fn last_log_line(paths: &Paths) -> String {
    std::fs::read_to_string(&paths.log_path)
        .ok()
        .and_then(|t| {
            let lines: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
            lines
                .iter()
                .rev()
                .find(|l| l.starts_with("error:"))
                .or(lines.last())
                .map(|l| l.trim_start_matches("error:").trim().to_string())
        })
        .unwrap_or_else(|| "no log output".into())
}
