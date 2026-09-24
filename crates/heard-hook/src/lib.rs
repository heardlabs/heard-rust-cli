//! `heard-hook` — the binary Claude Code and Codex spawn on every tool call.
//!
//! It is a **dumb pipe**, and that is the whole point. `python -m heard.hook`
//! pays ~200 ms of frozen-interpreter startup and then *sleeps 800 ms*
//! (`flush_delay_ms`) waiting for the agent to flush its transcript, parses
//! that transcript, dedups against a flock'd file and renders a template —
//! all on the agent's critical path, several times a minute.
//!
//! This binary does none of that. It reads stdin, parses it once, wraps it in
//! one `{"cmd":"hook","agent":…,"payload":{…}}` frame carrying the payload
//! verbatim, writes that frame to the daemon socket, and exits. The flush
//! wait, the transcript read, the dedup and the `hook_event_name` dispatch
//! all move into the daemon, which is already long-lived and already has the
//! session state.
//!
//! ## Editions
//!
//! One crate builds every edition's hook. What differs is a [`HookConfig`]:
//! the state directory the socket lives in, and whether the two CLI-edition
//! behaviours below are on. This crate's own binary runs
//! [`HookConfig::CLI_EDITION`]; another edition (an app embedding the core) builds its
//! own three-line `main` around `heard_hook::run(&HookConfig::plain("heard"))`
//! — its own socket, no auto-start, no intercept.
//!
//! ## Auto-start (CLI edition)
//!
//! When nothing is listening on the socket, the hook spawns `heard daemon` —
//! the `heard` binary beside its own executable — fully detached (its own
//! process group, stdio on `/dev/null`, never waited for), then retries the
//! send for up to [`AUTOSTART_BUDGET`]. A lock file beside the socket makes a
//! burst of hooks spawn once: the rest only retry. Nothing here can fail the
//! agent: no `heard` binary, a spawn error, a daemon that never comes up —
//! all end in the same silent exit 0.
//! `HEARD_HOOK_NO_AUTOSTART=1` turns it off (the test suites set it).
//!
//! ## The zero-token intercept (CLI edition, Claude Code)
//!
//! A `UserPromptSubmit` whose prompt starts with `/heard-<verb>` or
//! `@heard <verb>` is never forwarded and never reaches the model. The hook
//! runs `heard <verb> <args…> --porcelain` synchronously (2 s budget) and
//! prints Claude Code's block decision with the command's one-line answer:
//!
//! ```text
//! {"decision":"block","reason":"Heard: Focus mode"}
//! ```
//!
//! Every other prompt, and every other event, is forwarded unchanged.
//!
//! ## The differential tee
//!
//! With `HEARD_DIFFERENTIAL_SOCKET=<path>` in its environment the hook also
//! writes the byte-identical frame to that second socket — the Rust daemon
//! running `--differential` beside the live Python one. The primary send
//! always happens FIRST; the tee then gets at most 50 ms (a dead socket fails
//! the connect immediately) and can never change what the primary daemon
//! receives or the exit code. A tee path equal to the primary socket is
//! ignored, so a misconfiguration cannot double-deliver.
//!
//! ## It never fails loudly
//!
//! Every path exits `0`. A hook that returns non-zero or writes to stderr
//! interrupts the agent the user is actually working with, and no narration
//! is worth that. Bad JSON, no socket, a dead daemon, an unknown agent name —
//! all are silent no-ops. The only stdout is the intercept's decision and
//! the health probe's reply.
//!
//! ## Measured
//!
//! Release build, M-series Mac, 500 sequential invocations against a
//! listening socket with a realistic `PreToolUse` payload and a terminal
//! environment to bind: **2.4–4.4 ms** wall per invocation across runs, of
//! which **1.6–3.2 ms** is the shell's own `fork`/`exec` floor
//! (`/usr/bin/true` in the identical loop, measured alongside). The hook's own
//! cost is **~0.7 ms**: one `read`, one parse, a dozen `getenv`s, one
//! `connect`, one `write`. The CLI edition adds one substring scan of the
//! payload (for `UserPromptSubmit`) on the path where the daemon is up.
//!
//! ## Usage
//!
//! ```text
//! heard-hook claude-code [--edition heard-cli]   # payload on stdin
//! heard-hook codex [--edition heard-cli]
//! heard-hook <agent> --health-probe <32-char nonce>
//! heard-hook --version
//! ```
//!
//! `--edition <name>` is the installer's ownership marker and is otherwise
//! ignored: the edition is fixed when the binary is built.

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use heard_proto::{Agent, Event, HealthProbe, HealthProbeResponse, HookEnv, HookFrame, Request};
use serde_json::value::RawValue;

