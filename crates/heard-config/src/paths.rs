//! The path constants from the top of `engine/heard/config.py`.
//!
//! Python computes them once, at import time, from `platformdirs` and `$HOME`;
//! the test suite then monkeypatches the module attributes to redirect them at
//! a tmp dir (see `engine/tests/conftest.py`). There is no module state to
//! patch in Rust, so the snapshot becomes a value: [`Paths`]. Construct it from
//! the process environment with [`Paths::from_env`], or from an explicit
//! [`PathEnv`] (which is what the golden corpus drives) or straight from a
//! directory with [`Paths::under`] (which is what tests do).
//!
//! # How `platformdirs` resolves these on macOS
//!
//! Read from `platformdirs/macos.py` + `platformdirs/_xdg.py` (4.11.7, the
//! version installed for the engine; `engine/pyproject.toml` only pins
//! `platformdirs>=4.0`):
//!
//! * `MacOS(XDGMixin, _MacOSDefaults)` — the XDG mixin comes FIRST, so an XDG
//!   environment variable wins over the Apple location on macOS too. That is
//!   new in 4.11; older platformdirs ignored XDG on macOS entirely, so an
//!   install running 4.0–4.10 with `XDG_CONFIG_HOME` exported would resolve
//!   differently. We follow the installed (modern) behaviour.
//! * `user_config_dir` and `user_data_dir` are BOTH
//!   `~/Library/Application Support/<appname>` on macOS — config and data land
//!   in the same directory, which is why `config.CONFIG_DIR == config.DATA_DIR`
//!   in practice.
//! * The XDG value is taken `.strip()`ped, and a blank value falls back as if
//!   unset. It is NOT `expanduser`'d, so a literal `~/...` stays literal.
//! * `_append_app_name_and_version` appends the appname (and a version, which
//!   Heard never passes) with `os.path.join`.

use crate::defaults::APP;
use crate::error::{ConfigError, Result};
use std::path::{Path, PathBuf};

/// The slice of the environment the path constants are derived from.
///
/// Held explicitly rather than read ad hoc so the golden corpus can pin the
/// resolution for environments this machine does not have.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathEnv {
    /// `$HOME`. Required — every constant below is `~`-rooted.
    pub home: Option<String>,
    /// `$XDG_CONFIG_HOME`.
    pub xdg_config_home: Option<String>,
    /// `$XDG_DATA_HOME`.
    pub xdg_data_home: Option<String>,
    /// `$HEARD_LEDGER_PATH` — the one env override `config.py` reads itself.
    pub heard_ledger_path: Option<String>,
}

impl PathEnv {
    /// Read the four variables from the process environment.
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).ok();
        PathEnv {
            home: get("HOME"),
            xdg_config_home: get("XDG_CONFIG_HOME"),
            xdg_data_home: get("XDG_DATA_HOME"),
            heard_ledger_path: get("HEARD_LEDGER_PATH"),
        }
    }
}

/// Every path constant `engine/heard/config.py` defines, resolved once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `CONFIG_DIR` — `platformdirs.user_config_dir("heard")`.
    pub config_dir: PathBuf,
    /// `DATA_DIR` — `platformdirs.user_data_dir("heard")`.
    pub data_dir: PathBuf,
    /// `CONFIG_PATH` — `CONFIG_DIR / "config.yaml"`.
    pub config_path: PathBuf,
    /// `MODELS_DIR` — `DATA_DIR / "models"`.
    pub models_dir: PathBuf,
    /// `SOCKET_PATH` — `DATA_DIR / "daemon.sock"`, the daemon's fire-and-forget socket.
    pub socket_path: PathBuf,
    /// `LOG_PATH` — `DATA_DIR / "daemon.log"`.
    pub log_path: PathBuf,
    /// `PID_PATH` — `DATA_DIR / "daemon.pid"`.
    pub pid_path: PathBuf,
    /// `HEARD_DIR` — `~/.heard`. The notch-facing dir, deliberately SEPARATE
    /// from `CONFIG_DIR`: `capture.py` and the notch read it, the daemon writes it.
    pub heard_dir: PathBuf,
    /// `TURNS_PATH` — `HEARD_DIR / "turns.jsonl"`.
    pub turns_path: PathBuf,
    /// `LEDGER_PATH` — `$HEARD_LEDGER_PATH` or `HEARD_DIR / "ledger.db"`.
    /// The D0.1c local-first event ledger. Under `HEARD_DIR`, NOT `DATA_DIR`.
    pub ledger_path: PathBuf,
}

