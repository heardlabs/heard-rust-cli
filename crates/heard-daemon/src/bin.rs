//! Argument parsing and the safety rails around which socket the binary is
//! allowed to bind — the standalone `heard-daemon` binary's.
//!
//! The CLI edition's `heard daemon` has its own composition root; this
//! module is kept as library API and for its socket-refusal tests.

use std::path::PathBuf;

/// What the binary was asked to do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Args {
    /// `--socket <path>`. Overrides `HEARD_DAEMON_SOCKET`.
    pub socket: Option<PathBuf>,
    /// `--differential <python-sock-path>` — the PYTHON daemon's socket, for
    /// the tee that compares what the two would say.
    ///
    /// Parsed and carried; the tee lives in the hook path and is the next
    /// lane. The flag exists now so the differential run's command line does
    /// not change when it does.
    pub differential: Option<PathBuf>,
    /// `--speech-log <path>`. Where would-be utterances are recorded.
    /// Defaults to `speech.jsonl` beside the socket.
    pub speech_log: Option<PathBuf>,
    /// `--state-dir <path>`. The root every Heard path is resolved under
    /// (`heard_config::Paths::under`). Defaults to the socket's directory.
    ///
    /// It deliberately does NOT default to the installed Heard's
    /// `~/Library/Application Support/heard`. Two things there are
    /// read-modify-write state that the Python daemon is actively using:
    /// `spoken/<session>.json` + `.offset` (which decide whether a line has
    /// already been narrated) and `history.jsonl`. A Rust daemon sharing them
    /// during a differential run would mark prose spoken on Python's behalf
    /// and silence the very divergence the run exists to find. Point this at
    /// a directory with a COPY of `config/config.yaml` instead.
    pub state_dir: Option<PathBuf>,
    /// `--help`.
    pub help: bool,
}

/// What the binary prints for `--help`.
pub const USAGE: &str = "\
heard-daemon — the Rust Heard daemon (routes narration, records instead of
speaking).

USAGE:
    heard-daemon [--socket <path>] [--state-dir <path>]
                 [--differential <path>] [--speech-log <path>]

OPTIONS:
    --socket <path>        Unix socket to bind. Required unless
                           HEARD_DAEMON_SOCKET is set. Must NOT be the live
                           daemon.sock of an installed Heard.
    --state-dir <path>     Root for config.yaml, spoken state and
                           history.jsonl (default: the socket's directory).
                           Config is read from <state-dir>/config/config.yaml
                           — copy your own there. It does NOT default to the
                           installed Heard's directory: the spoken-dedup files
                           are read-modify-write state the Python daemon is
                           using, and sharing them would hide the divergence a
                           differential run is looking for.
    --differential <path>  The Python daemon's socket, for the differential
                           run. Parsed and carried; the tee is not wired yet.
    --speech-log <path>    JSONL of every would-be utterance
                           (default: speech.jsonl beside the socket).
    -h, --help             Show this.

This binary never produces audio.\
";

/// Parse `argv[1..]`.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Args, String> {
    let mut out = Args::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => out.help = true,
            "--socket" => out.socket = Some(take(&mut it, "--socket")?.into()),
            "--differential" => out.differential = Some(take(&mut it, "--differential")?.into()),
            "--speech-log" => out.speech_log = Some(take(&mut it, "--speech-log")?.into()),
            "--state-dir" => out.state_dir = Some(take(&mut it, "--state-dir")?.into()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(out)
}

fn take<I: Iterator<Item = String>>(it: &mut I, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} needs a value"))
}

/// The socket this process may bind.
///
/// `--socket` wins, then `HEARD_DAEMON_SOCKET`. There is deliberately no
/// third fallback to [`heard_proto::transport::socket_path`]'s default: that
/// default is the socket a LIVE Python daemon is listening on, and a Rust
/// daemon that took it would silently intercept half the user's hooks. The
/// refusal below is the rail, and it is the whole point of the differential
/// design — the two daemons run side by side on different sockets.
pub fn resolve_socket(args: &Args) -> Result<PathBuf, String> {
    let path = match &args.socket {
        Some(p) => p.clone(),
        None => match std::env::var_os(heard_proto::transport::SOCKET_PATH_ENV) {
            Some(p) => PathBuf::from(p),
            None => {
                return Err(format!(
                    "no socket given: pass --socket or set {}. This binary will not \
                     bind the installed daemon's socket.",
                    heard_proto::transport::SOCKET_PATH_ENV
                ))
            }
        },
    };
    if is_installed_socket(&path) {
        return Err(format!(
            "refusing to bind {} — that is the installed Heard daemon's socket. \
             Use a different path; the Rust and Python daemons run side by side.",
            path.display()
        ));
    }
    Ok(path)
}

/// Would this path take the live install's socket?
fn is_installed_socket(path: &std::path::Path) -> bool {
    let Ok(home) = std::env::var("HOME") else {
        return false;
    };
    let real = std::path::Path::new(&home).join("Library/Application Support/heard/daemon.sock");
    path == real
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn every_flag_parses() {
        let args = parse(argv(&[
            "--socket",
            "/tmp/a.sock",
            "--differential",
            "/tmp/py.sock",
            "--speech-log",
            "/tmp/s.jsonl",
            "--state-dir",
            "/tmp/state",
        ]))
        .expect("parses");
        assert_eq!(args.socket, Some("/tmp/a.sock".into()));
        assert_eq!(args.differential, Some("/tmp/py.sock".into()));
        assert_eq!(args.speech_log, Some("/tmp/s.jsonl".into()));
        assert_eq!(args.state_dir, Some("/tmp/state".into()));
        assert!(!args.help);
    }

    #[test]
    fn a_flag_without_a_value_is_an_error() {
        assert!(parse(argv(&["--socket"])).is_err());
        assert!(parse(argv(&["--nope"])).is_err());
    }

    #[test]
    fn no_socket_anywhere_is_refused_rather_than_defaulted() {
        let args = Args::default();
        // The env var may be set in the test process; only assert the
        // refusal when it is not.
        if std::env::var_os(heard_proto::transport::SOCKET_PATH_ENV).is_none() {
            assert!(resolve_socket(&args).is_err());
        }
    }

    #[test]
    fn the_installed_socket_is_refused() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        let args = Args {
            socket: Some(
                std::path::Path::new(&home).join("Library/Application Support/heard/daemon.sock"),
            ),
            ..Args::default()
        };
        let err = resolve_socket(&args).expect_err("must refuse");
        assert!(err.contains("refusing to bind"), "{err}");
    }
}