/// Which edition this hook is, and what it does beyond forwarding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookConfig {
    /// The state directory under `~/Library/Application Support` (or the XDG
    /// data dir off macOS) whose `daemon.sock` the hook writes to.
    pub app_dir: &'static str,
    /// An environment variable that, when set and non-empty, IS the state
    /// root (replaces `~/Library/Application Support/<app_dir>`).
    pub root_env: Option<&'static str>,
    /// The CLI binary beside `heard-hook` that auto-start and the intercept
    /// run. `None` disables both.
    pub cli_bin: Option<&'static str>,
    /// Spawn `<cli_bin> daemon` when the socket is not listening.
    pub auto_start: bool,
    /// Answer `/heard-<verb>` prompts on Claude Code's `UserPromptSubmit`.
    pub intercept: bool,
}

impl HookConfig {
    /// The free CLI edition: `~/Library/Application Support/heard-cli/daemon.sock`
    /// (`HEARD_CLI_HOME` overrides the root), auto-start and intercept on.
    pub const CLI_EDITION: HookConfig = HookConfig {
        app_dir: "heard-cli",
        root_env: Some("HEARD_CLI_HOME"),
        cli_bin: Some("heard"),
        auto_start: true,
        intercept: true,
    };

    /// A forward-only hook for another edition's socket directory: no root
    /// override, no auto-start, no intercept.
    pub const fn plain(app_dir: &'static str) -> HookConfig {
        HookConfig {
            app_dir,
            root_env: None,
            cli_bin: None,
            auto_start: false,
            intercept: false,
        }
    }

    /// The daemon socket this edition talks to. `HEARD_DAEMON_SOCKET`
    /// (tests) wins over everything.
    pub fn socket_path(&self) -> Option<PathBuf> {
        if let Some(p) = std::env::var_os(heard_proto::transport::SOCKET_PATH_ENV) {
            return Some(PathBuf::from(p));
        }
        Some(self.state_root()?.join("daemon.sock"))
    }

    /// The edition's state root.
    pub fn state_root(&self) -> Option<PathBuf> {
        if let Some(var) = self.root_env {
            if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
                return Some(PathBuf::from(v));
            }
        }
        let home = PathBuf::from(std::env::var_os("HOME").filter(|h| !h.is_empty())?);
        let mut p = if cfg!(target_os = "macos") {
            home.join("Library/Application Support")
        } else {
            match std::env::var_os("XDG_DATA_HOME") {
                Some(x) if !x.is_empty() => PathBuf::from(x),
                _ => home.join(".local/share"),
            }
        };
        p.push(self.app_dir);
        Some(p)
    }
}

/// `hook.py`'s health-probe timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// `client.send()`'s socket timeout.
const SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Set by Heard's own `claude -p` subprocess. Without this guard the
/// narration pipeline's CLI fallback re-enters the hook on its own Stop
/// event and chases its tail: daemon narrates → spawns claude → claude
/// finishes → Stop hook → daemon narrates → …
pub const DISABLE_ENV: &str = "HEARD_HOOK_DISABLED";

/// `=1` turns auto-start off.
pub const NO_AUTOSTART_ENV: &str = "HEARD_HOOK_NO_AUTOSTART";

/// How long a hook that found no daemon keeps retrying the send.
pub const AUTOSTART_BUDGET: Duration = Duration::from_millis(300);

/// A spawn lock younger than this means another hook is already starting
/// the daemon; older, and it is left over from a daemon that since died.
const SPAWN_LOCK_STALE: Duration = Duration::from_secs(5);

/// How long the intercept waits for `heard <verb>`.
pub const INTERCEPT_TIMEOUT: Duration = Duration::from_secs(2);

/// The verbs the intercept answers. Anything else (`/heard-foo`, the user's
/// own command that happens to share the prefix) goes to the model as usual.
pub const INTERCEPT_VERBS: &[&str] = &[
    "mode", "voice", "persona", "speed", "alerts", "pause", "resume", "mute", "unmute", "status",
    "say",
];