/// `os.environ.get(k, "").strip()` — blank counts as unset, per `_xdg.py`.
fn xdg(value: Option<&String>) -> Option<&str> {
    value.map(|v| v.trim()).filter(|v| !v.is_empty())
}

impl Paths {
    /// Resolve from the process environment, for the full app ([`APP`]).
    pub fn from_env() -> Result<Self> {
        Self::resolve(&PathEnv::from_process())
    }

    /// Resolve from the process environment for `app`. See
    /// [`Paths::resolve_for`].
    pub fn from_env_for(app: &str) -> Result<Self> {
        Self::resolve_for(app, &PathEnv::from_process())
    }

    /// Resolve from an explicit environment for the full app: exactly
    /// [`Paths::resolve_for`]`(`[`APP`]`, env)`. Pure — this is the function
    /// the `paths.json` fixtures pin.
    pub fn resolve(env: &PathEnv) -> Result<Self> {
        Self::resolve_for(APP, env)
    }

    /// Resolve from an explicit environment for the application `app`.
    ///
    /// `CONFIG_DIR` and `DATA_DIR` are `platformdirs`' directories for `app`.
    /// For [`APP`] (`"heard"`) the result is byte-for-byte what
    /// [`Paths::resolve`] has always returned, `~/.heard` and the
    /// `HEARD_LEDGER_PATH` override included. Any OTHER app (the CLI edition
    /// passes [`crate::CLI_APP`]) gets its own `HEARD_DIR` of `~/.<app>`, and
    /// ignores `HEARD_LEDGER_PATH`, so nothing it resolves can land in the
    /// full app's state.
    pub fn resolve_for(app: &str, env: &PathEnv) -> Result<Self> {
        // `$HOME` is used verbatim — unlike the XDG values, Python does not
        // strip it (`os.path.expanduser` just concatenates).
        let home = env
            .home
            .as_deref()
            .filter(|h| !h.is_empty())
            .ok_or(ConfigError::NoHome)?;
        let home = Path::new(home);

        // platformdirs: XDG first (4.11's XDGMixin), else ~/Library/Application Support.
        let app_support = home.join("Library").join("Application Support");
        let config_dir = match xdg(env.xdg_config_home.as_ref()) {
            Some(base) => Path::new(base).join(app),
            None => app_support.join(app),
        };
        let data_dir = match xdg(env.xdg_data_home.as_ref()) {
            Some(base) => Path::new(base).join(app),
            None => app_support.join(app),
        };

        let is_app = app == APP;
        let heard_dir = if is_app {
            home.join(".heard")
        } else {
            home.join(format!(".{app}"))
        };
        // `os.environ.get("HEARD_LEDGER_PATH") or (HEARD_DIR / "ledger.db")` —
        // plain `or`, so "" falls back but " " does not get stripped.
        let ledger_path = match env.heard_ledger_path.as_deref() {
            Some(p) if is_app && !p.is_empty() => PathBuf::from(p),
            _ => heard_dir.join("ledger.db"),
        };

        Ok(Paths {
            config_path: config_dir.join("config.yaml"),
            models_dir: data_dir.join("models"),
            socket_path: data_dir.join("daemon.sock"),
            log_path: data_dir.join("daemon.log"),
            pid_path: data_dir.join("daemon.pid"),
            turns_path: heard_dir.join("turns.jsonl"),
            ledger_path,
            config_dir,
            data_dir,
            heard_dir,
        })
    }

    /// Every path under one throwaway directory — the Rust equivalent of
    /// `conftest.py`'s `_heard_config_dirs_isolated`. Tests and fixture
    /// replays use this so nothing can reach the user's real install.
    pub fn under(root: &Path) -> Self {
        let config_dir = root.join("config");
        let data_dir = root.join("data");
        let heard_dir = root.join("heard");
        Paths {
            config_path: config_dir.join("config.yaml"),
            models_dir: data_dir.join("models"),
            socket_path: data_dir.join("daemon.sock"),
            log_path: data_dir.join("daemon.log"),
            pid_path: data_dir.join("daemon.pid"),
            turns_path: heard_dir.join("turns.jsonl"),
            ledger_path: heard_dir.join("ledger.db"),
            config_dir,
            data_dir,
            heard_dir,
        }
    }

