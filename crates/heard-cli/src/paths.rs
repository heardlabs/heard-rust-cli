//! Where heard-cli keeps its state.
//!
//! One root, `~/Library/Application Support/heard-cli/` , so the CLI
//! never shares a socket, config or history with the Heard app, which lives
//! under `…/heard/`. `HEARD_CLI_HOME`
//! overrides the root; every test sets it.
//!
//! **Path resolution lives in [`resolve`] and nowhere else.** The CLI, the
//! daemon (`heard daemon` resolves through here too) and `heard-hook`'s CLI
//! edition (`HookConfig::CLI_EDITION`, which applies the same root rule:
//! `$HEARD_CLI_HOME`, else Application Support / the XDG data dir) all agree
//! on `<root>/daemon.sock`; a test pins that.
//!
//! Relation to `heard_config::Paths::resolve_for(CLI_APP, env)`: with no
//! override and no XDG variables the two agree on every file the daemon
//! touches (config, socket, pid, log, models — pinned by a test). They
//! differ on purpose in two places. `heard_dir`, the daemon's side-file dir
//! (`needs-you/`, `pending-questions/`), is the root here rather than
//! `~/.heard-cli`, so one root holds all state and `HEARD_CLI_HOME` isolates
//! all of it. And XDG_CONFIG_HOME is not consulted on macOS, because the
//! hook — which has no config crate — could not follow it to the socket.

use std::path::{Path, PathBuf};

use heard_config::Paths;

use crate::ui::{CliError, CliResult};

/// Environment variable that overrides the state root.
pub const HOME_ENV: &str = "HEARD_CLI_HOME";

/// The app directory name under Application Support.
pub const APP_DIR: &str = "heard-cli";

/// The state root: `$HEARD_CLI_HOME`, else the platform default.
pub fn state_root() -> CliResult<PathBuf> {
    if let Some(v) = std::env::var_os(HOME_ENV) {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or_else(|| {
            CliError::failure(
                "cannot find your home directory ($HOME is not set)",
                format!("set HOME, or point {HOME_ENV} at a directory for Heard's state"),
            )
        })?;
    let home = PathBuf::from(home);
    if cfg!(target_os = "macos") {
        Ok(home
            .join("Library")
            .join("Application Support")
            .join(APP_DIR))
    } else {
        let base = std::env::var_os("XDG_DATA_HOME")
            .filter(|x| !x.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("share"));
        Ok(base.join(APP_DIR))
    }
}

/// Every heard-cli path, resolved from the process environment.
pub fn resolve() -> CliResult<Paths> {
    Ok(for_root(&state_root()?))
}

/// The flat layout under one root: config, data, models, socket and the
/// daemon's "heard dir" all live in it. Built from [`Paths::under`] (which
/// keeps `ledger_path`, an app path nothing here uses, inside the root),
/// then flattened to the macOS shape (where platformdirs'
/// config dir and data dir are the same directory).
pub fn for_root(root: &Path) -> Paths {
    let base = Paths::under(root);
    Paths {
        config_dir: root.to_path_buf(),
        data_dir: root.to_path_buf(),
        config_path: root.join("config.yaml"),
        models_dir: root.join("models"),
        socket_path: root.join("daemon.sock"),
        log_path: root.join("daemon.log"),
        pid_path: root.join("daemon.pid"),
        heard_dir: root.to_path_buf(),
        turns_path: root.join("turns.jsonl"),
        ..base
    }
}

/// `history.jsonl`, which the speech queue appends to (heard-state's
/// `History::new(config_dir)`).
pub fn history_path(p: &Paths) -> PathBuf {
    p.config_dir.join("history.jsonl")
}

/// User personas: `<config_dir>/personas/*.md`, which the daemon's persona
/// source reads.
pub fn personas_dir(p: &Paths) -> PathBuf {
    p.config_dir.join("personas")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_matches_resolve_for_on_every_daemon_file() {
        let env = heard_config::PathEnv {
            home: Some("/Users/someone".into()),
            ..Default::default()
        };
        let core = Paths::resolve_for(heard_config::CLI_APP, &env).unwrap();
        let root = if cfg!(target_os = "macos") {
            PathBuf::from("/Users/someone/Library/Application Support/heard-cli")
        } else {
            PathBuf::from("/Users/someone/.local/share/heard-cli")
        };
        let ours = for_root(&root);
        if cfg!(target_os = "macos") {
            assert_eq!(ours.config_dir, core.config_dir);
            assert_eq!(ours.data_dir, core.data_dir);
            assert_eq!(ours.config_path, core.config_path);
            assert_eq!(ours.models_dir, core.models_dir);
            assert_eq!(ours.socket_path, core.socket_path);
            assert_eq!(ours.log_path, core.log_path);
            assert_eq!(ours.pid_path, core.pid_path);
        }
        assert_eq!(ours.socket_path, root.join("daemon.sock"));
        assert_eq!(ours.pid_path, root.join("daemon.pid"));
        assert_eq!(ours.log_path, root.join("daemon.log"));
        assert_eq!(ours.models_dir, root.join("models"));
        assert_eq!(history_path(&ours), root.join("history.jsonl"));
        assert_eq!(ours.heard_dir, root);
        assert!(ours.ledger_path.starts_with(&root));
    }
}