/// Run the hook with this process's argv, stdin and environment. Always
/// returns; the caller exits 0.
pub fn run(cfg: &HookConfig) {
    let _ = run_inner(cfg, std::env::args_os().skip(1).collect());
}

fn run_inner(cfg: &HookConfig, args: Vec<OsString>) -> Option<()> {
    let mut args = args.into_iter();
    let first = args.next()?;
    let first = first.to_str()?;

    if first == "--version" || first == "-V" {
        println!("heard-hook {}", env!("CARGO_PKG_VERSION"));
        return Some(());
    }

    // Unknown agent → nothing to do, exactly as `hook.py`'s AGENTS lookup.
    let agent = Agent::from_argv(first)?;
    let sock = cfg.socket_path()?;

    // `heard-hook <agent> --health-probe <nonce>`: a request, not a send.
    // Answers on stdout so `connection_health` can read it, and prints the
    // literal `{}` on any failure — the same thing `client.request()`
    // returns when the daemon is unreachable. It never starts a daemon.
    let second = args.next();
    if second.as_deref().and_then(|a| a.to_str()) == Some("--health-probe") {
        let nonce = args.next()?;
        health_probe(&sock, agent, nonce.to_str()?);
        return Some(());
    }

    if env_is_1(DISABLE_ENV) {
        return Some(());
    }

    // One read, one parse, one write. `hook_event_name` is NOT lifted out of
    // the payload — it is already in there, and the daemon dispatches on it.
    // A second copy on the frame could only ever drift from the first.
    let mut stdin = Vec::new();
    std::io::stdin().read_to_end(&mut stdin).ok()?;
    let payload: &RawValue = serde_json::from_slice(&stdin).ok()?;

    if cfg.intercept && agent == Agent::ClaudeCode {
        if let Some(cli) = cfg.cli_bin {
            if let Some((verb, rest)) = intercepted_command(&stdin) {
                let reason = run_cli_command(&sibling(cli), &verb, &rest);
                let decision = serde_json::json!({"decision": "block", "reason": reason});
                println!("{decision}");
                return Some(());
            }
        }
    }

    // The binding has to be derived HERE. This process is a child of the
    // agent CLI, which is a child of the terminal that launched it, so its
    // environment is the session's own process-tree evidence. The daemon's
    // environment is the menu-bar app's and names no terminal at all.
    let env = HookEnv::from_process();
    let frame = serde_json::to_vec(&HookFrame::new(agent, payload, env.binding())).ok()?;
    // The primary send FIRST: the tee can only ever run after the live
    // daemon already has its frame (or was found missing), so it cannot
    // delay it.
    let mut primary = send(&sock, &frame);
    if primary == Err(SendError::NotListening) && cfg.auto_start && !env_is_1(NO_AUTOSTART_ENV) {
        if let Some(cli) = cfg.cli_bin {
            if autostart(&sibling(cli), &sock) {
                let deadline = Instant::now() + AUTOSTART_BUDGET;
                while primary == Err(SendError::NotListening) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                    primary = send(&sock, &frame);
                }
            }
        }
    }
    tee(&sock, &frame);
    primary.ok()
}

fn env_is_1(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| v == "1")
}