    /// `config.CONFIG_PATH + ".signed-out"`. The core never writes it; a
    /// registered config layer may keep a marker here.
    pub fn signed_out_marker(&self) -> PathBuf {
        append_suffix(&self.config_path, ".signed-out")
    }

    /// `config.CONFIG_PATH + ".bak"`. The core never writes it; a registered
    /// config layer may keep a backup here (see `register_save_hook`).
    pub fn backup_path(&self) -> PathBuf {
        append_suffix(&self.config_path, ".bak")
    }

    /// `CONFIG_PATH.parent / ".config.lock"` — the cross-process advisory lock.
    pub fn lock_path(&self) -> PathBuf {
        self.config_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(".config.lock")
    }
}

/// `Path(str(p) + suffix)` — string concatenation, not `with_suffix`.
pub(crate) fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> PathEnv {
        PathEnv {
            home: Some("/tmp/h".into()),
            heard_ledger_path: Some("/tmp/elsewhere/ledger.db".into()),
            ..PathEnv::default()
        }
    }

    #[test]
    fn resolve_is_resolve_for_the_app() {
        assert_eq!(
            Paths::resolve(&env()).unwrap(),
            Paths::resolve_for(APP, &env()).unwrap()
        );
        let p = Paths::resolve(&env()).unwrap();
        assert_eq!(
            p.config_dir,
            Path::new("/tmp/h/Library/Application Support/heard")
        );
        assert_eq!(p.heard_dir, Path::new("/tmp/h/.heard"));
        assert_eq!(p.ledger_path, Path::new("/tmp/elsewhere/ledger.db"));
    }

    #[test]
    fn the_cli_edition_shares_nothing_with_the_app() {
        let cli = Paths::resolve_for(crate::CLI_APP, &env()).unwrap();
        let app = Paths::resolve(&env()).unwrap();
        let s = "/tmp/h/Library/Application Support/heard-cli";
        assert_eq!(cli.config_dir, Path::new(s));
        assert_eq!(cli.data_dir, Path::new(s));
        assert_eq!(cli.config_path, Path::new(s).join("config.yaml"));
        assert_eq!(cli.socket_path, Path::new(s).join("daemon.sock"));
        assert_eq!(cli.log_path, Path::new(s).join("daemon.log"));
        assert_eq!(cli.pid_path, Path::new(s).join("daemon.pid"));
        assert_eq!(cli.models_dir, Path::new(s).join("models"));
        assert_eq!(cli.heard_dir, Path::new("/tmp/h/.heard-cli"));
        assert_eq!(cli.turns_path, Path::new("/tmp/h/.heard-cli/turns.jsonl"));
        // HEARD_LEDGER_PATH belongs to the app; the CLI ignores it.
        assert_eq!(cli.ledger_path, Path::new("/tmp/h/.heard-cli/ledger.db"));
        for (a, b) in [
            (&cli.config_dir, &app.config_dir),
            (&cli.data_dir, &app.data_dir),
            (&cli.socket_path, &app.socket_path),
            (&cli.heard_dir, &app.heard_dir),
            (&cli.ledger_path, &app.ledger_path),
        ] {
            assert_ne!(a, b);
            assert!(
                !a.starts_with(b),
                "{} is under {}",
                a.display(),
                b.display()
            );
        }
    }

    #[test]
    fn resolve_for_honours_xdg_per_app() {
        let e = PathEnv {
            xdg_config_home: Some("/tmp/xc".into()),
            xdg_data_home: Some(" /tmp/xd ".into()),
            ..env()
        };
        let cli = Paths::resolve_for("heard-cli", &e).unwrap();
        assert_eq!(cli.config_dir, Path::new("/tmp/xc/heard-cli"));
        assert_eq!(cli.data_dir, Path::new("/tmp/xd/heard-cli"));
    }

    #[test]
    fn resolve_for_still_needs_a_home() {
        assert!(matches!(
            Paths::resolve_for("heard-cli", &PathEnv::default()),
            Err(ConfigError::NoHome)
        ));
    }
}