/// `name` beside this executable.
fn sibling(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

#[derive(Debug, PartialEq, Eq)]
enum SendError {
    /// The connect failed: no socket file, or nobody accepting on it.
    NotListening,
    /// Connected, then the write failed. Never retried — the daemon may
    /// already have part of the frame.
    Write,
}

/// Fire-and-forget: `connect → write → close`, no trailing newline.
fn send(sock: &Path, frame: &[u8]) -> Result<(), SendError> {
    let mut stream = UnixStream::connect(sock).map_err(|_| SendError::NotListening)?;
    let _ = stream.set_write_timeout(Some(SEND_TIMEOUT));
    stream.write_all(frame).map_err(|_| SendError::Write)?;
    stream.flush().map_err(|_| SendError::Write)
}

/// Spawn `<cli> daemon` detached, unless another hook already is. Returns
/// whether a daemon is plausibly on its way (so retrying is worth it).
fn autostart(cli: &Path, sock: &Path) -> bool {
    if !is_executable(cli) {
        return false;
    }
    let mut lock = sock.as_os_str().to_owned();
    lock.push(".spawn.lock");
    let lock = PathBuf::from(lock);
    if let Some(dir) = lock.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut took = OpenOptions::new().write(true).create_new(true).open(&lock);
    if took.is_err() {
        let stale = std::fs::metadata(&lock)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > SPAWN_LOCK_STALE);
        if !stale {
            // Someone else is spawning it right now: just wait for it.
            return true;
        }
        let _ = std::fs::remove_file(&lock);
        took = OpenOptions::new().write(true).create_new(true).open(&lock);
    }
    let Ok(mut f) = took else {
        return true;
    };
    let _ = write!(f, "{}", std::process::id());
    // Its own process group: a Ctrl-C in the agent's terminal, or the agent
    // killing the hook's group on a timeout, never reaches the daemon. stdio
    // on /dev/null so it holds none of the agent's pipes open. The Child is
    // dropped unwaited; when this process exits the daemon is reparented.
    // `heard daemon` is expected to `setsid()` itself to drop the
    // controlling terminal (this crate forbids the `unsafe` a pre_exec
    // setsid would need).
    Command::new(cli)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .is_ok()
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

// ---------------------------------------------------------------------------
// The intercept

/// The two fields the intercept looks at; everything else is skipped.
#[derive(serde::Deserialize)]
struct PromptPeek<'a> {
    #[serde(borrow, default)]
    hook_event_name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    prompt: Option<Cow<'a, str>>,
}

/// `Some((verb, args))` when this payload is a `UserPromptSubmit` whose
/// prompt is a Heard command.
fn intercepted_command(payload: &[u8]) -> Option<(String, Vec<String>)> {
    // Cheap pre-check so PreToolUse/PostToolUse/Stop pay one memchr-ish
    // scan and no second parse.
    if !contains(payload, b"UserPromptSubmit") {
        return None;
    }
    let peek: PromptPeek<'_> = serde_json::from_slice(payload).ok()?;
    if peek.hook_event_name.as_deref() != Some("UserPromptSubmit") {
        return None;
    }
    parse_prompt(peek.prompt.as_deref()?)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// `/heard-mode focus` or `@heard mode focus` → `("mode", ["focus"])`.
pub fn parse_prompt(prompt: &str) -> Option<(String, Vec<String>)> {
    let t = prompt.trim();
    let rest = if let Some(r) = t.strip_prefix("/heard-") {
        r
    } else {
        let r = t.strip_prefix("@heard")?;
        if !r.starts_with(char::is_whitespace) {
            return None;
        }
        r.trim_start()
    };
    let mut words = rest.split_whitespace();
    let verb = words.next()?;
    if !INTERCEPT_VERBS.contains(&verb) {
        return None;
    }
    Some((verb.to_owned(), words.map(str::to_owned).collect()))
}

/// Run `<cli> <verb> <args…> --porcelain` with a deadline and turn what it
/// printed into the block reason: `Heard: <first line>`.
pub fn run_cli_command(cli: &Path, verb: &str, args: &[String]) -> String {
    let child = Command::new(cli)
        .arg(verb)
        .args(args)
        .arg("--porcelain")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(_) => {
            return "Heard: the heard command was not found next to heard-hook; reinstall with install.sh".into()
        }
    };
    let out = child.stdout.take().map(drain);
    let err = child.stderr.take().map(drain);
    let deadline = Instant::now() + INTERCEPT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let Some(status) = status else {
        return format!("Heard: `heard {verb}` timed out");
    };
    let collect = |h: Option<std::thread::JoinHandle<Vec<u8>>>| {
        h.and_then(|h| h.join().ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    let (out, err) = (collect(out), collect(err));
    let line = first_line(&out)
        .or_else(|| first_line(&err))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if status.success() {
                "done".to_owned()
            } else {
                format!("`heard {verb}` failed")
            }
        });
    with_prefix(&line)
}

/// Read a pipe to EOF on a thread, so a chatty child never blocks on a
/// full pipe while we wait for it.
fn drain<R: Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        buf
    })
}

fn first_line(s: &str) -> Option<&str> {
    s.lines().map(str::trim).find(|l| !l.is_empty())
}

fn with_prefix(line: &str) -> String {
    for p in ["Heard:", "heard:"] {
        if let Some(rest) = line.strip_prefix(p) {
            return format!("Heard: {}", rest.trim_start());
        }
    }
    format!("Heard: {line}")
}

// ---------------------------------------------------------------------------
// Tee and health probe

/// `HEARD_DIFFERENTIAL_SOCKET` — the differential run's second socket.
const DIFFERENTIAL_ENV: &str = "HEARD_DIFFERENTIAL_SOCKET";

/// How long the tee may spend writing. A differential daemon that is gone
/// fails the connect at once (`ECONNREFUSED` / `ENOENT`); one that is alive
/// but not reading can hold the write at most this long before the hook
/// exits anyway. Bounded, and only ever AFTER the primary frame was sent.
const TEE_BUDGET: Duration = Duration::from_millis(50);

/// Send the SAME bytes to the differential socket, if one is configured.
///
/// Best effort in every way: no socket, a dead daemon, a slow reader, a
/// path equal to the primary one — each is a silent no-op, like every other
/// failure in this binary. The frame is byte-identical to the primary one so
/// the two daemons see exactly the same input.
fn tee(primary: &Path, frame: &[u8]) {
    let Some(path) = std::env::var_os(DIFFERENTIAL_ENV) else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let path = PathBuf::from(path);
    // Never tee into the primary socket: that would deliver every hook twice
    // to the live daemon.
    if path == primary {
        return;
    }
    let Ok(mut stream) = UnixStream::connect(&path) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(TEE_BUDGET));
    let _ = stream.write_all(frame);
}

fn health_probe(sock: &Path, agent: Agent, nonce: &str) {
    let probe = Request::Event(Event::HealthProbe(HealthProbe::new(agent, nonce)));
    let line = request(sock, &probe)
        .and_then(|buf| serde_json::from_slice::<HealthProbeResponse>(&buf).ok())
        .and_then(|r| serde_json::to_string(&r).ok())
        .unwrap_or_else(|| "{}".into());
    println!("{line}");
}

/// `client.request()`: send, half-close, read the reply to EOF.
fn request(sock: &Path, req: &Request) -> Option<Vec<u8>> {
    let mut stream = UnixStream::connect(sock).ok()?;
    stream.set_write_timeout(Some(PROBE_TIMEOUT)).ok()?;
    stream.set_read_timeout(Some(PROBE_TIMEOUT)).ok()?;
    stream.write_all(&serde_json::to_vec(req).ok()?).ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_that_are_heard_commands() {
        let p = |s: &str| parse_prompt(s).map(|(v, a)| (v, a.join(" ")));
        assert_eq!(
            p("/heard-mode focus"),
            Some(("mode".into(), "focus".into()))
        );
        assert_eq!(p("  /heard-pause \n"), Some(("pause".into(), "".into())));
        assert_eq!(p("@heard speed 1.2"), Some(("speed".into(), "1.2".into())));
        assert_eq!(
            p("@heard\tvoice  bm_george"),
            Some(("voice".into(), "bm_george".into()))
        );
        assert_eq!(
            p("/heard-say hello  there"),
            Some(("say".into(), "hello there".into()))
        );
        for no in [
            "please /heard-mode focus",
            "/heard-",
            "/heard-unknown x",
            "@heardmode focus",
            "@heard",
            "/heardmode",
            "/mode focus",
            "",
        ] {
            assert_eq!(parse_prompt(no), None, "{no:?}");
        }
    }

    #[test]
    fn reason_prefix_is_normalised() {
        assert_eq!(with_prefix("Focus mode"), "Heard: Focus mode");
        assert_eq!(with_prefix("Heard: Focus mode"), "Heard: Focus mode");
        assert_eq!(
            with_prefix("heard: daemon not running"),
            "Heard: daemon not running"
        );
        assert_eq!(first_line("\n  \nFocus mode\nmore"), Some("Focus mode"));
    }

    #[test]
    fn the_cli_edition_socket_lives_under_heard_cli() {
        let cfg = HookConfig::CLI_EDITION;
        assert_eq!(cfg.app_dir, "heard-cli");
        assert_eq!(HookConfig::plain("heard").cli_bin, None);
        assert!(!HookConfig::plain("heard").auto_start);
    }
}
